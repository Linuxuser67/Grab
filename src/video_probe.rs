//! Page probing + playlist parsing: URL predicates, Drive fallback, extractor-JSON
//! sanitation and collection parsing. Leaf module: the dialog, worker resolve and
//! engine expansion consume it through the `video` facade.

use crate::video_tools::VideoError;
use gettextrs::gettext;
#[cfg(test)]
use std::time::{SystemTime, UNIX_EPOCH};
use yt_dlp::model::Video;

/// Google Drive file id from share/download URLs (`file/d/<id>` or
/// `uc`/`open`/`download?id=<id>`). yt-dlp's Drive extractor is playback-API-only
/// (PDFs, docs and zips fail it with HTTP 400 instead of formats), so the dialog
/// falls back to this id for a direct download instead of stranding those files.
pub fn drive_file_id(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let host = parsed
        .host_str()?
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if !(host == "drive.google.com"
        || host.ends_with(".drive.google.com")
        || host == "docs.google.com"
        || host.ends_with(".docs.google.com")
        || host == "drive.usercontent.google.com"
        || host.ends_with(".drive.usercontent.google.com"))
    {
        return None;
    }
    let valid = |id: &str| {
        id.len() >= 28
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    };
    if let Some(mut segs) = parsed.path_segments()
        && segs.next() == Some("file")
        && segs.next() == Some("d")
        && let Some(id) = segs.next()
        && valid(id)
    {
        return Some(id.to_string());
    }
    if let Some(first) = parsed.path_segments().and_then(|mut s| s.next())
        && matches!(first, "uc" | "open" | "download")
    {
        for (key, value) in parsed.query_pairs() {
            if key == "id" && valid(&value) {
                return Some(value.into_owned());
            }
        }
    }
    None
}

/// Direct download URL for a Drive share link: the export endpoint with
/// `confirm=t` (same shape yt-dlp uses, so virus-scan-sized files skip the
/// confirmation page). The plain engine follows the redirect and the server
/// filename wins via Content-Disposition.
pub fn drive_direct_url(url: &str) -> Option<String> {
    drive_file_id(url).map(|id| {
        format!("https://drive.usercontent.google.com/download?id={id}&export=download&confirm=t")
    })
}

/// Unix timestamp now, seconds — the probe clock for `is_expired`.
/// Test-only: no production caller yet (`VideoInfo.expires_at` has no reader).
#[cfg(test)]
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Whether a resolved media URL is no longer usable. `None` ("not resolved")
/// counts as expired so restores always re-extract until a fresh URL is stored.
/// Test-only: no production caller yet.
#[cfg(test)]
pub fn is_expired(expires_at: Option<i64>) -> bool {
    match expires_at {
        None => true,
        Some(t) => now_unix() >= t,
    }
}

/// Whether the string is an HTTP(S) URL: the only scheme the dialog probes for
/// media (magnets belong to their own flows).
pub fn is_http_url(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https"))
}

/// Lowercase extension of the URL's last path segment, if it has one.
/// Query/fragment are stripped by the URL parser; a dot is required —
/// extensionless terminal segments (`/md`) are not extensions.
fn url_path_ext(parsed: &url::Url) -> Option<&str> {
    parsed
        .path_segments()
        .and_then(|mut s| s.next_back())
        .and_then(|last| last.rsplit_once('.').map(|(_, e)| e))
        .filter(|e| !e.is_empty())
}

/// Whether an HTTP(S) URL is obviously a direct file (not a page): its path ends
/// in a well-known extension. Such links skip the media probe — the plain engine
/// downloads them better anyway. Query/fragment stripped before matching, and
/// case-insensitive; a page masquerading as a file is the accepted residual.
pub fn is_direct_file_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    let Some(ext) = url_path_ext(&parsed) else {
        return false;
    };
    DIRECT_FILE_EXTS
        .binary_search(&ext.to_ascii_lowercase().as_str())
        .is_ok()
}

