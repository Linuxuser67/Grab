//! Network options + proxy/client plumbing: `DownloadOptions`, proxy resolution, per-attempt reqwest clients.

use crate::net_types::ResolvedProxy;
use crate::runtime::lock_recover;
use gettextrs::gettext;
use gtk4::gio;
use gtk4::gio::prelude::*;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Debug, Clone, Default)]
pub struct DownloadOptions {
    pub limit_rate: String,
    /// Parallel range connections for large downloads (1 = single stream).
    pub connections: i32,
    pub proxy_mode: String,
    pub proxy_type: String,
    pub proxy_host: String,
    pub proxy_port: i32,
    /// Raw browser-auth setting (`none` when off); exported to a jar once per attempt.
    pub cookies_browser: String,
}

/// Default UA for plain downloads (fixed; some hosts refuse bot-like UAs).
pub(crate) const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

pub const PROXY_MODE_SYSTEM: &str = "system";
pub const PROXY_MODE_MANUAL: &str = "manual";
pub const PROXY_MODE_DIRECT: &str = "direct";

pub const PROXY_MODE_VALUES: &[&str] = &[PROXY_MODE_SYSTEM, PROXY_MODE_MANUAL, PROXY_MODE_DIRECT];

/// Translated combo labels, index-aligned with [`PROXY_MODE_VALUES`].
pub fn proxy_mode_labels() -> Vec<String> {
    vec![gettext("System"), gettext("Manual"), gettext("Off")]
}

/// Combo index for stored mode; unknown falls back to system.
pub fn proxy_mode_index(value: &str) -> usize {
    crate::media_types::combo_index(PROXY_MODE_VALUES, value, 0)
}

/// Stored value for combo index; out-of-range falls back to system.
pub fn proxy_mode_value(index: usize) -> &'static str {
    crate::media_types::combo_value(PROXY_MODE_VALUES, index, PROXY_MODE_SYSTEM)
}

pub const PROXY_TYPE_VALUES: &[&str] = &["http", "https", "socks5"];

/// Protocol names are left untranslated, like codec labels.
pub fn proxy_type_labels() -> Vec<String> {
    ["HTTP", "HTTPS", "SOCKS5"].map(String::from).to_vec()
}

/// Combo index for stored type; unknown falls back to SOCKS5.
pub fn proxy_type_index(value: &str) -> usize {
    crate::media_types::combo_index(PROXY_TYPE_VALUES, value, 2)
}

/// Stored value for combo index; out-of-range falls back to SOCKS5.
pub fn proxy_type_value(index: usize) -> &'static str {
    crate::media_types::combo_value(PROXY_TYPE_VALUES, index, "socks5")
}

/// Loopback bypass when no ignore list: proxying localhost only breaks local services.
const LOOPBACK_BYPASS: &str = "localhost,127.0.0.1,::1";

/// One ignore entry: exact or subdomain suffix; ports and CIDR out of scope.
fn ignore_entry_normalized(pattern: &str) -> Option<String> {
    let p = pattern.trim().trim_end_matches('.').to_lowercase();
    let p = p.strip_prefix("*.").unwrap_or(&p);
    let p = p.strip_prefix('.').unwrap_or(p);
    // Strip a trailing :port (hyper-util matches NoProxy entries against
    // Uri::host(), which excludes the port — a port-suffixed entry would
    // never match and the bypass would silently fail). IPv6 literals in
    // brackets are left intact; bare IPv6 without brackets is ambiguous
    // and dropped.
    let p = if let Some(bracketed) = p.strip_prefix('[') {
        // [::1] or [::1]:8080 → keep the bracketed host, drop the port.
        let end = bracketed.find(']')?;
        &p[..end + 2] // include the closing bracket
    } else if p.matches(':').count() == 1 {
        // hostname:port or IPv4:port → strip the port.
        match p.rsplit_once(':') {
            Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => host,
            _ => p,
        }
    } else if p.contains(':') {
        return None; // bare IPv6 or malformed — out of scope
    } else {
        p
    };
    if p.is_empty() {
        return None;
    }
    Some(p.to_string())
}

/// Normalize a GNOME ignore-hosts list for reqwest NoProxy; unparseable entries dropped.
pub(crate) fn normalize_no_proxy(patterns: &[String]) -> String {
    patterns
        .iter()
        .filter_map(|p| ignore_entry_normalized(p))
        .collect::<Vec<_>>()
        .join(",")
}

fn system_proxy_settings() -> Option<gio::Settings> {
    let source = gio::SettingsSchemaSource::default()?;
    // Probe first: Settings::new panics on missing schema, so degrade to direct there.
    source.lookup("org.gnome.system.proxy", true)?;
    Some(gio::Settings::new("org.gnome.system.proxy"))
}

/// http + https proxies for one URL with bypass applied; callers differ only on failure surface.
fn http_proxies(url: &str, no_proxy_env: &str) -> Result<Vec<reqwest::Proxy>, reqwest::Error> {
    [reqwest::Proxy::http(url), reqwest::Proxy::https(url)]
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map(|proxies| {
            proxies
                .into_iter()
                .map(|p| p.no_proxy(reqwest::NoProxy::from_string(no_proxy_env)))
                .collect()
        })
}

