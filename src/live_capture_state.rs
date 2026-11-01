//! Durable classification for live captures whose source continuity can no longer be trusted.

use std::path::Path;

use serde::{Deserialize, Serialize};

const MARKER_NAME: &str = ".grab-live-unrecoverable.json";
const MARKER_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UnrecoverableLiveCapture {
    version: u32,
    pub(crate) protocol: String,
    pub(crate) reason: String,
    pub(crate) staged_bytes: u64,
}

impl UnrecoverableLiveCapture {
    fn validate(&self) -> Result<(), String> {
        if self.version != MARKER_VERSION
            || !matches!(self.protocol.as_str(), "hls" | "dash")
            || self.reason.trim().is_empty()
            || self.reason.len() > 512
        {
            return Err("The unrecoverable live-capture marker is invalid or unsupported".to_string());
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
) -> Result<(), String> {
    let staging = staging.to_path_buf();
    let marker = UnrecoverableLiveCapture {
        version: MARKER_VERSION,
        protocol: protocol.to_string(),
        reason: reason.to_string(),
        staged_bytes,
    };
    marker.validate()?;
    tokio::task::spawn_blocking(move || persist_sync(&staging, &marker))
        .await
        .map_err(|error| format!("Could not persist live-capture state: {error}"))?
}

fn persist_sync(staging: &Path, marker: &UnrecoverableLiveCapture) -> Result<(), String> {
    use std::io::Write as _;

    let target = staging.join(MARKER_NAME);
    match std::fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err("The unrecoverable live-capture marker is not a regular file".to_string());
        }
        Ok(_) => {
            let existing = read_sync(staging)?;
            if existing == *marker {
                return Ok(());
            }
            return Err("A different unrecoverable live-capture decision already exists".to_string());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("Could not inspect live-capture state: {error}")),
    }

    let mut temporary = tempfile::NamedTempFile::new_in(staging)
        .map_err(|error| format!("Could not create live-capture state: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("Could not secure live-capture state: {error}"))?;
    }
    serde_json::to_writer(&mut temporary, marker)
        .map_err(|error| format!("Could not serialize live-capture state: {error}"))?;
    temporary
        .flush()
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|error| format!("Could not flush live-capture state: {error}"))?;
    temporary
        .persist_noclobber(&target)
        .map_err(|error| format!("Could not publish live-capture state: {error}"))?;
    std::fs::File::open(staging)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("Could not sync live-capture state directory: {error}"))?;
    Ok(())
}

pub(crate) async fn read(staging: &Path) -> Result<Option<UnrecoverableLiveCapture>, String> {
    let staging = staging.to_path_buf();
    tokio::task::spawn_blocking(move || match std::fs::symlink_metadata(staging.join(MARKER_NAME)) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err("The unrecoverable live-capture marker is not a regular file".to_string())
        }
        Ok(_) => read_sync(&staging).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("Could not inspect live-capture state: {error}")),
    })
    .await
    .map_err(|error| format!("Could not read live-capture state: {error}"))?
}

fn read_sync(staging: &Path) -> Result<UnrecoverableLiveCapture, String> {
    let bytes = std::fs::read(staging.join(MARKER_NAME))
        .map_err(|error| format!("Could not read live-capture state: {error}"))?;
    if bytes.len() > 4096 {
        return Err("The unrecoverable live-capture marker exceeds its size limit".to_string());
    }
    let marker: UnrecoverableLiveCapture = serde_json::from_slice(&bytes)
        .map_err(|error| format!("The unrecoverable live-capture marker is malformed: {error}"))?;
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
        assert!(persist(directory.path(), "dash", "different reason", 99)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn malformed_marker_fails_closed() {
        let directory = tempfile::tempdir().expect("create staging");
        std::fs::write(directory.path().join(MARKER_NAME), b"{")
            .expect("write malformed marker");
        assert!(read(directory.path()).await.is_err());
    }
}
