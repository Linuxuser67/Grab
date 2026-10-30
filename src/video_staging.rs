//! Video staging/parts/manifest/resume scratch dirs and templates.
//! Consumed by the runner/engine through the `video` facade.
//!
//! # Ownership invariant (enforced by all cleanup paths)
//!
//! ```text
//! Manifest exact-match:  destructive delete (authoritative)
//! Filename inference:    occupancy only, never deletion
//! No manifest:           preserve everything
//! ```
//!
//! All staging cleanup functions (`clean_staging_files`,
//! `sweep_staging_preserving_recordings`, `sweep_dest_staging`) follow this
//! single model. No filename pattern may delete a staging file: ownership
//! must be recorded in a manifest (`staging_name` exact-match or
//! `staging_prefix`). (Separate stem-based helpers like `clean_dest_parts`
//! handle files beside the finished download, not staging.)

use crate::video_prefs::subtitle_content_languages;
use crate::video_tools::VideoError;
use gettextrs::gettext;
use std::path::{Component, Path, PathBuf};

/// Shared root for tiny engine scratch (cookie dumps, probe output). Stays on
/// tmpfs: only small files ever land here. Bulky download parts stage beside
/// the destination instead — `/tmp` is RAM-backed on many systems, so staging
/// there fills memory and fails large downloads with "no space left".
pub fn staging_root() -> PathBuf {
    std::env::temp_dir().join("grab-video")
}

/// Root for persistent per-item bookkeeping (resume manifests). This is
/// application state, not user content: it survives restarts but is not a
/// download. Debug-only `GRAB_STATE_DIR` seam: tests can point it at a scratch
/// dir; release builds always use the real location.
fn state_root() -> PathBuf {
    if let Some(dir) = debug_dir_override("GRAB_STATE_DIR") {
        return dir;
    }
    #[cfg(test)]
    {
        test_root("state")
    }
    #[cfg(not(test))]
    {
        xdg_dir("XDG_STATE_HOME")
            .unwrap_or_else(|| glib::home_dir().join(".local/state"))
            .join("grab")
    }
}

/// Root for single-use private scratch (probe info JSON holding signed URLs).
/// Ephemeral under `$XDG_RUNTIME_DIR`; when the session provides none, the
/// state root keeps it private and the startup sweep reclaims it.
fn runtime_root() -> PathBuf {
    if let Some(dir) = debug_dir_override("GRAB_RUNTIME_DIR") {
        return dir;
    }
    #[cfg(test)]
    {
        test_root("runtime")
    }
    #[cfg(not(test))]
    {
        match xdg_dir("XDG_RUNTIME_DIR") {
            Some(dir) => dir.join("grab"),
            None => state_root().join("run"),
        }
    }
}

/// Single-use probe JSON paths in the private scratch root. Private because
/// they can carry signed media URLs and request headers; single-use because a
/// resumed attempt resolves fresh.
pub(crate) fn info_json_path(item_id: u64) -> PathBuf {
    runtime_root().join(format!("info-{item_id}.json"))
}

pub(crate) fn live_info_json_path(item_id: u64) -> PathBuf {
    runtime_root().join(format!("live-{item_id}.json"))
}

/// An absolute XDG base dir, or `None` when unset or relative (the spec says
/// relative paths must be ignored).
#[cfg(not(test))]
fn xdg_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

#[cfg(debug_assertions)]
fn debug_dir_override(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

#[cfg(not(debug_assertions))]
fn debug_dir_override(_var: &str) -> Option<PathBuf> {
    None
}

/// One scratch root per test process. Tests run serially but their temp dirs
/// differ; the shared root keeps them off the developer's real state dirs.
#[cfg(test)]
fn test_root(kind: &str) -> PathBuf {
    static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("grab-test-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        root
    })
    .join(kind)
}

/// Staging files live directly in the destination dir, visible like other download managers:
/// A resolved per-item staging dir plus the root it is guarded under: the
/// dest-side root for current staging, the tmp root for legacy dirs still
/// draining from before dest-side staging.
#[derive(Debug, Clone)]
pub struct StagingLocation {
    pub dir: PathBuf,
    pub root: PathBuf,
}

/// Resolve the staging location: the destination dir itself. Staging files
/// are named via [`staging_file`]; there is no subfolder and no
/// legacy tmp fallback.
pub fn staging_location(dest_dir: &Path, _item_id: u64) -> StagingLocation {
    StagingLocation {
        dir: dest_dir.to_path_buf(),
        root: dest_dir.to_path_buf(),
    }
}

/// Resolve the staging location from a full destination *file* path (its
/// parent anchors the staging dir).
pub fn staging_location_for_dest(dest: &Path, item_id: u64) -> StagingLocation {
    match dest.parent() {
        Some(dir) => staging_location(dir, item_id),
        None => StagingLocation {
            dir: PathBuf::new(),
            root: PathBuf::new(),
        },
    }
}

/// Media extensions Grab writes (shared by the id-in-name matcher and the
/// legacy staging-suffix allowlist).
const MEDIA_EXTS: &[&str] = &[
    "mp4", "webm", "mkv", "m4a", "mp3", "ogg", "wav", "flac", "opus",
];

/// Whether `file_name` is a Grab staging file for `item_id` under the
/// id-in-name scheme: `{stem}.{id}.{ext}[.part|.ytdl]` or the deduped
/// `{stem}.{id}-{n}.{ext}[.part|.ytdl]`.
///
/// The id must be followed by a known media extension (optionally plus a
/// yt-dlp sidecar suffix). Pattern-based deletion only ever touches
/// `.part`/`.ytdl` sidecars — never a bare media file — so a user file like
/// `my.backup.4.mp4` can never be deleted via this matcher.
pub(crate) fn staging_name_matches_id(file_name: &str, item_id: u64) -> bool {
    // Strip yt-dlp sidecar suffixes first.
    let mut base = file_name;
    if let Some(s) = base.strip_suffix(".part") {
        base = s;
    } else if let Some(s) = base.strip_suffix(".ytdl") {
        base = s;
    }
    // Must end with .{media-ext}.
    let Some(dot) = base.rfind('.') else {
        return false;
    };
    if !MEDIA_EXTS.iter().any(|e| *e == &base[dot + 1..]) {
        return false;
    }
    let stem_with_id = &base[..dot];
    // Exact: {stem}.{id}
    let id_str = item_id.to_string();
    if stem_with_id
        .rfind('.')
        .map(|p| &stem_with_id[p + 1..])
        .is_some_and(|tail| tail == id_str)
    {
        return true;
    }
    // Deduped: {stem}.{id}-{n}
    let prefix = format!(".{id_str}-");
    if let Some(pos) = stem_with_id.rfind(&prefix) {
        let after = &stem_with_id[pos + prefix.len()..];
        return !after.is_empty() && after.chars().all(|c| c.is_ascii_digit());
    }
    false
}

/// In-memory check for staging files associated with an item id, using an
/// already-complete destination listing made off the GTK thread.
pub(crate) fn staging_occupied_in(names: &[String], item_id: u64) -> bool {
    // Id-in-name scheme: the file itself carries the id, so a lost manifest
    // cannot orphan the id into reuse.
    // Legacy patterns: grab-{id}- prefix (safe) and .{id}.live. (has marker, safe).
    // The bare .{id}. pattern is deliberately NOT checked: it false-positives
    // on user files like linux-5.4.0.tar.gz.
    let manifest = format!(".{item_id}.manifest.json");
    let prefix = format!("grab-{item_id}-");
    let live_pattern = format!(".{item_id}.live.");
    names.iter().any(|n| {
        n == &manifest
            || staging_name_matches_id(n, item_id)
            || n.starts_with(&prefix)
            || n.contains(&live_pattern)
    })
}

/// Create a staging dir under an explicit root, verifying it stays there (a
/// pre-planted symlink must not redirect parts).
pub fn ensure_staging_dir_in(root: &Path, dir: &Path) -> Result<PathBuf, VideoError> {
    // Refuse a symlinked root before creating anything through it: the
    // containment check below canonicalizes the root, which would otherwise
    // validate a link pointing anywhere.
    if !staging_root_is_real(root) {
        return Err(VideoError::staging(gettext(
            "staging root is not a real directory",
        )));
    }
    // Leaf-first: a planted symlink at the leaf is refused before the old
    // create_dir_all-then-verify ran. A same-root sibling link even passed
    // the containment check below (it canonicalizes inside the root), so
    // parts landed cross-item; other links were rejected, but only after the
    // filesystem had already been touched through the link.
    ensure_real_staging_leaf(dir)?;
    guarded_staging_dir(root, dir)
        .ok_or_else(|| VideoError::staging(gettext("staging directory escaped its root")))
}

/// Create a single directory with owner-only permissions (0700).
#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new().mode(0o700).create(dir)
}

/// Non-Unix fallback: no permission bits to set.
#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir(dir)
}

/// Create a private bookkeeping dir and any missing ancestors with owner-only
/// permissions (0700). A symlinked leaf is refused, never followed: these dirs
/// hold manifests and probe JSONs.
fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut missing = Vec::new();
    let mut current = dir;
    while std::fs::symlink_metadata(current).is_err() {
        missing.push(current.to_path_buf());
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent;
    }
    for path in missing.iter().rev() {
        match create_private_dir(path) {
            Ok(()) => {}
            // Lost a race for the name: the check below validates the winner.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_dir() => Ok(()),
        _ => Err(std::io::Error::other(
            "bookkeeping directory is not a real directory",
        )),
    }
}

