//! Pure manifest parsing and resource planning for native HLS and DASH downloads.
//!
//! The parser layer owns no network or filesystem access. It turns a manifest
//! body into validated remote resources and a local playlist description.

use std::collections::HashMap;
use std::time::Duration;

use m3u8_rs::{AlternativeMediaType, KeyMethod, MediaPlaylist, MediaPlaylistType, Playlist};
use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;

/// Maximum response body accepted by the native manifest intake.
pub(crate) const MAX_MANIFEST_BYTES: usize = 4 * 1024 * 1024;

pub(crate) mod dash;

/// Errors that make a manifest unsuitable for Grab's native download path.
#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum ManifestError {
    #[error("invalid manifest url")]
    InvalidManifestUrl,
    #[error("could not parse HLS playlist")]
    Parse,
    #[error("expected an HLS master playlist")]
    ExpectedMasterPlaylist,
    #[error("HLS media playlist contains no segments")]
    EmptyPlaylist,
    #[error("HLS master playlist contains no video variants")]
    NoVideoVariants,
    #[error("HLS variant references an unknown audio group")]
    InvalidAudioGroup,
    #[error("unsupported HLS playlist type")]
    UnsupportedPlaylist,
    #[error("unsupported HLS encryption method")]
    UnsupportedEncryption,
    #[error("unsupported HLS resource url")]
    UnsupportedResourceUrl,
    #[error("HLS byte range has no usable offset")]
    MissingByteRangeOffset,
    #[error("invalid or overflowing HLS byte range")]
    InvalidByteRange,
    #[error("invalid HLS segment duration")]
    InvalidDuration,
    #[error("HLS presentation duration is too large")]
    DurationOverflow,
    #[error("could not serialize the local HLS playlist")]
    Serialize,
}

/// A checked byte range in a remote HLS resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ManifestByteRange {
    pub(crate) offset: u64,
    pub(crate) length: u64,
}

/// Generate a stable staging filename for a manifest resource and optional byte range.
#[must_use]
pub(crate) fn resource_filename(
    prefix: &str,
    index: Option<usize>,
    url: &Url,
    range: Option<ManifestByteRange>,
) -> String {
    resource_filename_inner(prefix, index, url, range, false)
}

/// Generate a staging filename that preserves all query parameters.
///
/// HLS encryption keys use this form because changed query values can select new key bytes.
#[must_use]
pub(crate) fn resource_filename_exact_url(
    prefix: &str,
    index: Option<usize>,
    url: &Url,
    range: Option<ManifestByteRange>,
) -> String {
    resource_filename_inner(prefix, index, url, range, true)
}

fn resource_filename_inner(
    prefix: &str,
    index: Option<usize>,
    url: &Url,
    range: Option<ManifestByteRange>,
    preserve_query: bool,
) -> String {
    let stable_url = resource_url_identity(url, preserve_query);
    let mut hasher = Sha256::new();
    hasher.update(stable_url.as_str().as_bytes());
    if let Some(range) = range {
        hasher.update(range.offset.to_le_bytes());
        hasher.update(range.length.to_le_bytes());
    }
    let digest = hasher.finalize();
    let fingerprint: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    match index {
        Some(index) => format!("{prefix}-{index:06}-{fingerprint}.bin"),
        None => format!("{prefix}-{fingerprint}.bin"),
    }
}

/// Return a resource URL identity without known expiring-signature parameters.
#[must_use]
pub(crate) fn resource_cache_key(url: &Url) -> String {
    resource_url_identity(url, false).to_string()
}

fn resource_url_identity(url: &Url, preserve_query: bool) -> Url {
    let mut stable_url = url.clone();
    stable_url.set_fragment(None);
    if preserve_query {
        return stable_url;
    }

    let query_pairs: Vec<(String, String)> = stable_url
        .query_pairs()
        .filter(|(name, _)| !is_rotating_signature_parameter(name))
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();
    stable_url.set_query(None);
    if !query_pairs.is_empty() {
        stable_url.query_pairs_mut().extend_pairs(query_pairs);
    }
    stable_url
}

fn is_rotating_signature_parameter(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "sig"
            | "signature"
            | "expires"
            | "expiry"
            | "policy"
            | "key-pair-id"
            | "hmac"
            | "hdnea"
            | "hdnts"
            | "awsaccesskeyid"
            | "googleaccessid"
            | "x-amz-algorithm"
            | "x-amz-credential"
            | "x-amz-date"
            | "x-amz-expires"
            | "x-amz-security-token"
            | "x-amz-signature"
            | "x-goog-algorithm"
            | "x-goog-credential"
            | "x-goog-date"
            | "x-goog-expires"
            | "x-goog-signature"
    )
}

/// The kind of remote object fetched for a local HLS presentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HlsResourceKind {
    InitMap,
    Segment,
    /// AES-128 identity key localized beside the rewritten media playlist.
    Key,
}

