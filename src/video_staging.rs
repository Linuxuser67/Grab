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
/// `<dest_dir>/grab-<id>-<name>`. No subfolder. Same filesystem as the
/// finished file, so delivery is an atomic rename.
pub fn staging_file(dest_dir: &Path, item_id: u64, name: &str) -> PathBuf {
    dest_dir.join(format!("grab-{item_id}-{name}"))
}

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
    std::fs::read_dir(dest_dir)
        .ok()
        .map(|entries| {
            entries.filter_map(|e| e.ok()).any(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| n.starts_with(&prefix))
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

/// Remove all `grab-<id>-*` staging files for an item in the destination dir.
/// Remove all `grab-<id>-*` staging files for an item in the destination dir.
/// Only touches files with the item's prefix; never the dir itself or other files.
pub fn clean_staging_files(dest_dir: &Path, item_id: u64) {
    let prefix = format!("grab-{item_id}-");
    if let Ok(entries) = std::fs::read_dir(dest_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            if entry
                .file_name()
                .to_str()
                .map(|n| n.starts_with(&prefix))
                .unwrap_or(false)
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Remove a staging dir, guarded to stay under an explicit root (never user data).
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
fn reclaim_orphan_staging_in(root: &Path, dir: &Path) {
    let Some(canon) = guarded_staging_dir(root, dir) else {
        return;
    };
    // Re-verify after canonicalization: the target must still be a numeric
    // child of the root, so a symlink swapped in mid-sweep cannot divert the
    // removal onto the root itself or a non-item path.
    let item_id = canon
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.parse::<u64>().ok());
    if let Some(id) = item_id {
        sweep_staging_preserving_recordings(&canon, id);
    }
}

/// Reclaim per-item staging dirs with no live row (crash/kill leftovers: only
/// restored rows reuse their ids, so nothing swept can resume). Only numeric
/// dir names are touched — the `grab-cookies-*.txt` files and anything else
/// under the root are left alone. Runs at startup after the queue is restored,
/// before any worker starts, so nothing live is removed.
pub fn sweep_orphan_staging(keep: &std::collections::HashSet<u64>) {
    sweep_orphan_staging_in(&staging_root(), keep);
}

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
        // are the user's only copy — preserve them like the old per-dir sweep did.
        if let Some(rest) = name.strip_prefix("grab-")
            && let Some((id_str, suffix)) = rest.split_once('-')
            && let Ok(id) = id_str.parse::<u64>()
            && !keep.contains(&id)
            && !suffix.starts_with("final.")
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

pub(crate) fn sweep_orphan_staging_in(root: &Path, keep: &std::collections::HashSet<u64>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let is_orphan = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u64>().ok())
            .is_some_and(|id| !keep.contains(&id));
        if is_dir && is_orphan {
            reclaim_orphan_staging_in(root, &entry.path());
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

/// Fixed part names for a row: `<dest_dir>/grab-<id>-<kind>.<ext>`.
/// Dot-prefixed, hidden, directly in the destination dir (no subfolder).
pub(crate) fn part_path(dest_dir: &Path, item_id: u64, kind: &str, ext: &str) -> PathBuf {
    staging_file(dest_dir, item_id, &format!("{kind}.{ext}"))
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
const PART_KINDS: &[&str] = &["video.", "audio.", "hls.", "live."];

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
    strip_stem_suffix(file_name, stem).is_some_and(|r| PART_KINDS.iter().any(|k| r.starts_with(k)))
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
    for name in dir_file_names(staging) {
        if !name.starts_with(&prefix) {
            continue;
        }
        // Preserve completed recordings: they may be the user's only copy.
        if name.starts_with(&format!("{prefix}final.")) {
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
    staging_file(dest_dir, item_id, "manifest.json")
}

pub(crate) fn read_manifest(dest_dir: &Path, item_id: u64) -> Option<VideoManifest> {
    std::fs::read_to_string(manifest_path(dest_dir, item_id))
        .ok()
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
