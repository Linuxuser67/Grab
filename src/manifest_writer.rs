//! Crash-safe ordered writes for native manifest tracks.
//!
//! Static HLS resources stream into the destination part; other manifest
//! resources can be assembled from staged files. A digest journal and checkpoint
//! protect each committed resource prefix.

use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};

const CHECKPOINT_VERSION: u32 = 3;
const PREVIOUS_CHECKPOINT_VERSION: u32 = 2;
const LEGACY_CHECKPOINT_VERSION: u32 = 1;
const MAX_TRACK_BYTES: u64 = 1 << 40;
/// Maximum size of one staged manifest resource.
pub(crate) const MAX_RESOURCE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_CHECKPOINT_BYTES: u64 = 1024 * 1024;

#[path = "manifest_writer_journal.rs"]
mod journal;

#[cfg(test)]
use journal::read_resource_journal;
use journal::{append_resource_record, load_resource_records, write_resource_journal};

/// Length and SHA-256 digest of one resource committed to an ordered track.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommittedResource {
    pub(crate) length: u64,
    pub(crate) sha256: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WriterCheckpoint {
    version: u32,
    plan_fingerprint: String,
    next_resource_index: usize,
    committed_offset: u64,
    #[serde(default)]
    append_only: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckpointPolicy {
    RemoveAfterPublish,
    RetainAfterPublish,
    RetainSources,
}

#[derive(Debug)]
struct CheckpointPaths<'a> {
    checkpoint: &'a Path,
    journal: &'a Path,
    part: &'a Path,
    target: &'a Path,
    directory: &'a Path,
}

pub(crate) enum AppendNextOutcome {
    Committed,
    Stopped(crate::video::StopIntent),
}

enum CheckpointState {
    Fresh,
    Resume {
        next_resource_index: usize,
        committed_offset: u64,
        resources: Vec<CommittedResource>,
    },
    ResumeFromTarget {
        next_resource_index: usize,
        committed_offset: u64,
        resources: Vec<CommittedResource>,
    },
    AlreadyComplete {
        committed_offset: u64,
        resources: Vec<CommittedResource>,
    },
}

/// Incrementally assemble manifest resources in order.
///
/// Each committed resource has a fixed-size SHA-256 record in an append-only
/// journal. The checkpoint records the durable committed prefix length.
pub(crate) struct OrderedTrackWriter {
    resources: Vec<String>,
    directory: std::path::PathBuf,
    target: std::path::PathBuf,
    part: std::path::PathBuf,
    checkpoint_path: std::path::PathBuf,
    journal_path: std::path::PathBuf,
    fingerprint: String,
    output: tokio::fs::File,
    next_resource_index: usize,
    committed_offset: u64,
    pending_resource_digest: Sha256,
    pending_resource_bytes: u64,
    checkpoint_policy: CheckpointPolicy,
    destination_part: Option<DestinationPart>,
    append_only: bool,
}

struct DestinationPart {
    staging: std::path::PathBuf,
    destination: std::path::PathBuf,
    item_id: u64,
    track: crate::manifest_destination::TrackKind,
}

enum WriterMode {
    #[cfg(test)]
    Staged,
    #[cfg(test)]
    StagedRetainingCheckpoint,
    DestinationPart(DestinationPart),
    AppendableDestinationPart(DestinationPart),
    AppendableDestinationPartRetainingSources(DestinationPart),
}

impl WriterMode {
    fn into_parts(self) -> (CheckpointPolicy, Option<DestinationPart>, bool) {
        match self {
            #[cfg(test)]
            Self::Staged => (CheckpointPolicy::RemoveAfterPublish, None, false),
            #[cfg(test)]
            Self::StagedRetainingCheckpoint => (CheckpointPolicy::RetainAfterPublish, None, false),
            Self::DestinationPart(part) => {
                (CheckpointPolicy::RetainAfterPublish, Some(part), false)
            }
            Self::AppendableDestinationPart(part) => {
                (CheckpointPolicy::RetainAfterPublish, Some(part), true)
            }
            Self::AppendableDestinationPartRetainingSources(part) => {
                (CheckpointPolicy::RetainSources, Some(part), true)
            }
        }
    }
}

struct HashingWriter<'a> {
    output: &'a mut tokio::fs::File,
    digest: Sha256,
}

impl<'a> HashingWriter<'a> {
    fn new(output: &'a mut tokio::fs::File) -> Self {
        Self {
            output,
            digest: Sha256::new(),
        }
    }

    fn finish(self) -> String {
        digest_hex(&self.digest.finalize())
    }
}

impl tokio::io::AsyncWrite for HashingWriter<'_> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        match std::pin::Pin::new(&mut *this.output).poll_write(context, buffer) {
            std::task::Poll::Ready(Ok(written)) => {
                this.digest.update(&buffer[..written]);
                std::task::Poll::Ready(Ok(written))
            }
            other => other,
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.get_mut().output).poll_flush(context)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.get_mut().output).poll_shutdown(context)
    }
}

impl OrderedTrackWriter {
    /// Open a track writer, restoring its last durable prefix when available.
    ///
    /// The returned byte count is already committed to this track, including
    /// when the final rename completed before checkpoint cleanup.
    ///
    /// # Errors
    ///
    /// Returns an error if the resource plan, checkpoint, or staging files are invalid.
    #[cfg(test)]
    pub(crate) async fn open(
        resources: Vec<String>,
        directory: &Path,
        filename: &str,
    ) -> Result<(Option<Self>, u64), String> {
        Box::pin(Self::open_with_mode(
            resources,
            directory,
            filename,
            WriterMode::Staged,
        ))
        .await
    }

    /// Open a writer whose checkpoint remains available after publication.
    ///
    /// Live manifest recovery uses the retained checkpoint and digest journal to
    /// prove that staged source resources can be removed without losing resume.
    ///
    /// # Errors
    ///
    /// Returns an error if the resource plan, checkpoint, or staging files are invalid.
    #[cfg(test)]
    pub(crate) async fn open_retaining_checkpoint(
        resources: Vec<String>,
        directory: &Path,
        filename: &str,
    ) -> Result<(Option<Self>, u64), String> {
        Box::pin(Self::open_with_mode(
            resources,
            directory,
            filename,
            WriterMode::StagedRetainingCheckpoint,
        ))
        .await
    }