/// Whether an HTTP(S) URL points at a stream manifest: DASH (`mpd`) or a
/// playlist (`m3u8`, `m3u`, `pls`). These are never plain downloads — URI
/// activation opens the card (which probes them) instead of saving the
/// manifest as a file. Segments (`ts`) stay plain files: a lone segment is a
/// legitimate download.
pub fn is_stream_manifest_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    let Some(ext) = url_path_ext(&parsed) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "mpd" | "m3u8" | "m3u" | "pls"
    )
}

/// Extensions treated as direct files (never probed): installers, archives,
/// documents, ebooks, fonts, images, media, torrents and other unambiguous
/// downloads. Deliberately absent: pages and scripts, stream manifests (DASH
/// `mpd`; playlists `m3u8`, `m3u`, `pls`) and segments (`ts`). Sorted for
/// `binary_search` — keep it sorted; a test pins that.
pub(crate) const DIRECT_FILE_EXTS: &[&str] = &[
    "3g2",
    "3gp",
    "7z",
    "aac",
    "ac3",
    "ai",
    "aif",
    "aiff",
    "amr",
    "apk",
    "apks",
    "appimage",
    "arw",
    "asc",
    "avi",
    "avif",
    "azw",
    "azw3",
    "bin",
    "bmp",
    "bz2",
    "cab",
    "cbr",
    "cbz",
    "chm",
    "cr2",
    "crt",
    "crx",
    "css",
    "csv",
    "dat",
    "db",
    "deb",
    "djvu",
    "dmg",
    "dng",
    "doc",
    "docx",
    "dts",
    "eot",
    "eps",
    "epub",
    "exe",
    "f4v",
    "fb2",
    "flac",
    "flatpak",
    "flatpakref",
    "flv",
    "gif",
    "gpg",
    "gz",
    "heic",
    "heif",
    "ico",
    "img",
    "ipa",
    "iso",
    "jar",
    "jpeg",
    "jpg",
    "js",
    "json",
    "jxl",
    "key",
    "kra",
    "log",
    "lz4",
    "m2ts",
    "m4a",
    "m4b",
    "m4v",
    "md",
    "mid",
    "midi",
    "mjs",
    "mka",
    "mkv",
    "mobi",
    "mov",
    "mp3",
    "mp4",
    "mpeg",
    "mpg",
    "msi",
    "mts",
    "nef",
    "nzb",
    "odb",
    "odc",
    "odf",
    "odg",
    "odp",
    "ods",
    "odt",
    "oga",
    "ogg",
    "ogv",
    "opus",
    "otf",
    "ovpn",
    "par2",
    "pdf",
    "pem",
    "pkg",
    "ppt",
    "pptx",
    "psd",
    "qcow2",
    "rar",
    "raw",
    "rpm",
    "rtf",
    "run",
    "sfv",
    "sig",
    "snap",
    "sqlite",
    "srt",
    "svg",
    "tar",
    "tgz",
    "tif",
    "tiff",
    "torrent",
    "ttf",
    "txt",
    "vdi",
    "vhd",
    "vhdx",
    "vmdk",
    "vtt",
    "war",
    "wav",
    "webm",
    "wma",
    "wmv",
    "woff",
    "woff2",
    "xapk",
    "xcf",
    "xls",
    "xlsx",
    "xpi",
    "xz",
    "yaml",
    "yml",
    "zip",
    "zst",
];

/// Host part of a URL for logs; the journal gets the host, never the query string
/// (page URLs can carry tokens).
pub(crate) fn page_host(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_default()
}

/// Integer-valued fields the model demands as i64/u64 but extractors sometimes
/// emit as floats (Instagram reels report fractional durations). Truncation
/// matches the old display semantics.
const INT_FIELDS: &[&str] = &[
    "duration",
    "timestamp",
    "release_timestamp",
    "release_year",
    "view_count",
    "like_count",
    "comment_count",
    "channel_follower_count",
    "age_limit",
    "available_at",
    "filesize",
    "filesize_approx",
    "language_preference",
    "source_preference",
    "asr",
    "width",
    "height",
];

