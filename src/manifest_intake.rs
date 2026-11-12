//! Bounded URL intake for native HLS and static DASH manifests.
//!
//! This module only fetches and parses the presentation description. Media
//! resources remain remote until the queue/fetch phase consumes the selected
//! representation.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

use gettextrs::gettext;
use url::Url;

#[cfg(test)]
use crate::manifest::HlsVariant;
use crate::manifest::{HlsMasterPlan, HlsPlan, MAX_MANIFEST_BYTES};

use crate::manifest_plan::{hls_audio_rendition_key, hls_subtitle_rendition_key, hls_variant_key};

/// Download-time manifest requests retain their normal network tolerance.
const MANIFEST_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Native preview yields to yt-dlp quickly when its transport is unsuitable.
const MANIFEST_PREVIEW_REQUEST_TIMEOUT: Duration = Duration::from_secs(4);
const MANIFEST_LOOKUP_BUDGET: Duration = Duration::from_secs(10);
const MAX_MANIFEST_ATTEMPTS: u32 = 3;
const MAX_PREVIEW_MANIFEST_ATTEMPTS: u32 = 2;
const MANIFEST_RETRY_BASE_DELAY: Duration = Duration::from_millis(250);

// Preview metadata is short-lived: HLS/DASH source manifests can change, and
// query-bearing HLS resources may include opaque auth or content-selection
// parameters. Dynamic presentations are never cached.
const MANIFEST_PREVIEW_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const MANIFEST_PREVIEW_CACHE_CAPACITY: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ManifestPreviewCacheKey {
    url: String,
    cookies_browser: String,
    handoff_cookie_hash: Option<[u8; 32]>,
    proxy: Option<String>,
}

/// Cookie source a manifest client actually used. A preview may only be cached
/// under a key describing the same source, or a failed cookie export would store
/// an anonymous response under a browser-profile key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManifestClientAuth {
    Anonymous,
    Handoff,
    Browser,
}

impl ManifestPreviewCacheKey {
    fn matches_auth(&self, auth: ManifestClientAuth) -> bool {
        match auth {
            ManifestClientAuth::Handoff => self.handoff_cookie_hash.is_some(),
            ManifestClientAuth::Browser => {
                self.handoff_cookie_hash.is_none()
                    && !self.cookies_browser.is_empty()
                    && self.cookies_browser != "none"
            }
            ManifestClientAuth::Anonymous => {
                self.handoff_cookie_hash.is_none()
                    && (self.cookies_browser.is_empty() || self.cookies_browser == "none")
            }
        }
    }
}

static MANIFEST_PREVIEW_CACHE: LazyLock<
    Mutex<HashMap<ManifestPreviewCacheKey, (Instant, ManifestLookup)>>,
> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Build a preview cache key from the URL, the effective cookie source, the
/// staged jar (if any) and the proxy.
///
/// A browser profile is keyed by the browser setting, never by cookie contents,
/// so a rotation is at most [`MANIFEST_PREVIEW_CACHE_TTL`] stale. Extension
/// handoff jars are keyed by cookie contents, so a re-handoff of the same
/// credentials reuses the preview despite a new staged path.
fn has_query_parameters(url: &Url) -> bool {
    url.query().is_some_and(|query| !query.is_empty())
}

fn manifest_preview_cache_key(
    raw_url: &str,
    cookies_browser: &str,
    proxy: Option<&crate::net_types::ResolvedProxy>,
) -> Option<ManifestPreviewCacheKey> {
    let mut url = Url::parse(raw_url).ok()?;
    url.set_fragment(None);
    if crate::video_cache::has_known_rotating_auth_query(&url) {
        return None;
    }

    let effective_browser =
        preview_cookie_browser(&url, cookies_browser, ManifestTransportPolicy::HttpsOnly);
    let handoff_cookie_hash = crate::extension_server::live_handoff_identity(raw_url)
        .and_then(|(path, _)| crate::extension_server::handoff_cookie_hash(&path));

    Some(ManifestPreviewCacheKey {
        url: url.to_string(),
        cookies_browser: effective_browser.to_string(),
        handoff_cookie_hash,
        proxy: proxy.map(|resolved| resolved.cli_url.clone()),
    })
}

fn manifest_preview_cache_get(key: &ManifestPreviewCacheKey) -> Option<ManifestLookup> {
    let mut cache = MANIFEST_PREVIEW_CACHE.lock().ok()?;
    let expired = cache
        .get(key)
        .is_some_and(|(cached_at, _)| cached_at.elapsed() >= MANIFEST_PREVIEW_CACHE_TTL);
    if expired {
        cache.remove(key);
        return None;
    }
    cache.get(key).map(|(_, lookup)| lookup.clone())
}

