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
//!
//! Cookies are accepted only on a request with a valid token. An unpaired
//! request still hands off its URL (the confirm dialog protects it), but its
//! cookies are dropped. A wrong token is refused outright.
//!
//! Confirmation: `POST /add?confirm=1` holds the response until the user has
//! answered Grab's dialog (`200` approved, `409` declined, `408` no answer
//! in [`CONFIRM_WAIT`]), so the extension cancels the browser's own download
//! only once Grab has really taken over.

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use hmac::{Hmac, KeyInit as _, Mac};
use serde::Deserialize;
use sha2::Sha256;
use std::collections::{HashMap, VecDeque};
use std::io::Read as _;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, RwLock};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::mpsc::{Sender, error::TrySendError};
use tokio::sync::oneshot;

const PORT: u16 = 9412;
/// A URL plus JSON framing; the intake itself caps URLs at `download_intake::MAX_URL_LEN`.
const MAX_BODY_BYTES: usize = 32 * 1024;
/// Handoffs waiting for the GTK thread; beyond this the extension gets 429.
pub(crate) const QUEUE_DEPTH: usize = 16;
/// How long `/add?confirm=1` waits for the user to answer the dialog. The
/// extension's own timeout must be longer.
pub(crate) const CONFIRM_WAIT: Duration = Duration::from_secs(120);

// --- Pairing ---------------------------------------------------------------

const SECRET_LEN: usize = 32;
const SERVER_CONTEXT: &[u8] = b"grab-server\0";
const CLIENT_CONTEXT: &[u8] = b"grab-client\0";
type HmacSha256 = Hmac<Sha256>;

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

    /// Load the secret at `path`, creating it (0600) when missing or unreadable.
    pub(crate) fn load_or_create(path: &Path) -> std::io::Result<Self> {
        if let Some(secret) = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| decode_secret(t.trim()))
        {
            return Ok(Self::from_secret(secret));
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
        let secret = random_secret()?;
        write_secret(path, &secret)?;
        *self.secret.write().unwrap_or_else(|e| e.into_inner()) = secret;
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
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
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

fn random_secret() -> std::io::Result<[u8; SECRET_LEN]> {
    let mut secret = [0u8; SECRET_LEN];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut secret)?;
    Ok(secret)
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
    std::fs::rename(&tmp, path)
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
}

/// Staged cookies plus the browser UA for one handoff. The UA travels with
/// the cookies because `cf_clearance` is UA-bound: same jar, wrong UA still
/// fails the challenge.
#[derive(Clone, Default)]
struct HandoffCookies {
    path: PathBuf,
    user_agent: Option<String>,
}

/// Cookie files handed off from the extension, keyed by canonical URL. Every
/// yt-dlp spawn for the URL peeks the entry (probe, subtitle probe and media
/// download each spawn separately, and every leg must pass the challenge);
/// the finished download discards it. Bounded so abandoned handoffs (a card
/// opened but never added, a direct file that never probes) can't pile up.
static HANDOFF_COOKIES: LazyLock<Mutex<HandoffStore>> =
    LazyLock::new(|| Mutex::new(HandoffStore::default()));
/// Cap on staged handoff entries; past it the oldest entry is evicted with
/// its file. Matches the handoff queue depth: at most that many handoffs can
/// be in flight ahead of the GTK thread.
const MAX_HANDOFF_ENTRIES: usize = QUEUE_DEPTH;
static COOKIE_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Private 0700 staging dir for handoff cookie files. Duplicates the
/// directory logic from `crate::cookies::cookie_staging_dir` to avoid a
/// module cycle (`cookies` -> `video_spawn` -> `extension_server` -> `cookies`).
/// Uses the same `grab-cookies` directory so the startup sweep in `cookies`
/// cleans up stale handoff files.
fn handoff_staging_dir() -> Option<PathBuf> {
    let base = match std::env::var("XDG_RUNTIME_DIR") {
        Ok(s) if !s.is_empty() => PathBuf::from(s),
        _ => match std::env::var("XDG_CACHE_HOME") {
            Ok(s) if !s.is_empty() => PathBuf::from(s),
            _ => PathBuf::from(std::env::var_os("HOME")?).join(".cache"),
        },
    };
    let dir = base.join("grab-cookies");
    if let Some(parent) = dir.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::os::unix::fs::DirBuilderExt as _;
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return None,
    }
    #[cfg(unix)]
    if !crate::download_net::validate_secure_dir(&dir) {
        return None;
    }
    Some(dir)
}

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

