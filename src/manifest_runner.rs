//! Native HLS/DASH segment transfer, resume and container finalization.

use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use futures_util::{StreamExt as _, TryStreamExt as _};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
use url::Url;

use crate::engine_msg::EngineMsg;
use crate::manifest::{HlsPlan, HlsResourceKind, ManifestByteRange, dash::DashRepresentationPlan};
use crate::manifest_intake::{ManifestLookup, NativeManifest};
use crate::media_types::{ManifestKind, ManifestSource};

/// Upper bound for one media resource, independent of a server-supplied length.
const MAX_RESOURCE_BYTES: u64 = 1024 * 1024 * 1024;
/// Aggregate resource ceiling, matching the regular segmented downloader's 1 TiB trust limit.
const MAX_MANIFEST_DOWNLOAD_BYTES: u64 = 1 << 40;
/// Idle budget for receiving a response body; progressing transfers are not wall-clock limited.
const RESOURCE_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound parallel manifest requests to the same range as the regular HTTP engine.
const MAX_RESOURCE_CONNECTIONS: usize = 16;
/// Keep only a small diagnostic tail from FFmpeg.
const FFMPEG_LOG_LINES: usize = 32;

/// Inputs retained by the queue; URLs for individual segments are resolved again on every attempt.
#[derive(Clone, Debug)]
pub(crate) struct ManifestJob {
    pub(crate) item_id: u64,
    pub(crate) source: ManifestSource,
    pub(crate) dest: PathBuf,
    pub(crate) proxy: Option<crate::net_types::ResolvedProxy>,
    pub(crate) cookies_browser: String,
    pub(crate) connections: usize,
}

/// Result of one native manifest attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ManifestOutcome {
    Finished(u64),
    Aborted,
}

#[derive(Clone, Debug)]
struct Resource {
    url: Url,
    range: Option<ManifestByteRange>,
    filename: String,
    key: bool,
}

enum Tracks {
    Hls {
        video: HlsPlan,
        audio: Option<HlsPlan>,
    },
    Dash {
        video: DashRepresentationPlan,
        audio: Option<DashRepresentationPlan>,
    },
}

