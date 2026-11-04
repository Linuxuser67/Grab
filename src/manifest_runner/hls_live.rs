//! HLS live-capture staging, polling, and recovery.

use super::*;

async fn read_local_live_hls_plan(
    source_url: &str,
    directory: &Path,
) -> Result<Option<HlsPlan>, ManifestRunError> {
    let path = directory.join("local.m3u8");
    let metadata = match tokio::fs::symlink_metadata(&path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(ManifestRunError::Failed(format!(
                "Could not inspect the staged live HLS playlist: {error}"
            )));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ManifestRunError::Failed(
            "The staged live HLS playlist is not a regular file".to_string(),
        ));
    }
    if metadata.len() > crate::manifest::MAX_HLS_LOCAL_PLAYLIST_BYTES as u64 {
        return Err(ManifestRunError::Failed(
            "The staged live HLS playlist exceeds its size limit".to_string(),
        ));
    }
    let bytes = tokio::fs::read(&path).await.map_err(|error| {
        ManifestRunError::Failed(format!(
            "Could not read the staged live HLS recording: {error}"
        ))
    })?;
    HlsPlan::from_local_capture(source_url, &bytes)
        .map(Some)
        .map_err(|error| {
            ManifestRunError::Failed(format!(
                "Could not restore the staged live HLS recording: {error}"
            ))
        })
}

pub(super) async fn restore_local_live_hls_tracks(
    source_url: &str,
    video_dir: &Path,
    audio_dir: &Path,
) -> Result<Option<Tracks>, ManifestRunError> {
    let Some(video) = read_local_live_hls_plan(source_url, video_dir).await? else {
        return Ok(None);
    };
    let audio = read_local_live_hls_plan(source_url, audio_dir).await?;
    Ok(Some(Tracks::Hls {
        video,
        audio,
        subtitle: None,
        subtitle_language: None,
    }))
}

pub(super) async fn read_live_hls_marker(directory: &Path) -> Result<bool, String> {
    let marker = directory.join(LIVE_HLS_MARKER_NAME);
    let metadata = match tokio::fs::symlink_metadata(&marker).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!(
                "Could not inspect live HLS staging marker: {error}"
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("The live HLS staging marker is not a regular file".to_string());
    }
    if metadata.len() != LIVE_HLS_MARKER_CONTENT.len() as u64 {
        return Err("The live HLS staging marker has an unsupported version".to_string());
    }
    let contents = tokio::fs::read(&marker)
        .await
        .map_err(|error| format!("Could not read live HLS staging marker: {error}"))?;
    if contents != LIVE_HLS_MARKER_CONTENT {
        return Err("The live HLS staging marker has an unsupported version".to_string());
    }
    Ok(true)
}

pub(super) async fn ensure_live_hls_marker(directory: &Path) -> Result<(), String> {
    if read_live_hls_marker(directory).await? {
        return Ok(());
    }
    let marker = directory.join(LIVE_HLS_MARKER_NAME);
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
        .await
        .map_err(|error| format!("Could not create live HLS staging marker: {error}"))?;
    if let Err(error) = file.write_all(LIVE_HLS_MARKER_CONTENT).await {
        drop(file);
        let _ = remove_regular_file_if_present(&marker).await;
        return Err(format!("Could not write live HLS staging marker: {error}"));
    }
    if let Err(error) = file.sync_all().await {
        drop(file);
        let _ = remove_regular_file_if_present(&marker).await;
        return Err(format!("Could not flush live HLS staging marker: {error}"));
    }
    Ok(())
}

