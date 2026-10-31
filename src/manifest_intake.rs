//! Bounded URL intake for native HLS and static DASH manifests.
//!
//! This module only fetches and parses the presentation description. Media
//! resources remain remote until the queue/fetch phase consumes the selected
//! representation.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime};

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManifestTransportPolicy {
    HttpsOnly,
    #[cfg(test)]
    HttpFixture,
}

impl ManifestTransportPolicy {
    fn accepts_url(self, url: &Url) -> bool {
        let expected_scheme = match self {
            Self::HttpsOnly => "https",
            #[cfg(test)]
            Self::HttpFixture => "http",
        };
        url.scheme() == expected_scheme
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
    }

    fn planning_url(self, url: Url) -> Result<Url, ManifestIntakeError> {
        match self {
            Self::HttpsOnly => Ok(url),
            #[cfg(test)]
            Self::HttpFixture => {
                let mut url = url;
                url.set_scheme("https").map_err(|_| {
                    ManifestIntakeError::Failed(gettext("Invalid stream manifest URL"))
                })?;
                Ok(url)
            }
        }
    }
}

/// This is narrower than video::is_stream_manifest_url: native intake accepts
/// HTTPS .m3u8/.mpd only; the generic guard also reserves HTTP, .m3u and .pls.
fn native_manifest_kind(url: &Url) -> Option<crate::media_types::ManifestKind> {
    manifest_kind_for_transport(url, ManifestTransportPolicy::HttpsOnly)
}