/// Proxy from desktop settings (manual mode); PAC auto unsupported by design, missing schema degrades.
fn system_proxy() -> Option<ResolvedProxy> {
    use gtk4::gio::prelude::SettingsExt as _;
    let s = system_proxy_settings()?;
    if s.string("mode").as_str() != "manual" {
        return None;
    }
    let ignore: Vec<String> = s
        .strv("ignore-hosts")
        .iter()
        .map(|v| v.to_string())
        .collect();
    let bypassed = normalize_no_proxy(&ignore);
    let no_proxy_env = if bypassed.is_empty() {
        LOOPBACK_BYPASS.to_string()
    } else {
        bypassed
    };
    let no_proxy = reqwest::NoProxy::from_string(&no_proxy_env);
    let host = |key: &str| s.string(key).trim().to_string();
    let port = |key: &str| s.int(key);
    // Host-safe rule as in manual_proxy: dconf free text feeds URL construction, so reject outside host chars.
    let valid = |h: &str, p: i32| {
        !h.is_empty()
            && (1..=65535).contains(&p)
            && h.chars().all(|c| {
                c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']')
            })
    };
    // SOCKS first: one remote-resolving tunnel covers every scheme (also the Tor shape).
    let socks = host("socks-host");
    if valid(&socks, port("socks-port")) {
        let url = format!("socks5h://{}:{}", socks, port("socks-port"));
        let proxy = reqwest::Proxy::all(url.clone()).ok()?.no_proxy(no_proxy);
        return Some(ResolvedProxy {
            proxies: vec![proxy],
            cli_url: url,
            no_proxy_env,
        });
    }
    let http = host("http-host");
    let https = host("https-host");
    if s.boolean("use-same-proxy") && valid(&http, port("http-port")) {
        let url = format!("http://{}:{}", http, port("http-port"));
        let proxies = http_proxies(&url, &no_proxy_env).ok()?;
        return Some(ResolvedProxy {
            proxies,
            cli_url: url,
            no_proxy_env,
        });
    }
    // Split proxies: cover each scheme present; CLI gets the secure leg.
    let mut proxies = Vec::new();
    if valid(&http, port("http-port")) {
        proxies.push(
            reqwest::Proxy::http(format!("http://{}:{}", http, port("http-port")))
                .ok()?
                .no_proxy(reqwest::NoProxy::from_string(&no_proxy_env)),
        );
    }
    if valid(&https, port("https-port")) {
        proxies.push(
            reqwest::Proxy::https(format!("http://{}:{}", https, port("https-port")))
                .ok()?
                .no_proxy(reqwest::NoProxy::from_string(&no_proxy_env)),
        );
    }
    if proxies.is_empty() {
        return None;
    }
    let cli_url = if valid(&https, port("https-port")) {
        format!("http://{}:{}", https, port("https-port"))
    } else {
        format!("http://{}:{}", http, port("http-port"))
    };
    Some(ResolvedProxy {
        proxies,
        cli_url,
        no_proxy_env,
    })
}

/// Manual proxy: explicit demand fails loudly (never leaks direct); unauthenticated (password would travel in cleartext argv).
fn manual_proxy(o: &DownloadOptions) -> Result<Option<ResolvedProxy>, String> {
    let host = o.proxy_host.trim();
    if host.is_empty() {
        return Err(gettext("Proxy host is empty"));
    }
    if !(1..=65535).contains(&o.proxy_port) {
        return Err(gettext("Proxy port is out of range (1–65535)"));
    }
    // Free-text host feeds URL construction: reject anything outside host-safe chars to block userinfo/path smuggling.
    if !host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
    {
        return Err(gettext("Proxy host contains invalid characters"));
    }
    let no_proxy_env = LOOPBACK_BYPASS.to_string();
    let apply_bypass = |p: reqwest::Proxy| p.no_proxy(reqwest::NoProxy::from_string(&no_proxy_env));
    let (proxies, cli_url) = match o.proxy_type.as_str() {
        "http" => {
            let url = format!("http://{host}:{}", o.proxy_port);
            let proxies = http_proxies(&url, &no_proxy_env).map_err(|e| e.to_string())?;
            (proxies, url)
        }
        // HTTPS proxy: TLS to the proxy itself.
        "https" => {
            let url = format!("https://{host}:{}", o.proxy_port);
            let proxies = http_proxies(&url, &no_proxy_env).map_err(|e| e.to_string())?;
            (proxies, url)
        }
        // SOCKS5 always remote-resolving so local DNS doesn't leak hostnames.
        "socks5" => {
            let url = format!("socks5h://{host}:{}", o.proxy_port);
            let proxy = apply_bypass(reqwest::Proxy::all(url.clone()).map_err(|e| e.to_string())?);
            (vec![proxy], url)
        }
        other => {
            return Err(gettext("Unknown proxy type: {t}").replace("{t}", other));
        }
    };
    Ok(Some(ResolvedProxy {
        proxies,
        cli_url,
        no_proxy_env,
    }))
}

impl DownloadOptions {
    /// Snapshot the network-related GSettings keys.
    pub fn from_settings(s: &crate::settings::AppSettings) -> Self {
        Self {
            limit_rate: s.speed_limit(),
            connections: s.connections(),
            proxy_mode: s.proxy_mode(),
            proxy_type: s.proxy_type(),
            proxy_host: s.proxy_host(),
            proxy_port: s.proxy_port(),
            cookies_browser: s.cookies_browser(),
        }
    }

    /// Proxy for this attempt: manual fails loudly, system degrades to direct.
    pub fn proxy_config(&self) -> Result<Option<ResolvedProxy>, String> {
        match self.proxy_mode.as_str() {
            PROXY_MODE_DIRECT => Ok(None),
            PROXY_MODE_MANUAL => manual_proxy(self),
            // System default and unknown values resolve opportunistically.
            _ => Ok(system_proxy()),
        }
    }
}

#[cfg(test)]
pub(crate) fn http_client() -> Result<reqwest::Client, String> {
    client_builder().build().map_err(|e| e.to_string())
}

/// Build one client per download attempt. Connection pooling stays shared within
/// its requests and range workers, but DNS policy does not leak to other rows.
pub(crate) fn http_client_for(proxy: Option<&ResolvedProxy>) -> Result<reqwest::Client, String> {
    let mut builder = client_builder();
    if let Some(proxy) = proxy {
        for p in &proxy.proxies {
            builder = builder.proxy(p.clone());
        }
    }
    builder.build().map_err(|e| e.to_string())
}

/// Read-only view of a cookie jar for reqwest's cookie provider: staged
/// Client applying staged cookies via a provider: reqwest re-applies
/// in-scope cookies on every redirect hop, while a manually-stamped `Cookie`
/// header is stripped when a redirect changes host, port or scheme.
/// Unpooled (one per attempt): cookie-bearing downloads don't share the
/// global client's connection pool. The jar is fresh per attempt (built from
/// the cached export text), so Set-Cookie during the download is accepted —
/// like a browser — without carrying into the next download.
pub(crate) fn http_client_with_jar(
    proxy: Option<&ResolvedProxy>,
    jar: std::sync::Arc<reqwest::cookie::Jar>,
) -> Result<reqwest::Client, String> {
    let mut b = client_builder().cookie_provider(jar);
    if let Some(p) = proxy {
        for x in &p.proxies {
            b = b.proxy(x.clone());
        }
    }
    b.build().map_err(|e| e.to_string())
}

/// Build an attempt-scoped client with browser-bound cookies and the matching user agent.
pub(crate) fn http_client_with_jar_and_user_agent(
    proxy: Option<&ResolvedProxy>,
    jar: std::sync::Arc<reqwest::cookie::Jar>,
    user_agent: &str,
) -> Result<reqwest::Client, String> {
    let mut builder = client_builder().cookie_provider(jar).user_agent(user_agent);
    if let Some(proxy) = proxy {
        for resolved in &proxy.proxies {
            builder = builder.proxy(resolved.clone());
        }
    }
    builder.build().map_err(|error| error.to_string())
}