/// Run one HLS or static-DASH attempt, keeping completed resources for retry.
///
/// # Errors
/// Returns a display-ready failure. The queue worker sends such failures back
/// through the yt-dlp fallback path; the destination is never written until mux succeeds.
pub(crate) async fn run_manifest_download(
    job: ManifestJob,
    gate: Arc<crate::attempt_gate::AttemptGate>,
    mut abort: tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
    tx: tokio::sync::mpsc::UnboundedSender<EngineMsg>,
) -> Result<ManifestOutcome, String> {
    let Some(dest_dir) = job.dest.parent() else {
        return Err("The download destination has no parent directory".to_string());
    };
    let parent = dest_dir.to_path_buf();
    let item_id = job.item_id;
    let staging = tokio::task::spawn_blocking(move || {
        crate::video_staging::ensure_manifest_staging_dir(&parent, item_id)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("Could not prepare manifest staging: {error}"))??;

    let client = crate::manifest_intake::client_for_manifest(
        &job.source.url,
        &job.cookies_browser,
        job.proxy.as_ref(),
    )
    .await
    .map_err(|error| format!("Could not configure manifest networking: {error}"))?;
    let lookup = crate::manifest_intake::lookup_with_client(&client, &job.source.url)
        .await?
        .ok_or_else(|| "The stream manifest is no longer supported".to_string())?;
    if lookup.kind() != job.source.kind {
        return Err("The stream manifest changed protocol".to_string());
    }

    let tracks = resolve_tracks(&client, lookup, &job.source).await?;
    let video_dir = staging.join("video");
    ensure_track_dir(&video_dir).await?;
    let audio_dir = staging.join("audio");
    let has_audio = match &tracks {
        Tracks::Hls { audio, .. } => audio.is_some(),
        Tracks::Dash { audio, .. } => audio.is_some(),
    };
    if has_audio {
        ensure_track_dir(&audio_dir).await?;
    }

    tx.send(EngineMsg::Phase("Downloading stream segments…".to_string()))
        .ok();
    let concurrency = job.connections.clamp(1, MAX_RESOURCE_CONNECTIONS);
    let downloaded = Arc::new(AtomicU64::new(0));

    match &tracks {
        Tracks::Hls { video, audio } => {
            if !fetch_resources(
                &client,
                hls_resources(video),
                &video_dir,
                concurrency,
                &tx,
                &downloaded,
                &mut abort,
            )
            .await?
            {
                return Ok(ManifestOutcome::Aborted);
            }
            if let Some(audio) = audio {
                if !fetch_resources(
                    &client,
                    hls_resources(audio),
                    &audio_dir,
                    concurrency,
                    &tx,
                    &downloaded,
                    &mut abort,
                )
                .await?
                {
                    return Ok(ManifestOutcome::Aborted);
                }
            }
            write_playlist(&video_dir, video).await?;
            if let Some(audio) = audio {
                write_playlist(&audio_dir, audio).await?;
            }
        }
        Tracks::Dash { video, audio } => {
            if !fetch_resources(
                &client,
                dash_resources(video),
                &video_dir,
                concurrency,
                &tx,
                &downloaded,
                &mut abort,
            )
            .await?
            {
                return Ok(ManifestOutcome::Aborted);
            }
            if !concat_dash_track(video, &video_dir, "video-track.mp4", &mut abort).await? {
                return Ok(ManifestOutcome::Aborted);
            }
            if let Some(audio) = audio {
                if !fetch_resources(
                    &client,
                    dash_resources(audio),
                    &audio_dir,
                    concurrency,
                    &tx,
                    &downloaded,
                    &mut abort,
                )
                .await?
                {
                    return Ok(ManifestOutcome::Aborted);
                }
                if !concat_dash_track(audio, &audio_dir, "audio-track.mp4", &mut abort).await? {
                    return Ok(ManifestOutcome::Aborted);
                }
            }
        }
    }

    if abort.try_recv().is_ok() {
        return Ok(ManifestOutcome::Aborted);
    }
    tx.send(EngineMsg::Phase("Merging audio and video…".to_string()))
        .ok();

    let ffmpeg_dir =
        crate::video_tools::toolchain_dir_in(&crate::video_tools::ffmpeg_search_dirs())
            .ok_or_else(|| {
                "FFmpeg and ffprobe are required for native stream downloads".to_string()
            })?;
    let ffmpeg = ffmpeg_dir.join("ffmpeg");
    if !ffmpeg.is_file() {
        return Err("The FFmpeg executable could not be found".to_string());
    }

    let video_input = match tracks {
        Tracks::Hls { .. } => video_dir.join("local.m3u8"),
        Tracks::Dash { .. } => video_dir.join("video-track.mp4"),
    };
    let audio_input = has_audio.then(|| match job.source.kind {
        ManifestKind::Hls => audio_dir.join("local.m3u8"),
        ManifestKind::Dash => audio_dir.join("audio-track.mp4"),
    });
    let output_part = staging.join("output.part.mp4");
    remove_regular_file_if_present(&output_part).await?;
    if !run_ffmpeg(
        &ffmpeg,
        &video_input,
        audio_input.as_deref(),
        &output_part,
        &mut abort,
    )
    .await?
    {
        return Ok(ManifestOutcome::Aborted);
    }

    // Validate and size the muxed file before the delivery commit. After the
    // no-clobber rename succeeds, no later I/O failure may turn a delivered
    // download into a retry/fallback.
    let output_metadata = tokio::fs::symlink_metadata(&output_part)
        .await
        .map_err(|error| format!("Could not inspect the finalized stream: {error}"))?;
    if output_metadata.file_type().is_symlink()
        || !output_metadata.is_file()
        || output_metadata.len() == 0
    {
        return Err("FFmpeg did not produce a regular, non-empty media file".to_string());
    }
    let size = output_metadata.len();

    if !gate.try_commit() {
        return Ok(ManifestOutcome::Aborted);
    }
    match crate::file_names::rename_noreplace(&output_part, &job.dest) {
        Ok(()) => {
            gate.mark_delivered();
            crate::extension_server::discard_handoff_cookies(&job.source.url);
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(crate::engine_msg::DEST_EXISTS.to_string());
        }
        Err(error) => {
            return Err(format!(
                "Could not move the finished stream into place: {error}"
            ));
        }
    }
    let parent = job.dest.parent().map(Path::to_path_buf);
    if let Some(parent) = parent {
        let id = job.item_id;
        let _ = tokio::task::spawn_blocking(move || {
            crate::video_staging::clean_manifest_staging_files(&parent, id);
        })
        .await;
    }
    Ok(ManifestOutcome::Finished(size))
}

async fn resolve_tracks(
    client: &reqwest::Client,
    lookup: ManifestLookup,
    source: &ManifestSource,
) -> Result<Tracks, String> {
    let mut guarded_child_hosts = std::collections::HashSet::new();
    match lookup.manifest {
        NativeManifest::HlsMedia(plan) if source.representation_key == "hls-media" => {
            guard_hls_resources(&source.url, &plan, &mut guarded_child_hosts)?;
            Ok(Tracks::Hls {
                video: plan,
                audio: None,
            })
        }
        NativeManifest::HlsMaster(master) => {
            let variant = master
                .variants
                .iter()
                .find(|variant| {
                    crate::manifest_intake::hls_variant_key(variant) == source.representation_key
                })
                .ok_or_else(|| "The selected HLS variant is no longer available".to_string())?;
            let video_url = variant.url.clone();
            let audio_url = variant.audio_group.as_deref().and_then(|group| {
                master
                    .audio_renditions
                    .iter()
                    .filter(|rendition| rendition.group_id == group)
                    .find(|rendition| rendition.is_default)
                    .or_else(|| {
                        master
                            .audio_renditions
                            .iter()
                            .find(|rendition| rendition.group_id == group)
                    })
                    .and_then(|rendition| rendition.url.clone())
            });
            guard_manifest_child_url(&source.url, &video_url, &mut guarded_child_hosts)?;
            if let Some(audio_url) = audio_url.as_ref() {
                guard_manifest_child_url(&source.url, audio_url, &mut guarded_child_hosts)?;
            }
            let video =
                crate::manifest_intake::lookup_hls_media(client, video_url.as_str()).await?;
            guard_hls_resources(&source.url, &video, &mut guarded_child_hosts)?;
            let audio = match audio_url {
                Some(url) => {
                    let plan =
                        crate::manifest_intake::lookup_hls_media(client, url.as_str()).await?;
                    guard_hls_resources(&source.url, &plan, &mut guarded_child_hosts)?;
                    Some(plan)
                }
                None => None,
            };
            Ok(Tracks::Hls { video, audio })
        }
        NativeManifest::Dash(dash) => {
            let video =
                crate::manifest::dash::plan_dash_representation(&dash, &source.representation_key)
                    .map_err(|error| error.to_string())?;
            let audio = dash
                .default_audio()
                .map(|track| {
                    crate::manifest::dash::plan_dash_representation(&dash, &track.key)
                        .map_err(|error| error.to_string())
                })
                .transpose()?;
            for resource in video.resources.iter() {
                guard_manifest_child_url(&source.url, &resource.url, &mut guarded_child_hosts)?;
            }
            if let Some(audio) = audio.as_ref() {
                for resource in audio.resources.iter() {
                    guard_manifest_child_url(&source.url, &resource.url, &mut guarded_child_hosts)?;
                }
            }
            Ok(Tracks::Dash { video, audio })
        }
        NativeManifest::HlsMedia(_) => {
            Err("The selected HLS media playlist is invalid".to_string())
        }
    }
}

fn guard_hls_resources(
    parent_url: &str,
    plan: &HlsPlan,
    seen_hosts: &mut std::collections::HashSet<String>,
) -> Result<(), String> {
    for resource in plan.resources.iter() {
        guard_manifest_child_url(parent_url, &resource.url, seen_hosts)?;
    }
    Ok(())
}

/// Extend the browser-handoff DNS policy to every host discovered in a native
/// presentation. The same collection cap is used by playlist/gallery imports.
fn guard_manifest_child_url(
    parent_url: &str,
    child_url: &Url,
    seen_hosts: &mut std::collections::HashSet<String>,
) -> Result<(), String> {
    match crate::download_net::inherit_public_resolution_for_collection(
        parent_url,
        child_url.as_str(),
        seen_hosts,
    ) {
        Ok(()) => Ok(()),
        Err(crate::download_net::CollectionHostError::TooManyDistinctHosts) => Err(format!(
            "The stream manifest exceeds the limit of {} distinct resource hosts",
            crate::download_net::MAX_GUARDED_CHILD_HOSTS_PER_COLLECTION
        )),
        Err(crate::download_net::CollectionHostError::UnsafeDestination) => {
            Err("The stream manifest references a local or private network resource".to_string())
        }
    }
}

fn hls_resources(plan: &HlsPlan) -> Vec<Resource> {
    plan.resources
        .iter()
        .map(|resource| Resource {
            url: resource.url.clone(),
            range: resource.range,
            filename: resource.filename.clone(),
            key: resource.kind == HlsResourceKind::Key,
        })
        .collect()
}

fn dash_resources(plan: &DashRepresentationPlan) -> Vec<Resource> {
    plan.resources
        .iter()
        .map(|resource| Resource {
            url: resource.url.clone(),
            range: resource.range,
            filename: resource.filename.clone(),
            key: false,
        })
        .collect()
}

async fn ensure_track_dir(path: &Path) -> Result<(), String> {
    match tokio::fs::create_dir(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = tokio::fs::symlink_metadata(path)
                .await
                .map_err(|error| format!("Could not inspect manifest staging: {error}"))?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err("Manifest staging path is not a real directory".to_string());
            }
            Ok(())
        }
        Err(error) => Err(format!("Could not create manifest staging: {error}")),
    }
}

