//! Destination-side path planning for native manifest track parts.
//!
//! Exact destination-side part names and file ownership are persisted in XDG
//! state. A filename plan alone never authorizes opening or removing a file.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

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

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TrackKind {
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
        && Path::new(name)
            .file_name()
            .and_then(|file_name| file_name.to_str())
            == Some(name)
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

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct PartOwner {
    version: u32,
    item_id: u64,
    destination_key: String,
    file_name: String,
    device: u64,
    inode: u64,
}

/// Resolve one planned destination-side part without creating or modifying it.
pub(crate) fn part_path(
    staging: &Path,
    destination: &Path,
    item_id: u64,
    track: TrackKind,
) -> Result<PathBuf, String> {
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
    let plan = read_plan(&staging.join(PLAN_FILE))?
        .ok_or_else(|| "The manifest destination plan is missing".to_string())?;
    if plan.version != PLAN_VERSION
        || plan.item_id != item_id
        || plan.destination_key != destination_key
    {
        return Err("The manifest destination plan belongs to a different download".to_string());
    }
    let part = plan
        .parts
        .iter()
        .find(|part| part.track == track)
        .ok_or_else(|| {
            "The requested manifest track is absent from the destination plan".to_string()
        })?;
    if !safe_file_name(&part.file_name) {
        return Err("The manifest destination plan contains an unsafe filename".to_string());
    }
    Ok(canonical_destination_dir.join(&part.file_name))
}

/// Open the planned part only when its persisted ownership record proves that
/// it is the same regular file Grab created for this item and destination.
///
/// A file created without a durable owner record is deliberately treated as a
/// conflict. It is never adopted, truncated, or removed automatically.
pub(crate) fn open_owned_part(
    staging: &Path,
    destination: &Path,
    item_id: u64,
    track: TrackKind,
) -> Result<File, String> {
    let path = part_path(staging, destination, item_id, track)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "The manifest part filename is invalid".to_string())?;
    let owner_path = staging.join(format!("{file_name}.owner.json"));
    let destination_dir = destination
        .parent()
        .ok_or_else(|| "The download destination has no parent directory".to_string())?;
    let canonical_destination_dir = fs::canonicalize(destination_dir)
        .map_err(|error| format!("Could not resolve destination directory: {error}"))?;
    let destination_key = crate::video_staging::dest_key(&canonical_destination_dir);

    match read_part_owner(&owner_path)? {
        Some(owner) => {
            if owner.version != 1
                || owner.item_id != item_id
                || owner.destination_key != destination_key
                || owner.file_name != file_name
            {
                return Err(
                    "The manifest part ownership record does not match this download".to_string(),
                );
            }
            match fs::symlink_metadata(&path) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() || !metadata.is_file() {
                        return Err(
                            "The manifest part is not a regular file; existing data was preserved"
                                .to_string(),
                        );
                    }
                    let identity = metadata_identity(&metadata)?;
                    if identity.device != owner.device || identity.inode != owner.inode {
                        return Err("The manifest part was replaced after Grab created it; existing data was preserved".to_string());
                    }
                    let file = open_part_no_follow(&path)?;
                    let opened_identity = file_identity(&file)?;
                    if opened_identity.device != owner.device
                        || opened_identity.inode != owner.inode
                    {
                        return Err("The manifest part changed while it was being opened; existing data was preserved".to_string());
                    }
                    Ok(file)
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    let file = create_part_file(&path)?;
                    let identity = file_identity(&file)?;
                    let replacement = PartOwner {
                        version: 1,
                        item_id,
                        destination_key,
                        file_name: file_name.to_string(),
                        device: identity.device,
                        inode: identity.inode,
                    };
                    write_part_owner(staging, &owner_path, &replacement, true)?;
                    Ok(file)
                }
                Err(error) => Err(format!(
                    "Could not inspect destination manifest part: {error}"
                )),
            }
        }
        None => {
            let file = create_part_file(&path)?;
            let identity = file_identity(&file)?;
            let owner = PartOwner {
                version: 1,
                item_id,
                destination_key,
                file_name: file_name.to_string(),
                device: identity.device,
                inode: identity.inode,
            };
            write_part_owner(staging, &owner_path, &owner, false)?;
            Ok(file)
        }
    }
}