/// The leaf must be a real directory, never a symlink: build the parent
/// chain, then create the leaf atomically, so a pre-planted link is refused
/// up front and a link racing for the name can't be followed into place.
/// Used for staging dirs and for Grab's private bookkeeping dirs; names are
/// item ids, so unlike shareable folders there is nothing to dedupe to —
/// refusing loudly is the safe behavior.
pub(crate) fn ensure_real_staging_leaf(dir: &Path) -> Result<(), VideoError> {
    let refused = || VideoError::staging(gettext("staging directory is not a real directory"));
    match std::fs::symlink_metadata(dir) {
        // `symlink_metadata` doesn't follow the final component, so a link
        // reads as a link even when its target is a dir.
        Ok(md) if md.file_type().is_symlink() => Err(refused()),
        Ok(md) if !md.file_type().is_dir() => Err(refused()),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = dir.parent() {
                std::fs::create_dir_all(parent).map_err(VideoError::staging)?;
            }
            // Owner-only leaf: staging can hold private info-JSON temp files.
            // Only the leaf gets the mode — never the shared parent above.
            match create_private_dir(dir) {
                Ok(()) => Ok(()),
                // Lost a race for the name: whoever won must still be a real
                // dir — a link that appeared in the gap is refused.
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    match std::fs::symlink_metadata(dir) {
                        Ok(md) if md.file_type().is_dir() => Ok(()),
                        _ => Err(refused()),
                    }
                }
                Err(e) => Err(VideoError::staging(e)),
            }
        }
        Err(e) => Err(VideoError::staging(e)),
    }
}

/// The staging root itself must be a real directory. Canonicalizing a symlinked
/// root would make the containment check self-validating (the link target
/// trivially starts with itself), so a planted `.grab-video` link could divert
/// staging writes — and, worse, orphan-sweep deletions — anywhere.
fn staging_root_is_real(root: &Path) -> bool {
    match std::fs::symlink_metadata(root) {
        // symlink_metadata does not follow the final component, so a symlink
        // reports is_dir() == false here.
        Ok(meta) => meta.file_type().is_dir(),
        // A missing root is fine: create_dir_all builds it as a real dir.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

/// Canonicalize `dir`, returning it only if it stays under `root` (a planted
/// symlink that escapes refuses instead of diverting the removal).
fn guarded_staging_dir(root: &Path, dir: &Path) -> Option<PathBuf> {
    if !staging_root_is_real(root) {
        return None;
    }
    let (Ok(canon), Ok(root)) = (std::fs::canonicalize(dir), std::fs::canonicalize(root)) else {
        return None;
    };
    canon.starts_with(&root).then_some(canon)
}

/// Remove a staging dir, guarded to stay under the staging root (never user data).
/// Test-only: production cleans through [`clean_staging_in`] with the resolved root.
#[cfg(test)]
pub fn clean_staging(dir: &Path) {
    clean_staging_in(&staging_root(), dir);
}

/// Prepare a private, item-scoped staging directory for native manifest resources.
pub(crate) fn ensure_manifest_staging_dir(
    dest_dir: &Path,
    item_id: u64,
) -> Result<PathBuf, VideoError> {
    let dest_dir = std::fs::canonicalize(dest_dir).map_err(VideoError::staging)?;
    let dir = dest_dir.join(format!(".grab-manifest-{item_id}"));
    let marker = dir.join(".grab-manifest-owner");
    let expected = format!("grab-manifest-v1\n{item_id}\n{}", dest_dir.display());

    match create_private_dir(&dir) {
        Ok(()) => {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&marker)
                .map_err(VideoError::staging)?;
            use std::io::Write as _;
            file.write_all(expected.as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(VideoError::staging)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure_real_staging_leaf(&dir)?;
            let metadata = std::fs::symlink_metadata(&marker).map_err(VideoError::staging)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(VideoError::staging(
                    "manifest staging directory has no valid ownership marker",
                ));
            }
            let owner = std::fs::read_to_string(&marker).map_err(VideoError::staging)?;
            if owner != expected {
                return Err(VideoError::staging(
                    "manifest staging directory belongs to another attempt",
                ));
            }
        }
        Err(error) => return Err(VideoError::staging(error)),
    }
    Ok(dir)
}

/// Remove only a native-manifest staging directory carrying this row's marker.
pub(crate) fn clean_manifest_staging_files(dest_dir: &Path, item_id: u64) {
    let Ok(dest_dir) = std::fs::canonicalize(dest_dir) else {
        return;
    };
    let dir = dest_dir.join(format!(".grab-manifest-{item_id}"));
    let marker = dir.join(".grab-manifest-owner");
    let expected = format!("grab-manifest-v1\n{item_id}\n{}", dest_dir.display());
    let (Ok(dir_metadata), Ok(marker_metadata), Ok(owner)) = (
        std::fs::symlink_metadata(&dir),
        std::fs::symlink_metadata(&marker),
        std::fs::read_to_string(&marker),
    ) else {
        return;
    };
    if dir_metadata.file_type().is_symlink()
        || !dir_metadata.is_dir()
        || marker_metadata.file_type().is_symlink()
        || !marker_metadata.is_file()
        || owner != expected
    {
        return;
    }
    let Ok(canonical_dir) = std::fs::canonicalize(&dir) else {
        return;
    };
    if canonical_dir.parent() == Some(dest_dir.as_path()) {
        let _ = std::fs::remove_dir_all(canonical_dir);
    }
}

/// Remove Grab staging files for an item in the destination dir.
/// Manifest ownership is the sole destructive authority: only the exact
/// filenames recorded in the manifest (`staging_name`) and files under its
/// recorded `staging_prefix` are deleted. A manifest that records no ownership
/// is left alone.
pub fn clean_staging_files(dest_dir: &Path, item_id: u64) {
    remove_manifest_owned_files(dest_dir, item_id);
    clean_manifest_staging_files(dest_dir, item_id);
}

/// Remove a staging dir, guarded to stay under an explicit root (never user data).
/// Test-only: production uses `clean_staging_files` with the item's prefix.
#[cfg(test)]
pub(crate) fn clean_staging_in(root: &Path, dir: &Path) {
    if let Some(canon) = guarded_staging_dir(root, dir) {
        let _ = std::fs::remove_dir_all(canon);
    }
    drop_empty_staging_root(root);
}

/// Drop the staging root when the last item dir is gone. `remove_dir` only
/// succeeds on an empty dir, so a sibling item still staging (or a kept
/// recording) keeps the root; a planted symlink at the root is refused by
/// the guards, never followed.
/// Test-only: production never removes dest dirs.
#[cfg(test)]
pub(crate) fn drop_empty_staging_root(root: &Path) {
    // Same guards as the item-dir removal above: refuse a symlinked root
    // before canonicalizing (canonicalizing the link would make the
    // containment check self-validating), then remove through the canonical
    // path so a link swapped in mid-call cannot divert the removal.
    if !staging_root_is_real(root) {
        return;
    }
    let Some(canon) = guarded_staging_dir(root, root) else {
        return;
    };
    let _ = std::fs::remove_dir(canon);
}

/// Startup sweep for manifests that older versions left in the destination
/// dir (canonical + legacy names). New manifests live in the state dir and are
/// handled by [`sweep_state_staging`]. A missing dest dir is a no-op.
///
/// Also removes the previous versions' single-use probe JSONs
/// (`.grab-info-*` / `.grab-live-*`): they are never reused across restarts.
///
/// Legacy manifests are no longer parsed: the sweep deletes the bookkeeping
/// files themselves, plus anything a *state* manifest for the same row still
/// proves ownership of (`staging_name` / `staging_prefix`). A manifest that
/// records no ownership is left alone. A user's own files are never touched:
/// without a manifest, everything is preserved.
pub fn sweep_dest_staging(dest_dir: &Path, keep: &std::collections::HashSet<u64>) {
    // Manifest-driven cleanup: the ONLY destructive authority for staging
    // files is the manifest's `staging_name` / `staging_prefix`. Scan for
    // manifest files, extract orphan IDs, delete exactly what each manifest owns.
    let Ok(entries) = std::fs::read_dir(dest_dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name();
        let name_str = name.to_str().unwrap_or("");
        // Native manifest resources are scoped to a queue id and carry an
        // explicit ownership marker. A Done/removed row no longer owns them.
        if let Some(id) = name_str
            .strip_prefix(".grab-manifest-")
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|id| name_str == format!(".grab-manifest-{id}"))
        {
            if !keep.contains(&id) {
                clean_manifest_staging_files(dest_dir, id);
            }
            continue;
        }
        // Old single-use probe JSONs: stale by definition after a restart.
        if is_legacy_info_name(name_str) {
            let _ = std::fs::remove_file(entry.path());
            continue;
        }
        // Find manifest files: .{id}.manifest.json (canonical) or legacy.
        let id = if let Some(id_str) = name_str
            .strip_prefix('.')
            .and_then(|s| s.strip_suffix(".manifest.json"))
        {
            id_str.parse::<u64>().ok()
        } else if let Some(rest) = name_str
            .strip_prefix(".grab-")
            .and_then(|s| s.strip_suffix("-manifest.json"))
        {
            rest.parse::<u64>().ok()
        } else if let Some(rest) = name_str.strip_prefix("grab-").and_then(|s| {
            // Handle "grab-{id}-manifest.json" and "grab-{id}-.manifest.json"
            s.strip_suffix("-manifest.json")
                .or_else(|| s.strip_suffix("-.manifest.json"))
        }) {
            // Remove trailing dash if present (from "grab-{id}-.manifest.json")
            rest.strip_suffix('-').unwrap_or(rest).parse::<u64>().ok()
        } else {
            None
        };
        let Some(id) = id else { continue };
        if keep.contains(&id) {
            continue;
        }
        // Manifest exists = proof we created this. Delete exactly the files
        // it recorded, then the manifest itself.
        remove_manifest_owned_files(dest_dir, id);
    }
}

/// Startup sweep for state-dir manifests: reclaim every manifest whose
/// `(destination, id)` no longer has a queue row. `keep` carries the raw queue
/// dest strings; keys are normalized here so a trailing slash cannot hide a
/// live row. Also clears stale runtime scratch.
pub fn sweep_state_staging(keep: &std::collections::HashSet<(String, u64)>) {
    let keep: std::collections::HashSet<(String, u64)> = keep
        .iter()
        .map(|(dest, id)| (dest_key(Path::new(dest)), *id))
        .collect();
    if let Ok(entries) = std::fs::read_dir(state_root().join("video")) {
        for entry in entries.filter_map(|e| e.ok()) {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            // An interrupted atomic write leaves this temp, never a truncated
            // manifest. No writer runs during startup, so it is always debris.
            if name.ends_with(".tmp") {
                let _ = std::fs::remove_file(entry.path());
                continue;
            }
            let Some(id) = state_manifest_id(&name) else {
                continue;
            };
            let Some(manifest) = read_manifest_file(&entry.path()) else {
                // Unreadable manifest: keep it while any queued row still
                // claims the id, or a crash mid-write would drop resume state.
                if !keep.iter().any(|(_, kid)| *kid == id) {
                    let _ = std::fs::remove_file(entry.path());
                }
                continue;
            };
            let Some(dest) = manifest.dest_dir.as_deref() else {
                // No destination recorded = no file it could prove ownership
                // of. State manifests always record one; drop the stray unless
                // a queued row still claims the id.
                if !keep.iter().any(|(_, kid)| *kid == id) {
                    let _ = std::fs::remove_file(entry.path());
                }
                continue;
            };
            if keep.contains(&(dest_key(dest), id)) {
                continue;
            }
            // Act on the manifest we opened: its recorded destination may no
            // longer hash to the path in its own filename (moved/symlink).
            let Some(ownership) = manifest_ownership_of(&manifest) else {
                continue;
            };
            remove_owned_files(dest, &ownership);
            let _ = std::fs::remove_file(entry.path());
            remove_manifest_files(dest, id);
        }
    }
    sweep_private_temps();
}

