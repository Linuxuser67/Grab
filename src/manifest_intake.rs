//! Bounded URL intake for native HLS and static DASH manifests.
//!
//! This module only fetches and parses the presentation description. Media
//! resources remain remote until the queue/fetch phase consumes the selected
//! representation.

use std::time::Duration;

use gettextrs::{gettext, ngettext};
use url::Url;

use crate::manifest::{HlsMasterPlan, HlsPlan, MAX_MANIFEST_BYTES, dash::DashManifestPlan};

const MANIFEST_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManifestKind {
    Hls,
    Dash,
}

fn manifest_kind(url: &Url) -> Option<ManifestKind> {
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }

    let extension = url.path().rsplit('/').next()?.rsplit_once('.')?.1;
    if extension.eq_ignore_ascii_case("m3u8") {
        Some(ManifestKind::Hls)
    } else if extension.eq_ignore_ascii_case("mpd") {
        Some(ManifestKind::Dash)
    } else {
        None
    }
}

/// Whether a URL is eligible for the native HTTPS manifest lookup.
///
/// Other schemes and extensions remain on the established yt-dlp path.
pub(crate) fn supports(raw_url: &str) -> bool {
    Url::parse(raw_url)
        .ok()
        .is_some_and(|url| manifest_kind(&url).is_some())
}

/// One selectable video choice from a parsed native manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManifestChoice {
    pub(crate) key: String,
    pub(crate) label: String,
}

#[derive(Debug)]
enum NativeManifest {
    HlsMaster(HlsMasterPlan),
    HlsMedia(HlsPlan),
    Dash(DashManifestPlan),
}

/// Parsed manifest associated with the exact URL entered by the user.
///
/// The source URL is the freshness key used by the inline card. All child
/// resources have already been resolved against the post-redirect URL.
#[derive(Debug)]
pub(crate) struct ManifestLookup {
    pub(crate) source_url: String,
    manifest: NativeManifest,
}

impl ManifestLookup {
    /// Build the visible video choices in their manifest order.
    pub(crate) fn choices(&self) -> Vec<ManifestChoice> {
        match &self.manifest {
            NativeManifest::HlsMaster(master) => master
                .variants
                .iter()
                .enumerate()
                .map(|(index, variant)| {
                    let resolution = resolution_label(variant.width, variant.height);
                    let bitrate = bitrate_label(Some(
                        variant.average_bandwidth.unwrap_or(variant.bandwidth),
                    ));
                    let mut label = join_label_parts(
                        resolution,
                        bitrate,
                        codec_label(variant.codecs.as_deref()),
                    );

                    if let Some(group_id) = variant.audio_group.as_deref() {
                        let rendition = master
                            .audio_renditions
                            .iter()
                            .filter(|rendition| rendition.group_id == group_id)
                            .find(|rendition| rendition.is_default)
                            .or_else(|| {
                                master
                                    .audio_renditions
                                    .iter()
                                    .find(|rendition| rendition.group_id == group_id)
                            });
                        if let Some(rendition) = rendition {
                            let audio = rendition
                                .language
                                .as_deref()
                                .filter(|language| !language.is_empty())
                                .unwrap_or(&rendition.name);
                            if !audio.is_empty() {
                                label.push_str(" · ");
                                label.push_str(&short_label(audio));
                            }
                        }
                    }

                    ManifestChoice {
                        key: format!("hls-variant-{index}"),
                        label,
                    }
                })
                .collect(),
            NativeManifest::HlsMedia(_) => vec![ManifestChoice {
                key: "hls-media".to_string(),
                label: gettext("Source playlist"),
            }],
            NativeManifest::Dash(dash) => dash
                .video_representations
                .iter()
                .map(|representation| ManifestChoice {
                    key: representation.key.clone(),
                    label: join_label_parts(
                        resolution_label(representation.width, representation.height),
                        bitrate_label(representation.bandwidth),
                        codec_label(representation.codecs.as_deref()),
                    ),
                })
                .collect(),
        }
    }

    /// A short summary for the native manifest preview row.
    pub(crate) fn summary(&self) -> String {
        match &self.manifest {
            NativeManifest::HlsMaster(master) => format!(
                "{} · {}",
                count_label(
                    master.variants.len(),
                    "{} video variant",
                    "{} video variants"
                ),
                count_label(
                    master.audio_renditions.len(),
                    "{} alternate audio track",
                    "{} alternate audio tracks"
                )
            ),
            NativeManifest::HlsMedia(plan) => format!(
                "{} · {}",
                gettext("HLS media playlist"),
                format_duration(plan.duration)
            ),
            NativeManifest::Dash(dash) => format!(
                "{} · {} · {}",
                count_label(
                    dash.video_representations.len(),
                    "{} video representation",
                    "{} video representations"
                ),
                count_label(
                    dash.audio_representations.len(),
                    "{} audio representation",
                    "{} audio representations"
                ),
                format_duration(dash.duration)
            ),
        }
    }
}

