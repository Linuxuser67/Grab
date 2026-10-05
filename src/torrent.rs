//! Magnet + .torrent intake and engine (librqbit), bridged onto the HTTP pump.
//! One lazy global session serves every torrent row as the same `EngineMsg`; row identity is the queue id.

use std::{
    collections::HashMap,
    num::NonZeroU32,
    path::{Path, PathBuf},
    sync::Arc,
};

use gettextrs::gettext;
use gtk4::gio;
use gtk4::gio::prelude::*;
use gtk4::glib;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, Api, ConnectionOptions, ListenerOptions,
    ManagedTorrent, Session, SessionOptions, SessionPersistenceConfig, TorrentStatsState,
    api::TorrentIdOrHash,
};
use tokio::sync::{Mutex, OnceCell, mpsc::UnboundedSender};

use crate::engine_msg::EngineMsg;
use crate::file_names::{dedupe_filename, sane_filename, shorten_filename};
use crate::runtime::tokio_rt;

// rqbit's handle alias is private, so name it locally.
type ManagedTorrentHandle = Arc<ManagedTorrent>;

/// A live torrent row: registered at spawn, removed on terminal/cancel.
struct Active {
    hash_hex: String,
    handle: Option<ManagedTorrentHandle>,
    /// Pause arrived while the torrent was still being added.
    paused: bool,
}

static SESSION: OnceCell<Arc<Session>> = OnceCell::const_new();
static ACTIVE: std::sync::LazyLock<Mutex<HashMap<u64, Active>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
/// Pre-chosen file indices staged at intake (file-list dialog), consumed once at spawn; keyed by pseudo-URL.
static PENDING_SELECTIONS: std::sync::LazyLock<std::sync::Mutex<HashMap<String, Vec<usize>>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Lock the staged-selection map, recovering from poisoning.
fn lock_pending() -> std::sync::MutexGuard<'static, HashMap<String, Vec<usize>>> {
    PENDING_SELECTIONS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Stage a file selection for the next spawn of `pseudo_url`.
pub fn stage_selection(pseudo_url: &str, only_files: Vec<usize>) {
    lock_pending().insert(pseudo_url.to_string(), only_files);
}

/// Read the staged selection for `pseudo_url`, if any (peek, not take: retries re-add from scratch and need the same filter).
pub(crate) fn get_selection(pseudo_url: &str) -> Option<Vec<usize>> {
    lock_pending().get(pseudo_url).cloned()
}

/// Drop staged selections no live row references.
pub fn prune_selections(referenced: &std::collections::HashSet<String>) {
    lock_pending().retain(|k, _| referenced.contains(k));
}

/// Drop the staged selection for `pseudo_url` (a finished row never re-spawns).
pub(crate) fn drop_selection(pseudo_url: &str) {
    lock_pending().remove(pseudo_url);
}

/// True for either torrent source (lifecycle paths key off this).
pub fn is_torrent(s: &str) -> bool {
    is_magnet(s) || is_torrent_url(s)
}

/// Case-insensitive ASCII prefix check (`get`, not slicing: non-ASCII input must never panic).
fn has_prefix_ci(s: &str, prefix: &str) -> bool {
    s.trim_start()
        .get(..prefix.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
}

pub fn is_magnet(s: &str) -> bool {
    // Bare `magnet:` prefix only; tracker params contain `://` and must never reach the http branch.
    has_prefix_ci(s, "magnet:")
}

/// Parse a magnet link, rejecting anything rqbit cannot resolve to BTv1.
pub fn parse_magnet(s: &str) -> Result<librqbit::Magnet, String> {
    let m = librqbit::Magnet::parse(s.trim()).map_err(|e| format!("Invalid magnet link: {e}"))?;
    if m.as_id20().is_none() {
        return Err(gettext("Only BitTorrent v1 magnets are supported"));
    }
    Ok(m)
}

/// Raw torrent display name, lossy-decoded (callers apply `safe_torrent_name` and their own fallback).
fn raw_torrent_name<B: AsRef<[u8]>>(name: Option<&B>) -> Option<String> {
    name.map(|n| String::from_utf8_lossy(n.as_ref()).into_owned())
}

/// A torrent's advertised name prepared for the filesystem (`None` = caller falls back).
fn safe_torrent_name(name: Option<String>) -> Option<String> {
    name.filter(|n| sane_filename(n))
        .map(|n| shorten_filename(&n))
}

/// Display stub for a magnet row: the advertised name, else the info-hash hex.
pub fn stub_name(magnet: &str) -> Option<String> {
    let m = parse_magnet(magnet).ok()?;
    // A hostile display name must never reach the filesystem: fall back to the info-hash hex.
    m.name
        .clone()
        .filter(|n| crate::file_names::sane_filename(n))
        .or_else(|| m.as_id20().map(|id| id.as_string()))
}

fn bps(v: Option<u64>) -> Option<NonZeroU32> {
    v.and_then(|b| u32::try_from(b).ok())
        .and_then(NonZeroU32::new)
}

/// What the engine must fetch: a magnet link, or archived .torrent bytes.
#[derive(Debug, Clone)]
pub enum TorrentSource {
    Magnet(String),
    File(PathBuf),
}

/// Pseudo-URL prefix for archived .torrent files (never leaves the app).
pub fn is_torrent_url(s: &str) -> bool {
    has_prefix_ci(s, "torrent:")
}

/// Archive dir for .torrent files: alongside the session state.
fn torrents_dir() -> PathBuf {
    glib::user_data_dir().join("grab").join("torrents")
}

/// Absolute archive path for a `torrent:{abs-path}` pseudo-URL (constrained to our archive dir, so rows can only resolve/delete Grab-archived files).
pub fn archive_path_for_url(url: &str) -> Option<PathBuf> {
    let path = url.trim_start().get("torrent:".len()..)?;
    let path = PathBuf::from(path);
    // Existence is part of validity: intake rejects doodled pseudo-URLs fast.
    (path.is_absolute()
        && path.starts_with(torrents_dir())
        && path.extension().is_some_and(|e| e == "torrent")
        && path.exists())
    .then_some(path)
}

/// Sanitized file stem of a user-supplied file name (`file_stem` strips directories, so traversal never survives).
fn safe_stem(file_name: &str) -> Option<&str> {
    Path::new(file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| sane_filename(s))
}

/// Copy `.torrent` bytes into the archive, named after the sanitized file stem. Returns the queue pseudo-URL.
pub fn archive_torrent_file(file_name: &str, bytes: &[u8]) -> Result<String, String> {
    const MAX_TORRENT_BYTES: usize = 10_000_000;
    if bytes.len() > MAX_TORRENT_BYTES {
        return Err(gettext("Torrent file is too large (max 10 MB)"));
    }
    // Parse first: never archive bytes rqbit itself would reject.
    librqbit::torrent_from_bytes(bytes).map_err(|e| format!("Invalid torrent file: {e}"))?;
    let stem = safe_stem(file_name).unwrap_or("torrent");
    let dir = torrents_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("Cannot store torrent file: {e}"))?;
    // Atomic create_new loop: no check-then-write race.
    let mut tried: Vec<String> = Vec::new();
    for _ in 0..32 {
        let candidate = dedupe_filename(&format!("{stem}.torrent"), |n| {
            tried.iter().any(|t| t.as_str() == n)
        });
        tried.push(candidate.clone());
        let path = dir.join(&candidate);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                use std::io::Write;
                // Clean up partial file on write/sync failure: a corrupt
                // archive would otherwise linger and collide later.
                if let Err(e) = f.write_all(bytes).and_then(|()| f.sync_all()) {
                    let _ = std::fs::remove_file(&path);
                    return Err(format!("Cannot store torrent file: {e}"));
                }
                return Ok(format!("torrent:{}", path.to_string_lossy()));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("Cannot store torrent file: {e}")),
        }
    }
    Err(gettext(
        "Cannot store torrent file: too many name collisions",
    ))
}

