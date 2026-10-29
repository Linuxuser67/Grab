//! Loopback HTTP proxy for extractor subprocesses handling untrusted browser handoffs.
//!
//! yt-dlp/gallery-dl resolve DNS in their own processes, outside reqwest's
//! GuardedResolver. This proxy resolves each target itself, rejects private
//! and special-use addresses, then connects to the selected IP (not the name)
//! to avoid a DNS check/connect race. It supports HTTP CONNECT and ordinary
//! HTTP proxy requests. It is only enabled for browser-handoff hosts whose
//! public-resolution policy is active.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::fmt::Write as _;
use std::io;
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

static PROXY_STATE: OnceLock<ProxyState> = OnceLock::new();

#[derive(Clone)]
struct ProxyState {
    addr: SocketAddr,
    token: String,
    expected_authorization: String,
}

impl ProxyState {
    fn proxy_url(&self) -> String {
        format!("http://grab:{}@{}", self.token, self.addr)
    }
}

fn proxy_authorization_header(token: &str) -> String {
    format!("Basic {}", STANDARD.encode(format!("grab:{token}")))
}

fn random_proxy_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("couldn't generate guarded proxy token: {error}"))?;
    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut token, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(token)
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (&a, &b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    difference == 0
}

/// Match exactly one Proxy-Authorization header; duplicated headers are rejected.
fn proxy_authorization_matches(header: &[u8], expected: &str) -> bool {
    let text = String::from_utf8_lossy(header);
    let mut provided = None;
    for line in text.split("\r\n").skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("proxy-authorization") {
            if provided.is_some() {
                return false;
            }
            provided = Some(value.trim());
        }
    }
    provided.is_some_and(|value| constant_time_equal(value.as_bytes(), expected.as_bytes()))
}
const MAX_HEADER_BYTES: usize = 64 * 1024;
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound DNS resolution and sequential public-address connection attempts too:
/// each accepted socket holds one of the fixed proxy permits until connect ends.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// A hostname can resolve to many addresses; one blackholed address must not
/// consume the whole budget before trying the next.
const ADDRESS_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const TUNNEL_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_PROXY_CONNECTIONS: usize = 64;
static PROXY_CONNECTIONS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_PROXY_CONNECTIONS)));
static PROXY_CLOCK_ORIGIN: OnceLock<Instant> = OnceLock::new();

fn proxy_clock_ms() -> u64 {
    PROXY_CLOCK_ORIGIN
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

struct ActivityStream<S> {
    inner: S,
    last_activity_ms: Arc<AtomicU64>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for ActivityStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buffer.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(cx, buffer);
        if matches!(&result, Poll::Ready(Ok(()))) && buffer.filled().len() > before {
            this.last_activity_ms
                .store(proxy_clock_ms(), Ordering::Relaxed);
        }
        result
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for ActivityStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write(cx, buffer);
        if matches!(&result, Poll::Ready(Ok(count)) if *count > 0) {
            this.last_activity_ms
                .store(proxy_clock_ms(), Ordering::Relaxed);
        }
        result
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

async fn wait_for_tunnel_idle(last_activity_ms: Arc<AtomicU64>, idle_timeout: Duration) {
    let poll_interval = (idle_timeout / 4)
        .min(Duration::from_secs(10))
        .max(Duration::from_millis(1));
    loop {
        tokio::time::sleep(poll_interval).await;
        let idle_ms = proxy_clock_ms().saturating_sub(last_activity_ms.load(Ordering::Relaxed));
        if idle_ms >= idle_timeout.as_millis() as u64 {
            return;
        }
    }
}

async fn copy_bidirectional_with_idle_timeout(
    client: &mut TcpStream,
    upstream: &mut TcpStream,
    idle_timeout: Duration,
) -> io::Result<(u64, u64)> {
    let last_activity_ms = Arc::new(AtomicU64::new(proxy_clock_ms()));
    let mut client = ActivityStream {
        inner: client,
        last_activity_ms: Arc::clone(&last_activity_ms),
    };
    let mut upstream = ActivityStream {
        inner: upstream,
        last_activity_ms: Arc::clone(&last_activity_ms),
    };
    tokio::select! {
        result = tokio::io::copy_bidirectional(&mut client, &mut upstream) => result,
        _ = wait_for_tunnel_idle(last_activity_ms, idle_timeout) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "guarded proxy tunnel was idle for too long",
        )),
    }
}