/// Remove a destination part only while its inode still matches Grab's
/// durable ownership record. A missing file is an idempotent cleanup result.
///
/// # Errors
///
/// Returns an error if ownership cannot be proven or filesystem cleanup fails.
pub(crate) fn remove_owned_part(
    staging: &Path,
    destination: &Path,
    item_id: u64,
    track: TrackKind,
) -> Result<(), String> {
    let path = part_path(staging, destination, item_id, track)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "The manifest part filename is invalid".to_string())?;
    let owner_path = staging.join(format!("{file_name}.owner.json"));
    let Some(owner) = read_part_owner(&owner_path)? else {
        return Err("The manifest part has no ownership record; it was preserved".to_string());
    };
    let destination_dir = destination
        .parent()
        .ok_or_else(|| "The download destination has no parent directory".to_string())?;
    let canonical_destination_dir = fs::canonicalize(destination_dir)
        .map_err(|error| format!("Could not resolve destination directory: {error}"))?;
    let destination_key = crate::video_staging::dest_key(&canonical_destination_dir);
    if owner.version != 1
        || owner.item_id != item_id
        || owner.destination_key != destination_key
        || owner.file_name != file_name
    {
        return Err("The manifest part ownership record does not match this download".to_string());
    }
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(
                    "The manifest part was replaced; existing data was preserved".to_string(),
                );
            }
            let identity = metadata_identity(&metadata)?;
            if identity.device != owner.device || identity.inode != owner.inode {
                return Err(
                    "The manifest part was replaced; existing data was preserved".to_string(),
                );
            }
            fs::remove_file(&path).map_err(|error| {
                format!(
                    "Could not remove owned manifest part {}: {error}",
                    path.display()
                )
            })?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("Could not inspect owned manifest part: {error}")),
    }
    fs::remove_file(&owner_path)
        .map_err(|error| format!("Could not remove manifest part ownership record: {error}"))?;
    sync_staging_directory(staging)?;
    let parent = path
        .parent()
        .ok_or_else(|| "The manifest part has no parent directory".to_string())?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("Could not sync destination directory: {error}"))
}

#[derive(Clone, Copy)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

fn file_identity(file: &File) -> Result<FileIdentity, String> {
    let metadata = file
        .metadata()
        .map_err(|error| format!("Could not inspect destination manifest part: {error}"))?;
    if !metadata.is_file() {
        return Err("The destination manifest part is not a regular file".to_string());
    }
    metadata_identity(&metadata)
}

#[cfg(unix)]
fn metadata_identity(metadata: &fs::Metadata) -> Result<FileIdentity, String> {
    Ok(FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(not(unix))]
fn metadata_identity(_metadata: &fs::Metadata) -> Result<FileIdentity, String> {
    Err("Destination manifest part ownership requires Unix file identities".to_string())
}

fn create_part_file(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options.open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            "A file already exists at the planned manifest part path without a matching ownership proof; it was preserved".to_string()
        } else {
            format!("Could not exclusively create destination manifest part {}: {error}", path.display())
        }
    })?;
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("Could not secure destination manifest part: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("Could not sync destination manifest part: {error}"))?;
    let parent = path
        .parent()
        .ok_or_else(|| "The manifest part has no parent directory".to_string())?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            format!("Could not sync destination directory after creating manifest part: {error}")
        })?;
    Ok(file)
}

fn open_part_no_follow(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    options.open(path).map_err(|error| {
        format!(
            "Could not safely reopen destination manifest part {}: {error}",
            path.display()
        )
    })
}

fn read_part_owner(path: &Path) -> Result<Option<PartOwner>, String> {
    let bytes = match crate::bounded_file::read_regular_file_bounded(path, MAX_PLAN_BYTES) {
        Ok(bytes) => bytes,
        Err(crate::bounded_file::BoundedFileError::Io(error))
            if error.kind() == std::io::ErrorKind::NotFound =>
        {
            return Ok(None);
        }
        Err(error) => {
            return Err(format!(
                "Could not safely read manifest part ownership {}: {error}",
                path.display()
            ));
        }
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("Manifest part ownership record is invalid: {error}"))
}