/// Stub row name for an archived file: the sanitized file stem.
pub fn stub_name_for_file(file_name: &str) -> String {
    safe_stem(file_name)
        .map(shorten_filename)
        .unwrap_or_else(|| "torrent".to_string())
}

/// One listed file of a multi-file torrent (raw path for the filesystem, sanitized display form for the UI only).
pub struct TorrentFileEntry {
    /// Entry path from lossy-decoded components: the only path ever used for filesystem operations.
    pub raw_path: String,
    /// Display form of `raw_path`. UI only — never a filesystem path.
    pub display_path: String,
    pub length: u64,
}

/// Upper bound for a `.torrent` file read from disk.
pub(crate) const MAX_TORRENT_BYTES: u64 = 10_000_000;

/// Read a `.torrent` file with a single bounded open (no separate size check: a concurrent replacement must not slip between).
pub(crate) fn read_torrent_bytes(path: &std::path::Path) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_TORRENT_BYTES + 1)
        .read_to_end(&mut buf)
        .ok()?;
    (buf.len() as u64 <= MAX_TORRENT_BYTES).then_some(buf)
}

/// Bounded read of the archived `.torrent` behind a queue pseudo-URL (`None` when missing, unreadable, or over the ceiling).
pub(crate) fn read_archive_bytes(url: &str) -> Option<Vec<u8>> {
    read_torrent_bytes(&archive_path_for_url(url)?)
}

/// Whether a `.torrent` archive exists for the URL (magnets have none).
pub(crate) fn has_archive(url: &str) -> bool {
    archive_path_for_url(url).is_some_and(|p| p.exists())
}

/// Parse `.torrent` bytes into a display list for the file picker (display strips bidi/controls and caps length; selection is index-based).
fn sanitize_display_path(path: &str) -> String {
    const MAX_CHARS: usize = 120;
    let mut out: String = path
        .chars()
        .filter(|c| !c.is_control() && !crate::file_names::is_bidi_control(*c))
        .take(MAX_CHARS + 1)
        .collect();
    if out.chars().count() > MAX_CHARS {
        out = out.chars().take(MAX_CHARS).collect();
        out.push('…');
    }
    out
}

pub fn torrent_file_list(bytes: &[u8]) -> Result<(String, Vec<TorrentFileEntry>), String> {
    let meta =
        librqbit::torrent_from_bytes(bytes).map_err(|e| format!("Invalid torrent file: {e}"))?;
    let name = raw_torrent_name(meta.info.data.name.as_ref())
        .filter(|n| sane_filename(n))
        .unwrap_or_else(|| "torrent".to_string());
    let entries: Vec<TorrentFileEntry> =
        meta.info.data.files.as_ref().map_or(Vec::new(), |files| {
            files
                .iter()
                .enumerate()
                .map(|(i, f)| {
                    let parts: Vec<String> = f
                        .path
                        .iter()
                        .map(|c| String::from_utf8_lossy(c.as_ref()).into_owned())
                        .collect();
                    let raw = parts.join("/");
                    let shown = sanitize_display_path(&raw);
                    TorrentFileEntry {
                        raw_path: raw,
                        display_path: if shown.is_empty() {
                            format!("file {i}")
                        } else {
                            shown
                        },
                        length: f.length,
                    }
                })
                .collect()
        });
    Ok((name, entries))
}

/// Delete the archive behind a pseudo-URL. Missing files are fine.
pub fn delete_archive_for_url(url: &str) {
    if let Some(path) = archive_path_for_url(url) {
        let _ = std::fs::remove_file(path);
    }
}

