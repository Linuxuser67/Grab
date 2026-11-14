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

/// File format used for a native manifest track's staged part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TrackFileFormat {
    Ts,
    Mp4,
    Vtt,
}

impl TrackKind {
    /// Return the canonical staging filename for this track and file format.
    pub(crate) const fn part_filename(self, format: TrackFileFormat) -> &'static str {
        match (self, format) {
            (Self::Video, TrackFileFormat::Ts) => "video-track.ts",
            (Self::Video, TrackFileFormat::Mp4) => "video-track.mp4",
            (Self::Video, TrackFileFormat::Vtt) => "video-track.vtt",
            (Self::Audio, TrackFileFormat::Ts) => "audio-track.ts",
            (Self::Audio, TrackFileFormat::Mp4) => "audio-track.mp4",
            (Self::Audio, TrackFileFormat::Vtt) => "audio-track.vtt",
            (Self::Subtitles, TrackFileFormat::Ts) => "subtitle-track.ts",
            (Self::Subtitles, TrackFileFormat::Mp4) => "subtitle-track.mp4",
            (Self::Subtitles, TrackFileFormat::Vtt) => "subtitle-track.vtt",
        }
    }
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
    crate::file_names::is_single_component(name)
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

/// Resolve a destination part without blocking an async runtime worker.
pub(crate) async fn part_path_async(
    staging: PathBuf,
    destination: PathBuf,
    item_id: u64,
    track: TrackKind,
) -> Result<PathBuf, String> {
    tokio::task::spawn_blocking(move || part_path(&staging, &destination, item_id, track))
        .await
        .map_err(|error| format!("Destination part path task failed: {error}"))?
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

/// Verify that a planned destination part is still the exact regular file
/// claimed by this download. Unlike `open_owned_part`, this
/// function never creates a missing file.
///
/// # Errors
///
/// Returns an error if the ownership record or filesystem identity does not match.
pub(crate) fn verify_owned_part(
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
    let file = open_part_no_follow(&path)?;
    let identity = file_identity(&file)?;
    if identity.device != owner.device || identity.inode != owner.inode {
        return Err("The manifest part was replaced; existing data was preserved".to_string());
    }
    Ok(())
}

/// Open a destination part on a blocking worker and return its Tokio file handle.
pub(crate) async fn open_owned_part_async(
    staging: PathBuf,
    destination: PathBuf,
    item_id: u64,
    track: TrackKind,
) -> Result<tokio::fs::File, String> {
    let file = tokio::task::spawn_blocking(move || {
        open_owned_part(&staging, &destination, item_id, track)
    })
    .await
    .map_err(|error| format!("Destination part open task failed: {error}"))??;
    Ok(tokio::fs::File::from_std(file))
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
    let destination_dir = destination
        .parent()
        .ok_or_else(|| "The download destination has no parent directory".to_string())?;
    let canonical_destination_dir = fs::canonicalize(destination_dir)
        .map_err(|error| format!("Could not resolve destination directory: {error}"))?;
    let destination_key = crate::video_staging::dest_key(&canonical_destination_dir);
    remove_owned_part_path(staging, &path, item_id, &destination_key)
}

/// Remove every part recorded by a download's destination plan.
///
/// This is used before its staging directory is removed, while the per-part
/// ownership records are still available. Planned tracks that were never
/// created are accepted; existing files without a matching owner are kept.
///
/// # Errors
///
/// Returns an error if the plan or any part's ownership cannot be verified.
pub(crate) fn remove_planned_parts(
    staging: &Path,
    destination_dir: &Path,
    item_id: u64,
) -> Result<(), String> {
    let plan_path = staging.join(PLAN_FILE);
    let Some(plan) = read_plan(&plan_path)? else {
        let entries = fs::read_dir(staging)
            .map_err(|error| format!("Could not inspect manifest staging directory: {error}"))?;
        for entry in entries {
            let entry = entry
                .map_err(|error| format!("Could not inspect manifest staging entry: {error}"))?;
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.ends_with(".owner.json"))
            {
                return Err(
                    "Manifest part ownership records exist without a destination plan; files were preserved"
                        .to_string(),
                );
            }
        }
        return Ok(());
    };
    let canonical_destination_dir = fs::canonicalize(destination_dir)
        .map_err(|error| format!("Could not resolve destination directory: {error}"))?;
    let destination_key = crate::video_staging::dest_key(&canonical_destination_dir);
    if plan.version != PLAN_VERSION
        || plan.item_id != item_id
        || plan.destination_key != destination_key
    {
        return Err(
            "The manifest destination plan belongs to a different download; files were preserved"
                .to_string(),
        );
    }
    if !plan.parts.iter().any(|part| part.track == TrackKind::Video) {
        return Err(
            "The manifest destination plan has no video track; files were preserved".to_string(),
        );
    }
    for (index, part) in plan.parts.iter().enumerate() {
        if plan.parts[..index]
            .iter()
            .any(|previous| previous.track == part.track || previous.file_name == part.file_name)
        {
            return Err(
                "The manifest destination plan contains duplicate tracks or filenames; files were preserved"
                    .to_string(),
            );
        }
    }
    let entries = fs::read_dir(staging)
        .map_err(|error| format!("Could not inspect manifest staging directory: {error}"))?;
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("Could not inspect manifest staging entry: {error}"))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(owner_name) = name.strip_suffix(".owner.json") else {
            continue;
        };
        if !plan.parts.iter().any(|part| part.file_name == owner_name) {
            return Err(
                "Manifest staging contains an ownership record absent from the destination plan; files were preserved"
                    .to_string(),
            );
        }
    }

    for part in &plan.parts {
        if !safe_file_name(&part.file_name) {
            return Err("The manifest destination plan contains an unsafe filename".to_string());
        }
        let path = canonical_destination_dir.join(&part.file_name);
        remove_owned_part_path(staging, &path, item_id, &destination_key)?;
    }
    Ok(())
}

