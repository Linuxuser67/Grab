//! Probe identity: what a URL is and what a probe resolved to. Leaf module
//! (media types + yt_dlp model + url crate), reached through the `video` facade.

use yt_dlp::model::format::{Format, Protocol};
use yt_dlp::model::selector::VideoCodecPreference;
use yt_dlp::model::{DrmStatus, FORMAT_URL_LIFETIME, Video};

/// Hosts routed through the video extractor instead of the plain HTTP engine.
/// Suffix-matched (`music.youtube.com` counts), lowercase. DRM-free sites only:
/// DRM-walled ones fail at selection time, so listing them would promise what
/// the pipeline refuses to fetch. This list is the only place that decides
/// what a "video page" is.
const VIDEO_DOMAINS: &[&str] = &[
    "youtube.com",
    "youtu.be",
    "vimeo.com",
    "dailymotion.com",
    "tiktok.com",
    "twitch.tv",
    "kick.com",
    "rumble.com",
    "streamable.com",
    "reddit.com",
    "twitter.com",
    "x.com",
    "bilibili.com",
    "instagram.com",
    "facebook.com",
    "fb.watch",
    "threads.com",
    "bsky.app",
    "pinterest.com",
    "pin.it",
    "tumblr.com",
    "vk.com",
    "ok.ru",
    "coub.com",
    "bitchute.com",
    "odysee.com",
    "rutube.ru",
    "nicovideo.jp",
    "ted.com",
    "archive.org",
    "drive.google.com",
    "dropbox.com",
    "mediafire.com",
    "loom.com",
    "wistia.com",
    "wistia.net",
    "soundcloud.com",
    "bandcamp.com",
];

/// Decide whether `url` goes through the video extractor. Callers pass a fully
/// qualified `http(s)` URL (normalization runs first); anything else
/// (magnets, bare hosts, unknown schemes) is
/// [`VideoSource::Direct`](crate::media_types::VideoSource::Direct).
pub fn classify(url: &str) -> crate::media_types::VideoSource {
    match url::Url::parse(url) {
        Ok(u) if matches!(u.scheme(), "http" | "https") => {
            let is_video = u.host_str().is_some_and(video_domain);
            if is_video {
                crate::media_types::VideoSource::Page {
                    page_url: u.to_string(),
                    media_url: None,
                    expires_at: None,
                    quality: crate::media_types::default_video_quality(),
                    audio_only: false,
                    is_live: false,
                    video_format_id: None,
                    playlist_item_id: None,
                }
            } else {
                crate::media_types::VideoSource::Direct
            }
        }
        _ => crate::media_types::VideoSource::Direct,
    }
}

/// Convenience predicate for the enqueue/restore paths.
pub fn is_video_page(url: &str) -> bool {
    matches!(classify(url), crate::media_types::VideoSource::Page { .. })
}

/// Whether a resolved preview still matches the dialog's current text. Both the
/// dialog kick and the submit gate must agree: the extractor canonicalizes page
/// URLs (youtu.be → youtube.com/watch), so comparing the stored canonical URL
/// against the typed text would trap Add in a re-resolve loop. The round-trip
/// key (which exact text was resolved) is the stable one.
pub fn preview_fresh(info: &Option<ProbeResult>, last_ok: &str, url: &str) -> bool {
    !url.is_empty() && last_ok == url && info.is_some()
}

