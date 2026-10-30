//! Bounded URL intake for native HLS and static DASH manifests.
//!
//! This module only fetches and parses the presentation description. Media
//! resources remain remote until the queue/fetch phase consumes the selected
//! representation.

use std::time::Duration;

use gettextrs::{gettext, ngettext};
use sha2::{Digest, Sha256};
use url::Url;

use crate::manifest::{
    HlsMasterPlan, HlsPlan, HlsVariant, MAX_MANIFEST_BYTES, dash::DashManifestPlan,
};

const MANIFEST_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// This is narrower than video::is_stream_manifest_url: native intake accepts
/// HTTPS .m3u8/.mpd only; the generic guard also reserves HTTP, .m3u and .pls.
fn native_manifest_kind(url: &Url) -> Option<crate::media_types::ManifestKind> {
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }

    let extension = url.path().rsplit('/').next()?.rsplit_once('.')?.1;
    if extension.eq_ignore_ascii_case("m3u8") {
        Some(crate::media_types::ManifestKind::Hls)
    } else if extension.eq_ignore_ascii_case("mpd") {
        Some(crate::media_types::ManifestKind::Dash)
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
        .is_some_and(|url| native_manifest_kind(&url).is_some())
}

/// One selectable video choice from a parsed native manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManifestChoice {
    pub(crate) key: String,
    pub(crate) label: String,
}

#[derive(Debug)]
pub(crate) enum NativeManifest {
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
    pub(crate) manifest: NativeManifest,
}

