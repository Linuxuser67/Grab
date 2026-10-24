//! Resolved proxy shared by HTTP, yt-dlp spawns, and torrent sessions.

/// Proxy for one attempt: reqwest interceptors plus CLI form for yt-dlp spawns.
#[derive(Clone, Debug)]
pub(crate) struct ResolvedProxy {
    pub(crate) proxies: Vec<reqwest::Proxy>,
    /// Single URL for `--proxy` (SOCKS always remote-resolving).
    pub cli_url: String,
    /// Comma list for NO_PROXY env on yt-dlp spawns.
    pub no_proxy_env: String,
}

impl ResolvedProxy {
    /// SOCKS5 URL for torrents: `socks5h://` normalizes to `socks5://` (librqbit demands it); peers arrive as IPs so nothing leaks, HTTP(S) yields `None` (no CONNECT path, stays direct).
    pub fn torrent_socks_url(&self) -> Option<String> {
        let rest = self
            .cli_url
            .strip_prefix("socks5h://")
            .or_else(|| self.cli_url.strip_prefix("socks5://"))?;
        Some(format!("socks5://{rest}"))
    }
}
