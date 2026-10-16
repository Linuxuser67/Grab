//! gallery-dl argv builders: pure functions for download and count commands.
//!
//! All argv construction is pure (no I/O) for testability. The caller spawns
//! the process and parses output via [`gallery_progress`](crate::gallery_progress).

use std::path::{Path, PathBuf};

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
/// - Custom `output.mode`: parseable pipe format (see gallery_progress)
pub fn download_argv(
    tool: &Path,
    url: &str,
    dest_dir: &Path,
    cookies_file: Option<&Path>,
    filter: GalleryFilter,
    speed_limit: Option<u64>,
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
        // Custom pipe mode: success lines are bare paths, skip lines are
        // `# `-prefixed. Byte progress is disabled; file counts drive the UI.
        "output.mode={\"success\": \"{}\\n\", \"skip\": \"# {}\\n\"}".to_string(),
    ];
    if let Some(cookies) = cookies_file {
        argv.push("-C".to_string());
        argv.push(cookies.to_string_lossy().to_string());
    }
    push_filter(&mut argv, filter);
    if let Some(limit) = speed_limit {
        argv.push("-r".to_string());
        argv.push(limit.to_string());
    }
    // `--` guards against URLs starting with `-` being parsed as options.
    argv.push("--".to_string());
    argv.push(url.to_string());
    argv
}

/// Build the gallery-dl count argv (`-g` lists direct URLs, one per line).
/// Build argv for counting files in a gallery (probe).
/// Used by the gallery card UI to show "N files" before download.
pub fn count_argv(
    tool: &Path,
    url: &str,
    cookies_file: Option<&Path>,
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
    push_filter(&mut argv, filter);
    argv.push("--".to_string());
    argv.push(url.to_string());
    argv
}

/// Append `--filter <expr>` for the given gallery filter, if it has one.
fn push_filter(argv: &mut Vec<String>, filter: GalleryFilter) {
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
            GalleryFilter::All,
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
            GalleryFilter::All,
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
            GalleryFilter::All,
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
            GalleryFilter::All,
            None,
        );
        let c_pos = argv.iter().position(|a| a == "-C").unwrap();
        assert_eq!(argv[c_pos + 1], "/tmp/cookies.txt");
    }

    #[test]
    fn download_argv_filter_images() {
        let argv = download_argv(
            &tool(),
            "https://example.com/gallery",
            Path::new("/tmp/dest"),
            None,
            GalleryFilter::ImagesOnly,
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
            GalleryFilter::VideosOnly,
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
            GalleryFilter::All,
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
            GalleryFilter::ImagesOnly,
        );
        let pos = argv
            .iter()
            .position(|a| a == "--filter")
            .expect("filter flag");
        assert!(argv[pos + 1].contains("jpg"));
    }
}