/// Truncate float values to integers for [`INT_FIELDS`]; anything else
/// (strings, bools, nulls, objects) is untouched.
fn coerce_int_fields(obj: &mut serde_json::Map<String, serde_json::Value>) {
    for key in INT_FIELDS {
        if let Some(v) = obj.get_mut(*key)
            && let Some(f) = v.as_f64()
        {
            *v = serde_json::json!(f.trunc() as i64);
        }
    }
}

/// Repair one format entry in place: neutral defaults for required scalars plus
/// transport/container inference for sparse extractors, so one sparse entry cannot
/// fail the whole parse.
fn sanitize_format_entry(entry: &mut serde_json::Map<String, serde_json::Value>) {
    coerce_int_fields(entry);
    entry.entry("format").or_insert(serde_json::json!(""));
    entry.entry("format_id").or_insert(serde_json::json!(""));
    entry.entry("http_headers").or_insert(serde_json::json!({}));
    // Missing transport with an http(s) URL: plain HTTPS is the only sane
    // default — without it the format is invisible to every selector below.
    if !entry.get("protocol").is_some_and(|v| v.is_string()) {
        let http = entry
            .get("url")
            .and_then(|u| u.as_str())
            .is_some_and(|u| u.starts_with("http://") || u.starts_with("https://"));
        if http {
            entry.insert("protocol".to_string(), serde_json::json!("https"));
        }
    }
    if !entry.get("ext").is_some_and(|v| v.is_string()) {
        // Missing container with a telling URL: sniff the path suffix
        // (containers only, never manifests or storyboards) so the Unknown
        // fallback can still adopt instead of failing the row.
        let suffix = entry
            .get("url")
            .and_then(|u| u.as_str())
            .and_then(|u| u.split(['?', '#']).next())
            .and_then(|p| p.rsplit('.').next())
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(suffix.as_str(), "mp4" | "webm" | "avi" | "flv" | "ts") {
            entry.insert("ext".to_string(), serde_json::json!(suffix));
        }
    }
    if entry.contains_key("fragments") {
        entry.insert("fragments".to_string(), serde_json::json!([]));
    }
}

/// Fill in fields the installed yt-dlp omits but the crate's model demands —
/// otherwise one sparse object (a thumbnail without `preference`, an x.com page
/// without `live_status`) fails the entire preview parse. Arrays Grab never reads
/// are dropped; formats are repaired entry by entry instead.
pub(crate) fn sanitize_video_json(value: &mut serde_json::Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    coerce_int_fields(obj);
    obj.entry("id").or_insert(serde_json::json!(""));
    obj.entry("title").or_insert(serde_json::json!(""));
    obj.entry("age_limit").or_insert(serde_json::json!(0));
    obj.entry("live_status").or_insert(serde_json::json!(""));
    obj.entry("playable_in_embed")
        .or_insert(serde_json::json!(false));
    obj.entry("extractor").or_insert(serde_json::json!(""));
    obj.entry("extractor_key").or_insert(serde_json::json!(""));
    if !obj.get("_version").is_some_and(|v| v.is_object()) {
        obj.insert(
            "_version".to_string(),
            serde_json::json!({"version": "", "repository": ""}),
        );
    } else if let Some(ver) = obj.get_mut("_version").and_then(|v| v.as_object_mut()) {
        // Present-but-sparse version blocks fail the parse the same way.
        ver.entry("version").or_insert(serde_json::json!(""));
        ver.entry("repository").or_insert(serde_json::json!(""));
    }
    // downloader_options is never read: a sparse object would fail the parse
    // for nothing.
    obj.remove("downloader_options");
    // Unread arrays/objects: always reset, not just when absent, so one sparse
    // entry cannot fail the video. Some extractors (TikTok) also emit explicit
    // nulls, which serde defaults don't cover (those only fill missing keys).
    // Formats are read by the pipeline, so they are repaired below instead.
    for key in ["thumbnails", "chapters", "tags", "categories"] {
        obj.insert(key.to_string(), serde_json::json!([]));
    }
    for key in ["subtitles", "automatic_captions"] {
        obj.insert(key.to_string(), serde_json::json!({}));
    }
    obj.insert("heatmap".to_string(), serde_json::Value::Null);
    if !obj.get("formats").is_some_and(|v| v.is_array()) {
        obj.insert("formats".to_string(), serde_json::json!([]));
    }
    if let Some(formats) = obj.get_mut("formats").and_then(|f| f.as_array_mut()) {
        formats.retain(|f| f.is_object());
        for format in formats.iter_mut() {
            if let Some(entry) = format.as_object_mut() {
                sanitize_format_entry(entry);
            }
        }
    }
}

