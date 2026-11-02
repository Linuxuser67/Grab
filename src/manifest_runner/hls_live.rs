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
    })
}

fn live_hls_poll_interval(target_duration: u64, changed: bool) -> Duration {
    let target = Duration::from_secs(target_duration);
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
    track.playlist_url = playlist_url;
    track.ended = end_list;
    track.next_poll_at = next_poll_at;
    Ok(LiveHlsSnapshotOutcome::Committed)
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
) -> Result<LiveHlsCaptureOutcome, ManifestRunError> {
    let mut video = load_live_hls_track(video_plan, video_dir).await?;
    let mut audio = match audio_plan {
        Some(plan) => Some(load_live_hls_track(plan, audio_dir).await?),
        None => None,
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
    let staging_dir = video_dir.parent().unwrap_or(video_dir);
    ensure_manifest_mux_spaces(staging_dir, destination_dir, retained_bytes)?;
    if finish_only {
        return Ok(LiveHlsCaptureOutcome::Finish);
    }

    let audio_resource_bytes = audio
        .as_ref()
        .map_or(0, |track| track.accumulator.resource_bytes());
    if video
        .accumulator
        .resource_bytes()
        .checked_add(audio_resource_bytes)
        .is_none_or(|total| total > crate::manifest::MAX_HLS_LIVE_CAPTURE_BYTES)
    {
        return Ok(if video.accumulator.segment_count() > 0 {
            LiveHlsCaptureOutcome::FinishVideoOnly
        } else {
            LiveHlsCaptureOutcome::Discard
        });
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
            return Ok(match intent {
                crate::video::StopIntent::Pause => LiveHlsCaptureOutcome::Paused,
                crate::video::StopIntent::Finish => LiveHlsCaptureOutcome::Finish,
                crate::video::StopIntent::Discard => LiveHlsCaptureOutcome::Discard,
            });
        }
        LiveHlsSnapshotOutcome::ContinuityLost => {
            if video.accumulator.segment_count() == 0 {
                return Ok(LiveHlsCaptureOutcome::Discard);
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
            return Ok(LiveHlsCaptureOutcome::Unrecoverable);
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
                return Ok(LiveHlsCaptureOutcome::FinishVideoOnly);
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
                return Ok(match intent {
                    crate::video::StopIntent::Pause => LiveHlsCaptureOutcome::Paused,
                    crate::video::StopIntent::Finish => LiveHlsCaptureOutcome::FinishVideoOnly,
                    crate::video::StopIntent::Discard => LiveHlsCaptureOutcome::Discard,
                });
            }
            LiveHlsSnapshotOutcome::ContinuityLost => {
                if video.accumulator.segment_count() == 0 {
                    return Ok(LiveHlsCaptureOutcome::Discard);
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
                return Ok(LiveHlsCaptureOutcome::Unrecoverable);
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
                return Ok(match stop.unwrap_or(crate::video::StopIntent::Discard) {
                    crate::video::StopIntent::Pause => LiveHlsCaptureOutcome::Paused,
                    crate::video::StopIntent::Finish => LiveHlsCaptureOutcome::Finish,
                    crate::video::StopIntent::Discard => LiveHlsCaptureOutcome::Discard,
                });
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
                    return Ok(match intent {
                        crate::video::StopIntent::Pause => LiveHlsCaptureOutcome::Paused,
                        crate::video::StopIntent::Finish => LiveHlsCaptureOutcome::Finish,
                        crate::video::StopIntent::Discard => LiveHlsCaptureOutcome::Discard,
                    });
                }
                LiveHlsSnapshotOutcome::ContinuityLost => {
                    if video.accumulator.segment_count() == 0 {
                        return Ok(LiveHlsCaptureOutcome::Discard);
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
                    return Ok(LiveHlsCaptureOutcome::Unrecoverable);
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
                    return Ok(match intent {
                        crate::video::StopIntent::Pause => LiveHlsCaptureOutcome::Paused,
                        crate::video::StopIntent::Finish => LiveHlsCaptureOutcome::FinishVideoOnly,
                        crate::video::StopIntent::Discard => LiveHlsCaptureOutcome::Discard,
                    });
                }
                LiveHlsSnapshotOutcome::ContinuityLost => {
                    if video.accumulator.segment_count() == 0 {
                        return Ok(LiveHlsCaptureOutcome::Discard);
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
                    return Ok(LiveHlsCaptureOutcome::Unrecoverable);
                }
            }
        }
    }

    Ok(LiveHlsCaptureOutcome::Ended)
}
