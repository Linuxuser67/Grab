//! Real-file corpus tests, gated behind the `corpus` cargo feature.
//!
//! These tests read real-world files from `corpus/` (committed to the repo)
//! instead of synthetic inline fixtures. Run with:
//!   cargo nextest run --features corpus -E 'test(corpus_)'
//!
//! Without the feature, this module compiles to nothing.

#![cfg(feature = "corpus")]

use std::path::PathBuf;

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus")
}

/// yt-dlp JSON corpus: every file in corpus/ytdlp/ must survive
/// `sanitize_video_json` without panicking, and must still parse as JSON
/// afterwards. This catches unhandled shapes from real extractors.
#[test]
fn corpus_ytdlp_json_sanitizes_cleanly() {
    let dir = corpus_dir().join("ytdlp");
    let mut count = 0;
    for entry in std::fs::read_dir(&dir).expect("corpus/ytdlp missing") {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let mut value: serde_json::Value = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("{} is not valid JSON: {e}", path.display()));
        // Must not panic on real-world shapes.
        crate::video_probe::sanitize_video_json(&mut value);
        // Must still be valid JSON-serializable afterwards.
        let _ = serde_json::to_string(&value)
            .unwrap_or_else(|e| panic!("{} failed to re-serialize: {e}", path.display()));
        count += 1;
    }
    assert!(count > 0, "corpus/ytdlp contained no JSON files");
}

/// Content-Disposition corpus: every non-empty line in
/// corpus/content-disposition/headers.txt is fed to
/// `filename_from_content_disposition`. It must not panic, and when it
/// returns Some, the filename must be non-empty and contain no path
/// separators (traversal defense).
#[test]
fn corpus_content_disposition_no_panic_no_traversal() {
    let path = corpus_dir().join("content-disposition/headers.txt");
    let raw = std::fs::read_to_string(&path).expect("corpus headers.txt missing");
    let mut count = 0;
    for (lineno, line) in raw.lines().enumerate() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let result = crate::download_fetch::filename_from_content_disposition(line);
        if let Some(name) = result {
            assert!(
                !name.is_empty(),
                "line {} returned empty filename",
                lineno + 1
            );
            assert!(
                !name.contains('/') && !name.contains('\\'),
                "line {} returned filename with path separator: {name:?}",
                lineno + 1
            );
        }
        count += 1;
    }
    assert!(count > 0, "corpus headers.txt had no testable lines");
}
