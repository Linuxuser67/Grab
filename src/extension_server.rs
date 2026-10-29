//! Local HTTP server for browser extension integration.
//!
//! Replaces native messaging: the extension POSTs URLs to
//! `http://127.0.0.1:9412/add` instead of using a native messaging host.
//! No manifest installation, no extension IDs, works from Flatpak.
//!
//! Threat model: the socket is loopback-only, but any web page can still try
//! to reach it. CORS preflight stops a plain cross-origin POST, yet not DNS
//! rebinding (the rebound page is same-origin, so no preflight and an
//! attacker-chosen `Host`). Every request therefore passes [`guard`]: the
//! `Host` must be the loopback name this server is bound to, and an `Origin`
//! (always sent by browsers on POST) must belong to a browser extension.
//! Bodies and queue depth are bounded so a flood cannot exhaust memory or
//! bury the user in dialogs.
//!
//! Pairing: the guard cannot tell *which* extension is calling, nor can the
//! extension tell that the listener on 9412 is really Grab (any local process
//! may bind the port first). Grab therefore keeps a random 256-bit secret
//! (`extension-pairing` in its config dir) that the user copies from
//! Preferences into the extension. With it:
//! - `GET /hello?n=<nonce>` proves Grab knows the secret
//!   (`HMAC(secret, "grab-server\0" || nonce)`), so the extension can refuse to
//!   talk to an impostor before sending anything sensitive;
//! - `POST /add` carries `X-Grab-Token = HMAC(secret, "grab-client\0" || body)`.
//! - Current paired clients sign a millisecond timestamp and 128-bit nonce in
//!   that body. The timestamp has a two-minute clock-skew window and each nonce
//!   may be used only once, preventing a captured POST from being replayed.
//!
//! Cookies are accepted only with valid authentication and replay metadata.
//! Legacy paired clients that omit the metadata still hand off URLs, but are
//! forced through confirmation and remain URL-only until the extension updates.
//! Unpaired requests are also confirmation-only and never forward cookies; a
//! wrong token is refused outright.
//!
//! Confirmation: `POST /add?confirm=1` holds the response until the user has
//! answered Grab's dialog (`200` approved, `409` declined, `408` no answer
//! in [`CONFIRM_WAIT`]), so the extension cancels the browser's own download
//! only once Grab has really taken over.

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use hmac::{Hmac, KeyInit as _, Mac};
use hyper::Request as HyperRequest;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use serde::Deserialize;
use sha2::Sha256;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read as _;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::sync::mpsc::{Sender, error::TrySendError};
use tokio::sync::oneshot;
use tower::ServiceExt as _;

const PORT: u16 = 9412;
/// A URL plus JSON framing; the intake itself caps URLs at `download_intake::MAX_URL_LEN`.
const MAX_BODY_BYTES: usize = 32 * 1024;
/// Handoffs waiting for the GTK thread; beyond this the extension gets 429.
pub(crate) const QUEUE_DEPTH: usize = 16;
/// How long `/add?confirm=1` waits for the user to answer the dialog. The
/// extension's own timeout must be longer.
pub(crate) const CONFIRM_WAIT: Duration = Duration::from_secs(120);
/// Bound slow HTTP/1 request headers on the loopback extension API.
const EXTENSION_HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound the lifetime of one connection, including stalled request bodies.
/// Longer than CONFIRM_WAIT so a legitimate user confirmation can complete.
const EXTENSION_CONNECTION_TIMEOUT: Duration = Duration::from_secs(180);
/// Upper bound on confirmation requests in flight. The handoff queue depth
/// alone does not bound dialogs: the GTK thread drains it into one dialog per
/// request. Kept well below the queue so a flood cannot bury the user.
const MAX_PENDING_CONFIRMATIONS: usize = 8;

// --- Pairing ---------------------------------------------------------------

const SECRET_LEN: usize = 32;
const SERVER_CONTEXT: &[u8] = b"grab-server\0";
const CLIENT_CONTEXT: &[u8] = b"grab-client\0";
const HANDOFF_CLOCK_SKEW_MS: u64 = 120_000;
const MAX_RECENT_HANDOFF_NONCES: usize = 4096;
type HmacSha256 = Hmac<Sha256>;

#[derive(Default)]
struct HandoffReplayCache {
    /// Nonces remain reserved until their signed request timestamp cannot pass
    /// the freshness check. Never evict an unexpired nonce to make room.
    expires_at_ms: HashMap<String, u64>,
}

static HANDOFF_REPLAY_CACHE: LazyLock<Mutex<HandoffReplayCache>> =
    LazyLock::new(|| Mutex::new(HandoffReplayCache::default()));

fn unix_time_ms() -> Option<u64> {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| duration.as_millis().try_into().ok())
}

/// Atomically reserve a single-use nonce for an authenticated body.
fn reserve_handoff_nonce(timestamp_ms: Option<u64>, nonce: Option<&str>) -> Result<(), StatusCode> {
    let (Some(timestamp_ms), Some(nonce)) = (timestamp_ms, nonce) else {
        return Err(StatusCode::FORBIDDEN);
    };
    if nonce.len() != 32 || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(StatusCode::FORBIDDEN);
    }
    let now_ms = unix_time_ms().ok_or(StatusCode::FORBIDDEN)?;
    if now_ms.abs_diff(timestamp_ms) > HANDOFF_CLOCK_SKEW_MS {
        return Err(StatusCode::FORBIDDEN);
    }
    let nonce = nonce.to_ascii_lowercase();
    let mut cache = match HANDOFF_REPLAY_CACHE.lock() {
        Ok(cache) => cache,
        Err(poisoned) => {
            tracing::error!("handoff replay cache mutex poisoned; recovering fail-closed state");
            let cache = poisoned.into_inner();
            // The map has no cross-index invariant: after retaining only
            // timestamp-expiring entries it is safe to resume normal locking.
            HANDOFF_REPLAY_CACHE.clear_poison();
            cache
        }
    };
    cache.expires_at_ms.retain(|_, expiry| *expiry >= now_ms);
    if cache.expires_at_ms.contains_key(&nonce) {
        return Err(StatusCode::FORBIDDEN);
    }
    if cache.expires_at_ms.len() >= MAX_RECENT_HANDOFF_NONCES {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    cache
        .expires_at_ms
        .insert(nonce, timestamp_ms.saturating_add(HANDOFF_CLOCK_SKEW_MS));
    Ok(())
}

/// The shared secret between Grab and a paired extension.
pub(crate) struct Pairing {
    secret: RwLock<[u8; SECRET_LEN]>,
}

impl Pairing {
    fn from_secret(secret: [u8; SECRET_LEN]) -> Self {
        Self {
            secret: RwLock::new(secret),
        }
    }

    /// Load the private secret, creating it when missing. If the existing file
    /// is malformed, unreadable, or has unsafe permissions, replace it atomically
    /// but report that existing extensions must be paired again.
    pub(crate) fn load_or_create(path: &Path) -> std::io::Result<Self> {
        ensure_pairing_directory(path)?;
        match read_pairing_secret(path) {
            Ok(Some(secret)) => return Ok(Self::from_secret(secret)),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    path = ?path,
                    error = %error,
                    "pairing secret could not be trusted; generating a new code, so the browser extension must be paired again"
                );
                PAIRING_SECRET_RECOVERED.store(true, Ordering::Release);
            }
        }
        let secret = random_secret()?;
        write_secret(path, &secret)?;
        Ok(Self::from_secret(secret))
    }

    fn current(&self) -> [u8; SECRET_LEN] {
        *self.secret.read().unwrap_or_else(|e| e.into_inner())
    }

    /// The pairing code shown in Preferences (lowercase hex).
    pub(crate) fn code(&self) -> String {
        hex_encode(&self.current())
    }

    /// Replace the secret: every extension must be paired again.
    pub(crate) fn regenerate(&self, path: &Path) -> std::io::Result<String> {
        ensure_pairing_directory(path)?;
        let secret = random_secret()?;
        write_secret(path, &secret)?;
        *self.secret.write().unwrap_or_else(|e| e.into_inner()) = secret;
        PAIRING_SECRET_RECOVERED.store(false, Ordering::Release);
        Ok(self.code())
    }

    fn mac(&self, context: &[u8], message: &[u8]) -> HmacSha256 {
        let mut mac =
            HmacSha256::new_from_slice(&self.current()).expect("HMAC accepts keys of any length");
        mac.update(context);
        mac.update(message);
        mac
    }

    /// Proof for `GET /hello`: only a holder of the secret can produce it.
    fn proof(&self, nonce: &str) -> String {
        hex_encode(
            &self
                .mac(SERVER_CONTEXT, nonce.as_bytes())
                .finalize()
                .into_bytes(),
        )
    }

    /// Constant-time check of an `X-Grab-Token` over the raw request body.
    fn verify_token(&self, body: &[u8], token_hex: &str) -> bool {
        hex_decode(token_hex)
            .is_some_and(|tag| self.mac(CLIENT_CONTEXT, body).verify_slice(&tag).is_ok())
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    // Reject non-hex digits outright: `from_str_radix` accepts a leading `+`.
    if !text.len().is_multiple_of(2) || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

fn decode_secret(text: &str) -> Option<[u8; SECRET_LEN]> {
    hex_decode(text)?.try_into().ok()
}

static PAIRING_SECRET_RECOVERED: AtomicBool = AtomicBool::new(false);

/// Whether startup had to replace an unusable pairing file, so Preferences can
/// explain why the extension needs a new code.
pub(crate) fn pairing_secret_recovered() -> bool {
    PAIRING_SECRET_RECOVERED.load(Ordering::Acquire)
}

/// Ensure the secret's parent is a real, current-user-owned directory that is
/// not group/world writable before reading or replacing anything inside it.
fn ensure_pairing_directory(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    let parent = path.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
    match std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
    {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    if !crate::download_net::validate_secure_dir(parent) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "pairing secret directory must be a real directory owned by the current user and not group/world writable",
        ));
    }
    Ok(())
}

fn random_secret() -> std::io::Result<[u8; SECRET_LEN]> {
    let mut secret = [0u8; SECRET_LEN];
    getrandom::fill(&mut secret).map_err(|error| {
        std::io::Error::other(format!("system randomness unavailable: {error}"))
    })?;
    Ok(secret)
}

/// Read through an already-open file descriptor so validation and reading refer
/// to the same inode. Refuse symlinks and any file not private to this user.
fn read_pairing_secret(path: &Path) -> std::io::Result<Option<[u8; SECRET_LEN]>> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "pairing secret is not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let mode = metadata.permissions().mode();
        if metadata.uid() != crate::download_net::current_uid()
            || mode & 0o077 != 0
            || mode & 0o400 == 0
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "pairing secret must be owned by the current user and mode 0600 or stricter",
            ));
        }
    }
    // A pairing secret is only 64 hex bytes; bound reads even if another
    // local process replaces the file after the metadata check. O_NONBLOCK
    // above also avoids hanging if the path is raced to a FIFO.
    let mut text = String::new();
    file.take(129).read_to_string(&mut text)?;
    if text.len() > 128 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "pairing secret is unexpectedly large",
        ));
    }
    let secret = decode_secret(text.trim()).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "pairing secret is malformed",
        )
    })?;
    Ok(Some(secret))
}

/// Write the secret 0600 via a temp file and rename, so a crash never leaves a
/// truncated secret and no moment exists where the file is world-readable.
fn write_secret(path: &Path, secret: &[u8; SECRET_LEN]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
    let dir = path.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(hex_encode(secret).as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    // Sync the rename's directory entry as well as the file contents.
    std::fs::File::open(dir)?.sync_all()
}

/// Where the secret lives: `$XDG_CONFIG_HOME/grab/extension-pairing`.
pub(crate) fn pairing_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(s) if !s.is_empty() => PathBuf::from(s),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("grab").join("extension-pairing"))
}

static PAIRING: OnceLock<Arc<Pairing>> = OnceLock::new();

/// The running server's pairing state (for Preferences); `None` until the
/// server has started or when the secret could not be stored.
pub(crate) fn pairing() -> Option<Arc<Pairing>> {
    PAIRING.get().cloned()
}

// --- Handoff cookies -------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct Cookie {
    pub name: String,
    pub value: String,
    /// Backwards compatible: the extension originally sent only name/value.
    /// When present, these preserve the browser's cookie scope so host-only
    /// cookies aren't silently widened to subdomains.
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub host_only: Option<bool>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub secure: Option<bool>,
    #[serde(default)]
    pub expiration_date: Option<f64>,
}

/// Staged cookie files keyed by canonical URL, oldest first.
#[derive(Default)]
struct HandoffStore {
    by_key: HashMap<String, HandoffCookies>,
    /// Insertion order, so a full store evicts the oldest handoff instead of
    /// an arbitrary one (which could belong to a download still running).
    order: VecDeque<String>,
    /// Hosts whose older staged jars must not be reused. A cookieless
    /// handoff leaves no exact entry, so the same-host fallback would
    /// otherwise select a jar from an earlier handoff.
    suppressed_hosts: HashMap<String, SuppressedHost>,
    /// FIFO bound for `suppressed_hosts`; a local flood must not grow it.
    suppressed_order: VecDeque<String>,
}

/// Staged cookies plus the browser UA for one handoff. The UA travels with
/// the cookies because `cf_clearance` is UA-bound: same jar, wrong UA still
/// fails the challenge.
#[derive(Clone)]
struct HandoffCookies {
    path: PathBuf,
    user_agent: Option<String>,
    staged_at: SystemTime,
    /// Monotonic staging marker for tombstone ordering; wall-clock steps must
    /// not make an old jar look newer than a later cookieless handoff.
    staged_instant: Instant,
}

#[derive(Clone)]
struct SuppressedHost {
    /// Effective cutoff: newest approved suppression or in-flight request.
    at: Instant,
    /// Last approved cutoff remains in force while any older jar exists.
    approved_at: Option<Instant>,
    /// Unique cutoff for each pending handoff, so declines do not undo others.
    pending: Vec<(u64, Instant)>,
}

/// Cookie files handed off from the extension, keyed by canonical URL. Every
/// yt-dlp spawn for the URL peeks the entry (probe, subtitle probe and media
/// download each spawn separately, and every leg must pass the challenge);
/// the finished download discards it. Bounded so abandoned handoffs (a card
/// opened but never added, a direct file that never probes) can't pile up.
static HANDOFF_COOKIES: LazyLock<Mutex<HandoffStore>> =
    LazyLock::new(|| Mutex::new(HandoffStore::default()));

