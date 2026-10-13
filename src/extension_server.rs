//! Local HTTP server for browser extension integration.
//!
//! Replaces native messaging: the extension POSTs URLs to
//! `http://127.0.0.1:9412/add` instead of using a native messaging host.
//! No manifest installation, no extension IDs, works from Flatpak.
//!
//! Threat model: the socket is loopback-only, but any web page can still try
//! to reach it. CORS preflight stops a plain cross-origin POST, yet not DNS
//! rebinding (the rebound page is same-origin, so no preflight and an
//! attacker-chosen `Host`). Every request therefore passes [`guard`]: the
//! `Host` must be the loopback name this server is bound to, and an `Origin`
//! (always sent by browsers on POST) must belong to a browser extension.
//! Bodies and queue depth are bounded so a flood cannot exhaust memory or
//! bury the user in dialogs.

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use tokio::net::TcpListener;
use tokio::sync::mpsc::{Sender, error::TrySendError};

const PORT: u16 = 9412;
/// A URL plus JSON framing; the intake itself caps URLs at `download_intake::MAX_URL_LEN`.
const MAX_BODY_BYTES: usize = 32 * 1024;
/// Handoffs waiting for the GTK thread; beyond this the extension gets 429.
pub(crate) const QUEUE_DEPTH: usize = 16;

#[derive(Deserialize)]
pub(crate) struct Cookie {
    pub name: String,
    pub value: String,
    /// Backwards compatible: the extension originally sent only name/value.
    /// When present, these preserve the browser's cookie scope so host-only
    /// cookies aren't silently widened to subdomains.
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub host_only: Option<bool>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub secure: Option<bool>,
    #[serde(default)]
    pub expiration_date: Option<f64>,
}

/// Cookie files handed off from the extension, keyed by canonical URL. Every
/// yt-dlp spawn for the URL peeks the entry (probe, subtitle probe and media
/// download each spawn separately, and every leg must pass the challenge);
/// the finished download discards it. Bounded so abandoned handoffs (a card
/// opened but never added, a direct file that never probes) can't pile up.
static HANDOFF_COOKIES: LazyLock<Mutex<HashMap<String, PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Cap on staged handoff entries; past it an arbitrary entry is evicted with
/// its file. Matches the handoff queue depth: at most that many handoffs can
/// be in flight ahead of the GTK thread.
const MAX_HANDOFF_ENTRIES: usize = QUEUE_DEPTH;
static COOKIE_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Canonical map key for a handoff URL: the `url::Url` serialization, so the
/// raw extension string and the intake-normalized form hit the same entry
/// (`https://example.com` and `https://example.com/` are one key).
fn handoff_key(url: &str) -> Option<String> {
    url::Url::parse(url).ok().map(|u| u.as_str().to_owned())
}

/// Write extension-provided cookies to a temp file for yt-dlp's `--cookies`.
/// Returns the path, or None if there are no cookies or the write fails.
pub(crate) fn write_handoff_cookies(url: &str, cookies: &[Cookie]) -> Option<PathBuf> {
    if cookies.is_empty() {
        return None;
    }
    // Netscape cookie file format.
    let mut content = String::from("# Netscape HTTP Cookie File\n");
    let fallback_domain = url::Url::parse(url).ok()?.host_str()?.to_string();
    let mut written = 0;
    for cookie in cookies {
        // Sanitize: no tabs/newlines in Netscape format.
        let n = cookie.name.replace(['\t', '\n', '\r'], "");
        let v = cookie.value.replace(['\t', '\n', '\r'], "");
        if n.is_empty() {
            continue;
        }
        // Preserve the browser's scope when the extension sends it; otherwise
        // fall back to the previous behavior (tailmatch on the URL host).
        // A leading dot marks subdomain cookies in the browser; the tailmatch
        // flag (field 2) is what yt-dlp actually honors, so strip it.
        let domain = cookie
            .domain
            .as_deref()
            .map(|d| d.strip_prefix('.').unwrap_or(d))
            .unwrap_or(&fallback_domain);
        let tailmatch = match cookie.host_only {
            Some(true) => "FALSE",
            // Old extension sent no attributes: keep the previous tailmatch
            // default so those handoffs keep working.
            _ => "TRUE",
        };
        let path = cookie.path.as_deref().unwrap_or("/");
        let secure = if cookie.secure.unwrap_or(false) {
            "TRUE"
        } else {
            "FALSE"
        };
        let expiry = cookie.expiration_date.map(|e| e as i64).unwrap_or(0);
        content.push_str(&format!(
            "{domain}\t{tailmatch}\t{path}\t{secure}\t{expiry}\t{n}\t{v}\n"
        ));
        written += 1;
    }
    if written == 0 {
        return None;
    }
    let mut path = std::env::temp_dir();
    let count = COOKIE_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    path.push(format!(
        "grab-handoff-cookies-{}-{}",
        std::process::id(),
        count
    ));
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut file = opts.open(&path).ok()?;
    file.write_all(content.as_bytes()).ok()?;
    Some(path)
}