pub(crate) fn video_domain(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    VIDEO_DOMAINS
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

/// Whether a resolved page carries anything fetchable: at least one
/// plain-HTTPS or HLS-manifest format with a URL and no DRM. The dialog
/// probe offers the video path for unlisted pages on this.
pub fn has_fetchable_media(video: &Video) -> bool {
    video.formats.iter().any(|f| {
        matches!(f.protocol, Protocol::Https | Protocol::M3U8Native)
            && !matches!(f.has_drm, Some(DrmStatus::Yes))
            && f.download_info
                .url
                .as_deref()
                .is_some_and(|u| !u.trim().is_empty())
    })
}

/// Extraction result, kept deliberately small: the queue row needs the
/// title/duration and the *page URL* for expiry-safe re-resolve.
/// Strings are boxed: every field is written once at probe time, never mutated.
#[derive(Clone, Debug)]
pub struct VideoInfo {
    pub title: Box<str>,
    /// Duration in seconds; read only in tests today.
    #[allow(dead_code)]
    pub duration: Option<i64>,
    /// Preformatted duration from the extractor (e.g. "41:21").
    pub duration_string: Option<Box<str>>,
    /// Canonical page URL — the identity persisted across restarts.
    pub page_url: Box<str>,
    /// Unix time after which every resolved format URL is stale; written at
    /// probe time for expiry-safe re-resolve, no reader yet.
    #[allow(dead_code)]
    pub expires_at: Option<i64>,
    /// Pinnable video-only formats, tallest first. Computed once at resolve.
    pub formats: Box<[VideoFormatOption]>,
    /// Whether the page is currently live. Decides stop-and-keep behavior
    /// for HLS captures; refreshed on every resolve.
    pub is_live: bool,
    /// Whether the resolved formats include anything fetchable (see
    /// [`has_fetchable_media`]): the dialog offers the video path for
    /// unlisted pages on this instead of the domain list.
    pub fetchable: bool,
}

impl VideoInfo {
    pub(crate) fn from(v: &Video, fallback_page: &str, newest_first: bool) -> Self {
        let page_url = v
            .webpage_url
            .as_deref()
            .filter(|u| !u.is_empty())
            .unwrap_or(fallback_page)
            .to_string();
        let expires_at = v
            .formats
            .iter()
            .filter_map(|f| f.available_at)
            .min()
            .map(|t| t + FORMAT_URL_LIFETIME);
        Self {
            title: v.title.clone().into_boxed_str(),
            duration: v.duration,
            duration_string: v.duration_string.clone().map(String::into_boxed_str),
            page_url: page_url.into_boxed_str(),
            expires_at,
            formats: video_format_options(v, newest_first),
            is_live: v.is_live.unwrap_or(false),
            fetchable: has_fetchable_media(v),
        }
    }
}

#[derive(Clone, Debug)]
pub enum ProbeResult {
    Single(VideoInfo),
    Playlist(crate::media_types::PlaylistInfo),
}

impl ProbeResult {
    /// Canonical URL of the probed page; the round-trip freshness key.
    pub fn page_url(&self) -> &str {
        match self {
            ProbeResult::Single(v) => &v.page_url,
            ProbeResult::Playlist(p) => &p.page_url,
        }
    }

    /// Display title: video title or collection title.
    pub fn title(&self) -> &str {
        match self {
            ProbeResult::Single(v) => &v.title,
            ProbeResult::Playlist(p) => &p.title,
        }
    }

    /// Whether the probe found anything worth offering: a fetchable
    /// single, or a collection with at least one queueable item.
    pub fn fetchable(&self) -> bool {
        match self {
            ProbeResult::Single(v) => v.fetchable,
            ProbeResult::Playlist(p) => !p.items.is_empty(),
        }
    }
}

/// What one video worker attempt resolved to. Playlist-shaped pages expand
/// into per-item rows instead of failing.
#[derive(Debug)]
pub enum VideoOutcome {
    /// Bytes finished (or adopted) at dest.
    Finished(u64),
    /// The page resolved playlist-shaped with no picked entry: queue
    /// one row per item (see `expand_child_target`) instead of failing.
    /// Boxed: the enum would otherwise pay for the largest variant on every
    /// value, including the common `Finished(u64)`.
    Expand(Box<crate::media_types::PlaylistInfo>),
    /// Stopped before finishing; the canceller owns the row state.
    Aborted,
}

/// What one extractor dump resolved to: a single video, or a collection.
/// `parse_playlist_json` decides the shape.
#[derive(Debug)]
pub(crate) enum FetchedVideo {
    Single {
        video: Box<Video>,
        /// 0-based position of the picked entry in the re-resolved playlist
        /// (`None` for a plain single video). The download scopes itself with
        /// `--playlist-items` instead of `--no-playlist` so a picked row
        /// fetches its own entry, not the tray's first.
        playlist_index: Option<usize>,
    },
    Playlist(crate::media_types::PlaylistInfo),
}

/// One video-only format, deduplicated and labeled for the dialog combo.
/// `Box<str>`/slices: the list is built once at resolve and never mutated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFormatOption {
    pub id: Box<str>,
    pub label: Box<str>,
    pub height: u32,
}