/// Validate proxy compatibility before accepting an untrusted handoff. We
/// fail closed rather than silently bypassing a configured proxy or delegating
/// target DNS resolution to it.
pub(crate) fn validate_proxy_compatibility() -> Result<(), String> {
    let opts =
        crate::download_net::DownloadOptions::from_settings(&crate::settings::AppSettings::new());
    if opts.proxy_config()?.is_some()
        || [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ]
        .iter()
        .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()))
    {
        return Err(
            "Cannot safely accept this browser handoff while an upstream or environment proxy is configured; disable the proxy or use a direct user-entered URL".to_string(),
        );
    }
    Ok(())
}

/// Start the proxy once and return an authentication-bearing URL for the
/// child environment. The random credential is created once per Grab process.
pub(crate) fn ensure_started() -> Result<String, String> {
    validate_proxy_compatibility()?;
    if let Some(state) = PROXY_STATE.get() {
        return Ok(state.proxy_url());
    }
    let listener = StdTcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| format!("couldn't start the guarded extractor proxy: {e}"))?;
    start_proxy_listener(listener, &PROXY_STATE, random_proxy_token()?)
}

/// Register and serve a listener under Grab's process-lifetime runtime. The
/// state parameter lets tests exercise isolated initialization state too.
fn start_proxy_listener(
    listener: StdTcpListener,
    state: &OnceLock<ProxyState>,
    token: String,
) -> Result<String, String> {
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("couldn't configure the guarded extractor proxy: {e}"))?;
    let addr = listener
        .local_addr()
        .map_err(|e| format!("couldn't read guarded proxy address: {e}"))?;
    let expected_authorization = proxy_authorization_header(&token);
    let runtime = crate::runtime::tokio_rt();
    let listener = {
        let _entered = runtime.enter();
        TcpListener::from_std(listener)
            .map_err(|e| format!("couldn't start the guarded extractor proxy: {e}"))?
    };

    match state.set(ProxyState {
        addr,
        token,
        expected_authorization,
    }) {
        Ok(()) => {
            let expected_authorization = state
                .get()
                .map(|state| state.expected_authorization.clone())
                .ok_or_else(|| "guarded proxy state was not initialized".to_string())?;
            runtime.spawn(async move {
                loop {
                    let permit = match Arc::clone(&PROXY_CONNECTIONS).acquire_owned().await {
                        Ok(permit) => permit,
                        Err(_) => return,
                    };
                    match listener.accept().await {
                        Ok((stream, _peer)) => {
                            let expected_authorization = expected_authorization.clone();
                            tokio::spawn(async move {
                                let _permit = permit;
                                if let Err(e) =
                                    serve_connection(stream, &expected_authorization).await
                                {
                                    tracing::debug!(error = %e, "guarded extractor proxy request failed");
                                }
                            });
                        }
                        Err(e) => {
                            drop(permit);
                            tracing::warn!(error = %e, "guarded extractor proxy accept failed");
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    }
                }
            });
            state
                .get()
                .map(ProxyState::proxy_url)
                .ok_or_else(|| "guarded proxy state was not initialized".to_string())
        }
        Err(_) => state
            .get()
            .map(ProxyState::proxy_url)
            .ok_or_else(|| "guarded proxy state was not initialized".to_string()),
    }
}

async fn read_header(stream: &mut TcpStream) -> std::io::Result<(Vec<u8>, Vec<u8>)> {
    let mut received = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let count = stream.read(&mut chunk).await?;
        if count == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "proxy client closed before request headers",
            ));
        }
        received.extend_from_slice(&chunk[..count]);
        if let Some(end) = received
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|position| position + 4)
        {
            if end > MAX_HEADER_BYTES {
                break;
            }
            let trailing = received.split_off(end);
            return Ok((received, trailing));
        }
        if received.len() >= MAX_HEADER_BYTES {
            break;
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "proxy request headers exceed limit",
    ))
}