fn manifest_preview_cache_insert(key: ManifestPreviewCacheKey, lookup: &ManifestLookup) {
    if !is_cacheable_static_manifest(lookup) {
        return;
    }
    let Ok(mut cache) = MANIFEST_PREVIEW_CACHE.lock() else {
        return;
    };
    if cache.len() >= MANIFEST_PREVIEW_CACHE_CAPACITY && !cache.contains_key(&key) {
        cache.clear();
    }
    cache.insert(key, (Instant::now(), lookup.clone()));
}

fn is_cacheable_static_manifest(lookup: &ManifestLookup) -> bool {
    let Ok(source_url) = Url::parse(&lookup.source_url) else {
        return false;
    };
    let source_has_query = has_query_parameters(&source_url);

    match &lookup.manifest {
        // A master caches only short-lived track-selection metadata. The child
        // media playlist is still fetched at download time, so live windows are
        // never reused here. Any query-bearing child URL is excluded because its
        // parameter names and semantics are CDN-specific.
        crate::manifest_types::NativeManifest::HlsMaster(plan) => {
            !source_has_query
                && !plan.variants.iter().any(|variant| {
                    has_query_parameters(&variant.url)
                        || variant
                            .video_variant_url
                            .as_ref()
                            .is_some_and(has_query_parameters)
                })
                && !plan
                    .audio_renditions
                    .iter()
                    .filter_map(|rendition| rendition.url.as_ref())
                    .any(has_query_parameters)
                && !plan
                    .subtitle_renditions
                    .iter()
                    .any(|rendition| has_query_parameters(&rendition.url))
        }
        crate::manifest_types::NativeManifest::HlsMedia(plan) => {
            !source_has_query
                && !plan.is_live
                && !plan
                    .resources
                    .iter()
                    .any(|resource| has_query_parameters(&resource.url))
        }
        crate::manifest_types::NativeManifest::Dash(plan) => {
            matches!(
                &plan.presentation,
                crate::manifest::dash::DashPresentation::Static
            )
        }
    }
}

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

/// Manifest URL hints accepted for native intake before the response is read.
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

    let extension = url
        .path()
        .rsplit('/')
        .next()
        .and_then(|segment| segment.rsplit_once('.').map(|(_, extension)| extension));
    let from_extension = match extension {
        Some(extension) if extension.eq_ignore_ascii_case("m3u8") => {
            Some(crate::media_types::ManifestKind::Hls)
        }
        Some(extension) if extension.eq_ignore_ascii_case("mpd") => {
            Some(crate::media_types::ManifestKind::Dash)
        }
        _ => None,
    };
    from_extension.or_else(|| manifest_kind_from_query(url))
}

fn manifest_kind_from_query(url: &Url) -> Option<crate::media_types::ManifestKind> {
    url.query_pairs().find_map(|(key, value)| {
        if !matches!(
            key.to_ascii_lowercase().as_str(),
            "format" | "type" | "ext" | "manifest"
        ) {
            return None;
        }
        if matches!(
            value.to_ascii_lowercase().as_str(),
            "m3u8" | "hls" | "application/vnd.apple.mpegurl" | "application/x-mpegurl"
        ) {
            Some(crate::media_types::ManifestKind::Hls)
        } else if matches!(
            value.to_ascii_lowercase().as_str(),
            "mpd" | "dash" | "application/dash+xml"
        ) {
            Some(crate::media_types::ManifestKind::Dash)
        } else {
            None
        }
    })
}

/// Only common manifest endpoint names are probed without a file extension.
fn has_manifest_endpoint_name(url: &Url) -> bool {
    url.path_segments()
        .and_then(|mut segments| segments.next_back())
        .is_some_and(|segment| {
            ["master", "playlist", "manifest"]
                .iter()
                .any(|name| segment.eq_ignore_ascii_case(name))
        })
}

fn preview_cookie_browser<'a>(
    url: &Url,
    cookies_browser: &'a str,
    transport_policy: ManifestTransportPolicy,
) -> &'a str {
    if manifest_kind_for_transport(url, transport_policy).is_some() {
        cookies_browser
    } else {
        ""
    }
}

fn is_native_manifest_candidate(url: &Url, transport_policy: ManifestTransportPolicy) -> bool {
    manifest_kind_for_transport(url, transport_policy).is_some()
        || (transport_policy.accepts_url(url)
            && !crate::video_tools::is_youtube_url(url.as_str())
            && has_manifest_endpoint_name(url))
}

