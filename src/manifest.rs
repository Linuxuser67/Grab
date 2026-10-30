//! Pure manifest parsing and resource planning for native HLS and DASH downloads.
//!
//! The parser layer owns no network or filesystem access. It turns a manifest
//! body into validated remote resources and a local playlist description.

use std::collections::HashMap;
use std::time::Duration;

use m3u8_rs::{AlternativeMediaType, KeyMethod, MediaPlaylist, MediaPlaylistType, Playlist};
use thiserror::Error;
use url::Url;

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

/// The kind of remote object fetched for a local HLS presentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HlsResourceKind {
    InitMap,
    Segment,
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
    let base_url = parse_manifest_url(manifest_url)?;
    let playlist = m3u8_rs::parse_playlist_res(body).map_err(|_| ManifestError::Parse)?;

    let Playlist::MasterPlaylist(master) = playlist else {
        return Err(ManifestError::ExpectedMasterPlaylist);
    };

    // Session keys would require the master-playlist encryption semantics to
    // be reproduced locally. Keep those manifests on the established fallback.
    if !master.session_key.is_empty() {
        return Err(ManifestError::UnsupportedEncryption);
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

    Ok(HlsMasterPlan {
        variants: variants.into_boxed_slice(),
        audio_renditions: audio_renditions.into_boxed_slice(),
    })
}

/// Parse a selected HLS media playlist and construct its deterministic fetch plan.
///
/// Byte ranges are converted to explicit offsets, init maps are fetched once per
/// distinct URI/range pair, and every media segment gets its own local filename.
/// AES-128 key URLs remain remote for FFmpeg's HLS demuxer; other encryption
/// methods are deliberately rejected.
///
/// # Errors
/// Returns an error for live or unsupported playlists, invalid URLs or ranges,
/// unsupported encryption, invalid durations, or an empty media playlist.
pub(crate) fn plan_hls_media_playlist(
    manifest_url: &str,
    body: &[u8],
) -> Result<HlsPlan, ManifestError> {
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
    let mut next_range_offsets: HashMap<String, u64> = HashMap::new();
    let mut total_duration = Duration::ZERO;
    let mut maximum_segment_duration = 0.0_f64;

    for (index, segment) in playlist.segments.iter_mut().enumerate() {
        if segment
            .unknown_tags
            .iter()
            .any(|tag| is_unsupported_hls_tag(&tag.tag))
        {
            return Err(ManifestError::UnsupportedPlaylist);
        }

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
            let map_range = map
                .byte_range
                .as_ref()
                .map(|range| {
                    resolve_byte_range(
                        range.length,
                        range.offset,
                        &map_url,
                        &mut next_range_offsets,
                    )
                })
                .transpose()?;
            let map_key = (map_url.as_str().to_owned(), map_range);
            let filename = match map_filenames.get(&map_key) {
                Some(existing) => existing.clone(),
                None => {
                    let filename = format!("init-{:06}.bin", map_filenames.len());
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
            key.uri = Some(resolve_resource_url(&base_url, key_uri)?.to_string());
        }

        let segment_url = resolve_resource_url(&base_url, &segment.uri)?;
        let range = segment
            .byte_range
            .as_ref()
            .map(|range| {
                resolve_byte_range(
                    range.length,
                    range.offset,
                    &segment_url,
                    &mut next_range_offsets,
                )
            })
            .transpose()?;
        let filename = format!("segment-{:06}.bin", index);
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
    playlist.version = Some(
        if playlist
            .segments
            .iter()
            .any(|segment| segment.map.is_some())
        {
            6
        } else {
            3
        },
    );
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
    )
}

fn parse_manifest_url(raw: &str) -> Result<Url, ManifestError> {
    let url = Url::parse(raw).map_err(|_| ManifestError::InvalidManifestUrl)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(ManifestError::InvalidManifestUrl);
    }
    Ok(url)
}

fn resolve_resource_url(base_url: &Url, raw: &str) -> Result<Url, ManifestError> {
    let url = base_url
        .join(raw)
        .map_err(|_| ManifestError::UnsupportedResourceUrl)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(ManifestError::UnsupportedResourceUrl);
    }
    Ok(url)
}

fn resolve_byte_range(
    length: u64,
    explicit_offset: Option<u64>,
    url: &Url,
    next_offsets: &mut HashMap<String, u64>,
) -> Result<ManifestByteRange, ManifestError> {
    if length == 0 {
        return Err(ManifestError::InvalidByteRange);
    }

    let offset = match explicit_offset {
        Some(offset) => offset,
        None => *next_offsets
            .get(url.as_str())
            .ok_or(ManifestError::MissingByteRangeOffset)?,
    };
    let next_offset = offset
        .checked_add(length)
        .ok_or(ManifestError::InvalidByteRange)?;
    next_offsets.insert(url.as_str().to_owned(), next_offset);

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
        assert_eq!(parsed.segments[0].uri, "segment-000000.bin");
        assert_eq!(
            parsed.segments[0].map.as_ref().map(|map| map.uri.as_str()),
            Some("init-000000.bin")
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
#EXT-X-KEY:METHOD=AES-128,URI="../keys/media.key",IV=0x00000000000000000000000000000001
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
        assert_eq!(
            parsed.segments[0]
                .key
                .as_ref()
                .and_then(|key| key.uri.as_deref()),
            Some("https://cdn.example.test/keys/media.key")
        );
    }

    #[test]
    fn rejects_gap_and_low_latency_partial_segments() {
        let gap_body = b"#EXTM3U\n#EXTINF:2,\n#EXT-X-GAP\none.ts\n#EXT-X-ENDLIST\n";
        assert_eq!(
            plan_hls_media_playlist(BASE_URL, gap_body).err(),
            Some(ManifestError::UnsupportedPlaylist)
        );

        let partial_body = b"#EXTM3U\n#EXT-X-PART-INF:PART-TARGET=0.5\n#EXTINF:2,\none.ts\n#EXT-X-ENDLIST\n";
        assert_eq!(
            plan_hls_media_playlist(BASE_URL, partial_body).err(),
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
}
