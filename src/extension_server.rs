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
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::mpsc::{Sender, error::TrySendError};

const PORT: u16 = 9412;
/// A URL plus JSON framing; the intake itself caps URLs at `download_intake::MAX_URL_LEN`.
const MAX_BODY_BYTES: usize = 8 * 1024;
/// Handoffs waiting for the GTK thread; beyond this the extension gets 429.
pub(crate) const QUEUE_DEPTH: usize = 16;

#[derive(Deserialize)]
struct AddRequest {
    url: String,
}

/// Start the extension integration server. `tx` carries URLs to the GTK
/// thread, which enqueues them in the download manager. Runs until the
/// process exits.
pub async fn serve(tx: Sender<String>) {
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

async fn serve_listener(listener: TcpListener, tx: Sender<String>) {
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

fn router(tx: Sender<String>, port: u16) -> Router {
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

async fn handle_add(State(tx): State<Sender<String>>, Json(req): Json<AddRequest>) -> StatusCode {
    tracing::debug!("Extension handoff received");
    if req.url.trim().is_empty() {
        return StatusCode::BAD_REQUEST;
    }
    if req.url.len() > crate::download_intake::MAX_URL_LEN {
        return StatusCode::URI_TOO_LONG;
    }
    match tx.try_send(req.url) {
        Ok(()) => StatusCode::OK,
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
        rx: Receiver<String>,
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
        assert_eq!(
            srv.rx.recv().await.as_deref(),
            Some("https://example.com/file.zip")
        );
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
}
