//! Stream planning: which video/audio/HLS streams an attempt fetches, in
//! priority order. Leaf module (video_types + video_quality + yt_dlp model).

use crate::video_quality::{quality_height, selector_for_quality};
use crate::video_tools::VideoError;
use crate::video_types::{HlsSel, codec_preference, filesize_of};
use yt_dlp::model::format::{Extension, Format, FormatType, Protocol};
use yt_dlp::model::{DrmStatus, Video};

/// Smallest height at or above the cap, else the tallest; `None` takes the
/// tallest. Fetchability filtering stays with the callers. Pure.
fn pick_at_or_above<T: Clone>(mut cands: Vec<(T, u32)>, want: Option<u32>) -> Option<T> {
    cands.sort_by_key(|(_, h)| *h);
    match want {
        Some(cap) => cands
            .iter()
            .find(|(_, h)| *h >= cap)
            .or_else(|| cands.last())
            .map(|(item, _)| item.clone()),
        None => cands.pop().map(|(item, _)| item),
    }
}

/// Best muxed (audio+video) file for a height cap, same semantics as
/// [`select_hls_format`]. Only directly fetchable files qualify, so a DRM or
/// link-less entry can never shadow a playable one.
fn select_muxed_format(formats: &[Format], want: Option<u32>) -> Option<StreamSel> {
    let cands: Vec<(StreamSel, u32)> = formats
        .iter()
        .filter(|f| f.format_type().is_audio_and_video())
        .filter_map(|f| {
            let h = f.video_resolution.height.filter(|&h| h > 0)?;
            StreamSel::from_format(f).ok().map(|sel| (sel, h))
        })
        .collect();
    pick_at_or_above(cands, want)
}

/// Height of one format id in fresh metadata, for comparing an
/// adopted single file against the HLS preset below.
fn format_height(formats: &[Format], id: &str) -> Option<u32> {
    formats
        .iter()
        .find(|f| f.format_id == id)
        .and_then(|f| f.video_resolution.height.filter(|&h| h > 0))
}

/// Best HLS variant for a height cap: smallest height at or above the
/// cap, else the tallest available. Mirrors the crate's
/// closest-at-or-above preset semantics.
pub(crate) fn select_hls_format(formats: &[Format], want: Option<u32>) -> Option<HlsSel> {
    // Extractor order is arbitrary: sorting ascending makes the capped match
    // genuinely the smallest height at or above it. Height-less variants
    // sort as 0, matching the old inline order.
    let cands: Vec<(HlsSel, u32)> = formats
        .iter()
        .filter_map(HlsSel::from_format)
        .map(|s| {
            let h = s.height.unwrap_or(0);
            (s, h)
        })
        .collect();
    pick_at_or_above(cands, want)
}

/// Find one HLS variant by dialog-pinned id.
pub(crate) fn find_hls_format(formats: &[Format], id: &str) -> Option<HlsSel> {
    formats
        .iter()
        .find(|f| f.format_id == id)
        .and_then(HlsSel::from_format)
}

/// Find one format by id, accepting only what the pipeline can fetch.
/// `None` covers unknown ids and HLS/DRM/missing-URL formats alike: the
/// caller falls back to the quality preset.
pub(crate) fn find_usable_format(formats: &[Format], id: &str) -> Option<StreamSel> {
    formats
        .iter()
        .find(|f| f.format_id == id)
        .and_then(|f| StreamSel::from_format(f).ok())
}

/// Planned streams for one attempt: direct splits, an adopted single
/// file, or an HLS variant. Pure over the extracted metadata, so unit
/// tests can pin the selection order without spawning tools.
pub(crate) struct StreamPlan {
    pub(crate) video_sel: Option<StreamSel>,
    pub(crate) audio_sel: Option<StreamSel>,
    pub(crate) hls_sel: Option<HlsSel>,
}