/// Inspect only the bounded response body; arbitrary HTML pages never enter
/// this path because the URL must already have a manifest hint or endpoint.
fn sniff_manifest_kind(body: &[u8]) -> Option<crate::media_types::ManifestKind> {
    let body = std::str::from_utf8(body).ok()?;
    let mut body = body.trim_start_matches('\u{feff}').trim_start();
    if body.starts_with("#EXTM3U") {
        return Some(crate::media_types::ManifestKind::Hls);
    }

    // Allow the ordinary XML declaration and comments before the MPD root,
    // but do not classify HTML or arbitrary text containing an MPD fragment.
    loop {
        body = body.trim_start();
        if let Some(rest) = body.strip_prefix("<?xml") {
            body = rest.split_once("?>")?.1;
        } else if let Some(rest) = body.strip_prefix("<!--") {
            body = rest.split_once("-->")?.1;
        } else {
            break;
        }
    }
    body.starts_with("<MPD")
        .then_some(crate::media_types::ManifestKind::Dash)
}

/// Whether a URL is eligible for the native HTTPS manifest lookup.
///
/// HTTPS manifest extensions, explicit format query hints, and common
/// manifest endpoints are eligible; ordinary pages remain on yt-dlp.
pub(crate) fn supports(raw_url: &str) -> bool {
    Url::parse(raw_url)
        .ok()
        .is_some_and(|url| is_native_manifest_candidate(&url, ManifestTransportPolicy::HttpsOnly))
}

pub(crate) use crate::manifest_plan::preferred_choice_index;
pub(crate) use crate::manifest_types::{
    ManifestAudioChoice, ManifestChoice, ManifestLookup, NativeManifest,
};

/// Preview a candidate HLS/DASH manifest using the configured browser's cookies.
///
/// The response is streamed into a bounded buffer. Unsupported manifest
/// responses preserve their reason so it can be shown if yt-dlp also fails.
/// The total preview budget includes browser-cookie export and manifest intake,
/// then yields to the established yt-dlp fallback.
pub(crate) async fn lookup(
    raw_url: &str,
    cookies_browser: &str,
    proxy: Option<&crate::net_types::ResolvedProxy>,
) -> Result<Option<ManifestLookup>, String> {
    let requested_url = Url::parse(raw_url).map_err(|_| gettext("Invalid stream manifest URL"))?;
    if !is_native_manifest_candidate(&requested_url, ManifestTransportPolicy::HttpsOnly) {
        return Ok(None);
    }
    let cache_key = manifest_preview_cache_key(raw_url, cookies_browser, proxy);
    if let Some(key) = cache_key.as_ref()
        && let Some(mut cached) = manifest_preview_cache_get(key)
    {
        // Cache identity is canonical, but the card's freshness check is tied
        // to the exact text the user entered for this lookup.
        cached.source_url = raw_url.to_string();
        tracing::debug!(url_host = %requested_url.host_str().unwrap_or_default(), "native manifest preview cache hit");
        return Ok(Some(cached));
    }

    let cookies_browser = preview_cookie_browser(
        &requested_url,
        cookies_browser,
        ManifestTransportPolicy::HttpsOnly,
    );
    let (auth, result) = tokio::time::timeout(MANIFEST_LOOKUP_BUDGET, async {
        let (client, auth) =
            client_for_manifest_with_policy(raw_url, cookies_browser, proxy).await?;
        let lookup = lookup_preview_with_client(&client, raw_url, proxy.is_some())
            .await
            .map_err(|error| error.to_string())?;
        Ok::<_, String>((auth, lookup))
    })
    .await
    .unwrap_or_else(|_| {
        Err(format!(
            "Native manifest lookup exceeded its {} second preview budget",
            MANIFEST_LOOKUP_BUDGET.as_secs()
        ))
    })?;
    if let (Some(key), Some(lookup)) = (cache_key, result.as_ref())
        && key.matches_auth(auth)
    {
        manifest_preview_cache_insert(key, lookup);
    }
    Ok(result)
}

/// Build an attempt-scoped client, preferring a valid extension handoff jar and
/// otherwise exporting the configured browser cookies just like the HTTP engine.
pub(crate) async fn client_for_manifest(
    raw_url: &str,
    cookies_browser: &str,
    proxy: Option<&crate::net_types::ResolvedProxy>,
) -> Result<reqwest::Client, String> {
    client_for_manifest_with_policy(raw_url, cookies_browser, proxy)
        .await
        .map(|(client, _)| client)
}