fn private_v4(a: Ipv4Addr) -> bool {
    let o = a.octets();
    a.is_loopback()
        || a.is_private()
        || a.is_link_local()
        || a.is_unspecified()
        || o[0] == 0 // 0.0.0.0/8
        || (o[0] == 100 && (o[1] & 0xC0) == 64) // CGNAT 100.64/10
        || (o[0] == 198 && (o[1] & 0xFE) == 18) // benchmarking 198.18/15
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // IETF protocol assignments
        || (o[0] == 192 && o[1] == 0 && o[2] == 2) // TEST-NET-1
        || (o[0] == 192 && o[1] == 88 && o[2] == 99) // deprecated 6to4 relay anycast
        || (o[0] == 198 && o[1] == 51 && o[2] == 100) // TEST-NET-2
        || (o[0] == 203 && o[1] == 0 && o[2] == 113) // TEST-NET-3
        || o[0] >= 224 // multicast and reserved
}

/// IPv4 tucked into an IPv6 literal: NAT64 `64:ff9b::/96` and the deprecated IPv4-compatible `::a.b.c.d`.
fn embedded_v4(a: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = a.segments();
    let tail = Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8);
    (s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] || s[..6] == [0; 6]).then_some(tail)
}

fn private_v6(a: Ipv6Addr) -> bool {
    let s = a.segments();
    // NAT64 may carry a global IPv4 address. Apply the IPv4 policy to that
    // embedded destination; deprecated IPv4-compatible ::/96 is never global.
    if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return embedded_v4(a).is_none_or(private_v4);
    }
    if s[..6] == [0; 6] {
        return true;
    }
    a.is_loopback()
        || a.is_unspecified()
        || a.is_multicast()
        // Global unicast space is 2000::/3. Everything outside it is
        // non-global (ULA, link/site-local, unique-use and reserved space).
        || (s[0] & 0xE000) != 0x2000
        || (s[0] == 0x2001 && (s[1] & 0xFE00) == 0x0000) // IETF protocol assignments 2001::/23
        || (s[0] == 0x2001 && s[1] == 0x0DB8) // documentation 2001:db8::/32
        || s[0] == 0x2002 // deprecated 6to4 2002::/16
        || (s[0] == 0x3FFF && (s[1] & 0xF000) == 0x0000) // documentation 3fff::/20
}

/// Whether an address is loopback, private, link-local, CGNAT, reserved or
/// ULA, including IPv4 carried inside IPv6 (mapped, NAT64, IPv4-compatible).
pub(crate) fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => private_v4(a),
        IpAddr::V6(a) => a.to_ipv4_mapped().map_or_else(|| private_v6(a), private_v4),
    }
}

/// Whether a hostname targets this computer or a private network: literal IPs
/// (v4/v6) and `localhost`/`.local`/`.internal` names only. No DNS is
/// consulted here: a public name resolving to a private address is caught at
/// connect time by [`GuardedResolver`] instead (see [`require_public_resolution`]).
pub(crate) fn is_local_or_private_host(host: &str) -> bool {
    let mut h = host
        .trim_matches(|c| c == '[' || c == ']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    // An IPv6 zone id ("fe80::1%eth0") is not part of the syntax Rust parses.
    if let Some((addr, _zone)) = h.split_once('%') {
        h = addr.to_string();
    }
    if h == "localhost"
        || h.ends_with(".localhost")
        || h.ends_with(".local")
        || h.ends_with(".internal")
    {
        return true;
    }
    match h.parse::<IpAddr>() {
        Ok(ip) => is_private_ip(ip),
        // A dotless name ("router", "nas") resolves through the local search domain or hosts file, never the public DNS.
        Err(_) => !h.is_empty() && !h.contains('.') && !h.contains(':'),
    }
}

/// Exact hostnames originating from untrusted browser handoffs, with the
/// number of queue rows that still need the restriction. Entries with no rows
/// (an open card) keep the host registered; the cap fails closed rather than
/// evicting one that a card may still use. Redirects use a separate
/// resolver-only set.
static UNTRUSTED_HANDOFF_HOSTS: OnceLock<Mutex<std::collections::BTreeMap<String, usize>>> =
    OnceLock::new();
const UNTRUSTED_HANDOFF_HOSTS_CAP: usize = 4096;

/// Maximum number of distinct child hosts one untrusted collection may add.
/// Keep this in the shared network-policy layer so the automatic and picker
/// expansion paths apply the same cap.
pub(crate) const MAX_GUARDED_CHILD_HOSTS_PER_COLLECTION: usize = 64;

#[cfg(test)]
static UNTRUSTED_HANDOFF_POLICY_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Serialize tests that temporarily replace the process-global handoff policy.
#[cfg(test)]
pub(crate) fn lock_untrusted_handoff_policy_for_test() -> std::sync::MutexGuard<'static, ()> {
    lock_recover(&UNTRUSTED_HANDOFF_POLICY_TEST_LOCK)
}

/// Restore the process-global handoff policy when a test finishes.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct IsolatedUntrustedHandoffPolicy {
    _lock: std::sync::MutexGuard<'static, ()>,
    previous: std::collections::BTreeMap<String, usize>,
}

#[cfg(test)]
impl Drop for IsolatedUntrustedHandoffPolicy {
    fn drop(&mut self) {
        let cache = UNTRUSTED_HANDOFF_HOSTS.get_or_init(Default::default);
        *lock_recover(cache) = std::mem::take(&mut self.previous);
    }
}

