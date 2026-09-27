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

/// Per-destination staging root: `<dest_dir>/.grab-video`. Dot-prefixed, so
/// file managers hide the in-flight scratch next to the finished files.
pub fn dest_staging_root(dest_dir: &Path) -> PathBuf {
    dest_dir.join(".grab-video")
}

/// Per-item staging dir for a destination: `<dest_dir>/.grab-video/<id>/`.
/// Same filesystem as the finished file, so delivery is an atomic rename and
/// a crash leaves the scratch hidden beside the destination, not in tmpfs.
pub fn staging_dir_for(dest_dir: &Path, item_id: u64) -> PathBuf {
    dest_staging_root(dest_dir).join(item_id.to_string())
}

/// Legacy tmpfs location from before dest-side staging. Read fallback only:
/// a paused row keeps its resume data across the upgrade until its staging
/// drains through the normal completion/removal paths.
pub fn legacy_staging_dir(item_id: u64) -> PathBuf {
    staging_root().join(item_id.to_string())
}

/// Historic alias for the legacy tmpfs location (kept for tests probing
/// pre-upgrade layouts). Test-only: production resolves through
/// [`staging_location_for_dest`].
#[cfg(test)]
pub fn staging_dir(item_id: u64) -> PathBuf {
    legacy_staging_dir(item_id)
}

/// A resolved per-item staging dir plus the root it is guarded under: the
/// dest-side root for current staging, the tmp root for legacy dirs still
/// draining from before dest-side staging.
#[derive(Debug, Clone)]
pub struct StagingLocation {
    pub dir: PathBuf,
    pub root: PathBuf,
}

/// Resolve the staging dir in use: the dest-side one when present, else the
/// legacy tmp one when present, else the dest-side one (for creation).
pub fn staging_location(dest_dir: &Path, item_id: u64) -> StagingLocation {
    let fresh = staging_dir_for(dest_dir, item_id);
    if fresh.exists() {
        let root = dest_staging_root(dest_dir);
        return StagingLocation { dir: fresh, root };
    }
    let legacy = legacy_staging_dir(item_id);
    if legacy.exists() {
        return StagingLocation {
            dir: legacy,
            root: staging_root(),
        };
    }
    StagingLocation {
        dir: fresh,
        root: dest_staging_root(dest_dir),
    }
}

/// Resolve the staging location from a full destination *file* path (its
/// parent anchors the dest-side root). Falls back to the legacy tmp dir when
/// the destination has no parent.
pub fn staging_location_for_dest(dest: &Path, item_id: u64) -> StagingLocation {
    match dest.parent() {
        Some(dir) => staging_location(dir, item_id),
        None => StagingLocation {
            dir: legacy_staging_dir(item_id),
            root: staging_root(),
        },
    }
}

/// Whether any staging dir already exists for this id, dest-side or legacy:
/// the id allocator must skip it so a fresh row never lands on a leftover.
pub fn staging_occupied(dest_dir: &Path, item_id: u64) -> bool {
    staging_dir_for(dest_dir, item_id).exists() || legacy_staging_dir(item_id).exists()
}

/// Highest numeric staging dir present, if any (seeds the id allocator past leftovers).
pub fn highest_staging_index() -> Option<u64> {
    std::fs::read_dir(staging_root())
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|name| name.parse::<u64>().ok())
        .max()
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
    std::fs::create_dir_all(dir).map_err(VideoError::staging)?;
    guarded_staging_dir(root, dir)
        .ok_or_else(|| VideoError::staging(gettext("staging directory escaped its root")))
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

/// Remove a staging dir, guarded to stay under an explicit root (never user data).
pub(crate) fn clean_staging_in(root: &Path, dir: &Path) {
    if let Some(canon) = guarded_staging_dir(root, dir) {
        let _ = std::fs::remove_dir_all(canon);
    }
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
    let is_item = canon
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.parse::<u64>().ok())
        .is_some();
    if is_item {
        sweep_staging_preserving_recordings(&canon);
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

/// Sweep one destination's staging root (`<dest_dir>/.grab-video`): numeric
/// dirs with no live row are reclaimed exactly like the legacy tmp root. A
/// missing root (never staged here, destination deleted) is a no-op.
pub fn sweep_dest_staging(dest_dir: &Path, keep: &std::collections::HashSet<u64>) {
    sweep_orphan_staging_in(&dest_staging_root(dest_dir), keep);
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

/// Fixed part names inside a row's staging dir.
#[allow(dead_code)]
pub(crate) fn part_path(dir: &Path, kind: &str, ext: &str) -> PathBuf {
    dir.join(format!("{kind}.{ext}"))
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

/// Remove a leg's staging scratch, preserving completed `final.*` recordings (do not delete the user's only copy).
pub fn sweep_staging_preserving_recordings(staging: &Path) {
    for name in dir_file_names(staging) {
        if name.starts_with("final.") {
            continue;
        }
        let _ = std::fs::remove_file(staging.join(&name));
    }
    let _ = std::fs::remove_dir(staging);
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

pub(crate) fn manifest_path(dir: &Path) -> PathBuf {
    dir.join("manifest.json")
}

pub(crate) fn read_manifest(dir: &Path) -> Option<VideoManifest> {
    std::fs::read_to_string(manifest_path(dir))
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
