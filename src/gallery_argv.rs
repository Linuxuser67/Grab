//! gallery-dl argv builders: pure functions for download and count commands.
//!
//! All argv construction is pure (no I/O) for testability. The caller spawns
//! the process and parses output via [`gallery_progress`](crate::gallery_progress).

use std::path::{Path, PathBuf};

/// Instagram request throttling args: gallery-dl recommends 3s between
/// requests to avoid rate-limiting and temporary IP blocks.
/// `include_download_sleep`: also add `--sleep 1.5` (delay before each file
/// download). Only meaningful for actual downloads, not probes/counts.
pub fn instagram_throttle_args(url: &str, include_download_sleep: bool) -> Vec<String> {
    if is_instagram_url(url) {
        let mut args = vec!["--sleep-request".to_string(), "3".to_string()];
        if include_download_sleep {
            args.push("--sleep".to_string());
            args.push("1.5".to_string());
        }
        args
    } else {
        Vec::new()
    }
}

/// Video extensions recognized for gallery classification and filtering.
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "webm", "mov", "mkv", "m4v", "avi", "ts", "flv", "wmv", "mpg", "mpeg", "3gp", "3g2",
    "ogv", "m2ts", "mts", "asf", "f4v",
];

/// Image extensions recognized for gallery classification and filtering.
pub const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "jpe", "jfif", "png", "webp", "gif", "avif", "bmp", "svg", "tiff", "tif",
    "heic", "heif",
];

/// Filters for the gallery download (user's Images/Videos/All picker).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize)]
pub enum GalleryFilter {
    #[default]
    All,
    ImagesOnly,
    VideosOnly,
}

impl<'de> serde::Deserialize<'de> for GalleryFilter {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "ImagesOnly" => Ok(GalleryFilter::ImagesOnly),
            "VideosOnly" => Ok(GalleryFilter::VideosOnly),
            // Unknown (or "All") → All. A bad filter value must not
            // quarantine the queue entry.
            _ => Ok(GalleryFilter::All),
        }
    }
}