    /// Open a writer whose output is an exclusively claimed part beside the
    /// user's final destination. Recovery metadata and source resources remain
    /// in the XDG staging directory.
    ///
    /// # Errors
    ///
    /// Returns an error if the plan, checkpoint, ownership record, or files are invalid.
    pub(crate) async fn open_destination_part(
        resources: Vec<String>,
        directory: &Path,
        staging: &Path,
        filename: &str,
        destination: &Path,
        item_id: u64,
        track: crate::manifest_destination::TrackKind,
    ) -> Result<(Option<Self>, u64), String> {
        Box::pin(Self::open_with_mode(
            resources,
            directory,
            filename,
            WriterMode::DestinationPart(DestinationPart {
                staging: staging.to_path_buf(),
                destination: destination.to_path_buf(),
                item_id,
                track,
            }),
        ))
        .await
    }

    /// Open a destination-side part whose resource plan may grow between MPD refreshes.
    ///
    /// The checkpoint fingerprint covers only the committed resource prefix; every
    /// resumed prefix is still checked against the digest journal and owned part.
    ///
    /// # Errors
    ///
    /// Returns an error if the destination part, checkpoint, or committed prefix is invalid.
    pub(crate) async fn open_appendable_destination_part(
        resources: Vec<String>,
        directory: &Path,
        staging: &Path,
        filename: &str,
        destination: &Path,
        item_id: u64,
        track: crate::manifest_destination::TrackKind,
    ) -> Result<(Option<Self>, u64), String> {
        Box::pin(Self::open_with_mode(
            resources,
            directory,
            filename,
            WriterMode::AppendableDestinationPart(DestinationPart {
                staging: staging.to_path_buf(),
                destination: destination.to_path_buf(),
                item_id,
                track,
            }),
        ))
        .await
    }

    /// Open a growing destination part while retaining source resources in XDG staging.
    ///
    /// Live HLS keeps its committed local playlist and segments as its recovery index.
    ///
    /// # Errors
    ///
    /// Returns an error if the plan, checkpoint, ownership record, or files are invalid.
    pub(crate) async fn open_appendable_destination_part_retaining_sources(
        resources: Vec<String>,
        directory: &Path,
        staging: &Path,
        filename: &str,
        destination: &Path,
        item_id: u64,
        track: crate::manifest_destination::TrackKind,
    ) -> Result<(Option<Self>, u64), String> {
        Box::pin(Self::open_with_mode(
            resources,
            directory,
            filename,
            WriterMode::AppendableDestinationPartRetainingSources(DestinationPart {
                staging: staging.to_path_buf(),
                destination: destination.to_path_buf(),
                item_id,
                track,
            }),
        ))
        .await
    }

    async fn open_with_mode(
        resources: Vec<String>,
        directory: &Path,
        filename: &str,
        mode: WriterMode,
    ) -> Result<(Option<Self>, u64), String> {
        let (checkpoint_policy, destination_part, append_only) = mode.into_parts();
        if resources.is_empty() {
            return Err("The manifest recording contains no media resources".to_string());
        }
        if !safe_filename(filename) || resources.iter().any(|name| !safe_filename(name)) {
            return Err("The manifest track contains an unsafe staging filename".to_string());
        }
        let mut unique = std::collections::HashSet::with_capacity(resources.len());
        if resources.iter().any(|name| !unique.insert(name)) {
            return Err("The manifest track contains duplicate resource identities".to_string());
        }

        let fingerprint = plan_fingerprint(&resources)?;
        let target = directory.join(filename);
        let part = match destination_part.as_ref() {
            Some(destination_part) => {
                crate::manifest_destination::part_path_async(
                    destination_part.staging.clone(),
                    destination_part.destination.clone(),
                    destination_part.item_id,
                    destination_part.track,
                )
                .await?
            }
            None => directory.join(format!("{filename}.part")),
        };
        let checkpoint_path = directory.join(format!("{filename}.checkpoint.json"));
        let journal_path = directory.join(format!("{filename}.resources"));
        if let Some(destination_part) = destination_part.as_ref() {
            match tokio::fs::symlink_metadata(&part).await {
                Ok(_) => {
                    drop(
                        crate::manifest_destination::open_owned_part_async(
                            destination_part.staging.clone(),
                            destination_part.destination.clone(),
                            destination_part.item_id,
                            destination_part.track,
                        )
                        .await?,
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "Could not inspect destination manifest part: {error}"
                    ));
                }
            }
        }
        let checkpoint_state = load_checkpoint(
            CheckpointPaths {
                checkpoint: &checkpoint_path,
                journal: &journal_path,
                part: &part,
                target: &target,
                directory,
            },
            &fingerprint,
            &resources,
            destination_part.is_some(),
            append_only,
        )
        .await?;

        // A destination part is user-visible and outlives the source files.
        // Persist its zero-length checkpoint before creating/opening it so a
        // pause or crash before the first committed resource remains resumable.
        let fresh_destination_part =
            destination_part.is_some() && matches!(&checkpoint_state, CheckpointState::Fresh);
        let (next_resource_index, committed_offset, committed_resources, migration_source) =
            match checkpoint_state {
                CheckpointState::AlreadyComplete {
                    committed_offset,
                    resources: completed_resources,
                } => {
                    if destination_part.is_some() {
                        (
                            resources.len(),
                            committed_offset,
                            completed_resources,
                            Some(target.clone()),
                        )
                    } else {
                        if checkpoint_policy == CheckpointPolicy::RemoveAfterPublish {
                            remove_regular_file_if_present(&checkpoint_path).await?;
                            remove_regular_file_if_present(&journal_path).await?;
                            sync_directory(directory).await?;
                        }
                        return Ok((None, committed_offset));
                    }
                }
                CheckpointState::Fresh => {
                    if destination_part.is_none() {
                        remove_regular_file_if_present(&part).await?;
                        remove_regular_file_if_present(&target).await?;
                    }
                    remove_regular_file_if_present(&journal_path).await?;
                    (0, 0, Vec::new(), None)
                }
                CheckpointState::Resume {
                    next_resource_index,
                    committed_offset,
                    resources,
                } => (next_resource_index, committed_offset, resources, None),
                CheckpointState::ResumeFromTarget {
                    next_resource_index,
                    committed_offset,
                    resources,
                } => (
                    next_resource_index,
                    committed_offset,
                    resources,
                    Some(target.clone()),
                ),
            };
        if fresh_destination_part {
            persist_checkpoint(
                &checkpoint_path,
                directory,
                &WriterCheckpoint {
                    version: CHECKPOINT_VERSION,
                    plan_fingerprint: if append_only {
                        plan_fingerprint(&resources[..0])?
                    } else {
                        fingerprint.clone()
                    },
                    next_resource_index: 0,
                    committed_offset: 0,
                    append_only,
                },
            )
            .await?;
        }
        if checkpoint_policy == CheckpointPolicy::RetainAfterPublish {
            remove_committed_source_files(&resources, directory, &committed_resources).await?;
        }