/// One HTTPS resource required by the selected HLS media playlist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HlsResource {
    pub(crate) url: Url,
    pub(crate) range: Option<ManifestByteRange>,
    pub(crate) filename: String,
    pub(crate) kind: HlsResourceKind,
}

/// A video variant advertised by an HLS master playlist.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HlsVariant {
    pub(crate) url: Url,
    pub(crate) bandwidth: u64,
    pub(crate) average_bandwidth: Option<u64>,
    pub(crate) width: Option<u64>,
    pub(crate) height: Option<u64>,
    pub(crate) codecs: Option<String>,
    pub(crate) frame_rate: Option<f64>,
    pub(crate) audio_group: Option<String>,
}

/// An alternate audio rendition advertised by an HLS master playlist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HlsAudioRendition {
    pub(crate) url: Option<Url>,
    pub(crate) group_id: String,
    pub(crate) name: String,
    pub(crate) language: Option<String>,
    pub(crate) is_default: bool,
}

/// Master-playlist choices in manifest order; no variant is silently selected.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HlsMasterPlan {
    pub(crate) variants: Box<[HlsVariant]>,
    pub(crate) audio_renditions: Box<[HlsAudioRendition]>,
}

/// Validated media playlist, ordered resources, and local FFmpeg input.
#[derive(Clone, Debug)]
pub(crate) struct HlsPlan {
    pub(crate) resources: Box<[HlsResource]>,
    pub(crate) duration: Duration,
    local_playlist: MediaPlaylist,
}

impl HlsPlan {
    /// Serialize the rewritten VOD playlist consumed by FFmpeg after fetch.
    ///
    /// # Errors
    /// Returns a serialization error if the playlist writer fails.
    pub(crate) fn local_playlist_bytes(&self) -> Result<Vec<u8>, ManifestError> {
        let mut output = Vec::new();
        self.local_playlist
            .write_to(&mut output)
            .map_err(|_| ManifestError::Serialize)?;
        Ok(output)
    }
}

/// Parse an HLS master playlist into user-selectable video and audio choices.
///
/// Variant order is preserved. I-frame-only entries are excluded because they
/// are trick-play indexes, not independently downloadable video presentations.
///
/// # Errors
/// Returns an error for malformed playlists, unsupported session encryption,
/// or resource URLs that are not HTTPS.
pub(crate) fn parse_hls_master_playlist(
    manifest_url: &str,
    body: &[u8],
) -> Result<HlsMasterPlan, ManifestError> {
    if body.len() > MAX_MANIFEST_BYTES {
        return Err(ManifestError::Parse);
    }
    let base_url = parse_manifest_url(manifest_url)?;
    let playlist = m3u8_rs::parse_playlist_res(body).map_err(|_| ManifestError::Parse)?;

    let Playlist::MasterPlaylist(master) = playlist else {
        return Err(ManifestError::ExpectedMasterPlaylist);
    };

    // Session keys and unimplemented URI-affecting extensions need the
    // established extractor path; silently ignoring them can select wrong media.
    if !master.session_key.is_empty() {
        return Err(ManifestError::UnsupportedEncryption);
    }
    if master
        .unknown_tags
        .iter()
        .any(|tag| is_unsupported_hls_tag(&tag.tag))
    {
        return Err(ManifestError::UnsupportedPlaylist);
    }

    let variants = master
        .variants
        .into_iter()
        .filter(|variant| !variant.is_i_frame)
        .map(|variant| {
            if variant.uri.is_empty() {
                return Err(ManifestError::UnsupportedPlaylist);
            }
            let url = resolve_resource_url(&base_url, &variant.uri)?;
            let (width, height) = variant
                .resolution
                .map(|resolution| (Some(resolution.width), Some(resolution.height)))
                .unwrap_or((None, None));
            Ok(HlsVariant {
                url,
                bandwidth: variant.bandwidth,
                average_bandwidth: variant.average_bandwidth,
                width,
                height,
                codecs: variant.codecs,
                frame_rate: variant.frame_rate,
                audio_group: variant.audio,
            })
        })
        .collect::<Result<Vec<_>, ManifestError>>()?;

    if variants.is_empty() {
        return Err(ManifestError::NoVideoVariants);
    }

    let audio_renditions = master
        .alternatives
        .into_iter()
        .filter(|alternative| alternative.media_type == AlternativeMediaType::Audio)
        .map(|alternative| {
            let url = alternative
                .uri
                .as_deref()
                .map(|uri| resolve_resource_url(&base_url, uri))
                .transpose()?;
            Ok(HlsAudioRendition {
                url,
                group_id: alternative.group_id,
                name: alternative.name,
                language: alternative.language,
                is_default: alternative.default,
            })
        })
        .collect::<Result<Vec<_>, ManifestError>>()?;

    let has_unmatched_audio_group = variants.iter().any(|variant| {
        variant.audio_group.as_deref().is_some_and(|group_id| {
            !audio_renditions
                .iter()
                .any(|rendition| rendition.group_id == group_id)
        })
    });
    if has_unmatched_audio_group {
        return Err(ManifestError::InvalidAudioGroup);
    }

    Ok(HlsMasterPlan {
        variants: variants.into_boxed_slice(),
        audio_renditions: audio_renditions.into_boxed_slice(),
    })
}