/// A cookie may only be sent for the URL's own host or one of its parents.
fn domain_covers_host(domain: &str, host: &str) -> bool {
    let (domain, host) = (domain.to_ascii_lowercase(), host.to_ascii_lowercase());
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
pub(crate) fn write_handoff_cookies(url: &str, cookies: &[Cookie]) -> Option<PathBuf> {
    if cookies.is_empty() {
        return None;
    }
    // Netscape cookie file format.
    let mut content = String::from("# Netscape HTTP Cookie File\n");
    let fallback_domain = url::Url::parse(url).ok()?.host_str()?.to_string();
    let mut written = 0;
    for cookie in cookies {
        // Sanitize: no tabs/newlines in Netscape format.
        let n = cookie.name.replace(['\t', '\n', '\r'], "");
        let v = cookie.value.replace(['\t', '\n', '\r'], "");
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
    // The private 0700 staging dir (`XDG_RUNTIME_DIR`), the one the startup
    // sweep cleans; no usable dir means no cookies, never a fall back to /tmp.
    let mut path = handoff_staging_dir()?;
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
    file.write_all(content.as_bytes()).ok()?;
    Some(path)
}

/// Store a cookie file for a URL. Called by the extension handoff once the
/// handoff is accepted. Replaces any existing entry (deleting its file);
/// past [`MAX_HANDOFF_ENTRIES`] the oldest entry is evicted with its file.
pub(crate) fn store_handoff_cookies(url: &str, path: PathBuf, user_agent: Option<String>) {
    let Some(key) = handoff_key(url) else {
        let _ = std::fs::remove_file(path);
        return;
    };
    let Ok(mut store) = HANDOFF_COOKIES.lock() else {
        let _ = std::fs::remove_file(path);
        return;
    };
    // Sanitize the UA: it goes into a header and an argv; trim whitespace,
    // drop it if empty, cap length (browsers send < 512 chars).
    let user_agent = user_agent
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(512).collect());
    let entry = HandoffCookies { path, user_agent };
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
    HANDOFF_COOKIES.lock().ok().and_then(|store| {
        store.by_key.get(&key).cloned().or_else(|| {
            let host = url::Url::parse(url).ok()?.host_str()?.to_ascii_lowercase();
            store.by_key.iter().find_map(|(k, v)| {
                let khost = url::Url::parse(k).ok()?.host_str()?.to_ascii_lowercase();
                (khost == host).then(|| v.clone())
            })
        })
    })
}

fn handoff_cookie_file(url: &str) -> Option<PathBuf> {
    handoff_entry(url).map(|e| e.path)
}

/// Browser UA staged with a handoff's cookies, if any. Same lookup as the
/// cookies (exact URL, then same-host fallback).
pub(crate) fn handoff_user_agent(url: &str) -> Option<String> {
    handoff_entry(url).and_then(|e| e.user_agent)
}

/// `--cookies` argv for a URL's staged handoff cookies, if any. Peeks: the
/// entry stays staged until the download finishes (or is evicted), because
/// the probe, the subtitle probe and the media download each spawn yt-dlp
/// separately and every leg must pass the challenge.
pub(crate) fn handoff_cookie_args(url: &str) -> Vec<String> {
    match handoff_cookie_file(url) {
        Some(p) => vec!["--cookies".to_string(), p.to_string_lossy().into_owned()],
        None => Vec::new(),
    }
}

/// Cookie jar for a URL's staged handoff cookies, if any. For reqwest-handled
/// Path of the staged cookie file for `url`, if any. Peeks: the entry stays
/// until the download finishes or is evicted.
pub(crate) fn handoff_cookie_path(url: &str) -> Option<PathBuf> {
    handoff_cookie_file(url)
}

/// Drop a URL's staged handoff cookies and delete the file. Called when its
/// download finishes; failures and aborts keep the entry (bounded) so a retry
/// or resume still passes the challenge.
pub(crate) fn discard_handoff_cookies(url: &str) {
    let path = handoff_key(url).and_then(|key| {
        HANDOFF_COOKIES.lock().ok().and_then(|mut store| {
            store.order.retain(|k| k != &key);
            store.by_key.remove(&key).map(|e| e.path)
        })
    });
    if let Some(p) = path {
        let _ = std::fs::remove_file(p);
    }
}

/// Delete every staged cookie file. Called at shutdown: the entries only live
/// in memory, so files left behind would never be cleaned until the next
/// startup sweep.
pub(crate) fn purge_handoff_cookies() {
    let paths: Vec<PathBuf> = HANDOFF_COOKIES
        .lock()
        .map(|mut store| {
            store.order.clear();
            store.by_key.drain().map(|(_, e)| e.path).collect()
        })
        .unwrap_or_default();
    for p in paths {
        let _ = std::fs::remove_file(p);
    }
}

#[derive(Deserialize)]
struct AddRequest {
    url: String,
    #[serde(default)]
    cookies: Vec<Cookie>,
    /// Browser UA; `cf_clearance` is UA-bound, so it travels with the cookies.
    #[serde(default)]
    user_agent: Option<String>,
}

/// A URL handed off from the extension. Cookies (if any) are stored
/// separately in HANDOFF_COOKIES, keyed by canonical URL, for the video
/// probe and every yt-dlp download leg.
pub(crate) struct Handoff {
    pub url: String,
    /// Set for `/add?confirm=1`: the GTK side answers `true` only once the
    /// download is really queued, and `false` when the user declined, the
    /// enqueue failed, or only the inline card was opened. Dropping it
    /// unanswered counts as declined.
    pub decision: Option<oneshot::Sender<bool>>,
}

#[derive(Clone)]
struct AppState {
    tx: Sender<Handoff>,
    pairing: Option<Arc<Pairing>>,
    confirm_wait: Duration,
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
    let port = match listener.local_addr() {
        Ok(a) => a.port(),
        Err(e) => {
            tracing::warn!("Extension HTTP server has no local address: {e}");
            return;
        }
    };
    let state = AppState {
        tx,
        pairing,
        confirm_wait,
    };
    if let Err(e) = axum::serve(listener, router(state, port)).await {
        tracing::warn!("Extension HTTP server error: {e}");
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

async fn handle_add(
    State(st): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    tracing::debug!("Extension handoff received");
    if !is_json_content_type(&headers) {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE;
    }
    // A token, when sent, must verify: a stale pairing is an error the
    // extension should see, not something to quietly downgrade.
    let authenticated = match headers.get("x-grab-token") {
        None => false,
        Some(token) => {
            let valid = st
                .pairing
                .as_ref()
                .zip(token.to_str().ok())
                .is_some_and(|(p, t)| p.verify_token(&body, t));
            if !valid {
                return StatusCode::FORBIDDEN;
            }
            true
        }
    };
    let req: AddRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) if e.is_data() => return StatusCode::UNPROCESSABLE_ENTITY,
        Err(_) => return StatusCode::BAD_REQUEST,
    };
    if req.url.trim().is_empty() {
        return StatusCode::BAD_REQUEST;
    }
    if req.url.len() > crate::download_intake::MAX_URL_LEN {
        return StatusCode::URI_TOO_LONG;
    }
    // Bound cookie count: each is a small name/value pair.
    if req.cookies.len() > 100 {
        return StatusCode::BAD_REQUEST;
    }
    // Unauthenticated callers always wait for the user's answer: any local
    // process can POST without a token, so silent queuing would let
    // arbitrary software start downloads. The confirm dialog is the
    // authorization for callers without a pairing.
    let wait = q.get("confirm").is_some_and(|v| v == "1") || !authenticated;
    let (decision, answer) = if wait {
        let (tx, rx) = oneshot::channel();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };
    let handoff = Handoff {
        url: req.url.clone(),
        decision,
    };
    // Stage before queuing: the GTK side may start a probe the moment the
    // handoff is received, and every probe leg looks the cookies up by URL.
    let staged = if authenticated && let Some(path) = write_handoff_cookies(&req.url, &req.cookies)
    {
        store_handoff_cookies(&req.url, path, req.user_agent);
        true
    } else {
        false
    };
    match st.tx.try_send(handoff) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            if staged {
                discard_handoff_cookies(&req.url);
            }
            return StatusCode::TOO_MANY_REQUESTS;
        }
        Err(TrySendError::Closed(_)) => {
            if staged {
                discard_handoff_cookies(&req.url);
            }
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
    }
    let Some(answer) = answer else {
        return StatusCode::OK;
    };
    match tokio::time::timeout(st.confirm_wait, answer).await {
        Ok(Ok(true)) => StatusCode::OK,
        outcome => {
            // Declined, dropped unanswered or timed out: the extension keeps
            // the browser download, so the staged cookies have no user.
            discard_handoff_cookies(&req.url);
            if outcome.is_err() {
                StatusCode::REQUEST_TIMEOUT
            } else {
                StatusCode::CONFLICT
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::await_holding_lock)]
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
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = channel(depth);
        tokio::spawn(serve_listener(listener, tx, pairing, wait));
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

    /// `POST /add` on a fresh connection.
    async fn request(port: u16, headers: &[(&str, &str)], body: &str) -> String {
        request_to(port, "POST", "/add", headers, body).await
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
        handoff.decision.expect("decision").send(true).unwrap();
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
            handoff.decision.expect("decision").send(true).unwrap();
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
        handoff.decision.expect("decision").send(true).unwrap();
        assert_eq!(status(&task.await.unwrap()), "200");
        // Queue drained: third request is accepted (and waits for its dialog).
        let task3 = tokio::spawn({
            let host = host.clone();
            async move { request_to(srv.port, "POST", "/add", &[("Host", &host), JSON], BODY).await }
        });
        let handoff = srv.rx.recv().await.expect("handoff");
        handoff.decision.expect("decision").send(true).unwrap();
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
        handoff.decision.unwrap().send(true).unwrap();
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
        // A corrupt file is replaced rather than trusted.
        std::fs::write(&path, "not hex").unwrap();
        assert_eq!(
            Pairing::load_or_create(&path).unwrap().code().len(),
            SECRET_LEN * 2
        );
        let _ = std::fs::remove_dir_all(&dir);
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
            .send(true)
            .unwrap();
        assert_eq!(status(&task.await.unwrap()), "200");
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
        handoff
            .decision
            .expect("unauthenticated handoff carries a decision")
            .send(true)
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
            .send(false)
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
            request_to(
                port,
                "POST",
                "/add?confirm=1",
                &[("Host", &h2), ("X-Grab-Token", &tok), JSON],
                body,
            )
            .await
        });
        let handoff = srv.rx.recv().await.expect("handoff");
        handoff.decision.unwrap().send(false).unwrap();
        assert_eq!(status(&task.await.unwrap()), "409");
        assert!(
            handoff_cookie_args("https://example.com/d").is_empty(),
            "a declined handoff must not keep its cookies"
        );
    }

    #[tokio::test]
    async fn confirm_treats_an_unanswered_decision_as_declined() {
        let mut srv = start(4).await;
        let task = add_confirming(srv.port);
        drop(srv.rx.recv().await.expect("handoff").decision);
        assert_eq!(status(&task.await.unwrap()), "409");
    }

    #[tokio::test]
    async fn confirm_times_out_when_nobody_answers() {
        let mut srv = start_with(4, test_pairing(), Duration::from_millis(50)).await;
        let task = add_confirming(srv.port);
        let _pending = srv.rx.recv().await.expect("handoff");
        assert_eq!(status(&task.await.unwrap()), "408");
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
        assert!(srv.rx.recv().await.expect("handoff").decision.is_none());
    }
}