        let mut output = if let Some(destination_part) = destination_part.as_ref() {
            crate::manifest_destination::open_owned_part_async(
                destination_part.staging.clone(),
                destination_part.destination.clone(),
                destination_part.item_id,
                destination_part.track,
            )
            .await?
        } else {
            tokio::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&part)
                .await
                .map_err(|error| format!("Could not open the ordered manifest track: {error}"))?
        };
        if let Some(migration_source) = migration_source {
            let migration = async {
                let mut input = open_regular_file_no_follow_async(&migration_source)
                    .await
                    .map_err(|error| {
                        format!(
                            "Could not safely open the completed staging track for migration: {error}"
                        )
                    })?;
                verify_resource_ranges_file(&mut input, &migration_source, &committed_resources)
                    .await?;
                input
                    .seek(std::io::SeekFrom::Start(0))
                    .await
                    .map_err(|error| {
                        format!("Could not rewind the completed staging track: {error}")
                    })?;
                output.set_len(0).await.map_err(|error| {
                    format!("Could not reset the destination manifest part for migration: {error}")
                })?;
                output
                    .seek(std::io::SeekFrom::Start(0))
                    .await
                    .map_err(|error| {
                        format!("Could not seek to the start of the destination manifest part: {error}")
                    })?;
                let copied = tokio::io::copy(&mut input, &mut output)
                    .await
                    .map_err(|error| format!("Could not migrate the completed staging track: {error}"))?;
                if copied != committed_offset {
                    return Err(
                        "The completed staging track length does not match its checkpoint"
                            .to_string(),
                    );
                }
                output.sync_all().await.map_err(|error| {
                    format!("Could not sync the migrated destination manifest part: {error}")
                })?;
                if let Some(parent) = part.parent() {
                    sync_directory(parent).await?;
                }
                Ok::<(), String>(())
            }
            .await;

            if let Err(error) = migration {
                cleanup_failed_destination_migration(output, destination_part.as_ref()).await;
                return Err(error);
            }

            // Once the destination copy is durable, source cleanup failures must
            // preserve that copy for recovery rather than deleting the only good data.
            remove_regular_file_if_present(&target).await?;
            sync_directory(directory).await?;
        } else {
            output.set_len(committed_offset).await.map_err(|error| {
                format!("Could not restore the manifest track checkpoint: {error}")
            })?;
            output
                .seek(std::io::SeekFrom::Start(committed_offset))
                .await
                .map_err(|error| {
                    format!("Could not seek to the manifest track checkpoint: {error}")
                })?;
        }

        Ok((
            Some(Self {
                resources,
                directory: directory.to_path_buf(),
                target,
                part,
                checkpoint_path,
                journal_path,
                fingerprint,
                output,
                next_resource_index,
                committed_offset,
                pending_resource_digest: Sha256::new(),
                pending_resource_bytes: 0,
                checkpoint_policy,
                destination_part,
                append_only,
            }),
            committed_offset,
        ))
    }

    #[must_use]
    pub(crate) fn next_resource_index(&self) -> usize {
        self.next_resource_index
    }

    #[must_use]
    pub(crate) fn committed_offset(&self) -> u64 {
        self.committed_offset
    }

    /// Append one network chunk to the active resource in the destination part.
    ///
    /// Bytes remain uncommitted until the streamed resource is durably checkpointed.
    ///
    /// # Errors
    ///
    /// Returns an error if a size limit is exceeded or output writing fails.
    pub(crate) async fn write_streamed_chunk(&mut self, bytes: &[u8]) -> Result<(), String> {
        if self.next_resource_index >= self.resources.len() {
            return Err("All manifest resources have already been committed".to_string());
        }
        let chunk_length = u64::try_from(bytes.len())
            .map_err(|_| "The streamed HLS fragment is too large".to_string())?;
        let next_resource_bytes = self
            .pending_resource_bytes
            .checked_add(chunk_length)
            .filter(|length| *length <= MAX_RESOURCE_BYTES)
            .ok_or_else(|| "An HLS fragment exceeds the 1 GiB limit".to_string())?;
        self.committed_offset
            .checked_add(next_resource_bytes)
            .filter(|offset| *offset <= MAX_TRACK_BYTES)
            .ok_or_else(|| "The assembled HLS track exceeds the 1 TiB limit".to_string())?;

        self.output
            .write_all(bytes)
            .await
            .map_err(|error| format!("Could not write a streamed HLS fragment: {error}"))?;
        self.pending_resource_digest.update(bytes);
        self.pending_resource_bytes = next_resource_bytes;
        Ok(())
    }

    /// Commit the streamed resource and advance its durable manifest-order checkpoint.
    ///
    /// # Errors
    ///
    /// Returns an error if the resource is empty or durable output, journal, or checkpoint I/O fails.
    pub(crate) async fn commit_streamed_resource(&mut self) -> Result<u64, String> {
        if self.append_only || self.destination_part.is_none() {
            return Err(
                "Streamed resource commits require a non-appendable destination-part writer"
                    .to_string(),
            );
        }
        if self.pending_resource_bytes == 0 {
            return Err("The stream server returned an empty HLS fragment".to_string());
        }
        if self.next_resource_index >= self.resources.len() {
            return Err("All HLS resources have already been committed".to_string());
        }
        let next_offset = self
            .committed_offset
            .checked_add(self.pending_resource_bytes)
            .filter(|offset| *offset <= MAX_TRACK_BYTES)
            .ok_or_else(|| "The assembled HLS track exceeds the 1 TiB limit".to_string())?;
        self.output
            .sync_all()
            .await
            .map_err(|error| format!("Could not sync streamed HLS track data: {error}"))?;

        let resource = CommittedResource {
            length: self.pending_resource_bytes,
            sha256: digest_hex(&self.pending_resource_digest.clone().finalize()),
        };
        append_resource_record(&self.journal_path, self.next_resource_index, &resource).await?;
        let next_resource_index = self
            .next_resource_index
            .checked_add(1)
            .ok_or_else(|| "The HLS resource index overflowed".to_string())?;
        persist_checkpoint(
            &self.checkpoint_path,
            &self.directory,
            &WriterCheckpoint {
                version: CHECKPOINT_VERSION,
                plan_fingerprint: self.fingerprint.clone(),
                next_resource_index,
                committed_offset: next_offset,
                append_only: false,
            },
        )
        .await?;

        self.committed_offset = next_offset;
        self.next_resource_index = next_resource_index;
        self.pending_resource_digest = Sha256::new();
        self.pending_resource_bytes = 0;
        Ok(next_offset)
    }

    /// Discard bytes from an incomplete streamed resource, preserving the committed prefix.
    ///
    /// # Errors
    ///
    /// Returns an error if truncating or syncing the destination part fails.
    pub(crate) async fn rollback_streamed_resource(&mut self) -> Result<(), String> {
        self.output
            .set_len(self.committed_offset)
            .await
            .map_err(|error| format!("Could not roll back an incomplete HLS fragment: {error}"))?;
        self.output
            .seek(std::io::SeekFrom::Start(self.committed_offset))
            .await
            .map_err(|error| format!("Could not reposition the HLS destination part: {error}"))?;
        self.output.sync_all().await.map_err(|error| {
            format!("Could not sync the rolled-back HLS destination part: {error}")
        })?;
        self.pending_resource_digest = Sha256::new();
        self.pending_resource_bytes = 0;
        Ok(())
    }

    /// Append exactly the next resource in manifest order.
    ///
    /// Returns false when a user stop request interrupts the append. Any
    /// bytes beyond the last checkpoint are truncated before returning.
    ///
    /// # Errors
    ///
    /// Returns an error if the next source is invalid or durable output/checkpoint I/O fails.
    pub(crate) async fn append_next(
        &mut self,
        abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
    ) -> Result<bool, String> {
        Ok(matches!(
            self.append_next_outcome(abort).await?,
            AppendNextOutcome::Committed
        ))
    }

    pub(crate) async fn append_next_outcome(
        &mut self,
        abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
    ) -> Result<AppendNextOutcome, String> {
        let Some(resource_name) = self.resources.get(self.next_resource_index) else {
            return Err("All manifest resources have already been committed".to_string());
        };
        let resource_path = self.directory.join(resource_name);
        let metadata = tokio::fs::symlink_metadata(&resource_path)
            .await
            .map_err(|error| format!("Could not inspect a staged manifest segment: {error}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err("A staged manifest segment is not a regular file".to_string());
        }
        if metadata.len() == 0 || metadata.len() > MAX_RESOURCE_BYTES {
            return Err("A staged manifest segment has an invalid size".to_string());
        }
        let next_offset = self
            .committed_offset
            .checked_add(metadata.len())
            .filter(|offset| *offset <= MAX_TRACK_BYTES)
            .ok_or_else(|| "The assembled manifest track exceeds the 1 TiB limit".to_string())?;
        let mut input = tokio::fs::File::open(&resource_path)
            .await
            .map_err(|error| format!("Could not open a staged manifest segment: {error}"))?;

        let mut hashing_output = HashingWriter::new(&mut self.output);
        let copied = tokio::select! {
            biased;
            stop = &mut *abort => {
                drop(hashing_output);
                self.output.set_len(self.committed_offset).await
                    .map_err(|error| format!("Could not roll back an interrupted manifest write: {error}"))?;
                self.output.sync_all().await
                    .map_err(|error| format!("Could not sync the interrupted manifest track: {error}"))?;
                return Ok(AppendNextOutcome::Stopped(
                    stop.unwrap_or(crate::video::StopIntent::Discard),
                ));
            }
            result = tokio::io::copy(&mut input, &mut hashing_output) => {
                result.map_err(|error| format!("Could not append a manifest segment in order: {error}"))?
            }
        };
        let sha256 = hashing_output.finish();
        if copied != metadata.len() {
            self.output
                .set_len(self.committed_offset)
                .await
                .map_err(|error| {
                    format!("Could not roll back an incomplete manifest write: {error}")
                })?;
            self.output.sync_all().await.map_err(|error| {
                format!("Could not sync the incomplete manifest track: {error}")
            })?;
            return Err(
                "A staged manifest segment changed while it was being assembled".to_string(),
            );
        }

        self.output
            .sync_all()
            .await
            .map_err(|error| format!("Could not sync ordered manifest track data: {error}"))?;
        let resource = CommittedResource {
            length: copied,
            sha256,
        };
        append_resource_record(&self.journal_path, self.next_resource_index, &resource).await?;

        let next_resource_index = self
            .next_resource_index
            .checked_add(1)
            .ok_or_else(|| "The manifest resource index overflowed".to_string())?;
        persist_checkpoint(
            &self.checkpoint_path,
            &self.directory,
            &WriterCheckpoint {
                version: CHECKPOINT_VERSION,
                plan_fingerprint: if self.append_only {
                    plan_fingerprint(&self.resources[..next_resource_index])?
                } else {
                    self.fingerprint.clone()
                },
                next_resource_index,
                committed_offset: next_offset,
                append_only: self.append_only,
            },
        )
        .await?;

        self.committed_offset = next_offset;
        self.next_resource_index = next_resource_index;
        if self.checkpoint_policy == CheckpointPolicy::RetainAfterPublish {
            remove_committed_source_file_after_commit(&self.directory.join(resource_name)).await?;
        }
        Ok(AppendNextOutcome::Committed)
    }

    /// Publish the complete ordered track without replacing an existing file.
    ///
    /// # Errors
    ///
    /// Returns an error if the track is incomplete or publication cannot be synced.
    pub(crate) async fn finish(self) -> Result<(), String> {
        if self.pending_resource_bytes != 0 {
            return Err("The ordered track has an uncommitted streamed resource".to_string());
        }
        if self.next_resource_index != self.resources.len() {
            return Err(
                "The manifest track cannot be published before all resources are committed"
                    .to_string(),
            );
        }
        self.output
            .sync_all()
            .await
            .map_err(|error| format!("Could not flush the completed manifest track: {error}"))?;
        drop(self.output);
        if let Some(parent) = self.part.parent() {
            sync_directory(parent).await?;
        }
        if self.destination_part.is_none() {
            crate::file_names::rename_noreplace(&self.part, &self.target).map_err(|error| {
                format!("Could not publish the ordered manifest track: {error}")
            })?;
            sync_directory(&self.directory).await?;
        }
        if self.checkpoint_policy == CheckpointPolicy::RemoveAfterPublish {
            remove_regular_file_if_present(&self.checkpoint_path).await?;
            remove_regular_file_if_present(&self.journal_path).await?;
            sync_directory(&self.directory).await?;
        }
        Ok(())
    }
}

