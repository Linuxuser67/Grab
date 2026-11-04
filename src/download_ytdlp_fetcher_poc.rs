//! Test-only compatibility probes for the pinned yt-dlp HTTP fetcher.
//!
//! These are characterization tests, not a production integration. Some tests
//! intentionally assert unsafe upstream behavior so a future crate release can
//! be re-evaluated. Every destination is contained in a unique temporary dir.

use std::path::Path;
use std::sync::Arc;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use yt_dlp::download::Fetcher;

const ASSET: &[u8] = b"abcdefgh";

struct TestResponse {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl TestResponse {
    fn new(status: u16, body: &[u8]) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.to_vec(),
        }
    }

    fn partial(content_range: &str, body: &[u8]) -> Self {
        Self::new(206, body).with_header("Content-Range", content_range)
    }

    fn with_header(mut self, name: &'static str, value: &str) -> Self {
        self.headers.push((name, value.to_string()));
        self
    }
}

async fn start_server<F>(request_count: usize, responder: F) -> (String, JoinHandle<Vec<String>>)
where
    F: Fn(&str) -> TestResponse + Send + Sync + 'static,
{
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind local Fetcher test server");
    let address = listener.local_addr().expect("read local server address");

    let server = tokio::spawn(async move {
        let mut requests = Vec::with_capacity(request_count);
        for _ in 0..request_count {
            let accepted = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                listener.accept(),
            )
            .await;
            let (mut socket, _) = match accepted {
                Ok(Ok(connection)) => connection,
                Ok(Err(error)) => panic!("accept Fetcher request: {error}"),
                Err(_) => break,
            };
            let request = read_request_headers(&mut socket).await;
            let response = responder(&request);
            write_response(&mut socket, response).await;
            requests.push(request);
        }
        requests
    });

    (format!("http://{address}/asset.bin"), server)
}

async fn read_request_headers(socket: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        socket
            .read_exact(&mut byte)
            .await
            .expect("read one HTTP request-header byte");
        bytes.push(byte[0]);
        assert!(
            bytes.len() <= 16 * 1024,
            "HTTP request headers exceeded the test limit"
        );
        if bytes.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8(bytes).expect("HTTP request headers are ASCII")
}

async fn write_response(socket: &mut TcpStream, response: TestResponse) {
    let reason = match response.status {
        200 => "OK",
        206 => "Partial Content",
        500 => "Internal Server Error",
        _ => "Test Response",
    };
    let mut headers = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        reason,
        response.body.len()
    );
    for (name, value) in response.headers {
        headers.push_str(name);
        headers.push_str(": ");
        headers.push_str(&value);
        headers.push_str("\r\n");
    }
    headers.push_str("\r\n");

    socket
        .write_all(headers.as_bytes())
        .await
        .expect("write HTTP response headers");
    socket
        .write_all(&response.body)
        .await
        .expect("write HTTP response body");
}

fn requested_range(request: &str) -> Option<&str> {
    request.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("range").then_some(value.trim())
    })
}

#[test]
fn fetcher_accepts_grabs_guarded_reqwest_client() {
    let client = crate::download_net::http_client_for(None)
        .expect("build Grab's guarded HTTP client");
    let _fetcher = Fetcher::with_client(
        "https://cdn.example.test/asset.bin",
        Arc::new(client),
    );
}

fn fetcher(url: &str, segment_size: usize) -> Fetcher {
    let client = Arc::new(
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build shared reqwest test client"),
    );
    Fetcher::with_client(url, client)
        .with_parallel_segments(1)
        .with_segment_size(segment_size)
        .with_retry_attempts(1)
}

async fn assert_file(path: &Path, expected: &[u8]) {
    let actual = tokio::fs::read(path)
        .await
        .expect("read temporary Fetcher output");
    assert_eq!(actual.as_slice(), expected);
}

#[tokio::test]
async fn fetcher_can_reuse_a_shared_reqwest_client_for_plain_http() {
    let (url, server) = start_server(3, |request| match requested_range(request) {
        Some("bytes=0-0") => TestResponse::partial("bytes 0-0/8", &ASSET[..1]),
        Some("bytes=0-3") => TestResponse::partial("bytes 0-3/8", &ASSET[..4]),
        Some("bytes=4-7") => TestResponse::partial("bytes 4-7/8", &ASSET[4..]),
        range => panic!("unexpected Fetcher range request: {range:?}"),
    })
    .await;
    let directory = tempfile::tempdir().expect("create isolated Fetcher destination");
    let destination = directory.path().join("download.bin");

    fetcher(&url, 4)
        .fetch_asset(&destination)
        .await
        .expect("download plain HTTP asset through the shared client");

    let requests = server.await.expect("join plain HTTP test server");
    assert_eq!(requests.len(), 3);
    assert_eq!(requested_range(&requests[0]), Some("bytes=0-0"));
    assert_file(&destination, ASSET).await;
}

