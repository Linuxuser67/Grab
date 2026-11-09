//! Static DASH parsing and period-aware segment resource planning.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;

use super::dash_roles::{self, DASH_ROLE_SCHEME};
use super::{MAX_MANIFEST_BYTES, ManifestByteRange};

#[path = "manifest_dash_sidx.rs"]
mod sidx;

#[path = "manifest_dash_ll.rs"]
mod low_latency;
#[cfg(test)]
#[path = "manifest_dash_ll_tests.rs"]
mod low_latency_tests;
#[cfg(test)]
pub(crate) use sidx::resolve_segment_base_index;
pub(crate) use sidx::{
    DashSidxEntry, MAX_DASH_SIDX_SEGMENTS, dash_sidx_media_resource, parse_child_segment_index,
    parse_segment_base_index, validate_segment_base_sidx_ranges,
};
const MAX_REPRESENTATIONS: usize = 1_024;
const MAX_SEGMENTS: usize = 100_000;
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
    #[error(
        "DASH presentation contains SCTE-35 ad signaling that native download cannot safely remove"
    )]
    AdSignaling,
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
    /// SegmentTemplate Number, SegmentList index, or 0 for a single-file SegmentBase; initialization resources use None.
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
    pub(crate) segment_base_index: Option<DashSegmentBaseIndex>,
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

