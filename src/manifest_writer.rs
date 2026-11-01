//! Crash-safe ordered assembly for native DASH tracks.
//!
//! Segment requests remain concurrent and bounded by the runner. This module
//! commits their completed files to one track in manifest order. The checkpoint
//! advances only after the output bytes have been synced to disk.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncWriteExt as _, AsyncReadExt as _};

const CHECKPOINT_VERSION: u32 = 1;
const MAX_TRACK_BYTES: u64 = 1 << 40;
const MAX_CHECKPOINT_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WriterCheckpoint {
    version: u32,
    plan_fingerprint: String,
    next_resource_index: usize,
    committed_offset: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckpointState {
    Fresh,
    Resume { next_resource_index: usize, committed_offset: u64 },
    AlreadyComplete,
}

/// Assemble staged DASH resources in their declared order, retaining a durable
/// byte-offset checkpoint after every resource. A stopped or interrupted write
/// is truncated back to the last committed offset on the next attempt.
pub(crate) async fn write_ordered_track(
    resources: &[String],
    directory: &Path,
    filename: &str,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
) -> Result<bool, String> {
    if resources.is_empty() {
        return Err("The DASH recording contains no media resources".to_string());
    }
    if !safe_filename(filename) || resources.iter().any(|name| !safe_filename(name)) {
        return Err("The DASH track contains an unsafe staging filename".to_string());
    }
    let mut unique = std::collections::HashSet::with_capacity(resources.len());
    if resources.iter().any(|name| !unique.insert(name)) {
        return Err("The DASH track contains duplicate resource identities".to_string());
    }

    let fingerprint = plan_fingerprint(resources)?;
    let target = directory.join(filename);
    let part = directory.join(format!("{filename}.part"));
    let checkpoint_path = directory.join(format!("{filename}.checkpoint.json"));
    let checkpoint_state = load_checkpoint(
        &checkpoint_path,
        &part,
        &target,
        &fingerprint,
        resources.len(),
    )
    .await?;

    if checkpoint_state == CheckpointState::AlreadyComplete {
        remove_regular_file_if_present(&checkpoint_path).await?;
        sync_directory(directory).await?;
        return Ok(true);
    }

    let (mut next_resource_index, mut committed_offset) = match checkpoint_state {
        CheckpointState::Fresh => {
            remove_regular_file_if_present(&part).await?;
            remove_regular_file_if_present(&target).await?;
            (0, 0)
        }
        CheckpointState::Resume {
            next_resource_index,
            committed_offset,
        } => (next_resource_index, committed_offset),
        CheckpointState::AlreadyComplete => unreachable!(),
    };

    let mut output = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .open(&part)
        .await
        .map_err(|error| format!("Could not open the ordered DASH track: {error}"))?;
    output
        .set_len(committed_offset)
        .await
        .map_err(|error| format!("Could not restore the DASH track checkpoint: {error}"))?;
    output
        .seek(std::io::SeekFrom::Start(committed_offset))
        .await
        .map_err(|error| format!("Could not seek to the DASH track checkpoint: {error}"))?;

    while next_resource_index < resources.len() {
        let resource_path = directory.join(&resources[next_resource_index]);
        let metadata = tokio::fs::symlink_metadata(&resource_path)
            .await
            .map_err(|error| format!("Could not inspect a staged DASH segment: {error}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err("A staged DASH segment is not a regular file".to_string());
        }
        if metadata.len() == 0 {
            return Err("A staged DASH segment is empty".to_string());
        }
        let next_offset = committed_offset
            .checked_add(metadata.len())
            .filter(|offset| *offset <= MAX_TRACK_BYTES)
            .ok_or_else(|| "The assembled DASH track exceeds the 1 TiB limit".to_string())?;
        let mut input = tokio::fs::File::open(&resource_path)
            .await
            .map_err(|error| format!("Could not open a staged DASH segment: {error}"))?;

        let copied = tokio::select! {
            biased;
            _ = &mut *abort => {
                output.set_len(committed_offset).await
                    .map_err(|error| format!("Could not roll back an interrupted DASH write: {error}"))?;
                output.sync_all().await
                    .map_err(|error| format!("Could not sync the interrupted DASH track: {error}"))?;
                return Ok(false);
            }
            result = tokio::io::copy(&mut input, &mut output) => {
                result.map_err(|error| format!("Could not append a DASH segment in order: {error}"))?
            }
        };
        if copied != metadata.len() {
            output.set_len(committed_offset).await
                .map_err(|error| format!("Could not roll back an incomplete DASH write: {error}"))?;
            output.sync_all().await
                .map_err(|error| format!("Could not sync the incomplete DASH track: {error}"))?;
            return Err("A staged DASH segment changed while it was being assembled".to_string());
        }

        output
            .sync_all()
            .await
            .map_err(|error| format!("Could not sync ordered DASH track data: {error}"))?;
        committed_offset = next_offset;
        next_resource_index += 1;
        persist_checkpoint(
            &checkpoint_path,
            directory,
            &WriterCheckpoint {
                version: CHECKPOINT_VERSION,
                plan_fingerprint: fingerprint.clone(),
                next_resource_index,
                committed_offset,
            },
        )
        .await?;
    }

    output
        .sync_all()
        .await
        .map_err(|error| format!("Could not flush the completed DASH track: {error}"))?;
    drop(output);
    crate::file_names::rename_noreplace(&part, &target)
        .map_err(|error| format!("Could not publish the ordered DASH track: {error}"))?;
    sync_directory(directory).await?;
    remove_regular_file_if_present(&checkpoint_path).await?;
    sync_directory(directory).await?;
    Ok(true)
}

async fn load_checkpoint(
    checkpoint_path: &Path,
    part: &Path,
    target: &Path,
    fingerprint: &str,
    resource_count: usize,
) -> Result<CheckpointState, String> {
    let checkpoint_metadata = match tokio::fs::symlink_metadata(checkpoint_path).await {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err("The DASH writer checkpoint is not a regular file".to_string());
            }
            if metadata.len() > MAX_CHECKPOINT_BYTES {
                return Err("The DASH writer checkpoint exceeds its size limit".to_string());
            }
            Some(metadata)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("Could not inspect the DASH writer checkpoint: {error}")),
    };
    let Some(_metadata) = checkpoint_metadata else {
        return Ok(CheckpointState::Fresh);
    };

    let bytes = tokio::fs::read(checkpoint_path)
        .await
        .map_err(|error| format!("Could not read the DASH writer checkpoint: {error}"))?;
    let checkpoint: WriterCheckpoint = serde_json::from_slice(&bytes)
        .map_err(|error| format!("The DASH writer checkpoint is invalid: {error}"))?;
    if checkpoint.version != CHECKPOINT_VERSION
        || checkpoint.plan_fingerprint != fingerprint
        || checkpoint.next_resource_index > resource_count
        || checkpoint.committed_offset > MAX_TRACK_BYTES
        || (checkpoint.next_resource_index == 0) != (checkpoint.committed_offset == 0)
    {
        return Err("The DASH writer checkpoint does not match the selected resource plan".to_string());
    }

    let part_metadata = match tokio::fs::symlink_metadata(part).await {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err("The interrupted DASH track is not a regular file".to_string());
            }
            Some(metadata)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("Could not inspect the interrupted DASH track: {error}")),
    };
    if let Some(metadata) = part_metadata {
        if metadata.len() < checkpoint.committed_offset {
            return Err("The interrupted DASH track is shorter than its committed checkpoint".to_string());
        }
        return Ok(CheckpointState::Resume {
            next_resource_index: checkpoint.next_resource_index,
            committed_offset: checkpoint.committed_offset,
        });
    }

    if checkpoint.next_resource_index == resource_count {
        if let Ok(metadata) = tokio::fs::symlink_metadata(target).await {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err("The completed DASH track is not a regular file".to_string());
            }
            if metadata.len() == checkpoint.committed_offset {
                return Ok(CheckpointState::AlreadyComplete);
            }
        }
    }
    // The part file disappeared before the final rename. The source segments
    // are still authoritative, so safely rebuild the track from offset zero.
    remove_regular_file_if_present(checkpoint_path).await?;
    Ok(CheckpointState::Fresh)
}