/// Parse a selected HLS media playlist and construct its deterministic fetch plan.
///
/// Byte ranges are converted to explicit offsets, init maps are fetched once per
/// distinct URI/range pair, and every media segment gets its own local filename.
/// AES-128 identity keys are included in the local fetch plan and the
/// rewritten playlist points FFmpeg at the staged key file; other encryption
/// methods are deliberately rejected.
///
/// # Errors
/// Returns an error for live or unsupported playlists, invalid URLs or ranges,
/// unsupported encryption, invalid durations, or an empty media playlist.
pub(crate) fn plan_hls_media_playlist(
    manifest_url: &str,
    body: &[u8],
) -> Result<HlsPlan, ManifestError> {
    if body.len() > MAX_MANIFEST_BYTES {
        return Err(ManifestError::Parse);
    }
    let base_url = parse_manifest_url(manifest_url)?;
    let playlist = m3u8_rs::parse_media_playlist_res(body).map_err(|_| ManifestError::Parse)?;
    build_hls_plan(base_url, playlist)
}

fn build_hls_plan(base_url: Url, mut playlist: MediaPlaylist) -> Result<HlsPlan, ManifestError> {
    if !playlist.end_list || playlist.i_frames_only {
        return Err(ManifestError::UnsupportedPlaylist);
    }

    match playlist.playlist_type.as_ref() {
        Some(MediaPlaylistType::Event) | Some(MediaPlaylistType::Other(_)) => {
            return Err(ManifestError::UnsupportedPlaylist);
        }
        Some(MediaPlaylistType::Vod) | None => {}
    }

    if playlist.segments.is_empty() {
        return Err(ManifestError::EmptyPlaylist);
    }
    if playlist
        .unknown_tags
        .iter()
        .any(|tag| is_unsupported_hls_tag(&tag.tag))
    {
        return Err(ManifestError::UnsupportedPlaylist);
    }

    let mut resources = Vec::with_capacity(playlist.segments.len());
    let mut map_filenames: HashMap<(String, Option<ManifestByteRange>), String> = HashMap::new();
    let mut key_filenames: HashMap<String, String> = HashMap::new();
    // An implicit segment offset is defined only by the immediately preceding
    // media segment, and only when it was a sub-range of the same resource.
    let mut previous_segment_range: Option<(String, u64)> = None;
    // m3u8-rs resets these fields after each segment URI. HLS keeps each tag
    // active until the next tag, so retain the source values separately from
    // the rewritten per-segment copies below.
    let mut active_key = None;
    let mut active_map = None;
    let mut total_duration = Duration::ZERO;
    let mut maximum_segment_duration = 0.0_f64;

    for (index, segment) in playlist.segments.iter_mut().enumerate() {
        if segment.unknown_tags.iter().any(|tag| tag.tag == "X-KEY") {
            // A key tag the parser could not decode must never pass through
            // into FFmpeg, where it could bypass our encryption policy.
            return Err(ManifestError::UnsupportedEncryption);
        }
        if segment
            .unknown_tags
            .iter()
            .any(|tag| is_unsupported_hls_tag(&tag.tag))
        {
            return Err(ManifestError::UnsupportedPlaylist);
        }

        if let Some(key) = segment.key.as_ref() {
            active_key = Some(key.clone());
        }
        if let Some(map) = segment.map.as_ref() {
            active_map = Some(map.clone());
        }
        segment.key = active_key.clone();
        segment.map = active_map.clone();

        let segment_duration = f64::from(segment.duration);
        if !segment_duration.is_finite() || segment_duration <= 0.0 {
            return Err(ManifestError::InvalidDuration);
        }
        let duration = Duration::try_from_secs_f64(segment_duration)
            .map_err(|_| ManifestError::InvalidDuration)?;
        total_duration = total_duration
            .checked_add(duration)
            .ok_or(ManifestError::DurationOverflow)?;
        maximum_segment_duration = maximum_segment_duration.max(segment_duration);

        if let Some(map) = segment.map.as_mut() {
            let map_url = resolve_resource_url(&base_url, &map.uri)?;
            // EXT-X-MAP byte ranges do not have a preceding media-segment
            // range to infer from. Require an explicit offset rather than
            // guessing and fetching the wrong initialization bytes.
            let map_range = map
                .byte_range
                .as_ref()
                .map(|range| {
                    let offset = range.offset.ok_or(ManifestError::MissingByteRangeOffset)?;
                    checked_byte_range(range.length, offset)
                })
                .transpose()?;
            let map_key = (resource_cache_key(&map_url), map_range);
            let filename = match map_filenames.get(&map_key) {
                Some(existing) => existing.clone(),
                None => {
                    let filename = resource_filename("init", None, &map_url, map_range);
                    resources.push(HlsResource {
                        url: map_url,
                        range: map_range,
                        filename: filename.clone(),
                        kind: HlsResourceKind::InitMap,
                    });
                    map_filenames.insert(map_key, filename.clone());
                    filename
                }
            };
            map.uri = filename;
            map.byte_range = None;
        }

        if let Some(key) = segment.key.as_mut() {
            if key.method != KeyMethod::AES128 {
                return Err(ManifestError::UnsupportedEncryption);
            }
            // RFC 8216 requires an explicit IV for AES-128-encrypted
            // initialization sections; the media-sequence-derived IV rule
            // applies to media segments, not initialization sections.
            if segment.map.is_some() && key.iv.is_none() {
                return Err(ManifestError::UnsupportedEncryption);
            }
            if key
                .keyformat
                .as_deref()
                .is_some_and(|format| format != "identity")
                || key
                    .keyformatversions
                    .as_deref()
                    .is_some_and(|versions| versions != "1")
            {
                return Err(ManifestError::UnsupportedEncryption);
            }
            let key_uri = key
                .uri
                .as_deref()
                .ok_or(ManifestError::UnsupportedEncryption)?;
            let key_url = resolve_resource_url(&base_url, key_uri)?;
            let key_url_string = key_url.as_str().to_string();
            let filename = match key_filenames.get(&key_url_string) {
                Some(existing) => existing.clone(),
                None => {
                    let filename = resource_filename_exact_url("key", None, &key_url, None);
                    resources.push(HlsResource {
                        url: key_url,
                        range: None,
                        filename: filename.clone(),
                        kind: HlsResourceKind::Key,
                    });
                    key_filenames.insert(key_url_string, filename.clone());
                    filename
                }
            };
            key.uri = Some(filename);
        }

        let segment_url = resolve_resource_url(&base_url, &segment.uri)?;
        let range = match segment.byte_range.as_ref() {
            Some(byte_range) => {
                let resource_url = resource_cache_key(&segment_url);
                let offset = match byte_range.offset {
                    Some(offset) => offset,
                    None => previous_segment_range
                        .as_ref()
                        .filter(|(previous_url, _)| previous_url == &resource_url)
                        .map(|(_, next_offset)| *next_offset)
                        .ok_or(ManifestError::MissingByteRangeOffset)?,
                };
                let range = checked_byte_range(byte_range.length, offset)?;
                let next_offset = offset
                    .checked_add(byte_range.length)
                    .ok_or(ManifestError::InvalidByteRange)?;
                previous_segment_range = Some((resource_url, next_offset));
                Some(range)
            }
            None => {
                // A full-resource segment breaks implicit range continuation.
                previous_segment_range = None;
                None
            }
        };
        let filename = resource_filename("segment", Some(index), &segment_url, range);
        resources.push(HlsResource {
            url: segment_url,
            range,
            filename: filename.clone(),
            kind: HlsResourceKind::Segment,
        });
        segment.uri = filename;
        segment.byte_range = None;
    }

    let rounded_target_duration = maximum_segment_duration.ceil();
    if rounded_target_duration > u64::MAX as f64 {
        return Err(ManifestError::DurationOverflow);
    }
    playlist.target_duration = rounded_target_duration as u64;
    let has_initialization_map = playlist
        .segments
        .iter()
        .any(|segment| segment.map.is_some());
    let has_key_format_attributes = playlist.segments.iter().any(|segment| {
        segment
            .key
            .as_ref()
            .is_some_and(|key| key.keyformat.is_some() || key.keyformatversions.is_some())
    });
    playlist.version = Some(if has_initialization_map {
        6
    } else if has_key_format_attributes {
        5
    } else {
        3
    });
    playlist.playlist_type = Some(MediaPlaylistType::Vod);
    playlist.end_list = true;

    Ok(HlsPlan {
        resources: resources.into_boxed_slice(),
        duration: total_duration,
        local_playlist: playlist,
    })
}

