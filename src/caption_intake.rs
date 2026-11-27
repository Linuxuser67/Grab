//! Soft intake for browser-supplied caption tracks.
//!
//! Captions are optional metadata: an unreachable, oversized or malformed
//! track is logged and skipped here. A caption must never block or fail the
//! media download it accompanies.

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt as _;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use sha2::Digest as _;

use crate::extension_server::Caption;

/// Upper bound for one subtitle body (matches the extension's HLS inspection cap).
const MAX_CAPTION_BYTES: usize = 256 * 1024;
const CAPTION_FETCH_BUDGET: Duration = Duration::from_secs(10);
const MAX_CAPTION_REDIRECTS: usize = 5;
/// Same allowlist the `/add` boundary validates; rechecked before sending.
const ALLOWED_CAPTION_HEADERS: [&str; 4] =
    ["referer", "origin", "accept-language", "authorization"];
type CaptionRedirectLeases = Vec<std::sync::Arc<crate::download_net::PublicResolutionLease>>;

/// One caption file staged for later FFmpeg finalization (slice A3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StagedCaption {
    pub(crate) path: PathBuf,
    pub(crate) language: String,
}

/// Fetch and stage the first usable caption attached to a handoff.
///
/// `staging_dir` is the item's private XDG staging directory (created with
/// owner-only permissions). Failures are soft: they are logged and omitted
/// from the returned list, never surfaced to the media download.
pub(crate) async fn stage_handoff_captions(
    handoff_url: &str,
    captions: &[Caption],
    staging_dir: &Path,
    proxy: Option<&crate::net_types::ResolvedProxy>,
    source_referer: Option<&str>,
) -> Vec<StagedCaption> {
    if captions.is_empty() {
        return Vec::new();
    }

    // Pair each accepted caption with a public-only DNS lease. `/add` checked
    // the host string; the connection must still refuse an answer that
    // resolves into private space. Leases stay alive for the whole fetch.
    let mut leases = Vec::new();
    let mut accepted = Vec::new();
    for caption in captions {
        match caption_url_policy(caption).and_then(|host| {
            crate::download_net::acquire_public_resolution(&host)
                .ok_or_else(|| "public-only DNS policy is unavailable".to_string())
                .map(|lease| (host, lease))
        }) {
            Ok((_, lease)) => {
                leases.push(lease);
                accepted.push(caption);
            }
            Err(error) => {
                tracing::warn!(language = %caption.language, error = %error, "skipping caption with an unsafe URL");
            }
        }
    }
    if accepted.is_empty() {
        return Vec::new();
    }

    // A configured upstream proxy resolves the caption host itself and would
    // bypass the public-only DNS lease. Captions are optional, so skip them
    // rather than failing the associated media download.
    if proxy.is_some() {
        tracing::warn!("caption fetch skipped because the download has an upstream proxy");
        return Vec::new();
    }
    if let Err(error) = crate::subprocess_proxy::validate_proxy_compatibility() {
        tracing::warn!(error = %error, "caption fetch skipped because the configured proxy bypasses DNS safeguards");
        return Vec::new();
    }

    let client = match crate::manifest_intake::client_for_caption_handoff(handoff_url, proxy).await
    {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!(error = %error, "caption client unavailable; continuing without captions");
            return Vec::new();
        }
    };
    let staged =
        stage_with_client_and_referer(&client, &accepted, staging_dir, source_referer).await;
    drop(leases);
    staged
}

/// Re-check the URL at intake: a handoff must not be replayed through another
/// path with a relaxed client. Returns the host for the DNS lease.
fn caption_url_policy(caption: &Caption) -> Result<String, String> {
    let url = url::Url::parse(&caption.url).map_err(|error| error.to_string())?;
    let host = url
        .host_str()
        .ok_or_else(|| "caption URL has no host".to_string())?
        .to_owned();
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || host.is_empty()
        || !crate::download_net::is_public_destination_host(&host)
    {
        return Err("caption URL is not a credential-free public HTTPS destination".to_string());
    }
    Ok(host)
}

