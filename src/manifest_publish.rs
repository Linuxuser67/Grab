//! Atomic publication for native manifest downloads.
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Result of publishing a finalized native manifest output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ManifestPublishOutcome {
    Published,
    Aborted,
}

/// Cancels a detached blocking copy when its async owner is dropped.
struct DiscardOnDrop {
    gate: Arc<crate::attempt_gate::AttemptGate>,
    armed: bool,
}

impl DiscardOnDrop {
    fn new(gate: Arc<crate::attempt_gate::AttemptGate>) -> Self {
        Self { gate, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DiscardOnDrop {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.gate.discard();
        }
    }
}

/// A filesystem rollback that must finish before discard cleanup inspects delivery.
struct RollbackJob {
    path: PathBuf,
    restore_to: Option<PathBuf>,
}

impl RollbackJob {
    fn run(self, gate: &crate::attempt_gate::AttemptGate) {
        let rollback = match self.restore_to.as_deref() {
            Some(source) => crate::file_names::rename_noreplace_same_device(&self.path, source),
            None => std::fs::remove_file(&self.path),
        };
        if let Err(error) = rollback
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %self.path.display(),
                error = %error,
                "could not roll back an uncommitted manifest destination"
            );
            // The discard finalizer removes a destination whose rollback failed.
            gate.mark_delivered();
            return;
        }

        if let Some(source) = self.restore_to.as_deref() {
            for path in [source, self.path.as_path()] {
                if let Some(parent) = path.parent()
                    && let Err(error) = std::fs::File::open(parent).and_then(|dir| dir.sync_all())
                {
                    tracing::warn!(
                        path = %parent.display(),
                        error = %error,
                        "could not sync a rolled-back manifest directory"
                    );
                }
            }
        } else if let Some(parent) = self.path.parent()
            && let Err(error) = std::fs::File::open(parent).and_then(|dir| dir.sync_all())
        {
            tracing::warn!(
                path = %parent.display(),
                error = %error,
                "could not sync a rolled-back manifest directory"
            );
        }
    }
}

/// Completes the attempt's rollback registration even when the worker unwinds.
struct RollbackCompletion(Arc<crate::attempt_gate::AttemptGate>);

impl Drop for RollbackCompletion {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.mark_delivered();
        }
        self.0.finish_rollback();
    }
}

fn spawn_rollback(
    runtime: &tokio::runtime::Handle,
    job: RollbackJob,
    gate: Arc<crate::attempt_gate::AttemptGate>,
) -> tokio::task::JoinHandle<()> {
    gate.begin_rollback();
    runtime.spawn_blocking(move || {
        let _completion = RollbackCompletion(Arc::clone(&gate));
        job.run(&gate);
    })
}

/// Removes a destination created by this attempt unless its gate claims delivery.
struct PendingDestination {
    path: PathBuf,
    gate: Arc<crate::attempt_gate::AttemptGate>,
    /// A same-filesystem rename can be restored here if the directory sync fails.
    restore_to: Option<PathBuf>,
    armed: bool,
}

impl PendingDestination {
    fn new(
        path: &Path,
        gate: Arc<crate::attempt_gate::AttemptGate>,
        restore_to: Option<&Path>,
    ) -> Self {
        Self {
            path: path.to_path_buf(),
            gate,
            restore_to: restore_to.map(Path::to_path_buf),
            armed: false,
        }
    }

    fn arm(&mut self) {
        self.armed = true;
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    fn take_rollback_job(&mut self) -> Option<RollbackJob> {
        if !self.armed {
            return None;
        }
        self.armed = false;
        Some(RollbackJob {
            path: std::mem::take(&mut self.path),
            restore_to: self.restore_to.take(),
        })
    }

    /// Roll back on Tokio's blocking pool and wait for completion on normal exits.
    async fn rollback(mut self) {
        let Some(job) = self.take_rollback_job() else {
            return;
        };
        let gate = Arc::clone(&self.gate);
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                if let Err(error) = spawn_rollback(&runtime, job, Arc::clone(&gate)).await {
                    gate.mark_delivered();
                    tracing::warn!(
                        error = %error,
                        "manifest destination rollback worker failed"
                    );
                }
            }
            Err(_) => job.run(&gate),
        }
    }
}

impl Drop for PendingDestination {
    fn drop(&mut self) {
        let Some(job) = self.take_rollback_job() else {
            return;
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                // The finalizer waits for this registered job after joining the owner.
                drop(spawn_rollback(&runtime, job, Arc::clone(&self.gate)));
            }
            Err(_) => job.run(&self.gate),
        }
    }
}

