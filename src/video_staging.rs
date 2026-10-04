//! Video staging/parts/manifest/resume scratch dirs and templates.
//! Consumed by the runner/engine through the `video` facade.

use crate::video_prefs::subtitle_content_languages;
use crate::video_tools::VideoError;
use gettextrs::gettext;
use std::path::{Path, PathBuf};

/// Shared root for tiny engine scratch (cookie dumps, probe output). Stays on
/// tmpfs: only small files ever land here. Bulky download parts stage beside
/// the destination instead — `/tmp` is RAM-backed on many systems, so staging
/// there fills memory and fails large downloads with "no space left".
pub fn staging_root() -> PathBuf {
    std::env::temp_dir().join("grab-video")
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

/// Whether any staging file exists for this id: the id allocator must skip
/// it so a fresh row never lands on a leftover.
pub fn staging_occupied(dest_dir: &Path, item_id: u64) -> bool {
    let prefix = format!("grab-{item_id}-");
    // Manifest exists = occupied (it tracks the exact staging name).
    if manifest_path(dest_dir, item_id).exists() {
        return true;
    }
    // Legacy patterns: grab-{id}- prefix (safe) and .{id}.live. (has marker, safe).
    // The bare .{id}. pattern is deliberately NOT checked: it false-positives
    // on user files like linux-5.4.0.tar.gz.
    let live_pattern = format!(".{item_id}.live.");
    std::fs::read_dir(dest_dir)
        .ok()
        .map(|entries| {
            entries.filter_map(|e| e.ok()).any(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| n.starts_with(&prefix) || n.contains(&live_pattern))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// Create a staging dir, verifying it stays under the staging root (a pre-planted symlink must not redirect parts).
pub fn ensure_staging_dir(dir: &Path) -> Result<PathBuf, VideoError> {
    ensure_staging_dir_in(&staging_root(), dir)
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

/// The staging leaf must be a real directory, never a symlink: build the
/// parent chain, then create the leaf atomically, so a pre-planted link is
/// refused up front and a link racing for the name can't be followed into
/// place. Staging names are item ids, so unlike shareable folders there is
/// nothing to dedupe to — refusing loudly is the safe behavior.
fn ensure_real_staging_leaf(dir: &Path) -> Result<(), VideoError> {
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
            match std::fs::create_dir(dir) {
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

/// Whether a `grab-<id>-<suffix>` filename is a known Grab staging file.
/// Only these are safe to delete; a user's own `grab-<id>-notes.txt` must survive.
fn is_grab_staging_suffix(suffix: &str) -> bool {
    // .manifest.json: exact match (dot-prefixed, hidden from file views)
    if suffix == ".manifest.json" {
        return true;
    }
    // yt-dlp sidecars: *.part, *.ytdl (appended to the filenames we pass it)
    if suffix.ends_with(".part") || suffix.ends_with(".ytdl") {
        return true;
    }
    // Bare media extensions (e.g., "mp4" from grab-<id>.mp4): the base file
    // yt-dlp writes before appending .part.
    if matches!(
        suffix,
        "mp4" | "webm" | "mkv" | "m4a" | "mp3" | "ogg" | "wav" | "flac" | "opus"
    ) {
        return true;
    }
    // Remux/format parts: <kind>.<ext> where kind is a known Grab kind
    // (video, audio, live) and ext ends with a media extension.
    // Handles video.f137.mp4, live.mp4, etc. Conservative: require the dot.
    if let Some((kind, _rest)) = suffix.split_once('.') {
        let kind_ok = matches!(kind, "video" | "audio" | "live");
        // Get the last extension (e.g., "mp4" from "video.f137.mp4")
        let ext_ok = suffix
            .rsplit('.')
            .next()
            .map(|e| {
                matches!(
                    e,
                    "mp4" | "webm" | "mkv" | "m4a" | "mp3" | "ogg" | "wav" | "flac" | "opus"
                )
            })
            .unwrap_or(false);
        if kind_ok && ext_ok {
            return true;
        }
    }
    false
}

/// Remove Grab staging files for an item in the destination dir.
/// Only deletes files matching known staging patterns; never the dir itself,
/// other files, or a user's own `grab-<id>-*` files.
pub fn clean_staging_files(dest_dir: &Path, item_id: u64) {
    let prefix_hyphen = format!("grab-{item_id}-");
    let prefix_dot = format!("grab-{item_id}.");
    // Hidden manifest (dot-prefixed) plus legacy names.
    let hidden_manifest = format!(".grab-{item_id}-manifest.json");
    let legacy_manifests = [
        format!("grab-{item_id}-.manifest.json"),
        format!("grab-{item_id}-manifest.json"),
    ];
    if let Ok(entries) = std::fs::read_dir(dest_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name();
            let name_str = name.to_str().unwrap_or("");
            // Hidden or legacy manifest: always clean.
            if name_str == hidden_manifest || legacy_manifests.iter().any(|m| m == name_str) {
                let _ = std::fs::remove_file(entry.path());
                continue;
            }
            // Match grab-<id>-* (hyphen) or grab-<id>.* (dot, for part_path files).
            let suffix = name_str
                .strip_prefix(&prefix_hyphen)
                .or_else(|| name_str.strip_prefix(&prefix_dot));
            if let Some(suffix) = suffix
                && is_grab_staging_suffix(suffix)
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
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

/// Reclaim one orphan staging dir: the scratch goes, completed `final.*`
/// recordings stay (do not delete the user's only copy). The dir itself is
/// removed only if nothing worth keeping remains, so it stays skipped by the
/// id allocator.
/// Reclaim per-item staging dirs with no live row (crash/kill leftovers: only
/// restored rows reuse their ids, so nothing swept can resume). Only numeric
/// dir names are touched — the `grab-cookies-*.txt` files and anything else
/// under the root are left alone. Runs at startup after the queue is restored,
/// before any worker starts, so nothing live is removed.
/// Sweep one destination's staging files (`grab-<id>-*`): files for item IDs
/// with no live row are reclaimed. A missing dest dir is a no-op.
pub fn sweep_dest_staging(dest_dir: &Path, keep: &std::collections::HashSet<u64>) {
    let Ok(entries) = std::fs::read_dir(dest_dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name();
        let name = name.to_str().unwrap_or("");
        // Parse `grab-<id>-*` to get the item ID. Recordings (`grab-<id>-final.*`)
        // are the user's only copy — preserve them. Only delete known staging
        // file patterns; a user's own `grab-<id>-notes.txt` must survive.
        if let Some(rest) = name.strip_prefix("grab-")
            && let Some((id_str, suffix)) = rest.split_once('-')
            && let Ok(id) = id_str.parse::<u64>()
            && !keep.contains(&id)
            && !suffix.starts_with("final.")
            && is_grab_staging_suffix(suffix)
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
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
    /// Staging basename (e.g., "My Video.mp4" or "My Video-1.mp4").
    /// Lets cleanup find the temp files without an ID in the name.
    pub(crate) staging_name: Option<String>,
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

/// Render a path as a yt-dlp `-o` template: double literal `%` (genuine `%(name)s` fields left intact).
pub(crate) fn ytdlp_output_template(path: &Path) -> String {
    let s = path.to_string_lossy();
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        out.push(c);
        if c == '%' && !matches!(chars.clone().next(), Some('(')) {
            out.push('%');
        }
    }
    out
}

/// Grab-namespaced part infixes (the only names `clean_dest_parts` touches).
const PART_KINDS: &[&str] = &["video.", "audio.", "hls.", "live.", "live-"];

/// Reserve a `final.<n>.<ext>` remux slot via an atomic `.lease` sidecar (claim is check-then-use across overlapping attempts; fails closed).
pub(crate) fn reserve_remux_temp(staging: &Path, ext: &str) -> Result<PathBuf, VideoError> {
    let taken = dir_file_names(staging);
    for n in 1..=9999u32 {
        let name = format!("final.{n}.{ext}");
        if taken
            .iter()
            .any(|t| t == &name || t == &format!("{name}.lease"))
        {
            continue;
        }
        let lease = staging.join(format!("{name}.lease"));
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&lease)
        {
            Ok(_) => return Ok(staging.join(name)),
            // Someone else claimed it between the scan and the create.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            // Unwritable staging fails closed (a guess here would share one slot).
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
    // The bare {id}. pattern is NOT checked: it false-positives on user files.
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

/// File names directly inside `dir` (unreadable dirs read as empty).
pub(crate) fn dir_file_names(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Reclaim `final.<n>.<ext>.part` leftovers from attempts that died mid-ffmpeg (never completed recordings).
pub fn sweep_partial_remuxes(staging: &Path) {
    for name in dir_file_names(staging) {
        if name.starts_with("final.") && name.ends_with(".part") {
            let _ = std::fs::remove_file(staging.join(&name));
        }
    }
}

/// Remove a leg's staging scratch, preserving completed `grab-<id>-final.*` recordings (do not delete the user's only copy).
/// Only touches files with the item's `grab-<id>-` prefix; never the dest dir itself or other files.
pub fn sweep_staging_preserving_recordings(staging: &Path, item_id: u64) {
    let prefix = format!("grab-{item_id}-");
    // Hidden manifest: `.{id}.manifest.json` (new) and `.grab-{id}-manifest.json` (legacy).
    let hidden_manifest = format!(".{item_id}.manifest.json");
    let legacy_hidden = format!(".grab-{item_id}-manifest.json");
    // Exact staging names from the manifest: never pattern-match user files.
    // A bystander like `linux-5.4.0.tar.gz` must survive row id 4.
    let manifest_names: Vec<String> = read_manifest(staging, item_id)
        .and_then(|m| m.staging_name)
        .map(|base| {
            vec![
                base.clone(),
                format!("{base}.part"),
                format!("{base}.ytdl"),
            ]
        })
        .unwrap_or_default();
    // Legacy live pattern: .{id}.live. (e.g., "v.1.live.mp4.part"). Has the
    // `.live.` marker, so it cannot hit user files.
    let live_pattern = format!(".{item_id}.live.");
    for name in dir_file_names(staging) {
        // Delete the hidden manifest directly.
        if name == hidden_manifest || name == legacy_hidden {
            let _ = std::fs::remove_file(staging.join(&name));
            continue;
        }
        // Preserve completed recordings: they may be the user's only copy.
        if name.starts_with("final.") {
            continue;
        }
        // Exact manifest names only: no substring matching.
        if manifest_names.iter().any(|n| n == &name) {
            let _ = std::fs::remove_file(staging.join(&name));
            continue;
        }
        // Legacy live pattern (has .live. marker, safe).
        if name.contains(&live_pattern) {
            let _ = std::fs::remove_file(staging.join(&name));
            continue;
        }
        let Some(suffix) = name.strip_prefix(&prefix) else {
            continue;
        };
        // Preserve completed recordings: they may be the user's only copy.
        if suffix.starts_with("final.") {
            continue;
        }
        // Only delete known staging patterns; a user's own `grab-<id>-*` file survives.
        if !is_grab_staging_suffix(suffix) {
            continue;
        }
        let _ = std::fs::remove_file(staging.join(&name));
    }
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

pub(crate) fn manifest_path(dest_dir: &Path, item_id: u64) -> PathBuf {
    // Genuinely hidden: dot at the start of the basename. Internal bookkeeping,
    // invisible in normal file views. Just the ID: simple and unique.
    dest_dir.join(format!(".{item_id}.manifest.json"))
}

/// Previous manifest names, for backward-compatible reads.
fn legacy_manifest_paths(dest_dir: &Path, item_id: u64) -> [PathBuf; 3] {
    [
        dest_dir.join(format!(".grab-{item_id}-manifest.json")),
        dest_dir.join(format!("grab-{item_id}-.manifest.json")),
        dest_dir.join(format!("grab-{item_id}-manifest.json")),
    ]
}

pub(crate) fn read_manifest(dest_dir: &Path, item_id: u64) -> Option<VideoManifest> {
    // Try the hidden path first, then legacy names.
    std::fs::read_to_string(manifest_path(dest_dir, item_id))
        .ok()
        .or_else(|| {
            legacy_manifest_paths(dest_dir, item_id)
                .iter()
                .find_map(|p| std::fs::read_to_string(p).ok())
        })
        .and_then(|text| serde_json::from_str(&text).ok())
}

pub(crate) fn file_len(path: &Path) -> Option<u64> {
    std::fs::metadata(path).map(|m| m.len()).ok()
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
pub(crate) fn unified_candidate(file_name: &str) -> bool {
    let ext = Path::new(file_name).extension().and_then(|e| e.to_str());
    file_name.starts_with("grab-media.")
        && !is_ytdlp_fragment(file_name)
        && !matches!(ext, Some("part" | "srt" | "ytdl" | "temp" | "tmp" | "frag"))
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
        std::env::temp_dir().join(format!("grab-staging-{tag}-{}-{n}", std::process::id()))
    }

    #[test]
    fn success_path_leaves_no_empty_staging_root() {
        // The run_unified_ytdlp success tail: sweep the item's grab-<id>-* files.
        // The dest dir itself is never removed.
        let staging = unique_dir("success-root");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("grab-42-chunk.part"), b"scratch").unwrap();
        std::fs::write(staging.join("unrelated.txt"), b"keep").unwrap();
        sweep_staging_preserving_recordings(&staging, 42);
        assert!(
            !staging.join("grab-42-chunk.part").exists(),
            "item staging file is swept on success"
        );
        assert!(staging.exists(), "dest dir is never removed");
        assert!(
            staging.join("unrelated.txt").exists(),
            "unrelated files are never touched"
        );
        let _ = std::fs::remove_dir_all(&staging);
    }

    #[test]
    fn success_path_keeps_root_while_recording_remains() {
        // `grab-<id>-final.*` preservation: the recording stays, scratch is swept.
        // The dest dir itself is never removed.
        let staging = unique_dir("success-keep");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("grab-44-final.recording.mp4"), b"only copy").unwrap();
        std::fs::write(staging.join("grab-44-chunk.part"), b"scratch").unwrap();
        sweep_staging_preserving_recordings(&staging, 44);
        assert!(
            staging.join("grab-44-final.recording.mp4").exists(),
            "completed recording is preserved"
        );
        assert!(
            !staging.join("grab-44-chunk.part").exists(),
            "scratch is swept around the recording"
        );
        assert!(staging.exists(), "dest dir is never removed");
        let _ = std::fs::remove_dir_all(&staging);
    }

    #[test]
    fn sweep_preserving_recordings_rejects_non_staging_files() {
        // Critical: a user's own `grab-42-notes.txt` must survive the sweep,
        // even though it matches the `grab-<id>-` prefix. Only known Grab
        // staging patterns are deleted.
        let staging = unique_dir("allowlist-reject");
        std::fs::create_dir_all(&staging).unwrap();
        // Real staging files (must be deleted)
        std::fs::write(staging.join("grab-42-.manifest.json"), b"{}").unwrap();
        std::fs::write(staging.join("grab-42-video.f137.mp4.part"), b"part").unwrap();
        // User files that happen to match the prefix (must survive)
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
            !staging.join("grab-42-video.f137.mp4.part").exists(),
            "staging part file must be swept"
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
}
