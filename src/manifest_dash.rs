//! Static DASH parsing and period-aware segment resource planning.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;

use super::dash_roles::{self, DASH_ROLE_SCHEME};
use super::{MAX_MANIFEST_BYTES, ManifestByteRange};

use crate::manifest_types::MAX_DASH_SEGMENTS;

#[path = "manifest_dash_ll.rs"]
mod low_latency;
#[cfg(test)]
#[path = "manifest_dash_ll_tests.rs"]
mod low_latency_tests;
const MAX_REPRESENTATIONS: usize = 1_024;
const MIN_DYNAMIC_UPDATE_PERIOD: Duration = Duration::from_millis(500);
const MAX_DYNAMIC_UPDATE_PERIOD: Duration = Duration::from_secs(300);
// DASH-IF's current HTTPS identifier, the HTTP identifier emitted by FFmpeg,
// and the ISO trick-mode URN used by other DASH packagers.
const DASH_TRICK_MODE_SCHEMES: [&str; 3] = [
    "https://dashif.org/guidelines/trickmode",
    "http://dashif.org/guidelines/trickmode",
    "urn:mpeg:dash:trickmode:2016",
];

/// Errors returned when a DASH MPD is outside Grab's native v1 subset.
#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum DashError {
    #[error("invalid DASH manifest URL")]
    InvalidManifestUrl,
    #[error("DASH manifest is malformed or too large")]
    Parse,
    #[error("DASH presentation uses an unsupported period layout")]
    UnsupportedPresentation,
    #[error("DASH addressing or resource URL is unsupported")]
    UnsupportedAddressing,
    #[error("DASH segment description is invalid")]
    InvalidSegment,
    #[error("DASH presentation contains no supported video representation")]
    NoVideoRepresentations,
    #[error("DASH manifest contains too many representations or segments")]
    LimitExceeded,
    #[error("dynamic DASH has no currently available media segments")]
    NoAvailableSegments,
}

/// Whether a DASH representation carries video, audio, or supported text subtitles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DashTrackKind {
    Video,
    Audio,
    Subtitle,
}

/// A selectable video, audio, or supported subtitle representation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DashRepresentation {
    pub(crate) key: String,
    /// Stable live key also recognized when a dynamic MPD becomes static.
    pub(crate) live_key: String,
    /// Previous policy-sensitive key accepted while an existing capture is upgraded.
    pub(crate) legacy_live_key: Option<String>,
    pub(crate) representation_id: Option<String>,
    pub(crate) bandwidth: Option<u64>,
    pub(crate) width: Option<u64>,
    pub(crate) height: Option<u64>,
    pub(crate) mime_type: Option<String>,
    pub(crate) codecs: Option<String>,
    pub(crate) language: Option<String>,
    pub(crate) kind: DashTrackKind,
    pub(crate) is_default_audio: bool,
    pub(crate) is_default_subtitle: bool,
    adaptation_id: Option<String>,
    source: DashRepresentationSource,
    /// Additional period-local addressing sources in MPD presentation order.
    additional_sources: Vec<DashRepresentationSource>,
}

impl DashRepresentation {
    pub(crate) fn matches_key(&self, key: &str) -> bool {
        self.key == key || self.live_key == key || self.legacy_live_key.as_deref() == Some(key)
    }
}

/// Timing model shared by all representations in a DASH presentation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DashPresentation {
    Static,
    Dynamic(DashDynamicPresentation),
}

/// Server-clock information required to bound and refresh a dynamic MPD.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DashDynamicPresentation {
    pub(crate) availability_start_time: SystemTime,
    pub(crate) availability_end_time: Option<SystemTime>,
    pub(crate) server_now: SystemTime,
    pub(crate) period_start: Duration,
    pub(crate) minimum_update_period: Duration,
    pub(crate) time_shift_buffer_depth: Option<Duration>,
}

/// Parsed static or supported dynamic DASH presentation and its representations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DashManifestPlan {
    pub(crate) duration: Duration,
    pub(crate) presentation: DashPresentation,
    pub(crate) video_representations: Box<[DashRepresentation]>,
    pub(crate) audio_representations: Box<[DashRepresentation]>,
    pub(crate) subtitle_representations: Box<[DashRepresentation]>,
}

impl DashManifestPlan {
    /// Return the main audio representation, or the first audio representation.
    #[must_use]
    pub(crate) fn default_audio(&self) -> Option<&DashRepresentation> {
        self.audio_representations
            .iter()
            .find(|representation| representation.is_default_audio)
            .or_else(|| self.audio_representations.first())
    }
}

/// The type of remote resource in a selected DASH representation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DashResourceKind {
    Initialization,
    Segment,
}

/// One HTTPS resource needed to assemble a selected DASH representation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DashResource {
    pub(crate) url: Url,
    pub(crate) range: Option<ManifestByteRange>,
    pub(crate) filename: String,
    pub(crate) kind: DashResourceKind,
    /// SegmentTemplate Number or SegmentList index; initialization resources use None.
    pub(crate) segment_number: Option<u64>,
}

/// A signed nanosecond offset applied to one representation before muxing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DashTimestampOffset {
    nanoseconds: i128,
}

impl DashTimestampOffset {
    /// Format the offset for FFmpeg, omitting zero to preserve the old input path.
    #[must_use]
    pub(crate) fn ffmpeg_argument(self) -> Option<String> {
        if self.nanoseconds == 0 {
            return None;
        }

        let magnitude = self.nanoseconds.unsigned_abs();
        let seconds = magnitude / 1_000_000_000;
        let fraction = magnitude % 1_000_000_000;
        let sign = if self.nanoseconds.is_negative() {
            "-"
        } else {
            ""
        };
        Some(format!("{sign}{seconds}.{fraction:09}"))
    }
}

/// Expanded initialization and media resources for one representation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DashRepresentationPlan {
    pub(crate) representation: DashRepresentation,
    pub(crate) duration: Duration,
    pub(crate) presentation: DashPresentation,
    pub(crate) resources: Box<[DashResource]>,
    /// Maps the representation's sample timestamps onto the MPD presentation timeline.
    pub(crate) input_timestamp_offset: DashTimestampOffset,
    /// Whether the MPD advertises incomplete, progressively delivered media segments.
    pub(crate) low_latency: bool,
}

