//! End-to-end tests for native HLS/DASH resource fetching and MP4 finalization.
//!
//! These tests generate short fragmented-MP4 fixtures with FFmpeg, serve their
//! resources over loopback HTTP, and exercise the production transfer, track
//! assembly, and FFmpeg finalization helpers. Manifest URLs are parsed as HTTPS;
//! only the already-planned child resource URLs are redirected to loopback so
//! tests do not depend on public DNS or external media services.

use super::*;

use std::collections::{HashMap, hash_map::Entry};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use url::Url;

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock must be after the Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "grab-manifest-{label}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create manifest test directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tools_available() -> bool {
    let available = ["ffmpeg", "ffprobe"].into_iter().all(|program| {
        Command::new(program)
            .arg("-version")
            .output()
            .is_ok_and(|output| output.status.success())
    });
    if available {
        return true;
    }

    assert!(
        std::env::var_os("GRAB_REQUIRE_FFMPEG_MANIFEST").is_none(),
        "GRAB_REQUIRE_FFMPEG_MANIFEST=1 requires both ffmpeg and ffprobe"
    );
    eprintln!("SKIP native manifest media integration tests: ffmpeg/ffprobe unavailable");
    false
}

fn assert_command_succeeded(output: &Output, context: &str) {
    assert!(
        output.status.success(),
        "{context}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_mp4_has_audio_and_video(ffprobe: &Path, output: &Path) {
    let probe = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(output)
        .output()
        .expect("run ffprobe on finalized manifest download");
    assert_command_succeeded(&probe, "ffprobe rejected the finalized MP4");

    let streams = String::from_utf8_lossy(&probe.stdout);
    assert!(
        streams.lines().any(|line| line == "codec_type=video"),
        "finalized MP4 has no video stream: {streams}"
    );
    assert!(
        streams.lines().any(|line| line == "codec_type=audio"),
        "finalized MP4 has no audio stream: {streams}"
    );
}

fn fixture_files(directory: &Path, urls: &[Url]) -> HashMap<String, Vec<u8>> {
    let mut files = HashMap::with_capacity(urls.len());
    for url in urls {
        let filename = url
            .path()
            .rsplit('/')
            .next()
            .filter(|filename| !filename.is_empty())
            .expect("planned fixture URL has a filename");
        let path = directory.join(filename);
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|error| panic!("read fixture resource {}: {error}", path.display()));
        let request_path = format!("/{filename}");
        match files.entry(request_path) {
            Entry::Vacant(entry) => {
                entry.insert(bytes);
            }
            Entry::Occupied(entry) => {
                assert_eq!(
                    entry.get(),
                    &bytes,
                    "fixture resources collide at {}",
                    entry.key()
                );
            }
        }
    }
    files
}

async fn spawn_fixture_server(
    files: HashMap<String, Vec<u8>>,
    expected_requests: usize,
) -> (Url, tokio::task::JoinHandle<()>) {
    assert!(
        !files.is_empty(),
        "fixture server requires at least one resource"
    );
    assert!(
        expected_requests >= files.len(),
        "fixture request count cannot be smaller than the unique resource set"
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind loopback fixture server");
    let address = listener.local_addr().expect("read fixture server address");
    let server = tokio::spawn(async move {
        for _ in 0..expected_requests {
            let (mut socket, _) = listener.accept().await.expect("accept fixture request");
            let mut request = Vec::with_capacity(512);
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut buffer = [0_u8; 1024];
                let length = socket
                    .read(&mut buffer)
                    .await
                    .expect("read fixture request");
                assert_ne!(length, 0, "fixture request ended before its headers");
                request.extend_from_slice(&buffer[..length]);
                assert!(
                    request.len() <= 16 * 1024,
                    "fixture request headers exceeded the test limit"
                );
            }

            let request = String::from_utf8_lossy(&request);
            let request_path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("/");
            let request_path = request_path.split('?').next().unwrap_or("/");
            let response = match files.get(request_path) {
                Some(body) => {
                    let mut response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    response.extend_from_slice(body);
                    response
                }
                None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            };
            socket
                .write_all(&response)
                .await
                .expect("write fixture response");
        }
    });
    let base_url = Url::parse(&format!("http://{address}/")).expect("build fixture server URL");
    (base_url, server)
}

async fn wait_for_fixture_server(server: tokio::task::JoinHandle<()>) {
    tokio::time::timeout(Duration::from_secs(20), server)
        .await
        .expect("fixture server timed out waiting for resource requests")
        .expect("fixture server task failed");
}