async fn read_header_with_timeout(
    stream: &mut TcpStream,
    timeout: Duration,
) -> io::Result<(Vec<u8>, Vec<u8>)> {
    match tokio::time::timeout(timeout, read_header(stream)).await {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "proxy request headers timed out",
        )),
    }
}

fn parse_authority(value: &str, default_port: u16) -> Option<(String, u16)> {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = rest[..end].to_string();
        let tail = &rest[end + 1..];
        let port = if tail.is_empty() {
            default_port
        } else {
            tail.strip_prefix(':')?.parse().ok()?
        };
        return (!host.is_empty() && port != 0).then_some((host, port));
    }
    if let Some((host, raw_port)) = value.rsplit_once(':') {
        if host.contains(':') {
            return None; // IPv6 literals in authorities must be bracketed.
        }
        let port = raw_port.parse::<u16>().ok()?;
        return (!host.is_empty() && port != 0).then_some((host.to_string(), port));
    }
    (!value.is_empty() && default_port != 0).then_some((value.to_string(), default_port))
}

/// Resolve and connect to a public address under a total DNS/connect deadline.
async fn connect_public_with_timeout(host: &str, port: u16) -> io::Result<TcpStream> {
    connect_with_timeout(connect_public(host, port), CONNECT_TIMEOUT).await
}

async fn connect_with_timeout<F>(future: F, timeout: Duration) -> io::Result<TcpStream>
where
    F: std::future::Future<Output = io::Result<TcpStream>>,
{
    tokio::time::timeout(timeout, future).await.map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "guarded proxy DNS/connect timed out",
        )
    })?
}

/// Resolve and connect to a public address. The socket connects to the exact
/// IP selected here, never re-resolving the untrusted hostname. The caller
/// bounds the overall DNS/connect attempt.
async fn connect_public(host: &str, port: u16) -> std::io::Result<TcpStream> {
    use std::io::{Error, ErrorKind};

    // url::Url::host_str may include brackets for IPv6 URLs; CONNECT
    // authority parsing already removes them. Normalize both call paths.
    let host = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return if crate::download_net::is_private_ip(ip) {
            Err(Error::new(
                ErrorKind::PermissionDenied,
                "private address refused",
            ))
        } else {
            connect_address(SocketAddr::new(ip, port)).await
        };
    }

    let addrs = tokio::net::lookup_host((host, port)).await?;
    let mut saw_public = false;
    let mut last_error = None;
    for addr in addrs {
        if crate::download_net::is_private_ip(addr.ip()) {
            continue;
        }
        saw_public = true;
        match connect_address(addr).await {
            Ok(stream) => return Ok(stream),
            Err(e) => last_error = Some(e),
        }
    }
    if !saw_public {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            "destination resolved only to private or special-use addresses",
        ));
    }
    Err(last_error.unwrap_or_else(|| Error::new(ErrorKind::AddrNotAvailable, "no usable address")))
}

async fn connect_address(addr: SocketAddr) -> std::io::Result<TcpStream> {
    tokio::time::timeout(ADDRESS_CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "per-address connect timed out",
            )
        })?
}