impl ManifestLookup {
    /// Build the visible video choices in their manifest order.
    pub(crate) fn choices(&self) -> Vec<ManifestChoice> {
        match &self.manifest {
            NativeManifest::HlsMaster(master) => master
                .variants
                .iter()
                .map(|variant| {
                    let resolution = resolution_label(variant.width, variant.height);
                    let bitrate =
                        bitrate_label(Some(variant.average_bandwidth.unwrap_or(variant.bandwidth)));
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
                        key: hls_variant_key(variant),
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

    /// Validate and capture the exact representation selected in the picker.
    pub(crate) fn selected_source(
        &self,
        representation_key: &str,
    ) -> Option<crate::media_types::ManifestSource> {
        let kind = match &self.manifest {
            NativeManifest::HlsMaster(master) => {
                master
                    .variants
                    .iter()
                    .any(|variant| hls_variant_key(variant) == representation_key)
                    .then_some(())?;
                crate::media_types::ManifestKind::Hls
            }
            NativeManifest::HlsMedia(_) if representation_key == "hls-media" => {
                crate::media_types::ManifestKind::Hls
            }
            NativeManifest::HlsMedia(_) => return None,
            NativeManifest::Dash(dash) => {
                if !dash
                    .video_representations
                    .iter()
                    .any(|representation| representation.key == representation_key)
                {
                    return None;
                }
                crate::media_types::ManifestKind::Dash
            }
        };
        Some(crate::media_types::ManifestSource {
            url: self.source_url.clone(),
            kind,
            representation_key: representation_key.to_string(),
        })
    }

    /// Parsed manifest kind, used to reject mismatched persisted source metadata.
    pub(crate) fn kind(&self) -> crate::media_types::ManifestKind {
        match &self.manifest {
            NativeManifest::HlsMaster(_) | NativeManifest::HlsMedia(_) => {
                crate::media_types::ManifestKind::Hls
            }
            NativeManifest::Dash(_) => crate::media_types::ManifestKind::Dash,
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
    cookies_browser: &str,
    proxy: Option<&crate::net_types::ResolvedProxy>,
) -> Result<Option<ManifestLookup>, String> {
    let requested_url = Url::parse(raw_url).map_err(|_| gettext("Invalid stream manifest URL"))?;
    if native_manifest_kind(&requested_url).is_none() {
        return Ok(None);
    }
    let client = client_for_manifest(raw_url, cookies_browser, proxy).await?;
    lookup_with_client(&client, raw_url).await
}

/// Build an attempt-scoped client, preferring a valid extension handoff jar and
/// otherwise exporting the configured browser cookies just like the HTTP engine.
pub(crate) async fn client_for_manifest(
    raw_url: &str,
    cookies_browser: &str,
    proxy: Option<&crate::net_types::ResolvedProxy>,
) -> Result<reqwest::Client, String> {
    // A browser handoff is public-only. An upstream proxy resolves the target
    // remotely and would bypass the local guarded DNS resolver, so reject it
    // consistently in both preview and download attempts.
    let source_host = Url::parse(raw_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned));
    if source_host
        .as_deref()
        .is_some_and(crate::download_net::is_untrusted_handoff_host)
    {
        crate::subprocess_proxy::validate_proxy_compatibility()?;
    }
    if let Some((path, handoff_user_agent)) =
        crate::extension_server::live_handoff_identity(raw_url, cookies_browser)
        && let Ok(text) = tokio::fs::read_to_string(&path).await
    {
        let (jar, count) = crate::cookies::jar_from_export(&text);
        if count > 0 {
            let user_agent = handoff_user_agent
                .as_deref()
                .unwrap_or(crate::download_net::DEFAULT_USER_AGENT);
            return crate::download_net::http_client_with_jar_and_user_agent(
                proxy, jar, user_agent,
            );
        }
    }

    if !cookies_browser.is_empty()
        && cookies_browser != "none"
        && let Ok(youtube_bin) = crate::video_tools::resolve_ytdlp()
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        match tokio::time::timeout_at(deadline, async {
            crate::video_tools::ensure_deno(raw_url).await?;
            Ok::<_, crate::video_tools::VideoError>(
                crate::cookies::jar_for_browser(cookies_browser, &youtube_bin, raw_url).await,
            )
        })
        .await
        {
            Ok(Ok(Some(jar))) => {
                return crate::download_net::http_client_with_jar(proxy, jar);
            }
            Ok(Ok(None)) => {}
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "browser-cookie export skipped for native manifest");
            }
            Err(_) => {
                tracing::warn!(
                    "browser-cookie export exceeded its 120-second native manifest deadline"
                );
            }
        }
    }

    crate::download_net::http_client_for(proxy)
        .map_err(|_| gettext("The configured proxy could not be used for this manifest"))
}

/// Fetch a manifest with an existing per-attempt client so manifests and media
/// resources share the same proxy and DNS policy.
pub(crate) async fn lookup_with_client(
    client: &reqwest::Client,
    raw_url: &str,
) -> Result<Option<ManifestLookup>, String> {
    let requested_url = Url::parse(raw_url).map_err(|_| gettext("Invalid stream manifest URL"))?;
    let Some(kind) = native_manifest_kind(&requested_url) else {
        return Ok(None);
    };
    let (final_url, body) = fetch_bounded_manifest(client, requested_url).await?;
    let manifest = match kind {
        crate::media_types::ManifestKind::Hls => {
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
        crate::media_types::ManifestKind::Dash => {
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

/// Fetch and parse an HLS child media playlist using the caller's guarded client.
pub(crate) async fn lookup_hls_media(
    client: &reqwest::Client,
    raw_url: &str,
) -> Result<HlsPlan, String> {
    let requested_url = Url::parse(raw_url).map_err(|_| gettext("Invalid stream manifest URL"))?;
    let (final_url, body) = fetch_bounded_manifest(client, requested_url).await?;
    crate::manifest::plan_hls_media_playlist(final_url.as_str(), &body)
        .map_err(|error| error.to_string())
}

async fn fetch_bounded_manifest(
    client: &reqwest::Client,
    requested_url: Url,
) -> Result<(Url, Vec<u8>), String> {
    if !is_secure_manifest_url(&requested_url) {
        return Err(gettext("Invalid stream manifest URL"));
    }
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
    if !is_secure_manifest_url(&final_url) {
        return Err(gettext("The stream manifest redirected to an unsafe URL"));
    }
    let body = read_bounded_response(response).await?;
    Ok((final_url, body))
}

fn is_secure_manifest_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
}

/// Stable key for an HLS variant. Signed query strings are excluded so a
/// refreshed manifest can still identify the selected rendition.
pub(crate) fn hls_variant_key(variant: &HlsVariant) -> String {
    let mut hasher = Sha256::new();
    hasher.update(variant.url.host_str().unwrap_or_default().as_bytes());
    hasher.update(
        variant
            .url
            .port_or_known_default()
            .unwrap_or_default()
            .to_le_bytes(),
    );
    hasher.update([0]);
    hasher.update(variant.url.path().as_bytes());
    hasher.update([0]);
    hasher.update(variant.bandwidth.to_le_bytes());
    hasher.update(variant.average_bandwidth.unwrap_or_default().to_le_bytes());
    hasher.update(variant.width.unwrap_or_default().to_le_bytes());
    hasher.update(variant.height.unwrap_or_default().to_le_bytes());
    hasher.update(
        variant
            .frame_rate
            .unwrap_or_default()
            .to_bits()
            .to_le_bytes(),
    );
    if let Some(codecs) = variant.codecs.as_deref() {
        hasher.update(codecs.as_bytes());
    }
    hasher.update([0]);
    if let Some(audio_group) = variant.audio_group.as_deref() {
        hasher.update(audio_group.as_bytes());
    }
    let digest = hasher.finalize();
    let fingerprint: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("hls-variant-{fingerprint}")
}

/// Check persisted source metadata before restoring a native queue row.
pub(crate) fn valid_source(source: &crate::media_types::ManifestSource) -> bool {
    let Ok(url) = Url::parse(&source.url) else {
        return false;
    };
    if native_manifest_kind(&url) != Some(source.kind) || source.representation_key.is_empty() {
        return false;
    }
    match source.kind {
        crate::media_types::ManifestKind::Hls => {
            source.representation_key == "hls-media"
                || source
                    .representation_key
                    .strip_prefix("hls-variant-")
                    .is_some_and(|fingerprint| {
                        fingerprint.len() == 32
                            && fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
        }
        crate::media_types::ManifestKind::Dash => source
            .representation_key
            .strip_prefix("dash-")
            .is_some_and(|fingerprint| {
                fingerprint.len() == 32 && fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit())
            }),
    }
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
    codecs.filter(|codecs| !codecs.is_empty()).map(short_label)
}

fn short_label(value: &str) -> String {
    const MAX_LABEL_CHARS: usize = 36;
    let mut label = value.chars().take(MAX_LABEL_CHARS).collect::<String>();
    if value.chars().count() > MAX_LABEL_CHARS {
        label.push('…');
    }
    label
}

fn join_label_parts(resolution: String, bitrate: Option<String>, codecs: Option<String>) -> String {
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
        assert!(!supports(
            "https://user:secret@cdn.example.test/manifest.mpd"
        ));
        assert!(!supports("https://cdn.example.test/path/master.m3u8.exe"));
    }

    #[test]
    fn hls_variant_key_survives_signed_query_rotation() {
        let variant = |url: &str, height: Option<u64>| HlsVariant {
            url: Url::parse(url).expect("valid variant URL"),
            bandwidth: 2_000_000,
            average_bandwidth: Some(1_800_000),
            width: Some(1920),
            height,
            codecs: Some("avc1.640028".to_string()),
            frame_rate: Some(30.0),
            audio_group: Some("audio-main".to_string()),
        };
        let original = variant(
            "https://cdn.example.test/video/1080.m3u8?sig=old",
            Some(1080),
        );
        let refreshed = variant(
            "https://cdn.example.test/video/1080.m3u8?sig=new",
            Some(1080),
        );
        let different = variant(
            "https://cdn.example.test/video/1080.m3u8?sig=new",
            Some(720),
        );

        assert_eq!(hls_variant_key(&original), hls_variant_key(&refreshed));
        assert_ne!(hls_variant_key(&original), hls_variant_key(&different));
    }

    #[test]
    fn supports_manifest_urls_with_query_strings() {
        assert!(supports(
            "https://cdn.example.test/master.m3u8?expires=1&signature=x"
        ));
        assert!(supports("https://cdn.example.test/manifest.mpd#fragment"));
    }

    #[test]
    fn duration_labels_include_hours_without_losing_minutes() {
        assert_eq!(format_duration(Duration::from_secs(125)), "2:05");
        assert_eq!(format_duration(Duration::from_secs(3_661)), "1:01:01");
    }
}