fn loopback_resource_url(base_url: &Url, original: &Url) -> Url {
    let filename = original
        .path()
        .rsplit('/')
        .next()
        .filter(|filename| !filename.is_empty())
        .expect("planned fixture URL has a filename");
    let mut url = base_url.clone();
    url.set_path(&format!("/{filename}"));
    url.set_query(original.query());
    url
}

fn fixture_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build direct loopback HTTP client")
}

async fn read_fixture_request_path(socket: &mut tokio::net::TcpStream) -> String {
    let mut request = Vec::with_capacity(512);
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let mut buffer = [0_u8; 1024];
        let length = socket
            .read(&mut buffer)
            .await
            .expect("read fixture request");
        assert_ne!(length, 0, "fixture request ended before its headers");
        request.extend_from_slice(&buffer[..length]);
        assert!(
            request.len() <= 16 * 1024,
            "fixture request headers exceeded the test limit"
        );
    }

    String::from_utf8_lossy(&request)
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("/")
        .to_string()
}

async fn spawn_interrupted_resource_server(
    body: Vec<u8>,
    partial_size: usize,
) -> (
    Url,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<Vec<String>>,
) {
    assert!(
        partial_size > 0 && partial_size < body.len(),
        "partial response must contain fewer bytes than the complete resource"
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind resumable resource fixture server");
    let address = listener
        .local_addr()
        .expect("read resumable fixture server address");
    let (partial_sent_tx, partial_sent_rx) = tokio::sync::oneshot::channel();
    let (release_first_tx, release_first_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut paths = Vec::with_capacity(2);
        let (mut first, _) = listener
            .accept()
            .await
            .expect("accept interrupted resource request");
        paths.push(read_fixture_request_path(&mut first).await);

        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        first
            .write_all(headers.as_bytes())
            .await
            .expect("write interrupted resource headers");
        first
            .write_all(&body[..partial_size])
            .await
            .expect("write interrupted resource prefix");
        first.flush().await.expect("flush interrupted resource prefix");
        partial_sent_tx
            .send(())
            .expect("notify test after sending resource prefix");

        let _ = release_first_rx.await;
        // The client has cancelled this response; a closed socket is expected.
        let _ = first.write_all(&body[partial_size..]).await;
        drop(first);

        let (mut resumed, _) = listener
            .accept()
            .await
            .expect("accept resumed resource request");
        paths.push(read_fixture_request_path(&mut resumed).await);
        let mut response = headers.into_bytes();
        response.extend_from_slice(&body);
        resumed
            .write_all(&response)
            .await
            .expect("write resumed resource response");
        paths
    });
    let base_url = Url::parse(&format!("http://{address}/"))
        .expect("build resumable resource fixture URL");

    (base_url, partial_sent_rx, release_first_tx, server)
}

