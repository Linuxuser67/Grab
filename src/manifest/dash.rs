//! Static, single-period DASH parsing and segment resource planning.

use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;

use super::{MAX_MANIFEST_BYTES, ManifestByteRange};
const MAX_REPRESENTATIONS: usize = 1_024;
const MAX_SEGMENTS: usize = 100_000;
const MIN_DYNAMIC_UPDATE_PERIOD: Duration = Duration::from_millis(500);
const MAX_DYNAMIC_UPDATE_PERIOD: Duration = Duration::from_secs(300);
const DASH_ROLE_SCHEME: &str = "urn:mpeg:dash:role:2011";
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
    #[error("dynamic DASH has no currently available complete media segments")]
    NoAvailableSegments,
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
    /// Stable live key also recognized when a dynamic MPD becomes static.
    pub(crate) live_key: String,
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

/// Timing model shared by all representations in a single-period DASH presentation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DashPresentation {
    Static,
    Dynamic(DashDynamicPresentation),
}

/// Server-clock information required to bound and refresh an ordinary live MPD.
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

/// Expanded initialization and media resources for one representation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DashRepresentationPlan {
    pub(crate) representation: DashRepresentation,
    pub(crate) duration: Duration,
    pub(crate) presentation: DashPresentation,
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

/// Parse a static or bounded single-period dynamic DASH MPD into selectable representations.
///
/// Dynamic support is limited to HTTPS SegmentTemplate presentations with a
/// finite MPD update interval and complete segments. Multiple periods, DRM,
/// XLink, dynamic SegmentList, LL-DASH, SegmentBase, indexed addressing,
/// non-zero presentation-time offsets, and WebM remain on yt-dlp fallback.
///
/// # Errors
/// Returns an error when the MPD is malformed, exceeds the input limit, uses
/// unsupported features, or contains no supported video representation.
pub(crate) fn parse_dash_manifest(
    manifest_url: &str,
    body: &str,
) -> Result<DashManifestPlan, DashError> {
    parse_dash_manifest_at(manifest_url, body, SystemTime::now())
}

