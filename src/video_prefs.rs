//! Preference combos + active-value resolution: codec priority, subtitle
//! language and remux target labels/indices plus the allowlisted actives jobs
//! consume. Leaf module (media types + gettext).

use crate::video_tools::COOKIES_BROWSERS;
use gettextrs::gettext;

/// Translated combo labels, index-aligned with [`COOKIES_BROWSERS`]. Browser
/// names are proper nouns and stay untranslated; only None is prose.
pub fn cookies_browser_labels() -> Vec<String> {
    let mut labels = vec![
        "Brave".to_string(),
        "Chrome".to_string(),
        "Chromium".to_string(),
        "Edge".to_string(),
        "Firefox".to_string(),
        "Opera".to_string(),
        "Vivaldi".to_string(),
        "Whale".to_string(),
        "Zen".to_string(),
    ];
    labels.insert(0, gettext("None"));
    labels
}

/// Combo index for a stored browser value. Unknown values fall back to
/// None rather than selecting a browser the user didn't pick.
pub fn cookies_browser_index(value: &str) -> usize {
    crate::media_types::combo_index(COOKIES_BROWSERS, value, 0)
}

/// Stored value for a combo index. Out-of-range indexes fall back to off.
pub fn cookies_browser_value(index: usize) -> &'static str {
    crate::media_types::combo_value(COOKIES_BROWSERS, index, "none")
}

/// Codec priority modes for the `video-codec-priority` setting.
pub const CODEC_PRIORITY_NEWEST: &str = "newest";

pub const CODEC_PRIORITY_COMPATIBLE: &str = "compatible";

pub const CODEC_PRIORITY_VALUES: &[&str] = &[CODEC_PRIORITY_NEWEST, CODEC_PRIORITY_COMPATIBLE];

/// Translated combo labels, index-aligned with [`CODEC_PRIORITY_VALUES`].
pub fn codec_priority_labels() -> Vec<String> {
    vec![gettext("Newest first"), gettext("Most compatible")]
}

/// Combo index for a stored priority value. Unknown values fall back
/// to newest (the historical behavior).
pub fn codec_priority_index(value: &str) -> usize {
    crate::media_types::combo_index(CODEC_PRIORITY_VALUES, value, 0)
}

/// Stored value for a combo index. Out-of-range indexes fall back to newest.
pub fn codec_priority_value(index: usize) -> &'static str {
    crate::media_types::combo_value(CODEC_PRIORITY_VALUES, index, CODEC_PRIORITY_NEWEST)
}

/// Browser clients yt-dlp's `--impersonate` accepts, index-aligned with
/// [`impersonate_labels`]. Empty = none: yt-dlp's documented default (opt-in),
/// since its docs warn that forcing impersonation on every request may hurt
/// speed and stability.
pub const IMPERSONATE_VALUES: &[&str] = &["", "chrome", "edge", "safari"];

/// Translated combo labels, index-aligned with [`IMPERSONATE_VALUES`].
/// The client names are brands and stay untranslated.
pub fn impersonate_labels() -> Vec<String> {
    vec![
        gettext("None"),
        "Chrome".to_string(),
        "Edge".to_string(),
        "Safari".to_string(),
    ]
}

/// Combo index for a stored impersonation target. Unknown values fall back
/// to none.
pub fn impersonate_index(value: &str) -> usize {
    crate::media_types::combo_index(IMPERSONATE_VALUES, value, 0)
}

/// Stored value for a combo index. Out-of-range indexes fall back to none.
pub fn impersonate_value(index: usize) -> &'static str {
    crate::media_types::combo_value(IMPERSONATE_VALUES, index, "")
}

/// yt-dlp subtitle language codes offered in preferences, index-aligned
/// with [`subtitle_language_labels`]. `off` disables subtitle downloads.
pub(crate) const SUBTITLE_LANGUAGE_VALUES: &[&str] = &[
    "off", "en", "ar", "de", "es", "fr", "hi", "id", "it", "ja", "ko", "nl", "pl", "pt", "ru",
    "tr", "vi", "zh",
];