/// Recover a poisoned handoff store while restoring its bounded FIFO invariant.
fn lock_handoff_cookies() -> MutexGuard<'static, HandoffStore> {
    match HANDOFF_COOKIES.lock() {
        Ok(store) => store,
        Err(poisoned) => {
            tracing::error!("handoff cookie store mutex poisoned; repairing its FIFO index");
            let mut store = poisoned.into_inner();
            let keys: HashSet<String> = store.by_key.keys().cloned().collect();
            let mut seen = HashSet::new();
            store
                .order
                .retain(|key| keys.contains(key) && seen.insert(key.clone()));
            for key in keys {
                if seen.insert(key.clone()) {
                    store.order.push_back(key);
                }
            }
            let hosts: HashSet<String> = store.suppressed_hosts.keys().cloned().collect();
            let mut seen_hosts = HashSet::new();
            store
                .suppressed_order
                .retain(|host| hosts.contains(host) && seen_hosts.insert(host.clone()));
            for host in hosts {
                if seen_hosts.insert(host.clone()) {
                    store.suppressed_order.push_back(host);
                }
            }
            // The map/FIFO invariant is repaired while this guard is held.
            // Clear poison now, so future calls do not repeat O(n) repair/logging.
            HANDOFF_COOKIES.clear_poison();
            store
        }
    }
}
/// Cap on staged handoff entries; past it the oldest entry is evicted with
/// its file. Matches the handoff queue depth: at most that many handoffs can
/// be in flight ahead of the GTK thread.
const MAX_HANDOFF_ENTRIES: usize = QUEUE_DEPTH;
/// Keep stale-host tombstones bounded. Only hosts with a staged jar are
/// recorded, but bound the map for fail-safe behaviour under unexpected input.
const MAX_SUPPRESSED_HOSTS: usize = MAX_HANDOFF_ENTRIES * 4;
/// A staged jar at or past this age is a snapshot when a live browser source
/// is configured: yt-dlp merges `--cookies` after the browser jar, so its old
/// values would override the browser's rotated cookies. Probe and auto-add
/// downloads run immediately; this only affects cards left open for a while.
const HANDOFF_FRESH: Duration = Duration::from_secs(30 * 60);
static COOKIE_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);
static SUPPRESSION_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Canonical map key for a handoff URL: the `url::Url` serialization, so the
/// raw extension string and the intake-normalized form hit the same entry
/// (`https://example.com` and `https://example.com/` are one key).
fn handoff_key(url: &str) -> Option<String> {
    url::Url::parse(url).ok().map(|u| u.as_str().to_owned())
}

/// Cookie `domain` as a Netscape-file host: letters, digits and `.-_:[]` only.
/// The field is written unescaped between tabs, so anything else could add
/// columns or whole lines to the file.
fn valid_cookie_domain(domain: &str) -> bool {
    !domain.is_empty()
        && domain.len() <= 253
        && domain.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':' | b'[' | b']')
        })
}

/// A cookie may only be sent for the URL's own host or one of its parents,
/// never for a public suffix itself (such as `com` or `co.uk`). Use the
/// maintained Mozilla Public Suffix List rather than a short hand-written TLD
/// table, including private and wildcard rules.
fn domain_covers_host(domain: &str, host: &str) -> bool {
    let (domain, host) = (domain.to_ascii_lowercase(), host.to_ascii_lowercase());
    if psl::suffix_str(&domain).is_some_and(|suffix| suffix.eq_ignore_ascii_case(&domain)) {
        return false;
    }
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// Cookie `path` as a Netscape-file field: no control characters (tab and
/// newline included) and anchored at the root.
fn valid_cookie_path(path: &str) -> bool {
    path.starts_with('/') && !path.chars().any(char::is_control)
}

/// Write extension-provided cookies to a staging file for yt-dlp's `--cookies`.
/// Returns the path, or None if there are no usable cookies or the write fails.
/// Cookies whose domain or path is malformed, or whose domain does not cover
/// the URL's host, are dropped.
#[cfg(test)]
pub(crate) fn write_handoff_cookies(url: &str, cookies: &[Cookie]) -> Option<PathBuf> {
    write_handoff_cookies_with_count(url, cookies).map(|(path, _)| path)
}

/// Like `write_handoff_cookies`, but also reports how many cookies were
/// serialized after scope and format validation.
fn write_handoff_cookies_with_count(url: &str, cookies: &[Cookie]) -> Option<(PathBuf, usize)> {
    if cookies.is_empty() {
        return None;
    }
    // Netscape cookie file format.
    let mut content = String::from("# Netscape HTTP Cookie File\n");
    let fallback_domain = url::Url::parse(url).ok()?.host_str()?.to_string();
    let mut written = 0;
    for cookie in cookies {
        // Netscape fields are tab-delimited; strip all Unicode control chars,
        // not only ASCII tabs/newlines, before serializing.
        let n: String = cookie.name.chars().filter(|c| !c.is_control()).collect();
        let v: String = cookie.value.chars().filter(|c| !c.is_control()).collect();
        if n.is_empty() {
            continue;
        }
        // Preserve the browser's scope when the extension sends it; otherwise
        // fall back to the previous behavior (tailmatch on the URL host).
        // Netscape format requires the leading dot when tailmatch is TRUE
        // (Python's cookiejar rejects the whole file otherwise), and no
        // leading dot when FALSE.
        let tailmatch = match cookie.host_only {
            Some(true) => "FALSE",
            // Old extension sent no attributes: keep the previous tailmatch
            // default so those handoffs keep working.
            _ => "TRUE",
        };
        let raw_domain = cookie.domain.as_deref().unwrap_or(&fallback_domain);
        // Validate the undotted form (the validators expect no leading dot).
        let undotted = raw_domain.strip_prefix('.').unwrap_or(raw_domain);
        if !valid_cookie_domain(undotted) || !domain_covers_host(undotted, &fallback_domain) {
            continue;
        }
        // Netscape format requires the leading dot when tailmatch is TRUE
        // (Python's cookiejar rejects the whole file otherwise), and no
        // leading dot when FALSE. Add it only for the output line, after
        // validation.
        let domain = if tailmatch == "TRUE" {
            format!(".{undotted}")
        } else {
            undotted.to_string()
        };
        let domain = domain.as_str();
        let path = cookie.path.as_deref().unwrap_or("/");
        if !valid_cookie_path(path) {
            continue;
        }
        let secure = if cookie.secure.unwrap_or(false) {
            "TRUE"
        } else {
            "FALSE"
        };
        let expiry = cookie.expiration_date.map(|e| e as i64).unwrap_or(0);
        content.push_str(&format!(
            "{domain}\t{tailmatch}\t{path}\t{secure}\t{expiry}\t{n}\t{v}\n"
        ));
        written += 1;
    }
    if written == 0 {
        return None;
    }
    // The private staging dir (runtime dir, or cache fallback), the one the
    // startup sweep cleans; no usable dir means no cookies, never /tmp.
    let mut path = crate::download_net::cookie_staging_dir()?;
    let count = COOKIE_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    path.push(format!(
        "grab-handoff-cookies-{}-{}",
        std::process::id(),
        count
    ));
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut file = opts.open(&path).ok()?;
    if let Err(error) = file.write_all(content.as_bytes()) {
        drop(file);
        if let Err(cleanup_error) = std::fs::remove_file(&path) {
            tracing::debug!(
                path = %path.display(),
                error = %cleanup_error,
                "could not remove incomplete extension cookie file"
            );
        }
        tracing::debug!(
            path = %path.display(),
            %error,
            "could not write extension cookie file"
        );
        return None;
    }
    Some((path, written))
}

/// Store a cookie file for a URL. Called by the extension handoff once the
/// handoff is accepted. Replaces any existing entry (deleting its file);
/// past [`MAX_HANDOFF_ENTRIES`] the oldest entry is evicted with its file.
pub(crate) fn store_handoff_cookies(url: &str, path: PathBuf, user_agent: Option<String>) {
    let Some(key) = handoff_key(url) else {
        let _ = std::fs::remove_file(path);
        return;
    };
    let mut store = lock_handoff_cookies();
    // Sanitize the UA: it goes into a header and an argv; trim whitespace,
    // drop it if empty, cap length (browsers send < 512 chars).
    let user_agent = user_agent
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(512).collect());
    let entry = HandoffCookies {
        path,
        user_agent,
        staged_at: SystemTime::now(),
        staged_instant: Instant::now(),
    };
    if let Some(old) = store.by_key.insert(key.clone(), entry) {
        let _ = std::fs::remove_file(old.path);
        store.order.retain(|k| k != &key);
    }
    store.order.push_back(key);
    while store.by_key.len() > MAX_HANDOFF_ENTRIES {
        let Some(oldest) = store.order.pop_front() else {
            break;
        };
        if let Some(old) = store.by_key.remove(&oldest) {
            let _ = std::fs::remove_file(old.path);
        }
    }
    let hosts: Vec<String> = store.suppressed_hosts.keys().cloned().collect();
    for host in hosts {
        prune_suppression(&mut store, &host);
    }
}

/// `--cookies` argv for a URL's staged handoff cookies, if any. Peeks: the
/// entry stays staged until the download finishes (or is evicted), because
/// the probe, the subtitle probe and the media download each spawn yt-dlp
/// Staged entry for `url`, if any. Exact URL first, then any staged entry
/// for the same host: playlist items are queued under their own URLs, but
/// the cookies were staged for the collection page. The cookie file itself
/// enforces domain/path scoping, so the host fallback can't leak cookies
/// to a wrong scope.
fn handoff_entry(url: &str) -> Option<HandoffCookies> {
    let key = handoff_key(url)?;
    let store = lock_handoff_cookies();
    let host = url::Url::parse(url).ok()?.host_str()?.to_ascii_lowercase();
    let current = |entry: &HandoffCookies| {
        store
            .suppressed_hosts
            .get(&host)
            .is_none_or(|suppressed| entry.staged_instant >= suppressed.at)
    };
    store
        .by_key
        .get(&key)
        .filter(|entry| current(entry))
        .cloned()
        .or_else(|| {
            // Newest-first through insertion order: deterministic, and the
            // most recent handoff on the host is the best guess for the parent
            // collection (a HashMap walk would be arbitrary).
            store.order.iter().rev().find_map(|k| {
                let khost = url::Url::parse(k).ok()?.host_str()?.to_ascii_lowercase();
                if khost == host {
                    store.by_key.get(k).filter(|entry| current(entry)).cloned()
                } else {
                    None
                }
            })
        })
}

/// A suppression owned by one cookieless handoff. Each request has its own cutoff.
struct Suppression {
    host: String,
    id: u64,
    at: Instant,
    finished: bool,
    /// A queued handoff may still be processed by GTK after client disconnect.
    sent: bool,
}

impl Suppression {
    fn mark_sent(&mut self) {
        self.sent = true;
    }

    /// Release after a decline, timeout, queue refusal, or other failure.
    fn release(&mut self) {
        if !self.finished {
            finish_suppression(&self.host, self.id, self.at, false);
            self.finished = true;
        }
    }

    /// Keep older staged jars suppressed after an approved handoff.
    fn keep(mut self) {
        if !self.finished {
            finish_suppression(&self.host, self.id, self.at, true);
            self.finished = true;
        }
    }
}

impl Drop for Suppression {
    fn drop(&mut self) {
        if !self.finished {
            // Once sent to GTK, cancellation cannot prove the handoff was declined.
            // Fail closed until the older jar is removed or replaced.
            finish_suppression(&self.host, self.id, self.at, self.sent);
            self.finished = true;
        }
    }
}

/// Suppress older staged jars for a URL's host until the request is approved.
fn host_has_stale_handoff(store: &HandoffStore, host: &str, at: Instant) -> bool {
    store.by_key.iter().any(|(key, entry)| {
        entry.staged_instant < at
            && url::Url::parse(key)
                .ok()
                .and_then(|parsed| parsed.host_str().map(str::to_ascii_lowercase))
                .is_some_and(|key_host| key_host == host)
    })
}

/// Remove a tombstone only when no request depends on it and deleting it cannot
/// expose a jar older than the last approved cutoff.
fn prune_suppression(store: &mut HandoffStore, host: &str) {
    let Some(suppressed) = store.suppressed_hosts.get(host).cloned() else {
        return;
    };
    let removable = suppressed.pending.is_empty()
        && suppressed
            .approved_at
            .is_none_or(|at| !host_has_stale_handoff(store, host, at));
    if removable {
        store.suppressed_hosts.remove(host);
        store.suppressed_order.retain(|candidate| candidate != host);
    }
}

fn finish_suppression(host: &str, id: u64, at: Instant, approved: bool) {
    let mut store = lock_handoff_cookies();
    if let Some(suppressed) = store.suppressed_hosts.get_mut(host) {
        if let Some(index) = suppressed
            .pending
            .iter()
            .position(|(pending_id, _)| *pending_id == id)
        {
            suppressed.pending.remove(index);
        }
        if approved {
            suppressed.approved_at = Some(suppressed.approved_at.map_or(at, |old| old.max(at)));
        }
        if let Some(cutoff) = suppressed
            .pending
            .iter()
            .map(|(_, cutoff)| *cutoff)
            .chain(suppressed.approved_at)
            .max()
        {
            suppressed.at = cutoff;
        }
    }
    prune_suppression(&mut store, host);
}

fn suppress_handoff_host(url: &str) -> Option<Suppression> {
    let host = url::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_ascii_lowercase))?;
    let mut store = lock_handoff_cookies();
    // Without a staged jar there is nothing to suppress and no tombstone is needed.
    let has_staged = store.by_key.keys().any(|key| {
        url::Url::parse(key)
            .ok()
            .and_then(|parsed| parsed.host_str().map(str::to_ascii_lowercase))
            .is_some_and(|key_host| key_host == host)
    });
    if !has_staged {
        return None;
    }
    // Every current jar predates this cutoff. Decline later restores the prior
    // approved cutoff, while overlapping requests retain independent references.
    let id = SUPPRESSION_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let at = Instant::now();
    store
        .suppressed_hosts
        .entry(host.clone())
        .and_modify(|suppressed| {
            suppressed.at = suppressed.at.max(at);
            suppressed.pending.push((id, at));
        })
        .or_insert(SuppressedHost {
            at,
            approved_at: None,
            pending: vec![(id, at)],
        });
    store
        .suppressed_order
        .retain(|candidate| candidate != &host);
    store.suppressed_order.push_back(host.clone());
    while store.suppressed_hosts.len() > MAX_SUPPRESSED_HOSTS {
        let removable = store.suppressed_order.iter().position(|candidate| {
            store.suppressed_hosts.get(candidate).is_some_and(|entry| {
                entry.pending.is_empty()
                    && entry
                        .approved_at
                        .is_none_or(|cutoff| !host_has_stale_handoff(&store, candidate, cutoff))
            })
        });
        let Some(index) = removable else {
            break;
        };
        let Some(oldest) = store.suppressed_order.remove(index) else {
            break;
        };
        store.suppressed_hosts.remove(&oldest);
    }
    Some(Suppression {
        host,
        id,
        at,
        finished: false,
        sent: false,
    })
}

