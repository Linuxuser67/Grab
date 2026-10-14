//! Real-world-shaped corpus tests, gated behind the `corpus` cargo feature.
//!
//! These tests use fixture files in `corpus/` (committed to the repo) that
//! capture real-world parser edge cases: null-vs-missing keys, wild
//! Content-Disposition headers, hostile torrent paths, adversarial Unicode
//! titles. They are synthetic but modeled on documented wild behavior,
//! not on-the-wire captures.
//!
//! Run with:
//!   cargo nextest run --features corpus -E 'test(corpus_)'
//!
//! Without the feature, this module compiles to nothing.

#![cfg(feature = "corpus")]

use std::path::PathBuf;

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus")
}

/// yt-dlp JSON corpus: every file in corpus/ytdlp/*.json is run through
/// `sanitize_video_json`. The test asserts the sanitizer actually
/// repaired the input: required keys exist with correct types afterwards.
/// A no-op sanitizer would fail these.
#[test]
fn corpus_ytdlp_json_sanitizes_cleanly() {
    let dir = corpus_dir().join("ytdlp");
    let mut count = 0;
    for entry in std::fs::read_dir(&dir).expect("corpus/ytdlp missing") {
        let entry = entry.unwrap();
        let path = entry.path();
        // Fail on unexpected files instead of silently skipping them —
        // a misnamed fixture (.JSON, no extension) would otherwise never run.
        let ext = path.extension().and_then(|e| e.to_str());
        assert_eq!(
            ext,
            Some("json"),
            "unexpected file in corpus/ytdlp: {}",
            path.display()
        );
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let mut value: serde_json::Value = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("{} is not valid JSON: {e}", path.display()));
        // Must not panic on real-world shapes.
        crate::video_probe::sanitize_video_json(&mut value);
        let obj = value
            .as_object()
            .unwrap_or_else(|| panic!("{}: root is not an object after sanitize", path.display()));
        // The sanitizer must have filled these (proves it wasn't a no-op).
        for key in ["id", "title", "extractor", "live_status"] {
            assert!(
                obj.get(key).is_some_and(|v| v.is_string()),
                "{}: missing or non-string {key:?} after sanitize",
                path.display()
            );
        }
        assert!(
            obj.get("formats").is_some_and(|v| v.is_array()),
            "{}: formats is not an array after sanitize",
            path.display()
        );
        assert!(
            obj.get("thumbnails").is_some_and(|v| v.is_array()),
            "{}: thumbnails is not an array after sanitize",
            path.display()
        );
        count += 1;
    }
    assert!(count > 0, "corpus/ytdlp contained no JSON files");
}

/// Content-Disposition corpus: each line in
/// corpus/content-disposition/headers.txt is `input ||| expected`, where
/// expected is the filename or NONE. The test asserts exact output —
/// a parser returning None for everything would fail.
#[test]
fn corpus_content_disposition_expected_values() {
    let path = corpus_dir().join("content-disposition/headers.txt");
    let raw = std::fs::read_to_string(&path).expect("corpus headers.txt missing");
    let mut count = 0;
    for (lineno, line) in raw.lines().enumerate() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (input, expected) = line
            .split_once(" ||| ")
            .unwrap_or_else(|| panic!("line {} missing ' ||| ' separator", lineno + 1));
        let expected = if expected == "NONE" {
            None
        } else {
            Some(expected.to_string())
        };
        let actual = crate::download_fetch::filename_from_content_disposition(input);
        assert_eq!(
            actual,
            expected,
            "line {} mismatch for {input:?}",
            lineno + 1
        );
        count += 1;
    }
    assert!(count > 0, "corpus headers.txt had no testable lines");
}