/// Roll back an owned destination part when a legacy staging migration fails.
async fn cleanup_failed_destination_migration(
    output: tokio::fs::File,
    destination_part: Option<&DestinationPart>,
) {
    drop(output);
    if let Some(destination_part) = destination_part {
        let _ = crate::manifest_destination::remove_owned_part_async(
            destination_part.staging.clone(),
            destination_part.destination.clone(),
            destination_part.item_id,
            destination_part.track,
        )
        .await;
    }
}

pub(crate) async fn cleanup_destination_part(
    staging: &Path,
    track_directory: &Path,
    destination: &Path,
    item_id: u64,
    track: crate::manifest_destination::TrackKind,
    filename: &str,
) -> Result<(), String> {
    remove_regular_file_if_present(&track_directory.join(format!("{filename}.checkpoint.json")))
        .await?;
    remove_regular_file_if_present(&track_directory.join(format!("{filename}.resources"))).await?;
    sync_directory(track_directory).await?;
    crate::manifest_destination::remove_owned_part_async(
        staging.to_path_buf(),
        destination.to_path_buf(),
        item_id,
        track,
    )
    .await?;
    sync_directory(staging).await
}

/// Return the resource prefix proven durable by the ordered writer.
///
/// A prefix in the `.part` file is sufficient to recover a paused or
/// interrupted finalization. Missing source files are accepted only for
/// resources whose byte ranges are verified against that prefix.
///
/// # Errors
///
/// Returns an error if the checkpoint, digest journal, or committed output bytes are invalid.
pub(crate) async fn committed_resource_prefix(
    resources: &[String],
    directory: &Path,
    filename: &str,
) -> Result<Vec<CommittedResource>, String> {
    committed_resource_prefix_inner(resources, directory, filename, None).await
}

