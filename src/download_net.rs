//! Network options + proxy/client plumbing: `DownloadOptions`, proxy resolution, pooled reqwest clients.

use crate::net_types::ResolvedProxy;
use crate::runtime::lock_recover;
use gettextrs::gettext;
use gtk4::gio;
use gtk4::gio::prelude::*;
use std::sync::{Mutex, OnceLock};

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
            cache_key: format!("{url}|{no_proxy_env}"),
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
            cache_key: format!("{url}|{no_proxy_env}"),
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
        cache_key: format!("{cli_url}|{no_proxy_env}"),
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
        cache_key: format!("{cli_url}|{no_proxy_env}"),
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

pub(crate) fn http_client() -> Result<&'static reqwest::Client, String> {
    // Cached for the process lifetime, including failure: backend init
    // (TLS roots) does not heal mid-process, and retrying would re-log the same error.
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| client_builder().build().map_err(|e| e.to_string()))
        .as_ref()
        .map_err(Clone::clone)
}

/// Client honoring this attempt's proxy; proxied configs share one pooled client per config.
pub(crate) fn http_client_for(proxy: Option<&ResolvedProxy>) -> Result<reqwest::Client, String> {
    let Some(proxy) = proxy else {
        return http_client().cloned();
    };
    let cache = PROXIED.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    if let Some(client) = lock_recover(cache).get(&proxy.cache_key) {
        return Ok(client.clone());
    }
    let mut builder = client_builder();
    for p in &proxy.proxies {
        builder = builder.proxy(p.clone());
    }
    let client = builder.build().map_err(|e| e.to_string())?;
    lock_recover(cache).insert(proxy.cache_key.clone(), client.clone());
    Ok(client)
}

/// Read-only view of a cookie jar for reqwest's cookie provider: staged
/// cookies are sent on every request (including redirect hops), but
/// `Set-Cookie` responses are never written back — the shared jar stays a
/// read-only browser snapshot, so a browser logout takes effect at the next
/// TTL refresh.
struct ReadOnlyJar(std::sync::Arc<reqwest::cookie::Jar>);

impl reqwest::cookie::CookieStore for ReadOnlyJar {
    // Deliberate no-op: the jar is a snapshot, not a store.
    fn set_cookies(
        &self,
        _: &mut dyn Iterator<Item = &reqwest::header::HeaderValue>,
        _: &url::Url,
    ) {
    }

    fn cookies(&self, url: &url::Url) -> Option<reqwest::header::HeaderValue> {
        self.0.cookies(url)
    }
}

/// Client applying staged cookies via a provider: reqwest re-applies
/// in-scope cookies on every redirect hop, while a manually-stamped `Cookie`
/// header is stripped when a redirect changes host, port or scheme.
/// Unpooled (one per attempt): cookie-bearing downloads don't share the
/// global client's connection pool.
pub(crate) fn http_client_with_jar(
    proxy: Option<&ResolvedProxy>,
    jar: std::sync::Arc<reqwest::cookie::Jar>,
) -> Result<reqwest::Client, String> {
    let mut b = client_builder().cookie_provider(std::sync::Arc::new(ReadOnlyJar(jar)));
    if let Some(p) = proxy {
        for x in &p.proxies {
            b = b.proxy(x.clone());
        }
    }
    b.build().map_err(|e| e.to_string())
}

static PROXIED: OnceLock<Mutex<std::collections::HashMap<String, reqwest::Client>>> =
    OnceLock::new();

/// Test hook: how many unauthenticated proxied clients are pooled.
#[cfg(test)]
pub(crate) fn proxied_pool_len() -> usize {
    lock_recover(PROXIED.get_or_init(|| Mutex::new(std::collections::HashMap::new()))).len()
}