/// Browser UA staged with a handoff's cookies, if any. Same lookup as the
/// cookies (exact URL, then same-host fallback).
pub(crate) fn handoff_user_agent(url: &str, browser: &str) -> Option<String> {
    handoff_user_agent_at(url, browser, SystemTime::now())
}

fn handoff_user_agent_at(url: &str, browser: &str, now: SystemTime) -> Option<String> {
    live_handoff_entry_at(url, browser, now).and_then(|entry| entry.user_agent)
}

/// The staged jar and UA must be accepted as one identity: a download must not
/// keep a handoff UA when the jar is stale or unreadable and browser cookies are exported.
pub(crate) fn live_handoff_identity(url: &str, browser: &str) -> Option<(PathBuf, Option<String>)> {
    live_handoff_identity_at(url, browser, SystemTime::now())
}

fn live_handoff_identity_at(
    url: &str,
    browser: &str,
    now: SystemTime,
) -> Option<(PathBuf, Option<String>)> {
    live_handoff_entry_at(url, browser, now).map(|entry| (entry.path, entry.user_agent))
}

fn live_handoff_entry_at(url: &str, browser: &str, now: SystemTime) -> Option<HandoffCookies> {
    let live_browser = crate::video_tools::cookies_browser_spec(browser).is_some();
    handoff_entry(url).filter(|entry| handoff_jar_usable_at(entry, live_browser, now))
}

/// True when a staged jar may replace `--cookies-from-browser`. With a live
/// browser source, only a fresh jar wins; without one, a stale jar still
/// beats sending no cookies at all.
fn handoff_jar_usable_at(entry: &HandoffCookies, live_browser: bool, now: SystemTime) -> bool {
    // SystemTime includes suspend duration on Linux. A backwards wall-clock
    // adjustment fails closed as stale when a live browser source exists.
    !live_browser
        || now
            .duration_since(entry.staged_at)
            .is_ok_and(|age| age < HANDOFF_FRESH)
}

fn ytdlp_spawn_identity_at(cookies_browser: &str, page_url: &str, now: SystemTime) -> Vec<String> {
    let live_browser = crate::video_tools::cookies_browser_spec(cookies_browser).is_some();
    let entry = handoff_entry(page_url);
    // Filter once so the staged UA and cookie jar share the same freshness decision.
    let usable = entry.filter(|entry| handoff_jar_usable_at(entry, live_browser, now));
    let user_agent = usable
        .as_ref()
        .and_then(|entry| entry.user_agent.as_deref());
    match usable.as_ref() {
        Some(entry) => {
            let mut args = vec![
                "--cookies".to_string(),
                entry.path.to_string_lossy().into_owned(),
            ];
            args.extend(crate::video_tools::ytdlp_identity_args(
                "none", user_agent, page_url,
            ));
            args
        }
        _ => crate::video_tools::ytdlp_identity_args(cookies_browser, user_agent, page_url),
    }
}

/// Full yt-dlp identity tail for a handoff URL: the staged `--cookies` jar
/// when usable and the staged UA, then the shared identity args. One helper
/// so the probe, VOD, live-capture and subtitle spawns cannot drift. Skipping
/// `--cookies-from-browser` matters: yt-dlp aborts the whole spawn when its
/// keyring extraction fails, even when the jar would have worked.
pub(crate) fn ytdlp_spawn_identity(cookies_browser: &str, page_url: &str) -> Vec<String> {
    ytdlp_spawn_identity_at(cookies_browser, page_url, SystemTime::now())
}

/// `--cookies` argv for a URL's staged handoff cookies, if any. Test helper;
/// production spawns use [`ytdlp_spawn_identity`], which also applies the
/// freshness policy.
#[cfg(test)]
pub(crate) fn handoff_cookie_args(url: &str) -> Vec<String> {
    match handoff_entry(url).map(|e| e.path) {
        Some(p) => vec!["--cookies".to_string(), p.to_string_lossy().into_owned()],
        None => Vec::new(),
    }
}

/// Handoff jar for gallery-dl when it is safe to use alongside the configured
/// browser source. A stale snapshot would override rotated cookies exported
/// from the live browser, so withhold it and let gallery-dl use the export.
pub(crate) fn live_handoff_cookie_path(url: &str, browser: &str) -> Option<PathBuf> {
    live_handoff_cookie_path_at(url, browser, SystemTime::now())
}

fn live_handoff_cookie_path_at(url: &str, browser: &str, now: SystemTime) -> Option<PathBuf> {
    live_handoff_entry_at(url, browser, now).map(|entry| entry.path)
}

/// Drop a URL's staged handoff cookies and delete the file. Called when its
/// download finishes; failures and aborts keep the entry (bounded) so a retry
/// or resume still passes the challenge.
pub(crate) fn discard_handoff_cookies(url: &str) {
    if let Some(p) = take_handoff_cookie_path(url) {
        let _ = std::fs::remove_file(p);
    }
}

/// GTK-safe variant: drop the in-memory entry now and unlink its file on the
/// runtime's blocking pool.
pub(crate) fn discard_handoff_cookies_async(url: &str) {
    if let Some(p) = take_handoff_cookie_path(url) {
        drop(crate::runtime::tokio_rt().spawn_blocking(move || {
            let _ = std::fs::remove_file(p);
        }));
    }
}

fn take_handoff_cookie_path(url: &str) -> Option<PathBuf> {
    handoff_key(url).and_then(|key| {
        let mut store = lock_handoff_cookies();
        store.order.retain(|candidate| candidate != &key);
        let path = store.by_key.remove(&key).map(|entry| entry.path);
        if let Some(host) = url::Url::parse(&key)
            .ok()
            .and_then(|parsed| parsed.host_str().map(str::to_ascii_lowercase))
        {
            prune_suppression(&mut store, &host);
        }
        path
    })
}

/// Delete every staged cookie file. Called at shutdown: the entries only live
/// in memory, so files left behind would never be cleaned until the next
/// startup sweep.
pub(crate) fn purge_handoff_cookies() {
    let paths: Vec<PathBuf> = {
        let mut store = lock_handoff_cookies();
        store.order.clear();
        store.suppressed_hosts.clear();
        store.suppressed_order.clear();
        store.by_key.drain().map(|(_, e)| e.path).collect()
    };
    for p in paths {
        let _ = std::fs::remove_file(p);
    }
}

#[derive(Deserialize)]
struct AddRequest {
    url: String,
    /// Millisecond timestamp and unique nonce are included in the signed JSON body.
    /// Optional only for legacy unauthenticated URL-only requests; authenticated
    /// requests missing either field are rejected.
    #[serde(default)]
    timestamp_ms: Option<u64>,
    #[serde(default)]
    nonce: Option<String>,
    #[serde(default)]
    cookies: Vec<Cookie>,
    /// Browser UA; `cf_clearance` is UA-bound, so it travels with the cookies.
    #[serde(default)]
    user_agent: Option<String>,
}

/// A URL handed off from the extension. Cookies (if any) are stored
/// separately in HANDOFF_COOKIES, keyed by canonical URL, for the video
/// probe and every yt-dlp download leg.
/// Outcome of routing a handoff URL on the GTK side.
/// Replaces the old boolean: the extension needs to distinguish "card opened"
/// (cookies must be retained) from "declined" (cookies can be cleaned up).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HandoffOutcome {
    /// Download was queued (or async intake started).
    Queued,
    /// New Download card was opened; user action still pending.
    /// Cookies must be retained for the card's probe/download lifecycle.
    CardOpened,
    /// User explicitly declined the request.
    Declined,
    /// Routing or enqueueing failed.
    Failed,
}

/// Pure decision: map a handoff outcome to (HTTP status, discard_cookies).
/// Extracted for unit testing; the async handler uses this.
pub(crate) fn handoff_outcome_response(outcome: &HandoffOutcome) -> (axum::http::StatusCode, bool) {
    use axum::http::StatusCode;
    match outcome {
        HandoffOutcome::Queued => (StatusCode::OK, false),
        HandoffOutcome::CardOpened => (StatusCode::ACCEPTED, false),
        HandoffOutcome::Declined | HandoffOutcome::Failed => (StatusCode::CONFLICT, true),
    }
}

pub(crate) struct Handoff {
    pub url: String,
    /// Set for `/add?confirm=1`: the GTK side answers with the outcome.
    /// Dropping it unanswered counts as declined.
    pub decision: Option<oneshot::Sender<HandoffOutcome>>,
    /// Whether the extension proved possession of the pairing secret with
    /// current replay metadata. Unpaired and legacy callers are forced to
    /// confirm even when the user enabled auto-add.
    pub authenticated: bool,
    /// GTK closes the dialog at this instant, so it cannot outlive the
    /// server's confirmation window.
    pub expires_at: Option<Instant>,
}

#[derive(Clone)]
struct AppState {
    tx: Sender<Handoff>,
    pairing: Option<Arc<Pairing>>,
    confirm_wait: Duration,
    confirm_slots: Arc<Semaphore>,
}

/// Start the extension integration server. `tx` carries handoffs to the GTK
/// thread, which enqueues them in the download manager. Runs until the
/// process exits.
pub async fn serve(tx: Sender<Handoff>) {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, PORT));
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("Extension HTTP server couldn't bind {addr}: {e}");
            return;
        }
    };
    tracing::info!("Extension HTTP server listening on {addr}");
    let pairing = pairing_path().and_then(|path| match Pairing::load_or_create(&path) {
        Ok(p) => Some(Arc::new(p)),
        Err(e) => {
            tracing::warn!("Extension pairing secret unavailable ({path:?}): {e}");
            None
        }
    });
    if let Some(p) = &pairing {
        let _ = PAIRING.set(Arc::clone(p));
    }
    serve_listener(listener, tx, pairing, CONFIRM_WAIT).await;
}

async fn serve_listener(
    listener: TcpListener,
    tx: Sender<Handoff>,
    pairing: Option<Arc<Pairing>>,
    confirm_wait: Duration,
) {
    serve_listener_with_cap(
        listener,
        tx,
        pairing,
        confirm_wait,
        MAX_PENDING_CONFIRMATIONS,
    )
    .await;
}

async fn serve_listener_with_cap(
    listener: TcpListener,
    tx: Sender<Handoff>,
    pairing: Option<Arc<Pairing>>,
    confirm_wait: Duration,
    confirm_cap: usize,
) {
    serve_listener_with_cap_and_timeouts(
        listener,
        tx,
        pairing,
        confirm_wait,
        confirm_cap,
        EXTENSION_HEADER_READ_TIMEOUT,
        EXTENSION_CONNECTION_TIMEOUT,
    )
    .await;
}

async fn serve_listener_with_cap_and_timeouts(
    listener: TcpListener,
    tx: Sender<Handoff>,
    pairing: Option<Arc<Pairing>>,
    confirm_wait: Duration,
    confirm_cap: usize,
    header_read_timeout: Duration,
    connection_timeout: Duration,
) {
    let port = match listener.local_addr() {
        Ok(address) => address.port(),
        Err(error) => {
            tracing::warn!("Extension HTTP server has no local address: {error}");
            return;
        }
    };
    let state = AppState {
        tx,
        pairing,
        confirm_wait,
        confirm_slots: Arc::new(Semaphore::new(confirm_cap)),
    };
    let app = router(state, port);
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(connection) => connection,
            Err(error) => {
                tracing::warn!(%error, "Extension HTTP server accept failed");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let app = app.clone();
        tokio::spawn(async move {
            let service = app
                .map_request(|request: HyperRequest<Incoming>| request.map(axum::body::Body::new));
            let service = TowerToHyperService::new(service);
            let mut builder = http1::Builder::new();
            builder
                .timer(TokioTimer::default())
                .header_read_timeout(header_read_timeout)
                .keep_alive(false);
            match tokio::time::timeout(
                connection_timeout,
                builder.serve_connection(TokioIo::new(stream), service),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::debug!(%peer, %error, "Extension HTTP connection ended");
                }
                Err(_) => {
                    tracing::debug!(%peer, "Extension HTTP connection timed out");
                }
            }
        });
    }
}

fn router(state: AppState, port: u16) -> Router {
    let hosts: Arc<[String]> =
        Arc::from([format!("127.0.0.1:{port}"), format!("localhost:{port}")]);
    Router::new()
        .route("/add", post(handle_add))
        .route("/hello", get(handle_hello))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        // Outermost: rejected before the body is read.
        .layer(middleware::from_fn_with_state(hosts, guard))
        .with_state(state)
}

/// Reject requests that are not addressed to this server by name or that a
/// web page (rather than an extension) originated.
async fn guard(State(hosts): State<Arc<[String]>>, req: Request, next: Next) -> Response {
    if host_allowed(req.headers(), &hosts) && origin_allowed(req.headers()) {
        next.run(req).await
    } else {
        StatusCode::FORBIDDEN.into_response()
    }
}

fn host_allowed(headers: &HeaderMap, hosts: &[String]) -> bool {
    let mut values = headers.get_all(header::HOST).iter();
    match (values.next(), values.next()) {
        (Some(v), None) => v
            .to_str()
            .is_ok_and(|v| hosts.iter().any(|h| h.eq_ignore_ascii_case(v))),
        _ => false,
    }
}

/// No `Origin` means not a browser request (curl, scripts): the same trust as
/// any local process. A present `Origin` must be a browser extension's. The
/// scheme does not say *which* extension: pairing (see the module docs) is
/// what authenticates the caller.
fn origin_allowed(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(header::ORIGIN).iter();
    match (values.next(), values.next()) {
        (None, _) => true,
        (Some(v), None) => v.to_str().is_ok_and(is_extension_origin),
        _ => false,
    }
}