/// Store a cookie file for a URL. Called by the extension handoff once the
/// handoff is accepted. Replaces any existing entry (deleting its file);
/// past [`MAX_HANDOFF_ENTRIES`] an arbitrary entry is evicted with its file.
pub(crate) fn store_handoff_cookies(url: &str, path: PathBuf) {
    let Some(key) = handoff_key(url) else { return };
    let Ok(mut map) = HANDOFF_COOKIES.lock() else {
        return;
    };
    if let Some(old) = map.insert(key.clone(), path) {
        let _ = std::fs::remove_file(old);
        return;
    }
    if map.len() > MAX_HANDOFF_ENTRIES
        && let Some(evict) = map.keys().find(|k| *k != &key).cloned()
        && let Some(old) = map.remove(&evict)
    {
        let _ = std::fs::remove_file(old);
    }
}

/// `--cookies` argv for a URL's staged handoff cookies, if any. Peeks: the
/// entry stays staged until the download finishes (or is evicted), because
/// the probe, the subtitle probe and the media download each spawn yt-dlp
/// separately and every leg must pass the challenge. Add BEFORE
/// `ytdlp_identity_args` — that helper terminates argv with `-- <url>`, past
/// which yt-dlp reads positionals.
pub(crate) fn handoff_cookie_args(url: &str) -> Vec<String> {
    let path = handoff_key(url).and_then(|key| {
        HANDOFF_COOKIES
            .lock()
            .ok()
            .and_then(|map| map.get(&key).cloned())
    });
    match path {
        Some(p) => vec!["--cookies".to_string(), p.to_string_lossy().into_owned()],
        None => Vec::new(),
    }
}

/// Drop a URL's staged handoff cookies and delete the file. Called when its
/// download finishes; failures and aborts keep the entry (bounded) so a retry
/// or resume still passes the challenge.
pub(crate) fn discard_handoff_cookies(url: &str) {
    let path = handoff_key(url).and_then(|key| {
        HANDOFF_COOKIES
            .lock()
            .ok()
            .and_then(|mut map| map.remove(&key))
    });
    if let Some(p) = path {
        let _ = std::fs::remove_file(p);
    }
}

#[derive(Deserialize)]
struct AddRequest {
    url: String,
    #[serde(default)]
    cookies: Vec<Cookie>,
}

/// A URL handed off from the extension. Cookies (if any) are stored
/// separately in HANDOFF_COOKIES, keyed by canonical URL, for the video
/// probe and every yt-dlp download leg.
pub(crate) struct Handoff {
    pub url: String,
}

/// Start the extension integration server. `tx` carries handoffs to the GTK
/// thread, which enqueues them in the download manager. Runs until the
/// process exits.
pub async fn serve(tx: Sender<Handoff>) {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, PORT));
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("Extension HTTP server couldn't bind {addr}: {e}");
            return;
        }
    };
    tracing::info!("Extension HTTP server listening on {addr}");
    serve_listener(listener, tx).await;
}

async fn serve_listener(listener: TcpListener, tx: Sender<Handoff>) {
    let port = match listener.local_addr() {
        Ok(a) => a.port(),
        Err(e) => {
            tracing::warn!("Extension HTTP server has no local address: {e}");
            return;
        }
    };
    if let Err(e) = axum::serve(listener, router(tx, port)).await {
        tracing::warn!("Extension HTTP server error: {e}");
    }
}