/// Lock and temporarily clear the global handoff policy for a test.
#[cfg(test)]
#[must_use]
pub(crate) fn isolate_untrusted_handoff_policy_for_test() -> IsolatedUntrustedHandoffPolicy {
    let lock = lock_untrusted_handoff_policy_for_test();
    let cache = UNTRUSTED_HANDOFF_HOSTS.get_or_init(Default::default);
    let previous = std::mem::take(&mut *lock_recover(cache));
    IsolatedUntrustedHandoffPolicy {
        _lock: lock,
        previous,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CollectionHostError {
    TooManyDistinctHosts,
    UnsafeDestination,
}

fn host_key(host: &str) -> String {
    host.trim_matches(|c| c == '[' || c == ']')
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

/// Register a hostname or public IP literal for public-only resolution.
/// Private/local destinations are rejected here and must be handled only
/// through the explicit-confirmation path.
pub(crate) fn require_public_resolution(host: &str) -> bool {
    let key = host_key(host);
    if key.is_empty() {
        return false;
    }
    // Private literals/local names are allowed only through the separate
    // explicit-confirmation path. Public IP literals are also recorded: the
    // literal itself cannot rebind, but the extractor may follow links to other
    // hosts and its whole request tree still needs the guarded proxy.
    match key.parse::<IpAddr>() {
        Ok(ip) if is_private_ip(ip) => return false,
        Ok(_) => {}
        Err(_) if is_local_or_private_host(&key) => return false,
        Err(_) => {}
    }
    let mut hosts = lock_recover(UNTRUSTED_HANDOFF_HOSTS.get_or_init(Default::default));
    if policy_matches(&hosts, &key) {
        return true;
    }
    // Preserve existing restrictions at capacity. Fail closed: never evict an
    // entry here, because a zero-reference entry may still belong to an open
    // card that will consult this policy when the user adds its URL.
    if hosts.len() >= UNTRUSTED_HANDOFF_HOSTS_CAP {
        return false;
    }
    hosts.insert(key, 0);
    true
}

/// Associate one queued row with a host's public-only policy. The host must
/// already have been accepted by `require_public_resolution`; retaining it
/// again is harmless and keeps restored rows covered.
pub(crate) fn retain_public_resolution(host: &str) {
    let key = host_key(host);
    if key.is_empty() {
        return;
    }
    *lock_recover(UNTRUSTED_HANDOFF_HOSTS.get_or_init(Default::default))
        .entry(key)
        .or_insert(0) += 1;
}

/// Release a row's reference. The entry stays registered (an open card or a
/// concurrent row may still use it); the host cap fails closed rather than
/// evicting it.
pub(crate) fn release_public_resolution(host: &str) {
    let key = host_key(host);
    if key.is_empty() {
        return;
    }
    let mut hosts = lock_recover(UNTRUSTED_HANDOFF_HOSTS.get_or_init(Default::default));
    if let Some(refs) = hosts.get_mut(&key) {
        *refs = refs.saturating_sub(1);
    }
}

/// Match only explicitly registered names. Canonicalized pages and
/// queued collection children are registered via inherit_public_resolution;
/// treating an entire parent domain as untrusted would also affect unrelated
/// user-added rows on sibling subdomains.
fn policy_matches(hosts: &std::collections::BTreeMap<String, usize>, host: &str) -> bool {
    hosts.contains_key(host)
}

pub(crate) fn is_untrusted_handoff_host(host: &str) -> bool {
    policy_matches(
        &lock_recover(UNTRUSTED_HANDOFF_HOSTS.get_or_init(Default::default)),
        &host_key(host),
    )
}

/// Resolver-only decisions for cross-host redirects live on one reqwest client.
#[derive(Default)]
struct PublicDnsHosts {
    hosts: std::collections::HashSet<String>,
}

type PublicDnsPolicy = Arc<Mutex<PublicDnsHosts>>;
const PUBLIC_DNS_HOSTS_CAP: usize = 4096;

fn public_dns_policy() -> PublicDnsPolicy {
    Arc::new(Mutex::new(PublicDnsHosts::default()))
}

fn require_public_dns_filter(cache: &PublicDnsPolicy, host: &str) -> bool {
    let key = host_key(host);
    if key.is_empty() {
        return false;
    }
    match key.parse::<IpAddr>() {
        Ok(ip) => return !is_private_ip(ip),
        Err(_) if is_local_or_private_host(&key) => return false,
        Err(_) => {}
    }
    let mut cache = lock_recover(cache.as_ref());
    if cache.hosts.contains(&key) {
        return true;
    }
    // Preserve accepted decisions for this client so filtering cannot be lost
    // between redirect approval and DNS lookup. Clients are per-download,
    // making cap exhaustion local rather than poisoning later downloads.
    if cache.hosts.len() >= PUBLIC_DNS_HOSTS_CAP {
        tracing::warn!(host = %key, "refusing redirect host: per-client DNS policy is full");
        return false;
    }
    cache.hosts.insert(key);
    true
}

fn requires_public_dns_filter(host: &str, cache: &PublicDnsPolicy) -> bool {
    let key = host_key(host);
    is_untrusted_handoff_host(&key) || lock_recover(cache.as_ref()).hosts.contains(&key)
}

/// Extend an untrusted collection's public-only policy to a selected or
/// expanded child. A collection-local host set enforces the same per-collection
/// cap in both picker and automatic expansion flows.
pub(crate) fn inherit_public_resolution_for_collection(
    parent_url: &str,
    child_url: &str,
    seen_hosts: &mut std::collections::HashSet<String>,
) -> Result<(), CollectionHostError> {
    let Some(parent) = url::Url::parse(parent_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
    else {
        return Ok(());
    };
    if !is_untrusted_handoff_host(&parent) {
        return Ok(());
    }
    let Some(child) = url::Url::parse(child_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
    else {
        return Err(CollectionHostError::UnsafeDestination);
    };
    let key = host_key(&child);
    if !seen_hosts.contains(&key) && seen_hosts.len() >= MAX_GUARDED_CHILD_HOSTS_PER_COLLECTION {
        return Err(CollectionHostError::TooManyDistinctHosts);
    }
    if !require_public_resolution(&child) {
        return Err(CollectionHostError::UnsafeDestination);
    }
    // Insert only after the global handoff policy has accepted the hostname.
    seen_hosts.insert(key);
    Ok(())
}

/// Single-child convenience path for callers that are not expanding a
/// collection; no collection-local limit applies.
pub(crate) fn inherit_public_resolution(parent_url: &str, child_url: &str) -> bool {
    inherit_public_resolution_for_collection(
        parent_url,
        child_url,
        &mut std::collections::HashSet::new(),
    )
    .is_ok()
}

/// Drop private addresses from a DNS answer; a mixed answer keeps its public part.
fn retain_public(addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    addrs
        .into_iter()
        .filter(|a| !is_private_ip(a.ip()))
        .collect()
}

fn filter_public_addresses(
    host: &str,
    public_only: bool,
    addrs: Vec<SocketAddr>,
) -> Result<Vec<SocketAddr>, String> {
    if !public_only || addrs.is_empty() {
        return Ok(addrs);
    }
    let kept = retain_public(addrs);
    if kept.is_empty() {
        return Err(format!(
            "{host} resolves to a local or private address; refusing to connect"
        ));
    }
    Ok(kept)
}

async fn lookup_filtered(host: &str, public_only: bool) -> Result<Vec<SocketAddr>, String> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, 0))
        .await
        .map_err(|error| error.to_string())?
        .collect();
    filter_public_addresses(host, public_only, addrs)
}

/// DNS resolver used by each client. Filtering the answer that the socket
/// then connects to leaves no check/connect gap for DNS rebinding.
struct GuardedResolver {
    public_dns_hosts: PublicDnsPolicy,
    #[cfg(test)]
    dns_overrides: Arc<std::collections::HashMap<String, Vec<SocketAddr>>>,
}

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        let public_dns_hosts = Arc::clone(&self.public_dns_hosts);
        #[cfg(test)]
        let override_addrs = self.dns_overrides.get(&host_key(&host)).cloned();
        #[cfg(not(test))]
        let override_addrs: Option<Vec<SocketAddr>> = None;

        Box::pin(async move {
            let public_only = requires_public_dns_filter(&host, &public_dns_hosts);
            let addrs = match override_addrs {
                Some(addrs) => filter_public_addresses(&host, public_only, addrs)?,
                None => lookup_filtered(&host, public_only).await?,
            };
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

fn guarded_network_policy() -> (reqwest::redirect::Policy, GuardedResolver) {
    let public_dns_hosts = public_dns_policy();
    (
        redirect_policy(Arc::clone(&public_dns_hosts)),
        GuardedResolver {
            public_dns_hosts,
            #[cfg(test)]
            dns_overrides: Arc::new(std::collections::HashMap::new()),
        },
    )
}

#[cfg(test)]
fn guarded_network_policy_with_dns_overrides(
    overrides: std::collections::HashMap<String, Vec<SocketAddr>>,
) -> (reqwest::redirect::Policy, GuardedResolver) {
    let public_dns_hosts = public_dns_policy();
    let overrides = overrides
        .into_iter()
        .map(|(host, addrs)| (host_key(&host), addrs))
        .collect();
    (
        redirect_policy(Arc::clone(&public_dns_hosts)),
        GuardedResolver {
            public_dns_hosts,
            dns_overrides: Arc::new(overrides),
        },
    )
}

fn client_builder_with_policy(
    redirect: reqwest::redirect::Policy,
    resolver: GuardedResolver,
) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .no_proxy()
        .referer(false)
        .redirect(redirect)
        .dns_resolver(Arc::new(resolver))
}

/// Shared redirect policy: bounded hops, no https->http downgrades.
/// `previous` includes the initial URL, so `> 5` follows exactly 5 redirects
/// (matches reqwest's `Policy::limited(5)` semantics).
fn redirect_policy(public_dns_hosts: PublicDnsPolicy) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        let downgrade = attempt
            .previous()
            .last()
            .is_some_and(|u| u.scheme() == "https")
            && attempt.url().scheme() == "http";
        // A hostile page must not bounce a download into the LAN: refuse a
        // hop from a public host to a local/private one. Private-to-private
        // (LAN CDN) and private-to-public hops stay allowed.
        let to_private = attempt
            .url()
            .host_str()
            .is_some_and(is_local_or_private_host);
        let from_private = attempt
            .previous()
            .last()
            .and_then(|u| u.host_str())
            .is_some_and(is_local_or_private_host);
        if attempt.previous().len() > 5 || downgrade || (to_private && !from_private) {
            attempt.stop()
        } else {
            // A hop to a different host was chosen by the server, not the
            // user: its name must not resolve into local space either.
            if let Some(target) = attempt.url().host_str()
                && attempt.previous().last().and_then(|u| u.host_str()) != Some(target)
                && !to_private
                && !require_public_dns_filter(&public_dns_hosts, target)
            {
                // A changed public-host redirect is untrusted. Stop rather
                // than exceeding this client's bounded policy set; private-to-
                // private LAN redirects remain permitted after an explicit start.
                attempt.stop()
            } else {
                attempt.follow()
            }
        }
    })
}

