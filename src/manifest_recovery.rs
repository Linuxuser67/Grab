//! Plan-bound recovery metadata for native manifest staging.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::media_types::{ManifestKind, ManifestSource};

const RECOVERY_FILE: &str = "manifest-recovery.json";
const RECOVERY_VERSION: u32 = 1;
const MAX_RECOVERY_MARKER_BYTES: usize = 4096;

/// Structured failures from manifest recovery metadata operations.
#[derive(Debug, Error)]
pub(crate) enum RecoveryError {
    #[error("could not encode manifest recovery plan: {0}")]
    Serialize(#[source] serde_json::Error),
    #[error("Manifest recovery metadata is invalid: {0}")]
    Malformed(#[source] serde_json::Error),
    #[error("Manifest recovery metadata version {0} is unsupported")]
    UnsupportedVersion(u32),
    #[error("manifest recovery metadata is not a regular file")]
    NotRegularFile,
    #[error("manifest recovery metadata exceeds its size limit")]
    TooLarge,
    #[error(
        "The selected manifest tracks or output container changed; existing staged data was preserved. Remove and add the download again to start a new plan."
    )]
    FingerprintMismatch,
    #[error(
        "Existing manifest staging data has no plan fingerprint; refusing unsafe recovery and preserving the data. Remove and add the download again to start a new plan."
    )]
    UnfingerprintedStagedData,
    #[error("manifest recovery I/O failed while {operation}: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("manifest recovery validation task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

#[derive(Serialize)]
struct FingerprintInput<'a> {
    version: u32,
    manifest_kind: &'static str,
    video_key: &'a str,
    audio_key: Option<&'a str>,
    subtitle_key: Option<&'a str>,
    subtitle_language: Option<&'a str>,
    output_container: &'a str,
    embed_subtitles: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RecoveryMarker {
    version: u32,
    fingerprint: String,
}

/// Fingerprint only stable plan inputs. The manifest URL is intentionally excluded:
/// signed URLs and credentials must never become durable recovery identifiers.
pub(crate) fn fingerprint(
    source: &ManifestSource,
    output_container: &str,
    embed_subtitles: bool,
) -> Result<String, RecoveryError> {
    let input = FingerprintInput {
        version: RECOVERY_VERSION,
        manifest_kind: match source.kind {
            ManifestKind::Hls => "hls",
            ManifestKind::Dash => "dash",
        },
        video_key: source.representation_key.as_str(),
        audio_key: source.audio_key.as_ref().map(|key| key.as_str()),
        subtitle_key: source.subtitle_key.as_ref().map(|key| key.as_str()),
        subtitle_language: source.subtitle_language.as_deref(),
        output_container,
        embed_subtitles,
    };
    let encoded = serde_json::to_vec(&input).map_err(RecoveryError::Serialize)?;
    let digest = Sha256::digest(encoded);
    let mut result = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(&mut result, "{byte:02x}");
    }
    Ok(result)
}

/// Validate an existing staging plan, or initialize metadata before any media
/// resources are written. Blocking filesystem work stays off the async executor.
pub(crate) async fn validate_or_initialize(
    staging: &Path,
    fingerprint: String,
) -> Result<(), RecoveryError> {
    let staging = staging.to_path_buf();
    tokio::task::spawn_blocking(move || validate_or_initialize_sync(&staging, &fingerprint)).await?
}

fn validate_or_initialize_sync(staging: &Path, fingerprint: &str) -> Result<(), RecoveryError> {
    let marker_path = staging.join(RECOVERY_FILE);
    match fs::symlink_metadata(&marker_path) {
        Ok(_) => validate_existing_marker(&marker_path, fingerprint),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if directory_has_staged_data(staging)? {
                return Err(RecoveryError::UnfingerprintedStagedData);
            }
            write_recovery_marker(staging, &marker_path, fingerprint)
        }
        Err(source) => Err(RecoveryError::Io {
            operation: "inspecting recovery metadata",
            source,
        }),
    }
}

