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

/// Removes a destination created by this attempt unless its gate claims delivery.
struct PendingDestination {
    path: PathBuf,
    gate: Arc<crate::attempt_gate::AttemptGate>,
    armed: bool,
}

impl PendingDestination {
    fn new(path: &Path, gate: Arc<crate::attempt_gate::AttemptGate>) -> Self {
        Self {
            path: path.to_path_buf(),
            gate,
            armed: false,
        }
    }

    fn arm(&mut self) {
        self.armed = true;
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingDestination {
    fn drop(&mut self) {
        if self.armed
            && let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %self.path.display(),
                error = %error,
                "could not roll back an uncommitted manifest destination"
            );
            // The discard finalizer checks this after joining the async owner.
            self.gate.mark_delivered();
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
    let parent = destination.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "destination has no parent directory",
        )
    })?;
    let mut source_file = std::fs::File::open(source)?;
    let permissions = source_file.metadata()?.permissions();
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
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

/// Persist a fully synced temporary and claim delivery only after its final name exists.
fn persist_prepared_copy(
    temporary: tempfile::NamedTempFile,
    destination: &Path,
    gate: &Arc<crate::attempt_gate::AttemptGate>,
) -> std::io::Result<ManifestPublishOutcome> {
    let mut pending = PendingDestination::new(destination, Arc::clone(gate));
    let published_file = temporary
        .persist_noclobber(destination)
        .map_err(|error| error.error)?;
    drop(published_file);

    pending.arm();
    if !gate.try_commit() {
        return Ok(ManifestPublishOutcome::Aborted);
    }

    gate.mark_delivered();
    pending.disarm();
    Ok(ManifestPublishOutcome::Published)
}

/// Commit a same-filesystem rename or link after installing a rollback guard.
fn commit_renamed_destination(
    gate: &Arc<crate::attempt_gate::AttemptGate>,
    pending: &mut PendingDestination,
) -> ManifestPublishOutcome {
    pending.arm();
    if !gate.try_commit() {
        return ManifestPublishOutcome::Aborted;
    }

    gate.mark_delivered();
    pending.disarm();
    ManifestPublishOutcome::Published
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
            let mut pending = PendingDestination::new(destination, Arc::clone(gate));
            return Ok(commit_renamed_destination(gate, &mut pending));
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

    let outcome = persist_prepared_copy(temporary, destination, gate)?;
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
    fn destination_local_copy_is_private_until_no_clobber_persist() {
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

        let outcome = persist_prepared_copy(temporary, &destination, &gate).unwrap();
        assert_eq!(outcome, ManifestPublishOutcome::Published);
        assert!(gate.was_delivered());
        assert_eq!(std::fs::read(&destination).unwrap(), contents);
        assert!(
            source.exists(),
            "the source remains for normal staging cleanup"
        );
    }

    #[test]
    fn no_clobber_persist_preserves_an_existing_destination() {
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
        let mut temporary = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
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

        assert!(!directory.path().join("final.mp4").exists());
        assert!(
            !gate.is_active(),
            "the attempt gate must stop the next copy chunk"
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        assert!(source.exists(), "the staged source must remain intact");
    }
}