pub(super) async fn load_live_hls_track(
    plan: &HlsPlan,
    directory: &Path,
) -> Result<LiveHlsTrack, ManifestRunError> {
    let local_playlist = directory.join("local.m3u8");
    let mut accumulator = match tokio::fs::symlink_metadata(&local_playlist).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(ManifestRunError::Failed(
                "The staged live HLS playlist is not a regular file".to_string(),
            ));
        }
        Ok(metadata) if metadata.len() > crate::manifest::MAX_HLS_LOCAL_PLAYLIST_BYTES as u64 => {
            return Err(ManifestRunError::Failed(
                "The staged live HLS playlist exceeds its size limit".to_string(),
            ));
        }
        Ok(_) => {
            let bytes = tokio::fs::read(&local_playlist).await.map_err(|error| {
                ManifestRunError::Failed(format!(
                    "Could not read the staged live HLS recording: {error}"
                ))
            })?;
            HlsLiveAccumulator::from_local_playlist_bytes(&bytes).map_err(|error| {
                ManifestRunError::Failed(format!(
                    "Could not restore the staged live HLS recording: {error}"
                ))
            })?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => HlsLiveAccumulator::new(),
        Err(error) => {
            return Err(ManifestRunError::Failed(format!(
                "Could not inspect the staged live HLS recording: {error}"
            )));
        }
    };

    let current_resources: std::collections::HashMap<&str, &HlsResource> = plan
        .resources
        .iter()
        .map(|resource| (resource.filename.as_str(), resource))
        .collect();
    let restored_names = accumulator
        .resource_names()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let mut total_resource_bytes = 0_u64;
    for filename in restored_names {
        let path = directory.join(&filename);
        let metadata = match tokio::fs::symlink_metadata(&path).await {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(ManifestRunError::Failed(
                    "A staged live HLS resource is not a regular file".to_string(),
                ));
            }
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(ManifestRunError::Failed(format!(
                    "Could not inspect a staged live HLS resource: {error}"
                )));
            }
        };
        let expected = current_resources.get(filename.as_str()).copied();
        let is_key = accumulator.is_key_resource(&filename)
            || expected.is_some_and(|resource| resource.kind == HlsResourceKind::Key);
        let cached_is_valid = metadata.as_ref().is_some_and(|metadata| {
            metadata.len() > 0
                && metadata.len() <= MAX_RESOURCE_BYTES
                && (!is_key || metadata.len() == 16)
                && expected.is_none_or(|resource| {
                    resource
                        .range
                        .is_none_or(|range| metadata.len() == range.length)
                })
        });
        if cached_is_valid {
            if let Some(metadata) = metadata.as_ref() {
                total_resource_bytes = total_resource_bytes
                    .checked_add(metadata.len())
                    .ok_or_else(|| {
                        ManifestRunError::Failed(
                            "The staged live HLS recording byte total overflowed".to_string(),
                        )
                    })?;
            }
        } else {
            if expected.is_none() {
                return Err(ManifestRunError::Failed(
                    "A captured live HLS resource is missing or invalid and no longer appears in the current playlist"
                        .to_string(),
                ));
            }
            accumulator.forget_resource(&filename);
        }
    }
    accumulator
        .restore_resource_bytes(total_resource_bytes)
        .map_err(|error| ManifestRunError::Failed(error.to_string()))?;

    Ok(LiveHlsTrack {
        playlist_url: plan.playlist_url.clone(),
        directory: directory.to_path_buf(),
        accumulator,
        snapshot_at: Instant::now(),
        ended: false,
        next_poll_at: None,
        destination_part: None,
    })
}

const MAX_LIVE_HLS_POLL_INTERVAL: Duration = Duration::from_secs(60);

fn live_hls_poll_interval(target_duration: u64, changed: bool) -> Duration {
    let target = Duration::from_secs(target_duration).min(MAX_LIVE_HLS_POLL_INTERVAL);
    if changed { target } else { target.div_f32(2.0) }
}

pub(super) async fn write_live_playlist(
    directory: &Path,
    accumulator: &HlsLiveAccumulator,
) -> Result<(), String> {
    let bytes = accumulator
        .local_playlist_bytes()
        .map_err(|error| error.to_string())?;
    let target = directory.join("local.m3u8");
    let part = directory.join("local.m3u8.next.part");
    remove_regular_file_if_present(&part).await?;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part)
        .await
        .map_err(|error| format!("Could not create the live HLS playlist: {error}"))?;
    file.write_all(&bytes)
        .await
        .map_err(|error| format!("Could not write the live HLS playlist: {error}"))?;
    file.sync_all()
        .await
        .map_err(|error| format!("Could not flush the live HLS playlist: {error}"))?;
    drop(file);

    match tokio::fs::symlink_metadata(&target).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err("The staged live HLS playlist is not a regular file".to_string());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "Could not inspect the staged live HLS playlist: {error}"
            ));
        }
    }
    tokio::fs::rename(&part, &target)
        .await
        .map_err(|error| format!("Could not publish the live HLS playlist: {error}"))?;
    sync_staging_directory(directory).await
}

async fn fetched_hls_resource_bytes(
    directory: &Path,
    resources: &[HlsResource],
) -> Result<u64, ManifestRunError> {
    let mut total = 0_u64;
    for resource in resources {
        let path = directory.join(&resource.filename);
        let metadata = tokio::fs::symlink_metadata(&path).await.map_err(|error| {
            ManifestRunError::Failed(format!(
                "Could not inspect a fetched live HLS resource: {error}"
            ))
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ManifestRunError::Failed(
                "A fetched live HLS resource is not a regular file".to_string(),
            ));
        }
        let size = metadata.len();
        if size == 0
            || size > MAX_RESOURCE_BYTES
            || resource.range.is_some_and(|range| size != range.length)
            || (resource.kind == HlsResourceKind::Key && size != 16)
        {
            return Err(ManifestRunError::Failed(
                "A fetched live HLS resource has an invalid size".to_string(),
            ));
        }
        total = total.checked_add(size).ok_or_else(|| {
            ManifestRunError::Failed("The live HLS resource byte total overflowed".to_string())
        })?;
    }
    Ok(total)
}