fn router(tx: Sender<Handoff>, port: u16) -> Router {
    let hosts: Arc<[String]> =
        Arc::from([format!("127.0.0.1:{port}"), format!("localhost:{port}")]);
    Router::new()
        .route("/add", post(handle_add))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        // Outermost: rejected before the body is read.
        .layer(middleware::from_fn_with_state(hosts, guard))
        .with_state(tx)
}

/// Reject requests that are not addressed to this server by name or that a
/// web page (rather than an extension) originated.
async fn guard(State(hosts): State<Arc<[String]>>, req: Request, next: Next) -> Response {
    if host_allowed(req.headers(), &hosts) && origin_allowed(req.headers()) {
        next.run(req).await
    } else {
        StatusCode::FORBIDDEN.into_response()
    }
}

fn host_allowed(headers: &HeaderMap, hosts: &[String]) -> bool {
    let mut values = headers.get_all(header::HOST).iter();
    match (values.next(), values.next()) {
        (Some(v), None) => v
            .to_str()
            .is_ok_and(|v| hosts.iter().any(|h| h.eq_ignore_ascii_case(v))),
        _ => false,
    }
}

/// No `Origin` means not a browser request (curl, scripts): the same trust as
/// any local process. A present `Origin` must be a browser extension's.
fn origin_allowed(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(header::ORIGIN).iter();
    match (values.next(), values.next()) {
        (None, _) => true,
        (Some(v), None) => v.to_str().is_ok_and(is_extension_origin),
        _ => false,
    }
}