async fn persist_checkpoint(
    checkpoint_path: &Path,
    directory: &Path,
    checkpoint: &WriterCheckpoint,
) -> Result<(), String> {
    let bytes = serde_json::to_vec(checkpoint)
        .map_err(|error| format!("Could not encode the DASH writer checkpoint: {error}"))?;
    let temporary = directory.join(format!(
        "{}.checkpoint.tmp",
        checkpoint_path.file_name().and_then(|name| name.to_str()).unwrap_or("track")
    ));
    remove_regular_file_if_present(&temporary).await?;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .await
        .map_err(|error| format!("Could not create the DASH writer checkpoint: {error}"))?;
    file.write_all(&bytes)
        .await
        .map_err(|error| format!("Could not write the DASH writer checkpoint: {error}"))?;
    file.sync_all()
        .await
        .map_err(|error| format!("Could not sync the DASH writer checkpoint: {error}"))?;
    drop(file);
    tokio::fs::rename(&temporary, checkpoint_path)
        .await
        .map_err(|error| format!("Could not publish the DASH writer checkpoint: {error}"))?;
    sync_directory(directory).await
}

async fn sync_directory(directory: &Path) -> Result<(), String> {
    tokio::fs::File::open(directory)
        .await
        .and_then(|file| async { file.sync_all().await }.await)
        .map_err(|error| format!("Could not sync the DASH staging directory: {error}"))
}