fn validate_existing_marker(marker_path: &Path, fingerprint: &str) -> Result<(), RecoveryError> {
    let bytes =
        crate::bounded_file::read_regular_file_bounded(marker_path, MAX_RECOVERY_MARKER_BYTES)
            .map_err(|error| match error {
                crate::bounded_file::BoundedFileError::NotRegularFile => {
                    RecoveryError::NotRegularFile
                }
                crate::bounded_file::BoundedFileError::TooLarge => RecoveryError::TooLarge,
                crate::bounded_file::BoundedFileError::Io(source) => RecoveryError::Io {
                    operation: "reading recovery metadata",
                    source,
                },
            })?;
    let marker: RecoveryMarker =
        serde_json::from_slice(&bytes).map_err(RecoveryError::Malformed)?;
    if marker.version != RECOVERY_VERSION {
        return Err(RecoveryError::UnsupportedVersion(marker.version));
    }
    if marker.fingerprint != fingerprint {
        return Err(RecoveryError::FingerprintMismatch);
    }
    Ok(())
}

/// Only the staging ownership marker and empty directories are harmless when
/// adopting a fresh stage. Symlinks and special files are treated as data so
/// an unrecognized entry can never be silently overwritten or discarded.
fn directory_has_staged_data(directory: &Path) -> Result<bool, RecoveryError> {
    let entries = fs::read_dir(directory).map_err(|source| RecoveryError::Io {
        operation: "reading the staging directory",
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| RecoveryError::Io {
            operation: "reading a staging directory entry",
            source,
        })?;
        let name = entry.file_name();
        if name == ".grab-manifest-owner" || name == RECOVERY_FILE {
            continue;
        }
        let file_type = entry.file_type().map_err(|source| RecoveryError::Io {
            operation: "inspecting a staging directory entry",
            source,
        })?;
        if file_type.is_dir() {
            if directory_has_staged_data(&entry.path())? {
                return Ok(true);
            }
        } else {
            return Ok(true);
        }
    }
    Ok(false)
}

