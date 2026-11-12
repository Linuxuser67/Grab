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

use url::Url;

const FIXTURE_TIMEOUT: Duration = Duration::from_secs(20);
const INTERRUPTED_RESOURCE_BYTES: usize = 32 * 1024;
const INTERRUPTED_RESOURCE_PREFIX_BYTES: usize = 1024;

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

fn assert_has_audio_and_video(ffprobe: &Path, output: &Path) {
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
    assert_command_succeeded(&probe, "ffprobe rejected the finalized media file");

    let streams = String::from_utf8_lossy(&probe.stdout);
    assert!(
        streams.lines().any(|line| line == "codec_type=video"),
        "finalized media file has no video stream: {streams}"
    );
    assert!(
        streams.lines().any(|line| line == "codec_type=audio"),
        "finalized media file has no audio stream: {streams}"
    );
}

fn shift_fragmented_mp4_decode_times(path: &Path, delta: u64) {
    fn shift_boxes(bytes: &mut [u8], start: usize, end: usize, delta: u64, shifted: &mut usize) {
        let mut cursor = start;
        while cursor < end {
            assert!(
                cursor
                    .checked_add(8)
                    .is_some_and(|header_end| header_end <= end)
            );
            let size = u32::from_be_bytes(
                bytes[cursor..cursor + 4]
                    .try_into()
                    .expect("read generated MP4 box size"),
            );
            let kind = bytes[cursor + 4..cursor + 8].to_vec();
            let (box_size, header_size) = match size {
                0 => (end - cursor, 8_usize),
                1 => {
                    let extended = u64::from_be_bytes(
                        bytes[cursor + 8..cursor + 16]
                            .try_into()
                            .expect("read generated MP4 extended box size"),
                    );
                    (
                        usize::try_from(extended).expect("generated MP4 box fits usize"),
                        16_usize,
                    )
                }
                value => (
                    usize::try_from(value).expect("generated MP4 box fits usize"),
                    8,
                ),
            };
            assert!(
                box_size >= header_size,
                "generated MP4 box has a short header"
            );
            let box_end = cursor
                .checked_add(box_size)
                .filter(|box_end| *box_end <= end)
                .expect("generated MP4 box stays within its parent");
            let payload_start = cursor + header_size;

            match kind.as_slice() {
                b"moof" | b"traf" => {
                    shift_boxes(bytes, payload_start, box_end, delta, shifted);
                }
                b"tfdt" => {
                    let version = bytes[payload_start];
                    let decode_time_start = payload_start + 4;
                    match version {
                        0 => {
                            let value = u32::from_be_bytes(
                                bytes[decode_time_start..decode_time_start + 4]
                                    .try_into()
                                    .expect("read generated 32-bit tfdt"),
                            );
                            let shifted_value = value
                                .checked_add(u32::try_from(delta).expect("tfdt shift fits u32"))
                                .expect("generated tfdt does not overflow");
                            bytes[decode_time_start..decode_time_start + 4]
                                .copy_from_slice(&shifted_value.to_be_bytes());
                        }
                        1 => {
                            let value = u64::from_be_bytes(
                                bytes[decode_time_start..decode_time_start + 8]
                                    .try_into()
                                    .expect("read generated 64-bit tfdt"),
                            );
                            let shifted_value = value
                                .checked_add(delta)
                                .expect("generated tfdt does not overflow");
                            bytes[decode_time_start..decode_time_start + 8]
                                .copy_from_slice(&shifted_value.to_be_bytes());
                        }
                        other => panic!("generated fMP4 has unsupported tfdt version {other}"),
                    }
                    *shifted += 1;
                }
                _ => {}
            }

            cursor = box_end;
        }
        assert_eq!(cursor, end, "generated MP4 boxes exactly fill their parent");
    }

    let mut bytes = std::fs::read(path).expect("read generated fragmented MP4");
    let length = bytes.len();
    let mut shifted = 0;
    shift_boxes(&mut bytes, 0, length, delta, &mut shifted);
    assert!(shifted > 0, "generated fragmented MP4 contains tfdt boxes");
    std::fs::write(path, bytes).expect("write shifted fragmented MP4 timestamps");
}

fn ffprobe_stream_start_times(ffprobe: &Path, path: &Path) -> HashMap<String, f64> {
    let probe = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type,start_time",
            "-of",
            "compact=p=0",
        ])
        .arg(path)
        .output()
        .expect("run ffprobe for stream start times");
    assert_command_succeeded(&probe, "ffprobe could not inspect stream timestamps");

    String::from_utf8_lossy(&probe.stdout)
        .lines()
        .filter_map(|line| {
            let mut codec_type = None;
            let mut start_time = None;
            for field in line.split('|') {
                if let Some(value) = field.strip_prefix("codec_type=") {
                    codec_type = Some(value.to_string());
                } else if let Some(value) = field.strip_prefix("start_time=") {
                    start_time = value.parse::<f64>().ok();
                }
            }
            Some((codec_type?, start_time?))
        })
        .collect()
}

fn ffmpeg_encoder_available(encoder: &str) -> bool {
    let Ok(output) = Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    stdout
        .lines()
        .chain(stderr.lines())
        .any(|line| line.split_whitespace().nth(1) == Some(encoder))
}

#[tokio::test]
async fn mainstream_apple_hls_profiles_remux_with_stream_copy() {
    if !tools_available() {
        return;
    }

    struct Profile {
        name: &'static str,
        video_encoder: &'static str,
        video_codec: &'static str,
        video_tag: &'static str,
        pixel_format: &'static str,
        audio_encoder: &'static str,
        audio_codec: &'static str,
        audio_tag: &'static str,
        audio_channels: &'static str,
        color_transfer: Option<&'static str>,
        x265_transfer: Option<&'static str>,
    }

    let profiles = [
        Profile {
            name: "avc-aac-sdr",
            video_encoder: "libx264",
            video_codec: "h264",
            video_tag: "avc1",
            pixel_format: "yuv420p",
            audio_encoder: "aac",
            audio_codec: "aac",
            audio_tag: "mp4a",
            audio_channels: "2",
            color_transfer: None,
            x265_transfer: None,
        },
        Profile {
            name: "hevc-eac3-pq",
            video_encoder: "libx265",
            video_codec: "hevc",
            video_tag: "hvc1",
            pixel_format: "yuv420p10le",
            audio_encoder: "eac3",
            audio_codec: "eac3",
            audio_tag: "ec-3",
            audio_channels: "6",
            color_transfer: Some("smpte2084"),
            x265_transfer: Some("16"),
        },
        Profile {
            name: "hevc-ac3-hlg",
            video_encoder: "libx265",
            video_codec: "hevc",
            video_tag: "hvc1",
            pixel_format: "yuv420p10le",
            audio_encoder: "ac3",
            audio_codec: "ac3",
            audio_tag: "ac-3",
            audio_channels: "6",
            color_transfer: Some("arib-std-b67"),
            x265_transfer: Some("18"),
        },
        Profile {
            name: "avc-ac3-sdr-fallback",
            video_encoder: "libx264",
            video_codec: "h264",
            video_tag: "avc1",
            pixel_format: "yuv420p",
            audio_encoder: "ac3",
            audio_codec: "ac3",
            audio_tag: "ac-3",
            audio_channels: "6",
            color_transfer: None,
            x265_transfer: None,
        },
    ];

    let root = TempRoot::new("apple-hls-profiles");
    for profile in profiles {
        if !ffmpeg_encoder_available(profile.video_encoder)
            || !ffmpeg_encoder_available(profile.audio_encoder)
        {
            eprintln!(
                "SKIP Apple HLS profile {}: FFmpeg encoder {} or {} is unavailable",
                profile.name, profile.video_encoder, profile.audio_encoder
            );
            continue;
        }

        let playlist_name = format!("{}.m3u8", profile.name);
        let segment_pattern = format!("{}_%03d.m4s", profile.name);
        let init_name = format!("{}_init.mp4", profile.name);
        let mut encode = Command::new("ffmpeg");
        encode
            .current_dir(root.path())
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=160x90:rate=24",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=1000:sample_rate=48000",
                "-t",
                "2",
                "-threads",
                "1",
                "-c:v",
                profile.video_encoder,
                "-pix_fmt",
                profile.pixel_format,
                "-tag:v",
                profile.video_tag,
                "-c:a",
                profile.audio_encoder,
                "-b:a",
                "384k",
                "-ac",
                profile.audio_channels,
                "-tag:a",
                profile.audio_tag,
                "-color_primaries",
            ])
            .arg(if profile.color_transfer.is_some() {
                "bt2020"
            } else {
                "bt709"
            })
            .arg("-color_trc")
            .arg(profile.color_transfer.unwrap_or("bt709"))
            .arg("-colorspace")
            .arg(if profile.color_transfer.is_some() {
                "bt2020nc"
            } else {
                "bt709"
            });
        if let Some(transfer) = profile.x265_transfer {
            let parameters =
                format!("colorprim=9:transfer={transfer}:colormatrix=9:log-level=error:pools=1");
            encode.args(["-x265-params", parameters.as_str()]);
        }
        let encoded = encode
            .args([
                "-f",
                "hls",
                "-hls_time",
                "1",
                "-hls_playlist_type",
                "vod",
                "-hls_segment_type",
                "fmp4",
                "-hls_fmp4_init_filename",
                &init_name,
                "-hls_segment_filename",
                &segment_pattern,
                "-hls_flags",
                "independent_segments",
                &playlist_name,
            ])
            .output()
            .expect("run FFmpeg to generate an Apple-profile fMP4 HLS fixture");
        assert_command_succeeded(
            &encoded,
            &format!("{} HLS fixture generation", profile.name),
        );

        let playlist_path = root.path().join(&playlist_name);
        let playlist = std::fs::read(&playlist_path).expect("read generated HLS playlist");
        let plan = crate::manifest::plan_hls_media_playlist(
            &format!("https://fixture.example.test/{playlist_name}"),
            &playlist,
        )
        .expect("generated fMP4 HLS playlist should be accepted by the native planner");
        assert!(
            plan.is_single_init_fmp4_presentation(),
            "{} should use one fMP4 initialization section",
            profile.name
        );

        let output_path = root.path().join(format!("{}-copy.mp4", profile.name));
        let remuxed = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-allowed_extensions",
                "ALL",
                "-i",
            ])
            .arg(&playlist_path)
            .args([
                "-map",
                "0:v:0",
                "-map",
                "0:a:0",
                "-c",
                "copy",
                "-movflags",
                "+faststart",
            ])
            .arg(&output_path)
            .output()
            .expect("run FFmpeg stream-copy remux for Apple HLS fixture");
        assert_command_succeeded(&remuxed, &format!("{} stream-copy remux", profile.name));

        let probed = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_type,codec_name,codec_tag_string,color_transfer,channels,pix_fmt",
                "-of",
                "default=noprint_wrappers=1",
            ])
            .arg(&output_path)
            .output()
            .expect("run ffprobe on Apple HLS remux");
        assert_command_succeeded(&probed, &format!("{} ffprobe", profile.name));
        let stream_info = String::from_utf8_lossy(&probed.stdout);
        for expected in [
            format!("codec_name={}", profile.video_codec),
            format!("codec_tag_string={}", profile.video_tag),
            format!("codec_name={}", profile.audio_codec),
            format!("codec_tag_string={}", profile.audio_tag),
            format!("channels={}", profile.audio_channels),
            format!("pix_fmt={}", profile.pixel_format),
        ] {
            assert!(
                stream_info.contains(&expected),
                "{} is missing {expected}: {stream_info}",
                profile.name
            );
        }
        if let Some(expected_transfer) = profile.color_transfer {
            assert!(
                stream_info.contains(&format!("color_transfer={expected_transfer}")),
                "{} lost HDR transfer metadata: {stream_info}",
                profile.name
            );
        }

        // Exercise the production finalizer on the common H.264/AAC HLS
        // profile. MPEG-TS output must preserve both encoded streams.
        if profile.name == "avc-aac-sdr" {
            let ts_output = root.path().join("avc-aac-sdr-copy.ts");
            let (_abort_tx, mut abort) = tokio::sync::oneshot::channel();
            assert!(
                run_ffmpeg(
                    Path::new("ffmpeg"),
                    &playlist_path,
                    None,
                    SubtitleOutput::default(),
                    &ts_output,
                    &mut abort,
                )
                .await
                .expect("finalize Apple HLS fixture as MPEG-TS"),
                "MPEG-TS FFmpeg finalization unexpectedly aborted"
            );
            assert_has_audio_and_video(Path::new("ffprobe"), &ts_output);

            let ts_probe = Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-show_entries",
                    "format=format_name:stream=codec_name",
                    "-of",
                    "default=noprint_wrappers=1",
                ])
                .arg(&ts_output)
                .output()
                .expect("probe MPEG-TS output");
            assert_command_succeeded(&ts_probe, "ffprobe rejected MPEG-TS output");
            let ts_info = String::from_utf8_lossy(&ts_probe.stdout);
            assert!(
                ts_info.contains("format_name=mpegts"),
                "finalized TS file does not use the MPEG-TS muxer: {ts_info}"
            );
            assert!(
                ts_info.contains("codec_name=h264") && ts_info.contains("codec_name=aac"),
                "MPEG-TS finalization changed the H.264/AAC streams: {ts_info}"
            );
        }
    }
}

