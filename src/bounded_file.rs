//! Bounded reads for private metadata files.

use std::fs::{self, OpenOptions};
use std::io::{self, Read as _};
use std::path::Path;

/// Why a bounded regular-file read was rejected.
#[derive(Debug, thiserror::Error)]
pub(crate) enum BoundedFileError {
    #[error("path does not name a regular file")]
    NotRegularFile,
    #[error("file exceeds the configured size limit")]
    TooLarge,
    #[error("file read failed: {0}")]
    Io(#[source] io::Error),
}

/// Read a regular file without following symlinks or allowing an unbounded read.
///
/// The path is checked before opening to reject special files cheaply. The open
/// uses O_NOFOLLOW | O_NONBLOCK, and the opened descriptor is checked again to
/// close replacement races (including FIFO replacement). Reading is limited to
/// one byte beyond the limit so concurrent file growth is detected.
pub(crate) fn read_regular_file_bounded(
    path: &Path,
    max_bytes: usize,
) -> Result<Vec<u8>, BoundedFileError> {
    let max_bytes = u64::try_from(max_bytes).map_err(|_| BoundedFileError::TooLarge)?;
    let path_metadata = fs::symlink_metadata(path).map_err(BoundedFileError::Io)?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Err(BoundedFileError::NotRegularFile);
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NOCTTY | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|source| {
        #[cfg(unix)]
        let is_symlink = source.raw_os_error() == Some(libc::ELOOP);
        #[cfg(not(unix))]
        let is_symlink = false;

        if is_symlink {
            BoundedFileError::NotRegularFile
        } else {
            BoundedFileError::Io(source)
        }
    })?;

    let metadata = file.metadata().map_err(BoundedFileError::Io)?;
    if !metadata.file_type().is_file() {
        return Err(BoundedFileError::NotRegularFile);
    }
    if metadata.len() > max_bytes {
        return Err(BoundedFileError::TooLarge);
    }

    let capacity = usize::try_from(metadata.len()).map_err(|_| BoundedFileError::TooLarge)?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(BoundedFileError::Io)?;
    if u64::try_from(bytes.len()).map_err(|_| BoundedFileError::TooLarge)? > max_bytes {
        return Err(BoundedFileError::TooLarge);
    }

    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_regular_file_within_the_limit() {
        let directory = tempfile::tempdir().expect("create test directory");
        let path = directory.path().join("marker.json");
        fs::write(&path, b"ok").expect("write marker");

        assert_eq!(
            read_regular_file_bounded(&path, 32)
                .expect("read marker")
                .as_slice(),
            b"ok"
        );
    }

    #[test]
    fn rejects_a_file_over_the_limit() {
        let directory = tempfile::tempdir().expect("create test directory");
        let path = directory.path().join("marker.json");
        fs::write(&path, b"12345").expect("write marker");

        assert!(matches!(
            read_regular_file_bounded(&path, 4),
            Err(BoundedFileError::TooLarge)
        ));
    }

    #[test]
    fn rejects_directories() {
        let directory = tempfile::tempdir().expect("create test directory");

        assert!(matches!(
            read_regular_file_bounded(directory.path(), 32),
            Err(BoundedFileError::NotRegularFile)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks() {
        let directory = tempfile::tempdir().expect("create test directory");
        let target = directory.path().join("target.json");
        let link = directory.path().join("marker.json");
        fs::write(&target, b"{}").expect("write target");
        std::os::unix::fs::symlink(&target, &link).expect("create symlink");

        assert!(matches!(
            read_regular_file_bounded(&link, 32),
            Err(BoundedFileError::NotRegularFile)
        ));
    }
}