/// Shared builder: bounded hops, no downgrades.
pub(crate) fn client_builder() -> reqwest::ClientBuilder {
    // Bounded hops; refuse https->http downgrades; no ambient proxy (explicit settings or nothing); no automatic Referer (leaks URLs/tokens).
    let (redirect, resolver) = guarded_network_policy();
    client_builder_with_policy(redirect, resolver)
}

#[cfg(test)]
pub(crate) fn http_client_for_with_dns_overrides_for_test(
    proxy: Option<&ResolvedProxy>,
    overrides: std::collections::HashMap<String, Vec<SocketAddr>>,
) -> Result<reqwest::Client, String> {
    let (redirect, resolver) = guarded_network_policy_with_dns_overrides(overrides);
    let mut builder = client_builder_with_policy(redirect, resolver);
    if let Some(proxy) = proxy {
        for resolved in &proxy.proxies {
            builder = builder.proxy(resolved.clone());
        }
    }
    builder.build().map_err(|error| error.to_string())
}

/// Current Unix UID. libc marks getuid unsafe despite its async-signal-safe semantics;
/// centralize that FFI boundary for filesystem ownership checks.
#[cfg(unix)]
pub(crate) fn current_uid() -> libc::uid_t {
    // SAFETY: getuid has no pointer arguments and is async-signal-safe.
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
pub(crate) fn current_uid() -> u32 {
    0
}

/// Validate a directory is safe for sensitive use: not a symlink, owned by us,
/// not group/world-writable. Shared by cookie staging and tool exec dirs.
#[cfg(unix)]
pub(crate) fn validate_secure_dir(dir: &std::path::Path) -> bool {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let Ok(md) = std::fs::symlink_metadata(dir) else {
        return false;
    };
    if md.file_type().is_symlink() {
        tracing::warn!("secure dir is a symlink; refusing");
        return false;
    }
    let uid = current_uid();
    if md.uid() != uid {
        tracing::warn!("secure dir owned by another user; refusing");
        return false;
    }
    if md.permissions().mode() & 0o022 != 0 {
        tracing::warn!("secure dir is group/world-writable; refusing");
        return false;
    }
    true
}

#[cfg(not(unix))]
pub(crate) fn validate_secure_dir(_dir: &std::path::Path) -> bool {
    true
}

/// Private 0700 staging dir for cookie files: `$XDG_RUNTIME_DIR/grab-cookies`,
/// or `$XDG_CACHE_HOME`/`~/.cache` when unavailable. Shared by the handoff
/// receiver and cookie exports; tightens an existing merely-readable dir so a
/// 0755 leftover cannot expose staged file names.
pub(crate) fn cookie_staging_dir() -> Option<PathBuf> {
    let base = match std::env::var("XDG_RUNTIME_DIR") {
        Ok(s) if !s.is_empty() => PathBuf::from(s),
        _ => match std::env::var("XDG_CACHE_HOME") {
            Ok(s) if !s.is_empty() => PathBuf::from(s),
            _ => PathBuf::from(std::env::var_os("HOME")?).join(".cache"),
        },
    };
    let dir = base.join("grab-cookies");
    // Ensure the parent exists (e.g., ~/.cache may not exist yet).
    if let Some(parent) = dir.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Create 0700 atomically: DirBuilder::mode sets permissions at creation,
    // avoiding a chmod race window.
    use std::os::unix::fs::DirBuilderExt as _;
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => {
            tracing::debug!(error = %e, "cookie staging dir create failed");
            return None;
        }
    }
    if !validate_secure_dir(&dir) {
        return None;
    }
    // Group/world-writable dirs were refused above; this tightens a dir that is
    // merely readable by others (e.g. 0755) down to 0700.
    use std::os::unix::fs::PermissionsExt as _;
    if let Ok(md) = std::fs::metadata(&dir) {
        let mut perms = md.permissions();
        if perms.mode() & 0o077 != 0 {
            perms.set_mode(0o700);
            if let Err(e) = std::fs::set_permissions(&dir, perms) {
                tracing::debug!(error = %e, "cookie staging dir chmod failed");
                return None;
            }
        }
    }
    Some(dir)
}