/// Verify ownership before unlinking; filename similarity alone never authorizes deletion.
fn remove_owned_part_path(
    staging: &Path,
    path: &Path,
    item_id: u64,
    destination_key: &str,
) -> Result<(), String> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "The manifest part filename is invalid".to_string())?;
    let owner_path = staging.join(format!("{file_name}.owner.json"));
    let Some(owner) = read_part_owner(&owner_path)? else {
        return match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Ok(_) => Err("The manifest part has no ownership record; it was preserved".to_string()),
            Err(error) => Err(format!("Could not inspect unowned manifest part: {error}")),
        };
    };
    if owner.version != 1
        || owner.item_id != item_id
        || owner.destination_key != destination_key
        || owner.file_name != file_name
    {
        return Err("The manifest part ownership record does not match this download".to_string());
    }
    match fs::symlink_metadata(path) {
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
            let opened = open_part_no_follow(path)?;
            let opened_identity = file_identity(&opened)?;
            if opened_identity.device != owner.device || opened_identity.inode != owner.inode {
                return Err(
                    "The manifest part changed during cleanup; existing data was preserved"
                        .to_string(),
                );
            }
            drop(opened);
            let final_metadata = fs::symlink_metadata(path)
                .map_err(|error| format!("Could not recheck owned manifest part: {error}"))?;
            let final_identity = metadata_identity(&final_metadata)?;
            if final_metadata.file_type().is_symlink()
                || !final_metadata.is_file()
                || final_identity.device != owner.device
                || final_identity.inode != owner.inode
            {
                return Err(
                    "The manifest part changed before cleanup; existing data was preserved"
                        .to_string(),
                );
            }
            fs::remove_file(path).map_err(|error| {
                format!(
                    "Could not remove owned manifest part {}: {error}",
                    path.display()
                )
            })?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("Could not inspect owned manifest part: {error}")),
    }
    let parent = path
        .parent()
        .ok_or_else(|| "The manifest part has no parent directory".to_string())?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("Could not sync destination directory: {error}"))?;
    fs::remove_file(&owner_path)
        .map_err(|error| format!("Could not remove manifest part ownership record: {error}"))?;
    sync_staging_directory(staging)
}

