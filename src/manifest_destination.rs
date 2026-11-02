//! Destination-side path planning for native manifest track parts.
//!
//! Media bytes are moved here by the ordered writer in a follow-up change. This
//! module records exact filenames in XDG state before creating visible files;
//! the plan itself does not authorize cleanup of a destination-side file.

use std::fs::{self, File};
use std::io::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

const PLAN_FILE: &str = "manifest-destination.json";
const PLAN_VERSION: u32 = 1;
const MAX_PLAN_BYTES: usize = 16 * 1024;

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct DestinationPlan {
    version: u32,
    item_id: u64,
    destination_key: String,
    parts: Vec<DestinationPart>,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct DestinationPart {
    track: TrackKind,
    file_name: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum TrackKind {
    Video,
    Audio,
    Subtitles,
}

/// Persist the exact destination-side part names before any media files are
/// created. The XDG state directory is already bound to this item and
/// destination by its owner marker.
///
/// # Errors
///
/// Returns an error if the destination or staging directory is invalid, the
/// plan is malformed or belongs to another selection, or metadata cannot be
/// durably written.
pub(crate) fn ensure_plan(
    staging: &Path,
    destination: &Path,
    item_id: u64,
    has_audio: bool,
    has_subtitles: bool,
) -> Result<(), String> {
    let destination_dir = destination
        .parent()
        .ok_or_else(|| "The download destination has no parent directory".to_string())?;
    let canonical_destination_dir = fs::canonicalize(destination_dir).map_err(|error| {
        format!(
            "Could not resolve destination directory {}: {error}",
            destination_dir.display()
        )
    })?;
    let destination_key = crate::video_staging::dest_key(&canonical_destination_dir);
    let prefix = crate::video_argv::unified_staging_prefix(destination, item_id);

    let mut parts = vec![DestinationPart {
        track: TrackKind::Video,
        file_name: format!("{prefix}video.part"),
    }];
    if has_audio {
        parts.push(DestinationPart {
            track: TrackKind::Audio,
            file_name: format!("{prefix}audio.part"),
        });
    }
    if has_subtitles {
        parts.push(DestinationPart {
            track: TrackKind::Subtitles,
            file_name: format!("{prefix}subtitles.part"),
        });
    }

    if parts.iter().any(|part| !safe_file_name(&part.file_name)) {
        return Err("The manifest destination plan contains an unsafe filename".to_string());
    }

    let plan = DestinationPlan {
        version: PLAN_VERSION,
        item_id,
        destination_key,
        parts,
    };
    let path = staging.join(PLAN_FILE);
    match read_plan(&path) {
        Ok(Some(existing)) if existing == plan => return sync_staging_directory(staging),
        Ok(Some(_)) => {
            return Err(
                "The manifest destination files belong to a different download plan; existing data was preserved"
                    .to_string(),
            );
        }
        Ok(None) => {}
        Err(error) => return Err(error),
    }

    write_plan(staging, &path, &plan)
}

fn safe_file_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && Path::new(name).file_name().and_then(|file_name| file_name.to_str()) == Some(name)
        && !name.chars().any(char::is_control)
}

fn read_plan(path: &Path) -> Result<Option<DestinationPlan>, String> {
    let bytes = match crate::bounded_file::read_regular_file_bounded(path, MAX_PLAN_BYTES) {
        Ok(bytes) => bytes,
        Err(crate::bounded_file::BoundedFileError::Io(error))
            if error.kind() == std::io::ErrorKind::NotFound =>
        {
            return Ok(None);
        }
        Err(error) => {
            return Err(format!(
                "Could not safely read manifest destination metadata {}: {error}",
                path.display()
            ));
        }
    };
    let plan = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "Manifest destination metadata {} is invalid: {error}",
            path.display()
        )
    })?;
    Ok(Some(plan))
}