/// Client builder for tool installs (ffmpeg, Deno, yt-dlp): honors env
/// proxies via reqwest's default. See `tool_client_builder_proxied` for the
/// variant that also honors Grab's app proxy (Preferences → Network).
pub(crate) fn tool_client_builder() -> reqwest::ClientBuilder {
    let (redirect, resolver) = guarded_network_policy();
    reqwest::Client::builder()
        .referer(false)
        .redirect(redirect)
        .dns_resolver(Arc::new(resolver))
        // A stalled server must not hang the install indefinitely.
        .connect_timeout(std::time::Duration::from_secs(15))
        .read_timeout(std::time::Duration::from_secs(60))
}

/// Tool-install client builder honoring Grab's app proxy, not just env
/// proxies. A proxy-configured user's manual install must not go out direct
/// and reveal the machine's address to GitHub. Invalid manual proxy fails
/// loudly (Err) — never silently route direct around an explicit user
/// demand, mirroring the download path's discipline.
pub(crate) fn tool_client_builder_proxied() -> Result<reqwest::ClientBuilder, String> {
    let opts = DownloadOptions::from_settings(&crate::settings::AppSettings::new());
    let proxy = opts.proxy_config()?;
    let mut builder = tool_client_builder();
    if let Some(p) = proxy {
        for pr in &p.proxies {
            builder = builder.proxy(pr.clone());
        }
    }
    Ok(builder)
}

#[cfg(test)]
pub(crate) fn clear_public_resolution_for_tests() {
    lock_recover(UNTRUSTED_HANDOFF_HOSTS.get_or_init(Default::default)).clear();
}

