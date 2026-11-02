//! Durable classification for live captures whose source continuity can no longer be trusted.

use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::media_types::ManifestKind;

const MARKER_NAME: &str = ".grab-live-unrecoverable.json";
const MARKER_VERSION: u32 = 1;
const MAX_MARKER_BYTES: usize = 4096;

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
    #[error("could not serialize unrecoverable live-capture state: {0}")]
    Serialize(#[source] serde_json::Error),
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
    #[serde(with = "manifest_protocol")]
    pub(crate) protocol: ManifestKind,
    pub(crate) reason: String,
    pub(crate) staged_bytes: u64,
}

mod manifest_protocol {
    use crate::media_types::ManifestKind;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S>(protocol: &ManifestKind, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(match protocol {
            ManifestKind::Hls => "hls",
            ManifestKind::Dash => "dash",
        })
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<ManifestKind, D::Error>
    where
        D: Deserializer<'de>,
    {
        match String::deserialize(deserializer)?.as_str() {
            "hls" => Ok(ManifestKind::Hls),
            "dash" => Ok(ManifestKind::Dash),
            protocol => Err(serde::de::Error::unknown_variant(
                protocol,
                &["hls", "dash"],
            )),
        }
    }
}

impl UnrecoverableLiveCapture {
    fn validate(&self) -> Result<(), LiveCaptureStateError> {
        if self.version != MARKER_VERSION
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
    protocol: ManifestKind,
    reason: &str,
    staged_bytes: u64,
) -> Result<(), LiveCaptureStateError> {
    let staging = staging.to_path_buf();
    let marker = UnrecoverableLiveCapture {
        version: MARKER_VERSION,
        protocol,
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

    let mut temporary =
        tempfile::NamedTempFile::new_in(staging).map_err(|source| LiveCaptureStateError::Io {
            operation: "creating the marker",
            source,
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
    serde_json::to_writer(&mut temporary, marker).map_err(LiveCaptureStateError::Serialize)?;
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
    let bytes = crate::bounded_file::read_regular_file_bounded(
        &staging.join(MARKER_NAME),
        MAX_MARKER_BYTES,
    )
    .map_err(|error| match error {
        crate::bounded_file::BoundedFileError::NotRegularFile => {
            LiveCaptureStateError::NotRegularFile
        }
        crate::bounded_file::BoundedFileError::TooLarge => LiveCaptureStateError::TooLarge,
        crate::bounded_file::BoundedFileError::Io(source) => LiveCaptureStateError::Io {
            operation: "reading the marker",
            source,
        },
    })?;
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
        persist(directory.path(), ManifestKind::Hls, reason, 1234)
            .await
            .expect("persist marker");
        persist(directory.path(), ManifestKind::Hls, reason, 1234)
            .await
            .expect("repeat identical marker");
        let marker = read(directory.path())
            .await
            .expect("read marker")
            .expect("marker exists");
        assert_eq!(marker.protocol, ManifestKind::Hls);
        assert_eq!(marker.reason, reason);
        assert_eq!(marker.staged_bytes, 1234);
    }

    #[test]
    fn protocol_marker_keeps_the_legacy_lowercase_wire_format() {
        let marker = UnrecoverableLiveCapture {
            version: MARKER_VERSION,
            protocol: ManifestKind::Hls,
            reason: "HLS continuity was lost".to_string(),
            staged_bytes: 42,
        };

        let encoded = serde_json::to_value(&marker).expect("serialize marker");
        assert_eq!(encoded["protocol"], "hls");
        assert_eq!(
            serde_json::from_value::<UnrecoverableLiveCapture>(encoded)
                .expect("deserialize marker")
                .protocol,
            ManifestKind::Hls
        );
        assert!(
            serde_json::from_value::<UnrecoverableLiveCapture>(serde_json::json!({
                "version": MARKER_VERSION,
                "protocol": "unknown",
                "reason": "HLS continuity was lost",
                "staged_bytes": 42
            }))
            .is_err(),
            "unknown protocols must not be accepted"
        );
    }

    #[tokio::test]
    async fn marker_rejects_conflicting_decisions() {
        let directory = tempfile::tempdir().expect("create staging");
        persist(
            directory.path(),
            ManifestKind::Dash,
            "DASH timeline changed",
            99,
        )
        .await
        .expect("persist marker");
        assert!(matches!(
            persist(directory.path(), ManifestKind::Dash, "different reason", 99).await,
            Err(LiveCaptureStateError::ConflictingDecision)
        ));
    }

    #[tokio::test]
    async fn oversized_marker_is_rejected_before_deserialization() {
        let directory = tempfile::tempdir().expect("create staging");
        std::fs::write(
            directory.path().join(MARKER_NAME),
            vec![b' '; MAX_MARKER_BYTES + 1],
        )
        .expect("write oversized marker");

        assert!(matches!(
            read(directory.path()).await,
            Err(LiveCaptureStateError::TooLarge)
        ));
    }

    #[tokio::test]
    async fn malformed_marker_fails_closed() {
        let directory = tempfile::tempdir().expect("create staging");
        std::fs::write(directory.path().join(MARKER_NAME), b"{").expect("write malformed marker");
        assert!(matches!(
            read(directory.path()).await,
            Err(LiveCaptureStateError::Malformed(_))
        ));
    }
}
