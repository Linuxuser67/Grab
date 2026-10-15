//! gallery-dl probe: JSON metadata extraction, content classification, routing.
//!
//! On paste, Grab spawns the yt-dlp probe (existing) and the gallery-dl probe
//! (here) concurrently. This module parses gallery-dl's `-j` output and decides
//! which mode the New Download card shows, per the routing table in the design.

use std::path::PathBuf;
use std::time::Duration;

/// Timeout for the gallery-dl probe (same as the yt-dlp probe).
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// One item from `gallery-dl -j` output. Extractors emit wildly different
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

/// Routing decision from both probe results.
///
/// `ytdlp_video`: whether the yt-dlp probe found a playable video.
/// `gallery_items`: gallery-dl `-j` items (`None` = probe failed/unsupported).
/// `host_sticky`: the user's per-host mode preference, if any.
pub fn route(
    ytdlp_video: bool,
    gallery_items: Option<&[GalleryItem]>,
    host_sticky: Option<ProbeOutcome>,
) -> ProbeOutcome {
    // Sticky preference wins when both modes are viable; otherwise the table decides.
    let gallery_content: Option<GalleryContent> = gallery_items.and_then(|items| {
        items
            .iter()
            .map(|i| GalleryContent::from_extension(&i.extension))
            .find(|c| *c != GalleryContent::Unknown)
    });

    match (ytdlp_video, gallery_content) {
        (true, Some(GalleryContent::Image)) => {
            // Both viable: honor sticky, default Video.
            match host_sticky {
                Some(ProbeOutcome::Gallery) => ProbeOutcome::Gallery,
                _ => ProbeOutcome::VideoYtDlp {
                    gallery_toggle: true,
                },
            }
        }
        (true, _) => ProbeOutcome::VideoYtDlp {
            gallery_toggle: false,
        },
        (false, Some(GalleryContent::Video)) => ProbeOutcome::VideoGalleryDl,
        (false, Some(GalleryContent::Image)) => match host_sticky {
            Some(ProbeOutcome::VideoYtDlp { .. }) => ProbeOutcome::VideoYtDlp {
                gallery_toggle: true,
            },
            _ => ProbeOutcome::Gallery,
        },
        (false, _) => ProbeOutcome::None,
    }
}

/// Best-effort title from probe metadata. First non-empty wins:
/// `title` → `user`/`author` → `category` → URL-derived fallback.
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
    // Fallback: last URL path segment, via the url crate (not string splitting).
    url::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.path_segments()
                .and_then(|segs| segs.filter(|s| !s.is_empty()).last())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| url.to_string())
}

/// Run the gallery-dl probe: `gallery-dl --config-ignore -j --no-download --range 0 <url>`.
///
/// Returns `Ok(Some(items))` on success, `Ok(None)` when the URL is unsupported
/// (exit 64) or the probe otherwise yields nothing usable, `Err` on timeout or
/// spawn failure (the card then offers "Add anyway").
pub async fn probe_gallery(
    tool: &PathBuf,
    url: &str,
    cookies_file: Option<&PathBuf>,
) -> Result<Option<Vec<GalleryItem>>, crate::gallery::GalleryError> {
    use crate::gallery::GalleryError;
    let mut cmd = tokio::process::Command::new(tool);
    cmd.arg("--config-ignore")
        .arg("-j")
        .arg("--no-download")
        .arg("--range")
        .arg("0")
        .arg(url);
    if let Some(cookies) = cookies_file {
        cmd.arg("-C").arg(cookies);
    }
    let output = tokio::time::timeout(PROBE_TIMEOUT, cmd.output())
        .await
        .map_err(|_| GalleryError::install("gallery-dl probe timed out"))?
        .map_err(|e| GalleryError::install(format!("couldn't spawn gallery-dl: {e}")))?;

    // Exit 64 = NoExtractorError: unsupported URL, not a failure.
    if output.status.code() == Some(64) {
        return Ok(None);
    }
    if !output.status.success() {
        return Ok(None);
    }
    let items: Vec<GalleryItem> = serde_json::from_slice(&output.stdout)
        .map_err(|e| GalleryError::install(format!("couldn't parse gallery-dl output: {e}")))?;
    if items.is_empty() {
        return Ok(None);
    }
    Ok(Some(items))
}

/// Count files without downloading: `gallery-dl --config-ignore -g --no-download <url>`.
/// Returns the line count of direct file URLs, or `None` on any failure
/// (the card then shows an indeterminate count).
pub async fn count_gallery_files(
    tool: &PathBuf,
    url: &str,
    cookies_file: Option<&PathBuf>,
) -> Option<u32> {
    let mut cmd = tokio::process::Command::new(tool);
    cmd.arg("--config-ignore")
        .arg("-g")
        .arg("--no-download")
        .arg(url);
    if let Some(cookies) = cookies_file {
        cmd.arg("-C").arg(cookies);
    }
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
        // Sticky gallery wins when both viable.
        assert_eq!(
            route(true, Some(&images), Some(ProbeOutcome::Gallery)),
            ProbeOutcome::Gallery
        );
        // No sticky → default Video.
        assert_eq!(
            route(true, Some(&images), None),
            ProbeOutcome::VideoYtDlp {
                gallery_toggle: true
            }
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
        let item = item("jpg");
        // category is "test" (non-empty), so it wins over the URL.
        assert_eq!(probe_title(&item, "https://example.com/p/123"), "test");
    }

    #[test]
    fn probe_json_parses_real_shape() {
        // Shape of `gallery-dl -j` output (one item).
        let json = r#"[{
            "filename": "12345.jpg",
            "extension": "jpg",
            "category": "instagram",
            "title": "Sunset",
            "user": "someuser"
        }]"#;
        let items: Vec<GalleryItem> = serde_json::from_str(json).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].filename, "12345.jpg");
        assert_eq!(
            GalleryContent::from_extension(&items[0].extension),
            GalleryContent::Image
        );
        assert_eq!(probe_title(&items[0], "https://example.com/"), "Sunset");
    }
}
