//! Static, single-period DASH parsing and segment resource planning.

use std::time::Duration;

use thiserror::Error;
use url::Url;

use super::ManifestByteRange;

const MAX_MANIFEST_BYTES: usize = 4 * 1024 * 1024;
const MAX_REPRESENTATIONS: usize = 1_024;
const MAX_SEGMENTS: usize = 100_000;
const DASH_ROLE_SCHEME: &str = "urn:mpeg:dash:role:2011";

/// Errors returned when a DASH MPD is outside Grab's native v1 subset.
#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum DashError {
    #[error("invalid DASH manifest URL")]
    InvalidManifestUrl,
    #[error("DASH manifest is malformed or too large")]
    Parse,
    #[error("DASH presentation is not static single-period VOD")]
    UnsupportedPresentation,
    #[error("DASH addressing or resource URL is unsupported")]
    UnsupportedAddressing,
    #[error("DASH segment description is invalid")]
    InvalidSegment,
    #[error("DASH presentation contains no supported video representation")]
    NoVideoRepresentations,
    #[error("DASH manifest contains too many representations or segments")]
    LimitExceeded,
}

/// Whether a DASH representation carries video or audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DashTrackKind {
    Video,
    Audio,
}

/// A user-selectable video or automatically-selectable audio representation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DashRepresentation {
    pub(crate) key: String,
    pub(crate) representation_id: Option<String>,
    pub(crate) bandwidth: Option<u64>,
    pub(crate) width: Option<u64>,
    pub(crate) height: Option<u64>,
    pub(crate) mime_type: Option<String>,
    pub(crate) codecs: Option<String>,
    pub(crate) language: Option<String>,
    pub(crate) kind: DashTrackKind,
    pub(crate) is_default_audio: bool,
    source: DashRepresentationSource,
}

/// Parsed static DASH presentation and its selectable representations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DashManifestPlan {
    pub(crate) duration: Duration,
    pub(crate) video_representations: Box<[DashRepresentation]>,
    pub(crate) audio_representations: Box<[DashRepresentation]>,
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
}

