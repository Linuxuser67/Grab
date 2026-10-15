//! gallery-dl probe: JSON metadata extraction, content classification, routing.
//!
//! On paste, Grab spawns the yt-dlp probe (existing) and the gallery-dl probe
//! (here) concurrently. This module parses gallery-dl's `-j` output and decides
//! which mode the New Download card shows, per the routing table in the design.

use std::path::Path;
use std::time::Duration;

use crate::gallery_tools::GalleryError;

/// Timeout for the gallery-dl probe (same as the yt-dlp probe).
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// One file from `gallery-dl -j` output. Extractors emit wildly different
/// metadata keys; `filename`/`extension`/`category` are the reliable core,
/// everything else lands in `extra` for best-effort title derivation.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct GalleryItem {
    pub filename: String,
    pub extension: String,
    pub category: String,
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// A single message from `gallery-dl -j` output. The first message is always
/// the directory/context object (no filename); file messages follow.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged)]
enum ProbeMessage {
    File(GalleryItem),
    Other(serde_json::Value),
}

/// Content type derived from the probe JSON's `extension` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GalleryContent {
    Video,
    Image,
    Unknown,
}

impl GalleryContent {
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_ascii_lowercase().as_str() {
            "mp4" | "webm" | "mov" | "mkv" | "m4v" => Self::Video,
            "jpg" | "jpeg" | "png" | "webp" | "gif" | "avif" => Self::Image,
            _ => Self::Unknown,
        }
    }
}

/// Where the New Download card should land after both probes complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// yt-dlp found video. Gallery toggle iff gallery-dl also found images.
    VideoYtDlp { gallery_toggle: bool },
    /// yt-dlp found nothing; gallery-dl found video (e.g. Instagram stories).
    VideoGalleryDl,
    /// yt-dlp found nothing; gallery-dl found images.
    Gallery,
    /// Neither probe found anything usable.
    None,
}

/// Sticky per-host mode preference: which mode the user picked last time
/// both were viable. Only honored when both modes are viable now.
/// Phase 7 (gallery UI) will add the Gallery variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StickyMode {
    Video,
}

/// Routing decision from both probe results.
///
/// `ytdlp_video`: whether the yt-dlp probe found a playable video.
/// `gallery_items`: gallery-dl `-j` items (`None` = probe failed/unsupported).
/// `host_sticky`: the user's per-host mode preference, if any.
pub fn route(
    ytdlp_video: bool,
    gallery_items: Option<&[GalleryItem]>,
    host_sticky: Option<StickyMode>,
) -> ProbeOutcome {
    // Sticky preference wins when both modes are viable; otherwise the table decides.
    let (has_video, has_images) = gallery_items
        .map(|items| {
            let mut video = false;
            let mut images = false;
            for item in items {
                match GalleryContent::from_extension(&item.extension) {
                    GalleryContent::Video => video = true,
                    GalleryContent::Image => images = true,
                    GalleryContent::Unknown => {}
                }
            }
            (video, images)
        })
        .unwrap_or((false, false));

    match (ytdlp_video, has_video, has_images) {
        (true, _, true) => {
            // Both viable: honor sticky, default Video.
            // Phase 7 will add Gallery variant handling.
            ProbeOutcome::VideoYtDlp {
                gallery_toggle: true,
            }
        }
        (true, _, false) => ProbeOutcome::VideoYtDlp {
            gallery_toggle: false,
        },
        (false, true, _) => ProbeOutcome::VideoGalleryDl,
        (false, false, true) => ProbeOutcome::Gallery,
        (false, false, false) => ProbeOutcome::None,
    }
}