/// `{id}-{key}.manifest.json` -> id.
fn state_manifest_id(name: &str) -> Option<u64> {
    let (id, key) = name.strip_suffix(".manifest.json")?.split_once('-')?;
    if key.len() != 16 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    id.parse().ok()
}

/// Startup: all scratch JSON is per-attempt and never reused across a process.
fn sweep_private_temps() {
    let Ok(entries) = std::fs::read_dir(runtime_root()) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if is_private_temp_name(&name) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// `.grab-info-{id}.json` / `.grab-live-{id}.json` (pre-runtime-dir names).
fn is_legacy_info_name(name: &str) -> bool {
    let rest = name
        .strip_prefix(".grab-info-")
        .or_else(|| name.strip_prefix(".grab-live-"));
    rest.and_then(|s| s.strip_suffix(".json"))
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
}

/// `info-{id}.json` / `live-{id}.json` in the private scratch root.
fn is_private_temp_name(name: &str) -> bool {
    let name = name.strip_suffix(".tmp").unwrap_or(name);
    let rest = name
        .strip_prefix("info-")
        .or_else(|| name.strip_prefix("live-"));
    rest.and_then(|s| s.strip_suffix(".json"))
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
}

/// Sidecar recording completed parts, so a retry can skip straight to the merge.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct VideoManifest {
    pub(crate) page_url: String,
    pub(crate) quality: String,
    pub(crate) video_format_id: Option<String>,
    pub(crate) video_ext: String,
    pub(crate) audio_format_id: String,
    pub(crate) audio_ext: String,
    /// Final output size once the file has been renamed into place.
    /// Lets a retry after a crash adopt the finished file without any work.
    pub(crate) final_bytes: Option<u64>,
    /// Staging basename (e.g., "My Video.4.mp4").
    /// Exact name for cleanup; the id is also baked into the filename itself
    /// so the file stays attributable if the manifest is lost.
    pub(crate) staging_name: Option<String>,
    /// Ownership prefix for paths that stage many files (unified downloads):
    /// e.g., "My Video.4.". Any file in the dest dir starting with this
    /// (except `final.*`) is owned. Recorded in the manifest, so the
    /// pattern is manifest-anchored, not inferred.
    #[serde(default)]
    pub(crate) staging_prefix: Option<String>,
    /// Destination directory this attempt writes into. The state-dir filename
    /// already carries a hash of it; recording it lets the startup sweep find
    /// the files without reversing the hash. Absent in older manifests (which
    /// still live inside the destination and need no lookup).
    #[serde(default)]
    pub(crate) dest_dir: Option<PathBuf>,
}

impl VideoManifest {
    /// Whether the sidecar describes this exact attempt (anything else means re-download).
    fn matches(
        &self,
        page_url: &str,
        quality: &str,
        video: Option<(&str, &str)>,
        audio: (&str, &str),
    ) -> bool {
        self.page_url == page_url
            && self.quality == quality
            && self.audio_format_id == audio.0
            && self.audio_ext == audio.1
            && match (video, &self.video_format_id) {
                (Some((id, ext)), Some(mid)) => id == mid && ext == self.video_ext,
                (None, None) => self.video_ext.is_empty(),
                _ => false,
            }
    }
}

/// Dest-dir part names (`<stem>.<kind>.<ext>`): deterministic across attempts; feed yt-dlp via `ytdlp_output_template`.
pub(crate) fn dest_part_path(dest: &Path, kind: &str, ext: &str) -> PathBuf {
    let dir = dest.parent().unwrap_or_else(|| Path::new(""));
    let stem = dest
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "part".to_string());
    dir.join(format!("{stem}.{kind}.{ext}"))
}

/// Render a literal path as a yt-dlp `-o` template: every `%` is doubled except the trailing `%(ext)s` field.
pub(crate) fn ytdlp_output_template(path: &Path) -> String {
    let s = path.to_string_lossy();
    match s.strip_suffix("%(ext)s") {
        Some(stem) => format!("{}%(ext)s", stem.replace('%', "%%")),
        None => s.replace('%', "%%"),
    }
}

/// Grab-namespaced part infixes (the only names `clean_dest_parts` touches).
const PART_KINDS: &[&str] = &["video.", "audio.", "hls.", "live.", "live-"];

/// Reserve a row-namespaced remux slot via an atomic `.lease` sidecar.
pub(crate) fn reserve_remux_temp(
    staging: &Path,
    item_id: u64,
    ext: &str,
) -> Result<PathBuf, VideoError> {
    let taken = dir_file_names(staging);
    for n in 1..=9999u32 {
        let name = format!("final.{item_id}.{n}.{ext}");
        let partial = format!("{name}.part");
        if taken
            .iter()
            .any(|t| t == &name || t == &format!("{name}.lease") || t == &partial)
        {
            continue;
        }
        let lease = staging.join(format!("{name}.lease"));
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&lease)
        {
            Ok(mut file) => {
                use std::io::Write as _;
                if let Err(e) = file.write_all(b"grab-remux-lease-v1\n") {
                    let _ = std::fs::remove_file(&lease);
                    return Err(VideoError::staging(e.to_string()));
                }
                // The scan is only an optimization; don't claim a path if a
                // part appeared before the lease was created.
                if staging.join(&partial).exists() {
                    let _ = std::fs::remove_file(&lease);
                    continue;
                }
                return Ok(staging.join(name));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(VideoError::staging(e.to_string())),
        }
    }
    Err(VideoError::staging("no free remux slot"))
}

/// Drop the lease from `reserve_remux_temp` (best-effort; a stale lease only costs one slot).
pub(crate) fn release_remux_lease(temp: &Path) {
    let mut lease = temp.as_os_str().to_os_string();
    lease.push(".lease");
    let _ = std::fs::remove_file(std::path::PathBuf::from(lease));
}

/// Suffix of `file_name` past a `<stem>.` prefix, if any. Pure.
fn strip_stem_suffix<'a>(file_name: &'a str, stem: &str) -> Option<&'a str> {
    file_name
        .strip_prefix(stem)
        .and_then(|r| r.strip_prefix('.'))
}

pub(crate) fn is_grab_part(file_name: &str, stem: &str) -> bool {
    let Some(remainder) = strip_stem_suffix(file_name, stem) else {
        return false;
    };
    // Standard kinds: video., audio., hls., live. (legacy)
    if PART_KINDS.iter().any(|k| remainder.starts_with(k)) {
        return true;
    }
    // Legacy ID-namespaced live: {id}.live. (e.g., "1.live.mp4.part").
    // The bare {id}. pattern is NOT checked here: it false-positives on user
    // files like Movie.2024.mp4. Pattern-based deletion of id-in-name files
    // is unsafe (no marker); the ID allocator only checks these patterns
    // conservatively.
    if let Some(dot_pos) = remainder.find('.') {
        let (id_part, rest) = remainder.split_at(dot_pos);
        if !id_part.is_empty()
            && id_part.chars().all(|c| c.is_ascii_digit())
            && rest.starts_with(".live.")
        {
            return true;
        }
    }
    false
}

/// File names directly inside `dir`, preserving read errors for callers that
/// must not allocate identifiers from an incomplete directory snapshot.
/// A not-yet-created destination is equivalent to an empty directory.
pub(crate) fn try_dir_file_names(dir: &Path) -> std::io::Result<Vec<String>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    entries
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect()
}

/// Best-effort file listing for cleanup and display paths that can tolerate a
/// missing or unreadable directory.
pub(crate) fn dir_file_names(dir: &Path) -> Vec<String> {
    try_dir_file_names(dir).unwrap_or_default()
}

/// Reclaim this row's interrupted remuxes only. Remux temp files share the
/// destination directory with user data and other rows, so a global
/// `final.*.part` sweep could delete an active neighbor's output or a user file.
pub fn sweep_partial_remuxes(staging: &Path, item_id: u64) {
    let prefix = format!("final.{item_id}.");
    for name in dir_file_names(staging) {
        let Some(base) = name
            .strip_prefix(&prefix)
            .and_then(|s| s.strip_suffix(".part"))
        else {
            continue;
        };
        let Some((n, ext)) = base.split_once('.') else {
            continue;
        };
        if n.is_empty()
            || !n.bytes().all(|b| b.is_ascii_digit())
            || ext.is_empty()
            || ext.contains('.')
        {
            continue;
        }
        let lease_name = format!("{prefix}{n}.{ext}.lease");
        let lease = staging.join(&lease_name);
        // No matching lease means we have no positive evidence that the file
        // was Grab's remux scratch. Preserve it rather than guessing.
        if std::fs::symlink_metadata(&lease).is_ok_and(|m| m.file_type().is_file()) {
            let _ = std::fs::remove_file(staging.join(&name));
            let _ = std::fs::remove_file(lease);
        }
    }
}

