//! Bounded HTTP response-header detection for direct resources with opaque URLs.
//!
//! This is an intake probe only: it requests one byte to coax headers from
//! servers that reject HEAD, then drops the response without reading its body.

use std::time::Duration;

use reqwest::header::{CONTENT_DISPOSITION, CONTENT_TYPE, HeaderMap, RANGE};

const DIRECT_RESOURCE_PROBE_BUDGET: Duration = Duration::from_secs(3);
const DIRECT_RESOURCE_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// Return true when the server response is strong evidence of a downloadable
/// resource rather than a web page or a stream manifest.
pub(crate) async fn probe(
    raw_url: String,
    proxy: Option<crate::net_types::ResolvedProxy>,
    referer: Option<String>,
) -> bool {
    let host = crate::video_probe::page_host(&raw_url);
    let Ok(parsed_url) = url::Url::parse(&raw_url) else {
        return false;
    };
    if !matches!(parsed_url.scheme(), "http" | "https")
        || parsed_url.host_str().is_none()
        || !parsed_url.username().is_empty()
        || parsed_url.password().is_some()
    {
        return false;
    }

    let result = tokio::time::timeout(DIRECT_RESOURCE_PROBE_BUDGET, async {
        let client = crate::manifest_intake::client_for_direct_resource_probe(
            &raw_url,
            proxy.as_ref(),
            referer.as_deref(),
        )
        .await?;

        inspect_response_headers(&client, &raw_url).await
    })
    .await;

    match result {
        Ok(Ok(Some(content_type))) => {
            tracing::info!(
                url_host = %host,
                content_type = %content_type,
                "opaque URL resolved to a direct resource"
            );
            true
        }
        Ok(Ok(None)) => false,
        Ok(Err(error)) => {
            tracing::debug!(
                url_host = %host,
                error = %error,
                "direct-resource preflight did not classify URL"
            );
            false
        }
        Err(_) => {
            tracing::debug!(
                url_host = %host,
                budget_ms = DIRECT_RESOURCE_PROBE_BUDGET.as_millis() as u64,
                "direct-resource preflight timed out"
            );
            false
        }
    }
}

/// Send a bounded range request and classify only the response headers. The
/// body is intentionally never consumed, including when a server ignores Range.
async fn inspect_response_headers(
    client: &reqwest::Client,
    raw_url: &str,
) -> Result<Option<String>, String> {
    let response = client
        .get(raw_url)
        .header(RANGE, "bytes=0-0")
        .timeout(DIRECT_RESOURCE_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|error| error.without_url().to_string())?;

    if !response.status().is_success() {
        return Ok(None);
    }

    let final_url = response.url();
    if !matches!(final_url.scheme(), "http" | "https")
        || final_url.host_str().is_none()
        || !final_url.username().is_empty()
        || final_url.password().is_some()
        || crate::manifest_intake::supports(final_url.as_str())
        || crate::video::is_stream_manifest_url(final_url.as_str())
    {
        return Ok(None);
    }

    let headers = response.headers();
    if !is_direct_resource_headers(headers) {
        return Ok(None);
    }

    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map_or_else(
            || "content-disposition".to_string(),
            |value| {
                value
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
            },
        );

    // Dropping the response here closes the unused body stream; no media bytes
    // are downloaded or buffered by the preflight.
    Ok(Some(content_type))
}

fn is_direct_resource_headers(headers: &HeaderMap) -> bool {
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    let content_disposition = headers
        .get(CONTENT_DISPOSITION)
        .and_then(|value| value.to_str().ok());
    is_direct_resource_metadata(content_type, content_disposition)
}

fn is_direct_resource_metadata(
    content_type: Option<&str>,
    content_disposition: Option<&str>,
) -> bool {
    let mime = content_type
        .and_then(|value| value.split(';').next())
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();

    // Never turn an opaque manifest endpoint into a plain file download.
    if matches!(
        mime.as_str(),
        "application/vnd.apple.mpegurl"
            | "application/mpegurl"
            | "application/x-mpegurl"
            | "audio/mpegurl"
            | "audio/x-mpegurl"
            | "video/mpegurl"
            | "video/vnd.mpegurl"
            | "video/x-mpegurl"
            | "application/dash+xml"
    ) {
        return false;
    }

    if content_disposition.is_some_and(has_download_disposition) {
        return true;
    }

    if mime.starts_with("audio/")
        || mime.starts_with("video/")
        || mime.starts_with("image/")
        || mime.starts_with("font/")
    {
        return true;
    }

    matches!(
        mime.as_str(),
        "application/octet-stream"
            | "application/pdf"
            | "application/zip"
            | "application/x-zip-compressed"
            | "application/x-7z-compressed"
            | "application/vnd.rar"
            | "application/x-rar-compressed"
            | "application/gzip"
            | "application/x-gzip"
            | "application/x-tar"
            | "application/x-bzip2"
            | "application/x-xz"
            | "application/msword"
            | "application/rtf"
            | "application/wasm"
            | "application/epub+zip"
            | "application/vnd.ms-excel"
            | "application/vnd.ms-powerpoint"
            | "application/vnd.ms-fontobject"
            | "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            | "application/vnd.openxmlformats-officedocument.presentationml.presentation"
            | "application/vnd.oasis.opendocument.text"
            | "application/vnd.oasis.opendocument.spreadsheet"
            | "application/vnd.oasis.opendocument.presentation"
    )
}