/// Best-effort title from probe metadata. First non-empty wins:
/// `title` → `user`/`author` → `category` → URL-derived filename.
pub fn probe_title(item: &GalleryItem, url: &str) -> String {
    let str_field = |key: &str| {
        item.extra
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    if let Some(t) = str_field("title") {
        return t.to_string();
    }
    if let Some(u) = str_field("user").or_else(|| str_field("author")) {
        return u.to_string();
    }
    if !item.category.trim().is_empty() {
        return item.category.trim().to_string();
    }
    // Fallback: filename from URL (percent-decoded, validated).
    crate::file_names::filename_from_url(url)
}

/// Run the gallery-dl probe: `gallery-dl --config-ignore -j --no-download --range 1 <url>`.
///
/// Returns `Ok(Some(items))` on success, `Ok(None)` when the URL is unsupported
/// (exit 64) or yields no files. `Err` on timeout, spawn failure, or any other
/// non-zero exit — the card then shows "couldn't preview" but Add stays enabled
/// (optimistic path).
pub async fn probe_gallery(
    tool: &Path,
    url: &str,
    cookies_file: Option<&Path>,
) -> Result<Option<Vec<GalleryItem>>, GalleryError> {
    let mut cmd = tokio::process::Command::new(tool);
    cmd.arg("--config-ignore")
        .arg("-j")
        .arg("--no-download")
        .arg("--range")
        .arg("1")
        // Don't let gallery-dl write back to Grab's staged cookies file.
        .arg("-o")
        .arg("extractor.cookies-update=false")
        .arg(url);
    if let Some(cookies) = cookies_file {
        cmd.arg("-C").arg(cookies);
    }
    let output = tokio::time::timeout(PROBE_TIMEOUT, cmd.output())
        .await
        .map_err(|_| GalleryError::install("gallery-dl probe timed out"))?
        .map_err(|e| GalleryError::install(format!("couldn't spawn gallery-dl: {e}")))?;

    // Exit 32 (v1.30.0+) / 64 (older) = NoExtractorError: unsupported URL,
    // not a failure. v1.30.0 folded NoExtractorError (64) into InputError (32).
    if matches!(output.status.code(), Some(32) | Some(64)) {
        return Ok(None);
    }
    if !output.status.success() {
        return Err(GalleryError::install(format!(
            "gallery-dl probe failed (exit {})",
            output.status.code().unwrap_or(-1)
        )));
    }
    // `-j` emits the directory/context object first, then file objects.
    // Deserialize tolerantly: directory messages fall through to `Other`.
    let messages: Vec<ProbeMessage> = serde_json::from_slice(&output.stdout)
        .map_err(|e| GalleryError::install(format!("couldn't parse gallery-dl output: {e}")))?;
    let items: Vec<GalleryItem> = messages
        .into_iter()
        .filter_map(|m| match m {
            ProbeMessage::File(item) => Some(item),
            ProbeMessage::Other(v) => {
                // Non-file messages (directory info, etc.); skip but log at trace.
                tracing::trace!(value = ?v, "gallery-dl probe non-file message");
                None
            }
        })
        .collect();
    if items.is_empty() {
        return Ok(None);
    }
    Ok(Some(items))
}

/// Count files without downloading: `gallery-dl --config-ignore -g --no-download <url>`.
/// Returns the line count of direct file URLs, or `None` on any failure
/// (the card then shows an indeterminate count).
pub async fn count_gallery_files(
    tool: &Path,
    url: &str,
    cookies_file: Option<&Path>,
) -> Option<u32> {
    let argv = crate::gallery_argv::count_argv(tool, url, cookies_file);
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    let output = tokio::time::timeout(PROBE_TIMEOUT, cmd.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let count = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();
    Some(count as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(extension: &str) -> GalleryItem {
        GalleryItem {
            filename: format!("file.{extension}"),
            extension: extension.to_string(),
            category: "test".to_string(),
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn content_from_extension() {
        assert_eq!(GalleryContent::from_extension("mp4"), GalleryContent::Video);
        assert_eq!(GalleryContent::from_extension("MP4"), GalleryContent::Video);
        assert_eq!(
            GalleryContent::from_extension("webm"),
            GalleryContent::Video
        );
        assert_eq!(GalleryContent::from_extension("jpg"), GalleryContent::Image);
        assert_eq!(
            GalleryContent::from_extension("JPEG"),
            GalleryContent::Image
        );
        assert_eq!(
            GalleryContent::from_extension("webp"),
            GalleryContent::Image
        );
        assert_eq!(
            GalleryContent::from_extension("xyz"),
            GalleryContent::Unknown
        );
    }

    #[test]
    fn route_ytdlp_wins_with_gallery_toggle() {
        let items = vec![item("jpg")];
        assert_eq!(
            route(true, Some(&items), None),
            ProbeOutcome::VideoYtDlp {
                gallery_toggle: true
            }
        );
    }

    #[test]
    fn route_ytdlp_wins_no_toggle_without_images() {
        let items = vec![item("mp4")];
        assert_eq!(
            route(true, Some(&items), None),
            ProbeOutcome::VideoYtDlp {
                gallery_toggle: false
            }
        );
        assert_eq!(
            route(true, None, None),
            ProbeOutcome::VideoYtDlp {
                gallery_toggle: false
            }
        );
    }

    #[test]
    fn route_gallerydl_video_fallback() {
        // Instagram stories: yt-dlp finds nothing, gallery-dl finds video.
        let items = vec![item("mp4")];
        assert_eq!(
            route(false, Some(&items), None),
            ProbeOutcome::VideoGalleryDl
        );
    }

    #[test]
    fn route_gallery_images() {
        let items = vec![item("jpg"), item("png")];
        assert_eq!(route(false, Some(&items), None), ProbeOutcome::Gallery);
    }

    #[test]
    fn route_nothing() {
        assert_eq!(route(false, None, None), ProbeOutcome::None);
        let unknown = vec![item("xyz")];
        assert_eq!(route(false, Some(&unknown), None), ProbeOutcome::None);
    }

    #[test]
    fn route_sticky_host_preference() {
        let images = vec![item("jpg")];
        // Sticky video → default Video with toggle.
        assert_eq!(
            route(true, Some(&images), Some(StickyMode::Video)),
            ProbeOutcome::VideoYtDlp {
                gallery_toggle: true
            }
        );
        // No sticky → default Video.
        assert_eq!(
            route(true, Some(&images), None),
            ProbeOutcome::VideoYtDlp {
                gallery_toggle: true
            }
        );
        // Sticky never overrides an unviable mode: yt-dlp found nothing,
        // so Video is not an option even with a Video preference.
        assert_eq!(
            route(false, Some(&images), Some(StickyMode::Video)),
            ProbeOutcome::Gallery
        );
    }

    #[test]
    fn route_mixed_content() {
        // Order-independent: video + image → Video with gallery toggle.
        let mixed = vec![item("mp4"), item("jpg")];
        assert_eq!(
            route(true, Some(&mixed), None),
            ProbeOutcome::VideoYtDlp {
                gallery_toggle: true
            }
        );
        let mixed_rev = vec![item("jpg"), item("mp4")];
        assert_eq!(
            route(true, Some(&mixed_rev), None),
            ProbeOutcome::VideoYtDlp {
                gallery_toggle: true
            }
        );
        // yt-dlp found nothing: video wins over images for the fallback.
        assert_eq!(
            route(false, Some(&mixed), None),
            ProbeOutcome::VideoGalleryDl
        );
    }

    #[test]
    fn probe_title_prefers_title_field() {
        let mut extra = serde_json::Map::new();
        extra.insert(
            "title".to_string(),
            serde_json::Value::String("My Gallery".to_string()),
        );
        extra.insert(
            "user".to_string(),
            serde_json::Value::String("someuser".to_string()),
        );
        let item = GalleryItem {
            filename: "f.jpg".to_string(),
            extension: "jpg".to_string(),
            category: "instagram".to_string(),
            extra,
        };
        assert_eq!(
            probe_title(&item, "https://example.com/p/123"),
            "My Gallery"
        );
    }

    #[test]
    fn probe_title_falls_back_to_url() {
        // Empty category and no metadata → URL-derived filename.
        let item = GalleryItem {
            filename: "f.jpg".to_string(),
            extension: "jpg".to_string(),
            category: String::new(),
            extra: serde_json::Map::new(),
        };
        assert_eq!(
            probe_title(&item, "https://example.com/p/sunset.jpg"),
            "sunset.jpg"
        );
    }

    #[test]
    fn probe_json_skips_directory_message() {
        // Real `gallery-dl -j` output: directory/context object first
        // (no filename), then file objects.
        let json = r#"[
            {"category": "instagram", "subcategory": "posts", "user": "someuser"},
            {
                "filename": "12345.jpg",
                "extension": "jpg",
                "category": "instagram",
                "title": "Sunset",
                "user": "someuser"
            }
        ]"#;
        let messages: Vec<ProbeMessage> = serde_json::from_str(json).unwrap();
        assert_eq!(messages.len(), 2);
        let items: Vec<GalleryItem> = messages
            .into_iter()
            .filter_map(|m| match m {
                ProbeMessage::File(item) => Some(item),
                ProbeMessage::Other(_) => None,
            })
            .collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].filename, "12345.jpg");
        assert_eq!(
            GalleryContent::from_extension(&items[0].extension),
            GalleryContent::Image
        );
        assert_eq!(probe_title(&items[0], "https://example.com/"), "Sunset");
    }
}
