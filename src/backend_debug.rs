//! Backend debug output: opt-in via `GRAB_DEBUG_BACKEND=1`.
//!
//! When enabled, every yt-dlp/gallery-dl spawn logs its full argv and every
//! backend stderr chunk is echoed to the terminal live. The backends' pipes
//! are always captured for progress parsing; this only adds visibility.
//! Zero cost when disabled (a single relaxed atomic load per check).

use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Call once at startup. Enables backend debug output when the
/// `GRAB_DEBUG_BACKEND` environment variable is set (any value).
pub fn init() {
    if std::env::var_os("GRAB_DEBUG_BACKEND").is_some() {
        ENABLED.store(true, Ordering::Relaxed);
        eprintln!("[grab] GRAB_DEBUG_BACKEND=1: backend argv and stderr will be echoed");
    }
}

/// Whether backend debug output is enabled.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Log a backend spawn with its full argv. Cheap no-op when disabled.
pub fn log_spawn(program: &str, args: &[String]) {
    if enabled() {
        eprintln!("[grab-backend] spawn: {} {}", program, args.join(" "));
    }
}

/// Echo a raw backend stderr chunk to the terminal. Cheap no-op when disabled.
/// Call from stderr drain loops; the bytes are already being captured for
/// parsing, this only adds visibility.
pub fn echo_stderr(chunk: &[u8]) {
    if enabled() {
        use std::io::Write as _;
        let _ = std::io::stderr().write_all(chunk);
    }
}
