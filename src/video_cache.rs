//! Cache for successful, static yt-dlp video metadata.
//!
//! The upstream yt_dlp::cache::VideoCache supplies Moka L1 and Redb L2.
//! Grab owns cache identity because the same page can produce different
//! metadata under different cookies, user agents, proxies, or extractor args.
//! Authenticated previews stay in memory; everything else is persisted.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::OnceCell;
use yt_dlp::cache::{CacheConfig, VideoCache};
use yt_dlp::model::Video;

const VIDEO_CACHE_TTL: Duration = Duration::from_secs(15 * 60);
const CLEAN_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Authenticated previews stay in memory: the Redb layer keys rows by video id,
/// so two identities for one video would overwrite each other, and these entries
/// carry signed media URLs. A staged handoff jar outlives this window.
const AUTHENTICATED_PREVIEW_TTL: Duration = VIDEO_CACHE_TTL;
const AUTHENTICATED_PREVIEW_CAPACITY: usize = 32;
/// Namespace for video keys that must never reach the persistent cache.
const AUTHENTICATED_VIDEO_NAMESPACE: &str = "grab-ytdlp-video-auth-v1";

static VIDEO_CACHE: OnceCell<VideoCache> = OnceCell::const_new();
static LAST_CLEAN_AT: AtomicU64 = AtomicU64::new(0);
static AUTHENTICATED_PREVIEWS: LazyLock<Mutex<HashMap<String, (Instant, Video)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Return cached metadata only when the URL and current extraction identity
/// match. Errors in the optional cache never prevent a normal yt-dlp lookup.
pub(crate) async fn lookup_video(key: &str) -> Option<Video> {
    if key.starts_with(AUTHENTICATED_VIDEO_NAMESPACE) {
        return lookup_authenticated_preview(key);
    }
    let cache = video_cache().await?;
    clean_expired_if_due(cache).await;

    match cache.get(key).await {
        Ok(Some(cached_video)) => {
            let video = restore_persistent_cache_record(key, cached_video);
            if let Some(video) = video
                && is_cacheable_video(&video)
                && video.are_format_urls_fresh()
            {
                tracing::debug!("yt-dlp metadata cache hit");
                Some(video)
            } else {
                if let Err(error) = cache.remove(key).await {
                    tracing::warn!(error = %error, "could not remove stale yt-dlp metadata cache entry");
                }
                None
            }
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
///
/// Authenticated previews use an in-memory namespace instead of the persistent
/// cache: the crate's Redb layer keys rows by `video.id`, so two identities for
/// one video would overwrite each other, and signed media URLs stay off disk.
pub(crate) async fn store_video(key: &str, video: &Video) {
    if !is_cacheable_video(video) {
        return;
    }
    if key.starts_with(AUTHENTICATED_VIDEO_NAMESPACE) {
        store_authenticated_preview(key, video);
        return;
    }
    let Some(cache) = video_cache().await else {
        return;
    };

    let cached_video = persistent_cache_record(key, video);
    if let Err(error) = cache.put(key.to_string(), cached_video).await {
        tracing::warn!(error = %error, "yt-dlp metadata cache write failed");
        return;
    }
    clean_expired_if_due(cache).await;
}

fn is_cacheable_video(video: &Video) -> bool {
    !crate::video_types::yt_dlp_reports_currently_live(video.is_live, &video.live_status)
        && crate::video_types::has_fetchable_media(video)
}

/// Give each extraction identity a distinct Redb primary key without changing
/// the media ID used by the rest of the application.
fn persistent_cache_record(key: &str, video: &Video) -> Video {
    let mut cached_video = video.clone();
    cached_video.id = format!("{key}::{}", video.id);
    cached_video
}

/// Restore the media ID only for a record belonging to the requested extraction key.
fn restore_persistent_cache_record(key: &str, mut video: Video) -> Option<Video> {
    let media_id = video.id.strip_prefix(key)?.strip_prefix("::")?;
    video.id = media_id.to_owned();
    Some(video)
}

fn prune_authenticated_previews(previews: &mut HashMap<String, (Instant, Video)>, now: Instant) {
    previews.retain(|_, (stored_at, _)| {
        now.saturating_duration_since(*stored_at) < AUTHENTICATED_PREVIEW_TTL
    });
}

fn lookup_authenticated_preview(key: &str) -> Option<Video> {
    let mut previews = AUTHENTICATED_PREVIEWS.lock().ok()?;
    prune_authenticated_previews(&mut previews, Instant::now());
    let (_, video) = previews.get(key)?;
    if is_cacheable_video(video) && video.are_format_urls_fresh() {
        Some(video.clone())
    } else {
        None
    }
}

fn store_authenticated_preview(key: &str, video: &Video) {
    let Ok(mut previews) = AUTHENTICATED_PREVIEWS.lock() else {
        return;
    };
    prune_authenticated_previews(&mut previews, Instant::now());
    if previews.len() >= AUTHENTICATED_PREVIEW_CAPACITY && !previews.contains_key(key) {
        previews.clear();
    }
    previews.insert(key.to_string(), (Instant::now(), video.clone()));
}

async fn video_cache() -> Option<&'static VideoCache> {
    match VIDEO_CACHE
        .get_or_try_init(|| async {
            let cache_dir = secure_cache_directory().map_err(|error| error.to_string())?;
            let database = cache_dir.join("videos.redb");
            validate_cache_file_path(&database).map_err(|error| error.to_string())?;
            let config = CacheConfig::builder().cache_dir(cache_dir.clone()).build();
            let cache = VideoCache::new(&config, Some(VIDEO_CACHE_TTL.as_secs()))
                .await
                .map_err(|error| error.to_string())?;
            secure_cache_file(&database).map_err(|error| error.to_string())?;
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
pub(crate) fn secure_cache_directory() -> std::io::Result<PathBuf> {
    let directory = glib::user_cache_dir().join(crate::APP_ID).join("yt-dlp");
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

/// Reject an unsafe existing database path before Redb opens it.
///
/// A missing file is valid: Redb creates it during initialization.
pub(crate) fn validate_cache_file_path(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "yt-dlp cache database is not a regular file",
            ))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Restrict the persistent database itself as well as its containing directory.
pub(crate) fn secure_cache_file(path: &Path) -> std::io::Result<()> {
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
pub(crate) fn cache_key(
    raw_url: &str,
    youtube_bin: &Path,
    yt_version: &str,
    cookies_browser: &str,
    proxy: Option<&crate::net_types::ResolvedProxy>,
    identity_args: &[String],
) -> Option<String> {
    identity_key(
        raw_url,
        youtube_bin,
        yt_version,
        cookies_browser,
        proxy,
        identity_args,
        CacheScope::Video,
    )
}

/// Which surface a key is for. A playlist listing holds no signed media URLs,
/// so it may be scoped to a browser profile or a staged handoff jar. Video
/// metadata may be scoped to a jar's staged path, but not to a browser profile:
/// it carries signed media URLs.
enum CacheScope {
    Video,
    Playlist,
}

/// Playlist keys are scoped by the profile spec or the staged jar contents.
/// Equivalent handoffs share a key while cookie rotation changes it.
pub(crate) fn playlist_cache_key(
    raw_url: &str,
    youtube_bin: &Path,
    yt_version: &str,
    cookies_browser: &str,
    proxy: Option<&crate::net_types::ResolvedProxy>,
    identity_args: &[String],
) -> Option<String> {
    identity_key(
        raw_url,
        youtube_bin,
        yt_version,
        cookies_browser,
        proxy,
        identity_args,
        CacheScope::Playlist,
    )
}

fn identity_key(
    raw_url: &str,
    youtube_bin: &Path,
    yt_version: &str,
    cookies_browser: &str,
    proxy: Option<&crate::net_types::ResolvedProxy>,
    identity_args: &[String],
    scope: CacheScope,
) -> Option<String> {
    let mut url = url::Url::parse(raw_url).ok()?;
    url.set_fragment(None);
    if has_known_rotating_auth_query(&url) {
        return None;
    }
    canonicalize_youtube_url(&mut url);

    let browser_profile_is_active =
        crate::video_tools::cookies_browser_spec(cookies_browser).is_some();
    let has_cookie_file = identity_args
        .iter()
        .any(|argument| argument == "--cookies" || argument.starts_with("--cookies="));
    // A staged handoff jar replaces the configured browser profile. Its
    // contents, not its ephemeral path, identify the authentication context:
    // identical handoffs can reuse metadata, while cookie rotation changes
    // the key. A profile alone still refuses video metadata because it carries
    // signed media URLs; playlist listings may use either source.
    if matches!(scope, CacheScope::Video) && browser_profile_is_active && !has_cookie_file {
        return None;
    }
    // Authenticated video previews get their own namespace so they never reach
    // the persistent cache, which keys rows by video id.
    let schema = if matches!(scope, CacheScope::Video) && has_cookie_file {
        AUTHENTICATED_VIDEO_NAMESPACE
    } else {
        "grab-ytdlp-video-v1"
    };

    let mut hasher = Sha256::new();
    hash_field(&mut hasher, b"schema", schema.as_bytes());
    hash_field(&mut hasher, b"url", url.as_str().as_bytes());
    hash_field(&mut hasher, b"yt-dlp-version", yt_version.as_bytes());
    hash_field(
        &mut hasher,
        b"binary-path",
        youtube_bin.as_os_str().to_string_lossy().as_bytes(),
    );

    let binary_metadata = std::fs::metadata(youtube_bin).ok()?;
    if !binary_metadata.is_file() {
        return None;
    }
    hash_file_identity(&mut hasher, b"binary", youtube_bin, &binary_metadata);

    // Hash the identity flags, but not the positional URL (hashed above) or the
    // resolved Deno path: `find_deno` may only learn that path after the first
    // YouTube lookup, and the path does not change what is extracted.
    let mut skip_runtime_value = false;
    let mut skip_cookie_path = false;
    for argument in identity_args {
        let argument = argument.as_str();
        if argument == "--" {
            break;
        }
        if skip_runtime_value {
            skip_runtime_value = false;
            continue;
        }
        if skip_cookie_path {
            hash_cookie_file(&mut hasher, Path::new(argument))?;
            skip_cookie_path = false;
            continue;
        }
        if argument == "--js-runtimes" {
            skip_runtime_value = true;
            continue;
        }
        if argument == "--cookies" {
            skip_cookie_path = true;
            hash_field(&mut hasher, b"identity-arg", argument.as_bytes());
            continue;
        }
        if let Some(path) = argument.strip_prefix("--cookies=") {
            hash_cookie_file(&mut hasher, Path::new(path))?;
            hash_field(&mut hasher, b"identity-arg", b"--cookies");
            continue;
        }
        hash_field(&mut hasher, b"identity-arg", argument.as_bytes());
    }
    if skip_cookie_path {
        return None;
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
            hash_field(
                &mut hasher,
                name.as_bytes(),
                value.to_string_lossy().as_bytes(),
            );
        }
    }

    let impersonation = crate::settings::AppSettings::new().impersonate_browser();
    hash_field(&mut hasher, b"impersonation", impersonation.as_bytes());

    use std::fmt::Write as _;
    let mut digest_hex = String::with_capacity(64);
    for byte in hasher.finalize() {
        let _ = write!(&mut digest_hex, "{byte:02x}");
    }
    Some(format!("{schema}:{digest_hex}"))
}

/// Hash a staged cookie jar by content so per-handoff temporary paths do not
/// split cache entries. The digest is folded into the extraction key; raw
/// cookies are never stored in the key or persistent metadata.
fn hash_cookie_file(hasher: &mut Sha256, path: &Path) -> Option<()> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return None;
    }

    let mut file = std::fs::File::open(path).ok()?;
    let mut cookie_hasher = Sha256::new();
    let mut buffer = [0; 8192];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        cookie_hasher.update(&buffer[..read]);
    }
    hash_field(hasher, b"cookie-jar-content", &cookie_hasher.finalize());
    Some(())
}

fn hash_field(hasher: &mut Sha256, name: &[u8], value: &[u8]) {
    hasher.update((name.len() as u64).to_be_bytes());
    hasher.update(name);
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

/// Collapse YouTube spellings that address the same video so a share link and
/// its canonical watch URL share one cache entry. Other hosts are unchanged.
fn canonicalize_youtube_url(url: &mut url::Url) {
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    if !matches!(
        host.as_str(),
        "youtube.com" | "www.youtube.com" | "m.youtube.com" | "youtu.be"
    ) {
        return;
    }
    let path = url.path().trim_matches('/');
    let mut segments = path.split('/');
    let first = segments.next().unwrap_or_default();
    let second = segments.next();
    let video_id = if host == "youtu.be" {
        // `youtu.be/watch?v=…` is malformed; leave it to the query filter below
        // instead of reading "watch" as a video id.
        (second.is_none() && !first.is_empty() && !matches!(first, "watch" | "playlist"))
            .then(|| first.to_string())
    } else {
        match first {
            "shorts" | "embed" | "live" => second.filter(|id| !id.is_empty()).map(str::to_string),
            _ => None,
        }
    };
    let is_watch = matches!(url.path(), "/watch" | "/playlist");
    if video_id.is_none() && !is_watch {
        return;
    }
    let mut watch = if let Some(id) = video_id {
        // A `youtu.be` link can carry playlist context, so keep it: dropping
        // `list` would make a playlist share link collide with the plain video.
        let mut watch = url::Url::parse("https://www.youtube.com/watch").expect("static URL");
        watch.query_pairs_mut().append_pair("v", &id).extend_pairs(
            url.query_pairs()
                .filter(|(key, _)| key.as_ref() != "v")
                .map(|(key, value)| (key.into_owned(), value.into_owned())),
        );
        watch
    } else {
        url.clone()
    };
    watch
        .set_host(Some("www.youtube.com"))
        .expect("static YouTube host");
    // Keep only the parameters that change what yt-dlp extracts; share and
    // tracking parameters (si, t, feature, pp) do not. Sorting keeps the link
    // order from splitting the cache.
    let mut kept: Vec<(String, String)> = watch
        .query_pairs()
        .filter(|(key, _)| matches!(key.as_ref(), "v" | "list" | "index"))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    kept.sort();
    watch.set_query(None);
    if !kept.is_empty() {
        watch.query_pairs_mut().extend_pairs(kept);
    }
    *url = watch;
}

#[cfg(not(unix))]
fn hash_system_time(hasher: &mut Sha256, name: &[u8], time: SystemTime) {
    if let Ok(duration) = time.duration_since(UNIX_EPOCH) {
        hash_field(hasher, name, &duration.as_nanos().to_be_bytes());
    }
}

/// Include stable filesystem identity and high-resolution change times.
///
/// Size plus modification time alone can collide when a binary or cookie jar is
/// replaced in place while preserving its length and mtime. Unix ctime and the
/// device/inode pair distinguish those updates without hashing file contents.
fn hash_file_identity(hasher: &mut Sha256, name: &[u8], path: &Path, metadata: &std::fs::Metadata) {
    hash_field(hasher, name, path.as_os_str().to_string_lossy().as_bytes());
    hash_field(hasher, b"file-size", &metadata.len().to_be_bytes());

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        hash_field(hasher, b"file-device", &metadata.dev().to_be_bytes());
        hash_field(hasher, b"file-inode", &metadata.ino().to_be_bytes());
        hash_field(hasher, b"file-ctime", &metadata.ctime().to_be_bytes());
        hash_field(
            hasher,
            b"file-ctime-nsec",
            &metadata.ctime_nsec().to_be_bytes(),
        );
        hash_field(hasher, b"file-mtime", &metadata.mtime().to_be_bytes());
        hash_field(
            hasher,
            b"file-mtime-nsec",
            &metadata.mtime_nsec().to_be_bytes(),
        );
    }

    #[cfg(not(unix))]
    if let Ok(modified) = metadata.modified() {
        hash_system_time(hasher, b"file-modified", modified);
    }
}

/// Detect only query names with established, conventional authentication semantics.
///
/// HLS manifest caching applies a stricter policy and rejects every query-bearing
/// source/resource; this predicate is used only for yt-dlp extraction identity.
pub(crate) fn has_known_rotating_auth_query(url: &url::Url) -> bool {
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
    use super::{
        AUTHENTICATED_PREVIEW_TTL, AUTHENTICATED_VIDEO_NAMESPACE, cache_key,
        has_known_rotating_auth_query, lookup_video, persistent_cache_record, playlist_cache_key,
        prune_authenticated_previews, restore_persistent_cache_record, store_video,
        validate_cache_file_path,
    };
    use crate::net_types::ResolvedProxy;
    use std::collections::HashMap;
    use std::path::Path;
    use std::time::{Duration, Instant};

    fn fake_binary(directory: &Path) -> std::path::PathBuf {
        let binary = directory.join("yt-dlp");
        std::fs::write(&binary, b"fake yt-dlp binary").expect("write fake binary");
        binary
    }

    #[test]
    fn cache_key_scopes_proxy_and_extractor_identity() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let binary = fake_binary(directory.path());
        let url = "https://example.test/watch?v=1";
        let args = vec!["--".to_string(), url.to_string()];
        let base = cache_key(url, &binary, "2026.10.23", "none", None, &args)
            .expect("anonymous lookup should be cacheable");
        assert_eq!(
            cache_key(url, &binary, "2026.10.23", "none", None, &args),
            Some(base.clone())
        );

        let changed_args = vec![
            "--user-agent".to_string(),
            "Browser/1".to_string(),
            "--".to_string(),
            url.to_string(),
        ];
        assert_ne!(
            cache_key(url, &binary, "2026.10.23", "none", None, &changed_args),
            Some(base.clone())
        );

        let proxy = ResolvedProxy {
            proxies: Vec::new(),
            cli_url: "http://user:password@proxy.example:8080".to_string(),
            no_proxy_env: "localhost".to_string(),
        };
        assert_ne!(
            cache_key(url, &binary, "2026.10.23", "none", Some(&proxy), &args),
            Some(base)
        );
    }

    #[test]
    fn browser_profile_video_keys_are_refused_and_handoff_keys_stay_in_memory() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let binary = fake_binary(directory.path());
        let url = "https://example.test/watch?v=1";
        let anonymous = vec!["--".to_string(), url.to_string()];
        assert!(cache_key(url, &binary, "2026.10.23", "chrome", None, &anonymous).is_none());

        let cookie_file = directory.path().join("handoff.cookies");
        std::fs::write(&cookie_file, "# Netscape HTTP Cookie File\n").expect("write cookie jar");
        let with_handoff = vec![
            "--cookies".to_string(),
            cookie_file.to_string_lossy().into_owned(),
            "--user-agent".to_string(),
            "Browser/1".to_string(),
            "--".to_string(),
            url.to_string(),
        ];
        let handoff_key = cache_key(url, &binary, "2026.10.23", "none", None, &with_handoff)
            .expect("a staged handoff jar may cache video metadata for its lifetime");
        assert!(
            handoff_key.starts_with(AUTHENTICATED_VIDEO_NAMESPACE),
            "authenticated video entries must stay out of the persistent cache"
        );
        assert_eq!(
            Some(handoff_key.clone()),
            cache_key(url, &binary, "2026.10.23", "firefox", None, &with_handoff),
            "a live jar replaces the configured profile, so the profile must not refuse it"
        );

        let equals_form = vec![
            format!("--cookies={}", cookie_file.display()),
            "--".to_string(),
            url.to_string(),
        ];
        assert!(
            cache_key(url, &binary, "2026.10.23", "none", None, &equals_form).is_some(),
            "both --cookies spellings are cacheable"
        );

        let equivalent_jar = directory.path().join("equivalent-handoff.cookies");
        std::fs::write(&equivalent_jar, "# Netscape HTTP Cookie File\n")
            .expect("write equivalent cookie jar");
        let equivalent_handoff = vec![
            "--cookies".to_string(),
            equivalent_jar.to_string_lossy().into_owned(),
            "--user-agent".to_string(),
            "Browser/1".to_string(),
            "--".to_string(),
            url.to_string(),
        ];
        assert_eq!(
            Some(handoff_key.clone()),
            cache_key(
                url,
                &binary,
                "2026.10.23",
                "none",
                None,
                &equivalent_handoff
            ),
            "identical cookie contents at a new staged path must reuse the in-memory preview"
        );

        let other_jar = directory.path().join("other-handoff.cookies");
        std::fs::write(&other_jar, "# Netscape HTTP Cookie File\nchanged-cookie\n")
            .expect("write changed cookie jar");
        let other_handoff = vec![
            "--cookies".to_string(),
            other_jar.to_string_lossy().into_owned(),
            "--user-agent".to_string(),
            "Browser/1".to_string(),
            "--".to_string(),
            url.to_string(),
        ];
        assert_ne!(
            Some(handoff_key),
            cache_key(url, &binary, "2026.10.23", "none", None, &other_handoff),
            "changed cookie contents must not reuse authenticated metadata"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cache_key_changes_when_binary_is_rewritten_with_same_length() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let binary = fake_binary(directory.path());
        let url = "https://example.test/watch?v=1";
        let args = vec!["--".to_string(), url.to_string()];
        let original_mtime = std::fs::metadata(&binary)
            .expect("binary metadata")
            .modified()
            .expect("binary modification time");
        let before = cache_key(url, &binary, "2026.10.23", "none", None, &args)
            .expect("initial binary identity");

        std::fs::write(&binary, b"other fake binary!").expect("rewrite binary");
        std::fs::File::options()
            .write(true)
            .open(&binary)
            .expect("open rewritten binary")
            .set_times(std::fs::FileTimes::new().set_modified(original_mtime))
            .expect("restore binary modification time");
        let after = cache_key(url, &binary, "2026.10.23", "none", None, &args)
            .expect("updated binary identity");
        assert_ne!(before, after);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_database_symlinks_before_redb_initialization() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("temporary directory");
        let target = directory.path().join("target");
        let database = directory.path().join("videos.redb");
        std::fs::write(&target, b"outside database").expect("write target");
        symlink(&target, &database).expect("create database symlink");

        assert!(validate_cache_file_path(&database).is_err());
        assert!(validate_cache_file_path(&directory.path().join("missing.redb")).is_ok());
    }

    #[test]
    fn accepts_regular_database_path_before_redb_initialization() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = directory.path().join("videos.redb");
        std::fs::write(&database, b"database").expect("write database");
        assert!(validate_cache_file_path(&database).is_ok());
    }

    #[test]
    fn rejects_known_rotating_auth_query_parameters() {
        for url in [
            "https://example.test/watch?v=1&token=abc",
            "https://example.test/watch?signature=abc",
            "https://example.test/watch?X-Amz-Signature=abc",
        ] {
            assert!(has_known_rotating_auth_query(&url::Url::parse(url).unwrap()));
        }
        assert!(!has_known_rotating_auth_query(
            &url::Url::parse("https://example.test/watch?v=1").unwrap()
        ));
        assert!(
            !has_known_rotating_auth_query(
                &url::Url::parse("https://example.test/master.m3u8?t=opaque&s=signature&e=86400")
                    .unwrap()
            ),
            "opaque t/s/e query fields are not assumed to be auth parameters"
        );
        assert!(
            !has_known_rotating_auth_query(
                &url::Url::parse("https://example.test/master.m3u8?f=418899&srv=server&i=0.4&sp=500&p1=one&p2=two&asn=327934")
                    .unwrap()
            ),
            "provider-specific parameters are not assumed to be auth parameters"
        );
    }

    #[test]
    fn cache_key_collapses_youtube_url_spellings() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let binary = fake_binary(directory.path());
        let canonical = "https://www.youtube.com/watch?v=dQw4w9WgXcQ";
        let base = cache_key(
            canonical,
            &binary,
            "2026.10.23",
            "none",
            None,
            &["--".to_string(), canonical.to_string()],
        )
        .expect("anonymous lookup should be cacheable");
        for url in [
            "https://youtu.be/dQw4w9WgXcQ?si=share-token",
            "https://www.youtube.com/shorts/dQw4w9WgXcQ",
            "https://m.youtube.com/watch?v=dQw4w9WgXcQ&t=30s",
            "https://www.youtube.com/watch?feature=share&v=dQw4w9WgXcQ",
        ] {
            assert_eq!(
                cache_key(
                    url,
                    &binary,
                    "2026.10.23",
                    "none",
                    None,
                    &["--".to_string(), url.to_string()],
                ),
                Some(base.clone()),
                "expected {url} to share the canonical key"
            );
        }
    }

    #[test]
    fn cache_key_keeps_playlist_context_on_short_links() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let binary = fake_binary(directory.path());
        let canonical = "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PL123&index=2";
        let with_playlist = cache_key(
            canonical,
            &binary,
            "2026.10.23",
            "none",
            None,
            &["--".to_string(), canonical.to_string()],
        )
        .expect("anonymous lookup should be cacheable");
        for url in [
            "https://youtu.be/dQw4w9WgXcQ?list=PL123&index=2&si=share-token",
            "https://www.youtube.com/watch?list=PL123&v=dQw4w9WgXcQ&index=2",
        ] {
            assert_eq!(
                cache_key(
                    url,
                    &binary,
                    "2026.10.23",
                    "none",
                    None,
                    &["--".to_string(), url.to_string()],
                ),
                Some(with_playlist.clone()),
                "expected {url} to keep its playlist context"
            );
        }
        // A playlist share link must not collide with the plain single video.
        let plain = "https://www.youtube.com/watch?v=dQw4w9WgXcQ";
        let without_playlist = cache_key(
            plain,
            &binary,
            "2026.10.23",
            "none",
            None,
            &["--".to_string(), plain.to_string()],
        )
        .expect("anonymous lookup should be cacheable");
        assert_ne!(without_playlist, with_playlist);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn video_cache_round_trips_a_grab_key() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config = yt_dlp::cache::CacheConfig::builder()
            .cache_dir(directory.path().to_path_buf())
            .build();
        let cache = yt_dlp::cache::VideoCache::new(&config, Some(60))
            .await
            .expect("create cache");
        let video: yt_dlp::model::Video = serde_json::from_value(serde_json::json!({
            "id": "x",
            "title": "T",
            "age_limit": 0,
            "live_status": "not_live",
            "playable_in_embed": true,
            "extractor": "generic",
            "extractor_key": "Generic",
            "_version": {"version": "2026.08.19", "repository": "yt-dlp"},
            "formats": [],
        }))
        .expect("test video must parse");
        let key = "grab-ytdlp-video-v1:round-trip";
        cache
            .put(key.to_string(), video.clone())
            .await
            .expect("store");
        let restored = cache.get(key).await.expect("read").expect("hit");
        assert_eq!(restored.id, video.id);
        assert_eq!(restored.title, video.title);
    }

    fn fetchable_video() -> yt_dlp::model::Video {
        serde_json::from_value(serde_json::json!({
            "id": "x",
            "title": "T",
            "age_limit": 0,
            "live_status": "not_live",
            "playable_in_embed": true,
            "extractor": "generic",
            "extractor_key": "Generic",
            "_version": {"version": "2026.08.19", "repository": "yt-dlp"},
            "formats": [{
                "format": "137 - 1920x1080",
                "format_id": "137",
                "protocol": "https",
                "ext": "mp4",
                "url": "https://cdn.example/v.mp4",
                "vcodec": "avc1.640028",
                "acodec": "none",
                "http_headers": {},
                "filesize": 100,
            }],
        }))
        .expect("test video must parse")
    }

    #[tokio::test(flavor = "current_thread")]
    async fn persistent_cache_keeps_extraction_keys_with_the_same_media_id_separate() {
        let directory = tempfile::tempdir().expect("temporary cache directory");
        let config = yt_dlp::cache::CacheConfig::builder()
            .cache_dir(directory.path().to_path_buf())
            .build();
        let first_key = "grab-ytdlp-video-v1:first-extraction";
        let second_key = "grab-ytdlp-video-v1:second-extraction";

        let mut first_video = fetchable_video();
        first_video.title = "First extraction".to_string();
        let mut second_video = first_video.clone();
        second_video.title = "Second extraction".to_string();

        let cache = yt_dlp::cache::VideoCache::new(&config, Some(60))
            .await
            .expect("create persistent cache");
        cache
            .put(
                first_key.to_string(),
                persistent_cache_record(first_key, &first_video),
            )
            .await
            .expect("store first extraction");
        cache
            .put(
                second_key.to_string(),
                persistent_cache_record(second_key, &second_video),
            )
            .await
            .expect("store second extraction");
        drop(cache);

        // Reopening discards the in-memory L1 cache and exercises Redb directly.
        let cache = yt_dlp::cache::VideoCache::new(&config, Some(60))
            .await
            .expect("reopen persistent cache");
        let first_cached = cache
            .get(first_key)
            .await
            .expect("read first extraction")
            .expect("first extraction remains cached");
        let second_cached = cache
            .get(second_key)
            .await
            .expect("read second extraction")
            .expect("second extraction remains cached");

        let first = restore_persistent_cache_record(first_key, first_cached)
            .expect("first record belongs to its extraction key");
        let second = restore_persistent_cache_record(second_key, second_cached)
            .expect("second record belongs to its extraction key");

        assert_eq!(first.id, "x");
        assert_eq!(second.id, "x");
        assert_eq!(first.title, "First extraction");
        assert_eq!(second.title, "Second extraction");
    }

    #[test]
    fn authenticated_preview_pruning_removes_expired_entries_and_keeps_fresh_ones() {
        let now = Instant::now();
        let expired_at = now
            .checked_sub(AUTHENTICATED_PREVIEW_TTL + Duration::from_secs(1))
            .expect("test instant is within the supported range");
        let mut previews = HashMap::from([
            ("expired".to_string(), (expired_at, fetchable_video())),
            ("fresh".to_string(), (now, fetchable_video())),
        ]);

        prune_authenticated_previews(&mut previews, now);

        assert!(!previews.contains_key("expired"));
        assert!(previews.contains_key("fresh"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn authenticated_video_keys_round_trip_through_the_public_path() {
        let key = format!("{AUTHENTICATED_VIDEO_NAMESPACE}:round-trip");
        let video = fetchable_video();
        assert!(
            lookup_video(&key).await.is_none(),
            "an unseen key must miss"
        );
        store_video(&key, &video).await;
        assert_eq!(
            lookup_video(&key).await.as_ref(),
            Some(&video),
            "the public path must route the auth namespace to memory"
        );
    }

    #[test]
    fn playlist_cache_key_allows_a_browser_profile_and_a_staged_handoff_jar() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let binary = fake_binary(directory.path());
        let url = "https://example.test/playlist?list=1";
        let profile_args = vec![
            "--cookies-from-browser=firefox".to_string(),
            "--".to_string(),
            url.to_string(),
        ];
        assert_eq!(
            cache_key(url, &binary, "2026.10.23", "firefox", None, &profile_args),
            None,
            "profile-scoped video metadata must stay out of the cache"
        );
        assert!(
            playlist_cache_key(url, &binary, "2026.10.23", "firefox", None, &profile_args)
                .is_some(),
            "a playlist listing has no signed URLs and may be cached per profile"
        );
        let handoff_jar = directory.path().join("handoff.jar");
        std::fs::write(&handoff_jar, "# Netscape HTTP Cookie File\n").expect("write cookie jar");
        let handoff_args = vec![
            "--cookies".to_string(),
            handoff_jar.to_string_lossy().into_owned(),
            "--".to_string(),
            url.to_string(),
        ];
        assert!(
            cache_key(url, &binary, "2026.10.23", "none", None, &handoff_args).is_some(),
            "video metadata is scoped to a staged handoff jar"
        );
        let handoff_key =
            playlist_cache_key(url, &binary, "2026.10.23", "none", None, &handoff_args)
                .expect("a playlist may be cached for the lifetime of its handoff jar");
        assert_eq!(
            Some(handoff_key.clone()),
            playlist_cache_key(url, &binary, "2026.10.23", "firefox", None, &handoff_args),
            "a live jar drops the configured profile out of the key"
        );
        let anonymous_args = vec!["--".to_string(), url.to_string()];
        assert_ne!(
            Some(handoff_key.clone()),
            playlist_cache_key(url, &binary, "2026.10.23", "none", None, &anonymous_args),
            "an anonymous lookup must not read a jar-scoped entry"
        );
        let other_jar = directory.path().join("other-handoff.jar");
        std::fs::write(&other_jar, "# Netscape HTTP Cookie File\nchanged-cookie\n")
            .expect("write changed cookie jar");
        let other_jar_args = vec![
            "--cookies".to_string(),
            other_jar.to_string_lossy().into_owned(),
            "--".to_string(),
            url.to_string(),
        ];
        assert_ne!(
            Some(handoff_key),
            playlist_cache_key(url, &binary, "2026.10.23", "none", None, &other_jar_args),
            "each staged jar must get its own playlist entry"
        );
    }

    #[test]
    fn cache_key_ignores_the_resolved_deno_path() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let binary = fake_binary(directory.path());
        let url = "https://www.youtube.com/watch?v=1";
        let bare = vec![
            "--no-js-runtimes".to_string(),
            "--js-runtimes".to_string(),
            "deno".to_string(),
            "--".to_string(),
            url.to_string(),
        ];
        let pinned = vec![
            "--no-js-runtimes".to_string(),
            "--js-runtimes".to_string(),
            "/opt/deno/bin/deno".to_string(),
            "--".to_string(),
            url.to_string(),
        ];
        assert_eq!(
            cache_key(url, &binary, "2026.10.23", "none", None, &bare),
            cache_key(url, &binary, "2026.10.23", "none", None, &pinned),
            "the Deno path is not part of the extraction contract"
        );
    }
}
