//! Durable classification for live captures whose source continuity can no longer be trusted.

use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

const MARKER_NAME: &str = ".grab-live-unrecoverable.json";
const MARKER_VERSION: u32 = 1;

/// Structured failures from unrecoverable live-capture state persistence.
#[derive(Debug, Error)]
pub(crate) enum LiveCaptureStateError {
    #[error("the unrecoverable live-capture marker is invalid or unsupported")]
    InvalidMarker,
    #[error("the unrecoverable live-capture marker is not a regular file")]
    NotRegularFile,
    #[error("the unrecoverable live-capture marker exceeds its size limit")]
    TooLarge,
    #[error("the unrecoverable live-capture marker is malformed: {0}")]
    Malformed(#[source] serde_json::Error),
    #[error("a different unrecoverable live-capture decision already exists")]
    ConflictingDecision,
    #[error("live-capture state I/O failed while {operation}: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("live-capture state task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UnrecoverableLiveCapture {
    version: u32,
    pub(crate) protocol: String,
    pub(crate) reason: String,
    pub(crate) staged_bytes: u64,
}

impl UnrecoverableLiveCapture {
    fn validate(&self) -> Result<(), LiveCaptureStateError> {
        if self.version != MARKER_VERSION
            || !matches!(self.protocol.as_str(), "hls" | "dash")
            || self.reason.trim().is_empty()
            || self.reason.len() > 512
        {
            return Err(LiveCaptureStateError::InvalidMarker);
        }
        Ok(())
    }
}

/// Persist a terminal continuity decision before finalization can begin.
pub(crate) async fn persist(
    staging: &Path,
    protocol: &str,
    reason: &str,
    staged_bytes: u64,
) -> Result<(), LiveCaptureStateError> {
    let staging = staging.to_path_buf();
    let marker = UnrecoverableLiveCapture {
        version: MARKER_VERSION,
        protocol: protocol.to_string(),
        reason: reason.to_string(),
        staged_bytes,
    };
    marker.validate()?;
    tokio::task::spawn_blocking(move || persist_sync(&staging, &marker)).await?
}

fn persist_sync(
    staging: &Path,
    marker: &UnrecoverableLiveCapture,
) -> Result<(), LiveCaptureStateError> {
    use std::io::Write as _;

    let target = staging.join(MARKER_NAME);
    match std::fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(LiveCaptureStateError::NotRegularFile);
        }
        Ok(_) => {
            let existing = read_sync(staging)?;
            if existing == *marker {
                return Ok(());
            }
            return Err(LiveCaptureStateError::ConflictingDecision);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(LiveCaptureStateError::Io {
                operation: "inspecting the marker",
                source,
            });
        }
    }

    let mut temporary = tempfile::NamedTempFile::new_in(staging).map_err(|source| {
        LiveCaptureStateError::Io {
            operation: "creating the marker",
            source,
        }
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|source| LiveCaptureStateError::Io {
                operation: "securing the marker",
                source,
            })?;
    }
    serde_json::to_writer(&mut temporary, marker)
        .map_err(LiveCaptureStateError::Malformed)?;
    temporary
        .flush()
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|source| LiveCaptureStateError::Io {
            operation: "flushing the marker",
            source,
        })?;
    temporary
        .persist_noclobber(&target)
        .map_err(|error| LiveCaptureStateError::Io {
            operation: "publishing the marker",
            source: error.error,
        })?;
    std::fs::File::open(staging)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| LiveCaptureStateError::Io {
            operation: "syncing the staging directory",
            source,
        })?;
    Ok(())
}

pub(crate) async fn read(
    staging: &Path,
) -> Result<Option<UnrecoverableLiveCapture>, LiveCaptureStateError> {
    let staging = staging.to_path_buf();
    tokio::task::spawn_blocking(move || {
        match std::fs::symlink_metadata(staging.join(MARKER_NAME)) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                Err(LiveCaptureStateError::NotRegularFile)
            }
            Ok(_) => read_sync(&staging).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(LiveCaptureStateError::Io {
                operation: "inspecting the marker",
                source,
            }),
        }
    })
    .await?
}

fn read_sync(staging: &Path) -> Result<UnrecoverableLiveCapture, LiveCaptureStateError> {
    let bytes = std::fs::read(staging.join(MARKER_NAME)).map_err(|source| {
        LiveCaptureStateError::Io {
            operation: "reading the marker",
            source,
        }
    })?;
    if bytes.len() > 4096 {
        return Err(LiveCaptureStateError::TooLarge);
    }
    let marker: UnrecoverableLiveCapture =
        serde_json::from_slice(&bytes).map_err(LiveCaptureStateError::Malformed)?;
    marker.validate()?;
    Ok(marker)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn marker_is_durable_and_idempotent() {
        let directory = tempfile::tempdir().expect("create staging");
        let reason = "HLS playlist sequence continuity was lost";
        persist(directory.path(), "hls", reason, 1234)
            .await
            .expect("persist marker");
        persist(directory.path(), "hls", reason, 1234)
            .await
            .expect("repeat identical marker");
        let marker = read(directory.path())
            .await
            .expect("read marker")
            .expect("marker exists");
        assert_eq!(marker.protocol, "hls");
        assert_eq!(marker.reason, reason);
        assert_eq!(marker.staged_bytes, 1234);
    }

    #[tokio::test]
    async fn marker_rejects_conflicting_decisions() {
        let directory = tempfile::tempdir().expect("create staging");
        persist(directory.path(), "dash", "DASH timeline changed", 99)
            .await
            .expect("persist marker");
        assert!(
            persist(directory.path(), "dash", "different reason", 99)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn malformed_marker_fails_closed() {
        let directory = tempfile::tempdir().expect("create staging");
        std::fs::write(directory.path().join(MARKER_NAME), b"{").expect("write malformed marker");
        assert!(read(directory.path()).await.is_err());
    }
}
