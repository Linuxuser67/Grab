//! Bounded URL intake for native HLS and static DASH manifests.
//!
//! This module only fetches and parses the presentation description. Media
//! resources remain remote until the queue/fetch phase consumes the selected
//! representation.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use gettextrs::gettext;
use sha2::{Digest, Sha256};
use url::Url;

use crate::manifest::{
    HlsMasterPlan, HlsPlan, HlsVariant, MAX_MANIFEST_BYTES, dash::DashManifestPlan,
};

const MANIFEST_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_MANIFEST_ATTEMPTS: u32 = 3;
const MANIFEST_RETRY_BASE_DELAY: Duration = Duration::from_millis(250);

#[must_use]
#[derive(Debug)]
pub(crate) enum ManifestIntakeError {
    Unsupported(String),
    Retryable(String),
    Failed(String),
}

impl std::fmt::Display for ManifestIntakeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::Unsupported(message) | Self::Retryable(message) | Self::Failed(message) => {
                message
            }
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for ManifestIntakeError {}

impl From<String> for ManifestIntakeError {
    fn from(error: String) -> Self {
        Self::Failed(error)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BrowserCookiePolicy {
    HandoffOnly,
    ExportConfiguredBrowser,
}

impl BrowserCookiePolicy {
    #[must_use]
    const fn allows_browser_export(self) -> bool {
        matches!(self, Self::ExportConfiguredBrowser)
    }
}

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
    height: Option<u64>,
    bandwidth: Option<u64>,
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

/// Select the closest representation to the configured video-quality preference.
#[must_use]
pub(crate) fn preferred_choice_index(choices: &[ManifestChoice], quality: &str) -> usize {
    let desired_height = crate::video_quality::quality_height(quality).map(u64::from);
    choices
        .iter()
        .enumerate()
        .min_by_key(|(index, choice)| {
            let height = choice.height.unwrap_or_default();
            let distance = desired_height.map_or(0, |desired| {
                choice
                    .height
                    .map_or(u64::MAX, |height| height.abs_diff(desired))
            });
            (
                distance,
                std::cmp::Reverse(height),
                std::cmp::Reverse(choice.bandwidth.unwrap_or_default()),
                *index,
            )
        })
        .map(|(index, _)| index)
        .unwrap_or(0)
}

impl ManifestLookup {
    /// Build visible choices from highest to lowest quality, retaining representation data.
    /// Equal-quality choices keep their original manifest order.
    pub(crate) fn choices(&self) -> Vec<ManifestChoice> {
        let mut choices = match &self.manifest {
            NativeManifest::HlsMaster(master) => master
                .variants
                .iter()
                .zip(hls_variant_keys(&master.variants))
                .map(|(variant, key)| {
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
                        key,
                        label,
                        height: variant.height,
                        bandwidth: Some(variant.average_bandwidth.unwrap_or(variant.bandwidth)),
                    }
                })
                .collect(),
            NativeManifest::HlsMedia(_) => vec![ManifestChoice {
                key: "hls-media".to_string(),
                label: gettext("Source playlist"),
                height: None,
                bandwidth: None,
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
                    height: representation.height,
                    bandwidth: representation.bandwidth,
                })
                .collect(),
        };
        choices.sort_by(|left, right| {
            right
                .height
                .cmp(&left.height)
                .then_with(|| right.bandwidth.cmp(&left.bandwidth))
        });
        choices
    }

    /// Return the representation closest to the configured video-quality preference.
    pub(crate) fn preferred_source(
        &self,
        quality: &str,
    ) -> Option<crate::media_types::ManifestSource> {
        let choices = self.choices();
        let index = preferred_choice_index(&choices, quality);
        choices
            .get(index)
            .and_then(|choice| self.selected_source(&choice.key))
    }

    /// Validate and capture the exact representation selected in the picker.
    pub(crate) fn selected_source(
        &self,
        representation_key: &str,
    ) -> Option<crate::media_types::ManifestSource> {
        let kind = match &self.manifest {
            NativeManifest::HlsMaster(master) => {
                hls_variant_keys(&master.variants)
                    .iter()
                    .any(|key| key == representation_key)
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
    let client = client_for_manifest_with_policy(
        raw_url,
        cookies_browser,
        proxy,
        BrowserCookiePolicy::HandoffOnly,
    )
    .await?;
    lookup_with_client(&client, raw_url)
        .await
        .map_err(|error| error.to_string())
}

/// Build an attempt-scoped client, preferring a valid extension handoff jar and
/// otherwise exporting the configured browser cookies just like the HTTP engine.
pub(crate) async fn client_for_manifest(
    raw_url: &str,
    cookies_browser: &str,
    proxy: Option<&crate::net_types::ResolvedProxy>,
) -> Result<reqwest::Client, String> {
    client_for_manifest_with_policy(
        raw_url,
        cookies_browser,
        proxy,
        BrowserCookiePolicy::ExportConfiguredBrowser,
    )
    .await
}

async fn client_for_manifest_with_policy(
    raw_url: &str,
    cookies_browser: &str,
    proxy: Option<&crate::net_types::ResolvedProxy>,
    cookie_policy: BrowserCookiePolicy,
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

    if cookie_policy.allows_browser_export()
        && !cookies_browser.is_empty()
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
) -> Result<Option<ManifestLookup>, ManifestIntakeError> {
    let requested_url = Url::parse(raw_url)
        .map_err(|_| ManifestIntakeError::Failed(gettext("Invalid stream manifest URL")))?;
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
) -> Result<HlsPlan, ManifestIntakeError> {
    let requested_url = Url::parse(raw_url)
        .map_err(|_| ManifestIntakeError::Failed(gettext("Invalid stream manifest URL")))?;
    let (final_url, body) = fetch_bounded_manifest(client, requested_url).await?;
    crate::manifest::plan_hls_media_playlist(final_url.as_str(), &body)
        .map_err(|error| ManifestIntakeError::Unsupported(error.to_string()))
}

async fn fetch_bounded_manifest(
    client: &reqwest::Client,
    requested_url: Url,
) -> Result<(Url, Vec<u8>), ManifestIntakeError> {
    if !is_secure_manifest_url(&requested_url) {
        return Err(ManifestIntakeError::Failed(gettext(
            "Invalid stream manifest URL",
        )));
    }

    let mut attempt = 0_u32;
    loop {
        attempt += 1;
        match fetch_bounded_manifest_once(client, &requested_url).await {
            Err(ManifestIntakeError::Retryable(error)) if attempt < MAX_MANIFEST_ATTEMPTS => {
                let delay = MANIFEST_RETRY_BASE_DELAY * attempt;
                tracing::debug!(
                    attempt,
                    delay_ms = delay.as_millis() as u64,
                    error = %error,
                    "retrying transient native manifest request"
                );
                tokio::time::sleep(delay).await;
            }
            result => return result,
        }
    }
}

fn manifest_status_error(status: reqwest::StatusCode) -> ManifestIntakeError {
    let message = format!("The stream server returned HTTP {}", status.as_u16());
    if status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
    {
        ManifestIntakeError::Retryable(message)
    } else {
        ManifestIntakeError::Unsupported(message)
    }
}

async fn fetch_bounded_manifest_once(
    client: &reqwest::Client,
    requested_url: &Url,
) -> Result<(Url, Vec<u8>), ManifestIntakeError> {
    let response = client
        .get(requested_url.clone())
        .timeout(MANIFEST_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|error| {
            ManifestIntakeError::Retryable(format!(
                "Could not retrieve the stream manifest: {error}"
            ))
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(manifest_status_error(status));
    }
    let final_url = response.url().clone();
    if !is_secure_manifest_url(&final_url) {
        return Err(ManifestIntakeError::Failed(gettext(
            "The stream manifest redirected to an unsafe URL",
        )));
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

/// Stable base identity for an HLS variant, retained for queued-item compatibility.
#[must_use]
fn hls_variant_key(variant: &HlsVariant) -> String {
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

/// Build unique keys for variants whose legacy identities collide.
///
/// Unique variants keep their 32-character persisted key. When multiple variants
/// share that key but have different content-selecting URLs, each receives a
/// URL-derived suffix; rotating authentication parameters are excluded.
#[must_use]
pub(crate) fn hls_variant_keys(variants: &[HlsVariant]) -> Vec<String> {
    let base_keys: Vec<String> = variants.iter().map(hls_variant_key).collect();
    let resource_urls: Vec<String> = variants
        .iter()
        .map(|variant| crate::manifest::resource_cache_key(&variant.url))
        .collect();

    let mut first_url_by_key = HashMap::<String, String>::with_capacity(base_keys.len());
    let mut ambiguous_keys = HashSet::<String>::new();
    for (base_key, resource_url) in base_keys.iter().zip(&resource_urls) {
        match first_url_by_key.get(base_key) {
            Some(first_url) if first_url != resource_url => {
                ambiguous_keys.insert(base_key.clone());
            }
            Some(_) => {}
            None => {
                first_url_by_key.insert(base_key.clone(), resource_url.clone());
            }
        }
    }

    base_keys
        .into_iter()
        .zip(resource_urls)
        .map(|(base_key, resource_url)| {
            if !ambiguous_keys.contains(&base_key) {
                return base_key;
            }
            let digest = Sha256::digest(resource_url.as_bytes());
            let suffix: String = digest[..16]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            format!("{base_key}{suffix}")
        })
        .collect()
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
                        matches!(fingerprint.len(), 32 | 64)
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

async fn read_bounded_response(
    mut response: reqwest::Response,
) -> Result<Vec<u8>, ManifestIntakeError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_MANIFEST_BYTES as u64)
    {
        return Err(ManifestIntakeError::Unsupported(format!(
            "The stream manifest exceeds the {} MiB size limit",
            MAX_MANIFEST_BYTES / (1024 * 1024)
        )));
    }

    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        ManifestIntakeError::Retryable(format!("Could not read the stream manifest: {error}"))
    })? {
        if body.len().saturating_add(chunk.len()) > MAX_MANIFEST_BYTES {
            return Err(ManifestIntakeError::Unsupported(format!(
                "The stream manifest exceeds the {} MiB size limit",
                MAX_MANIFEST_BYTES / (1024 * 1024)
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
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
    fn manifest_http_status_retries_rate_limits_and_server_errors_only() {
        assert!(matches!(
            manifest_status_error(reqwest::StatusCode::REQUEST_TIMEOUT),
            ManifestIntakeError::Retryable(_)
        ));
        assert!(matches!(
            manifest_status_error(reqwest::StatusCode::TOO_MANY_REQUESTS),
            ManifestIntakeError::Retryable(_)
        ));
        assert!(matches!(
            manifest_status_error(reqwest::StatusCode::SERVICE_UNAVAILABLE),
            ManifestIntakeError::Retryable(_)
        ));
        assert!(matches!(
            manifest_status_error(reqwest::StatusCode::NOT_FOUND),
            ManifestIntakeError::Unsupported(_)
        ));
        assert!(matches!(
            manifest_status_error(reqwest::StatusCode::FORBIDDEN),
            ManifestIntakeError::Unsupported(_)
        ));
    }

    #[test]
    fn preview_cookie_policy_does_not_export_browser_profiles() {
        assert!(!BrowserCookiePolicy::HandoffOnly.allows_browser_export());
        assert!(BrowserCookiePolicy::ExportConfiguredBrowser.allows_browser_export());
    }

    #[test]
    fn preferred_source_selects_the_configured_hls_representation() {
        let body = br#"#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=900000,RESOLUTION=1280x720
low.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=1800000,RESOLUTION=1920x1080
high.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=3500000,RESOLUTION=2560x1440
highest.m3u8
"#;
        let master = crate::manifest::parse_hls_master_playlist(
            "https://cdn.example.test/master.m3u8",
            body,
        )
        .expect("valid HLS master playlist");
        let lookup = ManifestLookup {
            source_url: "https://cdn.example.test/master.m3u8".to_string(),
            manifest: NativeManifest::HlsMaster(master),
        };

        let preferred = lookup
            .preferred_source("1080p")
            .expect("1080p HLS representation");
        let best = lookup
            .preferred_source("best")
            .expect("best HLS representation");

        let choices = lookup.choices();
        let preferred_choice = choices
            .iter()
            .find(|choice| choice.key == preferred.representation_key)
            .expect("preferred choice exists");
        let best_choice = choices
            .iter()
            .find(|choice| choice.key == best.representation_key)
            .expect("best choice exists");
        assert_eq!(preferred_choice.height, Some(1080));
        assert_eq!(best_choice.height, Some(1440));
        assert_eq!(
            choices.iter().map(|choice| choice.height).collect::<Vec<_>>(),
            vec![Some(1440), Some(1080), Some(720)]
        );
    }

    #[test]
    fn choices_sort_by_height_then_bandwidth_and_keep_ties_stable() {
        let body = br#"#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=1800000,RESOLUTION=1920x1080
first.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=900000,RESOLUTION=1280x720
low.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1920x1080
higher-bandwidth.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=1800000,RESOLUTION=1920x1080
second.m3u8
"#;
        let master = crate::manifest::parse_hls_master_playlist(
            "https://cdn.example.test/master.m3u8",
            body,
        )
        .expect("valid HLS master playlist");
        let original_keys = hls_variant_keys(&master.variants);
        let lookup = ManifestLookup {
            source_url: "https://cdn.example.test/master.m3u8".to_string(),
            manifest: NativeManifest::HlsMaster(master),
        };

        let choices = lookup.choices();
        assert_eq!(
            choices.iter().map(|choice| choice.height).collect::<Vec<_>>(),
            vec![Some(1080), Some(1080), Some(1080), Some(720)],
            "choices should be ordered by descending height"
        );
        assert_eq!(
            choices.iter().map(|choice| choice.key.as_str()).collect::<Vec<_>>(),
            vec![
                original_keys[2].as_str(),
                original_keys[0].as_str(),
                original_keys[3].as_str(),
                original_keys[1].as_str(),
            ],
            "bandwidth should break height ties, and equal ties should keep manifest order"
        );
    }

    #[test]
    fn preferred_choice_index_matches_configured_height_and_best_uses_highest() {
        let choices = [
            ManifestChoice {
                key: "720".to_string(),
                label: "720p".to_string(),
                height: Some(720),
                bandwidth: Some(1_000_000),
            },
            ManifestChoice {
                key: "1080".to_string(),
                label: "1080p".to_string(),
                height: Some(1080),
                bandwidth: Some(2_500_000),
            },
            ManifestChoice {
                key: "1440".to_string(),
                label: "1440p".to_string(),
                height: Some(1440),
                bandwidth: Some(4_000_000),
            },
        ];

        assert_eq!(preferred_choice_index(&choices, "1080p"), 1);
        assert_eq!(preferred_choice_index(&choices, "best"), 2);
        assert_eq!(preferred_choice_index(&choices, "720p"), 0);
    }

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

        let different_query = variant(
            "https://cdn.example.test/video/1080.m3u8?quality=720&sig=new",
            Some(1080),
        );

        assert_eq!(hls_variant_key(&original), hls_variant_key(&refreshed));
        assert_ne!(hls_variant_key(&original), hls_variant_key(&different));
        assert_eq!(
            hls_variant_key(&refreshed),
            hls_variant_key(&different_query)
        );

        let collision_keys = hls_variant_keys(&[refreshed.clone(), different_query.clone()]);
        assert_ne!(collision_keys[0], collision_keys[1]);
        assert_eq!(collision_keys[0].len(), "hls-variant-".len() + 64);
        assert!(collision_keys.iter().all(|key| {
            valid_source(&crate::media_types::ManifestSource {
                url: "https://cdn.example.test/master.m3u8".to_string(),
                kind: crate::media_types::ManifestKind::Hls,
                representation_key: key.clone(),
            })
        }));

        let unique_keys = hls_variant_keys(std::slice::from_ref(&refreshed));
        assert_eq!(unique_keys[0], hls_variant_key(&refreshed));
    }

    #[test]
    fn supports_manifest_urls_with_query_strings() {
        assert!(supports(
            "https://cdn.example.test/master.m3u8?expires=1&signature=x"
        ));
        assert!(supports("https://cdn.example.test/manifest.mpd#fragment"));
    }
}
