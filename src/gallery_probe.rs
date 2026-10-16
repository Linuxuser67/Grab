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
/// Some extractors omit `extension`/`category`, hence the defaults.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct GalleryItem {
    pub filename: String,
    #[serde(default)]
    pub extension: String,
    #[serde(default)]
    pub category: String,
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// gallery-dl `-j` message types (first array element).
/// Verified against gallery-dl 1.32.16: messages are `[type, ...]` arrays.
mod msg_type {
    /// Directory/context message: `[2, {metadata}]`.
    pub const DIRECTORY: i64 = 2;
    /// File message: `[3, "url", {metadata}]`.
    pub const FILE: i64 = 3;
    /// Error message: `[-1, {error, message}]`.
    pub const ERROR: i64 = -1;
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
        let lower = ext.to_ascii_lowercase();
        if crate::gallery_argv::VIDEO_EXTENSIONS.contains(&lower.as_str()) {
            Self::Video
        } else if crate::gallery_argv::IMAGE_EXTENSIONS.contains(&lower.as_str()) {
            Self::Image
        } else {
            Self::Unknown
        }
    }
}

/// Whether a URL points directly to a media file (by extension). Used to skip
/// the gallery-dl probe when yt-dlp fails: a direct `.mp4` link isn't a gallery
/// page, so trying gallery-dl is wasted time.
pub fn url_is_direct_media(url: &str) -> bool {
    // Also cover audio extensions for direct downloads.
    const AUDIO: &[&str] = &[
        "mp3", "m4a", "aac", "ogg", "oga", "opus", "flac", "wav", "wma", "alac", "aiff",
    ];
    // Strip query/fragment, then take the path after the authority so a
    // host-only URL like https://example.mov isn't read as extension "mov".
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    let path = match without_query.split_once("://") {
        Some((_, rest)) => rest.split('/').skip(1).collect::<Vec<_>>().join("/"),
        None => without_query.to_string(),
    };
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    if ext.is_empty() || ext.contains('/') {
        return false;
    }
    !matches!(
        GalleryContent::from_extension(&ext),
        GalleryContent::Unknown
    ) || AUDIO.contains(&ext.as_str())
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StickyMode {
    Video,
    Gallery,
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
            match host_sticky {
                Some(StickyMode::Gallery) => ProbeOutcome::Gallery,
                _ => ProbeOutcome::VideoYtDlp {
                    gallery_toggle: true,
                },
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

/// Whether a probe error should be surfaced to the user instead of falling
/// through to the plain-download path. Auth/HTTP errors mean the URL needs
/// login; "unsupported URL" and timeouts mean gallery-dl just can't handle it.
pub fn should_surface(e: &GalleryError) -> bool {
    match e {
        GalleryError::Probe(msg) => {
            msg.contains("AuthenticationError")
                || msg.contains("AuthorizationError")
                || msg.contains("AuthRequired")
                || msg.contains("ChallengeError")
                || msg.contains("HttpError")
        }
        // Install/runtime errors (spawn failures, timeouts) fall through.
        _ => false,
    }
}

/// Whether a yt-dlp failure is worth trying gallery-dl for.
/// Only unsupported-URL and login-required errors: gallery-dl handles sites
/// yt-dlp can't (e.g. Instagram stories). Network errors, rate limits,
/// geo-blocks, and private/removed videos are not worth the probe delay
/// (up to 45s: 30s cookie export + 15s probe).
/// Pure for tests.
pub fn ytdlp_error_worth_gallery_dl(error: &str) -> bool {
    let lower = error.to_lowercase();
    // YouTube bot check: gallery-dl won't help. Must come first because the
    // message also carries a --cookies-from-browser hint.
    if lower.contains("confirm you're not a bot") {
        return false;
    }
    // Unsupported URL: yt-dlp doesn't know this site.
    if lower.contains("unsupported url") {
        return true;
    }
    // Login required for content access.
    // Covers: "You need to log in to access this content" (Instagram),
    // "This video is only available for registered users" (yt-dlp default),
    // "redirected to the login page", "locked behind the login page".
    if lower.contains("log in to access")
        || lower.contains("login required")
        || lower.contains("registered users")
        || lower.contains("login page")
        || lower.contains("locked behind the login")
    {
        return true;
    }
    false
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

/// Run the gallery-dl probe: `gallery-dl --config-ignore -j --no-download --range 1-5 <url>`.
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
        .arg("1-5")
        // Don't let gallery-dl write back to Grab's staged cookies file.
        .arg("-o")
        .arg("extractor.cookies-update=false");
    if let Some(cookies) = cookies_file {
        cmd.arg("-C").arg(cookies);
    }
    // `--` last: guards against URLs starting with `-` being parsed as options.
    cmd.arg("--").arg(url);
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
    // `-j` emits `[type, ...]` arrays: [2, {dir}] for directory context,
    // [3, "url", {meta}] for files, [-1, {error, message}] for errors.
    let messages: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)
        .map_err(|e| GalleryError::probe(format!("couldn't parse gallery-dl output: {e}")))?;
    let items = parse_probe_messages(&messages)?;
    if items.is_empty() {
        return Ok(None);
    }
    Ok(Some(items))
}

/// Parse `gallery-dl -j` message arrays into file items.
///
/// Message types (first array element): 2 = directory context, 3 = file,
/// -1 = error. Directory metadata is inherited by file items lacking
/// `title`/`user`/`author`. Returns `Err` on a type -1 error message.
fn parse_probe_messages(messages: &[serde_json::Value]) -> Result<Vec<GalleryItem>, GalleryError> {
    let mut items = Vec::new();
    // A `-1` error doesn't discard files already seen; stored here and
    // returned only if no items were collected.
    let mut pending_err: Option<GalleryError> = None;
    // Directory context from the most recent type-2 message; file items
    // inherit its title/user when they don't carry their own.
    let mut dir_ctx: Option<serde_json::Map<String, serde_json::Value>> = None;
    for m in messages {
        let t = m.get(0).and_then(|v| v.as_i64());
        match t {
            Some(msg_type::FILE) => {
                let meta = match m.get(2) {
                    Some(v) => v.clone(),
                    None => {
                        tracing::trace!(message = ?m, "gallery-dl probe file message without metadata");
                        continue;
                    }
                };
                let mut item: GalleryItem = match serde_json::from_value(meta) {
                    Ok(item) => item,
                    Err(e) => {
                        tracing::trace!(error = %e, "gallery-dl probe couldn't parse file metadata");
                        continue;
                    }
                };
                // Inherit directory title/user when the file lacks them.
                if let Some(dir) = dir_ctx.as_ref() {
                    for key in ["title", "user", "author"] {
                        if !item.extra.contains_key(key)
                            && let Some(v) = dir.get(key)
                        {
                            item.extra.insert(key.to_string(), v.clone());
                        }
                    }
                }
                // Stash the file URL from m[1] for potential use.
                if let Some(url) = m.get(1).and_then(|v| v.as_str()) {
                    item.extra.insert(
                        "_gallery_dl_url".to_string(),
                        serde_json::Value::String(url.to_string()),
                    );
                }
                items.push(item);
            }
            Some(msg_type::DIRECTORY) => {
                if let Some(obj) = m.get(1).and_then(|v| v.as_object()) {
                    dir_ctx = Some(obj.clone());
                }
            }
            Some(msg_type::ERROR) => {
                let err = m
                    .get(1)
                    .and_then(|v| v.get("error"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let msg = m
                    .get(1)
                    .and_then(|v| v.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                // Don't discard usable items: store the error and continue.
                // If we got files, return them; otherwise return the error.
                pending_err = Some(GalleryError::probe(format!("gallery-dl: {err}: {msg}")));
                continue;
            }
            _ => {
                tracing::trace!(message = ?m, "gallery-dl probe unknown message");
            }
        }
    }
    // If we got items despite an error, return them (log the error).
    // Otherwise return the pending error, or Ok(empty) if none.
    if !items.is_empty() {
        if let Some(e) = pending_err {
            tracing::warn!(error = %e, "gallery-dl probe had error but returned items");
        }
        Ok(items)
    } else if let Some(e) = pending_err {
        Err(e)
    } else {
        Ok(items)
    }
}

/// Count files without downloading: `gallery-dl --config-ignore -g --no-download <url>`.
/// Returns the line count of direct file URLs, or `None` on any failure
/// (the card then shows an indeterminate count).
pub async fn count_gallery_files(
    tool: &Path,
    url: &str,
    cookies_file: Option<&Path>,
    filter: crate::gallery_argv::GalleryFilter,
) -> Option<u32> {
    let argv = crate::gallery_argv::count_argv(tool, url, cookies_file, filter);
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

    #[test]
    fn should_surface_auth_errors() {
        assert!(should_surface(&GalleryError::probe(
            "gallery-dl: AuthenticationError: login required"
        )));
        assert!(should_surface(&GalleryError::probe(
            "gallery-dl: AuthorizationError: forbidden"
        )));
        assert!(should_surface(&GalleryError::probe(
            "gallery-dl: AuthRequired: account credentials required"
        )));
        assert!(should_surface(&GalleryError::probe(
            "gallery-dl: ChallengeError: captcha"
        )));
        assert!(should_surface(&GalleryError::probe(
            "gallery-dl: HttpError: 404"
        )));
    }

    #[test]
    fn should_not_surface_other_errors() {
        assert!(!should_surface(&GalleryError::probe(
            "gallery-dl: NoExtractorError: unsupported URL"
        )));
        assert!(!should_surface(&GalleryError::probe(
            "gallery-dl: NotFoundError: nothing found"
        )));
        assert!(!should_surface(&GalleryError::install("timeout")));
        assert!(!should_surface(&GalleryError::runtime("spawn failed")));
    }

    #[test]
    fn ytdlp_error_worth_gallery_dl_cases() {
        // Unsupported URL: worth trying gallery-dl.
        assert!(ytdlp_error_worth_gallery_dl(
            "ERROR: [generic] Unsupported URL: https://example.com/gallery"
        ));
        // Login required for content: worth trying (e.g. Instagram stories).
        assert!(ytdlp_error_worth_gallery_dl(
            "ERROR: [instagram:story] You need to log in to access this content."
        ));
        assert!(ytdlp_error_worth_gallery_dl(
            "ERROR: Login required to access this content"
        ));
        // yt-dlp default login message.
        assert!(ytdlp_error_worth_gallery_dl(
            "ERROR: This video is only available for registered users. Use --cookies-from-browser or --username"
        ));
        // Instagram login-page redirects.
        assert!(ytdlp_error_worth_gallery_dl(
            "ERROR: [instagram] The webpage request was redirected to the login page"
        ));
        assert!(ytdlp_error_worth_gallery_dl(
            "ERROR: [instagram] Main webpage is locked behind the login page"
        ));
        // YouTube bot check: not worth it (gallery-dl won't help).
        // Must stay false even though the message carries a cookies hint.
        assert!(!ytdlp_error_worth_gallery_dl(
            "ERROR: [youtube] Sign in to confirm you're not a bot. Use --cookies-from-browser"
        ));
        // Network errors: not worth the probe delay.
        assert!(!ytdlp_error_worth_gallery_dl(
            "ERROR: Unable to download webpage: <urlopen error [Errno -2] Name or service not known>"
        ));
        // Private video: not worth it.
        assert!(!ytdlp_error_worth_gallery_dl(
            "ERROR: [youtube] Private video. Sign in if you've been granted access to this video"
        ));
        // Removed video: not worth it.
        assert!(!ytdlp_error_worth_gallery_dl(
            "ERROR: [youtube] Video unavailable"
        ));
    }

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
        assert_eq!(GalleryContent::from_extension("flv"), GalleryContent::Video);
        assert_eq!(GalleryContent::from_extension("wmv"), GalleryContent::Video);
        assert_eq!(GalleryContent::from_extension("mpg"), GalleryContent::Video);
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
            GalleryContent::from_extension("heic"),
            GalleryContent::Image
        );
        assert_eq!(
            GalleryContent::from_extension("HEIF"),
            GalleryContent::Image
        );
        assert_eq!(
            GalleryContent::from_extension("xyz"),
            GalleryContent::Unknown
        );
    }

    #[test]
    fn url_is_direct_media_check() {
        assert!(url_is_direct_media("https://example.com/video.mp4"));
        assert!(url_is_direct_media("https://example.com/photo.HEIC"));
        assert!(url_is_direct_media(
            "https://example.com/song.mp3?token=abc#frag"
        ));
        assert!(!url_is_direct_media("https://example.com/page.html"));
        assert!(!url_is_direct_media("https://example.com/noext"));
        assert!(!url_is_direct_media("https://example.com/"));
        // Host-only URL: .mov is a TLD, not a file extension.
        assert!(!url_is_direct_media("https://example.mov"));
        assert!(!url_is_direct_media("https://example.mov/"));
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
            route(true, Some(&images), Some(StickyMode::Gallery)),
            ProbeOutcome::Gallery
        );
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
    fn probe_json_parses_real_array_format() {
        // Real `gallery-dl -j` output: [2, {dir}] then [3, "url", {meta}].
        let json = r#"[
            [2, {"category": "instagram", "subcategory": "posts", "user": "someuser"}],
            [3, "https://example.com/12345.jpg", {
                "filename": "12345.jpg",
                "extension": "jpg",
                "category": "instagram",
                "title": "Sunset"
            }]
        ]"#;
        let messages: Vec<serde_json::Value> = serde_json::from_str(json).unwrap();
        let items = parse_probe_messages(&messages).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].filename, "12345.jpg");
        assert_eq!(
            GalleryContent::from_extension(&items[0].extension),
            GalleryContent::Image
        );
        assert_eq!(probe_title(&items[0], "https://example.com/"), "Sunset");
        // File URL stashed from m[1].
        assert_eq!(
            items[0]
                .extra
                .get("_gallery_dl_url")
                .and_then(|v| v.as_str()),
            Some("https://example.com/12345.jpg")
        );
    }

    #[test]
    fn probe_json_inherits_directory_user() {
        // File without user inherits from the directory message.
        let json = r#"[
            [2, {"category": "instagram", "user": "someuser"}],
            [3, "https://example.com/12345.jpg", {
                "filename": "12345.jpg",
                "extension": "jpg"
            }]
        ]"#;
        let messages: Vec<serde_json::Value> = serde_json::from_str(json).unwrap();
        let items = parse_probe_messages(&messages).unwrap();
        assert_eq!(items.len(), 1);
        // extension/category default when omitted (S7).
        assert_eq!(items[0].extension, "jpg");
        assert_eq!(items[0].category, "");
        assert_eq!(probe_title(&items[0], "https://example.com/"), "someuser");
    }

    #[test]
    fn probe_json_error_message_returns_err() {
        // Type -1 error message becomes GalleryError::Probe.
        let json = r#"[
            [-1, {"error": "AuthenticationError", "message": "login required"}]
        ]"#;
        let messages: Vec<serde_json::Value> = serde_json::from_str(json).unwrap();
        let err = parse_probe_messages(&messages).unwrap_err();
        assert!(matches!(err, GalleryError::Probe(_)));
        assert!(err.to_string().contains("AuthenticationError"));
    }

    #[test]
    fn probe_error_after_files_returns_items() {
        // A -1 error after files doesn't discard them.
        let json = r#"[
            [3, "https://example.com/1.jpg", {"filename": "1.jpg", "extension": "jpg"}],
            [3, "https://example.com/2.jpg", {"filename": "2.jpg", "extension": "jpg"}],
            [-1, {"error": "HttpError", "message": "404"}]
        ]"#;
        let messages: Vec<serde_json::Value> = serde_json::from_str(json).unwrap();
        let items = parse_probe_messages(&messages).unwrap();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn probe_json_route_gallery_for_images() {
        // End-to-end: real-format messages route to Gallery.
        let json = r#"[
            [2, {"category": "test"}],
            [3, "https://example.com/a.jpg", {"filename": "a.jpg", "extension": "jpg"}],
            [3, "https://example.com/b.png", {"filename": "b.png", "extension": "png"}]
        ]"#;
        let messages: Vec<serde_json::Value> = serde_json::from_str(json).unwrap();
        let items = parse_probe_messages(&messages).unwrap();
        assert_eq!(route(false, Some(&items), None), ProbeOutcome::Gallery);
    }
}