/// Remove archives no live row references (stale Failed/Cancelled leftovers stay for retry).
pub fn sweep_archives(referenced: &std::collections::HashSet<String>) {
    sweep_archives_in(&torrents_dir(), referenced);
}

/// Testable core: sweep a directory, keeping only referenced archives.
pub fn sweep_archives_in(dir: &std::path::Path, referenced: &std::collections::HashSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("torrent") {
            continue;
        }
        let url = format!("torrent:{}", path.to_string_lossy());
        if !referenced.contains(&url) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Where a torrent's files go: multi-file torrents get a subfolder (magnets always land flat: counts arrive mid-download).
fn output_folder_for(
    dest: &std::path::Path,
    name: Option<String>,
    multi: bool,
    fallback: &str,
) -> std::path::PathBuf {
    if !multi {
        return dest.to_path_buf();
    }
    let dir = safe_torrent_name(name).unwrap_or_else(|| fallback.to_string());
    dest.join(dir)
}

/// Guard a torrent output folder at write time: `folder` is either the dest
/// dir itself (flat download) or `dest/<leaf>`. The engine follows its
/// output folder blindly, so a symlink planted at the leaf — after intake,
/// or while the row sat queued — would divert the writes outside the
/// download dir. The leaf goes through the same atomic create-and-dedupe as
/// collection subfolders: a squatted name lands in a fresh folder instead
/// of being followed, and an existing real dir is reused so resume keeps
/// working.
pub(crate) fn guard_output_folder(
    dest: &std::path::Path,
    folder: std::path::PathBuf,
) -> std::path::PathBuf {
    let (Some(parent), Some(leaf)) = (folder.parent(), folder.file_name()) else {
        return folder;
    };
    if parent != dest {
        return folder;
    }
    crate::file_names::create_guarded_dir(dest, &leaf.to_string_lossy())
}

/// Two or more files means the torrent downloads into its own folder.
fn is_multi_file<T>(files: Option<&[T]>) -> bool {
    files.is_some_and(|f| f.len() >= 2)
}

/// What the engine will download into for archived bytes (mirrors `run_torrent`'s File branch, so intake detects collisions upfront).
pub(crate) fn intake_plan(bytes: &[u8]) -> Option<(String, bool)> {
    let meta = librqbit::torrent_from_bytes(bytes).ok()?;
    let raw = raw_torrent_name(meta.info.data.name.as_ref());
    let multi = is_multi_file(meta.info.data.files.as_deref());
    let base = safe_torrent_name(raw).unwrap_or_else(|| meta.info_hash.as_string());
    Some((base, multi))
}

/// Real on-disk location of a torrent's files for deletion (recomputes the engine's output folder, so Delete trashes what was written).
pub fn torrent_output_dir(dest: &std::path::Path, url: &str) -> Option<PathBuf> {
    if !is_torrent_url(url) {
        return None;
    }
    let bytes = read_archive_bytes(url)?;
    let meta = librqbit::torrent_from_bytes(&bytes).ok()?;
    let multi = is_multi_file(meta.info.data.files.as_deref());
    if !multi {
        return None;
    }
    let name = raw_torrent_name(meta.info.data.name.as_ref());
    // Same fallback the engine uses (info-hash hex): deterministic match.
    let fallback = meta.info_hash.as_string();
    Some(output_folder_for(dest, name, true, &fallback))
}

/// Path of a single-file torrent's file (dest dir + metadata filename).
/// Returns None for multi-file torrents or if the archive is missing.
pub(crate) fn single_file_path(dest: &std::path::Path, url: &str) -> Option<std::path::PathBuf> {
    if !is_torrent_url(url) {
        return None;
    }
    let bytes = read_archive_bytes(url)?;
    let meta = librqbit::torrent_from_bytes(&bytes).ok()?;
    if is_multi_file(meta.info.data.files.as_deref()) {
        return None;
    }
    let name = safe_torrent_name(raw_torrent_name(meta.info.data.name.as_ref()))
        .unwrap_or_else(|| meta.info_hash.as_string());
    Some(dest.join(name))
}

/// Delete untoggled files after a filtered torrent finishes (librqbit pre-creates every file, so unselected ones must be removed here).
pub(crate) fn cleanup_unselected(folder: &std::path::Path, url: &str) {
    let Some(selected) = get_selection(url) else {
        return;
    };
    if selected.is_empty() {
        return;
    }
    let Some(bytes) = read_archive_bytes(url) else {
        return;
    };
    let Ok((_, entries)) = torrent_file_list(&bytes) else {
        return;
    };
    if entries.len() < 2 {
        return;
    }
    let keep: std::collections::HashSet<usize> = selected.into_iter().collect();
    for (i, entry) in entries.iter().enumerate() {
        if let Some(target) = deletion_target(folder, i, &keep, entry) {
            remove_file_and_prune_parents(&target, folder);
        }
    }
}

/// Deletion target for one unselected entry (never the display path: it is truncated/stripped and may name nothing on disk). Pure.
pub(crate) fn deletion_target(
    folder: &std::path::Path,
    index: usize,
    keep: &std::collections::HashSet<usize>,
    entry: &TorrentFileEntry,
) -> Option<std::path::PathBuf> {
    if keep.contains(&index) || is_hostile_entry(&entry.raw_path) {
        return None;
    }
    // Skip lossy paths: from_utf8_lossy replaced invalid bytes with U+FFFD,
    // so the raw_path may not match what's on disk. A miss is harmless,
    // but a collision could target the wrong file.
    if entry.raw_path.contains('\u{FFFD}') {
        return None;
    }
    Some(folder.join(&entry.raw_path))
}

/// Whether a torrent entry path may escape its folder (hostile entries are never joined).
fn is_hostile_entry(path: &str) -> bool {
    path.split('/')
        .any(|c| c.is_empty() || c == "." || c == "..")
}

/// Remove a file, then prune parents left empty (never removing `folder`).
/// Refuses if any intermediate component under `folder` is a symlink:
/// a pre-existing symlinked subdirectory would otherwise divert the
/// deletion outside the torrent folder.
///
/// TOCTOU note: the symlink check and the remove are not atomic. A symlink
/// swapped in between could divert the delete. Exploiting this requires
/// write access to the download directory, so the risk is low. For the
/// file itself we use `unlinkat` with `O_NOFOLLOW` on the parent fd, which
/// closes the window for the final component.
fn remove_file_and_prune_parents(path: &std::path::Path, folder: &std::path::Path) {
    // Bail if any intermediate component is a symlink or escapes `folder`.
    if path_has_symlink_component(path, folder) {
        return;
    }
    // Use unlinkat with O_NOFOLLOW on the parent: the filename is resolved
    // relative to the opened fd, so a symlink swapped in for the parent
    // after our check cannot divert the unlink. The File closes on drop —
    // no manual close needed.
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::io::AsRawFd;
        let name_c = std::ffi::CString::new(name.as_bytes()).ok();
        let dir_fd = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(parent)
            .ok();
        if let (Some(dir_fd), Some(name_c)) = (dir_fd, name_c) {
            // SAFETY: dir_fd is a valid open directory fd, name_c is a valid C string.
            unsafe {
                libc::unlinkat(dir_fd.as_raw_fd(), name_c.as_ptr(), 0);
            }
        } else {
            // Parent vanished or became a symlink; skip.
            return;
        }
    }
    let mut parent = path.parent();
    while let Some(dir) = parent {
        if dir == folder {
            break;
        }
        match std::fs::remove_dir(dir) {
            Ok(()) => parent = dir.parent(),
            Err(_) => break,
        }
    }
}