/// Resolve which streams an attempt fetches, in priority order:
///
/// 1. A dialog-pinned HLS id resolves first — [`find_usable_format`] takes
///    only plain HTTPS, so without this the pin would be dropped and then
///    shadowed by the muxed adoption below (x.com VODs).
/// 2. Direct splits: pinned HTTPS id, else the quality preset.
/// 3. Single-part adoption for a still-missing side, skipped once a pinned
///    HLS resolved or the request already holds both splits.
/// 4. The HLS preset is the last resort, except when the preset is taller
///    yet within the cap: Best match means best across transports.
pub(crate) fn plan_streams(
    video: &Video,
    quality: &str,
    audio_only_request: bool,
    video_format_id: Option<&str>,
    newest_codecs: bool,
    item_id: u64,
) -> StreamPlan {
    use yt_dlp::VideoSelection as _;
    // Pins and presets resolve for audio-only rows too: with no direct audio
    // they feed the HLS extract path instead of failing the row.
    let pinned_hls: Option<HlsSel> =
        video_format_id.and_then(|id| find_hls_format(&video.formats, id));
    let mut video_sel: Option<StreamSel> = if audio_only_request {
        None
    } else if pinned_hls.is_some() {
        // Explicit HLS pick: no direct selection and no fallback
        // chatter — the HLS path below consumes the pin.
        None
    } else if let Some(pinned) = video_format_id {
        find_usable_format(&video.formats, pinned).or_else(|| {
            tracing::info!(
                item_id,
                pinned,
                "pinned video format gone, falling back to preset"
            );
            video
                .select_video_format(
                    selector_for_quality(quality),
                    codec_preference(newest_codecs),
                )
                .and_then(|f| StreamSel::from_format(f).ok())
        })
    } else {
        video
            .select_video_format(
                selector_for_quality(quality),
                codec_preference(newest_codecs),
            )
            .and_then(|f| StreamSel::from_format(f).ok())
    };
    let mut audio_sel: Option<StreamSel> =
        select_audio_original_first(&video.formats).and_then(|f| StreamSel::from_format(f).ok());
    // Muxed-only sources (one file, both tracks): adopt it rather than fail on
    // the missing split. Unclassified video containers are the last resort
    // (TikTok-style sparse extractors) — only video extensions qualify, so
    // storyboards and manifests can never adopt here.
    let mut single_adopted = false;
    // Pre-adoption audio: restored if the HLS override below fires, so a
    // shadowed adoption never leaks its single-part mode into the HLS path.
    let pre_audio_sel = audio_sel.clone();
    if pinned_hls.is_none()
        && (audio_sel.is_none() || video_sel.is_none())
        && (!audio_only_request || audio_sel.is_none())
    {
        let muxed = video_sel.take_if(|v| v.has_audio).or_else(|| {
            // Height-aware: a bare "first muxed file" pick is lowest
            // first on extractors that list ascending (x.com), ignoring
            // the requested quality entirely.
            select_muxed_format(&video.formats, quality_height(quality))
        });
        if let Some(m) = muxed {
            audio_sel = Some(m);
            single_adopted = true;
        }
    }
    if pinned_hls.is_none()
        && !audio_only_request
        && video_sel.is_none()
        && (audio_sel.is_none() || !single_adopted)
    {
        // Height-aware like the muxed adoption above: a bare "first file"
        // pick is lowest first on extractors that list ascending, ignoring
        // the requested quality entirely. Height-less files keep the old
        // first-in-extractor-order pick. Only directly fetchable video
        // containers qualify, same as above.
        let mut sized: Vec<(StreamSel, u32)> = Vec::new();
        let mut first_unsized: Option<StreamSel> = None;
        for f in &video.formats {
            if f.format_type() != FormatType::Unknown
                || !matches!(
                    f.download_info.ext,
                    Extension::Mp4
                        | Extension::Webm
                        | Extension::Avi
                        | Extension::Flv
                        | Extension::Ts
                )
            {
                continue;
            }
            let Ok(sel) = StreamSel::from_format(f) else {
                continue;
            };
            if let Some(h) = f.video_resolution.height.filter(|&h| h > 0) {
                sized.push((sel, h));
            } else if first_unsized.is_none() {
                first_unsized = Some(sel);
            }
        }
        let adopted = pick_at_or_above(sized, quality_height(quality)).or(first_unsized);
        if let Some(m) = adopted {
            audio_sel = Some(m);
            // Covered by the HLS override below like the muxed adoption: a
            // taller in-cap variant still wins over a short direct file.
            single_adopted = true;
        }
    }
    // HLS fallback: nothing above is directly fetchable but manifest variants
    // exist. A resolved pin wins outright; the preset only runs when no direct
    // audio survived, except when the adoption took a muxed or unclassified
    // file shorter than an in-cap variant — then the variant wins, and ties
    // or over-cap variants keep the direct file.
    let hls_preset = select_hls_format(&video.formats, quality_height(quality));
    let mut hls_wins = false;
    if !audio_only_request
        && pinned_hls.is_none()
        && single_adopted
        && let (Some(preset), Some(muxed_h)) = (
            hls_preset.as_ref(),
            audio_sel
                .as_ref()
                .and_then(|s| format_height(&video.formats, &s.format_id)),
        )
        && let Some(preset_h) = preset.height
        && preset_h > muxed_h
        && quality_height(quality).is_none_or(|cap| preset_h <= cap)
    {
        hls_wins = true;
        audio_sel = pre_audio_sel;
    }
    let hls_sel: Option<HlsSel> = pinned_hls.or_else(|| {
        if hls_wins || audio_sel.is_none() {
            hls_preset.clone()
        } else {
            None
        }
    });
    StreamPlan {
        video_sel,
        audio_sel,
        hls_sel,
    }
}