async fn write_playlist(directory: &Path, plan: &HlsPlan) -> Result<(), String> {
    let bytes = plan
        .local_playlist_bytes()
        .map_err(|error| error.to_string())?;
    let target = directory.join("local.m3u8");
    let part = directory.join("local.m3u8.part");
    remove_regular_file_if_present(&target).await?;
    remove_regular_file_if_present(&part).await?;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part)
        .await
        .map_err(|error| format!("Could not write the local HLS playlist: {error}"))?;
    file.write_all(&bytes)
        .await
        .map_err(|error| format!("Could not write the local HLS playlist: {error}"))?;
    file.sync_all()
        .await
        .map_err(|error| format!("Could not flush the local HLS playlist: {error}"))?;
    drop(file);
    crate::file_names::rename_noreplace(&part, &target)
        .map_err(|error| format!("Could not publish the local HLS playlist: {error}"))
}

async fn fetch_resources(
    client: &reqwest::Client,
    resources: Vec<Resource>,
    directory: &Path,
    connections: usize,
    tx: &tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    downloaded: &Arc<AtomicU64>,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
) -> Result<bool, String> {
    let jobs = futures_util::stream::iter(resources.into_iter().map(|resource| {
        let client = client.clone();
        let directory = directory.to_path_buf();
        let tx = tx.clone();
        let downloaded = Arc::clone(downloaded);
        async move { fetch_resource(&client, resource, &directory, &tx, &downloaded).await }
    }))
    .buffer_unordered(connections.clamp(1, MAX_RESOURCE_CONNECTIONS));
    tokio::pin!(jobs);
    tokio::select! {
        biased;
        _ = abort => Ok(false),
        result = jobs.try_for_each(|_| async { Ok(()) }) => {
            result?;
            Ok(true)
        }
    }
}