fn write_plan(staging: &Path, path: &Path, plan: &DestinationPlan) -> Result<(), String> {
    let contents = serde_json::to_vec(plan)
        .map_err(|error| format!("Could not encode manifest destination metadata: {error}"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(staging)
        .map_err(|error| format!("Could not create manifest destination metadata: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("Could not secure manifest destination metadata: {error}"))?;
    }
    temporary
        .write_all(&contents)
        .map_err(|error| format!("Could not write manifest destination metadata: {error}"))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| format!("Could not sync manifest destination metadata: {error}"))?;
    match temporary.persist_noclobber(path) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            return match read_plan(path)? {
                Some(existing) if existing == *plan => sync_staging_directory(staging),
                _ => Err(
                    "The manifest destination plan changed during initialization; existing data was preserved"
                        .to_string(),
                ),
            };
        }
        Err(error) => {
            return Err(format!(
                "Could not publish manifest destination metadata {}: {}",
                path.display(),
                error.error
            ));
        }
    }
    sync_staging_directory(staging)
}

fn sync_staging_directory(staging: &Path) -> Result<(), String> {
    File::open(staging)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("Could not sync manifest staging directory: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_records_visible_track_part_names() {
        let destination_dir = tempfile::tempdir().expect("create destination");
        let staging = tempfile::tempdir().expect("create staging");
        let destination = destination_dir.path().join("Example video.mp4");

        ensure_plan(staging.path(), &destination, 42, true, true)
            .expect("write destination plan");

        let plan = read_plan(&staging.path().join(PLAN_FILE))
            .expect("read destination plan")
            .expect("destination plan exists");
        assert_eq!(plan.item_id, 42);
        assert_eq!(
            plan.parts
                .iter()
                .map(|part| part.file_name.as_str())
                .collect::<Vec<_>>(),
            [
                "Example video.42.video.part",
                "Example video.42.audio.part",
                "Example video.42.subtitles.part",
            ]
        );
    }

    #[test]
    fn matching_plan_is_idempotent_and_changed_plan_is_preserved() {
        let destination_dir = tempfile::tempdir().expect("create destination");
        let staging = tempfile::tempdir().expect("create staging");
        let destination = destination_dir.path().join("Example.mp4");

        ensure_plan(staging.path(), &destination, 7, false, false)
            .expect("write initial plan");
        let path = staging.path().join(PLAN_FILE);
        let before = fs::read(&path).expect("read initial metadata");

        ensure_plan(staging.path(), &destination, 7, false, false)
            .expect("matching plan is accepted");
        assert!(ensure_plan(staging.path(), &destination, 7, true, false).is_err());
        assert_eq!(fs::read(path).expect("read preserved metadata"), before);
    }

    #[cfg(unix)]
    #[test]
    fn unix_filename_backslashes_remain_valid() {
        let destination_dir = tempfile::tempdir().expect("create destination");
        let staging = tempfile::tempdir().expect("create staging");
        let destination = destination_dir.path().join("Example\\video.mp4");

        ensure_plan(staging.path(), &destination, 43, false, false)
            .expect("a backslash is valid in a Unix filename");

        let plan = read_plan(&staging.path().join(PLAN_FILE))
            .expect("read destination plan")
            .expect("destination plan exists");
        assert_eq!(plan.parts[0].file_name, "Example\\video.43.video.part");
    }

    #[cfg(unix)]
    #[test]
    fn destination_metadata_symlink_is_never_followed() {
        let destination_dir = tempfile::tempdir().expect("create destination");
        let staging = tempfile::tempdir().expect("create staging");
        let victim = staging.path().join("victim");
        fs::write(&victim, b"preserve").expect("write victim");
        std::os::unix::fs::symlink(&victim, staging.path().join(PLAN_FILE))
            .expect("create metadata symlink");

        assert!(ensure_plan(
            staging.path(),
            &destination_dir.path().join("Example.mp4"),
            7,
            false,
            false,
        )
        .is_err());
        assert_eq!(fs::read(victim).expect("read victim"), b"preserve");
    }
}