fn has_download_disposition(value: &str) -> bool {
    value
        .split(';')
        .next()
        .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("attachment"))
        || crate::download_fetch::filename_from_content_disposition(value).is_some()
}

#[cfg(test)]
mod tests {
    use super::{has_download_disposition, is_direct_resource_metadata};

    #[test]
    fn identifies_common_media_and_file_mime_types() {
        for mime in [
            "video/mp4",
            "audio/mpeg",
            "image/jpeg",
            "font/woff2",
            "application/octet-stream",
            "application/pdf",
            "application/zip",
            "application/x-7z-compressed",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        ] {
            assert!(
                is_direct_resource_metadata(Some(mime), None),
                "{mime} should be a direct resource"
            );
        }
    }

    #[test]
    fn does_not_classify_pages_or_structured_api_responses_as_files() {
        for mime in [
            "text/html",
            "application/xhtml+xml",
            "application/json",
            "application/problem+json",
            "application/xml",
            "text/plain",
            "application/vnd.api+json",
        ] {
            assert!(
                !is_direct_resource_metadata(Some(mime), None),
                "{mime} should remain on extractor routing"
            );
        }
    }

    #[test]
    fn never_classifies_hls_or_dash_manifest_mime_types_as_files() {
        for mime in [
            "application/vnd.apple.mpegurl",
            "application/mpegurl",
            "application/x-mpegurl",
            "audio/mpegurl",
            "audio/x-mpegurl",
            "video/mpegurl",
            "video/vnd.mpegurl",
            "video/x-mpegurl",
            "application/dash+xml",
        ] {
            assert!(
                !is_direct_resource_metadata(Some(mime), None),
                "{mime} is a manifest, not a plain file"
            );
            assert!(
                !is_direct_resource_metadata(
                    Some(mime),
                    Some("attachment; filename=\"opaque.bin\"")
                ),
                "{mime} must not become a file even with Content-Disposition"
            );
        }
    }

    #[tokio::test]
    async fn opaque_probe_sends_a_single_byte_range_and_does_not_read_the_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind local fixture");
        let address = listener.local_addr().expect("fixture address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept probe");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            loop {
                let count = stream.read(&mut chunk).await.expect("read request");
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&request);
            assert!(
                request
                    .lines()
                    .next()
                    .is_some_and(|line| line.starts_with("GET /opaque/resource ")),
                "the preflight must use GET for the opaque URL: {request}"
            );
            assert!(
                request
                    .lines()
                    .any(|line| line.eq_ignore_ascii_case("range: bytes=0-0")),
                "the preflight must request only the first byte: {request}"
            );

            stream
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\nContent-Type: application/octet-stream\r\nContent-Range: bytes 0-0/2000000\r\nContent-Length: 2000000\r\nConnection: close\r\n\r\nx",
                )
                .await
                .expect("write response headers");
        });

        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("build fixture client");
        let url = format!("http://{address}/opaque/resource");
        let result = super::inspect_response_headers(&client, &url)
            .await
            .expect("inspect response headers");

        assert_eq!(result.as_deref(), Some("application/octet-stream"));
        server.await.expect("fixture server task");
    }

    #[test]
    fn uses_content_disposition_when_mime_is_missing_or_generic() {
        assert!(has_download_disposition(
            "attachment; filename=\"archive.data\""
        ));
        assert!(has_download_disposition(
            "inline; filename*=UTF-8''archive.data"
        ));
        assert!(is_direct_resource_metadata(None, Some("attachment")));
        assert!(is_direct_resource_metadata(
            Some("application/octet-stream"),
            Some("inline; filename=\"archive.data\"")
        ));
        assert!(!is_direct_resource_metadata(Some("text/html"), None));
    }
}