#[expect(
    clippy::too_many_arguments,
    reason = "one rendition update shares its client, cancellation, progress and aggregate byte-budget context"
)]
pub(super) async fn apply_live_hls_snapshot(
    client: &reqwest::Client,
    plan: &HlsPlan,
    track: &mut LiveHlsTrack,
    connections: usize,
    tx: &tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    downloaded: &Arc<AtomicU64>,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
    other_rendition_bytes: u64,
) -> Result<LiveHlsSnapshotOutcome, ManifestRunError> {
    if plan.is_live == plan.end_list {
        if track.accumulator.segment_count() > 0 {
            return Ok(LiveHlsSnapshotOutcome::ContinuityLost);
        }
        return Err(ManifestRunError::Unsupported(
            "The live HLS playlist state is inconsistent".to_string(),
        ));
    }
    if track.destination_part.is_some() && !plan.is_plain_mpegts_presentation() {
        if track.accumulator.segment_count() > 0 {
            return Ok(LiveHlsSnapshotOutcome::Stopped(
                crate::video::StopIntent::Finish,
            ));
        }
        return Err(ManifestRunError::Unsupported(
            "The live HLS playlist is not a plain MPEG-TS presentation".to_string(),
        ));
    }
    let playlist_url = plan.playlist_url.clone();
    let update = match track.accumulator.prepare_update(plan) {
        Ok(update) => update,
        Err(crate::manifest::ManifestError::LiveSequenceGap) => {
            tracing::warn!(
                playlist = %diagnostic_url(&playlist_url),
                "live HLS playlist continuity was lost; preserving captured media"
            );
            return Ok(LiveHlsSnapshotOutcome::ContinuityLost);
        }
        Err(crate::manifest::ManifestError::LiveCaptureLimit) => {
            tracing::warn!(
                playlist = %diagnostic_url(&playlist_url),
                "live HLS capture reached a configured resource limit; preserving captured media"
            );
            return Ok(LiveHlsSnapshotOutcome::ContinuityLost);
        }
        Err(error) if track.accumulator.segment_count() > 0 => {
            tracing::warn!(
                playlist = %diagnostic_url(&playlist_url),
                error = %error,
                "live HLS update is no longer supported; preserving captured media"
            );
            return Ok(LiveHlsSnapshotOutcome::ContinuityLost);
        }
        Err(error) => return Err(ManifestRunError::Unsupported(error.to_string())),
    };
    let changed = update.changed;
    let end_list = update.end_list;
    let next_poll_at = if end_list {
        None
    } else {
        match track
            .snapshot_at
            .checked_add(live_hls_poll_interval(plan.source_target_duration, changed))
        {
            Some(deadline) => Some(deadline),
            None if track.accumulator.segment_count() > 0 => {
                tracing::warn!(
                    playlist = %diagnostic_url(&playlist_url),
                    "live HLS reload deadline is out of range; preserving captured media"
                );
                return Ok(LiveHlsSnapshotOutcome::ContinuityLost);
            }
            None => {
                return Err(ManifestRunError::Unsupported(
                    "The live HLS target duration is too large".to_string(),
                ));
            }
        }
    };
    let resources = hls_resources_from(&update.resources);
    let fetch_outcome = fetch_resources_with_stop(
        client,
        resources,
        &track.directory,
        connections,
        tx,
        downloaded,
        abort,
    )
    .await;
    let fetch_outcome = match fetch_outcome {
        Err(ManifestRunError::Unsupported(error)) if track.accumulator.segment_count() > 0 => {
            tracing::warn!(
                playlist = %diagnostic_url(&playlist_url),
                error = %error,
                "live HLS resource update is no longer supported; preserving captured media"
            );
            return Ok(LiveHlsSnapshotOutcome::ContinuityLost);
        }
        Err(error) => return Err(error),
        Ok(outcome) => outcome,
    };
    match fetch_outcome {
        ResourceFetchOutcome::Complete => {}
        ResourceFetchOutcome::Stopped(intent) => {
            return Ok(LiveHlsSnapshotOutcome::Stopped(intent));
        }
    }

    let added_resource_bytes =
        fetched_hls_resource_bytes(&track.directory, &update.resources).await?;
    if track.destination_part.is_some() {
        for resource in &update.resources {
            if resource.kind != HlsResourceKind::Segment
                || resource.range.is_some()
                || !super::is_mpegts_segment(&track.directory.join(&resource.filename)).await?
            {
                if track.accumulator.segment_count() > 0 {
                    return Ok(LiveHlsSnapshotOutcome::Stopped(
                        crate::video::StopIntent::Finish,
                    ));
                }
                return Err(ManifestRunError::Unsupported(
                    "A live HLS segment is not a compatible MPEG-TS fragment".to_string(),
                ));
            }
        }
    }
    match track.accumulator.commit_update_with_budget(
        update,
        added_resource_bytes,
        other_rendition_bytes,
    ) {
        Ok(()) => {}
        Err(crate::manifest::ManifestError::LiveCaptureLimit) => {
            tracing::warn!(
                playlist = %diagnostic_url(&playlist_url),
                "live HLS capture reached a configured resource limit; preserving captured media"
            );
            return Ok(LiveHlsSnapshotOutcome::ContinuityLost);
        }
        Err(error) if track.accumulator.segment_count() > 0 => {
            tracing::warn!(
                playlist = %diagnostic_url(&playlist_url),
                error = %error,
                "live HLS commit is no longer supported; preserving captured media"
            );
            return Ok(LiveHlsSnapshotOutcome::ContinuityLost);
        }
        Err(error) => return Err(ManifestRunError::Unsupported(error.to_string())),
    }
    write_live_playlist(&track.directory, &track.accumulator)
        .await
        .map_err(ManifestRunError::Failed)?;
    if let Some(intent) = append_live_hls_destination_part(track, abort).await? {
        return Ok(LiveHlsSnapshotOutcome::Stopped(intent));
    }
    track.playlist_url = playlist_url;
    track.ended = end_list;
    track.next_poll_at = next_poll_at;
    Ok(LiveHlsSnapshotOutcome::Committed)
}