impl GalleryFilter {
    /// gallery-dl `--filter` Python expression. `None` for All (no filter).
    /// Uses the `extension` keyword which every extractor provides;
    /// case-insensitive via `.lower()`. Built from the shared tables in
    /// [`gallery_probe`](crate::gallery_probe) so filters and classification agree.
    pub fn to_filter_expr(self) -> Option<String> {
        fn expr(exts: &[&str]) -> String {
            let list = exts
                .iter()
                .map(|e| format!("'{e}'"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("extension.lower() in ({list})")
        }
        match self {
            Self::All => None,
            Self::ImagesOnly => Some(expr(IMAGE_EXTENSIONS)),
            Self::VideosOnly => Some(expr(VIDEO_EXTENSIONS)),
        }
    }
}

/// Build the gallery-dl download argv.
///
/// - `-D`: exact destination directory (not `-d`, which nests under base-directory)
/// - `-f '/O'`: original filenames (not extractor-specific templates)
/// - `extractor.directory=[]`: flat output (no category/subcategory nesting)
/// - `extractor.cookies-update=false`: don't mutate Grab's staged cookies
/// - `downloader.http.progress=null`: suppress gallery-dl's stderr progress
/// - `output.progress=false`: disable multi-URL progress indicator
/// - `output.mode=pipe`: one file path per line (see gallery_progress)
///
/// Build a gallery-dl `--range` spec from 0-based selected indices.
/// gallery-dl uses 1-based indices; consecutive runs compress to `a-b`.
/// Empty input returns `None` (caller should reject empty selections).
/// Example: [0, 1, 2, 4, 6, 7] -> "1-3,5,7-8".
pub fn range_spec(selected: &[usize]) -> Option<String> {
    if selected.is_empty() {
        return None;
    }
    let mut sorted: Vec<usize> = selected.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut parts = Vec::new();
    let mut run_start = sorted[0];
    let mut run_end = sorted[0];
    for &i in &sorted[1..] {
        if i == run_end + 1 {
            run_end = i;
        } else {
            parts.push(format_range(run_start, run_end));
            run_start = i;
            run_end = i;
        }
    }
    parts.push(format_range(run_start, run_end));
    Some(parts.join(","))
}

/// Format one 0-based inclusive run as a 1-based gallery-dl range.
fn format_range(start: usize, end: usize) -> String {
    // gallery-dl indices are 1-based.
    let s = start + 1;
    let e = end + 1;
    if s == e {
        s.to_string()
    } else {
        format!("{s}-{e}")
    }
}

#[allow(clippy::too_many_arguments)]
pub fn download_argv(
    tool: &Path,
    url: &str,
    dest_dir: &Path,
    cookies_file: Option<&Path>,
    user_agent: Option<&str>,
    filter: GalleryFilter,
    speed_limit: Option<u64>,
    range: Option<&str>,
) -> Vec<String> {
    let mut argv = vec![
        tool.to_string_lossy().to_string(),
        "--config-ignore".to_string(),
        "-D".to_string(),
        dest_dir.to_string_lossy().to_string(),
        "-f".to_string(),
        "/O".to_string(),
        "-o".to_string(),
        "extractor.directory=[]".to_string(),
        "-o".to_string(),
        "extractor.cookies-update=false".to_string(),
        "-o".to_string(),
        "downloader.http.progress=null".to_string(),
        "-o".to_string(),
        "output.progress=false".to_string(),
        "-o".to_string(),
        // Pipe mode: one file path per line. Avoids the custom JSON
        // output.mode which triggers a gallery-dl internal bug
        // (AttributeError in output.py _make_func).
        "output.mode=pipe".to_string(),
    ];
    if let Some(cookies) = cookies_file {
        argv.push("-C".to_string());
        argv.push(cookies.to_string_lossy().to_string());
    }
    if let Some(ua) = user_agent {
        argv.push("--user-agent".to_string());
        argv.push(ua.to_string());
    }
    push_filter(&mut argv, filter);
    if let Some(limit) = speed_limit {
        argv.push("-r".to_string());
        argv.push(limit.to_string());
    }
    // Picker selection: only download the chosen indices.
    if let Some(spec) = range {
        argv.push("--range".to_string());
        argv.push(spec.to_string());
    }
    // Instagram rate-limits aggressively: throttle requests.
    argv.extend(instagram_throttle_args(url, true));
    // `--` guards against URLs starting with `-` being parsed as options.
    argv.push("--".to_string());
    argv.push(url.to_string());
    argv
}

/// Whether a URL is Instagram (rate-limited; skip extra enumerations).
pub fn is_instagram_url(url: &str) -> bool {
    let host = crate::video_probe::page_host(url);
    host == "instagram.com" || host.ends_with(".instagram.com")
}

/// Build the gallery-dl count argv (`-g` lists direct URLs, one per line).
/// Build argv for counting files in a gallery (probe).
/// Used by the gallery card UI to show "N files" before download.
pub fn count_argv(
    tool: &Path,
    url: &str,
    cookies_file: Option<&Path>,
    user_agent: Option<&str>,
    filter: GalleryFilter,
) -> Vec<String> {
    let mut argv = vec![
        tool.to_string_lossy().to_string(),
        "--config-ignore".to_string(),
        "-g".to_string(),
        "--no-download".to_string(),
        "-o".to_string(),
        "output.fallback=false".to_string(),
        "-o".to_string(),
        "extractor.cookies-update=false".to_string(),
    ];
    if let Some(cookies) = cookies_file {
        argv.push("-C".to_string());
        argv.push(cookies.to_string_lossy().to_string());
    }
    if let Some(ua) = user_agent {
        argv.push("--user-agent".to_string());
        argv.push(ua.to_string());
    }
    push_filter(&mut argv, filter);
    // Instagram throttling for the extraction requests (no --sleep: count
    // doesn't download files).
    argv.extend(instagram_throttle_args(url, false));
    argv.push("--".to_string());
    argv.push(url.to_string());
    argv
}

/// Append `--filter <expr>` for the given gallery filter, if it has one.
pub(crate) fn push_filter(argv: &mut Vec<String>, filter: GalleryFilter) {
    if let Some(expr) = filter.to_filter_expr() {
        argv.push("--filter".to_string());
        argv.push(expr);
    }
}

/// Resolve the gallery-dl binary path, or the install error message.
/// Used by the card to decide between "probe" and "install prompt".
pub fn resolve_tool() -> Result<PathBuf, String> {
    crate::gallery_tools::resolve_gallerydl().map_err(|e| e.to_string())
}

/// Inputs for a gallery download (one queue row per gallery).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GalleryJob {
    /// Manager id; skipped on serialize (restore assigns a fresh id via claim_id).
    #[serde(skip, default)]
    pub item_id: u64,
    pub page_url: String,
    /// Display title for the queue row (from probe metadata).
    pub title: String,
    /// File count from the probe (`-g` line count), if available.
    pub file_count: Option<u32>,
    pub dest_dir: PathBuf,
    pub filter: GalleryFilter,
    /// Browser spec for cookies (e.g. "chrome", "firefox", "none").
    /// Resolved to a staged file at spawn time (not enqueue time);
    /// the staged file is ephemeral.
    pub cookies_browser: String,
    /// Per-download speed cap in bytes/sec (`-r`/`--limit-rate`); `None` = unlimited.
    pub speed_limit: Option<u64>,
    /// gallery-dl `--range` spec for a file-picker selection (e.g. "1,3,5-7");
    /// `None` = download everything. Set by the picker (Phase 3); the backend
    /// just threads it through to the argv.
    #[serde(default)]
    pub range: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gallery_probe::GalleryContent;

    fn tool() -> PathBuf {
        PathBuf::from("/tmp/gallery-dl")
    }

    #[test]
    fn filter_expr_matches_classification_tables() {
        // Every table entry must appear in its filter expression and classify the same way.
        let images = GalleryFilter::ImagesOnly.to_filter_expr().expect("expr");
        for ext in IMAGE_EXTENSIONS {
            assert!(
                images.contains(&format!("'{ext}'")),
                "image ext '{ext}' missing from filter"
            );
            assert_eq!(GalleryContent::from_extension(ext), GalleryContent::Image);
        }
        let videos = GalleryFilter::VideosOnly.to_filter_expr().expect("expr");
        for ext in VIDEO_EXTENSIONS {
            assert!(
                videos.contains(&format!("'{ext}'")),
                "video ext '{ext}' missing from filter"
            );
            assert_eq!(GalleryContent::from_extension(ext), GalleryContent::Video);
        }
        assert!(GalleryFilter::All.to_filter_expr().is_none());
    }

    #[test]
    fn download_argv_uses_exact_dir() {
        let argv = download_argv(
            &tool(),
            "https://example.com/gallery",
            Path::new("/tmp/dest"),
            None,
            None,
            GalleryFilter::All,
            None,
            None,
        );
        // `-D`, not `-d` (exact dir, no nesting).
        let d_pos = argv.iter().position(|a| a == "-D").unwrap();
        assert_eq!(argv[d_pos + 1], "/tmp/dest");
        assert!(!argv.iter().any(|a| a == "-d"));
    }

    #[test]
    fn download_argv_original_filenames() {
        let argv = download_argv(
            &tool(),
            "https://example.com/gallery",
            Path::new("/tmp/dest"),
            None,
            None,
            GalleryFilter::All,
            None,
            None,
        );
        let f_pos = argv.iter().position(|a| a == "-f").unwrap();
        assert_eq!(argv[f_pos + 1], "/O");
    }

    #[test]
    fn download_argv_flat_output() {
        let argv = download_argv(
            &tool(),
            "https://example.com/gallery",
            Path::new("/tmp/dest"),
            None,
            None,
            GalleryFilter::All,
            None,
            None,
        );
        assert!(argv.contains(&"extractor.directory=[]".to_string()));
    }

    #[test]
    fn download_argv_cookies() {
        let argv = download_argv(
            &tool(),
            "https://example.com/gallery",
            Path::new("/tmp/dest"),
            Some(Path::new("/tmp/cookies.txt")),
            None,
            GalleryFilter::All,
            None,
            None,
        );
        let c_pos = argv.iter().position(|a| a == "-C").unwrap();
        assert_eq!(argv[c_pos + 1], "/tmp/cookies.txt");
    }

    #[test]
    fn download_argv_user_agent() {
        let argv = download_argv(
            &tool(),
            "https://example.com/gallery",
            Path::new("/tmp/dest"),
            None,
            Some("Mozilla/5.0 Test"),
            GalleryFilter::All,
            None,
            None,
        );
        let ua_pos = argv.iter().position(|a| a == "--user-agent").unwrap();
        assert_eq!(argv[ua_pos + 1], "Mozilla/5.0 Test");
    }

    #[test]
    fn download_argv_no_user_agent_by_default() {
        let argv = download_argv(
            &tool(),
            "https://example.com/gallery",
            Path::new("/tmp/dest"),
            None,
            None,
            GalleryFilter::All,
            None,
            None,
        );
        assert!(!argv.iter().any(|a| a == "--user-agent"));
    }

    #[test]
    fn is_instagram_url_matches() {
        assert!(is_instagram_url("https://www.instagram.com/p/ABC123/"));
        assert!(is_instagram_url("https://instagram.com/p/ABC123/"));
        assert!(!is_instagram_url("https://example.com/gallery"));
    }

    #[test]
    fn download_argv_filter_images() {
        let argv = download_argv(
            &tool(),
            "https://example.com/gallery",
            Path::new("/tmp/dest"),
            None,
            None,
            GalleryFilter::ImagesOnly,
            None,
            None,
        );
        let f_pos = argv.iter().position(|a| a == "--filter").unwrap();
        assert!(argv[f_pos + 1].contains("jpg"));
        assert!(!argv[f_pos + 1].contains("mp4"));
    }

    #[test]
    fn download_argv_filter_videos() {
        let argv = download_argv(
            &tool(),
            "https://example.com/gallery",
            Path::new("/tmp/dest"),
            None,
            None,
            GalleryFilter::VideosOnly,
            None,
            None,
        );
        let f_pos = argv.iter().position(|a| a == "--filter").unwrap();
        assert!(argv[f_pos + 1].contains("mp4"));
        assert!(!argv[f_pos + 1].contains("jpg"));
    }

    #[test]
    fn download_argv_no_filter_for_all() {
        let argv = download_argv(
            &tool(),
            "https://example.com/gallery",
            Path::new("/tmp/dest"),
            None,
            None,
            GalleryFilter::All,
            None,
            None,
        );
        assert!(!argv.iter().any(|a| a == "--filter"));
    }

    #[test]
    fn count_argv_no_fallback() {
        let argv = count_argv(
            &tool(),
            "https://example.com/gallery",
            None,
            None,
            GalleryFilter::All,
        );
        assert!(argv.contains(&"output.fallback=false".to_string()));
        assert!(argv.contains(&"-g".to_string()));
    }

    #[test]
    fn count_argv_applies_filter() {
        let argv = count_argv(
            &tool(),
            "https://example.com/gallery",
            None,
            None,
            GalleryFilter::ImagesOnly,
        );
        let pos = argv
            .iter()
            .position(|a| a == "--filter")
            .expect("filter flag");
        assert!(argv[pos + 1].contains("jpg"));
    }

    #[test]
    fn download_argv_instagram_gets_sleep_request() {
        let argv = download_argv(
            &tool(),
            "https://www.instagram.com/p/ABC123/",
            Path::new("/tmp/dest"),
            None,
            None,
            GalleryFilter::All,
            None,
            None,
        );
        let pos = argv
            .iter()
            .position(|a| a == "--sleep-request")
            .expect("sleep-request flag");
        assert_eq!(argv[pos + 1], "3");
        let pos = argv
            .iter()
            .position(|a| a == "--sleep")
            .expect("sleep flag");
        assert_eq!(argv[pos + 1], "1.5");
    }

    #[test]
    fn download_argv_range_selection() {
        // RED: a picker selection must reach gallery-dl as `--range <spec>`.
        let argv = download_argv(
            &tool(),
            "https://example.com/gallery",
            Path::new("/tmp/dest"),
            None,
            None,
            GalleryFilter::All,
            None,
            Some("1,3,5-7"),
        );
        let pos = argv
            .iter()
            .position(|a| a == "--range")
            .expect("--range flag");
        assert_eq!(argv[pos + 1], "1,3,5-7");
    }

    #[test]
    fn download_argv_no_range_by_default() {
        // No selection: no `--range` flag at all (download everything).
        let argv = download_argv(
            &tool(),
            "https://example.com/gallery",
            Path::new("/tmp/dest"),
            None,
            None,
            GalleryFilter::All,
            None,
            None,
        );
        assert!(!argv.iter().any(|a| a == "--range"));
    }

    #[test]
    fn range_spec_empty_is_none() {
        assert_eq!(range_spec(&[]), None);
    }

    #[test]
    fn range_spec_single() {
        assert_eq!(range_spec(&[0]), Some("1".to_string()));
        assert_eq!(range_spec(&[4]), Some("5".to_string()));
    }

    #[test]
    fn range_spec_consecutive_compresses() {
        assert_eq!(range_spec(&[0, 1, 2]), Some("1-3".to_string()));
    }

    #[test]
    fn range_spec_mixed() {
        assert_eq!(
            range_spec(&[0, 1, 2, 4, 6, 7]),
            Some("1-3,5,7-8".to_string())
        );
    }

    #[test]
    fn range_spec_unsorted_and_dupes() {
        // Picker indices arrive ascending, but be robust.
        assert_eq!(range_spec(&[7, 6, 0, 6]), Some("1,7-8".to_string()));
    }

    #[test]
    fn download_argv_non_instagram_no_sleep_request() {
        let argv = download_argv(
            &tool(),
            "https://example.com/gallery",
            Path::new("/tmp/dest"),
            None,
            None,
            GalleryFilter::All,
            None,
            None,
        );
        assert!(!argv.iter().any(|a| a == "--sleep-request"));
    }

    #[test]
    fn instagram_throttle_args_download_includes_both() {
        let args = instagram_throttle_args("https://www.instagram.com/p/ABC/", true);
        assert_eq!(
            args,
            vec![
                "--sleep-request".to_string(),
                "3".to_string(),
                "--sleep".to_string(),
                "1.5".to_string()
            ]
        );
    }

    #[test]
    fn instagram_throttle_args_probe_excludes_download_sleep() {
        let args = instagram_throttle_args("https://www.instagram.com/p/ABC/", false);
        assert_eq!(args, vec!["--sleep-request".to_string(), "3".to_string()]);
    }

    #[test]
    fn instagram_throttle_args_non_instagram_empty() {
        assert!(instagram_throttle_args("https://example.com/gallery", true).is_empty());
        assert!(instagram_throttle_args("https://example.com/gallery", false).is_empty());
    }

    #[test]
    fn count_argv_instagram_gets_sleep_request() {
        let argv = count_argv(
            &tool(),
            "https://www.instagram.com/p/ABC123/",
            None,
            None,
            GalleryFilter::All,
        );
        assert!(argv.contains(&"--sleep-request".to_string()));
        // Count doesn't download: no --sleep.
        assert!(!argv.iter().any(|a| a == "--sleep"));
    }
}