/// Whether a hostname targets this computer or a private network: literal IPs
/// (v4/v6) and `localhost`/`.local`/`.internal` names only. No DNS is
/// consulted, so a public name resolving to a private address is NOT caught
/// (a resolving check would be TOCTOU-raced against the connect anyway).
pub(crate) fn is_local_or_private_host(host: &str) -> bool {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    fn private_v4(a: Ipv4Addr) -> bool {
        let o = a.octets();
        a.is_loopback()
            || a.is_private()
            || a.is_link_local()
            || a.is_unspecified()
            || o[0] == 0 // 0.0.0.0/8
            || (o[0] == 100 && (o[1] & 0xC0) == 64) // CGNAT 100.64/10
            || (o[0] == 198 && (o[1] & 0xFE) == 18) // benchmarking 198.18/15
            || o[0] >= 240 // reserved 240/4 incl. broadcast
    }
    /// IPv4 tucked into an IPv6 literal: NAT64 `64:ff9b::/96` and the deprecated IPv4-compatible `::a.b.c.d`.
    fn embedded_v4(a: Ipv6Addr) -> Option<Ipv4Addr> {
        let s = a.segments();
        let tail = Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8);
        (s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] || s[..6] == [0; 6]).then_some(tail)
    }
    fn private_v6(a: Ipv6Addr) -> bool {
        a.is_loopback()
            || a.is_unspecified()
            || (a.segments()[0] & 0xFE00) == 0xFC00 // fc00::/7
            || (a.segments()[0] & 0xFFC0) == 0xFE80 // fe80::/10
            || (a.segments()[0] & 0xFFC0) == 0xFEC0 // deprecated site-local fec0::/10
            || embedded_v4(a).is_some_and(private_v4)
    }
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
        Ok(IpAddr::V4(a)) => private_v4(a),
        Ok(IpAddr::V6(a)) => a.to_ipv4_mapped().map_or_else(|| private_v6(a), private_v4),
        // A dotless name ("router", "nas") resolves through the local search domain or hosts file, never the public DNS.
        Err(_) => !h.is_empty() && !h.contains('.') && !h.contains(':'),
    }
}

/// Shared redirect policy: bounded hops, no https->http downgrades.
/// `previous` includes the initial URL, so `> 5` follows exactly 5 redirects
/// (matches reqwest's `Policy::limited(5)` semantics).
fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
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
            attempt.follow()
        }
    })
}

/// Shared builder: bounded hops, no downgrades (see [`http_client`]).
pub(crate) fn client_builder() -> reqwest::ClientBuilder {
    // Bounded hops; refuse https->http downgrades; no ambient proxy (explicit settings or nothing); no automatic Referer (leaks URLs/tokens).
    reqwest::Client::builder()
        .no_proxy()
        .referer(false)
        .redirect(redirect_policy())
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
    // SAFETY: getuid() is async-signal-safe; the unsafe marker is a libc-crate artifact.
    let uid = unsafe { libc::getuid() };
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

/// Whether a proxy resolution means tool installs / update probes must not run:
/// their clients cannot take the app proxy, so a direct fetch would reveal the
/// machine's address to GitHub. A manual proxy that fails to resolve counts as
/// active (fail closed): the user asked for a proxy, never fall back to direct.
pub(crate) fn proxy_blocks_tool_fetch(resolved: &Result<Option<ResolvedProxy>, String>) -> bool {
    match resolved {
        Ok(proxy) => proxy.is_some(),
        Err(_) => true,
    }
}

/// `proxy_blocks_tool_fetch` for the current settings.
pub(crate) fn app_proxy_blocks_tool_install(settings: &crate::settings::AppSettings) -> bool {
    proxy_blocks_tool_fetch(&DownloadOptions::from_settings(settings).proxy_config())
}

/// Client builder for tool installs (ffmpeg, quickjs, yt-dlp): honors env
/// proxies, unlike `client_builder()`. Tool installs are explicit user actions;
/// a user behind a corporate proxy needs the env proxy to reach GitHub.
/// It does NOT apply the proxy configured in Grab (neither does the `yt-dlp`
/// crate's release lookup/fetcher), so installs must be refused while an app
/// proxy is active: see `app_proxy_blocks_tool_install`.
pub(crate) fn tool_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .referer(false)
        .redirect(redirect_policy())
        // A stalled server must not hang the install indefinitely.
        .connect_timeout(std::time::Duration::from_secs(15))
        .read_timeout(std::time::Duration::from_secs(60))
}

#[cfg(test)]
mod tests {
    use super::is_local_or_private_host;

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
            "203.0.113.7",
        ] {
            assert!(!is_local_or_private_host(h), "public: {h}");
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
        for h in [
            "64:ff9b::808:808",
            "198.20.0.1",
            "239.255.255.255",
            "8.8.8.8",
        ] {
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

    #[test]
    fn proxy_blocks_tool_fetch_fails_closed() {
        use super::proxy_blocks_tool_fetch;
        use crate::net_types::ResolvedProxy;
        assert!(!proxy_blocks_tool_fetch(&Ok(None)));
        // A manual proxy that cannot resolve must not degrade to a direct fetch.
        assert!(proxy_blocks_tool_fetch(&Err("bad proxy".to_string())));
        let p = ResolvedProxy {
            proxies: Vec::new(),
            cli_url: "socks5h://127.0.0.1:9050".to_string(),
            no_proxy_env: String::new(),
            cache_key: "k".to_string(),
        };
        assert!(proxy_blocks_tool_fetch(&Ok(Some(p))));
    }
}