/// Copy staged bytes while the attempt remains active.
///
/// Returns `false` when the row is discarded before the copy completes.
fn copy_staged_bytes(
    source: &mut impl std::io::Read,
    destination: &mut impl std::io::Write,
    gate: &crate::attempt_gate::AttemptGate,
) -> std::io::Result<bool> {
    const COPY_BUFFER_SIZE: usize = 256 * 1024;

    let mut buffer = [0_u8; COPY_BUFFER_SIZE];
    loop {
        if !gate.is_active() {
            return Ok(false);
        }
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        destination.write_all(&buffer[..read])?;
    }

    Ok(gate.is_active())
}

/// Copy a staged output into a private temporary in the destination directory.
///
/// The returned temporary is not visible under the final name. Persist it with
/// no-clobber semantics only after the copy and file sync have succeeded.
fn prepare_destination_copy(
    source: &Path,
    destination: &Path,
    gate: &crate::attempt_gate::AttemptGate,
) -> std::io::Result<Option<tempfile::NamedTempFile>> {
    let mut source_file = std::fs::File::open(source)?;
    let permissions = source_file.metadata()?.permissions();
    let mut temporary = crate::file_names::publish_temp_for(destination)?;
    if !copy_staged_bytes(&mut source_file, temporary.as_file_mut(), gate)? {
        return Ok(None);
    }

    // Apply source permissions through the open descriptor before flushing the file.
    temporary.as_file().set_permissions(permissions)?;
    temporary.as_file().sync_all()?;

    if !gate.is_active() {
        return Ok(None);
    }

    Ok(Some(temporary))
}

/// Remove stale destination-derived publication temporaries from the download directory.
///
/// Only regular files matching the destination-temp naming scheme, owned by
/// the current user and older than the startup cutoff are removed. The directory
/// is never traversed recursively, and symlinked roots or entries are refused.
pub(crate) fn sweep_stale_publish_temporaries(
    download_dir: &Path,
    startup_started_at: std::time::SystemTime,
) {
    let Ok(directory_metadata) = std::fs::symlink_metadata(download_dir) else {
        return;
    };
    if !directory_metadata.file_type().is_dir() {
        return;
    }

    #[cfg(unix)]
    let my_uid = {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let uid = crate::download_net::current_uid();
        let mode = directory_metadata.permissions().mode();
        let is_user_owned = directory_metadata.uid() == uid;
        let is_sticky_shared_directory = mode & 0o1000 != 0 && mode & 0o0002 != 0;
        if !is_user_owned && !is_sticky_shared_directory {
            return;
        }
        uid
    };

    let Ok(entries) = std::fs::read_dir(download_dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        if !crate::file_names::is_publish_temporary_name(&name) {
            continue;
        }

        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.file_type().is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            if metadata.uid() != my_uid {
                continue;
            }
        }
        if metadata
            .modified()
            .is_ok_and(|modified| modified < startup_started_at)
            && let Err(error) = std::fs::remove_file(&path)
        {
            tracing::debug!(
                path = %path.display(),
                error = %error,
                "could not remove stale manifest publication temporary"
            );
        }
    }
}

/// Persist a fully synced temporary and claim delivery only after its final name exists.
async fn sync_parent_directory(destination: &Path) -> std::io::Result<()> {
    let parent = destination.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "destination has no parent directory",
        )
    })?;
    tokio::fs::File::open(parent).await?.sync_all().await
}

async fn persist_prepared_copy(
    temporary: tempfile::NamedTempFile,
    destination: &Path,
    gate: &Arc<crate::attempt_gate::AttemptGate>,
) -> std::io::Result<ManifestPublishOutcome> {
    let destination_for_worker = destination.to_path_buf();
    let gate_for_worker = Arc::clone(gate);
    let mut pending = match tokio::task::spawn_blocking(move || {
        let mut pending =
            PendingDestination::new(&destination_for_worker, Arc::clone(&gate_for_worker), None);
        crate::file_names::persist_noclobber_with_fallback(
            temporary,
            &destination_for_worker,
            || gate_for_worker.is_active(),
        )?;
        pending.arm();
        Ok::<_, std::io::Error>(pending)
    })
    .await
    {
        Ok(Ok(pending)) => pending,
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::Interrupted && !gate.is_active() => {
            return Ok(ManifestPublishOutcome::Aborted);
        }
        Ok(Err(error)) => return Err(error),
        Err(error) => return Err(std::io::Error::other(error)),
    };
    if let Err(error) = sync_parent_directory(destination).await {
        pending.rollback().await;
        return Err(error);
    }
    if !gate.try_commit() {
        pending.rollback().await;
        return Ok(ManifestPublishOutcome::Aborted);
    }

    gate.mark_delivered();
    pending.disarm();
    Ok(ManifestPublishOutcome::Published)
}