/// Fetch/stage helper with an injected client so the intake can be tested
/// against a local fixture server without GTK or a real public endpoint.
#[cfg(test)]
async fn stage_with_client(
    client: &reqwest::Client,
    captions: &[&Caption],
    staging_dir: &Path,
) -> Vec<StagedCaption> {
    stage_with_client_and_referer(client, captions, staging_dir, None).await
}

async fn stage_with_client_and_referer(
    client: &reqwest::Client,
    captions: &[&Caption],
    staging_dir: &Path,
    source_referer: Option<&str>,
) -> Vec<StagedCaption> {
    let mut staged = Vec::new();
    if let Err(error) = crate::video_staging::ensure_real_staging_leaf(staging_dir) {
        tracing::warn!(error = %error, "caption staging directory unavailable; continuing without captions");
        return staged;
    }
    for caption in captions {
        match stage_one(client, caption, staging_dir, source_referer).await {
            Ok(item) => {
                // Finalization selects one external track. Stop after the
                // first usable candidate (the browser orders these by match
                // quality) instead of downloading and retaining unused files.
                staged.push(item);
                break;
            }
            Err(error) => tracing::warn!(
                language = %caption.language,
                error = %error,
                "skipping invalid caption"
            ),
        }
    }
    staged
}

async fn stage_one(
    client: &reqwest::Client,
    caption: &Caption,
    staging_dir: &Path,
    source_referer: Option<&str>,
) -> Result<StagedCaption, String> {
    let bytes = fetch_caption_with_referer(client, caption, source_referer).await?;
    let format = caption.format.to_ascii_lowercase();
    if !valid_caption(&bytes, &format) {
        return Err(format!("body is not valid {format} content"));
    }
    let name = caption_file_name(caption, &format);
    crate::staged_file::write_staged_file_atomic(
        staging_dir,
        &format!("{name}.part"),
        &name,
        &bytes,
        crate::staged_file::StagedPublish::Replace,
    )
    .await?;
    Ok(StagedCaption {
        path: staging_dir.join(name),
        language: caption.language.clone(),
    })
}

#[cfg(test)]
async fn fetch_caption(client: &reqwest::Client, caption: &Caption) -> Result<Vec<u8>, String> {
    fetch_caption_with_referer(client, caption, None).await
}

async fn fetch_caption_with_referer(
    client: &reqwest::Client,
    caption: &Caption,
    source_referer: Option<&str>,
) -> Result<Vec<u8>, String> {
    let requested = url::Url::parse(&caption.url).map_err(|error| error.to_string())?;
    // Production callers already passed caption_url_policy and acquired a
    // public-only DNS lease. Keep this helper injectable for loopback fixtures.
    let mut headers = HeaderMap::new();
    for (name, value) in &caption.headers {
        if !ALLOWED_CAPTION_HEADERS.contains(&name.to_ascii_lowercase().as_str())
            || value.bytes().any(|byte| byte < 0x20 || byte == 0x7f)
        {
            return Err("caption request header is not allowed".to_string());
        }
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|error| error.to_string())?;
        let mut value = HeaderValue::from_str(value).map_err(|error| error.to_string())?;
        if matches!(name.as_str(), "authorization" | "origin" | "referer") {
            value.set_sensitive(true);
        }
        headers.insert(name, value);
    }
    if !headers.contains_key(reqwest::header::REFERER)
        && let Some(source) = source_referer
        && let Some(referer) =
            crate::referrer::origin_for_target(Some(crate::referrer::RefererContext {
                source,
                target: requested.as_str(),
            }))
    {
        let mut value = HeaderValue::from_str(&referer).map_err(|error| error.to_string())?;
        value.set_sensitive(true);
        headers.insert(reqwest::header::REFERER, value);
    }

    tokio::time::timeout(CAPTION_FETCH_BUDGET, async {
        let mut redirect_leases = Vec::new();
        let response =
            send_caption_request_with_redirects(client, &requested, headers, &mut redirect_leases)
                .await?;
        if !response.status().is_success() {
            return Err(format!(
                "caption request returned HTTP {}",
                response.status()
            ));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_CAPTION_BYTES as u64)
        {
            return Err("caption body exceeds the size limit".to_string());
        }
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| error.without_url().to_string())?;
            if body.len().saturating_add(chunk.len()) > MAX_CAPTION_BYTES {
                return Err("caption body exceeds the size limit".to_string());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    })
    .await
    .map_err(|_| "caption fetch exceeded its 10-second time limit".to_string())?
}

