//! HLS live-capture staging, polling, and recovery.

use super::*;

const LIVE_HLS_STEERING_REFRESH_TIMEOUT: Duration = Duration::from_secs(3);

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
        video: Box::new(video),
        audio: audio.map(Box::new),
        subtitle: None,
        subtitle_language: None,
        content_steering: None,
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
        requested_playlist_url: plan.requested_playlist_url.clone(),
        inherited_variables: plan.inherited_variables.clone(),
        directory: directory.to_path_buf(),
        accumulator,
        snapshot_at: Instant::now(),
        last_full_resource_verification: Instant::now(),
        ended: false,
        next_poll_at: None,
        target_duration: plan.source_target_duration,
        part_target: plan.part_target,
        blocking_reload: plan.blocking_reload,
        can_skip_until: plan.can_skip_until,
        rendition_reports: plan.rendition_reports.clone(),
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

async fn missing_hls_resources(
    directory: &Path,
    resources: &[HlsResource],
) -> Result<Vec<HlsResource>, ManifestRunError> {
    let mut missing = Vec::new();
    for resource in resources {
        let path = directory.join(&resource.filename);
        match tokio::fs::symlink_metadata(&path).await {
            Ok(metadata) if metadata.file_type().is_file() => {
                let size = metadata.len();
                let valid_size = size > 0
                    && size <= MAX_RESOURCE_BYTES
                    && resource.range.is_none_or(|range| size == range.length)
                    && (resource.kind != HlsResourceKind::Key || size == 16);
                if valid_size {
                    continue;
                }
                tokio::fs::remove_file(&path).await.map_err(|error| {
                    manifest_storage_error("Could not remove an invalid staged HLS resource", error)
                })?;
                missing.push(resource.clone());
            }
            Ok(_) => {
                return Err(ManifestRunError::Unsupported(
                    "A staged HLS resource path is not a regular file".to_string(),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(resource.clone());
            }
            Err(error) => {
                return Err(manifest_storage_error(
                    "Could not inspect a staged HLS resource",
                    error,
                ));
            }
        }
    }
    Ok(missing)
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

const LIVE_HLS_FULL_RESOURCE_VERIFY_INTERVAL: Duration = Duration::from_secs(60);

async fn verify_live_hls_resource_files<'a>(
    directory: &Path,
    filenames: impl Iterator<Item = &'a str>,
    has_committed_media: bool,
) -> Result<(), ManifestRunError> {
    for filename in filenames {
        let path = directory.join(filename);
        let metadata = match tokio::fs::symlink_metadata(&path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let message = if has_committed_media {
                    "An LL-HLS resource referenced by the recording is missing"
                } else {
                    "An LL-HLS resource referenced by the local playlist is missing"
                };
                return Err(ManifestRunError::Unsupported(message.to_string()));
            }
            Err(error) => {
                return Err(manifest_storage_error(
                    "Could not verify an HLS staged resource",
                    error,
                ));
            }
        };
        if !metadata.file_type().is_file() {
            return Err(ManifestRunError::Unsupported(
                "An LL-HLS resource referenced by the recording is not a regular file".to_string(),
            ));
        }
    }
    Ok(())
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
    let reload_delay = if plan.blocking_reload.is_some() && changed {
        Duration::ZERO
    } else if let Some(part_target) = plan.part_target {
        part_target.min(MAX_LIVE_HLS_POLL_INTERVAL)
    } else {
        live_hls_poll_interval(plan.source_target_duration, changed)
    };
    let next_poll_at = if end_list {
        None
    } else {
        match track.snapshot_at.checked_add(reload_delay) {
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
    // LL-HLS snapshots intentionally include overlapping resources so a
    // missing staged file can be repaired. Do not fetch an already-staged
    // regular file again (notably the shared EXT-X-MAP).
    let missing_resources = match missing_hls_resources(&track.directory, &update.resources).await {
        Ok(resources) => resources,
        Err(ManifestRunError::Unsupported(error)) if track.accumulator.segment_count() > 0 => {
            tracing::warn!(
                playlist = %diagnostic_url(&playlist_url),
                error = %error,
                "a staged live HLS resource became unsafe; preserving captured media"
            );
            return Ok(LiveHlsSnapshotOutcome::ContinuityLost);
        }
        Err(error) => return Err(error),
    };
    let resources = hls_resources_from(&missing_resources);
    let fetch_outcome = if resources.is_empty() {
        Ok(ResourceFetchOutcome::Complete)
    } else {
        fetch_resources_with_stop(
            client,
            resources,
            &track.directory,
            connections,
            tx,
            downloaded,
            abort,
        )
        .await
    };
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

    let new_resource_names = update.new_resource_names().collect::<HashSet<_>>();
    let newly_accounted_resources = update
        .resources
        .iter()
        .filter(|resource| new_resource_names.contains(resource.filename.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let added_resource_bytes =
        fetched_hls_resource_bytes(&track.directory, &newly_accounted_resources).await?;
    // The full staged set is verified on restore, periodically, and on ENDLIST.
    // The steady-state poll path checks only the resources referenced by this snapshot.
    let full_resource_verification_due = end_list
        || track.last_full_resource_verification.elapsed()
            >= LIVE_HLS_FULL_RESOURCE_VERIFY_INTERVAL;
    let verification = if full_resource_verification_due {
        verify_live_hls_resource_files(
            &track.directory,
            track
                .accumulator
                .resource_names()
                .chain(update.referenced_resource_names()),
            track.accumulator.segment_count() > 0,
        )
        .await
    } else {
        verify_live_hls_resource_files(
            &track.directory,
            update.referenced_resource_names(),
            track.accumulator.segment_count() > 0,
        )
        .await
    };
    if let Err(ManifestRunError::Unsupported(error)) = &verification
        && track.accumulator.segment_count() > 0
    {
        tracing::warn!(
            playlist = %diagnostic_url(&playlist_url),
            error = %error,
            "a staged live HLS resource became unavailable; preserving captured media"
        );
        return Ok(LiveHlsSnapshotOutcome::ContinuityLost);
    }
    verification?;
    if full_resource_verification_due {
        track.last_full_resource_verification = Instant::now();
    }
    if track.destination_part.is_some() {
        for resource in &update.resources {
            if resource.kind != HlsResourceKind::Segment || resource.range.is_some() {
                if track.accumulator.segment_count() > 0 {
                    return Ok(LiveHlsSnapshotOutcome::Stopped(
                        crate::video::StopIntent::Finish,
                    ));
                }
                return Err(ManifestRunError::Unsupported(
                    "A live HLS destination part requires complete, unranged media segments"
                        .to_string(),
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
    track.target_duration = plan.source_target_duration;
    track.part_target = plan.part_target;
    track.blocking_reload = plan.blocking_reload;
    track.can_skip_until = plan.can_skip_until;
    track.rendition_reports = plan.rendition_reports.clone();
    Ok(LiveHlsSnapshotOutcome::Committed)
}

fn reload_target_is_ahead(candidate: (u64, Option<u64>), current: (u64, Option<u64>)) -> bool {
    if candidate.0 != current.0 {
        return candidate.0 > current.0;
    }
    match (candidate.1, current.1) {
        (Some(candidate_part), Some(current_part)) => candidate_part > current_part,
        (Some(_), None) => true,
        (None, Some(_)) | (None, None) => false,
    }
}

/// Apply a selected rendition's report to its matching peer track.
///
/// Reports never cause Grab to fetch an arbitrary URI. The resolved report URI
/// must identify the already-selected peer playlist, and the reported next
/// position must be beyond the peer's last committed LL-HLS unit.
fn apply_rendition_report(source: &LiveHlsTrack, peer: &mut LiveHlsTrack) -> bool {
    if peer.ended || !peer.accumulator.is_ll_capture() {
        return false;
    }
    let peer_identity = crate::manifest::resource_cache_key(&peer.playlist_url);
    let Some(report) = source
        .rendition_reports
        .iter()
        .find(|report| crate::manifest::resource_cache_key(&report.playlist_url) == peer_identity)
    else {
        return false;
    };
    let Some(target) = report.next_reload() else {
        return false;
    };
    if peer
        .blocking_reload
        .is_some_and(|current| !reload_target_is_ahead(target, current))
    {
        return false;
    }

    let target_is_ahead = match peer.accumulator.last_ll_unit() {
        None => true,
        Some(crate::manifest::hls_ll::HlsLiveUnitId::Segment(sequence)) => target.0 > *sequence,
        Some(crate::manifest::hls_ll::HlsLiveUnitId::Part {
            media_sequence,
            part_index,
        }) => {
            target.0 > *media_sequence
                || (target.0 == *media_sequence
                    && target.1.is_some_and(|part| part > u64::from(*part_index)))
                || (target.0 == *media_sequence && target.1.is_none())
        }
    };
    if !target_is_ahead {
        return false;
    }

    if peer.blocking_reload.is_some() {
        peer.blocking_reload = Some(target);
    }
    peer.next_poll_at = Some(Instant::now());
    true
}

fn live_hls_track_can_use_destination_parts(track: &LiveHlsTrack) -> bool {
    // Eligibility is determined by the parsed playlist structure, not a
    // per-segment byte heuristic. FFmpeg validates the assembled recording.
    track.accumulator.plain_mpegts_resource_names().is_some()
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
    guarded_child_hosts: &mut crate::download_net::PublicResolutionSet,
    other_rendition_bytes: u64,
) -> Result<LiveHlsSnapshotOutcome, ManifestRunError> {
    let result = tokio::select! {
        biased;
        stop = &mut *abort => {
            return Ok(LiveHlsSnapshotOutcome::Stopped(
                stop.unwrap_or(crate::video::StopIntent::Discard),
            ));
        }
        result = crate::manifest_intake::lookup_hls_live_media(
            client,
            track.playlist_url.as_str(),
            track.blocking_reload,
            track.can_skip_until,
            &track.inherited_variables,
        ) => result,
    };
    let plan = match result {
        Ok(plan) => plan,
        Err(crate::manifest_intake::ManifestIntakeError::Retryable(error)) => {
            // One failed blocking reload is not retried in-place. Schedule the
            // next normal live poll and preserve already committed resources.
            let delay = track
                .part_target
                .unwrap_or_else(|| live_hls_poll_interval(track.target_duration, false))
                .min(MAX_LIVE_HLS_POLL_INTERVAL);
            track.snapshot_at = Instant::now();
            let Some(next_poll_at) = track.snapshot_at.checked_add(delay) else {
                return Ok(LiveHlsSnapshotOutcome::ContinuityLost);
            };
            track.next_poll_at = Some(next_poll_at);
            tracing::debug!(
                playlist = %diagnostic_url(&track.playlist_url),
                error = %error,
                delay_ms = delay.as_millis() as u64,
                "live HLS reload failed transiently; preserving capture and rescheduling the next poll"
            );
            return Ok(LiveHlsSnapshotOutcome::RetryScheduled);
        }
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
async fn refresh_live_hls_pathway(
    client: &reqwest::Client,
    source_url: &str,
    source: &ManifestSource,
    steering: &mut HlsContentSteering,
    video: &mut LiveHlsTrack,
    audio: &mut Option<LiveHlsTrack>,
    connections: usize,
    staging_dir: &Path,
    destination_dir: &Path,
    tx: &tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    downloaded: &Arc<AtomicU64>,
    guarded_child_hosts: &mut crate::download_net::PublicResolutionSet,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
) -> Result<Option<crate::video::StopIntent>, ManifestRunError> {
    let previous_pathway = steering.active_pathway_id.clone();
    if video.accumulator.is_ll_capture()
        || audio
            .as_ref()
            .is_some_and(|track| track.accumulator.is_ll_capture())
    {
        tracing::debug!("HLS Content Steering refresh deferred during LL-HLS capture");
        crate::manifest_intake::defer_hls_content_steering_refresh(steering);
        return Ok(None);
    }
    let refresh_result = tokio::select! {
        biased;
        stop = &mut *abort => return Ok(Some(
            stop.unwrap_or(crate::video::StopIntent::Discard)
        )),
        result = crate::manifest_intake::refresh_hls_content_steering_with_timeout(
            client,
            source_url,
            steering,
            LIVE_HLS_STEERING_REFRESH_TIMEOUT,
        ) => result,
    };
    let lookup = match refresh_result {
        Ok(Some(lookup)) => lookup,
        Ok(None) => return Ok(None),
        Err(error) => {
            tracing::debug!(
                error = %error,
                "HLS Content Steering refresh could not be applied; retaining current Pathway"
            );
            steering.active_pathway_id = previous_pathway;
            return Ok(None);
        }
    };

    let plan = match crate::manifest_plan::download_plan(lookup, source) {
        Ok(crate::manifest_plan::ManifestDownloadPlan::HlsMaster(plan)) => *plan,
        Ok(_) => {
            steering.active_pathway_id = previous_pathway;
            return Ok(None);
        }
        Err(error) => {
            tracing::debug!(
                error = %error,
                "HLS Content Steering refresh no longer matches the selected track"
            );
            steering.active_pathway_id = previous_pathway;
            return Ok(None);
        }
    };

    let video_changed = plan.video_url != video.requested_playlist_url;
    let audio_changed = match (audio.as_ref(), plan.audio_url.as_ref()) {
        (Some(track), Some(url)) => *url != track.requested_playlist_url,
        (None, None) => false,
        _ => {
            tracing::debug!(
                "HLS Content Steering refresh changed separate-audio topology; retaining current Pathway"
            );
            steering.active_pathway_id = previous_pathway;
            return Ok(None);
        }
    };
    if !video_changed && !audio_changed {
        return Ok(None);
    }
    if video_changed && audio_changed {
        tracing::debug!(
            "HLS Content Steering refresh changed both video and audio URLs; deferring a multi-track switch"
        );
        steering.active_pathway_id = previous_pathway;
        return Ok(None);
    }

    let mut next_video = if video_changed {
        if let Err(error) =
            guard_manifest_child_url(source_url, &plan.video_url, guarded_child_hosts)
        {
            tracing::debug!(
                error = %error,
                "refreshed HLS video playlist URL failed host validation; retaining current Pathway"
            );
            steering.active_pathway_id = previous_pathway;
            return Ok(None);
        }
        let lookup = tokio::select! {
            biased;
            stop = &mut *abort => return Ok(Some(
                stop.unwrap_or(crate::video::StopIntent::Discard)
            )),
            result = crate::manifest_intake::lookup_hls_media_with_variables(
                client,
                plan.video_url.as_str(),
                &plan.inherited_variables,
            ) => result,
        };
        let next = match lookup {
            Ok(next) => next,
            Err(error) => {
                tracing::debug!(
                    error = %error,
                    "refreshed HLS video playlist fetch failed; retaining current Pathway"
                );
                steering.active_pathway_id = previous_pathway;
                return Ok(None);
            }
        };
        if let Err(error) = guard_hls_resources(source_url, &next, guarded_child_hosts) {
            tracing::debug!(
                error = %error,
                "refreshed HLS video playlist resources failed validation; retaining current Pathway"
            );
            steering.active_pathway_id = previous_pathway;
            return Ok(None);
        }
        Some(next)
    } else {
        None
    };

    let mut next_audio = match (audio_changed, plan.audio_url.as_ref()) {
        (true, Some(url)) => {
            if let Err(error) = guard_manifest_child_url(source_url, url, guarded_child_hosts) {
                tracing::debug!(
                    error = %error,
                    "refreshed HLS audio playlist URL failed host validation; retaining current Pathway"
                );
                steering.active_pathway_id = previous_pathway;
                return Ok(None);
            }
            let lookup = tokio::select! {
                biased;
                stop = &mut *abort => return Ok(Some(
                    stop.unwrap_or(crate::video::StopIntent::Discard)
                )),
                result = crate::manifest_intake::lookup_hls_media_with_variables(
                    client,
                    url.as_str(),
                    &plan.inherited_variables,
                ) => result,
            };
            let next = match lookup {
                Ok(next) => next,
                Err(error) => {
                    tracing::debug!(
                        error = %error,
                        "refreshed HLS audio playlist fetch failed; retaining current Pathway"
                    );
                    steering.active_pathway_id = previous_pathway;
                    return Ok(None);
                }
            };
            if let Err(error) = guard_hls_resources(source_url, &next, guarded_child_hosts) {
                tracing::debug!(
                    error = %error,
                    "refreshed HLS audio playlist resources failed validation; retaining current Pathway"
                );
                steering.active_pathway_id = previous_pathway;
                return Ok(None);
            }
            Some(next)
        }
        _ => None,
    };

    if let Some(next) = next_video.as_mut() {
        let Some(next_sequence) = video.accumulator.next_sequence() else {
            steering.active_pathway_id = previous_pathway;
            return Ok(None);
        };
        if !video
            .accumulator
            .pathway_switch_preserves_discontinuity_sequence(next)
            || next.trim_live_snapshot_to_sequence(next_sequence).is_err()
            || video.accumulator.prepare_update(next).is_err()
        {
            tracing::debug!(
                playlist = %diagnostic_url(&next.playlist_url),
                "refreshed HLS video Pathway does not continue at the next media sequence"
            );
            steering.active_pathway_id = previous_pathway;
            return Ok(None);
        }
    }
    if let (Some(current), Some(next)) = (audio.as_ref(), next_audio.as_mut()) {
        let Some(next_sequence) = current.accumulator.next_sequence() else {
            steering.active_pathway_id = previous_pathway;
            return Ok(None);
        };
        if !current
            .accumulator
            .pathway_switch_preserves_discontinuity_sequence(next)
            || next.trim_live_snapshot_to_sequence(next_sequence).is_err()
            || current.accumulator.prepare_update(next).is_err()
        {
            tracing::debug!(
                playlist = %diagnostic_url(&next.playlist_url),
                "refreshed HLS audio Pathway does not continue at the next media sequence"
            );
            steering.active_pathway_id = previous_pathway;
            return Ok(None);
        }
    }

    if let Some(next) = next_video.as_ref() {
        match apply_live_hls_snapshot(
            client,
            next,
            video,
            connections,
            tx,
            downloaded,
            abort,
            audio
                .as_ref()
                .map_or(0, |track| track.accumulator.resource_bytes()),
        )
        .await
        {
            Ok(LiveHlsSnapshotOutcome::Committed) => {
                video.requested_playlist_url = next.requested_playlist_url.clone();
                ensure_manifest_mux_spaces(
                    staging_dir,
                    destination_dir,
                    downloaded.load(Ordering::Relaxed),
                )?;
            }
            Ok(LiveHlsSnapshotOutcome::Stopped(intent)) => return Ok(Some(intent)),
            Ok(LiveHlsSnapshotOutcome::RetryScheduled | LiveHlsSnapshotOutcome::ContinuityLost) => {
                tracing::debug!(
                    "refreshed HLS video Pathway could not be committed; retaining current route"
                );
                steering.active_pathway_id = previous_pathway;
                return Ok(None);
            }
            Err(error) => {
                tracing::debug!(
                    error = %error,
                    "refreshed HLS video Pathway request failed; retaining current route"
                );
                steering.active_pathway_id = previous_pathway;
                return Ok(None);
            }
        }
    }

    if let (Some(next), Some(current)) = (next_audio.as_ref(), audio.as_mut()) {
        match apply_live_hls_snapshot(
            client,
            next,
            current,
            connections,
            tx,
            downloaded,
            abort,
            video.accumulator.resource_bytes(),
        )
        .await
        {
            Ok(LiveHlsSnapshotOutcome::Committed) => {
                current.requested_playlist_url = next.requested_playlist_url.clone();
                ensure_manifest_mux_spaces(
                    staging_dir,
                    destination_dir,
                    downloaded.load(Ordering::Relaxed),
                )?;
            }
            Ok(LiveHlsSnapshotOutcome::Stopped(intent)) => return Ok(Some(intent)),
            Ok(LiveHlsSnapshotOutcome::RetryScheduled | LiveHlsSnapshotOutcome::ContinuityLost) => {
                tracing::warn!(
                    playlist = %diagnostic_url(&next.playlist_url),
                    "HLS audio Pathway refresh could not be committed; preserving the existing audio route"
                );
            }
            Err(error) => {
                tracing::warn!(
                    playlist = %diagnostic_url(&next.playlist_url),
                    error = %error,
                    "HLS audio Pathway refresh failed; preserving the existing audio route"
                );
            }
        }
    }

    if let Some(audio) = audio.as_mut() {
        apply_rendition_report(video, audio);
        apply_rendition_report(audio, video);
    }
    Ok(None)
}

#[expect(
    clippy::too_many_arguments,
    reason = "the runner coordinates the selected renditions, storage paths, progress, host policy, and shared stop signal"
)]
pub(super) async fn run_live_hls_tracks(
    client: &reqwest::Client,
    source_url: &str,
    source: &ManifestSource,
    mut content_steering: Option<&mut HlsContentSteering>,
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
    guarded_child_hosts: &mut crate::download_net::PublicResolutionSet,
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
        use_destination_parts = live_hls_track_can_use_destination_parts(&video);
        if use_destination_parts && let Some(track) = audio.as_ref() {
            use_destination_parts = live_hls_track_can_use_destination_parts(track);
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
        LiveHlsSnapshotOutcome::RetryScheduled => {}
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
            LiveHlsSnapshotOutcome::RetryScheduled => {}
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
    if let Some(track) = audio.as_mut() {
        apply_rendition_report(&video, track);
        apply_rendition_report(track, &mut video);
    }

    while !video.ended || audio.as_ref().is_some_and(|track| !track.ended) {
        let steering_refresh_at = content_steering
            .as_deref()
            .and_then(|steering| steering.refresh_at);
        let next_poll = [
            (!video.ended).then_some(video.next_poll_at).flatten(),
            audio
                .as_ref()
                .filter(|track| !track.ended)
                .and_then(|track| track.next_poll_at),
            steering_refresh_at,
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
        if let Some(steering) = content_steering.as_deref_mut()
            && steering.refresh_at.is_some_and(|deadline| deadline <= now)
            && let Some(intent) = refresh_live_hls_pathway(
                client,
                source_url,
                source,
                steering,
                &mut video,
                &mut audio,
                connections,
                staging_dir,
                destination_dir,
                tx,
                downloaded,
                guarded_child_hosts,
                abort,
            )
            .await?
        {
            return Ok(LiveHlsCapture::new(
                match intent {
                    crate::video::StopIntent::Pause => LiveHlsCaptureOutcome::Paused,
                    crate::video::StopIntent::Finish => LiveHlsCaptureOutcome::Finish,
                    crate::video::StopIntent::Discard => LiveHlsCaptureOutcome::Discard,
                },
                output,
            ));
        }
        if !video.ended
            && video
                .next_poll_at
                .is_some_and(|deadline| deadline <= Instant::now())
        {
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
                LiveHlsSnapshotOutcome::RetryScheduled => {}
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
            if let Some(audio_track) = audio.as_mut() {
                apply_rendition_report(&video, audio_track);
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
                LiveHlsSnapshotOutcome::RetryScheduled => {}
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
            apply_rendition_report(audio, &mut video);
        }
    }

    Ok(LiveHlsCapture::new(LiveHlsCaptureOutcome::Ended, output))
}

#[cfg(test)]
mod destination_part_tests {
    use super::*;

    const BASE_URL: &str = "https://cdn.example.test/live.m3u8";

    fn transport_stream(seed: u8) -> Vec<u8> {
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

    fn live_track_from_plan(plan: &HlsPlan, accumulator: HlsLiveAccumulator) -> LiveHlsTrack {
        LiveHlsTrack {
            playlist_url: plan.playlist_url.clone(),
            requested_playlist_url: plan.requested_playlist_url.clone(),
            inherited_variables: plan.inherited_variables.clone(),
            directory: PathBuf::new(),
            accumulator,
            snapshot_at: Instant::now(),
            last_full_resource_verification: Instant::now(),
            ended: false,
            next_poll_at: Some(Instant::now() + Duration::from_secs(30)),
            target_duration: plan.source_target_duration,
            part_target: plan.part_target,
            blocking_reload: plan.blocking_reload,
            can_skip_until: plan.can_skip_until,
            rendition_reports: plan.rendition_reports.clone(),
            destination_part: None,
        }
    }

    fn committed_part_accumulator(plan: &HlsPlan) -> HlsLiveAccumulator {
        let mut accumulator = HlsLiveAccumulator::new();
        let update = accumulator
            .prepare_update(plan)
            .expect("prepare peer LL-HLS snapshot");
        accumulator
            .commit_update_with_budget(update, 0, 0)
            .expect("commit peer LL-HLS media identities");
        accumulator
    }

    #[test]
    fn rendition_report_advances_only_the_matching_peer_track() {
        let source = crate::manifest::plan_hls_media_playlist(
            "https://cdn.example.test/live/video.m3u8",
            br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:30
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES
#EXTINF:1.0,
30.m4s
#EXT-X-RENDITION-REPORT:URI="audio.m3u8",LAST-MSN=31,LAST-PART=1
"#,
        )
        .expect("source playlist with a rendition report");
        let peer = crate::manifest::plan_hls_media_playlist(
            "https://cdn.example.test/live/audio.m3u8",
            br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:31
#EXT-X-MAP:URI="init.mp4"
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXT-X-PART:DURATION=0.5,URI="31.0.m4s",INDEPENDENT=YES
"#,
        )
        .expect("peer LL-HLS playlist");
        let mut source_track = live_track_from_plan(&source, HlsLiveAccumulator::new());
        let peer_accumulator = committed_part_accumulator(&peer);
        let mut peer_track = live_track_from_plan(&peer, peer_accumulator);
        let previous_deadline = peer_track.next_poll_at;
        let peer_identity = crate::manifest::resource_cache_key(&peer_track.playlist_url);

        assert!(apply_rendition_report(&source_track, &mut peer_track));
        assert_eq!(peer_track.blocking_reload, Some((31, Some(2))));
        assert!(
            peer_track.next_poll_at.expect("new peer poll deadline") <= Instant::now(),
            "a newly reported peer part should expedite the selected track"
        );
        assert_ne!(peer_track.next_poll_at, previous_deadline);
        assert_eq!(
            crate::manifest::resource_cache_key(&peer_track.playlist_url),
            peer_identity,
            "a rendition report must not change the selected peer URL"
        );

        // A report for an unrelated child playlist cannot retarget the selected track.
        source_track.rendition_reports = source_track
            .rendition_reports
            .iter()
            .cloned()
            .map(|mut report| {
                report.playlist_url = Url::parse("https://cdn.example.test/live/other.m3u8")
                    .expect("unrelated report URL");
                report
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        peer_track.next_poll_at = Some(Instant::now() + Duration::from_secs(30));
        peer_track.blocking_reload = Some((31, Some(1)));
        assert!(!apply_rendition_report(&source_track, &mut peer_track));
        assert_eq!(peer_track.blocking_reload, Some((31, Some(1))));
    }

    #[test]
    fn older_rendition_report_cannot_rewind_a_later_pending_reload() {
        let source = crate::manifest::plan_hls_media_playlist(
            "https://cdn.example.test/live/video.m3u8",
            br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:30
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES
#EXTINF:1.0,
30.m4s
#EXT-X-RENDITION-REPORT:URI="audio.m3u8",LAST-MSN=31,LAST-PART=2
"#,
        )
        .expect("source playlist with a behind report");
        let peer = crate::manifest::plan_hls_media_playlist(
            "https://cdn.example.test/live/audio.m3u8",
            br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:31
#EXT-X-MAP:URI="init.mp4"
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXT-X-PART:DURATION=0.5,URI="31.0.m4s",INDEPENDENT=YES
#EXT-X-PART:DURATION=0.5,URI="31.1.m4s"
"#,
        )
        .expect("peer LL-HLS playlist");
        let source_track = live_track_from_plan(&source, HlsLiveAccumulator::new());
        let mut peer_track = live_track_from_plan(&peer, committed_part_accumulator(&peer));
        peer_track.blocking_reload = Some((31, Some(5)));
        let deadline = peer_track.next_poll_at;

        assert!(!apply_rendition_report(&source_track, &mut peer_track));
        assert_eq!(peer_track.blocking_reload, Some((31, Some(5))));
        assert_eq!(peer_track.next_poll_at, deadline);
    }

    #[test]
    fn stale_rendition_report_does_not_rewind_or_repoll_a_peer() {
        let source = crate::manifest::plan_hls_media_playlist(
            "https://cdn.example.test/live/video.m3u8",
            br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:30
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES
#EXTINF:1.0,
30.m4s
#EXT-X-RENDITION-REPORT:URI="audio.m3u8",LAST-MSN=31,LAST-PART=0
"#,
        )
        .expect("source playlist with stale rendition report");
        let peer = crate::manifest::plan_hls_media_playlist(
            "https://cdn.example.test/live/audio.m3u8",
            br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:31
#EXT-X-MAP:URI="init.mp4"
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXT-X-PART:DURATION=0.5,URI="31.0.m4s",INDEPENDENT=YES
#EXT-X-PART:DURATION=0.5,URI="31.1.m4s"
"#,
        )
        .expect("peer playlist already ahead of report");
        let source_track = live_track_from_plan(&source, HlsLiveAccumulator::new());
        let mut peer_track = live_track_from_plan(&peer, committed_part_accumulator(&peer));
        let deadline = peer_track.next_poll_at;
        let blocking_reload = peer_track.blocking_reload;

        assert!(!apply_rendition_report(&source_track, &mut peer_track));
        assert_eq!(peer_track.next_poll_at, deadline);
        assert_eq!(peer_track.blocking_reload, blocking_reload);
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
            requested_playlist_url: first_plan.requested_playlist_url.clone(),
            inherited_variables: first_plan.inherited_variables.clone(),
            directory: video_dir.clone(),
            accumulator,
            snapshot_at: Instant::now(),
            last_full_resource_verification: Instant::now(),
            ended: false,
            next_poll_at: None,
            target_duration: first_plan.source_target_duration,
            part_target: first_plan.part_target,
            blocking_reload: first_plan.blocking_reload,
            can_skip_until: first_plan.can_skip_until,
            rendition_reports: first_plan.rendition_reports.clone(),
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