/// Trash all files listed in the torrent metadata (not the whole folder).
/// Skips hostile entries and U+FFFD paths. Returns true if the folder is empty afterwards.
/// Whether any intermediate component of `path` under `folder` is a symlink
/// or escapes `folder`. Used to refuse trashing through a symlinked directory.
fn path_has_symlink_component(path: &std::path::Path, folder: &std::path::Path) -> bool {
    let mut current = path.parent();
    while let Some(dir) = current {
        if dir == folder {
            return false;
        }
        if !dir.starts_with(folder) {
            return true;
        }
        if std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()) {
            return true;
        }
        current = dir.parent();
    }
    true
}

pub(crate) fn trash_torrent_contents(folder: &std::path::Path, url: &str) -> Result<bool, String> {
    let Some(bytes) = read_archive_bytes(url) else {
        return Err("no torrent archive".to_string());
    };
    let Ok(meta) = librqbit::torrent_from_bytes(&bytes) else {
        return Err("invalid torrent metadata".to_string());
    };
    let multi = is_multi_file(meta.info.data.files.as_deref());
    if !multi {
        // Single-file torrent: torrent_file_list returns empty (no selectable
        // children for the UI), so resolve the filename directly.
        let name = safe_torrent_name(raw_torrent_name(meta.info.data.name.as_ref()))
            .unwrap_or_else(|| meta.info_hash.as_string());
        let target = folder.join(name);
        if path_has_symlink_component(&target, folder) {
            return Err(format!(
                "refusing to trash through symlink: {}",
                target.display()
            ));
        }
        match gio::File::for_path(&target).trash(gio::Cancellable::NONE) {
            Ok(()) => {}
            Err(e) if e.kind::<gio::IOErrorEnum>() == Some(gio::IOErrorEnum::NotFound) => {}
            Err(e) => return Err(format!("could not move {} to Trash: {e}", target.display())),
        }
        return Ok(std::fs::read_dir(folder)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(true));
    }
    let Ok((_, entries)) = torrent_file_list(&bytes) else {
        return Err("invalid torrent file list".to_string());
    };
    let empty_keep: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for (i, entry) in entries.iter().enumerate() {
        if let Some(target) = deletion_target(folder, i, &empty_keep, entry) {
            // Refuse symlinked intermediate components (same as remove_file_and_prune_parents).
            if path_has_symlink_component(&target, folder) {
                continue;
            }
            // Trash the file (not permanent delete), then prune empty parents.
            match gio::File::for_path(&target).trash(gio::Cancellable::NONE) {
                Ok(()) => {}
                Err(e) if e.kind::<gio::IOErrorEnum>() == Some(gio::IOErrorEnum::NotFound) => {}
                Err(e) => return Err(format!("could not move {} to Trash: {e}", target.display())),
            }
            // Prune parents left empty by the trash (they're empty, safe to remove).
            let mut parent = target.parent();
            while let Some(dir) = parent {
                if dir == folder {
                    break;
                }
                match std::fs::remove_dir(dir) {
                    Ok(()) => parent = dir.parent(),
                    Err(_) => break,
                }
            }
        }
    }
    // Check if folder is empty.
    Ok(std::fs::read_dir(folder)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(true))
}

/// Network plan for one torrent add, resolved from settings at spawn.
/// Under SOCKS5, DHT/listener/UDP trackers and UPnP go dark instead of leaking around the tunnel; HTTP(S) proxies pass through.
pub(crate) struct TorrentNetPlan {
    pub dht: bool,
    /// Local Service Discovery (forced off under SOCKS5: multicast would leak local presence).
    pub lsd: bool,
    pub listen_port: i32,
    pub upnp: bool,
    pub trackers: Option<Vec<String>>,
    pub socks_proxy: Option<String>,
}