/// Verify a committed prefix stored in a caller-supplied output part.
pub(crate) async fn committed_resource_prefix_at(
    resources: &[String],
    directory: &Path,
    filename: &str,
    part: &Path,
) -> Result<Vec<CommittedResource>, String> {
    committed_resource_prefix_inner(resources, directory, filename, Some(part)).await
}

async fn committed_resource_prefix_inner(
    resources: &[String],
    directory: &Path,
    filename: &str,
    part_override: Option<&Path>,
) -> Result<Vec<CommittedResource>, String> {
    if resources.is_empty() {
        return Ok(Vec::new());
    }
    if !safe_filename(filename) || resources.iter().any(|name| !safe_filename(name)) {
        return Err("The manifest committed-resource plan is invalid".to_string());
    }
    let checkpoint_path = directory.join(format!("{filename}.checkpoint.json"));
    match tokio::fs::symlink_metadata(&checkpoint_path).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err("The manifest writer checkpoint is not a regular file".to_string());
        }
        Ok(metadata) if metadata.len() > MAX_CHECKPOINT_BYTES => {
            return Err("The manifest writer checkpoint exceeds its size limit".to_string());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!(
                "Could not inspect {}: {error}",
                checkpoint_path.display()
            ));
        }
    }
    let bytes = tokio::fs::read(&checkpoint_path)
        .await
        .map_err(|error| format!("Could not read {}: {error}", checkpoint_path.display()))?;
    let checkpoint: WriterCheckpoint = serde_json::from_slice(&bytes)
        .map_err(|error| format!("The manifest writer checkpoint is invalid: {error}"))?;
    if checkpoint.version == LEGACY_CHECKPOINT_VERSION {
        return Ok(Vec::new());
    }
    if !matches!(
        checkpoint.version,
        PREVIOUS_CHECKPOINT_VERSION | CHECKPOINT_VERSION
    ) {
        return Err("The manifest writer checkpoint version is unsupported".to_string());
    }
    let fingerprint_matches = if checkpoint.append_only {
        checkpoint.version == CHECKPOINT_VERSION
            && checkpoint.next_resource_index <= resources.len()
            && plan_fingerprint(&resources[..checkpoint.next_resource_index])?
                == checkpoint.plan_fingerprint
    } else {
        checkpoint.plan_fingerprint == plan_fingerprint(resources)?
    };
    if !fingerprint_matches {
        return Err(
            "The manifest writer checkpoint does not match the selected resource plan".to_string(),
        );
    }
    if checkpoint.next_resource_index > resources.len() {
        return Err(
            "The manifest writer checkpoint exceeds the selected resource plan".to_string(),
        );
    }

    let committed = load_resource_records(
        &directory.join(format!("{filename}.resources")),
        directory,
        resources,
        checkpoint.next_resource_index,
        checkpoint.committed_offset,
        true,
    )
    .await?;

    let part = part_override.map_or_else(
        || directory.join(format!("{filename}.part")),
        Path::to_path_buf,
    );
    let target = directory.join(filename);
    let part_metadata = match tokio::fs::symlink_metadata(&part).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err("The interrupted manifest track is not a regular file".to_string());
        }
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(format!("Could not inspect {}: {error}", part.display()));
        }
    };
    let target_metadata = match tokio::fs::symlink_metadata(&target).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err("The published manifest track is not a regular file".to_string());
        }
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(format!("Could not inspect {}: {error}", target.display()));
        }
    };
    if part_metadata.is_some() && target_metadata.is_some() {
        if part_override.is_none() || checkpoint.next_resource_index != resources.len() {
            return Err(
                "Both the interrupted and published manifest track files exist".to_string(),
            );
        }
        let target_length = target_metadata
            .as_ref()
            .map_or(0, |metadata| metadata.len());
        if target_length != checkpoint.committed_offset {
            return Err("The completed staging track does not match its checkpoint".to_string());
        }
        verify_resource_ranges(&target, &committed).await?;
        return Ok(committed);
    }

    let output_path = if let Some(metadata) = &part_metadata {
        if metadata.len() < checkpoint.committed_offset {
            return Err(
                "The interrupted manifest track is shorter than its checkpoint".to_string(),
            );
        }
        Some(&part)
    } else if let Some(metadata) = &target_metadata {
        if checkpoint.next_resource_index != resources.len()
            || metadata.len() != checkpoint.committed_offset
        {
            return Err(
                "The published manifest track does not match its complete checkpoint".to_string(),
            );
        }
        Some(&target)
    } else {
        None
    };

    if let Some(output_path) = output_path {
        verify_resource_ranges(output_path, &committed).await?;
        Ok(committed)
    } else {
        // Without output bytes, the runner must continue using staged sources.
        // The writer validates them before resetting this checkpoint.
        Ok(Vec::new())
    }
}