async fn client_for_manifest_with_policy(
    raw_url: &str,
    cookies_browser: &str,
    proxy: Option<&crate::net_types::ResolvedProxy>,
) -> Result<(reqwest::Client, ManifestClientAuth), String> {
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
        crate::extension_server::live_handoff_identity(raw_url)
        && let Ok(text) = tokio::fs::read_to_string(&path).await
    {
        let (jar, count) = crate::cookies::jar_from_export(&text);
        if count > 0 {
            let user_agent = handoff_user_agent
                .as_deref()
                .unwrap_or(crate::download_net::DEFAULT_USER_AGENT);
            return crate::download_net::http_client_with_jar_and_user_agent(
                proxy, jar, user_agent,
            )
            .map(|client| (client, ManifestClientAuth::Handoff));
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
                return crate::download_net::http_client_with_jar(proxy, jar)
                    .map(|client| (client, ManifestClientAuth::Browser));
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
        .map(|client| (client, ManifestClientAuth::Anonymous))
        .map_err(|_| gettext("The configured proxy could not be used for this manifest"))
}

/// Fetch a manifest with an existing per-attempt client so manifests and media
/// resources share the same proxy and DNS policy.
/// Resolve a manifest for a queued download, retaining the download-time
/// network timeout and retry policy.
pub(crate) async fn lookup_with_client(
    client: &reqwest::Client,
    raw_url: &str,
    proxy_configured: bool,
) -> Result<Option<ManifestLookup>, ManifestIntakeError> {
    lookup_with_client_policy(
        client,
        raw_url,
        ManifestTransportPolicy::HttpsOnly,
        ManifestFetchMode::Download,
        proxy_configured,
    )
    .await
}

/// Resolve a manifest for the Add-card preview using its shorter fallback budget.
pub(crate) async fn lookup_preview_with_client(
    client: &reqwest::Client,
    raw_url: &str,
    proxy_configured: bool,
) -> Result<Option<ManifestLookup>, ManifestIntakeError> {
    lookup_with_client_policy(
        client,
        raw_url,
        ManifestTransportPolicy::HttpsOnly,
        ManifestFetchMode::Preview,
        proxy_configured,
    )
    .await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManifestFetchMode {
    Preview,
    Download,
}

fn validate_hls_stable_track_ids(master: &HlsMasterPlan) -> Result<(), ManifestIntakeError> {
    // Every track gets a key: an Apple stable ID when the origin provides one,
    // else a derived content identity (stable across CDN and signed-URL
    // rotation). Only genuinely duplicate identities are rejected, because a
    // persisted selection could then resolve to the wrong track.
    let mut variant_keys = HashSet::with_capacity(master.variants.len());
    if !master
        .variants
        .iter()
        .all(|variant| variant_keys.insert(hls_variant_key(variant)))
    {
        return Err(ManifestIntakeError::Unsupported(gettext(
            "The HLS master playlist has duplicate stable video track identities",
        )));
    }

    let audio_groups = master
        .variants
        .iter()
        .filter_map(|variant| variant.audio_group.as_deref())
        .collect::<HashSet<_>>();
    let mut rendition_keys = HashSet::with_capacity(master.audio_renditions.len());
    if !master
        .audio_renditions
        .iter()
        .filter(|rendition| audio_groups.contains(rendition.group_id.as_str()))
        .all(|rendition| rendition_keys.insert(hls_audio_rendition_key(rendition)))
    {
        return Err(ManifestIntakeError::Unsupported(gettext(
            "The HLS master playlist has duplicate stable audio track identities",
        )));
    }

    let subtitle_groups = master
        .variants
        .iter()
        .filter_map(|variant| variant.subtitles_group.as_deref())
        .collect::<HashSet<_>>();
    let mut subtitle_keys = HashSet::with_capacity(master.subtitle_renditions.len());
    if !master
        .subtitle_renditions
        .iter()
        .filter(|rendition| subtitle_groups.contains(rendition.group_id.as_str()))
        .all(|rendition| subtitle_keys.insert(hls_subtitle_rendition_key(rendition)))
    {
        return Err(ManifestIntakeError::Unsupported(gettext(
            "The HLS master playlist has duplicate stable subtitle track identities",
        )));
    }

    Ok(())
}

/// Attach the master's signed query to selected child URLs on the same origin.
fn inherit_hls_master_query_parameters(
    master_url: &Url,
    master: &mut crate::manifest::HlsMasterPlan,
) {
    for variant in master.variants.iter_mut() {
        crate::manifest::inherit_query_parameters_same_origin(master_url, &mut variant.url);
        if let Some(video_variant_url) = variant.video_variant_url.as_mut() {
            crate::manifest::inherit_query_parameters_same_origin(master_url, video_variant_url);
        }
    }
    for rendition in master.audio_renditions.iter_mut() {
        if let Some(url) = rendition.url.as_mut() {
            crate::manifest::inherit_query_parameters_same_origin(master_url, url);
        }
    }
    for rendition in master.subtitle_renditions.iter_mut() {
        crate::manifest::inherit_query_parameters_same_origin(master_url, &mut rendition.url);
    }
}

async fn lookup_with_client_policy(
    client: &reqwest::Client,
    raw_url: &str,
    transport_policy: ManifestTransportPolicy,
    fetch_mode: ManifestFetchMode,
    _proxy_configured: bool,
) -> Result<Option<ManifestLookup>, ManifestIntakeError> {
    let requested_url = Url::parse(raw_url)
        .map_err(|_| ManifestIntakeError::Failed(gettext("Invalid stream manifest URL")))?;
    if !is_native_manifest_candidate(&requested_url, transport_policy) {
        return Ok(None);
    }
    let manifest_kind_hint = manifest_kind_for_transport(&requested_url, transport_policy);
    let original_request_url = requested_url.clone();
    let (final_url, body, server_now) = match fetch_mode {
        ManifestFetchMode::Preview => {
            fetch_bounded_preview_manifest_with_policy(client, requested_url, transport_policy)
                .await?
        }
        ManifestFetchMode::Download => {
            fetch_bounded_manifest_with_policy(client, requested_url, transport_policy).await?
        }
    };
    let mut final_url = transport_policy.planning_url(final_url)?;
    let kind = manifest_kind_hint
        .or_else(|| sniff_manifest_kind(&body))
        .ok_or_else(|| {
            ManifestIntakeError::Unsupported(
                "The response is not an HLS playlist or DASH MPD".to_string(),
            )
        })?;
    let manifest = match kind {
        crate::media_types::ManifestKind::Hls => {
            crate::manifest::inherit_query_parameters_same_origin_on_redirect(
                &original_request_url,
                &mut final_url,
            );
            match crate::manifest::parse_hls_master_playlist(final_url.as_str(), &body) {
                Ok(mut master) => {
                    inherit_hls_master_query_parameters(&final_url, &mut master);
                    validate_hls_stable_track_ids(&master)?;
                    NativeManifest::HlsMaster(Box::new(master))
                }
                Err(crate::manifest::ManifestError::ExpectedMasterPlaylist) => {
                    let mut plan =
                        crate::manifest::plan_hls_media_playlist(final_url.as_str(), &body)
                            .map_err(|error| {
                                ManifestIntakeError::Unsupported(format!(
                                    "The HLS media playlist is not supported: {error}"
                                ))
                            })?;
                    plan.requested_playlist_url = original_request_url;
                    NativeManifest::HlsMedia(Box::new(plan))
                }
                Err(error) => {
                    return Err(ManifestIntakeError::Unsupported(format!(
                        "The HLS master playlist is not supported: {error}"
                    )));
                }
            }
        }
        crate::media_types::ManifestKind::Dash => {
            let body = std::str::from_utf8(&body).map_err(|error| {
                ManifestIntakeError::Unsupported(format!(
                    "The DASH manifest is not valid UTF-8: {error}"
                ))
            })?;
            let plan =
                crate::manifest::dash::parse_dash_manifest_at(final_url.as_str(), body, server_now)
                    .map_err(|error| {
                        ManifestIntakeError::Unsupported(format!(
                            "The DASH presentation is not supported: {error}"
                        ))
                    })?;
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
    lookup_with_client_policy(
        client,
        raw_url,
        ManifestTransportPolicy::HttpFixture,
        ManifestFetchMode::Preview,
        false,
    )
    .await
}

/// Fetch and parse an HLS child media playlist with explicit parent variable imports.
pub(crate) async fn lookup_hls_media_with_variables(
    client: &reqwest::Client,
    raw_url: &str,
    inherited_variables: &[(String, String)],
) -> Result<HlsPlan, ManifestIntakeError> {
    let requested_url = Url::parse(raw_url)
        .map_err(|_| ManifestIntakeError::Failed(gettext("Invalid stream manifest URL")))?;
    let (mut final_url, body, _) = fetch_bounded_manifest(client, requested_url.clone()).await?;
    crate::manifest::inherit_query_parameters_same_origin_on_redirect(
        &requested_url,
        &mut final_url,
    );
    let mut plan = crate::manifest::plan_hls_media_playlist_with_variables(
        final_url.as_str(),
        &body,
        inherited_variables,
    )
    .map_err(|error| ManifestIntakeError::Unsupported(error.to_string()))?;
    plan.requested_playlist_url = requested_url;
    Ok(plan)
}

/// Poll one live HLS playlist snapshot. Blocking reloads use a single request:
/// transient failures return to the live-capture state machine without retries.
pub(crate) async fn lookup_hls_live_media(
    client: &reqwest::Client,
    raw_url: &str,
    blocking_reload: Option<(u64, Option<u64>)>,
    can_skip_until: Option<Duration>,
    inherited_variables: &[(String, String)],
) -> Result<HlsPlan, ManifestIntakeError> {
    let requested_url = hls_live_reload_url(raw_url, blocking_reload, can_skip_until)?;
    let authorization_url = strip_hls_blocking_reload_query(&requested_url);
    let (final_url, body, _) = fetch_bounded_manifest_once(
        client,
        &requested_url,
        ManifestTransportPolicy::HttpsOnly,
        MANIFEST_REQUEST_TIMEOUT,
        ManifestFetchMode::Download,
    )
    .await?;
    let mut final_url = strip_hls_blocking_reload_query(&final_url);
    crate::manifest::inherit_query_parameters_same_origin_on_redirect(
        &authorization_url,
        &mut final_url,
    );
    let mut plan = crate::manifest::plan_hls_media_playlist_with_variables(
        final_url.as_str(),
        &body,
        inherited_variables,
    )
    .map_err(|error| ManifestIntakeError::Unsupported(error.to_string()))?;
    plan.requested_playlist_url = authorization_url;
    Ok(plan)
}

fn hls_live_reload_url(
    raw_url: &str,
    blocking_reload: Option<(u64, Option<u64>)>,
    can_skip_until: Option<Duration>,
) -> Result<Url, ManifestIntakeError> {
    let mut requested_url = Url::parse(raw_url)
        .map_err(|_| ManifestIntakeError::Failed(gettext("Invalid stream manifest URL")))?;
    let mut reload_parameters = Vec::with_capacity(3);
    if let Some((media_sequence, part_index)) = blocking_reload {
        reload_parameters.push(format!("_HLS_msn={media_sequence}"));
        if let Some(part_index) = part_index {
            reload_parameters.push(format!("_HLS_part={part_index}"));
        }
    }
    if can_skip_until.is_some() {
        reload_parameters.push("_HLS_skip=YES".to_string());
    }
    if !reload_parameters.is_empty() {
        let existing_query = requested_url.query().unwrap_or_default();
        let query = if existing_query.is_empty() {
            reload_parameters.join("&")
        } else {
            format!("{existing_query}&{}", reload_parameters.join("&"))
        };
        requested_url.set_query(Some(&query));
    }
    Ok(requested_url)
}

fn strip_hls_blocking_reload_query(url: &Url) -> Url {
    let Some(query) = url.query() else {
        return url.clone();
    };
    let retained = query
        .split('&')
        .filter(|pair| {
            let name = pair.split_once('=').map_or(*pair, |(name, _)| name);
            let decoded_name = percent_encoding::percent_decode_str(name).collect::<Vec<_>>();
            !matches!(
                decoded_name.as_slice(),
                b"_HLS_msn" | b"_HLS_part" | b"_HLS_skip"
            )
        })
        .collect::<Vec<_>>();
    let retained_query = retained.join("&");
    let mut clean = url.clone();
    clean.set_query((!retained_query.is_empty()).then_some(retained_query.as_str()));
    clean
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
    fetch_bounded_manifest_with_limits(
        client,
        requested_url,
        transport_policy,
        MANIFEST_REQUEST_TIMEOUT,
        MAX_MANIFEST_ATTEMPTS,
        ManifestFetchMode::Download,
    )
    .await
}

async fn fetch_bounded_preview_manifest_with_policy(
    client: &reqwest::Client,
    requested_url: Url,
    transport_policy: ManifestTransportPolicy,
) -> Result<(Url, Vec<u8>, SystemTime), ManifestIntakeError> {
    fetch_bounded_preview_manifest_with_limits(
        client,
        requested_url,
        transport_policy,
        MANIFEST_LOOKUP_BUDGET,
        MANIFEST_PREVIEW_REQUEST_TIMEOUT,
        MAX_PREVIEW_MANIFEST_ATTEMPTS,
    )
    .await
}

async fn fetch_bounded_preview_manifest_with_limits(
    client: &reqwest::Client,
    requested_url: Url,
    transport_policy: ManifestTransportPolicy,
    preview_budget: Duration,
    request_timeout: Duration,
    max_attempts: u32,
) -> Result<(Url, Vec<u8>, SystemTime), ManifestIntakeError> {
    tokio::time::timeout(
        preview_budget,
        fetch_bounded_manifest_with_limits(
            client,
            requested_url,
            transport_policy,
            request_timeout,
            max_attempts,
            ManifestFetchMode::Preview,
        ),
    )
    .await
    .unwrap_or_else(|_| {
        Err(ManifestIntakeError::Unsupported(format!(
            "Native manifest lookup exceeded its {} ms preview budget",
            preview_budget.as_millis()
        )))
    })
}

async fn fetch_bounded_manifest_with_limits(
    client: &reqwest::Client,
    requested_url: Url,
    transport_policy: ManifestTransportPolicy,
    request_timeout: Duration,
    max_attempts: u32,
    fetch_mode: ManifestFetchMode,
) -> Result<(Url, Vec<u8>, SystemTime), ManifestIntakeError> {
    fetch_bounded_manifest_with_limits_and_size(
        client,
        requested_url,
        transport_policy,
        request_timeout,
        max_attempts,
        fetch_mode,
        MAX_MANIFEST_BYTES,
    )
    .await
}

async fn fetch_bounded_manifest_with_limits_and_size(
    client: &reqwest::Client,
    requested_url: Url,
    transport_policy: ManifestTransportPolicy,
    request_timeout: Duration,
    max_attempts: u32,
    fetch_mode: ManifestFetchMode,
    max_bytes: usize,
) -> Result<(Url, Vec<u8>, SystemTime), ManifestIntakeError> {
    if !transport_policy.accepts_url(&requested_url) {
        return Err(ManifestIntakeError::Failed(gettext(
            "Invalid stream manifest URL",
        )));
    }

    let mut attempt = 0_u32;
    loop {
        attempt += 1;
        match fetch_bounded_manifest_once_with_limit(
            client,
            &requested_url,
            transport_policy,
            request_timeout,
            fetch_mode,
            max_bytes,
        )
        .await
        {
            Err(ManifestIntakeError::Retryable(error)) if attempt < max_attempts => {
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

/// Only request timeouts and connection resets are worth retrying. DNS guard,
/// TLS, redirect-policy and other deterministic failures fall back immediately.
fn manifest_request_error(context: &str, error: reqwest::Error) -> ManifestIntakeError {
    let timed_out = error.is_timeout();
    let connection_reset = has_connection_reset_cause(&error);
    let retryable = timed_out || connection_reset;
    let detail = if timed_out {
        "the request timed out".to_string()
    } else if connection_reset {
        "the connection was reset".to_string()
    } else if error.is_dns() {
        "DNS resolution failed".to_string()
    } else {
        // Reqwest errors may carry the full signed manifest URL. Strip it
        // before using the transport's detailed diagnostic.
        error.without_url().to_string()
    };
    let message = format!("{context}: {detail}");
    if retryable {
        ManifestIntakeError::Retryable(message)
    } else {
        ManifestIntakeError::Unsupported(message)
    }
}

fn has_connection_reset_cause(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(cause) = current {
        if cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::ConnectionReset)
        {
            return true;
        }
        current = std::error::Error::source(cause);
    }
    false
}

fn manifest_status_error(
    status: reqwest::StatusCode,
    fetch_mode: ManifestFetchMode,
) -> ManifestIntakeError {
    let message = format!("The stream server returned HTTP {}", status.as_u16());
    let retryable = fetch_mode == ManifestFetchMode::Download
        && (status == reqwest::StatusCode::REQUEST_TIMEOUT
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || status.is_server_error());
    if retryable {
        ManifestIntakeError::Retryable(message)
    } else {
        ManifestIntakeError::Unsupported(message)
    }
}

async fn fetch_bounded_manifest_once(
    client: &reqwest::Client,
    requested_url: &Url,
    transport_policy: ManifestTransportPolicy,
    request_timeout: Duration,
    fetch_mode: ManifestFetchMode,
) -> Result<(Url, Vec<u8>, SystemTime), ManifestIntakeError> {
    fetch_bounded_manifest_once_with_limit(
        client,
        requested_url,
        transport_policy,
        request_timeout,
        fetch_mode,
        MAX_MANIFEST_BYTES,
    )
    .await
}

async fn fetch_bounded_manifest_once_with_limit(
    client: &reqwest::Client,
    requested_url: &Url,
    transport_policy: ManifestTransportPolicy,
    request_timeout: Duration,
    fetch_mode: ManifestFetchMode,
    max_bytes: usize,
) -> Result<(Url, Vec<u8>, SystemTime), ManifestIntakeError> {
    let response = client
        .get(requested_url.clone())
        .timeout(request_timeout)
        .send()
        .await
        .map_err(|error| manifest_request_error("Could not retrieve the stream manifest", error))?;
    let status = response.status();
    if !status.is_success() {
        return Err(manifest_status_error(status, fetch_mode));
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
    let body = read_bounded_response_with_limit(response, max_bytes, "stream manifest").await?;
    Ok((final_url, body, server_now))
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
    if !is_native_manifest_candidate(&url, ManifestTransportPolicy::HttpsOnly)
        || native_manifest_kind(&url).is_some_and(|kind| kind != source.kind)
        || source.representation_key.is_empty()
        || source
            .output_extension
            .as_deref()
            .is_some_and(|extension| !matches!(extension, "mp4" | "mkv" | "ts"))
    {
        return false;
    }
    let video_key_is_valid = match source.kind {
        crate::media_types::ManifestKind::Hls => {
            (source.representation_key.as_str() == "hls-media"
                && source.audio_key.is_none()
                && source.subtitle_key.is_none())
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
                has_hex_fingerprint(key, "dash-", 32) || has_hex_fingerprint(key, "dash-live-", 32)
            }
        });
    let subtitle_key_is_valid =
        source
            .subtitle_key
            .as_deref()
            .is_none_or(|key| match source.kind {
                crate::media_types::ManifestKind::Hls => {
                    has_hex_fingerprint(key, "hls-subtitle-", 32)
                }
                crate::media_types::ManifestKind::Dash => {
                    has_hex_fingerprint(key, "dash-", 32)
                        || has_hex_fingerprint(key, "dash-live-", 32)
                }
            });
    video_key_is_valid && audio_key_is_valid && subtitle_key_is_valid
}

async fn read_bounded_response_with_limit(
    mut response: reqwest::Response,
    max_bytes: usize,
    context: &str,
) -> Result<Vec<u8>, ManifestIntakeError> {
    if response
        .content_length()
        .is_some_and(|length| usize::try_from(length).map_or(true, |length| length > max_bytes))
    {
        return Err(ManifestIntakeError::Unsupported(format!(
            "The {context} exceeds its byte limit"
        )));
    }

    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| manifest_request_error(&format!("Could not read the {context}"), error))?
    {
        if body.len().saturating_add(chunk.len()) > max_bytes {
            return Err(ManifestIntakeError::Unsupported(format!(
                "The {context} exceeds its byte limit"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
#[path = "manifest_intake_tests.rs"]
mod tests;

#[cfg(test)]
mod hls_live_reload_url_tests {
    use super::*;

    #[test]
    fn blocking_reload_without_an_open_part_omits_hls_part() {
        let url = hls_live_reload_url(
            "https://cdn.example.test/live.m3u8?token=secret",
            Some((314, None)),
            None,
        )
        .expect("build blocking reload URL");
        let query = url.query_pairs().collect::<Vec<_>>();
        assert!(
            query
                .iter()
                .any(|(name, value)| name == "_HLS_msn" && value == "314")
        );
        assert!(!query.iter().any(|(name, _)| name == "_HLS_part"));
        assert!(!query.iter().any(|(name, _)| name == "_HLS_skip"));
        assert!(
            query
                .iter()
                .any(|(name, value)| name == "token" && value == "secret")
        );
    }

    #[test]
    fn delta_reload_adds_skip_and_optional_part_parameters() {
        let url = hls_live_reload_url(
            "https://cdn.example.test/live.m3u8",
            Some((315, Some(2))),
            Some(Duration::from_secs(24)),
        )
        .expect("build delta blocking reload URL");
        let query = url.query_pairs().collect::<Vec<_>>();
        assert!(
            query
                .iter()
                .any(|(name, value)| name == "_HLS_msn" && value == "315")
        );
        assert!(
            query
                .iter()
                .any(|(name, value)| name == "_HLS_part" && value == "2")
        );
        assert!(
            query
                .iter()
                .any(|(name, value)| name == "_HLS_skip" && value == "YES")
        );
    }

    #[test]
    fn stripping_reload_parameters_preserves_signed_origin_query() {
        let url = Url::parse(
            "https://cdn.example.test/live.m3u8?token=secret&_HLS_msn=10&_HLS_part=1&_HLS_skip=YES",
        )
        .expect("fixture URL");
        let clean = strip_hls_blocking_reload_query(&url);
        assert_eq!(clean.query(), Some("token=secret"));
    }
}
#[cfg(test)]
mod manifest_intake_tests {
    use super::*;

    #[test]
    fn stripping_blocking_reload_parameters_preserves_raw_query_encoding() {
        let url = Url::parse(
            "https://cdn.example.test/live.m3u8?sig=a%2Fb&label=hello%20world&tilde=~&flag&_HLS_msn=12&_HLS_part=3&_HLS_skip=YES",
        )
        .expect("live playlist URL");
        let cleaned = strip_hls_blocking_reload_query(&url);
        assert_eq!(
            cleaned.query(),
            Some("sig=a%2Fb&label=hello%20world&tilde=~&flag"),
            "the retained query must remain byte-for-byte identical",
        );

        let encoded_control =
            Url::parse("https://cdn.example.test/live.m3u8?sig=keep&%5FHLS_msn=12")
                .expect("encoded blocking reload URL");
        assert_eq!(
            strip_hls_blocking_reload_query(&encoded_control).query(),
            Some("sig=keep"),
        );
    }

    #[test]
    fn live_reload_parameters_append_without_reencoding_opaque_query() {
        let url = hls_live_reload_url(
            "https://cdn.example.test/live.m3u8?sig=a%2Fb&label=hello%20world&tilde=~&flag",
            Some((42, Some(3))),
            Some(Duration::from_secs(6)),
        )
        .expect("live reload URL");
        assert_eq!(
            url.query(),
            Some(
                "sig=a%2Fb&label=hello%20world&tilde=~&flag&_HLS_msn=42&_HLS_part=3&_HLS_skip=YES"
            ),
        );
    }
}