/// Resolve the effective listen port and UPnP flag: only when seeding is
/// enabled; otherwise both are forced off (backend matches UI gating).
pub(crate) fn resolve_listen_config(
    seed_finished: bool,
    listen_port: i32,
    upnp: bool,
) -> (i32, bool) {
    if seed_finished {
        (listen_port, upnp)
    } else {
        (0, false)
    }
}

/// Clamp a configured listen port to u16: fail loudly on out-of-range
/// (defense-in-depth for dconf bypassing the GSettings range).
pub(crate) fn clamp_listen_port(listen_port: i32) -> Result<u16, String> {
    u16::try_from(listen_port).map_err(|_| "torrent listen port out of range (0-65535)".to_string())
}

pub(crate) fn plan_torrent_net(
    dht: bool,
    lsd: bool,
    listen_port: i32,
    upnp: bool,
    trackers: Option<Vec<String>>,
    proxy: Option<&crate::net_types::ResolvedProxy>,
) -> TorrentNetPlan {
    let Some(url) = proxy.and_then(|p| p.torrent_socks_url()) else {
        return TorrentNetPlan {
            dht,
            lsd,
            listen_port,
            upnp,
            trackers,
            socks_proxy: None,
        };
    };
    let trackers = trackers
        .map(|ts| {
            ts.into_iter()
                .filter(|t| t.starts_with("http://") || t.starts_with("https://"))
                .collect::<Vec<_>>()
        })
        .filter(|ts| !ts.is_empty());
    TorrentNetPlan {
        dht: false,
        lsd: false,
        listen_port: 0,
        upnp: false,
        trackers,
        socks_proxy: Some(url),
    }
}

/// Creation-scoped session inputs (bundled to stay under clippy's too-many-arguments lint).
#[derive(Clone, Debug)]
pub(crate) struct SessionConfig {
    pub dht: bool,
    pub peer_limit: Option<usize>,
    pub download_bps: Option<u64>,
    pub upload_bps: Option<u64>,
    pub listen_port: i32,
    pub upnp: bool,
    pub socks_proxy: Option<String>,
    pub blocklist_url: Option<String>,
    pub lsd: bool,
}

async fn ensure_session(cfg: SessionConfig) -> Result<Arc<Session>, String> {
    let SessionConfig {
        dht,
        peer_limit,
        download_bps,
        upload_bps,
        listen_port,
        upnp,
        socks_proxy,
        blocklist_url,
        lsd,
    } = cfg;
    SESSION
        .get_or_try_init(|| async {
            let dir = glib::user_data_dir().join("grab");
            std::fs::create_dir_all(&dir).map_err(|e| format!("Cannot create session dir: {e}"))?;
            // Fast resume: persist bitfields + session torrents across restarts (orphans swept once the queue is restored).
            let mut opts = SessionOptions {
                fastresume: true,
                persistence: Some(SessionPersistenceConfig::Json {
                    folder: Some(dir.join("session")),
                }),
                peer_limit,
                ..Default::default()
            };
            if !dht {
                opts.dht = None;
            }
            // Creation-scoped like DHT: no live setter exists, so edits apply to sessions created after them.
            opts.blocklist_url = blocklist_url;
            opts.disable_local_service_discovery = !lsd;
            // Creation-scoped like DHT/listener: proxy edits apply to sessions created after them.
            if let Some(proxy_url) = socks_proxy {
                opts.connect = Some(ConnectionOptions {
                    proxy_url: Some(proxy_url),
                    ..Default::default()
                });
            }
            // No live setter: applies here and per add, so new downloads pick up edits. 0 = disabled; positive ports bind dual-stack.
            if listen_port > 0 {
                let port = clamp_listen_port(listen_port)?;
                opts.listen = Some(ListenerOptions {
                    listen_addr: (std::net::Ipv6Addr::UNSPECIFIED, port).into(),
                    enable_upnp_port_forwarding: upnp,
                    ..Default::default()
                });
            }
            let session = Session::new_with_opts(dir, opts)
                .await
                .map_err(|e| format!("Cannot start torrent engine: {e}"))?;
            session.ratelimits.set_download_bps(bps(download_bps));
            session.ratelimits.set_upload_bps(bps(upload_bps));
            Ok(session)
        })
        .await
        .map(Arc::clone)
}

/// Validate the peer-blocklist preference (empty = off; anything else must be http(s), else fail loudly).
pub(crate) fn blocklist_url_of(raw: &str) -> Result<Option<String>, String> {
    let url = raw.trim();
    if url.is_empty() {
        return Ok(None);
    }
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Ok(Some(url.to_string()));
    }
    Err(gettext("Peer blocklist must be an http(s) URL"))
}
/// Live-apply the speed caps (peer limit has no live setter: creation + per-add only).
pub(crate) fn apply_live_limits(download_bps: Option<u64>, upload_bps: Option<u64>) {
    if let Some(s) = SESSION.get() {
        s.ratelimits.set_download_bps(bps(download_bps));
        s.ratelimits.set_upload_bps(bps(upload_bps));
    }
}

/// Snapshot of the live session, if the engine has started.
pub(crate) fn session_handle() -> Option<Arc<Session>> {
    SESSION.get().cloned()
}

/// Info-hash hexes claimed by live spawns (registered before the session exists, so not-yet-adding rows count).
pub(crate) async fn active_hashes() -> std::collections::HashSet<String> {
    ACTIVE
        .lock()
        .await
        .values()
        .map(|a| a.hash_hex.clone())
        .collect()
}