/// Follow a bounded redirect chain without letting Reqwest replay caption
/// credentials or browser context headers onto a different origin.
async fn send_caption_request_with_redirects(
    client: &reqwest::Client,
    requested: &url::Url,
    mut headers: HeaderMap,
    redirect_leases: &mut CaptionRedirectLeases,
) -> Result<reqwest::Response, String> {
    let mut current = requested.clone();
    for redirect_count in 0..=MAX_CAPTION_REDIRECTS {
        let response = client
            .get(current.clone())
            .headers(headers.clone())
            .send()
            .await
            .map_err(|error| error.without_url().to_string())?;
        if !response.status().is_redirection() {
            return Ok(response);
        }
        if redirect_count == MAX_CAPTION_REDIRECTS {
            return Err("caption exceeded the redirect limit".to_string());
        }
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| "caption redirect had no valid Location header".to_string())?;
        let next = current
            .join(location)
            .map_err(|_| "caption redirect URL was invalid".to_string())?;
        validate_caption_redirect(&current, &next, redirect_leases)?;

        if !same_caption_origin(&current, &next) {
            headers.remove(reqwest::header::AUTHORIZATION);
            headers.remove(reqwest::header::ORIGIN);
            headers.remove(reqwest::header::REFERER);
        }
        current = next;
    }
    Err("caption exceeded the redirect limit".to_string())
}

