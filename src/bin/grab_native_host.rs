//! Grab native messaging host.
//!
//! Spawned by the browser (via the manifest installed by
//! `grab --install-browser-host`). Uses the `native_messaging` crate for the
//! stdio wire protocol; only `grab://` URLs are accepted and handed to the
//! OS URI opener as a single argv element, never through a shell.

use native_messaging::host::{decode_message, encode_message, MAX_FROM_BROWSER};
use std::io::{self, Write};
use std::process::Command;

const GRAB_SCHEME: &str = "grab://";

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
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut output = io::stdout();
    loop {
        let raw = match decode_message(&mut input, MAX_FROM_BROWSER) {
            Ok(raw) => raw,
            Err(_) => break, // EOF or corrupt frame: browser is gone.
        };
        let url = serde_json::from_str::<serde_json::Value>(&raw)
            .ok()
            .and_then(|v| v.get("url").and_then(|u| u.as_str()).map(str::to_owned))
            .unwrap_or_default();
        let reply = match launch(&url) {
            Ok(()) => serde_json::json!({"success": true}),
            Err(e) => serde_json::json!({"success": false, "error": e}),
        };
        let frame = match encode_message(&reply) {
            Ok(f) => f,
            Err(_) => break,
        };
        if output.write_all(&frame).is_err() || output.flush().is_err() {
            break;
        }
    }
}