fn is_extension_origin(origin: &str) -> bool {
    ["moz-extension://", "chrome-extension://"]
        .iter()
        .filter_map(|scheme| origin.strip_prefix(scheme))
        .any(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

async fn handle_add(State(tx): State<Sender<Handoff>>, Json(req): Json<AddRequest>) -> StatusCode {
    tracing::debug!("Extension handoff received");
    if req.url.trim().is_empty() {
        return StatusCode::BAD_REQUEST;
    }
    if req.url.len() > crate::download_intake::MAX_URL_LEN {
        return StatusCode::URI_TOO_LONG;
    }
    // Bound cookie count: each is a small name/value pair.
    if req.cookies.len() > 100 {
        return StatusCode::BAD_REQUEST;
    }
    let handoff = Handoff {
        url: req.url.clone(),
    };
    match tx.try_send(handoff) {
        Ok(()) => {
            // Stage the cookies only for an accepted handoff; the video probe
            // and every download leg pick them up by URL.
            if let Some(path) = write_handoff_cookies(&req.url, &req.cookies) {
                store_handoff_cookies(&req.url, path);
            }
            StatusCode::OK
        }
        Err(TrySendError::Full(_)) => StatusCode::TOO_MANY_REQUESTS,
        Err(TrySendError::Closed(_)) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc::{Receiver, channel};

    struct Server {
        port: u16,
        rx: Receiver<Handoff>,
    }

    async fn start(depth: usize) -> Server {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = channel(depth);
        tokio::spawn(serve_listener(listener, tx));
        Server { port, rx }
    }

    /// One request on a fresh connection; returns the raw response.
    async fn request(port: u16, headers: &[(&str, &str)], body: &str) -> String {
        let mut req = String::from("POST /add HTTP/1.1\r\nConnection: close\r\n");
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
        let mut s = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out).await;
        String::from_utf8_lossy(&out).into_owned()
    }

    fn status(resp: &str) -> &str {
        resp.split_whitespace().nth(1).unwrap_or("")
    }

    const JSON: (&str, &str) = ("Content-Type", "application/json");
    const BODY: &str = r#"{"url":"https://example.com/file.zip"}"#;

    #[tokio::test]
    async fn accepts_loopback_host_without_origin() {
        let mut srv = start(4).await;
        let host = format!("127.0.0.1:{}", srv.port);
        let r = request(srv.port, &[("Host", &host), JSON], BODY).await;
        assert_eq!(status(&r), "200", "{r}");
        let handoff = srv.rx.recv().await.expect("handoff");
        assert_eq!(handoff.url, "https://example.com/file.zip");
    }

    #[tokio::test]
    async fn accepts_localhost_name_and_extension_origins() {
        let mut srv = start(4).await;
        let host = format!("localhost:{}", srv.port);
        for origin in [
            "moz-extension://8f1c2a4e-1b2c-4d5e-9f00-aabbccddeeff",
            "chrome-extension://abcdefghijklmnopabcdefghijklmnop",
        ] {
            let r = request(srv.port, &[("Host", &host), ("Origin", origin), JSON], BODY).await;
            assert_eq!(status(&r), "200", "{origin}: {r}");
            assert!(srv.rx.recv().await.is_some());
        }
    }

    #[tokio::test]
    async fn rejects_dns_rebinding_host() {
        let mut srv = start(4).await;
        for host in [
            format!("evil.example:{}", srv.port),
            "127.0.0.1".to_string(),
            format!("127.0.0.1:{}", srv.port + 1),
            format!("127.0.0.1.evil.example:{}", srv.port),
        ] {
            let r = request(srv.port, &[("Host", &host), JSON], BODY).await;
            assert_eq!(status(&r), "403", "{host}: {r}");
        }
        assert!(srv.rx.try_recv().is_err(), "nothing may reach the queue");
    }

    #[tokio::test]
    async fn rejects_web_and_malformed_origins() {
        let mut srv = start(4).await;
        let host = format!("127.0.0.1:{}", srv.port);
        for origin in [
            "https://evil.example",
            "null",
            "http://127.0.0.1:9412",
            "moz-extension://",
            "moz-extension://evil.example/x",
            "chrome-extension://id with space",
        ] {
            let r = request(srv.port, &[("Host", &host), ("Origin", origin), JSON], BODY).await;
            assert_eq!(status(&r), "403", "{origin}: {r}");
        }
        // Two Origin headers are ambiguous: refuse.
        let r = request(
            srv.port,
            &[
                ("Host", &host),
                ("Origin", "moz-extension://abc"),
                ("Origin", "https://evil.example"),
                JSON,
            ],
            BODY,
        )
        .await;
        assert_eq!(status(&r), "403", "{r}");
        assert!(srv.rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn rejects_non_json_oversized_and_bad_urls() {
        let mut srv = start(4).await;
        let host = format!("127.0.0.1:{}", srv.port);
        let h = [("Host", host.as_str())];

        let r = request(srv.port, &[h[0], ("Content-Type", "text/plain")], BODY).await;
        assert_eq!(status(&r), "415", "{r}");

        let big = format!(
            r#"{{"url":"https://example.com/{}"}}"#,
            "a".repeat(MAX_BODY_BYTES)
        );
        let r = request(srv.port, &[h[0], JSON], &big).await;
        assert_eq!(status(&r), "413", "{r}");

        let long = format!(
            r#"{{"url":"https://example.com/{}"}}"#,
            "a".repeat(crate::download_intake::MAX_URL_LEN)
        );
        let r = request(srv.port, &[h[0], JSON], &long).await;
        assert_eq!(status(&r), "414", "{r}");

        let r = request(srv.port, &[h[0], JSON], r#"{"url":"   "}"#).await;
        assert_eq!(status(&r), "400", "{r}");
        assert!(srv.rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn full_queue_answers_429_instead_of_growing() {
        let mut srv = start(1).await;
        let host = format!("127.0.0.1:{}", srv.port);
        let r = request(srv.port, &[("Host", &host), JSON], BODY).await;
        assert_eq!(status(&r), "200", "{r}");
        let r = request(srv.port, &[("Host", &host), JSON], BODY).await;
        assert_eq!(status(&r), "429", "{r}");
        assert!(srv.rx.recv().await.is_some());
        let r = request(srv.port, &[("Host", &host), JSON], BODY).await;
        assert_eq!(status(&r), "200", "queue drains: {r}");
    }

    /// Clear staged entries and delete their files: keeps map tests
    /// independent of each other.
    fn clear_handoff_cookies() {
        let paths: Vec<PathBuf> = HANDOFF_COOKIES
            .lock()
            .map(|mut map| map.drain().map(|(_, p)| p).collect())
            .unwrap_or_default();
        for p in paths {
            let _ = std::fs::remove_file(p);
        }
    }

    /// Minimal cookie as the original extension sent it (name/value only).
    fn cookie(name: &str, value: &str) -> Cookie {
        Cookie {
            name: name.to_string(),
            value: value.to_string(),
            domain: None,
            host_only: None,
            path: None,
            secure: None,
            expiration_date: None,
        }
    }

    #[test]
    fn write_handoff_cookies_netscape_format() {
        let cookies = vec![
            cookie("session", "abc123"),
            // Tabs/newlines would corrupt the tab-separated format.
            cookie("we\trd", "a\nb\rc"),
            // Empty names carry nothing.
            cookie("", "skip"),
        ];
        let path = write_handoff_cookies("https://example.com/watch?v=1", &cookies).expect("path");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "cookie file must be owner-only");
        }
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("# Netscape HTTP Cookie File"));
        let rows: Vec<&str> = lines.collect();
        assert_eq!(
            rows,
            [
                "example.com\tTRUE\t/\tFALSE\t0\tsession\tabc123",
                "example.com\tTRUE\t/\tFALSE\t0\twerd\tabc",
            ],
            "{text}"
        );
    }

    #[test]
    fn write_handoff_cookies_rejects_empty_and_bad_url() {
        assert!(write_handoff_cookies("https://example.com/", &[]).is_none());
        assert!(write_handoff_cookies("not a url", &[cookie("a", "b")]).is_none());
    }

    #[test]
    fn write_handoff_cookies_preserves_browser_scope() {
        // Host-only cookie: must not widen to subdomains.
        let cookies = vec![Cookie {
            name: "sess".to_string(),
            value: "s3cr3t".to_string(),
            domain: Some("videos.example.com".to_string()),
            host_only: Some(true),
            path: Some("/watch".to_string()),
            secure: Some(true),
            expiration_date: Some(1893456000.0),
        }];
        let path =
            write_handoff_cookies("https://videos.example.com/watch/1", &cookies).expect("path");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let rows: Vec<&str> = text.lines().skip(1).collect();
        assert_eq!(
            rows,
            ["videos.example.com\tFALSE\t/watch\tTRUE\t1893456000\tsess\ts3cr3t"],
            "{text}"
        );

        // Subdomain cookie with leading dot: tailmatch, dot stripped.
        let cookies = vec![Cookie {
            name: "prefs".to_string(),
            value: "dark".to_string(),
            domain: Some(".example.com".to_string()),
            host_only: Some(false),
            path: None,
            secure: None,
            expiration_date: None,
        }];
        let path = write_handoff_cookies("https://www.example.com/", &cookies).expect("path");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let rows: Vec<&str> = text.lines().skip(1).collect();
        assert_eq!(
            rows,
            ["example.com\tTRUE\t/\tFALSE\t0\tprefs\tdark"],
            "{text}"
        );
    }

    #[test]
    fn handoff_cookie_args_match_canonical_url_forms() {
        clear_handoff_cookies();
        // Stored under the bare domain; the intake hands the probe the
        // slash-terminated canonical form. One entry must serve both.
        let path = write_handoff_cookies("https://example.com", &[cookie("c", "v")]).expect("path");
        store_handoff_cookies("https://example.com", path.clone());
        for url in ["https://example.com", "https://example.com/"] {
            let args = handoff_cookie_args(url);
            assert_eq!(args.len(), 2, "{url}: {args:?}");
            assert_eq!(args[0], "--cookies");
            assert_eq!(args[1], path.to_string_lossy());
        }
        assert!(handoff_cookie_args("https://other.example/").is_empty());
        assert!(handoff_cookie_args("https://example.com/other").is_empty());
        clear_handoff_cookies();
    }

    #[test]
    fn discard_handoff_cookies_removes_entry_and_file() {
        clear_handoff_cookies();
        let path =
            write_handoff_cookies("https://example.com/item", &[cookie("c", "v")]).expect("path");
        store_handoff_cookies("https://example.com/item", path.clone());
        assert!(path.exists());
        discard_handoff_cookies("https://example.com/item");
        assert!(!path.exists(), "file must be deleted");
        assert!(
            handoff_cookie_args("https://example.com/item").is_empty(),
            "entry must be gone"
        );
        clear_handoff_cookies();
    }

    #[test]
    fn store_handoff_cookies_evicts_past_cap() {
        clear_handoff_cookies();
        let mut paths = Vec::new();
        for i in 0..=(MAX_HANDOFF_ENTRIES + 2) {
            let url = format!("https://example.com/evict-{i}");
            let path = write_handoff_cookies(&url, &[cookie("c", "v")]).expect("path");
            store_handoff_cookies(&url, path.clone());
            paths.push(path);
        }
        let len = HANDOFF_COOKIES.lock().unwrap().len();
        assert_eq!(len, MAX_HANDOFF_ENTRIES, "map must stay bounded");
        // Eviction is arbitrary, so assert on the count: every evicted
        // entry's file must be deleted, staged ones must survive.
        let surviving = paths.iter().filter(|p| p.exists()).count();
        assert_eq!(
            surviving, MAX_HANDOFF_ENTRIES,
            "evicted entries' files must be deleted"
        );
        clear_handoff_cookies();
    }

    #[tokio::test]
    async fn handle_add_with_cookies_stages_file_for_probe() {
        clear_handoff_cookies();
        let mut srv = start(4).await;
        let host = format!("127.0.0.1:{}", srv.port);
        let body =
            r#"{"url":"https://example.com/watch?v=7","cookies":[{"name":"cf","value":"abc"}]}"#;
        let r = request(srv.port, &[("Host", &host), JSON], body).await;
        assert_eq!(status(&r), "200", "{r}");
        let handoff = srv.rx.recv().await.expect("handoff");
        assert_eq!(handoff.url, "https://example.com/watch?v=7");
        // The probe (and later the download legs) resolve the same args.
        let args = handoff_cookie_args(&handoff.url);
        assert_eq!(args.len(), 2, "{args:?}");
        assert_eq!(args[0], "--cookies");
        let staged = PathBuf::from(&args[1]);
        assert!(staged.exists(), "staged file must exist");
        let text = std::fs::read_to_string(&staged).unwrap();
        assert!(
            text.contains("example.com\tTRUE\t/\tFALSE\t0\tcf\tabc"),
            "{text}"
        );
        clear_handoff_cookies();
    }

    #[tokio::test]
    async fn handle_add_rejects_too_many_cookies() {
        clear_handoff_cookies();
        let mut srv = start(4).await;
        let host = format!("127.0.0.1:{}", srv.port);
        let cookies: Vec<String> = (0..101)
            .map(|i| format!(r#"{{"name":"c{i}","value":"v"}}"#))
            .collect();
        let body = format!(
            r#"{{"url":"https://example.com/","cookies":[{}]}}"#,
            cookies.join(",")
        );
        let r = request(srv.port, &[("Host", &host), JSON], &body).await;
        assert_eq!(status(&r), "400", "{r}");
        assert!(srv.rx.try_recv().is_err());
        assert!(
            handoff_cookie_args("https://example.com/").is_empty(),
            "rejected handoff must stage nothing"
        );
        clear_handoff_cookies();
    }

    #[tokio::test]
    async fn handle_add_429_stages_nothing() {
        clear_handoff_cookies();
        let srv = start(1).await;
        let host = format!("127.0.0.1:{}", srv.port);
        let body = r#"{"url":"https://example.com/a","cookies":[{"name":"c","value":"v"}]}"#;
        let r = request(srv.port, &[("Host", &host), JSON], body).await;
        assert_eq!(status(&r), "200", "{r}");
        // Queue full: the handoff is refused, so its cookies must not linger.
        let body2 = r#"{"url":"https://example.com/b","cookies":[{"name":"c","value":"v"}]}"#;
        let r = request(srv.port, &[("Host", &host), JSON], body2).await;
        assert_eq!(status(&r), "429", "{r}");
        assert!(
            handoff_cookie_args("https://example.com/b").is_empty(),
            "refused handoff must stage nothing"
        );
        clear_handoff_cookies();
    }
}