fn reserve_download_bytes(downloaded: &AtomicU64, bytes: u64) -> Result<u64, String> {
    downloaded
        .try_update(Ordering::AcqRel, Ordering::Relaxed, |current| {
            current
                .checked_add(bytes)
                .filter(|total| *total <= MAX_MANIFEST_DOWNLOAD_BYTES)
        })
        .map(|previous| previous + bytes)
        .map_err(|_| {
            "The stream presentation exceeds Grab's 1 TiB native download limit".to_string()
        })
}

async fn fetch_resource(
    client: &reqwest::Client,
    resource: Resource,
    directory: &Path,
    tx: &tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    downloaded: &AtomicU64,
) -> Result<u64, String> {
    if !safe_resource_filename(&resource.filename) {
        return Err("The stream manifest contains an unsafe resource name".to_string());
    }
    let target = directory.join(&resource.filename);
    if let Ok(metadata) = tokio::fs::symlink_metadata(&target).await {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err("A manifest resource path is not a regular file".to_string());
        }
        let cached_range_matches = resource
            .range
            .is_none_or(|range| metadata.len() == range.length);
        if metadata.len() > 0
            && metadata.len() <= MAX_RESOURCE_BYTES
            && cached_range_matches
            && (!resource.key || metadata.len() == 16)
        {
            let size = metadata.len();
            let total = reserve_download_bytes(downloaded, size)?;
            tx.send(progress_message(total)).ok();
            return Ok(size);
        }
        tokio::fs::remove_file(&target)
            .await
            .map_err(|error| format!("Could not reset an incomplete manifest resource: {error}"))?;
    }

    let part = directory.join(format!("{}.part", resource.filename));
    remove_regular_file_if_present(&part).await?;
    let mut request = client.get(resource.url.clone());
    if let Some(range) = resource.range {
        let end = range
            .offset
            .checked_add(range.length)
            .and_then(|value| value.checked_sub(1))
            .ok_or_else(|| "The stream manifest contains an invalid byte range".to_string())?;
        request = request.header(
            reqwest::header::RANGE,
            format!("bytes={}-{end}", range.offset),
        );
    }
    let response = tokio::time::timeout(RESOURCE_IDLE_TIMEOUT, request.send())
        .await
        .map_err(|_| "Timed out while requesting a stream segment".to_string())?
        .map_err(|error| format!("Could not retrieve a stream segment: {error}"))?;
    if let Some(range) = resource.range {
        if response.status() != reqwest::StatusCode::PARTIAL_CONTENT
            || !content_range_matches(
                response
                    .headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|value| value.to_str().ok()),
                range,
            )
        {
            return Err("The stream server did not honor the requested byte range".to_string());
        }
    } else if !response.status().is_success() {
        return Err(format!(
            "The stream server returned HTTP {} for a segment",
            response.status().as_u16()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESOURCE_BYTES)
    {
        return Err("A stream segment exceeds the size limit".to_string());
    }
    if resource.key && response.content_length().is_some_and(|length| length != 16) {
        return Err("The HLS AES-128 key is not 16 bytes".to_string());
    }

    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part)
        .await
        .map_err(|error| format!("Could not create a segment file: {error}"))?;
    let mut stream = response.bytes_stream();
    let mut size = 0_u64;
    let pace_start = Instant::now();
    let mut paced = 0_u64;
    let mut last_progress = Instant::now();
    loop {
        let next = tokio::time::timeout(RESOURCE_IDLE_TIMEOUT, stream.next())
            .await
            .map_err(|_| "A stream segment stopped making progress".to_string())?;
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk.map_err(|error| format!("A stream segment transfer failed: {error}"))?;
        size = size
            .checked_add(chunk.len() as u64)
            .filter(|size| *size <= MAX_RESOURCE_BYTES)
            .ok_or_else(|| "A stream segment exceeds the size limit".to_string())?;
        if resource.key && size > 16 {
            return Err("The HLS AES-128 key is not 16 bytes".to_string());
        }
        file.write_all(&chunk)
            .await
            .map_err(|error| format!("Could not save a stream segment: {error}"))?;
        crate::download_rate::pace_chunk(
            &mut paced,
            pace_start,
            crate::download_rate::live_rate_limit(),
            chunk.len(),
        )
        .await;
        let total = reserve_download_bytes(downloaded, chunk.len() as u64)?;
        if last_progress.elapsed() >= Duration::from_millis(100) {
            tx.send(progress_message(total)).ok();
            last_progress = Instant::now();
        }
    }
    if resource.range.is_some_and(|range| size != range.length) {
        return Err("The stream server returned an incomplete byte range".to_string());
    }
    if size == 0 || (resource.key && size != 16) {
        return Err(if resource.key {
            "The HLS AES-128 key is not 16 bytes".to_string()
        } else {
            "The stream server returned an empty segment".to_string()
        });
    }
    file.sync_all()
        .await
        .map_err(|error| format!("Could not flush a stream segment: {error}"))?;
    drop(file);
    crate::file_names::rename_noreplace(&part, &target)
        .map_err(|error| format!("Could not publish a stream segment: {error}"))?;
    tx.send(progress_message(downloaded.load(Ordering::Relaxed)))
        .ok();
    Ok(size)
}