/// Netscape cookie corpus: each line in corpus/cookies/netscape.txt is
/// `input ||| expected`, where expected is SOME or NONE. Well-formed
/// lines must parse; malformed ones (bad expiry, short lines) must
/// return None, not panic or produce garbage.
#[test]
fn corpus_cookies_parse_expected() {
    let path = corpus_dir().join("cookies/netscape.txt");
    let raw = std::fs::read_to_string(&path).expect("corpus netscape.txt missing");
    let mut count = 0;
    for (lineno, line) in raw.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (input, expected) = line
            .split_once(" ||| ")
            .unwrap_or_else(|| panic!("line {} missing ' ||| ' separator", lineno + 1));
        let expect_some = expected == "SOME";
        // Must not panic on real-world edge cases (huge expiries, bad numbers).
        let actual = crate::cookies::parse_netscape_line(input);
        assert_eq!(
            actual.is_some(),
            expect_some,
            "line {}: expected {} for {input:?}, got {:?}",
            lineno + 1,
            if expect_some { "SOME" } else { "NONE" },
            actual.is_some()
        );
        count += 1;
    }
    assert!(count > 0, "corpus netscape.txt had no testable lines");
}

/// Torrent corpus: every .torrent file in corpus/torrents/ must parse
/// (or fail cleanly) via librqbit without panicking. Valid torrents have
/// their names run through the sanitization path.
#[test]
fn corpus_torrents_parse_without_panic() {
    let dir = corpus_dir().join("torrents");
    let mut count = 0;
    for entry in std::fs::read_dir(&dir).expect("corpus/torrents missing") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("torrent") {
            continue;
        }
        let bytes =
            std::fs::read(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        // librqbit must not panic; invalid files return Err.
        if let Ok(meta) = librqbit::torrent_from_bytes(&bytes) {
            let name = meta.info.data.name.as_ref();
            let display = crate::torrent::sanitize_display_path(name);
            assert!(
                !display.contains(".."),
                "{}: sanitized name contains ..: {display:?}",
                path.display()
            );
        }
        count += 1;
    }
    assert!(count > 0, "corpus/torrents contained no .torrent files");
}

/// Title corpus: every line in corpus/titles/titles.txt is fed to
/// `sanitize_folder_name`. It must not panic, must return non-empty
/// output, and must contain no path separators.
#[test]
fn corpus_titles_sanitize_cleanly() {
    let path = corpus_dir().join("titles/titles.txt");
    let raw = std::fs::read_to_string(&path).expect("corpus titles.txt missing");
    let mut count = 0;
    for (lineno, line) in raw.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let result = crate::file_names::sanitize_folder_name(line);
        assert!(!result.is_empty(), "line {} sanitized to empty", lineno + 1);
        assert!(
            !result.contains('/') && !result.contains('\\'),
            "line {} sanitized with path separator: {result:?}",
            lineno + 1
        );
        count += 1;
    }
    assert!(count > 0, "corpus titles.txt had no testable lines");
}

/// Content-Range corpus: every line in corpus/http/content-range.txt is
/// fed to `parse_content_range`. It must not panic. When it returns
/// Some, start <= end must hold (sane range).
#[test]
fn corpus_content_range_parses_safely() {
    let path = corpus_dir().join("http/content-range.txt");
    let raw = std::fs::read_to_string(&path).expect("corpus content-range.txt missing");
    let mut count = 0;
    for (lineno, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Must not panic on real-world variants (case, whitespace, */total, overflow).
        if let Some((start, end, _total)) = crate::download_fetch::parse_content_range(line) {
            assert!(
                start <= end,
                "line {} parsed to inverted range {start}-{end}",
                lineno + 1
            );
        }
        count += 1;
    }
    assert!(count > 0, "corpus content-range.txt had no testable lines");
}

/// yt-dlp progress corpus: every line in corpus/ytdlp/progress.txt is
/// fed to `parse_ytdlp_template`. It must not panic. Non-Grab lines
/// and error lines must return None.
#[test]
fn corpus_ytdlp_progress_parses_safely() {
    let path = corpus_dir().join("ytdlp/progress.txt");
    let raw = std::fs::read_to_string(&path).expect("corpus progress.txt missing");
    let mut count = 0;
    for (lineno, line) in raw.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Must not panic on truncated, malformed, or non-Grab lines.
        let result = crate::video_progress::parse_ytdlp_template(line);
        if !line.starts_with("[Grab];") || line.contains(";error;") {
            assert!(
                result.is_none(),
                "line {} should not parse: {line:?}",
                lineno + 1
            );
        }
        count += 1;
    }
    assert!(count > 0, "corpus progress.txt had no testable lines");
}
