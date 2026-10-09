//! Local HTTP server for browser extension integration.
//!
//! Replaces native messaging: the extension POSTs URLs to
//! `http://127.0.0.1:9412/add` instead of using a native messaging host.
//! No manifest installation, no extension IDs, works from Flatpak.

use axum::{Json, Router, routing::post};
use serde::Deserialize;
use std::net::SocketAddr;

const BIND_ADDR: &str = "127.0.0.1";
const PORT: u16 = 9412;

#[derive(Deserialize)]
struct AddRequest {
    url: String,
}

/// Start the extension integration server. Runs until the process exits.
pub async fn serve() {
    let app = Router::new().route("/add", post(handle_add));

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

async fn handle_add(Json(req): Json<AddRequest>) -> axum::http::StatusCode {
    tracing::info!("Extension handoff: {}", req.url);
    // TODO: enqueue the URL in the download manager.
    // Return 501 so the extension falls back to grab:// until wired up.
    axum::http::StatusCode::NOT_IMPLEMENTED
}