fn progress_message(downloaded: u64) -> EngineMsg {
    EngineMsg::Progress {
        downloaded,
        total: None,
        uploaded: 0,
        upload_bps: 0,
    }
}

fn safe_resource_filename(filename: &str) -> bool {
    !filename.is_empty()
        && filename != "."
        && filename != ".."
        && !filename.contains('/')
        && !filename.contains('\\')
        && Path::new(filename)
            .file_name()
            .and_then(|name| name.to_str())
            == Some(filename)
}

fn content_range_matches(header: Option<&str>, range: ManifestByteRange) -> bool {
    let Some(value) = header.and_then(|header| header.strip_prefix("bytes ")) else {
        return false;
    };
    let Some((positions, total)) = value.split_once('/') else {
        return false;
    };
    let Some((start, end)) = positions.split_once('-') else {
        return false;
    };
    let (Ok(start), Ok(end), Ok(total)) = (
        start.parse::<u64>(),
        end.parse::<u64>(),
        total.parse::<u64>(),
    ) else {
        return false;
    };
    range
        .offset
        .checked_add(range.length)
        .is_some_and(|exclusive_end| {
            start == range.offset && end == exclusive_end - 1 && total >= exclusive_end
        })
}

async fn concat_dash_track(
    plan: &DashRepresentationPlan,
    directory: &Path,
    filename: &str,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
) -> Result<bool, String> {
    let target = directory.join(filename);
    let part = directory.join(format!("{filename}.part"));
    remove_regular_file_if_present(&target).await?;
    remove_regular_file_if_present(&part).await?;
    let mut output = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part)
        .await
        .map_err(|error| format!("Could not create a DASH track file: {error}"))?;
    for resource in plan.resources.iter() {
        let input_path = directory.join(&resource.filename);
        let mut input = tokio::fs::File::open(&input_path)
            .await
            .map_err(|error| format!("Could not open a DASH segment: {error}"))?;
        tokio::select! {
            biased;
            _ = &mut *abort => return Ok(false),
            result = tokio::io::copy(&mut input, &mut output) => {
                result.map_err(|error| format!("Could not assemble a DASH track: {error}"))?;
            }
        }
    }
    tokio::select! {
        biased;
        _ = &mut *abort => return Ok(false),
        result = output.sync_all() => {
            result.map_err(|error| format!("Could not flush a DASH track: {error}"))?;
        }
    }
    drop(output);
    crate::file_names::rename_noreplace(&part, &target)
        .map(|()| true)
        .map_err(|error| format!("Could not publish a DASH track: {error}"))
}