#[tokio::test]
async fn interrupted_manifest_resource_fetch_preserves_completed_files_for_resume() {
    let root = TempRoot::new("resume");
    let staging = root.path().join("staging");
    tokio::fs::create_dir_all(&staging)
        .await
        .expect("create manifest resume staging directory");

    let client = fixture_client();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let downloaded = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let complete_bytes = b"completed manifest initialization resource".to_vec();
    let (complete_base_url, complete_server) = spawn_fixture_server(
        HashMap::from([("/complete.bin".to_string(), complete_bytes.clone())]),
        1,
    )
    .await;
    let complete_resource = Resource {
        url: complete_base_url
            .join("complete.bin")
            .expect("build completed resource URL"),
        range: None,
        filename: "complete.bin".to_string(),
        key: false,
    };
    let (_complete_abort_tx, mut complete_abort) = tokio::sync::oneshot::channel();
    assert!(
        fetch_resources(
            &client,
            vec![complete_resource.clone()],
            &staging,
            1,
            &tx,
            &downloaded,
            &mut complete_abort,
        )
        .await
        .expect("fetch completed resource"),
        "completed resource fetch unexpectedly aborted"
    );
    wait_for_fixture_server(complete_server).await;
    assert_eq!(
        tokio::fs::read(staging.join("complete.bin"))
            .await
            .expect("read completed resource"),
        complete_bytes
    );

    let slow_bytes = vec![0x5a; 32 * 1024];
    let partial_size = 1024;
    let (slow_base_url, partial_sent, release_first, slow_server) =
        spawn_interrupted_resource_server(slow_bytes.clone(), partial_size).await;
    let slow_resource = Resource {
        url: slow_base_url
            .join("slow.bin")
            .expect("build interrupted resource URL"),
        range: None,
        filename: "slow.bin".to_string(),
        key: false,
    };
    let (abort_tx, abort_rx) = tokio::sync::oneshot::channel();
    let attempt_client = client.clone();
    let attempt_directory = staging.clone();
    let attempt_tx = tx.clone();
    let attempt_downloaded = std::sync::Arc::clone(&downloaded);
    let attempt_resource = slow_resource.clone();
    let attempt = tokio::spawn(async move {
        let mut abort = abort_rx;
        fetch_resources(
            &attempt_client,
            vec![attempt_resource],
            &attempt_directory,
            1,
            &attempt_tx,
            &attempt_downloaded,
            &mut abort,
        )
        .await
    });

    tokio::time::timeout(Duration::from_secs(20), partial_sent)
        .await
        .expect("fixture server did not send the resource prefix")
        .expect("fixture server task ended before sending the prefix");
    let partial_path = staging.join("slow.bin.part");
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if tokio::fs::metadata(&partial_path)
                .await
                .is_ok_and(|metadata| metadata.len() >= partial_size as u64)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("resource prefix was not written to its staging file");
    let expected_partial_total = complete_bytes.len() as u64 + partial_size as u64;
    tokio::time::timeout(Duration::from_secs(20), async {
        while downloaded.load(std::sync::atomic::Ordering::Relaxed) < expected_partial_total {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("partial resource bytes were not reserved before cancellation");

    abort_tx
        .send(crate::video::StopIntent::Preserve)
        .expect("signal pause to the manifest resource fetch");
    let aborted = tokio::time::timeout(Duration::from_secs(20), attempt)
        .await
        .expect("interrupted resource fetch did not stop")
        .expect("interrupted resource fetch task panicked")
        .expect("interrupted resource fetch failed");
    assert!(!aborted, "interrupted resource fetch must report an abort");
    assert!(
        !staging.join("slow.bin").exists(),
        "an incomplete resource must not be published as complete"
    );
    assert_eq!(
        downloaded.load(std::sync::atomic::Ordering::Relaxed),
        complete_bytes.len() as u64,
        "aborting a partial resource must refund its byte reservation"
    );
    release_first
        .send(())
        .expect("release the cancelled fixture response");

    // A new attempt starts its progress accounting from zero; complete files are
    // recognized from staging and the interrupted .part is replaced from byte 0.
    downloaded.store(0, std::sync::atomic::Ordering::Relaxed);
    let (_resume_abort_tx, mut resume_abort) = tokio::sync::oneshot::channel();
    assert!(
        fetch_resources(
            &client,
            vec![complete_resource, slow_resource],
            &staging,
            1,
            &tx,
            &downloaded,
            &mut resume_abort,
        )
        .await
        .expect("resume manifest resource fetch"),
        "resumed manifest resource fetch unexpectedly aborted"
    );
    let requests = tokio::time::timeout(Duration::from_secs(20), slow_server)
        .await
        .expect("resumable fixture server did not finish")
        .expect("resumable fixture server task panicked");
    assert_eq!(
        requests,
        vec!["/slow.bin".to_string(), "/slow.bin".to_string()],
        "resume must reuse complete resources and retry the partial resource"
    );
    assert_eq!(
        tokio::fs::read(staging.join("complete.bin"))
            .await
            .expect("read preserved complete resource"),
        complete_bytes
    );
    assert_eq!(
        tokio::fs::read(staging.join("slow.bin"))
            .await
            .expect("read resumed resource"),
        slow_bytes
    );
    assert!(
        !partial_path.exists(),
        "successful retry must remove the stale partial file"
    );
    assert_eq!(
        downloaded.load(std::sync::atomic::Ordering::Relaxed),
        complete_bytes.len() as u64 + slow_bytes.len() as u64,
        "resumed progress must include each completed resource exactly once"
    );
}

#[tokio::test]
async fn hls_fmp4_fixture_downloads_and_finalizes_with_audio_and_video() {
    if !tools_available() {
        return;
    }

    let root = TempRoot::new("hls");
    let fixture_dir = root.path().join("fixture");
    std::fs::create_dir_all(&fixture_dir).expect("create HLS fixture directory");
    let generated = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=96x64:rate=12",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:sample_rate=48000",
            "-t",
            "3",
            "-shortest",
            "-c:v",
            "mpeg4",
            "-q:v",
            "5",
            "-c:a",
            "aac",
            "-b:a",
            "64k",
            "-f",
            "hls",
            "-hls_time",
            "1",
            "-hls_playlist_type",
            "vod",
            "-hls_segment_type",
            "fmp4",
            "-hls_fmp4_init_filename",
            "init.mp4",
            "-hls_segment_filename",
        ])
        .arg(fixture_dir.join("segment%03d.m4s"))
        .arg(fixture_dir.join("media.m3u8"))
        .output()
        .expect("generate HLS fMP4 fixture");
    assert_command_succeeded(&generated, "FFmpeg failed to generate the HLS fixture");

    let source_url = "https://manifest-origin.fixture.invalid/master.m3u8";
    let media_manifest_url = "https://cdn.fixture.invalid/media.m3u8";
    let manifest =
        std::fs::read(fixture_dir.join("media.m3u8")).expect("read generated HLS media playlist");
    let mut plan = crate::manifest::plan_hls_media_playlist(media_manifest_url, &manifest)
        .expect("parse generated HLS media playlist");

    {
        let _policy_test_guard = crate::download_net::isolate_untrusted_handoff_policy_for_test();
        assert!(crate::download_net::require_public_resolution(
            "manifest-origin.fixture.invalid"
        ));
        let mut guarded_child_hosts = std::collections::HashSet::new();
        let media_url = Url::parse(media_manifest_url).expect("HLS media manifest URL is valid");
        guard_manifest_child_url(source_url, &media_url, &mut guarded_child_hosts)
            .expect("the selected HLS media playlist host should be guarded");
        guard_hls_resources(source_url, &plan, &mut guarded_child_hosts)
            .expect("all planned HLS resources should pass the production host guard");
    }
    let urls: Vec<_> = plan
        .resources
        .iter()
        .map(|resource| resource.url.clone())
        .collect();
    let files = fixture_files(&fixture_dir, &urls);
    let (server_url, server) = spawn_fixture_server(files, urls.len()).await;
    for resource in &mut plan.resources {
        resource.url = loopback_resource_url(&server_url, &resource.url);
    }

    let video_dir = root.path().join("staging/video");
    tokio::fs::create_dir_all(&video_dir)
        .await
        .expect("create HLS video staging directory");
    let client = fixture_client();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let downloaded = Arc::new(AtomicU64::new(0));
    let (_abort_tx, mut abort) = tokio::sync::oneshot::channel();
    assert!(
        fetch_resources(
            &client,
            hls_resources(&plan),
            &video_dir,
            3,
            &tx,
            &downloaded,
            &mut abort,
        )
        .await
        .expect("fetch HLS resources"),
        "HLS resource fetch unexpectedly aborted"
    );
    wait_for_fixture_server(server).await;
    assert!(downloaded.load(Ordering::Relaxed) > 0);

    write_playlist(&video_dir, &plan)
        .await
        .expect("write local HLS playlist");
    let output_part = root.path().join("hls-output.part.mp4");
    assert!(
        run_ffmpeg(
            Path::new("ffmpeg"),
            &video_dir.join("local.m3u8"),
            None,
            &output_part,
            &mut abort,
        )
        .await
        .expect("finalize HLS fixture with FFmpeg"),
        "HLS FFmpeg finalization unexpectedly aborted"
    );
    let output = root.path().join("hls-output.mp4");
    crate::file_names::rename_noreplace(&output_part, &output)
        .expect("publish finalized HLS fixture");
    assert_mp4_has_audio_and_video(Path::new("ffprobe"), &output);
}

#[tokio::test]
async fn dash_fmp4_fixture_downloads_assembles_tracks_and_finalizes() {
    if !tools_available() {
        return;
    }

    let root = TempRoot::new("dash");
    let fixture_dir = root.path().join("fixture");
    std::fs::create_dir_all(&fixture_dir).expect("create DASH fixture directory");
    let generated = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=96x64:rate=12",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:sample_rate=48000",
            "-map",
            "0:v:0",
            "-map",
            "1:a:0",
            "-t",
            "3",
            "-c:v",
            "mpeg4",
            "-q:v",
            "5",
            "-c:a",
            "aac",
            "-b:a",
            "64k",
            "-f",
            "dash",
            "-seg_duration",
            "1",
            "-use_template",
            "1",
            "-use_timeline",
            "0",
            "-init_seg_name",
            "init-$RepresentationID$.m4s",
            "-media_seg_name",
            "chunk-$RepresentationID$-$Number%05d$.m4s",
            "-adaptation_sets",
            "id=0,streams=v id=1,streams=a",
        ])
        .arg(fixture_dir.join("manifest.mpd"))
        .output()
        .expect("generate DASH fMP4 fixture");
    assert_command_succeeded(&generated, "FFmpeg failed to generate the DASH fixture");

    let source_url = "https://manifest-origin.fixture.invalid/master.mpd";
    let manifest_url = "https://cdn.fixture.invalid/manifest.mpd";
    let manifest =
        std::fs::read_to_string(fixture_dir.join("manifest.mpd")).expect("read generated DASH MPD");
    let parsed = crate::manifest::dash::parse_dash_manifest(manifest_url, &manifest)
        .expect("parse generated DASH MPD");
    let video_key = parsed
        .video_representations
        .first()
        .expect("generated DASH fixture has a video representation")
        .key
        .clone();
    let audio_key = parsed
        .default_audio()
        .expect("generated DASH fixture has an audio representation")
        .key
        .clone();
    let mut video_plan = crate::manifest::dash::plan_dash_representation(&parsed, &video_key)
        .expect("plan generated DASH video representation");
    let mut audio_plan = crate::manifest::dash::plan_dash_representation(&parsed, &audio_key)
        .expect("plan generated DASH audio representation");

    {
        let _policy_test_guard = crate::download_net::isolate_untrusted_handoff_policy_for_test();
        assert!(crate::download_net::require_public_resolution(
            "manifest-origin.fixture.invalid"
        ));
        let mut guarded_child_hosts = std::collections::HashSet::new();
        for resource in video_plan
            .resources
            .iter()
            .chain(audio_plan.resources.iter())
        {
            guard_manifest_child_url(source_url, &resource.url, &mut guarded_child_hosts)
                .expect("all planned DASH resources should pass the production host guard");
        }
    }

    let mut urls: Vec<_> = video_plan
        .resources
        .iter()
        .map(|resource| resource.url.clone())
        .collect();
    urls.extend(
        audio_plan
            .resources
            .iter()
            .map(|resource| resource.url.clone()),
    );
    let files = fixture_files(&fixture_dir, &urls);
    let (server_url, server) = spawn_fixture_server(files, urls.len()).await;
    for resource in &mut video_plan.resources {
        resource.url = loopback_resource_url(&server_url, &resource.url);
    }
    for resource in &mut audio_plan.resources {
        resource.url = loopback_resource_url(&server_url, &resource.url);
    }

    let video_dir = root.path().join("staging/video");
    let audio_dir = root.path().join("staging/audio");
    tokio::fs::create_dir_all(&video_dir)
        .await
        .expect("create DASH video staging directory");
    tokio::fs::create_dir_all(&audio_dir)
        .await
        .expect("create DASH audio staging directory");
    let client = fixture_client();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let downloaded = Arc::new(AtomicU64::new(0));
    let (_abort_tx, mut abort) = tokio::sync::oneshot::channel();

    assert!(
        fetch_resources(
            &client,
            dash_resources(&video_plan),
            &video_dir,
            3,
            &tx,
            &downloaded,
            &mut abort,
        )
        .await
        .expect("fetch DASH video resources"),
        "DASH video resource fetch unexpectedly aborted"
    );
    assert!(
        fetch_resources(
            &client,
            dash_resources(&audio_plan),
            &audio_dir,
            3,
            &tx,
            &downloaded,
            &mut abort,
        )
        .await
        .expect("fetch DASH audio resources"),
        "DASH audio resource fetch unexpectedly aborted"
    );
    wait_for_fixture_server(server).await;
    assert!(downloaded.load(Ordering::Relaxed) > 0);

    assert!(
        concat_dash_track(&video_plan, &video_dir, "video-track.mp4", &mut abort)
            .await
            .expect("assemble DASH video track"),
        "DASH video assembly unexpectedly aborted"
    );
    assert!(
        concat_dash_track(&audio_plan, &audio_dir, "audio-track.mp4", &mut abort)
            .await
            .expect("assemble DASH audio track"),
        "DASH audio assembly unexpectedly aborted"
    );

    let output_part = root.path().join("dash-output.part.mp4");
    assert!(
        run_ffmpeg(
            Path::new("ffmpeg"),
            &video_dir.join("video-track.mp4"),
            Some(&audio_dir.join("audio-track.mp4")),
            &output_part,
            &mut abort,
        )
        .await
        .expect("finalize DASH fixture with FFmpeg"),
        "DASH FFmpeg finalization unexpectedly aborted"
    );
    let output = root.path().join("dash-output.mp4");
    crate::file_names::rename_noreplace(&output_part, &output)
        .expect("publish finalized DASH fixture");
    assert_mp4_has_audio_and_video(Path::new("ffprobe"), &output);
}