/// Return a complete published track's verified resource metadata.
///
/// # Errors
///
/// Returns an error if the published track or its retained checkpoint is invalid.
#[cfg(test)]
pub(crate) async fn committed_resources(
    resources: &[String],
    directory: &Path,
    filename: &str,
) -> Result<Option<Vec<CommittedResource>>, String> {
    if resources.is_empty() {
        return Ok(None);
    }
    let committed = committed_resource_prefix(resources, directory, filename).await?;
    if committed.len() != resources.len() {
        return Ok(None);
    }
    let part = directory.join(format!("{filename}.part"));
    match tokio::fs::symlink_metadata(&part).await {
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!("Could not inspect {}: {error}", part.display()));
        }
    }
    let target = directory.join(filename);
    match tokio::fs::symlink_metadata(&target).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err("The published manifest track is not a regular file".to_string())
        }
        Ok(_) => Ok(Some(committed)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("Could not inspect {}: {error}", target.display())),
    }
}

/// Remove one source after its output range and checkpoint are durable.
///
/// The directory entry is synced when the full prefix is cleaned on resume or
/// when the track is published; an interrupted deletion is safe to repeat.
async fn remove_committed_source_file_after_commit(path: &Path) -> Result<(), String> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err("A staged manifest resource is not a regular file".to_string())
        }
        Ok(_) => tokio::fs::remove_file(path)
            .await
            .map_err(|error| format!("Could not remove {}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Could not inspect {}: {error}", path.display())),
    }
}

/// Remove source resources only after their committed output ranges have been
/// verified. Missing files are expected after a crash during this cleanup.
async fn remove_committed_source_files(
    resources: &[String],
    directory: &Path,
    committed: &[CommittedResource],
) -> Result<(), String> {
    if committed.len() > resources.len() {
        return Err("The manifest committed prefix exceeds its resource plan".to_string());
    }
    let mut removed_any = false;
    for name in resources.iter().take(committed.len()) {
        let path = directory.join(name);
        match tokio::fs::symlink_metadata(&path).await {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err("A staged manifest resource is not a regular file".to_string());
            }
            Ok(_) => {
                tokio::fs::remove_file(&path)
                    .await
                    .map_err(|error| format!("Could not remove {}: {error}", path.display()))?;
                removed_any = true;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!("Could not inspect {}: {error}", path.display()));
            }
        }
    }
    if removed_any {
        sync_directory(directory).await?;
    }
    Ok(())
}

async fn verify_resource_ranges(
    output_path: &Path,
    committed: &[CommittedResource],
) -> Result<(), String> {
    let mut file = open_regular_file_no_follow_async(output_path).await?;
    verify_resource_ranges_file(&mut file, output_path, committed).await
}

async fn verify_resource_ranges_file(
    file: &mut tokio::fs::File,
    output_path: &Path,
    committed: &[CommittedResource],
) -> Result<(), String> {
    file.seek(std::io::SeekFrom::Start(0))
        .await
        .map_err(|error| {
            format!(
                "Could not seek {} for manifest verification: {error}",
                output_path.display()
            )
        })?;
    let mut buffer = [0_u8; 64 * 1024];
    for resource in committed {
        let mut remaining = resource.length;
        let mut digest = Sha256::new();
        while remaining > 0 {
            let chunk_length = usize::try_from(remaining.min(64 * 1024))
                .map_err(|_| "The committed manifest resource length is too large".to_string())?;
            file.read_exact(&mut buffer[..chunk_length])
                .await
                .map_err(|error| {
                    format!(
                        "Could not read {} for manifest verification: {error}",
                        output_path.display()
                    )
                })?;
            digest.update(&buffer[..chunk_length]);
            remaining -= u64::try_from(chunk_length)
                .map_err(|_| "The committed manifest resource length is too large".to_string())?;
        }
        if digest_hex(&digest.finalize()) != resource.sha256 {
            return Err(
                "The ordered manifest track contains corrupted committed bytes".to_string(),
            );
        }
    }
    Ok(())
}

/// Remove remaining source resources after a complete track is published.
/// The retained checkpoint and journal make this operation repeatable.
///
/// # Errors
///
/// Returns an error if the complete output cannot be verified or source cleanup fails.
#[cfg(test)]
pub(crate) async fn cleanup_committed_resources(
    resources: &[String],
    directory: &Path,
    filename: &str,
) -> Result<(), String> {
    let committed = committed_resources(resources, directory, filename)
        .await?
        .ok_or_else(|| {
            "The completed manifest track has no retained verified checkpoint".to_string()
        })?;
    if committed.len() != resources.len() {
        return Err(
            "The completed manifest checkpoint has an incomplete resource list".to_string(),
        );
    }
    remove_committed_source_files(resources, directory, &committed).await
}