/// Best directly-fetchable video-only stream per height (codec rank, then
/// filesize). Plain HTTPS without DRM only; muxed and audio-only streams never
/// qualify. Pure.
fn best_direct_by_height(
    formats: &[Format],
    newest_first: bool,
) -> std::collections::HashMap<u32, &Format> {
    use std::collections::HashMap;
    let mut best: HashMap<u32, &Format> = HashMap::new();
    for f in formats {
        if f.protocol != Protocol::Https {
            continue;
        }
        if matches!(f.has_drm, Some(DrmStatus::Yes)) {
            continue;
        }
        let vcodec = f.codec_info.video_codec.as_deref().unwrap_or("none");
        if vcodec == "none" {
            continue;
        }
        let acodec = f.codec_info.audio_codec.as_deref().unwrap_or("none");
        if acodec != "none" {
            continue;
        }
        let Some(h) = f.video_resolution.height.filter(|&h| h > 0) else {
            continue;
        };
        let replace = match best.get(&h) {
            None => true,
            Some(cur) => {
                let cur_rank = codec_rank(
                    cur.codec_info.video_codec.as_deref().unwrap_or("none"),
                    newest_first,
                );
                let new_rank = codec_rank(vcodec, newest_first);
                (new_rank < cur_rank)
                    || (new_rank == cur_rank
                        && filesize_of(f).unwrap_or(0) > filesize_of(cur).unwrap_or(0))
            }
        };
        if replace {
            best.insert(h, f);
        }
    }
    best
}

/// Short transport/codec tag for a picker row: HLS variants show their
/// transport, not a codec ffmpeg will consume. Remote strings are allowlisted
/// to label-safe chars so bidi overrides or newlines can't spoof the dropdown.
/// Pure.
fn format_short_label(f: &Format) -> String {
    if f.protocol == Protocol::M3U8Native {
        return "HLS".to_string();
    }
    let raw = f.codec_info.video_codec.as_deref().unwrap_or("?");
    let clean: String = raw
        .split('.')
        .next()
        .unwrap_or("?")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '+')
        .take(16)
        .collect();
    if clean.is_empty() {
        "?".to_string()
    } else {
        clean
    }
}

/// Rank a codec string against an ordered table of prefix groups: first
/// matching group wins, anything else ranks past the table. Shared by both
/// priority modes so the two orders can't drift. Pure.
fn codec_rank_in(vcodec: &str, table: &[&[&str]]) -> u8 {
    let c = vcodec.to_ascii_lowercase();
    table
        .iter()
        .position(|group| group.iter().any(|p| c.starts_with(p)))
        .map(|i| i as u8)
        .unwrap_or(table.len() as u8)
}

/// Newest-first codec rank, mirroring yt-dlp's `+vcodec:av01` sort: AV1 wins
/// ties at the same height, then VP9, HEVC, AVC1, anything else — never at the
/// cost of resolution.
const NEWEST_ORDER: &[&[&str]] = &[
    &["av01", "av1"],
    &["vp9"],
    &["hev1", "hvc1", "h265"],
    &["avc1", "h264"],
];

fn codec_rank_newest(vcodec: &str) -> u8 {
    codec_rank_in(vcodec, NEWEST_ORDER)
}