impl DashRepresentationPlan {
    /// Whether MP4 subtitle sample timestamps use the MPD's zero-origin
    /// presentation timeline for a single static DASH Period.
    ///
    /// The mapping is not safe when a Period has a non-zero start, the media
    /// timeline uses presentationTimeOffset, or multiple Period-local tracks
    /// have been concatenated.
    #[must_use]
    pub(crate) fn has_zero_origin_fragmented_subtitle_timeline(&self) -> bool {
        matches!(&self.presentation, DashPresentation::Static)
            && self.representation.additional_sources.is_empty()
            && self.representation.source.period_start.is_zero()
            && matches!(
                &self.representation.source.addressing,
                DashAddressing::Template(template)
                    if template.presentation_time_offset == 0
            )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DashRepresentationSource {
    base_url: Url,
    addressing: DashAddressing,
    period_start: Duration,
}

fn source_timestamp_offset(
    source: &DashRepresentationSource,
) -> Result<DashTimestampOffset, DashError> {
    let (timescale, presentation_time_offset) = match &source.addressing {
        DashAddressing::Template(template) => {
            (template.timescale, template.presentation_time_offset)
        }
        DashAddressing::List(list) => (list.timescale, list.presentation_time_offset),
    };
    if timescale == 0 {
        return Err(DashError::InvalidSegment);
    }

    let period_start_nanos = i128::try_from(source.period_start.as_nanos())
        .map_err(|_| DashError::UnsupportedPresentation)?;
    let pto_nanos = i128::from(presentation_time_offset)
        .checked_mul(1_000_000_000)
        .and_then(|scaled| scaled.checked_add(i128::from(timescale / 2)))
        .and_then(|scaled| scaled.checked_div(i128::from(timescale)))
        .ok_or(DashError::UnsupportedPresentation)?;
    let nanoseconds = period_start_nanos
        .checked_sub(pto_nanos)
        .ok_or(DashError::UnsupportedPresentation)?;

    Ok(DashTimestampOffset { nanoseconds })
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DashAddressing {
    Template(DashTemplate),
    List(DashList),
}

#[derive(Clone, Copy)]
struct RepresentationIdentity<'a> {
    id: Option<&'a str>,
    kind: DashTrackKind,
    bandwidth: Option<u64>,
    width: Option<u64>,
    height: Option<u64>,
    mime_type: Option<&'a str>,
    codecs: Option<&'a str>,
    language: Option<&'a str>,
    base_url: &'a Url,
    addressing: &'a DashAddressing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DashTemplate {
    media: String,
    initialization: Option<DashInitialization>,
    start_number: u64,
    end_number: Option<u64>,
    timescale: u64,
    duration_ticks: Option<u64>,
    timeline: Option<Box<[DashTimelineEntry]>>,
    /// Sample-timeline timestamp corresponding to the Period start, in timescale units.
    presentation_time_offset: u64,
    /// Availability offset applied to this SegmentTemplate after inheritance.
    availability_time_offset: Duration,
    /// Whether a media segment is complete when its adjusted availability begins.
    availability_time_complete: bool,
    period_duration: Duration,
}

impl DashTemplate {
    fn is_low_latency(&self) -> bool {
        !self.availability_time_complete
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DashTimelineEntry {
    start: Option<u64>,
    duration: u64,
    repeat: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DashInitialization {
    Template(String),
    Resource {
        url: Option<String>,
        range: Option<ManifestByteRange>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DashList {
    initialization: Option<DashInitialization>,
    timescale: u64,
    presentation_time_offset: u64,
    /// Effective finite BaseURL availability offset for dynamic presentations.
    availability_time_offset: Duration,
    segments: Box<[DashListSegment]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DashListSegment {
    media: String,
    range: Option<ManifestByteRange>,
    time: Option<u64>,
    duration: Option<u64>,
}

#[cfg(test)]
/// Test-facing wrapper around `parse_dash_manifest_at` using the current time.
pub(crate) fn parse_dash_manifest(
    manifest_url: &str,
    body: &str,
) -> Result<DashManifestPlan, DashError> {
    parse_dash_manifest_at(manifest_url, body, SystemTime::now())
}

/// Parse static period-continuous or bounded single-period dynamic DASH into selectable representations.
///
/// Dynamic support is limited to HTTPS SegmentTemplate presentations with a
/// finite MPD update interval. The bounded LL-DASH profile also accepts
/// SegmentTemplate availability offsets and chunked, incomplete media responses.
/// Multiple static periods require explicit DASH-IF period-continuity signaling,
/// stable adaptation/representation IDs, and contiguous timing. DRM, XLink,
/// SegmentBase/SIDX, adaptation-set Subset/Preselection combinations, dynamic
/// SegmentList, BaseURL-level availability signalling, and WebM remain on
/// yt-dlp fallback.
///
/// # Errors
/// Returns an error when the MPD is malformed, exceeds the input limit, uses
/// unsupported features, or contains no supported video representation.
///
/// The `server_now` argument is the availability clock derived from the HTTP Date header.
pub(crate) fn parse_dash_manifest_at(
    manifest_url: &str,
    body: &str,
    server_now: SystemTime,
) -> Result<DashManifestPlan, DashError> {
    if body.len() > MAX_MANIFEST_BYTES {
        return Err(DashError::Parse);
    }

    let manifest_url = parse_manifest_url(manifest_url)?;
    // OutputProtection describes the rendering device's output link (for example,
    // an HDCP requirement), not media encryption or segment addressing. Grab
    // downloads the encoded resources and does not render them, so this descriptor
    // does not restrict native download planning. ContentProtection remains
    // explicitly rejected below because it signals protected media.
    let mpd = dash_mpd_core::parse(body).map_err(|_| DashError::Parse)?;
    let mut presentation = match mpd.mpdtype.as_deref() {
        None | Some("static") => {
            if mpd.minimumUpdatePeriod.is_some()
                || mpd.availabilityStartTime.is_some()
                || mpd.availabilityEndTime.is_some()
                || mpd.timeShiftBufferDepth.is_some()
            {
                return Err(DashError::UnsupportedPresentation);
            }
            DashPresentation::Static
        }
        Some("dynamic") => {
            let availability_start_time = mpd
                .availabilityStartTime
                .as_ref()
                .cloned()
                .map(SystemTime::from)
                .ok_or(DashError::UnsupportedPresentation)?;
            let minimum_update_period = mpd
                .minimumUpdatePeriod
                .filter(|period| {
                    *period >= MIN_DYNAMIC_UPDATE_PERIOD && *period <= MAX_DYNAMIC_UPDATE_PERIOD
                })
                .ok_or(DashError::UnsupportedPresentation)?;
            let time_shift_buffer_depth = mpd.timeShiftBufferDepth;
            if time_shift_buffer_depth.is_some_and(|depth| depth.is_zero()) {
                return Err(DashError::UnsupportedPresentation);
            }
            let availability_end_time = mpd
                .availabilityEndTime
                .as_ref()
                .cloned()
                .map(SystemTime::from);
            DashPresentation::Dynamic(DashDynamicPresentation {
                availability_start_time,
                availability_end_time,
                server_now,
                period_start: Duration::ZERO,
                minimum_update_period,
                time_shift_buffer_depth,
            })
        }
        Some(_) => return Err(DashError::UnsupportedPresentation),
    };

    let is_dynamic = matches!(presentation, DashPresentation::Dynamic(_));
    if !mpd.PatchLocation.is_empty()
        || mpd.periods.is_empty()
        || mpd.periods.len() > dash_roles::MAX_PERIODS
        || !mpd.ContentProtection.is_empty()
        || (is_dynamic && mpd.periods.len() != 1)
    {
        return Err(DashError::UnsupportedPresentation);
    }

    // One bounded auxiliary XML pass collects both the source-order
    // representation metadata and SegmentList compatibility flags.
    let role_metadata = dash_roles::representation_metadata(body.as_bytes(), MAX_REPRESENTATIONS)
        .ok_or(DashError::Parse)?;
    // The bounded metadata pass recognizes actual XLink attributes by
    // namespace; a declaration without any XLink attributes is harmless.
    if role_metadata.has_xlink_attributes {
        return Err(DashError::UnsupportedPresentation);
    }
    if role_metadata.has_unsupported_segment_list_timing_attributes {
        return Err(DashError::UnsupportedAddressing);
    }
    if is_dynamic && role_metadata.has_invalid_segment_list_presentation_time_offset {
        return Err(DashError::Parse);
    }
    if is_dynamic && role_metadata.has_nonzero_segment_list_presentation_time_offset {
        return Err(DashError::UnsupportedAddressing);
    }
    if is_dynamic && role_metadata.has_invalid_segment_list_start_number {
        return Err(DashError::Parse);
    }
    if is_dynamic && role_metadata.has_nondefault_segment_list_start_number {
        return Err(DashError::UnsupportedAddressing);
    }
    // The typed parser and bounded metadata pass must describe the same source-order
    // structure for every Period; otherwise a foreign element could shift role indexes.
    if role_metadata.periods.len() != mpd.periods.len()
        || mpd
            .periods
            .iter()
            .zip(&role_metadata.periods)
            .any(|(period, metadata)| {
                metadata.len() != period.adaptations.len()
                    || period
                        .adaptations
                        .iter()
                        .zip(metadata)
                        .any(|(adaptation, metadata)| {
                            adaptation.representations.len() != metadata.len()
                                || adaptation.representations.iter().zip(metadata).any(
                                    |(representation, metadata)| representation.id != metadata.id,
                                )
                        })
            })
    {
        return Err(DashError::Parse);
    }

    let mpd_base = resolve_base_url(manifest_url.clone(), &mpd.base_url)?;
    let mut video_representations = Vec::new();
    let mut audio_representations = Vec::new();
    let mut subtitle_representations = Vec::new();
    let mut total_duration = Duration::ZERO;
    let mut previous_period_end = Duration::ZERO;
    let mut previous_period_duration = Duration::ZERO;

    for (period_index, (period, period_metadata)) in
        mpd.periods.iter().zip(&role_metadata.periods).enumerate()
    {
        if period.href.is_some()
            || !period.ContentProtection.is_empty()
            || !period.subsets.is_empty()
            || !period.pre_selections.is_empty()
            || (mpd.periods.len() > 1 && !period.empty_adaptations.is_empty())
        {
            return Err(DashError::UnsupportedPresentation);
        }

        if mpd.periods.len() > 1
            && (period.id.as_deref().is_none_or(str::is_empty)
                || mpd.periods[..period_index]
                    .iter()
                    .any(|previous| previous.id == period.id))
        {
            return Err(DashError::UnsupportedPresentation);
        }
        let period_start = period.start.unwrap_or(previous_period_end);
        // Live DASH finish/resume restores track files without a retained
        // representation plan, so it cannot apply a non-zero Period offset.
        if mpd.periods.len() > 1
            && ((period_index == 0 && period_start != Duration::ZERO)
                || period_start != previous_period_end)
        {
            return Err(DashError::UnsupportedPresentation);
        }
        let next_period_start = mpd
            .periods
            .get(period_index + 1)
            .and_then(|next| next.start);
        let duration = period
            .duration
            .or_else(|| {
                next_period_start.and_then(|next_start| next_start.checked_sub(period_start))
            })
            .or_else(|| {
                mpd.mediaPresentationDuration
                    .and_then(|whole| whole.checked_sub(period_start))
            })
            .filter(|duration| !duration.is_zero())
            .unwrap_or_default();
        if !is_dynamic && duration.is_zero() {
            return Err(DashError::UnsupportedPresentation);
        }
        let period_end = period_start
            .checked_add(duration)
            .ok_or(DashError::UnsupportedPresentation)?;
        if period_index + 1 == mpd.periods.len()
            && mpd
                .mediaPresentationDuration
                .is_some_and(|whole| whole != period_end)
        {
            return Err(DashError::UnsupportedPresentation);
        }
        total_duration = total_duration
            .checked_add(duration)
            .ok_or(DashError::LimitExceeded)?;

        if period_index > 0 {
            let previous_period = &mpd.periods[period_index - 1];
            let previous_id = previous_period
                .id
                .as_deref()
                .ok_or(DashError::UnsupportedPresentation)?;
            validate_period_continuity(previous_period, period, previous_id)?;
        }

        if period_index == 0
            && let DashPresentation::Dynamic(dynamic) = &mut presentation
        {
            dynamic.period_start = period_start;
        }
        let addressing_duration = if is_dynamic { Duration::MAX } else { duration };
        let period_base = resolve_base_url(mpd_base.clone(), &period.BaseURL)?;

        let mut period_video = Vec::new();
        let mut period_audio = Vec::new();
        let mut period_subtitles = Vec::new();

        for (adaptation_index, adaptation) in period.adaptations.iter().enumerate() {
            let adaptation_metadata = &period_metadata[adaptation_index];
            if adaptation.href.is_some() || !adaptation.ContentProtection.is_empty() {
                return Err(DashError::UnsupportedPresentation);
            }
            if has_trick_mode_properties(
                &adaptation.essential_property,
                &adaptation.supplemental_property,
            ) {
                continue;
            }

            let adaptation_base = resolve_base_url(period_base.clone(), &adaptation.BaseURL)?;
            for (representation_index, representation) in
                adaptation.representations.iter().enumerate()
            {
                let representation_is_main = adaptation_metadata
                    .get(representation_index)
                    .ok_or(DashError::Parse)?
                    .is_main;
                if representation.href.is_some() || !representation.ContentProtection.is_empty() {
                    return Err(DashError::UnsupportedPresentation);
                }
                if has_trick_mode_properties(
                    &representation.essential_property,
                    &representation.supplemental_property,
                ) {
                    continue;
                }

                let mime_type = representation
                    .mimeType
                    .clone()
                    .or_else(|| adaptation.mimeType.clone());
                if mime_type
                    .as_deref()
                    .is_some_and(|mime| mime.to_ascii_lowercase().contains("webm"))
                {
                    continue;
                }

                let kind = track_kind(
                    representation.contentType.as_deref(),
                    mime_type.as_deref(),
                    adaptation.contentType.as_deref(),
                    adaptation.mimeType.as_deref(),
                    representation
                        .codecs
                        .as_deref()
                        .or(adaptation.codecs.as_deref()),
                );
                let Some(kind) = kind else {
                    continue;
                };
                let is_main = representation_is_main
                    || adaptation.Role.iter().any(|role| {
                        role.schemeIdUri == DASH_ROLE_SCHEME
                            && role.value.as_deref() == Some("main")
                    });
                let is_default_audio = kind == DashTrackKind::Audio && is_main;

                let representation_base =
                    resolve_base_url(adaptation_base.clone(), &representation.BaseURL)?;
                let mut addressing = effective_addressing(
                    period,
                    adaptation,
                    representation,
                    addressing_duration,
                    is_dynamic,
                    adaptation_metadata
                        .get(representation_index)
                        .ok_or(DashError::Parse)?
                        .segment_list_presentation_time_offset
                        .unwrap_or(0),
                )?;
                let base_url_offset = effective_base_url_availability_offset(
                    [
                        mpd.base_url.as_slice(),
                        period.BaseURL.as_slice(),
                        adaptation.BaseURL.as_slice(),
                        representation.BaseURL.as_slice(),
                    ],
                    is_dynamic,
                )?;
                match &mut addressing {
                    DashAddressing::Template(template) => {
                        template.availability_time_offset = template
                            .availability_time_offset
                            .checked_add(base_url_offset)
                            .ok_or(DashError::UnsupportedAddressing)?;
                        low_latency::validate_template_availability(
                            is_dynamic,
                            template.availability_time_offset,
                            template.availability_time_complete,
                        )?;
                        low_latency::validate_template_mime(
                            template.is_low_latency(),
                            mime_type.as_deref(),
                        )?;
                    }
                    DashAddressing::List(list) => {
                        let segment_list_offset = adaptation_metadata
                            .get(representation_index)
                            .ok_or(DashError::Parse)?
                            .segment_list_availability_time_offset
                            .as_deref()
                            .unwrap_or("0")
                            .parse::<f64>()
                            .map_err(|_| DashError::UnsupportedAddressing)?;
                        let segment_list_offset =
                            low_latency::parse_availability_time_offset(segment_list_offset)?;
                        if !is_dynamic && !segment_list_offset.is_zero() {
                            return Err(DashError::UnsupportedAddressing);
                        }
                        list.availability_time_offset = list
                            .availability_time_offset
                            .checked_add(segment_list_offset)
                            .and_then(|offset| offset.checked_add(base_url_offset))
                            .ok_or(DashError::UnsupportedAddressing)?;
                    }
                }
                let representation_id = representation.id.clone();
                let bandwidth = representation.bandwidth;
                let width = representation.width.or(adaptation.width);
                let height = representation.height.or(adaptation.height);
                let codecs = representation
                    .codecs
                    .clone()
                    .or_else(|| adaptation.codecs.clone());
                let language = representation
                    .lang
                    .clone()
                    .or_else(|| adaptation.lang.clone());
                let identity = RepresentationIdentity {
                    id: representation_id.as_deref(),
                    kind,
                    bandwidth,
                    width,
                    height,
                    mime_type: mime_type.as_deref(),
                    codecs: codecs.as_deref(),
                    language: language.as_deref(),
                    base_url: &representation_base,
                    addressing: &addressing,
                };
                let live_key = stable_live_representation_key(identity);
                let legacy_live_key = is_dynamic
                    .then(|| legacy_live_representation_key(identity))
                    .filter(|legacy| legacy != &live_key);
                let key = if is_dynamic {
                    live_key.clone()
                } else {
                    stable_representation_key(identity)
                };

                let choice = DashRepresentation {
                    key,
                    live_key,
                    legacy_live_key,
                    representation_id,
                    bandwidth,
                    width,
                    height,
                    mime_type,
                    codecs,
                    language,
                    kind,
                    is_default_audio,
                    is_default_subtitle: kind == DashTrackKind::Subtitle && is_main,
                    adaptation_id: adaptation.id.clone(),
                    source: DashRepresentationSource {
                        base_url: representation_base,
                        addressing,
                        period_start,
                    },
                    additional_sources: Vec::new(),
                };

                match kind {
                    DashTrackKind::Video => period_video.push(choice),
                    DashTrackKind::Audio => period_audio.push(choice),
                    DashTrackKind::Subtitle => period_subtitles.push(choice),
                }

                if period_video.len() + period_audio.len() + period_subtitles.len()
                    > MAX_REPRESENTATIONS
                {
                    return Err(DashError::LimitExceeded);
                }
            }
        }

        if period_index == 0 {
            video_representations = period_video;
            audio_representations = period_audio;
            subtitle_representations = period_subtitles;
        } else {
            append_period_sources(
                &mut video_representations,
                period_video,
                previous_period_duration,
            )?;
            append_period_sources(
                &mut audio_representations,
                period_audio,
                previous_period_duration,
            )?;
            append_period_sources(
                &mut subtitle_representations,
                period_subtitles,
                previous_period_duration,
            )?;
        }

        previous_period_end = period_end;
        previous_period_duration = duration;
    }

    if video_representations.is_empty() {
        return Err(DashError::NoVideoRepresentations);
    }
    if mpd.periods.len() > 1
        && !representation_keys_are_unambiguous(
            video_representations
                .iter()
                .chain(&audio_representations)
                .chain(&subtitle_representations),
        )
    {
        return Err(DashError::UnsupportedPresentation);
    }

    Ok(DashManifestPlan {
        duration: total_duration,
        presentation,
        video_representations: video_representations.into_boxed_slice(),
        audio_representations: audio_representations.into_boxed_slice(),
        subtitle_representations: subtitle_representations.into_boxed_slice(),
    })
}

const DASH_PERIOD_CONTINUITY_SCHEME: &str = "urn:mpeg:dash:period-continuity:2015";
const DASH_PERIOD_CONNECTIVITY_SCHEME: &str = "urn:mpeg:dash:period-connectivity:2015";

/// Require DASH-IF period continuity, associated assets, and stable track identities.
fn validate_period_continuity(
    previous: &dash_mpd_core::Period,
    current: &dash_mpd_core::Period,
    previous_period_id: &str,
) -> Result<(), DashError> {
    if previous.adaptations.len() != current.adaptations.len()
        || previous.asset_identifier.is_none()
        || previous.asset_identifier != current.asset_identifier
    {
        return Err(DashError::UnsupportedPresentation);
    }

    let mut previous_adaptations =
        std::collections::HashMap::with_capacity(previous.adaptations.len());
    for adaptation in &previous.adaptations {
        let id = adaptation
            .id
            .as_deref()
            .filter(|id| !id.is_empty())
            .ok_or(DashError::UnsupportedPresentation)?;
        if previous_adaptations.insert(id, adaptation).is_some() {
            return Err(DashError::UnsupportedPresentation);
        }
    }

    let mut current_adaptation_ids =
        std::collections::HashSet::with_capacity(current.adaptations.len());
    for adaptation in &current.adaptations {
        let adaptation_id = adaptation
            .id
            .as_deref()
            .filter(|id| !id.is_empty())
            .ok_or(DashError::UnsupportedPresentation)?;
        if !current_adaptation_ids.insert(adaptation_id) {
            return Err(DashError::UnsupportedPresentation);
        }

        let previous_adaptation = previous_adaptations
            .get(adaptation_id)
            .copied()
            .ok_or(DashError::UnsupportedPresentation)?;
        let continuity_signaled = adaptation.supplemental_property.iter().any(|property| {
            property.schemeIdUri == DASH_PERIOD_CONTINUITY_SCHEME
                && property.value.as_deref() == Some(previous_period_id)
        });
        let connectivity_also_signaled = adaptation
            .supplemental_property
            .iter()
            .any(|property| property.schemeIdUri == DASH_PERIOD_CONNECTIVITY_SCHEME);
        if !continuity_signaled
            || connectivity_also_signaled
            || previous_adaptation.representations.len() != adaptation.representations.len()
        {
            return Err(DashError::UnsupportedPresentation);
        }

        let mut previous_representation_ids =
            std::collections::HashSet::with_capacity(previous_adaptation.representations.len());
        for representation in &previous_adaptation.representations {
            let id = representation
                .id
                .as_deref()
                .filter(|id| !id.is_empty())
                .ok_or(DashError::UnsupportedPresentation)?;
            if !previous_representation_ids.insert(id) {
                return Err(DashError::UnsupportedPresentation);
            }
        }

        let mut current_representation_ids =
            std::collections::HashSet::with_capacity(adaptation.representations.len());
        for representation in &adaptation.representations {
            let id = representation
                .id
                .as_deref()
                .filter(|id| !id.is_empty())
                .ok_or(DashError::UnsupportedPresentation)?;
            if !current_representation_ids.insert(id) || !previous_representation_ids.contains(id) {
                return Err(DashError::UnsupportedPresentation);
            }
        }
    }

    Ok(())
}

fn period_duration_to_ticks(duration: Duration, timescale: u64) -> Result<u64, DashError> {
    let scaled_nanos = duration
        .as_nanos()
        .checked_mul(u128::from(timescale))
        .ok_or(DashError::UnsupportedPresentation)?;
    let nanos_per_second = 1_000_000_000_u128;
    if scaled_nanos % nanos_per_second != 0 {
        return Err(DashError::UnsupportedPresentation);
    }
    u64::try_from(scaled_nanos / nanos_per_second).map_err(|_| DashError::UnsupportedPresentation)
}

fn representation_keys_are_unambiguous<'a>(
    mut representations: impl Iterator<Item = &'a DashRepresentation>,
) -> bool {
    let mut keys = std::collections::HashSet::new();
    representations.all(|representation| {
        keys.insert(representation.key.as_str())
            && (representation.live_key == representation.key
                || keys.insert(representation.live_key.as_str()))
    })
}

/// Append a period's source only when it preserves the selected representation's decoding identity.
fn append_period_sources(
    existing: &mut [DashRepresentation],
    incoming: Vec<DashRepresentation>,
    previous_period_duration: Duration,
) -> Result<(), DashError> {
    if existing.len() != incoming.len() {
        return Err(DashError::UnsupportedPresentation);
    }

    let mut incoming_ids = std::collections::HashSet::with_capacity(incoming.len());
    if incoming.iter().any(|next| {
        !incoming_ids.insert((
            next.adaptation_id.as_deref(),
            next.representation_id.as_deref(),
        ))
    }) {
        return Err(DashError::UnsupportedPresentation);
    }

    for next in incoming {
        let current = existing
            .iter_mut()
            .find(|candidate| {
                candidate.adaptation_id == next.adaptation_id
                    && candidate.representation_id == next.representation_id
            })
            .ok_or(DashError::UnsupportedPresentation)?;

        if current.kind != next.kind
            || current.mime_type != next.mime_type
            || current.codecs != next.codecs
            || current.width != next.width
            || current.height != next.height
            || current.language != next.language
        {
            return Err(DashError::UnsupportedPresentation);
        }

        let previous_source = current.additional_sources.last().unwrap_or(&current.source);
        let (DashAddressing::Template(previous_template), DashAddressing::Template(next_template)) =
            (&previous_source.addressing, &next.source.addressing)
        else {
            return Err(DashError::UnsupportedAddressing);
        };
        if source_timestamp_offset(previous_source)? != source_timestamp_offset(&next.source)? {
            return Err(DashError::UnsupportedPresentation);
        }
        if previous_template.timescale != next_template.timescale
            || previous_template.initialization.is_some() != next_template.initialization.is_some()
        {
            return Err(DashError::UnsupportedPresentation);
        }
        let period_ticks =
            period_duration_to_ticks(previous_period_duration, previous_template.timescale)?;
        let expected_presentation_time_offset = previous_template
            .presentation_time_offset
            .checked_add(period_ticks)
            .ok_or(DashError::UnsupportedPresentation)?;
        if next_template.presentation_time_offset != expected_presentation_time_offset {
            return Err(DashError::UnsupportedPresentation);
        }
        current.additional_sources.push(next.source);
    }

    Ok(())
}

/// Expand a chosen representation into ordered initialization and media resources.
///
/// # Errors
/// Returns an error when the key is unknown or segment addressing cannot be
/// expanded without guessing.
pub(crate) fn plan_dash_representation(
    manifest: &DashManifestPlan,
    representation_key: &str,
) -> Result<DashRepresentationPlan, DashError> {
    let mut representation = manifest
        .video_representations
        .iter()
        .chain(manifest.audio_representations.iter())
        .chain(manifest.subtitle_representations.iter())
        .find(|candidate| candidate.matches_key(representation_key))
        .cloned()
        .ok_or(DashError::UnsupportedAddressing)?;

    let live_selection = representation_key == representation.live_key
        || representation.legacy_live_key.as_deref() == Some(representation_key);
    let low_latency = matches!(
        &representation.source.addressing,
        DashAddressing::Template(template) if template.is_low_latency()
    );
    let mut sources = Vec::with_capacity(1 + representation.additional_sources.len());
    sources.push(representation.source.clone());
    sources.append(&mut representation.additional_sources);

    let mut resources: Vec<DashResource> = Vec::new();
    let mut segment_count = 0_usize;
    for (period_index, source) in sources.iter().enumerate() {
        let mut period_representation = representation.clone();
        period_representation.source = source.clone();
        let mut period_resources = match (&manifest.presentation, &source.addressing) {
            (DashPresentation::Dynamic(dynamic), DashAddressing::Template(template)) => {
                expand_dynamic_template(&period_representation, &template.media, template, dynamic)?
            }
            (DashPresentation::Dynamic(dynamic), DashAddressing::List(list)) => {
                expand_dynamic_list(&period_representation, list, dynamic)?
            }
            (DashPresentation::Static, DashAddressing::Template(template)) if live_selection => {
                expand_static_live_template(
                    &period_representation,
                    &template.media,
                    template,
                    sources.len() > 1,
                )?
            }
            (DashPresentation::Static, DashAddressing::Template(template)) => expand_template(
                &period_representation,
                &template.media,
                template,
                sources.len() > 1,
            )?,
            (DashPresentation::Static, DashAddressing::List(_)) if live_selection => {
                return Err(DashError::UnsupportedAddressing);
            }
            (DashPresentation::Static, DashAddressing::List(list)) => {
                expand_list(&period_representation, list)?
            }
        };

        if period_index > 0 {
            let period_prefix = format!("dash-period-{period_index}");
            for (resource_index, resource) in period_resources.iter_mut().enumerate() {
                if resource.kind == DashResourceKind::Segment {
                    resource.filename = super::resource_filename(
                        &period_prefix,
                        Some(resource_index),
                        &resource.url,
                        resource.range,
                    );
                }
            }
        }

        if period_index > 0 {
            let previous_initialization = resources
                .iter()
                .find(|resource| resource.kind == DashResourceKind::Initialization);
            let period_initialization = period_resources
                .iter()
                .find(|resource| resource.kind == DashResourceKind::Initialization);
            let initialization_matches = match (previous_initialization, period_initialization) {
                (Some(previous), Some(current)) => {
                    previous.url == current.url && previous.range == current.range
                }
                (None, None) => true,
                _ => false,
            };
            if !initialization_matches {
                return Err(DashError::UnsupportedPresentation);
            }
        }

        let period_segment_count = period_resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .count();
        if sources.len() > 1 && period_segment_count == 0 {
            return Err(DashError::InvalidSegment);
        }
        segment_count = segment_count
            .checked_add(period_segment_count)
            .filter(|count| *count <= MAX_DASH_SEGMENTS)
            .ok_or(DashError::LimitExceeded)?;
        for resource in period_resources {
            if period_index > 0 && resource.kind == DashResourceKind::Initialization {
                continue;
            }
            resources.push(resource);
        }
    }
    let input_timestamp_offset = if matches!(&manifest.presentation, DashPresentation::Static) {
        source_timestamp_offset(&representation.source)?
    } else {
        DashTimestampOffset { nanoseconds: 0 }
    };
    if !matches!(&manifest.presentation, DashPresentation::Dynamic(_))
        && !resources
            .iter()
            .any(|resource| resource.kind == DashResourceKind::Segment)
    {
        return Err(DashError::InvalidSegment);
    }

    if !matches!(&manifest.presentation, DashPresentation::Dynamic(_))
        && !resources
            .iter()
            .any(|resource| resource.kind == DashResourceKind::Segment)
    {
        return Err(DashError::InvalidSegment);
    }

    Ok(DashRepresentationPlan {
        representation,
        duration: manifest.duration,
        presentation: manifest.presentation.clone(),
        resources: resources.into_boxed_slice(),
        input_timestamp_offset,
        low_latency,
    })
}

fn parse_manifest_url(raw: &str) -> Result<Url, DashError> {
    let url = Url::parse(raw).map_err(|_| DashError::InvalidManifestUrl)?;
    validate_https_url(url).map_err(|_| DashError::InvalidManifestUrl)
}

fn validate_https_url(url: Url) -> Result<Url, DashError> {
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(DashError::UnsupportedAddressing);
    }
    Ok(url)
}

/// Stable identifier for a DASH representation, independent of adaptation
/// and representation order. Query signatures are excluded from BaseURL identity.
fn stable_representation_key(identity: RepresentationIdentity<'_>) -> String {
    let RepresentationIdentity {
        id,
        kind,
        bandwidth,
        width,
        height,
        mime_type,
        codecs,
        language,
        base_url,
        addressing,
    } = identity;
    let mut hasher = Sha256::new();
    hasher.update(match kind {
        DashTrackKind::Video => b"video".as_slice(),
        DashTrackKind::Audio => b"audio".as_slice(),
        DashTrackKind::Subtitle => b"subtitle".as_slice(),
    });
    hash_optional_text(&mut hasher, id);
    hash_optional_text(&mut hasher, base_url.host_str());
    hash_optional_number(&mut hasher, base_url.port_or_known_default().map(u64::from));
    hash_optional_text(&mut hasher, Some(base_url.path()));
    hash_optional_text(&mut hasher, mime_type);
    hash_optional_text(&mut hasher, codecs);
    hash_optional_text(&mut hasher, language);
    hash_optional_number(&mut hasher, bandwidth);
    hash_optional_number(&mut hasher, width);
    hash_optional_number(&mut hasher, height);

    match addressing {
        DashAddressing::Template(template) => {
            hasher.update(b"template");
            hasher.update(template.media.as_bytes());
            hash_optional_number(&mut hasher, Some(template.start_number));
            hash_optional_number(&mut hasher, template.end_number);
            hash_optional_number(&mut hasher, Some(template.timescale));
            hash_optional_number(&mut hasher, template.duration_ticks);
            match &template.initialization {
                Some(DashInitialization::Template(value)) => {
                    hasher.update(b"init-template");
                    hasher.update(value.as_bytes());
                }
                Some(DashInitialization::Resource { url, range }) => {
                    hasher.update(b"init-resource");
                    hash_optional_text(&mut hasher, url.as_deref());
                    hash_optional_range(&mut hasher, *range);
                }
                None => {
                    hasher.update(b"no-init");
                }
            }
            if let Some(timeline) = template.timeline.as_deref() {
                hasher.update((timeline.len() as u64).to_le_bytes());
                for entry in timeline {
                    hash_optional_number(&mut hasher, entry.start);
                    hasher.update(entry.duration.to_le_bytes());
                    hasher.update(entry.repeat.to_le_bytes());
                }
            }
        }
        DashAddressing::List(list) => {
            hasher.update(b"list");
            hasher.update(list.timescale.to_le_bytes());
            match &list.initialization {
                Some(DashInitialization::Template(value)) => {
                    hasher.update(b"init-template");
                    hasher.update(value.as_bytes());
                }
                Some(DashInitialization::Resource { url, range }) => {
                    hasher.update(b"init-resource");
                    hash_optional_text(&mut hasher, url.as_deref());
                    hash_optional_range(&mut hasher, *range);
                }
                None => {
                    hasher.update(b"no-init");
                }
            }
            hasher.update((list.segments.len() as u64).to_le_bytes());
            for segment in list.segments.iter() {
                hasher.update(segment.media.as_bytes());
                hash_optional_range(&mut hasher, segment.range);
                hash_optional_number(&mut hasher, segment.time);
                hash_optional_number(&mut hasher, segment.duration);
            }
        }
    }

    let digest = hasher.finalize();
    let fingerprint: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("dash-{fingerprint}")
}

/// Stable representation identity excluding the changing live segment window.
fn stable_live_representation_key(identity: RepresentationIdentity<'_>) -> String {
    live_representation_key(identity, false)
}

/// Preserve the former policy-sensitive key for an in-progress capture being upgraded.
fn legacy_live_representation_key(identity: RepresentationIdentity<'_>) -> String {
    live_representation_key(identity, true)
}

fn live_representation_key(
    identity: RepresentationIdentity<'_>,
    include_availability_policy: bool,
) -> String {
    let RepresentationIdentity {
        id,
        kind,
        bandwidth,
        width,
        height,
        mime_type,
        codecs,
        language,
        base_url,
        addressing,
    } = identity;
    let mut hasher = Sha256::new();
    hasher.update(match kind {
        DashTrackKind::Video => b"video".as_slice(),
        DashTrackKind::Audio => b"audio".as_slice(),
        DashTrackKind::Subtitle => b"subtitle".as_slice(),
    });
    hash_optional_text(&mut hasher, id);
    hash_optional_text(&mut hasher, base_url.host_str());
    hash_optional_number(&mut hasher, base_url.port_or_known_default().map(u64::from));
    hash_optional_text(&mut hasher, Some(base_url.path()));
    hash_optional_text(&mut hasher, mime_type);
    hash_optional_text(&mut hasher, codecs);
    hash_optional_text(&mut hasher, language);
    hash_optional_number(&mut hasher, bandwidth);
    hash_optional_number(&mut hasher, width);
    hash_optional_number(&mut hasher, height);
    match addressing {
        DashAddressing::Template(template) => {
            hasher.update(b"template");
            hasher.update(template.media.as_bytes());
            hash_optional_number(&mut hasher, Some(template.timescale));
            // Availability policy is refreshable metadata, not representation identity.
            if include_availability_policy
                && (!template.availability_time_offset.is_zero()
                    || !template.availability_time_complete)
            {
                hasher.update(b"availability");
                hasher.update(template.availability_time_offset.as_nanos().to_le_bytes());
                hasher.update([u8::from(template.availability_time_complete)]);
            }
            match &template.initialization {
                Some(DashInitialization::Template(value)) => {
                    hasher.update(b"init-template");
                    hasher.update(value.as_bytes());
                }
                Some(DashInitialization::Resource { url, range }) => {
                    hasher.update(b"init-resource");
                    hash_optional_text(&mut hasher, url.as_deref());
                    hash_optional_range(&mut hasher, *range);
                }
                None => hasher.update(b"no-init"),
            }
        }
        DashAddressing::List(list) => {
            hasher.update(b"list");
            hasher.update(list.timescale.to_le_bytes());
            hash_optional_number(
                &mut hasher,
                list.segments.first().and_then(|segment| segment.duration),
            );
            match &list.initialization {
                Some(DashInitialization::Template(value)) => {
                    hasher.update(b"init-template");
                    hasher.update(value.as_bytes());
                }
                Some(DashInitialization::Resource { url, range }) => {
                    hasher.update(b"init-resource");
                    hash_separate_initialization_identity(&mut hasher, base_url, url.as_deref());
                    hash_optional_range(&mut hasher, *range);
                }
                None => hasher.update(b"no-init"),
            }
        }
    }
    let digest = hasher.finalize();
    let fingerprint: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("dash-live-{fingerprint}")
}

fn hash_separate_initialization_identity(
    hasher: &mut Sha256,
    base_url: &Url,
    source_url: Option<&str>,
) {
    if let Some(source_url) = source_url {
        hasher.update(b"separate-init");
        let identity = base_url
            .join(source_url)
            .map(|url| super::resource_cache_key(&url))
            .unwrap_or_else(|_| source_url.to_string());
        hash_optional_text(hasher, Some(&identity));
    }
}

fn hash_optional_text(hasher: &mut Sha256, value: Option<&str>) {
    if let Some(value) = value {
        hasher.update([1]);
        hasher.update(value.as_bytes());
    } else {
        hasher.update([0]);
    }
}

fn hash_optional_number(hasher: &mut Sha256, value: Option<u64>) {
    match value {
        Some(value) => {
            hasher.update([1]);
            hasher.update(value.to_le_bytes());
        }
        None => hasher.update([0]),
    }
}

fn hash_optional_range(hasher: &mut Sha256, range: Option<ManifestByteRange>) {
    match range {
        Some(range) => {
            hasher.update([1]);
            hasher.update(range.offset.to_le_bytes());
            hasher.update(range.length.to_le_bytes());
        }
        None => hasher.update([0]),
    }
}

/// Resolve exactly one BaseURL at each inheritance level. Choosing among CDNs
/// is deliberately left to a later selection layer instead of silently guessing.
fn resolve_base_url(
    base_url: Url,
    candidates: &[dash_mpd_core::BaseURL],
) -> Result<Url, DashError> {
    let mut values = candidates
        .iter()
        .map(|candidate| candidate.base.trim())
        .filter(|value| !value.is_empty());
    let Some(value) = values.next() else {
        if candidates.iter().any(|candidate| {
            candidate.byte_range.is_some()
                || candidate.availability_time_complete == Some(false)
                || candidate.timeShiftBufferDepth.is_some()
        }) {
            return Err(DashError::UnsupportedAddressing);
        }
        return Ok(base_url);
    };

    if values.next().is_some()
        || candidates.iter().any(|candidate| {
            candidate.byte_range.is_some()
                || candidate.availability_time_complete == Some(false)
                || candidate.timeShiftBufferDepth.is_some()
        })
    {
        return Err(DashError::UnsupportedAddressing);
    }

    let resolved = base_url
        .join(value)
        .map_err(|_| DashError::UnsupportedAddressing)?;
    validate_https_url(resolved)
}

/// Sum finite BaseURL availability offsets across the selected URL hierarchy.
fn effective_base_url_availability_offset<'a>(
    levels: impl IntoIterator<Item = &'a [dash_mpd_core::BaseURL]>,
    is_dynamic: bool,
) -> Result<Duration, DashError> {
    let mut total = Duration::ZERO;
    for candidate in levels.into_iter().flatten() {
        let Some(offset) = candidate.availability_time_offset else {
            continue;
        };
        let parsed = low_latency::parse_availability_time_offset(offset)?;
        if !is_dynamic {
            return Err(DashError::UnsupportedAddressing);
        }
        total = total
            .checked_add(parsed)
            .ok_or(DashError::UnsupportedAddressing)?;
    }
    Ok(total)
}

fn has_trick_mode_properties(
    essential_properties: &[dash_mpd_core::EssentialProperty],
    supplemental_properties: &[dash_mpd_core::SupplementalProperty],
) -> bool {
    essential_properties
        .iter()
        .any(|property| is_trick_mode_scheme(&property.schemeIdUri))
        || supplemental_properties
            .iter()
            .any(|property| is_trick_mode_scheme(&property.schemeIdUri))
}

fn is_trick_mode_scheme(scheme_id_uri: &str) -> bool {
    DASH_TRICK_MODE_SCHEMES.contains(&scheme_id_uri)
}

fn track_kind(
    representation_content_type: Option<&str>,
    representation_mime_type: Option<&str>,
    adaptation_content_type: Option<&str>,
    adaptation_mime_type: Option<&str>,
    codecs: Option<&str>,
) -> Option<DashTrackKind> {
    let content_type = representation_content_type.or(adaptation_content_type);
    let mime_type = representation_mime_type.or(adaptation_mime_type);

    if content_type == Some("video") || mime_type.is_some_and(|mime| mime.starts_with("video/")) {
        Some(DashTrackKind::Video)
    } else if content_type == Some("audio")
        || mime_type.is_some_and(|mime| mime.starts_with("audio/"))
    {
        Some(DashTrackKind::Audio)
    } else if mime_type.is_some_and(|mime| mime.eq_ignore_ascii_case("application/mp4"))
        && codecs.is_some_and(|codecs| {
            codecs.split(',').any(|codec| {
                let codec = codec.trim().to_ascii_lowercase();
                codec == "stpp"
                    || codec.starts_with("stpp.")
                    || codec == "wvtt"
                    || codec.starts_with("wvtt.")
            })
        })
    {
        Some(DashTrackKind::Subtitle)
    } else {
        None
    }
}

fn effective_addressing(
    period: &dash_mpd_core::Period,
    adaptation: &dash_mpd_core::AdaptationSet,
    representation: &dash_mpd_core::Representation,
    period_duration: Duration,
    is_dynamic: bool,
    segment_list_presentation_time_offset: u64,
) -> Result<DashAddressing, DashError> {
    if representation.representation_index.is_some() {
        // The supported profile requires RepresentationIndex to be nested in
        // SegmentBase, where index and initialization addressing can be resolved.
        return Err(DashError::UnsupportedAddressing);
    }

    let templates = [
        period.SegmentTemplate.as_ref(),
        adaptation.SegmentTemplate.as_ref(),
        representation.SegmentTemplate.as_ref(),
    ];
    let lists = [
        period.SegmentList.as_ref(),
        adaptation.SegmentList.as_ref(),
        representation.SegmentList.as_ref(),
    ];
    let bases = [
        period.SegmentBase.as_ref(),
        adaptation.SegmentBase.as_ref(),
        representation.SegmentBase.as_ref(),
    ];
    // FFmpeg's DASH demuxer does not implement SegmentBase/SIDX addressing.
    if bases.iter().any(Option::is_some) {
        return Err(DashError::UnsupportedAddressing);
    }

    let has_template = templates.iter().any(Option::is_some);
    let has_list = lists.iter().any(Option::is_some);

    if is_dynamic && (has_template == has_list) {
        return Err(DashError::UnsupportedAddressing);
    }
    let addressing = match (has_template, has_list) {
        (true, false) => {
            let template = effective_template(&templates, period_duration, is_dynamic)?;
            if is_dynamic && template.presentation_time_offset != 0 {
                return Err(DashError::UnsupportedAddressing);
            }
            DashAddressing::Template(template)
        }
        (false, true) => {
            let mime_type = representation
                .mimeType
                .as_deref()
                .or(adaptation.mimeType.as_deref());
            if is_dynamic
                && !mime_type.is_some_and(|mime| {
                    mime.eq_ignore_ascii_case("video/mp4") || mime.eq_ignore_ascii_case("audio/mp4")
                })
            {
                return Err(DashError::UnsupportedAddressing);
            }
            effective_list(&lists, is_dynamic, segment_list_presentation_time_offset)
                .map(DashAddressing::List)?
        }
        _ => return Err(DashError::UnsupportedAddressing),
    };
    if is_dynamic
        && matches!(
            &addressing,
            DashAddressing::Template(template)
                if !template.media.contains("$Number") && !template.media.contains("$Time")
        )
    {
        return Err(DashError::UnsupportedAddressing);
    }
    Ok(addressing)
}

fn effective_template(
    levels: &[Option<&dash_mpd_core::SegmentTemplate>],
    period_duration: Duration,
    is_dynamic: bool,
) -> Result<DashTemplate, DashError> {
    let mut media = None;
    let mut initialization_template = None;
    let mut initialization_element = None;
    let mut start_number = None;
    let mut end_number = None;
    let mut timescale = None;
    let mut duration_ticks = None;
    let mut presentation_time_offset = None;
    let mut availability_time_offset = None;
    let mut availability_time_complete = None;
    let mut timeline = None;

    for template in levels.iter().flatten() {
        if template.index.is_some()
            || template.indexRange.is_some()
            || template.indexRangeExact == Some(true)
            || template.representation_index.is_some()
            || template.bitstreamSwitching.is_some()
            || template.failover_content.is_some()
            || template.eptDelta.is_some_and(|delta| delta != 0)
            || template.pbDelta.is_some_and(|delta| delta != 0)
            || template.BitstreamSwitching.is_some()
        {
            return Err(DashError::UnsupportedAddressing);
        }

        media = template.media.clone().or(media);
        initialization_template = template.initialization.clone().or(initialization_template);
        start_number = template.startNumber.or(start_number);
        end_number = template.endNumber.or(end_number);
        timescale = template.timescale.or(timescale);
        // Child SegmentTemplate attributes override inherited attributes.
        presentation_time_offset = template.presentationTimeOffset.or(presentation_time_offset);
        availability_time_offset = template.availabilityTimeOffset.or(availability_time_offset);
        availability_time_complete = template
            .availabilityTimeComplete
            .or(availability_time_complete);

        if let Some(duration) = template.duration {
            if !duration.is_finite()
                || duration <= 0.0
                || duration.fract() != 0.0
                || duration >= u64::MAX as f64
            {
                return Err(DashError::InvalidSegment);
            }
            duration_ticks = Some(duration as u64);
        }

        if let Some(initialization) = &template.Initialization {
            initialization_element = Some(initialization.clone());
        }
        if let Some(segment_timeline) = &template.SegmentTimeline {
            timeline = Some(segment_timeline.segments.as_slice());
        }
    }

    let media = media
        .filter(|media| !media.is_empty())
        .ok_or(DashError::UnsupportedAddressing)?;
    if initialization_template.is_some() && initialization_element.is_some() {
        return Err(DashError::UnsupportedAddressing);
    }
    if initialization_template
        .as_deref()
        .is_some_and(str::is_empty)
        || initialization_element
            .as_ref()
            .and_then(|element| element.sourceURL.as_deref())
            .is_some_and(str::is_empty)
    {
        return Err(DashError::InvalidSegment);
    }

    let initialization = if let Some(template) = initialization_template {
        Some(DashInitialization::Template(template))
    } else if let Some(element) = initialization_element {
        let range = element.range.as_deref().map(parse_byte_range).transpose()?;
        Some(DashInitialization::Resource {
            url: element.sourceURL,
            range,
        })
    } else {
        None
    };
    if availability_time_complete == Some(false) && initialization.is_none() {
        return Err(DashError::UnsupportedAddressing);
    }

    let timescale = timescale.unwrap_or(1);
    if timescale == 0 {
        return Err(DashError::InvalidSegment);
    }
    if end_number.is_some_and(|end| end < start_number.unwrap_or(1)) {
        return Err(DashError::InvalidSegment);
    }

    let timeline = timeline
        .map(|entries| {
            entries
                .iter()
                .map(|entry| {
                    if entry.d == 0
                        || entry.r.is_some_and(|repeat| repeat < -1)
                        || entry.n.is_some()
                        || entry.k.is_some()
                    {
                        return Err(DashError::UnsupportedAddressing);
                    }
                    Ok(DashTimelineEntry {
                        start: entry.t,
                        duration: entry.d,
                        repeat: entry.r.unwrap_or(0),
                    })
                })
                .collect::<Result<Vec<_>, DashError>>()
                .map(Vec::into_boxed_slice)
        })
        .transpose()?;

    if timeline.is_none() && duration_ticks.is_none() {
        return Err(DashError::UnsupportedAddressing);
    }

    let availability_time_offset = availability_time_offset
        .map(low_latency::parse_availability_time_offset)
        .transpose()?
        .unwrap_or_default();
    let availability_time_complete = availability_time_complete.unwrap_or(true);
    low_latency::validate_template_availability(
        is_dynamic,
        availability_time_offset,
        availability_time_complete,
    )?;

    Ok(DashTemplate {
        media,
        initialization,
        start_number: start_number.unwrap_or(1),
        end_number,
        timescale,
        duration_ticks,
        timeline,
        presentation_time_offset: presentation_time_offset.unwrap_or(0),
        availability_time_offset,
        availability_time_complete,
        period_duration,
    })
}

fn effective_list(
    levels: &[Option<&dash_mpd_core::SegmentList>],
    is_dynamic: bool,
    presentation_time_offset: u64,
) -> Result<DashList, DashError> {
    let mut initialization = None;
    let mut segment_urls = None;
    let mut segment_list_seen = false;
    let mut timescale = 1_u64;
    let mut timeline = None;

    for list in levels.iter().flatten() {
        segment_list_seen = true;
        timescale = list.timescale.unwrap_or(timescale);
        if let Some(segment_timeline) = &list.SegmentTimeline {
            timeline = Some(segment_timeline.segments.as_slice());
        }
        if list.indexRange.is_some()
            || list.indexRangeExact == Some(true)
            || list.href.is_some()
            || list.sltype.is_some()
            || list.show.is_some()
            || list.BitstreamSwitching.is_some()
        {
            return Err(DashError::UnsupportedAddressing);
        }

        if let Some(element) = &list.Initialization {
            initialization = Some(DashInitialization::Resource {
                url: element.sourceURL.clone(),
                range: element.range.as_deref().map(parse_byte_range).transpose()?,
            });
        }

        if !list.segment_urls.is_empty() {
            if segment_urls.is_some() {
                return Err(DashError::UnsupportedAddressing);
            }
            segment_urls = Some(list.segment_urls.as_slice());
        }
    }

    if !segment_list_seen {
        return Err(DashError::UnsupportedAddressing);
    }

    let segment_urls = segment_urls.ok_or(DashError::InvalidSegment)?;
    if segment_urls.is_empty() || segment_urls.len() > MAX_DASH_SEGMENTS {
        return Err(if segment_urls.len() > MAX_DASH_SEGMENTS {
            DashError::LimitExceeded
        } else {
            DashError::InvalidSegment
        });
    }

    if timescale == 0 {
        return Err(DashError::InvalidSegment);
    }
    let timeline = is_dynamic
        .then_some(timeline)
        .flatten()
        .map(|entries| {
            entries
                .iter()
                .map(|entry| {
                    if entry.d == 0
                        || entry.r.is_some_and(|repeat| repeat != 0)
                        || entry.n.is_some()
                        || entry.k.is_some()
                    {
                        return Err(DashError::UnsupportedAddressing);
                    }
                    Ok(DashTimelineEntry {
                        start: entry.t,
                        duration: entry.d,
                        repeat: 0,
                    })
                })
                .collect::<Result<Vec<_>, DashError>>()
        })
        .transpose()?;
    if is_dynamic
        && !matches!(
            initialization.as_ref(),
            Some(DashInitialization::Resource {
                url: Some(url),
                ..
            }) if !url.trim().is_empty()
        )
    {
        return Err(DashError::UnsupportedAddressing);
    }
    if is_dynamic
        && !timeline.as_ref().is_some_and(|entries| {
            entries.len() == segment_urls.len()
                && entries.first().is_some_and(|entry| entry.start == Some(0))
                && entries.iter().all(|entry| entry.start.is_some())
        })
    {
        return Err(DashError::UnsupportedAddressing);
    }

    let segments = segment_urls
        .iter()
        .enumerate()
        .map(|(index, segment)| {
            if segment.index.is_some() || segment.indexRange.is_some() {
                return Err(DashError::UnsupportedAddressing);
            }
            let media = segment
                .media
                .as_deref()
                .filter(|media| !media.is_empty())
                .ok_or(DashError::InvalidSegment)?;
            let range = segment
                .mediaRange
                .as_deref()
                .map(parse_byte_range)
                .transpose()?;
            let (time, duration) = timeline
                .as_ref()
                .and_then(|entries| entries.get(index))
                .map_or((None, None), |entry| (entry.start, Some(entry.duration)));
            Ok(DashListSegment {
                media: media.to_owned(),
                range,
                time,
                duration,
            })
        })
        .collect::<Result<Vec<_>, DashError>>()?;

    if is_dynamic {
        let mut previous_end = None;
        for segment in &segments {
            let start = segment.time.ok_or(DashError::UnsupportedAddressing)?;
            let duration = segment.duration.ok_or(DashError::UnsupportedAddressing)?;
            let end = start
                .checked_add(duration)
                .ok_or(DashError::InvalidSegment)?;
            if previous_end.is_some_and(|previous| start != previous) {
                return Err(DashError::UnsupportedAddressing);
            }
            previous_end = Some(end);
        }
    }

    Ok(DashList {
        initialization,
        timescale,
        presentation_time_offset,
        availability_time_offset: Duration::ZERO,
        segments: segments.into_boxed_slice(),
    })
}

/// Parse DASH's inclusive first-last byte-range syntax.
fn parse_byte_range(raw: &str) -> Result<ManifestByteRange, DashError> {
    let (first, last) = raw.split_once('-').ok_or(DashError::InvalidSegment)?;
    let offset = first
        .parse::<u64>()
        .map_err(|_| DashError::InvalidSegment)?;
    let last = last.parse::<u64>().map_err(|_| DashError::InvalidSegment)?;
    let length = last
        .checked_sub(offset)
        .and_then(|distance| distance.checked_add(1))
        .ok_or(DashError::InvalidSegment)?;
    if offset.checked_add(length).is_none() {
        return Err(DashError::InvalidSegment);
    }
    Ok(ManifestByteRange { offset, length })
}

fn expand_dynamic_list(
    representation: &DashRepresentation,
    list: &DashList,
    dynamic: &DashDynamicPresentation,
) -> Result<Vec<DashResource>, DashError> {
    let Some(DashInitialization::Resource { url, range }) = &list.initialization else {
        return Err(DashError::UnsupportedAddressing);
    };
    if url.is_none() {
        return Err(DashError::UnsupportedAddressing);
    }
    let clock = dynamic
        .server_now
        .min(dynamic.availability_end_time.unwrap_or(dynamic.server_now));
    let elapsed = clock
        .duration_since(dynamic.availability_start_time)
        .ok()
        .and_then(|elapsed| elapsed.checked_sub(dynamic.period_start))
        .ok_or(DashError::NoAvailableSegments)?;

    let mut segment_duration = None;
    for segment in &list.segments {
        let time = segment.time.ok_or(DashError::UnsupportedAddressing)?;
        let duration = segment.duration.ok_or(DashError::UnsupportedAddressing)?;
        if duration == 0 || time % duration != 0 {
            return Err(DashError::UnsupportedAddressing);
        }
        if segment_duration.is_some_and(|previous| previous != duration) {
            return Err(DashError::UnsupportedAddressing);
        }
        segment_duration = Some(duration);
    }
    let segment_duration = segment_duration.ok_or(DashError::UnsupportedAddressing)?;
    let window_depth = dynamic.time_shift_buffer_depth.unwrap_or(
        dynamic
            .minimum_update_period
            .saturating_add(ticks_to_duration(segment_duration, list.timescale)?),
    );
    let available_elapsed = elapsed
        .checked_add(list.availability_time_offset)
        .ok_or(DashError::UnsupportedPresentation)?;
    let elapsed_ticks = duration_to_ticks(available_elapsed, list.timescale)?;
    let window_start_ticks =
        duration_to_ticks(elapsed.saturating_sub(window_depth), list.timescale)?;

    let initialization_url = url
        .as_deref()
        .map(|raw| resolve_resource(&representation.source.base_url, raw))
        .transpose()?
        .unwrap_or_else(|| representation.source.base_url.clone());
    let initialization_filename =
        super::resource_filename("dash-live-init", None, &initialization_url, *range);
    let mut resources = Vec::with_capacity(list.segments.len().saturating_add(1));
    resources.push(DashResource {
        url: initialization_url.clone(),
        range: *range,
        filename: initialization_filename,
        kind: DashResourceKind::Initialization,
        segment_number: None,
    });

    let mut whole_resources = HashSet::new();
    let mut ranged_resources: HashMap<String, Vec<ManifestByteRange>> = HashMap::new();
    for segment in &list.segments {
        let time = segment.time.ok_or(DashError::UnsupportedAddressing)?;
        let duration = segment.duration.ok_or(DashError::UnsupportedAddressing)?;
        let end = time
            .checked_add(duration)
            .ok_or(DashError::InvalidSegment)?;
        let url = resolve_resource(&representation.source.base_url, &segment.media)?;
        let url_key = url.as_str().to_owned();
        if let Some(segment_range) = segment.range {
            if whole_resources.contains(&url_key) {
                return Err(DashError::InvalidSegment);
            }
            ranged_resources
                .entry(url_key.clone())
                .or_default()
                .push(segment_range);
        } else if !whole_resources.insert(url_key.clone())
            || ranged_resources.contains_key(&url_key)
        {
            return Err(DashError::InvalidSegment);
        }
        if url == initialization_url {
            let (Some(initialization_range), Some(segment_range)) = (*range, segment.range) else {
                return Err(DashError::UnsupportedAddressing);
            };
            if ranges_overlap(initialization_range, segment_range)? {
                return Err(DashError::InvalidSegment);
            }
        }
        if end > elapsed_ticks || end <= window_start_ticks {
            continue;
        }
        let filename = super::resource_filename("dash-live-segment", None, &url, segment.range);
        let number = time
            .checked_div(segment_duration)
            .and_then(|index| index.checked_add(1))
            .ok_or(DashError::InvalidSegment)?;
        resources.push(DashResource {
            url,
            range: segment.range,
            filename,
            kind: DashResourceKind::Segment,
            segment_number: Some(number),
        });
    }
    for ranges in ranged_resources.values_mut() {
        ranges.sort_unstable_by_key(|range| range.offset);
        for pair in ranges.windows(2) {
            if ranges_overlap(pair[0], pair[1])? {
                return Err(DashError::InvalidSegment);
            }
        }
    }

    if resources.len() == 1 {
        return Err(DashError::NoAvailableSegments);
    }
    Ok(resources)
}

fn expand_dynamic_template(
    representation: &DashRepresentation,
    media_template: &str,
    template: &DashTemplate,
    dynamic: &DashDynamicPresentation,
) -> Result<Vec<DashResource>, DashError> {
    let clock = dynamic
        .server_now
        .min(dynamic.availability_end_time.unwrap_or(dynamic.server_now));
    let elapsed = clock
        .duration_since(dynamic.availability_start_time)
        .ok()
        .and_then(|elapsed| elapsed.checked_sub(dynamic.period_start))
        .ok_or(DashError::NoAvailableSegments)?;
    let available_elapsed = elapsed
        .checked_add(template.availability_time_offset)
        .ok_or(DashError::UnsupportedPresentation)?;
    let elapsed_ticks = duration_to_ticks(available_elapsed, template.timescale)?;
    let window_depth = match dynamic.time_shift_buffer_depth {
        Some(depth) => depth,
        None => {
            // When the MPD omits TSBD, retain at least one complete segment
            // beyond the advertised update interval so the next poll overlaps
            // the previous window instead of losing media between refreshes.
            let segment_ticks = template
                .duration_ticks
                .into_iter()
                .chain(
                    template
                        .timeline
                        .iter()
                        .flat_map(|entries| entries.iter().map(|entry| entry.duration)),
                )
                .max()
                .ok_or(DashError::UnsupportedAddressing)?;
            dynamic
                .minimum_update_period
                .saturating_add(ticks_to_duration(segment_ticks, template.timescale)?)
        }
    };
    let window_start_ticks =
        duration_to_ticks(elapsed.saturating_sub(window_depth), template.timescale)?;
    let segments = dynamic_segment_times(template, elapsed_ticks, window_start_ticks)?;

    let mut resources =
        Vec::with_capacity(segments.len() + usize::from(template.initialization.is_some()));
    if let Some(initialization) = &template.initialization {
        let (url, range) = match initialization {
            DashInitialization::Template(uri_template) => {
                let uri = substitute_template(
                    uri_template,
                    representation,
                    template.start_number,
                    0,
                    false,
                )?;
                (
                    resolve_resource(&representation.source.base_url, &uri)?,
                    None,
                )
            }
            DashInitialization::Resource { url, range } => {
                let resolved = match url.as_deref() {
                    Some(uri) => resolve_resource(&representation.source.base_url, uri)?,
                    None => representation.source.base_url.clone(),
                };
                (resolved, *range)
            }
        };
        let filename = super::resource_filename("dash-live-init", None, &url, range);
        resources.push(DashResource {
            url,
            range,
            filename,
            kind: DashResourceKind::Initialization,
            segment_number: None,
        });
    }

    for (number, time) in segments {
        let uri = substitute_template(media_template, representation, number, time, true)?;
        let url = resolve_resource(&representation.source.base_url, &uri)?;
        // URL identity is stable across sliding MPD windows and includes the
        // Number/Time-expanded URI; do not include the window-relative index.
        let filename = super::resource_filename("dash-live-segment", None, &url, None);
        resources.push(DashResource {
            url,
            range: None,
            filename,
            kind: DashResourceKind::Segment,
            segment_number: Some(number),
        });
    }
    Ok(resources)
}

fn expand_static_live_template(
    representation: &DashRepresentation,
    media_template: &str,
    template: &DashTemplate,
    require_period_alignment: bool,
) -> Result<Vec<DashResource>, DashError> {
    let mut resources = expand_template(
        representation,
        media_template,
        template,
        require_period_alignment,
    )?;
    for resource in &mut resources {
        resource.filename = match resource.kind {
            DashResourceKind::Initialization => {
                super::resource_filename("dash-live-init", None, &resource.url, resource.range)
            }
            DashResourceKind::Segment => {
                super::resource_filename("dash-live-segment", None, &resource.url, None)
            }
        };
    }
    Ok(resources)
}

fn ticks_to_duration(ticks: u64, timescale: u64) -> Result<Duration, DashError> {
    if timescale == 0 {
        return Err(DashError::InvalidSegment);
    }
    let nanos = u128::from(ticks)
        .checked_mul(1_000_000_000)
        .ok_or(DashError::InvalidSegment)?
        / u128::from(timescale);
    let seconds = u64::try_from(nanos / 1_000_000_000).map_err(|_| DashError::LimitExceeded)?;
    let subsecond_nanos =
        u32::try_from(nanos % 1_000_000_000).map_err(|_| DashError::InvalidSegment)?;
    Ok(Duration::new(seconds, subsecond_nanos))
}

fn duration_to_ticks(duration: Duration, timescale: u64) -> Result<u64, DashError> {
    let ticks = duration
        .as_nanos()
        .checked_mul(u128::from(timescale))
        .ok_or(DashError::InvalidSegment)?
        / 1_000_000_000;
    u64::try_from(ticks).map_err(|_| DashError::LimitExceeded)
}

fn dynamic_segment_times(
    template: &DashTemplate,
    elapsed_ticks: u64,
    window_start_ticks: u64,
) -> Result<Vec<(u64, u64)>, DashError> {
    let mut available = Vec::new();
    let mut number = template.start_number;

    if let Some(timeline) = template.timeline.as_deref() {
        let mut next_start = 0_u64;
        let mut previous_start = None;
        let mut previous_run_uses_negative_repeat = false;
        for (index, entry) in timeline.iter().enumerate() {
            let start = entry.start.unwrap_or(next_start);
            let starts_before_previous = previous_start.is_some_and(|previous| start < previous);
            let overlaps_previous_run =
                entry.start.is_some() && !previous_run_uses_negative_repeat && start < next_start;
            if starts_before_previous || overlaps_previous_run {
                return Err(DashError::InvalidSegment);
            }

            let total_count = match entry.repeat {
                repeat if repeat >= 0 => u64::try_from(repeat)
                    .ok()
                    .and_then(|repeat| repeat.checked_add(1))
                    .ok_or(DashError::InvalidSegment)?,
                -1 => {
                    let next_explicit_start = timeline.get(index + 1).and_then(|next| next.start);
                    match next_explicit_start {
                        Some(end) => {
                            let distance = end
                                .checked_sub(start)
                                .filter(|distance| *distance > 0)
                                .ok_or(DashError::InvalidSegment)?;
                            ceil_div_u64(distance, entry.duration)?
                        }
                        None => elapsed_ticks
                            .checked_sub(start)
                            .map_or(0, |remaining| remaining / entry.duration),
                    }
                }
                _ => return Err(DashError::InvalidSegment),
            };
            // SegmentTimeline entries after the inclusive endNumber are not
            // part of this representation. Check before the saturating count
            // calculation: saturating_sub would otherwise turn an exhausted
            // range into one extra segment.
            if template.end_number.is_some_and(|end| number > end) {
                break;
            }
            let total_count = total_count.min(
                template
                    .end_number
                    .map_or(u64::MAX, |end| end.saturating_sub(number).saturating_add(1)),
            );
            if total_count > MAX_DASH_SEGMENTS as u64 {
                return Err(DashError::LimitExceeded);
            }

            for offset in 0..total_count {
                let time = offset
                    .checked_mul(entry.duration)
                    .and_then(|offset| start.checked_add(offset))
                    .ok_or(DashError::InvalidSegment)?;
                let end = time
                    .checked_add(entry.duration)
                    .ok_or(DashError::InvalidSegment)?;
                if end <= elapsed_ticks && end > window_start_ticks {
                    available.push((number, time));
                    if available.len() > MAX_DASH_SEGMENTS {
                        return Err(DashError::LimitExceeded);
                    }
                }
                number = number.checked_add(1).ok_or(DashError::InvalidSegment)?;
            }

            let span = total_count
                .checked_mul(entry.duration)
                .ok_or(DashError::InvalidSegment)?;
            next_start = start.checked_add(span).ok_or(DashError::InvalidSegment)?;
            previous_start = Some(start);
            previous_run_uses_negative_repeat = entry.repeat == -1;
        }
    } else {
        let duration = template
            .duration_ticks
            .filter(|duration| *duration > 0)
            .ok_or(DashError::UnsupportedAddressing)?;
        let complete_count = elapsed_ticks / duration;
        let bounded_count = template.end_number.map_or(complete_count, |end| {
            complete_count.min(end.saturating_sub(number).saturating_add(1))
        });
        let first_available = (window_start_ticks / duration).min(bounded_count);
        let first_index = if bounded_count == 0 {
            0
        } else {
            first_available.min(bounded_count.saturating_sub(1))
        };
        let start_index = if bounded_count == 0 { 0 } else { first_index };
        let start_number = number
            .checked_add(start_index)
            .ok_or(DashError::InvalidSegment)?;
        let mut index = start_index;
        while index < bounded_count {
            if available.len() >= MAX_DASH_SEGMENTS {
                return Err(DashError::LimitExceeded);
            }
            let time = index
                .checked_mul(duration)
                .ok_or(DashError::InvalidSegment)?;
            available.push((
                start_number
                    .checked_add(index - start_index)
                    .ok_or(DashError::InvalidSegment)?,
                time,
            ));
            index += 1;
        }
    }

    Ok(available)
}

fn expand_template(
    representation: &DashRepresentation,
    media_template: &str,
    template: &DashTemplate,
    require_period_alignment: bool,
) -> Result<Vec<DashResource>, DashError> {
    let segments = expand_segment_times(template)?;
    if require_period_alignment {
        let period_ticks = period_duration_to_ticks(template.period_duration, template.timescale)?;
        let expected_end = template
            .presentation_time_offset
            .checked_add(period_ticks)
            .ok_or(DashError::UnsupportedPresentation)?;
        let mut expected_start = template.presentation_time_offset;
        for (_, start, duration) in &segments {
            if *start != expected_start {
                return Err(DashError::UnsupportedPresentation);
            }
            expected_start = start
                .checked_add(*duration)
                .ok_or(DashError::UnsupportedPresentation)?;
        }
        if expected_start != expected_end {
            return Err(DashError::UnsupportedPresentation);
        }
    }
    if segments.is_empty() || segments.len() > MAX_DASH_SEGMENTS {
        return Err(if segments.len() > MAX_DASH_SEGMENTS {
            DashError::LimitExceeded
        } else {
            DashError::InvalidSegment
        });
    }

    let initialization_count = if template.initialization.is_some() {
        1
    } else {
        0
    };
    let mut resources = Vec::with_capacity(segments.len() + initialization_count);

    if let Some(initialization) = &template.initialization {
        let (url, range) = match initialization {
            DashInitialization::Template(uri_template) => {
                let uri = substitute_template(
                    uri_template,
                    representation,
                    template.start_number,
                    0,
                    false,
                )?;
                (
                    resolve_resource(&representation.source.base_url, &uri)?,
                    None,
                )
            }
            DashInitialization::Resource { url, range } => {
                let resolved = match url.as_deref() {
                    Some(uri) => resolve_resource(&representation.source.base_url, uri)?,
                    None => representation.source.base_url.clone(),
                };
                (resolved, *range)
            }
        };
        let filename = super::resource_filename("dash-init", None, &url, range);
        resources.push(DashResource {
            url,
            range,
            filename,
            kind: DashResourceKind::Initialization,
            segment_number: None,
        });
    }

    for (index, (number, time, _duration)) in segments.into_iter().enumerate() {
        let uri = substitute_template(media_template, representation, number, time, true)?;
        let url = resolve_resource(&representation.source.base_url, &uri)?;
        let filename = super::resource_filename("dash-segment", Some(index), &url, None);
        resources.push(DashResource {
            url,
            range: None,
            filename,
            kind: DashResourceKind::Segment,
            segment_number: Some(number),
        });
    }

    Ok(resources)
}

fn expand_segment_times(template: &DashTemplate) -> Result<Vec<(u64, u64, u64)>, DashError> {
    let mut output = Vec::new();
    let mut number = template.start_number;

    if let Some(timeline) = &template.timeline {
        let mut next_start = 0_u64;
        let mut previous_start = None;
        let mut previous_run_uses_negative_repeat = false;
        let mut expanded_count = 0_usize;

        for (index, entry) in timeline.iter().enumerate() {
            let start = entry.start.unwrap_or(next_start);
            let starts_before_previous = previous_start.is_some_and(|previous| start < previous);
            let overlaps_previous_run =
                entry.start.is_some() && !previous_run_uses_negative_repeat && start < next_start;
            if starts_before_previous || overlaps_previous_run {
                return Err(DashError::InvalidSegment);
            }

            let count = match entry.repeat {
                repeat if repeat >= 0 => u64::try_from(repeat)
                    .ok()
                    .and_then(|repeat| repeat.checked_add(1))
                    .ok_or(DashError::InvalidSegment)?,
                -1 => {
                    // The negative repeat is bounded only by the next S@t,
                    // not by a later explicit timestamp. DASH defines the
                    // number of repeats with a ceiling, so a final segment
                    // may overlap the next S@t or Period end.
                    let next_explicit_start = timeline.get(index + 1).and_then(|next| next.start);
                    match next_explicit_start {
                        Some(end) => {
                            let distance = end
                                .checked_sub(start)
                                .filter(|distance| *distance > 0)
                                .ok_or(DashError::InvalidSegment)?;
                            ceil_div_u64(distance, entry.duration)?
                        }
                        None => segments_until_period_end(
                            template.period_duration,
                            template.timescale,
                            template.presentation_time_offset,
                            start,
                            entry.duration,
                        )?,
                    }
                }
                _ => return Err(DashError::InvalidSegment),
            };

            for repeat in 0..count {
                if template.end_number.is_some_and(|end| number > end) {
                    return Ok(output);
                }
                expanded_count = expanded_count
                    .checked_add(1)
                    .filter(|count| *count <= MAX_DASH_SEGMENTS)
                    .ok_or(DashError::LimitExceeded)?;

                // S@t is on the sample timeline and remains the value used for
                // $Time$. PTO only maps that timestamp onto the Period timeline.
                let time = repeat
                    .checked_mul(entry.duration)
                    .and_then(|offset| start.checked_add(offset))
                    .ok_or(DashError::InvalidSegment)?;
                if !time_precedes_period_end(
                    time,
                    template.period_duration,
                    template.timescale,
                    template.presentation_time_offset,
                )? {
                    return Err(DashError::InvalidSegment);
                }

                let segment_end = time
                    .checked_add(entry.duration)
                    .ok_or(DashError::InvalidSegment)?;
                if segment_end > template.presentation_time_offset {
                    if time < template.presentation_time_offset {
                        // A segment crossing the Period start cannot be trimmed
                        // by the current resource planner; leave it to yt-dlp.
                        return Err(DashError::UnsupportedAddressing);
                    }
                    output.push((number, time, entry.duration));
                }
                number = number.checked_add(1).ok_or(DashError::InvalidSegment)?;
            }

            let span = count
                .checked_mul(entry.duration)
                .ok_or(DashError::InvalidSegment)?;
            next_start = start.checked_add(span).ok_or(DashError::InvalidSegment)?;
            previous_start = Some(start);
            previous_run_uses_negative_repeat = entry.repeat == -1;
        }

        return Ok(output);
    }

    let duration_ticks = template
        .duration_ticks
        .ok_or(DashError::UnsupportedAddressing)?;
    let numerator = template
        .period_duration
        .as_nanos()
        .checked_mul(u128::from(template.timescale))
        .ok_or(DashError::InvalidSegment)?;
    let denominator = u128::from(duration_ticks)
        .checked_mul(1_000_000_000)
        .ok_or(DashError::InvalidSegment)?;
    let count = ceil_div_u128(numerator, denominator)?;
    let count = usize::try_from(count).map_err(|_| DashError::LimitExceeded)?;
    if count == 0 || count > MAX_DASH_SEGMENTS {
        return Err(if count > MAX_DASH_SEGMENTS {
            DashError::LimitExceeded
        } else {
            DashError::InvalidSegment
        });
    }

    for index in 0..count {
        if template.end_number.is_some_and(|end| number > end) {
            break;
        }
        let offset = u64::try_from(index).map_err(|_| DashError::LimitExceeded)?;
        let time = offset
            .checked_mul(duration_ticks)
            .and_then(|offset| template.presentation_time_offset.checked_add(offset))
            .ok_or(DashError::InvalidSegment)?;
        if !time_precedes_period_end(
            time,
            template.period_duration,
            template.timescale,
            template.presentation_time_offset,
        )? {
            return Err(DashError::InvalidSegment);
        }
        output.push((number, time, duration_ticks));
        number = number.checked_add(1).ok_or(DashError::InvalidSegment)?;
    }

    Ok(output)
}

fn ceil_div_u64(numerator: u64, denominator: u64) -> Result<u64, DashError> {
    if denominator == 0 {
        return Err(DashError::InvalidSegment);
    }
    let numerator = u128::from(numerator);
    let denominator = u128::from(denominator);
    ceil_div_u128(numerator, denominator)
}

fn ceil_div_u128(numerator: u128, denominator: u128) -> Result<u64, DashError> {
    if denominator == 0 {
        return Err(DashError::InvalidSegment);
    }
    let quotient = numerator
        .checked_add(denominator - 1)
        .ok_or(DashError::InvalidSegment)?
        / denominator;
    u64::try_from(quotient).map_err(|_| DashError::LimitExceeded)
}

fn period_end_on_sample_timeline(
    period_duration: Duration,
    timescale: u64,
    presentation_time_offset: u64,
) -> Result<u128, DashError> {
    let period_duration_scaled = period_duration
        .as_nanos()
        .checked_mul(u128::from(timescale))
        .ok_or(DashError::InvalidSegment)?;
    let period_start_scaled = u128::from(presentation_time_offset)
        .checked_mul(1_000_000_000)
        .ok_or(DashError::InvalidSegment)?;
    period_start_scaled
        .checked_add(period_duration_scaled)
        .ok_or(DashError::InvalidSegment)
}

fn segments_until_period_end(
    period_duration: Duration,
    timescale: u64,
    presentation_time_offset: u64,
    segment_start: u64,
    segment_duration: u64,
) -> Result<u64, DashError> {
    if segment_duration == 0 {
        return Err(DashError::InvalidSegment);
    }
    let period_end_scaled =
        period_end_on_sample_timeline(period_duration, timescale, presentation_time_offset)?;
    let start_scaled = u128::from(segment_start)
        .checked_mul(1_000_000_000)
        .ok_or(DashError::InvalidSegment)?;
    let remaining = period_end_scaled
        .checked_sub(start_scaled)
        .filter(|remaining| *remaining > 0)
        .ok_or(DashError::InvalidSegment)?;
    let segment_duration_scaled = u128::from(segment_duration)
        .checked_mul(1_000_000_000)
        .ok_or(DashError::InvalidSegment)?;
    ceil_div_u128(remaining, segment_duration_scaled)
}

fn time_precedes_period_end(
    segment_start: u64,
    period_duration: Duration,
    timescale: u64,
    presentation_time_offset: u64,
) -> Result<bool, DashError> {
    let period_end_scaled =
        period_end_on_sample_timeline(period_duration, timescale, presentation_time_offset)?;
    let segment_start_scaled = u128::from(segment_start)
        .checked_mul(1_000_000_000)
        .ok_or(DashError::InvalidSegment)?;
    Ok(segment_start_scaled < period_end_scaled)
}

fn substitute_template(
    template: &str,
    representation: &DashRepresentation,
    number: u64,
    time: u64,
    allow_segment_tokens: bool,
) -> Result<String, DashError> {
    let mut output = String::with_capacity(template.len() + 32);
    let mut chars = template.chars().peekable();

    while let Some(character) = chars.next() {
        if character != '$' {
            output.push(character);
            continue;
        }

        if chars.peek() == Some(&'$') {
            chars.next();
            output.push('$');
            continue;
        }

        let mut token = String::new();
        let mut closed = false;
        for next in chars.by_ref() {
            if next == '$' {
                closed = true;
                break;
            }
            token.push(next);
        }
        if !closed || token.is_empty() {
            return Err(DashError::InvalidSegment);
        }

        let (name, width) = match token.split_once("%0") {
            Some((name, format)) => {
                let width = format
                    .strip_suffix('d')
                    .and_then(|digits| digits.parse::<usize>().ok())
                    .filter(|width| (1..=32).contains(width))
                    .ok_or(DashError::InvalidSegment)?;
                (name, Some(width))
            }
            None => (token.as_str(), None),
        };

        let value = match name {
            "RepresentationID" if width.is_none() => {
                let id = representation
                    .representation_id
                    .as_deref()
                    .filter(|id| !id.is_empty())
                    .filter(|id| {
                        id.chars().all(|character| {
                            character.is_ascii_alphanumeric()
                                || matches!(character, '-' | '_' | '.' | '~')
                        })
                    })
                    .ok_or(DashError::InvalidSegment)?;
                id.to_owned()
            }
            "Number" if allow_segment_tokens => format_number(number, width),
            "Time" if allow_segment_tokens => format_number(time, width),
            "Bandwidth" => format_number(
                representation.bandwidth.ok_or(DashError::InvalidSegment)?,
                width,
            ),
            _ => return Err(DashError::UnsupportedAddressing),
        };
        output.push_str(&value);
    }

    Ok(output)
}

fn format_number(value: u64, width: Option<usize>) -> String {
    match width {
        Some(width) => format!("{value:0width$}"),
        None => value.to_string(),
    }
}

fn resolve_resource(base_url: &Url, raw: &str) -> Result<Url, DashError> {
    let resolved = base_url
        .join(raw)
        .map_err(|_| DashError::UnsupportedAddressing)?;
    let resolved = validate_https_url(resolved)?;
    validate_media_url(&resolved)?;
    Ok(resolved)
}

fn validate_media_url(url: &Url) -> Result<(), DashError> {
    if url.path().to_ascii_lowercase().ends_with(".webm") {
        return Err(DashError::UnsupportedPresentation);
    }
    Ok(())
}

/// Return whether two checked half-open ranges overlap.
fn ranges_overlap(left: ManifestByteRange, right: ManifestByteRange) -> Result<bool, DashError> {
    let left_end = left
        .offset
        .checked_add(left.length)
        .ok_or(DashError::InvalidSegment)?;
    let right_end = right
        .offset
        .checked_add(right.length)
        .ok_or(DashError::InvalidSegment)?;
    Ok(left.offset < right_end && right.offset < left_end)
}

fn expand_list(
    representation: &DashRepresentation,
    list: &DashList,
) -> Result<Vec<DashResource>, DashError> {
    let base_url = &representation.source.base_url;
    let initialization_count = if list.initialization.is_some() { 1 } else { 0 };
    let mut resources = Vec::with_capacity(list.segments.len() + initialization_count);

    if let Some(initialization) = &list.initialization {
        let (url, range) = match initialization {
            DashInitialization::Template(_) => return Err(DashError::UnsupportedAddressing),
            DashInitialization::Resource { url, range } => {
                let resolved = match url.as_deref() {
                    Some(uri) => resolve_resource(base_url, uri)?,
                    None => base_url.clone(),
                };
                (resolved, *range)
            }
        };
        let filename = super::resource_filename("dash-init", None, &url, range);
        resources.push(DashResource {
            url,
            range,
            filename,
            kind: DashResourceKind::Initialization,
            segment_number: None,
        });
    }

    for (index, segment) in list.segments.iter().enumerate() {
        let url = resolve_resource(base_url, &segment.media)?;
        let filename = super::resource_filename("dash-segment", Some(index), &url, segment.range);
        resources.push(DashResource {
            url,
            range: segment.range,
            filename,
            kind: DashResourceKind::Segment,
            segment_number: Some(u64::try_from(index).map_err(|_| DashError::LimitExceeded)?),
        });
    }

    Ok(resources)
}

#[cfg(test)]
#[path = "manifest_dash_tests.rs"]
mod tests;