async fn load_checkpoint(
    paths: CheckpointPaths<'_>,
    fingerprint: &str,
    resources: &[String],
    allow_part_and_target: bool,
    append_only: bool,
) -> Result<CheckpointState, String> {
    let CheckpointPaths {
        checkpoint: checkpoint_path,
        journal: journal_path,
        part,
        target,
        directory,
    } = paths;
    let checkpoint_metadata = match tokio::fs::symlink_metadata(checkpoint_path).await {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err("The manifest writer checkpoint is not a regular file".to_string());
            }
            if metadata.len() > MAX_CHECKPOINT_BYTES {
                return Err("The manifest writer checkpoint exceeds its size limit".to_string());
            }
            Some(metadata)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(format!(
                "Could not inspect {}: {error}",
                checkpoint_path.display()
            ));
        }
    };
    let Some(_metadata) = checkpoint_metadata else {
        // Appendable destination parts outlive their source segments. A missing
        // checkpoint therefore cannot be interpreted as a fresh recording:
        // doing so would reset the committed offset and truncate the only copy.
        if allow_part_and_target {
            for (path, description) in [
                (journal_path, "resource journal"),
                (part, "destination part"),
                (target, "published destination"),
            ] {
                match tokio::fs::symlink_metadata(path).await {
                    Ok(_) => {
                        return Err(format!(
                            "The manifest writer checkpoint is missing while its {description} still exists; existing data was preserved"
                        ));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!(
                            "Could not inspect {} while validating a missing checkpoint: {error}",
                            path.display()
                        ));
                    }
                }
            }
        }
        return Ok(CheckpointState::Fresh);
    };

    let bytes = tokio::fs::read(checkpoint_path)
        .await
        .map_err(|error| format!("Could not read {}: {error}", checkpoint_path.display()))?;
    let mut checkpoint: WriterCheckpoint = serde_json::from_slice(&bytes)
        .map_err(|error| format!("The manifest writer checkpoint is invalid: {error}"))?;
    let fingerprint_matches = if append_only && checkpoint.append_only {
        checkpoint.version == CHECKPOINT_VERSION
            && checkpoint.next_resource_index <= resources.len()
            && plan_fingerprint(&resources[..checkpoint.next_resource_index])?
                == checkpoint.plan_fingerprint
    } else {
        !checkpoint.append_only && checkpoint.plan_fingerprint == fingerprint
    };
    if !matches!(
        checkpoint.version,
        LEGACY_CHECKPOINT_VERSION | PREVIOUS_CHECKPOINT_VERSION | CHECKPOINT_VERSION
    ) || !fingerprint_matches
        || checkpoint.next_resource_index > resources.len()
        || checkpoint.committed_offset > MAX_TRACK_BYTES
        || (checkpoint.next_resource_index == 0) != (checkpoint.committed_offset == 0)
    {
        return Err(
            "The manifest writer checkpoint does not match the selected resource plan".to_string(),
        );
    }

    let part_metadata = match tokio::fs::symlink_metadata(part).await {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err("The interrupted manifest track is not a regular file".to_string());
            }
            Some(metadata)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(format!("Could not inspect {}: {error}", part.display()));
        }
    };
    let target_metadata = match tokio::fs::symlink_metadata(target).await {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err("The published manifest track is not a regular file".to_string());
            }
            Some(metadata)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(format!("Could not inspect {}: {error}", target.display()));
        }
    };
    if part_metadata.is_some() && target_metadata.is_some() && !allow_part_and_target {
        return Err("Both the interrupted and published manifest track files exist".to_string());
    }
    if target_metadata.is_some() && checkpoint.next_resource_index != resources.len() {
        return Err("A published manifest track has an incomplete checkpoint".to_string());
    }

    let committed = if checkpoint.version == LEGACY_CHECKPOINT_VERSION {
        let mut committed = Vec::with_capacity(checkpoint.next_resource_index);
        let mut offset = 0_u64;
        for resource_name in resources.iter().take(checkpoint.next_resource_index) {
            let resource_path = directory.join(resource_name);
            let metadata = tokio::fs::symlink_metadata(&resource_path)
                .await
                .map_err(|error| {
                    format!("Could not validate a legacy committed manifest resource: {error}")
                })?;
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || metadata.len() == 0
                || metadata.len() > MAX_RESOURCE_BYTES
            {
                return Err(
                    "A legacy committed manifest resource is not a valid regular file".to_string(),
                );
            }
            committed.push(CommittedResource {
                length: metadata.len(),
                sha256: hash_file_range(&resource_path, 0, metadata.len()).await?,
            });
            offset = offset
                .checked_add(metadata.len())
                .filter(|value| *value <= MAX_TRACK_BYTES)
                .ok_or_else(|| {
                    "The legacy committed manifest prefix exceeds the 1 TiB limit".to_string()
                })?;
        }
        if offset != checkpoint.committed_offset {
            return Err(
                "The legacy manifest checkpoint offset does not match its resources".to_string(),
            );
        }
        committed
    } else {
        load_resource_records(
            journal_path,
            directory,
            resources,
            checkpoint.next_resource_index,
            checkpoint.committed_offset,
            true,
        )
        .await?
    };

    if part_metadata.is_some() && target_metadata.is_some() {
        if !allow_part_and_target || checkpoint.next_resource_index != resources.len() {
            return Err("Both interrupted and published manifest tracks exist for an unsupported checkpoint state".to_string());
        }
        let target_length = target_metadata
            .as_ref()
            .map_or(0, |metadata| metadata.len());
        if target_length != checkpoint.committed_offset {
            return Err("The completed staging track does not match its checkpoint".to_string());
        }
        verify_resource_ranges(target, &committed).await?;
        return Ok(CheckpointState::ResumeFromTarget {
            next_resource_index: checkpoint.next_resource_index,
            committed_offset: checkpoint.committed_offset,
            resources: committed,
        });
    }

    let output_path = if part_metadata.is_some() {
        Some(part)
    } else if target_metadata.is_some() {
        Some(target)
    } else {
        None
    };
    if let Some(output_path) = output_path {
        let output_metadata = (if part_metadata.is_some() {
            part_metadata.as_ref()
        } else {
            target_metadata.as_ref()
        })
        .ok_or_else(|| "The ordered manifest output metadata is unavailable".to_string())?;
        let length_is_valid = if part_metadata.is_some() {
            output_metadata.len() >= checkpoint.committed_offset
        } else {
            output_metadata.len() == checkpoint.committed_offset
        };
        if !length_is_valid {
            return Err(
                "The manifest track length does not match its committed checkpoint".to_string(),
            );
        }
        verify_resource_ranges(output_path, &committed).await?;
    } else {
        // Without output bytes, rebuild only while every committed source still
        // exists and matches its checkpoint. A deleted source must never be lost.
        for (index, resource_name) in resources
            .iter()
            .take(checkpoint.next_resource_index)
            .enumerate()
        {
            let resource_path = directory.join(resource_name);
            let metadata = tokio::fs::symlink_metadata(&resource_path)
                .await
                .map_err(|error| {
                    format!(
                        "A committed manifest source is missing and no track bytes remain: {error}"
                    )
                })?;
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || metadata.len() != committed[index].length
                || hash_file_range(&resource_path, 0, metadata.len()).await?
                    != committed[index].sha256
            {
                return Err(
                    "A committed manifest source is invalid and no track bytes remain".to_string(),
                );
            }
        }
        remove_regular_file_if_present(checkpoint_path).await?;
        remove_regular_file_if_present(journal_path).await?;
        sync_directory(directory).await?;
        return Ok(CheckpointState::Fresh);
    }

    if checkpoint.version == LEGACY_CHECKPOINT_VERSION {
        write_resource_journal(journal_path, directory, &committed).await?;
        checkpoint.version = CHECKPOINT_VERSION;
        checkpoint.append_only = false;
        persist_checkpoint(checkpoint_path, directory, &checkpoint).await?;
    }

    if part_metadata.is_some() {
        Ok(CheckpointState::Resume {
            next_resource_index: checkpoint.next_resource_index,
            committed_offset: checkpoint.committed_offset,
            resources: committed,
        })
    } else {
        Ok(CheckpointState::AlreadyComplete {
            committed_offset: checkpoint.committed_offset,
            resources: committed,
        })
    }
}