fn is_unsupported_hls_tag(tag: &str) -> bool {
    matches!(
        tag,
        "X-GAP"
            | "X-PART"
            | "X-PRELOAD-HINT"
            | "X-SERVER-CONTROL"
            | "X-PART-INF"
            | "X-RENDITION-REPORT"
            | "X-SKIP"
            | "X-KEY"
            | "X-DEFINE"
            | "X-CONTENT-STEERING"
    )
}

fn is_valid_https_resource_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
}

fn parse_manifest_url(raw: &str) -> Result<Url, ManifestError> {
    let url = Url::parse(raw).map_err(|_| ManifestError::InvalidManifestUrl)?;
    if !is_valid_https_resource_url(&url) {
        return Err(ManifestError::InvalidManifestUrl);
    }
    Ok(url)
}

fn resolve_resource_url(base_url: &Url, raw: &str) -> Result<Url, ManifestError> {
    let url = base_url
        .join(raw)
        .map_err(|_| ManifestError::UnsupportedResourceUrl)?;
    if !is_valid_https_resource_url(&url) {
        return Err(ManifestError::UnsupportedResourceUrl);
    }
    Ok(url)
}

fn checked_byte_range(length: u64, offset: u64) -> Result<ManifestByteRange, ManifestError> {
    if length == 0 || offset.checked_add(length).is_none() {
        return Err(ManifestError::InvalidByteRange);
    }

    Ok(ManifestByteRange { offset, length })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE_URL: &str = "https://cdn.example.test/path/master.m3u8";

    #[test]
    fn master_playlist_keeps_variants_in_manifest_order_and_audio_renditions() {
        let body = br#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio",NAME="English",LANGUAGE="en",DEFAULT=YES,URI="audio/en.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=1200000,AVERAGE-BANDWIDTH=1000000,RESOLUTION=1280x720,CODECS="avc1.64001f,mp4a.40.2",AUDIO="audio"
video/720.m3u8
#EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=200000,URI="video/thumbs.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1920x1080
video/1080.m3u8
"#;
        let plan = parse_hls_master_playlist(BASE_URL, body)
            .expect("valid master playlist should produce picker options");

        assert_eq!(plan.variants.len(), 2);
        assert_eq!(
            plan.variants[0].url.as_str(),
            "https://cdn.example.test/path/video/720.m3u8"
        );
        assert_eq!(plan.variants[0].bandwidth, 1_200_000);
        assert_eq!(plan.variants[0].width, Some(1280));
        assert_eq!(plan.variants[0].height, Some(720));
        assert_eq!(plan.variants[0].audio_group.as_deref(), Some("audio"));
        assert_eq!(plan.variants[1].height, Some(1080));
        assert_eq!(plan.audio_renditions.len(), 1);
        assert_eq!(plan.audio_renditions[0].name, "English");
        assert_eq!(
            plan.audio_renditions[0].url.as_ref().map(Url::as_str),
            Some("https://cdn.example.test/path/audio/en.m3u8")
        );
    }

    #[test]
    fn rejects_variants_with_unmatched_audio_groups() {
        let body = br#"#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=1200000,AUDIO="missing"
video/720.m3u8
"#;
        assert_eq!(
            parse_hls_master_playlist(BASE_URL, body).err(),
            Some(ManifestError::InvalidAudioGroup)
        );
    }

    #[test]
    fn local_playlist_round_trips_with_map_and_preserves_sequence_and_target() {
        let body = br#"#EXTM3U
#EXT-X-VERSION:4
#EXT-X-TARGETDURATION:99
#EXT-X-MEDIA-SEQUENCE:338559
#EXT-X-PLAYLIST-TYPE:VOD
#EXT-X-MAP:URI="init.mp4",BYTERANGE="16@0"
#EXTINF:3.2,
#EXT-X-BYTERANGE:4@16
media.mp4
#EXTINF:4,
#EXT-X-BYTERANGE:4
media.mp4
#EXT-X-ENDLIST
"#;
        let plan = plan_hls_media_playlist(BASE_URL, body)
            .expect("valid VOD playlist should produce a local fetch plan");

        assert!((plan.duration.as_secs_f64() - 7.2).abs() < 0.000_001);
        assert_eq!(plan.resources.len(), 3);
        assert_eq!(plan.resources[0].kind, HlsResourceKind::InitMap);
        assert_eq!(
            plan.resources[0].range,
            Some(ManifestByteRange {
                offset: 0,
                length: 16
            })
        );
        assert_eq!(
            plan.resources[1].range,
            Some(ManifestByteRange {
                offset: 16,
                length: 4
            })
        );
        assert_eq!(
            plan.resources[2].range,
            Some(ManifestByteRange {
                offset: 20,
                length: 4
            })
        );

        let bytes = plan
            .local_playlist_bytes()
            .expect("local playlist should serialize");
        let parsed = m3u8_rs::parse_media_playlist_res(&bytes)
            .expect("serialized local playlist should parse");
        assert_eq!(parsed.media_sequence, 338_559);
        assert_eq!(parsed.target_duration, 4);
        assert_eq!(parsed.version, Some(6));
        assert_eq!(parsed.playlist_type, Some(MediaPlaylistType::Vod));
        assert!(parsed.end_list);
        assert_eq!(parsed.segments.len(), 2);
        assert!(
            parsed.segments[0].uri.starts_with("segment-000000-"),
            "local segment names include their remote resource identity"
        );
        assert!(
            parsed.segments[0]
                .map
                .as_ref()
                .is_some_and(|map| map.uri.starts_with("init-"))
        );
        assert!(
            parsed
                .segments
                .iter()
                .all(|segment| segment.byte_range.is_none())
        );
        assert!(
            parsed.segments[0]
                .map
                .as_ref()
                .is_some_and(|map| map.byte_range.is_none())
        );
    }

    #[test]
    fn resource_filenames_change_when_a_segment_url_changes() {
        let first =
            plan_hls_media_playlist(BASE_URL, b"#EXTM3U\n#EXTINF:2,\none.ts\n#EXT-X-ENDLIST\n")
                .expect("first media playlist");
        let second =
            plan_hls_media_playlist(BASE_URL, b"#EXTM3U\n#EXTINF:2,\ntwo.ts\n#EXT-X-ENDLIST\n")
                .expect("second media playlist");
        assert_ne!(
            first.resources[0].filename, second.resources[0].filename,
            "a changed resource URL must not reuse bytes cached under the old segment ordinal"
        );
    }

    #[test]
    fn deduplicates_repeated_initialization_maps() {
        let body = br#"#EXTM3U
#EXT-X-MAP:URI="init.mp4"
#EXTINF:2,
one.m4s
#EXT-X-MAP:URI="init.mp4"
#EXTINF:2,
two.m4s
#EXT-X-MAP:URI="init.mp4"
#EXTINF:2,
three.m4s
#EXT-X-ENDLIST
"#;
        let plan = plan_hls_media_playlist(BASE_URL, body)
            .expect("identical init maps should be shared by local segments");

        assert_eq!(
            plan.resources
                .iter()
                .filter(|resource| resource.kind == HlsResourceKind::InitMap)
                .count(),
            1
        );

        let bytes = plan
            .local_playlist_bytes()
            .expect("local playlist should serialize");
        let parsed =
            m3u8_rs::parse_media_playlist_res(&bytes).expect("serialized playlist should parse");
        let map_names = parsed
            .segments
            .iter()
            .filter_map(|segment| segment.map.as_ref().map(|map| map.uri.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(map_names[0], map_names[1]);
        assert_eq!(map_names[1], map_names[2]);
        assert!(map_names[0].starts_with("init-"));
    }

    #[test]
    fn implicit_byte_range_requires_the_immediately_previous_segment_on_same_resource() {
        let body = b"#EXTM3U\n#EXTINF:1,\nmedia.mp4\n#EXTINF:1,\n#EXT-X-BYTERANGE:4@0\nmedia.mp4\n#EXTINF:1,\n#EXT-X-BYTERANGE:4\nmedia.mp4\n#EXT-X-ENDLIST\n";
        let plan = plan_hls_media_playlist(BASE_URL, body)
            .expect("consecutive byte ranges of one resource should continue");
        assert_eq!(
            plan.resources[2].range,
            Some(ManifestByteRange {
                offset: 4,
                length: 4
            })
        );

        let interrupted = b"#EXTM3U\n#EXTINF:1,\nmedia.mp4\n#EXTINF:1,\n#EXT-X-BYTERANGE:4@0\nmedia.mp4\n#EXTINF:1,\nother.mp4\n#EXTINF:1,\n#EXT-X-BYTERANGE:4\nmedia.mp4\n#EXT-X-ENDLIST\n";
        assert_eq!(
            plan_hls_media_playlist(BASE_URL, interrupted).err(),
            Some(ManifestError::MissingByteRangeOffset)
        );
    }

    #[test]
    fn init_map_byte_range_requires_an_explicit_offset() {
        let body = b"#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"16\"\n#EXTINF:2,\nmedia.mp4\n#EXT-X-ENDLIST\n";
        assert_eq!(
            plan_hls_media_playlist(BASE_URL, body).err(),
            Some(ManifestError::MissingByteRangeOffset)
        );
    }

    #[test]
    fn local_playlist_without_map_uses_version_three() {
        let body = br#"#EXTM3U
#EXTINF:2.1,
one.ts
#EXTINF:1.1,
two.ts
#EXT-X-ENDLIST
"#;
        let plan = plan_hls_media_playlist(BASE_URL, body)
            .expect("simple VOD playlist should produce a local plan");
        let bytes = plan
            .local_playlist_bytes()
            .expect("playlist should serialize");
        let parsed =
            m3u8_rs::parse_media_playlist_res(&bytes).expect("serialized playlist should parse");
        assert_eq!(parsed.version, Some(3));
        assert_eq!(parsed.target_duration, 3);
    }

    #[test]
    fn aes_128_key_url_is_absolutized_for_ffmpeg() {
        let body = br#"#EXTM3U
#EXT-X-KEY:METHOD=AES-128,URI="../keys/media.key",IV=0x00000000000000000000000000000001,KEYFORMAT="identity"
#EXTINF:2,
one.ts
#EXT-X-ENDLIST
"#;
        let plan = plan_hls_media_playlist(BASE_URL, body)
            .expect("AES-128 VOD playlist should remain available to FFmpeg");
        let bytes = plan
            .local_playlist_bytes()
            .expect("playlist should serialize");
        let parsed =
            m3u8_rs::parse_media_playlist_res(&bytes).expect("serialized playlist should parse");
        assert!(
            parsed.segments[0]
                .key
                .as_ref()
                .and_then(|key| key.uri.as_deref())
                .is_some_and(|uri| uri.starts_with("key-"))
        );
        assert_eq!(
            plan.resources
                .iter()
                .find(|resource| resource.kind == HlsResourceKind::Key)
                .map(|resource| resource.url.as_str()),
            Some("https://cdn.example.test/keys/media.key")
        );
        assert_eq!(parsed.version, Some(5));
    }

    #[test]
    fn carries_active_key_and_map_to_following_segments() {
        let body = br#"#EXTM3U
#EXT-X-KEY:METHOD=AES-128,URI="key.bin",IV=0x00000000000000000000000000000001
#EXTINF:2,
first.ts
#EXT-X-MAP:URI="init.mp4"
#EXTINF:2,
second.m4s
#EXTINF:2,
third.m4s
#EXT-X-ENDLIST
"#;
        let plan = plan_hls_media_playlist(BASE_URL, body)
            .expect("key and map scope should persist after their tags");

        let bytes = plan
            .local_playlist_bytes()
            .expect("local playlist should serialize");
        let parsed =
            m3u8_rs::parse_media_playlist_res(&bytes).expect("serialized playlist should parse");
        assert_eq!(parsed.segments.len(), 3);
        assert!(
            parsed
                .segments
                .iter()
                .all(|segment| segment.key.as_ref().is_some_and(|key| key
                    .uri
                    .as_deref()
                    .is_some_and(|uri| uri.starts_with("key-"))))
        );
        assert!(parsed.segments[0].map.is_none());
        let first_map = parsed.segments[1].map.as_ref().map(|map| map.uri.as_str());
        let second_map = parsed.segments[2].map.as_ref().map(|map| map.uri.as_str());
        assert_eq!(first_map, second_map);
        assert!(first_map.is_some_and(|uri| uri.starts_with("init-")));
    }

    #[test]
    fn rejects_encrypted_map_when_the_active_key_was_declared_on_an_earlier_segment() {
        let body = br#"#EXTM3U
#EXT-X-KEY:METHOD=AES-128,URI="key.bin"
#EXTINF:2,
one.ts
#EXT-X-MAP:URI="init.mp4"
#EXTINF:2,
two.m4s
#EXT-X-ENDLIST
"#;
        assert_eq!(
            plan_hls_media_playlist(BASE_URL, body).err(),
            Some(ManifestError::UnsupportedEncryption)
        );
    }

    #[test]
    fn rejects_method_none_key_tags_with_or_without_iv() {
        let without_iv = br#"#EXTM3U
#EXT-X-KEY:METHOD=NONE
#EXTINF:2,
one.ts
#EXT-X-ENDLIST
"#;
        let with_iv = br#"#EXTM3U
#EXT-X-KEY:METHOD=NONE,IV=0x00000000000000000000000000000001
#EXTINF:2,
one.ts
#EXT-X-ENDLIST
"#;

        for body in [without_iv.as_slice(), with_iv.as_slice()] {
            assert_eq!(
                plan_hls_media_playlist(BASE_URL, body).err(),
                Some(ManifestError::UnsupportedEncryption)
            );
        }
    }

    #[test]
    fn rejects_gap_and_low_latency_partial_segments() {
        let gap_body = b"#EXTM3U\n#EXTINF:2,\n#EXT-X-GAP\none.ts\n#EXT-X-ENDLIST\n";
        assert_eq!(
            plan_hls_media_playlist(BASE_URL, gap_body).err(),
            Some(ManifestError::UnsupportedPlaylist)
        );

        let partial_body =
            b"#EXTM3U\n#EXT-X-PART-INF:PART-TARGET=0.5\n#EXTINF:2,\none.ts\n#EXT-X-ENDLIST\n";
        assert_eq!(
            plan_hls_media_playlist(BASE_URL, partial_body).err(),
            Some(ManifestError::UnsupportedPlaylist)
        );
    }

    #[test]
    fn rejects_encrypted_initialization_maps_without_an_explicit_iv() {
        let body = br#"#EXTM3U
#EXT-X-KEY:METHOD=AES-128,URI="key.bin"
#EXT-X-MAP:URI="init.mp4"
#EXTINF:2,
one.ts
#EXT-X-ENDLIST
"#;
        assert_eq!(
            plan_hls_media_playlist(BASE_URL, body).err(),
            Some(ManifestError::UnsupportedEncryption)
        );
    }

    #[test]
    fn rejects_uri_affecting_hls_extensions_until_they_are_supported() {
        let body = br#"#EXTM3U
#EXT-X-CONTENT-STEERING:SERVER-URI="steering.json",PATHWAY-ID="default"
#EXT-X-STREAM-INF:BANDWIDTH=1000000
video.m3u8
"#;
        assert_eq!(
            parse_hls_master_playlist(BASE_URL, body).err(),
            Some(ManifestError::UnsupportedPlaylist)
        );

        let media = b"#EXTM3U\n#EXT-X-DEFINE:NAME=\"path\",VALUE=\"video\"\n#EXTINF:2,\n{$path}/one.ts\n#EXT-X-ENDLIST\n";
        assert_eq!(
            plan_hls_media_playlist(BASE_URL, media).err(),
            Some(ManifestError::UnsupportedPlaylist)
        );
    }

    #[test]
    fn rejects_live_playlists_without_end_list() {
        let body = b"#EXTM3U\n#EXTINF:2,\none.ts\n";
        assert_eq!(
            plan_hls_media_playlist(BASE_URL, body).err(),
            Some(ManifestError::UnsupportedPlaylist)
        );
    }

    #[test]
    fn rejects_sample_aes_instead_of_passing_unsupported_crypto_to_ffmpeg() {
        let body = br#"#EXTM3U
#EXT-X-KEY:METHOD=SAMPLE-AES,URI="key.bin"
#EXTINF:2,
one.ts
#EXT-X-ENDLIST
"#;
        assert_eq!(
            plan_hls_media_playlist(BASE_URL, body).err(),
            Some(ManifestError::UnsupportedEncryption)
        );
    }

    #[test]
    fn media_resource_cache_reuses_bytes_after_signature_rotation() {
        let first =
            Url::parse("https://cdn.example.test/seg/1.m4s?quality=1080&sig=old&expires=100")
                .expect("first signed resource URL");
        let refreshed =
            Url::parse("https://cdn.example.test/seg/1.m4s?quality=1080&expires=200&sig=new")
                .expect("refreshed signed resource URL");
        let different_variant =
            Url::parse("https://cdn.example.test/seg/1.m4s?quality=720&sig=new&expires=200")
                .expect("different representation URL");

        assert_eq!(
            resource_filename("segment", Some(1), &first, None),
            resource_filename("segment", Some(1), &refreshed, None),
            "rotating signatures must not invalidate completed media resources",
        );
        assert_ne!(
            resource_filename("segment", Some(1), &first, None),
            resource_filename("segment", Some(1), &different_variant, None),
            "content-selecting query parameters must remain part of resource identity",
        );
    }

    #[test]
    fn encryption_key_cache_preserves_all_query_parameters() {
        let first = Url::parse("https://cdn.example.test/key.bin?sig=old")
            .expect("first encryption key URL");
        let refreshed = Url::parse("https://cdn.example.test/key.bin?sig=new")
            .expect("refreshed encryption key URL");

        assert_ne!(
            resource_filename_exact_url("key", None, &first, None),
            resource_filename_exact_url("key", None, &refreshed, None),
            "a changed key URL must not reuse stale key bytes",
        );
    }

    #[test]
    fn rejects_non_https_manifest_and_resource_urls() {
        let body = b"#EXTM3U\n#EXTINF:2,\none.ts\n#EXT-X-ENDLIST\n";
        assert_eq!(
            plan_hls_media_playlist("http://cdn.example.test/video.m3u8", body).err(),
            Some(ManifestError::InvalidManifestUrl)
        );

        let insecure_body =
            b"#EXTM3U\n#EXTINF:2,\nhttp://cdn.example.test/one.ts\n#EXT-X-ENDLIST\n";
        assert_eq!(
            plan_hls_media_playlist(BASE_URL, insecure_body).err(),
            Some(ManifestError::UnsupportedResourceUrl)
        );
    }

    #[test]
    fn rejects_oversized_hls_manifest_bodies_before_parsing() {
        let body = vec![b' '; MAX_MANIFEST_BYTES + 1];
        assert_eq!(
            parse_hls_master_playlist(BASE_URL, &body).err(),
            Some(ManifestError::Parse)
        );
        assert_eq!(
            plan_hls_media_playlist(BASE_URL, &body).err(),
            Some(ManifestError::Parse)
        );
    }
}