/// Commit a same-filesystem rename or link after installing a rollback guard.
async fn commit_renamed_destination(
    gate: &Arc<crate::attempt_gate::AttemptGate>,
    mut pending: PendingDestination,
    source: &Path,
    destination: &Path,
) -> std::io::Result<ManifestPublishOutcome> {
    pending.arm();
    if let Err(error) = sync_parent_directory(destination).await {
        pending.rollback().await;
        return Err(error);
    }
    if let Err(error) = sync_parent_directory(source).await {
        pending.rollback().await;
        return Err(error);
    }
    if !gate.try_commit() {
        pending.rollback().await;
        return Ok(ManifestPublishOutcome::Aborted);
    }

    gate.mark_delivered();
    pending.disarm();
    Ok(ManifestPublishOutcome::Published)
}

/// Publish a finalized output without blocking Tokio workers during cross-device copies.
pub(crate) async fn publish(
    source: &Path,
    destination: &Path,
    gate: &Arc<crate::attempt_gate::AttemptGate>,
) -> std::io::Result<ManifestPublishOutcome> {
    if !gate.is_active() {
        return Ok(ManifestPublishOutcome::Aborted);
    }

    match crate::file_names::rename_noreplace_same_device(source, destination) {
        Ok(()) => {
            let pending = PendingDestination::new(destination, Arc::clone(gate), Some(source));
            return commit_renamed_destination(gate, pending, source, destination).await;
        }
        Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {}
        Err(error) => return Err(error),
    }

    let mut cancellation = DiscardOnDrop::new(Arc::clone(gate));
    let source_for_copy = source.to_path_buf();
    let destination_for_copy = destination.to_path_buf();
    let copy_gate = Arc::clone(gate);
    let temporary = tokio::task::spawn_blocking(move || {
        prepare_destination_copy(&source_for_copy, &destination_for_copy, &copy_gate)
    })
    .await
    .map_err(std::io::Error::other)??;

    let Some(temporary) = temporary else {
        return Ok(ManifestPublishOutcome::Aborted);
    };
    if !gate.is_active() {
        return Ok(ManifestPublishOutcome::Aborted);
    }

    let outcome = persist_prepared_copy(temporary, destination, gate).await?;
    cancellation.disarm();
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DiscardAfterFirstWrite<'a> {
        file: &'a mut std::fs::File,
        gate: Arc<crate::attempt_gate::AttemptGate>,
        discarded: bool,
    }

    impl std::io::Write for DiscardAfterFirstWrite<'_> {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            std::io::Write::write_all(self.file, buffer)?;
            if !self.discarded {
                self.discarded = true;
                let _ = self.gate.discard();
            }
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            std::io::Write::flush(self.file)
        }
    }

    #[test]
    fn startup_sweep_removes_only_old_file_derived_publish_temporaries() {
        let directory = tempfile::tempdir().expect("create download directory");
        let stale = directory.path().join(".episode.mkv.publish-stale.partial");
        std::fs::write(&stale, b"orphaned media").expect("write stale temporary");
        std::thread::sleep(std::time::Duration::from_millis(20));
        let startup_started_at = std::time::SystemTime::now();
        let fresh = directory.path().join(".episode.mkv.publish-fresh.partial");
        std::fs::write(&fresh, b"active media").expect("write fresh temporary");
        let unrelated = directory.path().join(".other-temp.partial");
        std::fs::write(&unrelated, b"unrelated").expect("write unrelated file");
        let similar_but_unowned = directory
            .path()
            .join(".episode.mkv.publish-user-file.partial");
        std::fs::write(&similar_but_unowned, b"user file")
            .expect("write similar user file");

        sweep_stale_publish_temporaries(directory.path(), startup_started_at);

        assert!(!stale.exists(), "pre-startup publish leftovers are removed");
        assert!(fresh.exists(), "files created after startup are preserved");
        assert!(
            unrelated.exists(),
            "unrelated temporary files are preserved"
        );
        assert!(
            similar_but_unowned.exists(),
            "a similar user filename with a non-random suffix is preserved"
        );
    }

    #[cfg(unix)]
    #[test]
    fn startup_sweep_does_not_follow_or_remove_symlink_entries() {
        let directory = tempfile::tempdir().expect("create download directory");
        let target = directory.path().join("user-file");
        std::fs::write(&target, b"user data").expect("write target");
        let link = directory.path().join(".final.mkv.publish-link.partial");
        std::os::unix::fs::symlink(&target, &link).expect("create symlink");

        sweep_stale_publish_temporaries(
            directory.path(),
            std::time::SystemTime::now() + std::time::Duration::from_secs(1),
        );

        assert!(link.is_symlink(), "symlink entries are not swept");
        assert_eq!(std::fs::read(&target).unwrap(), b"user data");
    }

    #[tokio::test]
    async fn same_filesystem_publication_commits_the_renamed_output() {
        let directory = tempfile::tempdir().expect("create publication root");
        let staging = directory.path().join("staging");
        let downloads = directory.path().join("downloads");
        std::fs::create_dir(&staging).expect("create staging directory");
        std::fs::create_dir(&downloads).expect("create destination directory");
        let source = staging.join("output.part.mp4");
        let destination = downloads.join("final.mp4");
        std::fs::write(&source, b"complete media payload").expect("write staged output");
        let gate = crate::attempt_gate::AttemptGate::new();

        let outcome = publish(&source, &destination, &gate)
            .await
            .expect("sync both rename parents before delivery");

        assert_eq!(outcome, ManifestPublishOutcome::Published);
        assert!(gate.was_delivered());
        assert!(
            !source.exists(),
            "same-filesystem publication moves staging output"
        );
        assert_eq!(
            std::fs::read(&destination).expect("read delivered output"),
            b"complete media payload"
        );
    }

    #[tokio::test]
    async fn destination_local_copy_is_private_until_no_clobber_persist() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.part");
        let destination = directory.path().join("final.mp4");
        let contents = vec![0x5a; 768 * 1024];
        std::fs::write(&source, &contents).unwrap();
        let gate = crate::attempt_gate::AttemptGate::new();

        let temporary = prepare_destination_copy(&source, &destination, &gate)
            .unwrap()
            .expect("copy should prepare while the gate is active");

        assert!(
            !destination.exists(),
            "the final name stays absent before persist"
        );
        assert_eq!(temporary.path().parent(), destination.parent());
        assert_eq!(std::fs::read(temporary.path()).unwrap(), contents);

        let outcome = persist_prepared_copy(temporary, &destination, &gate)
            .await
            .unwrap();
        assert_eq!(outcome, ManifestPublishOutcome::Published);
        assert!(gate.was_delivered());
        assert_eq!(std::fs::read(&destination).unwrap(), contents);
        assert!(
            source.exists(),
            "the source remains for normal staging cleanup"
        );
    }

    #[tokio::test]
    async fn cancelled_prepared_publication_returns_aborted() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("final.mp4");
        let temporary = crate::file_names::publish_temp_for(&destination).unwrap();
        std::fs::write(temporary.path(), b"prepared output").unwrap();
        let gate = crate::attempt_gate::AttemptGate::new();
        assert!(gate.discard());

        let outcome = persist_prepared_copy(temporary, &destination, &gate)
            .await
            .expect("cancellation is an aborted publication");

        assert_eq!(outcome, ManifestPublishOutcome::Aborted);
        assert!(!destination.exists());
    }

    #[tokio::test]
    async fn no_clobber_persist_preserves_an_existing_destination() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.part");
        let destination = directory.path().join("final.mp4");
        std::fs::write(&source, b"source").unwrap();
        std::fs::write(&destination, b"user file").unwrap();
        let gate = crate::attempt_gate::AttemptGate::new();
        let temporary = prepare_destination_copy(&source, &destination, &gate)
            .unwrap()
            .expect("temporary copy should complete");

        let error = persist_prepared_copy(temporary, &destination, &gate)
            .await
            .expect_err("an existing destination must win");

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&destination).unwrap(), b"user file");
        assert!(gate.is_active(), "a failed persist does not claim delivery");
        assert!(!gate.was_delivered());
    }

    #[test]
    fn dropping_the_async_owner_guard_discards_the_attempt() {
        let gate = crate::attempt_gate::AttemptGate::new();

        drop(DiscardOnDrop::new(Arc::clone(&gate)));

        assert!(
            !gate.is_active(),
            "dropping the owner cancels future copy work"
        );
    }

    #[test]
    fn discarded_gate_stops_the_copy_and_preserves_the_source() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.part");
        let destination = directory.path().join("final.mp4");
        std::fs::write(&source, vec![0x31; 1024 * 1024]).unwrap();
        let gate = crate::attempt_gate::AttemptGate::new();
        assert!(gate.discard());

        let temporary = prepare_destination_copy(&source, &destination, &gate)
            .expect("discard is a normal cancellation");

        assert!(temporary.is_none());
        assert!(!destination.exists());
        assert!(source.exists(), "staged source is retained for row cleanup");
        assert!(!gate.was_delivered());
    }

    #[test]
    fn cancellation_between_copy_chunks_drops_the_destination_temporary() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.part");
        std::fs::write(&source, vec![0x31; 1024 * 1024]).unwrap();
        let mut source_file = std::fs::File::open(&source).unwrap();
        let final_destination = directory.path().join("final.mp4");
        let mut temporary =
            crate::file_names::publish_temp_for(&final_destination).unwrap();
        let gate = crate::attempt_gate::AttemptGate::new();
        let mut destination = DiscardAfterFirstWrite {
            file: temporary.as_file_mut(),
            gate: Arc::clone(&gate),
            discarded: false,
        };

        let copied = copy_staged_bytes(&mut source_file, &mut destination, &gate)
            .expect("cancellation is not a storage error");

        assert!(
            !copied,
            "the copy must stop after the first discarded chunk"
        );
        assert!(destination.discarded);
        drop(destination);
        drop(temporary);

        assert!(!final_destination.exists());
        assert!(
            !gate.is_active(),
            "the attempt gate must stop the next copy chunk"
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        assert!(source.exists(), "the staged source must remain intact");
    }

    #[tokio::test]
    async fn dropping_pending_destination_restores_renamed_source_before_finalization() {
        let directory = tempfile::tempdir().expect("create publication root");
        let source = directory.path().join("source.part");
        let destination = directory.path().join("final.mp4");
        std::fs::write(&source, b"staged output").expect("write staged output");
        crate::file_names::rename_noreplace_same_device(&source, &destination)
            .expect("move staged output into pending destination");
        let gate = crate::attempt_gate::AttemptGate::new();
        let mut pending = PendingDestination::new(&destination, Arc::clone(&gate), Some(&source));
        pending.arm();

        drop(pending);
        gate.wait_for_rollbacks().await;

        assert!(source.exists(), "rollback restores the staging source");
        assert!(!destination.exists(), "rollback removes the published name");
        assert!(
            !gate.was_delivered(),
            "successful rollback is not a delivery"
        );
    }

    #[tokio::test]
    async fn dropping_pending_copy_destination_removes_it_before_finalization() {
        let directory = tempfile::tempdir().expect("create publication root");
        let destination = directory.path().join("final.mp4");
        std::fs::write(&destination, b"published orphan").expect("write destination");
        let gate = crate::attempt_gate::AttemptGate::new();
        let mut pending = PendingDestination::new(&destination, Arc::clone(&gate), None);
        pending.arm();

        drop(pending);
        gate.wait_for_rollbacks().await;

        assert!(
            !destination.exists(),
            "rollback removes the copied destination"
        );
        assert!(
            !gate.was_delivered(),
            "successful rollback is not a delivery"
        );
    }

    #[tokio::test]
    async fn failed_pending_destination_rollback_marks_the_orphan_delivered() {
        let directory = tempfile::tempdir().expect("create publication root");
        let source = directory.path().join("source.part");
        let destination = directory.path().join("final.mp4");
        std::fs::write(&source, b"existing source").expect("write source");
        std::fs::write(&destination, b"published orphan").expect("write destination");
        let gate = crate::attempt_gate::AttemptGate::new();
        let mut pending = PendingDestination::new(&destination, Arc::clone(&gate), Some(&source));
        pending.arm();

        drop(pending);
        gate.wait_for_rollbacks().await;

        assert!(
            gate.was_delivered(),
            "failed rollback must be visible to the finalizer"
        );
        assert!(
            destination.exists(),
            "a no-clobber rollback preserves the existing source"
        );
        assert_eq!(
            std::fs::read(&source).expect("read source"),
            b"existing source"
        );
    }
}