/// Information needed to resolve a static SegmentBase index into media ranges.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DashSegmentBaseIndex {
    /// Media resource containing the referenced byte ranges.
    pub(crate) url: Url,
    /// Resource containing the SIDX box; this may differ from the media URL.
    pub(crate) index_url: Url,
    pub(crate) index_range: ManifestByteRange,
    pub(crate) index_is_external: bool,
    /// Initialization resource, which may be separate from the indexed media resource.
    pub(crate) initialization_url: Url,
    pub(crate) initialization_is_external: bool,
    pub(crate) initialization_range: Option<ManifestByteRange>,
    pub(crate) index_range_exact: bool,
    pub(crate) timescale: Option<u64>,
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
        DashAddressing::List(list) => (list.timescale, 0),
        DashAddressing::Base(base) => (base.timescale, base.presentation_time_offset),
        DashAddressing::IndexedBase(index) => {
            (index.timescale.unwrap_or(1), index.presentation_time_offset)
        }
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
    /// A single media file addressed by `BaseURL` (SegmentBase).
    Base(DashBase),
    /// A static single-file representation whose SIDX describes byte ranges.
    IndexedBase(DashBaseIndex),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DashBase {
    timescale: u64,
    presentation_time_offset: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DashBaseIndex {
    index_range: ManifestByteRange,
    index_url: Option<String>,
    initialization_url: Option<String>,
    initialization_range: Option<ManifestByteRange>,
    index_range_exact: bool,
    timescale: Option<u64>,
    presentation_time_offset: u64,
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

fn period_has_scte35_ad_signaling(period: &dash_mpd_core::Period) -> bool {
    fn is_scte35_scheme(scheme: &str) -> bool {
        scheme
            .trim()
            .to_ascii_lowercase()
            .starts_with("urn:scte:scte35:")
    }

    period
        .event_streams
        .iter()
        .any(|stream| is_scte35_scheme(&stream.schemeIdUri))
        || period.adaptations.iter().any(|adaptation| {
            adaptation
                .InbandEventStream
                .iter()
                .any(|stream| is_scte35_scheme(&stream.schemeIdUri))
                || adaptation.representations.iter().any(|representation| {
                    representation
                        .InbandEventStream
                        .iter()
                        .any(|stream| is_scte35_scheme(&stream.schemeIdUri))
                })
        })
}

/// Parse static period-continuous or bounded single-period dynamic DASH into selectable representations.
///
/// Dynamic support is limited to HTTPS SegmentTemplate presentations with a
/// finite MPD update interval. The bounded LL-DASH profile also accepts
/// SegmentTemplate availability offsets and chunked, incomplete media responses.
/// Static SegmentBase supports
/// either a single-file download or indexed byte-range resources described by a
/// flat SIDX and an Initialization range. Single-file assets are still subject
/// to the 1 GiB per-resource limit. Multiple static periods require explicit DASH-IF
/// period-continuity signaling, stable adaptation/representation IDs, and contiguous timing.
/// DRM, XLink, adaptation-set
/// Subset/Preselection combinations, dynamic SegmentList, SegmentBase
/// with an Initialization@sourceURL or non-zero presentation-time offset,
/// hierarchical SIDX, BaseURL-level availability signalling, and WebM
/// remain on yt-dlp fallback.
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
    if mpd.periods.iter().any(period_has_scte35_ad_signaling) {
        return Err(DashError::AdSignaling);
    }
    if dash_roles::has_nonzero_segment_list_presentation_time_offset(body.as_bytes())
        .ok_or(DashError::Parse)?
    {
        return Err(DashError::UnsupportedAddressing);
    }

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
    if is_dynamic
        && (dash_roles::has_nondefault_segment_list_start_number(body.as_bytes())
            .ok_or(DashError::Parse)?
            || dash_roles::has_unsupported_segment_list_timing_attributes(body.as_bytes())
                .ok_or(DashError::Parse)?)
    {
        return Err(DashError::UnsupportedAddressing);
    }
    if !mpd.PatchLocation.is_empty()
        || mpd.periods.is_empty()
        || mpd.periods.len() > dash_roles::MAX_PERIODS
        || !mpd.ContentProtection.is_empty()
        || (is_dynamic && mpd.periods.len() != 1)
    {
        return Err(DashError::UnsupportedPresentation);
    }

    // The typed parser and bounded metadata pass must describe the same source-order
    // structure for every Period; otherwise a foreign element could shift role indexes.
    let representation_metadata =
        dash_roles::representation_metadata(body.as_bytes(), MAX_REPRESENTATIONS)
            .ok_or(DashError::Parse)?;
    if representation_metadata.len() != mpd.periods.len()
        || mpd
            .periods
            .iter()
            .zip(&representation_metadata)
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
        mpd.periods.iter().zip(&representation_metadata).enumerate()
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
                let addressing = effective_addressing(
                    period,
                    adaptation,
                    representation,
                    addressing_duration,
                    is_dynamic,
                )?;
                if let DashAddressing::Template(template) = &addressing {
                    low_latency::validate_template_mime(
                        template.is_low_latency(),
                        mime_type.as_deref(),
                    )?;
                }
                if matches!(
                    &addressing,
                    DashAddressing::Base(_) | DashAddressing::IndexedBase(_)
                ) && !is_single_file_media_url(&representation_base, &manifest_url)
                {
                    return Err(DashError::UnsupportedAddressing);
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

    // Concatenating independently addressed SegmentBase files or SIDX indexes
    // would require container-aware remapping. Multi-period native assembly is
    // therefore limited to fragment-addressed resources with a shared init.
    if sources.len() > 1
        && sources.iter().any(|source| {
            matches!(
                &source.addressing,
                DashAddressing::Base(_) | DashAddressing::IndexedBase(_)
            )
        })
    {
        return Err(DashError::UnsupportedAddressing);
    }

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
            (
                DashPresentation::Dynamic(_),
                DashAddressing::Base(_) | DashAddressing::IndexedBase(_),
            ) => {
                return Err(DashError::UnsupportedAddressing);
            }
            (DashPresentation::Static, DashAddressing::Base(_)) => {
                expand_base(&period_representation)?
            }
            (DashPresentation::Static, DashAddressing::IndexedBase(index)) => {
                expand_indexed_base(&period_representation, index)?
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
            .filter(|count| *count <= MAX_SEGMENTS)
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
    let segment_base_index = match (&representation.source.addressing, sources.len()) {
        (DashAddressing::IndexedBase(index), 1) => {
            let index_url = index
                .index_url
                .as_deref()
                .map(|raw| resolve_resource(&representation.source.base_url, raw))
                .transpose()?
                .unwrap_or_else(|| representation.source.base_url.clone());
            let index_is_external = index_url != representation.source.base_url;
            let initialization_url = index
                .initialization_url
                .as_deref()
                .map(|raw| resolve_resource(&representation.source.base_url, raw))
                .transpose()?
                .unwrap_or_else(|| representation.source.base_url.clone());
            let initialization_is_external = initialization_url != representation.source.base_url;
            if !initialization_is_external && index.initialization_range.is_none() {
                return Err(DashError::UnsupportedAddressing);
            }
            Some(DashSegmentBaseIndex {
                url: representation.source.base_url.clone(),
                index_url,
                index_range: index.index_range,
                index_is_external,
                initialization_url,
                initialization_is_external,
                initialization_range: index.initialization_range,
                index_range_exact: index.index_range_exact,
                timescale: index.timescale,
            })
        }
        (DashAddressing::IndexedBase(_), _) => return Err(DashError::UnsupportedAddressing),
        _ => None,
    };

    if !matches!(&manifest.presentation, DashPresentation::Dynamic(_))
        && segment_base_index.is_none()
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
        segment_base_index,
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
        DashAddressing::Base(_) => {
            hasher.update(b"base");
        }
        DashAddressing::IndexedBase(index) => {
            hasher.update(b"indexed-base");
            hash_optional_range(&mut hasher, Some(index.index_range));
            hash_optional_range(&mut hasher, index.initialization_range);
            hash_optional_text(&mut hasher, index.index_url.as_deref());
            // Keep the historical key byte-for-byte when Initialization uses
            // an in-file range; only the newly supported addressing adds identity.
            hash_separate_initialization_identity(
                &mut hasher,
                base_url,
                index.initialization_url.as_deref(),
            );
            hasher.update([u8::from(index.index_range_exact)]);
            hash_optional_number(&mut hasher, index.timescale);
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
        DashAddressing::Base(_) => {
            hasher.update(b"base");
        }
        DashAddressing::IndexedBase(index) => {
            hasher.update(b"indexed-base");
            hash_optional_range(&mut hasher, Some(index.index_range));
            hash_optional_range(&mut hasher, index.initialization_range);
            hash_optional_text(&mut hasher, index.index_url.as_deref());
            hash_separate_initialization_identity(
                &mut hasher,
                base_url,
                index.initialization_url.as_deref(),
            );
            hasher.update([u8::from(index.index_range_exact)]);
            hash_optional_number(&mut hasher, index.timescale);
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

/// A `SegmentBase` BaseURL must name a media file, not the MPD itself or a
/// directory; otherwise the resolved URL cannot be the whole media resource.
fn is_single_file_media_url(url: &Url, manifest_url: &Url) -> bool {
    url.path_segments()
        .and_then(|mut segments| segments.next_back())
        .is_some_and(|segment| !segment.is_empty())
        && url.path() != manifest_url.path()
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
                || candidate
                    .availability_time_offset
                    .is_some_and(|offset| offset != 0.0)
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
                || candidate
                    .availability_time_offset
                    .is_some_and(|offset| offset != 0.0)
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

    let has_template = templates.iter().any(Option::is_some);
    let has_list = lists.iter().any(Option::is_some);
    if bases.iter().any(Option::is_some) {
        if is_dynamic || has_template || has_list {
            return Err(DashError::UnsupportedAddressing);
        }
        let mut index_range = None;
        let mut initialization_range = None;
        let mut initialization_source_url = None;
        let mut index_range_exact = None;
        let mut timescale = None;
        let mut presentation_time_offset = None;
        let mut index_source_url = None;
        let mut has_representation_index = false;
        for base in bases.iter().flatten() {
            if base.timescale == Some(0)
                || base
                    .availabilityTimeOffset
                    .is_some_and(|offset| offset != 0.0)
                || base.availabilityTimeComplete == Some(false)
                || base.eptDelta.is_some_and(|delta| delta != 0)
                || base.pbDelta.is_some_and(|delta| delta != 0)
                || base.failover_content.is_some()
            {
                return Err(DashError::UnsupportedAddressing);
            }

            index_range = base.indexRange.as_deref().or(index_range);
            index_range_exact = base.indexRangeExact.or(index_range_exact);
            timescale = base.timescale.or(timescale);
            presentation_time_offset = base.presentationTimeOffset.or(presentation_time_offset);
            if let Some(representation_index) = base.representation_index.as_ref() {
                // Child elements replace inherited children rather than
                // inheriting individual attributes from the parent element.
                has_representation_index = true;
                index_source_url = representation_index
                    .sourceURL
                    .as_deref()
                    .map(str::trim)
                    .filter(|source| !source.is_empty())
                    .map(str::to_string);
                if let Some(range) = representation_index.range.as_deref() {
                    index_range = Some(range);
                }
            }
            if let Some(initialization) = base.Initialization.as_ref() {
                // An Initialization child replaces the inherited child; missing
                // attributes must not silently reuse values from the parent.
                let source_url = initialization.sourceURL.as_deref().map(str::trim);
                if source_url.is_some_and(str::is_empty) {
                    return Err(DashError::InvalidSegment);
                }
                initialization_source_url = source_url.map(str::to_string);
                initialization_range = Some(initialization.range.as_deref());
            }
        }

        if has_representation_index && index_range.is_none() {
            // An index without an explicit bounded range would require an
            // unbounded whole-resource fetch.
            return Err(DashError::UnsupportedAddressing);
        }
        if index_range.is_none() && index_range_exact.is_some() {
            return Err(DashError::UnsupportedAddressing);
        }

        if let Some(index_range) = index_range {
            let index_range = parse_byte_range(index_range)?;
            let initialization_range = initialization_range
                .flatten()
                .map(parse_byte_range)
                .transpose()?;
            if initialization_source_url.is_none() && initialization_range.is_none() {
                return Err(DashError::UnsupportedAddressing);
            }
            if timescale == Some(0)
                || (initialization_source_url.is_none()
                    && index_source_url.is_none()
                    && ranges_overlap(
                        index_range,
                        initialization_range.ok_or(DashError::UnsupportedAddressing)?,
                    )?)
            {
                return Err(DashError::UnsupportedAddressing);
            }
            return Ok(DashAddressing::IndexedBase(DashBaseIndex {
                index_range,
                index_url: index_source_url,
                initialization_url: initialization_source_url,
                initialization_range,
                index_range_exact: index_range_exact.unwrap_or(false),
                timescale,
                presentation_time_offset: presentation_time_offset.unwrap_or(0),
            }));
        }
        if has_representation_index
            || initialization_source_url.is_some()
            || initialization_range.is_some()
        {
            return Err(DashError::UnsupportedAddressing);
        }
        return Ok(DashAddressing::Base(DashBase {
            timescale: timescale.unwrap_or(1),
            presentation_time_offset: presentation_time_offset.unwrap_or(0),
        }));
    }

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
            effective_list(&lists, is_dynamic).map(DashAddressing::List)?
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
    if segment_urls.is_empty() || segment_urls.len() > MAX_SEGMENTS {
        return Err(if segment_urls.len() > MAX_SEGMENTS {
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
    let elapsed_ticks = duration_to_ticks(elapsed, list.timescale)?;
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
            if total_count > MAX_SEGMENTS as u64 {
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
                    if available.len() > MAX_SEGMENTS {
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
            if available.len() >= MAX_SEGMENTS {
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
    if segments.is_empty() || segments.len() > MAX_SEGMENTS {
        return Err(if segments.len() > MAX_SEGMENTS {
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
                    .filter(|count| *count <= MAX_SEGMENTS)
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
    if count == 0 || count > MAX_SEGMENTS {
        return Err(if count > MAX_SEGMENTS {
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

/// A single-file `SegmentBase` representation: the `BaseURL` is the media file.
fn expand_base(representation: &DashRepresentation) -> Result<Vec<DashResource>, DashError> {
    // Retain the whole-file path for unindexed SegmentBase representations.
    let url = representation.source.base_url.clone();
    validate_media_url(&url)?;
    let filename = super::resource_filename("dash-segment", Some(0), &url, None);
    Ok(vec![DashResource {
        url,
        range: None,
        filename,
        kind: DashResourceKind::Segment,
        segment_number: Some(0),
    }])
}

fn expand_indexed_base(
    representation: &DashRepresentation,
    index: &DashBaseIndex,
) -> Result<Vec<DashResource>, DashError> {
    let media_url = representation.source.base_url.clone();
    let url = index
        .initialization_url
        .as_deref()
        .map(|raw| resolve_resource(&media_url, raw))
        .transpose()?
        .unwrap_or_else(|| media_url.clone());
    validate_media_url(&url)?;
    if url == media_url && index.initialization_range.is_none() {
        return Err(DashError::UnsupportedAddressing);
    }
    let filename = super::resource_filename("dash-init", Some(0), &url, index.initialization_range);
    Ok(vec![DashResource {
        url,
        range: index.initialization_range,
        filename,
        kind: DashResourceKind::Initialization,
        segment_number: None,
    }])
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
mod tests {
    use super::*;

    const BASE_URL: &str = "https://cdn.example.test/path/manifest.mpd";
    #[derive(Clone)]
    struct SharedLogWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for SharedLogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let mut output = self
                .0
                .lock()
                .map_err(|_| std::io::Error::other("DASH test log buffer is poisoned"))?;
            output.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedLogWriter {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn capture_dash_warnings(
        parse: impl FnOnce() -> Result<DashManifestPlan, DashError>,
    ) -> (Result<DashManifestPlan, DashError>, String) {
        let output = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(SharedLogWriter(std::sync::Arc::clone(&output)))
            .finish();

        let result = tracing::subscriber::with_default(subscriber, parse);
        let captured = output.lock().expect("DASH diagnostic output lock").clone();
        (
            result,
            String::from_utf8(captured).expect("DASH diagnostic logs are UTF-8"),
        )
    }

    fn build_sidx_box(first_offset: u32, references: &[(u32, u32, u32)]) -> Vec<u8> {
        let size = 32_u32 + u32::try_from(references.len()).expect("test reference count") * 12;
        let mut bytes = Vec::with_capacity(usize::try_from(size).expect("test index size"));
        bytes.extend_from_slice(&size.to_be_bytes());
        bytes.extend_from_slice(b"sidx");
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes.extend_from_slice(&1_u32.to_be_bytes());
        bytes.extend_from_slice(&1_000_u32.to_be_bytes());
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(&first_offset.to_be_bytes());
        bytes.extend_from_slice(&0_u16.to_be_bytes());
        bytes.extend_from_slice(
            &u16::try_from(references.len())
                .expect("test reference count")
                .to_be_bytes(),
        );
        for (reference, duration, sap) in references {
            bytes.extend_from_slice(&reference.to_be_bytes());
            bytes.extend_from_slice(&duration.to_be_bytes());
            bytes.extend_from_slice(&sap.to_be_bytes());
        }
        bytes
    }

    fn test_range(bytes: &[u8]) -> ManifestByteRange {
        ManifestByteRange {
            offset: 100,
            length: u64::try_from(bytes.len()).expect("test SIDX length"),
        }
    }

    fn indexed_base_plan(
        initialization_range: ManifestByteRange,
        index_range: ManifestByteRange,
        index_range_exact: bool,
    ) -> DashSegmentBaseIndex {
        DashSegmentBaseIndex {
            url: Url::parse("https://cdn.example.test/path/media.mp4").expect("test media URL"),
            index_url: Url::parse("https://cdn.example.test/path/media.mp4")
                .expect("test index URL"),
            index_range,
            index_is_external: false,
            initialization_url: Url::parse("https://cdn.example.test/path/media.mp4")
                .expect("test initialization URL"),
            initialization_is_external: false,
            initialization_range: Some(initialization_range),
            index_range_exact,
            timescale: Some(1_000),
        }
    }

    #[test]
    fn plans_indexed_segment_base_with_initialization_range() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <BaseURL>media.mp4</BaseURL>
  <Period duration="PT2S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <Representation id="v1">
        <SegmentBase timescale="1000" indexRange="100-143" indexRangeExact="true">
          <Initialization range="0-99"/>
        </SegmentBase>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let manifest =
            parse_dash_manifest(BASE_URL, body).expect("indexed SegmentBase should parse");
        let representation = &manifest.video_representations[0];
        let plan = plan_dash_representation(&manifest, &representation.key)
            .expect("indexed SegmentBase should produce a deferred SIDX plan");

        let index = plan
            .segment_base_index
            .as_ref()
            .expect("deferred SIDX request");
        assert_eq!(
            index.index_range,
            ManifestByteRange {
                offset: 100,
                length: 44,
            }
        );
        assert_eq!(
            index.initialization_range,
            Some(ManifestByteRange {
                offset: 0,
                length: 100,
            })
        );
        assert!(index.index_range_exact);
        assert_eq!(index.timescale, Some(1_000));
        assert_eq!(plan.resources.len(), 1);
        assert_eq!(plan.resources[0].kind, DashResourceKind::Initialization);
        assert_eq!(plan.resources[0].range, index.initialization_range);
    }

    #[test]
    fn plans_segment_base_with_separate_initialization_url() {
        let body = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <BaseURL>media.mp4</BaseURL>
  <Period duration="PT2S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <Representation id="v1">
        <SegmentBase timescale="1000" indexRange="100-143">
          <Initialization sourceURL="init.mp4"/>
        </SegmentBase>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let manifest = parse_dash_manifest(BASE_URL, body).expect("separate init URL should parse");
        let representation = &manifest.video_representations[0];
        let plan = plan_dash_representation(&manifest, &representation.key)
            .expect("separate init URL should be planned");

        let index = plan
            .segment_base_index
            .as_ref()
            .expect("deferred SIDX request");
        assert!(index.initialization_is_external);
        assert_eq!(
            index.initialization_url.as_str(),
            "https://cdn.example.test/path/init.mp4"
        );
        assert_eq!(index.initialization_range, None);
        assert_eq!(plan.resources.len(), 1);
        assert_eq!(plan.resources[0].kind, DashResourceKind::Initialization);
        assert_eq!(plan.resources[0].url, index.initialization_url);
        assert_eq!(plan.resources[0].range, None);
    }

    #[test]
    fn plans_separate_initialization_url_with_a_bounded_range() {
        let body = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <BaseURL>media.mp4</BaseURL>
  <Period duration="PT2S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <Representation id="v1">
        <SegmentBase timescale="1000" indexRange="100-143">
          <Initialization sourceURL="init.mp4" range="20-119"/>
        </SegmentBase>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let manifest =
            parse_dash_manifest(BASE_URL, body).expect("ranged separate init should parse");
        let representation = &manifest.video_representations[0];
        let plan = plan_dash_representation(&manifest, &representation.key)
            .expect("ranged separate init should be planned");
        let expected = ManifestByteRange {
            offset: 20,
            length: 100,
        };
        assert_eq!(plan.resources[0].range, Some(expected));
        assert_eq!(
            plan.resources[0].url.as_str(),
            "https://cdn.example.test/path/init.mp4"
        );
        assert!(
            plan.segment_base_index
                .expect("indexed base")
                .initialization_is_external
        );
    }

    #[test]
    fn separate_initialization_url_participates_in_representation_identity() {
        let body = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <BaseURL>media.mp4</BaseURL>
  <Period duration="PT2S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <Representation id="v1">
        <SegmentBase timescale="1000" indexRange="100-143">
          <Initialization sourceURL="init-one.mp4"/>
        </SegmentBase>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let second = body.replace("init-one.mp4", "init-two.mp4");
        let first_manifest =
            parse_dash_manifest(BASE_URL, body).expect("first initialization URL parses");
        let second_manifest =
            parse_dash_manifest(BASE_URL, &second).expect("second initialization URL parses");

        assert_ne!(
            first_manifest.video_representations[0].key,
            second_manifest.video_representations[0].key
        );
    }

    #[test]
    fn rotating_initialization_signatures_do_not_change_representation_identity() {
        let body = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <BaseURL>media.mp4</BaseURL>
  <Period duration="PT2S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <Representation id="v1">
        <SegmentBase timescale="1000" indexRange="100-143">
          <Initialization sourceURL="init.mp4?sig=first"/>
        </SegmentBase>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let second = body.replace("sig=first", "sig=second");
        let first_manifest =
            parse_dash_manifest(BASE_URL, body).expect("first signed init URL parses");
        let second_manifest =
            parse_dash_manifest(BASE_URL, &second).expect("rotated signed init URL parses");

        assert_eq!(
            first_manifest.video_representations[0].key,
            second_manifest.video_representations[0].key
        );
    }

    #[test]
    fn rejects_unbounded_in_file_initialization_resource() {
        let body = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <BaseURL>media.mp4</BaseURL>
  <Period duration="PT2S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <Representation id="v1">
        <SegmentBase timescale="1000" indexRange="100-143">
          <Initialization sourceURL="media.mp4"/>
        </SegmentBase>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let manifest = parse_dash_manifest(BASE_URL, body).expect("manifest syntax is valid");
        let representation = &manifest.video_representations[0];
        assert_eq!(
            plan_dash_representation(&manifest, &representation.key).err(),
            Some(DashError::UnsupportedAddressing)
        );
    }

    #[test]
    fn plans_external_representation_index_with_its_own_bounded_range() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <BaseURL>media.mp4</BaseURL>
  <Period duration="PT2S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <Representation id="v1">
        <SegmentBase timescale="1000">
          <Initialization range="0-99"/>
          <RepresentationIndex sourceURL="index.sidx" range="0-43"/>
        </SegmentBase>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let manifest =
            parse_dash_manifest(BASE_URL, body).expect("external RepresentationIndex should parse");
        let representation = &manifest.video_representations[0];
        let plan = plan_dash_representation(&manifest, &representation.key)
            .expect("external RepresentationIndex should plan");

        let index = plan
            .segment_base_index
            .as_ref()
            .expect("external SIDX request");
        assert_eq!(
            index.index_url.as_str(),
            "https://cdn.example.test/path/index.sidx"
        );
        assert_eq!(
            index.url.as_str(),
            "https://cdn.example.test/path/media.mp4"
        );
        assert!(index.index_is_external);
        assert_eq!(
            index.index_range,
            ManifestByteRange {
                offset: 0,
                length: 44,
            }
        );
    }

    #[test]
    fn representation_index_range_overrides_segment_base_index_range() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <BaseURL>media.mp4</BaseURL>
  <Period duration="PT2S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <Representation id="v1">
        <SegmentBase timescale="1000" indexRange="100-143">
          <Initialization range="0-99"/>
          <RepresentationIndex sourceURL="media.mp4" range="200-243"/>
        </SegmentBase>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let manifest =
            parse_dash_manifest(BASE_URL, body).expect("RepresentationIndex range should parse");
        let representation = &manifest.video_representations[0];
        let plan = plan_dash_representation(&manifest, &representation.key)
            .expect("RepresentationIndex range should plan");
        let index = plan.segment_base_index.as_ref().expect("indexed plan");

        assert!(!index.index_is_external);
        assert_eq!(
            index.index_range,
            ManifestByteRange {
                offset: 200,
                length: 44,
            }
        );
    }

    #[test]
    fn representation_index_without_bounded_range_falls_back() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <BaseURL>media.mp4</BaseURL>
  <Period duration="PT2S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <Representation id="v1">
        <SegmentBase timescale="1000">
          <Initialization range="0-99"/>
          <RepresentationIndex sourceURL="index.sidx"/>
        </SegmentBase>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        assert_eq!(
            parse_dash_manifest(BASE_URL, body).err(),
            Some(DashError::UnsupportedAddressing)
        );
    }

    #[test]
    fn plans_nonzero_segment_base_presentation_time_offset() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT4S">
  <BaseURL>media.mp4</BaseURL>
  <Period start="PT2S" duration="PT2S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <Representation id="v1">
        <SegmentBase timescale="1000" presentationTimeOffset="8100" indexRange="100-143">
          <Initialization range="0-99"/>
        </SegmentBase>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let manifest =
            parse_dash_manifest(BASE_URL, body).expect("static SegmentBase PTO should be planned");
        let representation = &manifest.video_representations[0];
        let plan = plan_dash_representation(&manifest, &representation.key)
            .expect("non-zero SegmentBase PTO should produce a plan");
        assert_eq!(
            plan.input_timestamp_offset.ffmpeg_argument().as_deref(),
            Some("-6.100000000")
        );
    }

    #[test]
    fn rejects_base_url_byte_range_alternative_request_templates() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <BaseURL byteRange="$base$-$first$-$last$">media.mp4</BaseURL>
  <Period duration="PT2S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <Representation id="v1">
        <SegmentBase timescale="1000" indexRange="100-143">
          <Initialization range="0-99"/>
        </SegmentBase>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        assert_eq!(
            parse_dash_manifest(BASE_URL, body).err(),
            Some(DashError::UnsupportedAddressing)
        );
    }

    #[test]
    fn indexed_segment_base_without_initialization_range_falls_back() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <BaseURL>media.mp4</BaseURL>
  <Period duration="PT2S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <Representation id="v1">
        <SegmentBase indexRange="100-143"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        assert_eq!(
            parse_dash_manifest(BASE_URL, body).err(),
            Some(DashError::UnsupportedAddressing)
        );
    }

    #[test]
    fn rejects_index_range_exact_without_an_effective_index_range() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <BaseURL>media.mp4</BaseURL>
  <Period duration="PT2S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentBase indexRangeExact="true"/>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        assert_eq!(
            parse_dash_manifest(BASE_URL, body).err(),
            Some(DashError::UnsupportedAddressing)
        );
    }

    #[test]
    fn rejects_zero_segment_base_timescale_without_an_index_range() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <BaseURL>media.mp4</BaseURL>
  <Period duration="PT2S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentBase timescale="0"/>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        assert_eq!(
            parse_dash_manifest(BASE_URL, body).err(),
            Some(DashError::UnsupportedAddressing)
        );
    }

    #[test]
    fn applies_sidx_first_offset_after_the_index_box() {
        let bytes = build_sidx_box(8, &[(64, 1_000, 0x9000_0000)]);
        let index = indexed_base_plan(
            ManifestByteRange {
                offset: 0,
                length: 100,
            },
            test_range(&bytes),
            true,
        );
        let resources = resolve_segment_base_index(&index, &bytes, 1_000)
            .expect("first_offset should be applied to the end of SIDX");
        assert_eq!(
            resources[0].range,
            Some(ManifestByteRange {
                offset: 100 + u64::try_from(bytes.len()).expect("test SIDX length") + 8,
                length: 64,
            })
        );
    }

    #[test]
    fn resolves_flat_sidx_references_to_ordered_media_ranges() {
        let bytes = build_sidx_box(0, &[(100, 2_000, 0x9000_0000), (200, 2_000, 0x9000_0000)]);
        let index = indexed_base_plan(
            ManifestByteRange {
                offset: 0,
                length: 100,
            },
            test_range(&bytes),
            true,
        );

        let resources =
            resolve_segment_base_index(&index, &bytes, 1_000).expect("flat SIDX should resolve");
        assert_eq!(resources.len(), 2);
        let first_media_offset = 100_u64
            .checked_add(u64::try_from(bytes.len()).expect("test SIDX length"))
            .expect("test media offset");
        assert_eq!(
            resources[0].range,
            Some(ManifestByteRange {
                offset: first_media_offset,
                length: 100,
            })
        );
        assert_eq!(
            resources[1].range,
            Some(ManifestByteRange {
                offset: first_media_offset + 100,
                length: 200,
            })
        );
        assert_eq!(resources[0].segment_number, Some(0));
        assert_eq!(resources[1].segment_number, Some(1));
        assert!(
            resources
                .iter()
                .all(|resource| resource.kind == DashResourceKind::Segment)
        );
    }

    #[test]
    fn resolves_sidx_inside_non_exact_index_range() {
        let mut bytes = vec![0, 0, 0, 8, b'f', b'r', b'e', b'e'];
        bytes.extend_from_slice(&build_sidx_box(0, &[(64, 1_000, 0x9000_0000)]));
        let index = indexed_base_plan(
            ManifestByteRange {
                offset: 0,
                length: 100,
            },
            test_range(&bytes),
            false,
        );

        let resources = resolve_segment_base_index(&index, &bytes, 1_000)
            .expect("non-exact index range may contain adjacent boxes");
        assert_eq!(
            resources[0].range,
            Some(ManifestByteRange {
                offset: 100 + u64::try_from(bytes.len()).expect("test SIDX length"),
                length: 64,
            })
        );
    }

    #[test]
    fn rejects_hierarchical_sidx_and_out_of_file_references() {
        let hierarchical = build_sidx_box(0, &[(0x8000_0040, 1_000, 0x9000_0000)]);
        let index = indexed_base_plan(
            ManifestByteRange {
                offset: 0,
                length: 100,
            },
            test_range(&hierarchical),
            true,
        );
        assert_eq!(
            resolve_segment_base_index(&index, &hierarchical, 1_000).err(),
            Some(DashError::UnsupportedAddressing)
        );

        let oversized = build_sidx_box(0, &[(0x7fff_ffff, 1_000, 0x9000_0000)]);
        let oversized_index = indexed_base_plan(
            ManifestByteRange {
                offset: 0,
                length: 100,
            },
            test_range(&oversized),
            true,
        );
        assert_eq!(
            resolve_segment_base_index(&oversized_index, &oversized, 500).err(),
            Some(DashError::InvalidSegment)
        );
        assert_eq!(
            resolve_segment_base_index(&oversized_index, &oversized, u64::MAX).err(),
            Some(DashError::LimitExceeded)
        );
    }

    #[test]
    fn rejects_mismatched_sidx_timescale_and_truncated_index_bytes() {
        let bytes = build_sidx_box(0, &[(64, 1_000, 0x9000_0000)]);
        let mut index = indexed_base_plan(
            ManifestByteRange {
                offset: 0,
                length: 100,
            },
            test_range(&bytes),
            true,
        );
        index.timescale = Some(90_000);
        assert_eq!(
            resolve_segment_base_index(&index, &bytes, 1_000).err(),
            Some(DashError::UnsupportedAddressing)
        );

        index.timescale = Some(1_000);
        index.index_range_exact = true;
        assert_eq!(
            resolve_segment_base_index(&index, &bytes[..bytes.len() - 1], 1_000).err(),
            Some(DashError::InvalidSegment)
        );
    }

    #[test]
    fn accepts_prefix_boxes_before_sidx_when_index_range_is_exact() {
        let mut bytes = vec![0, 0, 0, 8, b'f', b'r', b'e', b'e'];
        bytes.extend_from_slice(&build_sidx_box(0, &[(64, 1_000, 0x9000_0000)]));
        let index = indexed_base_plan(
            ManifestByteRange {
                offset: 0,
                length: 100,
            },
            test_range(&bytes),
            true,
        );
        let resources = resolve_segment_base_index(&index, &bytes, 1_000)
            .expect("indexRangeExact describes a prefix, not one box extent");
        assert_eq!(resources.len(), 1);
        assert_eq!(
            resources[0].range,
            Some(ManifestByteRange {
                offset: 100 + u64::try_from(bytes.len()).expect("test SIDX length"),
                length: 64,
            })
        );
    }

    #[test]
    fn rejects_exact_index_prefix_when_first_media_range_starts_inside_it() {
        let mut bytes = build_sidx_box(0, &[(64, 1_000, 0x9000_0000)]);
        bytes.extend_from_slice(&[0, 0, 0, 8, b'f', b'r', b'e', b'e']);
        let index = indexed_base_plan(
            ManifestByteRange {
                offset: 0,
                length: 100,
            },
            test_range(&bytes),
            true,
        );
        assert_eq!(
            resolve_segment_base_index(&index, &bytes, 1_000).err(),
            Some(DashError::UnsupportedAddressing)
        );
    }

    #[test]
    fn rejects_index_range_offset_overflow() {
        let bytes = build_sidx_box(0, &[(64, 1_000, 0x9000_0000)]);
        let mut index = indexed_base_plan(
            ManifestByteRange {
                offset: 0,
                length: 100,
            },
            ManifestByteRange {
                offset: u64::MAX - 10,
                length: u64::try_from(bytes.len()).expect("test SIDX length"),
            },
            true,
        );
        index.index_range.length = 20;
        assert_eq!(
            resolve_segment_base_index(&index, &bytes, u64::MAX).err(),
            Some(DashError::InvalidSegment)
        );
    }

    #[test]
    fn supported_static_and_dynamic_fixtures_have_no_ignored_element_warnings() {
        let (static_result, static_logs) = capture_dash_warnings(|| {
            parse_dash_manifest(
                BASE_URL,
                include_str!("../tests/fixtures/dash/static-segment-template.mpd"),
            )
        });
        assert!(static_result.is_ok(), "static fixture should parse");
        assert!(
            !static_logs.contains("Unused XML element"),
            "supported static fixture unexpectedly logged ignored elements: {static_logs}"
        );

        let (dynamic_result, dynamic_logs) = capture_dash_warnings(|| {
            parse_dash_manifest_at(
                BASE_URL,
                include_str!("../tests/fixtures/dash/dynamic-segment-template.mpd"),
                SystemTime::UNIX_EPOCH + Duration::from_secs(120),
            )
        });
        assert!(dynamic_result.is_ok(), "dynamic fixture should parse");
        assert!(
            !dynamic_logs.contains("Unused XML element"),
            "supported dynamic fixture unexpectedly logged ignored elements: {dynamic_logs}"
        );
    }

    #[test]
    fn warns_when_the_typed_dash_model_ignores_output_protection() {
        let (result, logs) = capture_dash_warnings(|| {
            parse_dash_manifest(
                BASE_URL,
                include_str!("../tests/fixtures/dash/unmodeled-output-protection.mpd"),
            )
        });

        assert!(
            result.is_ok(),
            "the typed parser should remain tolerant of unknown elements"
        );
        assert!(
            logs.contains("Unused XML element") && logs.contains("OutputProtection"),
            "expected an ignored OutputProtection diagnostic, got: {logs}"
        );

        // OutputProtection is an HDCP/output-device constraint, not DRM. Since
        // Grab downloads the encoded media rather than rendering it, the normal
        // native planner should still expand the same complete media resources.
        let manifest = result.expect("unmodeled output protection must not block downloads");
        let video = manifest
            .video_representations
            .first()
            .expect("video representation");
        let plan = plan_dash_representation(&manifest, &video.key)
            .expect("output protection must not block native resource planning");
        let segment_urls = plan
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .map(|resource| resource.url.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            segment_urls,
            [
                "https://cdn.example.test/path/seg-1.m4s",
                "https://cdn.example.test/path/seg-2.m4s"
            ]
        );
    }

    #[test]
    fn warns_when_the_typed_dash_model_ignores_vendor_extensions() {
        let (result, logs) = capture_dash_warnings(|| {
            parse_dash_manifest(
                BASE_URL,
                include_str!("../tests/fixtures/dash/vendor-extension.mpd"),
            )
        });

        assert!(
            result.is_ok(),
            "the typed parser should remain tolerant of vendor extensions"
        );
        assert!(
            logs.contains("Unused XML element") && logs.contains("SegmentPresentation"),
            "expected an ignored vendor extension diagnostic, got: {logs}"
        );
    }

    fn mpd_with_trick_mode_property(
        on_adaptation: bool,
        element: &str,
        scheme_id_uri: &str,
    ) -> String {
        let property = format!(r#"<{element} schemeIdUri="{scheme_id_uri}" value="0"/>"#);
        let adaptation_property = if on_adaptation { &property } else { "" };
        let representation_property = if on_adaptation { "" } else { &property };

        format!(
            r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10S">
  <Period duration="PT10S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      {adaptation_property}
      <SegmentTemplate timescale="1" duration="5" media="seg-$Number$.m4s"/>
      <Representation id="trick" bandwidth="1000000">{representation_property}</Representation>
      <Representation id="normal" bandwidth="2000000"/>
    </AdaptationSet>
  </Period>
</MPD>"#
        )
    }

    const TEMPLATE_MPD: &str = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10S">
  <BaseURL>media/</BaseURL>
  <Period duration="PT10S">
    <AdaptationSet id="1" contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1" duration="4" startNumber="5"
                       initialization="init-$RepresentationID$.mp4"
                       media="$RepresentationID$/$Number%03d$-$Time$.m4s"/>
      <Representation id="v1" bandwidth="1000000" width="1280" height="720" codecs="avc1.64001f"/>
      <Representation id="v2" bandwidth="2500000" width="1920" height="1080" codecs="avc1.640028"/>
    </AdaptationSet>
    <AdaptationSet id="2" contentType="audio" mimeType="audio/mp4" lang="en">
      <Role schemeIdUri="urn:mpeg:dash:role:2011" value="main"/>
      <SegmentTemplate timescale="1" duration="5" initialization="audio-init.mp4" media="audio/$Number$.m4s"/>
      <Representation id="a1" bandwidth="128000" codecs="mp4a.40.2"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

    #[test]
    fn parses_supported_mp4_subtitle_representations_only() {
        let body = TEMPLATE_MPD.replace(
            "</Period>",
            r#"
    <AdaptationSet contentType="text" mimeType="application/mp4" lang="en">
      <Role schemeIdUri="urn:mpeg:dash:role:2011" value="main"/>
      <SegmentTemplate timescale="1" duration="5" initialization="sub-init.mp4" media="sub-$Number$.m4s"/>
      <Representation id="sub-en" bandwidth="1000" codecs="wvtt"/>
    </AdaptationSet>
    <AdaptationSet contentType="text" mimeType="text/vtt" lang="fr">
      <SegmentTemplate duration="5" media="unsupported-$Number$.vtt"/>
      <Representation id="sub-fr" bandwidth="1000"/>
    </AdaptationSet>
</Period>"#,
        );
        let plan = parse_dash_manifest(BASE_URL, &body)
            .expect("supported DASH subtitle representation should parse");

        assert_eq!(plan.subtitle_representations.len(), 1);
        let subtitle = &plan.subtitle_representations[0];
        assert_eq!(subtitle.representation_id.as_deref(), Some("sub-en"));
        assert_eq!(subtitle.language.as_deref(), Some("en"));
        assert!(subtitle.is_default_subtitle);
        assert_eq!(subtitle.kind, DashTrackKind::Subtitle);
        let download = plan_dash_representation(&plan, &subtitle.key)
            .expect("subtitle representation should be plannable");
        assert_eq!(download.representation.key, subtitle.key);
        assert_eq!(download.resources.len(), 3);
        assert!(download.has_zero_origin_fragmented_subtitle_timeline());
    }

    #[test]
    fn classifies_stpp_as_a_supported_zero_origin_mp4_subtitle() {
        let body = TEMPLATE_MPD.replace(
            "</Period>",
            r#"
    <AdaptationSet contentType="text" mimeType="application/mp4" lang="en">
      <Role schemeIdUri="urn:mpeg:dash:role:2011" value="main"/>
      <SegmentTemplate timescale="1" duration="5" initialization="ttml-init.mp4" media="ttml-$Number$.m4s"/>
      <Representation id="sub-ttml" bandwidth="1000" codecs="stpp.ttml.im1t"/>
    </AdaptationSet>
</Period>"#,
        );
        let manifest =
            parse_dash_manifest(BASE_URL, &body).expect("STPP representation should be recognized");
        assert_eq!(manifest.subtitle_representations.len(), 1);
        let representation = &manifest.subtitle_representations[0];
        assert_eq!(
            representation.representation_id.as_deref(),
            Some("sub-ttml")
        );
        assert_eq!(representation.codecs.as_deref(), Some("stpp.ttml.im1t"));

        let plan = plan_dash_representation(&manifest, &representation.key)
            .expect("STPP representation should be plannable");
        assert!(plan.has_zero_origin_fragmented_subtitle_timeline());
    }

    #[test]
    fn wvtt_sample_timing_rejects_nonzero_period_start_and_presentation_offset() {
        let body = TEMPLATE_MPD.replace(
            "</Period>",
            r#"
    <AdaptationSet contentType="text" mimeType="application/mp4" lang="en">
      <SegmentTemplate timescale="1" duration="5" initialization="sub-init.mp4" media="sub-$Number$.m4s"/>
      <Representation id="sub-en" bandwidth="1000" codecs="wvtt"/>
    </AdaptationSet>
</Period>"#,
        );
        let offset_body = body.replace(
            r#"timescale="1" duration="5" initialization="sub-init.mp4""#,
            r#"timescale="1" duration="5" presentationTimeOffset="3" initialization="sub-init.mp4""#,
        );
        let offset_manifest = parse_dash_manifest(BASE_URL, &offset_body)
            .expect("a supported nonzero-PTO DASH manifest should parse");
        let offset_choice = &offset_manifest.subtitle_representations[0];
        let offset_plan = plan_dash_representation(&offset_manifest, &offset_choice.key)
            .expect("nonzero-PTO subtitle should remain plannable");
        assert!(!offset_plan.has_zero_origin_fragmented_subtitle_timeline());

        let period_body = body.replace(
            r#"<Period duration="PT10S">"#,
            r#"<Period start="PT2S" duration="PT8S">"#,
        );
        let period_manifest = parse_dash_manifest(BASE_URL, &period_body)
            .expect("a supported single Period with an explicit start should parse");
        let period_choice = &period_manifest.subtitle_representations[0];
        let period_plan = plan_dash_representation(&period_manifest, &period_choice.key)
            .expect("nonzero-start subtitle should remain plannable");
        assert!(!period_plan.has_zero_origin_fragmented_subtitle_timeline());
    }

    #[test]
    fn representation_level_main_role_selects_the_declared_audio_track() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10S">
  <Period duration="PT10S">
    <AdaptationSet contentType="audio" mimeType="audio/mp4">
      <SegmentTemplate duration="5" media="first-$Number$.m4s"/>
      <Representation id="first" bandwidth="128000"/>
    </AdaptationSet>
    <AdaptationSet contentType="audio" mimeType="audio/mp4">
      <SegmentTemplate duration="5" media="main-$Number$.m4s"/>
      <Representation id="declared-main" bandwidth="192000">
        <Role schemeIdUri="urn:mpeg:dash:role:2011" value="main"/>
      </Representation>
    </AdaptationSet>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate duration="5" media="video-$Number$.m4s"/>
      <Representation id="video" bandwidth="1000000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let plan = parse_dash_manifest(BASE_URL, body)
            .expect("a representation-level main role should be preserved");

        assert_eq!(plan.audio_representations.len(), 2);
        assert!(!plan.audio_representations[0].is_default_audio);
        assert!(plan.audio_representations[1].is_default_audio);
        assert_eq!(
            plan.default_audio()
                .and_then(|track| track.representation_id.as_deref()),
            Some("declared-main"),
            "Representation-level Role=main must override the first-track fallback"
        );
    }

    #[test]
    fn parses_video_and_audio_representations_and_selects_main_audio() {
        let plan = parse_dash_manifest(BASE_URL, TEMPLATE_MPD)
            .expect("static single-period MPD should parse");

        assert_eq!(plan.duration, Duration::from_secs(10));
        assert_eq!(plan.video_representations.len(), 2);
        assert_eq!(
            plan.video_representations[0].representation_id.as_deref(),
            Some("v1")
        );
        assert_eq!(plan.video_representations[0].height, Some(720));
        assert_eq!(plan.video_representations[1].height, Some(1080));
        assert_eq!(plan.audio_representations.len(), 1);
        assert_eq!(
            plan.default_audio()
                .and_then(|audio| audio.representation_id.as_deref()),
            Some("a1")
        );
    }

    #[test]
    fn representation_key_survives_manifest_order_changes() {
        let original =
            parse_dash_manifest(BASE_URL, TEMPLATE_MPD).expect("original MPD should parse");
        let original_v1 = original
            .video_representations
            .iter()
            .find(|representation| representation.representation_id.as_deref() == Some("v1"))
            .expect("v1 representation");
        let selected_key = original_v1.key.clone();

        let reordered_body = TEMPLATE_MPD.replace(
            concat!(
                "      <Representation id=\"v1\" bandwidth=\"1000000\" width=\"1280\" height=\"720\" codecs=\"avc1.64001f\"/>\n",
                "      <Representation id=\"v2\" bandwidth=\"2500000\" width=\"1920\" height=\"1080\" codecs=\"avc1.640028\"/>"
            ),
            concat!(
                "      <Representation id=\"v2\" bandwidth=\"2500000\" width=\"1920\" height=\"1080\" codecs=\"avc1.640028\"/>\n",
                "      <Representation id=\"v1\" bandwidth=\"1000000\" width=\"1280\" height=\"720\" codecs=\"avc1.64001f\"/>"
            ),
        );
        assert_ne!(
            reordered_body, TEMPLATE_MPD,
            "fixture must reorder the variants"
        );

        let refreshed =
            parse_dash_manifest(BASE_URL, &reordered_body).expect("reordered MPD should parse");
        let refreshed_v1 = refreshed
            .video_representations
            .iter()
            .find(|representation| representation.representation_id.as_deref() == Some("v1"))
            .expect("v1 representation after refresh");
        assert_eq!(refreshed_v1.key, selected_key);
        assert!(
            plan_dash_representation(&refreshed, &selected_key).is_ok(),
            "the queued key must still resolve to v1 after a manifest reorder"
        );
    }

    #[test]
    fn excludes_adaptation_sets_marked_for_trick_mode() {
        for (element, scheme_id_uri) in [
            (
                "EssentialProperty",
                "https://dashif.org/guidelines/trickmode",
            ),
            (
                "EssentialProperty",
                "http://dashif.org/guidelines/trickmode",
            ),
            (
                "SupplementalProperty",
                "http://dashif.org/guidelines/trickmode",
            ),
        ] {
            let body = mpd_with_trick_mode_property(true, element, scheme_id_uri);
            assert_eq!(
                parse_dash_manifest(BASE_URL, &body).err(),
                Some(DashError::NoVideoRepresentations),
                "{element} with {scheme_id_uri} must exclude the entire AdaptationSet"
            );
        }
    }

    #[test]
    fn excludes_representations_marked_for_trick_mode() {
        for (element, scheme_id_uri) in [
            ("EssentialProperty", "urn:mpeg:dash:trickmode:2016"),
            (
                "SupplementalProperty",
                "https://dashif.org/guidelines/trickmode",
            ),
        ] {
            let body = mpd_with_trick_mode_property(false, element, scheme_id_uri);
            let plan = parse_dash_manifest(BASE_URL, &body)
                .expect("regular video representation should remain selectable");

            assert_eq!(plan.video_representations.len(), 1);
            assert_eq!(
                plan.video_representations[0].representation_id.as_deref(),
                Some("normal"),
                "{element} with {scheme_id_uri} must exclude only its Representation"
            );
        }
    }

    fn dynamic_mpd(addressing: &str, extra_attributes: &str) -> String {
        format!(
            r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     type="dynamic"
     availabilityStartTime="1970-01-01T00:00:00Z"
     minimumUpdatePeriod="PT4S"
     {extra_attributes}>
  <Period start="PT0S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      {addressing}
      <Representation id="v1" bandwidth="1000000" width="1280" height="720"/>
    </AdaptationSet>
  </Period>
</MPD>"#
        )
    }

    fn dynamic_test_clock(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn dynamic_duration_template_selects_only_complete_segments_inside_time_shift_window() {
        let body = dynamic_mpd(
            r#"<SegmentTemplate timescale="1" startNumber="1" duration="4" initialization="init.mp4" media="seg-$Number$.m4s"/>"#,
            r#"timeShiftBufferDepth="PT20S""#,
        );
        let manifest = parse_dash_manifest_at(BASE_URL, &body, dynamic_test_clock(120))
            .expect("bounded dynamic MPD should parse");
        let video = manifest.video_representations.first().expect("video track");
        assert!(matches!(
            manifest.presentation,
            DashPresentation::Dynamic(_)
        ));

        let plan = plan_dash_representation(&manifest, &video.key)
            .expect("expand current complete live segments");
        let numbers = plan
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .map(|resource| resource.segment_number.expect("media segment number"))
            .collect::<Vec<_>>();
        assert_eq!(numbers, [26, 27, 28, 29, 30]);
        assert_eq!(plan.resources[0].kind, DashResourceKind::Initialization);
        assert!(
            plan.resources[1..]
                .iter()
                .all(|resource| resource.filename.starts_with("dash-live-segment-"))
        );
    }

    #[test]
    fn dynamic_without_time_shift_depth_starts_at_the_newest_complete_segment() {
        let body = dynamic_mpd(
            r#"<SegmentTemplate timescale="1" startNumber="10" duration="4" media="seg-$Number$.m4s"/>"#,
            "",
        );
        let manifest = parse_dash_manifest_at(BASE_URL, &body, dynamic_test_clock(120))
            .expect("dynamic MPD without TSBD should parse");
        let representation = &manifest.video_representations[0];
        let plan = plan_dash_representation(&manifest, &representation.live_key)
            .expect("plan newest complete live segment");
        let numbers = plan
            .resources
            .iter()
            .filter_map(|resource| resource.segment_number)
            .collect::<Vec<_>>();
        assert_eq!(numbers, [38, 39]);
    }

    #[test]
    fn dynamic_timeline_negative_repeat_uses_only_complete_segments_in_window() {
        let body = dynamic_mpd(
            r#"<SegmentTemplate timescale="1" startNumber="1" media="seg-$Number$-$Time$.m4s">
  <SegmentTimeline><S t="0" d="2" r="-1"/></SegmentTimeline>
</SegmentTemplate>"#,
            r#"timeShiftBufferDepth="PT4S""#,
        );
        let manifest = parse_dash_manifest_at(BASE_URL, &body, dynamic_test_clock(11))
            .expect("dynamic timeline MPD should parse");
        let representation = &manifest.video_representations[0];
        let plan = plan_dash_representation(&manifest, &representation.key)
            .expect("expand complete timeline window");
        let segments = plan
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .collect::<Vec<_>>();
        assert_eq!(
            segments
                .iter()
                .map(|resource| resource.segment_number.expect("segment number"))
                .collect::<Vec<_>>(),
            [4, 5]
        );
        assert_eq!(
            segments
                .iter()
                .map(|resource| resource.url.path().to_string())
                .collect::<Vec<_>>(),
            ["/path/seg-4-6.m4s", "/path/seg-5-8.m4s"]
        );
    }

    #[test]
    fn dynamic_timeline_negative_repeat_uses_ceiling_before_next_explicit_start() {
        let body = dynamic_mpd(
            r#"<SegmentTemplate timescale="1" startNumber="10" media="seg-$Number$-$Time$.m4s">
  <SegmentTimeline>
    <S t="0" d="2" r="-1"/>
    <S t="5" d="2"/>
  </SegmentTimeline>
</SegmentTemplate>"#,
            r#"timeShiftBufferDepth="PT20S""#,
        );
        let manifest = parse_dash_manifest_at(BASE_URL, &body, dynamic_test_clock(9))
            .expect("dynamic timeline MPD should parse");
        let representation = &manifest.video_representations[0];
        let plan = plan_dash_representation(&manifest, &representation.live_key)
            .expect("expand complete dynamic timeline segments");
        let segments = plan
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .collect::<Vec<_>>();

        assert_eq!(
            segments
                .iter()
                .map(|resource| resource.segment_number.expect("segment number"))
                .collect::<Vec<_>>(),
            [10, 11, 12, 13],
            "the negative repeat includes the final segment that overlaps the next S@t"
        );
        assert_eq!(
            segments
                .iter()
                .map(|resource| resource.url.path().to_string())
                .collect::<Vec<_>>(),
            [
                "/path/seg-10-0.m4s",
                "/path/seg-11-2.m4s",
                "/path/seg-12-4.m4s",
                "/path/seg-13-5.m4s",
            ]
        );
    }

    #[test]
    fn dynamic_timeline_stops_at_inclusive_end_number() {
        let body = dynamic_mpd(
            r#"<SegmentTemplate timescale="1" startNumber="1" endNumber="2" media="seg-$Number$-$Time$.m4s">
  <SegmentTimeline>
    <S t="0" d="2"/>
    <S t="2" d="2"/>
    <S t="4" d="2"/>
  </SegmentTimeline>
</SegmentTemplate>"#,
            r#"timeShiftBufferDepth="PT20S""#,
        );
        let manifest = parse_dash_manifest_at(BASE_URL, &body, dynamic_test_clock(11))
            .expect("dynamic MPD with endNumber should parse");
        let representation = &manifest.video_representations[0];
        let plan = plan_dash_representation(&manifest, &representation.live_key)
            .expect("expand segments through endNumber");
        let numbers = plan
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .map(|resource| resource.segment_number.expect("media segment number"))
            .collect::<Vec<_>>();
        assert_eq!(numbers, [1, 2]);
    }

    #[test]
    fn dynamic_timeline_rejects_overlap_after_positive_repeat() {
        let body = dynamic_mpd(
            r#"<SegmentTemplate timescale="1" startNumber="1" media="seg-$Time$.m4s">
  <SegmentTimeline>
    <S t="0" d="4" r="2"/>
    <S t="4" d="4"/>
  </SegmentTimeline>
</SegmentTemplate>"#,
            r#"timeShiftBufferDepth="PT20S""#,
        );
        let manifest = parse_dash_manifest_at(BASE_URL, &body, dynamic_test_clock(20))
            .expect("dynamic timeline MPD syntax should parse");
        let representation = &manifest.video_representations[0];

        assert_eq!(
            plan_dash_representation(&manifest, &representation.live_key).err(),
            Some(DashError::InvalidSegment),
            "an explicit start inside a positive-repeat run must be rejected"
        );
    }

    #[test]
    fn accepts_dynamic_single_period_with_nonzero_period_start() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic"
     availabilityStartTime="1970-01-01T00:00:00Z" minimumUpdatePeriod="PT4S">
  <Period start="PT3600S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate duration="4" media="video-$Number$.m4s"/>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let manifest = parse_dash_manifest_at(BASE_URL, body, dynamic_test_clock(3604))
            .expect("single-period dynamic MPD may have a nonzero period start");
        let DashPresentation::Dynamic(dynamic) = &manifest.presentation else {
            panic!("fixture is dynamic");
        };
        assert_eq!(dynamic.period_start, Duration::from_secs(3600));

        let representation = &manifest.video_representations[0];
        let plan = plan_dash_representation(&manifest, &representation.key)
            .expect("existing dynamic Period-start planning remains supported");
        assert_eq!(
            plan.input_timestamp_offset.ffmpeg_argument(),
            None,
            "the static PTO change must not change dynamic finalization semantics"
        );
    }

    #[test]
    fn plans_period_continuous_static_dash_as_one_resource_sequence() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT4S">
  <Period id="p1" start="PT0S" duration="PT2S">
    <AssetIdentifier schemeIdUri="urn:example:asset" value="asset-1"/>
    <AdaptationSet id="video" contentType="video" mimeType="video/mp4" codecs="avc1.640028">
      <SegmentTemplate timescale="1" startNumber="1" duration="1"
                       initialization="init.mp4" media="video-$Number$.m4s"/>
      <Representation id="v1" bandwidth="1000000" width="1280" height="720"/>
    </AdaptationSet>
  </Period>
  <Period id="p2" start="PT2S" duration="PT2S">
    <AssetIdentifier schemeIdUri="urn:example:asset" value="asset-1"/>
    <AdaptationSet id="video" contentType="video" mimeType="video/mp4" codecs="avc1.640028">
      <SupplementalProperty schemeIdUri="urn:mpeg:dash:period-continuity:2015" value="p1"/>
      <SegmentTemplate timescale="1" startNumber="3" duration="1" presentationTimeOffset="2"
                       initialization="init.mp4" media="video-$Number$.m4s"/>
      <Representation id="v1" bandwidth="1000000" width="1280" height="720"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

        let manifest = parse_dash_manifest(BASE_URL, body)
            .expect("explicitly continuous static periods should parse");
        assert_eq!(manifest.duration, Duration::from_secs(4));
        assert_eq!(manifest.video_representations.len(), 1);

        let plan = plan_dash_representation(&manifest, &manifest.video_representations[0].key)
            .expect("period resources should be combined in presentation order");
        assert_eq!(plan.duration, Duration::from_secs(4));
        assert_eq!(
            plan.resources
                .iter()
                .filter(|resource| resource.kind == DashResourceKind::Initialization)
                .count(),
            1,
            "continuous periods share the first initialization section"
        );
        let segment_numbers = plan
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .map(|resource| resource.segment_number)
            .collect::<Vec<_>>();
        assert_eq!(segment_numbers, [Some(1), Some(2), Some(3), Some(4)]);

        let repeated_url_body = body.replace("video-$Number$.m4s", "video.m4s");
        let repeated_url_manifest = parse_dash_manifest(BASE_URL, &repeated_url_body)
            .expect("repeated media URLs are valid with continuous period timing");
        let repeated_url_plan = plan_dash_representation(
            &repeated_url_manifest,
            &repeated_url_manifest.video_representations[0].key,
        )
        .expect("period staging names should remain unique");
        let mut filenames = std::collections::HashSet::new();
        assert!(
            repeated_url_plan
                .resources
                .iter()
                .all(|resource| filenames.insert(resource.filename.as_str()))
        );

        let different_asset = body.replacen(r#"value="asset-1""#, r#"value="asset-2""#, 1);
        assert_eq!(
            parse_dash_manifest(BASE_URL, &different_asset).err(),
            Some(DashError::UnsupportedPresentation)
        );

        let discontinuous_timeline = body.replace(
            r#"presentationTimeOffset="2""#,
            r#"presentationTimeOffset="1""#,
        );
        assert_eq!(
            parse_dash_manifest(BASE_URL, &discontinuous_timeline).err(),
            Some(DashError::UnsupportedPresentation)
        );

        let changed_initialization = body
            .rsplit_once(r#"initialization="init.mp4" "#)
            .map(|(prefix, suffix)| format!("{prefix}{suffix}"))
            .expect("second period initialization exists");
        assert_eq!(
            parse_dash_manifest(BASE_URL, &changed_initialization).err(),
            Some(DashError::UnsupportedPresentation)
        );

        let changed_initialization_url = body
            .rsplit_once(r#"initialization="init.mp4""#)
            .map(|(prefix, suffix)| format!(r#"{prefix}initialization="init2.mp4"{suffix}"#))
            .expect("second period initialization exists");
        let changed_init_manifest = parse_dash_manifest(BASE_URL, &changed_initialization_url)
            .expect("period init addressing parses before continuity is planned");
        assert_eq!(
            plan_dash_representation(
                &changed_init_manifest,
                &changed_init_manifest.video_representations[0].key,
            )
            .err(),
            Some(DashError::UnsupportedPresentation)
        );

        let crossing_segment = body.replace(r#"duration="1""#, r#"duration="3""#);
        let crossing_manifest = parse_dash_manifest(BASE_URL, &crossing_segment)
            .expect("period metadata is valid before segment-boundary planning");
        assert_eq!(
            plan_dash_representation(
                &crossing_manifest,
                &crossing_manifest.video_representations[0].key,
            )
            .err(),
            Some(DashError::UnsupportedPresentation)
        );
    }

    #[test]
    fn plans_three_continuous_periods_using_each_adjacent_timeline() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT6S">
  <Period id="p1" start="PT0S" duration="PT2S">
    <AssetIdentifier schemeIdUri="urn:example:asset" value="asset-1"/>
    <AdaptationSet id="video" contentType="video" mimeType="video/mp4" codecs="avc1.640028">
      <SegmentTemplate timescale="1" startNumber="1" duration="1"
                       initialization="init.mp4" media="video-$Number$.m4s"/>
      <Representation id="v1" width="1280" height="720"/>
    </AdaptationSet>
  </Period>
  <Period id="p2" start="PT2S" duration="PT2S">
    <AssetIdentifier schemeIdUri="urn:example:asset" value="asset-1"/>
    <AdaptationSet id="video" contentType="video" mimeType="video/mp4" codecs="avc1.640028">
      <SupplementalProperty schemeIdUri="urn:mpeg:dash:period-continuity:2015" value="p1"/>
      <SegmentTemplate timescale="1" startNumber="3" duration="1" presentationTimeOffset="2"
                       initialization="init.mp4" media="video-$Number$.m4s"/>
      <Representation id="v1" width="1280" height="720"/>
    </AdaptationSet>
  </Period>
  <Period id="p3" start="PT4S" duration="PT2S">
    <AssetIdentifier schemeIdUri="urn:example:asset" value="asset-1"/>
    <AdaptationSet id="video" contentType="video" mimeType="video/mp4" codecs="avc1.640028">
      <SupplementalProperty schemeIdUri="urn:mpeg:dash:period-continuity:2015" value="p2"/>
      <SegmentTemplate timescale="1" startNumber="5" duration="1" presentationTimeOffset="4"
                       initialization="init.mp4" media="video-$Number$.m4s"/>
      <Representation id="v1" width="1280" height="720"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

        let manifest = parse_dash_manifest(BASE_URL, body)
            .expect("three adjacent period-continuous periods should parse");
        let plan = plan_dash_representation(&manifest, &manifest.video_representations[0].key)
            .expect("all three period resource sequences should be appended");
        let segment_numbers = plan
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .map(|resource| resource.segment_number)
            .collect::<Vec<_>>();
        assert_eq!(
            segment_numbers,
            [Some(1), Some(2), Some(3), Some(4), Some(5), Some(6)]
        );
        assert_eq!(
            plan.resources
                .iter()
                .filter(|resource| resource.kind == DashResourceKind::Initialization)
                .count(),
            1
        );
    }

    #[test]
    fn rejects_multi_period_dash_without_continuity_or_with_timing_gaps() {
        let missing_continuity = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT4S">
  <Period id="p1" start="PT0S" duration="PT2S">
    <AssetIdentifier schemeIdUri="urn:example:asset" value="asset-1"/>
    <AdaptationSet id="video" contentType="video" mimeType="video/mp4">
      <SegmentTemplate duration="1" media="video-$Number$.m4s"/>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
  <Period id="p2" start="PT2S" duration="PT2S">
    <AssetIdentifier schemeIdUri="urn:example:asset" value="asset-1"/>
    <AdaptationSet id="video" contentType="video" mimeType="video/mp4">
      <SegmentTemplate duration="1" startNumber="3" media="video-$Number$.m4s"/>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        assert_eq!(
            parse_dash_manifest(BASE_URL, missing_continuity).err(),
            Some(DashError::UnsupportedPresentation)
        );

        let gap = missing_continuity
            .replace(
                r#"<Period id="p2" start="PT2S" duration="PT2S">"#,
                r#"<Period id="p2" start="PT3S" duration="PT1S">"#,
            )
            .replace(
                r#"<AssetIdentifier schemeIdUri="urn:example:asset" value="asset-1"/>
    <AdaptationSet id="video" contentType="video" mimeType="video/mp4">
      <SegmentTemplate duration="1" startNumber="3""#,
                r#"<AssetIdentifier schemeIdUri="urn:example:asset" value="asset-1"/>
    <AdaptationSet id="video" contentType="video" mimeType="video/mp4">
      <SupplementalProperty schemeIdUri="urn:mpeg:dash:period-continuity:2015" value="p1"/>
      <SegmentTemplate duration="1" startNumber="3""#,
            );
        assert_eq!(
            parse_dash_manifest(BASE_URL, &gap).err(),
            Some(DashError::UnsupportedPresentation)
        );

        let nonzero_start = missing_continuity.replace(r#"start="PT0S""#, r#"start="PT1S""#);
        assert_eq!(
            parse_dash_manifest(BASE_URL, &nonzero_start).err(),
            Some(DashError::UnsupportedPresentation)
        );
    }

    #[test]
    fn dynamic_representation_key_survives_timeline_growth_and_static_finalization() {
        let dynamic = dynamic_mpd(
            r#"<SegmentTemplate timescale="1" startNumber="1" duration="4" media="seg-$Number$.m4s"/>"#,
            r#"timeShiftBufferDepth="PT20S""#,
        );
        let static_mpd = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT120S">
  <Period duration="PT120S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1" startNumber="1" duration="4" media="seg-$Number$.m4s"/>
      <Representation id="v1" bandwidth="1000000" width="1280" height="720"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let live = parse_dash_manifest_at(BASE_URL, &dynamic, dynamic_test_clock(120))
            .expect("parse dynamic representation");
        let finalized = parse_dash_manifest(BASE_URL, static_mpd)
            .expect("parse equivalent static presentation");
        let live_representation = &live.video_representations[0];
        let static_representation = &finalized.video_representations[0];
        assert_eq!(live_representation.live_key, static_representation.live_key);
        assert_ne!(live_representation.key, static_representation.key);
        let live_plan = plan_dash_representation(&live, &live_representation.live_key)
            .expect("plan current dynamic window");
        let finalized_plan = plan_dash_representation(&finalized, &live_representation.live_key)
            .expect("plan final static presentation with the queued live key");
        let dynamic_resource = live_plan
            .resources
            .iter()
            .find(|resource| resource.segment_number == Some(26))
            .expect("dynamic window starts with segment 26");
        let final_resource = finalized_plan
            .resources
            .iter()
            .find(|resource| resource.segment_number == Some(26))
            .expect("final MPD includes captured segment 26");
        assert_eq!(
            dynamic_resource.filename, final_resource.filename,
            "dynamic and static finalization must retain the same staged resource identity"
        );

        let later = dynamic_mpd(
            r#"<SegmentTemplate timescale="1" startNumber="1" duration="4" media="seg-$Number$.m4s">
  <SegmentTimeline><S t="0" d="4" r="-1"/></SegmentTimeline>
</SegmentTemplate>"#,
            r#"timeShiftBufferDepth="PT20S""#,
        );
        let refreshed = parse_dash_manifest_at(BASE_URL, &later, dynamic_test_clock(124))
            .expect("parse refreshed dynamic timeline");
        assert_eq!(
            refreshed.video_representations[0].live_key,
            live_representation.live_key
        );
    }

    #[test]
    fn dynamic_segment_list_uses_explicit_timeline_times_as_stable_sequence_ids() {
        let mut urls = String::new();
        let mut timeline = String::new();
        for index in 0..60_u64 {
            let time = index * 2;
            urls.push_str(&format!(r#"<SegmentURL media="seg-{time}.m4s"/>"#));
            timeline.push_str(&format!(r#"<S t="{time}" d="2"/>"#));
        }
        let body = format!(
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic" availabilityStartTime="1970-01-01T00:00:00Z" minimumUpdatePeriod="PT2S" timeShiftBufferDepth="PT10S"><Period><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentList timescale="1"><Initialization sourceURL="init.mp4"/><SegmentTimeline>{timeline}</SegmentTimeline>{urls}</SegmentList><Representation id="v1" bandwidth="1000000"/></AdaptationSet></Period></MPD>"#
        );
        let manifest = parse_dash_manifest_at(
            BASE_URL,
            &body,
            SystemTime::UNIX_EPOCH + Duration::from_secs(120),
        )
        .expect("dynamic SegmentList with explicit timeline should parse");
        let representation = &manifest.video_representations[0];
        let plan = plan_dash_representation(&manifest, &representation.live_key)
            .expect("dynamic SegmentList should expand");
        let numbers = plan
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .filter_map(|resource| resource.segment_number)
            .collect::<Vec<_>>();
        assert_eq!(numbers, vec![56, 57, 58, 59, 60]);
    }

    #[test]
    fn dynamic_segment_list_rejects_nondefault_start_number() {
        let body = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic" availabilityStartTime="1970-01-01T00:00:00Z" minimumUpdatePeriod="PT2S" timeShiftBufferDepth="PT10S"><Period><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentList startNumber="2" timescale="1"><Initialization sourceURL="init.mp4"/><SegmentTimeline><S t="0" d="2"/></SegmentTimeline><SegmentURL media="seg-0.m4s"/></SegmentList><Representation id="v1" bandwidth="1000000"/></AdaptationSet></Period></MPD>"#;
        assert_eq!(
            parse_dash_manifest_at(
                BASE_URL,
                body,
                SystemTime::UNIX_EPOCH + Duration::from_secs(120)
            )
            .err(),
            Some(DashError::UnsupportedAddressing)
        );
    }

    #[test]
    fn dynamic_segment_list_requires_an_explicit_initialization_url() {
        for initialization in [
            "",
            r#"<Initialization/>"#,
            r#"<Initialization range="0-99"/>"#,
        ] {
            let body = format!(
                r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic" availabilityStartTime="1970-01-01T00:00:00Z" minimumUpdatePeriod="PT2S" timeShiftBufferDepth="PT10S"><Period><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentList timescale="1">{initialization}<SegmentTimeline><S t="0" d="2"/></SegmentTimeline><SegmentURL media="seg-0.m4s"/></SegmentList><Representation id="v1" bandwidth="1000000"/></AdaptationSet></Period></MPD>"#
            );
            assert_eq!(
                parse_dash_manifest_at(
                    BASE_URL,
                    &body,
                    SystemTime::UNIX_EPOCH + Duration::from_secs(120)
                )
                .err(),
                Some(DashError::UnsupportedAddressing),
                "{initialization}"
            );
        }
    }

    #[test]
    fn dynamic_segment_list_rejects_duplicate_or_overlapping_resources() {
        for (initialization, segments) in [
            (
                r#"<Initialization sourceURL="init.mp4"/>"#,
                r#"<SegmentURL media="same.m4s"/><SegmentURL media="same.m4s"/>"#,
            ),
            (
                r#"<Initialization sourceURL="init.mp4"/>"#,
                r#"<SegmentURL media="same.m4s" mediaRange="0-99"/><SegmentURL media="same.m4s" mediaRange="50-149"/>"#,
            ),
            (
                r#"<Initialization sourceURL="same.m4s" range="0-99"/>"#,
                r#"<SegmentURL media="same.m4s" mediaRange="50-149"/><SegmentURL media="other.m4s"/>"#,
            ),
        ] {
            let body = format!(
                r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic" availabilityStartTime="1970-01-01T00:00:00Z" minimumUpdatePeriod="PT2S" timeShiftBufferDepth="PT10S"><Period><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentList timescale="1">{initialization}<SegmentTimeline><S t="0" d="2"/><S t="2" d="2"/></SegmentTimeline>{segments}</SegmentList><Representation id="v1" bandwidth="1000000"/></AdaptationSet></Period></MPD>"#
            );
            let manifest = parse_dash_manifest_at(
                BASE_URL,
                &body,
                SystemTime::UNIX_EPOCH + Duration::from_secs(120),
            )
            .expect("valid addressing should parse before resource expansion");
            let representation = &manifest.video_representations[0];
            assert_eq!(
                plan_dash_representation(&manifest, &representation.live_key).err(),
                Some(DashError::InvalidSegment)
            );
        }
    }

    #[test]
    fn dynamic_segment_list_rejects_implicit_timeline_times_and_repeat_runs() {
        for timeline in [r#"<S d="2"/>"#, r#"<S t="100" d="2" r="1"/>"#] {
            let body = format!(
                r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic" availabilityStartTime="1970-01-01T00:00:00Z" minimumUpdatePeriod="PT2S" timeShiftBufferDepth="PT10S"><Period><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentList timescale="1"><Initialization sourceURL="init.mp4"/><SegmentTimeline>{timeline}</SegmentTimeline><SegmentURL media="seg-100.m4s"/><SegmentURL media="seg-102.m4s"/></SegmentList><Representation id="v1" bandwidth="1000000"/></AdaptationSet></Period></MPD>"#
            );
            assert_eq!(
                parse_dash_manifest_at(
                    BASE_URL,
                    &body,
                    SystemTime::UNIX_EPOCH + Duration::from_secs(120)
                )
                .err(),
                Some(DashError::UnsupportedAddressing),
                "{timeline}"
            );
        }
    }

    #[test]
    fn dynamic_dash_requires_bounded_update_metadata_and_rejects_segment_list() {
        let missing_ast = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic" minimumUpdatePeriod="PT4S"><Period><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentTemplate duration="4" media="s-$Number$.m4s"/><Representation id="v1"/></AdaptationSet></Period></MPD>"#;
        let missing_update = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic" availabilityStartTime="1970-01-01T00:00:00Z"><Period><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentTemplate duration="4" media="s-$Number$.m4s"/><Representation id="v1"/></AdaptationSet></Period></MPD>"#;
        let list = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic" availabilityStartTime="1970-01-01T00:00:00Z" minimumUpdatePeriod="PT4S"><Period><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentList><SegmentURL media="segment.m4s"/></SegmentList><Representation id="v1"/></AdaptationSet></Period></MPD>"#;
        assert_eq!(
            parse_dash_manifest_at(BASE_URL, missing_ast, dynamic_test_clock(20)).err(),
            Some(DashError::UnsupportedPresentation)
        );
        assert_eq!(
            parse_dash_manifest_at(BASE_URL, missing_update, dynamic_test_clock(20)).err(),
            Some(DashError::UnsupportedPresentation)
        );
        assert_eq!(
            parse_dash_manifest_at(BASE_URL, list, dynamic_test_clock(20)).err(),
            Some(DashError::UnsupportedAddressing)
        );
    }

    #[test]
    fn dynamic_dash_does_not_plan_a_segment_before_it_is_complete() {
        let body = dynamic_mpd(
            r#"<SegmentTemplate timescale="1" duration="4" media="seg-$Number$.m4s"/>"#,
            "",
        );
        let manifest = parse_dash_manifest_at(BASE_URL, &body, dynamic_test_clock(3))
            .expect("dynamic presentation can arrive before its first complete segment");
        let plan = plan_dash_representation(&manifest, &manifest.video_representations[0].live_key)
            .expect("an empty first live window remains refreshable");
        assert!(
            !plan
                .resources
                .iter()
                .any(|resource| resource.kind == DashResourceKind::Segment),
            "a segment is not exposed until its whole duration is available"
        );
    }

    #[test]
    fn expands_duration_template_with_initialization_and_padded_numbers() {
        let plan =
            parse_dash_manifest(BASE_URL, TEMPLATE_MPD).expect("valid template MPD should parse");
        let video = &plan.video_representations[0];
        let resources = plan_dash_representation(&plan, &video.key)
            .expect("selected representation should expand");

        assert_eq!(resources.resources.len(), 4);
        assert_eq!(
            resources.resources[0].kind,
            DashResourceKind::Initialization
        );
        assert_eq!(
            resources.resources[0].url.as_str(),
            "https://cdn.example.test/path/media/init-v1.mp4"
        );
        assert_eq!(
            resources.resources[1].url.as_str(),
            "https://cdn.example.test/path/media/v1/005-0.m4s"
        );
        assert_eq!(
            resources.resources[3].url.as_str(),
            "https://cdn.example.test/path/media/v1/007-8.m4s"
        );
    }

    #[test]
    fn expands_timeline_with_explicit_start_and_bounded_negative_repeat() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10S">
  <Period duration="PT10S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1" startNumber="10" media="seg-$Number$-$Time$.m4s">
        <SegmentTimeline>
          <S t="0" d="2" r="1"/>
          <S t="4" d="3" r="-1"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v1" bandwidth="500000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let plan = parse_dash_manifest(BASE_URL, body).expect("timeline MPD should parse");
        let video = &plan.video_representations[0];
        let resources =
            plan_dash_representation(&plan, &video.key).expect("timeline should expand");

        let urls = resources
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .map(|resource| resource.url.path().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            urls,
            [
                "/path/seg-10-0.m4s",
                "/path/seg-11-2.m4s",
                "/path/seg-12-4.m4s",
                "/path/seg-13-7.m4s"
            ]
        );
    }

    #[test]
    fn expands_static_timeline_using_presentation_time_offset() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT4S">
  <Period duration="PT4S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1000" startNumber="7"
                       presentationTimeOffset="900000"
                       media="seg-$Number$-$Time$.m4s">
        <SegmentTimeline>
          <S t="900000" d="2000" r="1"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let plan = parse_dash_manifest(BASE_URL, body).expect("non-zero PTO timeline should parse");
        let video = &plan.video_representations[0];
        let resources = plan_dash_representation(&plan, &video.key)
            .expect("PTO-aligned timeline should expand");

        let urls = resources
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .map(|resource| resource.url.path().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(urls, ["/path/seg-7-900000.m4s", "/path/seg-8-902000.m4s"]);
    }

    #[test]
    fn expands_static_duration_template_from_presentation_time_offset() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT4S">
  <Period duration="PT4S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1000" duration="2000"
                       presentationTimeOffset="900000"
                       media="seg-$Time$.m4s"/>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let plan = parse_dash_manifest(BASE_URL, body)
            .expect("duration template PTO may exceed the Period duration in raw units");
        let resources = plan_dash_representation(&plan, &plan.video_representations[0].key)
            .expect("duration template should expand from the sample-timeline offset");

        let urls = resources
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .map(|resource| resource.url.path().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(urls, ["/path/seg-900000.m4s", "/path/seg-902000.m4s"]);
    }

    #[test]
    fn negative_timeline_repeat_uses_presentation_time_offset_for_period_end() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT4S">
  <Period duration="PT4S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1000" presentationTimeOffset="900000"
                       media="seg-$Time$.m4s">
        <SegmentTimeline>
          <S t="900000" d="2000" r="-1"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let plan = parse_dash_manifest(BASE_URL, body).expect("non-zero PTO timeline should parse");
        let resources = plan_dash_representation(&plan, &plan.video_representations[0].key)
            .expect("negative repeat should reach the PTO-adjusted Period end");

        let urls = resources
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .map(|resource| resource.url.path().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(urls, ["/path/seg-900000.m4s", "/path/seg-902000.m4s"]);
    }

    #[test]
    fn skips_timeline_segments_before_the_period_and_preserves_numbers() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <Period duration="PT2S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1" startNumber="10"
                       presentationTimeOffset="4"
                       media="seg-$Number$-$Time$.m4s">
        <SegmentTimeline>
          <S t="0" d="2" r="2"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let plan = parse_dash_manifest(BASE_URL, body).expect("static timeline should parse");
        let resources = plan_dash_representation(&plan, &plan.video_representations[0].key)
            .expect("the segment at the Period start should remain available");

        let urls = resources
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .map(|resource| resource.url.path().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(urls, ["/path/seg-12-4.m4s"]);
    }

    #[test]
    fn falls_back_when_a_timeline_segment_crosses_the_period_start() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <Period duration="PT2S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1" presentationTimeOffset="4"
                       media="seg-$Time$.m4s">
        <SegmentTimeline>
          <S t="3" d="2"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let plan = parse_dash_manifest(BASE_URL, body).expect("MPD structure should parse");
        assert_eq!(
            plan_dash_representation(&plan, &plan.video_representations[0].key).err(),
            Some(DashError::UnsupportedAddressing)
        );
    }

    #[test]
    fn resolves_presentation_time_offset_with_representation_precedence() {
        let inherited = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <Period duration="PT2S">
    <SegmentTemplate timescale="1" duration="2" presentationTimeOffset="100"
                     media="seg-$Time$.m4s"/>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate presentationTimeOffset="200"/>
      <Representation id="v1">
        <SegmentTemplate presentationTimeOffset="300"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let plan =
            parse_dash_manifest(BASE_URL, inherited).expect("inherited templates should parse");
        let resources = plan_dash_representation(&plan, &plan.video_representations[0].key)
            .expect("the most specific PTO should be used");
        assert_eq!(resources.resources[0].url.path(), "/path/seg-300.m4s");

        let explicit_zero = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <Period duration="PT2S">
    <SegmentTemplate timescale="1" duration="2" presentationTimeOffset="100"
                     media="seg-$Time$.m4s"/>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <Representation id="v1">
        <SegmentTemplate presentationTimeOffset="0"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let plan = parse_dash_manifest(BASE_URL, explicit_zero)
            .expect("explicit zero should override PTO");
        let resources = plan_dash_representation(&plan, &plan.video_representations[0].key)
            .expect("explicit zero PTO should be honored");
        assert_eq!(resources.resources[0].url.path(), "/path/seg-0.m4s");
    }

    #[test]
    fn representation_specific_pto_offsets_include_period_start() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT6S">
  <Period start="PT2S" duration="PT4S">
    <AdaptationSet id="video" contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1000" duration="2000"
                       presentationTimeOffset="1000" media="video-$Time$.m4s"/>
      <Representation id="v1"/>
    </AdaptationSet>
    <AdaptationSet id="audio" contentType="audio" mimeType="audio/mp4">
      <SegmentTemplate timescale="1000" duration="2000"
                       presentationTimeOffset="8000" media="audio-$Time$.m4s"/>
      <Representation id="a1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let manifest = parse_dash_manifest(BASE_URL, body)
            .expect("static DASH with distinct track PTO values should parse");
        let video = plan_dash_representation(&manifest, &manifest.video_representations[0].key)
            .expect("plan video representation");
        let audio = plan_dash_representation(&manifest, &manifest.audio_representations[0].key)
            .expect("plan audio representation");

        assert_eq!(
            video.input_timestamp_offset.ffmpeg_argument().as_deref(),
            Some("1.000000000")
        );
        assert_eq!(
            audio.input_timestamp_offset.ffmpeg_argument().as_deref(),
            Some("-6.000000000")
        );
    }

    #[test]
    fn zero_origin_dash_omits_ffmpeg_timestamp_offset() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <Period duration="PT2S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1" duration="2" media="segment-$Number$.m4s"/>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let manifest =
            parse_dash_manifest(BASE_URL, body).expect("zero-origin static DASH should parse");
        let plan = plan_dash_representation(&manifest, &manifest.video_representations[0].key)
            .expect("plan zero-origin representation");
        assert_eq!(plan.input_timestamp_offset.ffmpeg_argument(), None);
    }

    #[test]
    fn pto_changes_preserve_the_existing_representation_key() {
        fn plan_for_pto(presentation_time_offset: u64) -> (String, String) {
            let body = format!(
                r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <Period duration="PT2S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1000" duration="2000"
                       presentationTimeOffset="{presentation_time_offset}"
                       media="segment-$Time$.m4s"/>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#
            );
            let manifest =
                parse_dash_manifest(BASE_URL, &body).expect("static PTO manifest should parse");
            let representation = &manifest.video_representations[0];
            let plan = plan_dash_representation(&manifest, &representation.key)
                .expect("static PTO representation should be planned");
            (
                representation.key.clone(),
                plan.input_timestamp_offset
                    .ffmpeg_argument()
                    .expect("non-zero PTO produces an offset"),
            )
        }

        let (first_key, first_offset) = plan_for_pto(8_000);
        let (second_key, second_offset) = plan_for_pto(9_000);
        assert_eq!(
            first_key, second_key,
            "persisted representation keys remain stable"
        );
        assert_ne!(
            first_offset, second_offset,
            "PTO remains separate plan metadata"
        );
    }

    #[test]
    fn rejects_nonzero_segment_list_presentation_time_offset() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <Period duration="PT2S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentList presentationTimeOffset="9000">
        <Initialization sourceURL="init.mp4"/>
        <SegmentURL media="segment.m4s"/>
      </SegmentList>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        assert_eq!(
            parse_dash_manifest(BASE_URL, body).err(),
            Some(DashError::UnsupportedAddressing)
        );
    }

    #[test]
    fn dynamic_template_with_nonzero_presentation_time_offset_uses_fallback() {
        let body = dynamic_mpd(
            r#"<SegmentTemplate timescale="1" duration="4"
                               presentationTimeOffset="4" media="seg-$Number$.m4s"/>"#,
            "",
        );
        assert_eq!(
            parse_dash_manifest_at(BASE_URL, &body, dynamic_test_clock(20)).err(),
            Some(DashError::UnsupportedAddressing)
        );
    }

    #[test]
    fn rejects_negative_template_presentation_time_offset() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <Period duration="PT2S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1" duration="2"
                       presentationTimeOffset="-1" media="seg-$Time$.m4s"/>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        assert!(parse_dash_manifest(BASE_URL, body).is_err());
    }

    #[test]
    fn negative_timeline_repeat_uses_ceiling_to_next_explicit_start() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10S">
  <Period duration="PT10S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1" media="seg-$Time$.m4s">
        <SegmentTimeline>
          <S t="0" d="2" r="-1"/>
          <S t="5" d="3" r="0"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let plan = parse_dash_manifest(BASE_URL, body).expect("timeline MPD should parse");
        let video = &plan.video_representations[0];
        let resources = plan_dash_representation(&plan, &video.key)
            .expect("negative repeat should expand to the next explicit start");
        let times = resources
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .map(|resource| resource.url.path().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            times,
            [
                "/path/seg-0.m4s",
                "/path/seg-2.m4s",
                "/path/seg-4.m4s",
                "/path/seg-5.m4s"
            ]
        );
    }

    #[test]
    fn rejects_explicit_timeline_start_overlapping_previous_positive_repeat_run() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT12S">
  <Period duration="PT12S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1" media="seg-$Time$.m4s">
        <SegmentTimeline>
          <S t="0" d="4" r="2"/>
          <S t="4" d="4"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let plan = parse_dash_manifest(BASE_URL, body).expect("MPD syntax is valid");
        let video = &plan.video_representations[0];

        assert_eq!(
            plan_dash_representation(&plan, &video.key).err(),
            Some(DashError::InvalidSegment),
            "the explicit t=4 entry overlaps the preceding run ending at t=12"
        );
    }

    #[test]
    fn negative_timeline_repeat_uses_ceiling_to_period_end() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10S">
  <Period duration="PT10S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1" media="seg-$Time$.m4s">
        <SegmentTimeline>
          <S t="4" d="4" r="-1"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let plan = parse_dash_manifest(BASE_URL, body).expect("timeline MPD should parse");
        let video = &plan.video_representations[0];
        let resources = plan_dash_representation(&plan, &video.key)
            .expect("last negative repeat should reach the Period end");
        let times = resources
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .map(|resource| resource.url.path().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(times, ["/path/seg-4.m4s", "/path/seg-8.m4s"]);
    }

    #[test]
    fn video_and_audio_resource_filenames_are_representation_scoped() {
        let plan = parse_dash_manifest(BASE_URL, TEMPLATE_MPD)
            .expect("static video/audio MPD should parse");
        let video = plan_dash_representation(&plan, &plan.video_representations[0].key)
            .expect("video representation should expand");
        let audio = plan_dash_representation(&plan, &plan.audio_representations[0].key)
            .expect("audio representation should expand");

        let video_names = video
            .resources
            .iter()
            .map(|resource| resource.filename.as_str())
            .collect::<std::collections::HashSet<_>>();
        let audio_names = audio
            .resources
            .iter()
            .map(|resource| resource.filename.as_str())
            .collect::<std::collections::HashSet<_>>();
        assert!(
            video_names.is_disjoint(&audio_names),
            "audio and video staging names must not collide"
        );
        assert!(
            video
                .resources
                .iter()
                .all(|resource| { resource.filename.starts_with("dash-") })
        );
        assert!(
            audio
                .resources
                .iter()
                .all(|resource| { resource.filename.starts_with("dash-") })
        );
    }

    #[test]
    fn expands_segment_list_with_initialization_and_byte_ranges() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT4S">
  <Period duration="PT4S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentList>
        <Initialization sourceURL="all.mp4" range="0-999"/>
        <SegmentURL media="all.mp4" mediaRange="1000-1999"/>
        <SegmentURL media="all.mp4" mediaRange="2000-2999"/>
      </SegmentList>
      <Representation id="v1" bandwidth="500000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let plan = parse_dash_manifest(BASE_URL, body).expect("SegmentList MPD should parse");
        let video = &plan.video_representations[0];
        let resources =
            plan_dash_representation(&plan, &video.key).expect("SegmentList should expand");

        assert_eq!(resources.resources.len(), 3);
        assert_eq!(
            resources.resources[0].range,
            Some(ManifestByteRange {
                offset: 0,
                length: 1_000
            })
        );
        assert_eq!(
            resources.resources[1].range,
            Some(ManifestByteRange {
                offset: 1_000,
                length: 1_000
            })
        );
        assert_eq!(
            resources.resources[2].range,
            Some(ManifestByteRange {
                offset: 2_000,
                length: 1_000
            })
        );
    }

    #[test]
    fn rejects_dynamic_multi_period_drm_xlink_and_webm() {
        let rejected = [
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic"><Period/></MPD>"#,
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT1S"/><Period duration="PT1S"/></MPD>"#,
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><ContentProtection schemeIdUri="urn:uuid:00000000-0000-0000-0000-000000000000"/><Period duration="PT2S"/></MPD>"#,
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" xmlns:xlink="http://www.w3.org/1999/xlink" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S" xlink:href="external.xml"/></MPD>"#,
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S"><AdaptationSet contentType="video" mimeType="video/webm"><SegmentList><SegmentURL media="v.webm"/></SegmentList><Representation id="v1"/></AdaptationSet></Period></MPD>"#,
        ];

        for body in rejected {
            assert!(
                parse_dash_manifest(BASE_URL, body).is_err(),
                "unsupported MPD should not enter native path: {body}"
            );
        }
    }

    #[test]
    fn expands_single_file_segment_base_as_one_media_resource() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT4S">
  <Period duration="PT4S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <Representation id="v1" bandwidth="500000">
        <BaseURL>video.mp4</BaseURL>
        <SegmentBase/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let plan = parse_dash_manifest(BASE_URL, body).expect("SegmentBase MPD should parse");
        let video = plan.video_representations.first().expect("video track");
        let resources =
            plan_dash_representation(&plan, &video.key).expect("SegmentBase should expand");

        assert_eq!(resources.resources.len(), 1);
        assert_eq!(resources.resources[0].kind, DashResourceKind::Segment);
        assert_eq!(
            resources.resources[0].url.as_str(),
            "https://cdn.example.test/path/video.mp4"
        );
        assert_eq!(resources.resources[0].range, None);
        assert_eq!(resources.resources[0].segment_number, Some(0));
        assert!(resources.resources[0].filename.starts_with("dash-segment"));
    }

    #[test]
    fn segment_base_inherits_adaptation_set_base_url() {
        let body = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT4S"><Period duration="PT4S"><AdaptationSet contentType="video" mimeType="video/mp4"><BaseURL>clip.mp4</BaseURL><Representation id="v1" bandwidth="500000"><SegmentBase/></Representation></AdaptationSet></Period></MPD>"#;
        let plan = parse_dash_manifest(BASE_URL, body).expect("inherited SegmentBase should parse");
        let resources = plan_dash_representation(&plan, &plan.video_representations[0].key)
            .expect("inherited SegmentBase should expand");
        assert_eq!(
            resources.resources[0].url.as_str(),
            "https://cdn.example.test/path/clip.mp4"
        );
    }

    #[test]
    fn segment_base_rejects_webm_base_url() {
        let body = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S"><AdaptationSet contentType="video" mimeType="video/mp4"><Representation id="v1" bandwidth="500000"><BaseURL>video.webm</BaseURL><SegmentBase/></Representation></AdaptationSet></Period></MPD>"#;
        let plan = parse_dash_manifest(BASE_URL, body)
            .expect("SegmentBase with WebM base URL should parse");
        assert_eq!(
            plan_dash_representation(&plan, &plan.video_representations[0].key).err(),
            Some(DashError::UnsupportedPresentation)
        );
    }

    #[test]
    fn segment_base_requires_an_explicit_base_url() {
        let body = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S"><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentBase indexRange="0-999"/><Representation id="v1"/></AdaptationSet></Period></MPD>"#;

        assert_eq!(
            parse_dash_manifest(BASE_URL, body).err(),
            Some(DashError::UnsupportedAddressing)
        );
    }

    #[test]
    fn segment_base_rejects_dynamic_presentations() {
        let dynamic = dynamic_mpd(
            r#"<BaseURL>video.mp4</BaseURL><SegmentBase indexRange="0-999"/>"#,
            "",
        );
        assert_eq!(
            parse_dash_manifest_at(BASE_URL, &dynamic, dynamic_test_clock(120)).err(),
            Some(DashError::UnsupportedAddressing)
        );
    }

    #[test]
    fn segment_base_rejects_unsupported_availability_and_delta_attributes() {
        let cases = [
            r#"availabilityTimeOffset="1""#,
            r#"availabilityTimeComplete="false""#,
            r#"eptDelta="1""#,
            r#"pdDelta="1""#,
        ];
        for attributes in cases {
            let body = format!(
                r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S"><AdaptationSet contentType="video" mimeType="video/mp4"><Representation id="v1"><BaseURL>video.mp4</BaseURL><SegmentBase {attributes}/></Representation></AdaptationSet></Period></MPD>"#,
                attributes = attributes
            );
            assert_eq!(
                parse_dash_manifest(BASE_URL, &body).err(),
                Some(DashError::UnsupportedAddressing),
                "{attributes}"
            );
        }
    }

    #[test]
    fn segment_base_rejects_mixed_addressing() {
        // SegmentTemplate on the parent plus SegmentBase on the representation.
        let mixed_template = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S"><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentTemplate media="s-$Number$.m4s" duration="2"/><Representation id="v1"><BaseURL>video.mp4</BaseURL><SegmentBase indexRange="0-999"/></Representation></AdaptationSet></Period></MPD>"#;
        assert_eq!(
            parse_dash_manifest(BASE_URL, mixed_template).err(),
            Some(DashError::UnsupportedAddressing)
        );

        // SegmentList on a parent plus SegmentBase on the representation.
        let mixed_list = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S"><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentList><SegmentURL media="a.mp4"/></SegmentList><Representation id="v1"><BaseURL>video.mp4</BaseURL><SegmentBase indexRange="0-999"/></Representation></AdaptationSet></Period></MPD>"#;
        assert_eq!(
            parse_dash_manifest(BASE_URL, mixed_list).err(),
            Some(DashError::UnsupportedAddressing)
        );

        // A BaseURL that points at a directory resolves without a file name.
        let directory = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S"><AdaptationSet contentType="video" mimeType="video/mp4"><Representation id="v1"><BaseURL>folder/</BaseURL><SegmentBase indexRange="0-999"/></Representation></AdaptationSet></Period></MPD>"#;
        assert_eq!(
            parse_dash_manifest(BASE_URL, directory).err(),
            Some(DashError::UnsupportedAddressing)
        );

        // A BaseURL that is the MPD URL itself names the manifest, not media.
        let manifest = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S"><AdaptationSet contentType="video" mimeType="video/mp4"><Representation id="v1"><BaseURL>manifest.mpd</BaseURL><SegmentBase indexRange="0-999"/></Representation></AdaptationSet></Period></MPD>"#;
        assert_eq!(
            parse_dash_manifest(BASE_URL, manifest).err(),
            Some(DashError::UnsupportedAddressing)
        );
    }

    #[test]
    fn rejects_subset_and_preselection_combinations() {
        let subset = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <Period duration="PT2S">
    <Subset id="1" contains="1 2"/>
    <AdaptationSet id="1" contentType="video" mimeType="video/mp4">
      <SegmentTemplate duration="2" media="video-$Number$.m4s"/>
      <Representation id="v1"/>
    </AdaptationSet>
    <AdaptationSet id="2" contentType="audio" mimeType="audio/mp4">
      <SegmentTemplate duration="2" media="audio-$Number$.m4s"/>
      <Representation id="a1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let preselection = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <Period duration="PT2S">
    <AdaptationSet id="1" contentType="audio" mimeType="audio/mp4">
      <SegmentTemplate duration="2" media="audio-$Number$.m4s"/>
      <Representation id="a1"/>
    </AdaptationSet>
    <Preselection id="main" tag="main" preselectionComponents="1"
                  codecs="ac-4.02.01.03"/>
  </Period>
</MPD>"#;

        let parsed_subset = dash_mpd_core::parse(subset).expect("Subset fixture MPD should parse");
        assert_eq!(parsed_subset.periods[0].subsets.len(), 1);
        assert_eq!(
            parse_dash_manifest(BASE_URL, subset).err(),
            Some(DashError::UnsupportedPresentation)
        );

        let parsed_preselection =
            dash_mpd_core::parse(preselection).expect("Preselection fixture MPD should parse");
        assert_eq!(parsed_preselection.periods[0].pre_selections.len(), 1);
        assert_eq!(
            parse_dash_manifest(BASE_URL, preselection).err(),
            Some(DashError::UnsupportedPresentation)
        );
    }

    #[test]
    fn rejects_unknown_templates_and_invalid_byte_ranges() {
        let unknown = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S"><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentTemplate media="$Unknown$.m4s" duration="2"/><Representation id="v1"/></AdaptationSet></Period></MPD>"#;
        let invalid_range = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S"><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentList><SegmentURL media="v.mp4" mediaRange="9-2"/></SegmentList><Representation id="v1"/></AdaptationSet></Period></MPD>"#;

        assert_eq!(
            parse_dash_manifest(BASE_URL, unknown)
                .and_then(|plan| plan_dash_representation(
                    &plan,
                    &plan.video_representations[0].key
                ))
                .err(),
            Some(DashError::UnsupportedAddressing)
        );
        assert_eq!(
            parse_dash_manifest(BASE_URL, invalid_range).err(),
            Some(DashError::InvalidSegment)
        );
    }

    #[test]
    fn rejects_non_https_and_oversized_manifests() {
        assert_eq!(
            parse_dash_manifest("http://cdn.example.test/manifest.mpd", TEMPLATE_MPD).err(),
            Some(DashError::InvalidManifestUrl)
        );

        let oversized = " ".repeat(MAX_MANIFEST_BYTES + 1);
        assert_eq!(
            parse_dash_manifest(BASE_URL, &oversized).err(),
            Some(DashError::Parse)
        );
    }

    #[test]
    fn rejects_scte35_ad_signaling_in_period_and_inband_event_streams() {
        let period_event = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S"><EventStream schemeIdUri="urn:scte:scte35:2013:xml"><Event id="1" presentationTime="0" duration="2"/></EventStream><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentTemplate media="v-$Number$.m4s" duration="2"/><Representation id="v1" bandwidth="500000"/></AdaptationSet></Period></MPD>"#;
        let inband_event = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S"><AdaptationSet contentType="video" mimeType="video/mp4"><InbandEventStream schemeIdUri="urn:scte:scte35:2013:bin"/><SegmentTemplate media="v-$Number$.m4s" duration="2"/><Representation id="v1" bandwidth="500000"/></AdaptationSet></Period></MPD>"#;

        for body in [period_event, inband_event] {
            assert_eq!(
                parse_dash_manifest(BASE_URL, body).err(),
                Some(DashError::AdSignaling),
                "SCTE-35 ad signaling must fail closed: {body}"
            );
        }
    }
}
