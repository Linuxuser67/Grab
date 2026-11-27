//! Atomic publication of small files inside an owned staging directory.
//!
//! Both the native manifest runner and optional caption intake use this
//! primitive. Keeping it below both consumers avoids a module dependency cycle.

use std::path::Path;

/// Whether an existing regular target may be atomically replaced.
#[derive(Clone, Copy)]
pub(crate) enum StagedPublish {
    Replace,
    NoReplace,
}

/// Atomically write bytes under a staging directory.
///
/// The temporary file is created exclusively and flushed before publication.
/// Symlink/non-file targets are rejected; no-replace publication never clobbers
/// an existing target.
pub(crate) async fn write_staged_file_atomic(
    directory: &Path,
    temporary_name: &str,
    target_name: &str,
    bytes: &[u8],
    publish: StagedPublish,
) -> Result<(), String> {
    use tokio::io::AsyncWriteExt as _;

    let temporary = directory.join(temporary_name);
    let target = directory.join(target_name);
    remove_regular_file_if_present(&temporary).await?;

    let result = async {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .await
            .map_err(|error| format!("Could not create staged file {target_name}: {error}"))?;
        file.write_all(bytes)
            .await
            .map_err(|error| format!("Could not write staged file {target_name}: {error}"))?;
        file.sync_all()
            .await
            .map_err(|error| format!("Could not flush staged file {target_name}: {error}"))?;
        drop(file);

        match publish {
            StagedPublish::Replace => {
                match tokio::fs::symlink_metadata(&target).await {
                    Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                        return Err(format!("Staged target {target_name} is not a regular file"));
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!(
                            "Could not inspect staged target {target_name}: {error}"
                        ));
                    }
                }
                tokio::fs::rename(&temporary, &target)
                    .await
                    .map_err(|error| {
                        format!("Could not publish staged file {target_name}: {error}")
                    })
            }
            StagedPublish::NoReplace => crate::file_names::rename_noreplace(&temporary, &target)
                .map_err(|error| format!("Could not publish staged file {target_name}: {error}")),
        }
    }
    .await;

    if result.is_err() {
        let _ = remove_regular_file_if_present(&temporary).await;
    }
    result
}

async fn remove_regular_file_if_present(path: &Path) -> Result<(), String> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err("A staged path is not a regular file".to_string())
        }
        Ok(_) => tokio::fs::remove_file(path)
            .await
            .map_err(|error| format!("Could not remove staged file: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Could not inspect staged file: {error}")),
    }
}