async fn live_hls_track_has_only_mpegts_resources(
    track: &LiveHlsTrack,
) -> Result<bool, ManifestRunError> {
    let Some(resources) = track.accumulator.plain_mpegts_resource_names() else {
        return Ok(false);
    };
    for filename in resources {
        if !super::is_mpegts_segment(&track.directory.join(filename)).await? {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn append_live_hls_destination_part(
    track: &LiveHlsTrack,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
) -> Result<Option<crate::video::StopIntent>, ManifestRunError> {
    let Some(destination) = track.destination_part.as_ref() else {
        return Ok(None);
    };
    let resources = track
        .accumulator
        .plain_mpegts_resource_names()
        .ok_or_else(|| {
            ManifestRunError::Failed(
                "The live HLS recording no longer matches its MPEG-TS destination part".to_string(),
            )
        })?;
    if resources.is_empty() {
        return Ok(None);
    }

    let (writer, _) =
        crate::manifest_writer::OrderedTrackWriter::open_appendable_destination_part_retaining_sources(
            resources.clone(),
            &track.directory,
            &destination.staging,
            destination.filename,
            &destination.destination,
            destination.item_id,
            destination.track,
        )
        .await
        .map_err(ManifestRunError::Failed)?;
    let Some(mut writer) = writer else {
        return Ok(None);
    };

    while writer.next_resource_index() < resources.len() {
        let index = writer.next_resource_index();
        let filename = resources.get(index).ok_or_else(|| {
            ManifestRunError::Failed(
                "The live HLS destination-part checkpoint exceeds its resource plan".to_string(),
            )
        })?;
        if !super::is_mpegts_segment(&track.directory.join(filename)).await? {
            return Err(ManifestRunError::Failed(
                "A staged live HLS segment is not a compatible MPEG-TS fragment".to_string(),
            ));
        }
        match writer
            .append_next_outcome(abort)
            .await
            .map_err(ManifestRunError::Failed)?
        {
            crate::manifest_writer::AppendNextOutcome::Committed => {}
            crate::manifest_writer::AppendNextOutcome::Stopped(intent) => {
                return Ok(Some(intent));
            }
        }
    }

    writer.finish().await.map_err(ManifestRunError::Failed)?;
    Ok(None)
}

pub(super) struct SavedLiveHlsDestinationParts<'a> {
    pub(super) source_url: &'a str,
    pub(super) staging: &'a Path,
    pub(super) destination: &'a Path,
    pub(super) item_id: u64,
    pub(super) video_dir: &'a Path,
    pub(super) audio_dir: &'a Path,
    pub(super) has_audio: bool,
}

pub(super) async fn complete_saved_live_hls_destination_parts(
    context: SavedLiveHlsDestinationParts<'_>,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
) -> Result<Option<crate::video::StopIntent>, ManifestRunError> {
    let SavedLiveHlsDestinationParts {
        source_url,
        staging,
        destination,
        item_id,
        video_dir,
        audio_dir,
        has_audio,
    } = context;
    let Some(video_plan) = read_local_live_hls_plan(source_url, video_dir).await? else {
        return Ok(None);
    };
    if !video_plan.is_plain_mpegts_presentation() {
        return Ok(None);
    }

    let mut video = load_live_hls_track(&video_plan, video_dir).await?;
    video.destination_part = Some(LiveHlsDestination {
        staging: staging.to_path_buf(),
        destination: destination.to_path_buf(),
        item_id,
        track: crate::manifest_destination::TrackKind::Video,
        filename: "video-track.ts",
    });
    if let Some(intent) = append_live_hls_destination_part(&video, abort).await? {
        return Ok(Some(intent));
    }

    if has_audio
        && let Some(audio_plan) = read_local_live_hls_plan(source_url, audio_dir).await?
        && audio_plan.is_plain_mpegts_presentation()
    {
        let mut audio = load_live_hls_track(&audio_plan, audio_dir).await?;
        audio.destination_part = Some(LiveHlsDestination {
            staging: staging.to_path_buf(),
            destination: destination.to_path_buf(),
            item_id,
            track: crate::manifest_destination::TrackKind::Audio,
            filename: "audio-track.ts",
        });
        if let Some(intent) = append_live_hls_destination_part(&audio, abort).await? {
            return Ok(Some(intent));
        }
    }

    Ok(None)
}

#[expect(
    clippy::too_many_arguments,
    reason = "one rendition poll carries the same guarded-network and cancellation context into its update"
)]
async fn poll_live_hls_track(
    client: &reqwest::Client,
    source_url: &str,
    track: &mut LiveHlsTrack,
    connections: usize,
    tx: &tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    downloaded: &Arc<AtomicU64>,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
    guarded_child_hosts: &mut std::collections::HashSet<String>,
    other_rendition_bytes: u64,
) -> Result<LiveHlsSnapshotOutcome, ManifestRunError> {
    let result = tokio::select! {
        biased;
        stop = &mut *abort => {
            return Ok(LiveHlsSnapshotOutcome::Stopped(
                stop.unwrap_or(crate::video::StopIntent::Discard),
            ));
        }
        result = crate::manifest_intake::lookup_hls_media(
            client,
            track.playlist_url.as_str(),
        ) => result,
    };
    let plan = match result {
        Ok(plan) => plan,
        Err(crate::manifest_intake::ManifestIntakeError::Unsupported(error))
            if track.accumulator.segment_count() > 0 =>
        {
            tracing::warn!(
                playlist = %diagnostic_url(&track.playlist_url),
                error = %error,
                "live HLS playlist is no longer supported; preserving captured media"
            );
            return Ok(LiveHlsSnapshotOutcome::ContinuityLost);
        }
        Err(error) => return Err(error.into()),
    };
    track.snapshot_at = Instant::now();
    if let Err(error) =
        guard_manifest_child_url(source_url, &plan.playlist_url, guarded_child_hosts)
    {
        if track.accumulator.segment_count() > 0 {
            tracing::warn!(
                playlist = %diagnostic_url(&plan.playlist_url),
                error = %error,
                "live HLS playlist host is no longer eligible; preserving captured media"
            );
            return Ok(LiveHlsSnapshotOutcome::ContinuityLost);
        }
        return Err(ManifestRunError::Unsupported(error.to_string()));
    }
    if let Err(error) = guard_hls_resources(source_url, &plan, guarded_child_hosts) {
        if track.accumulator.segment_count() > 0 {
            tracing::warn!(
                playlist = %diagnostic_url(&plan.playlist_url),
                error = %error,
                "live HLS resource host is no longer eligible; preserving captured media"
            );
            return Ok(LiveHlsSnapshotOutcome::ContinuityLost);
        }
        return Err(ManifestRunError::Unsupported(error.to_string()));
    }
    apply_live_hls_snapshot(
        client,
        &plan,
        track,
        connections,
        tx,
        downloaded,
        abort,
        other_rendition_bytes,
    )
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "the paired live renditions share one bounded capture session and cancellation signal"
)]
pub(super) async fn run_live_hls_tracks(
    client: &reqwest::Client,
    source_url: &str,
    destination_dir: &Path,
    destination: &Path,
    item_id: u64,
    staging: &Path,
    video_plan: &HlsPlan,
    audio_plan: Option<&HlsPlan>,
    finish_only: bool,
    video_dir: &Path,
    audio_dir: &Path,
    connections: usize,
    tx: &tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    downloaded: &Arc<AtomicU64>,
    guarded_child_hosts: &mut std::collections::HashSet<String>,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
) -> Result<LiveHlsCapture, ManifestRunError> {
    let mut video = load_live_hls_track(video_plan, video_dir).await?;
    let mut audio = match audio_plan {
        Some(plan) => Some(load_live_hls_track(plan, audio_dir).await?),
        None => None,
    };
    let mut use_destination_parts = video_plan.is_plain_mpegts_presentation()
        && audio_plan.is_none_or(HlsPlan::is_plain_mpegts_presentation);
    if use_destination_parts {
        use_destination_parts = live_hls_track_has_only_mpegts_resources(&video).await?;
        if use_destination_parts && let Some(track) = audio.as_ref() {
            use_destination_parts = live_hls_track_has_only_mpegts_resources(track).await?;
        }
    }
    if use_destination_parts {
        video.destination_part = Some(LiveHlsDestination {
            staging: staging.to_path_buf(),
            destination: destination.to_path_buf(),
            item_id,
            track: crate::manifest_destination::TrackKind::Video,
            filename: "video-track.ts",
        });
        if let Some(track) = audio.as_mut() {
            track.destination_part = Some(LiveHlsDestination {
                staging: staging.to_path_buf(),
                destination: destination.to_path_buf(),
                item_id,
                track: crate::manifest_destination::TrackKind::Audio,
                filename: "audio-track.ts",
            });
        }
    }
    let output = if use_destination_parts {
        super::LiveHlsOutput::DestinationParts
    } else {
        super::LiveHlsOutput::LocalPlaylist
    };
    let retained_bytes = video
        .accumulator
        .resource_bytes()
        .checked_add(
            audio
                .as_ref()
                .map_or(0, |track| track.accumulator.resource_bytes()),
        )
        .filter(|total| *total <= crate::manifest::MAX_HLS_LIVE_CAPTURE_BYTES)
        .ok_or_else(|| {
            ManifestRunError::Failed(
                "The staged live HLS recording exceeds Grab's native byte limit".to_string(),
            )
        })?;
    downloaded.store(retained_bytes, Ordering::Relaxed);
    ensure_manifest_mux_spaces(staging, destination_dir, retained_bytes)?;
    if use_destination_parts {
        if let Some(intent) = append_live_hls_destination_part(&video, abort).await? {
            return Ok(LiveHlsCapture::new(
                match intent {
                    crate::video::StopIntent::Pause => LiveHlsCaptureOutcome::Paused,
                    crate::video::StopIntent::Finish => LiveHlsCaptureOutcome::Finish,
                    crate::video::StopIntent::Discard => LiveHlsCaptureOutcome::Discard,
                },
                output,
            ));
        }
        if let Some(audio) = audio.as_ref()
            && let Some(intent) = append_live_hls_destination_part(audio, abort).await?
        {
            return Ok(LiveHlsCapture::new(
                match intent {
                    crate::video::StopIntent::Pause => LiveHlsCaptureOutcome::Paused,
                    crate::video::StopIntent::Finish => LiveHlsCaptureOutcome::FinishVideoOnly,
                    crate::video::StopIntent::Discard => LiveHlsCaptureOutcome::Discard,
                },
                output,
            ));
        }
    }
    if finish_only {
        return Ok(LiveHlsCapture::new(LiveHlsCaptureOutcome::Finish, output));
    }
    let staging_dir = staging;

    let audio_resource_bytes = audio
        .as_ref()
        .map_or(0, |track| track.accumulator.resource_bytes());
    if video
        .accumulator
        .resource_bytes()
        .checked_add(audio_resource_bytes)
        .is_none_or(|total| total > crate::manifest::MAX_HLS_LIVE_CAPTURE_BYTES)
    {
        return Ok(LiveHlsCapture::new(
            if video.accumulator.segment_count() > 0 {
                LiveHlsCaptureOutcome::FinishVideoOnly
            } else {
                LiveHlsCaptureOutcome::Discard
            },
            output,
        ));
    }

    let all_tracks_ended = video_plan.end_list && audio_plan.is_none_or(|plan| plan.end_list);
    if !all_tracks_ended {
        guard_manifest_child_url(source_url, &video_plan.playlist_url, guarded_child_hosts)
            .map_err(|error| ManifestRunError::Unsupported(error.to_string()))?;
        if let Some(plan) = audio_plan {
            guard_manifest_child_url(source_url, &plan.playlist_url, guarded_child_hosts)
                .map_err(|error| ManifestRunError::Unsupported(error.to_string()))?;
        }
    }

    match apply_live_hls_snapshot(
        client,
        video_plan,
        &mut video,
        connections,
        tx,
        downloaded,
        abort,
        audio_resource_bytes,
    )
    .await?
    {
        LiveHlsSnapshotOutcome::Committed => {
            ensure_manifest_mux_spaces(
                staging_dir,
                destination_dir,
                downloaded.load(Ordering::Relaxed),
            )?;
        }
        LiveHlsSnapshotOutcome::Stopped(intent) => {
            return Ok(LiveHlsCapture::new(
                match intent {
                    crate::video::StopIntent::Pause => LiveHlsCaptureOutcome::Paused,
                    crate::video::StopIntent::Finish => LiveHlsCaptureOutcome::Finish,
                    crate::video::StopIntent::Discard => LiveHlsCaptureOutcome::Discard,
                },
                output,
            ));
        }
        LiveHlsSnapshotOutcome::ContinuityLost => {
            if video.accumulator.segment_count() == 0 {
                return Ok(LiveHlsCapture::new(LiveHlsCaptureOutcome::Discard, output));
            }
            persist_unrecoverable_live_capture(
                staging_dir,
                ManifestKind::Hls,
                "HLS playlist continuity could not be verified",
                video.accumulator.resource_bytes(),
                downloaded,
                tx,
            )
            .await?;
            return Ok(LiveHlsCapture::new(
                LiveHlsCaptureOutcome::Unrecoverable,
                output,
            ));
        }
    }
    if let (Some(plan), Some(track)) = (audio_plan, audio.as_mut()) {
        let audio_outcome = apply_live_hls_snapshot(
            client,
            plan,
            track,
            connections,
            tx,
            downloaded,
            abort,
            video.accumulator.resource_bytes(),
        )
        .await;
        let audio_outcome = match audio_outcome {
            Err(ManifestRunError::Unsupported(error)) => {
                tracing::warn!(
                    playlist = %diagnostic_url(&plan.playlist_url),
                    error = %error,
                    "live HLS audio rendition is no longer supported; preserving video capture"
                );
                return Ok(LiveHlsCapture::new(
                    LiveHlsCaptureOutcome::FinishVideoOnly,
                    output,
                ));
            }
            Err(error) => return Err(error),
            Ok(outcome) => outcome,
        };
        match audio_outcome {
            LiveHlsSnapshotOutcome::Committed => {
                ensure_manifest_mux_spaces(
                    staging_dir,
                    destination_dir,
                    downloaded.load(Ordering::Relaxed),
                )?;
            }
            LiveHlsSnapshotOutcome::Stopped(intent) => {
                return Ok(LiveHlsCapture::new(
                    match intent {
                        crate::video::StopIntent::Pause => LiveHlsCaptureOutcome::Paused,
                        crate::video::StopIntent::Finish => LiveHlsCaptureOutcome::FinishVideoOnly,
                        crate::video::StopIntent::Discard => LiveHlsCaptureOutcome::Discard,
                    },
                    output,
                ));
            }
            LiveHlsSnapshotOutcome::ContinuityLost => {
                if video.accumulator.segment_count() == 0 {
                    return Ok(LiveHlsCapture::new(LiveHlsCaptureOutcome::Discard, output));
                }
                persist_unrecoverable_live_capture(
                    staging_dir,
                    ManifestKind::Hls,
                    "HLS playlist continuity could not be verified",
                    video.accumulator.resource_bytes(),
                    downloaded,
                    tx,
                )
                .await?;
                return Ok(LiveHlsCapture::new(
                    LiveHlsCaptureOutcome::Unrecoverable,
                    output,
                ));
            }
        }
    }

    while !video.ended || audio.as_ref().is_some_and(|track| !track.ended) {
        let next_poll = [
            (!video.ended).then_some(video.next_poll_at).flatten(),
            audio
                .as_ref()
                .filter(|track| !track.ended)
                .and_then(|track| track.next_poll_at),
        ]
        .into_iter()
        .flatten()
        .min();
        let Some(next_poll) = next_poll else {
            return Err(ManifestRunError::Failed(
                "A live HLS playlist has no next reload deadline".to_string(),
            ));
        };
        tokio::select! {
            biased;
            stop = &mut *abort => {
                return Ok(LiveHlsCapture::new(
                    match stop.unwrap_or(crate::video::StopIntent::Discard) {
                        crate::video::StopIntent::Pause => LiveHlsCaptureOutcome::Paused,
                        crate::video::StopIntent::Finish => LiveHlsCaptureOutcome::Finish,
                        crate::video::StopIntent::Discard => LiveHlsCaptureOutcome::Discard,
                    },
                    output,
                ));
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(next_poll)) => {}
        }

        let now = Instant::now();
        if !video.ended && video.next_poll_at.is_some_and(|deadline| deadline <= now) {
            match poll_live_hls_track(
                client,
                source_url,
                &mut video,
                connections,
                tx,
                downloaded,
                abort,
                guarded_child_hosts,
                audio
                    .as_ref()
                    .map_or(0, |track| track.accumulator.resource_bytes()),
            )
            .await?
            {
                LiveHlsSnapshotOutcome::Committed => {
                    ensure_manifest_mux_spaces(
                        staging_dir,
                        destination_dir,
                        downloaded.load(Ordering::Relaxed),
                    )?;
                }
                LiveHlsSnapshotOutcome::Stopped(intent) => {
                    return Ok(LiveHlsCapture::new(
                        match intent {
                            crate::video::StopIntent::Pause => LiveHlsCaptureOutcome::Paused,
                            crate::video::StopIntent::Finish => LiveHlsCaptureOutcome::Finish,
                            crate::video::StopIntent::Discard => LiveHlsCaptureOutcome::Discard,
                        },
                        output,
                    ));
                }
                LiveHlsSnapshotOutcome::ContinuityLost => {
                    if video.accumulator.segment_count() == 0 {
                        return Ok(LiveHlsCapture::new(LiveHlsCaptureOutcome::Discard, output));
                    }
                    persist_unrecoverable_live_capture(
                        staging_dir,
                        ManifestKind::Hls,
                        "HLS playlist continuity could not be verified",
                        video.accumulator.resource_bytes(),
                        downloaded,
                        tx,
                    )
                    .await?;
                    return Ok(LiveHlsCapture::new(
                        LiveHlsCaptureOutcome::Unrecoverable,
                        output,
                    ));
                }
            }
        }
        if let Some(audio) = audio.as_mut()
            && !audio.ended
            && audio
                .next_poll_at
                .is_some_and(|deadline| deadline <= Instant::now())
        {
            match poll_live_hls_track(
                client,
                source_url,
                audio,
                connections,
                tx,
                downloaded,
                abort,
                guarded_child_hosts,
                video.accumulator.resource_bytes(),
            )
            .await?
            {
                LiveHlsSnapshotOutcome::Committed => {
                    ensure_manifest_mux_spaces(
                        staging_dir,
                        destination_dir,
                        downloaded.load(Ordering::Relaxed),
                    )?;
                }
                LiveHlsSnapshotOutcome::Stopped(intent) => {
                    return Ok(LiveHlsCapture::new(
                        match intent {
                            crate::video::StopIntent::Pause => LiveHlsCaptureOutcome::Paused,
                            crate::video::StopIntent::Finish => {
                                LiveHlsCaptureOutcome::FinishVideoOnly
                            }
                            crate::video::StopIntent::Discard => LiveHlsCaptureOutcome::Discard,
                        },
                        output,
                    ));
                }
                LiveHlsSnapshotOutcome::ContinuityLost => {
                    if video.accumulator.segment_count() == 0 {
                        return Ok(LiveHlsCapture::new(LiveHlsCaptureOutcome::Discard, output));
                    }
                    persist_unrecoverable_live_capture(
                        staging_dir,
                        ManifestKind::Hls,
                        "HLS playlist continuity could not be verified",
                        video.accumulator.resource_bytes(),
                        downloaded,
                        tx,
                    )
                    .await?;
                    return Ok(LiveHlsCapture::new(
                        LiveHlsCaptureOutcome::Unrecoverable,
                        output,
                    ));
                }
            }
        }
    }

    Ok(LiveHlsCapture::new(LiveHlsCaptureOutcome::Ended, output))
}

