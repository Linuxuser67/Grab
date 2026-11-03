//! Persistent cache for successful, static yt-dlp video metadata.
//!
//! The upstream yt_dlp::cache::VideoCache supplies Moka L1 and Redb L2.
//! Grab owns cache identity because the same page can produce different
//! metadata under different cookies, user agents, proxies, or extractor args.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::OnceCell;
use yt_dlp::cache::{CacheConfig, VideoCache};
use yt_dlp::model::Video;

const VIDEO_CACHE_TTL: Duration = Duration::from_secs(15 * 60);
const CLEAN_INTERVAL: Duration = Duration::from_secs(5 * 60);

static VIDEO_CACHE: OnceCell<VideoCache> = OnceCell::const_new();
static LAST_CLEAN_AT: AtomicU64 = AtomicU64::new(0);

/// Return cached metadata only when the URL and current extraction identity
/// match. Errors in the optional cache never prevent a normal yt-dlp lookup.
pub(crate) async fn lookup_video(
    raw_url: &str,
    youtube_bin: &Path,
    yt_version: &str,
    cookies_browser: &str,
    proxy: Option<&crate::net_types::ResolvedProxy>,
) -> Option<Video> {
    let key = cache_key(raw_url, youtube_bin, yt_version, cookies_browser, proxy)?;
    let cache = video_cache().await?;
    clean_expired_if_due(cache).await;

    match cache.get(&key).await {
        Ok(Some(video)) if is_cacheable_video(&video) && video.are_format_urls_fresh() => {
            tracing::debug!(url_host = %crate::video_probe::page_host(raw_url), "yt-dlp metadata cache hit");
            Some(video)
        }
        Ok(Some(_)) => {
            if let Err(error) = cache.remove(&key).await {
                tracing::warn!(error = %error, "could not remove stale yt-dlp metadata cache entry");
            }
            None
        }
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(error = %error, "yt-dlp metadata cache read failed; probing normally");
            None
        }
    }
}

/// Store only parsed, static single-video metadata. Never cache playlist
/// stubs, live streams, or failed/empty extractions.
pub(crate) async fn store_video(
    raw_url: &str,
    youtube_bin: &Path,
    yt_version: &str,
    cookies_browser: &str,
    proxy: Option<&crate::net_types::ResolvedProxy>,
    video: &Video,
) {
    if !is_cacheable_video(video) {
        return;
    }
    let Some(key) = cache_key(raw_url, youtube_bin, yt_version, cookies_browser, proxy) else {
        return;
    };
    let Some(cache) = video_cache().await else {
        return;
    };

    if let Err(error) = cache.put(key, video.clone()).await {
        tracing::warn!(error = %error, "yt-dlp metadata cache write failed");
        return;
    }
    clean_expired_if_due(cache).await;
}

fn is_cacheable_video(video: &Video) -> bool {
    !crate::video_types::yt_dlp_reports_currently_live(video.is_live, &video.live_status)
        && crate::video_types::has_fetchable_media(video)
}

async fn video_cache() -> Option<&'static VideoCache> {
    match VIDEO_CACHE
        .get_or_try_init(|| async {
            let cache_dir = secure_cache_directory().map_err(|error| error.to_string())?;
            let config = CacheConfig::builder().cache_dir(cache_dir.clone()).build();
            let cache = VideoCache::new(&config, Some(VIDEO_CACHE_TTL.as_secs()))
                .await
                .map_err(|error| error.to_string())?;
            secure_cache_file(&cache_dir.join("videos.redb"))
                .map_err(|error| error.to_string())?;
            Ok::<VideoCache, String>(cache)
        })
        .await
    {
        Ok(cache) => Some(cache),
        Err(error) => {
            tracing::warn!(error = %error, "yt-dlp metadata cache unavailable; probing normally");
            None
        }
    }
}

/// Use the stable XDG cache location, not the runtime directory used for
/// transient cookie handoff files. The database contains extractor URLs.
fn secure_cache_directory() -> std::io::Result<PathBuf> {
    let directory = glib::user_cache_dir()
        .join(crate::APP_ID)
        .join("yt-dlp");
    std::fs::create_dir_all(&directory)?;

    let metadata = std::fs::symlink_metadata(&directory)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "yt-dlp cache path is not a real directory",
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut permissions = metadata.permissions();
        if permissions.mode() & 0o077 != 0 {
            permissions.set_mode(0o700);
            std::fs::set_permissions(&directory, permissions)?;
        }
    }
    Ok(directory)
}

/// Restrict the persistent database itself as well as its containing directory.
fn secure_cache_file(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "yt-dlp cache database is not a regular file",
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut permissions = metadata.permissions();
        if permissions.mode() & 0o077 != 0 {
            permissions.set_mode(0o600);
            std::fs::set_permissions(path, permissions)?;
        }
    }
    Ok(())
}

