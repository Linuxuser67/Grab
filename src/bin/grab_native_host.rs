//! Grab native messaging host.
//!
//! Spawned by the browser (via the manifest installed by
//! `grab --install-browser-host`). Uses the `native_messaging` crate for the
//! stdio wire protocol; only `grab://` URLs are accepted and handed to the
//! OS URI opener as a single argv element, never through a shell.

use native_messaging::host::{MAX_FROM_BROWSER, decode_message, encode_message};
use std::io::{self, Write};
use std::process::Command;

const GRAB_SCHEME: &str = "grab://";

fn find_opener() -> Option<String> {
    use std::os::unix::fs::PermissionsExt as _;
    // PATH lookup without a shell. Empty entries (=> CWD) are skipped, and
    // the executable bit is checked: a non-executable match is unusable.
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for name in ["xdg-open", "gio"] {
            let candidate = dir.join(name);
            if candidate.is_file()
                && candidate
                    .metadata()
                    .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
            {
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
    let is_gio = std::path::Path::new(&opener)
        .file_name()
        .is_some_and(|n| n == "gio");
    if is_gio {
        cmd.arg("open");
    }
    // Detach fully: the child must not inherit the browser's
    // native-messaging pipe, and a reaper thread avoids zombies when the
    // host stays alive across launches (persistent connectNative port).
    let mut child = cmd
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn main() {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut output = io::stdout();
    while let Ok(raw) = decode_message(&mut input, MAX_FROM_BROWSER) {
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