#[cfg(test)]
mod destination_part_tests {
    use super::*;

    const BASE_URL: &str = "https://cdn.example.test/live.m3u8";

    fn transport_stream(seed: u8) -> Vec<u8> {
        let mut bytes = vec![0_u8; 188 * 3];
        for offset in [0, 188, 376] {
            bytes[offset] = 0x47;
        }
        bytes[4] = seed;
        bytes
    }

    async fn stage_update_resources(
        resources: &[HlsResource],
        directory: &Path,
        first_seed: u8,
    ) -> (Vec<Vec<u8>>, u64) {
        let mut bodies = Vec::with_capacity(resources.len());
        let mut bytes = 0_u64;
        for (index, resource) in resources.iter().enumerate() {
            let body = transport_stream(first_seed.saturating_add(index as u8));
            tokio::fs::write(directory.join(&resource.filename), &body)
                .await
                .expect("stage live HLS MPEG-TS resource");
            bytes = bytes
                .checked_add(body.len() as u64)
                .expect("test fixture byte count stays in range");
            bodies.push(body);
        }
        (bodies, bytes)
    }

    #[test]
    fn live_hls_poll_interval_is_bounded() {
        assert_eq!(live_hls_poll_interval(3_600, true), Duration::from_secs(60));
        assert_eq!(
            live_hls_poll_interval(3_600, false),
            Duration::from_secs(30)
        );
        assert_eq!(live_hls_poll_interval(4, false), Duration::from_secs(2));
    }