async fn reply_status(stream: &mut TcpStream, code: u16, reason: &str) {
    let response =
        format!("HTTP/1.1 {code} {reason}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
    let _ = stream.write_all(response.as_bytes()).await;
}

async fn reply_proxy_auth_required(stream: &mut TcpStream) -> io::Result<()> {
    stream
        .write_all(
            b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"Grab guarded extractor proxy\"\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        )
        .await
}

async fn serve_connection(
    mut client: TcpStream,
    expected_authorization: &str,
) -> std::io::Result<()> {
    let (header, trailing) = match read_header_with_timeout(&mut client, HEADER_TIMEOUT).await {
        Ok(parts) => parts,
        Err(error) => {
            if error.kind() == io::ErrorKind::InvalidData {
                reply_status(&mut client, 431, "Request Header Fields Too Large").await;
            } else if error.kind() == io::ErrorKind::TimedOut {
                reply_status(&mut client, 408, "Request Timeout").await;
            }
            return Err(error);
        }
    };
    if !proxy_authorization_matches(&header, expected_authorization) {
        reply_proxy_auth_required(&mut client).await?;
        return Ok(());
    }
    let text = String::from_utf8_lossy(&header);
    let mut lines = text.split("\r\n");
    let Some(request_line) = lines.next() else {
        reply_status(&mut client, 400, "Bad Request").await;
        return Ok(());
    };
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        reply_status(&mut client, 400, "Bad Request").await;
        return Ok(());
    };

    if method.eq_ignore_ascii_case("CONNECT") {
        let Some((host, port)) = parse_authority(target, 443) else {
            reply_status(&mut client, 400, "Bad CONNECT Authority").await;
            return Ok(());
        };
        let mut upstream = match connect_public_with_timeout(&host, port).await {
            Ok(stream) => stream,
            Err(e) => {
                tracing::debug!(host, port, error = %e, "guarded proxy refused or timed out connecting to CONNECT destination");
                if e.kind() == io::ErrorKind::TimedOut {
                    reply_status(&mut client, 504, "Gateway Timeout").await;
                } else {
                    reply_status(&mut client, 403, "Destination Refused").await;
                }
                return Ok(());
            }
        };
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        if !trailing.is_empty() {
            upstream.write_all(&trailing).await?;
        }
        let _ =
            copy_bidirectional_with_idle_timeout(&mut client, &mut upstream, TUNNEL_IDLE_TIMEOUT)
                .await;
        return Ok(());
    }

    let parsed = match url::Url::parse(target) {
        Ok(url) if url.scheme() == "http" && url.host_str().is_some() => url,
        _ => {
            reply_status(&mut client, 400, "Absolute HTTP URL Required").await;
            return Ok(());
        }
    };
    let host = parsed.host_str().unwrap_or_default().to_string();
    let port = parsed.port_or_known_default().unwrap_or(80);
    let mut upstream = match connect_public_with_timeout(&host, port).await {
        Ok(stream) => stream,
        Err(e) => {
            tracing::debug!(host, port, error = %e, "guarded proxy refused or timed out connecting to HTTP destination");
            if e.kind() == io::ErrorKind::TimedOut {
                reply_status(&mut client, 504, "Gateway Timeout").await;
            } else {
                reply_status(&mut client, 403, "Destination Refused").await;
            }
            return Ok(());
        }
    };

    let path = match parsed.query() {
        Some(query) => format!("{}?{query}", parsed.path()),
        None => parsed.path().to_string(),
    };
    let path = if path.is_empty() { "/" } else { &path };
    let mut forwarded = format!("{method} {path} {version}\r\n");
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, _)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("connection")
            || name.eq_ignore_ascii_case("proxy-connection")
            || name.eq_ignore_ascii_case("proxy-authorization")
        {
            continue;
        }
        forwarded.push_str(line);
        forwarded.push_str("\r\n");
    }
    forwarded.push_str("Connection: close\r\n\r\n");
    upstream.write_all(forwarded.as_bytes()).await?;
    if !trailing.is_empty() {
        upstream.write_all(&trailing).await?;
    }
    let _ =
        copy_bidirectional_with_idle_timeout(&mut client, &mut upstream, TUNNEL_IDLE_TIMEOUT).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_started_from_short_lived_runtime_survives_runtime_drop() {
        // Local state makes the test independent of other tests that might
        // initialize the process-wide proxy before this test runs.
        let proxy_url = std::thread::spawn(|| {
            let caller_runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("short-lived caller runtime");
            let state = OnceLock::new();
            caller_runtime
                .block_on(async {
                    let listener = StdTcpListener::bind(("127.0.0.1", 0)).unwrap();
                    start_proxy_listener(listener, &state, "test-token".to_string())
                })
                .expect("start guarded proxy")
        })
        .join()
        .expect("caller thread");
        let parsed = url::Url::parse(&proxy_url).expect("proxy URL");
        assert_eq!(parsed.username(), "grab");
        assert_eq!(parsed.password(), Some("test-token"));
        let address: SocketAddr = format!(
            "{}:{}",
            parsed.host_str().expect("proxy host"),
            parsed.port().expect("proxy port")
        )
        .parse()
        .expect("proxy address");

        let response = crate::runtime::tokio_rt()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(2), async {
                    let mut client = TcpStream::connect(address).await?;
                    client
                        .write_all(b"CONNECT 127.0.0.1:9 HTTP/1.1\r\nHost: 127.0.0.1:9\r\n\r\n")
                        .await?;
                    let mut response = Vec::new();
                    client.read_to_end(&mut response).await?;
                    Ok::<_, io::Error>(response)
                })
                .await
                .expect("proxy must process a request after caller runtime exits")
            })
            .expect("proxy connection and request");

        assert!(
            String::from_utf8_lossy(&response).starts_with("HTTP/1.1 407"),
            "proxy should require authentication and survive caller runtime shutdown: {}",
            String::from_utf8_lossy(&response)
        );
    }

    #[test]
    fn proxy_connection_semaphore_caps_active_clients() {
        let semaphore = Arc::clone(&PROXY_CONNECTIONS);
        let permit = semaphore
            .clone()
            .try_acquire_many_owned(MAX_PROXY_CONNECTIONS as u32)
            .expect("all connection slots start available");
        assert!(semaphore.try_acquire().is_err());
        drop(permit);
        assert!(semaphore.try_acquire().is_ok());
    }

    #[tokio::test]
    async fn proxy_dns_and_connect_attempt_timeout_is_enforced() {
        let error = connect_with_timeout(
            std::future::pending::<io::Result<TcpStream>>(),
            Duration::from_millis(20),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn proxy_header_timeout_releases_a_client_that_sends_nothing() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_header_with_timeout(&mut stream, Duration::from_millis(30)).await
        });
        let _client = TcpStream::connect(addr).await.unwrap();
        let error = server.await.unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn proxy_tunnel_idle_timeout_releases_quiet_sockets() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(addr).await.unwrap();
        let (mut upstream, _) = listener.accept().await.unwrap();
        let error = copy_bidirectional_with_idle_timeout(
            &mut client,
            &mut upstream,
            Duration::from_millis(40),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn parses_ipv4_and_bracketed_ipv6_authorities() {
        assert_eq!(
            parse_authority("cdn.example:8443", 443),
            Some(("cdn.example".to_string(), 8443))
        );
        assert_eq!(
            parse_authority("[2001:4860:4860::8888]:443", 80),
            Some(("2001:4860:4860::8888".to_string(), 443))
        );
        assert_eq!(parse_authority("[::1]junk", 443), None);
        assert_eq!(parse_authority("2001:4860::1:443", 443), None);
    }

    #[tokio::test]
    async fn header_reader_preserves_bytes_after_the_header() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (read_tx, read_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_rx.await.unwrap();
            read_header(&mut stream).await.unwrap()
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"POST http://example.com/upload HTTP/1.1\r\nContent-Length: 4\r\n\r\nbody")
            .await
            .unwrap();
        read_tx.send(()).unwrap();

        let (header, trailing) = server.await.unwrap();
        assert!(header.ends_with(b"\r\n\r\n"));
        assert_eq!(trailing, b"body");
    }

    #[tokio::test]
    async fn proxy_rejects_private_connect_destinations() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let expected_authorization = proxy_authorization_header("test-token");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_connection(stream, &expected_authorization).await.unwrap();
        });

        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        let request = format!(
            "CONNECT 127.0.0.1:9 HTTP/1.1\r\nHost: 127.0.0.1:9\r\nProxy-Authorization: {}\r\n\r\n",
            proxy_authorization_header("test-token")
        );
        client.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert!(
            String::from_utf8_lossy(&response).starts_with("HTTP/1.1 403"),
            "proxy must reject private CONNECT targets: {}",
            String::from_utf8_lossy(&response)
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn refuses_private_ip_literals_before_connecting() {
        for host in ["127.0.0.1", "10.0.0.1", "::1", "[::1]", "fd00::1"] {
            let err = connect_public(host, 80).await.unwrap_err();
            assert_eq!(
                err.kind(),
                std::io::ErrorKind::PermissionDenied,
                "{host}: {err}"
            );
        }
        let err = connect_public("localhost", 80).await.unwrap_err();
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::PermissionDenied,
            "localhost: {err}"
        );
    }
}