/// Verify a destination part on a blocking worker.
pub(crate) async fn verify_owned_part_async(
    staging: PathBuf,
    destination: PathBuf,
    item_id: u64,
    track: TrackKind,
) -> Result<(), String> {
    tokio::task::spawn_blocking(move || verify_owned_part(&staging, &destination, item_id, track))
        .await
        .map_err(|error| format!("Destination part verification task failed: {error}"))?
}

/// Remove an owned destination part on a blocking worker.
pub(crate) async fn remove_owned_part_async(
    staging: PathBuf,
    destination: PathBuf,
    item_id: u64,
    track: TrackKind,
) -> Result<(), String> {
    tokio::task::spawn_blocking(move || remove_owned_part(&staging, &destination, item_id, track))
        .await
        .map_err(|error| format!("Destination part cleanup task failed: {error}"))?
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
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options.open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            "A file already exists at the planned manifest part path without a matching ownership proof; it was preserved".to_string()
        } else {
            format!("Could not exclusively create destination manifest part {}: {error}", path.display())
        }
    })?;
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

    /// Removes only plan-listed parts with matching durable owner records.
    #[cfg(unix)]
    #[test]
    fn discard_cleanup_removes_planned_parts_and_allows_unopened_tracks() {
        let destination_dir = tempfile::tempdir().expect("create destination");
        let staging = tempfile::tempdir().expect("create staging");
        let destination = destination_dir.path().join("Example video.mp4");
        ensure_plan(staging.path(), &destination, 42, true, true).expect("write destination plan");

        use std::io::Write as _;
        for track in [TrackKind::Video, TrackKind::Audio] {
            let mut file = open_owned_part(staging.path(), &destination, 42, track)
                .expect("create owned track part");
            file.write_all(b"partial media").expect("write track part");
            file.sync_all().expect("sync track part");
        }
        let user_file = destination_dir.path().join("Example video.keep.part");
        fs::write(&user_file, b"user data").expect("create unrelated user file");

        remove_planned_parts(staging.path(), destination_dir.path(), 42)
            .expect("remove all planned owned parts");

        for track in [TrackKind::Video, TrackKind::Audio, TrackKind::Subtitles] {
            let path = part_path(staging.path(), &destination, 42, track);
            if track == TrackKind::Subtitles {
                assert!(
                    !path.expect("resolve subtitle part").exists(),
                    "a planned but unopened track stays absent"
                );
            } else {
                assert!(
                    !path.expect("resolve track part").exists(),
                    "owned track part must be removed"
                );
            }
        }
        assert!(user_file.exists(), "unrelated user files must survive");
        assert!(
            !staging
                .path()
                .join("Example video.42.video.part.owner.json")
                .exists(),
            "ownership records are removed with their part"
        );
    }

    #[test]
    fn discard_cleanup_rejects_owner_records_absent_from_the_plan() {
        let destination_dir = tempfile::tempdir().expect("create destination");
        let staging = tempfile::tempdir().expect("create staging");
        let destination = destination_dir.path().join("Example video.mp4");
        ensure_plan(staging.path(), &destination, 45, false, false)
            .expect("write destination plan");

        use std::io::Write as _;
        let mut file = open_owned_part(staging.path(), &destination, 45, TrackKind::Video)
            .expect("create owned video part");
        file.write_all(b"partial media").expect("write video part");
        drop(file);
        fs::write(
            staging
                .path()
                .join("Example video.45.audio.part.owner.json"),
            b"{}",
        )
        .expect("create unplanned owner record");

        let video_part = part_path(staging.path(), &destination, 45, TrackKind::Video)
            .expect("resolve video part");
        let error = remove_planned_parts(staging.path(), destination_dir.path(), 45)
            .expect_err("unplanned ownership records must be rejected");
        assert!(
            error.contains("absent from the destination plan"),
            "unexpected cleanup error: {error}"
        );
        assert!(
            video_part.exists(),
            "no partial is removed before validation"
        );
        assert!(
            staging
                .path()
                .join("Example video.45.video.part.owner.json")
                .exists(),
            "existing owner metadata must remain"
        );
    }

    #[test]
    fn discard_cleanup_rejects_a_plan_without_a_video_track() {
        let destination_dir = tempfile::tempdir().expect("create destination");
        let staging = tempfile::tempdir().expect("create staging");
        let destination = destination_dir.path().join("Example video.mp4");
        ensure_plan(staging.path(), &destination, 44, false, false)
            .expect("write destination plan");

        let mut plan = read_plan(&staging.path().join(PLAN_FILE))
            .expect("read destination plan")
            .expect("destination plan exists");
        plan.parts.clear();
        fs::write(
            staging.path().join(PLAN_FILE),
            serde_json::to_vec(&plan).expect("encode incomplete destination plan"),
        )
        .expect("replace destination plan");

        let error = remove_planned_parts(staging.path(), destination_dir.path(), 44)
            .expect_err("plan without video must be rejected");
        assert!(
            error.contains("no video track"),
            "unexpected plan validation error: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn discard_cleanup_preserves_replaced_parts_and_their_ownership_record() {
        let destination_dir = tempfile::tempdir().expect("create destination");
        let staging = tempfile::tempdir().expect("create staging");
        let destination = destination_dir.path().join("Example video.mp4");
        ensure_plan(staging.path(), &destination, 43, true, false).expect("write destination plan");

        use std::io::Write as _;
        let mut file = open_owned_part(staging.path(), &destination, 43, TrackKind::Video)
            .expect("create owned video part");
        file.write_all(b"partial media").expect("write video part");
        drop(file);

        let part = part_path(staging.path(), &destination, 43, TrackKind::Video)
            .expect("resolve video part");
        let replacement = destination_dir.path().join("replacement.tmp");
        fs::write(&replacement, b"user replacement").expect("create replacement");
        fs::remove_file(&part).expect("remove original part");
        fs::rename(&replacement, &part).expect("replace part with foreign file");

        let error = remove_planned_parts(staging.path(), destination_dir.path(), 43)
            .expect_err("replacement must not be deleted");
        assert!(
            error.contains("replaced"),
            "unexpected cleanup error: {error}"
        );
        assert_eq!(
            fs::read(&part).expect("read preserved replacement"),
            b"user replacement"
        );
        assert!(
            staging
                .path()
                .join("Example video.43.video.part.owner.json")
                .exists(),
            "ownership metadata must remain when cleanup cannot verify a part"
        );
    }

    #[cfg(unix)]
    #[test]
    fn destination_part_is_private_when_created() {
        use std::os::unix::fs::PermissionsExt as _;

        let destination_dir = tempfile::tempdir().expect("create destination");
        let staging = tempfile::tempdir().expect("create staging");
        let destination = destination_dir.path().join("Example.mp4");
        ensure_plan(staging.path(), &destination, 46, false, false).expect("write plan");

        let file = open_owned_part(staging.path(), &destination, 46, TrackKind::Video)
            .expect("create owned video part");
        let mode = file
            .metadata()
            .expect("inspect part permissions")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(mode, 0o600, "destination part must be private at creation");
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
        verify_owned_part(staging.path(), &destination, 42, TrackKind::Video)
            .expect("verify owned destination part");
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
        assert!(verify_owned_part(staging.path(), &destination, 42, TrackKind::Video).is_err());
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

    #[test]
    fn track_part_filenames_follow_track_and_file_format() {
        assert_eq!(
            TrackKind::Video.part_filename(TrackFileFormat::Ts),
            "video-track.ts"
        );
        assert_eq!(
            TrackKind::Audio.part_filename(TrackFileFormat::Mp4),
            "audio-track.mp4"
        );
        assert_eq!(
            TrackKind::Subtitles.part_filename(TrackFileFormat::Vtt),
            "subtitle-track.vtt"
        );
    }
}
