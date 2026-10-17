//! Backend debug output: opt-in via `GRAB_DEBUG_BACKEND=1`.
//!
//! When enabled, every yt-dlp/gallery-dl spawn logs its full argv and every
//! backend stderr chunk is echoed to the terminal live. The backends' pipes
//! are always captured for progress parsing; this only adds visibility.
//! Zero cost when disabled (a single relaxed atomic load per check).

use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Call once at startup. Enables backend debug output when the
/// `GRAB_DEBUG_BACKEND` environment variable parses as true (see
/// [`parse_flag`]): unset, empty, `0`, and `false` all keep it off.
pub fn init() {
    if parse_flag(std::env::var_os("GRAB_DEBUG_BACKEND").as_deref()) {
        ENABLED.store(true, Ordering::Relaxed);
        eprintln!("[grab] GRAB_DEBUG_BACKEND=1: backend argv and stderr will be echoed");
    }
}

/// `GRAB_DEBUG_BACKEND` truthiness: true unless unset, empty, `0`, or
/// `false`. Pure for tests.
fn parse_flag(v: Option<&std::ffi::OsStr>) -> bool {
    match v {
        None => false,
        Some(v) => !v.is_empty() && !matches!(v.to_str(), Some("0") | Some("false")),
    }
}

/// Whether backend debug output is enabled.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Log a backend spawn with its full argv, exactly as the child will see it
/// (`as_std()` includes builder-added args). Cheap no-op when disabled — no
/// caller-side `enabled()` gate needed.
pub fn log_command(cmd: &tokio::process::Command) {
    if !enabled() {
        return;
    }
    let std_cmd = cmd.as_std();
    let program = std_cmd.get_program().to_string_lossy().into_owned();
    let args: Vec<String> = std_cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    eprintln!("[grab-backend] spawn: {program} {}", args.join(" "));
}

/// Echo one already-split backend stderr line to the terminal. Cheap no-op
/// when disabled. Call from line-based drain loops; the line is still
/// captured for parsing and the failure tail.
pub fn echo_line(line: &str) {
    if enabled() {
        eprintln!("[grab-backend] gallery-dl stderr: {line}");
    }
}

/// Echo a raw backend stderr chunk to the terminal. Cheap no-op when disabled.
/// Call from byte-based drain loops; the bytes are already being captured for
/// parsing, this only adds visibility.
pub fn echo_stderr(chunk: &[u8]) {
    if enabled() {
        use std::io::Write as _;
        let _ = std::io::stderr().write_all(chunk);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn parse_flag_table() {
        // Unset, empty, "0", and "false" keep backend debug off.
        assert!(!parse_flag(None));
        assert!(!parse_flag(Some(OsStr::new(""))));
        assert!(!parse_flag(Some(OsStr::new("0"))));
        assert!(!parse_flag(Some(OsStr::new("false"))));
        // Anything else — including junk — turns it on.
        assert!(parse_flag(Some(OsStr::new("1"))));
        assert!(parse_flag(Some(OsStr::new("true"))));
        assert!(parse_flag(Some(OsStr::new("yes"))));
    }
}
