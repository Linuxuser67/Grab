//! Intake normalization: URL length cap + routing (magnets/.torrent to torrent engine, rest to http(s)).

use gettextrs::gettext;

/// Max accepted URL length (bytes); bounds queue-file and UI memory.
pub const MAX_URL_LEN: usize = 2048;

/// Normalize user input into a URL string, adding `https://` to bare hosts.
///
/// # Errors
/// Returns a display-ready message when the input is not a usable URL.
pub fn normalize_url(input: &str) -> Result<String, String> {
    // Strip pasted BOM (trim leaves U+FEFF), else magnets misroute to the scheme branch.
    let trimmed = input.trim().trim_start_matches('\u{feff}');
    if crate::torrent::is_magnet(trimmed) {
        // Magnets skip URL parsing and the HTTP cap (parsed locally, never sent); still capped against abuse.
        const MAX_MAGNET_LEN: usize = 16384;
        if trimmed.len() > MAX_MAGNET_LEN {
            return Err(gettext("Magnet link is too long (max {n} characters)")
                .replace("{n}", &MAX_MAGNET_LEN.to_string()));
        }
        // Validated here so the row stores the trimmed link.
        return crate::torrent::parse_magnet(trimmed)
            .map(|_| trimmed.to_string())
            .map_err(|error| error.to_string());
    }
    if crate::torrent::is_torrent_url(trimmed) {
        // .torrent pseudo-URLs skip parsing/cap like magnets; missing archive means stale entry.
        return crate::torrent::archive_path_for_url(trimmed)
            .map(|_| trimmed.to_string())
            .ok_or_else(|| gettext("Torrent file is missing from the archive"));
    }
    if trimmed.len() > MAX_URL_LEN {
        return Err(gettext("URL is too long (max {n} characters)")
            .replace("{n}", &MAX_URL_LEN.to_string()));
    }
    // Explicit scheme is authoritative; bare `host:port` must not take this branch (parses with scheme "localhost").
    if trimmed.contains("://") {
        let u = url::Url::parse(trimmed)
            .map_err(|_| gettext("Invalid URL: {url}").replace("{url}", trimmed))?;
        if !u.username().is_empty() || u.password().is_some() {
            return Err(gettext("URLs with a username/password are not supported"));
        }
        return match u.scheme() {
            "http" | "https" => Ok(u.to_string()),
            "ftp" => Err(gettext("FTP is not supported (use http/https)")),
            s => Err(gettext("Unsupported scheme: {s} (use http/https)").replace("{s}", s)),
        };
    }
    let bare =
        !trimmed.contains(' ') && (trimmed.contains('.') || trimmed.starts_with("localhost"));
    if bare {
        let with_scheme = format!("https://{trimmed}");
        if let Ok(u) = url::Url::parse(&with_scheme)
            && u.username().is_empty()
            && u.password().is_none()
        {
            return Ok(u.to_string());
        }
    }
    Err(gettext("Invalid URL: {url}").replace("{url}", trimmed))
}
