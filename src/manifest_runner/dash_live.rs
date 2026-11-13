//! Dynamic DASH live-capture staging, polling, and recovery.

use super::*;

pub(super) async fn dash_live_state_exists(directory: &Path) -> Result<bool, ManifestRunError> {
    let path = directory.join(DASH_LIVE_STATE_NAME);
    match tokio::fs::symlink_metadata(&path).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(ManifestRunError::Failed(
                "The staged dynamic DASH state is not a regular file".to_string(),
            ))
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(ManifestRunError::Failed(format!(
            "Could not inspect dynamic DASH staging state: {error}"
        ))),
    }
}

pub(super) async fn read_dash_live_representation_key(
    directory: &Path,
) -> Result<String, ManifestRunError> {
    let path = directory.join(DASH_LIVE_STATE_NAME);
    let metadata = tokio::fs::symlink_metadata(&path).await.map_err(|error| {
        ManifestRunError::Failed(format!(
            "Could not inspect saved dynamic DASH audio state: {error}"
        ))
    })?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_DASH_LIVE_STATE_BYTES as u64
    {
        return Err(ManifestRunError::Failed(
            "The saved dynamic DASH audio state is not a valid regular file".to_string(),
        ));
    }
    let bytes = tokio::fs::read(&path).await.map_err(|error| {
        ManifestRunError::Failed(format!(
            "Could not read saved dynamic DASH audio state: {error}"
        ))
    })?;
    let state: DashLiveState = serde_json::from_slice(&bytes).map_err(|error| {
        ManifestRunError::Failed(format!(
            "Could not parse saved dynamic DASH audio state: {error}"
        ))
    })?;
    validate_dash_live_state(&state, state.representation_key.as_str())?;
    Ok(state.representation_key.as_str().to_string())
}

#[cfg(test)]
pub(super) async fn load_dash_live_track(
    representation_key: &str,
    directory: &Path,
    current_plan: Option<&DashRepresentationPlan>,
    track_filename: &'static str,
) -> Result<(DashLiveTrack, u64), ManifestRunError> {
    Box::pin(load_dash_live_track_inner(
        representation_key,
        directory,
        current_plan,
        track_filename,
        None,
    ))
    .await
}

