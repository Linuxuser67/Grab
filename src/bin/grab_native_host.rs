//! Grab native messaging host.
//!
//! Spawned by the browser (via the manifest installed by
//! `grab --install-browser-host`). Speaks the Native Messaging stdio
//! protocol: 4-byte little-endian length prefix + UTF-8 JSON, both directions.
//!
//! Only `grab://` URLs are accepted; anything else is rejected without
//! executing anything. The URL is handed to the OS URI opener as a single
//! argv element, never through a shell.

use std::io::{self, Read, Write};
use std::process::Command;

const GRAB_SCHEME: &str = "grab://";
const MAX_MESSAGE: usize = 1024 * 1024;

fn read_message() -> Option<serde_json::Value> {
    let mut len_buf = [0u8; 4];
    io::stdin().read_exact(&mut len_buf).ok()?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 || len > MAX_MESSAGE {
        return None;
    }
    let mut buf = vec![0u8; len];
    io::stdin().read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

fn write_message(payload: &serde_json::Value) {
    let raw = serde_json::to_vec(payload).unwrap_or_default();
    let mut out = io::stdout();
    let _ = out.write_all(&(raw.len() as u32).to_le_bytes());
    let _ = out.write_all(&raw);
    let _ = out.flush();
}

fn find_opener() -> Option<String> {
    // PATH lookup without a shell.
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in ["xdg-open", "gio"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    None
}

fn launch(url: &str) -> Result<(), String> {
    if !url.starts_with(GRAB_SCHEME) {
        return Err("only grab:// URLs are accepted".to_string());
    }
    let opener = find_opener().ok_or_else(|| "no URI opener found (xdg-open/gio)".to_string())?;
    let mut cmd = Command::new(&opener);
    if opener.ends_with("gio") {
        cmd.arg("open");
    }
    cmd.arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Spawn and detach: the browser must not wait on the GUI app.
    cmd.spawn().map_err(|e| e.to_string())?;
    Ok(())
}

fn main() {
    loop {
        let msg = match read_message() {
            Some(m) => m,
            None => break,
        };
        let url = msg.get("url").and_then(|v| v.as_str()).unwrap_or("");
        match launch(url) {
            Ok(()) => write_message(&serde_json::json!({"success": true})),
            Err(e) => write_message(&serde_json::json!({"success": false, "error": e})),
        }
    }
}