/// Info-hash hex for a torrent queue URL (`None` = unresolvable; the row fails at spawn, the orphan sweep skips it).
pub(crate) fn info_hash_for_url(url: &str) -> Option<String> {
    if is_magnet(url) {
        return parse_magnet(url).ok()?.as_id20().map(|h| h.as_string());
    }
    if is_torrent_url(url) {
        let bytes = read_archive_bytes(url)?;
        let meta = librqbit::torrent_from_bytes(&bytes).ok()?;
        return Some(meta.info_hash.as_string());
    }
    None
}

/// Drop session torrents no queue row claims (session persistence re-adds everything, including crash orphans with no UI row).
pub(crate) async fn sweep_session_orphans(keep: &std::collections::HashSet<String>) {
    let Some(session) = session_handle() else {
        return;
    };
    // Auto-restore completes inside Session::new_with_opts, so a spawn racing us is already in ACTIVE or still queued — never swept from under it.
    let api = Api::new(session.clone(), None);
    let torrents = api.api_torrent_list().torrents;
    let active = active_hashes().await;
    for t in torrents {
        if keep.contains(&t.info_hash) || active.contains(&t.info_hash) {
            continue;
        }
        let Some(id) = t.id else { continue };
        // Session::delete panics on metadata-less magnets: skip those for the next launch instead of poisoning the sweep.
        let resolved = session
            .get(TorrentIdOrHash::Id(id))
            .is_some_and(|h| h.metadata.load().is_some());
        if !resolved {
            tracing::debug!(info_hash = %t.info_hash, "keeping metadata-less orphan for the next launch");
            continue;
        }
        tracing::debug!(info_hash = %t.info_hash, "dropping orphaned session torrent");
        let _ = session.delete(TorrentIdOrHash::Id(id), false).await;
    }
}

/// Unpause an adopted persisted handle, then re-check the row's paused flag (a racing pause wins, never silently lost).
async fn unpause_adopted(session: &Arc<Session>, id: u64, handle: &ManagedTorrentHandle) {
    let _ = session.unpause(handle).await;
    if ACTIVE.lock().await.get(&id).is_some_and(|a| a.paused) {
        let _ = session.pause(handle).await;
    }
}

/// Pause a live torrent (fire-and-forget: the poll loop keeps running, but every later state reads Paused).
pub(crate) fn pause_download(id: u64) {
    tokio_rt().spawn(async move {
        let handle = {
            let mut active = ACTIVE.lock().await;
            match active.get_mut(&id) {
                Some(a) => {
                    a.paused = true;
                    a.handle.clone()
                }
                None => return,
            }
        };
        if let (Some(h), Some(s)) = (handle, SESSION.get()) {
            let _ = s.pause(&h).await;
        }
    });
}

/// Drop a torrent row from the session, keeping partial files on disk (cancel/retry resumes; explicit delete discards).
pub(crate) fn forget_download(id: u64) {
    tokio_rt().spawn(async move {
        let active = ACTIVE.lock().await.remove(&id);
        if let (Some(a), Some(s)) = (active, SESSION.get())
            && let Some(h) = a.handle
        {
            // Same skip-guard as the orphan sweep: Session::delete panics on metadata-less magnets.
            if h.metadata.load().is_some() {
                let _ = s.delete(TorrentIdOrHash::Hash(h.info_hash()), false).await;
            } else {
                tracing::debug!(id, info_hash = %h.info_hash().as_string(), "keeping metadata-less torrent for the orphan sweep");
            }
        }
    });
}

/// Seed-limit check for the poll loop (`t0` = first finished tick; seed-time counts from there). Pure.
fn seed_limits_hit(
    seed_ratio: f64,
    seed_time_min: i32,
    t0: std::time::Instant,
    total_bytes: u64,
    uploaded_bytes: u64,
) -> bool {
    let ratio_hit = seed_ratio > 0.0
        && total_bytes > 0
        && uploaded_bytes as f64 >= seed_ratio * total_bytes as f64;
    let time_hit = seed_time_min > 0
        && t0.elapsed() >= std::time::Duration::from_secs(seed_time_min as u64 * 60);
    ratio_hit || time_hit
}

/// Evict a finished row from the session unless still seeding (completed rows would otherwise leak RAM one torrent at a time).
async fn maybe_evict(session: &Session, id: TorrentIdOrHash, finished: bool, seed_finished: bool) {
    if finished && !seed_finished {
        let _ = session.delete(id, false).await;
    }
}

/// Progress message for one poll tick (torrent rows carry live upload counters; HTTP rows send zeros).
fn progress_msg(downloaded: u64, total: Option<u64>, uploaded: u64, upload_bps: u64) -> EngineMsg {
    EngineMsg::Progress {
        downloaded,
        total,
        uploaded,
        upload_bps,
    }
}