#[tokio::test]
async fn fetcher_accepts_a_200_response_to_a_segment_range() {
    let (url, server) = start_server(3, |request| match requested_range(request) {
        Some("bytes=0-0") => TestResponse::partial("bytes 0-0/8", &ASSET[..1]),
        // The server ignores the requested 0-3 range and returns a different full object.
        Some("bytes=0-3") => TestResponse::new(200, b"Xbcdefgh"),
        Some("bytes=4-7") => TestResponse::partial("bytes 4-7/8", &ASSET[4..]),
        range => panic!("unexpected Fetcher range request: {range:?}"),
    })
    .await;
    let directory = tempfile::tempdir().expect("create isolated Fetcher destination");
    let destination = directory.path().join("download.bin");

    fetcher(&url, 4)
        .fetch_asset(&destination)
        .await
        .expect("Fetcher treats any successful status as a successful segment");

    let requests = server.await.expect("join ignored-range test server");
    assert_eq!(requests.len(), 3);
    let actual = tokio::fs::read(&destination)
        .await
        .expect("read ignored-range result");
    assert_eq!(actual.as_slice(), b"Xbcdefgh");
    assert_ne!(
        actual.as_slice(),
        ASSET,
        "a 200 response to a range request must not be treated as the requested segment"
    );
}

#[tokio::test]
async fn fetcher_accepts_a_mismatched_content_range_value() {
    let (url, server) = start_server(3, |request| match requested_range(request) {
        Some("bytes=0-0") => TestResponse::partial("bytes 0-0/8", &ASSET[..1]),
        // The body is from bytes 4-7 even though the request was for bytes 0-3.
        Some("bytes=0-3") => TestResponse::partial("bytes 4-7/8", &ASSET[4..]),
        Some("bytes=4-7") => TestResponse::partial("bytes 4-7/8", &ASSET[4..]),
        range => panic!("unexpected Fetcher range request: {range:?}"),
    })
    .await;
    let directory = tempfile::tempdir().expect("create isolated Fetcher destination");
    let destination = directory.path().join("download.bin");

    fetcher(&url, 4)
        .fetch_asset(&destination)
        .await
        .expect("Fetcher accepts the mismatched Content-Range response");

    let requests = server.await.expect("join mismatched Content-Range server");
    assert_eq!(requests.len(), 3);
    assert_file(&destination, b"efghefgh").await;
}

#[tokio::test]
async fn fetcher_does_not_compare_same_size_files_with_response_validators() {
    let (url, server) = start_server(1, |_request| {
        TestResponse::partial("bytes 0-0/8", &ASSET[..1]).with_header("ETag", "\"generation-2\"")
    })
    .await;
    let directory = tempfile::tempdir().expect("create isolated Fetcher destination");
    let destination = directory.path().join("download.bin");
    tokio::fs::write(&destination, b"old-data")
        .await
        .expect("write same-size previous representation");

    fetcher(&url, 4)
        .fetch_asset(&destination)
        .await
        .expect("Fetcher treats the same-size destination as complete");

    let requests = server.await.expect("join validator test server");
    assert_eq!(requests.len(), 1, "only the range probe should be sent");
    assert_eq!(requested_range(&requests[0]), Some("bytes=0-0"));
    assert_file(&destination, b"old-data").await;
}

#[tokio::test]
async fn fetcher_retry_builder_does_not_disable_the_default_retry_policy() {
    let (url, server) = start_server(4, |request| match requested_range(request) {
        Some("bytes=0-0") => TestResponse::partial("bytes 0-0/8", &ASSET[..1]),
        Some("bytes=0-7") => TestResponse::new(500, &[]),
        range => panic!("unexpected Fetcher range request: {range:?}"),
    })
    .await;
    let directory = tempfile::tempdir().expect("create isolated Fetcher destination");
    let destination = directory.path().join("download.bin");
    let client = Arc::new(
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build shared reqwest test client"),
    );
    let result = Fetcher::with_client(&url, client)
        .with_parallel_segments(1)
        .with_segment_size(8)
        .with_retry_attempts(0)
        .fetch_asset(&destination)
        .await;

    assert!(result.is_err(), "the repeated server error must fail the transfer");
    let requests = server.await.expect("join retry characterization server");
    assert_eq!(
        requests
            .iter()
            .filter(|request| requested_range(request) == Some("bytes=0-7"))
            .count(),
        3,
        "the pinned 2.9.1 Fetcher still uses RetryPolicy::default()"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn fetcher_follows_a_destination_symlink_and_truncates_its_target() {
    use std::os::unix::fs::symlink;

    let (url, server) = start_server(3, |request| match requested_range(request) {
        Some("bytes=0-0") => TestResponse::partial("bytes 0-0/8", &ASSET[..1]),
        Some("bytes=0-3") => TestResponse::partial("bytes 0-3/8", &ASSET[..4]),
        Some("bytes=4-7") => TestResponse::partial("bytes 4-7/8", &ASSET[4..]),
        range => panic!("unexpected Fetcher range request: {range:?}"),
    })
    .await;
    let directory = tempfile::tempdir().expect("create isolated Fetcher destination");
    let target = directory.path().join("foreign-file.bin");
    let destination = directory.path().join("download.bin");
    tokio::fs::write(&target, b"old")
        .await
        .expect("write foreign target");
    symlink(&target, &destination).expect("make destination a symlink to the foreign file");

    fetcher(&url, 4)
        .fetch_asset(&destination)
        .await
        .expect("Fetcher opens the path reached through the symlink");

    let requests = server.await.expect("join symlink test server");
    assert_eq!(requests.len(), 3);
    let metadata = tokio::fs::symlink_metadata(&destination)
        .await
        .expect("inspect destination symlink");
    assert!(
        metadata.file_type().is_symlink(),
        "Fetcher follows the pre-existing symlink rather than replacing it"
    );
    assert_file(&target, ASSET).await;
}