#[cfg(test)]
pub(crate) fn public_resolution_refs(host: &str) -> Option<usize> {
    lock_recover(UNTRUSTED_HANDOFF_HOSTS.get_or_init(Default::default))
        .get(&host_key(host))
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn private_ip_classification_matches_host_check() {
        for a in ["127.0.0.1", "10.1.2.3", "::1", "::ffff:10.0.0.1", "fd00::1"] {
            assert!(is_private_ip(a.parse().unwrap()), "private: {a}");
        }
        for a in ["8.8.8.8", "2606:4700:4700::1111", "::ffff:8.8.8.8"] {
            assert!(!is_private_ip(a.parse().unwrap()), "public: {a}");
        }
    }

    #[test]
    fn retain_public_drops_private_answers_only() {
        let kept = retain_public(vec![sa("127.0.0.1:0"), sa("8.8.8.8:0"), sa("[fd00::1]:0")]);
        assert_eq!(kept, vec![sa("8.8.8.8:0")]);
        assert!(retain_public(vec![sa("192.168.1.5:0")]).is_empty());
    }

    #[test]
    fn public_resolution_marks_only_public_destinations() {
        let _test_lock = lock_untrusted_handoff_policy_for_test();
        let cache = UNTRUSTED_HANDOFF_HOSTS.get_or_init(Default::default);
        let previous = {
            let mut set = lock_recover(cache);
            std::mem::take(&mut *set)
        };
        assert!(require_public_resolution("Marked-Name.EXAMPLE."));
        assert!(is_untrusted_handoff_host("marked-name.example"));
        // Public literals are recorded so extractor subprocesses can guard
        // the wider request tree, even though the literal itself cannot rebind.
        assert!(require_public_resolution("8.8.8.8"));
        assert!(is_untrusted_handoff_host("8.8.8.8"));
        for h in ["127.0.0.1", "[::1]", "localhost", "printer.local", "nas"] {
            assert!(!require_public_resolution(h), "must reject {h}");
            assert!(!is_untrusted_handoff_host(h), "must not mark {h}");
        }
        assert!(!is_untrusted_handoff_host("never-marked.example"));
        *lock_recover(cache) = previous;
    }

    #[test]
    fn redirect_dns_policy_does_not_mark_user_queue_rows_as_untrusted() {
        let policy = public_dns_policy();
        assert!(require_public_dns_filter(&policy, "redirect-only.example"));
        assert!(requires_public_dns_filter("redirect-only.example", &policy));
        assert!(!is_untrusted_handoff_host("redirect-only.example"));
    }

    #[test]
    fn untrusted_policy_is_exact_and_children_are_registered_explicitly() {
        let _test_lock = lock_untrusted_handoff_policy_for_test();
        let cache = UNTRUSTED_HANDOFF_HOSTS.get_or_init(Default::default);
        let previous = {
            let mut set = lock_recover(cache);
            std::mem::take(&mut *set)
        };
        assert!(require_public_resolution("www.example.com"));
        assert!(is_untrusted_handoff_host("www.example.com"));
        assert!(!is_untrusted_handoff_host("example.com"));
        assert!(!is_untrusted_handoff_host("cdn.www.example.com"));
        assert!(inherit_public_resolution(
            "https://www.example.com/list",
            "https://example.com/video.mp4"
        ));
        assert!(is_untrusted_handoff_host("example.com"));
        assert!(!is_untrusted_handoff_host("other-example.com"));
        *lock_recover(cache) = previous;
    }

    #[test]
    fn collection_host_limit_is_shared_and_counts_only_accepted_hosts() {
        let _test_lock = lock_untrusted_handoff_policy_for_test();
        let cache = UNTRUSTED_HANDOFF_HOSTS.get_or_init(Default::default);
        let previous = {
            let mut set = lock_recover(cache);
            std::mem::take(&mut *set)
        };
        assert!(require_public_resolution("collection-limit.example"));
        let mut seen = std::collections::HashSet::new();
        for i in 0..MAX_GUARDED_CHILD_HOSTS_PER_COLLECTION {
            inherit_public_resolution_for_collection(
                "https://collection-limit.example/list",
                &format!("https://child-{i}.example/video"),
                &mut seen,
            )
            .expect("each host below the limit should be accepted");
        }
        assert_eq!(seen.len(), MAX_GUARDED_CHILD_HOSTS_PER_COLLECTION);
        assert_eq!(
            inherit_public_resolution_for_collection(
                "https://collection-limit.example/list",
                "https://too-many.example/video",
                &mut seen,
            ),
            Err(CollectionHostError::TooManyDistinctHosts)
        );
        assert!(!is_untrusted_handoff_host("too-many.example"));
        // Repeated hosts remain allowed after the limit; they do not consume
        // another slot or become unguarded.
        inherit_public_resolution_for_collection(
            "https://collection-limit.example/list",
            "https://child-0.example/another-video",
            &mut seen,
        )
        .expect("an already-counted host should remain accepted");
        assert_eq!(seen.len(), MAX_GUARDED_CHILD_HOSTS_PER_COLLECTION);
        *lock_recover(cache) = previous;
    }

    #[test]
    fn redirect_dns_policy_cap_is_scoped_to_one_client() {
        let first = public_dns_policy();
        for i in 0..PUBLIC_DNS_HOSTS_CAP {
            assert!(require_public_dns_filter(
                &first,
                &format!("redirect-{i}.example")
            ));
        }
        assert!(!require_public_dns_filter(
            &first,
            "redirect-overflow.example"
        ));
        assert!(requires_public_dns_filter("redirect-0.example", &first));

        let next = public_dns_policy();
        assert!(require_public_dns_filter(
            &next,
            "redirect-overflow.example"
        ));
        assert!(!requires_public_dns_filter(
            "redirect-overflow.example",
            &first
        ));
        assert!(requires_public_dns_filter(
            "redirect-overflow.example",
            &next
        ));
    }

    #[test]
    fn untrusted_collection_policy_is_inherited_by_queued_children() {
        let _test_lock = lock_untrusted_handoff_policy_for_test();
        let cache = UNTRUSTED_HANDOFF_HOSTS.get_or_init(Default::default);
        let previous = {
            let mut set = lock_recover(cache);
            std::mem::take(&mut *set)
        };
        assert!(require_public_resolution("collection.example"));
        assert!(inherit_public_resolution(
            "https://collection.example/list",
            "https://cdn.example/video.mp4"
        ));
        assert!(is_untrusted_handoff_host("cdn.example"));
        assert!(!inherit_public_resolution(
            "https://collection.example/list",
            "http://127.0.0.1/admin"
        ));
        *lock_recover(cache) = previous;
    }

    #[test]
    fn public_resolution_cap_fails_closed() {
        let _test_lock = lock_untrusted_handoff_policy_for_test();
        let cache = UNTRUSTED_HANDOFF_HOSTS.get_or_init(Default::default);
        let previous = {
            let mut set = lock_recover(cache);
            std::mem::take(&mut *set)
        };
        for i in 0..UNTRUSTED_HANDOFF_HOSTS_CAP {
            let host = format!("test-{i}.example");
            assert!(require_public_resolution(&host));
            retain_public_resolution(&host);
        }
        assert!(is_untrusted_handoff_host("test-0.example"));
        assert!(!require_public_resolution("overflow.example"));
        assert!(
            is_untrusted_handoff_host("test-0.example"),
            "an active row's policy must survive cap pressure"
        );
        // Releasing a row does not free the entry at cap: a zero-reference
        // entry may still belong to an open card, so eviction would be
        // fail-open. New destinations stay refused until restart.
        release_public_resolution("test-0.example");
        assert!(!require_public_resolution("overflow.example"));
        assert!(is_untrusted_handoff_host("test-0.example"));
        *lock_recover(cache) = previous;
    }

    #[test]
    fn releasing_a_row_keeps_the_host_registered() {
        let _test_lock = lock_untrusted_handoff_policy_for_test();
        let cache = UNTRUSTED_HANDOFF_HOSTS.get_or_init(Default::default);
        let previous = {
            let mut set = lock_recover(cache);
            std::mem::take(&mut *set)
        };
        assert!(require_public_resolution("idle.example"));
        retain_public_resolution("idle.example");
        release_public_resolution("idle.example");
        assert!(is_untrusted_handoff_host("idle.example"));
        // A second release must not wrap or remove the entry.
        release_public_resolution("idle.example");
        assert!(is_untrusted_handoff_host("idle.example"));
        *lock_recover(cache) = previous;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lookup_blocks_private_answer_only_when_required() {
        // `localhost` resolves to loopback via the hosts file: no network needed.
        let err = lookup_filtered("localhost", true).await.unwrap_err();
        assert!(err.contains("refusing to connect"), "{err}");
        let ok = lookup_filtered("localhost", false).await.unwrap();
        assert!(!ok.is_empty() && ok.iter().all(|a| a.ip().is_loopback()));
    }

    #[test]
    fn local_or_private_host_classification() {
        // Loopback and private v4.
        for h in [
            "127.0.0.1",
            "127.1.2.3",
            "10.0.0.5",
            "172.16.4.9",
            "192.168.1.1",
            "169.254.10.20",
            "0.0.0.0",
            "100.64.0.1",  // CGNAT
            "100.127.9.9", // CGNAT
        ] {
            assert!(is_local_or_private_host(h), "private: {h}");
        }
        // Public v4, including CGNAT-adjacent ranges.
        for h in [
            "8.8.8.8",
            "1.1.1.1",
            "100.63.255.255",
            "100.128.0.1",
            "93.184.216.34",
        ] {
            assert!(!is_local_or_private_host(h), "public: {h}");
        }
        for h in [
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.7",
            "224.0.0.1",
            "239.255.255.255",
        ] {
            assert!(is_local_or_private_host(h), "non-global: {h}");
        }
        for h in ["2001:db8::1", "2001:2::1", "2002::1", "3fff::1", "ff02::1"] {
            assert!(is_local_or_private_host(h), "non-global IPv6: {h}");
        }
        // v6: loopback, ULA, link-local, mapped v4.
        for h in [
            "::1",
            "[::1]",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "::ffff:192.168.0.1",
        ] {
            assert!(is_local_or_private_host(h), "private v6: {h}");
        }
        assert!(
            !is_local_or_private_host("2606:4700:4700::1111"),
            "public v6"
        );
        assert!(
            !is_local_or_private_host("::ffff:8.8.8.8"),
            "mapped public v4"
        );
        // Embedded v4 (NAT64, IPv4-compatible), site-local, zone ids, newer v4 ranges.
        for h in [
            "64:ff9b::7f00:1",
            "64:ff9b::a00:1",
            "::127.0.0.1",
            "::7f00:1",
            "::808:808",
            "fec0::1",
            "fe80::1%eth0",
            "198.18.0.1",
            "198.19.255.255",
            "240.0.0.1",
            "255.255.255.255",
            "0.1.2.3",
        ] {
            assert!(is_local_or_private_host(h), "private (extended): {h}");
        }
        for h in ["64:ff9b::808:808", "198.20.0.1", "8.8.8.8"] {
            assert!(!is_local_or_private_host(h), "public (extended): {h}");
        }
        // Dotless names resolve locally; dotted public names do not.
        for h in ["router", "nas", "intranet"] {
            assert!(is_local_or_private_host(h), "dotless: {h}");
        }
        assert!(!is_local_or_private_host("example.com"));
        // Names: localhost family and mDNS/internal suffixes.
        for h in [
            "localhost",
            "LOCALHOST",
            "localhost.",
            "foo.localhost",
            "printer.local",
            "db.internal",
        ] {
            assert!(is_local_or_private_host(h), "local name: {h}");
        }
        // Public names, and lookalikes that must NOT match the suffix rules.
        for h in [
            "example.com",
            "example.local.evil.com",
            "localhost.evil.com",
        ] {
            assert!(!is_local_or_private_host(h), "public name: {h}");
        }
        // No DNS: a public name resolving to a private address is not caught.
        assert!(!is_local_or_private_host("internal.example.com"));
    }

    /// In-memory GSettings for proxy tests; the serial suite makes the
    /// process-global backend safe to mutate.
    fn proxy_test_settings() -> gtk4::gio::Settings {
        // SAFETY: single-threaded setup phase (cargo runs with --test-threads=1).
        unsafe {
            std::env::set_var("GSETTINGS_SCHEMA_DIR", env!("GRAB_SCHEMA_DIR"));
            std::env::set_var("GSETTINGS_BACKEND", "memory");
        }
        gtk4::gio::Settings::new(crate::APP_ID)
    }

    fn set_manual_proxy(s: &gtk4::gio::Settings, host: &str, port: i32) {
        use gtk4::gio::prelude::SettingsExt as _;
        s.set_string("proxy-mode", "manual").unwrap();
        s.set_string("proxy-type", "http").unwrap();
        s.set_string("proxy-host", host).unwrap();
        s.set_int("proxy-port", port).unwrap();
    }

    fn reset_proxy(s: &gtk4::gio::Settings) {
        use gtk4::gio::prelude::SettingsExt as _;
        s.set_string("proxy-mode", "system").unwrap();
    }

    #[test]
    fn tool_client_builder_proxied_fails_closed_on_invalid_manual_proxy() {
        let s = proxy_test_settings();
        set_manual_proxy(&s, "not a host!!", 8080);
        let err = super::tool_client_builder_proxied().unwrap_err();
        assert!(
            err.contains("invalid characters"),
            "must fail loudly, never go direct: {err}"
        );
        reset_proxy(&s);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tool_client_builder_proxied_routes_through_app_proxy() {
        // Fake HTTP proxy: records the absolute-form request line, then
        // answers with a canned body. A direct fetch would fail DNS on
        // example.invalid; reaching the proxy proves the app proxy was applied.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_port = listener.local_addr().unwrap().port();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let seen_clone = seen.clone();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            use tokio::io::AsyncReadExt as _;
            use tokio::io::AsyncWriteExt as _;
            let mut buf = vec![0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
            *seen_clone.lock().unwrap() = request.lines().next().unwrap_or("").to_string();
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\nproxied")
                .await
                .unwrap();
        });

        let s = proxy_test_settings();
        set_manual_proxy(&s, "127.0.0.1", proxy_port as i32);
        let client = super::tool_client_builder_proxied()
            .expect("valid manual proxy")
            .build()
            .expect("client builds");
        let body = client
            .get("http://example.invalid/")
            .send()
            .await
            .expect("proxy answers")
            .text()
            .await
            .expect("body");
        assert_eq!(body, "proxied");
        let request_line = seen.lock().unwrap().clone();
        assert!(
            request_line.starts_with("GET http://example.invalid/"),
            "proxy must see absolute-form URI, got: {request_line}"
        );
        reset_proxy(&s);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn guarded_resolver_rejects_private_dns_answers_before_connecting() {
        use tokio::time::{Duration, timeout};

        let _policy = isolate_untrusted_handoff_policy_for_test();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind resolver test listener");
        let port = listener
            .local_addr()
            .expect("read resolver test port")
            .port();
        let host = "manifest-rebinding.example.test";
        assert!(require_public_resolution(host));

        let client = http_client_for_with_dns_overrides_for_test(
            None,
            std::collections::HashMap::from([(
                host.to_string(),
                vec![format!("127.0.0.1:{port}").parse().expect("loopback socket address")],
            )]),
        )
        .expect("build guarded client with deterministic DNS");

        let result = client
            .get(format!("http://{host}:{port}/master.m3u8"))
            .send()
            .await;
        assert!(result.is_err(), "private DNS answer must be rejected");

        assert!(
            timeout(Duration::from_millis(300), listener.accept())
                .await
                .is_err(),
            "the guarded client must reject the DNS answer before opening a socket"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn redirect_target_dns_is_filtered_by_the_same_client_resolver() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::time::{Duration, timeout};

        let _policy = isolate_untrusted_handoff_policy_for_test();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind redirect test listener");
        let port = listener
            .local_addr()
            .expect("read redirect test port")
            .port();
        let target_host = "redirect-rebinding.example.test";
        let client = http_client_for_with_dns_overrides_for_test(
            None,
            std::collections::HashMap::from([(
                target_host.to_string(),
                vec!["127.0.0.1:0".parse().expect("loopback socket address")],
            )]),
        )
        .expect("build guarded client with deterministic DNS");

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept initial request");
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 1024];
                let read = socket.read(&mut chunk).await.expect("read initial request");
                assert_ne!(read, 0, "initial request ended before its headers");
                request.extend_from_slice(&chunk[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
                assert!(
                    request.len() <= 16 * 1024,
                    "request headers exceeded test limit"
                );
            }
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{target_host}:{port}/private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write redirect response");
            drop(socket);

            timeout(Duration::from_millis(300), listener.accept())
                .await
                .is_err()
        });

        let result = client
            .get(format!("http://127.0.0.1:{port}/start"))
            .send()
            .await;
        assert!(
            result.is_err(),
            "redirect target resolving to loopback must be rejected"
        );
        assert!(
            server.await.expect("redirect fixture task"),
            "the redirected request must be rejected before a second socket is opened"
        );
    }

}
