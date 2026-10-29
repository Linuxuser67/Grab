//! Browser cookies for the direct HTTP engine, extracted via yt-dlp into an in-memory jar.
//! Temp file is owner-only and deleted after parse; values never logged, failures mean no cookies.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[cfg(test)]
use reqwest::cookie::CookieStore as _;

/// Jar reuse TTL: outlives hourly site expiries, yet browser logout takes effect without restart.
const JAR_TTL: Duration = Duration::from_secs(5 * 60);

struct CachedJar {
    /// Netscape export text; a fresh writable Jar is built from it per
    /// attempt, so Set-Cookie during one download never carries into the next.
    text: String,
    at: Instant,
}

fn jar_cache() -> &'static Mutex<std::collections::HashMap<String, CachedJar>> {
    static CACHE: OnceLock<Mutex<std::collections::HashMap<String, CachedJar>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// One parsed Netscape line; `None` for comments, blanks and malformed lines.
/// Returns (domain, Set-Cookie value) preserving the browser's scope:
/// host-only cookies (subdomain flag FALSE) omit `Domain`, path and expiry
/// are kept so cookies don't leak to sibling subdomains or paths.
fn parse_netscape_line(line: &str) -> Option<(String, String)> {
    let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut fields = line.split('\t');
    let domain = fields.next()?.trim();
    let subdomain = fields.next()?.trim();
    let path = fields.next()?.trim();
    let secure = fields.next()?.trim();
    let expiry = fields.next()?.trim();
    let name = fields.next()?.trim();
    let value = fields.next()?.trim();
    if domain.is_empty() || name.is_empty() {
        return None;
    }
    // Set-Cookie shape; skip `;` values rather than truncating, keep Secure https-only.
    if value.contains(';') {
        return None;
    }
    // Subdomain flag TRUE → Domain cookie (siblings included); FALSE →
    // host-only (omit Domain so it doesn't leak to subdomains).
    let domain_attr = if subdomain.eq_ignore_ascii_case("TRUE") {
        format!("; Domain={domain}")
    } else {
        String::new()
    };
    let path_attr = if path.is_empty() {
        String::new()
    } else {
        format!("; Path={path}")
    };
    let secure_attr = if secure.eq_ignore_ascii_case("TRUE") {
        "; Secure"
    } else {
        ""
    };
    // Expiry 0 = session cookie; otherwise format as an HTTP date.
    // Clamp to year 9999: httpdate panics at/past it, and huge u64s overflow UNIX_EPOCH.
    const MAX_TS: u64 = 253_402_300_799; // 9999-12-31 23:59:59 UTC
    let expires_attr = match expiry.parse::<u64>() {
        Ok(0) | Err(_) => String::new(),
        Ok(ts) => {
            let ts = ts.min(MAX_TS);
            match std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(ts)) {
                Some(t) => format!("; Expires={}", httpdate::fmt_http_date(t)),
                None => String::new(),
            }
        }
    };
    Some((
        domain.to_string(),
        format!("{name}={value}{domain_attr}{path_attr}{secure_attr}{expires_attr}"),
    ))
}

/// URL owning a cookie domain: leading dots are registry noise.
fn url_for_domain(domain: &str) -> Option<url::Url> {
    url::Url::parse(&format!("https://{}/", domain.trim_start_matches('.'))).ok()
}

/// Adopt exported cookies; one bad line can't poison the profile.
pub(crate) fn jar_from_export(text: &str) -> (Arc<reqwest::cookie::Jar>, usize) {
    let jar = Arc::new(reqwest::cookie::Jar::default());
    let mut count = 0;
    for line in text.lines() {
        let Some((domain, cookie)) = parse_netscape_line(line) else {
            continue;
        };
        let Some(url) = url_for_domain(&domain) else {
            continue;
        };
        jar.add_cookie_str(&cookie, &url);
        count += 1;
    }
    (jar, count)
}