fn write_part_owner(
    staging: &Path,
    path: &Path,
    owner: &PartOwner,
    replace_existing: bool,
) -> Result<(), String> {
    let bytes = serde_json::to_vec(owner)
        .map_err(|error| format!("Could not encode manifest part ownership: {error}"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(staging)
        .map_err(|error| format!("Could not create manifest part ownership record: {error}"))?;
    #[cfg(unix)]
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("Could not secure manifest part ownership record: {error}"))?;
    temporary
        .write_all(&bytes)
        .map_err(|error| format!("Could not write manifest part ownership record: {error}"))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| format!("Could not sync manifest part ownership record: {error}"))?;
    if replace_existing {
        temporary.persist(path).map_err(|error| {
            format!("Could not update manifest part ownership: {}", error.error)
        })?;
    } else {
        temporary.persist_noclobber(path).map_err(|error| {
            format!(
                "Could not durably record manifest part ownership: {}",
                error.error
            )
        })?;
    }
    sync_staging_directory(staging)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn plan_records_visible_track_part_names() {
        let destination_dir = tempfile::tempdir().expect("create destination");
        let staging = tempfile::tempdir().expect("create staging");
        let destination = destination_dir.path().join("Example video.mp4");

        ensure_plan(staging.path(), &destination, 42, true, true).expect("write destination plan");

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
    fn owned_part_is_exclusive_durable_and_reopenable() {
        let destination_dir = tempfile::tempdir().expect("create destination");
        let staging = tempfile::tempdir().expect("create staging");
        let destination = destination_dir.path().join("Example.mp4");
        ensure_plan(staging.path(), &destination, 42, false, false).expect("write plan");

        let part = part_path(staging.path(), &destination, 42, TrackKind::Video)
            .expect("resolve part path");
        let mut file = open_owned_part(staging.path(), &destination, 42, TrackKind::Video)
            .expect("claim destination part");
        file.write_all(b"partial media").expect("write part");
        file.sync_all().expect("sync part");
        drop(file);

        let reopened = open_owned_part(staging.path(), &destination, 42, TrackKind::Video)
            .expect("reopen owned part");
        assert_eq!(reopened.metadata().expect("part metadata").len(), 13);
        drop(reopened);
        assert_eq!(fs::read(&part).expect("read part"), b"partial media");

        remove_owned_part(staging.path(), &destination, 42, TrackKind::Video)
            .expect("remove owned part");
        assert!(!part.exists());
    }

    #[test]
    fn preexisting_destination_part_without_owner_is_preserved() {
        let destination_dir = tempfile::tempdir().expect("create destination");
        let staging = tempfile::tempdir().expect("create staging");
        let destination = destination_dir.path().join("Example.mp4");
        ensure_plan(staging.path(), &destination, 42, false, false).expect("write plan");
        let part = part_path(staging.path(), &destination, 42, TrackKind::Video)
            .expect("resolve part path");
        fs::write(&part, b"foreign data").expect("create foreign file");

        assert!(open_owned_part(staging.path(), &destination, 42, TrackKind::Video).is_err());
        assert_eq!(
            fs::read(part).expect("read preserved file"),
            b"foreign data"
        );
    }

    #[test]
    fn replaced_destination_part_is_not_reopened_or_removed() {
        let destination_dir = tempfile::tempdir().expect("create destination");
        let staging = tempfile::tempdir().expect("create staging");
        let destination = destination_dir.path().join("Example.mp4");
        ensure_plan(staging.path(), &destination, 42, false, false).expect("write plan");
        let part = part_path(staging.path(), &destination, 42, TrackKind::Video)
            .expect("resolve part path");
        let original = open_owned_part(staging.path(), &destination, 42, TrackKind::Video)
            .expect("claim destination part");
        fs::remove_file(&part).expect("replace owned part");
        fs::write(&part, b"replacement").expect("write replacement");

        assert!(open_owned_part(staging.path(), &destination, 42, TrackKind::Video).is_err());
        assert!(remove_owned_part(staging.path(), &destination, 42, TrackKind::Video).is_err());
        assert_eq!(fs::read(part).expect("read replacement"), b"replacement");
        drop(original);
    }


    #[cfg(unix)]
    #[test]
    fn preexisting_destination_symlink_is_never_followed() {
        let destination_dir = tempfile::tempdir().expect("create destination");
        let staging = tempfile::tempdir().expect("create staging");
        let destination = destination_dir.path().join("Example.mp4");
        ensure_plan(staging.path(), &destination, 42, false, false).expect("write plan");
        let part = part_path(staging.path(), &destination, 42, TrackKind::Video)
            .expect("resolve part path");
        let victim = staging.path().join("victim");
        fs::write(&victim, b"preserve").expect("write victim");
        std::os::unix::fs::symlink(&victim, &part).expect("create destination symlink");

        assert!(open_owned_part(staging.path(), &destination, 42, TrackKind::Video).is_err());
        assert_eq!(fs::read(victim).expect("read victim"), b"preserve");
        assert!(
            fs::symlink_metadata(part)
                .expect("inspect symlink")
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn missing_owned_part_is_recreated_with_a_new_identity() {
        let destination_dir = tempfile::tempdir().expect("create destination");
        let staging = tempfile::tempdir().expect("create staging");
        let destination = destination_dir.path().join("Example.mp4");
        ensure_plan(staging.path(), &destination, 42, false, false).expect("write plan");
        let part = part_path(staging.path(), &destination, 42, TrackKind::Video)
            .expect("resolve part path");
        drop(
            open_owned_part(staging.path(), &destination, 42, TrackKind::Video)
                .expect("claim destination part"),
        );
        fs::remove_file(&part).expect("simulate missing owned part");

        let recreated = open_owned_part(staging.path(), &destination, 42, TrackKind::Video)
            .expect("recreate missing owned part");
        assert_eq!(recreated.metadata().expect("part metadata").len(), 0);
        drop(recreated);
        assert_eq!(fs::metadata(part).expect("inspect recreated part").len(), 0);
    }

    #[test]
    fn matching_plan_is_idempotent_and_changed_plan_is_preserved() {
        let destination_dir = tempfile::tempdir().expect("create destination");
        let staging = tempfile::tempdir().expect("create staging");
        let destination = destination_dir.path().join("Example.mp4");

        ensure_plan(staging.path(), &destination, 7, false, false).expect("write initial plan");
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

        assert!(
            ensure_plan(
                staging.path(),
                &destination_dir.path().join("Example.mp4"),
                7,
                false,
                false,
            )
            .is_err()
        );
        assert_eq!(fs::read(victim).expect("read victim"), b"preserve");
    }
}
