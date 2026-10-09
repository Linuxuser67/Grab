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

    tracing::info!("Extension HTTP server listening on {addr}");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind extension server");

    axum::serve(listener, app)
        .await
        .expect("serve extension server");
}

async fn handle_add(Json(req): Json<AddRequest>) -> &'static str {
    tracing::info!("Extension handoff: {}", req.url);
    // TODO: enqueue the URL in the download manager.
    "ok"
}