fn same_caption_origin(left: &url::Url, right: &url::Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

fn validate_caption_redirect(
    current: &url::Url,
    next: &url::Url,
    redirect_leases: &mut CaptionRedirectLeases,
) -> Result<(), String> {
    let host = next
        .host_str()
        .ok_or_else(|| "caption redirect had no host".to_string())?;
    let loopback_fixture = {
        #[cfg(test)]
        {
            let loopback = |url: &url::Url| {
                url.host_str()
                    .and_then(|host| host.parse::<std::net::IpAddr>().ok())
                    .is_some_and(|ip| ip.is_loopback())
            };
            current.scheme() == "http"
                && next.scheme() == "http"
                && loopback(current)
                && loopback(next)
        }
        #[cfg(not(test))]
        {
            false
        }
    };
    if (!loopback_fixture && next.scheme() != "https")
        || !next.username().is_empty()
        || next.password().is_some()
        || (!loopback_fixture && !crate::download_net::is_public_destination_host(host))
    {
        return Err(
            "caption redirect was not a credential-free public HTTPS destination".to_string(),
        );
    }
    if !same_caption_origin(current, next) && !loopback_fixture {
        let lease = crate::download_net::acquire_public_resolution(host)
            .ok_or_else(|| "caption redirect could not acquire a public DNS lease".to_string())?;
        redirect_leases.push(lease);
    }
    Ok(())
}

fn valid_caption(bytes: &[u8], format: &str) -> bool {
    match format {
        "vtt" => valid_vtt(bytes),
        "srt" => valid_srt(bytes),
        _ => false,
    }
}

fn valid_vtt(bytes: &[u8]) -> bool {
    if std::str::from_utf8(bytes).is_err() {
        return false;
    }
    let body = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    let body = body
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .map_or(&[][..], |index| &body[index..]);
    let Some(rest) = body.strip_prefix(b"WEBVTT") else {
        return false;
    };
    rest.first().is_none_or(u8::is_ascii_whitespace)
}

fn valid_srt(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let mut lines = text.trim_start_matches('\u{feff}').lines();
    while let Some(index) = lines.next() {
        if !is_cue_index(index) {
            continue;
        }
        let Some(timing) = lines.next() else {
            return false;
        };
        let Some((start, end)) = timing.split_once("-->") else {
            continue;
        };
        if valid_srt_time(start) && valid_srt_time(end) {
            return true;
        }
    }
    false
}

fn is_cue_index(line: &str) -> bool {
    let line = line.trim();
    !line.is_empty() && line.bytes().all(|byte| byte.is_ascii_digit())
}

fn valid_srt_time(value: &str) -> bool {
    let value = value.trim();
    let Some((hms, millis)) = value.split_once(',') else {
        return false;
    };
    let mut parts = hms.split(':');
    let (Some(hours), Some(minutes), Some(seconds), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    if hours.len() < 2
        || minutes.len() != 2
        || seconds.len() != 2
        || millis.len() != 3
        || [hours, minutes, seconds, millis]
            .iter()
            .any(|part| !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return false;
    }
    let Ok(minutes) = minutes.parse::<u8>() else {
        return false;
    };
    let Ok(seconds) = seconds.parse::<u8>() else {
        return false;
    };
    minutes < 60 && seconds < 60
}

/// Single safe component: language from the validated metadata, a short
/// fingerprint of the full identity, and the format extension.
fn caption_file_name(caption: &Caption, format: &str) -> String {
    let language: String = caption
        .language
        .bytes()
        .filter(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
        .map(char::from)
        .collect();
    let language = if language.is_empty() {
        "und".to_string()
    } else {
        language
    };
    let identity = format!("{}\0{}\0{}", caption.language, caption.url, format);
    let fingerprint: String = sha2::Sha256::digest(identity.as_bytes())
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{language}.{fingerprint}.{format}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    fn caption(url: &str, language: &str, format: &str) -> Caption {
        Caption {
            url: url.to_string(),
            headers: Vec::new(),
            language: language.to_string(),
            format: format.to_string(),
        }
    }

    fn fixture_client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("fixture client")
    }

    fn response(body: &[u8]) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes()
        .into_iter()
        .chain(body.iter().copied())
        .collect()
    }

    /// One GET per response, in order; returns the raw request headers seen.
    async fn fixture_server(responses: Vec<Vec<u8>>) -> (String, JoinHandle<Vec<String>>) {
        fixture_server_at("127.0.0.1:0", "127.0.0.1", responses).await
    }

    async fn fixture_server_at(
        bind_address: &str,
        host: &str,
        responses: Vec<Vec<u8>>,
    ) -> (String, JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind(bind_address)
            .await
            .expect("bind caption fixture");
        let port = listener.local_addr().expect("fixture address").port();
        let server = tokio::spawn(async move {
            let mut seen = Vec::new();
            for (index, response) in responses.into_iter().enumerate() {
                let accept_timeout = if index == 0 {
                    Duration::from_secs(5)
                } else {
                    Duration::from_secs(1)
                };
                let accepted = tokio::time::timeout(accept_timeout, listener.accept()).await;
                let (mut stream, _) = match accepted {
                    Ok(Ok(connection)) => connection,
                    Err(_) if index > 0 => break,
                    Ok(Err(error)) => panic!("accept caption request: {error}"),
                    Err(error) => panic!("fixture accept: {error}"),
                };
                let mut request = Vec::new();
                let mut chunk = [0_u8; 4096];
                loop {
                    let count = stream.read(&mut chunk).await.expect("read caption request");
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..count]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                seen.push(String::from_utf8_lossy(&request).into_owned());
                stream
                    .write_all(&response)
                    .await
                    .expect("write caption response");
            }
            seen
        });
        (format!("http://{host}:{port}/caption"), server)
    }
    #[test]
    fn accepts_vtt_with_bom_and_leading_whitespace() {
        assert!(valid_vtt(
            b"\xef\xbb\xbf  \nWEBVTT\n\n00:00:00.000 --> 00:00:01.000\nHi\n"
        ));
        assert!(valid_vtt(b"WEBVTT"));
    }

    #[test]
    fn rejects_non_webvtt_bodies() {
        assert!(!valid_vtt(b"<html>not a caption</html>"));
        assert!(!valid_vtt(b"WEBVTTX\n"));
        assert!(!valid_vtt(b""));
    }

    #[test]
    fn accepts_srt_with_a_numbered_cue() {
        assert!(valid_srt(
            b"1\r\n00:00:01,000 --> 00:00:02,000\r\nHello\r\n"
        ));
        assert!(valid_srt(
            b"\xef\xbb\xbf12\n01:02:03,456 --> 01:02:04,500\nText\n"
        ));
    }

    #[test]
    fn rejects_srt_without_a_cue_index_or_timestamps() {
        assert!(!valid_srt(b"Just plain text without cues\n"));
        assert!(!valid_srt(b"1\nnot a timestamp\nHello\n"));
        assert!(!valid_srt(b"one\n00:00:01,000 --> 00:00:02,000\n"));
        assert!(!valid_srt(b"1\n00:99:99,000 --> 00:00:02,000\n"));
        assert!(!valid_srt(b"1\n0:00:01,00 --> 00:00:02,000\n"));
        assert!(!valid_srt(b""));
    }

    #[test]
    fn staged_caption_name_is_a_single_safe_component() {
        let name = caption_file_name(
            &caption("https://subs.example.test/en.vtt?sig=x", "en", "vtt"),
            "vtt",
        );
        assert!(name.chars().all(|c| !matches!(c, '/' | '\\' | ':' | '?')));
        assert!(name.starts_with("en.") && name.ends_with(".vtt"));
        assert_eq!(
            name,
            caption_file_name(
                &caption("https://subs.example.test/en.vtt?sig=x", "en", "vtt"),
                "vtt"
            )
        );
        assert_ne!(
            name,
            caption_file_name(
                &caption("https://subs.example.test/fr.vtt?sig=x", "fr", "vtt"),
                "vtt"
            )
        );
    }

    #[test]
    fn caption_url_policy_rejects_non_public_and_non_https_urls() {
        for url in [
            "http://subs.example.test/en.vtt",
            "https://user:pass@subs.example.test/en.vtt",
            "https://127.0.0.1/en.vtt",
            "https://router/en.vtt",
            "file:///etc/passwd",
            "not a url",
        ] {
            assert!(
                caption_url_policy(&caption(url, "en", "vtt")).is_err(),
                "{url} must be refused"
            );
        }
        assert_eq!(
            caption_url_policy(&caption("https://subs.example.test/en.vtt", "en", "vtt"))
                .expect("public caption host"),
            "subs.example.test"
        );
    }

    #[tokio::test]
    async fn unsafe_caption_urls_are_dropped_before_any_fetch() {
        let directory = tempfile::tempdir().expect("staging dir");
        let bad = caption("http://subs.example.test/en.vtt", "en", "vtt");

        let staged = stage_handoff_captions(
            "https://example.test/watch",
            &[bad],
            directory.path(),
            None,
            None,
        )
        .await;

        assert!(staged.is_empty());
        assert_eq!(
            std::fs::read_dir(directory.path())
                .expect("staging listing")
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn valid_vtt_is_staged_atomically() {
        let body = b"WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nHi\n";
        let (url, server) = fixture_server(vec![response(body)]).await;
        let directory = tempfile::tempdir().expect("staging dir");
        let track = caption(&url, "en", "vtt");

        let staged = stage_with_client(&fixture_client(), &[&track], directory.path()).await;

        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].language, "en");
        assert_eq!(staged[0].path.parent(), Some(directory.path()));
        assert_eq!(std::fs::read(&staged[0].path).expect("staged bytes"), body);
        assert_eq!(
            std::fs::read_dir(directory.path())
                .expect("staging listing")
                .count(),
            1,
            "the temporary part file must be published away"
        );
        server.await.expect("fixture server");
    }

    #[tokio::test]
    async fn oversized_body_is_rejected_without_staging() {
        let body = vec![b'x'; MAX_CAPTION_BYTES + 1];
        // Chunked: no declared length, so the streaming bound must catch it.
        let mut raw =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
        raw.extend_from_slice(format!("{:x}\r\n", body.len()).as_bytes());
        raw.extend_from_slice(&body);
        raw.extend_from_slice(b"\r\n0\r\n\r\n");
        let (url, server) = fixture_server(vec![raw]).await;
        let directory = tempfile::tempdir().expect("staging dir");
        let track = caption(&url, "en", "vtt");

        let staged = stage_with_client(&fixture_client(), &[&track], directory.path()).await;

        assert!(staged.is_empty());
        assert_eq!(
            std::fs::read_dir(directory.path())
                .expect("staging listing")
                .count(),
            0
        );
        server.await.expect("fixture server");
    }

    #[tokio::test]
    async fn non_webvtt_body_is_a_soft_failure() {
        let (url, server) = fixture_server(vec![response(b"<html>nope</html>")]).await;
        let directory = tempfile::tempdir().expect("staging dir");
        let track = caption(&url, "en", "vtt");

        let staged = stage_with_client(&fixture_client(), &[&track], directory.path()).await;

        assert!(
            staged.is_empty(),
            "a bad caption must be skipped, not panic"
        );
        server.await.expect("fixture server");
    }

    #[tokio::test]
    async fn one_bad_caption_does_not_block_a_valid_one() {
        let (url, server) = fixture_server(vec![
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            response(b"WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nHi\n"),
        ])
        .await;
        let directory = tempfile::tempdir().expect("staging dir");
        let bad = caption(&format!("{url}/first"), "en", "vtt");
        let good = caption(&format!("{url}/second"), "fr", "vtt");

        let staged = stage_with_client(&fixture_client(), &[&bad, &good], directory.path()).await;

        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].language, "fr");
        server.await.expect("fixture server");
    }

    #[tokio::test]
    async fn stops_after_the_first_usable_caption() {
        let (url, server) =
            fixture_server(vec![response(b"WEBVTT\n"), response(b"WEBVTT\n")]).await;
        let directory = tempfile::tempdir().expect("staging dir");
        let first = caption(&format!("{url}/first"), "en", "vtt");
        let second = caption(&format!("{url}/second"), "fr", "vtt");

        let staged =
            stage_with_client(&fixture_client(), &[&first, &second], directory.path()).await;

        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].language, "en");
        let requests = server.await.expect("fixture server");
        assert_eq!(
            requests.len(),
            1,
            "the first usable caption prevents later candidates from being fetched",
        );
    }

    #[tokio::test]
    async fn source_referer_is_reduced_to_origin_for_caption_requests() {
        let (url, server) = fixture_server(vec![response(b"WEBVTT\n")]).await;
        let directory = tempfile::tempdir().expect("staging dir");
        let track = caption(&url, "en", "vtt");
        let staged = stage_with_client_and_referer(
            &fixture_client(),
            &[&track],
            directory.path(),
            Some("http://source.example.test/watch/private?token=secret#player"),
        )
        .await;

        assert_eq!(staged.len(), 1);
        let requests = server.await.expect("fixture server");
        let request = requests.first().expect("one request").to_ascii_lowercase();
        assert!(
            request.contains("referer: http://source.example.test/"),
            "{request}"
        );
        assert!(!request.contains("token=secret"), "{request}");
        assert!(!request.contains("/watch/private"), "{request}");
    }

    #[tokio::test]
    async fn source_context_and_authorization_headers_are_not_forwarded_across_redirects() {
        let (target_url, target_server) =
            fixture_server_at("0.0.0.0:0", "127.0.0.2", vec![response(b"WEBVTT\n")]).await;
        let redirected_url = target_url;
        let redirect = format!(
            "HTTP/1.1 302 Found\r\nLocation: {redirected_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .into_bytes();
        let (source_url, source_server) = fixture_server(vec![redirect]).await;
        let client = fixture_client();
        let mut track = caption(&source_url, "en", "vtt");
        track.headers = vec![
            (
                "Origin".to_string(),
                "https://player.example.test".to_string(),
            ),
            (
                "Authorization".to_string(),
                "Bearer private-token".to_string(),
            ),
        ];
        let directory = tempfile::tempdir().expect("staging dir");

        let staged = stage_with_client_and_referer(
            &client,
            &[&track],
            directory.path(),
            Some("http://source.example.test/watch/private?token=secret"),
        )
        .await;

        assert_eq!(staged.len(), 1);
        let initial = source_server.await.expect("source request");
        let initial = initial
            .first()
            .expect("initial request")
            .to_ascii_lowercase();
        assert!(
            initial.contains("authorization: bearer private-token"),
            "{initial}"
        );
        assert!(
            initial.contains("origin: https://player.example.test"),
            "{initial}"
        );
        assert!(
            initial.contains("referer: http://source.example.test/"),
            "{initial}"
        );

        let redirected = target_server.await.expect("redirected request");
        let redirected = redirected
            .first()
            .expect("follow-up request")
            .to_ascii_lowercase();
        assert!(!redirected.contains("authorization:"), "{redirected}");
        assert!(!redirected.contains("origin:"), "{redirected}");
        assert!(!redirected.contains("referer:"), "{redirected}");
    }

    #[tokio::test]
    async fn same_origin_redirect_keeps_caption_request_headers() {
        let redirect = b"HTTP/1.1 302 Found\r\nLocation: /caption/final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
        let (url, server) = fixture_server(vec![redirect, response(b"WEBVTT\\n")]).await;
        let client = fixture_client();
        let mut track = caption(&format!("{url}/start"), "en", "vtt");
        track.headers = vec![
            (
                "Origin".to_string(),
                "http://player.example.test".to_string(),
            ),
            (
                "Authorization".to_string(),
                "Bearer same-origin-token".to_string(),
            ),
        ];
        let directory = tempfile::tempdir().expect("staging dir");

        let staged = stage_with_client_and_referer(
            &client,
            &[&track],
            directory.path(),
            Some("http://source.example.test/watch?token=secret"),
        )
        .await;

        assert_eq!(staged.len(), 1);
        let requests = server.await.expect("same-origin redirect requests");
        assert_eq!(requests.len(), 2);
        let second = requests[1].to_ascii_lowercase();
        assert!(
            second.contains("authorization: bearer same-origin-token"),
            "{second}"
        );
        assert!(
            second.contains("origin: http://player.example.test"),
            "{second}"
        );
        assert!(
            second.contains("referer: http://source.example.test/"),
            "{second}"
        );
    }

    #[tokio::test]
    async fn caption_redirect_chain_is_limited_to_five_hops() {
        let redirect = b"HTTP/1.1 302 Found\r\nLocation: /caption\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
        let (url, server) = fixture_server(vec![redirect; MAX_CAPTION_REDIRECTS + 1]).await;
        let client = fixture_client();
        let track = caption(&url, "en", "vtt");

        let result = fetch_caption_with_referer(&client, &track, None).await;

        assert!(
            result.is_err(),
            "six redirects must exceed the five-hop limit"
        );
        let requests = server.await.expect("redirect chain requests");
        assert_eq!(requests.len(), MAX_CAPTION_REDIRECTS + 1);
    }

    #[tokio::test]
    async fn caption_headers_and_staged_cookies_are_propagated() {
        let (url, server) = fixture_server(vec![response(b"WEBVTT\n")]).await;
        let jar = Arc::new(reqwest::cookie::Jar::default());
        jar.add_cookie_str(
            "sid=abc; Path=/",
            &url::Url::parse(&url).expect("fixture URL"),
        );
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .cookie_provider(jar)
            .build()
            .expect("fixture client with jar");
        let mut track = caption(&url, "en", "vtt");
        track.headers = vec![
            (
                "Referer".to_string(),
                "https://example.test/watch".to_string(),
            ),
            ("Authorization".to_string(), "Bearer token".to_string()),
        ];
        let directory = tempfile::tempdir().expect("staging dir");

        let staged = stage_with_client(&client, &[&track], directory.path()).await;

        assert_eq!(staged.len(), 1);
        let requests = server.await.expect("fixture server");
        let request = requests.first().expect("one request").to_ascii_lowercase();
        assert!(request.contains("cookie: sid=abc"), "{request}");
        assert!(
            request.contains("referer: https://example.test/watch"),
            "{request}"
        );
        assert!(request.contains("authorization: bearer token"), "{request}");
    }

    #[test]
    fn disallowed_or_control_caption_headers_are_refused() {
        let mut track = caption("http://127.0.0.1/caption", "en", "vtt");
        track.headers = vec![("X-Evil".to_string(), "value".to_string())];
        let runtime = crate::runtime::tokio_rt();
        let error = runtime
            .block_on(fetch_caption(&fixture_client(), &track))
            .expect_err("unexpected header must be refused");
        assert!(error.contains("not allowed"), "{error}");

        track.headers = vec![("Referer".to_string(), "a\r\nX-Evil: b".to_string())];
        let error = runtime
            .block_on(fetch_caption(&fixture_client(), &track))
            .expect_err("header injection must be refused");
        assert!(error.contains("not allowed"), "{error}");
    }
}