/// Expanded initialization and media resources for one representation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DashRepresentationPlan {
    pub(crate) representation: DashRepresentation,
    pub(crate) duration: Duration,
    pub(crate) resources: Box<[DashResource]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DashRepresentationSource {
    base_url: Url,
    addressing: DashAddressing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DashAddressing {
    Template(DashTemplate),
    List(DashList),
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
    period_duration: Duration,
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
    segments: Box<[DashListSegment]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DashListSegment {
    media: String,
    range: Option<ManifestByteRange>,
}

/// Parse a static, single-period DASH MPD into video and audio representations.
///
/// The native v1 path supports SegmentTemplate and SegmentList. Dynamic
/// presentations, multiple periods, DRM, XLink, SegmentBase, indexed
/// addressing, non-zero presentation-time offsets, and WebM representations
/// remain on the existing yt-dlp fallback path.
///
/// # Errors
/// Returns an error when the MPD is malformed, exceeds the input limit, uses
/// unsupported features, or contains no supported video representation.
pub(crate) fn parse_dash_manifest(
    manifest_url: &str,
    body: &str,
) -> Result<DashManifestPlan, DashError> {
    if body.len() > MAX_MANIFEST_BYTES {
        return Err(DashError::Parse);
    }

    let manifest_url = parse_manifest_url(manifest_url)?;
    let mpd = dash_mpd_core::parse(body).map_err(|_| DashError::Parse)?;

    if !matches!(mpd.mpdtype.as_deref(), None | Some("static"))
        || mpd.minimumUpdatePeriod.is_some()
        || mpd.availabilityStartTime.is_some()
        || mpd.timeShiftBufferDepth.is_some()
        || !mpd.PatchLocation.is_empty()
        || mpd.periods.len() != 1
        || !mpd.ContentProtection.is_empty()
    {
        return Err(DashError::UnsupportedPresentation);
    }

    let period = &mpd.periods[0];
    if period.href.is_some() || !period.ContentProtection.is_empty() {
        return Err(DashError::UnsupportedPresentation);
    }

    let duration = period
        .duration
        .or_else(|| {
            mpd.mediaPresentationDuration.and_then(|whole_duration| {
                whole_duration.checked_sub(period.start.unwrap_or(Duration::ZERO))
            })
        })
        .filter(|duration| !duration.is_zero())
        .ok_or(DashError::UnsupportedPresentation)?;

    let mpd_base = resolve_base_url(manifest_url, &mpd.base_url)?;
    let period_base = resolve_base_url(mpd_base, &period.BaseURL)?;

    let mut video_representations = Vec::new();
    let mut audio_representations = Vec::new();

    for (adaptation_index, adaptation) in period.adaptations.iter().enumerate() {
        if adaptation.href.is_some() || !adaptation.ContentProtection.is_empty() {
            return Err(DashError::UnsupportedPresentation);
        }

        let adaptation_base = resolve_base_url(period_base.clone(), &adaptation.BaseURL)?;
        let is_default_audio = adaptation.Role.iter().any(|role| {
            role.schemeIdUri == DASH_ROLE_SCHEME && role.value.as_deref() == Some("main")
        });

        for (representation_index, representation) in
            adaptation.representations.iter().enumerate()
        {
            if representation.href.is_some() || !representation.ContentProtection.is_empty() {
                return Err(DashError::UnsupportedPresentation);
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
            );
            let Some(kind) = kind else {
                // Text, metadata, and unknown adaptation sets are not media inputs.
                continue;
            };

            let representation_base =
                resolve_base_url(adaptation_base.clone(), &representation.BaseURL)?;
            let addressing = effective_addressing(period, adaptation, representation, duration)?;
            let key = format!("a{adaptation_index}-r{representation_index}");

            let choice = DashRepresentation {
                key,
                representation_id: representation.id.clone(),
                bandwidth: representation.bandwidth,
                width: representation.width.or(adaptation.width),
                height: representation.height.or(adaptation.height),
                mime_type,
                codecs: representation
                    .codecs
                    .clone()
                    .or_else(|| adaptation.codecs.clone()),
                language: representation.lang.clone().or_else(|| adaptation.lang.clone()),
                kind,
                is_default_audio: kind == DashTrackKind::Audio && is_default_audio,
                source: DashRepresentationSource {
                    base_url: representation_base,
                    addressing,
                },
            };

            match kind {
                DashTrackKind::Video => video_representations.push(choice),
                DashTrackKind::Audio => audio_representations.push(choice),
            }

            if video_representations.len() + audio_representations.len() > MAX_REPRESENTATIONS {
                return Err(DashError::LimitExceeded);
            }
        }
    }

    if video_representations.is_empty() {
        return Err(DashError::NoVideoRepresentations);
    }

    Ok(DashManifestPlan {
        duration,
        video_representations: video_representations.into_boxed_slice(),
        audio_representations: audio_representations.into_boxed_slice(),
    })
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
    let representation = manifest
        .video_representations
        .iter()
        .chain(manifest.audio_representations.iter())
        .find(|candidate| candidate.key == representation_key)
        .cloned()
        .ok_or(DashError::UnsupportedAddressing)?;

    let resources = match &representation.source.addressing {
        DashAddressing::Template(template) => {
            expand_template(&representation, &template.media, template)?
        }
        DashAddressing::List(list) => expand_list(&representation.source.base_url, list)?,
    };

    if !resources.iter().any(|resource| resource.kind == DashResourceKind::Segment) {
        return Err(DashError::InvalidSegment);
    }

    Ok(DashRepresentationPlan {
        representation,
        duration: manifest.duration,
        resources: resources.into_boxed_slice(),
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

/// Resolve exactly one BaseURL at each inheritance level. Choosing among CDNs
/// is deliberately left to a later selection layer instead of silently guessing.
fn resolve_base_url(
    base_url: Url,
    candidates: &[dash_mpd_core::BaseURL],
) -> Result<Url, DashError> {
    let mut values = candidates.iter().map(|candidate| candidate.base.trim()).filter(|value| !value.is_empty());
    let Some(value) = values.next() else {
        if candidates.iter().any(|candidate| {
            candidate.byte_range.is_some()
                || candidate.availability_time_offset.is_some_and(|offset| offset != 0.0)
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
                || candidate.availability_time_offset.is_some_and(|offset| offset != 0.0)
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

fn track_kind(
    representation_content_type: Option<&str>,
    representation_mime_type: Option<&str>,
    adaptation_content_type: Option<&str>,
    adaptation_mime_type: Option<&str>,
) -> Option<DashTrackKind> {
    let content_type = representation_content_type.or(adaptation_content_type);
    let mime_type = representation_mime_type.or(adaptation_mime_type);

    if content_type == Some("video")
        || mime_type.is_some_and(|mime| mime.starts_with("video/"))
    {
        Some(DashTrackKind::Video)
    } else if content_type == Some("audio")
        || mime_type.is_some_and(|mime| mime.starts_with("audio/"))
    {
        Some(DashTrackKind::Audio)
    } else {
        None
    }
}

fn effective_addressing(
    period: &dash_mpd_core::Period,
    adaptation: &dash_mpd_core::AdaptationSet,
    representation: &dash_mpd_core::Representation,
    period_duration: Duration,
) -> Result<DashAddressing, DashError> {
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

    if period.SegmentBase.is_some()
        || adaptation.SegmentBase.is_some()
        || representation.SegmentBase.is_some()
    {
        return Err(DashError::UnsupportedAddressing);
    }

    let has_template = templates.iter().any(Option::is_some);
    let has_list = lists.iter().any(Option::is_some);
    match (has_template, has_list) {
        (true, false) => {
            effective_template(&templates, period_duration).map(DashAddressing::Template)
        }
        (false, true) => effective_list(&lists).map(DashAddressing::List),
        _ => Err(DashError::UnsupportedAddressing),
    }
}

fn effective_template(
    levels: &[Option<&dash_mpd_core::SegmentTemplate>],
    period_duration: Duration,
) -> Result<DashTemplate, DashError> {
    let mut media = None;
    let mut initialization_template = None;
    let mut initialization_element = None;
    let mut start_number = None;
    let mut end_number = None;
    let mut timescale = None;
    let mut duration_ticks = None;
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
            || template.presentationTimeOffset.is_some_and(|offset| offset != 0)
            || template.availabilityTimeOffset.is_some_and(|offset| offset != 0.0)
            || template.availabilityTimeComplete == Some(false)
            || template.BitstreamSwitching.is_some()
        {
            return Err(DashError::UnsupportedAddressing);
        }

        media = template.media.clone().or(media);
        initialization_template = template.initialization.clone().or(initialization_template);
        start_number = template.startNumber.or(start_number);
        end_number = template.endNumber.or(end_number);
        timescale = template.timescale.or(timescale);

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

    let initialization = if let Some(template) = initialization_template {
        Some(DashInitialization::Template(template))
    } else if let Some(element) = initialization_element {
        let range = element
            .range
            .as_deref()
            .map(parse_byte_range)
            .transpose()?;
        Some(DashInitialization::Resource {
            url: element.sourceURL,
            range,
        })
    } else {
        None
    };

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

    Ok(DashTemplate {
        media,
        initialization,
        start_number: start_number.unwrap_or(1),
        end_number,
        timescale,
        duration_ticks,
        timeline,
        period_duration,
    })
}

fn effective_list(
    levels: &[Option<&dash_mpd_core::SegmentList>],
) -> Result<DashList, DashError> {
    let mut initialization = None;
    let mut segment_urls = None;
    let mut segment_list_seen = false;

    for list in levels.iter().flatten() {
        segment_list_seen = true;
        if list.indexRange.is_some()
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
                range: element
                    .range
                    .as_deref()
                    .map(parse_byte_range)
                    .transpose()?,
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

    let segments = segment_urls
        .iter()
        .map(|segment| {
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
            Ok(DashListSegment {
                media: media.to_owned(),
                range,
            })
        })
        .collect::<Result<Vec<_>, DashError>>()?;

    Ok(DashList {
        initialization,
        segments: segments.into_boxed_slice(),
    })
}

/// Parse DASH's inclusive first-last byte-range syntax.
fn parse_byte_range(raw: &str) -> Result<ManifestByteRange, DashError> {
    let (first, last) = raw.split_once('-').ok_or(DashError::InvalidSegment)?;
    let offset = first.parse::<u64>().map_err(|_| DashError::InvalidSegment)?;
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

fn expand_template(
    representation: &DashRepresentation,
    media_template: &str,
    template: &DashTemplate,
) -> Result<Vec<DashResource>, DashError> {
    let segments = expand_segment_times(template)?;
    if segments.is_empty() || segments.len() > MAX_SEGMENTS {
        return Err(if segments.len() > MAX_SEGMENTS {
            DashError::LimitExceeded
        } else {
            DashError::InvalidSegment
        });
    }

    let mut resources = Vec::with_capacity(
        segments.len() + usize::from(template.initialization.is_some()),
    );

    if let Some(initialization) = &template.initialization {
        let (url, range) = match initialization {
            DashInitialization::Template(uri_template) => {
                let uri = substitute_template(
                    uri_template,
                    representation,
                    template.start_number,
                    0,
                )?;
                (resolve_resource(&representation.source.base_url, &uri)?, None)
            }
            DashInitialization::Resource { url, range } => {
                let resolved = match url.as_deref() {
                    Some(uri) => resolve_resource(&representation.source.base_url, uri)?,
                    None => representation.source.base_url.clone(),
                };
                (resolved, *range)
            }
        };
        resources.push(DashResource {
            url,
            range,
            filename: "init.bin".to_owned(),
            kind: DashResourceKind::Initialization,
        });
    }

    for (index, (number, time)) in segments.into_iter().enumerate() {
        let uri = substitute_template(
            media_template,
            representation,
            number,
            time,
        )?;
        resources.push(DashResource {
            url: resolve_resource(&representation.source.base_url, &uri)?,
            range: None,
            filename: format!("segment-{index:06}.bin"),
            kind: DashResourceKind::Segment,
        });
    }

    Ok(resources)
}

fn expand_segment_times(template: &DashTemplate) -> Result<Vec<(u64, u64)>, DashError> {
    let mut output = Vec::new();
    let mut number = template.start_number;

    if let Some(timeline) = &template.timeline {
        let period_end = duration_to_ticks_ceil(template.period_duration, template.timescale)?;
        let mut next_start = 0_u64;

        for (index, entry) in timeline.iter().enumerate() {
            let start = entry.start.unwrap_or(next_start);
            if start < next_start {
                return Err(DashError::InvalidSegment);
            }

            let count = match entry.repeat {
                repeat if repeat >= 0 => u64::try_from(repeat)
                    .ok()
                    .and_then(|repeat| repeat.checked_add(1))
                    .ok_or(DashError::InvalidSegment)?,
                -1 => {
                    let next_explicit_start = timeline
                        .iter()
                        .skip(index + 1)
                        .find_map(|next| next.start);
                    let end = next_explicit_start.unwrap_or(period_end);
                    if end <= start {
                        return Err(DashError::InvalidSegment);
                    }
                    if next_explicit_start.is_some() && (end - start) % entry.duration != 0 {
                        return Err(DashError::InvalidSegment);
                    }
                    (end - start)
                        .checked_add(entry.duration - 1)
                        .ok_or(DashError::InvalidSegment)?
                        / entry.duration
                }
                _ => return Err(DashError::InvalidSegment),
            };

            for repeat in 0..count {
                if template.end_number.is_some_and(|end| number > end) {
                    return Ok(output);
                }
                let time = repeat
                    .checked_mul(entry.duration)
                    .and_then(|offset| start.checked_add(offset))
                    .ok_or(DashError::InvalidSegment)?;
                output.push((number, time));
                if output.len() > MAX_SEGMENTS {
                    return Err(DashError::LimitExceeded);
                }
                number = number.checked_add(1).ok_or(DashError::InvalidSegment)?;
            }

            let span = count
                .checked_mul(entry.duration)
                .ok_or(DashError::InvalidSegment)?;
            next_start = start.checked_add(span).ok_or(DashError::InvalidSegment)?;
        }

        return Ok(output);
    }

    let duration_ticks = template.duration_ticks.ok_or(DashError::UnsupportedAddressing)?;
    let numerator = template
        .period_duration
        .as_nanos()
        .checked_mul(u128::from(template.timescale))
        .ok_or(DashError::InvalidSegment)?;
    let denominator = u128::from(duration_ticks)
        .checked_mul(1_000_000_000)
        .ok_or(DashError::InvalidSegment)?;
    let count = numerator
        .checked_add(denominator - 1)
        .ok_or(DashError::InvalidSegment)?
        / denominator;
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
            .ok_or(DashError::InvalidSegment)?;
        output.push((number, time));
        number = number.checked_add(1).ok_or(DashError::InvalidSegment)?;
    }

    Ok(output)
}

fn duration_to_ticks_ceil(duration: Duration, timescale: u64) -> Result<u64, DashError> {
    let numerator = duration
        .as_nanos()
        .checked_mul(u128::from(timescale))
        .ok_or(DashError::InvalidSegment)?;
    let ticks = numerator
        .checked_add(999_999_999)
        .ok_or(DashError::InvalidSegment)?
        / 1_000_000_000;
    u64::try_from(ticks).map_err(|_| DashError::InvalidSegment)
}

fn substitute_template(
    template: &str,
    representation: &DashRepresentation,
    number: u64,
    time: u64,
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
            "Number" => format_number(number, width),
            "Time" => format_number(time, width),
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
    validate_https_url(resolved)
}

fn expand_list(base_url: &Url, list: &DashList) -> Result<Vec<DashResource>, DashError> {
    let mut resources = Vec::with_capacity(
        list.segments.len() + usize::from(list.initialization.is_some()),
    );

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
        resources.push(DashResource {
            url,
            range,
            filename: "init.bin".to_owned(),
            kind: DashResourceKind::Initialization,
        });
    }

    for (index, segment) in list.segments.iter().enumerate() {
        resources.push(DashResource {
            url: resolve_resource(base_url, &segment.media)?,
            range: segment.range,
            filename: format!("segment-{index:06}.bin"),
            kind: DashResourceKind::Segment,
        });
    }

    Ok(resources)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE_URL: &str = "https://cdn.example.test/path/manifest.mpd";

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
    fn parses_video_and_audio_representations_and_selects_main_audio() {
        let plan = parse_dash_manifest(BASE_URL, TEMPLATE_MPD)
            .expect("static single-period MPD should parse");

        assert_eq!(plan.duration, Duration::from_secs(10));
        assert_eq!(plan.video_representations.len(), 2);
        assert_eq!(plan.video_representations[0].representation_id.as_deref(), Some("v1"));
        assert_eq!(plan.video_representations[0].height, Some(720));
        assert_eq!(plan.video_representations[1].height, Some(1080));
        assert_eq!(plan.audio_representations.len(), 1);
        assert_eq!(
            plan.default_audio().and_then(|audio| audio.representation_id.as_deref()),
            Some("a1")
        );
    }

    #[test]
    fn expands_duration_template_with_initialization_and_padded_numbers() {
        let plan = parse_dash_manifest(BASE_URL, TEMPLATE_MPD)
            .expect("valid template MPD should parse");
        let video = &plan.video_representations[0];
        let resources = plan_dash_representation(&plan, &video.key)
            .expect("selected representation should expand");

        assert_eq!(resources.resources.len(), 4);
        assert_eq!(resources.resources[0].kind, DashResourceKind::Initialization);
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
        let plan = parse_dash_manifest(BASE_URL, body)
            .expect("timeline MPD should parse");
        let video = &plan.video_representations[0];
        let resources = plan_dash_representation(&plan, &video.key)
            .expect("timeline should expand");

        let urls = resources.resources.iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .map(|resource| resource.url.path().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            urls,
            [
                "/seg-10-0.m4s",
                "/seg-11-2.m4s",
                "/seg-12-4.m4s",
                "/seg-13-7.m4s"
            ]
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
        let plan = parse_dash_manifest(BASE_URL, body)
            .expect("SegmentList MPD should parse");
        let video = &plan.video_representations[0];
        let resources = plan_dash_representation(&plan, &video.key)
            .expect("SegmentList should expand");

        assert_eq!(resources.resources.len(), 3);
        assert_eq!(
            resources.resources[0].range,
            Some(ManifestByteRange { offset: 0, length: 1_000 })
        );
        assert_eq!(
            resources.resources[1].range,
            Some(ManifestByteRange { offset: 1_000, length: 1_000 })
        );
        assert_eq!(
            resources.resources[2].range,
            Some(ManifestByteRange { offset: 2_000, length: 1_000 })
        );
    }

    #[test]
    fn rejects_dynamic_multi_period_drm_xlink_segment_base_webm_and_nonzero_offset() {
        let rejected = [
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic"><Period/></MPD>"#,
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT1S"/><Period duration="PT1S"/></MPD>"#,
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><ContentProtection schemeIdUri="urn:uuid:00000000-0000-0000-0000-000000000000"/><Period duration="PT2S"/></MPD>"#,
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" xmlns:xlink="http://www.w3.org/1999/xlink" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S" xlink:href="external.xml"/></MPD>"#,
            r#"<MPD xmlns="urn:mpeg:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S"><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentBase indexRange="0-10"/><Representation id="v1"/></AdaptationSet></Period></MPD>"#,
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S"><AdaptationSet contentType="video" mimeType="video/webm"><SegmentList><SegmentURL media="v.webm"/></SegmentList><Representation id="v1"/></AdaptationSet></Period></MPD>"#,
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S"><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentTemplate media="s-$Number$.m4s" duration="2" presentationTimeOffset="1"/><Representation id="v1"/></AdaptationSet></Period></MPD>"#,
        ];

        for body in rejected {
            assert!(
                parse_dash_manifest(BASE_URL, body).is_err(),
                "unsupported MPD should not enter native path: {body}"
            );
        }
    }

    #[test]
    fn rejects_unknown_templates_and_invalid_byte_ranges() {
        let unknown = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S"><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentTemplate media="$Unknown$.m4s" duration="2"/><Representation id="v1"/></AdaptationSet></Period></MPD>"#;
        let invalid_range = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S"><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentList><SegmentURL media="v.mp4" mediaRange="9-2"/></SegmentList><Representation id="v1"/></AdaptationSet></Period></MPD>"#;

        assert_eq!(
            parse_dash_manifest(BASE_URL, unknown)
                .and_then(|plan| plan_dash_representation(&plan, &plan.video_representations[0].key))
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
}