fn is_extension_origin(origin: &str) -> bool {
    ["moz-extension://", "chrome-extension://"]
        .iter()
        .filter_map(|scheme| origin.strip_prefix(scheme))
        .any(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

/// `GET /hello?n=<16-64 hex chars>`: prove this server holds the pairing secret.
async fn handle_hello(
    State(st): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let Some(pairing) = &st.pairing else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let nonce = q
        .get("n")
        .filter(|n| (16..=64).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_hexdigit()));
    let Some(nonce) = nonce else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    Json(serde_json::json!({ "proof": pairing.proof(nonce) })).into_response()
}

fn is_json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"))
}

/// Attach the number of cookies serialized into the staged jar.
/// Invalid, out-of-scope, and malformed cookies are skipped by design, but
/// the extension should be able to detect a partial cookie handoff.
fn cookie_report_response(accepted: usize, status: StatusCode) -> Response {
    let value = accepted
        .to_string()
        .parse::<HeaderValue>()
        .expect("an integer is always a valid header value");
    let mut response = status.into_response();
    response
        .headers_mut()
        .insert("x-grab-cookies-accepted", value);
    response
}

async fn handle_add(
    State(st): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    tracing::debug!("Extension handoff received");
    if !is_json_content_type(&headers) {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    // A token, when sent, must verify: a stale pairing is an error the
    // extension should see, not something to quietly downgrade.
    let token_authenticated = match headers.get("x-grab-token") {
        None => false,
        Some(token) => {
            let valid = st
                .pairing
                .as_ref()
                .zip(token.to_str().ok())
                .is_some_and(|(p, t)| p.verify_token(&body, t));
            if !valid {
                return StatusCode::FORBIDDEN.into_response();
            }
            true
        }
    };
    let req: AddRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) if e.is_data() => return StatusCode::UNPROCESSABLE_ENTITY.into_response(),
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    if req.url.trim().is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if req.url.len() > crate::download_intake::MAX_URL_LEN {
        return StatusCode::URI_TOO_LONG.into_response();
    }
    // Bound cookie count: each is a small name/value pair.
    if req.cookies.len() > 100 {
        return StatusCode::BAD_REQUEST.into_response();
    }
    // Legacy compatibility: old paired extensions do not include the
    // timestamp/nonce pair. Preserve their URL handoff, but downgrade them to
    // the same confirmation-only, no-cookie behavior as unpaired callers.
    // A partially present or malformed pair is never treated as legacy.
    // Remove after 2027-03-01, once extension builds predating the nonce have
    // aged out of auto-update.
    let legacy_authenticated =
        token_authenticated && req.timestamp_ms.is_none() && req.nonce.is_none();
    if token_authenticated
        && !legacy_authenticated
        && let Err(status) = reserve_handoff_nonce(req.timestamp_ms, req.nonce.as_deref())
    {
        return status.into_response();
    }
    let authenticated = token_authenticated && !legacy_authenticated;
    if legacy_authenticated {
        tracing::debug!("legacy paired extension handoff requires confirmation and drops cookies");
    }
    // Unauthenticated callers always wait for the user's answer: any local
    // process can POST without a token, so silent queuing would let
    // arbitrary software start downloads. The confirm dialog is the
    // authorization for callers without a pairing.
    let wait = q.get("confirm").is_some_and(|v| v == "1") || !authenticated;
    // The GTK thread drains the handoff queue into dialogs, so the queue
    // depth alone does not bound how many can be open. Refuse excess
    // confirmations instead of letting them pile up on the user.
    let _confirm_slot = if wait {
        match Arc::clone(&st.confirm_slots).try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => return StatusCode::TOO_MANY_REQUESTS.into_response(),
        }
    } else {
        None
    };
    // Also reject before staging when the queue is already full: staging
    // first could evict an in-flight handoff's jar only to refuse this one.
    if st.tx.capacity() == 0 {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let (decision, answer) = if wait {
        let (tx, rx) = oneshot::channel();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };
    let handoff = Handoff {
        url: req.url.clone(),
        decision,
        authenticated,
        expires_at: wait.then(|| Instant::now() + st.confirm_wait),
    };
    // Stage before queuing: the GTK side may start a probe the moment the
    // handoff is received, and every probe leg looks the cookies up by URL.
    // An authenticated handoff with no usable cookies discards the stale
    // exact entry, and every cookieless handoff suppresses older same-host
    // jars so the fallback cannot resurrect an earlier browser session.
    let (staged, accepted_cookie_count) = if authenticated {
        match write_handoff_cookies_with_count(&req.url, &req.cookies) {
            Some((path, accepted)) => {
                store_handoff_cookies(&req.url, path, req.user_agent);
                (true, accepted)
            }
            None => {
                discard_handoff_cookies(&req.url);
                (false, 0)
            }
        }
    } else {
        (false, 0)
    };
    let mut suppression = if !staged {
        suppress_handoff_host(&req.url)
    } else {
        None
    };
    match st.tx.try_send(handoff) {
        Ok(()) => {
            if let Some(suppression) = suppression.as_mut() {
                suppression.mark_sent();
            }
        }
        Err(TrySendError::Full(_)) => {
            if staged {
                discard_handoff_cookies(&req.url);
            }
            // Keep the nonce reserved. The extension creates a fresh nonce on
            // each 429 retry; releasing this one would let a captured rejected
            // POST become valid later when the queue has space again.
            return StatusCode::TOO_MANY_REQUESTS.into_response();
        }
        Err(TrySendError::Closed(_)) => {
            if staged {
                discard_handoff_cookies(&req.url);
            }
            // Do not reopen a consumed nonce after an internal queue failure.
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    let Some(answer) = answer else {
        // The handoff was accepted for immediate routing; keep suppression
        // active for the download it may start.
        if let Some(suppression) = suppression {
            suppression.keep();
        }
        return cookie_report_response(accepted_cookie_count, StatusCode::OK);
    };
    match tokio::time::timeout(st.confirm_wait, answer).await {
        Ok(Ok(outcome)) => {
            let (status, discard) = handoff_outcome_response(&outcome);
            if discard {
                if let Some(suppression) = suppression.as_mut() {
                    suppression.release();
                }
                if staged {
                    discard_handoff_cookies(&req.url);
                }
                // The explicit release above prevents the Drop guard from
                // decrementing a second time.
            } else if let Some(suppression) = suppression {
                suppression.keep();
            }
            cookie_report_response(if discard { 0 } else { accepted_cookie_count }, status)
        }
        outcome => {
            // Dropped unanswered or timed out: this request will not start a
            // download, so release its suppression before returning.
            if let Some(suppression) = suppression.as_mut() {
                suppression.release();
            }
            drop(suppression);
            if staged {
                discard_handoff_cookies(&req.url);
            }
            let status = if outcome.is_err() {
                StatusCode::REQUEST_TIMEOUT
            } else {
                StatusCode::CONFLICT
            };
            cookie_report_response(0, status)
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::await_holding_lock,
    reason = "tests hold the serial-test guard across awaits by design"
)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc::{Receiver, channel};

    struct Server {
        port: u16,
        rx: Receiver<Handoff>,
    }

    const TEST_SECRET: [u8; SECRET_LEN] = [7; SECRET_LEN];

    fn test_pairing() -> Option<Arc<Pairing>> {
        Some(Arc::new(Pairing::from_secret(TEST_SECRET)))
    }

    async fn start(depth: usize) -> Server {
        start_with(depth, test_pairing(), Duration::from_secs(5)).await
    }

    async fn start_with(depth: usize, pairing: Option<Arc<Pairing>>, wait: Duration) -> Server {
        start_with_cap(depth, pairing, wait, MAX_PENDING_CONFIRMATIONS).await
    }

    async fn start_with_cap(
        depth: usize,
        pairing: Option<Arc<Pairing>>,
        wait: Duration,
        confirm_cap: usize,
    ) -> Server {
        start_with_cap_and_timeouts(
            depth,
            pairing,
            wait,
            confirm_cap,
            EXTENSION_HEADER_READ_TIMEOUT,
            EXTENSION_CONNECTION_TIMEOUT,
        )
        .await
    }

    async fn start_with_cap_and_timeouts(
        depth: usize,
        pairing: Option<Arc<Pairing>>,
        wait: Duration,
        confirm_cap: usize,
        header_read_timeout: Duration,
        connection_timeout: Duration,
    ) -> Server {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = channel(depth);
        tokio::spawn(serve_listener_with_cap_and_timeouts(
            listener,
            tx,
            pairing,
            wait,
            confirm_cap,
            header_read_timeout,
            connection_timeout,
        ));
        Server { port, rx }
    }

    /// `X-Grab-Token` value a paired extension would send for `body`.
    fn token(body: &str) -> String {
        let mac = Pairing::from_secret(TEST_SECRET).mac(CLIENT_CONTEXT, body.as_bytes());
        hex_encode(&mac.finalize().into_bytes())
    }

    /// One request on a fresh connection; returns the raw response.
    async fn request_to(
        port: u16,
        method: &str,
        target: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> String {
        let mut req = format!("{method} {target} HTTP/1.1\r\nConnection: close\r\n");
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
        let mut s = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out).await;
        String::from_utf8_lossy(&out).into_owned()
    }

    /// POST /add or a confirmation URL on a fresh connection. When a valid
    /// test MAC is supplied, upgrade the fixture to timestamp/nonce metadata
    /// and re-sign it before sending.
    async fn request_with_target(
        port: u16,
        target: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> String {
        static TEST_NONCE_COUNTER: AtomicU64 = AtomicU64::new(1);
        let mut body_owned = body.to_string();
        let mut headers_owned: Vec<(String, String)> = headers
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect();
        if let Some((_, supplied_token)) = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("x-grab-token"))
            && *supplied_token == token(body)
        {
            let mut json: serde_json::Value = serde_json::from_str(body).expect("test JSON");
            let object = json.as_object_mut().expect("JSON object");
            object
                .entry("timestamp_ms")
                .or_insert_with(|| serde_json::json!(unix_time_ms().expect("system clock")));
            object.entry("nonce").or_insert_with(|| {
                serde_json::json!(format!(
                    "{:032x}",
                    TEST_NONCE_COUNTER.fetch_add(1, Ordering::Relaxed)
                ))
            });
            body_owned = serde_json::to_string(&json).expect("serialize v2 request");
            let fresh_token = token(&body_owned);
            for (name, value) in &mut headers_owned {
                if name.eq_ignore_ascii_case("x-grab-token") {
                    *value = fresh_token.clone();
                }
            }
        }
        let header_refs: Vec<(&str, &str)> = headers_owned
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        request_to(port, "POST", target, &header_refs, &body_owned).await
    }

    async fn request(port: u16, headers: &[(&str, &str)], body: &str) -> String {
        request_with_target(port, "/add", headers, body).await
    }

    fn status(resp: &str) -> &str {
        resp.split_whitespace().nth(1).unwrap_or("")
    }

    const JSON: (&str, &str) = ("Content-Type", "application/json");
    const BODY: &str = r#"{"url":"https://example.com/file.zip"}"#;

    #[tokio::test]
    async fn accepts_loopback_host_without_origin() {
        let mut srv = start(4).await;
        let host = format!("127.0.0.1:{}", srv.port);
        // Unauthenticated: waits for the user's confirmation.
        let task = tokio::spawn(async move {
            request_to(srv.port, "POST", "/add", &[("Host", &host), JSON], BODY).await
        });
        let handoff = srv.rx.recv().await.expect("handoff");
        assert_eq!(handoff.url, "https://example.com/file.zip");
        assert!(!handoff.authenticated);
        assert!(handoff.expires_at.is_some());
        handoff
            .decision
            .expect("decision")
            .send(HandoffOutcome::Queued)
            .unwrap();
        assert_eq!(status(&task.await.unwrap()), "200");
    }

    #[tokio::test]
    async fn accepts_localhost_name_and_extension_origins() {
        let mut srv = start(4).await;
        let host = format!("localhost:{}", srv.port);
        for origin in [
            "moz-extension://8f1c2a4e-1b2c-4d5e-9f00-aabbccddeeff",
            "chrome-extension://abcdefghijklmnopabcdefghijklmnop",
        ] {
            // Unauthenticated: waits for the user's confirmation.
            let task = tokio::spawn({
                let host = host.clone();
                async move {
                    request_to(
                        srv.port,
                        "POST",
                        "/add",
                        &[("Host", &host), ("Origin", origin), JSON],
                        BODY,
                    )
                    .await
                }
            });
            let handoff = srv.rx.recv().await.expect("handoff");
            handoff
                .decision
                .expect("decision")
                .send(HandoffOutcome::Queued)
                .unwrap();
            let r = task.await.unwrap();
            assert_eq!(status(&r), "200", "{origin}: {r}");
        }
    }

    #[tokio::test]
    async fn rejects_dns_rebinding_host() {
        let mut srv = start(4).await;
        for host in [
            format!("evil.example:{}", srv.port),
            "127.0.0.1".to_string(),
            format!("127.0.0.1:{}", srv.port + 1),
            format!("127.0.0.1.evil.example:{}", srv.port),
        ] {
            let r = request(srv.port, &[("Host", &host), JSON], BODY).await;
            assert_eq!(status(&r), "403", "{host}: {r}");
        }
        assert!(srv.rx.try_recv().is_err(), "nothing may reach the queue");
    }

    #[tokio::test]
    async fn rejects_web_and_malformed_origins() {
        let mut srv = start(4).await;
        let host = format!("127.0.0.1:{}", srv.port);
        for origin in [
            "https://evil.example",
            "null",
            "http://127.0.0.1:9412",
            "moz-extension://",
            "moz-extension://evil.example/x",
            "chrome-extension://id with space",
        ] {
            let r = request(srv.port, &[("Host", &host), ("Origin", origin), JSON], BODY).await;
            assert_eq!(status(&r), "403", "{origin}: {r}");
        }
        // Two Origin headers are ambiguous: refuse.
        let r = request(
            srv.port,
            &[
                ("Host", &host),
                ("Origin", "moz-extension://abc"),
                ("Origin", "https://evil.example"),
                JSON,
            ],
            BODY,
        )
        .await;
        assert_eq!(status(&r), "403", "{r}");
        assert!(srv.rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn rejects_non_json_oversized_and_bad_urls() {
        let mut srv = start(4).await;
        let host = format!("127.0.0.1:{}", srv.port);
        let h = [("Host", host.as_str())];

        let r = request(srv.port, &[h[0], ("Content-Type", "text/plain")], BODY).await;
        assert_eq!(status(&r), "415", "{r}");

        let big = format!(
            r#"{{"url":"https://example.com/{}"}}"#,
            "a".repeat(MAX_BODY_BYTES)
        );
        let r = request(srv.port, &[h[0], JSON], &big).await;
        assert_eq!(status(&r), "413", "{r}");

        let long = format!(
            r#"{{"url":"https://example.com/{}"}}"#,
            "a".repeat(crate::download_intake::MAX_URL_LEN)
        );
        let r = request(srv.port, &[h[0], JSON], &long).await;
        assert_eq!(status(&r), "414", "{r}");

        let r = request(srv.port, &[h[0], JSON], r#"{"url":"   "}"#).await;
        assert_eq!(status(&r), "400", "{r}");
        assert!(srv.rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn full_queue_answers_429_instead_of_growing() {
        let mut srv = start(1).await;
        let host = format!("127.0.0.1:{}", srv.port);
        // First request waits for confirmation (unauthenticated), occupying
        // the single queue slot.
        let task = tokio::spawn({
            let host = host.clone();
            async move { request_to(srv.port, "POST", "/add", &[("Host", &host), JSON], BODY).await }
        });
        // Give it a moment to queue.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !task.is_finished(),
            "first request must wait for the dialog"
        );
        // Queue full: second request gets 429 immediately.
        let r = request(srv.port, &[("Host", &host), JSON], BODY).await;
        assert_eq!(status(&r), "429", "{r}");
        // Answer the first dialog: it completes, queue drains.
        let handoff = srv.rx.recv().await.expect("handoff");
        handoff
            .decision
            .expect("decision")
            .send(HandoffOutcome::Queued)
            .unwrap();
        assert_eq!(status(&task.await.unwrap()), "200");
        // Queue drained: third request is accepted (and waits for its dialog).
        let task3 = tokio::spawn({
            let host = host.clone();
            async move { request_to(srv.port, "POST", "/add", &[("Host", &host), JSON], BODY).await }
        });
        let handoff = srv.rx.recv().await.expect("handoff");
        handoff
            .decision
            .expect("decision")
            .send(HandoffOutcome::Queued)
            .unwrap();
        let r = task3.await.unwrap();
        assert_eq!(status(&r), "200", "queue drains: {r}");
    }

    /// The staging map is process-global: tests that touch it run one at a
    /// time, and start from an empty store.
    static STORE_LOCK: Mutex<()> = Mutex::new(());

    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        let guard = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        purge_handoff_cookies();
        guard
    }

    /// Clear staged entries and delete their files.
    fn clear_handoff_cookies() {
        purge_handoff_cookies();
    }

    /// Minimal cookie as the original extension sent it (name/value only).
    fn cookie(name: &str, value: &str) -> Cookie {
        Cookie {
            name: name.to_string(),
            value: value.to_string(),
            domain: None,
            host_only: None,
            path: None,
            secure: None,
            expiration_date: None,
        }
    }

    #[test]
    fn write_handoff_cookies_netscape_format() {
        let cookies = vec![
            cookie("session", "abc123"),
            // Tabs/newlines would corrupt the tab-separated format.
            cookie("we\trd", "a\nb\rc"),
            // Empty names carry nothing.
            cookie("", "skip"),
            // Host-only cookies get FALSE with no leading dot.
            Cookie {
                name: "hostonly".to_string(),
                value: "xyz".to_string(),
                domain: Some("example.com".to_string()),
                host_only: Some(true),
                path: None,
                secure: None,
                expiration_date: None,
            },
            // Pre-dotted domain from the extension is handled.
            Cookie {
                name: "dotted".to_string(),
                value: "123".to_string(),
                domain: Some(".example.com".to_string()),
                host_only: Some(false),
                path: None,
                secure: None,
                expiration_date: None,
            },
        ];
        let path = write_handoff_cookies("https://example.com/watch?v=1", &cookies).expect("path");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "cookie file must be owner-only");
        }
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("# Netscape HTTP Cookie File"));
        let rows: Vec<&str> = lines.collect();
        assert_eq!(
            rows,
            [
                ".example.com\tTRUE\t/\tFALSE\t0\tsession\tabc123",
                ".example.com\tTRUE\t/\tFALSE\t0\twerd\tabc",
                "example.com\tFALSE\t/\tFALSE\t0\thostonly\txyz",
                ".example.com\tTRUE\t/\tFALSE\t0\tdotted\t123",
            ],
            "{text}"
        );
    }

    #[test]
    fn write_handoff_cookies_rejects_empty_and_bad_url() {
        assert!(write_handoff_cookies("https://example.com/", &[]).is_none());
        assert!(write_handoff_cookies("not a url", &[cookie("a", "b")]).is_none());
    }

    #[test]
    fn write_handoff_cookies_preserves_browser_scope() {
        // Host-only cookie: must not widen to subdomains.
        let cookies = vec![Cookie {
            name: "sess".to_string(),
            value: "s3cr3t".to_string(),
            domain: Some("videos.example.com".to_string()),
            host_only: Some(true),
            path: Some("/watch".to_string()),
            secure: Some(true),
            expiration_date: Some(1893456000.0),
        }];
        let path =
            write_handoff_cookies("https://videos.example.com/watch/1", &cookies).expect("path");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let rows: Vec<&str> = text.lines().skip(1).collect();
        assert_eq!(
            rows,
            ["videos.example.com\tFALSE\t/watch\tTRUE\t1893456000\tsess\ts3cr3t"],
            "{text}"
        );

        // Subdomain cookie with leading dot: tailmatch, dot preserved.
        let cookies = vec![Cookie {
            name: "prefs".to_string(),
            value: "dark".to_string(),
            domain: Some(".example.com".to_string()),
            host_only: Some(false),
            path: None,
            secure: None,
            expiration_date: None,
        }];
        let path = write_handoff_cookies("https://www.example.com/", &cookies).expect("path");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let rows: Vec<&str> = text.lines().skip(1).collect();
        assert_eq!(
            rows,
            [".example.com\tTRUE\t/\tFALSE\t0\tprefs\tdark"],
            "{text}"
        );
    }

    #[test]
    fn cookie_domain_must_not_be_a_public_suffix() {
        assert!(!domain_covers_host("com", "example.com"));
        assert!(!domain_covers_host("co.uk", "example.co.uk"));
        assert!(!domain_covers_host("github.io", "user.github.io"));
        assert!(domain_covers_host("example.co.uk", "www.example.co.uk"));
        assert!(domain_covers_host("example.com", "example.com"));
    }

    #[test]
    fn handoff_cookie_args_match_canonical_url_forms() {
        let _g = exclusive();
        // Stored under the bare domain; the intake hands the probe the
        // slash-terminated canonical form. One entry must serve both.
        let path = write_handoff_cookies("https://example.com", &[cookie("c", "v")]).expect("path");
        store_handoff_cookies("https://example.com", path.clone(), None);
        for url in ["https://example.com", "https://example.com/"] {
            let args = handoff_cookie_args(url);
            assert_eq!(args.len(), 2, "{url}: {args:?}");
            assert_eq!(args[0], "--cookies");
            assert_eq!(args[1], path.to_string_lossy());
        }
        assert!(handoff_cookie_args("https://other.example/").is_empty());
        // Same-host fallback: a playlist item queued under its own URL finds
        // the cookies staged for the collection page.
        let args = handoff_cookie_args("https://example.com/other");
        assert_eq!(args.len(), 2, "{args:?}");
        assert_eq!(args[1], path.to_string_lossy());
        clear_handoff_cookies();
    }

    #[test]
    fn handoff_jar_usable_only_while_fresh_with_a_live_browser() {
        let entry = HandoffCookies {
            path: PathBuf::from("/tmp/jar"),
            user_agent: None,
            staged_at: SystemTime::now(),
            staged_instant: Instant::now(),
        };
        assert!(handoff_jar_usable_at(&entry, true, entry.staged_at));
        assert!(!handoff_jar_usable_at(
            &entry,
            true,
            entry.staged_at + HANDOFF_FRESH
        ));
        assert!(handoff_jar_usable_at(
            &entry,
            false,
            entry.staged_at + HANDOFF_FRESH
        ));
    }

    #[test]
    fn handoff_user_agent_uses_the_same_freshness_policy_as_its_jar() {
        let _g = exclusive();
        let url = "https://ua-freshness.example.com/file";
        let path = write_handoff_cookies(url, &[cookie("c", "v")]).expect("path");
        store_handoff_cookies(url, path.clone(), Some("UA/1".into()));
        let now = SystemTime::now();

        assert_eq!(
            handoff_user_agent_at(url, "brave", now),
            Some("UA/1".into())
        );
        assert_eq!(
            live_handoff_identity_at(url, "brave", now),
            Some((path.clone(), Some("UA/1".into())))
        );

        let stale_at = now + HANDOFF_FRESH + Duration::from_secs(1);
        assert_eq!(handoff_user_agent_at(url, "brave", stale_at), None);
        assert_eq!(live_handoff_identity_at(url, "brave", stale_at), None);
        // Without a configured browser, the staged jar and its UA remain the only source.
        assert_eq!(
            handoff_user_agent_at(url, "none", stale_at),
            Some("UA/1".into())
        );
        assert_eq!(
            live_handoff_identity_at(url, "none", stale_at),
            Some((path, Some("UA/1".into())))
        );
        clear_handoff_cookies();
    }

    #[test]
    fn ytdlp_spawn_identity_drops_a_stale_jar_for_live_cookies() {
        let _g = exclusive();
        let url = "https://freshness.example.com/v";
        let path = write_handoff_cookies(url, &[cookie("c", "v")]).expect("path");
        store_handoff_cookies(url, path.clone(), Some("UA/1".into()));
        let now = SystemTime::now();
        let fresh = ytdlp_spawn_identity_at("brave", url, now);
        assert_eq!(fresh[0], "--cookies");
        assert_eq!(fresh[1], path.to_string_lossy().into_owned());
        assert!(
            !fresh
                .iter()
                .any(|a| a.starts_with("--cookies-from-browser="))
        );
        assert!(fresh.iter().any(|a| a == "UA/1"));
        let later = now + HANDOFF_FRESH + Duration::from_secs(1);
        let stale = ytdlp_spawn_identity_at("brave", url, later);
        assert!(!stale.iter().any(|a| a == "--cookies"));
        assert!(
            stale
                .iter()
                .any(|a| a.starts_with("--cookies-from-browser="))
        );
        // The stale jar's UA must not be paired with the configured browser's
        // live cookies; keep the UA only when the staged jar is used.
        assert!(!stale.iter().any(|a| a == "UA/1"));
        // Without a live source the stale jar and its UA remain the only source.
        let offline = ytdlp_spawn_identity_at("none", url, later);
        assert_eq!(offline[0], "--cookies");
        assert!(offline.iter().any(|a| a == "UA/1"));
        clear_handoff_cookies();
    }

    #[test]
    fn live_handoff_path_withholds_a_stale_jar_only_with_a_live_browser() {
        let _g = exclusive();
        let url = "https://gallery-freshness.example.com/album";
        let path = write_handoff_cookies(url, &[cookie("c", "v")]).expect("path");
        store_handoff_cookies(url, path.clone(), Some("UA/1".into()));

        let now = SystemTime::now();
        assert_eq!(
            live_handoff_cookie_path_at(url, "brave", now),
            Some(path.clone()),
            "fresh handoff jars remain preferred"
        );

        let stale_at = now + HANDOFF_FRESH;
        assert_eq!(
            live_handoff_cookie_path_at(url, "brave", stale_at),
            None,
            "a stale jar must not override the live browser's rotated cookies"
        );
        assert_eq!(
            live_handoff_cookie_path_at(url, "none", stale_at),
            Some(path),
            "without a browser source, the handoff jar is still the only cookie source"
        );
        clear_handoff_cookies();
    }

    #[test]
    fn discard_handoff_cookies_removes_entry_and_file() {
        let _g = exclusive();
        let path =
            write_handoff_cookies("https://example.com/item", &[cookie("c", "v")]).expect("path");
        store_handoff_cookies("https://example.com/item", path.clone(), None);
        assert!(path.exists());
        discard_handoff_cookies("https://example.com/item");
        assert!(!path.exists(), "file must be deleted");
        assert!(
            handoff_cookie_args("https://example.com/item").is_empty(),
            "entry must be gone"
        );
        clear_handoff_cookies();
    }

    #[test]
    fn store_handoff_cookies_evicts_past_cap() {
        let _g = exclusive();
        let mut paths = Vec::new();
        for i in 0..=(MAX_HANDOFF_ENTRIES + 2) {
            let url = format!("https://example.com/evict-{i}");
            let path = write_handoff_cookies(&url, &[cookie("c", "v")]).expect("path");
            store_handoff_cookies(&url, path.clone(), None);
            paths.push(path);
        }
        let len = HANDOFF_COOKIES.lock().unwrap().by_key.len();
        assert_eq!(len, MAX_HANDOFF_ENTRIES, "map must stay bounded");
        // Oldest first: the earliest handoffs are the ones evicted, with
        // their files; the newest MAX_HANDOFF_ENTRIES survive.
        let evicted = paths.len() - MAX_HANDOFF_ENTRIES;
        for (i, path) in paths.iter().enumerate() {
            assert_eq!(path.exists(), i >= evicted, "entry {i}");
        }
        // The evicted URL's own entry is gone; the host fallback still finds
        // a surviving same-host entry (playlist-item behavior).
        assert_eq!(handoff_cookie_args("https://example.com/evict-0").len(), 2);
        let newest = format!("https://example.com/evict-{}", paths.len() - 1);
        assert_eq!(handoff_cookie_args(&newest).len(), 2);
        clear_handoff_cookies();
    }

    #[test]
    fn handoff_cookie_fields_strip_all_control_characters() {
        let _g = exclusive();
        let url = "https://example.com/control-fields";
        let path = write_handoff_cookies(url, &[cookie("na\u{000b}me", "va\u{0085}l\u{0000}ue")])
            .expect("sanitized cookie");
        let text = std::fs::read_to_string(&path).expect("cookie file");
        assert!(text.contains("example.com\tTRUE\t/\tFALSE\t0\tname\tvalue\n"));
        assert!(
            text.chars()
                .all(|c| !c.is_control() || matches!(c, '\n' | '\t')),
            "only Netscape line and field delimiters may remain: {text:?}"
        );
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn handle_add_with_cookies_stages_file_for_probe() {
        let _g = exclusive();
        let mut srv = start(4).await;
        let host = format!("127.0.0.1:{}", srv.port);
        let body =
            r#"{"url":"https://example.com/watch?v=7","cookies":[{"name":"cf","value":"abc"}]}"#;
        let tok = token(body);
        let r = request(
            srv.port,
            &[("Host", &host), ("X-Grab-Token", &tok), JSON],
            body,
        )
        .await;
        assert_eq!(status(&r), "200", "{r}");
        assert!(
            r.to_ascii_lowercase()
                .contains("x-grab-cookies-accepted: 1"),
            "response must report the one cookie that was staged: {r}"
        );
        let handoff = srv.rx.recv().await.expect("handoff");
        assert_eq!(handoff.url, "https://example.com/watch?v=7");
        // The probe (and later the download legs) resolve the same args.
        let args = handoff_cookie_args(&handoff.url);
        assert_eq!(args.len(), 2, "{args:?}");
        assert_eq!(args[0], "--cookies");
        let staged = PathBuf::from(&args[1]);
        assert!(staged.exists(), "staged file must exist");
        let text = std::fs::read_to_string(&staged).unwrap();
        assert!(
            text.contains("example.com\tTRUE\t/\tFALSE\t0\tcf\tabc"),
            "{text}"
        );
        clear_handoff_cookies();
    }

    #[tokio::test]
    async fn handle_add_reports_cookies_dropped_by_scope_validation() {
        let _g = exclusive();
        clear_handoff_cookies();
        let mut srv = start(4).await;
        let host = format!("127.0.0.1:{}", srv.port);
        let body = r#"{"url":"https://example.com/file.zip","cookies":[{"name":"session","value":"abc","domain":"unrelated.example","host_only":false,"path":"/"}]}"#;
        let tok = token(body);
        let r = request(
            srv.port,
            &[("Host", &host), ("X-Grab-Token", &tok), JSON],
            body,
        )
        .await;
        assert_eq!(status(&r), "200", "{r}");
        assert!(
            r.to_ascii_lowercase()
                .contains("x-grab-cookies-accepted: 0"),
            "a cookie rejected by domain validation must be reported: {r}"
        );
        let handoff = srv.rx.recv().await.expect("handoff");
        assert!(
            handoff_cookie_args(&handoff.url).is_empty(),
            "a rejected cookie must not be staged"
        );
        clear_handoff_cookies();
    }

    #[tokio::test]
    async fn handle_add_cookieless_handoff_discards_stale_entry() {
        let _g = exclusive();
        let mut srv = start(4).await;
        let host = format!("127.0.0.1:{}", srv.port);
        let url = "https://example.com/watch?v=8";
        // First: authenticated handoff with cookies stages the entry.
        let body =
            r#"{"url":"https://example.com/watch?v=8","cookies":[{"name":"cf","value":"abc"}]}"#;
        let tok = token(body);
        let r = request(
            srv.port,
            &[("Host", &host), ("X-Grab-Token", &tok), JSON],
            body,
        )
        .await;
        assert_eq!(status(&r), "200", "{r}");
        let _ = srv.rx.recv().await.expect("handoff");
        let args = handoff_cookie_args(url);
        assert_eq!(args.len(), 2, "{args:?}");
        let staged = PathBuf::from(&args[1]);
        assert!(staged.exists(), "staged file must exist");
        // Second: authenticated handoff for the same URL with no cookies.
        // The stale entry must be discarded, not reused.
        let body2 = r#"{"url":"https://example.com/watch?v=8","cookies":[]}"#;
        let tok2 = token(body2);
        let r2 = request(
            srv.port,
            &[("Host", &host), ("X-Grab-Token", &tok2), JSON],
            body2,
        )
        .await;
        assert_eq!(status(&r2), "200", "{r2}");
        let _ = srv.rx.recv().await.expect("handoff");
        assert!(
            live_handoff_cookie_path(url, "none").is_none(),
            "stale entry must be gone"
        );
        assert!(!staged.exists(), "staged file must be deleted");
        clear_handoff_cookies();
    }

    #[tokio::test]
    async fn handle_add_rejects_too_many_cookies() {
        let _g = exclusive();
        let mut srv = start(4).await;
        let host = format!("127.0.0.1:{}", srv.port);
        let cookies: Vec<String> = (0..101)
            .map(|i| format!(r#"{{"name":"c{i}","value":"v"}}"#))
            .collect();
        let body = format!(
            r#"{{"url":"https://example.com/","cookies":[{}]}}"#,
            cookies.join(",")
        );
        let r = request(srv.port, &[("Host", &host), JSON], &body).await;
        assert_eq!(status(&r), "400", "{r}");
        assert!(srv.rx.try_recv().is_err());
        assert!(
            handoff_cookie_args("https://example.com/").is_empty(),
            "rejected handoff must stage nothing"
        );
        clear_handoff_cookies();
    }

    #[tokio::test]
    async fn cookieless_handoff_blocks_same_host_fallback_until_new_jar() {
        let _g = exclusive();
        let mut srv = start(4).await;
        let host = format!("127.0.0.1:{}", srv.port);
        let old_url = "https://same-host.example/old";
        let body =
            r#"{"url":"https://same-host.example/old","cookies":[{"name":"c","value":"old"}]}"#;
        let tok = token(body);
        let r = request(
            srv.port,
            &[("Host", &host), ("X-Grab-Token", &tok), JSON],
            body,
        )
        .await;
        assert_eq!(status(&r), "200", "{r}");
        let _ = srv.rx.recv().await.expect("handoff");
        let old_args = handoff_cookie_args(old_url);
        assert_eq!(old_args.len(), 2, "old jar must be staged");
        let old_path = PathBuf::from(&old_args[1]);

        // A new authenticated handoff for a sibling URL without cookies must
        // not fall back to the earlier browser session.
        let sibling = "https://same-host.example/new";
        let body = r#"{"url":"https://same-host.example/new","cookies":[]}"#;
        let tok = token(body);
        let r = request(
            srv.port,
            &[("Host", &host), ("X-Grab-Token", &tok), JSON],
            body,
        )
        .await;
        assert_eq!(status(&r), "200", "{r}");
        let _ = srv.rx.recv().await.expect("handoff");
        assert!(
            handoff_cookie_args(sibling).is_empty(),
            "the same-host fallback must not revive an older jar"
        );
        assert!(
            old_path.exists(),
            "suppression must not delete the old file"
        );

        // A later handoff that does stage cookies clears the tombstone's
        // effect by being newer than it.
        let body =
            r#"{"url":"https://same-host.example/fresh","cookies":[{"name":"c","value":"new"}]}"#;
        let tok = token(body);
        let r = request(
            srv.port,
            &[("Host", &host), ("X-Grab-Token", &tok), JSON],
            body,
        )
        .await;
        assert_eq!(status(&r), "200", "{r}");
        let _ = srv.rx.recv().await.expect("handoff");
        let fresh_args = handoff_cookie_args(sibling);
        assert_eq!(fresh_args.len(), 2, "newer handoff should be usable");
        assert_ne!(fresh_args[1], old_args[1], "new jar, not the old one");
        clear_handoff_cookies();
    }

    #[tokio::test]
    async fn pending_confirmation_cap_rejects_a_dialog_flood() {
        let mut srv = start_with_cap(4, test_pairing(), Duration::from_secs(5), 1).await;
        let host = format!("127.0.0.1:{}", srv.port);
        let task = tokio::spawn({
            let host = host.clone();
            async move { request_to(srv.port, "POST", "/add", &[("Host", &host), JSON], BODY).await }
        });
        let handoff = srv.rx.recv().await.expect("handoff");
        // One confirmation is in flight; a second must be refused instead of
        // opening another dialog, even though the handoff queue has room.
        let r = request(srv.port, &[("Host", &host), JSON], BODY).await;
        assert_eq!(status(&r), "429", "{r}");
        handoff
            .decision
            .expect("unauthenticated handoff carries a decision")
            .send(HandoffOutcome::Queued)
            .unwrap();
        assert_eq!(status(&task.await.unwrap()), "200");
    }

    #[test]
    fn accepted_suppression_survives_cancellation_drop() {
        let _g = exclusive();
        clear_handoff_cookies();
        let url = "https://cancelled.example/file";
        let path = write_handoff_cookies(url, &[cookie("c", "v")]).expect("path");
        store_handoff_cookies(url, path, None);
        let mut suppression = suppress_handoff_host(url).expect("suppression");
        suppression.mark_sent();
        drop(suppression);
        assert!(
            handoff_cookie_args(url).is_empty(),
            "a queued handoff may still be shown by GTK after its client disconnects"
        );
        clear_handoff_cookies();
    }

    #[test]
    fn overlapping_suppressions_keep_the_tombstone_until_all_release() {
        let _g = exclusive();
        clear_handoff_cookies();
        let url = "https://overlap.example/file";
        let path = write_handoff_cookies(url, &[cookie("c", "v")]).expect("path");
        store_handoff_cookies(url, path, None);
        let first = suppress_handoff_host(url).expect("first suppression");
        let second = suppress_handoff_host(url).expect("second suppression");
        drop(first);
        assert!(
            handoff_cookie_args(url).is_empty(),
            "one pending handoff must keep the jar hidden"
        );
        second.keep();
        assert!(
            handoff_cookie_args(url).is_empty(),
            "an approved handoff keeps suppressing the older jar"
        );
        clear_handoff_cookies();
    }

    #[test]
    fn approved_suppression_is_reclaimed_when_the_old_jar_is_removed() {
        let _g = exclusive();
        clear_handoff_cookies();
        let url = "https://reclaimed.example/file";
        let path = write_handoff_cookies(url, &[cookie("c", "old")]).expect("old jar");
        store_handoff_cookies(url, path, None);
        suppress_handoff_host(url).expect("suppression").keep();

        assert!(handoff_cookie_args(url).is_empty());
        assert!(
            lock_handoff_cookies()
                .suppressed_hosts
                .contains_key("reclaimed.example")
        );
        discard_handoff_cookies(url);
        assert!(
            !lock_handoff_cookies()
                .suppressed_hosts
                .contains_key("reclaimed.example")
        );
        clear_handoff_cookies();
    }

    #[test]
    fn approved_suppression_is_reclaimed_when_the_old_jar_is_replaced() {
        let _g = exclusive();
        clear_handoff_cookies();
        let url = "https://replaced.example/file";
        let old_path = write_handoff_cookies(url, &[cookie("c", "old")]).expect("old jar");
        store_handoff_cookies(url, old_path.clone(), None);
        suppress_handoff_host(url).expect("suppression").keep();

        let fresh_path = write_handoff_cookies(url, &[cookie("c", "fresh")]).expect("fresh jar");
        store_handoff_cookies(url, fresh_path.clone(), None);

        assert!(
            !lock_handoff_cookies()
                .suppressed_hosts
                .contains_key("replaced.example")
        );
        let args = handoff_cookie_args(url);
        assert_eq!(args.len(), 2);
        assert_eq!(PathBuf::from(&args[1]), fresh_path);
        assert_ne!(PathBuf::from(&args[1]), old_path);
        clear_handoff_cookies();
    }

    #[test]
    fn declined_suppression_restores_approved_cutoff_and_allows_newer_fallback() {
        let _g = exclusive();
        clear_handoff_cookies();
        let old_url = "https://cutoff.example/old";
        let old_path = write_handoff_cookies(old_url, &[cookie("c", "old")]).expect("old jar");
        store_handoff_cookies(old_url, old_path.clone(), None);
        suppress_handoff_host(old_url)
            .expect("first suppression")
            .keep();

        let fresh_url = "https://cutoff.example/fresh";
        let fresh_path =
            write_handoff_cookies(fresh_url, &[cookie("c", "fresh")]).expect("fresh jar");
        store_handoff_cookies(fresh_url, fresh_path.clone(), None);
        let fresh_args = handoff_cookie_args(fresh_url);
        assert_eq!(fresh_args.len(), 2);
        assert_eq!(PathBuf::from(&fresh_args[1]), fresh_path);

        let declined = suppress_handoff_host(fresh_url).expect("second suppression");
        assert!(handoff_cookie_args(fresh_url).is_empty());
        drop(declined);

        let fresh_args = handoff_cookie_args(fresh_url);
        assert_eq!(fresh_args.len(), 2);
        assert_eq!(PathBuf::from(&fresh_args[1]), fresh_path);

        // Declining the newer request rolls back only its pending cutoff. The
        // old exact-URL jar stays suppressed, while lookup may use the newer
        // same-host jar as fallback instead of reviving the old jar.
        let old_args = handoff_cookie_args(old_url);
        assert_eq!(old_args.len(), 2);
        assert_eq!(PathBuf::from(&old_args[1]), fresh_path);
        assert_ne!(PathBuf::from(&old_args[1]), old_path);
        clear_handoff_cookies();
    }

    #[test]
    fn approved_suppression_entries_stay_bounded_as_old_jars_are_evicted() {
        let _g = exclusive();
        clear_handoff_cookies();

        for index in 0..(MAX_SUPPRESSED_HOSTS * 2) {
            let url = format!("https://host-{index}.example.com/file");
            let path = write_handoff_cookies(&url, &[cookie("c", "v")]).expect("jar");
            store_handoff_cookies(&url, path, None);
            suppress_handoff_host(&url)
                .expect("staged jar should be suppressed")
                .keep();

            let store = lock_handoff_cookies();
            assert!(
                store.suppressed_hosts.len() <= MAX_HANDOFF_ENTRIES,
                "suppression entries without a stale staged jar should be reclaimed"
            );
            assert!(
                store.suppressed_hosts.len() <= MAX_SUPPRESSED_HOSTS,
                "tombstone map must remain within its documented cap"
            );
        }
        clear_handoff_cookies();
    }

    #[test]
    fn cookieless_suppression_ignores_hosts_without_a_staged_jar() {
        let _g = exclusive();
        clear_handoff_cookies();
        assert!(
            suppress_handoff_host("https://no-jar.example/file").is_none(),
            "there is nothing to suppress, and a flood must not grow the map"
        );
        let url = "https://no-jar.example/file";
        let path = write_handoff_cookies(url, &[cookie("c", "v")]).expect("path");
        store_handoff_cookies(url, path, None);
        assert_eq!(
            handoff_cookie_args(url).len(),
            2,
            "a later legitimate jar must still be usable"
        );
        clear_handoff_cookies();
    }

    #[test]
    fn handoff_host_fallback_returns_newest_entry() {
        let _g = exclusive();
        // Stage two different URLs on the same host; the newer one must win
        // the host fallback deterministically.
        let old_url = "https://example.com/old";
        let new_url = "https://example.com/new";
        let old_path = write_handoff_cookies(old_url, &[cookie("c", "old")]).expect("path");
        store_handoff_cookies(old_url, old_path, Some("OldUA/1.0".to_string()));
        let new_path = write_handoff_cookies(new_url, &[cookie("c", "new")]).expect("path");
        store_handoff_cookies(new_url, new_path.clone(), Some("NewUA/1.0".to_string()));
        // Look up a third URL on the same host repeatedly: under HashMap
        // ordering the winner would vary, but newest-first is deterministic.
        for _ in 0..50 {
            let entry = handoff_entry("https://example.com/other").expect("fallback entry");
            assert_eq!(
                entry.path, new_path,
                "host fallback must return the newest entry"
            );
            assert_eq!(entry.user_agent.as_deref(), Some("NewUA/1.0"));
        }
        clear_handoff_cookies();
    }

    #[tokio::test]
    async fn handle_add_429_stages_nothing() {
        let _g = exclusive();
        let srv = start(1).await;
        let host = format!("127.0.0.1:{}", srv.port);
        let body = r#"{"url":"https://example.com/a","cookies":[{"name":"c","value":"v"}]}"#;
        let tok = token(body);
        let r = request(
            srv.port,
            &[("Host", &host), ("X-Grab-Token", &tok), JSON],
            body,
        )
        .await;
        assert_eq!(status(&r), "200", "{r}");
        // Queue full: the handoff is refused, so its cookies must not linger.
        // Use a distinct host: the same-host fallback would (correctly) find
        // the first handoff's cookies.
        let body2 = r#"{"url":"https://other.example/b","cookies":[{"name":"c","value":"v"}]}"#;
        let tok2 = token(body2);
        let r = request(
            srv.port,
            &[("Host", &host), ("X-Grab-Token", &tok2), JSON],
            body2,
        )
        .await;
        assert_eq!(status(&r), "429", "{r}");
        assert!(
            handoff_cookie_args("https://other.example/b").is_empty(),
            "refused handoff must stage nothing"
        );
        clear_handoff_cookies();
    }

    // --- pairing -----------------------------------------------------------

    #[tokio::test]
    async fn hello_proves_knowledge_of_the_secret() {
        let srv = start(4).await;
        let host = format!("127.0.0.1:{}", srv.port);
        let nonce = "00112233445566778899aabbccddeeff";
        let r = request_to(
            srv.port,
            "GET",
            &format!("/hello?n={nonce}"),
            &[("Host", &host)],
            "",
        )
        .await;
        assert_eq!(status(&r), "200", "{r}");
        let expected = Pairing::from_secret(TEST_SECRET).proof(nonce);
        assert!(r.contains(&expected), "{r}");
        // A different secret yields a different proof: an impostor cannot fake it.
        let other = Pairing::from_secret([9; SECRET_LEN]).proof(nonce);
        assert!(!r.contains(&other));
    }

    #[test]
    fn hex_decode_rejects_non_hex_digits() {
        assert_eq!(hex_decode("0F"), Some(vec![0x0f]));
        // `from_str_radix` accepts a leading `+`; the decoder must not.
        assert_eq!(hex_decode("+f"), None);
        assert_eq!(hex_decode("0f+0"), None);
        assert_eq!(hex_decode("0g"), None);
        assert_eq!(hex_decode("0"), None);
    }

    #[tokio::test]
    async fn hello_rejects_bad_nonce_unpaired_server_and_bad_host() {
        let srv = start(4).await;
        let host = format!("127.0.0.1:{}", srv.port);
        for target in [
            "/hello",
            "/hello?n=zz",
            "/hello?n=abcd",
            "/hello?n=xyzxyzxyzxyzxyzxyz",
        ] {
            let r = request_to(srv.port, "GET", target, &[("Host", &host)], "").await;
            assert_eq!(status(&r), "400", "{target}: {r}");
        }
        let r = request_to(
            srv.port,
            "GET",
            "/hello?n=00112233445566778899aabbccddeeff",
            &[("Host", "evil.example")],
            "",
        )
        .await;
        assert_eq!(status(&r), "403", "{r}");

        let unpaired = start_with(4, None, Duration::from_secs(5)).await;
        let host = format!("127.0.0.1:{}", unpaired.port);
        let r = request_to(
            unpaired.port,
            "GET",
            "/hello?n=00112233445566778899aabbccddeeff",
            &[("Host", &host)],
            "",
        )
        .await;
        assert_eq!(status(&r), "503", "{r}");
    }

    #[tokio::test]
    async fn unpaired_request_waits_for_confirmation_and_drops_cookies() {
        let _g = exclusive();
        let mut srv = start(4).await;
        let host = format!("127.0.0.1:{}", srv.port);
        let body = r#"{"url":"https://example.com/p","cookies":[{"name":"s","value":"v"}]}"#;
        // Unauthenticated: waits for the user's confirmation.
        let task = tokio::spawn({
            let host = host.clone();
            async move { request_to(srv.port, "POST", "/add", &[("Host", &host), JSON], body).await }
        });
        let handoff = srv.rx.recv().await.expect("handoff");
        assert_eq!(handoff.url, "https://example.com/p");
        assert!(
            handoff.decision.is_some(),
            "unpaired handoff must carry a decision"
        );
        assert!(
            handoff_cookie_args("https://example.com/p").is_empty(),
            "cookies without a valid token must never be staged"
        );
        handoff
            .decision
            .unwrap()
            .send(HandoffOutcome::Queued)
            .unwrap();
        let r = task.await.unwrap();
        assert_eq!(status(&r), "200", "{r}");
    }

    #[tokio::test]
    async fn wrong_or_unverifiable_token_is_refused() {
        let mut srv = start(4).await;
        let host = format!("127.0.0.1:{}", srv.port);
        let tok_other_body = token(r#"{"url":"https://example.com/other"}"#);
        let wrong_secret = hex_encode(
            &Pairing::from_secret([9; SECRET_LEN])
                .mac(CLIENT_CONTEXT, BODY.as_bytes())
                .finalize()
                .into_bytes(),
        );
        for tok in [tok_other_body.as_str(), wrong_secret.as_str(), "zz", ""] {
            let r = request(
                srv.port,
                &[("Host", &host), ("X-Grab-Token", tok), JSON],
                BODY,
            )
            .await;
            assert_eq!(status(&r), "403", "token {tok:?}: {r}");
        }
        // A server with no secret cannot verify anything, so it refuses too.
        let unpaired = start_with(4, None, Duration::from_secs(5)).await;
        let host = format!("127.0.0.1:{}", unpaired.port);
        let tok = token(BODY);
        let r = request(
            unpaired.port,
            &[("Host", &host), ("X-Grab-Token", &tok), JSON],
            BODY,
        )
        .await;
        assert_eq!(status(&r), "403", "{r}");
        assert!(srv.rx.try_recv().is_err(), "nothing may reach the queue");
    }

    #[test]
    fn regenerate_changes_the_secret_and_persists_it() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("grab-pairing-test-{}", std::process::id()));
        let path = dir.join("nested").join("extension-pairing");
        let _ = std::fs::remove_dir_all(&dir);
        let first = Pairing::load_or_create(&path).unwrap();
        let code = first.code();
        assert_eq!(code.len(), SECRET_LEN * 2);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "secret file must be owner-only");
        // Reloading yields the same secret; regenerating a new one, on disk too.
        assert_eq!(Pairing::load_or_create(&path).unwrap().code(), code);
        let fresh = first.regenerate(&path).unwrap();
        assert_ne!(fresh, code);
        assert_eq!(Pairing::load_or_create(&path).unwrap().code(), fresh);
        // Unsafe permissions and malformed content trigger an explicit recovery
        // warning and an atomically replaced 0600 secret, not a silent rotation.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let secured = Pairing::load_or_create(&path).unwrap();
        assert_ne!(secured.code(), fresh);
        assert!(pairing_secret_recovered());
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "unsafe permission bits must not survive recovery"
        );
        let code = secured.code();
        std::fs::write(&path, "not hex").unwrap();
        let recovered = Pairing::load_or_create(&path).unwrap();
        assert_ne!(recovered.code(), code);
        assert!(pairing_secret_recovered());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pairing_fifo_is_recovered_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt as _;

        let dir =
            std::env::temp_dir().join(format!("grab-pairing-fifo-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("extension-pairing");
        let raw_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: raw_path is a NUL-terminated path that stays alive for mkfifo.
        assert_eq!(unsafe { libc::mkfifo(raw_path.as_ptr(), 0o600) }, 0);

        let pairing = Pairing::load_or_create(&path).unwrap();
        assert_eq!(pairing.code().len(), SECRET_LEN * 2);
        assert!(std::fs::metadata(&path).unwrap().is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn legacy_authenticated_handoff_is_confirmed_and_drops_cookies() {
        let mut srv = start(4).await;
        let host = format!("127.0.0.1:{}", srv.port);
        let url = "https://legacy-client.example.test/file.zip";
        let body = r#"{"url":"https://legacy-client.example.test/file.zip","cookies":[{"name":"session","value":"secret"}]}"#;
        let signed = token(body);
        let task = tokio::spawn(async move {
            request_to(
                srv.port,
                "POST",
                "/add",
                &[("Host", &host), ("X-Grab-Token", &signed), JSON],
                body,
            )
            .await
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !task.is_finished(),
            "legacy authenticated requests must still require confirmation"
        );
        let handoff = srv
            .rx
            .recv()
            .await
            .expect("legacy URL should reach the queue");
        assert!(
            handoff.decision.is_some(),
            "legacy calls must remain confirmation-only"
        );
        assert!(
            handoff_cookie_args(url).is_empty(),
            "legacy cookies must not be staged"
        );
        handoff
            .decision
            .expect("legacy request carries a decision")
            .send(HandoffOutcome::Queued)
            .unwrap();
        assert_eq!(status(&task.await.unwrap()), "200");
    }

    #[tokio::test]
    async fn authenticated_handoff_rejects_replayed_body_nonce() {
        let mut srv = start(4).await;
        let host = format!("127.0.0.1:{}", srv.port);
        let mut nonce_bytes = [0u8; 16];
        getrandom::fill(&mut nonce_bytes).expect("operating-system random source");
        let nonce = hex_encode(&nonce_bytes);
        let timestamp_ms = unix_time_ms().expect("system clock");
        let body = format!(
            r#"{{"url":"https://replay.example/file.zip","timestamp_ms":{timestamp_ms},"nonce":"{nonce}","cookies":[]}}"#
        );
        let signed = token(&body);
        let headers = [
            ("Host", host.as_str()),
            ("X-Grab-Token", signed.as_str()),
            JSON,
        ];

        let first = request_to(srv.port, "POST", "/add", &headers, &body).await;
        assert_eq!(status(&first), "200", "{first}");
        let _ = srv.rx.recv().await.expect("first handoff should be queued");

        let replay = request_to(srv.port, "POST", "/add", &headers, &body).await;
        assert_eq!(status(&replay), "403", "{replay}");
        assert!(
            srv.rx.try_recv().is_err(),
            "a replay must not enqueue another handoff"
        );
    }

    #[tokio::test]
    async fn authenticated_nonce_stays_reserved_after_queue_full() {
        let mut srv = start(1).await;
        let host = format!("127.0.0.1:{}", srv.port);

        // Fill the single queue slot with a valid authenticated handoff. It
        // responds immediately, so the assertion below doesn't depend on a
        // sleep or a race with the server task.
        let mut fill_nonce_bytes = [0u8; 16];
        getrandom::fill(&mut fill_nonce_bytes).expect("operating-system random source");
        let fill_nonce = hex_encode(&fill_nonce_bytes);
        let fill_timestamp_ms = unix_time_ms().expect("system clock");
        let fill_body = format!(
            r#"{{"url":"https://queue-fill.example/first","timestamp_ms":{fill_timestamp_ms},"nonce":"{fill_nonce}","cookies":[]}}"#
        );
        let fill_token = token(&fill_body);
        let fill_headers = [
            ("Host", host.as_str()),
            ("X-Grab-Token", fill_token.as_str()),
            JSON,
        ];
        let accepted = request_to(srv.port, "POST", "/add", &fill_headers, &fill_body).await;
        assert_eq!(status(&accepted), "200", "{accepted}");

        let mut nonce_bytes = [0u8; 16];
        getrandom::fill(&mut nonce_bytes).expect("operating-system random source");
        let nonce = hex_encode(&nonce_bytes);
        let timestamp_ms = unix_time_ms().expect("system clock");
        let body = format!(
            r#"{{"url":"https://queue-retry.example/file.zip","timestamp_ms":{timestamp_ms},"nonce":"{nonce}","cookies":[]}}"#
        );
        let signed = token(&body);
        let headers = [
            ("Host", host.as_str()),
            ("X-Grab-Token", signed.as_str()),
            JSON,
        ];

        let refused = request_to(srv.port, "POST", "/add", &headers, &body).await;
        assert_eq!(status(&refused), "429", "{refused}");

        // Free the queue after the signed request has been refused. Retrying
        // the exact captured body must remain a replay, not become a new job.
        let _first = srv.rx.recv().await.expect("queue-filling handoff");
        let replay = request_to(srv.port, "POST", "/add", &headers, &body).await;
        assert_eq!(status(&replay), "403", "{replay}");
        assert!(srv.rx.try_recv().is_err(), "a replay must not enqueue");
    }

    // --- cookie file hardening --------------------------------------------

    fn scoped(name: &str, domain: &str, path: &str) -> Cookie {
        Cookie {
            name: name.to_string(),
            value: "v".to_string(),
            domain: Some(domain.to_string()),
            host_only: Some(true),
            path: Some(path.to_string()),
            secure: None,
            expiration_date: None,
        }
    }

    #[test]
    fn write_handoff_cookies_refuses_field_injection() {
        let url = "https://example.com/";
        for bad in [
            scoped("a", "example.com\nevil.com\tTRUE\t/\tFALSE\t0\tx\ty", "/"),
            scoped("a", "example.com\tTRUE", "/"),
            scoped("a", "example.com", "/p\tq"),
            scoped("a", "example.com", "/p\nq"),
            scoped("a", "example.com", "no-leading-slash"),
            scoped("a", "", "/"),
        ] {
            assert!(write_handoff_cookies(url, &[bad]).is_none());
        }
    }

    #[test]
    fn write_handoff_cookies_refuses_other_domains_keeps_valid_ones() {
        let cookies = vec![
            scoped("good", "example.com", "/"),
            // Another site's cookie, a sibling and a lookalike suffix.
            scoped("evil", "evil.com", "/"),
            scoped("sibling", "other.example.com", "/"),
            scoped("suffix", "ample.com", "/"),
        ];
        let path = write_handoff_cookies("https://www.example.com/x", &cookies).expect("path");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let rows: Vec<&str> = text.lines().skip(1).collect();
        assert_eq!(rows, ["example.com\tFALSE\t/\tFALSE\t0\tgood\tv"], "{text}");
    }

    #[test]
    fn purge_handoff_cookies_deletes_every_staged_file() {
        let _g = exclusive();
        let mut paths = Vec::new();
        for i in 0..3 {
            let url = format!("https://example.com/purge-{i}");
            let path = write_handoff_cookies(&url, &[cookie("c", "v")]).expect("path");
            store_handoff_cookies(&url, path.clone(), None);
            paths.push(path);
        }
        assert!(paths.iter().all(|p| p.exists()));
        purge_handoff_cookies();
        assert!(paths.iter().all(|p| !p.exists()), "files must be deleted");
        assert!(handoff_cookie_args("https://example.com/purge-0").is_empty());
    }

    #[test]
    fn store_handoff_cookies_deletes_the_file_when_the_url_has_no_key() {
        let _g = exclusive();
        let path =
            write_handoff_cookies("https://example.com/k", &[cookie("c", "v")]).expect("path");
        store_handoff_cookies("not a url", path.clone(), None);
        assert!(
            !path.exists(),
            "an unkeyable handoff must not leave its file behind"
        );
    }

    // --- confirm=1 ---------------------------------------------------------

    /// `POST /add?confirm=1` in the background; the test answers the dialog.
    fn add_confirming(port: u16) -> tokio::task::JoinHandle<String> {
        let host = format!("127.0.0.1:{port}");
        tokio::spawn(async move {
            request_to(
                port,
                "POST",
                "/add?confirm=1",
                &[("Host", &host), JSON],
                BODY,
            )
            .await
        })
    }

    #[tokio::test]
    async fn confirm_waits_for_the_answer_and_reports_approval() {
        let mut srv = start(4).await;
        let task = add_confirming(srv.port);
        let handoff = srv.rx.recv().await.expect("handoff");
        assert!(!task.is_finished(), "must wait for the dialog");
        handoff
            .decision
            .expect("confirm handoff carries a decision")
            .send(HandoffOutcome::Queued)
            .unwrap();
        assert_eq!(status(&task.await.unwrap()), "200");
    }

    #[tokio::test]
    async fn confirm_card_opened_returns_accepted_and_keeps_cookies() {
        let _g = exclusive();
        let mut srv = start(4).await;
        let host = format!("127.0.0.1:{}", srv.port);
        let url = "https://example.com/card";
        let body = r#"{"url":"https://example.com/card","cookies":[{"name":"c","value":"v"}]}"#;
        let tok = token(body);
        let (port, h2) = (srv.port, host.clone());
        let task = tokio::spawn(async move {
            request_with_target(
                port,
                "/add?confirm=1",
                &[("Host", &h2), ("X-Grab-Token", &tok), JSON],
                body,
            )
            .await
        });
        let handoff = srv.rx.recv().await.expect("handoff");
        handoff
            .decision
            .expect("confirm handoff carries a decision")
            .send(HandoffOutcome::CardOpened)
            .unwrap();
        assert_eq!(status(&task.await.unwrap()), "202");
        assert_eq!(
            handoff_cookie_args(url).len(),
            2,
            "card-opened must retain staged cookies for the probe/download"
        );
    }

    #[tokio::test]
    async fn confirm_failed_returns_conflict_and_drops_cookies() {
        let _g = exclusive();
        let mut srv = start(4).await;
        let host = format!("127.0.0.1:{}", srv.port);
        let body = r#"{"url":"https://example.com/f","cookies":[{"name":"c","value":"v"}]}"#;
        let tok = token(body);
        let (port, h2) = (srv.port, host.clone());
        let task = tokio::spawn(async move {
            request_with_target(
                port,
                "/add?confirm=1",
                &[("Host", &h2), ("X-Grab-Token", &tok), JSON],
                body,
            )
            .await
        });
        let handoff = srv.rx.recv().await.expect("handoff");
        handoff
            .decision
            .unwrap()
            .send(HandoffOutcome::Failed)
            .unwrap();
        assert_eq!(status(&task.await.unwrap()), "409");
        assert!(
            handoff_cookie_args("https://example.com/f").is_empty(),
            "a failed handoff must not keep its cookies"
        );
    }

    #[tokio::test]
    async fn unauthenticated_handoff_waits_for_confirmation() {
        // No token, no ?confirm=1: must still wait for the user's answer.
        let mut srv = start(4).await;
        let host = format!("127.0.0.1:{}", srv.port);
        let task = tokio::spawn(async move {
            request_to(srv.port, "POST", "/add", &[("Host", &host), JSON], BODY).await
        });
        // Give the server a moment to receive and queue.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !task.is_finished(),
            "unauthenticated handoff must wait for the dialog"
        );
        let handoff = srv.rx.recv().await.expect("handoff");
        assert!(!handoff.authenticated);
        assert!(handoff.expires_at.is_some());
        handoff
            .decision
            .expect("unauthenticated handoff carries a decision")
            .send(HandoffOutcome::Queued)
            .unwrap();
        assert_eq!(status(&task.await.unwrap()), "200");
    }

    #[tokio::test]
    async fn unauthenticated_handoff_decline_returns_conflict() {
        let mut srv = start(4).await;
        let host = format!("127.0.0.1:{}", srv.port);
        let task = tokio::spawn(async move {
            request_to(srv.port, "POST", "/add", &[("Host", &host), JSON], BODY).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let handoff = srv.rx.recv().await.expect("handoff");
        handoff
            .decision
            .expect("unauthenticated handoff carries a decision")
            .send(HandoffOutcome::Declined)
            .unwrap();
        assert_eq!(status(&task.await.unwrap()), "409");
    }

    #[tokio::test]
    async fn confirm_reports_decline_and_drops_cookies() {
        let _g = exclusive();
        let mut srv = start(4).await;
        let host = format!("127.0.0.1:{}", srv.port);
        let body = r#"{"url":"https://example.com/d","cookies":[{"name":"c","value":"v"}]}"#;
        let tok = token(body);
        let (port, h2) = (srv.port, host.clone());
        let task = tokio::spawn(async move {
            request_with_target(
                port,
                "/add?confirm=1",
                &[("Host", &h2), ("X-Grab-Token", &tok), JSON],
                body,
            )
            .await
        });
        let handoff = srv.rx.recv().await.expect("handoff");
        handoff
            .decision
            .unwrap()
            .send(HandoffOutcome::Declined)
            .unwrap();
        assert_eq!(status(&task.await.unwrap()), "409");
        assert!(
            handoff_cookie_args("https://example.com/d").is_empty(),
            "a declined handoff must not keep its cookies"
        );
    }

    #[tokio::test]
    async fn unauthenticated_decline_preserves_staged_entry_but_approval_hides_it() {
        let _g = exclusive();
        let mut srv = start(4).await;
        let host = format!("127.0.0.1:{}", srv.port);
        let url = "https://example.com/keep";
        // Authenticated handoff stages cookies for the URL.
        let body = r#"{"url":"https://example.com/keep","cookies":[{"name":"c","value":"v"}]}"#;
        let tok = token(body);
        let r = request(
            srv.port,
            &[("Host", &host), ("X-Grab-Token", &tok), JSON],
            body,
        )
        .await;
        assert_eq!(status(&r), "200", "{r}");
        let _ = srv.rx.recv().await.expect("handoff");
        let args = handoff_cookie_args(url);
        assert_eq!(args.len(), 2, "entry must be staged");
        let staged = PathBuf::from(&args[1]);

        // Unauthenticated confirm=1 for the same URL, declined. The request
        // must not inherit the earlier paired jar, but declining must leave
        // that jar available to unrelated work.
        let body2 = r#"{"url":"https://example.com/keep"}"#;
        let (port, h2) = (srv.port, host.clone());
        let task = tokio::spawn(async move {
            request_to(
                port,
                "POST",
                "/add?confirm=1",
                &[("Host", &h2), JSON],
                body2,
            )
            .await
        });
        let handoff = srv.rx.recv().await.expect("handoff");
        handoff
            .decision
            .expect("unauthenticated handoff carries a decision")
            .send(HandoffOutcome::Declined)
            .unwrap();
        assert_eq!(status(&task.await.unwrap()), "409");
        assert_eq!(
            handoff_cookie_args(url).len(),
            2,
            "a declined request must not hide the staged jar from other work"
        );
        assert!(
            staged.exists(),
            "declining an unauthenticated handoff must not delete the staged file"
        );

        // Approving the same unpaired handoff does hide the older paired jar
        // from the request it authorized; the file itself is still kept.
        let (port, h2) = (srv.port, host.clone());
        let task = tokio::spawn(async move {
            request_to(
                port,
                "POST",
                "/add?confirm=1",
                &[("Host", &h2), JSON],
                r#"{"url":"https://example.com/keep"}"#,
            )
            .await
        });
        let handoff = srv.rx.recv().await.expect("handoff");
        handoff
            .decision
            .expect("unauthenticated handoff carries a decision")
            .send(HandoffOutcome::Queued)
            .unwrap();
        assert_eq!(status(&task.await.unwrap()), "200");
        assert!(
            handoff_cookie_args(url).is_empty(),
            "an approved unpaired handoff must not reuse a paired jar"
        );
        assert!(staged.exists(), "suppression must not delete the old file");
        clear_handoff_cookies();
    }

    #[tokio::test]
    async fn confirm_treats_an_unanswered_decision_as_declined() {
        let mut srv = start(4).await;
        let task = add_confirming(srv.port);
        drop(srv.rx.recv().await.expect("handoff").decision);
        assert_eq!(status(&task.await.unwrap()), "409");
    }

    #[tokio::test]
    async fn header_read_timeout_closes_an_incomplete_request() {
        let server = start_with_cap_and_timeouts(
            4,
            test_pairing(),
            Duration::from_secs(5),
            MAX_PENDING_CONFIRMATIONS,
            Duration::from_millis(50),
            Duration::from_secs(5),
        )
        .await;
        let mut stream = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, server.port))
            .await
            .unwrap();
        stream
            .write_all(b"GET /hello HTTP/1.1\r\nHost:")
            .await
            .unwrap();
        let mut byte = [0_u8; 1];
        let result = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut byte)).await;
        assert!(
            matches!(result, Ok(Ok(0)) | Ok(Err(_))),
            "server should close an incomplete request after the header deadline"
        );
    }

    #[tokio::test]
    async fn connection_timeout_closes_a_stalled_request_body() {
        let server = start_with_cap_and_timeouts(
            4,
            test_pairing(),
            Duration::from_secs(5),
            MAX_PENDING_CONFIRMATIONS,
            Duration::from_secs(5),
            Duration::from_millis(50),
        )
        .await;
        let host = format!("127.0.0.1:{}", server.port);
        let request = format!(
            "POST /add HTTP/1.1\r\nHost: {host}\r\nOrigin: chrome-extension://abc123\r\nContent-Type: application/json\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{{\"url\":"
        );
        let mut stream = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, server.port))
            .await
            .unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut byte = [0_u8; 1];
        let result = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut byte)).await;
        assert!(
            matches!(result, Ok(Ok(0)) | Ok(Err(_))),
            "server should close a request whose body stalls past the connection deadline"
        );
    }

    #[tokio::test]
    async fn confirm_times_out_when_nobody_answers() {
        let mut srv = start_with(4, test_pairing(), Duration::from_millis(50)).await;
        let task = add_confirming(srv.port);
        let _pending = srv.rx.recv().await.expect("handoff");
        assert_eq!(status(&task.await.unwrap()), "408");
    }

    #[tokio::test]
    async fn confirm_timeout_releases_the_pending_slot() {
        let mut srv = start_with_cap(4, test_pairing(), Duration::from_millis(50), 1).await;
        let first = add_confirming(srv.port);
        let _pending = srv.rx.recv().await.expect("handoff");
        assert_eq!(status(&first.await.unwrap()), "408");
        // The timed-out request's permit must be gone, not leaked.
        let second = add_confirming(srv.port);
        let handoff = srv.rx.recv().await.expect("second handoff");
        handoff
            .decision
            .expect("handoff carries a decision")
            .send(HandoffOutcome::Queued)
            .unwrap();
        assert_eq!(status(&second.await.unwrap()), "200");
    }

    #[tokio::test]
    async fn without_confirm_the_answer_is_immediate() {
        // The fast path still exists, but only for authenticated (paired)
        // callers. Unauthenticated callers always wait (see
        // unauthenticated_handoff_waits_for_confirmation).
        let mut srv = start(4).await;
        let host = format!("127.0.0.1:{}", srv.port);
        let tok = token(BODY);
        let r = request(
            srv.port,
            &[("Host", &host), ("X-Grab-Token", &tok), JSON],
            BODY,
        )
        .await;
        assert_eq!(status(&r), "200", "{r}");
        let handoff = srv.rx.recv().await.expect("handoff");
        assert!(handoff.authenticated);
        assert!(handoff.decision.is_none());
        assert!(
            handoff.expires_at.is_none(),
            "an immediate handoff has no dialog to expire"
        );
    }

    #[test]
    fn handoff_outcome_queued_maps_to_200_no_discard() {
        let (status, discard) = handoff_outcome_response(&HandoffOutcome::Queued);
        assert_eq!(status, axum::http::StatusCode::OK);
        assert!(!discard);
    }

    #[test]
    fn handoff_outcome_card_opened_maps_to_202_no_discard() {
        let (status, discard) = handoff_outcome_response(&HandoffOutcome::CardOpened);
        assert_eq!(status, axum::http::StatusCode::ACCEPTED);
        assert!(!discard);
    }

    #[test]
    fn handoff_outcome_declined_maps_to_409_discard() {
        let (status, discard) = handoff_outcome_response(&HandoffOutcome::Declined);
        assert_eq!(status, axum::http::StatusCode::CONFLICT);
        assert!(discard);
    }

    #[test]
    fn handoff_outcome_failed_maps_to_409_discard() {
        let (status, discard) = handoff_outcome_response(&HandoffOutcome::Failed);
        assert_eq!(status, axum::http::StatusCode::CONFLICT);
        assert!(discard);
    }
}