/// Parse a DASH MPD using the HTTP server's Date header as the availability clock.
pub(crate) fn parse_dash_manifest_at(
    manifest_url: &str,
    body: &str,
    server_now: SystemTime,
) -> Result<DashManifestPlan, DashError> {
    if body.len() > MAX_MANIFEST_BYTES {
        return Err(DashError::Parse);
    }

    let manifest_url = parse_manifest_url(manifest_url)?;
    let mpd = dash_mpd_core::parse(body).map_err(|_| DashError::Parse)?;

    let presentation = match mpd.mpdtype.as_deref() {
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
                    *period >= MIN_DYNAMIC_UPDATE_PERIOD
                        && *period <= MAX_DYNAMIC_UPDATE_PERIOD
                })
                .ok_or(DashError::UnsupportedPresentation)?;
            let time_shift_buffer_depth = mpd.timeShiftBufferDepth;
            if time_shift_buffer_depth.is_some_and(Duration::is_zero) {
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
        || mpd.periods.len() != 1
        || !mpd.ContentProtection.is_empty()
    {
        return Err(DashError::UnsupportedPresentation);
    }

    let period = &mpd.periods[0];
    if period.href.is_some() || !period.ContentProtection.is_empty() {
        return Err(DashError::UnsupportedPresentation);
    }

    let period_start = period.start.unwrap_or(Duration::ZERO);
    let duration = period
        .duration
        .or_else(|| {
            mpd.mediaPresentationDuration.and_then(|whole_duration| {
                whole_duration.checked_sub(period_start)
            })
        })
        .filter(|duration| !duration.is_zero())
        .unwrap_or(Duration::ZERO);
    if !is_dynamic && duration.is_zero() {
        return Err(DashError::UnsupportedPresentation);
    }
    let presentation = match presentation {
        DashPresentation::Dynamic(mut dynamic) => {
            dynamic.period_start = period_start;
            DashPresentation::Dynamic(dynamic)
        }
        DashPresentation::Static => DashPresentation::Static,
    };
    let addressing_duration = if is_dynamic {
        Duration::MAX
    } else {
        duration
    };

    let mpd_base = resolve_base_url(manifest_url, &mpd.base_url)?;
    let period_base = resolve_base_url(mpd_base, &period.BaseURL)?;

    let mut video_representations = Vec::new();
    let mut audio_representations = Vec::new();

    for adaptation in &period.adaptations {
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
        for representation in &adaptation.representations {
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
            );
            let Some(kind) = kind else {
                // Text, metadata, and unknown adaptation sets are not media inputs.
                continue;
            };
            // dash-mpd-core 0.1.0 exposes Role only on AdaptationSet, not
            // Representation. A spec-valid Representation-level Role=main
            // is therefore ignored; default_audio() falls back to the first
            // audio representation unless its AdaptationSet declares main.
            let is_default_audio = kind == DashTrackKind::Audio
                && adaptation.Role.iter().any(|role| {
                    role.schemeIdUri == DASH_ROLE_SCHEME && role.value.as_deref() == Some("main")
                });

            let representation_base =
                resolve_base_url(adaptation_base.clone(), &representation.BaseURL)?;
            let addressing = effective_addressing(
                period,
                adaptation,
                representation,
                addressing_duration,
                is_dynamic,
            )?;
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
            let key = if is_dynamic {
                live_key.clone()
            } else {
                stable_representation_key(identity)
            };

            let choice = DashRepresentation {
                key,
                live_key,
                representation_id,
                bandwidth,
                width,
                height,
                mime_type,
                codecs,
                language,
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
        presentation,
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
        .find(|candidate| {
            candidate.key == representation_key || candidate.live_key == representation_key
        })
        .cloned()
        .ok_or(DashError::UnsupportedAddressing)?;

    let live_selection = representation_key == representation.live_key;
    let resources = match (&manifest.presentation, &representation.source.addressing) {
        (DashPresentation::Dynamic(dynamic), DashAddressing::Template(template)) => {
            expand_dynamic_template(&representation, &template.media, template, dynamic)?
        }
        (DashPresentation::Dynamic(_), DashAddressing::List(_)) => {
            return Err(DashError::UnsupportedAddressing);
        }
        (DashPresentation::Static, DashAddressing::Template(template)) if live_selection => {
            expand_static_live_template(&representation, &template.media, template)?
        }
        (DashPresentation::Static, DashAddressing::Template(template)) => {
            expand_template(&representation, &template.media, template)?
        }
        (DashPresentation::Static, DashAddressing::List(_)) if live_selection => {
            return Err(DashError::UnsupportedAddressing);
        }
        (DashPresentation::Static, DashAddressing::List(list)) => {
            expand_list(&representation, list)?
        }
    };

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
    if let DashAddressing::Template(template) = addressing {
        hasher.update(b"template");
        hasher.update(template.media.as_bytes());
        hash_optional_number(&mut hasher, Some(template.timescale));
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
    let digest = hasher.finalize();
    let fingerprint: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("dash-live-{fingerprint}")
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
) -> Option<DashTrackKind> {
    let content_type = representation_content_type.or(adaptation_content_type);
    let mime_type = representation_mime_type.or(adaptation_mime_type);

    if content_type == Some("video") || mime_type.is_some_and(|mime| mime.starts_with("video/")) {
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
    is_dynamic: bool,
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
    if is_dynamic && (!has_template || has_list) {
        return Err(DashError::UnsupportedAddressing);
    }
    let addressing = match (has_template, has_list) {
        (true, false) => {
            effective_template(&templates, period_duration).map(DashAddressing::Template)?
        }
        (false, true) => effective_list(&lists).map(DashAddressing::List)?,
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
            || template
                .presentationTimeOffset
                .is_some_and(|offset| offset != 0)
            || template
                .availabilityTimeOffset
                .is_some_and(|offset| offset != 0.0)
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

fn effective_list(levels: &[Option<&dash_mpd_core::SegmentList>]) -> Result<DashList, DashError> {
    let mut initialization = None;
    let mut segment_urls = None;
    let mut segment_list_seen = false;

    for list in levels.iter().flatten() {
        segment_list_seen = true;
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
    let elapsed_ticks = duration_to_ticks(elapsed, template.timescale)?;
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
) -> Result<Vec<DashResource>, DashError> {
    let mut resources = expand_template(representation, media_template, template)?;
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
    let seconds = u64::try_from(nanos / 1_000_000_000)
        .map_err(|_| DashError::LimitExceeded)?;
    let subsecond_nanos = u32::try_from(nanos % 1_000_000_000)
        .map_err(|_| DashError::InvalidSegment)?;
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
        for (index, entry) in timeline.iter().enumerate() {
            let start = entry.start.unwrap_or(next_start);
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
        }
    } else {
        let duration = template
            .duration_ticks
            .filter(|duration| *duration > 0)
            .ok_or(DashError::UnsupportedAddressing)?;
        let complete_count = elapsed_ticks / duration;
        let bounded_count = template
            .end_number
            .map_or(complete_count, |end| {
                complete_count.min(end.saturating_sub(number).saturating_add(1))
            });
        let first_available = (window_start_ticks / duration).min(bounded_count);
        let first_index = if bounded_count == 0 {
            0
        } else {
            first_available.min(bounded_count.saturating_sub(1))
        };
        let start_index = if bounded_count == 0 {
            0
        } else {
            first_index
        };
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
) -> Result<Vec<DashResource>, DashError> {
    let segments = expand_segment_times(template)?;
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

    for (index, (number, time)) in segments.into_iter().enumerate() {
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

fn expand_segment_times(template: &DashTemplate) -> Result<Vec<(u64, u64)>, DashError> {
    let mut output = Vec::new();
    let mut number = template.start_number;

    if let Some(timeline) = &template.timeline {
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
                let time = repeat
                    .checked_mul(entry.duration)
                    .and_then(|offset| start.checked_add(offset))
                    .ok_or(DashError::InvalidSegment)?;
                if !time_precedes_period_end(time, template.period_duration, template.timescale)? {
                    return Err(DashError::InvalidSegment);
                }
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
            .ok_or(DashError::InvalidSegment)?;
        output.push((number, time));
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

fn segments_until_period_end(
    period_duration: Duration,
    timescale: u64,
    segment_start: u64,
    segment_duration: u64,
) -> Result<u64, DashError> {
    if segment_duration == 0 {
        return Err(DashError::InvalidSegment);
    }
    let period_end_scaled = period_duration
        .as_nanos()
        .checked_mul(u128::from(timescale))
        .ok_or(DashError::InvalidSegment)?;
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
) -> Result<bool, DashError> {
    let period_end_scaled = period_duration
        .as_nanos()
        .checked_mul(u128::from(timescale))
        .ok_or(DashError::InvalidSegment)?;
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
    if resolved.path().to_ascii_lowercase().ends_with(".webm") {
        return Err(DashError::UnsupportedPresentation);
    }
    Ok(resolved)
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
    fn representation_level_main_role_falls_back_to_first_audio_track() {
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
            .expect("Representation-level Role is ignored by dash-mpd-core 0.1.0");

        assert_eq!(plan.audio_representations.len(), 2);
        assert!(
            plan.audio_representations
                .iter()
                .all(|track| !track.is_default_audio)
        );
        assert_eq!(
            plan.default_audio()
                .and_then(|track| track.representation_id.as_deref()),
            Some("first"),
            "without an AdaptationSet Role=main, the first audio track is the fallback"
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
        assert!(matches!(manifest.presentation, DashPresentation::Dynamic(_)));

        let plan = plan_dash_representation(&manifest, &video.key)
            .expect("expand current complete live segments");
        let numbers = plan
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .map(|resource| resource.segment_number.expect("media segment number"))
            .collect::<Vec<_>>();
        assert_eq!(numbers, [26, 27, 28, 29, 30]);
        assert_eq!(
            plan.resources[0].kind,
            DashResourceKind::Initialization
        );
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
        let finalized_plan =
            plan_dash_representation(&finalized, &live_representation.live_key)
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
            dynamic_resource.filename,
            final_resource.filename,
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
        let plan = plan_dash_representation(
            &manifest,
            &manifest.video_representations[0].live_key,
        )
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
    fn rejects_dynamic_multi_period_drm_xlink_segment_base_webm_and_nonzero_offset() {
        let rejected = [
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic"><Period/></MPD>"#,
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT1S"/><Period duration="PT1S"/></MPD>"#,
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><ContentProtection schemeIdUri="urn:uuid:00000000-0000-0000-0000-000000000000"/><Period duration="PT2S"/></MPD>"#,
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" xmlns:xlink="http://www.w3.org/1999/xlink" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S" xlink:href="external.xml"/></MPD>"#,
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S"><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentBase indexRange="0-10"/><Representation id="v1"/></AdaptationSet></Period></MPD>"#,
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
}