async fn load_dash_live_track_inner(
    representation_key: &str,
    directory: &Path,
    current_plan: Option<&DashRepresentationPlan>,
    track_filename: &'static str,
    destination_part: Option<DashLiveDestination>,
) -> Result<(DashLiveTrack, u64), ManifestRunError> {
    if current_plan.is_some_and(|plan| !plan.representation.matches_key(representation_key)) {
        return Err(ManifestRunError::Failed(
            "The refreshed DASH representation no longer matches the queued selection".to_string(),
        ));
    }
    let path = directory.join(DASH_LIVE_STATE_NAME);
    let mut state = match tokio::fs::symlink_metadata(&path).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(ManifestRunError::Failed(
                "The staged dynamic DASH state is not a regular file".to_string(),
            ));
        }
        Ok(metadata) if metadata.len() > MAX_DASH_LIVE_STATE_BYTES as u64 => {
            return Err(ManifestRunError::Failed(
                "The staged dynamic DASH state exceeds its size limit".to_string(),
            ));
        }
        Ok(_) => {
            let bytes = tokio::fs::read(&path).await.map_err(|error| {
                ManifestRunError::Failed(format!(
                    "Could not read dynamic DASH staging state: {error}"
                ))
            })?;
            serde_json::from_slice::<DashLiveState>(&bytes).map_err(|error| {
                ManifestRunError::Failed(format!(
                    "Could not restore dynamic DASH staging state: {error}"
                ))
            })?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if current_plan.is_none() {
                return Err(ManifestRunError::Failed(
                    "No committed dynamic DASH recording is available".to_string(),
                ));
            }
            DashLiveState::new(representation_key)
        }
        Err(error) => {
            return Err(ManifestRunError::Failed(format!(
                "Could not inspect dynamic DASH staging state: {error}"
            )));
        }
    };
    let previous_representation_key = state.representation_key.clone();
    let expected_representation_key = current_plan
        .map(|plan| ManifestRepresentationKey::from(plan.representation.live_key.as_str()))
        .unwrap_or_else(|| state.representation_key.clone());
    if current_plan.is_some()
        && state.representation_key.as_str() != representation_key
        && state.representation_key != expected_representation_key
    {
        return Err(ManifestRunError::Failed(
            "The saved dynamic DASH state belongs to a different representation".to_string(),
        ));
    }
    state
        .representation_key
        .clone_from(&expected_representation_key);
    validate_dash_live_state(&state, expected_representation_key.as_str())?;

    let resource_names = state
        .resource_names()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let committed_resources = if let Some(destination_part) = destination_part.as_ref() {
        let staging = directory.parent().unwrap_or(directory);
        let part = crate::manifest_destination::part_path_async(
            staging.to_path_buf(),
            destination_part.destination.clone(),
            destination_part.item_id,
            destination_part.track,
        )
        .await
        .map_err(ManifestRunError::Failed)?;
        match tokio::fs::symlink_metadata(&part).await {
            Ok(_) => {
                crate::manifest_destination::verify_owned_part_async(
                    staging.to_path_buf(),
                    destination_part.destination.clone(),
                    destination_part.item_id,
                    destination_part.track,
                )
                .await
                .map_err(ManifestRunError::Failed)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(ManifestRunError::Failed(format!(
                    "Could not inspect the live DASH destination part: {error}"
                )));
            }
        }
        crate::manifest_writer::committed_resource_prefix_at(
            &resource_names,
            directory,
            track_filename,
            &part,
        )
        .await
        .map_err(ManifestRunError::Failed)?
    } else {
        crate::manifest_writer::committed_resource_prefix(
            &resource_names,
            directory,
            track_filename,
        )
        .await
        .map_err(ManifestRunError::Failed)?
    };
    let committed_resource_count = committed_resources.len();
    let committed_bytes = committed_resources
        .iter()
        .try_fold(0_u64, |total, resource| {
            if resource.length == 0 || resource.length > MAX_RESOURCE_BYTES {
                return Err(ManifestRunError::Failed(
                    "A committed dynamic DASH resource has an invalid size".to_string(),
                ));
            }
            total.checked_add(resource.length).ok_or_else(|| {
                ManifestRunError::Failed(
                    "The committed dynamic DASH byte total overflowed".to_string(),
                )
            })
        })?;
    let current_resources: std::collections::HashMap<&str, &DashResource> = current_plan
        .into_iter()
        .flat_map(|plan| plan.resources.iter())
        .map(|resource| (resource.filename.as_str(), resource))
        .collect();
    let mut retained_bytes = 0_u64;
    for (index, filename) in state.resource_names().enumerate() {
        let expected = current_resources.get(filename).copied();
        if let Some(committed) = committed_resources.get(index) {
            if committed.length == 0
                || committed.length > MAX_RESOURCE_BYTES
                || expected.is_some_and(|resource| {
                    resource
                        .range
                        .is_some_and(|range| range.length != committed.length)
                })
            {
                return Err(ManifestRunError::Failed(
                    "A committed dynamic DASH resource has an invalid length".to_string(),
                ));
            }
            retained_bytes = retained_bytes
                .checked_add(committed.length)
                .ok_or_else(|| {
                    ManifestRunError::Failed(
                        "The staged dynamic DASH byte total overflowed".to_string(),
                    )
                })?;
            continue;
        }

        let resource_path = directory.join(filename);
        let metadata = match tokio::fs::symlink_metadata(&resource_path).await {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(ManifestRunError::Failed(
                    "A staged dynamic DASH resource is not a regular file".to_string(),
                ));
            }
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(ManifestRunError::Failed(format!(
                    "Could not inspect {}: {error}",
                    resource_path.display()
                )));
            }
        };
        if let Some(metadata) = metadata {
            let cached_is_valid = metadata.len() > 0
                && metadata.len() <= MAX_RESOURCE_BYTES
                && expected.is_none_or(|resource| {
                    resource
                        .range
                        .is_none_or(|range| metadata.len() == range.length)
                });
            if cached_is_valid {
                retained_bytes = retained_bytes.checked_add(metadata.len()).ok_or_else(|| {
                    ManifestRunError::Failed(
                        "The staged dynamic DASH byte total overflowed".to_string(),
                    )
                })?;
            } else if expected.is_none() {
                return Err(ManifestRunError::Failed(
                    "A captured dynamic DASH resource is missing or invalid and no longer appears in the current MPD"
                        .to_string(),
                ));
            }
        } else if expected.is_none() {
            return Err(ManifestRunError::Failed(
                "A captured dynamic DASH resource is missing and no longer appears in the current MPD"
                    .to_string(),
            ));
        }
    }
    if retained_bytes > MAX_MANIFEST_DOWNLOAD_BYTES {
        return Err(ManifestRunError::Failed(
            "The staged dynamic DASH recording exceeds Grab's 1 TiB limit".to_string(),
        ));
    }

    let track = DashLiveTrack {
        directory: directory.to_path_buf(),
        state,
        track_filename,
        committed_resource_count,
        committed_bytes,
        destination_part,
    };
    if track.state.representation_key != previous_representation_key {
        write_dash_live_state(&track).await?;
    }

    Ok((track, retained_bytes))
}

/// Load a live DASH rendition while binding its output to the owned destination part.
pub(super) async fn load_dash_live_track_for_destination(
    representation_key: &str,
    directory: &Path,
    current_plan: Option<&DashRepresentationPlan>,
    track_filename: &'static str,
    destination: &Path,
    item_id: u64,
    track: crate::manifest_destination::TrackKind,
) -> Result<(DashLiveTrack, u64), ManifestRunError> {
    Box::pin(load_dash_live_track_inner(
        representation_key,
        directory,
        current_plan,
        track_filename,
        Some(DashLiveDestination {
            destination: destination.to_path_buf(),
            item_id,
            track,
        }),
    ))
    .await
}