/// Picker cap: a thousand-row dialog is not a picker anymore. The item count shown
/// notes the truncation.
pub(crate) const MAX_PLAYLIST_ITEMS: usize = 500;

/// True only when the collection held more items than the fetch cap: lenient
/// parsing can drop unusable entries too, so `total > items.len()` alone is no
/// evidence of truncation.
pub(crate) fn playlist_truncated(pl: &crate::media_types::PlaylistInfo) -> bool {
    pl.total > pl.items.len() && pl.items.len() == MAX_PLAYLIST_ITEMS
}

/// Lenient i64 for extractor JSON (durations arrive as floats, e.g. fractional
/// Instagram reel durations); floats truncate toward zero, negatives included —
/// callers clamp display.
fn json_i64(value: &serde_json::Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|f| f.trunc() as i64))
}

/// Non-empty string field, borrowed.
fn json_str<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

/// Parse one flat-playlist entry into a queueable item. Entries are usually stubs
/// (`_type: "url"`), but fully-extracted video objects pass through the same
/// field reads. Nulls, non-objects and entries without a usable page URL are
/// dropped.
fn parse_playlist_item(
    entry: &serde_json::Value,
    position: usize,
) -> Option<crate::media_types::PlaylistItem> {
    entry.as_object()?;
    let is_http = |u: &str| u.starts_with("http://") || u.starts_with("https://");
    // `webpage_url` is the canonical item page; bare `url` doubles as one for
    // extractors that only emit it (it is the video id on YouTube).
    let page_url = json_str(entry, "webpage_url")
        .filter(|u| is_http(u))
        .or_else(|| json_str(entry, "url").filter(|u| is_http(u)))?;
    let id = json_str(entry, "id")
        .map(str::to_string)
        .unwrap_or_else(|| format!("item-{}", position + 1));
    let title = json_str(entry, "title")
        .map(str::to_string)
        .unwrap_or_else(|| id.clone());
    let index = entry
        .get("playlist_index")
        .and_then(json_i64)
        .and_then(|i| usize::try_from(i).ok())
        .filter(|&i| i > 0) // `playlist_index` is documented 1-based; 0 is bogus.
        .unwrap_or(position + 1);
    Some(crate::media_types::PlaylistItem {
        index,
        id: id.into_boxed_str(),
        title: title.into_boxed_str(),
        page_url: page_url.to_string(),
        duration: entry.get("duration").and_then(json_i64),
    })
}

/// Parse playlist-shaped probe JSON (`_type: "playlist"` with an `entries` array,
/// as `--flat-playlist --dump-single-json` emits). Returns `None` for
/// single-video JSON so the caller falls through to the video path.
pub(crate) fn parse_playlist_json(
    value: &serde_json::Value,
    url: &str,
) -> Option<crate::media_types::PlaylistInfo> {
    let obj = value.as_object()?;
    let entries = obj.get("entries").and_then(|e| e.as_array())?;
    // A single video never carries `entries`; belt-and-braces in case an
    // extractor nests one anyway.
    if obj.get("_type").and_then(|t| t.as_str()) == Some("video") {
        return None;
    }
    let extractor_key = obj
        .get("extractor_key")
        .and_then(|k| k.as_str())
        .unwrap_or("");
    let total = entries.len();
    let items: Vec<crate::media_types::PlaylistItem> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| parse_playlist_item(e, i))
        .take(MAX_PLAYLIST_ITEMS)
        .collect();
    Some(crate::media_types::PlaylistInfo {
        id: json_str(value, "id").unwrap_or("").into(),
        title: json_str(value, "title").unwrap_or(url).into(),
        page_url: json_str(value, "webpage_url").unwrap_or(url).into(),
        kind: crate::media_types::PlaylistKind::classify(url, extractor_key),
        total,
        items: items.into_boxed_slice(),
    })
}