async fn remove_regular_file_if_present(path: &Path) -> Result<(), String> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err("A DASH writer staging path is not a regular file".to_string())
        }
        Ok(_) => tokio::fs::remove_file(path)
            .await
            .map_err(|error| format!("Could not remove stale DASH writer state: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Could not inspect DASH writer state: {error}")),
    }
}

fn safe_filename(filename: &str) -> bool {
    !filename.is_empty()
        && filename != "."
        && filename != ".."
        && !filename.contains('/')
        && !filename.contains('\\')
        && !filename.chars().any(char::is_control)
}

fn plan_fingerprint(resources: &[String]) -> Result<String, String> {
    let encoded = serde_json::to_vec(resources)
        .map_err(|error| format!("Could not encode the DASH resource plan: {error}"))?;
    let digest = Sha256::digest(encoded);
    let mut fingerprint = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut fingerprint, "{byte:02x}");
    }
    Ok(fingerprint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn test_directory() -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock must follow the Unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "grab-ordered-writer-{}-{nonce}",
            std::process::id()
        ))
    }

    async fn stopped_receiver() -> tokio::sync::oneshot::Receiver<crate::video::StopIntent> {
        let (_tx, rx) = tokio::sync::oneshot::channel();
        rx
    }

    #[tokio::test]
    async fn writes_resources_in_manifest_order_and_publishes_track() {
        let directory = test_directory();
        tokio::fs::create_dir_all(&directory).await.expect("create staging");
        tokio::fs::write(directory.join("second.bin"), b"second").await.expect("write second");
        tokio::fs::write(directory.join("first.bin"), b"first").await.expect("write first");
        let resources = vec!["first.bin".to_string(), "second.bin".to_string()];
        let mut abort = stopped_receiver().await;

        assert!(write_ordered_track(&resources, &directory, "track.mp4", &mut abort).await.expect("write track"));
        assert_eq!(tokio::fs::read(directory.join("track.mp4")).await.expect("read track"), b"firstsecond");
        assert!(!directory.join("track.mp4.checkpoint.json").exists());
        tokio::fs::remove_dir_all(directory).await.expect("remove staging");
    }

    #[tokio::test]
    async fn resumes_from_checkpoint_and_truncates_uncommitted_tail() {
        let directory = test_directory();
        tokio::fs::create_dir_all(&directory).await.expect("create staging");
        tokio::fs::write(directory.join("first.bin"), b"first").await.expect("write first");
        tokio::fs::write(directory.join("second.bin"), b"second").await.expect("write second");
        let resources = vec!["first.bin".to_string(), "second.bin".to_string()];
        let fingerprint = plan_fingerprint(&resources).expect("fingerprint");
        let checkpoint = WriterCheckpoint {
            version: CHECKPOINT_VERSION,
            plan_fingerprint: fingerprint,
            next_resource_index: 1,
            committed_offset: 5,
        };
        tokio::fs::write(
            directory.join("track.mp4.part"),
            b"first-uncommitted-tail",
        ).await.expect("write interrupted output");
        tokio::fs::write(
            directory.join("track.mp4.checkpoint.json"),
            serde_json::to_vec(&checkpoint).expect("serialize checkpoint"),
        ).await.expect("write checkpoint");
        let mut abort = stopped_receiver().await;

        assert!(write_ordered_track(&resources, &directory, "track.mp4", &mut abort).await.expect("resume track"));
        assert_eq!(tokio::fs::read(directory.join("track.mp4")).await.expect("read track"), b"firstsecond");
        tokio::fs::remove_dir_all(directory).await.expect("remove staging");
    }

    #[tokio::test]
    async fn rejects_checkpoint_for_a_different_resource_order() {
        let directory = test_directory();
        tokio::fs::create_dir_all(&directory).await.expect("create staging");
        let resources = vec!["first.bin".to_string(), "second.bin".to_string()];
        let checkpoint = WriterCheckpoint {
            version: CHECKPOINT_VERSION,
            plan_fingerprint: plan_fingerprint(&["second.bin".to_string(), "first.bin".to_string()]).expect("fingerprint"),
            next_resource_index: 1,
            committed_offset: 5,
        };
        tokio::fs::write(directory.join("track.mp4.checkpoint.json"), serde_json::to_vec(&checkpoint).expect("serialize checkpoint"))
            .await.expect("write checkpoint");
        let mut abort = stopped_receiver().await;

        assert!(write_ordered_track(&resources, &directory, "track.mp4", &mut abort).await.is_err());
        tokio::fs::remove_dir_all(directory).await.expect("remove staging");
    }

    #[tokio::test]
    async fn recovers_crash_after_track_rename_before_checkpoint_removal() {
        let directory = test_directory();
        tokio::fs::create_dir_all(&directory).await.expect("create staging");
        let resources = vec!["first.bin".to_string()];
        tokio::fs::write(directory.join("first.bin"), b"complete").await.expect("write resource");
        let checkpoint = WriterCheckpoint {
            version: CHECKPOINT_VERSION,
            plan_fingerprint: plan_fingerprint(&resources).expect("fingerprint"),
            next_resource_index: 1,
            committed_offset: 8,
        };
        tokio::fs::write(directory.join("track.mp4"), b"complete").await.expect("write published track");
        tokio::fs::write(directory.join("track.mp4.checkpoint.json"), serde_json::to_vec(&checkpoint).expect("serialize checkpoint"))
            .await.expect("write checkpoint");
        let mut abort = stopped_receiver().await;

        assert!(write_ordered_track(&resources, &directory, "track.mp4", &mut abort).await.expect("recover completed track"));
        assert!(!directory.join("track.mp4.checkpoint.json").exists());
        tokio::fs::remove_dir_all(directory).await.expect("remove staging");
    }

    #[test]
    fn rejects_unsafe_and_duplicate_resource_names() {
        assert!(!safe_filename("../outside.bin"));
        assert!(!safe_filename("nested/file.bin"));
        assert!(safe_filename("segment.bin"));
        let names = vec!["segment.bin".to_string(), "segment.bin".to_string()];
        let mut unique = std::collections::HashSet::new();
        assert!(names.iter().any(|name| !unique.insert(name)));
        let _ = Arc::new(());
    }
}
