//! Soft intake for browser-supplied caption tracks.
//!
//! Captions are optional metadata: an unreachable, oversized or malformed
//! track is logged and skipped here. A caption must never block or fail the
//! media download it accompanies.

// The intake API is consumed by the later finalization slice; until then the
// whole module is intentionally unreferenced by production code.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt as _;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use sha2::Digest as _;
use tokio::io::AsyncWriteExt as _;

use crate::extension_server::Caption;

/// Upper bound for one subtitle body (matches the extension's HLS inspection cap).
const MAX_CAPTION_BYTES: usize = 256 * 1024;
const CAPTION_FETCH_BUDGET: Duration = Duration::from_secs(10);
/// Same allowlist the `/add` boundary validates; rechecked before sending.
const ALLOWED_CAPTION_HEADERS: [&str; 4] =
    ["referer", "origin", "accept-language", "authorization"];

/// One caption file staged for later FFmpeg finalization (slice A3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StagedCaption {
    pub(crate) path: PathBuf,
    pub(crate) language: String,
    pub(crate) format: String,
}

/// Fetch and stage every usable caption attached to a handoff.
///
/// `staging_dir` is the item's private XDG staging directory (created with
/// owner-only permissions). Failures are soft: they are logged and omitted
/// from the returned list, never surfaced to the media download.
pub(crate) async fn stage_handoff_captions(
    handoff_url: &str,
    captions: &[Caption],
    staging_dir: &Path,
    proxy: Option<&crate::net_types::ResolvedProxy>,
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

    let client = match crate::manifest_intake::client_for_caption_handoff(handoff_url, proxy).await
    {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!(error = %error, "caption client unavailable; continuing without captions");
            return Vec::new();
        }
    };
    let staged = stage_with_client(&client, &accepted, staging_dir).await;
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
    if url.scheme() != "https" || !crate::download_net::is_public_destination_host(&host) {
        return Err("caption URL is not a public HTTPS destination".to_string());
    }
    Ok(host)
}

/// Fetch/stage helper with an injected client so the intake can be tested
/// against a local fixture server without GTK or a real public endpoint.
async fn stage_with_client(
    client: &reqwest::Client,
    captions: &[&Caption],
    staging_dir: &Path,
) -> Vec<StagedCaption> {
    let mut staged = Vec::new();
    if let Err(error) = crate::video_staging::ensure_real_staging_leaf(staging_dir) {
        tracing::warn!(error = %error, "caption staging directory unavailable; continuing without captions");
        return staged;
    }
    for caption in captions {
        match stage_one(client, caption, staging_dir).await {
            Ok(item) => staged.push(item),
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
) -> Result<StagedCaption, String> {
    let bytes = fetch_caption(client, caption).await?;
    let format = caption.format.to_ascii_lowercase();
    if !valid_caption(&bytes, &format) {
        return Err(format!("body is not valid {format} content"));
    }
    let name = caption_file_name(caption, &format);
    stage_atomically(staging_dir, &name, &bytes).await?;
    Ok(StagedCaption {
        path: staging_dir.join(name),
        language: caption.language.clone(),
        format,
    })
}

/// Publish a single staged caption file atomically without depending on the
/// manifest runner (that would introduce an import cycle).
async fn stage_atomically(directory: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    let temporary = directory.join(format!("{name}.part"));
    let target = directory.join(name);
    let _ = tokio::fs::remove_file(&temporary).await;

    let result = async {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .await
            .map_err(|error| format!("Could not create staged caption {name}: {error}"))?;
        file.write_all(bytes)
            .await
            .map_err(|error| format!("Could not write staged caption {name}: {error}"))?;
        file.sync_all()
            .await
            .map_err(|error| format!("Could not flush staged caption {name}: {error}"))?;
        drop(file);

        match tokio::fs::symlink_metadata(&target).await {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(format!("Staged caption {name} is not a regular file"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("Could not inspect staged caption {name}: {error}")),
        }
        tokio::fs::rename(&temporary, &target)
            .await
            .map_err(|error| format!("Could not publish staged caption {name}: {error}"))
    }
    .await;

    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result
}

async fn fetch_caption(client: &reqwest::Client, caption: &Caption) -> Result<Vec<u8>, String> {
    let requested = url::Url::parse(&caption.url).map_err(|error| error.to_string())?;
    let mut headers = HeaderMap::new();
    for (name, value) in &caption.headers {
        if !ALLOWED_CAPTION_HEADERS.contains(&name.to_ascii_lowercase().as_str())
            || value.bytes().any(|byte| byte < 0x20 || byte == 0x7f)
        {
            return Err("caption request header is not allowed".to_string());
        }
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|error| error.to_string())?;
        let value = HeaderValue::from_str(value).map_err(|error| error.to_string())?;
        headers.insert(name, value);
    }

    let response = client
        .get(requested.clone())
        .timeout(CAPTION_FETCH_BUDGET)
        .headers(headers)
        .send()
        .await
        .map_err(|error| error.without_url().to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "caption request returned HTTP {}",
            response.status()
        ));
    }
    // The guarded client already refuses https->http and public->private hops.
    // Recheck the landed URL so an injected client cannot silently bypass it.
    let landed = response.url();
    if landed != &requested
        && (!matches!(landed.scheme(), "https")
            || !landed
                .host_str()
                .is_some_and(crate::download_net::is_public_destination_host))
    {
        return Err("caption redirected away from public HTTPS".to_string());
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
}

fn valid_caption(bytes: &[u8], format: &str) -> bool {
    match format {
        "vtt" => valid_vtt(bytes),
        "srt" => valid_srt(bytes),
        _ => false,
    }
}

fn valid_vtt(bytes: &[u8]) -> bool {
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
    [hours, minutes, seconds, millis]
        .iter()
        .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
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
    use tokio::io::AsyncReadExt as _;
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
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind caption fixture");
        let address = listener.local_addr().expect("fixture address");
        let server = tokio::spawn(async move {
            let mut seen = Vec::new();
            for response in responses {
                let (mut stream, _) =
                    tokio::time::timeout(Duration::from_secs(5), listener.accept())
                        .await
                        .expect("fixture accept")
                        .expect("accept caption request");
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
        (format!("http://{address}/caption"), server)
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

        let staged =
            stage_handoff_captions("https://example.test/watch", &[bad], directory.path(), None)
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
        assert_eq!(staged[0].format, "vtt");
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