fn write_recovery_marker(
    staging: &Path,
    marker_path: &Path,
    fingerprint: &str,
) -> Result<(), RecoveryError> {
    let marker = RecoveryMarker {
        version: RECOVERY_VERSION,
        fingerprint: fingerprint.to_string(),
    };
    let contents = serde_json::to_vec(&marker).map_err(RecoveryError::Serialize)?;

    // NamedTempFile creates a private file in the same directory, so
    // persist_noclobber() publishes it atomically without replacing an existing marker.
    let mut temporary =
        tempfile::NamedTempFile::new_in(staging).map_err(|source| RecoveryError::Io {
            operation: "creating recovery metadata",
            source,
        })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|source| RecoveryError::Io {
                operation: "securing recovery metadata",
                source,
            })?;
    }
    temporary
        .write_all(&contents)
        .map_err(|source| RecoveryError::Io {
            operation: "writing recovery metadata",
            source,
        })?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|source| RecoveryError::Io {
            operation: "syncing recovery metadata",
            source,
        })?;
    match temporary.persist_noclobber(marker_path) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            // Another attempt may have initialized the same item concurrently.
            // Never replace its plan marker; accept only an exact match.
            validate_existing_marker(marker_path, fingerprint)?;
        }
        Err(error) => {
            return Err(RecoveryError::Io {
                operation: "publishing recovery metadata",
                source: error.error,
            });
        }
    }

    // Persist the directory entry before the runner starts committing media data.
    File::open(staging)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| RecoveryError::Io {
            operation: "syncing the staging directory",
            source,
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> ManifestSource {
        ManifestSource {
            url: "https://origin.example.test/master.m3u8?token=secret".to_string(),
            kind: ManifestKind::Hls,
            representation_key: "hls-variant-stable".into(),
            output_extension: None,
            audio_key: Some("hls-audio-stable".into()),
            subtitle_key: Some("hls-subtitle-stable".into()),
            subtitle_language: Some("en".to_string()),
        }
    }

    #[test]
    fn fingerprint_excludes_manifest_url_and_credentials() {
        let original = source();
        let mut refreshed = original.clone();
        refreshed.url = "https://cdn.example.test/master.m3u8?token=rotated".to_string();

        assert_eq!(
            fingerprint(&original, "mp4", true).expect("fingerprint original"),
            fingerprint(&refreshed, "mp4", true).expect("fingerprint refreshed")
        );
    }

    #[test]
    fn fingerprint_changes_for_selection_and_output_options() {
        let original = source();
        let baseline = fingerprint(&original, "mp4", true).expect("baseline fingerprint");

        let mut changed_video = original.clone();
        changed_video.representation_key = "hls-variant-other".into();
        assert_ne!(
            baseline,
            fingerprint(&changed_video, "mp4", true).expect("changed video fingerprint")
        );

        let mut changed_audio = original.clone();
        changed_audio.audio_key = None;
        assert_ne!(
            baseline,
            fingerprint(&changed_audio, "mp4", true).expect("changed audio fingerprint")
        );

        let mut changed_subtitle = original.clone();
        changed_subtitle.subtitle_key = None;
        assert_ne!(
            baseline,
            fingerprint(&changed_subtitle, "mp4", true).expect("changed subtitle fingerprint")
        );

        let mut changed_language = original.clone();
        changed_language.subtitle_language = Some("fr".to_string());
        assert_ne!(
            baseline,
            fingerprint(&changed_language, "mp4", true).expect("changed language fingerprint")
        );
        assert_ne!(
            baseline,
            fingerprint(&original, "mkv", true).expect("Matroska fingerprint")
        );
        assert_ne!(
            baseline,
            fingerprint(&original, "ts", true).expect("MPEG-TS fingerprint")
        );
        assert_ne!(
            baseline,
            fingerprint(&original, "mp4", false).expect("external subtitle fingerprint")
        );
    }

    #[tokio::test]
    async fn fresh_owned_staging_directory_can_be_initialized() {
        let staging = tempfile::tempdir().expect("create staging directory");
        fs::write(staging.path().join(".grab-manifest-owner"), b"owned")
            .expect("write staging ownership marker");
        fs::create_dir(staging.path().join("video")).expect("create empty video directory");
        let expected = fingerprint(&source(), "mp4", true).expect("fingerprint");

        validate_or_initialize(staging.path(), expected)
            .await
            .expect("initialize a fresh owned staging directory");
    }

    #[tokio::test]
    async fn matching_fingerprint_allows_staged_data_to_be_reused() {
        let staging = tempfile::tempdir().expect("create staging directory");
        let expected = fingerprint(&source(), "mp4", true).expect("fingerprint");

        validate_or_initialize(staging.path(), expected.clone())
            .await
            .expect("initialize recovery marker");
        fs::write(staging.path().join("video-track.part"), b"committed")
            .expect("write staged resource");
        validate_or_initialize(staging.path(), expected)
            .await
            .expect("validate matching recovery marker");
    }

    #[tokio::test]
    async fn changed_fingerprint_rejects_recovery_without_deleting_staged_data() {
        let staging = tempfile::tempdir().expect("create staging directory");
        let original = fingerprint(&source(), "mp4", true).expect("fingerprint");
        validate_or_initialize(staging.path(), original)
            .await
            .expect("initialize recovery marker");
        let resource = staging.path().join("video-track.part");
        fs::write(&resource, b"preserve me").expect("write staged resource");

        let changed = fingerprint(&source(), "mkv", true).expect("changed fingerprint");
        assert!(matches!(
            validate_or_initialize(staging.path(), changed).await,
            Err(RecoveryError::FingerprintMismatch)
        ));
        assert_eq!(
            fs::read(resource).expect("read preserved resource"),
            b"preserve me"
        );
    }

    #[tokio::test]
    async fn legacy_staging_without_fingerprint_is_rejected_and_preserved() {
        let staging = tempfile::tempdir().expect("create staging directory");
        let resource = staging.path().join("video-track.part");
        fs::write(&resource, b"legacy data").expect("write legacy resource");
        let expected = fingerprint(&source(), "mp4", true).expect("fingerprint");

        assert!(matches!(
            validate_or_initialize(staging.path(), expected).await,
            Err(RecoveryError::UnfingerprintedStagedData)
        ));
        assert_eq!(
            fs::read(resource).expect("read preserved resource"),
            b"legacy data"
        );
    }

    #[tokio::test]
    async fn oversized_recovery_metadata_is_rejected() {
        let staging = tempfile::tempdir().expect("create staging directory");
        fs::write(
            staging.path().join(RECOVERY_FILE),
            vec![b' '; MAX_RECOVERY_MARKER_BYTES + 1],
        )
        .expect("write oversized recovery marker");
        let expected = fingerprint(&source(), "mp4", true).expect("fingerprint");

        assert!(matches!(
            validate_or_initialize(staging.path(), expected).await,
            Err(RecoveryError::TooLarge)
        ));
    }

    #[tokio::test]
    async fn malformed_recovery_metadata_is_rejected() {
        let staging = tempfile::tempdir().expect("create staging directory");
        fs::write(staging.path().join(RECOVERY_FILE), b"{").expect("write malformed metadata");
        let expected = fingerprint(&source(), "mp4", true).expect("fingerprint");

        assert!(matches!(
            validate_or_initialize(staging.path(), expected).await,
            Err(RecoveryError::Malformed(_))
        ));
    }
}