/// One stream selected for download: owned values so the pipeline holds no
/// borrow on the extractor metadata across awaits.
#[derive(Debug, Clone)]
pub(crate) struct StreamSel {
    pub(crate) format_id: String,
    pub(crate) ext: String,
    /// Direct fetch URL. Written at plan time; the runners address
    /// splits by format id today, so only tests read this.
    #[allow(dead_code)]
    pub(crate) url: String,
    pub(crate) size: Option<u64>,
    /// Whether the stream may carry an audio track: true unless the
    /// extractor explicitly marks it audio-less (`acodec: none`). Sparse
    /// extractors (Instagram stories) omit `acodec` on muxed direct files,
    /// so absent means adoptable — matching yt-dlp's single-file `best`.
    pub(crate) has_audio: bool,
}

impl StreamSel {
    /// Build from an extractor format, accepting only what the pipeline can
    /// fetch: plain-HTTPS, DRM-free, with a URL. Anything else is a clean
    /// unavailable error — never playlist bytes merged as media, never
    /// encrypted garbage saved as a finished file.
    pub(crate) fn from_format(f: &Format) -> Result<Self, VideoError> {
        if f.protocol != Protocol::Https {
            return Err(VideoError::unavailable());
        }
        if matches!(f.has_drm, Some(DrmStatus::Yes)) {
            return Err(VideoError::unavailable());
        }
        Ok(Self {
            format_id: f.format_id.clone(),
            ext: f.download_info.ext.as_str().to_string(),
            url: f
                .download_info
                .url
                .clone()
                .ok_or_else(VideoError::unavailable)?,
            size: filesize_of(f),
            // Absent acodec is "unknown", not "none": sparse extractors
            // (Instagram stories) only set `acodec: none` when the media is
            // truly audio-less, and omit it on muxed direct files. Treating
            // unknown as adoptable keeps those single-file downloads alive;
            // an explicit `none` still fails cleanly when no audio split
            // exists.
            has_audio: f
                .codec_info
                .audio_codec
                .as_deref()
                .is_none_or(|c| c != "none"),
        })
    }
}

/// Best direct audio track, original language first.
///
/// YouTube ships auto-dubbed audio as separate tracks, and the crate's
/// `select_audio_format(Best, …)` has no language awareness, so a
/// higher-bitrate dub beats the original. Ranking `language_preference`
/// above the same quality/bitrate/rate/channel order — as upstream yt-dlp
/// does — keeps untagged extractors tying straight through unchanged.
/// Pure for tests.
pub(crate) fn select_audio_original_first(formats: &[Format]) -> Option<&Format> {
    formats
        .iter()
        .filter(|f| {
            // A video height means a video stream: some extractors' direct
            // MP4s report no video codec (the crate then calls them audio)
            // while carrying heights, and must never win the audio leg,
            // otherwise the bogus track suppresses the HLS preset downstream.
            f.is_audio() && f.video_resolution.height.is_none() && StreamSel::from_format(f).is_ok()
        })
        .max_by(|a, b| {
            let (al, aq, ab, aa, ac) = audio_rank_key(a);
            let (bl, bq, bb, ba, bc) = audio_rank_key(b);
            al.cmp(&bl)
                .then_with(|| aq.total_cmp(&bq))
                .then_with(|| ab.total_cmp(&bb))
                .then_with(|| aa.cmp(&ba))
                .then_with(|| ac.cmp(&bc))
        })
}

/// Audio rank key in yt-dlp `lang`-first order (language, quality, bitrate,
/// sample rate, channels). Missing fields score neutral, so untagged tracks
/// fall back to bitrate. The comparator keeps `total_cmp` on the float lanes
/// so the ordering stays total even though NaN cannot appear. Pure.
fn audio_rank_key(f: &Format) -> (i64, f64, f64, i64, i64) {
    (
        f.language_preference.unwrap_or(0),
        f.quality_info.quality.map(|q| *q).unwrap_or(0.0),
        f.rates_info.audio_rate.map(|r| *r).unwrap_or(0.0),
        f.codec_info.asr.unwrap_or(0),
        f.codec_info.audio_channels.unwrap_or(0),
    )
}