/// Remove a leg's staging scratch, preserving completed recordings (do not delete the user's only copy).
/// Only touches this item's files; never the dest dir itself or other files.
///
/// Ownership comes from the manifest's `staging_name` (exact) and
/// `staging_prefix` (manifest-recorded prefix). `final.*` is always preserved.
pub fn sweep_staging_preserving_recordings(staging: &Path, item_id: u64) {
    // Manifest ownership is the SOLE destructive authority for staging files.
    // Delete exactly the files the manifest owns, except `final.*` (completed
    // recordings that may be the user's only copy).
    let Some(ownership) = manifest_ownership(staging, item_id) else {
        // No ownership recorded. A valid manifest is left alone (it is the
        // only proof of ownership); missing/corrupt sidecars are removed.
        if read_manifest(staging, item_id).is_none() {
            remove_manifest_files(staging, item_id);
        }
        return;
    };
    for name in owned_file_names(staging, &ownership) {
        // Preserve completed recordings: they may be the user's only copy.
        if name.starts_with("final.") {
            continue;
        }
        let path = staging.join(&name);
        // Basename only; never allow manifest data to escape staging.
        if path.file_name().and_then(|n| n.to_str()) != Some(name.as_str()) {
            continue;
        }
        let _ = std::fs::remove_file(path);
    }
    // Remove the manifest itself plus the item's single-use probe temps.
    remove_manifest_files(staging, item_id);
    // Never remove_dir: staging is the user's dest dir, not a dedicated subfolder.
}

/// Whether `stem` already hosts Grab part files or subtitle sidecars (intake treats it as taken).
pub(crate) fn stem_reserved_in(names: &[String], stem: &str) -> bool {
    if stem.is_empty() {
        return false;
    }
    names.iter().any(|n| is_grab_part(n, stem)) || stem_has_subtitle_sidecar(names, stem)
}

/// Whether `stem` hosts a subtitle sidecar for any offered language (kept out of `is_grab_part` so removal never sweeps sidecars).
fn stem_has_subtitle_sidecar(names: &[String], stem: &str) -> bool {
    if stem.is_empty() {
        return false;
    }
    names.iter().any(|n| {
        strip_stem_suffix(n, stem)
            .and_then(|r| r.strip_suffix(".srt"))
            .is_some_and(|lang| subtitle_content_languages().any(|l| l == lang))
    })
}

/// Delete a row's dest-dir part files (never the finished file).
/// Delete a row's dest-dir part files (never the finished file).
/// Only legacy marked patterns (`PART_KINDS`, `.live.`) are matched: the
/// id-in-name scheme has no marker, so pattern-deleting it would risk user
/// files like `Movie.2024.mp4.part`. ID reuse is prevented separately by
/// the allocator's `staging_occupied_in` check.
pub fn clean_dest_parts(dest: &Path) {
    let (Some(dir), Some(stem)) = (dest.parent(), dest.file_stem().and_then(|s| s.to_str())) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && is_grab_part(name, stem)
        {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// `<output-stem>.<lang>.srt` beside `output` (allowlisted language, so it can never escape its directory).
pub(crate) fn sidecar_path_for(output: &Path, lang: &str) -> PathBuf {
    let stem = output
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "part".to_string());
    output
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(format!("{stem}.{lang}.srt"))
}

/// Best-effort sidecar collection (never fails the download; never clobbers an existing file).
pub(crate) fn collect_sidecar(src: &Path, dest: &Path, lang: &str) {
    if !src.exists() {
        return;
    }
    let dst = sidecar_path_for(dest, lang);
    // Never clobber a foreign sidecar that arrived mid-download; ours stays sweepable beside the part file.
    if let Err(e) = crate::file_names::rename_noreplace(src, &dst) {
        tracing::warn!(
            src = %src.display(),
            dst = %dst.display(),
            error = %e,
            "subtitle sidecar left beside the part file"
        );
    }
}

/// Stable identifier for a destination directory: FNV-1a 64 over the resolved
/// absolute path (canonical when it exists, lexical otherwise). Two
/// destinations that reuse an item id get distinct manifests, and a restart
/// that spells the same path differently — a symlinked Downloads dir, a
/// trailing slash, `.`/`..` segments — still finds the same one.
pub(crate) fn dest_key(dest: &Path) -> String {
    let normalized = std::fs::canonicalize(dest).unwrap_or_else(|_| lexical_absolute(dest));
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in normalized.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Absolute path with `.` dropped and `..` resolved lexically. No filesystem
/// access: the key must stay stable for a destination that does not exist yet.
fn lexical_absolute(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(component);
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Persistent manifest location: `$XDG_STATE_HOME/grab/video/{id}-{key}.json`.
/// The key scopes it to one destination dir, so an id reused elsewhere never
/// adopts this row's parts.
pub(crate) fn manifest_path(dest_dir: &Path, item_id: u64) -> PathBuf {
    state_root()
        .join("video")
        .join(format!("{item_id}-{}.manifest.json", dest_key(dest_dir)))
}

/// Previous manifest names, all inside the destination dir: the canonical
/// pre-migration name first, then three older variants. They are never read;
/// the startup sweep deletes the files by name so the dest dir stops
/// collecting Grab bookkeeping.
fn legacy_manifest_paths(dest_dir: &Path, item_id: u64) -> [PathBuf; 4] {
    [
        dest_dir.join(format!(".{item_id}.manifest.json")),
        dest_dir.join(format!(".grab-{item_id}-manifest.json")),
        dest_dir.join(format!("grab-{item_id}-.manifest.json")),
        dest_dir.join(format!("grab-{item_id}-manifest.json")),
    ]
}

/// Ownership proof from a manifest: exact staging names and/or a recorded prefix.
/// Both are manifest-anchored (written by the runner), never inferred.
struct ManifestOwnership {
    /// Exact basenames: `staging_name` plus its `.part`/`.ytdl` sidecars.
    exact: Vec<String>,
    /// Prefix (e.g., `"My Video.4."`); any file in the dir starting with it
    /// (except `final.*`) is owned.
    prefix: Option<String>,
}

fn manifest_ownership(dest_dir: &Path, item_id: u64) -> Option<ManifestOwnership> {
    manifest_ownership_of(&read_manifest(dest_dir, item_id)?)
}

/// Ownership fields of one already-parsed manifest.
fn manifest_ownership_of(m: &VideoManifest) -> Option<ManifestOwnership> {
    let mut exact = Vec::new();
    if let Some(base) = &m.staging_name {
        exact.push(base.clone());
        exact.push(format!("{base}.part"));
        exact.push(format!("{base}.ytdl"));
    }
    let prefix = m.staging_prefix.clone();
    if exact.is_empty() && prefix.is_none() {
        return None;
    }
    Some(ManifestOwnership { exact, prefix })
}

/// All owned basenames: exact names plus prefix matches (excluding `final.*`).
/// Confined to `dest_dir`; symlinks are not followed (only the dir's own
/// entries are considered).
fn owned_file_names(dest_dir: &Path, ownership: &ManifestOwnership) -> Vec<String> {
    let mut names = ownership.exact.clone();
    if let (Some(prefix), Ok(entries)) = (&ownership.prefix, std::fs::read_dir(dest_dir)) {
        for entry in entries.filter_map(|e| e.ok()) {
            // Only regular files; never follow symlinks out of the dir.
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if !ft.is_file() {
                continue;
            }
            if let Some(name) = entry.file_name().to_str()
                && name.starts_with(prefix.as_str())
                && !name.starts_with("final.")
                && !names.iter().any(|n| n == name)
            {
                names.push(name.to_string());
            }
        }
    }
    names
}

/// Delete the manifest sidecar files (canonical + legacy names), the item's
/// single-use probe JSONs, and the old dest-dir info JSONs. Every path here is
/// Grab-created bookkeeping, never user content.
fn remove_manifest_files(dest_dir: &Path, item_id: u64) {
    let _ = std::fs::remove_file(manifest_path(dest_dir, item_id));
    for path in legacy_manifest_paths(dest_dir, item_id) {
        let _ = std::fs::remove_file(path);
    }
    remove_private_temps(item_id);
    let _ = std::fs::remove_file(dest_dir.join(format!(".grab-info-{item_id}.json")));
    let _ = std::fs::remove_file(dest_dir.join(format!(".grab-live-{item_id}.json")));
}

/// Remove the item's single-use probe JSONs from the private scratch root.
pub(crate) fn remove_private_temps(item_id: u64) {
    let _ = std::fs::remove_file(info_json_path(item_id));
    let _ = std::fs::remove_file(live_info_json_path(item_id));
}

/// Delete exactly the files a manifest owns. The manifest's `staging_name`
/// and `staging_prefix` are the only proof of ownership.
///
/// A manifest that exists but records no ownership is left alone: it is the
/// only proof of ownership, and deleting it would make its files unreclaimable.
/// A missing or corrupt manifest still has its sidecar files removed (they are
/// ours), but nothing else is touched.
fn remove_manifest_owned_files(dest_dir: &Path, item_id: u64) {
    let Some(ownership) = manifest_ownership(dest_dir, item_id) else {
        // No ownership recorded. If a manifest file exists and is valid, leave
        // it alone. If it's missing/corrupt, remove the sidecars (ours).
        if read_manifest(dest_dir, item_id).is_none() {
            remove_manifest_files(dest_dir, item_id);
        }
        return;
    };
    remove_owned_files(dest_dir, &ownership);
    remove_manifest_files(dest_dir, item_id);
}

/// Delete exactly the files a manifest's ownership fields name.
fn remove_owned_files(dest_dir: &Path, ownership: &ManifestOwnership) {
    // Read owned names BEFORE deleting the manifest.
    let names = owned_file_names(dest_dir, ownership);
    for name in names {
        let path = dest_dir.join(&name);
        // Basename only; never allow manifest data to escape dest_dir.
        if path.file_name().and_then(|n| n.to_str()) != Some(name.as_str()) {
            continue;
        }
        let _ = std::fs::remove_file(path);
    }
}

/// Read the state-dir manifest for this destination. Manifest files left in
/// the destination dir by older versions are not read: the startup sweep
/// removes them (and their probe JSONs) by name.
pub(crate) fn read_manifest(dest_dir: &Path, item_id: u64) -> Option<VideoManifest> {
    read_manifest_file(&manifest_path(dest_dir, item_id))
}

/// Parse one manifest file. Corrupt or unreadable sidecars read as absent.
fn read_manifest_file(path: &Path) -> Option<VideoManifest> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub(crate) fn file_len(path: &Path) -> Option<u64> {
    std::fs::metadata(path).map(|m| m.len()).ok()
}

/// Write `bytes` to `path` with owner-only permissions (0600), without
/// blocking the async executor.
///
/// The manifests and probe info-JSON temp files can hold signed media URLs
/// and request headers, so they must not be world-readable. The bytes go to a
/// sibling temp first and are renamed into place: an interrupted write leaves
/// the previous file intact. `mode(0o600)` only applies at creation, so the
/// temp is removed first (previous crash debris) and opened with `create_new`:
/// the state dir can sit below a shared folder, and anything planted at the
/// temp path — a symlink, most importantly — must fail the open, not be
/// followed or truncated. Flushes before renaming: `write_all` only queues
/// into tokio's buffer, and a reader right after must see the whole thing.
#[cfg(unix)]
pub(crate) async fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt as _;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        ensure_private_dir(parent)?;
    }
    // Write beside the target and rename: an interrupted write leaves the
    // previous file intact instead of truncating it.
    let temp = private_temp_path(path);
    let _ = tokio::fs::remove_file(&temp).await;
    let mut file = open_new_private(&temp).await?;
    file.write_all(bytes).await?;
    file.flush().await?;
    drop(file);
    tokio::fs::rename(&temp, path).await
}

/// Sibling path for an atomic write, e.g. `info-1.json.tmp`.
fn private_temp_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Open `path` exclusively for writing with owner-only permissions: if
/// anything — a regular file, a symlink — is present, the open fails instead
/// of truncating it or following it. Split out so tests can pin the
/// exclusive open directly; `write_private`'s remove→open gap is closed by
/// it.
#[cfg(unix)]
async fn open_new_private(path: &Path) -> std::io::Result<tokio::fs::File> {
    tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .await
}

/// Non-Unix fallback: no permission bits to set, still async + flushed.
#[cfg(not(unix))]
pub(crate) async fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt as _;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        ensure_private_dir(parent)?;
    }
    let mut file = tokio::fs::File::create(path).await?;
    file.write_all(bytes).await?;
    file.flush().await?;
    Ok(())
}