/// Fetch and parse a supported manifest. Unsupported or malformed manifests
/// return None so the established yt-dlp resolver can handle them.
///
/// The response is streamed into a bounded buffer; an untrusted Content-Length
/// is only an early rejection hint, never the memory bound.
pub(crate) async fn lookup(
    raw_url: &str,
    proxy: Option<&crate::net_types::ResolvedProxy>,
) -> Result<Option<ManifestLookup>, String> {
    let requested_url = Url::parse(raw_url).map_err(|_| gettext("Invalid stream manifest URL"))?;
    let Some(kind) = manifest_kind(&requested_url) else {
        return Ok(None);
    };

    let client = crate::download_net::http_client_for(proxy)
        .map_err(|_| gettext("The configured proxy could not be used for this manifest"))?;
    let response = client
        .get(requested_url)
        .timeout(MANIFEST_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|_| gettext("Could not retrieve the stream manifest"))?;

    if !response.status().is_success() {
        return Err(format!(
            "The stream server returned HTTP {}",
            response.status().as_u16()
        ));
    }

    let final_url = response.url().clone();
    if final_url.scheme() != "https"
        || final_url.host_str().is_none()
        || !final_url.username().is_empty()
        || final_url.password().is_some()
    {
        return Ok(None);
    }

    let body = read_bounded_response(response).await?;
    let manifest = match kind {
        ManifestKind::Hls => {
            match crate::manifest::parse_hls_master_playlist(final_url.as_str(), &body) {
                Ok(master) => NativeManifest::HlsMaster(master),
                Err(crate::manifest::ManifestError::ExpectedMasterPlaylist) => {
                    match crate::manifest::plan_hls_media_playlist(final_url.as_str(), &body) {
                        Ok(plan) => NativeManifest::HlsMedia(plan),
                        Err(_) => return Ok(None),
                    }
                }
                Err(_) => return Ok(None),
            }
        }
        ManifestKind::Dash => {
            let Ok(body) = std::str::from_utf8(&body) else {
                return Ok(None);
            };
            let Ok(plan) = crate::manifest::dash::parse_dash_manifest(final_url.as_str(), body)
            else {
                return Ok(None);
            };
            NativeManifest::Dash(plan)
        }
    };

    Ok(Some(ManifestLookup {
        source_url: raw_url.to_string(),
        manifest,
    }))
}

async fn read_bounded_response(mut response: reqwest::Response) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_MANIFEST_BYTES as u64)
    {
        return Err(format!(
            "The stream manifest exceeds the {} MiB size limit",
            MAX_MANIFEST_BYTES / (1024 * 1024)
        ));
    }

    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| gettext("Could not read the stream manifest"))?
    {
        if body.len().saturating_add(chunk.len()) > MAX_MANIFEST_BYTES {
            return Err(format!(
                "The stream manifest exceeds the {} MiB size limit",
                MAX_MANIFEST_BYTES / (1024 * 1024)
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn count_label(count: usize, singular: &str, plural: &str) -> String {
    ngettext(singular, plural, count as u32).replace("{}", &count.to_string())
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let remaining_seconds = seconds % 60;

    if hours > 0 {
        format!("{hours}:{minutes:02}:{remaining_seconds:02}")
    } else {
        format!("{minutes}:{remaining_seconds:02}")
    }
}

fn resolution_label(width: Option<u64>, height: Option<u64>) -> String {
    match (width, height) {
        (_, Some(height)) if height > 0 => format!("{height}p"),
        (Some(width), _) if width > 0 => format!("{width}px wide"),
        _ => gettext("Unknown resolution"),
    }
}

fn bitrate_label(bandwidth: Option<u64>) -> Option<String> {
    bandwidth
        .filter(|bandwidth| *bandwidth > 0)
        .map(|bandwidth| format!("{:.1} Mb/s", bandwidth as f64 / 1_000_000.0))
}

fn codec_label(codecs: Option<&str>) -> Option<String> {
    codecs
        .filter(|codecs| !codecs.is_empty())
        .map(short_label)
}

fn short_label(value: &str) -> String {
    const MAX_LABEL_CHARS: usize = 36;
    let mut label = value.chars().take(MAX_LABEL_CHARS).collect::<String>();
    if value.chars().count() > MAX_LABEL_CHARS {
        label.push('…');
    }
    label
}

fn join_label_parts(
    resolution: String,
    bitrate: Option<String>,
    codecs: Option<String>,
) -> String {
    let mut parts = vec![resolution];
    parts.extend(bitrate);
    parts.extend(codecs);
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_https_hls_and_dash_manifest_paths() {
        assert!(supports("https://cdn.example.test/master.m3u8?token=abc"));
        assert!(supports("https://cdn.example.test/path/manifest.MPD"));
        assert!(!supports("http://cdn.example.test/master.m3u8"));
        assert!(!supports("https://cdn.example.test/video.mp4"));
        assert!(!supports("https://cdn.example.test/manifest.m3u"));
        assert!(!supports("https://cdn.example.test/manifest.pls"));
        assert!(!supports("https://cdn.example.test/manifest.m3u8/"));
        assert!(!supports("https://user:secret@cdn.example.test/manifest.mpd"));
        assert!(!supports("https://cdn.example.test/path/master.m3u8.exe"));
    }

    #[test]
    fn supports_manifest_urls_with_query_strings() {
        assert!(supports("https://cdn.example.test/master.m3u8?expires=1&signature=x"));
        assert!(supports("https://cdn.example.test/manifest.mpd#fragment"));
    }

    #[test]
    fn duration_labels_include_hours_without_losing_minutes() {
        assert_eq!(format_duration(Duration::from_secs(125)), "2:05");
        assert_eq!(format_duration(Duration::from_secs(3_661)), "1:01:01");
    }
}