async fn append_live_dash_destination_part(
    track: &mut DashLiveTrack,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
) -> Result<Option<crate::video::StopIntent>, ManifestRunError> {
    let Some(destination_part) = track.destination_part.as_ref() else {
        return Ok(None);
    };
    let resources = track
        .state
        .resource_names()
        .map(str::to_string)
        .collect::<Vec<_>>();
    if resources.is_empty() {
        return Ok(None);
    }
    let staging = track.directory.parent().unwrap_or(&track.directory);
    let resource_count = resources.len();
    let (Some(mut writer), _) =
        crate::manifest_writer::OrderedTrackWriter::open_appendable_destination_part(
            resources,
            &track.directory,
            staging,
            track.track_filename,
            &destination_part.destination,
            destination_part.item_id,
            destination_part.track,
        )
        .await
        .map_err(ManifestRunError::Failed)?
    else {
        return Ok(None);
    };
    while writer.next_resource_index() < resource_count {
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
    track.committed_resource_count = writer.next_resource_index();
    track.committed_bytes = writer.committed_offset();
    writer.finish().await.map_err(ManifestRunError::Failed)?;
    Ok(None)
}

/// Ensure a previously finished live DASH capture has owned destination-side track parts.
pub(super) async fn ensure_live_dash_destination_parts(
    representation_key: &str,
    audio_key: Option<&str>,
    video_dir: &Path,
    audio_dir: &Path,
    destination: &Path,
    item_id: u64,
    include_audio: bool,
) -> Result<(), ManifestRunError> {
    let (mut video, _) = load_dash_live_track_for_destination(
        representation_key,
        video_dir,
        None,
        "video-track.mp4",
        destination,
        item_id,
        crate::manifest_destination::TrackKind::Video,
    )
    .await?;
    let (_keep_alive, mut never_abort) = tokio::sync::oneshot::channel();
    if append_live_dash_destination_part(&mut video, &mut never_abort)
        .await?
        .is_some()
    {
        return Err(ManifestRunError::Failed(
            "The completed live DASH video track was unexpectedly interrupted".to_string(),
        ));
    }

    if include_audio && dash_live_state_exists(audio_dir).await? {
        let key = match audio_key {
            Some(key) => key.to_string(),
            None => read_dash_live_representation_key(audio_dir).await?,
        };
        let (mut audio, _) = load_dash_live_track_for_destination(
            &key,
            audio_dir,
            None,
            "audio-track.mp4",
            destination,
            item_id,
            crate::manifest_destination::TrackKind::Audio,
        )
        .await?;
        if append_live_dash_destination_part(&mut audio, &mut never_abort)
            .await?
            .is_some()
        {
            return Err(ManifestRunError::Failed(
                "The completed live DASH audio track was unexpectedly interrupted".to_string(),
            ));
        }
    }
    Ok(())
}

pub(super) fn validate_dash_live_state(
    state: &DashLiveState,
    representation_key: &str,
) -> Result<(), ManifestRunError> {
    if state.version != DASH_LIVE_STATE_VERSION
        || state.representation_key.as_str() != representation_key
        || !representation_key
            .strip_prefix("dash-live-")
            .is_some_and(|fingerprint| {
                fingerprint.len() == 32 && fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        || state.segments.len() > MAX_DASH_LIVE_SEGMENTS
    {
        return Err(ManifestRunError::Failed(
            "The saved dynamic DASH state is incompatible with this queue item".to_string(),
        ));
    }
    if state.initialization.as_deref().is_some_and(|name| {
        !safe_resource_filename(name)
            || !name.starts_with("dash-live-init-")
            || !name.ends_with(".bin")
    }) {
        return Err(ManifestRunError::Failed(
            "The saved dynamic DASH initialization name is invalid".to_string(),
        ));
    }
    let mut seen_names = std::collections::HashSet::new();
    if let Some(initialization) = state.initialization.as_deref() {
        seen_names.insert(initialization);
    }
    let mut previous_number = None;
    for segment in &state.segments {
        if !safe_resource_filename(&segment.filename)
            || !segment.filename.starts_with("dash-live-segment-")
            || !segment.filename.ends_with(".bin")
            || !seen_names.insert(segment.filename.as_str())
            || previous_number
                .is_some_and(|previous: u64| previous.checked_add(1) != Some(segment.number))
        {
            return Err(ManifestRunError::Failed(
                "The saved dynamic DASH segment index is invalid".to_string(),
            ));
        }
        previous_number = Some(segment.number);
    }
    Ok(())
}

pub(super) async fn write_dash_live_state(track: &DashLiveTrack) -> Result<(), ManifestRunError> {
    validate_dash_live_state(&track.state, track.state.representation_key.as_str())?;
    let bytes = serde_json::to_vec(&track.state)
        .map_err(|error| ManifestRunError::Failed(error.to_string()))?;
    if bytes.len() > MAX_DASH_LIVE_STATE_BYTES {
        return Err(ManifestRunError::Failed(
            "The dynamic DASH state exceeds its serialization limit".to_string(),
        ));
    }

    let target = track.directory.join(DASH_LIVE_STATE_NAME);
    match tokio::fs::symlink_metadata(&target).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(ManifestRunError::Failed(
                "The staged dynamic DASH state is not a regular file".to_string(),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(ManifestRunError::Failed(format!(
                "Could not inspect dynamic DASH staging state: {error}"
            )));
        }
    }

    let part = track
        .directory
        .join(format!("{DASH_LIVE_STATE_NAME}.next.part"));
    remove_regular_file_if_present(&part)
        .await
        .map_err(ManifestRunError::Failed)?;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part)
        .await
        .map_err(|error| {
            ManifestRunError::Failed(format!("Could not create dynamic DASH state: {error}"))
        })?;
    if let Err(error) = file.write_all(&bytes).await {
        drop(file);
        let _ = remove_regular_file_if_present(&part).await;
        return Err(ManifestRunError::Failed(format!(
            "Could not write dynamic DASH state: {error}"
        )));
    }
    if let Err(error) = file.sync_all().await {
        drop(file);
        let _ = remove_regular_file_if_present(&part).await;
        return Err(ManifestRunError::Failed(format!(
            "Could not flush dynamic DASH state: {error}"
        )));
    }
    drop(file);
    tokio::fs::rename(&part, &target).await.map_err(|error| {
        ManifestRunError::Failed(format!("Could not publish dynamic DASH state: {error}"))
    })?;
    sync_staging_directory(&track.directory)
        .await
        .map_err(ManifestRunError::Failed)
}

// The committed prefix is cached by DashLiveTrack after one full integrity
// check. Refreshes preserve that prefix and add suffix resources, so rehashing
// it per poll would turn long recordings into repeated full-track reads.
pub(super) async fn dash_live_track_bytes(
    state: &DashLiveState,
    directory: &Path,
    committed_resource_count: usize,
    committed_bytes: u64,
) -> Result<u64, ManifestRunError> {
    if committed_resource_count > state.resource_names().count() {
        return Err(ManifestRunError::Failed(
            "The committed dynamic DASH prefix exceeds its resource state".to_string(),
        ));
    }
    let mut total = committed_bytes;
    for filename in state.resource_names().skip(committed_resource_count) {
        let metadata = tokio::fs::symlink_metadata(directory.join(filename))
            .await
            .map_err(|error| {
                ManifestRunError::Failed(format!(
                    "Could not inspect a committed dynamic DASH resource: {error}"
                ))
            })?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() == 0
            || metadata.len() > MAX_RESOURCE_BYTES
        {
            return Err(ManifestRunError::Failed(
                "A committed dynamic DASH resource has an invalid file type or size".to_string(),
            ));
        }
        total = total.checked_add(metadata.len()).ok_or_else(|| {
            ManifestRunError::Failed("The dynamic DASH byte total overflowed".to_string())
        })?;
    }
    Ok(total)
}

fn dash_presentation_ended(presentation: &DashPresentation) -> bool {
    match presentation {
        DashPresentation::Static => true,
        DashPresentation::Dynamic(dynamic) => dynamic
            .availability_end_time
            .is_some_and(|end| dynamic.server_now >= end),
    }
}

fn dash_next_refresh_deadline(
    presentation: &DashPresentation,
) -> Result<Option<Instant>, ManifestRunError> {
    match presentation {
        DashPresentation::Static => Ok(None),
        DashPresentation::Dynamic(dynamic) => Instant::now()
            .checked_add(dynamic.minimum_update_period)
            .map(Some)
            .ok_or_else(|| {
                ManifestRunError::Failed(
                    "The dynamic DASH update interval is outside the supported clock range"
                        .to_string(),
                )
            }),
    }
}

async fn dash_live_resource_needs_fetch(
    directory: &Path,
    resource: &DashResource,
) -> Result<bool, ManifestRunError> {
    let path = directory.join(&resource.filename);
    match tokio::fs::symlink_metadata(&path).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(ManifestRunError::Failed(
                "A dynamic DASH resource path is not a regular file".to_string(),
            ))
        }
        Ok(metadata) => Ok(metadata.len() == 0
            || metadata.len() > MAX_RESOURCE_BYTES
            || resource
                .range
                .is_some_and(|range| metadata.len() != range.length)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(ManifestRunError::Failed(format!(
            "Could not inspect a dynamic DASH resource: {error}"
        ))),
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "one rendition update shares its client, cancellation, progress and aggregate byte-budget context"
)]
pub(super) async fn apply_dash_live_snapshot(
    client: &reqwest::Client,
    plan: &DashRepresentationPlan,
    track: &mut DashLiveTrack,
    connections: usize,
    tx: &tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    downloaded: &Arc<AtomicU64>,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
    other_rendition_bytes: u64,
) -> Result<DashLiveUpdateOutcome, ManifestRunError> {
    if plan.representation.live_key != track.state.representation_key.as_str() {
        return Ok(DashLiveUpdateOutcome::ContinuityLost);
    }

    let mut candidate = track.state.clone();
    let committed_resource_names: std::collections::HashSet<&str> = track
        .state
        .resource_names()
        .take(track.committed_resource_count)
        .collect();
    let mut resources_to_fetch = Vec::new();
    let mut filenames_to_fetch = std::collections::HashSet::new();
    let initialization = plan
        .resources
        .iter()
        .find(|resource| resource.kind == DashResourceKind::Initialization);
    if let Some(initialization) = initialization {
        match candidate.initialization.as_deref() {
            Some(previous) if previous != initialization.filename => {
                if !candidate.segments.is_empty() || track.committed_resource_count > 0 {
                    tracing::warn!(
                        representation = %plan.representation.live_key,
                        "dynamic DASH initialization changed after media was captured"
                    );
                    return Ok(DashLiveUpdateOutcome::ContinuityLost);
                }
                candidate.initialization = Some(initialization.filename.clone());
            }
            None if !candidate.segments.is_empty() || track.committed_resource_count > 0 => {
                tracing::warn!(
                    representation = %plan.representation.live_key,
                    "dynamic DASH introduced an initialization resource after media was captured"
                );
                return Ok(DashLiveUpdateOutcome::ContinuityLost);
            }
            None => {
                candidate.initialization = Some(initialization.filename.clone());
            }
            Some(_) => {}
        }
        let initialization_is_committed =
            committed_resource_names.contains(initialization.filename.as_str());
        if !initialization_is_committed
            && dash_live_resource_needs_fetch(&track.directory, initialization).await?
            && filenames_to_fetch.insert(initialization.filename.clone())
        {
            resources_to_fetch.push(initialization.clone());
        }
    }

    let mut previous_remote_number = None;
    for resource in plan
        .resources
        .iter()
        .filter(|resource| resource.kind == DashResourceKind::Segment)
    {
        let Some(number) = resource.segment_number else {
            return Err(ManifestRunError::Unsupported(
                "A dynamic DASH segment has no stable sequence number".to_string(),
            ));
        };
        if previous_remote_number.is_some_and(|previous| number <= previous) {
            return Err(ManifestRunError::Unsupported(
                "The dynamic DASH MPD contains unordered segment numbers".to_string(),
            ));
        }
        previous_remote_number = Some(number);

        let first_number = candidate.segments.first().map(|segment| segment.number);
        if first_number.is_some_and(|first| number < first) {
            continue;
        }
        if let Some(existing) = candidate
            .segments
            .iter()
            .find(|segment| segment.number == number)
        {
            if existing.filename != resource.filename {
                tracing::warn!(
                    representation = %plan.representation.live_key,
                    segment_number = number,
                    "dynamic DASH changed the URL of an already captured segment"
                );
                return Ok(DashLiveUpdateOutcome::ContinuityLost);
            }
        } else {
            match candidate.next_number() {
                None => {}
                Some(next) if number == next => {}
                Some(next) if number > next => {
                    tracing::warn!(
                        representation = %plan.representation.live_key,
                        expected_segment = next,
                        received_segment = number,
                        "dynamic DASH MPD skipped a required segment sequence"
                    );
                    return Ok(DashLiveUpdateOutcome::ContinuityLost);
                }
                Some(_) => {
                    return Ok(DashLiveUpdateOutcome::ContinuityLost);
                }
            }
            if candidate.segments.len() >= MAX_DASH_LIVE_SEGMENTS {
                tracing::warn!(
                    representation = %plan.representation.live_key,
                    "dynamic DASH recording reached its segment limit"
                );
                return Ok(DashLiveUpdateOutcome::ContinuityLost);
            }
            candidate.segments.push(DashLiveSegment {
                number,
                filename: resource.filename.clone(),
            });
        }

        let resource_is_committed = committed_resource_names.contains(resource.filename.as_str());
        if !resource_is_committed
            && dash_live_resource_needs_fetch(&track.directory, resource).await?
            && filenames_to_fetch.insert(resource.filename.clone())
        {
            resources_to_fetch.push(resource.clone());
        }
    }

    if !resources_to_fetch.is_empty() {
        if plan.low_latency {
            // Fetch media in presentation order so a progressive response can never
            // block an older retained segment that is still inside the time-shift window.
            resources_to_fetch.sort_by_key(|resource| {
                (
                    resource.kind == DashResourceKind::Segment,
                    resource.segment_number.unwrap_or_default(),
                )
            });
        }
        match fetch_resources_with_stop(
            client,
            dash_resources_from(&resources_to_fetch),
            &track.directory,
            if plan.low_latency { 1 } else { connections },
            tx,
            downloaded,
            abort,
        )
        .await
        {
            Ok(ResourceFetchOutcome::Complete) => {}
            Ok(ResourceFetchOutcome::Stopped(intent)) => {
                return Ok(DashLiveUpdateOutcome::Stopped(intent));
            }
            // A permanent failure for a newly advertised live segment is a
            // continuity boundary, not a reason to hand the recording to
            // yt-dlp. The caller preserves any already committed video.
            Err(ManifestRunError::Unsupported(error))
                if track.committed_resource_count > 0 || other_rendition_bytes > 0 =>
            {
                tracing::warn!(
                    representation = %plan.representation.live_key,
                    error = %error,
                    "dynamic DASH resource became unavailable; preserving committed capture"
                );
                return Ok(DashLiveUpdateOutcome::ContinuityLost);
            }
            Err(error) => return Err(error),
        }
    }

    if candidate.segments.len() > MAX_DASH_LIVE_SEGMENTS {
        return Ok(DashLiveUpdateOutcome::ContinuityLost);
    }
    let retained_bytes = dash_live_track_bytes(
        &candidate,
        &track.directory,
        track.committed_resource_count,
        track.committed_bytes,
    )
    .await?;
    if retained_bytes
        .checked_add(other_rendition_bytes)
        .is_none_or(|total| total > MAX_MANIFEST_DOWNLOAD_BYTES)
    {
        tracing::warn!(
            representation = %plan.representation.live_key,
            "dynamic DASH recording reached its aggregate byte limit"
        );
        return Ok(DashLiveUpdateOutcome::ContinuityLost);
    }

    if candidate.segments != track.state.segments
        || candidate.initialization != track.state.initialization
    {
        let next_track = DashLiveTrack {
            directory: track.directory.clone(),
            state: candidate,
            track_filename: track.track_filename,
            committed_resource_count: track.committed_resource_count,
            committed_bytes: track.committed_bytes,
            destination_part: track.destination_part.clone(),
        };
        write_dash_live_state(&next_track).await?;
        track.state = next_track.state;
    }
    if let Some(intent) = append_live_dash_destination_part(track, abort).await? {
        return Ok(DashLiveUpdateOutcome::Stopped(intent));
    }
    downloaded.store(
        retained_bytes.saturating_add(other_rendition_bytes),
        Ordering::Relaxed,
    );
    Ok(DashLiveUpdateOutcome::Committed)
}

#[expect(
    clippy::too_many_arguments,
    reason = "the live poll loop owns the shared DASH manifest selection, rendition states, progress and cancellation"
)]
pub(super) async fn run_live_dash_tracks(
    client: &reqwest::Client,
    proxy_configured: bool,
    source_url: &str,
    destination_dir: &Path,
    destination: &Path,
    item_id: u64,
    representation_key: &str,
    audio_key: Option<&str>,
    video_plan: Option<&DashRepresentationPlan>,
    audio_plan: Option<&DashRepresentationPlan>,
    video_dir: &Path,
    audio_dir: &Path,
    connections: usize,
    tx: &tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    downloaded: &Arc<AtomicU64>,
    guarded_child_hosts: &mut crate::download_net::PublicResolutionSet,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
) -> Result<DashLiveCapture, ManifestRunError> {
    let (mut video, video_bytes) = load_dash_live_track_for_destination(
        representation_key,
        video_dir,
        video_plan,
        "video-track.mp4",
        destination,
        item_id,
        crate::manifest_destination::TrackKind::Video,
    )
    .await?;
    // Upgrade policy-sensitive queue keys to the stable identity used by MPD refreshes.
    let selected_video_key = video.state.representation_key.clone();
    let staging_dir = video_dir.parent().unwrap_or(video_dir);
    let mut audio_key = audio_key
        .map(str::to_string)
        .or_else(|| audio_plan.map(|plan| plan.representation.live_key.clone()));
    let mut audio = match audio_plan {
        Some(plan) => {
            let key = audio_key
                .clone()
                .unwrap_or_else(|| plan.representation.live_key.clone());
            audio_key = Some(key.clone());
            Some(
                load_dash_live_track_for_destination(
                    &key,
                    audio_dir,
                    Some(plan),
                    "audio-track.mp4",
                    destination,
                    item_id,
                    crate::manifest_destination::TrackKind::Audio,
                )
                .await?,
            )
        }
        None if dash_live_state_exists(audio_dir).await? => {
            let key = match audio_key.as_deref() {
                Some(key) => key.to_string(),
                None => read_dash_live_representation_key(audio_dir).await?,
            };
            audio_key = Some(key.clone());
            Some(
                load_dash_live_track_for_destination(
                    &key,
                    audio_dir,
                    None,
                    "audio-track.mp4",
                    destination,
                    item_id,
                    crate::manifest_destination::TrackKind::Audio,
                )
                .await?,
            )
        }
        None => None,
    };
    if let Some((track, _)) = audio.as_ref() {
        audio_key = Some(track.state.representation_key.as_str().to_string());
    }
    macro_rules! finish_capture {
        ($outcome:expr) => {{
            let outcome = discard_unrecoverable_dash_capture_without_video(
                $outcome,
                !video.state.segments.is_empty(),
            );
            if matches!(outcome, DashLiveCaptureOutcome::Unrecoverable) {
                let staged_video_bytes = dash_live_track_bytes(
                    &video.state,
                    &video.directory,
                    video.committed_resource_count,
                    video.committed_bytes,
                )
                .await?;
                persist_unrecoverable_live_capture(
                    staging_dir,
                    ManifestKind::Dash,
                    "DASH presentation continuity could not be verified",
                    staged_video_bytes,
                    downloaded,
                    tx,
                )
                .await?;
            }
            let video_resources = video
                .state
                .resource_names()
                .map(str::to_string)
                .collect::<Vec<_>>();
            let audio_resources = if matches!(
                outcome,
                DashLiveCaptureOutcome::FinishVideoOnly | DashLiveCaptureOutcome::Unrecoverable
            ) {
                None
            } else {
                audio.as_ref().map(|(track, _)| {
                    track
                        .state
                        .resource_names()
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
            };
            return Ok(DashLiveCapture {
                outcome,
                video_resources,
                audio_resources,
            });
        }};
    }

    if let Some(intent) = append_live_dash_destination_part(&mut video, abort).await? {
        finish_capture!(match intent {
            crate::video::StopIntent::Pause => DashLiveCaptureOutcome::Paused,
            crate::video::StopIntent::Finish => DashLiveCaptureOutcome::Finish,
            crate::video::StopIntent::Discard => DashLiveCaptureOutcome::Discard,
        });
    }
    if let Some((track, _)) = audio.as_mut()
        && let Some(intent) = append_live_dash_destination_part(track, abort).await?
    {
        finish_capture!(match intent {
            crate::video::StopIntent::Pause => DashLiveCaptureOutcome::Paused,
            crate::video::StopIntent::Finish => DashLiveCaptureOutcome::Finish,
            crate::video::StopIntent::Discard => DashLiveCaptureOutcome::Discard,
        });
    }

    let audio_bytes = audio.as_ref().map_or(0, |(track, bytes)| {
        let _ = track;
        *bytes
    });
    let total_bytes = video_bytes.checked_add(audio_bytes).ok_or_else(|| {
        ManifestRunError::Failed("The staged dynamic DASH byte total overflowed".to_string())
    })?;
    if total_bytes > MAX_MANIFEST_DOWNLOAD_BYTES {
        finish_capture!(if !video.state.segments.is_empty() {
            DashLiveCaptureOutcome::FinishVideoOnly
        } else {
            DashLiveCaptureOutcome::Discard
        });
    }
    downloaded.store(total_bytes, Ordering::Relaxed);
    ensure_manifest_mux_spaces(staging_dir, destination_dir, total_bytes)?;

    let (Some(mut current_video_plan), mut current_audio_plan) =
        (video_plan.cloned(), audio_plan.cloned())
    else {
        finish_capture!(if video.state.segments.is_empty() {
            DashLiveCaptureOutcome::Discard
        } else {
            DashLiveCaptureOutcome::Finish
        });
    };
    let mut next_refresh_at = dash_next_refresh_deadline(&current_video_plan.presentation)?;

    match apply_dash_live_snapshot(
        client,
        &current_video_plan,
        &mut video,
        connections,
        tx,
        downloaded,
        abort,
        audio_bytes,
    )
    .await?
    {
        DashLiveUpdateOutcome::Committed => {
            ensure_manifest_mux_spaces(
                staging_dir,
                destination_dir,
                downloaded.load(Ordering::Relaxed),
            )?;
        }
        DashLiveUpdateOutcome::Stopped(intent) => {
            finish_capture!(match intent {
                crate::video::StopIntent::Pause => DashLiveCaptureOutcome::Paused,
                crate::video::StopIntent::Finish => DashLiveCaptureOutcome::Finish,
                crate::video::StopIntent::Discard => DashLiveCaptureOutcome::Discard,
            });
        }
        DashLiveUpdateOutcome::ContinuityLost => {
            finish_capture!(if video.state.segments.is_empty() {
                DashLiveCaptureOutcome::Discard
            } else {
                DashLiveCaptureOutcome::Unrecoverable
            });
        }
    }

    if let Some(audio) = audio.as_mut() {
        let Some(plan) = current_audio_plan.as_ref() else {
            finish_capture!(DashLiveCaptureOutcome::FinishVideoOnly);
        };
        let updated_video_bytes = dash_live_track_bytes(
            &video.state,
            &video.directory,
            video.committed_resource_count,
            video.committed_bytes,
        )
        .await?;
        match apply_dash_live_snapshot(
            client,
            plan,
            &mut audio.0,
            connections,
            tx,
            downloaded,
            abort,
            updated_video_bytes,
        )
        .await?
        {
            DashLiveUpdateOutcome::Committed => {
                ensure_manifest_mux_spaces(
                    staging_dir,
                    destination_dir,
                    downloaded.load(Ordering::Relaxed),
                )?;
            }
            DashLiveUpdateOutcome::Stopped(intent) => {
                finish_capture!(match intent {
                    crate::video::StopIntent::Pause => DashLiveCaptureOutcome::Paused,
                    crate::video::StopIntent::Finish => {
                        DashLiveCaptureOutcome::FinishVideoOnly
                    }
                    crate::video::StopIntent::Discard => DashLiveCaptureOutcome::Discard,
                });
            }
            DashLiveUpdateOutcome::ContinuityLost => {
                finish_capture!(DashLiveCaptureOutcome::Unrecoverable);
            }
        }
    }
    loop {
        if dash_presentation_ended(&current_video_plan.presentation) {
            finish_capture!(DashLiveCaptureOutcome::Ended);
        }
        let DashPresentation::Dynamic(_) = &current_video_plan.presentation else {
            finish_capture!(DashLiveCaptureOutcome::Ended);
        };
        let deadline = next_refresh_at.ok_or_else(|| {
            ManifestRunError::Failed(
                "A static DASH presentation unexpectedly entered the refresh loop".to_string(),
            )
        })?;
        tokio::select! {
            biased;
            stop = &mut *abort => {
                finish_capture!(match crate::video::stop_intent_or_pause(stop) {
                    crate::video::StopIntent::Pause => DashLiveCaptureOutcome::Paused,
                    crate::video::StopIntent::Finish => DashLiveCaptureOutcome::Finish,
                    crate::video::StopIntent::Discard => DashLiveCaptureOutcome::Discard,
                });
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {}
        }

        let lookup = tokio::select! {
            biased;
            stop = &mut *abort => {
                finish_capture!(match crate::video::stop_intent_or_pause(stop) {
                    crate::video::StopIntent::Pause => DashLiveCaptureOutcome::Paused,
                    crate::video::StopIntent::Finish => DashLiveCaptureOutcome::Finish,
                    crate::video::StopIntent::Discard => DashLiveCaptureOutcome::Discard,
                });
            }
            result = crate::manifest_intake::lookup_with_client(
                client,
                source_url,
                proxy_configured,
            ) => result,
        };
        let lookup = match lookup {
            Ok(Some(lookup)) if lookup.kind() == ManifestKind::Dash => lookup,
            Ok(Some(_)) | Ok(None) => {
                tracing::warn!(
                    "dynamic DASH MPD is no longer supported; preserving committed media"
                );
                finish_capture!(if video.state.segments.is_empty() {
                    DashLiveCaptureOutcome::Discard
                } else {
                    DashLiveCaptureOutcome::FinishVideoOnly
                });
            }
            Err(crate::manifest_intake::ManifestIntakeError::Retryable(error)) => {
                return Err(ManifestRunError::Retryable(error));
            }
            Err(error) => {
                tracing::warn!(error = %error, "dynamic DASH MPD refresh failed; preserving committed media");
                finish_capture!(if video.state.segments.is_empty() {
                    DashLiveCaptureOutcome::Discard
                } else {
                    DashLiveCaptureOutcome::FinishVideoOnly
                });
            }
        };
        let refreshed = resolve_tracks(
            client,
            lookup,
            &ManifestSource {
                url: source_url.to_string(),
                kind: ManifestKind::Dash,
                representation_key: ManifestRepresentationKey::from(selected_video_key.as_str()),
                output_extension: None,
                audio_key: audio_key
                    .clone()
                    .map(crate::media_types::ManifestAudioKey::from),
                subtitle_key: None,
                subtitle_language: None,
            },
            guarded_child_hosts,
        )
        .await;
        let refreshed = match refreshed {
            Ok(Tracks::Dash { video, audio, .. }) => (*video, audio.map(|audio| *audio)),
            Ok(_) => {
                finish_capture!(if video.state.segments.is_empty() {
                    DashLiveCaptureOutcome::Discard
                } else {
                    DashLiveCaptureOutcome::FinishVideoOnly
                });
            }
            Err(ManifestRunError::Unsupported(error)) => {
                tracing::warn!(error = %error, "dynamic DASH track selection disappeared; preserving committed media");
                finish_capture!(if video.state.segments.is_empty() {
                    DashLiveCaptureOutcome::Discard
                } else {
                    DashLiveCaptureOutcome::FinishVideoOnly
                });
            }
            Err(error) => return Err(error),
        };
        current_video_plan = refreshed.0;
        current_audio_plan = refreshed.1;
        next_refresh_at = dash_next_refresh_deadline(&current_video_plan.presentation)?;
        let current_audio_bytes = match audio.as_ref() {
            Some((track, _)) => {
                dash_live_track_bytes(
                    &track.state,
                    &track.directory,
                    track.committed_resource_count,
                    track.committed_bytes,
                )
                .await?
            }
            None => 0,
        };

        match apply_dash_live_snapshot(
            client,
            &current_video_plan,
            &mut video,
            connections,
            tx,
            downloaded,
            abort,
            current_audio_bytes,
        )
        .await?
        {
            DashLiveUpdateOutcome::Committed => {
                ensure_manifest_mux_spaces(
                    staging_dir,
                    destination_dir,
                    downloaded.load(Ordering::Relaxed),
                )?;
            }
            DashLiveUpdateOutcome::Stopped(intent) => {
                finish_capture!(match intent {
                    crate::video::StopIntent::Pause => DashLiveCaptureOutcome::Paused,
                    crate::video::StopIntent::Finish => DashLiveCaptureOutcome::Finish,
                    crate::video::StopIntent::Discard => DashLiveCaptureOutcome::Discard,
                });
            }
            DashLiveUpdateOutcome::ContinuityLost => {
                finish_capture!(if video.state.segments.is_empty() {
                    DashLiveCaptureOutcome::Discard
                } else {
                    DashLiveCaptureOutcome::Unrecoverable
                });
            }
        }

        match (&mut audio, current_audio_plan.as_ref()) {
            (Some((track, _)), Some(plan)) => {
                let video_bytes = dash_live_track_bytes(
                    &video.state,
                    &video.directory,
                    video.committed_resource_count,
                    video.committed_bytes,
                )
                .await?;
                match apply_dash_live_snapshot(
                    client,
                    plan,
                    track,
                    connections,
                    tx,
                    downloaded,
                    abort,
                    video_bytes,
                )
                .await?
                {
                    DashLiveUpdateOutcome::Committed => {
                        ensure_manifest_mux_spaces(
                            staging_dir,
                            destination_dir,
                            downloaded.load(Ordering::Relaxed),
                        )?;
                    }
                    DashLiveUpdateOutcome::Stopped(intent) => {
                        finish_capture!(match intent {
                            crate::video::StopIntent::Pause => DashLiveCaptureOutcome::Paused,
                            crate::video::StopIntent::Finish => {
                                DashLiveCaptureOutcome::FinishVideoOnly
                            }
                            crate::video::StopIntent::Discard => DashLiveCaptureOutcome::Discard,
                        });
                    }
                    DashLiveUpdateOutcome::ContinuityLost => {
                        finish_capture!(if video.state.segments.is_empty() {
                            DashLiveCaptureOutcome::Discard
                        } else {
                            DashLiveCaptureOutcome::Unrecoverable
                        });
                    }
                }
            }
            (None, Some(plan)) => {
                let key = audio_key
                    .clone()
                    .unwrap_or_else(|| plan.representation.live_key.clone());
                let (mut track, _) = load_dash_live_track_for_destination(
                    &key,
                    audio_dir,
                    Some(plan),
                    "audio-track.mp4",
                    destination,
                    item_id,
                    crate::manifest_destination::TrackKind::Audio,
                )
                .await?;
                let video_bytes = dash_live_track_bytes(
                    &video.state,
                    &video.directory,
                    video.committed_resource_count,
                    video.committed_bytes,
                )
                .await?;
                match apply_dash_live_snapshot(
                    client,
                    plan,
                    &mut track,
                    connections,
                    tx,
                    downloaded,
                    abort,
                    video_bytes,
                )
                .await?
                {
                    DashLiveUpdateOutcome::Committed => {
                        ensure_manifest_mux_spaces(
                            staging_dir,
                            destination_dir,
                            downloaded.load(Ordering::Relaxed),
                        )?;
                        let bytes = dash_live_track_bytes(
                            &track.state,
                            &track.directory,
                            track.committed_resource_count,
                            track.committed_bytes,
                        )
                        .await?;
                        audio_key = Some(key);
                        audio = Some((track, bytes));
                    }
                    DashLiveUpdateOutcome::Stopped(intent) => {
                        finish_capture!(match intent {
                            crate::video::StopIntent::Pause => DashLiveCaptureOutcome::Paused,
                            crate::video::StopIntent::Finish => {
                                DashLiveCaptureOutcome::FinishVideoOnly
                            }
                            crate::video::StopIntent::Discard => DashLiveCaptureOutcome::Discard,
                        });
                    }
                    DashLiveUpdateOutcome::ContinuityLost => {
                        finish_capture!(DashLiveCaptureOutcome::Unrecoverable);
                    }
                }
            }
            (Some(_), None) => {
                finish_capture!(if video.state.segments.is_empty() {
                    DashLiveCaptureOutcome::Discard
                } else {
                    DashLiveCaptureOutcome::FinishVideoOnly
                });
            }
            (None, None) => {}
        }

        if dash_presentation_ended(&current_video_plan.presentation) {
            finish_capture!(DashLiveCaptureOutcome::Ended);
        }
    }
}