/// Allocated bytes on disk (`None` off-Unix: callers treat that as dense).
#[cfg(unix)]
fn allocated_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(path).map(|m| m.blocks() * 512).ok()
}

#[cfg(not(unix))]
fn allocated_bytes(_path: &Path) -> Option<u64> {
    None
}

/// Whether a part file is a sparse shell (full apparent size, almost nothing on disk): never adopt it.
pub(crate) fn is_sparse_shell(path: &Path) -> bool {
    match (file_len(path), allocated_bytes(path)) {
        (Some(len), Some(allocated)) => len > 0 && allocated < len,
        _ => false,
    }
}

/// Upper bound for a sane unified temp: planned total + 10% estimate wobble + 8 MiB post-merge overhead. Pure.
pub(crate) fn unified_temp_limit(total: u64) -> u64 {
    total
        .saturating_add(total / 10)
        .saturating_add(8 * 1024 * 1024)
}

/// What the next attempt should do, decided from the sidecar and disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumePlan {
    /// Finished file already in place (adopt it).
    Finished,
    /// Temp incomplete or absent: re-spawn and let yt-dlp resume its own `.part` shell.
    Resume,
    /// (Re)download everything, wiping staging first.
    Fresh,
}

/// Inputs for [`resume_plan`], bundled so the signature stays small.
pub(crate) struct ResumeQuery<'a> {
    pub manifest: Option<&'a VideoManifest>,
    pub dest: &'a Path,
    pub staging: &'a Path,
    pub page_url: &'a str,
    pub quality: &'a str,
    pub video: Option<(&'a str, &'a str)>,
    pub audio: (&'a str, &'a str),
    /// Freshly selected combined total (`None` = unknown: oversize undetectable, bytes reusable).
    pub total: Option<u64>,
}

pub(crate) fn resume_plan(q: &ResumeQuery) -> ResumePlan {
    let Some(m) = q.manifest else {
        return ResumePlan::Fresh;
    };
    if !m.matches(q.page_url, q.quality, q.video, q.audio) {
        return ResumePlan::Fresh;
    }
    if let Some(final_bytes) = m.final_bytes
        && file_len(q.dest) == Some(final_bytes)
    {
        return ResumePlan::Finished;
    }
    // Oversize temps and sparse shells wipe and restart; anything else lets yt-dlp resume or download fresh.
    if let Some(temp) = discover_unified_output(q.staging, None) {
        let len = file_len(&temp);
        if q.total
            .is_some_and(|t| len.is_some_and(|n| n > unified_temp_limit(t)))
            || is_sparse_shell(&temp)
        {
            return ResumePlan::Fresh;
        }
    }
    ResumePlan::Resume
}
/// Whether a staging filename may be the unified download's claimed output (never fragments, `.part` shells, sidecars, or metadata).
/// The template is `{stem}.{id}.%(ext)s`, so the output is a media file; the
/// staging dir is the dest dir itself, so non-media files (photos, documents)
/// must never be claimed.
pub(crate) fn unified_candidate(file_name: &str) -> bool {
    let ext = Path::new(file_name).extension().and_then(|e| e.to_str());
    // Media containers yt-dlp can produce. Anything else (jpg, webp, json,
    // srt, part shells, temp files) is never the claimed output.
    const UNIFIED_MEDIA_EXTS: &[&str] = &[
        "mp4", "webm", "mkv", "mka", "ts", "m4a", "mp3", "ogg", "wav", "flac", "opus", "avi",
        "mov", "m4v", "aac", "weba", "flv", "f4v", "ogv", "wmv", "3gp", "3g2",
    ];
    ext.is_some_and(|e| UNIFIED_MEDIA_EXTS.iter().any(|m| m.eq_ignore_ascii_case(e)))
        && !file_name.starts_with('.')
        && !is_ytdlp_fragment(file_name)
}

/// yt-dlp's own merge temp names (`<stem>.f<id>.<ext>`): never the claimed output. Pure.
pub(crate) fn is_ytdlp_fragment(file_name: &str) -> bool {
    let Some(dot_f) = file_name.find(".f") else {
        return false;
    };
    let after_f = &file_name[dot_f + 2..];
    let digits = after_f.len()
        - after_f
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .len();
    digits > 0 && after_f[digits..].starts_with('.')
}