fn manifest_kind_for_transport(
    url: &Url,
    transport_policy: ManifestTransportPolicy,
) -> Option<crate::media_types::ManifestKind> {
    if !transport_policy.accepts_url(url) {
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

/// One selectable audio rendition associated with a selected video representation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManifestAudioChoice {
    pub(crate) key: String,
    pub(crate) label: String,
    pub(crate) is_default: bool,
}

#[derive(Debug)]
pub(crate) enum NativeManifest {
    HlsMaster(HlsMasterPlan),
    HlsMedia(Box<HlsPlan>),
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
                    let label = join_label_parts(
                        resolution,
                        bitrate,
                        codec_label(variant.codecs.as_deref()),
                    );

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

    /// List the audio renditions belonging to one selected video representation.
    ///
    /// Every supported audio choice remains available. HLS order follows the
    /// playlist; the default choice is selected by `preferred_audio_choice_index`. Renditions
    /// without a URI represent audio embedded in the selected video playlist.
    pub(crate) fn audio_choices(&self, video_key: &str) -> Vec<ManifestAudioChoice> {
        match &self.manifest {
            NativeManifest::HlsMaster(master) => {
                let group_id = master
                    .variants
                    .iter()
                    .zip(hls_variant_keys(&master.variants))
                    .find_map(|(variant, key)| {
                        (key == video_key)
                            .then_some(variant.audio_group.as_deref())
                            .flatten()
                    });
                let Some(group_id) = group_id else {
                    return Vec::new();
                };
                master
                    .audio_renditions
                    .iter()
                    .zip(hls_audio_rendition_keys(&master.audio_renditions))
                    .filter(|(rendition, _)| rendition.group_id == group_id)
                    .map(|(rendition, key)| ManifestAudioChoice {
                        key,
                        label: hls_audio_rendition_label(rendition),
                        is_default: rendition.is_default,
                    })
                    .collect()
            }
            NativeManifest::HlsMedia(_) => Vec::new(),
            NativeManifest::Dash(dash) => {
                if !dash
                    .video_representations
                    .iter()
                    .any(|representation| representation.key == video_key)
                {
                    return Vec::new();
                }
                let mut choices = dash
                    .audio_representations
                    .iter()
                    .map(|representation| {
                        let title = representation
                            .language
                            .as_deref()
                            .filter(|language| !language.is_empty())
                            .or(representation.representation_id.as_deref())
                            .map(short_label)
                            .unwrap_or_else(|| gettext("Audio track"));
                        let mut label = join_label_parts(
                            title,
                            bitrate_label(representation.bandwidth),
                            codec_label(representation.codecs.as_deref()),
                        );
                        if representation.is_default_audio {
                            label.push_str(" · ");
                            label.push_str(&gettext("Default"));
                        }
                        ManifestAudioChoice {
                            key: representation.key.clone(),
                            label,
                            is_default: representation.is_default_audio,
                        }
                    })
                    .collect::<Vec<_>>();
                choices.sort_by_key(|choice| !choice.is_default);
                choices
            }
        }
    }

    /// Return the default audio choice, or the first available choice.
    #[must_use]
    pub(crate) fn preferred_audio_choice_index(choices: &[ManifestAudioChoice]) -> usize {
        choices
            .iter()
            .position(|choice| choice.is_default)
            .unwrap_or(0)
    }

    /// Return the representation closest to the configured video-quality preference.
    pub(crate) fn preferred_source(
        &self,
        quality: &str,
    ) -> Option<crate::media_types::ManifestSource> {
        let choices = self.choices();
        let index = preferred_choice_index(&choices, quality);
        choices.get(index).and_then(|choice| {
            let audio_choices = self.audio_choices(&choice.key);
            let audio_index = Self::preferred_audio_choice_index(&audio_choices);
            let audio_key = audio_choices
                .get(audio_index)
                .map(|audio| audio.key.as_str());
            self.selected_source(&choice.key, audio_key)
        })
    }

    /// Validate and capture the exact video and optional audio selections.
    pub(crate) fn selected_source(
        &self,
        representation_key: &str,
        audio_key: Option<&str>,
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
        if audio_key.is_some_and(|selected| {
            !self
                .audio_choices(representation_key)
                .iter()
                .any(|choice| choice.key == selected)
        }) {
            return None;
        }
        Some(crate::media_types::ManifestSource {
            url: self.source_url.clone(),
            kind,
            representation_key: representation_key.to_string(),
            audio_key: audio_key.map(str::to_string),
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
    lookup_with_client_policy(client, raw_url, ManifestTransportPolicy::HttpsOnly).await
}

async fn lookup_with_client_policy(
    client: &reqwest::Client,
    raw_url: &str,
    transport_policy: ManifestTransportPolicy,
) -> Result<Option<ManifestLookup>, ManifestIntakeError> {
    let requested_url = Url::parse(raw_url)
        .map_err(|_| ManifestIntakeError::Failed(gettext("Invalid stream manifest URL")))?;
    let Some(kind) = manifest_kind_for_transport(&requested_url, transport_policy) else {
        return Ok(None);
    };
    let (final_url, body, server_now) =
        fetch_bounded_manifest_with_policy(client, requested_url, transport_policy).await?;
    let final_url = transport_policy.planning_url(final_url)?;
    let manifest = match kind {
        crate::media_types::ManifestKind::Hls => {
            match crate::manifest::parse_hls_master_playlist(final_url.as_str(), &body) {
                Ok(master) => NativeManifest::HlsMaster(master),
                Err(crate::manifest::ManifestError::ExpectedMasterPlaylist) => {
                    match crate::manifest::plan_hls_media_playlist(final_url.as_str(), &body) {
                        Ok(plan) => NativeManifest::HlsMedia(Box::new(plan)),
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
            let Ok(plan) = crate::manifest::dash::parse_dash_manifest_at(
                final_url.as_str(),
                body,
                server_now,
            ) else {
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

#[cfg(test)]
async fn lookup_with_http_fixture_client(
    client: &reqwest::Client,
    raw_url: &str,
) -> Result<Option<ManifestLookup>, ManifestIntakeError> {
    lookup_with_client_policy(client, raw_url, ManifestTransportPolicy::HttpFixture).await
}

/// Fetch and parse an HLS child media playlist using the caller's guarded client.
pub(crate) async fn lookup_hls_media(
    client: &reqwest::Client,
    raw_url: &str,
) -> Result<HlsPlan, ManifestIntakeError> {
    let requested_url = Url::parse(raw_url)
        .map_err(|_| ManifestIntakeError::Failed(gettext("Invalid stream manifest URL")))?;
    let (final_url, body, _) = fetch_bounded_manifest(client, requested_url).await?;
    crate::manifest::plan_hls_media_playlist(final_url.as_str(), &body)
        .map_err(|error| ManifestIntakeError::Unsupported(error.to_string()))
}

async fn fetch_bounded_manifest(
    client: &reqwest::Client,
    requested_url: Url,
) -> Result<(Url, Vec<u8>, SystemTime), ManifestIntakeError> {
    fetch_bounded_manifest_with_policy(client, requested_url, ManifestTransportPolicy::HttpsOnly)
        .await
}

async fn fetch_bounded_manifest_with_policy(
    client: &reqwest::Client,
    requested_url: Url,
    transport_policy: ManifestTransportPolicy,
) -> Result<(Url, Vec<u8>, SystemTime), ManifestIntakeError> {
    if !transport_policy.accepts_url(&requested_url) {
        return Err(ManifestIntakeError::Failed(gettext(
            "Invalid stream manifest URL",
        )));
    }

    let mut attempt = 0_u32;
    loop {
        attempt += 1;
        match fetch_bounded_manifest_once(client, &requested_url, transport_policy).await {
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
    transport_policy: ManifestTransportPolicy,
) -> Result<(Url, Vec<u8>, SystemTime), ManifestIntakeError> {
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
    if !transport_policy.accepts_url(&final_url) {
        return Err(ManifestIntakeError::Failed(gettext(
            "The stream manifest redirected to an unsafe URL",
        )));
    }
    // DASH availability is defined against the server's wall clock. Using its
    // Date header avoids local clock skew turning future segments into misses
    // or not-yet-complete segments into premature fetches.
    let server_now = response
        .headers()
        .get(reqwest::header::DATE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| httpdate::parse_http_date(value).ok())
        .unwrap_or_else(SystemTime::now);
    let body = read_bounded_response(response).await?;
    Ok((final_url, body, server_now))
}

fn hls_audio_rendition_key(rendition: &crate::manifest::HlsAudioRendition) -> String {
    let mut hasher = Sha256::new();
    let resource_identity = rendition
        .url
        .as_ref()
        .map(crate::manifest::resource_cache_key);
    for value in [
        Some(rendition.group_id.as_str()),
        Some(rendition.name.as_str()),
        rendition.language.as_deref(),
        resource_identity.as_deref(),
    ] {
        if let Some(value) = value {
            hasher.update([1]);
            hasher.update((value.len() as u64).to_le_bytes());
            hasher.update(value.as_bytes());
        } else {
            hasher.update([0]);
        }
    }
    let digest = hasher.finalize();
    let fingerprint: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("hls-audio-{fingerprint}")
}

/// Stable HLS audio identities ignore rotating signature query parameters.
#[must_use]
pub(crate) fn hls_audio_rendition_keys(
    renditions: &[crate::manifest::HlsAudioRendition],
) -> Vec<String> {
    renditions.iter().map(hls_audio_rendition_key).collect()
}

fn hls_audio_rendition_label(rendition: &crate::manifest::HlsAudioRendition) -> String {
    let language = rendition
        .language
        .as_deref()
        .filter(|language| !language.is_empty());
    let name = (!rendition.name.is_empty()).then_some(rendition.name.as_str());
    let mut parts = Vec::with_capacity(3);
    if let Some(language) = language {
        parts.push(short_label(language));
    }
    if let Some(name) = name.filter(|name| Some(*name) != language) {
        parts.push(short_label(name));
    }
    if rendition.url.is_none() {
        parts.push(gettext("Embedded audio"));
    }
    if rendition.is_default {
        parts.push(gettext("Default"));
    }
    if parts.is_empty() {
        gettext("Audio track")
    } else {
        parts.join(" · ")
    }
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

fn has_hex_fingerprint(value: &str, prefix: &str, length: usize) -> bool {
    value.strip_prefix(prefix).is_some_and(|fingerprint| {
        fingerprint.len() == length && fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

/// Check persisted source metadata before restoring a native queue row.
pub(crate) fn valid_source(source: &crate::media_types::ManifestSource) -> bool {
    let Ok(url) = Url::parse(&source.url) else {
        return false;
    };
    if native_manifest_kind(&url) != Some(source.kind) || source.representation_key.is_empty() {
        return false;
    }
    let video_key_is_valid = match source.kind {
        crate::media_types::ManifestKind::Hls => {
            (source.representation_key == "hls-media" && source.audio_key.is_none())
                || has_hex_fingerprint(&source.representation_key, "hls-variant-", 32)
                || has_hex_fingerprint(&source.representation_key, "hls-variant-", 64)
        }
        crate::media_types::ManifestKind::Dash => {
            has_hex_fingerprint(&source.representation_key, "dash-", 32)
                || has_hex_fingerprint(&source.representation_key, "dash-live-", 32)
        }
    };
    let audio_key_is_valid = source
        .audio_key
        .as_deref()
        .is_none_or(|key| match source.kind {
            crate::media_types::ManifestKind::Hls => has_hex_fingerprint(key, "hls-audio-", 32),
            crate::media_types::ManifestKind::Dash => {
                has_hex_fingerprint(key, "dash-", 32)
                    || has_hex_fingerprint(key, "dash-live-", 32)
            },
        });
    video_key_is_valid && audio_key_is_valid
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
            choices
                .iter()
                .map(|choice| choice.height)
                .collect::<Vec<_>>(),
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
            choices
                .iter()
                .map(|choice| choice.height)
                .collect::<Vec<_>>(),
            vec![Some(1080), Some(1080), Some(1080), Some(720)],
            "choices should be ordered by descending height"
        );
        assert_eq!(
            choices
                .iter()
                .map(|choice| choice.key.as_str())
                .collect::<Vec<_>>(),
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
    fn hls_audio_choices_are_scoped_to_variant_and_preselect_default() {
        let body = br#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio-en",NAME="English",LANGUAGE="en",DEFAULT=YES,AUTOSELECT=YES,URI="audio/en.m3u8?sig=old"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio-en",NAME="French",LANGUAGE="fr",DEFAULT=NO,AUTOSELECT=YES,URI="audio/fr.m3u8?sig=old"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio-es",NAME="Spanish",LANGUAGE="es",DEFAULT=YES,AUTOSELECT=YES,URI="audio/es.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=1800000,RESOLUTION=1920x1080,AUDIO="audio-en"
video/en.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=900000,RESOLUTION=1280x720,AUDIO="audio-es"
video/es.m3u8
"#;
        let master = crate::manifest::parse_hls_master_playlist(
            "https://cdn.example.test/master.m3u8",
            body,
        )
        .expect("valid HLS master playlist");
        let video_keys = hls_variant_keys(&master.variants);
        let lookup = ManifestLookup {
            source_url: "https://cdn.example.test/master.m3u8".to_string(),
            manifest: NativeManifest::HlsMaster(master),
        };

        let english_choices = lookup.audio_choices(&video_keys[0]);
        assert_eq!(english_choices.len(), 2);
        assert!(english_choices[0].is_default);
        assert!(
            english_choices
                .iter()
                .any(|choice| choice.label.contains("French"))
        );
        assert_eq!(lookup.audio_choices(&video_keys[1]).len(), 1);

        let default_source = lookup
            .preferred_source("1080p")
            .expect("preferred HLS video and default audio");
        assert_eq!(default_source.representation_key, video_keys[0]);
        assert_eq!(
            default_source.audio_key.as_deref(),
            Some(english_choices[0].key.as_str())
        );

        let selected = lookup
            .selected_source(&video_keys[0], Some(&english_choices[1].key))
            .expect("selected alternate audio rendition");
        assert_eq!(
            selected.audio_key.as_deref(),
            Some(english_choices[1].key.as_str())
        );
        assert!(
            lookup
                .selected_source(&video_keys[1], Some(&english_choices[1].key))
                .is_none(),
            "an audio rendition must not be selectable from another variant's group"
        );
    }

    #[test]
    fn hls_audio_keys_ignore_rotating_signature_query_parameters() {
        let rendition = |url: &str| crate::manifest::HlsAudioRendition {
            url: Some(Url::parse(url).expect("valid audio rendition URL")),
            group_id: "audio-main".to_string(),
            name: "English".to_string(),
            language: Some("en".to_string()),
            is_default: true,
        };
        let original = rendition("https://cdn.example.test/audio/en.m3u8?sig=old");
        let refreshed = rendition("https://cdn.example.test/audio/en.m3u8?sig=new");

        assert_eq!(
            hls_audio_rendition_key(&original),
            hls_audio_rendition_key(&refreshed)
        );
    }

    #[test]
    fn dash_audio_choices_include_every_track_and_pin_the_main_role() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10S">
  <Period duration="PT10S">
    <AdaptationSet id="video" contentType="video" mimeType="video/mp4">
      <SegmentTemplate duration="5" media="video-$Number$.m4s"/>
      <Representation id="v1" bandwidth="1000000" width="1280" height="720" codecs="avc1.64001f"/>
    </AdaptationSet>
    <AdaptationSet id="audio-en" contentType="audio" mimeType="audio/mp4" lang="en">
      <Role schemeIdUri="urn:mpeg:dash:role:2011" value="main"/>
      <SegmentTemplate duration="5" media="audio-en-$Number$.m4s"/>
      <Representation id="a-en" bandwidth="128000" codecs="mp4a.40.2"/>
    </AdaptationSet>
    <AdaptationSet id="audio-fr" contentType="audio" mimeType="audio/mp4" lang="fr">
      <SegmentTemplate duration="5" media="audio-fr-$Number$.m4s"/>
      <Representation id="a-fr" bandwidth="192000" codecs="mp4a.40.2"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let dash = crate::manifest::dash::parse_dash_manifest(
            "https://cdn.example.test/manifest.mpd",
            body,
        )
        .expect("valid static DASH presentation");
        let video_key = dash.video_representations[0].key.clone();
        let main_audio_key = dash
            .default_audio()
            .expect("DASH main audio representation")
            .key
            .clone();
        let lookup = ManifestLookup {
            source_url: "https://cdn.example.test/manifest.mpd".to_string(),
            manifest: NativeManifest::Dash(dash),
        };

        let choices = lookup.audio_choices(&video_key);
        assert_eq!(choices.len(), 2);
        assert_eq!(choices[0].key, main_audio_key);
        assert!(choices[0].is_default);
        assert!(choices.iter().any(|choice| choice.label.contains("fr")));

        let selected = lookup
            .selected_source(&video_key, Some(&choices[1].key))
            .expect("select the alternate DASH audio representation");
        assert_eq!(selected.audio_key.as_deref(), Some(choices[1].key.as_str()));
        let preferred = lookup
            .preferred_source("720p")
            .expect("default DASH audio is selected automatically");
        assert_eq!(
            preferred.audio_key.as_deref(),
            Some(main_audio_key.as_str())
        );
    }

    #[test]
    fn manifest_source_audio_key_is_optional_and_validated() {
        let legacy: crate::media_types::ManifestSource =
            serde_json::from_value(serde_json::json!({
                "url": "https://cdn.example.test/master.m3u8",
                "kind": "Hls",
                "representation_key": "hls-media"
            }))
            .expect("legacy manifest source without an audio key remains compatible");
        assert_eq!(legacy.audio_key, None);

        let mut source = crate::media_types::ManifestSource {
            url: "https://cdn.example.test/master.m3u8".to_string(),
            kind: crate::media_types::ManifestKind::Hls,
            representation_key: "hls-variant-0123456789abcdef0123456789abcdef".to_string(),
            audio_key: Some("hls-audio-0123456789abcdef0123456789abcdef".to_string()),
        };
        assert!(valid_source(&source));
        source.audio_key = Some("not-a-stable-audio-key".to_string());
        assert!(!valid_source(&source));
        source.representation_key = "hls-media".to_string();
        source.audio_key = None;
        assert!(valid_source(&source));
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
                audio_key: None,
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

    #[tokio::test(flavor = "current_thread")]
    async fn manifest_lookup_follows_signed_redirect_with_proxy_and_cookie_scope() {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind manifest proxy fixture");
        let proxy_port = listener
            .local_addr()
            .expect("read manifest proxy address")
            .port();
        let seen_requests = Arc::new(Mutex::new(Vec::new()));
        let requests = Arc::clone(&seen_requests);
        let manifest_body = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1800000,RESOLUTION=1280x720\nvideo/720.m3u8?sig=child\n";
        let response_body = manifest_body.to_string();

        let mut proxy_server = tokio::spawn(async move {
            for request_index in 0..2 {
                let (mut socket, _) = listener.accept().await.expect("accept proxy request");
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0_u8; 1024];
                    let read = socket.read(&mut chunk).await.expect("read proxy request");
                    assert_ne!(read, 0, "proxy request ended before its headers");
                    request.extend_from_slice(&chunk[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                    assert!(
                        request.len() <= 16 * 1024,
                        "proxy request headers exceeded test limit"
                    );
                }
                requests
                    .lock()
                    .expect("lock captured proxy requests")
                    .push(String::from_utf8_lossy(&request).into_owned());

                let response = if request_index == 0 {
                    "HTTP/1.1 302 Found\r\nLocation: /signed/master.m3u8?sig=rotated\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        response_body.len(),
                        response_body
                    )
                };
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write proxy response");
            }
        });

        let proxy_url = format!("http://127.0.0.1:{proxy_port}");
        let proxy = crate::net_types::ResolvedProxy {
            proxies: vec![reqwest::Proxy::http(&proxy_url).expect("valid test proxy")],
            cli_url: proxy_url,
            no_proxy_env: String::new(),
        };
        let cookie_url = Url::parse("http://manifest.example.test/").expect("cookie origin URL");
        let jar = Arc::new(reqwest::cookie::Jar::default());
        jar.add_cookie_str("sid=manifest-fixture", &cookie_url);
        let client = crate::download_net::http_client_with_jar(Some(&proxy), jar)
            .expect("build manifest client with proxy and cookie provider");

        let lookup_result = tokio::time::timeout(
            Duration::from_secs(5),
            lookup_with_http_fixture_client(
                &client,
                "http://manifest.example.test/master.m3u8?sig=initial",
            ),
        )
        .await;

        let server_result = tokio::time::timeout(Duration::from_secs(5), &mut proxy_server).await;
        if server_result.is_err() {
            proxy_server.abort();
        }
        server_result
            .expect("proxy fixture server timed out")
            .expect("proxy fixture server task failed");

        let lookup = lookup_result
            .expect("manifest lookup timed out")
            .expect("manifest lookup request")
            .expect("fixture manifest should be supported");

        let NativeManifest::HlsMaster(master) = lookup.manifest else {
            panic!("fixture should parse as an HLS master playlist");
        };
        let variant = master
            .variants
            .first()
            .expect("fixture master contains a variant");
        assert_eq!(
            variant.url.as_str(),
            "https://manifest.example.test/signed/video/720.m3u8?sig=child",
            "relative child resources must resolve from the redirected, signed manifest URL"
        );

        let requests = seen_requests.lock().expect("lock captured proxy requests");
        assert_eq!(
            requests.len(),
            2,
            "the proxy should receive both redirect hops"
        );
        assert!(
            requests[0].starts_with("GET http://manifest.example.test/master.m3u8?sig=initial "),
            "first request should retain the original signed URL: {}",
            requests[0]
                .lines()
                .next()
                .unwrap_or("<missing request line>")
        );
        assert!(
            requests[1]
                .starts_with("GET http://manifest.example.test/signed/master.m3u8?sig=rotated "),
            "redirect request should use the refreshed signed URL: {}",
            requests[1]
                .lines()
                .next()
                .unwrap_or("<missing request line>")
        );
        for request in requests.iter() {
            assert!(
                request
                    .lines()
                    .any(|line| line.eq_ignore_ascii_case("cookie: sid=manifest-fixture")),
                "the cookie provider must apply the in-scope cookie on every redirect hop: {request}"
            );
        }
    }
}
