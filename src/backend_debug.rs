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
    let mut args = Vec::new();
    let mut redact_next_extractor_args = false;
    for arg in std_cmd.get_args() {
        let text = arg.to_string_lossy();
        if redact_next_extractor_args {
            args.push(redact_extractor_args(&text));
            redact_next_extractor_args = false;
        } else if text == "--extractor-args" {
            args.push(text.into_owned());
            redact_next_extractor_args = true;
        } else if let Some(value) = text.strip_prefix("--extractor-args=") {
            args.push(format!("--extractor-args={}", redact_extractor_args(value)));
        } else {
            args.push(redact_query_tokens(&text));
        }
    }
    eprintln!("[grab-backend] spawn: {program} {}", args.join(" "));
}

/// Hide all extractor-argument values in diagnostics. User settings may contain
/// PO tokens or credentials that are not URL query parameters.
fn redact_extractor_args(text: &str) -> String {
    let Some((extractor, args)) = text.split_once(':') else {
        return "(redacted)".to_string();
    };
    let args = args
        .split(';')
        .map(|entry| match entry.split_once('=') {
            Some((key, _)) => format!("{key}=(redacted)"),
            None => entry.to_string(),
        })
        .collect::<Vec<_>>()
        .join(";");
    format!("{extractor}:{args}")
}

/// Redact query strings and fragments from URL-shaped argument tokens before
/// writing diagnostics. Signed media URLs often act as bearer credentials.
fn redact_query_tokens(text: &str) -> String {
    let is_special =
        |b: u8| b.is_ascii_whitespace() || matches!(b, b'"' | b'\'' | b'(' | b')' | b'[' | b']');
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] < 0x80 && is_special(bytes[i]) {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && !(bytes[i] < 0x80 && is_special(bytes[i])) {
            i += 1;
        }
        let token = &text[start..i];
        match token.find("://").and_then(|_| token.find(['?', '#'])) {
            Some(cut) => {
                out.push_str(&token[..cut]);
                out.push_str("(...)");
            }
            None => out.push_str(token),
        }
    }
    out
}

/// Echo one already-split backend stderr line to the terminal. Cheap no-op
/// when disabled. Call from line-based drain loops; the line is still
/// captured for parsing and the failure tail.
pub fn echo_line(line: &str) {
    if enabled() {
        eprintln!(
            "[grab-backend] gallery-dl stderr: {}",
            redact_query_tokens(line)
        );
    }
}

/// Echo a raw backend stderr chunk to the terminal. Cheap no-op when disabled.
/// Call from byte-based drain loops; the bytes are already being captured for
/// parsing, this only adds visibility.
pub fn echo_stderr(chunk: &[u8]) {
    if enabled() {
        use std::io::Write as _;
        let raw = String::from_utf8_lossy(chunk);
        let redacted = redact_query_tokens(&raw);
        let _ = std::io::stderr().write_all(redacted.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn extractor_argument_redaction_hides_token_values() {
        assert_eq!(
            redact_extractor_args("youtube:player_client=android;po_token=secret"),
            "youtube:player_client=(redacted);po_token=(redacted)"
        );
        assert_eq!(redact_extractor_args("no-colon-secret"), "(redacted)");
    }

    #[test]
    fn command_url_redaction_hides_signed_query_and_fragment() {
        assert_eq!(
            redact_query_tokens("https://media.example/file?token=secret#segment"),
            "https://media.example/file(...)"
        );
        assert_eq!(
            redact_query_tokens("warning ('https://media.example/file?sig=secret')"),
            "warning ('https://media.example/file(...)')"
        );
        assert_eq!(
            redact_query_tokens("https://media.example/file plain"),
            "https://media.example/file plain"
        );
    }

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
