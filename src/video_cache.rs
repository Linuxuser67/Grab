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
pub(crate) async fn lookup_video(key: &str) -> Option<Video> {
    let cache = video_cache().await?;
    clean_expired_if_due(cache).await;

    match cache.get(key).await {
        Ok(Some(video)) if is_cacheable_video(&video) && video.are_format_urls_fresh() => {
            tracing::debug!("yt-dlp metadata cache hit");
            Some(video)
        }
        Ok(Some(_)) => {
            if let Err(error) = cache.remove(key).await {
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
///
/// shortcut: the crate's Redb layer keys rows by `video.id`, so two identities
/// for the same video overwrite one row and a later L2 lookup can return the
/// other identity's metadata. Only anonymous extractions are cached, so the
/// cross-identity data differs at most by proxy/impersonation. Upgrade to a
/// per-identity store if authenticated or geo-scoped metadata is ever cached.
pub(crate) async fn store_video(key: &str, video: &Video) {
    if !is_cacheable_video(video) {
        return;
    }
    let Some(cache) = video_cache().await else {
        return;
    };

    if let Err(error) = cache.put(key.to_string(), video.clone()).await {
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
    let mut url = url::Url::parse(raw_url).ok()?;
    url.set_fragment(None);
    if has_rotating_auth_query(&url) {
        return None;
    }
    canonicalize_youtube_url(&mut url);

    let browser_profile_is_active =
        crate::video_tools::cookies_browser_spec(cookies_browser).is_some();
    let has_cookie_file = identity_args
        .iter()
        .any(|argument| argument == "--cookies" || argument.starts_with("--cookies="));
    // Authenticated extraction can return session-bound metadata and media
    // URLs. Do not persist it: extension handoff cookie files are temporary,
    // and browser-profile cookies can change independently of this process.
    if browser_profile_is_active || has_cookie_file {
        return None;
    }

    let mut hasher = Sha256::new();
    hash_field(&mut hasher, b"schema", b"grab-ytdlp-video-v1");
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
    for argument in identity_args {
        let argument = argument.as_str();
        if argument == "--" {
            break;
        }
        if skip_runtime_value {
            skip_runtime_value = false;
            continue;
        }
        if argument == "--js-runtimes" {
            skip_runtime_value = true;
            continue;
        }
        hash_field(&mut hasher, b"identity-arg", argument.as_bytes());
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
    Some(format!("grab-ytdlp-video-v1:{digest_hex}"))
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

/// URLs carrying rotating authentication/signature parameters are not reusable.
pub(crate) fn has_rotating_auth_query(url: &url::Url) -> bool {
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
    use super::{cache_key, has_rotating_auth_query, validate_cache_file_path};
    use crate::net_types::ResolvedProxy;
    use std::path::Path;

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
    fn authenticated_extractions_are_not_persistently_cached() {
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
        assert!(cache_key(url, &binary, "2026.10.23", "none", None, &with_handoff).is_none());

        let equals_form = vec![
            format!("--cookies={}", cookie_file.display()),
            "--".to_string(),
            url.to_string(),
        ];
        assert!(cache_key(url, &binary, "2026.10.23", "none", None, &equals_form).is_none());
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
