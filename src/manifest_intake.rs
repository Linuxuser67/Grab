//! Bounded URL intake for native HLS and static DASH manifests.
//!
//! This module only fetches and parses the presentation description. Media
//! resources remain remote until the queue/fetch phase consumes the selected
//! representation.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

use gettextrs::gettext;
use url::Url;

#[cfg(test)]
use crate::manifest::HlsVariant;
use crate::manifest::{HlsMasterPlan, HlsPlan, MAX_MANIFEST_BYTES};

#[path = "manifest_dash_xlink.rs"]
mod dash_xlink;

const MAX_DASH_XLINK_COUNT: usize = 16;
use crate::manifest_plan::{hls_audio_rendition_key, hls_subtitle_rendition_key, hls_variant_key};

/// Download-time manifest requests retain their normal network tolerance.
const MANIFEST_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Native preview yields to yt-dlp quickly when its transport is unsuitable.
const MANIFEST_PREVIEW_REQUEST_TIMEOUT: Duration = Duration::from_secs(4);
const MANIFEST_LOOKUP_BUDGET: Duration = Duration::from_secs(10);
const MAX_MANIFEST_ATTEMPTS: u32 = 3;
const MAX_PREVIEW_MANIFEST_ATTEMPTS: u32 = 2;
const MANIFEST_RETRY_BASE_DELAY: Duration = Duration::from_millis(250);