/// Poll one managed torrent into the pump. Returns true when the download finished.
async fn poll_loop(
    session: Arc<Session>,
    handle: ManagedTorrentHandle,
    stub: &str,
    seed_finished: bool,
    seed_ratio: f64,
    seed_time_min: i32,
    tx: &UnboundedSender<EngineMsg>,
) -> bool {
    let mut suggested = false;
    let mut finished = false;
    let mut finished_at: Option<std::time::Instant> = None;
    // Per-piece haves borrow the session only, polled on the same 500ms tick (a failed poll just skips a frame).
    let api = Api::new(session.clone(), None);
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let stats = handle.stats();
        if !suggested {
            suggested = true;
            if let Some(name) = handle.name() {
                let short = shorten_filename(&name);
                if name != stub
                    && sane_filename(&short)
                    && tx.send(EngineMsg::SuggestName(short)).is_err()
                {
                    break;
                }
            }
        }
        let total = (stats.total_bytes > 0).then_some(stats.total_bytes);
        if tx
            .send(progress_msg(
                stats.progress_bytes,
                total,
                stats.uploaded_bytes,
                stats
                    .live
                    .as_ref()
                    .map(|l| l.upload_speed.as_bytes())
                    .unwrap_or(0),
            ))
            .is_err()
        {
            break;
        }
        if let Ok((have, _)) = api.api_dump_haves(TorrentIdOrHash::Hash(handle.info_hash())) {
            let have: Vec<bool> = have.iter().by_vals().collect();
            if tx.send(EngineMsg::TorrentPieces(have)).is_err() {
                break;
            }
        }
        if matches!(stats.state, TorrentStatsState::Error) {
            let _ = tx.send(EngineMsg::Failed(
                stats.error.unwrap_or_else(|| gettext("Torrent failed")),
            ));
            break;
        }
        if stats.finished {
            if !seed_finished {
                let _ = session.pause(&handle).await;
                let _ = tx.send(EngineMsg::Finished {
                    size: stats.total_bytes,
                });
                finished = true;
                break;
            }
            // Seeding requested: stay on the loop until a rule fires (with neither rule set this seeds forever, keeping a working Stop).
            let t0 = *finished_at.get_or_insert_with(std::time::Instant::now);
            if seed_limits_hit(
                seed_ratio,
                seed_time_min,
                t0,
                stats.total_bytes,
                stats.uploaded_bytes,
            ) {
                let _ = session.pause(&handle).await;
                let _ = session
                    .delete(TorrentIdOrHash::Hash(handle.info_hash()), false)
                    .await;
                let _ = tx.send(EngineMsg::Finished {
                    size: stats.total_bytes,
                });
                finished = true;
                break;
            }
        }
    }
    finished
}

/// Torrent engine: add (or resume) one magnet, then poll stats into the pump.
/// Retry-after-pause reuses the live handle; a second row on the same hash fails.
pub(crate) struct TorrentJob {
    pub id: u64,
    pub source: TorrentSource,
    pub dest: PathBuf,
    pub seed_finished: bool,
    pub seed_ratio: f64,
    pub seed_time_min: i32,
    pub dht: bool,
    pub peer_limit: Option<usize>,
    pub download_bps: Option<u64>,
    pub upload_bps: Option<u64>,
    pub listen_port: i32,
    /// Ask the router to forward the listen port via UPnP (enabled automatically when a listen port is configured).
    pub upnp: bool,
    /// Extra tracker URLs from preferences (per-add, so edits apply to new downloads without restarting).
    pub trackers: Option<Vec<String>>,
    /// SOCKS5 proxy URL for the session (DHT, listener and UDP trackers already forced off alongside).
    pub socks_proxy: Option<String>,
    /// Peer blocklist URL from preferences (creation-scoped: read once at session start).
    pub blocklist_url: Option<String>,
    /// Local Service Discovery: find peers on the local network.
    pub lsd: bool,
    /// Pre-chosen file indices for multi-file torrents (intake dialog).
    pub only_files: Option<Vec<usize>>,
    /// Intake recorded a collision-proof subfolder as dest: use it directly instead of recomputing from metadata.
    pub dest_is_final: bool,
    pub tx: UnboundedSender<EngineMsg>,
}

/// Claim the registry slot for a spawn (resume when this row owns it; reject when a different row holds the same torrent).
fn claim_slot(active: &mut HashMap<u64, Active>, id: u64, hash_hex: &str) -> Result<bool, ()> {
    if let Some(a) = active.get_mut(&id) {
        if a.hash_hex != hash_hex {
            a.hash_hex = hash_hex.to_string();
            a.handle = None;
            a.paused = false;
            Ok(false)
        } else {
            a.paused = false;
            Ok(a.handle.is_some())
        }
    } else if active.values().any(|a| a.hash_hex == hash_hex) {
        Err(())
    } else {
        active.insert(
            id,
            Active {
                hash_hex: hash_hex.to_string(),
                handle: None,
                paused: false,
            },
        );
        Ok(false)
    }
}

