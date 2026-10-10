//! Local HTTP server for browser extension integration.
//!
//! Replaces native messaging: the extension POSTs URLs to
//! `http://127.0.0.1:9412/add` instead of using a native messaging host.
//! No manifest installation, no extension IDs, works from Flatpak.

use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use serde::Deserialize;
use std::net::SocketAddr;
use tokio::sync::mpsc::UnboundedSender;

const BIND_ADDR: &str = "127.0.0.1";
const PORT: u16 = 9412;

#[derive(Deserialize)]
struct AddRequest {
    url: String,
}

/// Start the extension integration server. `tx` carries URLs to the GTK
/// thread, which enqueues them in the download manager. Runs until the
/// process exits.
///
/// Binds to 127.0.0.1 only: any local process can POST, but nothing remote.
/// CSRF from websites is mitigated by CORS preflight (axum rejects the
/// `application/json` POST from web origins by default).
pub async fn serve(tx: UnboundedSender<String>) {
    let app = Router::new().route("/add", post(handle_add)).with_state(tx);

    let addr: SocketAddr = format!("{BIND_ADDR}:{PORT}")
        .parse()
        .expect("valid bind address");

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("Extension HTTP server couldn't bind {addr}: {e}");
            return;
        }
    };

    tracing::info!("Extension HTTP server listening on {addr}");

    if let Err(e) = axum::serve(listener, app).await {
        tracing::warn!("Extension HTTP server error: {e}");
    }
}

async fn handle_add(
    State(tx): State<UnboundedSender<String>>,
    Json(req): Json<AddRequest>,
) -> StatusCode {
    tracing::debug!("Extension handoff received");
    match tx.send(req.url) {
        Ok(()) => StatusCode::OK,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}