/// Redb TTL expiry is lazy; periodically reclaim expired rows while the cache
/// is active, rather than allowing old entries to accumulate indefinitely.
async fn clean_expired_if_due(cache: &VideoCache) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let interval = CLEAN_INTERVAL.as_secs();
    let previous = LAST_CLEAN_AT.load(Ordering::Relaxed);
    if now.saturating_sub(previous) < interval
        || LAST_CLEAN_AT
            .compare_exchange(previous, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return;
    }

    if let Err(error) = cache.clean().await {
        LAST_CLEAN_AT.store(0, Ordering::Relaxed);
        tracing::warn!(error = %error, "yt-dlp metadata cache cleanup failed");
    }
}

/// Cache identity includes all inputs that can affect extractor output. Hash
/// credentials and proxy URLs instead of writing them into the Redb keys.
fn cache_key(
    raw_url: &str,
    youtube_bin: &Path,
    yt_version: &str,
    cookies_browser: &str,
    proxy: Option<&crate::net_types::ResolvedProxy>,
) -> Option<String> {
    let mut url = url::Url::parse(raw_url).ok()?;
    url.set_fragment(None);
    if has_rotating_auth_query(&url) {
        return None;
    }

    let handoff = crate::extension_server::live_handoff_identity(raw_url, cookies_browser);
    let browser_profile_is_active =
        crate::video_tools::cookies_browser_spec(cookies_browser).is_some();
    // A browser profile can rotate outside Grab. Only a concrete, current
    // extension snapshot makes that authentication context cacheable.
    if browser_profile_is_active && handoff.is_none() {
        return None;
    }

    let mut hasher = Sha256::new();
    hash_field(&mut hasher, b"schema", b"grab-ytdlp-video-v1");
    hash_field(&mut hasher, b"url", url.as_str().as_bytes());
    hash_field(&mut hasher, b"yt-dlp-version", yt_version.as_bytes());
    hash_field(&mut hasher, b"binary-path", youtube_bin.as_os_str().to_string_lossy().as_bytes());

    let binary_metadata = std::fs::metadata(youtube_bin).ok()?;
    if !binary_metadata.is_file() {
        return None;
    }
    hash_field(&mut hasher, b"binary-size", &binary_metadata.len().to_be_bytes());
    if let Ok(modified) = binary_metadata.modified() {
        hash_system_time(&mut hasher, b"binary-modified", modified);
    }

    // This contains the selected extractor args, Deno runtime path, staged
    // cookie path, and paired User-Agent. It is hashed, never stored verbatim.
    for argument in crate::extension_server::ytdlp_spawn_identity(cookies_browser, raw_url) {
        hash_field(&mut hasher, b"identity-arg", argument.as_bytes());
    }

    if let Some((path, user_agent)) = handoff {
        let metadata = std::fs::metadata(&path).ok()?;
        if !metadata.is_file() || std::fs::File::open(&path).is_err() {
            return None;
        }
        hash_field(&mut hasher, b"handoff-cookie-path", path.as_os_str().to_string_lossy().as_bytes());
        hash_field(&mut hasher, b"handoff-cookie-size", &metadata.len().to_be_bytes());
        if let Ok(modified) = metadata.modified() {
            hash_system_time(&mut hasher, b"handoff-cookie-modified", modified);
        }
        if let Some(user_agent) = user_agent {
            hash_field(&mut hasher, b"handoff-user-agent", user_agent.as_bytes());
        }
    }

    if let Some(proxy) = proxy {
        hash_field(&mut hasher, b"proxy-url", proxy.cli_url.as_bytes());
        hash_field(&mut hasher, b"proxy-bypass", proxy.no_proxy_env.as_bytes());
    }
    for name in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
    ] {
        if let Some(value) = std::env::var_os(name) {
            hash_field(&mut hasher, name.as_bytes(), value.to_string_lossy().as_bytes());
        }
    }

    let impersonation = crate::settings::AppSettings::new().impersonate_browser();
    hash_field(&mut hasher, b"impersonation", impersonation.as_bytes());

    Some(format!("grab-ytdlp-video-v1:{:x}", hasher.finalize()))
}

fn hash_field(hasher: &mut Sha256, name: &[u8], value: &[u8]) {
    hasher.update((name.len() as u64).to_be_bytes());
    hasher.update(name);
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn hash_system_time(hasher: &mut Sha256, name: &[u8], time: SystemTime) {
    if let Ok(duration) = time.duration_since(UNIX_EPOCH) {
        hash_field(hasher, name, &duration.as_nanos().to_be_bytes());
    }
}

/// URLs carrying rotating authentication/signature parameters are not reusable.
fn has_rotating_auth_query(url: &url::Url) -> bool {
    url.query_pairs().any(|(name, _)| {
        matches!(
            name.to_ascii_lowercase().as_str(),
            "token"
                | "access_token"
                | "auth"
                | "authorization"
                | "sig"
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
    })
}

#[cfg(test)]
mod tests {
    use super::has_rotating_auth_query;

    #[test]
    fn rejects_rotating_auth_query_parameters() {
        for url in [
            "https://example.test/watch?v=1&token=abc",
            "https://example.test/watch?signature=abc",
            "https://example.test/watch?X-Amz-Signature=abc",
        ] {
            assert!(has_rotating_auth_query(&url::Url::parse(url).unwrap()));
        }
        assert!(!has_rotating_auth_query(
            &url::Url::parse("https://example.test/watch?v=1").unwrap()
        ));
    }
}