pub(crate) async fn run_torrent(job: TorrentJob) {
    let TorrentJob {
        id,
        source,
        dest,
        seed_finished,
        seed_ratio,
        seed_time_min,
        dht,
        peer_limit,
        download_bps,
        upload_bps,
        listen_port,
        upnp,
        trackers,
        socks_proxy,
        blocklist_url,
        lsd,
        only_files,
        dest_is_final,
        tx,
    } = job;
    let fail = |msg: String| {
        let _ = tx.send(EngineMsg::Failed(msg));
    };
    // Resolve identity before touching the registry: magnets parse the link, files parse their archived bytes.
    enum Adder {
        Magnet(String),
        File(Vec<u8>),
    }
    let (hash_hex, hash_id, stub, folder, adder) = match &source {
        TorrentSource::Magnet(magnet) => match parse_magnet(magnet) {
            Ok(m) => {
                let Some(hash_id) = m.as_id20() else {
                    fail("Only BitTorrent v1 magnets are supported".to_string());
                    return;
                };
                let hash_hex = hash_id.as_string();
                let stub = stub_name(magnet).unwrap_or_else(|| hash_hex.clone());
                (
                    hash_hex,
                    hash_id,
                    stub,
                    dest.clone(),
                    Adder::Magnet(magnet.clone()),
                )
            }
            Err(e) => {
                fail(e);
                return;
            }
        },
        TorrentSource::File(path) => {
            let Some(bytes) = read_torrent_bytes(path) else {
                fail(gettext("Cannot read torrent file"));
                return;
            };
            match librqbit::torrent_from_bytes(&bytes) {
                Ok(meta) => {
                    let hash_id = meta.info_hash;
                    let hash_hex = hash_id.as_string();
                    let raw_name = raw_torrent_name(meta.info.data.name.as_ref());
                    let multi = is_multi_file(meta.info.data.files.as_deref());
                    let stub =
                        safe_torrent_name(raw_name.clone()).unwrap_or_else(|| hash_hex.clone());
                    // Collision-recorded subfolder from intake: already final.
                    let folder = if dest_is_final {
                        dest
                    } else {
                        // The engine follows its output folder blindly:
                        // re-guard the recomputed leaf here — a symlink
                        // planted while queued would otherwise divert the
                        // writes. Already-guarded paths reuse as-is.
                        guard_output_folder(&dest, output_folder_for(&dest, raw_name, multi, &stub))
                    };
                    (hash_hex, hash_id, stub, folder, Adder::File(bytes))
                }
                Err(e) => {
                    fail(format!("Invalid torrent file: {e}"));
                    return;
                }
            }
        }
    };
    // Slot policy (resume ours, reject another row's hash): `claim_slot`.
    let resumed = {
        let mut active = ACTIVE.lock().await;
        match claim_slot(&mut active, id, &hash_hex) {
            Ok(resumed) => resumed,
            Err(()) => {
                drop(active);
                fail(gettext("Torrent is already in the queue"));
                return;
            }
        }
    };
    let session = match ensure_session(SessionConfig {
        dht,
        peer_limit,
        download_bps,
        upload_bps,
        listen_port,
        upnp,
        socks_proxy,
        blocklist_url,
        lsd,
    })
    .await
    {
        Ok(s) => s,
        Err(e) => {
            ACTIVE.lock().await.remove(&id);
            fail(e);
            return;
        }
    };
    if resumed {
        if let Some(h) = ACTIVE.lock().await.get(&id).and_then(|a| a.handle.clone()) {
            unpause_adopted(&session, id, &h).await;
            let finished = poll_loop(
                session.clone(),
                h,
                &stub,
                seed_finished,
                seed_ratio,
                seed_time_min,
                &tx,
            )
            .await;
            // Eviction policy (finished rows, unless seeding): `maybe_evict`.
            maybe_evict(
                &session,
                TorrentIdOrHash::Hash(hash_id),
                finished,
                seed_finished,
            )
            .await;
        } else {
            fail(gettext("Torrent is no longer managed"));
        }
        ACTIVE.lock().await.remove(&id);
        return;
    }
    // A just-forgotten session entry vanishes asynchronously (forget is
    // fire-and-forget): wait for our hash to clear instead of tripping the
    // duplicate guard on our own deletion (worst case ~3s).
    for _ in 0..12 {
        if session.get(TorrentIdOrHash::Hash(hash_id)).is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    // Cloned before opts takes ownership (see the filter-match arm below).
    let want_files = only_files.clone();
    let opts = AddTorrentOptions {
        output_folder: Some(folder.to_string_lossy().into_owned()),
        // The intake collision-dedup path guarantees a fresh folder, so nothing
        // existing can be clobbered; others keep overwrite:true (resume data
        // must open).
        overwrite: !dest_is_final,
        // Current preference at add time: running torrents keep theirs.
        peer_limit,
        // Extra trackers from preferences (same live-at-add rule).
        trackers,
        // Multi-file selection chosen at intake (no live setter exists).
        only_files,
        ..Default::default()
    };
    let add = match adder {
        Adder::Magnet(magnet) => AddTorrent::from_url(magnet),
        Adder::File(bytes) => AddTorrent::from_bytes(bytes),
    };
    let handle = match session.add_torrent(add, Some(opts)).await {
        // The session outlives our registry: only adopt a stale handle
        // on a file-filter match, else fail fast (delete the owner first).
        Ok(AddTorrentResponse::AlreadyManaged(_, handle)) if handle.only_files() != want_files => {
            ACTIVE.lock().await.remove(&id);
            fail(gettext("Torrent is already in the queue"));
            return;
        }
        Ok(resp) => resp.into_handle(),
        Err(e) => {
            ACTIVE.lock().await.remove(&id);
            fail(format!("Cannot add torrent: {e}"));
            return;
        }
    };
    let Some(handle) = handle else {
        ACTIVE.lock().await.remove(&id);
        fail(gettext("Torrent produced no files"));
        return;
    };
    // Cancelled while adding: the pump is gone, so clean up the orphan.
    let keep = {
        let mut active = ACTIVE.lock().await;
        match active.get_mut(&id) {
            Some(a) => {
                a.handle = Some(handle.clone());
                true
            }
            None => false,
        }
    };
    // Drop the session entry; partial files stay for resume like any other cancel.
    if !keep {
        let _ = session
            .delete(TorrentIdOrHash::Hash(handle.info_hash()), false)
            .await;
        return;
    }
    if ACTIVE.lock().await.get(&id).is_some_and(|a| a.paused) {
        let _ = session.pause(&handle).await;
    } else if handle.is_paused() {
        // Adopted a persisted handle: session restore re-added it paused, but a
        // spawn always means "run" (fresh adds start live, so this is the
        // persistence path).
        unpause_adopted(&session, id, &handle).await;
    }
    let finished = poll_loop(
        session.clone(),
        handle,
        &stub,
        seed_finished,
        seed_ratio,
        seed_time_min,
        &tx,
    )
    .await;
    // Finished rows leave the session unless still seeding (see
    // `maybe_evict`). Files stay on disk.
    maybe_evict(
        &session,
        TorrentIdOrHash::Hash(hash_id),
        finished,
        seed_finished,
    )
    .await;
    ACTIVE.lock().await.remove(&id);
    // Archives live as long as their rows: delete_download() drops them,
    // remove() keeps them for Undo, sweep_archives() cleans orphans.
    // (Deleting on finish would break trash for Done rows, which need the
    // archive to locate a multi-file subfolder.)
}

#[cfg(test)]
#[path = "torrent_tests.rs"]
mod tests;