async fn remove_regular_file_if_present(path: &Path) -> Result<(), String> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err("A manifest staging path is not a regular file".to_string())
        }
        Ok(_) => tokio::fs::remove_file(path)
            .await
            .map_err(|error| format!("Could not clear an incomplete staging file: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Could not inspect manifest staging: {error}")),
    }
}

async fn run_ffmpeg(
    ffmpeg: &Path,
    video_input: &Path,
    audio_input: Option<&Path>,
    output: &Path,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
) -> Result<bool, String> {
    let mut command = tokio::process::Command::new(ffmpeg);
    command
        .arg("-hide_banner")
        .arg("-nostdin")
        .arg("-n")
        .arg("-i")
        .arg(video_input);
    if let Some(audio_input) = audio_input {
        command.arg("-i").arg(audio_input);
        command.arg("-map").arg("0:v:0").arg("-map").arg("1:a:0");
    } else {
        command.arg("-map").arg("0:v:0?").arg("-map").arg("0:a:0?");
    }
    command
        .args(["-c", "copy", "-movflags", "+faststart", "-f", "mp4"])
        .arg(output)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| format!("Could not start FFmpeg: {error}"))?;
    let stderr = child.stderr.take();
    let stderr_task = tokio::spawn(async move {
        let Some(stderr) = stderr else {
            return String::new();
        };
        let mut reader = tokio::io::BufReader::new(stderr).lines();
        let mut tail = std::collections::VecDeque::with_capacity(FFMPEG_LOG_LINES);
        while let Ok(Some(line)) = reader.next_line().await {
            if tail.len() == FFMPEG_LOG_LINES {
                tail.pop_front();
            }
            tail.push_back(line);
        }
        tail.into_iter().collect::<Vec<_>>().join("\n")
    });
    let status = tokio::select! {
        biased;
        _ = abort => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = stderr_task.await;
            return Ok(false);
        }
        status = child.wait() => status.map_err(|error| format!("FFmpeg could not finish: {error}"))?,
    };
    let log = stderr_task.await.unwrap_or_default();
    if !status.success() {
        let detail = log
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("FFmpeg could not mux the stream");
        return Err(format!("FFmpeg could not mux the stream: {detail}"));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_must_match_exact_requested_range_and_total() {
        let range = ManifestByteRange {
            offset: 16,
            length: 8,
        };
        assert!(content_range_matches(Some("bytes 16-23/64"), range));
        assert!(!content_range_matches(Some("bytes 0-7/64"), range));
        assert!(!content_range_matches(Some("bytes 16-22/64"), range));
        assert!(!content_range_matches(Some("bytes 16-23/20"), range));
    }

    #[test]
    fn resource_names_must_be_single_safe_path_components() {
        assert!(safe_resource_filename("segment-000001.bin"));
        assert!(!safe_resource_filename("../segment.bin"));
        assert!(!safe_resource_filename("nested/segment.bin"));
        assert!(!safe_resource_filename(""));
    }
}