/// Sweep stale cookie dumps left by crashes: runs at startup before any
/// worker starts, so nothing live is removed.
pub(crate) fn sweep_cookie_staging() {
    // Only delete files owned by our uid: /tmp is shared, and another
    // user's grab-cookies-*.txt is not ours to remove.
    #[cfg(unix)]
    let my_uid = crate::download_net::current_uid();
    let owned_by_me = |path: &std::path::Path| -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            // symlink_metadata: do not follow symlinks. A symlink-to-own-file
            // would pass a metadata() uid check vacuously, then we'd unlink
            // the link (not the target, but the check is meaningless).
            // Refuse symlinks outright.
            std::fs::symlink_metadata(path)
                .map(|m| !m.is_symlink() && m.uid() == my_uid)
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            true
        }
    };
    // Clean the current dir (runtime dir, or cache fallback).
    if let Some(dir) = crate::download_net::cookie_staging_dir()
        && let Ok(entries) = std::fs::read_dir(&dir)
    {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name();
            let name = name.to_str().unwrap_or("");
            // Only our cookie dumps and extension-handoff cookie files (the
            // latter are staged here too); never touch anything else.
            if ((name.starts_with("grab-cookies-") && name.ends_with(".txt"))
                || name.starts_with("grab-handoff-cookies-"))
                && owned_by_me(&entry.path())
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    // One-release migration cleanup for plaintext cookie dumps left by old
    // builds. Unlike the current private staging directory, /tmp is shared:
    // never scan the legacy path unless it is a real directory owned by us.
    let legacy_dir = crate::video::staging_root();
    sweep_legacy_cookie_dumps(&legacy_dir, &owned_by_me);
}