/// Locate the unified download's output in staging: `after_move` print when trustworthy, else scan. Both use `unified_candidate`.
pub(crate) fn discover_unified_output(staging: &Path, after_move: Option<&str>) -> Option<PathBuf> {
    if let Some(path) = after_move
        && let Ok(canonical) = std::fs::canonicalize(path)
        && canonical.starts_with(staging)
        && canonical.is_file()
        && let Some(name) = canonical.file_name().and_then(|n| n.to_str())
        && unified_candidate(name)
    {
        return Some(canonical);
    }
    std::fs::read_dir(staging)
        .ok()?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(unified_candidate)
        })
        .max_by_key(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Unique scratch dir per test (never a shared staging parent).
    fn unique_dir(tag: &str) -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        // Nanos timestamp: the counter resets and PIDs recycle across runs,
        // so neither alone guarantees uniqueness if a previous run left its dir behind.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "grab-staging-{tag}-{}-{n}-{nanos}",
            std::process::id()
        ))
    }

    #[test]
    fn native_manifest_staging_is_private_and_owned_cleanup_is_exact() {
        let dest = unique_dir("native-manifest-owned");
        std::fs::create_dir_all(&dest).expect("destination directory");
        let id = 8_000_021;
        let stage = ensure_manifest_staging_dir(&dest, id).expect("private manifest staging");
        let marker = stage.join(".grab-manifest-owner");
        assert!(marker.is_file());
        assert!(stage.join("video").parent().is_some());
        std::fs::write(stage.join("video-segment.bin"), b"segment").expect("staged segment");

        clean_manifest_staging_files(&dest, id);
        assert!(!stage.exists(), "only the marker-owned stage is removed");
        assert!(
            dest.is_dir(),
            "the user's destination directory is preserved"
        );
        let _ = std::fs::remove_dir_all(&dest);
    }

    #[test]
    fn startup_sweep_keeps_live_manifest_staging_and_reclaims_orphans() {
        let dest = unique_dir("native-manifest-sweep");
        std::fs::create_dir_all(&dest).expect("destination directory");
        let live_id = 8_000_023;
        let orphan_id = 8_000_024;
        let live = ensure_manifest_staging_dir(&dest, live_id).expect("live stage");
        let orphan = ensure_manifest_staging_dir(&dest, orphan_id).expect("orphan stage");
        std::fs::write(live.join("segment.bin"), b"live").expect("live resource");
        std::fs::write(orphan.join("segment.bin"), b"orphan").expect("orphan resource");

        sweep_dest_staging(&dest, &std::collections::HashSet::from([live_id]));

        assert!(
            live.join("segment.bin").is_file(),
            "queued row keeps its stage"
        );
        assert!(
            !orphan.exists(),
            "unreferenced marker-owned stage is reclaimed"
        );
        let _ = std::fs::remove_dir_all(&dest);
    }

    #[test]
    fn native_manifest_cleanup_preserves_unmarked_user_directory() {
        let dest = unique_dir("native-manifest-unmarked");
        std::fs::create_dir_all(&dest).expect("destination directory");
        let id = 8_000_022;
        let stage = dest.join(format!(".grab-manifest-{id}"));
        std::fs::create_dir(&stage).expect("user directory");
        let user_file = stage.join("notes.txt");
        std::fs::write(&user_file, b"user data").expect("user file");

        assert!(
            ensure_manifest_staging_dir(&dest, id).is_err(),
            "an existing unmarked directory must not be adopted"
        );
        clean_manifest_staging_files(&dest, id);
        assert_eq!(
            std::fs::read(&user_file).expect("user data remains"),
            b"user data"
        );
        let _ = std::fs::remove_dir_all(&dest);
    }

    fn test_manifest(staging_name: &str, dest_dir: Option<PathBuf>) -> VideoManifest {
        VideoManifest {
            page_url: "https://example.com".to_string(),
            quality: "1080p".to_string(),
            video_format_id: None,
            video_ext: "mp4".to_string(),
            audio_format_id: String::new(),
            audio_ext: String::new(),
            final_bytes: None,
            staging_name: Some(staging_name.to_string()),
            staging_prefix: None,
            dest_dir,
        }
    }

    fn write_state_manifest(dir: &Path, id: u64, manifest: &VideoManifest) {
        let path = manifest_path(dir, id);
        ensure_private_dir(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_string(manifest).unwrap()).unwrap();
    }

    /// Regression: info-JSON temp files must be owner-only (0600), even
    /// when a wider-mode file from a crashed run is already in place.
    #[tokio::test]
    #[cfg(unix)]
    async fn write_private_sets_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = unique_dir("write-private");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".grab-info-1.json");
        // Crash debris with 0644 permissions must not keep its mode.
        std::fs::write(&path, b"stale").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&path, b"{\"signed\":\"url\"}").await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"signed\":\"url\"}");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "info-JSON temp file must be owner-only");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A symlink planted at the temp-file path must never be followed: the
    /// write replaces the link itself, and the file it pointed at is left
    /// untouched. The exclusive open itself is pinned by
    /// `open_new_private_refuses_existing_paths`.
    #[tokio::test]
    #[cfg(unix)]
    async fn write_private_never_follows_a_symlink() {
        let dir = unique_dir("write-private-symlink");
        std::fs::create_dir_all(&dir).unwrap();
        let victim = dir.join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        let path = dir.join(".grab-info-2.json");
        std::os::unix::fs::symlink(&victim, &path).unwrap();
        write_private(&path, b"{\"signed\":\"url\"}").await.unwrap();
        assert_eq!(
            std::fs::read(&victim).unwrap(),
            b"keep",
            "symlink target must stay untouched"
        );
        let meta = std::fs::symlink_metadata(&path).unwrap();
        assert!(!meta.file_type().is_symlink(), "path must be a fresh file");
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"signed\":\"url\"}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Pins the exclusive open itself: an entry already at the path — a
    /// regular file or a symlink — must fail the open (`AlreadyExists`), not
    /// be truncated or followed. This is the check that fails if
    /// `create_new` is ever watered down; the remove→open race window itself
    /// is not deterministic from a test.
    #[tokio::test]
    #[cfg(unix)]
    async fn open_new_private_refuses_existing_paths() {
        let dir = unique_dir("open-new-private");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("exists");
        std::fs::write(&file, b"old").unwrap();
        let err = open_new_private(&file).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read(&file).unwrap(),
            b"old",
            "existing bytes stay untouched"
        );
        // A symlink fails the same way; its target is never opened.
        let victim = dir.join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        assert!(open_new_private(&link).await.is_err());
        assert_eq!(
            std::fs::read(&victim).unwrap(),
            b"keep",
            "symlink target stays untouched"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn success_path_leaves_no_bookkeeping_in_the_dest_dir() {
        // The runner's success tail: sweep the item's manifest-owned files and
        // its state-dir manifest. The dest dir itself is never removed.
        let staging = unique_dir("success-root");
        std::fs::create_dir_all(&staging).unwrap();
        let manifest = test_manifest("Title.42.mp4", Some(staging.clone()));
        std::fs::write(
            manifest_path(&staging, 42),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();
        std::fs::write(staging.join("Title.42.mp4.part"), b"scratch").unwrap();
        std::fs::write(staging.join("unrelated.txt"), b"keep").unwrap();
        sweep_staging_preserving_recordings(&staging, 42);
        assert!(
            !staging.join("Title.42.mp4.part").exists(),
            "item staging file is swept on success"
        );
        assert!(
            read_manifest(&staging, 42).is_none(),
            "state-dir manifest is swept on success"
        );
        assert!(staging.exists(), "dest dir is never removed");
        assert!(
            staging.join("unrelated.txt").exists(),
            "unrelated files are never touched"
        );
        let json: Vec<String> = dir_file_names(&staging)
            .into_iter()
            .filter(|n| n.ends_with(".json"))
            .collect();
        assert!(json.is_empty(), "JSON left in the dest dir: {json:?}");
        let _ = std::fs::remove_dir_all(&staging);
    }

    #[test]
    fn success_path_keeps_root_while_recording_remains() {
        // `final.*` preservation: the recording stays, manifest-owned scratch is swept.
        // The dest dir itself is never removed.
        let staging = unique_dir("success-keep");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("final.recording.mp4"), b"only copy").unwrap();
        // Create manifest owning the staging file.
        let manifest = VideoManifest {
            page_url: "https://example.com".to_string(),
            quality: "1080p".to_string(),
            video_format_id: None,
            video_ext: "mp4".to_string(),
            audio_format_id: String::new(),
            audio_ext: String::new(),
            final_bytes: None,
            staging_name: Some("Title.44.mp4".to_string()),
            staging_prefix: None,
            dest_dir: None,
        };
        write_state_manifest(&staging, 44, &manifest);
        std::fs::write(staging.join("Title.44.mp4.part"), b"scratch").unwrap();
        sweep_staging_preserving_recordings(&staging, 44);
        assert!(
            staging.join("final.recording.mp4").exists(),
            "completed recording is preserved"
        );
        assert!(
            !staging.join("Title.44.mp4.part").exists(),
            "scratch is swept around the recording"
        );
        assert!(staging.exists(), "dest dir is never removed");
        let _ = std::fs::remove_dir_all(&staging);
    }

    #[test]
    fn sweep_preserving_recordings_rejects_non_staging_files() {
        // Critical: files without manifests must survive the sweep.
        // Under manifest-only cleanup, only manifest-owned files are deleted.
        let staging = unique_dir("allowlist-reject");
        std::fs::create_dir_all(&staging).unwrap();
        // Legacy manifest (must be deleted - it's a manifest file)
        std::fs::write(staging.join("grab-42-.manifest.json"), b"{}").unwrap();
        // Part file WITHOUT manifest (must survive - no proof of ownership)
        std::fs::write(staging.join("grab-42-video.f137.mp4.part"), b"part").unwrap();
        // User files (must survive)
        std::fs::write(staging.join("grab-42-notes.txt"), b"user notes").unwrap();
        std::fs::write(staging.join("grab-42-export.zip"), b"user export").unwrap();
        // Bare file without prefix (must survive)
        std::fs::write(staging.join("video.mp4"), b"user video").unwrap();

        sweep_staging_preserving_recordings(&staging, 42);

        assert!(
            !staging.join("grab-42-.manifest.json").exists(),
            "staging manifest must be swept"
        );
        assert!(
            staging.join("grab-42-video.f137.mp4.part").exists(),
            "part file without manifest must survive (no ownership proof)"
        );
        assert!(
            staging.join("grab-42-notes.txt").exists(),
            "user's grab-42-notes.txt must survive the sweep"
        );
        assert!(
            staging.join("grab-42-export.zip").exists(),
            "user's grab-42-export.zip must survive the sweep"
        );
        assert!(
            staging.join("video.mp4").exists(),
            "bare video.mp4 must survive the sweep"
        );
        let _ = std::fs::remove_dir_all(&staging);
    }

    #[test]
    fn sweep_dest_staging_rejects_non_staging_files() {
        // Same allowlist pin for the orphan sweep: only known patterns go,
        // user files with the prefix survive.
        let dest_dir = unique_dir("dest-allowlist-reject");
        std::fs::create_dir_all(&dest_dir).unwrap();
        let mut keep = std::collections::HashSet::new();
        keep.insert(99u64); // live row
        // Orphan staging files (id 42 not in keep, must be deleted)
        std::fs::write(dest_dir.join("grab-42-.manifest.json"), b"{}").unwrap();
        // User files with matching prefix (must survive)
        std::fs::write(dest_dir.join("grab-42-notes.txt"), b"user notes").unwrap();
        // Live row's files (must survive)
        std::fs::write(dest_dir.join("grab-99-.manifest.json"), b"{}").unwrap();

        sweep_dest_staging(&dest_dir, &keep);

        assert!(
            !dest_dir.join("grab-42-.manifest.json").exists(),
            "orphan staging manifest must be swept"
        );
        assert!(
            dest_dir.join("grab-42-notes.txt").exists(),
            "user's grab-42-notes.txt must survive the orphan sweep"
        );
        assert!(
            dest_dir.join("grab-99-.manifest.json").exists(),
            "live row's files must survive"
        );
        let _ = std::fs::remove_dir_all(&dest_dir);
    }

    #[test]
    fn ensure_staging_dir_in_rejects_symlinked_dest_dir() {
        // Critical: if the dest dir is a symlink, ensure must error rather
        // than allow writes through the link to an arbitrary target.
        let base = unique_dir("symlink-dest");
        std::fs::create_dir_all(&base).unwrap();
        let target = base.join("real-target");
        std::fs::create_dir_all(&target).unwrap();
        let link = base.join("link-dest");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let res = ensure_staging_dir_in(&link, &link);
        assert!(
            res.is_err(),
            "symlinked dest dir must be refused, not followed"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn sweep_dest_staging_leaves_symlink_target_alone() {
        // Critical: sweep must not follow symlinks in the dest dir to delete
        // the target's files.
        let base = unique_dir("sweep-symlink");
        std::fs::create_dir_all(&base).unwrap();
        let target = base.join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("grab-42-.manifest.json"), b"{}").unwrap();
        let dest_dir = base.join("dest");
        std::fs::create_dir_all(&dest_dir).unwrap();
        std::os::unix::fs::symlink(&target, dest_dir.join("link")).unwrap();
        // A real orphan staging file in dest_dir (should be swept)
        std::fs::write(dest_dir.join("grab-42-.manifest.json"), b"{}").unwrap();

        let keep = std::collections::HashSet::new();
        sweep_dest_staging(&dest_dir, &keep);

        assert!(
            !dest_dir.join("grab-42-.manifest.json").exists(),
            "orphan staging file in dest dir must be swept"
        );
        assert!(
            target.join("grab-42-.manifest.json").exists(),
            "symlink target's files must survive the sweep"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn drop_empty_staging_root_removes_empty_root() {
        let base = unique_dir("drop-empty");
        let root = base.join(".grab-video");
        std::fs::create_dir_all(&root).unwrap();
        drop_empty_staging_root(&root);
        assert!(!root.exists(), "empty root is dropped");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn drop_empty_staging_root_keeps_root_with_live_sibling() {
        // A sibling item still staging (e.g. another playlist item
        // mid-download) keeps the root: remove_dir only succeeds when empty.
        let base = unique_dir("drop-sibling");
        let root = base.join(".grab-video");
        let sibling = root.join("43");
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("chunk.part"), b"in-flight").unwrap();
        drop_empty_staging_root(&root);
        assert!(root.exists(), "root with a live sibling item is kept");
        assert!(
            sibling.join("chunk.part").exists(),
            "sibling scratch is untouched"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn drop_empty_staging_root_ignores_missing_root() {
        let base = unique_dir("drop-missing");
        // Never created: must be a no-op, never an error or a panic.
        drop_empty_staging_root(&base.join(".grab-video"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn drop_empty_staging_root_refuses_symlink_root() {
        // A planted symlink at the root is refused by the guards, never
        // followed: the link and its target stay untouched.
        let base = unique_dir("drop-symlink");
        std::fs::create_dir_all(&base).unwrap();
        let target = base.join("real-target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("precious"), b"do not touch").unwrap();
        let link = base.join(".grab-video");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        drop_empty_staging_root(&link);
        assert!(link.is_symlink(), "planted symlink root is refused");
        assert_eq!(
            std::fs::read(target.join("precious")).unwrap(),
            b"do not touch",
            "link target is untouched"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn staging_name_matches_id_accepts_own_files() {
        // Own files under the id-in-name scheme must match.
        assert!(staging_name_matches_id("Title.4.mp4", 4));
        assert!(staging_name_matches_id("Title.4.mp4.part", 4));
        assert!(staging_name_matches_id("Title.4.mp4.ytdl", 4));
        assert!(staging_name_matches_id("Title.4-1.mp4", 4));
        assert!(staging_name_matches_id("Title.4-1.mp4.part", 4));
        assert!(staging_name_matches_id("My Video.42.webm.part", 42));
        // Other ids must not match.
        assert!(!staging_name_matches_id("Title.4.mp4", 5));
        assert!(!staging_name_matches_id("Title.4.mp4.part", 44));
    }

    #[test]
    fn staging_name_matches_id_rejects_user_files() {
        // User files must never match, even with tricky names.
        // Note: `my.backup.4.mp4` DOES match id 4 by pattern (the id is the
        // last dot-component), but that's safe: pattern-based deletion only
        // ever touches `.part`/`.ytdl` sidecars, never a bare media file.
        assert!(!staging_name_matches_id("linux-5.4.0.tar.gz", 4));
        assert!(!staging_name_matches_id("Title.mp4", 4));
        assert!(!staging_name_matches_id("Title.mp4.part", 4));
        assert!(!staging_name_matches_id("grab-4-notes.txt", 4));
        assert!(!staging_name_matches_id("4.mp4", 4));
    }

    #[test]
    fn manifest_loss_still_occupies_id_via_filename() {
        // Blocker 1: a lost manifest must not orphan the id into reuse.
        // The id-in-name scheme keeps the file attributable.
        let dir = unique_dir("manifest-loss-occupancy");
        std::fs::create_dir_all(&dir).unwrap();
        // Simulate a crashed capture: id-in-name files, no manifest.
        std::fs::write(dir.join("Title.7.mp4.part"), b"partial").unwrap();
        std::fs::write(dir.join("Title.7.mp4.ytdl"), b"state").unwrap();
        // No manifest written.

        assert!(
            staging_occupied_in(&dir_file_names(&dir), 7),
            "id 7 must stay occupied via id-in-name even with no manifest"
        );
        assert!(
            !staging_occupied_in(&dir_file_names(&dir), 8),
            "unrelated id 8 must not be occupied"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn staging_occupied_in_uses_a_prelisted_directory() {
        let names = vec![
            "Title.7.mp4.part".to_string(),
            ".9.manifest.json".to_string(),
            "grab-11-notes.txt".to_string(),
            "Title.13.live.mp4".to_string(),
        ];
        assert!(staging_occupied_in(&names, 7));
        assert!(staging_occupied_in(&names, 9));
        assert!(staging_occupied_in(&names, 11));
        assert!(staging_occupied_in(&names, 13));
        assert!(!staging_occupied_in(&names, 8));
    }

    #[test]
    fn try_dir_file_names_distinguishes_missing_from_unreadable() {
        let base = unique_dir("try-dir-file-names");
        assert!(try_dir_file_names(&base).unwrap().is_empty());

        let file = base.with_extension("not-a-directory");
        std::fs::write(&file, b"not a directory").unwrap();
        assert!(try_dir_file_names(&file).is_err());

        let _ = std::fs::remove_file(file);
    }

    #[test]
    fn manifest_loss_sweep_reclaims_sidecars_preserves_base() {
        // Without a manifest, id-in-name sidecars are NOT swept by pattern:
        // without a marker, `Title.7.mp4.part` is indistinguishable from a
        // user's own file. Deletion requires the manifest's exact name.
        // ID reuse is still prevented by the allocator's `staging_occupied_in` check.
        let dir = unique_dir("manifest-loss-sweep");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Title.7.mp4.part"), b"partial").unwrap();
        std::fs::write(dir.join("Title.7.mp4.ytdl"), b"state").unwrap();
        std::fs::write(dir.join("Title.7.mp4"), b"maybe-a-recording").unwrap();
        std::fs::write(dir.join("user-video.mp4"), b"user file").unwrap();
        // No manifest.

        sweep_staging_preserving_recordings(&dir, 7);

        assert!(
            dir.join("Title.7.mp4.part").exists(),
            "ambiguous id-in-name .part must NOT be pattern-deleted"
        );
        assert!(
            dir.join("Title.7.mp4.ytdl").exists(),
            "ambiguous id-in-name .ytdl must NOT be pattern-deleted"
        );
        assert!(
            dir.join("Title.7.mp4").exists(),
            "bare media file must be preserved without a manifest to confirm it"
        );
        assert!(
            dir.join("user-video.mp4").exists(),
            "user file must survive"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_manifest_files_still_attributable() {
        // A corrupt manifest reads as None; the id-in-name fallback must
        // still attribute the files so they are not orphaned.
        let dir = unique_dir("corrupt-manifest");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".7.manifest.json"), b"not valid json{{").unwrap();
        std::fs::write(dir.join("Title.7.mp4.part"), b"partial").unwrap();

        assert!(
            read_manifest(&dir, 7).is_none(),
            "corrupt manifest must read as None"
        );
        assert!(
            staging_occupied_in(&dir_file_names(&dir), 7),
            "id 7 must stay occupied via id-in-name despite corrupt manifest"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_rows_same_stem_get_distinct_files() {
        // Blocker 3: two live rows with the same stem must not collide.
        // The id in the name keeps them distinct.
        let dir = unique_dir("two-rows-same-stem");
        std::fs::create_dir_all(&dir).unwrap();
        // Simulate two claimed staging files for different ids, same stem.
        std::fs::write(dir.join("Title.4.mp4.part"), b"row4").unwrap();
        std::fs::write(dir.join("Title.9.mp4.part"), b"row9").unwrap();

        assert!(
            staging_occupied_in(&dir_file_names(&dir), 4),
            "id 4 occupied"
        );
        assert!(
            staging_occupied_in(&dir_file_names(&dir), 9),
            "id 9 occupied"
        );

        // Without a manifest, the sweep must NOT pattern-delete: Title.4.mp4.part
        // is indistinguishable from a user's own file. Deletion requires the
        // manifest's exact name.
        sweep_staging_preserving_recordings(&dir, 4);
        assert!(
            dir.join("Title.4.mp4.part").exists(),
            "row 4's part must survive without a manifest to confirm it"
        );
        assert!(
            dir.join("Title.9.mp4.part").exists(),
            "row 9's sibling part must survive row 4's sweep"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn orphan_sweep_attributes_via_id_in_name() {
        // Orphan id-in-name files are NOT swept by pattern: without a marker,
        // Title.11.mp4.part is indistinguishable from a user's own file.
        // sweep_dest_staging only removes legacy marked patterns.
        let dir = unique_dir("orphan-id-in-name");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Title.11.mp4.part"), b"orphan").unwrap();
        std::fs::write(dir.join("Title.12.mp4.part"), b"live").unwrap();
        let mut keep = std::collections::HashSet::new();
        keep.insert(12u64);

        sweep_dest_staging(&dir, &keep);

        assert!(
            dir.join("Title.11.mp4.part").exists(),
            "orphan id 11's part must survive: no marker, no deletion"
        );
        assert!(
            dir.join("Title.12.mp4.part").exists(),
            "live id 12's part must survive"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_dest_staging_preserves_user_live_file() {
        // A user's own MyRecording.42.live.mp4 must survive sweep_dest_staging.
        // No manifest = no proof of ownership = preserve.
        let dir = unique_dir("user-live");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("MyRecording.42.live.mp4"), b"user recording").unwrap();

        sweep_dest_staging(&dir, &std::collections::HashSet::new());

        assert!(
            dir.join("MyRecording.42.live.mp4").exists(),
            "user .live. file must be preserved"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_dest_staging_preserves_manifest_less_part_file() {
        // Title.42.mp4.part with NO manifest must survive. This is the most
        // important regression: filename alone is not ownership proof.
        let dir = unique_dir("no-manifest-part");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Title.42.mp4.part"), b"user data").unwrap();

        sweep_dest_staging(&dir, &std::collections::HashSet::new());

        assert!(
            dir.join("Title.42.mp4.part").exists(),
            "manifest-less part file must be preserved"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_sweep_removes_manifest_owned_file() {
        // With a state manifest recording staging_name, the exact files are
        // removed.
        let dir = unique_dir("manifest-owned");
        std::fs::create_dir_all(&dir).unwrap();

        let manifest = VideoManifest {
            page_url: "https://example.com".to_string(),
            quality: "1080p".to_string(),
            video_format_id: None,
            video_ext: "mp4".to_string(),
            audio_format_id: String::new(),
            audio_ext: String::new(),
            final_bytes: None,
            staging_name: Some("Title.42.mp4".to_string()),
            staging_prefix: None,
            dest_dir: Some(dir.clone()),
        };
        write_state_manifest(&dir, 42, &manifest);
        std::fs::write(dir.join("Title.42.mp4.part"), b"staging").unwrap();

        sweep_state_staging(&std::collections::HashSet::new());

        assert!(
            !dir.join("Title.42.mp4.part").exists(),
            "manifest-owned file should be removed"
        );
        assert!(
            read_manifest(&dir, 42).is_none(),
            "manifest should be removed after cleanup"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prefix_owned_files_are_reclaimed() {
        // A manifest recording staging_prefix owns every file under it
        // (except final.*): the unified path's parts, ytdl, and fragments.
        let dir = unique_dir("prefix-owned");
        std::fs::create_dir_all(&dir).unwrap();

        let manifest = VideoManifest {
            page_url: "https://example.com".to_string(),
            quality: "1080p".to_string(),
            video_format_id: None,
            video_ext: "mp4".to_string(),
            audio_format_id: String::new(),
            audio_ext: String::new(),
            final_bytes: None,
            staging_name: None,
            staging_prefix: Some("My Video.42.".to_string()),
            dest_dir: Some(dir.clone()),
        };
        write_state_manifest(&dir, 42, &manifest);
        std::fs::write(dir.join("My Video.42.f137.mp4.part"), b"part").unwrap();
        std::fs::write(dir.join("My Video.42.f251.webm.part"), b"part").unwrap();
        std::fs::write(dir.join("My Video.42.mp4.ytdl"), b"meta").unwrap();
        // A recording another leg owns must survive.
        std::fs::write(dir.join("final.recording.mp4"), b"keep").unwrap();
        // A user file that merely shares a word must survive.
        std::fs::write(dir.join("My Video.1080p.srt"), b"keep").unwrap();

        remove_manifest_owned_files(&dir, 42);

        assert!(
            !dir.join("My Video.42.f137.mp4.part").exists(),
            "prefix-owned part must go"
        );
        assert!(
            !dir.join("My Video.42.f251.webm.part").exists(),
            "prefix-owned part must go"
        );
        assert!(
            !dir.join("My Video.42.mp4.ytdl").exists(),
            "prefix-owned sidecar must go"
        );
        assert!(
            dir.join("final.recording.mp4").exists(),
            "another leg's recording must survive"
        );
        assert!(
            dir.join("My Video.1080p.srt").exists(),
            "user file outside the prefix must survive"
        );
        assert!(
            read_manifest(&dir, 42).is_none(),
            "manifest goes after its files"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_without_ownership_survives_sweep() {
        // A valid manifest that records neither staging_name nor
        // staging_prefix is left alone: it is the only proof of ownership,
        // and deleting it would make its files permanently unreclaimable.
        let dir = unique_dir("manifest-no-ownership");
        std::fs::create_dir_all(&dir).unwrap();

        let manifest = VideoManifest {
            page_url: "https://example.com".to_string(),
            quality: "1080p".to_string(),
            video_format_id: None,
            video_ext: "mp4".to_string(),
            audio_format_id: String::new(),
            audio_ext: String::new(),
            final_bytes: None,
            staging_name: None,
            staging_prefix: None,
            dest_dir: Some(dir.clone()),
        };
        write_state_manifest(&dir, 42, &manifest);
        std::fs::write(dir.join("orphan.mp4.part"), b"scratch").unwrap();

        remove_manifest_owned_files(&dir, 42);

        assert!(
            read_manifest(&dir, 42).is_some(),
            "manifest without ownership must survive the sweep"
        );
        assert!(
            dir.join("orphan.mp4.part").exists(),
            "files without ownership proof must survive"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unified_template_never_equals_dest() {
        // Staging is the dest dir itself: if the template could equal
        // `dest`, the claim rename would target the file onto itself and
        // fail with "file exists" after a successful download. The item id
        // in the template keeps them distinct.
        let staging = unique_dir("template-dest");
        std::fs::create_dir_all(&staging).unwrap();
        let dest = staging.join("My Video.mp4");
        let template = crate::video_argv::unified_output_template(&staging, &dest, 42);
        // yt-dlp substitutes %(ext)s; simulate the mp4 case.
        let produced = staging.join("My Video.42.mp4");
        assert_ne!(
            produced, dest,
            "template output must never equal the destination path"
        );
        assert!(
            template
                .to_str()
                .unwrap()
                .starts_with(staging.to_str().unwrap()),
            "template stays in the staging dir"
        );

        let _ = std::fs::remove_dir_all(&staging);
    }

    /// Fresh profiles have no `grab/video` chain: `write_private` builds it
    /// owner-only, and every dir it creates is 0700.
    #[tokio::test]
    #[cfg(unix)]
    async fn write_private_creates_private_parent_chain() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = unique_dir("write-parent");
        let path = root.join("grab/video/x.json");
        write_private(&path, b"{}").await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{}");
        for dir in [&root, &root.join("grab"), &root.join("grab/video")] {
            let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{} must be owner-only", dir.display());
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An item id reused in a second destination must not read the first
    /// destination's manifest: the file name carries a destination key.
    #[test]
    fn manifest_is_scoped_to_its_destination() {
        let dest_a = unique_dir("manifest-scope-a");
        let dest_b = unique_dir("manifest-scope-b");
        std::fs::create_dir_all(&dest_a).unwrap();
        std::fs::create_dir_all(&dest_b).unwrap();
        let id = 8_000_001u64;
        let path = manifest_path(&dest_a, id);
        let manifest = test_manifest("v.8000001.mp4", Some(dest_a.clone()));
        std::fs::write(&path, serde_json::to_string(&manifest).unwrap()).unwrap();

        assert_eq!(read_manifest(&dest_a, id).as_ref(), Some(&manifest));
        assert!(
            read_manifest(&dest_b, id).is_none(),
            "an id reused in another destination must not adopt this manifest"
        );
        assert_ne!(manifest_path(&dest_a, id), manifest_path(&dest_b, id));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dest_a);
        let _ = std::fs::remove_dir_all(&dest_b);
    }

    /// Startup reclaims state manifests whose `(destination, id)` is gone,
    /// deleting exactly the files that manifest owns, and clears stale probe
    /// temps while it is there.
    #[test]
    fn state_sweep_reclaims_orphans_and_keeps_live_rows() {
        let live = unique_dir("state-sweep-live");
        let orphan = unique_dir("state-sweep-orphan");
        std::fs::create_dir_all(&live).unwrap();
        std::fs::create_dir_all(&orphan).unwrap();
        let live_id = 8_000_002u64;
        let orphan_id = 8_000_003u64;
        for (dir, id) in [(&live, live_id), (&orphan, orphan_id)] {
            let name = format!("v.{id}.mp4");
            std::fs::write(dir.join(format!("{name}.part")), b"scratch").unwrap();
            let manifest = test_manifest(&name, Some(dir.clone()));
            std::fs::write(
                manifest_path(dir, id),
                serde_json::to_string(&manifest).unwrap(),
            )
            .unwrap();
        }
        let stale_temp = info_json_path(orphan_id);
        ensure_private_dir(stale_temp.parent().unwrap()).unwrap();
        std::fs::write(&stale_temp, b"{}").unwrap();

        let keep =
            std::collections::HashSet::from([(live.to_string_lossy().into_owned(), live_id)]);
        sweep_state_staging(&keep);

        assert!(
            read_manifest(&live, live_id).is_some(),
            "a queued row keeps its resume manifest"
        );
        assert!(live.join(format!("v.{live_id}.mp4.part")).exists());
        assert!(
            read_manifest(&orphan, orphan_id).is_none(),
            "orphan state manifest is reclaimed"
        );
        assert!(
            !orphan.join(format!("v.{orphan_id}.mp4.part")).exists(),
            "the orphan manifest's owned part is reclaimed"
        );
        assert!(!stale_temp.exists(), "startup clears stale probe temps");

        let _ = std::fs::remove_file(manifest_path(&live, live_id));
        let _ = std::fs::remove_dir_all(&live);
        let _ = std::fs::remove_dir_all(&orphan);
    }

    /// A crash mid-write can leave an unreadable manifest for a queued row.
    /// The sweep must not delete it: the row still needs to resume (or at
    /// least keep its parts reclaimable) despite the bad sidecar.
    #[test]
    fn state_sweep_keeps_a_queued_rows_unreadable_manifest() {
        let dir = unique_dir("state-sweep-corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let id = 8_000_005u64;
        let path = manifest_path(&dir, id);
        ensure_private_dir(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{truncated").unwrap();
        let keep = std::collections::HashSet::from([(dir.to_string_lossy().into_owned(), id)]);

        sweep_state_staging(&keep);
        assert!(
            path.exists(),
            "a queued row keeps its manifest even when unreadable"
        );

        sweep_state_staging(&std::collections::HashSet::new());
        assert!(
            !path.exists(),
            "an orphaned unreadable manifest is reclaimed"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Discarding a row removes its state manifest and both probe temps,
    /// including the old dest-dir names from pre-migration versions.
    #[test]
    fn clean_staging_files_removes_state_and_probe_temps() {
        let dir = unique_dir("clean-state");
        std::fs::create_dir_all(&dir).unwrap();
        let id = 8_000_004u64;
        let name = format!("v.{id}.mp4");
        std::fs::write(dir.join(format!("{name}.part")), b"scratch").unwrap();
        std::fs::write(dir.join("keep.txt"), b"user").unwrap();
        let manifest = test_manifest(&name, Some(dir.clone()));
        std::fs::write(
            manifest_path(&dir, id),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();
        ensure_private_dir(&runtime_root()).unwrap();
        let info = info_json_path(id);
        let live = live_info_json_path(id);
        std::fs::write(&info, b"{}").unwrap();
        std::fs::write(&live, b"{}").unwrap();
        let legacy_info = dir.join(format!(".grab-info-{id}.json"));
        let legacy_live = dir.join(format!(".grab-live-{id}.json"));
        std::fs::write(&legacy_info, b"{}").unwrap();
        std::fs::write(&legacy_live, b"{}").unwrap();

        clean_staging_files(&dir, id);

        assert!(!dir.join(format!("{name}.part")).exists());
        assert!(read_manifest(&dir, id).is_none(), "state manifest must go");
        assert!(!info.exists(), "runtime probe temp must go with the row");
        assert!(!live.exists(), "runtime live temp must go with the row");
        assert!(!legacy_info.exists(), "old dest-dir info JSON must go");
        assert!(!legacy_live.exists(), "old dest-dir live JSON must go");
        assert!(dir.join("keep.txt").exists(), "user files stay");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