    #[tokio::test]
    async fn live_hls_destination_part_appends_in_playlist_order_and_resumes_without_duplication() {
        let staging = tempfile::tempdir().expect("create live HLS staging");
        let video_dir = staging.path().join("video");
        tokio::fs::create_dir(&video_dir)
            .await
            .expect("create live HLS video directory");
        let destination_dir = tempfile::tempdir().expect("create live HLS destination");
        let destination = destination_dir.path().join("recording.mp4");
        crate::manifest_destination::ensure_plan(staging.path(), &destination, 73, false, false)
            .expect("persist live HLS destination plan");

        let first_plan = crate::manifest::plan_hls_media_playlist(
            BASE_URL,
            b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:4,\n100.ts\n#EXTINF:4,\n101.ts\n",
        )
        .expect("plan initial live HLS window");
        let mut accumulator = HlsLiveAccumulator::new();
        let first_update = accumulator
            .prepare_update(&first_plan)
            .expect("prepare first HLS window");
        let (first_bodies, first_bytes) =
            stage_update_resources(&first_update.resources, &video_dir, 1).await;
        accumulator
            .commit_update_with_budget(first_update, first_bytes, 0)
            .expect("commit first HLS window");
        write_live_playlist(&video_dir, &accumulator)
            .await
            .expect("persist first local HLS playlist");

        let destination_state = LiveHlsDestination {
            staging: staging.path().to_path_buf(),
            destination: destination.clone(),
            item_id: 73,
            track: crate::manifest_destination::TrackKind::Video,
            filename: "video-track.ts",
        };
        let mut track = LiveHlsTrack {
            playlist_url: first_plan.playlist_url.clone(),
            directory: video_dir.clone(),
            accumulator,
            snapshot_at: Instant::now(),
            ended: false,
            next_poll_at: None,
            destination_part: Some(destination_state.clone()),
        };
        let (_stop_tx, mut abort) = tokio::sync::oneshot::channel();
        assert_eq!(
            append_live_hls_destination_part(&track, &mut abort)
                .await
                .expect("append initial live HLS window"),
            None
        );

        let part = crate::manifest_destination::part_path(
            staging.path(),
            &destination,
            73,
            crate::manifest_destination::TrackKind::Video,
        )
        .expect("resolve visible HLS destination part");
        let expected_first = first_bodies.concat();
        assert_eq!(
            tokio::fs::read(&part).await.expect("read initial HLS part"),
            expected_first
        );
        for resource in &first_plan.resources {
            assert!(
                tokio::fs::symlink_metadata(video_dir.join(&resource.filename))
                    .await
                    .is_ok(),
                "source resources remain available to playlist-based recovery"
            );
        }

        let next_plan = crate::manifest::plan_hls_media_playlist(
            BASE_URL,
            b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:101\n#EXTINF:4,\n101.ts\n#EXTINF:4,\n102.ts\n",
        )
        .expect("plan overlapping live HLS window");
        let next_update = track
            .accumulator
            .prepare_update(&next_plan)
            .expect("prepare next HLS window");
        let (next_bodies, next_bytes) =
            stage_update_resources(&next_update.resources, &video_dir, 20).await;
        track
            .accumulator
            .commit_update_with_budget(next_update, next_bytes, 0)
            .expect("commit next HLS window");
        write_live_playlist(&video_dir, &track.accumulator)
            .await
            .expect("persist extended local HLS playlist");
        assert_eq!(
            append_live_hls_destination_part(&track, &mut abort)
                .await
                .expect("append next live HLS window"),
            None
        );

        let mut expected = expected_first;
        expected.extend_from_slice(&next_bodies.concat());
        assert_eq!(
            tokio::fs::read(&part)
                .await
                .expect("read extended HLS part"),
            expected
        );

        let mut resumed = load_live_hls_track(&next_plan, &video_dir)
            .await
            .expect("restore saved live HLS capture");
        resumed.destination_part = Some(destination_state);
        assert_eq!(
            append_live_hls_destination_part(&resumed, &mut abort)
                .await
                .expect("resume already committed HLS part"),
            None
        );
        assert_eq!(
            tokio::fs::read(&part).await.expect("read resumed HLS part"),
            expected,
            "a resumed writer must not duplicate its committed prefix"
        );
        assert!(
            tokio::fs::symlink_metadata(video_dir.join("video-track.ts.checkpoint.json"))
                .await
                .is_ok(),
            "the HLS writer checkpoint remains in XDG staging"
        );
        for resource in resumed.accumulator.resource_names() {
            assert!(
                tokio::fs::symlink_metadata(video_dir.join(resource))
                    .await
                    .is_ok(),
                "committed HLS source resources remain in XDG staging"
            );
        }
    }
}