/// Remove stale legacy cookie dumps from a validated directory. Kept separate
/// so the symlink-root refusal is directly regression-tested.
fn sweep_legacy_cookie_dumps(
    dir: &std::path::Path,
    owned_by_me: &impl Fn(&std::path::Path) -> bool,
) {
    if !crate::download_net::validate_secure_dir(dir) {
        tracing::debug!(path = %dir.display(), "refusing unsafe legacy cookie staging directory");
        return;
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name();
            let name = name.to_str().unwrap_or("");
            if name.starts_with("grab-cookies-")
                && name.ends_with(".txt")
                && owned_by_me(&entry.path())
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// RAII guard for a staged cookie file. Dropping it deletes the file
/// (ignoring errors). Keeps the guard alive in the spawned future so
/// pause/cancel drops it and never leaks the file.
pub(crate) struct StagedCookies(PathBuf);

impl StagedCookies {
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    /// Atomically create an owner-only staging file and immediately return
    /// its RAII owner. Production and cancellation tests share this path so
    /// no await can occur between successful creation and ownership.
    fn create_in(dir: &Path) -> std::io::Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        loop {
            let candidate = dir.join(format!(
                "grab-cookies-{}-{}.txt",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            match options.open(&candidate) {
                Ok(file) => {
                    drop(file);
                    return Ok(Self(candidate));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

impl Drop for StagedCookies {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub(crate) async fn export_cookies_to_file(
    youtube_bin: &Path,
    cookies_browser: &str,
    page_url: &str,
    timeout: Duration,
) -> Option<StagedCookies> {
    stage_cookie_file(youtube_bin, cookies_browser, page_url, timeout).await
}

/// Dump one profile's cookies through yt-dlp to a temp file. Its RAII owner
/// unlinks it on every return path, including cancellation during the wait.
async fn export_cookies(
    youtube_bin: &Path,
    cookies_browser: &str,
    page_url: &str,
    timeout: Duration,
) -> Option<String> {
    let staged = stage_cookie_file(youtube_bin, cookies_browser, page_url, timeout).await?;
    std::fs::read_to_string(staged.path()).ok()
}

/// Stage cookies to a secure temp file. The RAII owner is created immediately
/// after atomic creation, before any later await can cancel this future.
async fn stage_cookie_file(
    youtube_bin: &Path,
    cookies_browser: &str,
    page_url: &str,
    timeout: Duration,
) -> Option<StagedCookies> {
    // Runtime readiness is managed by the caller so its deadline can cover
    // Deno provisioning plus this export subprocess.
    let dir = crate::download_net::cookie_staging_dir()?;
    // The helper returns the owner in the same synchronous call that creates
    // the file, so cancellation cannot land in an unguarded gap.
    let staged = match StagedCookies::create_in(&dir) {
        Ok(staged) => staged,
        Err(error) => {
            tracing::warn!(error = %error, "cookie export temp file failed");
            return None;
        }
    };
    let mut cmd = tokio::process::Command::new(youtube_bin);
    cmd.arg("--ignore-config")
        .arg("--no-progress")
        .arg("--cookies")
        .arg(staged.path())
        .arg("--skip-download");
    // Same identity argv as every other yt-dlp spawn (player-client
    // workaround, cookies, `--` URL guard): one helper so flags can't drift.
    // Both cookie sources are intentional here: `--cookies` is this export's
    // output file and `--cookies-from-browser` is what fills it, so this spawn
    // must not use the handoff-aware ytdlp_spawn_identity.
    let identity_args =
        crate::video_tools::ytdlp_identity_args(cookies_browser, None, page_url);
    let guarded_proxy = match crate::video_spawn::guard_untrusted_args(&identity_args) {
        Ok(guarded) => guarded,
        Err(e) => {
            tracing::warn!(error = %e, "refusing cookie export without guarded extractor networking");
            return None;
        }
    };
    cmd.args(&identity_args);
    crate::video_spawn::apply_guarded_proxy_env(&mut cmd, guarded_proxy.as_deref());
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            tracing::warn!(error = %error, "cookie export process could not start");
            return None;
        }
    };
    let mut group = crate::video_spawn::ProcessGroupGuard::new(&child);
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        // Clean exit: leader reaped, so disarm before post-wait work.
        Ok(Ok(status)) => {
            group.disarm();
            status
        }
        _ => {
            // Timeout/wait failure kills/reaps the child group; staged drops on return.
            crate::video_spawn::reap_child(&mut child, &mut group).await;
            return None;
        }
    };
    if !status.success() {
        tracing::warn!("cookie export failed; continuing without browser cookies");
        return None;
    }
    Some(staged)
}

/// Cookie jar for one browser spec, cached briefly; `None` means plain requests.
/// The jar is built fresh from the cached export text on each call, so it is
/// writable per attempt: Set-Cookie during a download stays in that attempt's
/// jar and never carries into the next download.
pub(crate) async fn jar_for_browser(
    cookies_browser: &str,
    youtube_bin: &Path,
    page_url: &str,
) -> Option<Arc<reqwest::cookie::Jar>> {
    if cookies_browser.is_empty() || cookies_browser == "none" {
        return None;
    }
    if let Some(cached) = crate::runtime::lock_recover(jar_cache())
        .get(cookies_browser)
        .filter(|c| c.at.elapsed() < JAR_TTL)
    {
        let (jar, _) = jar_from_export(&cached.text);
        return Some(jar);
    }
    // Validated here (not inside the exporter) so an unknown browser still
    // fails closed instead of exporting with no --cookies-from-browser.
    crate::video_tools::cookies_browser_spec(cookies_browser)?;
    let text = export_cookies(
        youtube_bin,
        cookies_browser,
        page_url,
        Duration::from_secs(120),
    )
    .await?;
    let (jar, count) = jar_from_export(&text);
    tracing::debug!(cookies = count, "exported browser cookies");
    crate::runtime::lock_recover(jar_cache()).insert(
        cookies_browser.to_string(),
        CachedJar {
            text,
            at: Instant::now(),
        },
    );
    // Empty jar is valid (logged-out profile): send no Cookie header.
    Some(jar)
}

/// `Cookie` value for one URL, if the jar holds anything in scope.
/// Test-only: production sends cookies through the client's cookie provider.
#[cfg(test)]
pub(crate) fn cookie_header_for(
    jar: &reqwest::cookie::Jar,
    url: &str,
) -> Option<reqwest::header::HeaderValue> {
    let parsed: url::Url = url.parse().ok()?;
    jar.cookies(&parsed)
}

#[cfg(test)]
mod tests {
    use super::{StagedCookies, parse_netscape_line, sweep_legacy_cookie_dumps};

    #[cfg(unix)]
    #[test]
    fn legacy_cookie_sweep_refuses_symlinked_root() {
        use std::os::unix::fs::symlink;

        let unique = format!(
            "grab-cookie-sweep-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let base = std::env::temp_dir().join(unique);
        let target = base.join("target");
        let link = base.join("legacy-root");
        std::fs::create_dir_all(&target).unwrap();
        let stale_cookie = target.join("grab-cookies-old.txt");
        std::fs::write(&stale_cookie, "secret cookie data").unwrap();
        symlink(&target, &link).unwrap();

        sweep_legacy_cookie_dumps(&link, &|_| true);

        assert!(
            stale_cookie.exists(),
            "a symlink target must not be scanned or modified"
        );
        std::fs::remove_file(&link).unwrap();
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn staged_cookies_drop_deletes_file() {
        // Create a temp file, wrap it, drop the guard: file must be gone.
        // Mutation: Drop does nothing → file still exists → assertion fails.
        let path = std::env::temp_dir().join(format!(
            "grab-test-staged-{}.txt",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, "test").unwrap();
        assert!(path.exists());
        {
            let guard = StagedCookies(path.clone());
            assert_eq!(guard.path(), path.as_path());
            // guard drops here
        }
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn cancelling_cookie_export_drops_staged_plaintext_file() {
        let dir = std::env::temp_dir().join(format!(
            "grab-cancelled-cookie-export-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let staged = StagedCookies::create_in(&dir).unwrap();
        let path = staged.path().to_path_buf();
        assert!(path.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let task = tokio::spawn(async move {
            let _staged = staged;
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        task.abort();
        let _ = task.await;
        assert!(
            !path.exists(),
            "cancelling an export must unlink its staged file"
        );
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn netscape_host_only_omits_domain() {
        // subdomain FALSE → host-only: no Domain attribute, so the cookie
        // must not leak to sibling subdomains.
        // Mutation: always emit Domain → this assertion fails.
        let line = "example.com\tFALSE\t/\tTRUE\t1893456000\tsid\tabc123";
        let (domain, cookie) = parse_netscape_line(line).unwrap();
        assert_eq!(domain, "example.com");
        assert!(
            !cookie.contains("Domain="),
            "host-only must omit Domain: {cookie}"
        );
        assert!(cookie.contains("Path=/"), "path kept: {cookie}");
        assert!(cookie.contains("Secure"), "secure kept: {cookie}");
        assert!(cookie.contains("Expires="), "expiry kept: {cookie}");
    }

    #[test]
    fn netscape_domain_keeps_domain() {
        // subdomain TRUE → Domain cookie: Domain attribute present.
        let line = ".example.com\tTRUE\t/\tFALSE\t1893456000\tsid\tabc123";
        let (_, cookie) = parse_netscape_line(line).unwrap();
        assert!(
            cookie.contains("Domain=.example.com"),
            "domain kept: {cookie}"
        );
        assert!(
            !cookie.contains("Secure"),
            "non-secure omits Secure: {cookie}"
        );
    }

    #[test]
    fn netscape_session_cookie_omits_expires() {
        // Expiry 0 = session cookie: no Expires attribute.
        let line = "example.com\tFALSE\t/\tFALSE\t0\tsid\tabc123";
        let (_, cookie) = parse_netscape_line(line).unwrap();
        assert!(
            !cookie.contains("Expires="),
            "session omits Expires: {cookie}"
        );
    }

    #[test]
    fn netscape_path_scoped() {
        // Non-root path is preserved.
        let line = "example.com\tFALSE\t/api\tFALSE\t0\ttok\txyz";
        let (_, cookie) = parse_netscape_line(line).unwrap();
        assert!(cookie.contains("Path=/api"), "path kept: {cookie}");
    }

    #[test]
    fn huge_expiry_does_not_panic() {
        // Expiry u64::MAX would overflow UNIX_EPOCH + Duration: the clamp to
        // 9999-12-31 must prevent the panic. Mutation: remove the .min(MAX_TS)
        // → this test panics.
        let line = "example.com\tFALSE\t/\tTRUE\t18446744073709551615\tsid\tabc123";
        let (domain, cookie) = parse_netscape_line(line).unwrap();
        assert_eq!(domain, "example.com");
        // Clamped to 9999-12-31, not panicked.
        assert!(
            cookie.contains("Expires="),
            "expected Expires attr: {cookie}"
        );
        assert!(
            cookie.contains("9999"),
            "expected clamped to 9999: {cookie}"
        );
    }
}