async fn open_regular_file_no_follow_async(path: &Path) -> Result<tokio::fs::File, String> {
    let path = path.to_path_buf();
    let display_path = path.display().to_string();
    let file = tokio::task::spawn_blocking(move || open_regular_file_no_follow(&path))
        .await
        .map_err(|error| format!("manifest file open task failed: {error}"))?
        .map_err(|error| format!("Could not safely open {display_path}: {error}"))?;
    Ok(tokio::fs::File::from_std(file))
}

fn open_regular_file_no_follow(path: &Path) -> Result<std::fs::File, std::io::Error> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path is not a regular file",
        ));
    }
    Ok(file)
}

async fn hash_file_range(path: &Path, offset: u64, length: u64) -> Result<String, String> {
    let mut file = tokio::fs::OpenOptions::new()
        .read(true)
        .open(path)
        .await
        .map_err(|error| {
            format!(
                "Could not open {} for manifest byte verification: {error}",
                path.display()
            )
        })?;
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|error| {
            format!(
                "Could not seek in {} for manifest byte verification: {error}",
                path.display()
            )
        })?;
    let mut remaining = length;
    let mut buffer = [0_u8; 64 * 1024];
    let mut digest = Sha256::new();
    while remaining > 0 {
        let chunk_length = usize::try_from(remaining.min(64 * 1024))
            .map_err(|_| "The committed manifest resource length is too large".to_string())?;
        file.read_exact(&mut buffer[..chunk_length])
            .await
            .map_err(|error| {
                format!(
                    "Could not read {} for manifest byte verification: {error}",
                    path.display()
                )
            })?;
        digest.update(&buffer[..chunk_length]);
        remaining -= u64::try_from(chunk_length)
            .map_err(|_| "The committed manifest resource length is too large".to_string())?;
    }
    let digest = digest.finalize();
    Ok(digest_hex(&digest))
}

fn digest_hex(bytes: &[u8]) -> String {
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut value, "{byte:02x}");
    }
    value
}

async fn persist_checkpoint(
    checkpoint_path: &Path,
    directory: &Path,
    checkpoint: &WriterCheckpoint,
) -> Result<(), String> {
    let bytes = serde_json::to_vec(checkpoint)
        .map_err(|error| format!("Could not encode the manifest writer checkpoint: {error}"))?;
    let checkpoint_name = checkpoint_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "The manifest checkpoint filename is invalid".to_string())?;
    let temporary = directory.join(format!("{checkpoint_name}.tmp"));
    remove_regular_file_if_present(&temporary).await?;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .await
        .map_err(|error| format!("Could not create the manifest writer checkpoint: {error}"))?;
    file.write_all(&bytes)
        .await
        .map_err(|error| format!("Could not write the manifest writer checkpoint: {error}"))?;
    file.sync_all()
        .await
        .map_err(|error| format!("Could not sync the manifest writer checkpoint: {error}"))?;
    drop(file);
    tokio::fs::rename(&temporary, checkpoint_path)
        .await
        .map_err(|error| format!("Could not publish the manifest writer checkpoint: {error}"))?;
    sync_directory(directory).await
}

async fn sync_directory(directory: &Path) -> Result<(), String> {
    let directory_file = tokio::fs::File::open(directory)
        .await
        .map_err(|error| format!("Could not open {}: {error}", directory.display()))?;
    directory_file
        .sync_all()
        .await
        .map_err(|error| format!("Could not sync {}: {error}", directory.display()))
}

async fn remove_regular_file_if_present(path: &Path) -> Result<(), String> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err("A manifest writer staging path is not a regular file".to_string())
        }
        Ok(_) => tokio::fs::remove_file(path)
            .await
            .map_err(|error| format!("Could not remove {}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Could not inspect {}: {error}", path.display())),
    }
}

fn safe_filename(filename: &str) -> bool {
    crate::file_names::is_single_component(filename) && !filename.contains('\\')
}

fn plan_fingerprint(resources: &[String]) -> Result<String, String> {
    let encoded = serde_json::to_vec(resources)
        .map_err(|error| format!("Could not encode the manifest resource plan: {error}"))?;
    let digest = Sha256::digest(encoded);
    let mut fingerprint = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut fingerprint, "{byte:02x}");
    }
    Ok(fingerprint)
}

#[cfg(test)]
pub(crate) async fn write_ordered_track(
    resources: &[String],
    directory: &Path,
    filename: &str,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
) -> Result<bool, String> {
    let (Some(mut writer), _) =
        OrderedTrackWriter::open(resources.to_vec(), directory, filename).await?
    else {
        return Ok(true);
    };

    while writer.next_resource_index() < resources.len() {
        if !writer.append_next(abort).await? {
            return Ok(false);
        }
    }
    writer.finish().await?;
    Ok(true)
}

#[cfg(test)]
#[path = "manifest_writer_tests.rs"]
mod tests;
