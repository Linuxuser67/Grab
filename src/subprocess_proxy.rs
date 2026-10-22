//! Loopback HTTP proxy for extractor subprocesses handling untrusted browser handoffs.
//!
//! yt-dlp/gallery-dl resolve DNS in their own processes, outside reqwest's
//! GuardedResolver. This proxy resolves each target itself, rejects private
//! and special-use addresses, then connects to the selected IP (not the name)
//! to avoid a DNS check/connect race. It supports HTTP CONNECT and ordinary
//! HTTP proxy requests. It is only enabled for browser-handoff hosts whose
//! public-resolution policy is active.

use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::sync::OnceLock;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

static PROXY_ADDR: OnceLock<SocketAddr> = OnceLock::new();
const MAX_HEADER_BYTES: usize = 64 * 1024;

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

/// Start the proxy once and return its HTTP proxy URL.
pub(crate) fn ensure_started() -> Result<String, String> {
    validate_proxy_compatibility()?;

    if let Some(addr) = PROXY_ADDR.get() {
        return Ok(format!("http://{addr}"));
    }

    let listener = StdTcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| format!("couldn't start the guarded extractor proxy: {e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("couldn't configure the guarded extractor proxy: {e}"))?;
    let addr = listener
        .local_addr()
        .map_err(|e| format!("couldn't read guarded proxy address: {e}"))?;
    let listener = TcpListener::from_std(listener)
        .map_err(|e| format!("couldn't start the guarded extractor proxy: {e}"))?;

    match PROXY_ADDR.set(addr) {
        Ok(()) => {
            // Spawn on the runtime currently creating the listener, so its I/O
            // driver and accept task have the same lifetime/context.
            tokio::spawn(async move {
                loop {
                    match listener.accept().await {
                        Ok((stream, _peer)) => {
                            tokio::spawn(async move {
                                if let Err(e) = serve_connection(stream).await {
                                    tracing::debug!(error = %e, "guarded extractor proxy request failed");
                                }
                            });
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "guarded extractor proxy accept failed");
                        }
                    }
                }
            });
            Ok(format!("http://{addr}"))
        }
        Err(_) => {
            // A concurrent caller won initialization. Its listener is active;
            // this one's listener is dropped without being spawned.
            Ok(format!(
                "http://{}",
                PROXY_ADDR.get().expect("proxy address initialized")
            ))
        }
    }
}

async fn read_header(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut header = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    while header.len() < MAX_HEADER_BYTES {
        if stream.read(&mut byte).await? == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "proxy client closed before request headers",
            ));
        }
        header.push(byte[0]);
        if header.ends_with(b"\r\n\r\n") {
            return Ok(header);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "proxy request headers exceed limit",
    ))
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

/// Resolve and connect to a public address. The socket connects to the exact
/// IP selected here, never re-resolving the untrusted hostname.
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
            TcpStream::connect(SocketAddr::new(ip, port)).await
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
        match TcpStream::connect(addr).await {
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

async fn reply_status(stream: &mut TcpStream, code: u16, reason: &str) {
    let response =
        format!("HTTP/1.1 {code} {reason}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
    let _ = stream.write_all(response.as_bytes()).await;
}

async fn serve_connection(mut client: TcpStream) -> std::io::Result<()> {
    let header = match read_header(&mut client).await {
        Ok(header) => header,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::InvalidData {
                reply_status(&mut client, 431, "Request Header Fields Too Large").await;
            }
            return Err(e);
        }
    };
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
        let mut upstream = match connect_public(&host, port).await {
            Ok(stream) => stream,
            Err(e) => {
                tracing::debug!(host, port, error = %e, "guarded proxy refused CONNECT destination");
                reply_status(&mut client, 403, "Destination Refused").await;
                return Ok(());
            }
        };
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
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
    let mut upstream = match connect_public(&host, port).await {
        Ok(stream) => stream,
        Err(e) => {
            tracing::debug!(host, port, error = %e, "guarded proxy refused HTTP destination");
            reply_status(&mut client, 403, "Destination Refused").await;
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
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    async fn proxy_rejects_private_connect_destinations() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_connection(stream).await.unwrap();
        });

        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        client
            .write_all(b"CONNECT 127.0.0.1:9 HTTP/1.1\r\nHost: 127.0.0.1:9\r\n\r\n")
            .await
            .unwrap();
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