/// Instagram's story extractor stamps every tray entry with the pasted URL. For a
/// single-story link that is the *pasted* story, not the entry's: a row queued
/// from such an entry would re-download the wrong story. Point story items at the
/// owner's story tray instead — the worker re-resolves the tray and selects the
/// picked entry by id. Highlights keep their URL: a highlight *is* the collection.
pub(crate) fn retarget_story_items(url: &str, playlist: &mut crate::media_types::PlaylistInfo) {
    if playlist.kind != crate::media_types::PlaylistKind::Stories {
        return;
    }
    let Some(tray) = story_tray_url(url) else {
        return;
    };
    for item in &mut playlist.items {
        item.page_url = tray.clone();
    }
}

/// Non-empty path segments of an http(s) Instagram page URL. Shared preamble for
/// the story helpers below; each keeps its own shape validation.
fn instagram_path_segments(url: &str) -> Option<Vec<String>> {
    let parsed = url::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    if !matches!(
        parsed.host_str(),
        Some("instagram.com") | Some("www.instagram.com")
    ) {
        return None;
    }
    Some(
        parsed
            .path_segments()?
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// `https://www.instagram.com/stories/<user>/<story-id>/` → the owner's
/// story tray `https://www.instagram.com/stories/<user>/`. Returns `None` for
/// tray URLs (nothing to retarget), highlights and non-story links.
pub(crate) fn story_tray_url(url: &str) -> Option<String> {
    let segments = instagram_path_segments(url)?;
    let [stories, user, story_id] = segments.as_slice() else {
        return None;
    };
    if stories != "stories" {
        return None;
    }
    if user.eq_ignore_ascii_case("highlights") {
        return None;
    }
    if !story_id.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(format!("https://www.instagram.com/stories/{user}/"))
}

/// Queue target for one playlist entry under worker expansion: a
/// directly-addressable page. Story segments resolve to their own pages;
/// everything else keeps its listed page. `None` for entries with no http page,
/// or pointing back at the probed collection itself — compared against both the
/// row's normalized URL and the extractor's canonical one, since yt-dlp
/// canonicalizes (`youtu.be`→`watch`, trailing slashes) past what intake
/// normalized. A nested playlist would otherwise re-expand forever; accepted
/// residual, bounded by the 500-item cap.
pub(crate) fn expand_child_target(
    parent_url: &str,
    playlist_url: &str,
    item: &crate::media_types::PlaylistItem,
) -> Option<String> {
    if let Some(segment) = story_segment_url(parent_url, &item.id) {
        return Some(segment);
    }
    let page = item.page_url.as_str();
    if !is_http_url(page) || page == parent_url || page == playlist_url {
        return None;
    }
    Some(page.to_string())
}

/// Instagram story username from a tray (or single-story) URL:
/// `.../stories/<user>[/<id>/]`. `None` for highlights and non-story links.
fn story_tray_user(url: &str) -> Option<String> {
    let segments = instagram_path_segments(url)?;
    let [stories, user, ..] = segments.as_slice() else {
        return None;
    };
    if stories != "stories" {
        return None;
    }
    // Instagram usernames: 1–30 chars of letters, digits, periods and
    // underscores. Anything else is not a user tray — fail early to the
    // tray + entry-id fallback instead of queueing a late failure.
    if user.chars().count() > 30
        || !user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
    {
        return None;
    }
    if user.eq_ignore_ascii_case("highlights") {
        return None;
    }
    Some((*user).to_string())
}

/// Instagram shortcode alphabet (`_id_to_pk` in yt-dlp's extractor):
/// standard base64 order with `-_`, no padding.
const INSTA_SHORTCODE_TABLE: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Numeric media id for an Instagram shortcode, mirroring yt-dlp's
/// `_id_to_pk` (trailing 28-char private-post suffix stripped first).
/// Operates on bytes: the alphabet is ASCII-only, and a byte-index cut on
/// a `&str` could split a codepoint and panic on untrusted extractor input.
pub(crate) fn insta_shortcode_to_pk(shortcode: &str) -> Option<u64> {
    let bytes = shortcode.as_bytes();
    let code: &[u8] = if bytes.len() > 28 {
        &bytes[..bytes.len() - 28]
    } else {
        bytes
    };
    // Real shortcodes are ~11 chars; anything much longer is junk, not a future
    // format — fail closed to the tray fallback instead of queueing a bogus
    // segment page.
    if code.is_empty() || code.len() > 16 {
        return None;
    }
    let mut pk: u64 = 0;
    for &b in code {
        let digit = INSTA_SHORTCODE_TABLE.iter().position(|&t| t == b)? as u64;
        pk = pk.checked_mul(64)?.checked_add(digit)?;
    }
    // No real media id is zero (all-A input decodes to 0).
    if pk == 0 {
        return None;
    }
    Some(pk)
}

/// Directly-addressable page for one picked story segment:
/// `.../stories/<user>/<numeric-id>/`. Rows queued with this re-resolve the
/// segment itself as a single video — queuing the tray URL instead downloads
/// whatever the tray resolves to once per row (N copies of story one). Highlights
/// keep the tray, as does anything unparseable (the worker then falls back to tray
/// + entry-id selection).
pub(crate) fn story_segment_url(tray_url: &str, shortcode: &str) -> Option<String> {
    let user = story_tray_user(tray_url)?;
    let pk = insta_shortcode_to_pk(shortcode)?;
    Some(format!("https://www.instagram.com/stories/{user}/{pk}/"))
}

/// Worker error when the page resolved playlist-shaped and no entry could be
/// selected: a routing bug when the row was never picked from a playlist, an
/// expired story when it was.
pub(crate) fn playlist_resolve_error(playlist_item_id: Option<&str>) -> VideoError {
    VideoError::fetch(gettext(if playlist_item_id.is_some() {
        "the story is no longer available"
    } else {
        "the link opened a collection, not a single video"
    }))
}

/// The picked playlist entry for a row whose page re-resolves playlist-shaped
/// (stories/highlights: every entry carries the collection URL, so there is no
/// per-item page). Matches on the entry id persisted at pick time; `None`
/// when not picked or the entry is gone. Also returns the 0-based position,
/// so the download scopes itself with `--playlist-items` (1-based) instead of
/// silently taking the first entry via `--no-playlist`.
pub(crate) fn pick_playlist_entry(
    value: &serde_json::Value,
    playlist_item_id: Option<&str>,
) -> Option<(serde_json::Value, usize)> {
    let want = playlist_item_id?;
    value
        .get("entries")
        .and_then(|entries| entries.as_array())?
        .iter()
        .enumerate()
        .find(|(_, entry)| entry.get("id").and_then(|id| id.as_str()) == Some(want))
        .map(|(index, entry)| (entry.clone(), index))
}

/// Parse one video-shaped dump into the worker's [`Video`] model,
/// stamping formats with their video id.
pub(crate) fn parse_single_video(mut value: serde_json::Value) -> Result<Video, VideoError> {
    sanitize_video_json(&mut value);
    let mut video: Video = serde_json::from_value(value).map_err(VideoError::fetch)?;
    for format in &mut video.formats {
        format.video_id = Some(video.id.clone());
    }
    Ok(video)
}