/// Compatibility-first rank for players without HEVC/AV1 decoders: H.264
/// first, then VP9, HEVC, AV1, anything else.
const COMPATIBLE_ORDER: &[&[&str]] = &[
    &["avc1", "h264"],
    &["vp9"],
    &["hev1", "hvc1", "h265"],
    &["av01", "av1"],
];

fn codec_rank_compatible(vcodec: &str) -> u8 {
    codec_rank_in(vcodec, COMPATIBLE_ORDER)
}

/// Rank one codec under the stored priority mode.
pub(crate) fn codec_rank(vcodec: &str, newest_first: bool) -> u8 {
    if newest_first {
        codec_rank_newest(vcodec)
    } else {
        codec_rank_compatible(vcodec)
    }
}

/// Extractor codec preference matching the priority mode: the crate falls back
/// to all formats when the preferred codec is absent.
pub(crate) fn codec_preference(newest_first: bool) -> VideoCodecPreference {
    if newest_first {
        VideoCodecPreference::AV1
    } else {
        VideoCodecPreference::AVC1
    }
}

pub(crate) fn filesize_of(f: &Format) -> Option<u64> {
    f.file_info
        .filesize
        .or(f.file_info.filesize_approx)
        .filter(|&n| n > 0)
        .map(|n| n as u64)
}
/// Listable video-only formats for one video: best per height (codec rank, then
/// filesize), tallest first. HLS variants fill heights with no direct stream
/// (the worker pulls those via ffmpeg); muxed files stay on the automatic path.
pub fn video_format_options(video: &Video, newest_first: bool) -> Box<[VideoFormatOption]> {
    let mut best = best_direct_by_height(&video.formats, newest_first);
    // HLS gap-fill: heights with no direct stream still list, so the worker
    // can route them to ffmpeg.
    for f in &video.formats {
        if HlsSel::from_format(f).is_none() {
            continue;
        }
        let Some(h) = f.video_resolution.height.filter(|&h| h > 0) else {
            continue;
        };
        best.entry(h).or_insert(f);
    }
    let mut out: Vec<VideoFormatOption> = best
        .into_iter()
        .map(|(height, f)| {
            let short = format_short_label(f);
            let label = match filesize_of(f) {
                Some(n) => format!("{height}p · {short} · {}", fmt_video_bytes(n)),
                None => format!("{height}p · {short}"),
            };
            VideoFormatOption {
                id: f.format_id.clone().into_boxed_str(),
                label: label.into_boxed_str(),
                height,
            }
        })
        .collect();
    out.sort_by_key(|a| std::cmp::Reverse(a.height));
    out.into_boxed_slice()
}

/// Human size for format labels. Decimal units, one fraction digit.
fn fmt_video_bytes(n: u64) -> String {
    const GB: f64 = 1_000_000_000.0;
    const MB: f64 = 1_000_000.0;
    let f = n as f64;
    if f >= GB {
        format!("{:.1} GB", f / GB)
    } else if f >= MB {
        format!("{:.1} MB", f / MB)
    } else {
        format!("{n} B")
    }
}

/// One HLS manifest variant selected by the planner: the exact
/// format id runners pin, plus its height for caps and logging.
#[derive(Debug, Clone)]
pub(crate) struct HlsSel {
    pub(crate) format_id: String,
    pub(crate) height: Option<u32>,
}

impl HlsSel {
    /// Build from an extractor format: manifest protocol, DRM-free, with a
    /// playlist URL. The URL is validated but not stored — runners re-resolve
    /// by id. `pub(crate)` for the planner in `video_plan.rs`.
    pub(crate) fn from_format(f: &Format) -> Option<Self> {
        if f.protocol != Protocol::M3U8Native {
            return None;
        }
        if matches!(f.has_drm, Some(DrmStatus::Yes)) {
            return None;
        }
        f.download_info.url.clone().filter(|u| !u.is_empty())?;
        Some(Self {
            format_id: f.format_id.clone(),
            height: f.video_resolution.height.filter(|&h| h > 0),
        })
    }
}