#[tokio::test]
async fn hls_separate_inputs_preserve_rendition_timestamp_offsets() {
    if !tools_available() {
        return;
    }

    let root = TempRoot::new("hls-rendition-sync");
    let first_track = root.path().join("track_0");
    let second_track = root.path().join("track_1");
    std::fs::create_dir_all(&first_track).expect("create video fixture directory");
    std::fs::create_dir_all(&second_track).expect("create audio fixture directory");

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
            "-itsoffset",
            "2",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:sample_rate=48000",
            "-t",
            "5",
            "-map",
            "0:v:0",
            "-map",
            "1:a:0",
            "-c:v",
            "mpeg2video",
            "-g",
            "12",
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
            "-hls_list_size",
            "0",
            "-hls_flags",
            "program_date_time",
            "-master_pl_name",
            "source-master.m3u8",
            "-var_stream_map",
            "v:0,agroup:aud a:0,agroup:aud,default:yes",
            "-hls_segment_filename",
        ])
        .arg(root.path().join("track_%v/segment_%03d.bin"))
        .arg(root.path().join("track_%v/local.m3u8"))
        .output()
        .expect("generate separate HLS audio and video renditions");
    assert_command_succeeded(
        &generated,
        "FFmpeg failed to generate the HLS rendition fixture",
    );

    let video_dir = root.path().join("video");
    let audio_dir = root.path().join("audio");
    std::fs::rename(&first_track, &video_dir).expect("move video rendition into staging");
    std::fs::rename(&second_track, &audio_dir).expect("move audio rendition into staging");
    for playlist in [video_dir.join("local.m3u8"), audio_dir.join("local.m3u8")] {
        let contents =
            std::fs::read_to_string(&playlist).expect("read generated rendition playlist");
        assert!(
            contents.contains("#EXT-X-PROGRAM-DATE-TIME:"),
            "the fixture must contain preserved program date-time metadata"
        );
    }

    // Separate playlist inputs must retain the original rendition PTS clock.
    // Without -copyts FFmpeg rebases each HLS input independently.
    let video_playlist = video_dir.join("local.m3u8");
    let audio_playlist = audio_dir.join("local.m3u8");
    let output = root.path().join("rendition-sync.part.mp4");
    let (_stop_tx, mut abort) = tokio::sync::oneshot::channel();
    assert!(
        run_ffmpeg_with_offsets(
            Path::new("ffmpeg"),
            FfmpegInput {
                path: &video_playlist,
                timestamp_offset: None,
            },
            Some(FfmpegInput {
                path: &audio_playlist,
                timestamp_offset: None,
            }),
            true,
            SubtitleOutput::default(),
            &output,
            &mut abort,
        )
        .await
        .expect("finalize separate HLS renditions with source timestamps"),
        "FFmpeg unexpectedly aborted separate HLS rendition finalization"
    );

    let starts = ffprobe_stream_start_times(Path::new("ffprobe"), &output);
    let video_start = *starts
        .get("video")
        .expect("output contains video timestamps");
    let audio_start = *starts
        .get("audio")
        .expect("output contains audio timestamps");
    let offset = audio_start - video_start;
    assert!(
        (offset - 2.0).abs() < 0.1,
        "the source's 2-second A/V offset must survive finalization; video={video_start}, audio={audio_start}, delta={offset}"
    );

    // The fast destination-part path supplies raw transport-stream inputs,
    // not HLS playlists. In that case -copyts preserves the shared PTS clock.
    let concat_track = |directory: &Path| {
        let playlist_bytes =
            std::fs::read(directory.join("local.m3u8")).expect("read rendition playlist");
        let playlist =
            m3u8_rs::parse_media_playlist_res(&playlist_bytes).expect("parse rendition playlist");
        let mut bytes = Vec::new();
        for segment in playlist.segments {
            bytes.extend(
                std::fs::read(directory.join(segment.uri))
                    .expect("read rendition transport-stream segment"),
            );
        }
        bytes
    };
    let raw_video = root.path().join("video-track.ts");
    let raw_audio = root.path().join("audio-track.ts");
    std::fs::write(&raw_video, concat_track(&video_dir)).expect("assemble raw video track");
    std::fs::write(&raw_audio, concat_track(&audio_dir)).expect("assemble raw audio track");
    let raw_output = root.path().join("raw-rendition-sync.part.mp4");
    let (_raw_stop_tx, mut raw_abort) = tokio::sync::oneshot::channel();
    assert!(
        run_ffmpeg_with_offsets(
            Path::new("ffmpeg"),
            FfmpegInput {
                path: &raw_video,
                timestamp_offset: None,
            },
            Some(FfmpegInput {
                path: &raw_audio,
                timestamp_offset: None,
            }),
            true,
            SubtitleOutput::default(),
            &raw_output,
            &mut raw_abort,
        )
        .await
        .expect("finalize raw HLS track parts with preserved timestamps"),
        "FFmpeg unexpectedly aborted raw HLS track finalization"
    );
    let raw_starts = ffprobe_stream_start_times(Path::new("ffprobe"), &raw_output);
    let raw_video_start = *raw_starts
        .get("video")
        .expect("raw output contains video timestamps");
    let raw_audio_start = *raw_starts
        .get("audio")
        .expect("raw output contains audio timestamps");
    let raw_offset = raw_audio_start - raw_video_start;
    assert!(
        (raw_offset - 2.0).abs() < 0.1,
        "raw destination parts must retain the source A/V offset; video={raw_video_start}, audio={raw_audio_start}, delta={raw_offset}"
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
    tokio::time::timeout(FIXTURE_TIMEOUT, server)
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

struct OrderedDashFixtureServer {
    base_url: Url,
    first_media_request: tokio::sync::oneshot::Receiver<String>,
    release_first_media: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<(Vec<String>, bool)>,
}

async fn spawn_ordered_dash_fixture_server(
    files: HashMap<String, Vec<u8>>,
    expected_requests: usize,
) -> OrderedDashFixtureServer {
    assert!(
        !files.is_empty(),
        "DASH fixture server requires media resources"
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind ordered DASH fixture server");
    let address = listener
        .local_addr()
        .expect("read ordered DASH fixture server address");
    let (first_media_tx, first_media_rx) = tokio::sync::oneshot::channel();
    let (release_first_tx, release_first_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut requests = Vec::with_capacity(expected_requests);
        let mut pending = std::collections::VecDeque::new();
        let mut concurrent_request = false;
        let mut first_media_tx = Some(first_media_tx);
        let mut release_first_rx = Some(release_first_rx);

        for _ in 0..expected_requests {
            let (mut socket, request_path) = if let Some(request) = pending.pop_front() {
                request
            } else {
                let (mut socket, _) = listener
                    .accept()
                    .await
                    .expect("accept ordered DASH resource request");
                let path = read_fixture_request_path(&mut socket).await;
                (socket, path)
            };
            requests.push(request_path.clone());
            let body = files
                .get(&request_path)
                .expect("DASH fixture request must name a known resource");
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket
                .write_all(headers.as_bytes())
                .await
                .expect("write ordered DASH resource headers");

            if request_path.starts_with("/seg-") && first_media_tx.is_some() {
                if let Some(first_byte) = body.first() {
                    socket
                        .write_all(std::slice::from_ref(first_byte))
                        .await
                        .expect("write first byte of active DASH segment");
                    socket.flush().await.expect("flush progressive DASH prefix");
                }
                first_media_tx
                    .take()
                    .expect("first media notification exists")
                    .send(request_path)
                    .expect("notify test about first LL-DASH media request");

                // Detect any second connection while the first progressive response is open.
                let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
                while tokio::time::Instant::now() < deadline {
                    match tokio::time::timeout_at(deadline, listener.accept()).await {
                        Ok(Ok((mut next, _))) => {
                            concurrent_request = true;
                            let next_path = read_fixture_request_path(&mut next).await;
                            pending.push_back((next, next_path));
                        }
                        Ok(Err(error)) => panic!("accept concurrent DASH request: {error}"),
                        Err(_) => break,
                    }
                }
                release_first_rx
                    .take()
                    .expect("first media release receiver exists")
                    .await
                    .expect("test releases the progressive DASH response");
                if body.len() > 1 {
                    socket
                        .write_all(&body[1..])
                        .await
                        .expect("write remaining progressive DASH bytes");
                }
            } else {
                socket
                    .write_all(body)
                    .await
                    .expect("write ordered DASH resource body");
            }
        }

        (requests, concurrent_request)
    });
    let base_url =
        Url::parse(&format!("http://{address}/")).expect("build ordered DASH fixture URL");

    OrderedDashFixtureServer {
        base_url,
        first_media_request: first_media_rx,
        release_first_media: release_first_tx,
        task,
    }
}

struct InterruptedResourceServer {
    base_url: Url,
    partial_sent: tokio::sync::oneshot::Receiver<()>,
    release_first: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Vec<String>>,
}

async fn spawn_interrupted_resource_server(
    body: Vec<u8>,
    partial_size: usize,
) -> InterruptedResourceServer {
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
        first
            .flush()
            .await
            .expect("flush interrupted resource prefix");
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
    let base_url =
        Url::parse(&format!("http://{address}/")).expect("build resumable resource fixture URL");

    InterruptedResourceServer {
        base_url,
        partial_sent: partial_sent_rx,
        release_first: release_first_tx,
        task: server,
    }
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

    let slow_bytes = vec![0x5a; INTERRUPTED_RESOURCE_BYTES];
    let partial_size = INTERRUPTED_RESOURCE_PREFIX_BYTES;
    let InterruptedResourceServer {
        base_url: slow_base_url,
        partial_sent,
        release_first,
        task: slow_server,
    } = spawn_interrupted_resource_server(slow_bytes.clone(), partial_size).await;
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

    tokio::time::timeout(FIXTURE_TIMEOUT, partial_sent)
        .await
        .expect("fixture server did not send the resource prefix")
        .expect("fixture server task ended before sending the prefix");
    let partial_path = staging.join("slow.bin.part");
    tokio::time::timeout(FIXTURE_TIMEOUT, async {
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
    tokio::time::timeout(FIXTURE_TIMEOUT, async {
        while downloaded.load(std::sync::atomic::Ordering::Relaxed) < expected_partial_total {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("partial resource bytes were not reserved before cancellation");

    abort_tx
        .send(crate::video::StopIntent::Pause)
        .expect("signal pause to the manifest resource fetch");
    let aborted = tokio::time::timeout(FIXTURE_TIMEOUT, attempt)
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
    let requests = tokio::time::timeout(FIXTURE_TIMEOUT, slow_server)
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

fn localize_live_plan_resources(plan: &mut HlsPlan, fixture_base: &Url) {
    plan.requested_playlist_url = fixture_base
        .join("live.m3u8")
        .expect("build fixture requested playlist URL");
    plan.playlist_url = fixture_base
        .join("live.m3u8")
        .expect("build fixture live playlist URL");
    for resource in plan.resources.iter_mut() {
        resource.url = loopback_resource_url(fixture_base, &resource.url);
    }
}

fn live_hls_test_source(url: &str) -> crate::media_types::ManifestSource {
    crate::media_types::ManifestSource {
        url: url.to_string(),
        kind: crate::media_types::ManifestKind::Hls,
        representation_key: "live-hls-test".into(),
        output_extension: None,
        audio_key: None,
        subtitle_key: None,
        subtitle_language: None,
    }
}

#[tokio::test]
async fn saved_live_hls_capture_finalizes_without_contacting_the_origin() {
    let source = live_hls_test_source("https://origin.example.test/master.m3u8");
    let root = TempRoot::new("live-offline");
    let video_dir = root.path().join("video");
    let audio_dir = root.path().join("audio");
    tokio::fs::create_dir_all(&video_dir)
        .await
        .expect("create offline live video staging");

    let remote = crate::manifest::plan_hls_media_playlist(
        "https://origin.example.test/live.m3u8",
        b"#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:950\n#EXTINF:4,\n950.ts\n",
    )
    .expect("plan remote live playlist");
    let resource = remote.resources.first().expect("one initial segment");
    let segment_bytes = b"offline-committed-live-segment";
    tokio::fs::write(video_dir.join(&resource.filename), segment_bytes)
        .await
        .expect("write completed segment into owned staging");

    let mut accumulator = HlsLiveAccumulator::new();
    let update = accumulator
        .prepare_update(&remote)
        .expect("prepare initial live capture");
    accumulator
        .commit_update_with_budget(update, segment_bytes.len() as u64, 0)
        .expect("commit initial segment and byte count");
    write_live_playlist(&video_dir, &accumulator)
        .await
        .expect("persist committed local live playlist");

    let local_bytes = tokio::fs::read(video_dir.join("local.m3u8"))
        .await
        .expect("read committed local live playlist");
    let local_plan =
        HlsPlan::from_local_capture("https://origin.example.test/live.m3u8", &local_bytes)
            .expect("rebuild completed local plan");
    assert!(local_plan.end_list);
    assert!(local_plan.resources.is_empty());

    let client = fixture_client();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let downloaded = Arc::new(AtomicU64::new(0));
    let mut guarded_child_hosts = crate::download_net::PublicResolutionSet::default();
    let (_stop_tx, mut abort) = tokio::sync::oneshot::channel();
    let outcome = run_live_hls_tracks(
        &client,
        "https://origin.example.test/master.m3u8",
        &source,
        None,
        root.path(),
        root.path(),
        0,
        root.path(),
        &local_plan,
        None,
        false,
        &video_dir,
        &audio_dir,
        1,
        &tx,
        &downloaded,
        &mut guarded_child_hosts,
        &mut abort,
    )
    .await
    .expect("finalize stored capture without a network request");

    assert_eq!(outcome.outcome, LiveHlsCaptureOutcome::Ended);
    assert_eq!(outcome.output, super::LiveHlsOutput::LocalPlaylist);
    assert_eq!(
        downloaded.load(Ordering::Relaxed),
        segment_bytes.len() as u64,
        "restoring a saved capture restores its retained-byte progress without a new fetch"
    );

    let (_finish_tx, mut finish_abort) = tokio::sync::oneshot::channel();
    let finish_outcome = run_live_hls_tracks(
        &client,
        "https://origin.example.test/master.m3u8",
        &source,
        None,
        root.path(),
        root.path(),
        0,
        root.path(),
        &local_plan,
        None,
        true,
        &video_dir,
        &audio_dir,
        1,
        &tx,
        &downloaded,
        &mut guarded_child_hosts,
        &mut finish_abort,
    )
    .await
    .expect("resume a durable finish without polling the source");
    assert_eq!(finish_outcome.outcome, LiveHlsCaptureOutcome::Finish);
    assert_eq!(finish_outcome.output, super::LiveHlsOutput::LocalPlaylist);
    let final_playlist = m3u8_rs::parse_media_playlist_res(
        &tokio::fs::read(video_dir.join("local.m3u8"))
            .await
            .expect("read finalized local playlist"),
    )
    .expect("parse finalized local playlist");
    assert_eq!(final_playlist.segments.len(), 1);
    assert!(final_playlist.end_list);
    assert_eq!(
        tokio::fs::read(video_dir.join(&final_playlist.segments[0].uri))
            .await
            .expect("read original committed segment"),
        segment_bytes
    );
}

#[tokio::test]
async fn live_hls_snapshots_append_only_new_media_sequences_and_resume() {
    let root = TempRoot::new("live-hls");
    let video_dir = root.path().join("video");
    tokio::fs::create_dir_all(&video_dir)
        .await
        .expect("create live HLS video staging");
    let initial_body = b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:4,\n100.ts\n#EXTINF:4,\n101.ts\n";
    let next_body = b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:101\n#EXTINF:4,\n101.ts\n#EXTINF:4,\n102.ts\n#EXT-X-ENDLIST\n";
    let mut initial = crate::manifest::plan_hls_media_playlist(
        "https://origin.example.test/live.m3u8",
        initial_body,
    )
    .expect("plan initial live HLS window");
    let mut next = crate::manifest::plan_hls_media_playlist(
        "https://origin.example.test/live.m3u8",
        next_body,
    )
    .expect("plan overlapping live HLS window");
    let files = HashMap::from([
        ("/100.ts".to_string(), b"segment-100".to_vec()),
        ("/101.ts".to_string(), b"segment-101".to_vec()),
        ("/102.ts".to_string(), b"segment-102".to_vec()),
    ]);
    let (fixture_base, server) = spawn_fixture_server(files, 4).await;
    localize_live_plan_resources(&mut initial, &fixture_base);
    localize_live_plan_resources(&mut next, &fixture_base);

    let mut track = load_live_hls_track(&initial, &video_dir)
        .await
        .expect("initialize live HLS accumulator");
    let client = fixture_client();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let downloaded = Arc::new(AtomicU64::new(0));
    let (_stop_tx, mut abort) = tokio::sync::oneshot::channel();
    assert_eq!(
        apply_live_hls_snapshot(
            &client,
            &initial,
            &mut track,
            2,
            &tx,
            &downloaded,
            &mut abort,
            0,
        )
        .await
        .expect("fetch and commit initial live window"),
        LiveHlsSnapshotOutcome::Committed
    );
    assert_eq!(track.accumulator.segment_count(), 2);
    tokio::fs::remove_file(
        video_dir.join(
            m3u8_rs::parse_media_playlist_res(
                &tokio::fs::read(video_dir.join("local.m3u8"))
                    .await
                    .expect("read initial local live playlist"),
            )
            .expect("parse initial local live playlist")
            .segments[1]
                .uri
                .as_str(),
        ),
    )
    .await
    .expect("remove one cached overlapping segment to exercise resume repair");
    assert!(
        tokio::fs::symlink_metadata(video_dir.join("local.m3u8"))
            .await
            .is_ok(),
        "the committed local playlist must be durable before the next reload"
    );

    // A new attempt reconstructs the accumulator from the committed local
    // playlist. Its overlapping sequence must not be downloaded twice.
    let mut restored = load_live_hls_track(&next, &video_dir)
        .await
        .expect("restore live HLS accumulator from staging");
    assert_eq!(
        apply_live_hls_snapshot(
            &client,
            &next,
            &mut restored,
            2,
            &tx,
            &downloaded,
            &mut abort,
            0,
        )
        .await
        .expect("fetch and commit next live window"),
        LiveHlsSnapshotOutcome::Committed
    );
    wait_for_fixture_server(server).await;

    assert_eq!(restored.accumulator.segment_count(), 3);
    assert!(
        restored.ended,
        "the ENDLIST snapshot must stop further polling"
    );
    assert!(restored.next_poll_at.is_none());
    assert_eq!(
        downloaded.load(Ordering::Relaxed),
        b"segment-100".len() as u64 + b"segment-101".len() as u64 * 2 + b"segment-102".len() as u64,
        "the missing overlapping resource is repaired; all other cached segments are reused"
    );
    let local = tokio::fs::read(video_dir.join("local.m3u8"))
        .await
        .expect("read accumulated local playlist");
    let playlist =
        m3u8_rs::parse_media_playlist_res(&local).expect("accumulated local playlist parses");
    assert_eq!(playlist.media_sequence, 100);
    assert_eq!(playlist.segments.len(), 3);
    assert!(playlist.end_list);
    for (segment, expected) in playlist.segments.iter().zip([
        b"segment-100".as_slice(),
        b"segment-101".as_slice(),
        b"segment-102".as_slice(),
    ]) {
        assert_eq!(
            tokio::fs::read(video_dir.join(&segment.uri))
                .await
                .expect("read committed live segment"),
            expected
        );
    }
}

#[tokio::test]
async fn llhls_live_capture_reuses_parts_when_complete_parent_segments_arrive() {
    let root = TempRoot::new("llhls-parts");
    let video_dir = root.path().join("video");
    tokio::fs::create_dir_all(&video_dir)
        .await
        .expect("create LL-HLS video staging");

    let initial_body = br#"#EXTM3U
#EXT-X-VERSION:9
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:10
#EXT-X-MAP:URI="init.mp4"
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=NO,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXT-X-PART:DURATION=0.5,URI="10.0.m4s",INDEPENDENT=YES
#EXT-X-PART:DURATION=0.5,URI="10.1.m4s"
#EXTINF:1.0,
10.m4s
#EXT-X-PART:DURATION=0.5,URI="11.0.m4s"
"#;
    let next_body = br#"#EXTM3U
#EXT-X-VERSION:9
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:10
#EXT-X-MAP:URI="init.mp4"
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=NO,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXTINF:1.0,
10.m4s
#EXTINF:0.5,
11.m4s
#EXTINF:0.5,
12.m4s
#EXT-X-ENDLIST
"#;
    let mut initial = crate::manifest::plan_hls_media_playlist(
        "https://origin.example.test/live.m3u8",
        initial_body,
    )
    .expect("plan initial LL-HLS parts");
    let mut next = crate::manifest::plan_hls_media_playlist(
        "https://origin.example.test/live.m3u8",
        next_body,
    )
    .expect("plan parent-segment closure and next complete media segment");

    assert_eq!(initial.resources.len(), 4);
    assert_eq!(
        next.resources
            .iter()
            .filter(|resource| resource.kind == HlsResourceKind::Segment)
            .count(),
        3,
        "the remote snapshot describes two completed parents and one new media segment"
    );

    let files = HashMap::from([
        ("/init.mp4".to_string(), b"init-section".to_vec()),
        ("/10.0.m4s".to_string(), b"part-10-0".to_vec()),
        ("/10.1.m4s".to_string(), b"part-10-1".to_vec()),
        ("/11.0.m4s".to_string(), b"part-11-0".to_vec()),
        ("/12.m4s".to_string(), b"segment-12".to_vec()),
    ]);
    let (fixture_base, server) = spawn_fixture_server(files, 5).await;
    localize_live_plan_resources(&mut initial, &fixture_base);
    localize_live_plan_resources(&mut next, &fixture_base);

    let mut track = load_live_hls_track(&initial, &video_dir)
        .await
        .expect("initialize part-aware live track");
    let client = fixture_client();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let downloaded = Arc::new(AtomicU64::new(0));
    let (_stop_tx, mut abort) = tokio::sync::oneshot::channel();

    assert_eq!(
        apply_live_hls_snapshot(
            &client,
            &initial,
            &mut track,
            2,
            &tx,
            &downloaded,
            &mut abort,
            0,
        )
        .await
        .expect("fetch and commit confirmed LL-HLS parts"),
        LiveHlsSnapshotOutcome::Committed
    );
    assert_eq!(track.accumulator.segment_count(), 3);
    let first_local = m3u8_rs::parse_media_playlist_res(
        &tokio::fs::read(video_dir.join("local.m3u8"))
            .await
            .expect("read first LL-HLS local playlist"),
    )
    .expect("parse first LL-HLS local playlist");
    assert_eq!(first_local.segments.len(), 3);

    assert_eq!(
        apply_live_hls_snapshot(
            &client,
            &next,
            &mut track,
            2,
            &tx,
            &downloaded,
            &mut abort,
            0,
        )
        .await
        .expect("close existing parent segments without refetching them"),
        LiveHlsSnapshotOutcome::Committed
    );
    wait_for_fixture_server(server).await;

    assert_eq!(track.accumulator.segment_count(), 4);
    assert!(track.ended, "the ENDLIST snapshot ends live capture");
    assert!(track.next_poll_at.is_none());
    let final_local = m3u8_rs::parse_media_playlist_res(
        &tokio::fs::read(video_dir.join("local.m3u8"))
            .await
            .expect("read final LL-HLS local playlist"),
    )
    .expect("parse final LL-HLS local playlist");
    assert_eq!(final_local.segments.len(), 4);
    for (index, expected) in [
        b"part-10-0".as_slice(),
        b"part-10-1".as_slice(),
        b"part-11-0".as_slice(),
        b"segment-12".as_slice(),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            tokio::fs::read(video_dir.join(&final_local.segments[index].uri))
                .await
                .expect("read committed media unit"),
            expected
        );
    }
    assert_eq!(
        downloaded.load(Ordering::Relaxed),
        b"init-section".len() as u64
            + b"part-10-0".len() as u64
            + b"part-10-1".len() as u64
            + b"part-11-0".len() as u64
            + b"segment-12".len() as u64,
        "each confirmed part and shared init section must be fetched once"
    );
}

#[tokio::test]
async fn llhls_mapless_parts_are_committed_only_after_mpegts_validation() {
    let root = TempRoot::new("llhls-mapless-ts");
    let video_dir = root.path().join("video");
    tokio::fs::create_dir_all(&video_dir)
        .await
        .expect("create mapless LL-HLS staging");

    let mut transport_stream = vec![0xff; 188 * 3];
    for (index, packet) in transport_stream
        .as_chunks_mut::<188>()
        .0
        .iter_mut()
        .enumerate()
    {
        packet[0] = 0x47;
        packet[1] = 0;
        packet[2] = 0;
        packet[3] = 0x10 | (index as u8 & 0x0f);
    }

    let body = br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:5
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=NO,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXT-X-PART:DURATION=0.5,URI="5.0.ts",INDEPENDENT=YES
"#;
    let mut plan =
        crate::manifest::plan_hls_media_playlist("https://origin.example.test/live.m3u8", body)
            .expect("plan mapless LL-HLS MPEG-TS part");
    let mapless_parts = plan.ll_parts_without_init_map().collect::<Vec<_>>();
    assert_eq!(mapless_parts.len(), 1);
    assert!(mapless_parts[0].starts_with("segment-ll-00000000000000000005-part-000000-"));

    let files = HashMap::from([("/5.0.ts".to_string(), transport_stream.clone())]);
    let (fixture_base, server) = spawn_fixture_server(files, 1).await;
    localize_live_plan_resources(&mut plan, &fixture_base);
    let mut track = load_live_hls_track(&plan, &video_dir)
        .await
        .expect("initialize mapless LL-HLS track");
    let client = fixture_client();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let downloaded = Arc::new(AtomicU64::new(0));
    let (_stop_tx, mut abort) = tokio::sync::oneshot::channel();

    assert_eq!(
        apply_live_hls_snapshot(
            &client,
            &plan,
            &mut track,
            1,
            &tx,
            &downloaded,
            &mut abort,
            0,
        )
        .await
        .expect("validate and commit mapless MPEG-TS part"),
        LiveHlsSnapshotOutcome::Committed
    );
    wait_for_fixture_server(server).await;
    assert_eq!(track.accumulator.segment_count(), 1);
    let local = m3u8_rs::parse_media_playlist_res(
        &tokio::fs::read(video_dir.join("local.m3u8"))
            .await
            .expect("read mapless local playlist"),
    )
    .expect("parse mapless local playlist");
    assert!(local.segments[0].map.is_none());
    assert_eq!(
        tokio::fs::read(video_dir.join(&local.segments[0].uri))
            .await
            .expect("read committed mapless TS part"),
        transport_stream
    );
    assert_eq!(downloaded.load(Ordering::Relaxed), 188 * 3);
}

#[tokio::test]
async fn llhls_mapless_non_transport_stream_part_is_rejected_before_commit() {
    let root = TempRoot::new("llhls-mapless-invalid");
    let video_dir = root.path().join("video");
    tokio::fs::create_dir_all(&video_dir)
        .await
        .expect("create invalid mapless LL-HLS staging");
    let plan = crate::manifest::plan_hls_media_playlist(
        "https://origin.example.test/live.m3u8",
        br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:6
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=NO,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXT-X-PART:DURATION=0.5,URI="6.0.m4s",INDEPENDENT=YES
"#,
    )
    .expect("plan mapless candidate part");
    let (fixture_base, server) = spawn_fixture_server(
        HashMap::from([("/6.0.m4s".to_string(), b"not an MPEG-TS fragment".to_vec())]),
        1,
    )
    .await;
    let mut plan = plan;
    localize_live_plan_resources(&mut plan, &fixture_base);
    let mut track = load_live_hls_track(&plan, &video_dir)
        .await
        .expect("initialize invalid mapless track");
    let client = fixture_client();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let downloaded = Arc::new(AtomicU64::new(0));
    let (_stop_tx, mut abort) = tokio::sync::oneshot::channel();

    assert!(matches!(
        apply_live_hls_snapshot(
            &client,
            &plan,
            &mut track,
            1,
            &tx,
            &downloaded,
            &mut abort,
            0,
        )
        .await,
        Err(ManifestRunError::Unsupported(_))
    ));
    wait_for_fixture_server(server).await;
    assert_eq!(track.accumulator.segment_count(), 0);
    assert!(
        tokio::fs::symlink_metadata(video_dir.join("local.m3u8"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn live_hls_accumulated_playlist_finalizes_to_a_playable_mp4() {
    if !tools_available() {
        return;
    }

    let root = TempRoot::new("live-finalize");
    let fixture_dir = root.path().join("fixture");
    std::fs::create_dir_all(&fixture_dir).expect("create live HLS fixture directory");
    let source_playlist_path = fixture_dir.join("source.m3u8");
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
            "4",
            "-shortest",
            "-c:v",
            "mpeg2video",
            "-q:v",
            "5",
            "-g",
            "12",
            "-c:a",
            "aac",
            "-b:a",
            "64k",
            "-f",
            "hls",
            "-hls_time",
            "1",
            "-hls_list_size",
            "0",
            "-hls_segment_filename",
        ])
        .arg(fixture_dir.join("segment%03d.ts"))
        .arg(&source_playlist_path)
        .output()
        .expect("generate MPEG-TS HLS source fixture");
    assert_command_succeeded(&generated, "FFmpeg could not generate the live HLS fixture");

    let source_bytes =
        std::fs::read(&source_playlist_path).expect("read generated VOD HLS playlist");
    let source =
        m3u8_rs::parse_media_playlist_res(&source_bytes).expect("parse generated VOD HLS playlist");
    assert!(
        source.segments.len() >= 3,
        "fixture generation must produce at least three media segments"
    );

    let mut initial = source.clone();
    initial.segments.truncate(2);
    initial.playlist_type = None;
    initial.end_list = false;
    let mut initial_bytes = Vec::new();
    initial
        .write_to(&mut initial_bytes)
        .expect("serialize first live HLS window");

    let final_bytes = source_bytes;
    let mut initial_plan = crate::manifest::plan_hls_media_playlist(
        "https://origin.example.test/live.m3u8",
        &initial_bytes,
    )
    .expect("plan first live HLS window");
    let mut final_plan = crate::manifest::plan_hls_media_playlist(
        "https://origin.example.test/live.m3u8",
        &final_bytes,
    )
    .expect("plan final ENDLIST snapshot");

    let mut resource_urls = initial_plan
        .resources
        .iter()
        .chain(final_plan.resources.iter())
        .map(|resource| resource.url.clone())
        .collect::<Vec<_>>();
    resource_urls.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    resource_urls.dedup_by(|left, right| left.as_str() == right.as_str());
    let files = fixture_files(&fixture_dir, &resource_urls);
    let expected_requests = files.len();
    let (fixture_base, server) = spawn_fixture_server(files, expected_requests).await;
    localize_live_plan_resources(&mut initial_plan, &fixture_base);
    localize_live_plan_resources(&mut final_plan, &fixture_base);

    let video_dir = root.path().join("video");
    tokio::fs::create_dir_all(&video_dir)
        .await
        .expect("create live HLS staging");
    let mut track = load_live_hls_track(&initial_plan, &video_dir)
        .await
        .expect("initialize live HLS accumulator");
    let client = fixture_client();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let downloaded = Arc::new(AtomicU64::new(0));
    let (_stop_tx, mut abort) = tokio::sync::oneshot::channel();
    assert_eq!(
        apply_live_hls_snapshot(
            &client,
            &initial_plan,
            &mut track,
            4,
            &tx,
            &downloaded,
            &mut abort,
            0,
        )
        .await
        .expect("fetch first live HLS window"),
        LiveHlsSnapshotOutcome::Committed
    );
    assert_eq!(
        apply_live_hls_snapshot(
            &client,
            &final_plan,
            &mut track,
            4,
            &tx,
            &downloaded,
            &mut abort,
            0,
        )
        .await
        .expect("fetch final ENDLIST HLS window"),
        LiveHlsSnapshotOutcome::Committed
    );
    wait_for_fixture_server(server).await;

    assert!(track.ended, "the ENDLIST snapshot closes the capture");
    assert_eq!(track.accumulator.segment_count(), source.segments.len());

    let ffmpeg = Path::new("ffmpeg");
    let ffprobe = Path::new("ffprobe");
    let output = root.path().join("live-capture.part.mp4");
    assert!(
        tokio::time::timeout(
            FIXTURE_TIMEOUT,
            run_ffmpeg(
                ffmpeg,
                &video_dir.join("local.m3u8"),
                None,
                SubtitleOutput::default(),
                &output,
                &mut abort,
            ),
        )
        .await
        .expect("FFmpeg finalization timed out")
        .expect("FFmpeg live HLS finalization returned an error"),
        "FFmpeg finalization should complete without an abort"
    );
    assert_has_audio_and_video(ffprobe, &output);
}

#[tokio::test]
async fn live_hls_sequence_gap_preserves_already_committed_media() {
    let root = TempRoot::new("live-gap");
    let video_dir = root.path().join("video");
    tokio::fs::create_dir_all(&video_dir)
        .await
        .expect("create live gap staging");
    let first = crate::manifest::plan_hls_media_playlist(
        "https://origin.example.test/live.m3u8",
        b"#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:700\n#EXTINF:4,\n700.ts\n#EXTINF:4,\n701.ts\n",
    )
    .expect("plan first live window");
    let mut track = load_live_hls_track(&first, &video_dir)
        .await
        .expect("initialize live accumulator");
    let update = track
        .accumulator
        .prepare_update(&first)
        .expect("prepare first live window");
    track
        .accumulator
        .commit_update_with_budget(update, 0, 0)
        .expect("commit first live window");
    write_live_playlist(&video_dir, &track.accumulator)
        .await
        .expect("persist first live window");

    let gap = crate::manifest::plan_hls_media_playlist(
        "https://origin.example.test/live.m3u8",
        b"#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:704\n#EXTINF:4,\n704.ts\n#EXTINF:4,\n705.ts\n",
    )
    .expect("plan live window after a gap");
    let client = fixture_client();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let downloaded = Arc::new(AtomicU64::new(0));
    let (_stop_tx, mut abort) = tokio::sync::oneshot::channel();
    let outcome = apply_live_hls_snapshot(
        &client,
        &gap,
        &mut track,
        1,
        &tx,
        &downloaded,
        &mut abort,
        0,
    )
    .await
    .expect("continuity loss is a partial capture, not extractor fallback");

    assert_eq!(outcome, LiveHlsSnapshotOutcome::ContinuityLost);
    assert_eq!(track.accumulator.segment_count(), 2);
    assert!(
        tokio::fs::symlink_metadata(video_dir.join("local.m3u8"))
            .await
            .is_ok(),
        "the last complete playlist window remains available for finalization"
    );
}

#[tokio::test]
async fn live_hls_pause_keeps_the_last_committed_window() {
    let source = live_hls_test_source("https://origin.example.test/live.m3u8");
    let root = TempRoot::new("live-preserve");
    let video_dir = root.path().join("video");
    let audio_dir = root.path().join("audio");
    tokio::fs::create_dir_all(&audio_dir)
        .await
        .expect("create live HLS audio staging");
    tokio::fs::create_dir_all(&video_dir)
        .await
        .expect("create live preserve staging");
    let first_body =
        b"#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:550\n#EXTINF:4,\n550.ts\n";
    let next_body = b"#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:550\n#EXTINF:4,\n550.ts\n#EXTINF:4,\n551.ts\n";
    let mut first = crate::manifest::plan_hls_media_playlist(
        "https://origin.example.test/live.m3u8",
        first_body,
    )
    .expect("plan first live window");
    let mut next = crate::manifest::plan_hls_media_playlist(
        "https://origin.example.test/live.m3u8",
        next_body,
    )
    .expect("plan next live window");
    let (fixture_base, server) = spawn_fixture_server(
        HashMap::from([("/550.ts".to_string(), b"committed-segment".to_vec())]),
        1,
    )
    .await;
    localize_live_plan_resources(&mut first, &fixture_base);
    localize_live_plan_resources(&mut next, &fixture_base);

    let mut track = load_live_hls_track(&first, &video_dir)
        .await
        .expect("initialize live HLS accumulator");
    let client = fixture_client();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let downloaded = Arc::new(AtomicU64::new(0));
    let (_first_stop_tx, mut first_abort) = tokio::sync::oneshot::channel();
    assert_eq!(
        apply_live_hls_snapshot(
            &client,
            &first,
            &mut track,
            1,
            &tx,
            &downloaded,
            &mut first_abort,
            0,
        )
        .await
        .expect("fetch and commit first live window"),
        LiveHlsSnapshotOutcome::Committed
    );
    wait_for_fixture_server(server).await;

    let (stop_tx, mut stop_abort) = tokio::sync::oneshot::channel();
    stop_tx
        .send(crate::video::StopIntent::Pause)
        .expect("queue pause before the next window");
    assert_eq!(
        apply_live_hls_snapshot(
            &client,
            &next,
            &mut track,
            1,
            &tx,
            &downloaded,
            &mut stop_abort,
            0,
        )
        .await
        .expect("pause during the next live window"),
        LiveHlsSnapshotOutcome::Stopped(crate::video::StopIntent::Pause)
    );

    let local = tokio::fs::read(video_dir.join("local.m3u8"))
        .await
        .expect("read last committed live playlist");
    let playlist =
        m3u8_rs::parse_media_playlist_res(&local).expect("parse last committed live playlist");
    assert_eq!(playlist.segments.len(), 1);
    assert_eq!(track.accumulator.segment_count(), 1);
    assert_eq!(
        tokio::fs::read(video_dir.join(&playlist.segments[0].uri))
            .await
            .expect("read last committed live segment"),
        b"committed-segment"
    );

    let (finish_tx, mut finish_abort) = tokio::sync::oneshot::channel();
    finish_tx
        .send(crate::video::StopIntent::Finish)
        .expect("queue finish after pausing the committed capture");
    let mut guarded_child_hosts = crate::download_net::PublicResolutionSet::default();
    let finished = run_live_hls_tracks(
        &client,
        "https://origin.example.test/live.m3u8",
        &source,
        None,
        root.path(),
        root.path(),
        0,
        root.path(),
        &first,
        None,
        false,
        &video_dir,
        &audio_dir,
        1,
        &tx,
        &downloaded,
        &mut guarded_child_hosts,
        &mut finish_abort,
    )
    .await
    .expect("finish the restored live HLS capture");
    assert_eq!(finished.outcome, LiveHlsCaptureOutcome::Finish);
    assert_eq!(finished.output, super::LiveHlsOutput::LocalPlaylist);
}

#[tokio::test]
async fn live_hls_runner_returns_paused_without_finalizing() {
    let source = live_hls_test_source("https://origin.example.test/live.m3u8");
    let root = TempRoot::new("live-pause-runner");
    let video_dir = root.path().join("video");
    let audio_dir = root.path().join("audio");
    tokio::fs::create_dir_all(&video_dir)
        .await
        .expect("create live HLS video staging");
    tokio::fs::create_dir_all(&audio_dir)
        .await
        .expect("create live HLS audio staging");
    let plan = crate::manifest::plan_hls_media_playlist(
        "https://origin.example.test/live.m3u8",
        b"#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:500\n#EXTINF:4,\n500.ts\n",
    )
    .expect("plan a live media window");
    let client = fixture_client();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let downloaded = Arc::new(AtomicU64::new(0));
    let (stop_tx, mut abort) = tokio::sync::oneshot::channel();
    stop_tx
        .send(crate::video::StopIntent::Pause)
        .expect("queue pause before the first live HLS request");
    let mut guarded_child_hosts = crate::download_net::PublicResolutionSet::default();

    let outcome = run_live_hls_tracks(
        &client,
        "https://origin.example.test/live.m3u8",
        &source,
        None,
        root.path(),
        root.path(),
        0,
        root.path(),
        &plan,
        None,
        false,
        &video_dir,
        &audio_dir,
        1,
        &tx,
        &downloaded,
        &mut guarded_child_hosts,
        &mut abort,
    )
    .await
    .expect("pause the live HLS runner");

    assert_eq!(outcome.outcome, LiveHlsCaptureOutcome::Paused);
    assert!(
        tokio::fs::symlink_metadata(video_dir.join("local.m3u8"))
            .await
            .is_err(),
        "pausing before the first commit must not create an FFmpeg input"
    );
}

#[tokio::test]
async fn live_hls_pause_does_not_commit_an_interrupted_window() {
    let root = TempRoot::new("live-stop");
    let video_dir = root.path().join("video");
    tokio::fs::create_dir_all(&video_dir)
        .await
        .expect("create live stop staging");
    let plan = crate::manifest::plan_hls_media_playlist(
        "https://origin.example.test/live.m3u8",
        b"#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:500\n#EXTINF:4,\n500.ts\n",
    )
    .expect("plan a live media window");
    let mut track = load_live_hls_track(&plan, &video_dir)
        .await
        .expect("initialize live HLS accumulator");
    let client = fixture_client();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let downloaded = Arc::new(AtomicU64::new(0));
    let (stop_tx, mut abort) = tokio::sync::oneshot::channel();
    stop_tx
        .send(crate::video::StopIntent::Pause)
        .expect("queue pause");
    let stopped = apply_live_hls_snapshot(
        &client,
        &plan,
        &mut track,
        1,
        &tx,
        &downloaded,
        &mut abort,
        0,
    )
    .await
    .expect("a stop before fetching is not a manifest error");

    assert_eq!(
        stopped,
        LiveHlsSnapshotOutcome::Stopped(crate::video::StopIntent::Pause)
    );
    assert_eq!(track.accumulator.segment_count(), 0);
    assert!(
        tokio::fs::symlink_metadata(video_dir.join("local.m3u8"))
            .await
            .is_err(),
        "an incomplete first window must never become the FFmpeg input"
    );
    assert_eq!(downloaded.load(Ordering::Relaxed), 0);
}

fn dynamic_dash_fixture_plan(server_seconds: u64) -> DashRepresentationPlan {
    let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     type="dynamic"
     availabilityStartTime="1970-01-01T00:00:00Z"
     minimumUpdatePeriod="PT4S"
     timeShiftBufferDepth="PT20S">
  <Period start="PT0S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1" startNumber="1" duration="4" media="seg-$Number$.m4s"/>
      <Representation id="v1" bandwidth="1000000" width="1280" height="720"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
    let manifest = crate::manifest::dash::parse_dash_manifest_at(
        "https://cdn.fixture.test/path/manifest.mpd",
        body,
        UNIX_EPOCH + Duration::from_secs(server_seconds),
    )
    .expect("parse dynamic DASH fixture MPD");
    let representation = manifest
        .video_representations
        .first()
        .expect("dynamic video representation");
    crate::manifest::dash::plan_dash_representation(&manifest, &representation.live_key)
        .expect("plan dynamic DASH fixture segments")
}

fn low_latency_dash_fixture_plan(server_seconds: u64) -> DashRepresentationPlan {
    let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     type="dynamic"
     availabilityStartTime="1970-01-01T00:00:00Z"
     minimumUpdatePeriod="PT4S"
     timeShiftBufferDepth="PT20S">
  <Period start="PT0S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1" startNumber="1" duration="4"
                       initialization="init.mp4"
                       media="seg-$Number$.m4s"
                       availabilityTimeOffset="4"
                       availabilityTimeComplete="false"/>
      <Representation id="v1" bandwidth="1000000" width="1280" height="720"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
    let manifest = crate::manifest::dash::parse_dash_manifest_at(
        "https://cdn.fixture.test/path/manifest.mpd",
        body,
        UNIX_EPOCH + Duration::from_secs(server_seconds),
    )
    .expect("parse low-latency DASH fixture MPD");
    let representation = manifest
        .video_representations
        .first()
        .expect("low-latency DASH video representation");
    crate::manifest::dash::plan_dash_representation(&manifest, &representation.live_key)
        .expect("plan low-latency DASH fixture resources")
}

#[tokio::test]
async fn dynamic_dash_snapshot_resumes_overlapping_window_and_repairs_missing_segment() {
    let root = TempRoot::new("dynamic-dash-resume");
    let fixture_dir = root.path().join("fixture");
    let video_dir = root.path().join("video");
    std::fs::create_dir_all(&fixture_dir).expect("create DASH fixture files");
    tokio::fs::create_dir_all(&video_dir)
        .await
        .expect("create dynamic DASH staging");

    let mut initial = dynamic_dash_fixture_plan(120);
    let mut next = dynamic_dash_fixture_plan(124);
    let initial_numbers = initial
        .resources
        .iter()
        .filter_map(|resource| resource.segment_number)
        .collect::<Vec<_>>();
    let next_numbers = next
        .resources
        .iter()
        .filter_map(|resource| resource.segment_number)
        .collect::<Vec<_>>();
    assert_eq!(initial_numbers, [26, 27, 28, 29, 30]);
    assert_eq!(next_numbers, [27, 28, 29, 30, 31]);
    assert_eq!(
        initial.representation.live_key, next.representation.live_key,
        "the selected representation identity must survive a sliding MPD window"
    );

    let mut urls = initial
        .resources
        .iter()
        .chain(next.resources.iter())
        .map(|resource| resource.url.clone())
        .collect::<Vec<_>>();
    urls.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    urls.dedup_by(|left, right| left.as_str() == right.as_str());
    for number in 26..=31 {
        std::fs::write(
            fixture_dir.join(format!("seg-{number}.m4s")),
            format!("complete-segment-{number}"),
        )
        .expect("write dynamic DASH fixture segment");
    }
    let files = fixture_files(&fixture_dir, &urls);
    let (fixture_base, server) = spawn_fixture_server(files, 7).await;
    for plan in [&mut initial, &mut next] {
        for resource in plan.resources.iter_mut() {
            resource.url = loopback_resource_url(&fixture_base, &resource.url);
        }
    }

    let representation_key = initial.representation.live_key.clone();
    let (mut track, _) = load_dash_live_track(
        &representation_key,
        &video_dir,
        Some(&initial),
        "video-track.mp4",
    )
    .await
    .expect("create first dynamic DASH capture state");
    let client = fixture_client();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let downloaded = Arc::new(AtomicU64::new(0));
    let (_stop_tx, mut abort) = tokio::sync::oneshot::channel();

    assert_eq!(
        apply_dash_live_snapshot(
            &client,
            &initial,
            &mut track,
            3,
            &tx,
            &downloaded,
            &mut abort,
            0,
        )
        .await
        .expect("download and commit first dynamic DASH window"),
        DashLiveUpdateOutcome::Committed
    );
    assert_eq!(
        track
            .state
            .segments
            .iter()
            .map(|segment| segment.number)
            .collect::<Vec<_>>(),
        initial_numbers
    );

    let missing = track.state.segments[2].filename.clone();
    tokio::fs::remove_file(video_dir.join(&missing))
        .await
        .expect("remove one overlapping media segment to exercise repair");
    let (mut restored, retained_bytes) = load_dash_live_track(
        &representation_key,
        &video_dir,
        Some(&next),
        "video-track.mp4",
    )
    .await
    .expect("restore dynamic DASH state with one missing cache file");
    assert!(retained_bytes > 0);
    assert_eq!(restored.state.segments.len(), 5);

    assert_eq!(
        apply_dash_live_snapshot(
            &client,
            &next,
            &mut restored,
            3,
            &tx,
            &downloaded,
            &mut abort,
            0,
        )
        .await
        .expect("repair missing overlap and append the new sequence"),
        DashLiveUpdateOutcome::Committed
    );
    wait_for_fixture_server(server).await;

    assert_eq!(
        restored
            .state
            .segments
            .iter()
            .map(|segment| segment.number)
            .collect::<Vec<_>>(),
        [26, 27, 28, 29, 30, 31]
    );
    assert_eq!(
        downloaded.load(Ordering::Relaxed),
        (26..=31)
            .map(|number| format!("complete-segment-{number}").len() as u64)
            .sum::<u64>(),
        "only new media and the deliberately missing overlap should be fetched"
    );
    for segment in &restored.state.segments {
        assert_eq!(
            tokio::fs::read(video_dir.join(&segment.filename))
                .await
                .expect("read captured dynamic DASH resource"),
            format!("complete-segment-{}", segment.number).as_bytes()
        );
    }
    let state_path = video_dir.join(DASH_LIVE_STATE_NAME);
    let state_bytes = tokio::fs::read(&state_path)
        .await
        .expect("read committed dynamic DASH state");
    let state_text = String::from_utf8(state_bytes).expect("state file is UTF-8 JSON");
    assert!(
        !state_text.contains("https://"),
        "signed resource URLs must not be persisted in the live resume state"
    );
}

#[tokio::test]
async fn low_latency_dash_restores_legacy_policy_key_as_stable_identity() {
    let root = TempRoot::new("lldash-legacy-key");
    let video_dir = root.path().join("video");
    tokio::fs::create_dir_all(&video_dir)
        .await
        .expect("create legacy LL-DASH staging directory");
    let plan = low_latency_dash_fixture_plan(120);
    let legacy_key = plan
        .representation
        .legacy_live_key
        .as_deref()
        .expect("LL-DASH has a legacy policy-sensitive key")
        .to_string();
    let stable_key = plan.representation.live_key.clone();
    assert_ne!(legacy_key, stable_key);
    let first_segment = plan
        .resources
        .iter()
        .find(|resource| resource.kind == DashResourceKind::Segment)
        .expect("legacy plan has a first media resource");
    tokio::fs::write(
        video_dir.join(&first_segment.filename),
        b"legacy-captured-segment",
    )
    .await
    .expect("write a resource from the legacy capture");
    let mut legacy_state = DashLiveState::new(&legacy_key);
    legacy_state.segments.push(DashLiveSegment {
        number: first_segment.segment_number.expect("media segment number"),
        filename: first_segment.filename.clone(),
    });
    let legacy_bytes =
        serde_json::to_vec(&legacy_state).expect("serialize previous live DASH state");
    tokio::fs::write(video_dir.join(DASH_LIVE_STATE_NAME), legacy_bytes)
        .await
        .expect("persist previous policy-sensitive representation key");

    let (track, retained_bytes) =
        load_dash_live_track(&legacy_key, &video_dir, Some(&plan), "video-track.mp4")
            .await
            .expect("restore a queue item carrying the previous LL-DASH key");

    assert_eq!(
        track.state.representation_key.as_str(),
        stable_key,
        "restored live state is immediately migrated to the policy-independent key"
    );
    assert!(
        retained_bytes > 0,
        "migration preserves prior captured bytes"
    );
    assert_eq!(
        track.state.segments.len(),
        1,
        "migration preserves the segment journal"
    );
    let saved = tokio::fs::read(video_dir.join(DASH_LIVE_STATE_NAME))
        .await
        .expect("read migrated live DASH state");
    let saved: DashLiveState =
        serde_json::from_slice(&saved).expect("parse migrated live DASH state");
    assert_eq!(saved.representation_key, track.state.representation_key);
}

#[tokio::test]
async fn low_latency_dash_fetches_initialization_then_oldest_segments_with_one_request() {
    let root = TempRoot::new("lldash-order");
    let fixture_dir = root.path().join("fixture");
    let video_dir = root.path().join("video");
    std::fs::create_dir_all(&fixture_dir).expect("create LL-DASH fixture files");
    tokio::fs::create_dir_all(&video_dir)
        .await
        .expect("create LL-DASH staging directory");

    let mut plan = low_latency_dash_fixture_plan(120);
    assert!(plan.low_latency);
    assert_eq!(
        plan.resources
            .iter()
            .filter_map(|resource| resource.segment_number)
            .collect::<Vec<_>>(),
        [26, 27, 28, 29, 30, 31]
    );
    for resource in &plan.resources {
        let body = if resource.kind == DashResourceKind::Initialization {
            b"initialization-resource".to_vec()
        } else {
            format!(
                "complete-segment-{}",
                resource.segment_number.expect("media segment number")
            )
            .into_bytes()
        };
        let fixture_name = resource
            .url
            .path()
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .expect("LL-DASH fixture URL has a filename");
        std::fs::write(fixture_dir.join(fixture_name), body)
            .expect("write LL-DASH fixture resource");
    }

    let original_urls = plan
        .resources
        .iter()
        .map(|resource| resource.url.clone())
        .collect::<Vec<_>>();
    let files = fixture_files(&fixture_dir, &original_urls);
    let OrderedDashFixtureServer {
        base_url,
        first_media_request,
        release_first_media,
        task: server,
    } = spawn_ordered_dash_fixture_server(files, plan.resources.len()).await;
    for resource in plan.resources.iter_mut() {
        resource.url = loopback_resource_url(&base_url, &resource.url);
    }

    let representation_key = plan.representation.live_key.clone();
    let (mut track, _) = load_dash_live_track(
        &representation_key,
        &video_dir,
        Some(&plan),
        "video-track.mp4",
    )
    .await
    .expect("create LL-DASH live track state");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let downloaded = Arc::new(AtomicU64::new(0));
    let (_stop_tx, mut abort) = tokio::sync::oneshot::channel();
    let client = fixture_client();

    let apply = tokio::spawn(async move {
        let outcome = apply_dash_live_snapshot(
            &client,
            &plan,
            &mut track,
            8,
            &tx,
            &downloaded,
            &mut abort,
            0,
        )
        .await?;
        Ok::<_, ManifestRunError>((outcome, track))
    });
    let first_request = tokio::time::timeout(FIXTURE_TIMEOUT, first_media_request)
        .await
        .expect("LL-DASH did not start the first media request")
        .expect("LL-DASH request observer exited early");
    assert_eq!(
        first_request, "/seg-26.m4s",
        "the oldest retained media segment is requested before the progressive tail"
    );
    release_first_media
        .send(())
        .expect("release the first progressive media response");

    let (outcome, track) = tokio::time::timeout(FIXTURE_TIMEOUT, apply)
        .await
        .expect("LL-DASH snapshot did not finish")
        .expect("LL-DASH snapshot task panicked")
        .expect("apply LL-DASH snapshot");
    let (requests, concurrent) = tokio::time::timeout(FIXTURE_TIMEOUT, server)
        .await
        .expect("ordered LL-DASH fixture server timed out")
        .expect("ordered LL-DASH fixture server failed");
    assert_eq!(outcome, DashLiveUpdateOutcome::Committed);
    assert!(
        !concurrent,
        "LL-DASH must not open another request while the progressive response is active"
    );
    assert_eq!(
        requests,
        vec![
            "/init.mp4".to_string(),
            "/seg-26.m4s".to_string(),
            "/seg-27.m4s".to_string(),
            "/seg-28.m4s".to_string(),
            "/seg-29.m4s".to_string(),
            "/seg-30.m4s".to_string(),
            "/seg-31.m4s".to_string(),
        ],
        "initialization is followed by oldest-first media requests"
    );
    assert_eq!(
        track
            .state
            .segments
            .iter()
            .map(|segment| segment.number)
            .collect::<Vec<_>>(),
        [26, 27, 28, 29, 30, 31],
        "the journal preserves presentation order"
    );
}

#[tokio::test]
async fn low_latency_dash_does_not_commit_interrupted_tail_and_refetches_on_resume() {
    let root = TempRoot::new("lldash-interrupt");
    let fixture_dir = root.path().join("fixture");
    let video_dir = root.path().join("video");
    std::fs::create_dir_all(&fixture_dir).expect("create LL-DASH fixture files");
    tokio::fs::create_dir_all(&video_dir)
        .await
        .expect("create LL-DASH staging directory");

    let mut initial = low_latency_dash_fixture_plan(120);
    let mut refreshed = low_latency_dash_fixture_plan(120);
    initial.resources = initial
        .resources
        .iter()
        .filter(|resource| resource.segment_number != Some(31))
        .cloned()
        .collect::<Vec<_>>()
        .into_boxed_slice();
    for resource in &initial.resources {
        let body = if resource.kind == DashResourceKind::Initialization {
            b"initialization-resource".to_vec()
        } else {
            format!(
                "complete-segment-{}",
                resource
                    .segment_number
                    .expect("complete media segment number")
            )
            .into_bytes()
        };
        let fixture_name = resource
            .url
            .path()
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .expect("LL-DASH fixture URL has a filename");
        std::fs::write(fixture_dir.join(fixture_name), body)
            .expect("write complete LL-DASH backlog resource");
    }

    let original_urls = initial
        .resources
        .iter()
        .map(|resource| resource.url.clone())
        .collect::<Vec<_>>();
    let (fixture_base, fixture_server) = spawn_fixture_server(
        fixture_files(&fixture_dir, &original_urls),
        initial.resources.len(),
    )
    .await;
    for resource in initial.resources.iter_mut() {
        resource.url = loopback_resource_url(&fixture_base, &resource.url);
    }
    for resource in refreshed.resources.iter_mut() {
        if resource.segment_number != Some(31) {
            resource.url = loopback_resource_url(&fixture_base, &resource.url);
        }
    }

    let representation_key = initial.representation.live_key.clone();
    let (mut track, _) = load_dash_live_track(
        &representation_key,
        &video_dir,
        Some(&initial),
        "video-track.mp4",
    )
    .await
    .expect("create LL-DASH state before the progressive tail is captured");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let downloaded = Arc::new(AtomicU64::new(0));
    let client = fixture_client();
    let (_initial_stop_tx, mut initial_abort) = tokio::sync::oneshot::channel();
    assert_eq!(
        apply_dash_live_snapshot(
            &client,
            &initial,
            &mut track,
            8,
            &tx,
            &downloaded,
            &mut initial_abort,
            0,
        )
        .await
        .expect("capture complete LL-DASH backlog"),
        DashLiveUpdateOutcome::Committed
    );
    wait_for_fixture_server(fixture_server).await;
    assert_eq!(track.state.segments.len(), 5);

    let body = vec![0x5a; INTERRUPTED_RESOURCE_BYTES];
    let InterruptedResourceServer {
        base_url: slow_base_url,
        partial_sent,
        release_first,
        task: slow_server,
    } = spawn_interrupted_resource_server(body.clone(), INTERRUPTED_RESOURCE_PREFIX_BYTES).await;
    let tail = refreshed
        .resources
        .iter_mut()
        .find(|resource| resource.segment_number == Some(31))
        .expect("refreshed MPD includes the progressive tail");
    tail.url = slow_base_url
        .join("seg-31.m4s")
        .expect("build progressive tail URL");
    let tail_filename = tail.filename.clone();
    let resume_plan = refreshed.clone();

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let attempt_client = client.clone();
    let attempt_tx = tx.clone();
    let attempt_downloaded = Arc::clone(&downloaded);
    let attempt = tokio::spawn(async move {
        let mut abort = stop_rx;
        let outcome = apply_dash_live_snapshot(
            &attempt_client,
            &refreshed,
            &mut track,
            8,
            &attempt_tx,
            &attempt_downloaded,
            &mut abort,
            0,
        )
        .await?;
        Ok::<_, ManifestRunError>((outcome, track))
    });

    tokio::time::timeout(FIXTURE_TIMEOUT, partial_sent)
        .await
        .expect("progressive LL-DASH tail did not send its first chunk")
        .expect("progressive tail server stopped before sending a chunk");
    let partial_path = video_dir.join(format!("{tail_filename}.part"));
    tokio::time::timeout(FIXTURE_TIMEOUT, async {
        loop {
            if tokio::fs::metadata(&partial_path)
                .await
                .is_ok_and(|metadata| metadata.len() >= INTERRUPTED_RESOURCE_PREFIX_BYTES as u64)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("progressive tail bytes were not written to its .part file");

    stop_tx
        .send(crate::video::StopIntent::Pause)
        .expect("pause the in-progress LL-DASH segment");
    let (stopped, mut track) = tokio::time::timeout(FIXTURE_TIMEOUT, attempt)
        .await
        .expect("LL-DASH did not stop the in-progress resource")
        .expect("LL-DASH stop task panicked")
        .expect("stop interrupted LL-DASH snapshot");
    assert_eq!(
        stopped,
        DashLiveUpdateOutcome::Stopped(crate::video::StopIntent::Pause)
    );
    assert_eq!(
        track
            .state
            .segments
            .iter()
            .map(|segment| segment.number)
            .collect::<Vec<_>>(),
        [26, 27, 28, 29, 30],
        "the open segment must not enter the durable live journal"
    );
    assert!(
        !track
            .state
            .segments
            .iter()
            .any(|segment| segment.filename == tail_filename),
        "an in-progress response must not be journal-committed"
    );
    assert!(
        !video_dir.join(&tail_filename).exists(),
        "an incomplete response must not be published under its final filename"
    );
    let saved_bytes = tokio::fs::read(video_dir.join(DASH_LIVE_STATE_NAME))
        .await
        .expect("read persisted state after interrupted tail");
    let saved_state: DashLiveState =
        serde_json::from_slice(&saved_bytes).expect("parse persisted state after interruption");
    assert_eq!(
        saved_state
            .segments
            .iter()
            .map(|segment| segment.number)
            .collect::<Vec<_>>(),
        [26, 27, 28, 29, 30],
        "the persisted journal must not include the incomplete tail"
    );
    release_first
        .send(())
        .expect("release the cancelled progressive response");

    let (_resume_stop_tx, mut resume_abort) = tokio::sync::oneshot::channel();
    assert_eq!(
        apply_dash_live_snapshot(
            &client,
            &resume_plan,
            &mut track,
            8,
            &tx,
            &downloaded,
            &mut resume_abort,
            0,
        )
        .await
        .expect("resume the interrupted LL-DASH tail"),
        DashLiveUpdateOutcome::Committed
    );
    let requests = tokio::time::timeout(FIXTURE_TIMEOUT, slow_server)
        .await
        .expect("progressive tail was not requested again")
        .expect("progressive tail fixture server failed");
    assert_eq!(
        requests,
        vec!["/seg-31.m4s".to_string(), "/seg-31.m4s".to_string()],
        "resume fetches the incomplete segment again from byte zero"
    );
    assert!(
        track
            .state
            .segments
            .iter()
            .any(|segment| segment.filename == tail_filename),
        "the tail is committed only after the second response closes"
    );
    assert_eq!(track.state.segments.len(), 6);
    assert_eq!(
        tokio::fs::read(video_dir.join(tail_filename))
            .await
            .expect("read the resumed progressive tail"),
        body
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
        let mut guarded_child_hosts = crate::download_net::PublicResolutionSet::default();
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
            SubtitleOutput::default(),
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
    assert_has_audio_and_video(Path::new("ffprobe"), &output);

    let mkv_part = root.path().join("hls-output.part.mkv");
    assert!(
        run_ffmpeg(
            Path::new("ffmpeg"),
            &video_dir.join("local.m3u8"),
            None,
            SubtitleOutput::default(),
            &mkv_part,
            &mut abort,
        )
        .await
        .expect("finalize HLS fixture as Matroska"),
        "Matroska FFmpeg finalization unexpectedly aborted"
    );
    let mkv_output = root.path().join("hls-output.mkv");
    crate::file_names::rename_noreplace(&mkv_part, &mkv_output)
        .expect("publish finalized Matroska fixture");
    assert_has_audio_and_video(Path::new("ffprobe"), &mkv_output);

    // The static destination-part fast path assembles the same track without a
    // local playlist, so the concatenated part must still be muxable.
    let staging = root.path().join("staging");
    let destination_dir = root.path().join("destination");
    std::fs::create_dir_all(&destination_dir).expect("create HLS destination directory");
    let destination = destination_dir.join("Example.mp4");
    crate::manifest_destination::ensure_plan(&staging, &destination, 91, false, false)
        .expect("persist HLS fMP4 destination plan");
    let resource_names = plan
        .resources
        .iter()
        .map(|resource| resource.filename.clone())
        .collect::<Vec<_>>();
    let (writer, committed) = crate::manifest_writer::OrderedTrackWriter::open_destination_part(
        resource_names,
        &video_dir,
        &staging,
        "video-track.ts",
        &destination,
        91,
        crate::manifest_destination::TrackKind::Video,
    )
    .await
    .expect("open HLS fMP4 destination part");
    assert_eq!(
        committed, 0,
        "a fresh destination part has no committed bytes"
    );
    let mut writer = writer.expect("fresh HLS fMP4 destination part");
    while writer.next_resource_index() < plan.resources.len() {
        assert!(
            writer
                .append_next(&mut abort)
                .await
                .expect("append HLS fMP4 resource to the destination part")
        );
    }
    writer
        .finish()
        .await
        .expect("finish HLS fMP4 destination part");
    let part = crate::manifest_destination::part_path(
        &staging,
        &destination,
        91,
        crate::manifest_destination::TrackKind::Video,
    )
    .expect("resolve HLS fMP4 destination part");
    let part_output = root.path().join("hls-part-output.mp4");
    assert!(
        run_ffmpeg(
            Path::new("ffmpeg"),
            &part,
            None,
            SubtitleOutput::default(),
            &part_output,
            &mut abort,
        )
        .await
        .expect("finalize the assembled HLS fMP4 destination part"),
        "destination part FFmpeg finalization unexpectedly aborted"
    );
    assert_has_audio_and_video(Path::new("ffprobe"), &part_output);
}

#[tokio::test]
async fn dash_per_track_timestamp_offsets_align_fragmented_mp4_inputs() {
    if !tools_available() {
        return;
    }

    let root = TempRoot::new("dash-pto-finalize");
    let video_input = root.path().join("video-track.mp4");
    let audio_input = root.path().join("audio-track.mp4");

    let video = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=96x64:rate=12",
            "-t",
            "3",
            "-c:v",
            "mpeg4",
            "-q:v",
            "5",
            "-video_track_timescale",
            "1000",
            "-movflags",
            "+empty_moov+frag_keyframe+default_base_moof",
            "-f",
            "mp4",
        ])
        .arg(&video_input)
        .output()
        .expect("generate fragmented-MP4 video fixture");
    assert_command_succeeded(&video, "FFmpeg could not generate the video input");

    let audio = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:sample_rate=48000",
            "-t",
            "3",
            "-c:a",
            "aac",
            "-b:a",
            "64k",
            "-movflags",
            "+empty_moov+frag_keyframe+default_base_moof",
            "-f",
            "mp4",
        ])
        .arg(&audio_input)
        .output()
        .expect("generate fragmented-MP4 audio fixture");
    assert_command_succeeded(&audio, "FFmpeg could not generate the audio input");

    // Model two independent DASH sample timelines: video begins five seconds
    // behind the Period timeline, while audio begins two seconds behind it.
    // This keeps the fixture local and deterministic without a media packager.
    shift_fragmented_mp4_decode_times(&video_input, 5_000);
    shift_fragmented_mp4_decode_times(&audio_input, 96_000);

    let ffprobe = Path::new("ffprobe");
    let video_starts = ffprobe_stream_start_times(ffprobe, &video_input);
    let audio_starts = ffprobe_stream_start_times(ffprobe, &audio_input);
    let video_start = *video_starts.get("video").expect("video input stream");
    let audio_start = *audio_starts.get("audio").expect("audio input stream");
    assert!(
        (video_start - 5.0).abs() < 0.01,
        "the input video must start at 5 seconds to exercise the PTO offset: {video_start}"
    );
    assert!(
        (audio_start - 2.0).abs() < 0.01,
        "the input audio must start at 2 seconds to exercise its independent PTO offset: {audio_start}"
    );

    let output = root.path().join("dash-pto-output.part.mp4");
    let (_stop_tx, mut abort) = tokio::sync::oneshot::channel();
    assert!(
        run_ffmpeg_with_offsets(
            Path::new("ffmpeg"),
            FfmpegInput {
                path: &video_input,
                timestamp_offset: Some("-5.000000000"),
            },
            Some(FfmpegInput {
                path: &audio_input,
                timestamp_offset: Some("-2.000000000"),
            }),
            false,
            SubtitleOutput::default(),
            &output,
            &mut abort,
        )
        .await
        .expect("finalize independently offset DASH tracks"),
        "DASH FFmpeg timestamp-offset finalization unexpectedly aborted"
    );

    let output_starts = ffprobe_stream_start_times(ffprobe, &output);
    let video_start = *output_starts.get("video").expect("finalized video stream");
    let audio_start = *output_starts.get("audio").expect("finalized audio stream");
    let one_video_frame = 1.0 / 12.0;
    assert!(
        (video_start - audio_start).abs() < one_video_frame,
        "finalized video/audio starts differ by more than one video frame: video={video_start}, audio={audio_start}"
    );
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
        let mut guarded_child_hosts = crate::download_net::PublicResolutionSet::default();
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
            SubtitleOutput::default(),
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
    assert_has_audio_and_video(Path::new("ffprobe"), &output);
}

#[tokio::test]
async fn dynamic_dash_fmp4_fixture_runs_live_capture_and_finalizes_playable_mp4() {
    if !tools_available() {
        return;
    }

    let root = TempRoot::new("dynamic-dash-finalize");
    let fixture_dir = root.path().join("fixture");
    std::fs::create_dir_all(&fixture_dir).expect("create dynamic DASH fixture directory");
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
        .expect("generate dynamic DASH fMP4 fixture");
    assert_command_succeeded(
        &generated,
        "FFmpeg failed to generate the dynamic DASH fixture",
    );

    let source_url = "https://manifest-origin.fixture.invalid/master.mpd";
    let manifest_url = "https://cdn.fixture.invalid/manifest.mpd";
    let manifest =
        std::fs::read_to_string(fixture_dir.join("manifest.mpd")).expect("read generated DASH MPD");
    let dynamic_manifest = if manifest.contains("type=\"static\"") {
        manifest.replacen("type=\"static\"", "type=\"dynamic\"", 1)
    } else {
        manifest.replacen("<MPD ", "<MPD type=\"dynamic\" ", 1)
    };
    let dynamic_manifest = dynamic_manifest.replacen(
        "<MPD ",
        "<MPD availabilityStartTime=\"1970-01-01T00:00:00Z\" availabilityEndTime=\"1970-01-01T00:00:03Z\" minimumUpdatePeriod=\"PT4S\" timeShiftBufferDepth=\"PT10S\" ",
        1,
    );
    let parsed = crate::manifest::dash::parse_dash_manifest_at(
        manifest_url,
        &dynamic_manifest,
        UNIX_EPOCH + Duration::from_secs(3),
    )
    .expect("parse generated dynamic DASH MPD");
    assert!(
        matches!(
            &parsed.presentation,
            crate::manifest::dash::DashPresentation::Dynamic(_)
        ),
        "generated fixture must use the native dynamic DASH path"
    );

    let video_key = parsed
        .video_representations
        .first()
        .expect("generated dynamic DASH fixture has video")
        .live_key
        .clone();
    let audio_key = parsed
        .default_audio()
        .expect("generated dynamic DASH fixture has audio")
        .live_key
        .clone();
    let mut video_plan = crate::manifest::dash::plan_dash_representation(&parsed, &video_key)
        .expect("plan dynamic DASH video representation");
    let mut audio_plan = crate::manifest::dash::plan_dash_representation(&parsed, &audio_key)
        .expect("plan dynamic DASH audio representation");
    assert!(
        video_plan
            .resources
            .iter()
            .any(|resource| resource.kind == DashResourceKind::Segment),
        "dynamic DASH planning must expose complete video segments"
    );
    assert!(
        audio_plan
            .resources
            .iter()
            .any(|resource| resource.kind == DashResourceKind::Segment),
        "dynamic DASH planning must expose complete audio segments"
    );

    let mut urls = video_plan
        .resources
        .iter()
        .map(|resource| resource.url.clone())
        .collect::<Vec<_>>();
    urls.extend(
        audio_plan
            .resources
            .iter()
            .map(|resource| resource.url.clone()),
    );
    let files = fixture_files(&fixture_dir, &urls);
    let (fixture_base, server) = spawn_fixture_server(files, urls.len()).await;
    for resource in &mut video_plan.resources {
        resource.url = loopback_resource_url(&fixture_base, &resource.url);
    }
    for resource in &mut audio_plan.resources {
        resource.url = loopback_resource_url(&fixture_base, &resource.url);
    }

    let staging = root.path().join("staging");
    let video_dir = staging.join("video");
    let audio_dir = staging.join("audio");
    let destination = root.path().join("dynamic-dash-output.mp4");
    let item_id = 801;
    tokio::fs::create_dir_all(&staging)
        .await
        .expect("create live DASH staging directory");
    crate::manifest_destination::ensure_plan(&staging, &destination, item_id, true, false)
        .expect("create live DASH destination plan");
    tokio::fs::create_dir_all(&video_dir)
        .await
        .expect("create dynamic DASH video staging");
    tokio::fs::create_dir_all(&audio_dir)
        .await
        .expect("create dynamic DASH audio staging");
    let client = fixture_client();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let downloaded = Arc::new(AtomicU64::new(0));
    let (_stop_tx, mut abort) = tokio::sync::oneshot::channel();

    let mut guarded_child_hosts = crate::download_net::PublicResolutionSet::default();
    let capture = run_live_dash_tracks(
        &client,
        false,
        source_url,
        root.path(),
        &destination,
        item_id,
        &video_key,
        Some(&audio_key),
        Some(&video_plan),
        Some(&audio_plan),
        &video_dir,
        &audio_dir,
        3,
        &tx,
        &downloaded,
        &mut guarded_child_hosts,
        &mut abort,
    )
    .await
    .expect("run dynamic DASH capture");
    assert_eq!(
        capture.outcome,
        DashLiveCaptureOutcome::Ended,
        "availabilityEndTime must finish the bounded fixture capture"
    );
    wait_for_fixture_server(server).await;
    assert!(
        downloaded.load(Ordering::Relaxed) > 0,
        "dynamic DASH capture must account for fetched media"
    );

    assert!(
        capture.audio_resources.is_some(),
        "dynamic DASH capture retains its selected audio track"
    );
    let video_input = crate::manifest_destination::part_path(
        &staging,
        &destination,
        item_id,
        crate::manifest_destination::TrackKind::Video,
    )
    .expect("resolve live DASH video part");
    let audio_input = crate::manifest_destination::part_path(
        &staging,
        &destination,
        item_id,
        crate::manifest_destination::TrackKind::Audio,
    )
    .expect("resolve live DASH audio part");

    let output_part = root.path().join("dynamic-dash-output.part.mp4");
    assert!(
        run_ffmpeg(
            Path::new("ffmpeg"),
            &video_input,
            Some(&audio_input),
            SubtitleOutput::default(),
            &output_part,
            &mut abort,
        )
        .await
        .expect("finalize dynamic DASH fixture with FFmpeg"),
        "dynamic DASH FFmpeg finalization unexpectedly aborted"
    );
    let output = root.path().join("dynamic-dash-output.mp4");
    crate::file_names::rename_noreplace(&output_part, &output)
        .expect("publish finalized dynamic DASH fixture");
    assert_has_audio_and_video(Path::new("ffprobe"), &output);
}

#[tokio::test]
async fn dynamic_dash_live_runner_pauses_and_retains_committed_window() {
    let root = TempRoot::new("dynamic-dash-preserve");
    let fixture_dir = root.path().join("fixture");
    let video_dir = root.path().join("video");
    let audio_dir = root.path().join("audio");
    let destination = root.path().join("dynamic-dash-output.mp4");
    let item_id = 802;
    crate::manifest_destination::ensure_plan(root.path(), &destination, item_id, false, false)
        .expect("create live DASH destination plan");
    std::fs::create_dir_all(&fixture_dir).expect("create dynamic DASH fixture files");
    tokio::fs::create_dir_all(&video_dir)
        .await
        .expect("create dynamic DASH video staging");
    tokio::fs::create_dir_all(&audio_dir)
        .await
        .expect("create dynamic DASH audio staging");

    let mut video_plan = dynamic_dash_fixture_plan(120);
    let representation_key = video_plan.representation.live_key.clone();
    let urls = video_plan
        .resources
        .iter()
        .map(|resource| resource.url.clone())
        .collect::<Vec<_>>();
    for number in 26..=30 {
        std::fs::write(
            fixture_dir.join(format!("seg-{number}.m4s")),
            format!("committed-dynamic-segment-{number}"),
        )
        .expect("write dynamic DASH fixture segment");
    }
    let files = fixture_files(&fixture_dir, &urls);
    let (fixture_base, server) = spawn_fixture_server(files, urls.len()).await;
    for resource in &mut video_plan.resources {
        resource.url = loopback_resource_url(&fixture_base, &resource.url);
    }

    let client = fixture_client();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let downloaded = Arc::new(AtomicU64::new(0));
    let (stop_tx, mut abort) = tokio::sync::oneshot::channel();
    let state_path = video_dir.join(DASH_LIVE_STATE_NAME);
    let stop_task = tokio::spawn(async move {
        tokio::time::timeout(FIXTURE_TIMEOUT, async {
            loop {
                if tokio::fs::try_exists(&state_path)
                    .await
                    .expect("inspect committed dynamic DASH state")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("dynamic DASH runner did not commit its first window");
        stop_tx
            .send(crate::video::StopIntent::Pause)
            .expect("send pause after committed dynamic DASH window");
    });

    let mut guarded_child_hosts = crate::download_net::PublicResolutionSet::default();
    let capture = run_live_dash_tracks(
        &client,
        false,
        "https://origin.example.test/live.mpd",
        root.path(),
        &destination,
        item_id,
        &representation_key,
        None,
        Some(&video_plan),
        None,
        &video_dir,
        &audio_dir,
        3,
        &tx,
        &downloaded,
        &mut guarded_child_hosts,
        &mut abort,
    )
    .await
    .expect("pause dynamic DASH capture after its first committed window");
    stop_task.await.expect("pause observer task failed");
    wait_for_fixture_server(server).await;

    assert_eq!(capture.outcome, DashLiveCaptureOutcome::Paused);
    assert_eq!(
        capture
            .video_resources
            .iter()
            .filter(|name| name.starts_with("dash-live-segment-"))
            .count(),
        5,
        "pause must return the committed segment set"
    );
    let (restored, retained_bytes) = load_dash_live_track_for_destination(
        &representation_key,
        &video_dir,
        None,
        "video-track.mp4",
        &destination,
        item_id,
        crate::manifest_destination::TrackKind::Video,
    )
    .await
    .expect("restore the preserved dynamic DASH capture");
    assert_eq!(restored.state.segments.len(), 5);
    assert_eq!(
        retained_bytes,
        downloaded.load(Ordering::Relaxed),
        "restored byte accounting must match the committed live capture"
    );

    let (finish_tx, mut finish_abort) = tokio::sync::oneshot::channel();
    finish_tx
        .send(crate::video::StopIntent::Finish)
        .expect("queue finish after restoring the paused capture");
    let finished = run_live_dash_tracks(
        &client,
        false,
        "https://origin.example.test/live.mpd",
        root.path(),
        &destination,
        item_id,
        &representation_key,
        None,
        Some(&video_plan),
        None,
        &video_dir,
        &audio_dir,
        3,
        &tx,
        &downloaded,
        &mut guarded_child_hosts,
        &mut finish_abort,
    )
    .await
    .expect("finish the restored dynamic DASH capture");
    assert_eq!(finished.outcome, DashLiveCaptureOutcome::Finish);
}

#[tokio::test]
async fn live_hls_snapshot_appends_fetched_mpegts_fragments_to_visible_destination_part() {
    let root = TempRoot::new("live-destination-part");
    let video_dir = root.path().join("video");
    tokio::fs::create_dir_all(&video_dir)
        .await
        .expect("create live HLS video staging");
    let destination_root = TempRoot::new("live-destination-part-output");
    let destination = destination_root.path().join("recording.mp4");
    crate::manifest_destination::ensure_plan(root.path(), &destination, 731, false, false)
        .expect("persist live HLS destination plan");

    let make_transport_stream = |seed: u8| {
        let mut bytes = vec![0xff; 188 * 3];
        for (index, packet) in bytes
            .as_chunks_mut::<{ mpeg_ts::ts::TS_PACKET_SIZE }>()
            .0
            .iter_mut()
            .enumerate()
        {
            packet[0] = 0x47;
            packet[1] = 0;
            packet[2] = 0;
            packet[3] = 0x10 | (seed.wrapping_add(index as u8) & 0x0f);
        }
        bytes[4] = seed;
        bytes
    };
    let first_body = make_transport_stream(1);
    let second_body = make_transport_stream(2);
    let initial_body = b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:700\n#EXTINF:4,\n700.ts\n#EXTINF:4,\n701.ts\n";
    let mut initial_plan = crate::manifest::plan_hls_media_playlist(
        "https://origin.example.test/live.m3u8",
        initial_body,
    )
    .expect("plan initial live HLS snapshot");
    let resource_files = initial_plan
        .resources
        .iter()
        .zip([first_body.clone(), second_body.clone()])
        .map(|(resource, body)| {
            let filename = resource
                .url
                .path()
                .rsplit('/')
                .next()
                .expect("planned segment has a filename");
            (format!("/{filename}"), body)
        })
        .collect::<HashMap<_, _>>();
    let (base_url, server) = spawn_fixture_server(resource_files, 2).await;
    localize_live_plan_resources(&mut initial_plan, &base_url);

    let client = fixture_client();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let downloaded = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (_stop_tx, mut abort) = tokio::sync::oneshot::channel();
    let mut track = load_live_hls_track(&initial_plan, &video_dir)
        .await
        .expect("load new live HLS track");
    track.destination_part = Some(LiveHlsDestination {
        staging: root.path().to_path_buf(),
        destination: destination.clone(),
        item_id: 731,
        track: crate::manifest_destination::TrackKind::Video,
        filename: "video-track.ts",
    });

    assert_eq!(
        apply_live_hls_snapshot(
            &client,
            &initial_plan,
            &mut track,
            2,
            &tx,
            &downloaded,
            &mut abort,
            0,
        )
        .await
        .expect("capture and append initial HLS snapshot"),
        LiveHlsSnapshotOutcome::Committed
    );
    wait_for_fixture_server(server).await;

    let visible_part = crate::manifest_destination::part_path(
        root.path(),
        &destination,
        731,
        crate::manifest_destination::TrackKind::Video,
    )
    .expect("resolve visible HLS destination part");
    let mut expected = first_body;
    expected.extend_from_slice(&second_body);
    assert_eq!(
        tokio::fs::read(&visible_part)
            .await
            .expect("read growing HLS destination part"),
        expected
    );
    assert!(
        tokio::fs::symlink_metadata(video_dir.join("local.m3u8"))
            .await
            .is_ok(),
        "the local playlist remains the recovery index"
    );
    for resource in track.accumulator.resource_names() {
        assert!(
            tokio::fs::symlink_metadata(video_dir.join(resource))
                .await
                .is_ok(),
            "the original segment remains in XDG staging for resume"
        );
    }
    assert_eq!(
        downloaded.load(std::sync::atomic::Ordering::Relaxed),
        expected.len() as u64
    );
}

#[tokio::test]
async fn hls_sample_aes_mpegts_is_decrypted_by_iori_ssa_before_ffmpeg_finalization() {
    if !tools_available() {
        return;
    }

    let root = TempRoot::new("sample-aes");
    let encrypted_segment = include_bytes!("../tests/fixtures/hls-sample-aes/ac3.ts").to_vec();
    let key = include_bytes!("../tests/fixtures/hls-sample-aes/key.bin").to_vec();
    let expected_downloaded =
        u64::try_from(encrypted_segment.len()).expect("fixture size fits in u64") * 2
            + u64::try_from(key.len()).expect("key size fits in u64");
    let iv = include_bytes!("../tests/fixtures/hls-sample-aes/iv.bin");
    let iv_hex = iv
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<String>();
    let playlist = format!(
        "#EXTM3U\n#EXT-X-VERSION:5\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\",IV=0x{iv_hex}\n#EXTINF:2,\nsegment-0.ts\n#EXTINF:2,\nsegment-0.ts\n#EXT-X-ENDLIST\n"
    );
    let mut plan = crate::manifest::plan_hls_media_playlist(
        "https://origin.example.test/index.m3u8",
        playlist.as_bytes(),
    )
    .expect("plan identity SAMPLE-AES MPEG-TS fixture");
    assert!(
        !plan.is_destination_assemblable_candidate(),
        "encrypted HLS must use the local playlist path for per-segment VSD decryption"
    );

    let files = HashMap::from([
        ("/segment-0.ts".to_string(), encrypted_segment),
        ("/key.bin".to_string(), key.to_vec()),
    ]);
    let (base_url, server) = spawn_fixture_server(files, plan.resources.len()).await;
    for resource in plan.resources.iter_mut() {
        resource.url = loopback_resource_url(&base_url, &resource.url);
    }

    let video_dir = root.path().join("video");
    tokio::fs::create_dir_all(&video_dir)
        .await
        .expect("create SAMPLE-AES staging directory");
    let client = fixture_client();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let downloaded = Arc::new(AtomicU64::new(0));
    let (_abort_tx, mut abort) = tokio::sync::oneshot::channel();
    assert!(
        fetch_resources(
            &client,
            hls_resources(&plan),
            &video_dir,
            2,
            &tx,
            &downloaded,
            &mut abort,
        )
        .await
        .expect("fetch SAMPLE-AES segment and identity key"),
        "SAMPLE-AES resource fetch unexpectedly aborted"
    );
    wait_for_fixture_server(server).await;
    assert_eq!(downloaded.load(Ordering::Relaxed), expected_downloaded);

    write_playlist(&video_dir, &plan)
        .await
        .expect("write localized SAMPLE-AES playlist");
    decrypt_hls_sample_aes_segments(&video_dir, &plan)
        .await
        .expect("decrypt SAMPLE-AES segment with iori-ssa");
    let local_playlist_bytes = tokio::fs::read(video_dir.join("local.m3u8"))
        .await
        .expect("read decrypted local HLS playlist");
    let local_playlist_text = String::from_utf8_lossy(&local_playlist_bytes);
    assert!(
        !local_playlist_text.contains("METHOD=SAMPLE-AES"),
        "the local playlist must not ask FFmpeg to decrypt already-clear media"
    );
    let local_playlist = m3u8_rs::parse_media_playlist_res(&local_playlist_bytes)
        .expect("parse decrypted local HLS playlist");
    assert_eq!(local_playlist.segments.len(), 2);
    assert!(
        local_playlist
            .segments
            .iter()
            .all(|segment| segment.key.is_none()),
        "decrypted segments must no longer carry encryption key tags"
    );
    assert_ne!(
        local_playlist.segments[0].uri, local_playlist.segments[1].uri,
        "repeated references to one encrypted URI must get independent decrypted files"
    );
    // FFmpeg below is authoritative for the complete decrypted playlist;
    // do not duplicate its per-segment packet framing heuristics here.
    let output = root.path().join("sample-aes-output.mkv");
    assert!(
        run_ffmpeg(
            Path::new("ffmpeg"),
            &video_dir.join("local.m3u8"),
            None,
            SubtitleOutput::default(),
            &output,
            &mut abort,
        )
        .await
        .expect("finalize SAMPLE-AES fixture with FFmpeg"),
        "SAMPLE-AES FFmpeg finalization unexpectedly aborted"
    );

    let decoded = Command::new("ffmpeg")
        .args([
            "-protocol_whitelist",
            "file,crypto",
            "-allowed_extensions",
            "bin",
            "-allowed_segment_extensions",
            "bin",
            "-extension_picky",
            "0",
            "-v",
            "error",
            "-xerror",
            "-i",
        ])
        .arg(video_dir.join("local.m3u8"))
        .args(["-map", "0:a:0", "-vn", "-f", "null", "-"])
        .output()
        .expect("decode the SAMPLE-AES source to validate decrypted frames");
    assert_command_succeeded(&decoded, "FFmpeg could not decode the SAMPLE-AES source");

    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(&output)
        .output()
        .expect("probe decrypted SAMPLE-AES output");
    assert_command_succeeded(&probe, "ffprobe rejected decrypted SAMPLE-AES output");
    assert!(
        String::from_utf8_lossy(&probe.stdout)
            .lines()
            .any(|line| line == "codec_name=ac3"),
        "FFmpeg did not expose the decrypted AC-3 stream: {}",
        String::from_utf8_lossy(&probe.stdout)
    );
}

#[tokio::test]
async fn hls_cue_markers_do_not_remove_media_segments() {
    let root = TempRoot::new("hls-cue-markers");
    let fixture_dir = root.path().join("fixture");
    std::fs::create_dir_all(&fixture_dir).expect("create HLS cue-marker fixture directory");
    for filename in ["program-before.ts", "ad.ts", "program-after.ts"] {
        std::fs::write(
            fixture_dir.join(filename),
            format!("fixture bytes for {filename}"),
        )
        .expect("write HLS segment fixture");
    }

    let manifest = br#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXTINF:4,
program-before.ts
#EXT-X-CUE-OUT:4
#EXTINF:4,
ad.ts
#EXT-X-CUE-IN
#EXTINF:4,
program-after.ts
#EXT-X-ENDLIST
"#;
    let media_manifest_url = "https://cdn.fixture.invalid/ad-break.m3u8";
    let mut plan = crate::manifest::plan_hls_media_playlist(media_manifest_url, manifest)
        .expect("plan a static playlist carrying ad cue markers");

    let segment_urls = plan
        .resources
        .iter()
        .filter(|resource| resource.kind == crate::manifest::HlsResourceKind::Segment)
        .map(|resource| resource.url.as_str().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        segment_urls,
        vec![
            "https://cdn.fixture.invalid/program-before.ts".to_string(),
            "https://cdn.fixture.invalid/ad.ts".to_string(),
            "https://cdn.fixture.invalid/program-after.ts".to_string(),
        ]
    );

    let urls = plan
        .resources
        .iter()
        .map(|resource| resource.url.clone())
        .collect::<Vec<_>>();
    let files = fixture_files(&fixture_dir, &urls);
    let (server_url, server) = spawn_fixture_server(files, urls.len()).await;
    for resource in &mut plan.resources {
        resource.url = loopback_resource_url(&server_url, &resource.url);
    }

    let video_dir = root.path().join("staging/video");
    tokio::fs::create_dir_all(&video_dir)
        .await
        .expect("create HLS ad-removal staging directory");
    let client = fixture_client();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let downloaded = Arc::new(AtomicU64::new(0));
    let (_abort_tx, mut abort) = tokio::sync::oneshot::channel();
    assert!(
        fetch_resources(
            &client,
            hls_resources(&plan),
            &video_dir,
            2,
            &tx,
            &downloaded,
            &mut abort,
        )
        .await
        .expect("fetch all media segments, including the marked interval"),
        "HLS cue-marker fixture fetch unexpectedly aborted"
    );
    wait_for_fixture_server(server).await;
    assert!(downloaded.load(Ordering::Relaxed) > 0);

    write_playlist(&video_dir, &plan)
        .await
        .expect("write the complete local HLS playlist");
    let local_bytes = tokio::fs::read(video_dir.join("local.m3u8"))
        .await
        .expect("read the complete local HLS playlist");
    let local = m3u8_rs::parse_media_playlist_res(&local_bytes)
        .expect("parse the ad-free local HLS playlist");
    assert_eq!(local.segments.len(), 3);
    let planned_segments = plan
        .resources
        .iter()
        .filter(|resource| resource.kind == crate::manifest::HlsResourceKind::Segment)
        .collect::<Vec<_>>();
    assert_eq!(local.segments[1].uri, planned_segments[1].filename);
    assert!(
        local
            .segments
            .iter()
            .any(|segment| { segment.unknown_tags.iter().any(|tag| tag.tag == "CUE-OUT") }),
        "cue markers should remain inert metadata in the local playlist"
    );
    for resource in &plan.resources {
        assert!(
            tokio::fs::metadata(video_dir.join(&resource.filename))
                .await
                .is_ok(),
            "planned program resource was not staged: {}",
            resource.filename
        );
    }
}