pub fn subtitle_language_labels() -> Vec<String> {
    vec![
        gettext("Off"),
        gettext("English"),
        gettext("Arabic"),
        gettext("German"),
        gettext("Spanish"),
        gettext("French"),
        gettext("Hindi"),
        gettext("Indonesian"),
        gettext("Italian"),
        gettext("Japanese"),
        gettext("Korean"),
        gettext("Dutch"),
        gettext("Polish"),
        gettext("Portuguese"),
        gettext("Russian"),
        gettext("Turkish"),
        gettext("Vietnamese"),
        gettext("Chinese"),
    ]
}

/// Combo index for a stored subtitle language code. Unknown or empty
/// values fall back to Off (the default: no subtitles).
pub fn subtitle_language_index(value: &str) -> usize {
    crate::media_types::combo_index(SUBTITLE_LANGUAGE_VALUES, value, 0)
}

/// Stored code for a combo index. Out-of-range indexes fall back to Off.
pub fn subtitle_language_value(index: usize) -> &'static str {
    crate::media_types::combo_value(SUBTITLE_LANGUAGE_VALUES, index, "off")
}

/// Active raw setting for one job: trimmed and lowercased, then allowlisted
/// against `values` minus `"off"`. `off`, empty and unknown codes (hand-edited
/// dconf) all resolve to `None`: a code yt-dlp would only warn about is never
/// requested, and the value reaching the CLI always comes from the fixed list.
fn allowlisted_active(raw: &str, values: &[&str]) -> Option<String> {
    let norm = raw.trim().to_ascii_lowercase();
    values
        .iter()
        .find(|v| **v == norm && **v != "off")
        .map(|v| v.to_string())
}

/// Active subtitle language for one job (see [`allowlisted_active`]).
pub(crate) fn subtitle_lang_active(raw: &str) -> Option<String> {
    allowlisted_active(raw, SUBTITLE_LANGUAGE_VALUES)
}

/// Offered subtitle languages that produce sidecars: the full list minus the
/// `off` marker. Single source so writers and deleters cannot drift.
pub(crate) fn subtitle_content_languages() -> impl Iterator<Item = &'static str> {
    SUBTITLE_LANGUAGE_VALUES
        .iter()
        .copied()
        .filter(|l| *l != "off")
}

/// yt-dlp remux target containers offered in preferences, index-aligned
/// with [`remux_video_labels`]. `off` disables remuxing.
pub(crate) const REMUX_VIDEO_VALUES: &[&str] = &["off", "mp4", "mkv", "webm"];

pub fn remux_video_labels() -> Vec<String> {
    vec![
        gettext("Off"),
        // Container names are proper nouns: never translated.
        "MP4".to_string(),
        "MKV".to_string(),
        "WebM".to_string(),
    ]
}

/// Combo index for a stored remux target. Unknown or empty values fall
/// back to Off (the default).
pub fn remux_video_index(value: &str) -> usize {
    crate::media_types::combo_index(REMUX_VIDEO_VALUES, value, 0)
}

/// Stored code for a combo index. Out-of-range indexes fall back to Off.
pub fn remux_video_value(index: usize) -> &'static str {
    crate::media_types::combo_value(REMUX_VIDEO_VALUES, index, "off")
}

/// Active remux target for one job (see [`allowlisted_active`]).
pub(crate) fn remux_video_active(raw: &str) -> Option<String> {
    allowlisted_active(raw, REMUX_VIDEO_VALUES)
}

/// Subtitle fetch flags for the media leg: exact language with
/// automatic-caption fallback, converted to SRT. The language is resolved
/// beforehand by `resolve_subtitle_lang` (preferred if the video offers it,
/// else the English fallback), and `--embed-subs` is added by the argv
/// builders when the embed preference is on. Conversion points at Grab's
/// resolved ffmpeg.
pub(crate) fn subtitle_cli_args(lang: &str) -> Vec<String> {
    vec![
        "--write-subs".to_string(),
        "--sub-langs".to_string(),
        lang.to_string(),
        "--write-auto-subs".to_string(),
        "--convert-subs".to_string(),
        "srt".to_string(),
    ]
}
