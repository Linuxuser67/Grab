//! Browser cookies for the direct HTTP engine, extracted via yt-dlp into an in-memory jar.
//! Temp file is owner-only and deleted after parse; values never logged, failures mean no cookies.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use reqwest::cookie::CookieStore as _;

/// Jar reuse TTL: outlives hourly site expiries, yet browser logout takes effect without restart.
const JAR_TTL: Duration = Duration::from_secs(5 * 60);

struct CachedJar {
    jar: Arc<reqwest::cookie::Jar>,
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
    let expires_attr = match expiry.parse::<u64>() {
        Ok(0) | Err(_) => String::new(),
        Ok(ts) => {
            let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(ts);
            format!("; Expires={}", httpdate::fmt_http_date(t))
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

/// Secure directory for cookie dumps: `$XDG_RUNTIME_DIR/grab-cookies` (0700,
/// owned by us) instead of the shared `/tmp/grab-video`. Falls back to the
/// legacy path if `XDG_RUNTIME_DIR` is unset. Verifies uid ownership so a
/// pre-created directory by another user is rejected.
fn cookie_staging_dir() -> Option<PathBuf> {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| crate::video::staging_root());
    let dir = base.join("grab-cookies");
    // Create 0700 if missing; atomic create_dir avoids symlink races.
    match std::fs::create_dir(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => {
            tracing::debug!(error = %e, "cookie staging dir create failed");
            return None;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        // Must be owned by us and not group/world-accessible.
        let Ok(md) = std::fs::symlink_metadata(&dir) else {
            return None;
        };
        if md.file_type().is_symlink() {
            tracing::warn!("cookie staging dir is a symlink; refusing");
            return None;
        }
        let uid = unsafe { libc::getuid() };
        if md.uid() != uid {
            tracing::warn!("cookie staging dir owned by another user; refusing");
            return None;
        }
        // Tighten to 0700 if it's looser (XDG_RUNTIME_DIR is 0700, but be explicit).
        let mut perms = md.permissions();
        if perms.mode() & 0o077 != 0 {
            perms.set_mode(0o700);
            let _ = std::fs::set_permissions(&dir, perms);
        }
    }
    Some(dir)
}

/// Sweep stale cookie dumps left by crashes: runs at startup before any
/// worker starts, so nothing live is removed.
pub(crate) fn sweep_cookie_staging() {
    let Some(dir) = cookie_staging_dir() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name();
        let name = name.to_str().unwrap_or("");
        // Only our cookie dumps; never touch anything else.
        if name.starts_with("grab-cookies-") && name.ends_with(".txt") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Dump one profile's cookies through yt-dlp to a temp file; caller deletes it.
async fn export_cookies(
    youtube_bin: &Path,
    cookies_browser: &str,
    page_url: &str,
    timeout: Duration,
) -> Option<String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let Some(dir) = cookie_staging_dir() else {
        return None;
    };
    // Unique per attempt, created atomically owner-only: a planted symlink fails instead of diverting the dump.
    let path: PathBuf = loop {
        let candidate: PathBuf = dir.join(format!(
            "grab-cookies-{}-{}.txt",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        match opts.open(&candidate) {
            Ok(_) => break candidate,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                tracing::debug!(error = %e, "cookie export temp file failed");
                return None;
            }
        }
    };
    let mut cmd = tokio::process::Command::new(youtube_bin);
    cmd.arg("--ignore-config")
        .arg("--no-progress")
        .arg("--cookies")
        .arg(&path)
        .arg("--skip-download");
    // Same identity argv as every other yt-dlp spawn (player-client
    // workaround, cookies, `--` URL guard): one helper so flags can't drift.
    cmd.args(crate::video_tools::ytdlp_identity_args(
        cookies_browser,
        None,
        page_url,
    ));
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().ok()?;
    let mut group = crate::video_spawn::ProcessGroupGuard::new(&child);
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        // Clean exit: leader reaped, so disarm before post-wait work.
        Ok(Ok(status)) => {
            group.disarm();
            status
        }
        _ => {
            // Timeout/wait failure: SIGKILL the group off the cookie DB lock, reap, remove temp file, fall back.
            crate::video_spawn::reap_child(&mut child, &mut group).await;
            let _ = std::fs::remove_file(&path);
            return None;
        }
    };
    if !status.success() {
        tracing::debug!("cookie export failed, continuing without cookies");
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let text = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);
    text
}

/// Cookie jar for one browser spec, cached briefly; `None` means plain requests.
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
        return Some(cached.jar.clone());
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
            jar: jar.clone(),
            at: Instant::now(),
        },
    );
    // Empty jar is valid (logged-out profile): send no Cookie header.
    Some(jar)
}

/// `Cookie` value for one URL, if the jar holds anything in scope.
pub(crate) fn cookie_header_for(
    jar: &reqwest::cookie::Jar,
    url: &str,
) -> Option<reqwest::header::HeaderValue> {
    let parsed: url::Url = url.parse().ok()?;
    jar.cookies(&parsed)
}

#[cfg(test)]
mod tests {
    use super::parse_netscape_line;

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
}
