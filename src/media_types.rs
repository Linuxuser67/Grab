//! Media identity types: what a resource is, not how it downloads.

/// `video-quality` values + per-item quality; index-aligned with Preferences/dialog combos. Labels translated at call sites.
pub const VIDEO_QUALITY_VALUES: &[&str] = &["best", "2160p", "1440p", "1080p", "720p", "480p"];

/// Default quality when nothing is stored (or the stored value is unknown).
pub fn default_video_quality() -> String {
    "1080p".to_string()
}

/// Combo index for stored value; unknown falls back so hand-edited dconf can't desync combo. Pure.
pub(crate) fn combo_index(values: &[&str], value: &str, fallback: usize) -> usize {
    values.iter().position(|v| *v == value).unwrap_or(fallback)
}

/// Stored value for index; out-of-range falls back instead of panicking. Pure.
pub(crate) fn combo_value<'a>(values: &[&'a str], index: usize, fallback: &'a str) -> &'a str {
    values.get(index).copied().unwrap_or(fallback)
}

/// Model index for stored quality; unknown falls back to the 1080p row.
pub fn quality_index(value: &str) -> usize {
    combo_index(VIDEO_QUALITY_VALUES, value, 3)
}

/// Stored value for combo index; out-of-range falls back to 1080p.
pub fn quality_value(index: usize) -> &'static str {
    combo_value(VIDEO_QUALITY_VALUES, index, "1080p")
}

/// Per-row video choices; bundled to stay under the argument-count lint.
#[derive(Debug, Clone)]
pub struct VideoChoices {
    pub quality: String,
    /// Per-download audio-only switch; deliberately not a preference (no global default).
    pub audio_only: bool,
    pub video_format_id: Option<String>,
    pub is_live: bool,
    /// Playlist entry id this row was picked from; worker selects by id since collection URLs re-resolve the tray.
    pub playlist_item_id: Option<String>,
}

/// Where a resource comes from; serialized into the queue file, stable across releases.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum VideoSource {
    /// Ordinary file for the plain HTTP engine.
    Direct,
    /// Media behind a page needing yt-dlp; identity is the page URL, `media_url`/`expires_at` transient, quality/audio per-item choices.
    Page {
        page_url: String,
        media_url: Option<String>,
        expires_at: Option<i64>,
        #[serde(default = "default_video_quality")]
        quality: String,
        #[serde(default)]
        audio_only: bool,
        /// Whether the page is live; persisted so restores keep live behavior, refreshed on resolve.
        #[serde(default)]
        is_live: bool,
        /// Pinned format id (`None` = preset decides); falls back to preset when id vanishes.
        #[serde(default)]
        video_format_id: Option<String>,
        /// Playlist entry id (see `VideoChoices`); persisted so restores still resolve the picked story.
        #[serde(default)]
        playlist_item_id: Option<String>,
    },
}

/// Collection flavor; affects picker labels and single-story retargeting, not the pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaylistKind {
    Playlist,
    Stories,
    Highlights,
}

impl PlaylistKind {
    /// Classify from probed URL with extractor key backup; heuristic, wrong guess only mislabels count.
    pub(crate) fn classify(page_url: &str, extractor_key: &str) -> Self {
        let url = page_url.to_lowercase();
        let key = extractor_key.to_lowercase();
        if url.contains("highlight") || key.contains("highlight") {
            Self::Highlights
        } else if url.contains("/stories/") || key.contains("stories") {
            Self::Stories
        } else {
            Self::Playlist
        }
    }
}

/// One collection entry; picker needs identity + label, rows re-resolve item pages at download time.
/// `page_url` stays `String`: story retargeting rewrites it after construction.
#[derive(Clone, Debug)]
pub struct PlaylistItem {
    /// 1-based position; extractor contract, picker addresses by position.
    #[allow(dead_code)]
    pub index: usize,
    pub id: Box<str>,
    pub title: Box<str>,
    /// Canonical per-item page URL — the identity queued and persisted.
    pub page_url: String,
    /// Duration seconds, when the listing reports one.
    pub duration: Option<i64>,
}

/// Probed collection: picker lists items, queue gets one row per choice.
/// Boxed strings/slice: built once at probe, never grown afterwards.
#[derive(Clone, Debug)]
pub struct PlaylistInfo {
    /// Extractor collection id; contract only, picker uses positions.
    #[allow(dead_code)]
    pub id: Box<str>,
    pub title: Box<str>,
    /// The collection URL that was probed.
    pub page_url: Box<str>,
    pub kind: PlaylistKind,
    /// Entries reported by the extractor, before the picker cap.
    pub total: usize,
    pub items: Box<[PlaylistItem]>,
}