const MAX_HLS_STEERING_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_HLS_STEERING_PATHWAY_CLONES: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
struct HlsPathwayClone {
    base_id: String,
    id: String,
    host: Option<String>,
    params: BTreeMap<String, String>,
    per_variant_uris: HashMap<String, String>,
    per_rendition_uris: HashMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HlsSteeringManifest {
    pathway_priority: Vec<String>,
    pathway_clones: Vec<HlsPathwayClone>,
}

// Preview metadata is short-lived: HLS/DASH source manifests can change, and
// signed child URLs may expire. Dynamic presentations are never cached.
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
fn manifest_preview_cache_key(
    raw_url: &str,
    cookies_browser: &str,
    proxy: Option<&crate::net_types::ResolvedProxy>,
) -> Option<ManifestPreviewCacheKey> {
    let mut url = Url::parse(raw_url).ok()?;
    url.set_fragment(None);
    if crate::video_cache::has_rotating_auth_query(&url) {
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
    match &lookup.manifest {
        // A master caches only short-lived track-selection metadata. The child
        // media playlist is still fetched at download time, so live windows are
        // never reused here. Signed child URLs are excluded.
        crate::manifest_types::NativeManifest::HlsMaster(plan) => {
            plan.content_steering.is_none()
                && !plan
                    .variants
                    .iter()
                    .any(|variant| crate::video_cache::has_rotating_auth_query(&variant.url))
                && !plan
                    .audio_renditions
                    .iter()
                    .filter_map(|rendition| rendition.url.as_ref())
                    .any(crate::video_cache::has_rotating_auth_query)
                && !plan
                    .subtitle_renditions
                    .iter()
                    .any(|rendition| crate::video_cache::has_rotating_auth_query(&rendition.url))
        }
        crate::manifest_types::NativeManifest::HlsMedia(plan) => {
            !plan.is_live
                && !plan
                    .resources
                    .iter()
                    .any(|resource| crate::video_cache::has_rotating_auth_query(&resource.url))
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

#[derive(Debug, Default, PartialEq, Eq)]
struct PathwayStableTrackIds {
    variants: HashSet<(Option<String>, Option<String>)>,
    audio: HashSet<String>,
    subtitles: HashSet<String>,
}

fn validate_hls_content_steering_stable_ids(
    master: &crate::manifest::HlsMasterPlan,
) -> Result<(), ManifestIntakeError> {
    let mut by_pathway = HashMap::<String, PathwayStableTrackIds>::new();
    for variant in &master.variants {
        if variant.stable_variant_id.is_none() && variant.stable_video_rendition_id.is_none() {
            return Err(ManifestIntakeError::Unsupported(gettext(
                "Content Steering requires stable IDs for every downloadable video rendition",
            )));
        }
        if variant.video_rendition_group.is_some() && variant.stable_video_rendition_id.is_none() {
            return Err(ManifestIntakeError::Unsupported(gettext(
                "Content Steering requires stable IDs for alternate video renditions",
            )));
        }
        let identity = by_pathway.entry(variant.pathway_id.clone()).or_default();
        identity.variants.insert((
            variant.stable_variant_id.clone(),
            variant.stable_video_rendition_id.clone(),
        ));
        if let Some(group_id) = variant.audio_group.as_deref() {
            for rendition in master
                .audio_renditions
                .iter()
                .filter(|rendition| rendition.group_id == group_id && rendition.url.is_some())
            {
                let stable_id = rendition.stable_rendition_id.as_ref().ok_or_else(|| {
                    ManifestIntakeError::Unsupported(gettext(
                        "Content Steering requires stable IDs for external audio renditions",
                    ))
                })?;
                identity.audio.insert(stable_id.clone());
            }
        }
        if let Some(group_id) = variant.subtitles_group.as_deref() {
            for rendition in master
                .subtitle_renditions
                .iter()
                .filter(|rendition| rendition.group_id == group_id)
            {
                let stable_id = rendition.stable_rendition_id.as_ref().ok_or_else(|| {
                    ManifestIntakeError::Unsupported(gettext(
                        "Content Steering requires stable IDs for subtitle renditions",
                    ))
                })?;
                identity.subtitles.insert(stable_id.clone());
            }
        }
    }
    let mut pathways = by_pathway.values();
    let expected = pathways.next().ok_or_else(|| {
        ManifestIntakeError::Unsupported(gettext(
            "Content Steering master playlist has no downloadable Pathways",
        ))
    })?;
    if pathways.any(|identity| identity != expected) {
        return Err(ManifestIntakeError::Unsupported(gettext(
            "Content Steering Pathways do not advertise matching stable rendition identities",
        )));
    }
    Ok(())
}

fn parse_hls_steering_manifest(body: &[u8]) -> Result<HlsSteeringManifest, ManifestIntakeError> {
    if body.len() > MAX_HLS_STEERING_MANIFEST_BYTES {
        return Err(ManifestIntakeError::Unsupported(
            "The HLS Content Steering manifest exceeds its byte limit".to_string(),
        ));
    }
    let value: serde_json::Value = serde_json::from_slice(body).map_err(|error| {
        ManifestIntakeError::Unsupported(format!(
            "The HLS Content Steering manifest is not valid JSON: {error}"
        ))
    })?;
    if value.get("VERSION").and_then(serde_json::Value::as_u64) != Some(1) {
        return Err(ManifestIntakeError::Unsupported(
            "The HLS Content Steering manifest has an unsupported VERSION".to_string(),
        ));
    }
    if value
        .get("TTL")
        .and_then(serde_json::Value::as_u64)
        .is_none()
    {
        return Err(ManifestIntakeError::Unsupported(
            "The HLS Content Steering manifest has an invalid TTL".to_string(),
        ));
    }
    if value
        .get("RELOAD-URI")
        .is_some_and(|reload| reload.as_str().is_none())
    {
        return Err(ManifestIntakeError::Unsupported(
            "The HLS Content Steering RELOAD-URI is invalid".to_string(),
        ));
    }

    let priority = value
        .get("PATHWAY-PRIORITY")
        .and_then(serde_json::Value::as_array)
        .filter(|priority| !priority.is_empty())
        .ok_or_else(|| {
            ManifestIntakeError::Unsupported(
                "The HLS Content Steering PATHWAY-PRIORITY list is missing or empty".to_string(),
            )
        })?;
    let mut seen = HashSet::with_capacity(priority.len());
    let mut pathway_ids = Vec::with_capacity(priority.len());
    for item in priority {
        let pathway_id = item.as_str().ok_or_else(|| {
            ManifestIntakeError::Unsupported(
                "The HLS Content Steering PATHWAY-PRIORITY list contains a non-string ID"
                    .to_string(),
            )
        })?;
        if !crate::manifest::is_valid_hls_pathway_id(pathway_id) {
            return Err(ManifestIntakeError::Unsupported(
                "The HLS Content Steering PATHWAY-PRIORITY list contains an invalid ID".to_string(),
            ));
        }
        if !seen.insert(pathway_id.to_string()) {
            return Err(ManifestIntakeError::Unsupported(
                "The HLS Content Steering PATHWAY-PRIORITY list contains duplicate IDs".to_string(),
            ));
        }
        pathway_ids.push(pathway_id.to_string());
    }

    let clones: Vec<HlsPathwayClone> = value
        .get("PATHWAY-CLONES")
        .map(|value| {
            let clones = value.as_array().ok_or_else(|| {
                ManifestIntakeError::Unsupported(
                    "The HLS Content Steering PATHWAY-CLONES member is invalid".to_string(),
                )
            })?;
            if clones.is_empty() || clones.len() > MAX_HLS_STEERING_PATHWAY_CLONES {
                return Err(ManifestIntakeError::Unsupported(
                    "The HLS Content Steering PATHWAY-CLONES count is outside its supported limit"
                        .to_string(),
                ));
            }
            clones.iter().map(parse_hls_pathway_clone).collect()
        })
        .transpose()?
        .unwrap_or_default();
    let mut clone_ids = HashSet::with_capacity(clones.len());
    for clone in &clones {
        if !clone_ids.insert(clone.id.clone()) {
            return Err(ManifestIntakeError::Unsupported(
                "The HLS Content Steering PATHWAY-CLONES list contains duplicate IDs".to_string(),
            ));
        }
    }

    Ok(HlsSteeringManifest {
        pathway_priority: pathway_ids,
        pathway_clones: clones,
    })
}

fn parse_hls_pathway_clone(
    value: &serde_json::Value,
) -> Result<HlsPathwayClone, ManifestIntakeError> {
    let invalid = || {
        ManifestIntakeError::Unsupported(
            "The HLS Content Steering PATHWAY-CLONES member is invalid".to_string(),
        )
    };
    let base_id = value
        .get("BASE-ID")
        .and_then(serde_json::Value::as_str)
        .filter(|value| crate::manifest::is_valid_hls_pathway_id(value))
        .ok_or_else(invalid)?
        .to_string();
    let id = value
        .get("ID")
        .and_then(serde_json::Value::as_str)
        .filter(|value| crate::manifest::is_valid_hls_pathway_id(value))
        .ok_or_else(invalid)?
        .to_string();
    if id == base_id {
        return Err(invalid());
    }
    let replacement = value
        .get("URI-REPLACEMENT")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(invalid)?;
    let host = replacement
        .get("HOST")
        .map(|value| {
            let host = value
                .as_str()
                .filter(|host| !host.is_empty())
                .ok_or_else(invalid)?;
            let probe = Url::parse(&format!("https://{host}/")).map_err(|_| invalid())?;
            let has_explicit_port = host
                .strip_prefix('[')
                .and_then(|value| value.split_once(']'))
                .map_or_else(|| host.contains(':'), |(_, suffix)| suffix.starts_with(':'));
            if has_explicit_port
                || probe.host_str().is_none()
                || probe.port().is_some()
                || !probe.username().is_empty()
                || probe.password().is_some()
                || probe.path() != "/"
                || probe.query().is_some()
                || probe.fragment().is_some()
            {
                return Err(invalid());
            }
            Ok(host.to_string())
        })
        .transpose()?;

    let params = replacement
        .get("PARAMS")
        .map(|value| {
            let object = value.as_object().ok_or_else(invalid)?;
            let mut params = BTreeMap::new();
            let mut decoded_names = HashSet::with_capacity(object.len());
            for (name, value) in object {
                let value = value.as_str().ok_or_else(invalid)?;
                let decoded_name = decode_hls_query_component(name).ok_or_else(invalid)?;
                if decoded_name.starts_with("_HLS_")
                    || !decoded_names.insert(decoded_name)
                    || !is_valid_hls_query_component(name, false)
                    || !is_valid_hls_query_component(value, true)
                {
                    return Err(invalid());
                }
                params.insert(name.clone(), value.to_string());
            }
            Ok(params)
        })
        .transpose()?
        .unwrap_or_default();
    let per_variant_uris = parse_hls_steering_uri_overrides(replacement.get("PER-VARIANT-URIS"))?;
    let per_rendition_uris =
        parse_hls_steering_uri_overrides(replacement.get("PER-RENDITION-URIS"))?;

    Ok(HlsPathwayClone {
        base_id,
        id,
        host,
        params,
        per_variant_uris,
        per_rendition_uris,
    })
}

fn is_valid_hls_query_component(value: &str, allow_empty: bool) -> bool {
    if value.is_empty() {
        return allow_empty;
    }
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            index += 1;
            continue;
        }
        if byte == b'%'
            && index + 2 < bytes.len()
            && bytes[index + 1].is_ascii_hexdigit()
            && bytes[index + 2].is_ascii_hexdigit()
        {
            index += 3;
            continue;
        }
        return false;
    }
    true
}

fn decode_hls_query_component(value: &str) -> Option<String> {
    let input = value.as_bytes();
    let mut decoded = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] == b'%' {
            let high = *input.get(index + 1)?;
            let low = *input.get(index + 2)?;
            let high = (high as char).to_digit(16)?;
            let low = (low as char).to_digit(16)?;
            decoded.push(u8::try_from(high * 16 + low).ok()?);
            index += 3;
        } else {
            decoded.push(input[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn append_hls_pathway_params(uri: &mut Url, params: &BTreeMap<String, String>) {
    if params.is_empty() {
        return;
    }

    let replaced_names = params
        .keys()
        .filter_map(|name| decode_hls_query_component(name))
        .collect::<HashSet<_>>();
    let mut retained = uri
        .query()
        .filter(|query| !query.is_empty())
        .map(|query| {
            query
                .split('&')
                .filter(|pair| {
                    let name = pair.split_once('=').map_or(*pair, |(name, _)| name);
                    !decode_hls_query_component(name)
                        .is_some_and(|name| replaced_names.contains(&name))
                })
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    retained.extend(params.iter().map(|(name, value)| format!("{name}={value}")));
    let query = retained.join("&");
    uri.set_query((!query.is_empty()).then_some(query.as_str()));
}

fn parse_hls_steering_uri_overrides(
    value: Option<&serde_json::Value>,
) -> Result<HashMap<String, String>, ManifestIntakeError> {
    let Some(value) = value else {
        return Ok(HashMap::new());
    };
    let object = value.as_object().ok_or_else(|| {
        ManifestIntakeError::Unsupported(
            "The HLS Content Steering URI override map is invalid".to_string(),
        )
    })?;
    let mut overrides = HashMap::with_capacity(object.len());
    for (stable_id, uri) in object {
        let uri = uri.as_str().filter(|uri| !uri.is_empty()).ok_or_else(|| {
            ManifestIntakeError::Unsupported(
                "The HLS Content Steering URI override map contains an invalid URI".to_string(),
            )
        })?;
        if stable_id.is_empty() {
            return Err(ManifestIntakeError::Unsupported(
                "The HLS Content Steering URI override map contains an empty stable ID".to_string(),
            ));
        }
        overrides.insert(stable_id.clone(), uri.to_string());
    }
    Ok(overrides)
}

fn replace_hls_pathway_uri(
    source: &Url,
    pathway_clone: &HlsPathwayClone,
    override_uri: Option<&str>,
) -> Option<Url> {
    let mut uri = source.clone();
    if let Some(host) = pathway_clone.host.as_deref() {
        uri.set_host(Some(host)).ok()?;
    }

    append_hls_pathway_params(&mut uri, &pathway_clone.params);

    if let Some(override_uri) = override_uri {
        uri = Url::parse(override_uri).ok()?;
    }
    if uri.scheme() != "https"
        || uri.host_str().is_none()
        || !uri.username().is_empty()
        || uri.password().is_some()
        || uri.fragment().is_some()
    {
        return None;
    }
    let host = uri.host_str()?;
    if !crate::download_net::is_public_destination_host(host) {
        return None;
    }
    Some(uri)
}

fn unique_cloned_group_id(pathway_id: &str, group_id: &str, used: &mut HashSet<String>) -> String {
    let base = format!("{pathway_id}__{group_id}");
    if used.insert(base.clone()) {
        return base;
    }
    for suffix in 1_u32.. {
        let candidate = format!("{base}_{suffix}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("u32 suffix space is finite and all IDs are exhausted")
}

fn apply_hls_pathway_clones(
    master: &mut crate::manifest::HlsMasterPlan,
    clones: &[HlsPathwayClone],
) {
    for pathway_clone in clones {
        let pathway_ids = master
            .variants
            .iter()
            .map(|variant| variant.pathway_id.as_str())
            .collect::<HashSet<_>>();
        if pathway_ids.contains(pathway_clone.id.as_str()) {
            continue;
        }
        let base_variants = master
            .variants
            .iter()
            .filter(|variant| variant.pathway_id == pathway_clone.base_id)
            .cloned()
            .collect::<Vec<_>>();
        if base_variants.is_empty() {
            // Unknown BASE-ID values are ignored as required by Content Steering.
            continue;
        }

        let audio_groups = base_variants
            .iter()
            .filter_map(|variant| variant.audio_group.clone())
            .collect::<HashSet<_>>();
        let subtitle_groups = base_variants
            .iter()
            .filter_map(|variant| variant.subtitles_group.clone())
            .collect::<HashSet<_>>();
        let video_groups = base_variants
            .iter()
            .filter_map(|variant| variant.video_rendition_group.clone())
            .collect::<HashSet<_>>();
        let mut used_audio_groups = master
            .audio_renditions
            .iter()
            .map(|rendition| rendition.group_id.clone())
            .collect::<HashSet<_>>();
        let mut used_subtitle_groups = master
            .subtitle_renditions
            .iter()
            .map(|rendition| rendition.group_id.clone())
            .collect::<HashSet<_>>();
        let audio_group_map = audio_groups
            .iter()
            .map(|group| {
                (
                    (*group).to_string(),
                    unique_cloned_group_id(&pathway_clone.id, group, &mut used_audio_groups),
                )
            })
            .collect::<HashMap<_, _>>();
        let subtitle_group_map = subtitle_groups
            .iter()
            .map(|group| {
                (
                    (*group).to_string(),
                    unique_cloned_group_id(&pathway_clone.id, group, &mut used_subtitle_groups),
                )
            })
            .collect::<HashMap<_, _>>();
        let mut used_video_groups = master
            .variants
            .iter()
            .filter_map(|variant| variant.video_rendition_group.clone())
            .collect::<HashSet<_>>();
        let video_group_map = video_groups
            .iter()
            .map(|group| {
                (
                    (*group).to_string(),
                    unique_cloned_group_id(&pathway_clone.id, group, &mut used_video_groups),
                )
            })
            .collect::<HashMap<_, _>>();

        let base_variant_count = base_variants.len();
        let cloned_variants = base_variants
            .into_iter()
            .map(|mut variant| {
                if let Some(stream_url) = variant.video_variant_url.as_ref() {
                    let stream_override = variant
                        .stable_variant_id
                        .as_deref()
                        .and_then(|stable_id| pathway_clone.per_variant_uris.get(stable_id))
                        .map(String::as_str);
                    let cloned_stream_url =
                        replace_hls_pathway_uri(stream_url, pathway_clone, stream_override)?;
                    if variant.url == *stream_url {
                        // A URI-less VIDEO rendition is embedded in its
                        // STREAM-INF playlist and has no separate rendition URI.
                        variant.url = cloned_stream_url.clone();
                    } else {
                        let rendition_override = variant
                            .stable_video_rendition_id
                            .as_deref()
                            .and_then(|stable_id| pathway_clone.per_rendition_uris.get(stable_id))
                            .map(String::as_str);
                        variant.url = replace_hls_pathway_uri(
                            &variant.url,
                            pathway_clone,
                            rendition_override,
                        )?;
                    }
                    variant.video_variant_url = Some(cloned_stream_url);
                } else {
                    let variant_override = variant
                        .stable_variant_id
                        .as_deref()
                        .and_then(|stable_id| pathway_clone.per_variant_uris.get(stable_id))
                        .map(String::as_str);
                    variant.url =
                        replace_hls_pathway_uri(&variant.url, pathway_clone, variant_override)?;
                }
                if let Some(group) = variant.audio_group.as_mut() {
                    *group = audio_group_map.get(group)?.clone();
                }
                if let Some(group) = variant.subtitles_group.as_mut() {
                    *group = subtitle_group_map.get(group)?.clone();
                }
                if let Some(group) = variant.video_rendition_group.as_mut() {
                    *group = video_group_map.get(group)?.clone();
                }
                variant.pathway_id.clone_from(&pathway_clone.id);
                Some(variant)
            })
            .collect::<Option<Vec<_>>>();

        // A clone is atomic: if any advertised member cannot be represented
        // safely, do not publish a partial pathway with missing track choices.
        let base_audio = master
            .audio_renditions
            .iter()
            .filter(|rendition| audio_groups.contains(rendition.group_id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        let base_subtitles = master
            .subtitle_renditions
            .iter()
            .filter(|rendition| subtitle_groups.contains(rendition.group_id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        let cloned_audio = base_audio
            .into_iter()
            .map(|mut rendition| {
                rendition.group_id = audio_group_map.get(&rendition.group_id)?.clone();
                if let Some(uri) = rendition.url.as_ref() {
                    let override_uri = rendition
                        .stable_rendition_id
                        .as_deref()
                        .and_then(|stable_id| pathway_clone.per_rendition_uris.get(stable_id))
                        .map(String::as_str);
                    rendition.url =
                        Some(replace_hls_pathway_uri(uri, pathway_clone, override_uri)?);
                }
                Some(rendition)
            })
            .collect::<Option<Vec<_>>>();
        let cloned_subtitles = base_subtitles
            .into_iter()
            .map(|mut rendition| {
                rendition.group_id = subtitle_group_map.get(&rendition.group_id)?.clone();
                let override_uri = rendition
                    .stable_rendition_id
                    .as_deref()
                    .and_then(|stable_id| pathway_clone.per_rendition_uris.get(stable_id))
                    .map(String::as_str);
                rendition.url =
                    replace_hls_pathway_uri(&rendition.url, pathway_clone, override_uri)?;
                Some(rendition)
            })
            .collect::<Option<Vec<_>>>();

        let Some(cloned_variants) = cloned_variants else {
            continue;
        };
        if cloned_variants.len() != base_variant_count
            || cloned_audio.is_none()
            || cloned_subtitles.is_none()
        {
            continue;
        }
        master.variants = master
            .variants
            .iter()
            .cloned()
            .chain(cloned_variants)
            .collect::<Vec<_>>()
            .into_boxed_slice();
        master.audio_renditions = master
            .audio_renditions
            .iter()
            .cloned()
            .chain(cloned_audio.unwrap_or_default())
            .collect::<Vec<_>>()
            .into_boxed_slice();
        master.subtitle_renditions = master
            .subtitle_renditions
            .iter()
            .cloned()
            .chain(cloned_subtitles.unwrap_or_default())
            .collect::<Vec<_>>()
            .into_boxed_slice();
    }
}

fn hls_steering_fetch_limits(fetch_mode: ManifestFetchMode) -> (Duration, u32) {
    match fetch_mode {
        ManifestFetchMode::Preview => (
            MANIFEST_PREVIEW_REQUEST_TIMEOUT,
            MAX_PREVIEW_MANIFEST_ATTEMPTS,
        ),
        // Native download steering does not retry: a failed optional steering
        // request falls back to the master's advertised Pathways.
        ManifestFetchMode::Download => (MANIFEST_REQUEST_TIMEOUT, 1),
    }
}

fn hls_steering_request_url(
    steering: &crate::manifest::HlsContentSteering,
) -> Result<Url, ManifestIntakeError> {
    let mut request_url = steering.server_url.clone();
    if request_url
        .query_pairs()
        .any(|(name, _)| name == "_HLS_pathway")
    {
        return Err(ManifestIntakeError::Unsupported(
            "The HLS Content Steering SERVER-URI must not contain _HLS_pathway".to_string(),
        ));
    }
    if let Some(pathway_id) = steering.initial_pathway_id.as_deref() {
        request_url
            .query_pairs_mut()
            .append_pair("_HLS_pathway", pathway_id);
    }
    Ok(request_url)
}

async fn fetch_hls_steering_manifest(
    client: &reqwest::Client,
    master_url: &Url,
    steering: &crate::manifest::HlsContentSteering,
    transport_policy: ManifestTransportPolicy,
    fetch_mode: ManifestFetchMode,
    proxy_configured: bool,
) -> Result<Vec<u8>, ManifestIntakeError> {
    let request_url = hls_steering_request_url(steering)?;

    // An upstream proxy resolves the target remotely, outside Grab's guarded
    // resolver. Steering is optional, so keep the advertised master Pathways
    // rather than make a proxied request whose DNS destination cannot be
    // checked locally.
    if proxy_configured {
        return Err(ManifestIntakeError::Unsupported(
            "HLS Content Steering fetches are disabled while an upstream proxy is configured"
                .to_string(),
        ));
    }

    // The master URL was selected by the user. A different SERVER-URI host
    // came from untrusted playlist data, so register it before the resolver
    // can connect and reject private/local destinations up front.
    if request_url.origin() != master_url.origin() {
        let host = request_url.host_str().ok_or_else(|| {
            ManifestIntakeError::Unsupported(
                "The HLS Content Steering SERVER-URI has no hostname".to_string(),
            )
        })?;
        if !crate::download_net::require_public_resolution(host) {
            return Err(ManifestIntakeError::Unsupported(
                "The HLS Content Steering SERVER-URI failed the public-host safety policy"
                    .to_string(),
            ));
        }
    }

    let (request_timeout, max_attempts) = hls_steering_fetch_limits(fetch_mode);
    let (_, body, _) = fetch_bounded_manifest_with_limits_and_size(
        client,
        request_url,
        transport_policy,
        request_timeout,
        max_attempts,
        fetch_mode,
        MAX_HLS_STEERING_MANIFEST_BYTES,
    )
    .await?;
    Ok(body)
}

fn filter_hls_master_to_pathway(
    master: &mut crate::manifest::HlsMasterPlan,
    pathway_id: &str,
) -> Result<(), ManifestIntakeError> {
    if !master
        .variants
        .iter()
        .any(|variant| variant.pathway_id == pathway_id)
    {
        return Err(ManifestIntakeError::Unsupported(format!(
            "The selected HLS Content Steering Pathway {pathway_id:?} is not present in the master playlist"
        )));
    }
    let selected_variants = master
        .variants
        .iter()
        .filter(|variant| variant.pathway_id == pathway_id)
        .cloned()
        .collect::<Vec<_>>();
    let audio_groups = selected_variants
        .iter()
        .filter_map(|variant| variant.audio_group.clone())
        .collect::<HashSet<_>>();
    let subtitle_groups = selected_variants
        .iter()
        .filter_map(|variant| variant.subtitles_group.clone())
        .collect::<HashSet<_>>();
    master.variants = selected_variants.into_boxed_slice();
    master.audio_renditions = master
        .audio_renditions
        .iter()
        .filter(|rendition| audio_groups.contains(rendition.group_id.as_str()))
        .cloned()
        .collect::<Vec<_>>()
        .into_boxed_slice();
    master.subtitle_renditions = master
        .subtitle_renditions
        .iter()
        .filter(|rendition| subtitle_groups.contains(rendition.group_id.as_str()))
        .cloned()
        .collect::<Vec<_>>()
        .into_boxed_slice();
    Ok(())
}

fn select_hls_content_steering_pathway(
    master: &mut crate::manifest::HlsMasterPlan,
    priority: &[String],
) -> Result<(), ManifestIntakeError> {
    let available = master
        .variants
        .iter()
        .map(|variant| variant.pathway_id.as_str())
        .collect::<HashSet<_>>();
    if let Some(initial) = master
        .content_steering
        .as_ref()
        .and_then(|steering| steering.initial_pathway_id.as_deref())
        && !available.contains(initial)
    {
        return Err(ManifestIntakeError::Unsupported(
            "The HLS Content Steering initial PATHWAY-ID is not present in the master playlist"
                .to_string(),
        ));
    }
    let selected = priority
        .iter()
        .find(|pathway_id| available.contains(pathway_id.as_str()))
        .cloned()
        .or_else(|| {
            master
                .content_steering
                .as_ref()
                .and_then(|steering| steering.initial_pathway_id.clone())
        })
        .or_else(|| {
            master
                .variants
                .first()
                .map(|variant| variant.pathway_id.clone())
        })
        .ok_or_else(|| {
            ManifestIntakeError::Unsupported(
                "HLS Content Steering master playlist has no downloadable Pathway".to_string(),
            )
        })?;
    filter_hls_master_to_pathway(master, &selected)
}

async fn apply_hls_content_steering(
    client: &reqwest::Client,
    master_url: &Url,
    mut master: crate::manifest::HlsMasterPlan,
    transport_policy: ManifestTransportPolicy,
    fetch_mode: ManifestFetchMode,
    proxy_configured: bool,
) -> Result<crate::manifest::HlsMasterPlan, ManifestIntakeError> {
    let Some(steering) = master.content_steering.as_ref() else {
        return Ok(master);
    };
    validate_hls_content_steering_stable_ids(&master)?;
    let priority = match fetch_hls_steering_manifest(
        client,
        master_url,
        steering,
        transport_policy,
        fetch_mode,
        proxy_configured,
    )
    .await
    {
        Ok(body) => {
            // A malformed steering response must not prevent downloads from
            // Pathways already advertised by the multivariant playlist.
            match parse_hls_steering_manifest(&body) {
                Ok(manifest) => {
                    apply_hls_pathway_clones(&mut master, &manifest.pathway_clones);
                    manifest.pathway_priority
                }
                Err(error) => {
                    tracing::debug!(error = %error, "HLS Content Steering response was invalid; using advertised Pathways");
                    Vec::new()
                }
            }
        }
        Err(error) => {
            // A failed optional steering fetch falls back to the master
            // playlist's already-advertised Pathways. Keep a diagnostic so
            // DNS policy, proxy and server failures are observable.
            tracing::debug!(
                host = %steering.server_url.host_str().unwrap_or_default(),
                error = %error,
                "HLS Content Steering fetch failed; using advertised Pathways"
            );
            Vec::new()
        }
    };
    select_hls_content_steering_pathway(&mut master, &priority)?;
    Ok(master)
}

async fn lookup_with_client_policy(
    client: &reqwest::Client,
    raw_url: &str,
    transport_policy: ManifestTransportPolicy,
    fetch_mode: ManifestFetchMode,
    proxy_configured: bool,
) -> Result<Option<ManifestLookup>, ManifestIntakeError> {
    let requested_url = Url::parse(raw_url)
        .map_err(|_| ManifestIntakeError::Failed(gettext("Invalid stream manifest URL")))?;
    if !is_native_manifest_candidate(&requested_url, transport_policy) {
        return Ok(None);
    }
    let manifest_kind_hint = manifest_kind_for_transport(&requested_url, transport_policy);
    let (final_url, body, server_now) = match fetch_mode {
        ManifestFetchMode::Preview => {
            fetch_bounded_preview_manifest_with_policy(client, requested_url, transport_policy)
                .await?
        }
        ManifestFetchMode::Download => {
            fetch_bounded_manifest_with_policy(client, requested_url, transport_policy).await?
        }
    };
    let final_url = transport_policy.planning_url(final_url)?;
    let kind = manifest_kind_hint
        .or_else(|| sniff_manifest_kind(&body))
        .ok_or_else(|| {
            ManifestIntakeError::Unsupported(
                "The response is not an HLS playlist or DASH MPD".to_string(),
            )
        })?;
    let manifest = match kind {
        crate::media_types::ManifestKind::Hls => {
            match crate::manifest::parse_hls_master_playlist(final_url.as_str(), &body) {
                Ok(master) => {
                    let master = apply_hls_content_steering(
                        client,
                        &final_url,
                        master,
                        transport_policy,
                        fetch_mode,
                        proxy_configured,
                    )
                    .await?;
                    validate_hls_stable_track_ids(&master)?;
                    NativeManifest::HlsMaster(master)
                }
                Err(crate::manifest::ManifestError::ExpectedMasterPlaylist) => {
                    let plan = crate::manifest::plan_hls_media_playlist(final_url.as_str(), &body)
                        .map_err(|error| {
                            ManifestIntakeError::Unsupported(format!(
                                "The HLS media playlist is not supported: {error}"
                            ))
                        })?;
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
            let body = resolve_dash_xlinks(
                client,
                &final_url,
                &body,
                transport_policy,
                proxy_configured,
            )
            .await?;
            let body = std::str::from_utf8(&body).map_err(|error| {
                ManifestIntakeError::Unsupported(format!(
                    "The DASH manifest is not valid UTF-8: {error}"
                ))
            })?;
            let plan =
                crate::manifest::dash::parse_dash_manifest_at(final_url.as_str(), body, server_now)
                    .map_err(|error| match error {
                        crate::manifest::dash::DashError::AdSignaling => {
                            // Do not route ad-marked DASH through yt-dlp: until
                            // cue-aware removal exists, failing closed is safer
                            // than downloading the signaled insertion interval.
                            ManifestIntakeError::Failed(
                                "DASH SCTE-35 ad signaling was detected; this presentation was not downloaded because ad intervals cannot yet be removed safely".to_string(),
                            )
                        }
                        error => ManifestIntakeError::Unsupported(format!(
                            "The DASH presentation is not supported: {error}"
                        )),
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

/// Resolve one bounded layer of DASH XLink onLoad references before MPD parsing.
async fn resolve_dash_xlinks(
    client: &reqwest::Client,
    parent_url: &Url,
    body: &[u8],
    transport_policy: ManifestTransportPolicy,
    proxy_configured: bool,
) -> Result<Vec<u8>, ManifestIntakeError> {
    let links = dash_xlink::find_onload_links(body).map_err(|error| {
        ManifestIntakeError::Unsupported(format!(
            "The DASH XLink structure is not supported: {error}"
        ))
    })?;
    if links.is_empty() {
        return Ok(body.to_vec());
    }
    // The configured proxy resolves destinations remotely, so even a
    // same-origin XLink could redirect to a private host without passing
    // through Grab's guarded resolver. Do not follow any XLinks with a proxy.
    if proxy_configured || crate::subprocess_proxy::validate_proxy_compatibility().is_err() {
        // Use Failed rather than Unsupported: Unsupported hands the URL to
        // yt-dlp, which would repeat the same proxy-side DNS exposure.
        return Err(ManifestIntakeError::Failed(
            "DASH XLink fetching is disabled while an upstream proxy is configured because the proxy bypasses local destination-address checks"
                .to_string(),
        ));
    }
    if links.len() > MAX_DASH_XLINK_COUNT {
        return Err(ManifestIntakeError::Unsupported(
            "The DASH MPD contains too many XLink references".to_string(),
        ));
    }

    let mut seen_hosts = HashSet::new();
    let mut total_link_bytes = 0_usize;
    let mut replacements = Vec::with_capacity(links.len());
    for link in &links {
        let replacement = match &link.action {
            dash_xlink::DashXlinkAction::Remove => Vec::new(),
            dash_xlink::DashXlinkAction::Fetch(href) => {
                let target = parent_url.join(href).map_err(|_| {
                    ManifestIntakeError::Unsupported(
                        "A DASH XLink reference has an invalid URL".to_string(),
                    )
                })?;
                if !transport_policy.accepts_url(&target) || target.fragment().is_some() {
                    return Err(ManifestIntakeError::Unsupported(
                        "DASH XLink references must be credential-free HTTPS URLs without fragments"
                            .to_string(),
                    ));
                }
                let host = target.host_str().ok_or_else(|| {
                    ManifestIntakeError::Unsupported(
                        "A DASH XLink target has no hostname".to_string(),
                    )
                })?;
                let host_key = host.trim_end_matches('.').to_ascii_lowercase();
                if !seen_hosts.contains(&host_key)
                    && seen_hosts.len()
                        >= crate::download_net::MAX_GUARDED_CHILD_HOSTS_PER_COLLECTION
                {
                    return Err(ManifestIntakeError::Unsupported(
                        "The DASH MPD references too many distinct XLink hosts".to_string(),
                    ));
                }
                // A directly entered manifest is trusted as the user-selected
                // origin, but a different host is still an untrusted remote
                // link. Force public-only DNS for those children so a hostile
                // MPD cannot turn XLink into a request to a private address.
                if target.origin() != parent_url.origin()
                    && !crate::download_net::require_public_resolution(host)
                {
                    return Err(ManifestIntakeError::Unsupported(
                        "A DASH XLink target failed the public-host safety policy".to_string(),
                    ));
                }
                crate::download_net::inherit_public_resolution_for_collection(
                    parent_url.as_str(),
                    target.as_str(),
                    &mut seen_hosts,
                )
                .map_err(|_| {
                    ManifestIntakeError::Unsupported(
                        "A DASH XLink target failed the child-host safety policy".to_string(),
                    )
                })?;
                seen_hosts.insert(host_key);

                let remaining = MAX_MANIFEST_BYTES
                    .checked_sub(body.len())
                    .and_then(|size| size.checked_sub(total_link_bytes))
                    .ok_or_else(|| {
                        ManifestIntakeError::Unsupported(
                            "The expanded DASH manifest exceeds its size limit".to_string(),
                        )
                    })?;
                let response = client
                    .get(target.clone())
                    .timeout(MANIFEST_REQUEST_TIMEOUT)
                    .send()
                    .await
                    .map_err(|error| {
                        manifest_request_error("Could not retrieve a DASH XLink resource", error)
                    })?;
                if !response.status().is_success() {
                    return Err(manifest_status_error(
                        response.status(),
                        ManifestFetchMode::Download,
                    ));
                }
                let final_url = response.url();
                if !transport_policy.accepts_url(final_url) || final_url.origin() != target.origin()
                {
                    return Err(ManifestIntakeError::Unsupported(
                        "A DASH XLink resource redirected outside its original HTTPS origin"
                            .to_string(),
                    ));
                }
                let linked_body =
                    read_bounded_response_with_limit(response, remaining, "DASH XLink resource")
                        .await?;
                total_link_bytes =
                    total_link_bytes
                        .checked_add(linked_body.len())
                        .ok_or_else(|| {
                            ManifestIntakeError::Unsupported(
                                "The expanded DASH manifest exceeds its size limit".to_string(),
                            )
                        })?;
                dash_xlink::linked_element(&linked_body, link.element)
                    .map_err(|error| {
                        ManifestIntakeError::Unsupported(format!(
                            "A DASH XLink resource did not contain one supported linked element: {error}"
                        ))
                    })?
                    .to_vec()
            }
        };
        replacements.push(replacement);
    }

    dash_xlink::replace_linked_elements(body, &links, &replacements, MAX_MANIFEST_BYTES).map_err(
        |error| {
            ManifestIntakeError::Unsupported(format!(
                "The expanded DASH manifest exceeds the supported structure or size limits: {error}"
            ))
        },
    )
}

/// Fetch and parse an HLS child media playlist with explicit parent variable imports.
pub(crate) async fn lookup_hls_media_with_variables(
    client: &reqwest::Client,
    raw_url: &str,
    inherited_variables: &[(String, String)],
) -> Result<HlsPlan, ManifestIntakeError> {
    let requested_url = Url::parse(raw_url)
        .map_err(|_| ManifestIntakeError::Failed(gettext("Invalid stream manifest URL")))?;
    let (final_url, body, _) = fetch_bounded_manifest(client, requested_url).await?;
    crate::manifest::plan_hls_media_playlist_with_variables(
        final_url.as_str(),
        &body,
        inherited_variables,
    )
    .map_err(|error| ManifestIntakeError::Unsupported(error.to_string()))
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
    let (final_url, body, _) = fetch_bounded_manifest_once(
        client,
        &requested_url,
        ManifestTransportPolicy::HttpsOnly,
        MANIFEST_REQUEST_TIMEOUT,
        ManifestFetchMode::Download,
    )
    .await?;
    let final_url = strip_hls_blocking_reload_query(&final_url);
    crate::manifest::plan_hls_media_playlist_with_variables(
        final_url.as_str(),
        &body,
        inherited_variables,
    )
    .map_err(|error| ManifestIntakeError::Unsupported(error.to_string()))
}

fn hls_live_reload_url(
    raw_url: &str,
    blocking_reload: Option<(u64, Option<u64>)>,
    can_skip_until: Option<Duration>,
) -> Result<Url, ManifestIntakeError> {
    let mut requested_url = Url::parse(raw_url)
        .map_err(|_| ManifestIntakeError::Failed(gettext("Invalid stream manifest URL")))?;
    {
        let mut query = requested_url.query_pairs_mut();
        if let Some((media_sequence, part_index)) = blocking_reload {
            query.append_pair("_HLS_msn", &media_sequence.to_string());
            if let Some(part_index) = part_index {
                query.append_pair("_HLS_part", &part_index.to_string());
            }
        }
        if can_skip_until.is_some() {
            query.append_pair("_HLS_skip", "YES");
        }
    }
    Ok(requested_url)
}

fn strip_hls_blocking_reload_query(url: &Url) -> Url {
    let retained = url
        .query_pairs()
        .filter(|(name, _)| !matches!(name.as_ref(), "_HLS_msn" | "_HLS_part" | "_HLS_skip"))
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    let mut clean = url.clone();
    clean.set_query(None);
    if !retained.is_empty() {
        clean.query_pairs_mut().extend_pairs(retained);
    }
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
mod tests {
    use super::*;
    use crate::extension_server::handoff_cookie_hash;
    use crate::manifest_plan::{
        hls_audio_rendition_key, hls_audio_rendition_keys, hls_variant_key, hls_variant_keys,
    };

    #[tokio::test(flavor = "current_thread")]
    async fn dash_xlink_resolves_a_period_through_the_bounded_client() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind XLink fixture");
        let address = listener.local_addr().expect("fixture address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept XLink request");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            loop {
                let read = stream.read(&mut chunk).await.expect("read XLink request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(request).expect("HTTP request is UTF-8");
            assert!(request.starts_with("GET /period.xml "));
            let linked = r#"<Period xmlns="urn:mpeg:dash:schema:mpd:2011" duration="PT2S"/>"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                linked.len(),
                linked
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write XLink response");
        });

        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build fixture client");
        let parent = Url::parse(&format!("http://{address}/manifest.mpd")).expect("parent URL");
        let body = br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" xmlns:xlink="http://www.w3.org/1999/xlink"><Period xlink:href="period.xml" xlink:actuate="onLoad"/></MPD>"#;
        let expanded = resolve_dash_xlinks(
            &client,
            &parent,
            body,
            ManifestTransportPolicy::HttpFixture,
            false,
        )
        .await
        .expect("bounded Period XLink resolution");
        server.await.expect("XLink fixture task");
        let expanded = std::str::from_utf8(&expanded).expect("expanded MPD is UTF-8");
        assert!(expanded.contains(r#"duration="PT2S""#));
        assert!(!expanded.contains("xlink:href"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dash_xlink_fails_without_fallback_when_proxy_is_configured() {
        let client = reqwest::Client::new();
        let parent = Url::parse("https://cdn.example.test/manifest.mpd").expect("parent URL");
        let body = br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" xmlns:xlink="http://www.w3.org/1999/xlink"><Period xlink:href="https://other.example.test/period.xml" xlink:actuate="onLoad"/></MPD>"#;

        let error = resolve_dash_xlinks(
            &client,
            &parent,
            body,
            ManifestTransportPolicy::HttpsOnly,
            true,
        )
        .await
        .expect_err("proxied XLinks must fail before fetching");
        assert!(matches!(error, ManifestIntakeError::Failed(_)));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dash_xlink_rejects_private_off_origin_targets() {
        let client = reqwest::Client::new();
        let parent = Url::parse("https://cdn.example.test/manifest.mpd").expect("parent URL");
        for target in [
            "https://127.0.0.1/period.xml",
            "https://localhost/period.xml",
        ] {
            let body = format!(
                r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" xmlns:xlink="http://www.w3.org/1999/xlink"><Period xlink:href="{target}" xlink:actuate="onLoad"/></MPD>"#
            );
            let error = resolve_dash_xlinks(
                &client,
                &parent,
                body.as_bytes(),
                ManifestTransportPolicy::HttpsOnly,
                false,
            )
            .await
            .expect_err("off-origin private XLink target must be rejected before fetching");
            assert!(matches!(error, ManifestIntakeError::Unsupported(_)));
        }
    }

    #[test]
    fn native_manifest_gate_accepts_explicit_format_query_hints() {
        assert!(supports("https://cdn.example.test/stream?format=m3u8"));
        assert!(supports(
            "https://cdn.example.test/stream?type=application/dash%2Bxml"
        ));
        assert!(!supports("https://cdn.example.test/watch?v=video"));
        assert!(!supports("http://cdn.example.test/stream?format=m3u8"));
    }

    #[test]
    fn native_manifest_gate_accepts_only_named_extensionless_endpoints() {
        assert!(supports("https://cdn.example.test/master"));
        assert!(supports("https://cdn.example.test/live/playlist?token=x"));
        assert!(supports("https://cdn.example.test/manifest"));
        assert!(!supports("https://cdn.example.test/watch"));
        assert!(!supports("https://cdn.example.test/index"));
    }

    #[test]
    fn endpoint_only_preview_candidates_skip_browser_cookie_export() {
        let browser = "firefox";
        let endpoint = Url::parse("https://cdn.example.test/playlist").unwrap();
        let explicit_hint = Url::parse("https://cdn.example.test/stream?format=m3u8").unwrap();

        assert_eq!(
            preview_cookie_browser(&endpoint, browser, ManifestTransportPolicy::HttpsOnly),
            "",
            "endpoint-only URLs are sniffed without decrypting browser cookies"
        );
        assert_eq!(
            preview_cookie_browser(&explicit_hint, browser, ManifestTransportPolicy::HttpsOnly),
            browser,
            "explicit manifest hints retain authenticated preview support"
        );
    }

    #[tokio::test]
    async fn youtube_playlist_page_skips_native_preview_before_cookie_export() {
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            lookup(
                "https://www.youtube.com/playlist?list=PL_test",
                "firefox",
                None,
            ),
        )
        .await
        .expect("YouTube playlist URLs must be rejected before browser-cookie export");

        assert!(
            result
                .expect("ordinary YouTube playlist pages do not enter native intake")
                .is_none(),
            "the endpoint-name heuristic must not route YouTube pages to native intake"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn extensionless_master_endpoint_is_sniffed_as_hls() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind extensionless manifest fixture");
        let address = listener.local_addr().expect("fixture address");
        let body = concat!(
            "#EXTM3U\n",
            "#EXT-X-VERSION:3\n",
            "#EXT-X-TARGETDURATION:4\n",
            "#EXT-X-MEDIA-SEQUENCE:0\n",
            "#EXTINF:4.0,\n",
            "segment0.ts\n",
            "#EXT-X-ENDLIST\n"
        );
        let response_body = body.to_string();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept manifest request");
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 1024];
                let read = socket
                    .read(&mut chunk)
                    .await
                    .expect("read manifest request");
                assert_ne!(read, 0, "request ended before HTTP headers");
                request.extend_from_slice(&chunk[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
                assert!(request.len() < 16 * 1024, "request headers stay bounded");
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write manifest response");
        });

        let url = format!("http://{address}/master");
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build local manifest client");
        let lookup = lookup_with_http_fixture_client(&client, &url)
            .await
            .expect("extensionless manifest lookup")
            .expect("HLS signature should select native parsing");
        server.await.expect("fixture server task");

        let NativeManifest::HlsMedia(plan) = lookup.manifest else {
            panic!("extensionless HLS body should produce a media playlist");
        };
        assert_eq!(plan.resources.len(), 1);
        assert_eq!(
            plan.resources[0].url.as_str(),
            format!("https://{address}/segment0.ts")
        );
    }

    #[test]
    fn manifest_body_sniffing_requires_a_manifest_signature() {
        assert_eq!(
            sniff_manifest_kind(b"\xef\xbb\xbf  #EXTM3U\n#EXT-X-ENDLIST"),
            Some(crate::media_types::ManifestKind::Hls)
        );
        assert_eq!(
            sniff_manifest_kind(
                br#"<?xml version="1.0" encoding="UTF-8"?><MPD xmlns="urn:mpeg:dash:schema:mpd:2011"/>"#
            ),
            Some(crate::media_types::ManifestKind::Dash)
        );
        assert_eq!(
            sniff_manifest_kind(
                br#"<!-- generated MPD --><!-- another comment --><?xml version="1.0"?><MPD xmlns="urn:mpeg:dash:schema:mpd:2011"/>"#
            ),
            Some(crate::media_types::ManifestKind::Dash)
        );
        assert_eq!(
            sniff_manifest_kind(b"<!doctype html><html><body>video</body></html>"),
            None
        );
        assert_eq!(sniff_manifest_kind(b"not a manifest"), None);
    }

    #[test]
    fn manifest_preview_http_statuses_fall_back_without_retry() {
        for status in [
            reqwest::StatusCode::REQUEST_TIMEOUT,
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            reqwest::StatusCode::NOT_FOUND,
            reqwest::StatusCode::FORBIDDEN,
        ] {
            assert!(
                matches!(
                    manifest_status_error(status, ManifestFetchMode::Preview),
                    ManifestIntakeError::Unsupported(_)
                ),
                "preview HTTP {status} should yield immediately to yt-dlp"
            );
        }
    }

    #[test]
    fn manifest_download_retries_only_transient_http_statuses() {
        for status in [
            reqwest::StatusCode::REQUEST_TIMEOUT,
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
        ] {
            assert!(
                matches!(
                    manifest_status_error(status, ManifestFetchMode::Download),
                    ManifestIntakeError::Retryable(_)
                ),
                "download HTTP {status} should retain transient retries"
            );
        }
        for status in [
            reqwest::StatusCode::NOT_FOUND,
            reqwest::StatusCode::FORBIDDEN,
        ] {
            assert!(matches!(
                manifest_status_error(status, ManifestFetchMode::Download),
                ManifestIntakeError::Unsupported(_)
            ));
        }
    }

    #[test]
    fn manifest_transport_retries_only_timeouts_and_connection_resets() {
        let reset = std::io::Error::from(std::io::ErrorKind::ConnectionReset);
        let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert!(has_connection_reset_cause(&reset));
        assert!(!has_connection_reset_cause(&refused));
        assert!(!has_connection_reset_cause(&std::io::Error::other(
            "DNS resolution was refused by the guarded resolver"
        )));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn manifest_preview_budget_cancels_a_slow_server_request() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind slow manifest fixture");
        let port = listener
            .local_addr()
            .expect("read slow manifest fixture address")
            .port();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("accept manifest request");
            tokio::time::sleep(Duration::from_millis(100)).await;
            drop(socket);
        });
        let url = Url::parse(&format!("http://127.0.0.1:{port}/master.m3u8"))
            .expect("fixture manifest URL");
        let started = tokio::time::Instant::now();

        let result = fetch_bounded_preview_manifest_with_limits(
            &reqwest::Client::new(),
            url,
            ManifestTransportPolicy::HttpFixture,
            Duration::from_millis(20),
            Duration::from_secs(1),
            1,
        )
        .await;

        assert!(
            matches!(
                &result,
                Err(ManifestIntakeError::Unsupported(message))
                    if message.contains("20 ms preview budget")
            ),
            "slow preview should yield to the extractor fallback: {result:?}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "preview should stop at its deadline instead of waiting for the request timeout"
        );
        server.await.expect("slow manifest fixture task");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn manifest_transport_failure_does_not_disclose_signed_url() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve a local port");
        let port = listener.local_addr().expect("read reserved port").port();
        drop(listener);
        let url = format!("http://127.0.0.1:{port}/master.m3u8?sig=secret-token");
        let error = reqwest::Client::new()
            .get(&url)
            .timeout(Duration::from_secs(1))
            .send()
            .await
            .expect_err("the reserved port is no longer listening");

        let classified = manifest_request_error("Could not retrieve the stream manifest", error);
        let message = classified.to_string();
        assert!(matches!(classified, ManifestIntakeError::Unsupported(_)));
        assert!(message.contains("Could not retrieve the stream manifest"));
        assert!(!message.contains("secret-token"));
    }

    #[test]
    fn preferred_source_selects_the_configured_hls_representation() {
        let body = br#"#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=900000,RESOLUTION=1280x720,STABLE-VARIANT-ID="fixture-variant-1"
low.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=1800000,RESOLUTION=1920x1080,STABLE-VARIANT-ID="fixture-variant-2"
high.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=3500000,RESOLUTION=2560x1440,STABLE-VARIANT-ID="fixture-variant-3"
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
            .preferred_source("1080p", None)
            .expect("1080p HLS representation");
        let best = lookup
            .preferred_source("best", None)
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
#EXT-X-STREAM-INF:BANDWIDTH=1800000,RESOLUTION=1920x1080,STABLE-VARIANT-ID="fixture-variant-4"
first.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=900000,RESOLUTION=1280x720,STABLE-VARIANT-ID="fixture-variant-5"
low.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1920x1080,STABLE-VARIANT-ID="fixture-variant-6"
higher-bandwidth.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=1800000,RESOLUTION=1920x1080,STABLE-VARIANT-ID="fixture-variant-7"
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
                original_keys[2].as_deref().expect("stable variant key"),
                original_keys[0].as_deref().expect("stable variant key"),
                original_keys[3].as_deref().expect("stable variant key"),
                original_keys[1].as_deref().expect("stable variant key"),
            ],
            "bandwidth should break height ties, and equal ties should keep manifest order"
        );
    }

    #[test]
    fn preferred_choice_index_matches_configured_height_and_best_uses_highest() {
        let choices = [
            ManifestChoice {
                key: "720".into(),
                label: "720p".to_string(),
                height: Some(720),
                bandwidth: Some(1_000_000),
            },
            ManifestChoice {
                key: "1080".into(),
                label: "1080p".to_string(),
                height: Some(1080),
                bandwidth: Some(2_500_000),
            },
            ManifestChoice {
                key: "1440".into(),
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
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio-en",NAME="English",LANGUAGE="en",DEFAULT=YES,AUTOSELECT=YES,STABLE-RENDITION-ID="fixture-rendition-1",URI="audio/en.m3u8?sig=old"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio-en",NAME="French",LANGUAGE="fr",DEFAULT=NO,AUTOSELECT=YES,STABLE-RENDITION-ID="fixture-rendition-2",URI="audio/fr.m3u8?sig=old"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio-es",NAME="Spanish",LANGUAGE="es",DEFAULT=YES,AUTOSELECT=YES,STABLE-RENDITION-ID="fixture-rendition-3",URI="audio/es.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=1800000,RESOLUTION=1920x1080,AUDIO="audio-en",STABLE-VARIANT-ID="fixture-variant-8"
video/en.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=900000,RESOLUTION=1280x720,AUDIO="audio-es",STABLE-VARIANT-ID="fixture-variant-9"
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

        let english_video_key = crate::media_types::ManifestRepresentationKey::from(
            video_keys[0].as_deref().expect("stable English video key"),
        );
        let spanish_video_key = crate::media_types::ManifestRepresentationKey::from(
            video_keys[1].as_deref().expect("stable Spanish video key"),
        );
        let english_choices = lookup.audio_choices(&english_video_key);
        assert_eq!(english_choices.len(), 2);
        assert!(english_choices[0].is_default);
        assert!(
            english_choices
                .iter()
                .any(|choice| choice.label.contains("French"))
        );
        assert_eq!(lookup.audio_choices(&spanish_video_key).len(), 1);

        let default_source = lookup
            .preferred_source("1080p", None)
            .expect("preferred HLS video and default audio");
        assert_eq!(default_source.representation_key, english_video_key);
        assert_eq!(
            default_source.audio_key.as_deref(),
            Some(english_choices[0].key.as_str())
        );

        let selected = lookup
            .selected_source(&english_video_key, Some(&english_choices[1].key), None)
            .expect("selected alternate audio rendition");
        assert_eq!(
            selected.audio_key.as_deref(),
            Some(english_choices[1].key.as_str())
        );
        assert!(
            lookup
                .selected_source(&spanish_video_key, Some(&english_choices[1].key), None)
                .is_none(),
            "an audio rendition must not be selectable from another variant's group"
        );
    }

    #[test]
    fn hls_audio_key_uses_stable_rendition_id_across_url_rotation() {
        let rendition = |url: &str| crate::manifest::HlsAudioRendition {
            url: Some(Url::parse(url).expect("valid audio rendition URL")),
            group_id: "audio-main".to_string(),
            name: "English".to_string(),
            language: Some("en".to_string()),
            is_default: true,
            stable_rendition_id: Some("rendition-en".to_string()),
        };
        let original = rendition("https://cdn.example.test/audio/en.m3u8?sig=old");
        let refreshed = rendition("https://cdn.example.test/audio/en-new.m3u8?sig=new");
        let mut missing_id = rendition("https://cdn.example.test/audio/no-id.m3u8");
        missing_id.stable_rendition_id = None;

        assert_eq!(
            hls_audio_rendition_key(&original),
            hls_audio_rendition_key(&refreshed)
        );
        assert!(
            hls_audio_rendition_key(&missing_id).is_some(),
            "renditions without Apple stable IDs derive a content key"
        );
    }

    #[test]
    fn hls_stable_rendition_key_survives_url_rotation() {
        let rendition = |url: &str| crate::manifest::HlsAudioRendition {
            url: Some(Url::parse(url).expect("valid audio rendition URL")),
            group_id: "audio-main".to_string(),
            name: "English".to_string(),
            language: Some("en".to_string()),
            is_default: true,
            stable_rendition_id: Some("rendition-en".to_string()),
        };
        let original = rendition("https://cdn.example.test/audio/en-old.m3u8?sig=old");
        let refreshed = rendition("https://cdn.example.test/audio/en-new.m3u8?sig=new");

        assert_eq!(
            hls_audio_rendition_key(&original),
            hls_audio_rendition_key(&refreshed),
            "stable rendition IDs survive URL and signature rotation"
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
        let video_key = crate::media_types::ManifestRepresentationKey::from(
            dash.video_representations[0].key.clone(),
        );
        let main_audio_key = crate::media_types::ManifestAudioKey::from(
            dash.default_audio()
                .expect("DASH main audio representation")
                .key
                .clone(),
        );
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
            .selected_source(&video_key, Some(&choices[1].key), None)
            .expect("select the alternate DASH audio representation");
        assert_eq!(selected.audio_key.as_deref(), Some(choices[1].key.as_str()));
        let preferred = lookup
            .preferred_source("720p", None)
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
        assert_eq!(legacy.subtitle_key, None);
        assert_eq!(legacy.subtitle_language, None);

        // Existing queue files store these identities as plain JSON strings.
        let legacy_with_tracks: crate::media_types::ManifestSource =
            serde_json::from_value(serde_json::json!({
                "url": "https://cdn.example.test/master.m3u8",
                "kind": "Hls",
                "representation_key": "hls-variant-0123456789abcdef0123456789abcdef",
                "audio_key": "hls-audio-0123456789abcdef0123456789abcdef",
                "subtitle_key": "hls-subtitle-0123456789abcdef0123456789abcdef",
                "subtitle_language": "en"
            }))
            .expect("legacy string keys remain compatible with typed identities");
        assert_eq!(
            legacy_with_tracks.representation_key.as_str(),
            "hls-variant-0123456789abcdef0123456789abcdef"
        );
        assert_eq!(
            legacy_with_tracks.audio_key.as_deref(),
            Some("hls-audio-0123456789abcdef0123456789abcdef")
        );
        assert_eq!(
            legacy_with_tracks.subtitle_key.as_deref(),
            Some("hls-subtitle-0123456789abcdef0123456789abcdef")
        );
        let encoded =
            serde_json::to_value(&legacy_with_tracks).expect("serialize typed manifest identities");
        assert_eq!(
            encoded["representation_key"],
            "hls-variant-0123456789abcdef0123456789abcdef"
        );
        assert_eq!(
            encoded["audio_key"],
            "hls-audio-0123456789abcdef0123456789abcdef"
        );
        assert_eq!(
            encoded["subtitle_key"],
            "hls-subtitle-0123456789abcdef0123456789abcdef"
        );

        let mut source = crate::media_types::ManifestSource {
            url: "https://cdn.example.test/master.m3u8".to_string(),
            kind: crate::media_types::ManifestKind::Hls,
            representation_key: "hls-variant-0123456789abcdef0123456789abcdef".into(),
            audio_key: Some("hls-audio-0123456789abcdef0123456789abcdef".into()),
            subtitle_key: None,
            subtitle_language: None,
        };
        assert!(valid_source(&source));
        source.url = "https://cdn.example.test/master".to_string();
        assert!(
            valid_source(&source),
            "extensionless endpoint kind is carried by the persisted source"
        );
        source.url = "https://cdn.example.test/master.m3u8".to_string();
        source.audio_key = Some("not-a-stable-audio-key".into());
        assert!(!valid_source(&source));
        source.audio_key = None;
        source.subtitle_key = Some("hls-audio-0123456789abcdef0123456789abcdef".into());
        assert!(!valid_source(&source));
        source.subtitle_key = Some("hls-subtitle-0123456789abcdef0123456789abcdef".into());
        assert!(valid_source(&source));
        source.subtitle_key = None;
        source.representation_key = format!("hls-variant-{}", "0".repeat(64)).into();
        assert!(
            valid_source(&source),
            "legacy queue keys remain restorable so the runner can fall back to yt-dlp"
        );
        source.representation_key = "hls-media".into();
        source.audio_key = None;
        assert!(valid_source(&source));
        source.subtitle_key = Some("hls-subtitle-0123456789abcdef0123456789abcdef".into());
        assert!(!valid_source(&source));
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
    fn hls_variant_keys_prefer_stable_ids_over_content_identities() {
        let variant = |url: &str, stable_id: &str| HlsVariant {
            url: Url::parse(url).expect("valid variant URL"),
            bandwidth: 2_000_000,
            average_bandwidth: Some(1_800_000),
            width: Some(1920),
            height: Some(1080),
            codecs: Some("avc1.640028".to_string()),
            frame_rate: Some(30.0),
            audio_group: None,
            subtitles_group: None,
            video_rendition_group: None,
            video_variant_path: None,
            video_variant_url: None,
            video_rendition_name: None,
            stable_video_rendition_id: None,
            stable_variant_id: Some(stable_id.to_string()),
            pathway_id: ".".to_string(),
        };
        let original = variant(
            "https://cdn.example.test/video/old.m3u8?sig=old",
            "variant-1080",
        );
        let refreshed = variant(
            "https://cdn.example.test/video/new.m3u8?sig=new",
            "variant-1080",
        );
        let different = variant(
            "https://cdn.example.test/video/720.m3u8?sig=new",
            "variant-720",
        );
        let mut missing_id = original.clone();
        missing_id.stable_variant_id = None;
        let mut rotated_host = missing_id.clone();
        rotated_host.url =
            Url::parse("https://mirror.example.test/video/old.m3u8?sig=new").expect("valid URL");

        let derived = hls_variant_key(&missing_id)
            .expect("variants without Apple stable IDs keep a content-derived key");
        assert_eq!(hls_variant_key(&rotated_host), Some(derived));
        assert_eq!(hls_variant_key(&original), hls_variant_key(&refreshed));
        assert_ne!(hls_variant_key(&original), hls_variant_key(&different));
        assert_eq!(hls_variant_keys(&[original, refreshed, different]).len(), 3);
    }

    #[test]
    fn hls_variant_content_keys_survive_host_and_signature_rotation() {
        // Twitter/X amplify-style master: no STABLE-VARIANT-ID at all.
        let variant = |url: &str, height: u64, width: u64, bandwidth: u64| HlsVariant {
            url: Url::parse(url).expect("valid variant URL"),
            bandwidth,
            average_bandwidth: Some(bandwidth * 2 / 3),
            width: Some(width),
            height: Some(height),
            codecs: Some("mp4a.40.2,avc1.4D401F".to_string()),
            frame_rate: Some(30.0),
            audio_group: Some(format!("audio-{bandwidth}")),
            subtitles_group: None,
            video_rendition_group: None,
            video_variant_path: None,
            video_variant_url: None,
            video_rendition_name: None,
            stable_video_rendition_id: None,
            stable_variant_id: None,
            pathway_id: ".".to_string(),
        };
        let original = variant(
            "https://video.twimg.com/amplify_video/6/pl/avc1/854x480/abc.m3u8",
            480,
            854,
            1_630_685,
        );
        let refreshed = variant(
            "https://video10.twimg.com/amplify_video/6/pl/avc1/854x480/abc.m3u8?sig=rotated",
            480,
            854,
            1_630_685,
        );
        let other = variant(
            "https://video.twimg.com/amplify_video/6/pl/avc1/640x360/xyz.m3u8",
            360,
            640,
            817_916,
        );

        assert_eq!(
            hls_variant_key(&original),
            hls_variant_key(&refreshed),
            "host and signed-query rotation must not change the identity"
        );
        assert_ne!(
            hls_variant_key(&original),
            hls_variant_key(&other),
            "different tracks must not share a content identity"
        );
    }

    #[test]
    fn hls_video_rendition_keys_combine_stable_variant_and_rendition_ids() {
        let make_master = |base: &str, variant_uri: &str, main_uri: &str, camera_uri: &str| {
            let body = format!(
                "#EXTM3U\n#EXT-X-MEDIA:TYPE=VIDEO,GROUP-ID=\"angles\",NAME=\"Main\",URI=\"{main_uri}\",STABLE-RENDITION-ID=\"main\"\n#EXT-X-MEDIA:TYPE=VIDEO,GROUP-ID=\"angles\",NAME=\"Camera B\",URI=\"{camera_uri}\",STABLE-RENDITION-ID=\"camera-b\"\n#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1920x1080,VIDEO=\"angles\",STABLE-VARIANT-ID=\"variant-1080\"\n{variant_uri}\n"
            );
            crate::manifest::parse_hls_master_playlist(base, body.as_bytes())
                .expect("valid video rendition master")
        };
        let original = make_master(
            "https://cdn.example.test/master.m3u8",
            "video/combined.m3u8",
            "video/main.m3u8",
            "video/camera-b.m3u8",
        );
        let refreshed = make_master(
            "https://mirror.example.test/master.m3u8",
            "rotated/combined.m3u8",
            "rotated/main.m3u8",
            "rotated/camera-b.m3u8",
        );
        let original_keys = hls_variant_keys(&original.variants);
        let refreshed_keys = hls_variant_keys(&refreshed.variants);
        assert_eq!(original_keys, refreshed_keys);
        assert_ne!(original_keys[0], original_keys[1]);
    }

    #[test]
    fn hls_video_rendition_content_keys_include_parent_variant_and_audio_group() {
        let variant = |parent_path: &str, audio_group: &str| HlsVariant {
            url: Url::parse("https://cdn.example.test/angles/main.m3u8").expect("valid angle URL"),
            bandwidth: 2_000_000,
            average_bandwidth: Some(1_800_000),
            width: Some(1920),
            height: Some(1080),
            codecs: Some("avc1.640028".to_string()),
            frame_rate: Some(30.0),
            audio_group: Some(audio_group.to_string()),
            subtitles_group: None,
            video_rendition_group: Some("angles".to_string()),
            video_variant_path: Some(parent_path.to_string()),
            video_variant_url: None,
            video_rendition_name: Some("Main".to_string()),
            stable_video_rendition_id: None,
            stable_variant_id: None,
            pathway_id: ".".to_string(),
        };
        let first = variant("/video/first.m3u8", "audio-en");
        let other_variant = variant("/video/second.m3u8", "audio-en");
        let other_audio = variant("/video/first.m3u8", "audio-fr");
        assert_ne!(hls_variant_key(&first), hls_variant_key(&other_variant));
        assert_ne!(hls_variant_key(&first), hls_variant_key(&other_audio));
    }

    #[test]
    fn hls_stable_variant_key_survives_resource_rotation() {
        let variant = |url: &str| HlsVariant {
            url: Url::parse(url).expect("valid variant URL"),
            bandwidth: 2_000_000,
            average_bandwidth: Some(1_800_000),
            width: Some(1920),
            height: Some(1080),
            codecs: Some("avc1.640028".to_string()),
            frame_rate: Some(30.0),
            audio_group: Some("audio-main".to_string()),
            subtitles_group: None,
            video_rendition_group: None,
            video_variant_path: None,
            video_variant_url: None,
            video_rendition_name: None,
            stable_video_rendition_id: None,
            stable_variant_id: Some("variant-1080".to_string()),
            pathway_id: ".".to_string(),
        };
        let original = variant("https://cdn.example.test/video/old.m3u8?sig=old");
        let refreshed = variant("https://cdn.example.test/video/new.m3u8?sig=new");

        assert_eq!(
            hls_variant_key(&original),
            hls_variant_key(&refreshed),
            "stable variant IDs survive resource and signature rotation"
        );
    }

    #[test]
    fn selected_source_rejects_url_derived_hls_keys() {
        let body = br#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio",NAME="English",LANGUAGE="en",STABLE-RENDITION-ID="rendition-en",URI="audio/en.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=1280x720,AUDIO="audio",STABLE-VARIANT-ID="variant-720"
video/720.m3u8
"#;
        let master = crate::manifest::parse_hls_master_playlist(
            "https://cdn.example.test/master.m3u8",
            body,
        )
        .expect("valid HLS master playlist");
        let current_video_key = hls_variant_keys(&master.variants)[0]
            .as_deref()
            .expect("stable video key")
            .to_string();
        let current_audio_key = hls_audio_rendition_keys(&master.audio_renditions)[0]
            .as_deref()
            .expect("stable audio key")
            .to_string();
        let lookup = ManifestLookup {
            source_url: "https://cdn.example.test/master.m3u8".to_string(),
            manifest: NativeManifest::HlsMaster(master),
        };

        assert!(
            lookup
                .selected_source(
                    &crate::media_types::ManifestRepresentationKey::from(
                        "hls-variant-00000000000000000000000000000000",
                    ),
                    Some(&crate::media_types::ManifestAudioKey::from(
                        current_audio_key.as_str(),
                    )),
                    None,
                )
                .is_none()
        );
        assert!(
            lookup
                .selected_source(
                    &crate::media_types::ManifestRepresentationKey::from(
                        current_video_key.as_str()
                    ),
                    Some(&crate::media_types::ManifestAudioKey::from(
                        "hls-audio-00000000000000000000000000000000",
                    )),
                    None,
                )
                .is_none()
        );
    }

    #[test]
    fn hls_master_rejects_duplicate_track_identities() {
        let body = br#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio",NAME="English",STABLE-RENDITION-ID="rendition-en",URI="audio/en.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=1000000,AUDIO="audio",STABLE-VARIANT-ID="variant-720"
video/720.m3u8
"#;
        let mut master = crate::manifest::parse_hls_master_playlist(
            "https://cdn.example.test/master.m3u8",
            body,
        )
        .expect("valid HLS master playlist");
        assert!(validate_hls_stable_track_ids(&master).is_ok());

        let duplicate = master.variants[0].clone();
        master.variants = vec![duplicate.clone(), duplicate].into_boxed_slice();
        assert!(matches!(
            validate_hls_stable_track_ids(&master),
            Err(ManifestIntakeError::Unsupported(_))
        ));
        master.variants = vec![master.variants[0].clone()].into_boxed_slice();

        // Attribute-less tracks must collide through their content identities
        // too; this is the dominant path for ordinary non-Apple masters.
        let mut attribute_less = master.variants[0].clone();
        attribute_less.stable_variant_id = None;
        master.variants = vec![attribute_less.clone(), attribute_less].into_boxed_slice();
        assert!(validate_hls_stable_track_ids(&master).is_err());
        master.variants = vec![master.variants[0].clone()].into_boxed_slice();

        let duplicate_rendition = master.audio_renditions[0].clone();
        master.audio_renditions =
            vec![duplicate_rendition.clone(), duplicate_rendition].into_boxed_slice();
        assert!(validate_hls_stable_track_ids(&master).is_err());
        master.audio_renditions = vec![master.audio_renditions[0].clone()].into_boxed_slice();

        let mut attribute_less_rendition = master.audio_renditions[0].clone();
        attribute_less_rendition.stable_rendition_id = None;
        master.audio_renditions =
            vec![attribute_less_rendition.clone(), attribute_less_rendition].into_boxed_slice();
        assert!(validate_hls_stable_track_ids(&master).is_err());
    }

    #[test]
    fn hls_master_supports_tracks_without_stable_ids() {
        // Twitter/X amplify-style master: attribute-less audio and variants.
        let body = br#"#EXTM3U
#EXT-X-VERSION:6
#EXT-X-INDEPENDENT-SEGMENTS
#EXT-X-MEDIA:NAME="Audio",TYPE=AUDIO,GROUP-ID="audio-32000",AUTOSELECT=YES,URI="/pl/mp4a/32000/a.m3u8"
#EXT-X-MEDIA:NAME="Audio",TYPE=AUDIO,GROUP-ID="audio-128000",AUTOSELECT=YES,URI="/pl/mp4a/128000/b.m3u8"

#EXT-X-STREAM-INF:AVERAGE-BANDWIDTH=146321,BANDWIDTH=241630,RESOLUTION=480x270,CODECS="mp4a.40.2,avc1.4D4015",AUDIO="audio-32000"
/pl/avc1/480x270/v1.m3u8
#EXT-X-STREAM-INF:AVERAGE-BANDWIDTH=792011,BANDWIDTH=1630685,RESOLUTION=854x480,CODECS="mp4a.40.2,avc1.64001F",AUDIO="audio-128000"
/pl/avc1/854x480/v3.m3u8
"#;
        let master = crate::manifest::parse_hls_master_playlist(
            "https://video.twimg.com/amplify_video/6/pl/master.m3u8",
            body,
        )
        .expect("valid HLS master playlist");

        validate_hls_stable_track_ids(&master)
            .expect("attribute-less masters stay on the native path");
        let keys = hls_variant_keys(&master.variants);
        assert!(
            keys.iter().all(Option::is_some),
            "every variant gets a durable content-derived key"
        );
        assert_ne!(keys[0], keys[1], "distinct tracks get distinct keys");
        let lookup = ManifestLookup {
            source_url: "https://video.twimg.com/amplify_video/6/pl/master.m3u8".to_string(),
            manifest: NativeManifest::HlsMaster(master),
        };
        assert_eq!(
            lookup.choices().len(),
            2,
            "attribute-less masters offer selectable variants"
        );
    }

    #[test]
    fn hls_master_supports_subtitle_renditions_without_stable_ids() {
        let body = br#"#EXTM3U
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs",NAME="English",LANGUAGE="en",URI="subs/en.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=1000000,SUBTITLES="subs",STABLE-VARIANT-ID="variant-720"
video/720.m3u8
"#;
        let master = crate::manifest::parse_hls_master_playlist(
            "https://cdn.example.test/master.m3u8",
            body,
        )
        .expect("HLS parser retains subtitle metadata");
        assert!(
            validate_hls_stable_track_ids(&master).is_ok(),
            "subtitle renditions without Apple stable IDs fall back to content identities"
        );
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
        let manifest_body = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1800000,RESOLUTION=1280x720,STABLE-VARIANT-ID=\"preview-variant\"\nvideo/720.m3u8?sig=child\n";
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

    #[test]
    fn preview_cache_accepts_static_hls_and_returns_a_clone() {
        let body = br#"#EXTM3U
#EXT-X-TARGETDURATION:10
#EXTINF:10,
segment.ts
#EXT-X-ENDLIST
"#;
        let plan = crate::manifest::plan_hls_media_playlist(
            "https://cache-preview.example.test/media.m3u8",
            body,
        )
        .expect("valid static HLS media playlist");
        let lookup = ManifestLookup {
            source_url: "https://cache-preview.example.test/media.m3u8".to_string(),
            manifest: crate::manifest_types::NativeManifest::HlsMedia(Box::new(plan)),
        };
        assert!(is_cacheable_static_manifest(&lookup));

        let key = ManifestPreviewCacheKey {
            url: lookup.source_url.clone(),
            cookies_browser: "none".to_string(),
            handoff_cookie_hash: None,
            proxy: None,
        };
        manifest_preview_cache_insert(key.clone(), &lookup);
        let cached = manifest_preview_cache_get(&key).expect("cache hit");
        assert_eq!(cached.source_url, lookup.source_url);
        assert_eq!(cached.choices(), lookup.choices());
    }

    #[test]
    fn preview_cache_accepts_master_selection_metadata_without_signed_children() {
        let body = br#"#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=1280x720
video/720.m3u8
"#;
        let master = crate::manifest::parse_hls_master_playlist(
            "https://cache-preview.example.test/master.m3u8",
            body,
        )
        .expect("valid HLS master");
        let lookup = ManifestLookup {
            source_url: "https://cache-preview.example.test/master.m3u8".to_string(),
            manifest: crate::manifest_types::NativeManifest::HlsMaster(master),
        };
        assert!(is_cacheable_static_manifest(&lookup));
    }

    #[test]
    fn preview_cache_rejects_live_hls_media_playlists() {
        let body = br#"#EXTM3U
#EXT-X-TARGETDURATION:10
#EXTINF:10,
segment.ts
"#;
        let plan = crate::manifest::plan_hls_media_playlist(
            "https://cache-preview.example.test/live.m3u8",
            body,
        )
        .expect("valid live HLS media playlist");
        let lookup = ManifestLookup {
            source_url: "https://cache-preview.example.test/live.m3u8".to_string(),
            manifest: crate::manifest_types::NativeManifest::HlsMedia(Box::new(plan)),
        };
        assert!(!is_cacheable_static_manifest(&lookup));
    }

    #[test]
    fn preview_cache_expires_entries_after_the_ttl() {
        let key = ManifestPreviewCacheKey {
            url: "https://cache-expiry.example.test/media.m3u8".to_string(),
            cookies_browser: "none".to_string(),
            handoff_cookie_hash: None,
            proxy: None,
        };
        MANIFEST_PREVIEW_CACHE.lock().expect("cache lock").insert(
            key.clone(),
            (
                Instant::now() - MANIFEST_PREVIEW_CACHE_TTL - Duration::from_secs(1),
                ManifestLookup {
                    source_url: key.url.clone(),
                    manifest: crate::manifest_types::NativeManifest::HlsMedia(Box::new(
                        crate::manifest::plan_hls_media_playlist(
                            &key.url,
                            br#"#EXTM3U
#EXT-X-TARGETDURATION:10
#EXTINF:10,
segment.ts
#EXT-X-ENDLIST
"#,
                        )
                        .expect("valid static HLS media playlist"),
                    )),
                },
            ),
        );
        assert!(manifest_preview_cache_get(&key).is_none());
    }

    #[test]
    fn preview_cache_rejects_signed_urls_but_keys_browser_profiles() {
        assert!(
            manifest_preview_cache_key(
                "https://cache-preview.example.test/master.m3u8?X-Amz-Signature=secret",
                "none",
                None,
            )
            .is_none(),
            "signed manifest URLs must not be retained"
        );
        let firefox = manifest_preview_cache_key(
            "https://cache-preview.example.test/master.m3u8",
            "firefox",
            None,
        );
        assert!(
            firefox.is_some(),
            "a browser profile is keyed by the browser setting with a short TTL"
        );
        assert_ne!(
            firefox,
            manifest_preview_cache_key(
                "https://cache-preview.example.test/master.m3u8",
                "chrome",
                None,
            ),
            "each browser profile must get its own preview entry"
        );
    }

    #[test]
    fn preview_cache_hashes_handoff_cookie_contents_not_paths() {
        let directory = tempfile::tempdir().expect("temporary cookie directory");
        let first = directory.path().join("first.jar");
        let second = directory.path().join("second.jar");
        let contents = b"# Netscape HTTP Cookie File\n.example.test\tTRUE\t/\tFALSE\t0\tsid\tv\n";
        std::fs::write(&first, contents).expect("first jar");
        std::fs::write(&second, contents).expect("second jar");

        assert_eq!(
            handoff_cookie_hash(&first),
            handoff_cookie_hash(&second),
            "a new staged path with the same cookies must reuse the preview"
        );

        std::fs::write(&second, b"# Netscape HTTP Cookie File\n").expect("changed jar");
        assert_ne!(handoff_cookie_hash(&first), handoff_cookie_hash(&second));
        assert!(handoff_cookie_hash(&directory.path().join("missing.jar")).is_none());
    }

    #[test]
    fn preview_cache_key_only_matches_the_source_that_served_it() {
        let url = "https://cache-preview.example.test/master.m3u8";
        let anonymous = manifest_preview_cache_key(url, "none", None).expect("anonymous key");
        let browser = manifest_preview_cache_key(url, "firefox", None).expect("browser key");
        let handoff = ManifestPreviewCacheKey {
            handoff_cookie_hash: Some([7_u8; 32]),
            ..browser.clone()
        };

        assert!(anonymous.matches_auth(ManifestClientAuth::Anonymous));
        assert!(!anonymous.matches_auth(ManifestClientAuth::Handoff));
        assert!(!anonymous.matches_auth(ManifestClientAuth::Browser));

        assert!(browser.matches_auth(ManifestClientAuth::Browser));
        assert!(
            !browser.matches_auth(ManifestClientAuth::Anonymous),
            "a failed cookie export must not store an anonymous preview under a profile key"
        );

        assert!(handoff.matches_auth(ManifestClientAuth::Handoff));
        assert!(!handoff.matches_auth(ManifestClientAuth::Browser));
    }
}

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
mod content_steering_tests {
    use super::*;

    const MASTER_URL: &str = "https://cdn.example.test/master.m3u8";

    fn master() -> crate::manifest::HlsMasterPlan {
        crate::manifest::parse_hls_master_playlist(
            MASTER_URL,
            br#"#EXTM3U
#EXT-X-CONTENT-STEERING:SERVER-URI="steering.json",PATHWAY-ID="cdn-a"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio-a",NAME="English",URI="audio/a.m3u8",STABLE-RENDITION-ID="audio-en"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio-b",NAME="English",URI="audio/b.m3u8",STABLE-RENDITION-ID="audio-en"
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs-a",NAME="English",URI="subs/a.m3u8",STABLE-RENDITION-ID="subs-en"
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs-b",NAME="English",URI="subs/b.m3u8",STABLE-RENDITION-ID="subs-en"
#EXT-X-STREAM-INF:BANDWIDTH=1200000,AUDIO="audio-a",SUBTITLES="subs-a",STABLE-VARIANT-ID="main",PATHWAY-ID="cdn-a"
video/a.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=1200000,AUDIO="audio-b",SUBTITLES="subs-b",STABLE-VARIANT-ID="main",PATHWAY-ID="cdn-b"
video/b.m3u8
"#,
        )
        .expect("valid Content Steering master")
    }

    #[test]
    fn steering_request_url_preserves_existing_query_and_adds_current_pathway() {
        let steering = crate::manifest::HlsContentSteering {
            server_url: Url::parse("https://steering.example.test/path/list.json?signature=abc123")
                .expect("valid steering URL"),
            initial_pathway_id: Some("cdn-a".to_string()),
        };
        let url = hls_steering_request_url(&steering).expect("build steering request URL");
        let pairs = url.query_pairs().collect::<Vec<_>>();
        assert!(
            pairs
                .iter()
                .any(|(name, value)| name == "signature" && value == "abc123")
        );
        assert!(
            pairs
                .iter()
                .any(|(name, value)| name == "_HLS_pathway" && value == "cdn-a")
        );
    }

    #[test]
    fn steering_request_url_rejects_preexisting_reserved_pathway_query() {
        let steering = crate::manifest::HlsContentSteering {
            server_url: Url::parse(
                "https://steering.example.test/path/list.json?_HLS_pathway=other",
            )
            .expect("valid steering URL"),
            initial_pathway_id: Some("cdn-a".to_string()),
        };
        assert!(matches!(
            hls_steering_request_url(&steering),
            Err(ManifestIntakeError::Unsupported(_))
        ));
    }

    #[test]
    fn parses_and_applies_content_steering_pathway_clones() {
        let json = br#"{
            "VERSION": 1,
            "TTL": 300,
            "PATHWAY-PRIORITY": ["clone-a", "cdn-b"],
            "PATHWAY-CLONES": [{
                "BASE-ID": "cdn-a",
                "ID": "clone-a",
                "URI-REPLACEMENT": {
                    "HOST": "backup.example.test",
                    "PARAMS": {"z": "last", "token": "rotated"},
                    "PER-VARIANT-URIS": {
                        "main": "https://video.example.test/override.m3u8"
                    },
                    "PER-RENDITION-URIS": {
                        "audio-en": "https://audio.example.test/override.m3u8",
                        "subs-en": "https://subs.example.test/override.m3u8"
                    }
                }
            }]
        }"#;
        let steering = parse_hls_steering_manifest(json).expect("valid clone manifest");
        assert_eq!(steering.pathway_priority, ["clone-a", "cdn-b"]);
        assert_eq!(steering.pathway_clones.len(), 1);

        let mut master = master();
        apply_hls_pathway_clones(&mut master, &steering.pathway_clones);
        assert_eq!(master.variants.len(), 3);
        assert_eq!(master.audio_renditions.len(), 3);
        assert_eq!(master.subtitle_renditions.len(), 3);
        validate_hls_content_steering_stable_ids(&master)
            .expect("cloned pathways retain matching stable identities");

        let cloned = master
            .variants
            .iter()
            .find(|variant| variant.pathway_id == "clone-a")
            .expect("cloned video variant");
        assert_eq!(
            cloned.url.as_str(),
            "https://video.example.test/override.m3u8"
        );
        let cloned_audio = master
            .audio_renditions
            .iter()
            .find(|rendition| rendition.group_id == cloned.audio_group.as_deref().unwrap())
            .expect("cloned audio rendition");
        assert_eq!(
            cloned_audio.url.as_ref().unwrap().as_str(),
            "https://audio.example.test/override.m3u8"
        );
        let cloned_subtitles = master
            .subtitle_renditions
            .iter()
            .find(|rendition| rendition.group_id == cloned.subtitles_group.as_deref().unwrap())
            .expect("cloned subtitle rendition");
        assert_eq!(
            cloned_subtitles.url.as_str(),
            "https://subs.example.test/override.m3u8"
        );

        select_hls_content_steering_pathway(&mut master, &steering.pathway_priority)
            .expect("select cloned pathway");
        assert_eq!(master.variants.len(), 1);
        assert_eq!(master.variants[0].pathway_id, "clone-a");
        assert_eq!(master.audio_renditions.len(), 1);
        assert_eq!(master.subtitle_renditions.len(), 1);
    }

    #[test]
    fn query_parameter_replacement_decodes_names_without_form_plus_semantics() {
        let source =
            Url::parse("https://origin.example.test/a.m3u8?token%2Bkey=old&keep=yes").unwrap();
        let clone = HlsPathwayClone {
            base_id: "origin".to_string(),
            id: "backup".to_string(),
            host: None,
            params: BTreeMap::from([("token%2Bkey".to_string(), "new%2Fvalue".to_string())]),
            per_variant_uris: HashMap::new(),
            per_rendition_uris: HashMap::new(),
        };
        let replaced =
            replace_hls_pathway_uri(&source, &clone, None).expect("safe replacement URL");
        assert_eq!(replaced.query(), Some("keep=yes&token%2Bkey=new%2Fvalue"));
    }

    #[test]
    fn pathway_clone_applies_variant_and_video_rendition_uri_overrides_separately() {
        let mut master = crate::manifest::parse_hls_master_playlist(
            MASTER_URL,
            br#"#EXTM3U
#EXT-X-CONTENT-STEERING:SERVER-URI="steering.json",PATHWAY-ID="cdn-a"
#EXT-X-MEDIA:TYPE=VIDEO,GROUP-ID="angles",NAME="Main",URI="video/angle.m3u8",STABLE-RENDITION-ID="angle-main"
#EXT-X-STREAM-INF:BANDWIDTH=1200000,VIDEO="angles",STABLE-VARIANT-ID="stream-main",PATHWAY-ID="cdn-a"
video/variant.m3u8
"#,
        )
        .expect("video-rendition master playlist");
        let clone = parse_hls_steering_manifest(
            br#"{
                "VERSION": 1,
                "TTL": 300,
                "PATHWAY-PRIORITY": ["clone-a"],
                "PATHWAY-CLONES": [{
                    "BASE-ID": "cdn-a",
                    "ID": "clone-a",
                    "URI-REPLACEMENT": {
                        "HOST": "backup.example.test",
                        "PER-VARIANT-URIS": {
                            "stream-main": "https://variant.example.test/override.m3u8"
                        },
                        "PER-RENDITION-URIS": {
                            "angle-main": "https://rendition.example.test/override.m3u8"
                        }
                    }
                }]
            }"#,
        )
        .expect("valid video pathway clone");
        apply_hls_pathway_clones(&mut master, &clone.pathway_clones);
        let cloned = master
            .variants
            .iter()
            .find(|variant| variant.pathway_id == "clone-a")
            .expect("cloned video variant");
        assert_ne!(
            cloned.video_rendition_group, master.variants[0].video_rendition_group,
            "the video rendition group must be unique to its cloned Pathway"
        );
        assert_eq!(
            cloned.video_variant_url.as_ref().unwrap().as_str(),
            "https://variant.example.test/override.m3u8"
        );
        assert_eq!(
            cloned.url.as_str(),
            "https://rendition.example.test/override.m3u8"
        );
        validate_hls_content_steering_stable_ids(&master)
            .expect("video rendition clone preserves its stable identities");
    }

    #[test]
    fn pathway_clone_accepts_bracketed_ipv6_host_without_port() {
        let source = Url::parse("https://origin.example.test/a.m3u8").unwrap();
        let clone = HlsPathwayClone {
            base_id: "origin".to_string(),
            id: "ipv6".to_string(),
            host: Some("[2001:4860:4860::8888]".to_string()),
            params: BTreeMap::new(),
            per_variant_uris: HashMap::new(),
            per_rendition_uris: HashMap::new(),
        };
        let replaced = replace_hls_pathway_uri(&source, &clone, None)
            .expect("public IPv6 literal should be accepted");
        assert_eq!(replaced.host_str(), Some("[2001:4860:4860::8888]"));
    }

    #[test]
    fn pathway_clone_applies_host_and_sorted_query_parameter_replacements() {
        let source = Url::parse("https://origin.example.test/a.m3u8?token=old&keep=yes").unwrap();
        let clone = HlsPathwayClone {
            base_id: "origin".to_string(),
            id: "backup".to_string(),
            host: Some("backup.example.test".to_string()),
            params: BTreeMap::from([
                ("z".to_string(), "last".to_string()),
                ("token".to_string(), "new%20value".to_string()),
                ("a".to_string(), "first".to_string()),
            ]),
            per_variant_uris: HashMap::new(),
            per_rendition_uris: HashMap::new(),
        };
        let replaced =
            replace_hls_pathway_uri(&source, &clone, None).expect("safe replacement URL");
        assert_eq!(replaced.host_str(), Some("backup.example.test"));
        assert_eq!(
            replaced.query(),
            Some("keep=yes&a=first&token=new%20value&z=last")
        );
    }

    #[test]
    fn ignores_unknown_clone_base_and_rejects_invalid_clone_shapes() {
        let json = br#"{
            "VERSION": 1,
            "TTL": 300,
            "PATHWAY-PRIORITY": ["missing", "cdn-a"],
            "PATHWAY-CLONES": [{
                "BASE-ID": "not-advertised",
                "ID": "missing",
                "URI-REPLACEMENT": {"HOST": "backup.example.test"}
            }]
        }"#;
        let steering = parse_hls_steering_manifest(json).expect("valid clone with unknown base");
        let mut master = master();
        apply_hls_pathway_clones(&mut master, &steering.pathway_clones);
        assert_eq!(master.variants.len(), 2);

        for invalid in [
            br#"{"VERSION":1,"TTL":1,"PATHWAY-PRIORITY":["a","a"]}"#.as_slice(),
            br#"{"VERSION":1,"TTL":1,"PATHWAY-PRIORITY":["a"],"PATHWAY-CLONES":[]}"#.as_slice(),
            br#"{"VERSION":1,"TTL":1,"PATHWAY-PRIORITY":["a"],"PATHWAY-CLONES":[{"BASE-ID":"a","ID":"b","URI-REPLACEMENT":{"HOST":"backup.example.test:8443"}}]}"#.as_slice(),
            br#"{"VERSION":1,"TTL":1,"PATHWAY-PRIORITY":["a"],"PATHWAY-CLONES":[{"BASE-ID":"a","ID":"b","URI-REPLACEMENT":{"HOST":"backup.example.test:443"}}]}"#.as_slice(),
            br#"{"VERSION":1,"TTL":1,"PATHWAY-PRIORITY":["a"],"PATHWAY-CLONES":[{"BASE-ID":"a","ID":"b","URI-REPLACEMENT":{"HOST":"[2001:4860:4860::8888]:443"}}]}"#.as_slice(),
            br#"{"VERSION":1,"TTL":1,"PATHWAY-PRIORITY":["a"],"PATHWAY-CLONES":[{"BASE-ID":"a","ID":"b","URI-REPLACEMENT":{"PARAMS":{"_HLS_bad":"value"}}}]}"#.as_slice(),
            br#"{"VERSION":1,"TTL":1,"PATHWAY-PRIORITY":["a"],"PATHWAY-CLONES":[{"BASE-ID":"a","ID":"b","URI-REPLACEMENT":{"PARAMS":{"token":"bad&value"}}}]}"#.as_slice(),
        ] {
            assert!(matches!(
                parse_hls_steering_manifest(invalid),
                Err(ManifestIntakeError::Unsupported(_))
            ));
        }
    }

    #[test]
    fn pathway_clone_does_not_publish_partial_pathway_for_unsafe_override() {
        let clone = HlsPathwayClone {
            base_id: "cdn-a".to_string(),
            id: "bad-clone".to_string(),
            host: None,
            params: BTreeMap::new(),
            per_variant_uris: HashMap::from([(
                "main".to_string(),
                "http://insecure.example.test/video.m3u8".to_string(),
            )]),
            per_rendition_uris: HashMap::new(),
        };
        let mut master = master();
        apply_hls_pathway_clones(&mut master, &[clone]);
        assert_eq!(master.variants.len(), 2);
        assert_eq!(master.audio_renditions.len(), 2);
        assert_eq!(master.subtitle_renditions.len(), 2);
    }

    #[test]
    fn selects_priority_pathway_and_filters_its_audio_and_subtitle_groups() {
        let mut master = master();
        validate_hls_content_steering_stable_ids(&master)
            .expect("all direct pathways have matching stable track identities");
        select_hls_content_steering_pathway(
            &mut master,
            &["unavailable".to_string(), "cdn-b".to_string()],
        )
        .expect("select the first available priority pathway");
        assert_eq!(master.variants.len(), 1);
        assert_eq!(master.variants[0].pathway_id, "cdn-b");
        assert!(master.variants[0].url.path().ends_with("/video/b.m3u8"));
        assert_eq!(master.audio_renditions.len(), 1);
        assert!(
            master.audio_renditions[0]
                .url
                .as_ref()
                .expect("audio URI")
                .path()
                .ends_with("/audio/b.m3u8")
        );
        assert_eq!(master.subtitle_renditions.len(), 1);
        assert!(
            master.subtitle_renditions[0]
                .url
                .path()
                .ends_with("/subs/b.m3u8")
        );
    }

    #[test]
    fn uses_initial_pathway_when_no_direct_priority_path_is_available() {
        let mut master = master();
        select_hls_content_steering_pathway(&mut master, &["unknown".to_string()])
            .expect("use the initial pathway while no direct priority path is available");
        assert_eq!(master.variants.len(), 1);
        assert_eq!(master.variants[0].pathway_id, "cdn-a");
        assert!(master.variants[0].url.path().ends_with("/video/a.m3u8"));
    }

    #[test]
    fn uses_first_advertised_pathway_when_steering_has_no_initial_pathway() {
        let mut master = master();
        master
            .content_steering
            .as_mut()
            .expect("steering declaration")
            .initial_pathway_id = None;
        select_hls_content_steering_pathway(&mut master, &[])
            .expect("fall back to the first advertised Pathway");
        assert_eq!(master.variants.len(), 1);
        assert_eq!(master.variants[0].pathway_id, "cdn-a");
    }

    #[test]
    fn refuses_pathways_without_matching_stable_track_sets() {
        let mut master = master();
        master.variants[1].stable_variant_id = Some("different-variant".to_string());
        assert!(matches!(
            validate_hls_content_steering_stable_ids(&master),
            Err(ManifestIntakeError::Unsupported(_))
        ));
    }

    #[test]
    fn steering_fetch_limits_follow_preview_and_no_download_retry_policy() {
        assert_eq!(
            hls_steering_fetch_limits(ManifestFetchMode::Preview),
            (
                MANIFEST_PREVIEW_REQUEST_TIMEOUT,
                MAX_PREVIEW_MANIFEST_ATTEMPTS
            )
        );
        assert_eq!(
            hls_steering_fetch_limits(ManifestFetchMode::Download),
            (MANIFEST_REQUEST_TIMEOUT, 1)
        );
    }

    async fn steering_fixture_response(
        status: u16,
        body: Vec<u8>,
        check_for_retry: bool,
    ) -> (Url, tokio::task::JoinHandle<(String, bool)>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind steering fixture");
        let address = listener.local_addr().expect("fixture address");
        let url =
            Url::parse(&format!("http://{address}/steering.json")).expect("fixture steering URL");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept steering request");
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 1024];
                let read = stream.read(&mut chunk).await.expect("read request");
                assert_ne!(read, 0, "request ended before HTTP headers");
                request.extend_from_slice(&chunk[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
                assert!(request.len() < 16 * 1024, "request headers stay bounded");
            }
            let request = String::from_utf8(request).expect("request is UTF-8");
            let reason = match status {
                200 => "OK",
                503 => "Service Unavailable",
                _ => "Fixture",
            };
            let headers = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            // A bounded reader may reject an oversized Content-Length without
            // consuming the body, so a peer closing early is expected there.
            let _ = stream.write_all(headers.as_bytes()).await;
            let _ = stream.write_all(&body).await;

            let second_request = if check_for_retry {
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_ok()
            } else {
                false
            };
            (request, second_request)
        });
        (url, server)
    }

    fn set_steering_url(master: &mut crate::manifest::HlsMasterPlan, url: Url) {
        master
            .content_steering
            .as_mut()
            .expect("steering declaration")
            .server_url = url;
    }

    fn steering_test_client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build test HTTP client")
    }

    #[tokio::test(flavor = "current_thread")]
    async fn content_steering_fetch_selects_priority_from_network_response() {
        let body = br#"{"VERSION":1,"TTL":300,"PATHWAY-PRIORITY":["cdn-b"]}"#.to_vec();
        let (steering_url, server) = steering_fixture_response(200, body, false).await;
        let master_url = steering_url.join("/master.m3u8").expect("master URL");
        let mut master = master();
        set_steering_url(&mut master, steering_url);
        let selected = apply_hls_content_steering(
            &steering_test_client(),
            &master_url,
            master,
            ManifestTransportPolicy::HttpFixture,
            ManifestFetchMode::Preview,
            false,
        )
        .await
        .expect("network steering response should be applied");
        let (request, _) = server.await.expect("steering fixture task");
        assert!(request.starts_with("GET /steering.json?_HLS_pathway=cdn-a "));
        assert_eq!(selected.variants.len(), 1);
        assert_eq!(selected.variants[0].pathway_id, "cdn-b");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn malformed_steering_network_response_falls_back_to_initial_pathway() {
        let (steering_url, server) =
            steering_fixture_response(200, b"not-json".to_vec(), false).await;
        let master_url = steering_url.join("/master.m3u8").expect("master URL");
        let mut master = master();
        set_steering_url(&mut master, steering_url);
        let selected = apply_hls_content_steering(
            &steering_test_client(),
            &master_url,
            master,
            ManifestTransportPolicy::HttpFixture,
            ManifestFetchMode::Preview,
            false,
        )
        .await
        .expect("invalid optional steering response should fall back");
        server.await.expect("steering fixture task");
        assert_eq!(selected.variants.len(), 1);
        assert_eq!(selected.variants[0].pathway_id, "cdn-a");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn preview_steering_server_error_is_not_retried() {
        let (steering_url, server) = steering_fixture_response(503, Vec::new(), true).await;
        let master_url = steering_url.join("/master.m3u8").expect("master URL");
        let mut master = master();
        set_steering_url(&mut master, steering_url);
        let selected = apply_hls_content_steering(
            &steering_test_client(),
            &master_url,
            master,
            ManifestTransportPolicy::HttpFixture,
            ManifestFetchMode::Preview,
            false,
        )
        .await
        .expect("failed optional steering request should fall back");
        let (request, second_request) = server.await.expect("steering fixture task");
        assert!(request.starts_with("GET /steering.json"));
        assert!(!second_request, "preview must not retry an HTTP 503");
        assert_eq!(selected.variants[0].pathway_id, "cdn-a");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn oversized_steering_response_is_rejected_by_streaming_reader() {
        let (steering_url, server) =
            steering_fixture_response(200, vec![b'x'; MAX_HLS_STEERING_MANIFEST_BYTES + 1], false)
                .await;
        let master_url = steering_url.join("/master.m3u8").expect("master URL");
        let steering = crate::manifest::HlsContentSteering {
            server_url: steering_url,
            initial_pathway_id: Some("cdn-a".to_string()),
        };
        let error = fetch_hls_steering_manifest(
            &steering_test_client(),
            &master_url,
            &steering,
            ManifestTransportPolicy::HttpFixture,
            ManifestFetchMode::Preview,
            false,
        )
        .await
        .expect_err("oversized response must be rejected before full buffering");
        server.await.expect("steering fixture task");
        assert!(error.to_string().contains("byte limit"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn private_cross_origin_steering_server_is_rejected_before_connect() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind private destination sentinel");
        let address = listener.local_addr().expect("sentinel address");
        let master_url = Url::parse("http://master.example.test/master.m3u8").expect("master URL");
        let steering = crate::manifest::HlsContentSteering {
            server_url: Url::parse(&format!("http://{address}/steering.json"))
                .expect("private steering URL"),
            initial_pathway_id: Some("cdn-a".to_string()),
        };
        let error = fetch_hls_steering_manifest(
            &steering_test_client(),
            &master_url,
            &steering,
            ManifestTransportPolicy::HttpFixture,
            ManifestFetchMode::Preview,
            false,
        )
        .await
        .expect_err("private cross-origin SERVER-URI must be rejected");
        assert!(error.to_string().contains("public-host safety policy"));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err(),
            "the private listener must not receive a request"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cross_origin_steering_dns_rebinding_to_private_ip_is_blocked() {
        let _policy = crate::download_net::isolate_untrusted_handoff_policy_for_test();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind private DNS sentinel");
        let address = listener.local_addr().expect("sentinel address");
        let steering_host = "steering.example.test";
        let steering_url = Url::parse(&format!(
            "http://{steering_host}:{}/steering.json",
            address.port()
        ))
        .expect("steering URL");
        let master_url = Url::parse("http://master.example.test/master.m3u8").expect("master URL");
        let (client, dns_override_hits) =
            crate::download_net::http_client_for_with_dns_overrides_for_test(
                None,
                HashMap::from([(
                    steering_host.to_string(),
                    vec![
                        format!("127.0.0.1:{}", address.port())
                            .parse()
                            .expect("private DNS answer"),
                    ],
                )]),
            )
            .expect("build guarded client with private DNS override");
        let steering = crate::manifest::HlsContentSteering {
            server_url: steering_url,
            initial_pathway_id: Some("cdn-a".to_string()),
        };

        let error = fetch_hls_steering_manifest(
            &client,
            &master_url,
            &steering,
            ManifestTransportPolicy::HttpFixture,
            ManifestFetchMode::Preview,
            false,
        )
        .await
        .expect_err("a public-looking steering host resolving privately must fail closed");
        assert!(
            dns_override_hits.load(std::sync::atomic::Ordering::Relaxed),
            "the guarded resolver must inspect the steering host's DNS answer"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err(),
            "the private DNS answer must never receive the steering request"
        );
        assert!(!error.to_string().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn steering_fetch_is_skipped_when_upstream_proxy_is_configured() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind proxy sentinel");
        let address = listener.local_addr().expect("sentinel address");
        let master_url = Url::parse(&format!("http://{address}/master.m3u8")).expect("master URL");
        let steering = crate::manifest::HlsContentSteering {
            server_url: Url::parse(&format!("http://{address}/steering.json"))
                .expect("steering URL"),
            initial_pathway_id: Some("cdn-a".to_string()),
        };
        let error = fetch_hls_steering_manifest(
            &steering_test_client(),
            &master_url,
            &steering,
            ManifestTransportPolicy::HttpFixture,
            ManifestFetchMode::Preview,
            true,
        )
        .await
        .expect_err("proxy-backed steering must be skipped");
        assert!(error.to_string().contains("upstream proxy"));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err(),
            "proxy policy must reject before issuing a request"
        );
    }
}
