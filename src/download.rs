use crate::engine_msg::{DEST_EXISTS, EngineMsg};
use crate::file_names::{
    dedupe_filename, filename_from_url, fmt_bytes, name_stem, rename_noreplace,
    restrict_filename_ascii, sane_filename, shorten_filename,
};
use crate::runtime::tokio_rt;
use gettextrs::{gettext, ngettext};
use gtk4::gio::prelude::*;
use gtk4::{gio, glib};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant, SystemTime};

mod cancellation;
mod deletion;
mod engine;
mod queue_persistence;
mod queue_restore;
mod scheduler;
#[cfg(test)]
use cancellation::cleanup_discarded_video_files;
#[cfg(test)]
use engine::SpawnVideoParams;
#[cfg(test)]
use queue_restore::PendingRestore;

/// Counter for unique queue temp-file names. Combined with the PID, this makes
/// temp paths unique across writers in the same process.
static QUEUE_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Tracks fire-and-forget final-path sizing even when its GLib future is
/// cancelled during shutdown.
struct SizingGuard(Rc<Cell<usize>>);

impl SizingGuard {
    fn new(inflight: Rc<Cell<usize>>) -> Self {
        inflight.set(inflight.get().saturating_add(1));
        Self(inflight)
    }
}

impl Drop for SizingGuard {
    fn drop(&mut self) {
        self.0.set(self.0.get().saturating_sub(1));
    }
}

#[cfg(test)]
mod sizing_guard_tests {
    use super::*;

    #[test]
    fn sizing_guard_balances_the_counter_when_dropped() {
        let inflight = Rc::new(Cell::new(0));
        let guard = SizingGuard::new(Rc::clone(&inflight));
        assert_eq!(inflight.get(), 1);

        let second = SizingGuard::new(Rc::clone(&inflight));
        assert_eq!(inflight.get(), 2);

        drop(guard);
        assert_eq!(inflight.get(), 1);
        drop(second);
        assert_eq!(inflight.get(), 0);
    }
}

/// Substitute translated message placeholders in one pass. Inserted remote
/// filenames and error text are never scanned again for placeholder syntax.
fn format_gettext_template(template: &str, vars: &[(&str, &str)]) -> String {
    let mut output = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        output.push_str(&rest[..start]);
        let tail = &rest[start..];
        if let Some((key, value)) = vars.iter().find(|(key, _)| {
            let placeholder = format!("{{{key}}}");
            tail.starts_with(&placeholder)
        }) {
            output.push_str(value);
            rest = &tail[key.len() + 2..];
        } else {
            output.push('{');
            rest = &tail[1..];
        }
    }
    output.push_str(rest);
    output
}

#[cfg(test)]
mod gettext_template_tests {
    use super::*;

    #[test]
    fn inserted_values_are_not_rescanned_as_placeholders() {
        assert_eq!(
            format_gettext_template(
                "Restored {restored}/{total}; first failure: {filename}: {error}",
                &[
                    ("restored", "2"),
                    ("total", "5"),
                    ("filename", "video-{total}.mkv"),
                    ("error", "file contains {restored}"),
                ],
            ),
            "Restored 2/5; first failure: video-{total}.mkv: file contains {restored}"
        );
    }

    #[test]
    fn unknown_placeholders_are_preserved() {
        assert_eq!(
            format_gettext_template("Unknown {other}; value {value}", &[("value", "ok")]),
            "Unknown {other}; value ok"
        );
    }
}

/// Re-register the network restriction persisted for a guarded queue row.
/// Keep the download and gallery restore paths identical as the policy evolves.
fn recheck_public_host(url: &str, kind: &str) -> Result<(), String> {
    let host = url::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_string))
        .ok_or_else(|| format!("Invalid guarded {kind} URL"))?;
    if !crate::download_net::require_public_resolution(&host) {
        return Err(format!(
            "Could not restore the guarded {kind} because its host could not be verified as public"
        ));
    }
    Ok(())
}

/// Facade: HTTP fetch engine lives in [`download_fetch`](crate::download_fetch)
/// now (no re-exports: the manager consumes it here, tests import it directly).
use crate::download_fetch::{
    FetchCtx, StartMode, apply_torrent_limits, run_download, truncate_to_prefix,
};
/// Facade: intake normalization lives in [`download_intake`](crate::download_intake)
/// now; the re-exports keep the in-tree `crate::download::X` paths working.
pub use crate::download_intake::normalize_url;
/// Facade: network options + proxy/client plumbing lives in
/// [`download_net`](crate::download_net); re-exports keep `crate::download::X` working.
use crate::download_net::http_client_for;
pub use crate::download_net::{
    DownloadOptions, PROXY_MODE_MANUAL, proxy_mode_index, proxy_mode_labels, proxy_mode_value,
    proxy_type_index, proxy_type_labels, proxy_type_value,
};
/// Facade: segmented-piece math lives in [`download_pieces`](crate::download_pieces)
/// now (no re-exports: the engine consumes it here, tests import it directly).
use crate::download_pieces::BLOCK_CELLS;
use crate::download_pieces::SegmentState;
/// Facade: rate parsing/pacing + progress text lives in
/// [`download_rate`](crate::download_rate); the engine consumes it here, tests import it directly.
use crate::download_rate::{fmt_eta, format_amounts, parse_rate, publish_rate_limit};
/// Facade: the row object lives in [`download_row`](crate::download_row) now;
/// this re-export keeps every `crate::download::X` path working.
pub use crate::download_row::DownloadItem;
pub use crate::download_store::DownloadStatus;
/// Facade: queue persistence model lives in [`download_store`](crate::download_store)
/// now; the status re-export keeps every `crate::download::X` path working.
/// (The stored structs stay imported below without re-export.)
use crate::download_store::{QUEUE_VERSION, StoredItem, StoredQueue};
use crate::video::AttemptGate;

/// Shared runtime projection index for collection-owned files and transient GTK rows.
/// Persisted records stay in their owning GalleryJob or VideoPlaylistState.
#[derive(Default)]
struct CollectionProjectionIndex {
    state_indices: HashMap<String, usize>,
    rows: HashMap<String, DownloadItem>,
    /// Only playlists maintain a progress aggregate; gallery progress comes from gallery-dl.
    summary: Option<CollectionProjectionSummary>,
}

/// Incremental aggregate for a playlist's persisted file state.
#[derive(Clone, Copy, Debug, Default)]
struct CollectionProjectionSummary {
    total: usize,
    done: usize,
    progress_sum: f64,
}

impl CollectionProjectionSummary {
    fn update(
        &mut self,
        old_status: DownloadStatus,
        old_progress: f64,
        new_status: DownloadStatus,
        new_progress: f64,
    ) {
        self.done = self
            .done
            .saturating_sub(usize::from(old_status == DownloadStatus::Done))
            .saturating_add(usize::from(new_status == DownloadStatus::Done));
        self.progress_sum = (self.progress_sum - old_progress.clamp(0.0, 1.0)
            + new_progress.clamp(0.0, 1.0))
        .max(0.0);
    }

    fn fraction(self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            (self.progress_sum / self.total as f64).clamp(0.0, 1.0)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CollectionProjectionKind {
    Gallery,
    Playlist,
}

fn restored_collection_child_status(
    kind: CollectionProjectionKind,
    owner_status: DownloadStatus,
    child_status: DownloadStatus,
) -> DownloadStatus {
    use DownloadStatus::{Cancelled, Done, Downloading, Failed, Paused, Queued};

    match (kind, owner_status, child_status) {
        (_, Done, _) => Done,
        (CollectionProjectionKind::Gallery, Failed, Queued | Paused | Downloading) => Failed,
        (CollectionProjectionKind::Gallery, Cancelled, Queued | Paused | Downloading) => Cancelled,
        (CollectionProjectionKind::Gallery, Paused, Queued | Downloading) => Paused,
        (CollectionProjectionKind::Gallery, Queued, Downloading | Paused) => Queued,
        (CollectionProjectionKind::Playlist, Failed, Downloading) => Failed,
        (CollectionProjectionKind::Playlist, Cancelled, Downloading) => Cancelled,
        (CollectionProjectionKind::Playlist, Paused, Downloading) => Paused,
        (CollectionProjectionKind::Playlist, Queued, Downloading) => Queued,
        (_, _, status) => status,
    }
}

fn collection_child_status_detail(
    kind: CollectionProjectionKind,
    status: DownloadStatus,
) -> String {
    match (kind, status) {
        (CollectionProjectionKind::Gallery, DownloadStatus::Queued) => gettext("Queued"),
        (CollectionProjectionKind::Playlist, DownloadStatus::Queued) => {
            gettext("Waiting in collection")
        }
        (_, DownloadStatus::Downloading) => gettext("Downloading"),
        (_, DownloadStatus::Paused) => gettext("Paused"),
        (_, DownloadStatus::Done) => gettext("Finished"),
        (CollectionProjectionKind::Gallery, DownloadStatus::Failed) => {
            gettext("Gallery file failed")
        }
        (CollectionProjectionKind::Playlist, DownloadStatus::Failed) => gettext("Failed"),
        (_, DownloadStatus::Cancelled) => gettext("Cancelled"),
        (_, DownloadStatus::Scheduled) => gettext("Scheduled"),
    }
}

#[cfg(test)]
mod collection_projection_tests {
    use super::*;

    #[test]
    fn restore_status_policy_preserves_gallery_and_playlist_semantics() {
        use DownloadStatus::{Cancelled, Done, Downloading, Failed, Paused, Queued};

        assert_eq!(
            restored_collection_child_status(CollectionProjectionKind::Gallery, Failed, Queued),
            Failed
        );
        assert_eq!(
            restored_collection_child_status(CollectionProjectionKind::Gallery, Paused, Queued),
            Paused
        );
        assert_eq!(
            restored_collection_child_status(CollectionProjectionKind::Playlist, Failed, Queued),
            Queued
        );
        assert_eq!(
            restored_collection_child_status(
                CollectionProjectionKind::Playlist,
                Failed,
                Downloading
            ),
            Failed
        );
        assert_eq!(
            restored_collection_child_status(CollectionProjectionKind::Gallery, Done, Cancelled),
            Done
        );
    }

    #[test]
    fn playlist_projection_summary_updates_done_and_progress_incrementally() {
        let mut summary = CollectionProjectionSummary {
            total: 3,
            done: 1,
            progress_sum: 1.75,
        };

        summary.update(DownloadStatus::Downloading, 0.25, DownloadStatus::Done, 1.0);

        assert_eq!(summary.done, 2);
        assert!((summary.progress_sum - 2.5).abs() < f64::EPSILON);
        assert!((summary.fraction() - (2.5 / 3.0)).abs() < f64::EPSILON);
    }
}

/// A removal whose cleanup is still draining; `worker_abort` is absent when
/// no worker had to be awaited.
struct PendingDiscard {
    /// Aborts the *worker*: aborting the finalizer would drop this handle and detach the task instead of stopping it.
    worker_abort: Option<tokio::task::AbortHandle>,
    finalizer: tokio::task::JoinHandle<()>,
}

/// Await a GLib-local future without allowing shutdown to exceed its join budget.
async fn wait_for_local_future<F>(future: F, timeout: Duration) -> bool
where
    F: std::future::Future<Output = ()>,
{
    use futures_util::future::{Either, select};

    if timeout.is_zero() {
        return false;
    }
    matches!(
        select(Box::pin(future), Box::pin(glib::timeout_future(timeout)),).await,
        Either::Left(_)
    )
}

/// Destinations with a discard in flight. Stem-wide: the finalizer sweeps
/// `<stem>.<kind>.*`, so reservations key on parent dir + file stem too.
/// Counts, not a set: stacked remove/Undo cycles need one release per teardown.
#[derive(Debug, Default)]
struct Reservations {
    /// Exact destination paths with a discard in flight, by active count.
    paths: std::collections::HashMap<std::path::PathBuf, usize>,
    /// `(parent dir, file stem)` pairs with a discard in flight, by count.
    stems: std::collections::HashMap<(std::path::PathBuf, String), usize>,
}

/// Sweep key for `clean_dest_parts`; `None` exactly when the sweeper early-returns.
fn reservation_stem(dest: &std::path::Path) -> Option<(std::path::PathBuf, String)> {
    match (dest.parent(), dest.file_stem().and_then(|s| s.to_str())) {
        (Some(dir), Some(stem)) => Some((dir.to_path_buf(), stem.to_string())),
        _ => None,
    }
}

impl Reservations {
    fn insert(&mut self, dest: &std::path::Path) {
        *self.paths.entry(dest.to_path_buf()).or_default() += 1;
        if let Some(key) = reservation_stem(dest) {
            *self.stems.entry(key).or_default() += 1;
        }
    }

    fn remove(&mut self, dest: &std::path::Path) {
        decrement(&mut self.paths, &dest.to_path_buf());
        if let Some(key) = reservation_stem(dest) {
            decrement(&mut self.stems, &key);
        }
    }

    fn contains(&self, dest: &std::path::Path) -> bool {
        self.paths.contains_key(dest)
            || reservation_stem(dest).is_some_and(|key| self.stems.contains_key(&key))
    }
}

/// Temporary finished-name claims held while a blocking worker performs a no-clobber rename.
struct FinalNameReservations {
    reservations: std::sync::Arc<std::sync::Mutex<Reservations>>,
    paths: Vec<std::path::PathBuf>,
}

impl FinalNameReservations {
    /// Leave one reservation alive for a discard finalizer to release after it
    /// has swept the moved file and row-owned staging.
    fn retain_path(&mut self, retained: &std::path::Path) {
        if let Ok(mut reservations) = self.reservations.lock() {
            for path in self.paths.drain(..) {
                if path.as_path() != retained {
                    reservations.remove(&path);
                }
            }
        }
    }
}

impl Drop for FinalNameReservations {
    fn drop(&mut self) {
        if let Ok(mut reservations) = self.reservations.lock() {
            for path in &self.paths {
                reservations.remove(path);
            }
        }
        // A row restored through Undo may have been parked behind one of these claims.
        wake_queue();
    }
}

fn decrement<K: Eq + std::hash::Hash>(counts: &mut std::collections::HashMap<K, usize>, key: &K) {
    let empty = counts
        .get_mut(key)
        .map(|n| {
            *n = n.saturating_sub(1);
            *n == 0
        })
        .unwrap_or(false);
    if empty {
        counts.remove(key);
    }
}

/// Row detail while a video row waits for the resolver worker.
fn pending_resolve_detail(audio_only: bool) -> String {
    if audio_only {
        gettext("Waiting to resolve audio…")
    } else {
        gettext("Waiting to resolve media…")
    }
}

/// Whether `parent` resolves inside `dest_dir`: canonicalizing both sides
/// proves no planted symlink in the parent chain diverts the write outside
/// the destination. Unresolvable paths fail closed.
fn parent_contained_in_dest(parent: &std::path::Path, dest_dir: &str) -> bool {
    match (
        std::fs::canonicalize(parent),
        std::fs::canonicalize(dest_dir),
    ) {
        (Ok(canon), Ok(base)) => canon.starts_with(&base),
        _ => false,
    }
}

/// Create the download's parent dir, refusing a planted symlink. An existing
/// parent is containment-checked before any filesystem touch; a missing one
/// is created then verified (a link can be swapped in mid-call).
/// `symlink_metadata` sees even dangling links. A `create_dir_all` failure
/// is `Err`, not a containment refusal, so callers report it distinctly.
fn ensure_contained_parent(
    parent: &std::path::Path,
    dest_dir: &str,
) -> Result<bool, std::io::Error> {
    if std::fs::symlink_metadata(parent).is_ok() && !parent_contained_in_dest(parent, dest_dir) {
        return Ok(false);
    }
    std::fs::create_dir_all(parent)?;
    Ok(parent_contained_in_dest(parent, dest_dir))
}

pub struct DownloadManager {
    // Borrow discipline: never hold RefCells across `set_*` notifies or `changed()`; GTK re-enters and panics. Keep borrows scoped.
    store: gio::ListStore,
    settings: crate::settings::AppSettings,
    running: RefCell<HashMap<u64, tokio::task::JoinHandle<()>>>,
    /// Aborted workers whose task has not finished dropping its owned resources yet.
    /// These rows cannot be restarted, but do not consume a global download slot.
    stopping: std::sync::Arc<std::sync::Mutex<HashSet<u64>>>,
    /// Rows whose filesystem preparation is in flight; these consume scheduler slots too.
    preparing: RefCell<HashSet<u64>>,
    /// Filesystem-preparation task handles; abort them on manager drop so thread-bound GLib futures cannot leak into another test's context.
    preparation_handles: RefCell<HashMap<u64, glib::JoinHandle<()>>>,
    /// Pump task handles by download ID, so tests can wait for pump completion (a `spawn_future_local` task reaped on another thread trips glib's thread guard).
    pub(crate) pump_handles: RefCell<HashMap<u64, glib::JoinHandle<()>>>,
    #[cfg(test)]
    final_name_after_move_hook: RefCell<Option<std::sync::Arc<dyn Fn() + Send + Sync>>>,
    next_id: Cell<u64>,
    on_change: RefCell<Option<Box<dyn Fn()>>>,
    batch: Cell<u32>,
    /// Server-advertised names, applied at Finished so the engine never writes through a renamed path mid-transfer.
    pending_names: RefCell<HashMap<u64, String>>,
    /// Cached queued-row count backing the per-row Queue button; refreshed in changed().
    queued: Cell<usize>,
    /// Spawn generation per row. A stale pump future must not touch progress, status, notifications, or the new engine's handle.
    epoch: RefCell<HashMap<u64, u64>>,
    /// Resume bitmaps for segmented downloads (session-only, main thread).
    segment_state: RefCell<HashMap<u64, SegmentState>>,
    /// Torrent per-piece haves by row (session-only, main thread, never
    /// persisted: re-polled from the session on every spawn).
    torrent_pieces: RefCell<HashMap<u64, Vec<bool>>>,
    /// Server-advertised Last-Modified by row, applied at Finished when
    /// the keep-server-date setting is on. Best-effort only.
    server_mtime: RefCell<HashMap<u64, SystemTime>>,
    /// Video-page source by row (in-memory only, like the maps above):
    /// persisted on [`StoredItem`] and re-staged on restore.
    video_sources: RefCell<HashMap<u64, crate::media_types::VideoSource>>,
    /// Gallery jobs by row id. Stored at enqueue time (carries the user's
    /// filter selection); consumed by spawn_gallery.
    gallery_jobs: RefCell<HashMap<u64, crate::gallery_argv::GalleryJob>>,
    /// O(1) path-to-state and path-to-row lookups for high-frequency gallery progress.
    /// Rebuilt lazily from the persisted job and discarded with its owner.
    gallery_file_row_index: RefCell<HashMap<u64, CollectionProjectionIndex>>,
    /// O(1) playlist-entry and child-row lookup plus incremental summary counters.
    /// Rebuilt lazily from owner state and discarded with that owner.
    video_playlist_file_row_index: RefCell<HashMap<u64, CollectionProjectionIndex>>,
    /// Abort senders for resolver workers, by row. Signalled from pause/park/cancel so extractor streams stop promptly.
    video_abort: RefCell<HashMap<u64, tokio::sync::oneshot::Sender<crate::video::StopIntent>>>,
    /// Delivery decision per video attempt, by row. Manager and worker arbitrate on this one object.
    gates: RefCell<HashMap<u64, std::sync::Arc<AttemptGate>>>,
    /// Destinations with a discard in flight (exact path + stem key). Consulted by intake *and* `unremove`, which bypasses intake.
    reservations: std::sync::Arc<std::sync::Mutex<Reservations>>,
    /// Finalizers reclaiming a removed row's scratch, by row. A worker abort handle is kept when one had to be awaited; aborting the finalizer detaches it (#180).
    discards: RefCell<HashMap<u64, PendingDiscard>>,
    /// Rows currently capturing a live stream. Pause/cancel/park only signal these; the worker finalizes and its message drives the row.
    live_rows: RefCell<std::collections::HashSet<u64>>,
    /// Rows whose engine ever started (session-only, persisted on [`StoredItem`]):
    /// a fresh row refuses an existing dest Parabolic-style, while a restored
    /// row resumes its own partial.
    started: RefCell<HashSet<u64>>,
    /// Set by shutdown(): stale engine futures must not re-persist or
    /// re-mark rows once the authoritative shutdown persist has run.
    draining: Cell<bool>,
    /// In-flight finished-row disk sizings (see `set_finished_detail`): the
    /// futures are fire-and-forget, so the test suite drains on this counter
    /// instead of guessing how long GIO's blocking pool takes.
    sizing_inflight: Rc<Cell<usize>>,
    /// Raw `--dump-single-json` by page URL (single videos only), cached at
    /// probe time. The spawn takes it to build `--load-info-json`, skipping
    /// a second extraction. Session-only; never persisted. Stale entries
    /// are harmless: yt-dlp re-extracts if a loaded format URL 403/404s.
    probe_json_cache: RefCell<HashMap<ProbeJsonKey, (std::time::Instant, serde_json::Value)>>,
    /// The one-shot schedule timer, removed when no scheduled rows remain.
    scheduler_source: RefCell<Option<glib::SourceId>>,
    /// Re-arms the timer when any queue row changes.
    scheduler_refresh: RefCell<Option<Box<dyn Fn()>>>,
    /// Guards normal async shutdown and the synchronous fallback.
    shutdown_started: Cell<bool>,
    shutdown_finished: Cell<bool>,
    /// Explicit restore lifecycle; Failed prevents partial models from overwriting
    /// the last known-good queue file and releases intake waiters.
    restore_state: Cell<RestoreState>,
    /// Queue mutations received while loading are persisted after restore completes.
    restore_dirty: Cell<bool>,
    /// Production queue loader; retained so orderly shutdown waits before persisting.
    restore_task: RefCell<Option<glib::JoinHandle<()>>>,
    /// Multi-subscriber readiness signal; external URL intake waits until IDs are restored.
    restore_ready: tokio::sync::watch::Sender<bool>,
    /// Queue persist channel: `watch` keeps only the latest value, so rapid
    /// state changes coalesce to a single write. The tokio task (below) is the
    /// sole writer, which makes writes ordered by construction.
    /// Tests write synchronously and never use the channel.
    #[cfg_attr(test, allow(dead_code))]
    persist_tx: RefCell<Option<tokio::sync::watch::Sender<Option<StoredQueue>>>>,
    /// Handle for the queue persist task; awaited on shutdown after the final
    /// state is sent. Tests never spawn it.
    #[cfg_attr(test, allow(dead_code))]
    persist_task: RefCell<Option<tokio::task::JoinHandle<()>>>,
}

/// What a stop means for the worker and the bytes it has written (manager policy; distinct from the worker's `StopIntent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// Keep what is recorded: signal the worker so it finalizes and delivers the partial.
    Preserve,
    /// Throw it away: only row removal does this, so no delivered file outlives its row.
    Discard,
}

#[derive(Debug, Clone)]
pub(crate) struct RemovedSnapshot {
    pub url: String,
    pub dest_dir: String,
    pub filename: String,
    pub status: DownloadStatus,
    pub progress: f64,
    pub detail: String,
    pub output_dir: String,
    pub segments: Option<SegmentState>,
    /// Staged video source, so Undo on a video row keeps the Page marker instead of demoting it.
    pub video_source: Option<crate::media_types::VideoSource>,
    /// Whether the engine ever started this row: an Undo-restored partial is
    /// the row's own bytes, so the spawn gate must resume it instead of
    /// refusing it Parabolic-style as a foreign file at the destination.
    pub started: bool,
    /// Preserve the untrusted-handoff policy reference when Undo restores a row.
    pub public_only: bool,
    /// Keep a restored child in its original multi-item group.
    pub group: Option<crate::download_store::DownloadGroup>,
    /// Restore a removed gallery collection as one gallery-dl process.
    pub gallery_job: Option<crate::gallery_argv::GalleryJob>,
}

impl RemovedSnapshot {
    /// Capture all state needed to reconstruct a removed queue row.
    pub(crate) fn capture(manager: &DownloadManager, item: &DownloadItem) -> Self {
        let id = item.id();
        Self {
            url: item.url().to_string(),
            dest_dir: item.dest_dir().to_string(),
            filename: item.filename().to_string(),
            status: item.status(),
            progress: item.progress(),
            detail: item.detail().to_string(),
            output_dir: item.output_dir().to_string(),
            segments: manager.segments_of(id),
            video_source: manager.video_source(id),
            started: manager.has_started(id),
            public_only: item.public_only(),
            group: item.group(),
            gallery_job: manager.gallery_job(id),
        }
    }
}

fn saturating_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod saturating_count_tests {
    use super::saturating_u32;

    #[test]
    fn count_conversion_saturates_without_truncating() {
        assert_eq!(saturating_u32(0), 0);
        assert_eq!(saturating_u32(42), 42);
        assert_eq!(saturating_u32(usize::MAX), u32::MAX);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreState {
    /// Queue restore has not yet finished.
    Pending,
    /// Queue restore and synchronous staging cleanup finished successfully.
    Ready,
    /// Queue restore failed; keep the persisted queue file untouched.
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreOutcome {
    Succeeded,
    Failed,
}

/// Makes unexpected restore-task exits observable to every waiting intake path.
/// Releases intake waiters when a restore task exits unexpectedly and records
/// failure rather than allowing a partial model to replace the saved queue.
struct RestoreTaskGuard {
    manager: std::rc::Weak<DownloadManager>,
    finished: bool,
}

impl Drop for RestoreTaskGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Some(manager) = self.manager.upgrade() {
            if manager.restore_state.get() == RestoreState::Pending {
                manager.finish_restore(RestoreOutcome::Failed);
            } else {
                manager.restore_ready.send_replace(true);
            }
        }
    }
}

impl Drop for DownloadManager {
    fn drop(&mut self) {
        // Abort thread-bound local tasks on their owning thread. Tests and app
        // shutdown normally drain them; this backstop prevents an unfinished
        // preparation/pump from being reaped by another test's context thread.
        for (_, handle) in self.preparation_handles.borrow_mut().drain() {
            handle.abort();
        }
        for (_, handle) in self.pump_handles.borrow_mut().drain() {
            handle.abort();
        }
        // Remove the scheduler tick: `timeout_add_seconds_local` binds its
        // closure to the creating thread via glib's thread guard, so a leaked
        // source dispatched by a later context iteration on another thread
        // panics ("Value accessed from different thread...").
        self.stop_scheduler();
        // Clear the thread-local only if it holds THIS manager's Weak.
        // A second manager created on this thread overwrites the slot; an
        // unconditional clear here would break the survivor's wakeups.
        // (Managers must not overlap on the same thread: `wake_queue()` routes
        // to whichever manager last published its Weak.)
        let self_ptr = self as *const Self;
        MANAGER_WEAK.with(|w| {
            let mut slot = w.borrow_mut();
            let is_mine = slot.as_ref().is_some_and(|weak| {
                weak.upgrade()
                    .is_some_and(|m| std::rc::Rc::as_ptr(&m) == self_ptr)
            });
            if is_mine {
                *slot = None;
            }
        });
    }
}

/// RAII batch guard: bulk inserts open one with [`DownloadManager::batch_guard`]
/// and dropping it closes the batch (persist the queue, refresh the UI, kick the
/// scheduler) exactly once. An early return between open and close can no longer
/// leave the UI frozen behind a raised batch counter. Nesting-safe.
#[must_use = "dropping the guard immediately closes the batch"]
pub struct BatchGuard {
    manager: std::rc::Rc<DownloadManager>,
}

impl Drop for BatchGuard {
    fn drop(&mut self) {
        self.manager.close_batch();
    }
}

/// Wake the queue from any thread: schedules a one-shot `start_next()` on the
/// main loop. Uses `idle_add` (thread-safe) with a `Send` closure that captures
/// nothing — it finds the manager via a thread-local set in `new()`. The source
/// returns `Break` after firing once, so it never spins the main loop (tests'
/// `quiesce()` needs the context to go idle). No `!Send` values are held, so a
/// source reaped on another thread can't trip glib's thread guard.
fn wake_queue() {
    glib::idle_add(|| {
        MANAGER_WEAK.with(|w| {
            if let Some(weak) = w.borrow().as_ref()
                && let Some(m) = weak.upgrade()
                // Never start rows while tearing down: shutdown awaits discard finalizers, each of which wakes the queue.
                && !m.draining.get()
            {
                m.start_next();
            }
        });
        glib::ControlFlow::Break
    });
}

thread_local! {
    /// The current thread's DownloadManager, for `wake_queue()`'s `Send` closure.
    /// The closure runs on the main thread via `idle_add`, so reading the
    /// thread-local there yields the manager created on that thread.
    /// Managers must not overlap on the same thread: a second `new()` overwrites
    /// the slot, and `Drop` only clears its own entry (see the identity check).
    static MANAGER_WEAK: RefCell<Option<std::rc::Weak<DownloadManager>>> =
        const { RefCell::new(None) };
}

/// Cache key for probe info JSON: the JSON is only reusable when the
/// download runs under the same conditions the probe did — same page, same
/// cookie source, same proxy. (page_url, cookies_browser, proxy cli_url)
pub(crate) type ProbeJsonKey = (String, String, Option<String>);

/// Probe info JSON older than this is never reused: signed media URLs inside
/// go stale, and the extractor's format list may have rotated.
const PROBE_JSON_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// Whether a cached probe JSON timestamp is too old to reuse.
fn probe_json_expired(cached_at: std::time::Instant) -> bool {
    cached_at.elapsed() > PROBE_JSON_TTL
}

/// Return whether a stored video source belongs to the normalized queue URL.
fn video_source_matches_url(source: &crate::media_types::VideoSource, url: &str) -> bool {
    match source {
        crate::media_types::VideoSource::Page { page_url, .. } => {
            normalize_url(page_url).is_ok_and(|page| page == url)
        }
        crate::media_types::VideoSource::Manifest { source } => {
            crate::manifest_intake::valid_source(source)
                && normalize_url(&source.url).is_ok_and(|manifest_url| manifest_url == url)
        }
        crate::media_types::VideoSource::Playlist {
            page_url, state, ..
        } => !state.files.is_empty() && normalize_url(page_url).is_ok_and(|page| page == url),
        crate::media_types::VideoSource::Direct => false,
    }
}

/// Queue + engine owner: persists the queue, spawns downloads, notifies the UI.
impl DownloadManager {
    /// Cache a probe's raw info JSON for `--load-info-json`.
    /// Overwrites any earlier entry for the key. Bounded (64 entries):
    /// probed-but-never-downloaded URLs must not accumulate ~100KB each
    /// for the whole session.
    pub(crate) fn cache_probe_json(&self, key: ProbeJsonKey, json: serde_json::Value) {
        let mut cache = self.probe_json_cache.borrow_mut();
        // A probed video may occupy two entries (typed + retargeted URL keys).
        if cache.len() >= 64 {
            cache.clear();
        }
        cache.insert(key, (std::time::Instant::now(), json));
    }

    /// Take the cached probe JSON for a key (single use: the download
    /// consumes it). Entries older than `PROBE_JSON_TTL` are dropped —
    /// signed media URLs inside go stale.
    pub(crate) fn take_probe_json(&self, key: &ProbeJsonKey) -> Option<serde_json::Value> {
        let (cached_at, json) = self.probe_json_cache.borrow_mut().remove(key)?;
        if probe_json_expired(cached_at) {
            None
        } else {
            Some(json)
        }
    }

    /// Create a manager over `store`; call [`DownloadManager::restore_queue`] once.
    pub fn new(store: gio::ListStore, settings: crate::settings::AppSettings) -> Rc<Self> {
        // Queue persist worker: `watch` keeps only the latest value (coalescing
        // built in); the task is the sole writer, so writes are ordered.
        // `changed()` delivers the final value before the closed-channel Err,
        // so the last persist completes before shutdown awaits the task.
        #[cfg(not(test))]
        let (persist_tx, persist_task) = {
            let (tx, rx) = tokio::sync::watch::channel(None::<StoredQueue>);
            let task = tokio_rt().spawn(Self::persist_task_loop(rx, |queue: StoredQueue| {
                if let Some(text) = Self::serialize_snapshot(queue) {
                    Self::write_queue_file(&text);
                }
            }));
            (Some(tx), Some(task))
        };
        #[cfg(test)]
        let (persist_tx, persist_task) = (None, None);

        let (restore_ready, _) = tokio::sync::watch::channel(false);
        let this = Rc::new(Self {
            store,
            settings,
            running: RefCell::new(HashMap::new()),
            stopping: std::sync::Arc::new(std::sync::Mutex::new(HashSet::new())),
            preparing: RefCell::new(HashSet::new()),
            preparation_handles: RefCell::new(HashMap::new()),
            pump_handles: RefCell::new(HashMap::new()),
            #[cfg(test)]
            final_name_after_move_hook: RefCell::new(None),
            next_id: Cell::new(1),
            on_change: RefCell::new(None),
            batch: Cell::new(0),
            pending_names: RefCell::new(HashMap::new()),
            queued: Cell::new(0),
            epoch: RefCell::new(HashMap::new()),
            segment_state: RefCell::new(HashMap::new()),
            started: RefCell::new(HashSet::new()),
            torrent_pieces: RefCell::new(HashMap::new()),
            server_mtime: RefCell::new(HashMap::new()),
            video_sources: RefCell::new(HashMap::new()),
            gallery_jobs: RefCell::new(HashMap::new()),
            gallery_file_row_index: RefCell::new(HashMap::new()),
            video_playlist_file_row_index: RefCell::new(HashMap::new()),
            video_abort: RefCell::new(HashMap::new()),
            gates: RefCell::new(HashMap::new()),
            reservations: std::sync::Arc::new(std::sync::Mutex::new(Reservations::default())),
            discards: RefCell::new(HashMap::new()),
            live_rows: RefCell::new(std::collections::HashSet::new()),
            draining: Cell::new(false),
            sizing_inflight: Rc::new(Cell::new(0)),
            probe_json_cache: RefCell::new(HashMap::new()),
            scheduler_source: RefCell::new(None),
            scheduler_refresh: RefCell::new(None),
            shutdown_started: Cell::new(false),
            shutdown_finished: Cell::new(false),
            restore_state: Cell::new(if cfg!(test) {
                RestoreState::Ready
            } else {
                RestoreState::Pending
            }),
            restore_dirty: Cell::new(false),
            restore_task: RefCell::new(None),
            restore_ready,
            persist_tx: RefCell::new(persist_tx),
            persist_task: RefCell::new(persist_task),
        });
        // Publish the Weak for `wake_queue()`'s `Send` closure (see `MANAGER_WEAK`).
        MANAGER_WEAK.with(|w| *w.borrow_mut() = Some(Rc::downgrade(&this)));
        // Live preferences: raising the limit wakes queued rows now; lowering it parks the newest running rows. Weak ref: settings must not keep the manager alive.
        // Owner-thread guard: queue actions run only where the manager was created (production writes are main-thread; foreign-thread writes only via the test backend).
        let owner = std::thread::current().id();
        let weak = Rc::downgrade(&this);
        this.settings
            .connect_changed(Some(crate::settings::key::MAX_CONCURRENT), move |_, _| {
                if std::thread::current().id() != owner {
                    return;
                }
                if let Some(m) = weak.upgrade() {
                    m.start_next();
                    m.preempt_excess();
                    m.changed();
                }
            });
        let settings_weak = this.settings.downgrade();
        this.settings
            .connect_changed(Some(crate::settings::key::SPEED_LIMIT), move |_, _| {
                if let Some(s) = settings_weak.upgrade() {
                    let s = crate::settings::AppSettings::from(s);
                    publish_rate_limit(&s);
                    apply_torrent_limits(&s);
                }
            });
        let settings_weak = this.settings.downgrade();
        this.settings.connect_changed(
            Some(crate::settings::key::TORRENT_UPLOAD_LIMIT),
            move |_, _| {
                if let Some(s) = settings_weak.upgrade() {
                    let s = crate::settings::AppSettings::from(s);
                    apply_torrent_limits(&s);
                }
            },
        );
        // Scheduling preference: toggling it off stops the 30s timer entirely;
        // toggling it on restarts it. Weak ref: settings must not keep the manager alive.
        let sched_weak = Rc::downgrade(&this);
        this.settings.connect_changed(
            Some(crate::settings::key::ENABLE_SCHEDULED_DOWNLOADS),
            move |_, _| {
                if std::thread::current().id() != owner {
                    return;
                }
                if let Some(m) = sched_weak.upgrade() {
                    m.sync_scheduler();
                }
            },
        );
        this
    }

    /// UI refresh callback, invoked after every state change.
    pub fn set_on_change(&self, cb: impl Fn() + 'static) {
        *self.on_change.borrow_mut() = Some(Box::new(cb));
    }

    /// Open a bulk-intake batch (playlist picker, worker expansion, queue restore).
    /// The returned guard closes it on drop: persist the queue, refresh the UI
    /// and kick the scheduler exactly once. Nesting-safe counter.
    pub fn batch_guard(self: &Rc<Self>) -> BatchGuard {
        self.batch.set(self.batch.get() + 1);
        BatchGuard {
            manager: Rc::clone(self),
        }
    }

    /// Close one batch level. Only the outermost close kicks the scheduler: the
    /// per-insert kick is deferred while a batch is open (see `insert`).
    fn close_batch(self: &Rc<Self>) {
        self.batch.set(self.batch.get().saturating_sub(1));
        self.persist_queue();
        self.changed();
        if self.batch.get() == 0 {
            self.start_next();
        }
    }

    /// Recount queued rows and refresh the UI. Deferred while a batch is open
    /// (like `persist_queue`): each step walks the whole store at GObject-ref
    /// cost per element, so a 500-entry import would pay it 500 times. Every
    /// batch opener ends with its own refresh, so nothing is lost by waiting.
    fn changed(&self) {
        if self.batch.get() > 0 {
            return;
        }
        // Collection children are passive progress rows. Count only their
        // persisted owner once so a 100-file playlist never inflates the
        // global queued count or the scheduler's concurrency model.
        self.queued.set(
            self.owners()
                .filter(|item| item.status() == DownloadStatus::Queued)
                .count(),
        );
        if let Some(refresh) = self.scheduler_refresh.borrow().as_ref() {
            refresh();
        }
        if let Some(cb) = self.on_change.borrow().as_ref() {
            cb();
        }
    }

    fn alloc_id(&self) -> u64 {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        id
    }

    /// In-memory allocator for callers that listed the destination off GTK.
    /// Callers must pass a complete listing; an unreadable directory must be
    /// reported rather than falling back to filesystem I/O on the main thread.
    fn alloc_id_for_in(&self, existing: &[String]) -> u64 {
        loop {
            let id = self.alloc_id();
            if !crate::video_staging::staging_occupied_in(existing, id) {
                return id;
            }
        }
    }

    /// Claim a stored id only if this session has not already assigned it.
    fn claim_persisted_id(&self, stored: Option<u64>) -> Option<u64> {
        let id = stored?;
        // A hand-edited/duplicated queue must never share one staging identity.
        if self.find(id).is_some() || self.epoch.borrow().contains_key(&id) {
            return None;
        }
        // Keep the allocator ahead of every adopted id so a new row never reuses its number.
        if id >= self.next_id.get() {
            self.next_id.set(id + 1);
        }
        Some(id)
    }

    /// Reuse a persisted id or allocate against a destination snapshot made off GTK.
    fn claim_id_in(&self, stored: Option<u64>, existing: &[String]) -> u64 {
        self.claim_persisted_id(stored)
            .unwrap_or_else(|| self.alloc_id_for_in(existing))
    }

    /// The underlying download list.
    pub fn store(&self) -> &gio::ListStore {
        &self.store
    }

    /// Find an item by id.
    pub fn find(&self, id: u64) -> Option<DownloadItem> {
        self.items().find(|it| it.id() == id)
    }

    /// Every row in the list store, in list order.
    fn items(&self) -> impl Iterator<Item = DownloadItem> + '_ {
        let store = &self.store;
        (0..store.n_items()).filter_map(|i| store.item(i).and_downcast::<DownloadItem>())
    }

    /// Queue owners are the schedulable and countable downloads. Collection
    /// children are passive projections and must not participate independently.
    fn owners(&self) -> impl Iterator<Item = DownloadItem> + '_ {
        self.items().filter(|item| !item.is_collection_child())
    }

    /// Remove every passive child row belonging to the supplied collection
    /// owners with one ListStore walk and reverse-order removals.
    fn remove_collection_child_rows(&self, owner_ids: &HashSet<u64>) {
        if owner_ids.is_empty() {
            return;
        }
        let positions = (0..self.store.n_items())
            .filter_map(|position| {
                self.store
                    .item(position)
                    .and_downcast::<DownloadItem>()
                    .filter(|item| {
                        item.is_collection_child()
                            && item
                                .group()
                                .is_some_and(|group| owner_ids.contains(&group.id()))
                    })
                    .map(|_| position)
            })
            .collect::<Vec<_>>();
        for position in positions.into_iter().rev() {
            self.store.remove(position);
        }
    }

    /// Whether a non-finished row already owns this normalized URL.
    pub(crate) fn has_nonterminal_url(&self, url: &str) -> bool {
        self.items()
            .any(|item| item.status() != DownloadStatus::Done && item.url() == url)
    }

    /// Position of the row with `id`, if it is still listed.
    fn position_of(&self, id: u64) -> Option<u32> {
        (0..self.store.n_items()).find(|&i| {
            self.store
                .item(i)
                .and_downcast::<DownloadItem>()
                .is_some_and(|it| it.id() == id)
        })
    }

    /// Preferences backing this queue (for dialog defaults).
    pub fn settings(&self) -> &crate::settings::AppSettings {
        &self.settings
    }

    /// Video-page source staged for a row, if any.
    pub fn video_source(&self, id: u64) -> Option<crate::media_types::VideoSource> {
        self.video_sources.borrow().get(&id).cloned()
    }

    /// The delivery gate for `id`, if this row has a video attempt.
    pub(crate) fn gate_for(&self, id: u64) -> Option<std::sync::Arc<AttemptGate>> {
        self.gates.borrow().get(&id).cloned()
    }

    /// Whether `dest` has a row removal still tearing down (exact path and stem-wide sweep key).
    pub(crate) fn dest_reserved(&self, dest: &std::path::Path) -> bool {
        self.reservations
            .lock()
            .map(|r| r.contains(dest))
            .unwrap_or(false)
    }

    fn reserve_dest(&self, dest: &std::path::Path) {
        if let Ok(mut r) = self.reservations.lock() {
            r.insert(dest);
        }
    }

    fn release_dest(&self, dest: &std::path::Path) {
        if let Ok(mut r) = self.reservations.lock() {
            r.remove(dest);
        }
    }

    /// Whether the row is currently capturing a live stream.
    pub fn is_live_video(&self, id: u64) -> bool {
        self.live_rows.borrow().contains(&id)
    }

    /// Whether this row uses the native manifest engine.
    pub(crate) fn is_manifest_download(&self, id: u64) -> bool {
        matches!(
            self.video_sources.borrow().get(&id),
            Some(crate::media_types::VideoSource::Manifest { .. })
        )
    }

    /// Whether an active live worker can still receive a stop intent.
    pub(crate) fn can_finish_live_recording(&self, id: u64) -> bool {
        self.is_live_video(id) && self.video_abort.borrow().contains_key(&id)
    }

    /// Whether a live capture can be paused without losing its committed resume state.
    pub(crate) fn can_pause_live_capture(&self, id: u64) -> bool {
        self.can_finish_live_recording(id) && self.is_manifest_download(id)
    }

    /// Request finalization of an active live recording without marking it cancelled.
    pub(crate) fn finish_live_recording(&self, id: u64) -> bool {
        if !self.can_finish_live_recording(id) {
            return false;
        }
        if !self.stop_video_worker(id, crate::video::StopIntent::Finish) {
            self.changed();
            return false;
        }
        if let Some(item) = self.find(id) {
            item.set_detail(gettext("Finalizing recording…"));
        }
        self.persist_queue();
        self.changed();
        true
    }

    /// Explicit absolute dest, else the effective download dir.
    pub(crate) fn resolve_dir(&self, dest_dir: Option<&str>) -> String {
        // Explicit destinations must be absolute (relative dirs would resolve against the launcher CWD and fail the sandbox).
        dest_dir
            .filter(|s| !s.is_empty() && std::path::Path::new(s).is_absolute())
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.effective_download_dir())
    }

    /// Prepare the validated magnet and torrent-session settings for metadata lookup.
    ///
    /// # Errors
    /// Returns a display-ready message when the magnet or torrent network
    /// configuration is invalid.
    pub(crate) fn prepare_magnet_metadata_lookup(
        &self,
        magnet: &str,
    ) -> Result<(String, crate::torrent::SessionConfig), String> {
        let magnet = normalize_url(magnet)?;
        let settings = &self.settings;
        let blocklist_url = crate::torrent::blocklist_url_of(&settings.torrent_blocklist_url())
            .map_err(|error| error.to_string())?;
        let proxy = DownloadOptions::from_settings(settings).proxy_config()?;
        let (listen_port, upnp) = crate::torrent::resolve_listen_config(
            settings.torrent_seed_finished(),
            settings.torrent_listen_port(),
            settings.torrent_upnp(),
        );
        let net = crate::torrent::plan_torrent_net(
            settings.torrent_dht(),
            settings.torrent_lsd(),
            listen_port,
            upnp,
            None,
            proxy.as_ref(),
        );
        Ok((
            magnet,
            crate::torrent::SessionConfig {
                dht: net.dht,
                peer_limit: None,
                download_bps: parse_rate(settings.speed_limit().trim()),
                upload_bps: parse_rate(settings.torrent_upload_limit().trim()),
                listen_port: net.listen_port,
                upnp: net.upnp,
                socks_proxy: net.socks_proxy,
                blocklist_url,
                lsd: net.lsd,
            },
        ))
    }

    /// Queue a download after listing its destination off GTK. The
    /// engine refuses a fresh row whose destination already exists, so
    /// duplicates surface as an error instead of scattering ` (1)` copies.
    ///
    /// # Errors
    /// Returns a display-ready message when the URL or filename is invalid.
    pub(crate) async fn enqueue_async_batched(
        self: &Rc<Self>,
        url: &str,
        dest_dir: Option<&str>,
        filename: Option<&str>,
    ) -> Result<(DownloadItem, BatchGuard), String> {
        let dir = self.resolve_dir(dest_dir);
        let listing_dir = dir.clone();
        let existing = gio::spawn_blocking(move || {
            crate::video_staging::try_dir_file_names(std::path::Path::new(&listing_dir))
        })
        .await
        .map_err(|error| format!("Could not inspect download folder: {error:?}"))?
        .map_err(|error| format!("Could not inspect download folder: {error}"))?;
        let guard = self.batch_guard();
        let item = self.enqueue_with_listing(url, Some(&dir), filename, &existing)?;
        Ok((item, guard))
    }

    pub(crate) async fn enqueue_async(
        self: &Rc<Self>,
        url: &str,
        dest_dir: Option<&str>,
        filename: Option<&str>,
    ) -> Result<DownloadItem, String> {
        let (item, guard) = self.enqueue_async_batched(url, dest_dir, filename).await?;
        drop(guard);
        Ok(item)
    }

    /// Synchronous intake for tests; production paths list the destination off
    /// GTK and call [`Self::enqueue_async_batched`].
    #[cfg(test)]
    pub fn enqueue(
        self: &Rc<Self>,
        url: &str,
        dest_dir: Option<&str>,
        filename: Option<&str>,
    ) -> Result<DownloadItem, String> {
        let dir = self.resolve_dir(dest_dir);
        let existing = crate::video_staging::dir_file_names(std::path::Path::new(&dir));
        self.enqueue_with_listing(url, Some(&dir), filename, &existing)
    }

    fn enqueue_with_listing(
        self: &Rc<Self>,
        url: &str,
        dest_dir: Option<&str>,
        filename: Option<&str>,
        existing: &[String],
    ) -> Result<DownloadItem, String> {
        let url = normalize_url(url)?;
        let dir = self.resolve_dir(dest_dir);
        let name = filename
            .filter(|s| sane_filename(s))
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                if crate::torrent::is_magnet(&url) {
                    // Real name arrives with metadata; the info-hash stub labels the row until SuggestName renames it.
                    crate::torrent::stub_name(&url).unwrap_or_else(|| filename_from_url(&url))
                } else {
                    filename_from_url(&url)
                }
            });
        let name = shorten_filename(&name);
        // Torrents record engine subfolders as output_dir, so the taken check covers those too.
        let name = dedupe_filename(&name, |n| {
            let p = std::path::Path::new(&dir).join(n);
            // Plain rows never reach `clean_dest_parts`, so only the exact destination needs a reservation.
            self.dest_reserved(&p)
                || self.items().any(|it| {
                    (it.dest_dir() == dir && it.filename() == n)
                        || it.output_dir() == p.to_string_lossy()
                })
        });
        let item = DownloadItem::new(self.alloc_id_for_in(existing), &url, &name, &dir);
        if crate::torrent::is_magnet(&url) {
            // Magnets carry no archive to recompute the output folder from later, so record their subfolder now.
            item.set_output_dir(
                std::path::Path::new(&dir)
                    .join(&name)
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        Ok(self.insert(item))
    }

    /// Intake for .torrent files: archive the bytes, then enqueue the
    /// pseudo-URL like any other download (stub from the file stem, real
    /// name arrives with metadata via SuggestName).
    ///
    /// # Errors
    /// Returns a display-ready message when the bytes are not a valid torrent.
    /// Archive and enqueue a .torrent without performing disk I/O on GTK.
    pub async fn enqueue_torrent_file_async(
        self: &Rc<Self>,
        bytes: Vec<u8>,
        file_name: &str,
        dest_dir: Option<&str>,
        only_files: Option<Vec<usize>>,
    ) -> Result<DownloadItem, String> {
        let (item, guard) = self
            .enqueue_torrent_file_async_batched(bytes, file_name, dest_dir, only_files)
            .await?;
        drop(guard);
        Ok(item)
    }

    pub(crate) async fn enqueue_torrent_file_async_batched(
        self: &Rc<Self>,
        bytes: Vec<u8>,
        file_name: &str,
        dest_dir: Option<&str>,
        only_files: Option<Vec<usize>>,
    ) -> Result<(DownloadItem, BatchGuard), String> {
        let file_name = file_name.to_string();
        let dir = self.resolve_dir(dest_dir);
        let worker_file_name = file_name.clone();
        let worker_dir = dir.clone();
        let (pseudo, base, existing) = gio::spawn_blocking(move || {
            // Validate the torrent and destination before archiving so a failed
            // intake does not leave an unqueued archive behind.
            let base = crate::torrent::intake_plan(&bytes).map(|(base, _multi)| base);
            let existing =
                crate::video_staging::try_dir_file_names(std::path::Path::new(&worker_dir))
                    .map_err(|error| format!("Could not inspect torrent destination: {error}"))?;
            let pseudo = crate::torrent::archive_torrent_file(&worker_file_name, &bytes)
                .map_err(|error| error.to_string())?;
            Ok::<_, String>((pseudo, base, existing))
        })
        .await
        .map_err(|error| format!("Could not prepare torrent file: {error:?}"))??;

        // Hold the queue only during the synchronous commit, not during disk I/O.
        let guard = self.batch_guard();
        if let Some(selection) = only_files {
            crate::torrent::stage_selection(&pseudo, selection);
        }
        let stub = crate::torrent::stub_name_for_file(&file_name);
        let item = self.enqueue_with_listing(&pseudo, Some(&dir), Some(&stub), &existing)?;

        // The engine writes with overwrite:true, so reserve a collision-free
        // output folder from the listing snapshot before it can be spawned.
        if let Some(base) = base {
            let candidate = std::path::Path::new(&dir).join(&base);
            let candidate_s = candidate.to_string_lossy().into_owned();
            let disk_taken = existing.iter().any(|entry| entry == &base);
            let row_taken = self.items().any(|row| row.output_dir() == candidate_s);
            if disk_taken || row_taken || self.dest_reserved(&candidate) {
                let name = dedupe_filename(&base, |name| {
                    let path = std::path::Path::new(&dir).join(name);
                    existing.iter().any(|entry| entry == name)
                        || self.dest_reserved(&path)
                        || self
                            .items()
                            .any(|row| row.output_dir() == path.to_string_lossy())
                });
                item.set_output_dir(
                    std::path::Path::new(&dir)
                        .join(&name)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        Ok((item, guard))
    }

    /// Final filename policy, shared by intake naming and late server/container suggestions: shorten, then the opt-in ASCII fold. Dedupe stays at the call sites.
    fn finalize_filename(&self, name: &str) -> String {
        let name = shorten_filename(name);
        if self.settings.restrict_filenames() {
            // Opt-in ASCII fold where Grab actually names the file (yt-dlp's flag is a no-op on literal output paths).
            restrict_filename_ascii(&name)
        } else {
            name
        }
    }

    /// Enqueue a video page: dialog-routed or probe-proven. The source is staged
    /// before insert, so the persist inside [`DownloadManager::insert`] already
    /// carries it and [`DownloadManager::start_next`] parks the row for the
    /// resolver worker instead of feeding the page to the HTTP engine.
    ///
    /// # Errors
    /// Returns a display-ready message when the URL or filename is invalid.
    /// Enqueue a video page after preparing its directory listing off GTK.
    #[cfg(test)]
    pub async fn enqueue_video_async(
        self: &Rc<Self>,
        page_url: &str,
        dest_dir: Option<&str>,
        filename: Option<&str>,
        choices: crate::media_types::VideoChoices,
    ) -> Result<DownloadItem, String> {
        let (item, guard) = self
            .enqueue_video_async_batched(page_url, dest_dir, filename, choices)
            .await?;
        drop(guard);
        Ok(item)
    }

    pub(crate) async fn enqueue_video_async_batched(
        self: &Rc<Self>,
        page_url: &str,
        dest_dir: Option<&str>,
        filename: Option<&str>,
        choices: crate::media_types::VideoChoices,
    ) -> Result<(DownloadItem, BatchGuard), String> {
        let dir = self.resolve_dir(dest_dir);
        let listing_dir = dir.clone();
        let existing = gio::spawn_blocking(move || {
            crate::video_staging::try_dir_file_names(std::path::Path::new(&listing_dir))
        })
        .await
        .map_err(|error| format!("Could not inspect download folder: {error:?}"))?
        .map_err(|error| format!("Could not inspect download folder: {error}"))?;
        let guard = self.batch_guard();
        let item = self.enqueue_video_staged(page_url, &dir, filename, choices, &existing)?;
        Ok((item, guard))
    }

    /// Preserve the editable stem, but derive the media suffix from the selected rendition.
    pub(crate) fn manifest_filename_for_container(
        filename: Option<&str>,
        source_url: &str,
        container: crate::media_types::ManifestContainer,
    ) -> String {
        let fallback_stem = crate::file_names::title_from_manifest_url(source_url);
        let stem = filename
            .filter(|name| sane_filename(name))
            .and_then(|name| {
                std::path::Path::new(name)
                    .file_stem()
                    .and_then(|stem| stem.to_str())
            })
            .filter(|stem| !stem.is_empty())
            .unwrap_or(&fallback_stem);
        format!("{stem}.{}", container.extension())
    }

    /// Queue a native HLS/DASH selection after listing its destination off GTK.
    pub(crate) async fn enqueue_manifest_async_batched(
        self: &Rc<Self>,
        source: crate::media_types::ManifestSource,
        dest_dir: Option<&str>,
        filename: Option<&str>,
    ) -> Result<(DownloadItem, BatchGuard), String> {
        let dir = self.resolve_dir(dest_dir);
        let listing_dir = dir.clone();
        let existing = gio::spawn_blocking(move || {
            crate::video_staging::try_dir_file_names(std::path::Path::new(&listing_dir))
        })
        .await
        .map_err(|error| format!("Could not inspect download folder: {error:?}"))?
        .map_err(|error| format!("Could not inspect download folder: {error}"))?;

        // Reuse format metadata already present in the Add-card preview. HLS
        // master child playlists are intentionally not fetched here; their
        // conservative Matroska default avoids a second, potentially one-shot
        // manifest request before the queue row appears.
        let container = source
            .output_container
            .unwrap_or(crate::media_types::ManifestContainer::Mkv);
        let requested_name =
            Self::manifest_filename_for_container(filename, &source.url, container);

        let guard = self.batch_guard();
        let item = self.enqueue_manifest_staged(source, &dir, Some(&requested_name), &existing)?;
        Ok((item, guard))
    }

    /// Enqueue a gallery download (image gallery via gallery-dl).
    /// Creates a DownloadItem with gallery flags set; the actual download
    /// is driven by the gallery runner (phase 12).
    /// Enqueue a gallery after listing its destination off GTK.
    #[cfg(test)]
    pub async fn enqueue_gallery_async(
        self: &Rc<Self>,
        job: &crate::gallery_argv::GalleryJob,
    ) -> Result<DownloadItem, String> {
        let (item, guard) = self.enqueue_gallery_async_batched(job).await?;
        drop(guard);
        Ok(item)
    }

    pub(crate) async fn enqueue_gallery_async_batched(
        self: &Rc<Self>,
        job: &crate::gallery_argv::GalleryJob,
    ) -> Result<(DownloadItem, BatchGuard), String> {
        let url = normalize_url(&job.page_url)?;
        // Resolve the destination before recording it: empty/relative paths must never
        // become dependent on the launcher CWD. The sanitized title is used as a
        // restore-safe folder component, not as an arbitrary path from metadata.
        let dir = self.resolve_dir(Some(&job.dest_dir.to_string_lossy()));
        let title = crate::file_names::sanitize_folder_name(&job.title);
        let listing_dir = dir.clone();
        let existing = gio::spawn_blocking(move || {
            crate::video_staging::try_dir_file_names(std::path::Path::new(&listing_dir))
        })
        .await
        .map_err(|error| format!("Could not inspect gallery destination: {error:?}"))?
        .map_err(|error| format!("Could not inspect gallery destination: {error}"))?;

        // Only hold the queue during the synchronous commit, never across listing I/O.
        let guard = self.batch_guard();
        // Only claim a folder that this row can prove is new. A pre-existing user
        // directory or another queued gallery must never become a trash target.
        let mut unique_title = title.clone();
        let mut counter = 1;
        while existing.iter().any(|entry| entry == &unique_title)
            || self
                .items()
                .any(|it| it.dest_dir() == dir && it.filename() == unique_title)
            || self.dest_reserved(&std::path::Path::new(&dir).join(&unique_title))
        {
            counter += 1;
            unique_title = format!("{title}-{counter}");
        }
        let item = DownloadItem::new(self.alloc_id_for_in(&existing), &url, &unique_title, &dir);
        item.set_is_gallery(true);
        item.set_group(
            crate::download_store::DownloadGroup::try_new(item.id(), &job.title).or_else(|| {
                crate::download_store::DownloadGroup::try_new(item.id(), &unique_title)
            }),
        );
        item.set_gallery_file_count(job.file_count.unwrap_or(0));
        item.set_gallery_files_done(0);
        item.set_gallery_url(url.clone());
        item.set_detail(gettext("Gallery"));
        let gallery_dir = std::path::Path::new(&dir).join(&unique_title);
        let mut stored_job = job.clone();
        stored_job.dest_dir = gallery_dir;
        stored_job.item_id = item.id();
        self.gallery_jobs.borrow_mut().insert(item.id(), stored_job);
        Ok((self.insert(item), guard))
    }

    /// [`DownloadManager::enqueue_video`] with a precomputed directory listing,
    /// for bulk imports that would otherwise readdir once per row. The snapshot
    /// stays correct as rows land: the per-row dedupe also consults the live
    /// store and the stem reservation map. An on-disk file never renames the
    /// intake (see [`DownloadManager::enqueue`]).
    pub(crate) fn enqueue_video_staged(
        self: &Rc<Self>,
        page_url: &str,
        dir: &str,
        filename: Option<&str>,
        choices: crate::media_types::VideoChoices,
        existing: &[String],
    ) -> Result<DownloadItem, String> {
        let url = normalize_url(page_url)?;
        // Treat (page URL, playlist entry id) as an identity: playlist children
        // share the collection URL, so URL alone would block every sibling.
        // This stops the same import from queuing an item that a non-terminal
        // row already owes, e.g. when an import runs twice.
        let incoming_pid = choices.playlist_item_id.clone();
        if self.items().any(|it| {
            it.status() != DownloadStatus::Done
                && it.url() == url
                && self
                    .video_sources
                    .borrow()
                    .get(&it.id())
                    .and_then(|src| match src {
                        crate::media_types::VideoSource::Page {
                            playlist_item_id, ..
                        } => playlist_item_id.clone(),
                        _ => None,
                    })
                    == incoming_pid
        }) {
            return Err(gettext("This item is already in the list"));
        }
        let name = self.finalize_filename(
            &filename
                .filter(|s| sane_filename(s))
                .map(|s| s.to_string())
                .unwrap_or_else(|| filename_from_url(&url)),
        );
        let name = dedupe_filename(&name, |n| {
            let p = std::path::Path::new(dir).join(n);
            crate::video_staging::stem_reserved_in(existing, name_stem(n))
                // Stem-wide reservation (see `is_name_taken`): a discard in flight owns this destination.
                || self.dest_reserved(&p)
                || self.items()
                    .any(|it| {
                        (it.dest_dir() == dir && it.filename() == n)
                            || it.output_dir() == p.to_string_lossy()
                    })
        });
        let item = DownloadItem::new(self.alloc_id_for_in(existing), &url, &name, dir);
        item.set_detail(pending_resolve_detail(choices.audio_only));
        self.video_sources.borrow_mut().insert(
            item.id(),
            crate::media_types::VideoSource::Page {
                page_url: url,
                media_url: None,
                expires_at: None,
                quality: choices.quality,
                audio_only: choices.audio_only,
                is_live: choices.is_live,
                video_format_id: choices.video_format_id,
                playlist_item_id: choices.playlist_item_id,
                backend: None,
            },
        );
        Ok(self.insert(item))
    }

    /// Return the existing owner for a playlist URL, preferring an unfinished
    /// collection over an older completed one.
    pub(crate) fn video_playlist_owner_for_url(&self, page_url: &str) -> Option<DownloadItem> {
        let url = normalize_url(page_url).ok()?;
        let mut completed = None;
        let mut cancelled = None;
        for item in self.items() {
            if item.url() != url
                || !matches!(
                    self.video_source(item.id()),
                    Some(crate::media_types::VideoSource::Playlist { .. })
                )
            {
                continue;
            }
            match item.status() {
                DownloadStatus::Done => completed = Some(item),
                DownloadStatus::Cancelled => cancelled = Some(item),
                _ => return Some(item),
            }
        }
        completed.or(cancelled)
    }

    /// Existing playlist output folder, only when its stored path is a direct
    /// child of the owner's destination directory.
    pub(crate) fn video_playlist_destination_for_url(&self, page_url: &str) -> Option<String> {
        let owner = self.video_playlist_owner_for_url(page_url)?;
        let Some(crate::media_types::VideoSource::Playlist { state, .. }) =
            self.video_source(owner.id())
        else {
            return None;
        };
        let output_dir = std::path::Path::new(&state.dest_dir);
        (output_dir.is_absolute()
            && output_dir.parent() == Some(std::path::Path::new(&owner.dest_dir()))
            && output_dir.file_name().is_some())
        .then_some(state.dest_dir)
    }

    /// Enqueue selected yt-dlp playlist entries as one schedulable owner.
    /// Re-imports for the same URL extend that owner instead of creating a
    /// second process; a running owner is restarted with the enlarged selection.
    pub(crate) fn enqueue_video_playlist_staged(
        self: &Rc<Self>,
        page_url: &str,
        title: &str,
        dest_dir: &str,
        files: Vec<crate::media_types::VideoPlaylistFile>,
        quality: String,
        audio_only: bool,
    ) -> Result<DownloadItem, String> {
        let url = normalize_url(page_url)?;
        let output_dir = std::path::Path::new(dest_dir);
        // The picker validates the directory on GIO's blocking pool. Keep this
        // GTK-thread commit path limited to path-shape checks.
        if !output_dir.is_absolute() || output_dir.file_name().is_none() {
            return Err(gettext("The playlist destination folder is invalid"));
        }
        let parent_dir = output_dir
            .parent()
            .ok_or_else(|| gettext("The playlist destination folder is invalid"))?;
        let filename = output_dir
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| gettext("The playlist destination folder is invalid"))?;
        if !sane_filename(filename) {
            return Err(gettext("The playlist destination folder is invalid"));
        }
        if files.is_empty() {
            return Err(gettext("Select at least one playlist entry"));
        }

        let mut seen_ids = HashSet::new();
        for file in &files {
            if file.id.trim().is_empty()
                || file.id.len() > 512
                || file.title.trim().is_empty()
                || !seen_ids.insert(file.id.clone())
            {
                return Err(gettext("The selected playlist entries are invalid"));
            }
            let _ = normalize_url(&file.page_url)?;
        }

        if let Some(owner) = self.video_playlist_owner_for_url(&url) {
            return self
                .extend_video_playlist_staged(&owner, dest_dir, files, &quality, audio_only);
        }

        let id = self.alloc_id();
        let title = if title.trim().is_empty() {
            filename
        } else {
            title
        };
        let state = crate::media_types::VideoPlaylistState {
            title: title.to_string(),
            dest_dir: dest_dir.to_string(),
            files,
        };
        let item = DownloadItem::new(id, &url, filename, &parent_dir.to_string_lossy());
        item.set_public_only(
            url::Url::parse(&url)
                .ok()
                .and_then(|parsed| parsed.host_str().map(str::to_string))
                .is_some_and(|host| {
                    self.items().any(|row| {
                        row.public_only()
                            && url::Url::parse(&row.url())
                                .ok()
                                .and_then(|parsed| parsed.host_str().map(str::to_string))
                                .is_some_and(|other| other == host)
                    }) || crate::download_net::is_untrusted_handoff_host(&host)
                }),
        );
        item.set_is_collection_owner(true);
        item.set_group(crate::download_store::DownloadGroup::try_new(id, title));
        item.set_gallery_url(url.clone());
        item.set_gallery_file_count(saturating_u32(state.files.len()));
        item.set_gallery_files_done(0);
        item.set_detail(pending_resolve_detail(audio_only));

        let batch = self.batch_guard();
        self.video_sources.borrow_mut().insert(
            id,
            crate::media_types::VideoSource::Playlist {
                page_url: url,
                quality,
                audio_only,
                state: state.clone(),
            },
        );
        let item = self.insert(item);
        self.restore_video_playlist_file_rows(&item, &state.files);
        drop(batch);
        Ok(item)
    }

    /// Extend one existing parent-owned playlist collection with new entry IDs.
    /// The caller has already validated and inspected the shared destination off GTK.
    fn extend_video_playlist_staged(
        self: &Rc<Self>,
        owner: &DownloadItem,
        dest_dir: &str,
        files: Vec<crate::media_types::VideoPlaylistFile>,
        quality: &str,
        audio_only: bool,
    ) -> Result<DownloadItem, String> {
        if owner.status() == DownloadStatus::Cancelled {
            return Err(gettext(
                "This playlist collection was cancelled. Remove it before adding more entries.",
            ));
        }

        let Some(crate::media_types::VideoSource::Playlist {
            quality: existing_quality,
            audio_only: existing_audio_only,
            state,
            ..
        }) = self.video_source(owner.id())
        else {
            return Err(gettext(
                "The existing playlist collection is no longer available",
            ));
        };
        if state.dest_dir != dest_dir {
            return Err(gettext(
                "This playlist is already queued in a different destination folder.",
            ));
        }
        if existing_quality != quality || existing_audio_only != audio_only {
            return Err(gettext(
                "This playlist is already queued with different download options.",
            ));
        }

        let existing_ids = state
            .files
            .iter()
            .map(|file| file.id.as_str())
            .collect::<HashSet<_>>();
        let additions = files
            .into_iter()
            .filter(|file| !existing_ids.contains(file.id.as_str()))
            .collect::<Vec<_>>();
        if additions.is_empty() {
            return Ok(owner.clone());
        }

        let previous_status = owner.status();
        let _batch = self.batch_guard();
        // Commit the validated source update before changing worker or owner
        // state. If the source disappeared, the active collection stays intact.
        let added_files = {
            let mut sources = self.video_sources.borrow_mut();
            let Some(crate::media_types::VideoSource::Playlist { state, .. }) =
                sources.get_mut(&owner.id())
            else {
                return Err(gettext(
                    "The existing playlist collection is no longer available",
                ));
            };
            state.files.extend(additions.iter().cloned());
            additions
        };
        // Rebuild once from the extended source while retaining row objects
        // already present in the GTK store.
        self.video_playlist_file_row_index
            .borrow_mut()
            .remove(&owner.id());

        if previous_status == DownloadStatus::Downloading {
            // The active yt-dlp process has already captured its playlist-index
            // list. Stop it and let the next attempt resolve the enlarged ID set.
            self.pause(owner.id());
        }
        let requeue = matches!(
            previous_status,
            DownloadStatus::Downloading | DownloadStatus::Done | DownloadStatus::Failed
        );
        if requeue {
            owner.set_status(DownloadStatus::Queued);
        }

        self.restore_video_playlist_file_rows(owner, &added_files);
        if requeue {
            // A new invocation owns the expanded ID set. Reset every unfinished
            // projection consistently; completed entries remain untouched.
            self.prepare_video_playlist_file_rows_for_attempt(owner);
        }
        self.persist_queue();
        self.changed();
        Ok(owner.clone())
    }

    /// Commit a manifest row after its destination snapshot has been prepared.
    fn enqueue_manifest_staged(
        self: &Rc<Self>,
        source: crate::media_types::ManifestSource,
        dir: &str,
        filename: Option<&str>,
        existing: &[String],
    ) -> Result<DownloadItem, String> {
        if !crate::manifest_intake::valid_source(&source) {
            return Err(gettext("The selected stream representation is invalid"));
        }
        let url = normalize_url(&source.url)?;
        let mut requested_name = filename
            .filter(|name| sane_filename(name))
            .map(str::to_string)
            .unwrap_or_else(|| {
                format!(
                    "{}.mkv",
                    crate::file_names::title_from_manifest_url(&source.url)
                )
            });
        match std::path::Path::new(&requested_name)
            .extension()
            .and_then(|extension| extension.to_str())
        {
            Some(extension)
                if extension.eq_ignore_ascii_case("mp4")
                    || extension.eq_ignore_ascii_case("mkv")
                    || extension.eq_ignore_ascii_case("ts") => {}
            Some(_) => {
                return Err(gettext(
                    "Native HLS/DASH downloads require an MP4, MKV, or TS filename",
                ));
            }
            None => requested_name.push_str(".mkv"),
        }
        let name = self.finalize_filename(&requested_name);
        let name = dedupe_filename(&name, |candidate| {
            let path = std::path::Path::new(dir).join(candidate);
            crate::video_staging::stem_reserved_in(existing, name_stem(candidate))
                || self.dest_reserved(&path)
                || self.items().any(|item| {
                    (item.dest_dir() == dir && item.filename() == candidate)
                        || item.output_dir() == path.to_string_lossy()
                })
        });
        let item = DownloadItem::new(self.alloc_id_for_in(existing), &url, &name, dir);
        item.set_detail(pending_resolve_detail(false));
        self.video_sources.borrow_mut().insert(
            item.id(),
            crate::media_types::VideoSource::Manifest { source },
        );
        Ok(self.insert(item))
    }

    /// Queue one row per playlist entry for collection-shaped resolves with no picked entry. Returns (added, reported total). One persist for the whole import.
    /// Queue collection children after creating/listing their destination off GTK.
    /// Convert a single-video carrier that resolved to a playlist into one
    /// parent-owned yt-dlp collection. Child entries are projections, not
    /// separately scheduled DownloadItems.
    async fn expand_playlist_rows_async(
        weak: std::rc::Weak<Self>,
        id: u64,
        generation: u64,
        item: &DownloadItem,
        pl: &crate::media_types::PlaylistInfo,
    ) -> (usize, usize, usize) {
        let Some(this) = weak.upgrade() else {
            return (0, pl.total, 0);
        };
        let (quality, audio_only) = match this.video_source(id) {
            Some(crate::media_types::VideoSource::Page {
                quality,
                audio_only,
                ..
            }) => (quality, audio_only),
            _ => return (0, pl.total, 0),
        };
        let base_dir = this.resolve_dir(Some(&item.dest_dir()));
        let title = pl.title.to_string();
        drop(this);

        // Collection folder creation and readdir can block on network mounts.
        let prepared = gio::spawn_blocking(move || {
            let dest_dir = crate::file_names::collection_subdir(&base_dir, &title);
            crate::video_staging::try_dir_file_names(std::path::Path::new(&dest_dir))
                .map(|_| dest_dir)
        })
        .await;
        let dest_dir = match prepared {
            Ok(Ok(dest_dir)) => dest_dir,
            Ok(Err(error)) => {
                tracing::warn!(
                    error = %error,
                    parent_id = id,
                    "playlist collection directory listing failed"
                );
                return (0, pl.total, 0);
            }
            Err(error) => {
                tracing::warn!(
                    error = ?error,
                    parent_id = id,
                    "playlist collection directory preparation failed"
                );
                return (0, pl.total, 0);
            }
        };

        let Some(this) = weak.upgrade() else {
            return (0, pl.total, 0);
        };
        if !this.is_current(id, generation) {
            return (0, pl.total, 0);
        }
        let Some(current) = this.find(id) else {
            return (0, pl.total, 0);
        };
        if matches!(
            current.status(),
            DownloadStatus::Cancelled | DownloadStatus::Paused
        ) {
            return (0, pl.total, 0);
        }

        let mut guarded_child_hosts = crate::download_net::PublicResolutionSet::default();
        let mut files = Vec::with_capacity(pl.items.len());
        let mut seen_ids = HashSet::new();
        let mut skipped_by_host_limit = 0usize;
        for entry in &pl.items {
            if entry.id.trim().is_empty() || !seen_ids.insert(entry.id.to_string()) {
                continue;
            }
            let Some(url) =
                crate::video_probe::expand_child_target(&item.url(), &pl.page_url, entry)
            else {
                continue;
            };
            match guarded_child_hosts.inherit(&item.url(), &url) {
                Ok(()) => {}
                Err(crate::download_net::CollectionHostError::TooManyDistinctHosts) => {
                    skipped_by_host_limit += 1;
                    continue;
                }
                Err(crate::download_net::CollectionHostError::PolicyCapReached) => {
                    tracing::warn!(
                        parent_id = id,
                        child_url = %url,
                        "skipping playlist item because the process public-host policy is full"
                    );
                    skipped_by_host_limit += 1;
                    continue;
                }
                Err(crate::download_net::CollectionHostError::UnsafeDestination) => continue,
            }
            files.push(crate::media_types::VideoPlaylistFile {
                id: entry.id.to_string(),
                title: entry.title.to_string(),
                page_url: url,
                status: DownloadStatus::Queued,
                progress: 0.0,
                path: None,
            });
        }

        if files.is_empty() {
            return (0, pl.total, skipped_by_host_limit);
        }

        let filename = std::path::Path::new(&dest_dir)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("playlist")
            .to_string();
        let state = crate::media_types::VideoPlaylistState {
            title: pl.title.to_string(),
            dest_dir: dest_dir.clone(),
            files,
        };
        let group = crate::download_store::DownloadGroup::try_new(id, &state.title);
        current.set_filename(filename);
        current.set_is_collection_owner(true);
        current.set_group(group);
        current.set_gallery_url(item.url());
        current.set_gallery_file_count(saturating_u32(state.files.len()));
        current.set_gallery_files_done(0);
        current.set_progress(0.0);
        current.set_detail(gettext("Playlist • waiting for a download slot"));
        current.set_status(DownloadStatus::Queued);
        this.video_sources.borrow_mut().insert(
            id,
            crate::media_types::VideoSource::Playlist {
                page_url: current.url().to_string(),
                quality,
                audio_only,
                state: state.clone(),
            },
        );
        this.restore_video_playlist_file_rows(&current, &state.files);
        this.persist_queue();
        this.changed();
        (state.files.len(), pl.total, skipped_by_host_limit)
    }

    /// Synchronous restore helper for tests; production passes a prelisted destination.
    ///
    /// # Errors
    /// Returns a display-ready message when the stored entry is invalid.
    #[cfg(test)]
    pub fn restore_existing(self: &Rc<Self>, stored: &StoredItem) -> Result<DownloadItem, String> {
        let existing = crate::video_staging::dir_file_names(std::path::Path::new(&stored.dest_dir));
        self.restore_existing_with_listing(stored, &existing)
    }

    fn restore_existing_with_listing(
        self: &Rc<Self>,
        stored: &StoredItem,
        existing: &[String],
    ) -> Result<DownloadItem, String> {
        let StoredItem {
            id: stored_id,
            url: raw_url,
            dest_dir,
            filename: raw_filename,
            status,
            segments,
            video_source,
            ..
        } = stored;
        let url = normalize_url(raw_url)?;
        // Legacy queues predate origin metadata. Preserve their established
        // behavior; new rows persist an explicit value for browser handoffs.
        let public_only = stored.public_only.unwrap_or(false);
        if public_only {
            recheck_public_host(&url, "download")?;
        }
        if !sane_filename(raw_filename) {
            return Err(format!("Invalid filename in queue: {raw_filename}"));
        }
        if !std::path::Path::new(dest_dir).is_absolute() {
            return Err(format!("Invalid destination in queue: {dest_dir}"));
        }
        let filename = shorten_filename(raw_filename);
        let item = DownloadItem::new(
            self.claim_id_in(*stored_id, existing),
            &url,
            &filename,
            dest_dir,
        );
        item.set_public_only(public_only);
        item.set_group(stored.group.clone());
        // Resumed rows requeue; only settled rows keep their status.
        // Scheduled rows keep their status and timestamp; overdue ones queue immediately.
        let now = glib::DateTime::now_local()
            .map(|dt| dt.to_unix())
            .unwrap_or(0);
        let scheduled_at = stored.scheduled_at.unwrap_or(0);
        item.set_scheduled_at(scheduled_at);
        // Resume validators for If-Range (empty = none / predates the field).
        if let Some(e) = stored.etag.as_deref() {
            item.set_etag(e);
        }
        if let Some(lm) = stored.last_modified.as_deref() {
            item.set_last_modified(lm);
        }
        item.set_status(match status {
            DownloadStatus::Paused | DownloadStatus::Failed | DownloadStatus::Done => *status,
            DownloadStatus::Scheduled if scheduled_at > now => DownloadStatus::Scheduled,
            _ => DownloadStatus::Queued,
        });
        if let Some(st) = segments {
            self.segment_state
                .borrow_mut()
                .insert(item.id(), st.clone());
        }
        let mut playlist_file_rows = None;
        if let Some(src) = video_source {
            let source_matches = video_source_matches_url(src, &url);
            if source_matches {
                let audio_only = matches!(
                    src,
                    crate::media_types::VideoSource::Page {
                        audio_only: true,
                        ..
                    } | crate::media_types::VideoSource::Playlist {
                        audio_only: true,
                        ..
                    }
                );
                if let crate::media_types::VideoSource::Playlist { state, .. } = src {
                    item.set_is_collection_owner(true);
                    item.set_group(
                        crate::download_store::DownloadGroup::try_new(item.id(), &state.title)
                            .or_else(|| stored.group.clone()),
                    );
                    item.set_gallery_url(url.clone());
                    item.set_gallery_file_count(saturating_u32(state.files.len()));
                    item.set_gallery_files_done(saturating_u32(
                        state
                            .files
                            .iter()
                            .filter(|file| file.status == DownloadStatus::Done)
                            .count(),
                    ));
                    playlist_file_rows = Some(state.files.clone());
                }
                item.set_detail(pending_resolve_detail(audio_only));
                self.video_sources
                    .borrow_mut()
                    .insert(item.id(), src.clone());
            } else {
                tracing::warn!("dropping invalid or mismatched video source");
            }
        }
        // A row the engine started in a past session resumes its own
        // partial; only a never-started row refuses an existing dest.
        // Legacy queue files (started == None) keep the old resume behavior.
        if stored.started.unwrap_or(true) {
            self.started.borrow_mut().insert(item.id());
        }
        let batch = self.batch_guard();
        let item = self.insert(item);
        if let Some(files) = playlist_file_rows.as_deref() {
            self.restore_video_playlist_file_rows(&item, files);
        }
        drop(batch);
        Ok(item)
    }

    fn restore_existing_gallery_with_listing(
        self: &Rc<Self>,
        stored: &StoredItem,
        job: Option<crate::gallery_argv::GalleryJob>,
        existing: &[String],
    ) -> Result<DownloadItem, String> {
        let mut job = job.ok_or_else(|| "gallery entry missing job".to_string())?;
        let url = normalize_url(&stored.url)?;
        // Legacy queues predate origin metadata; only new rows can prove
        // they came from a browser handoff.
        let public_only = stored.public_only.unwrap_or(false);
        if public_only {
            recheck_public_host(&url, "gallery")?;
        }
        if !std::path::Path::new(&stored.dest_dir).is_absolute() {
            return Err(format!("Invalid destination in queue: {}", stored.dest_dir));
        }
        // The row's folder name (with its -2 suffix) was sane-validated on
        // load; use it verbatim so the restored row matches the on-disk
        // subfolder instead of the payload's base title.
        let title = stored.filename.clone();
        let id = self.claim_id_in(stored.id, existing);
        job.item_id = id;
        let item = DownloadItem::new(id, &url, &title, &stored.dest_dir);
        item.set_public_only(public_only);
        item.set_is_gallery(true);
        item.set_group(
            crate::download_store::DownloadGroup::try_new(id, &job.title)
                .or_else(|| crate::download_store::DownloadGroup::try_new(id, &title)),
        );
        let file_count = job.file_count.unwrap_or(0);
        item.set_gallery_file_count(file_count);
        // Restore the completion counter used by the gallery progress bar.
        let files_done = if job.file_rows.is_empty() && stored.status == DownloadStatus::Done {
            file_count
        } else {
            job.file_rows
                .iter()
                .filter(|file| file.status == DownloadStatus::Done)
                .count() as u32
        };
        item.set_gallery_files_done(files_done);
        item.set_gallery_url(url.clone());
        item.set_detail(gettext("Gallery"));
        // Status: Done/Failed/Paused kept; Downloading → Queued via the _ arm.
        // (Scheduled doesn't apply to galleries.)
        item.set_status(match stored.status {
            DownloadStatus::Paused | DownloadStatus::Failed | DownloadStatus::Done => stored.status,
            _ => DownloadStatus::Queued,
        });
        // Done restores with a full bar; everything else starts at 0.
        item.set_progress(if stored.status == DownloadStatus::Done {
            1.0
        } else {
            0.0
        });
        // Insert the job BEFORE self.insert: insert → start_next →
        // spawn_gallery reads gallery_jobs synchronously.
        let file_rows = job.file_rows.clone();
        self.gallery_jobs.borrow_mut().insert(id, job);
        if stored.started.unwrap_or(true) {
            self.started.borrow_mut().insert(id);
        }
        let item = self.insert(item);
        self.restore_gallery_file_rows(&item, &file_rows);
        Ok(item)
    }

    /// Claim one reference on a host's public-only policy for this row.
    fn claim_public_policy(item: &DownloadItem) {
        if let Some(host) = url::Url::parse(&item.url())
            .ok()
            .and_then(|url| url.host_str().map(str::to_string))
            && (item.public_only() || crate::download_net::is_untrusted_handoff_host(&host))
        {
            item.set_public_only(true);
            crate::download_net::retain_public_resolution(&host);
        }
    }

    /// Drop this row's policy reference. Any remaining card, transfer, or
    /// queue-row lease keeps the host registered until its owner is done.
    fn release_public_policy(item: &DownloadItem) {
        if item.public_only()
            && let Some(host) = url::Url::parse(&item.url())
                .ok()
                .and_then(|url| url.host_str().map(str::to_string))
        {
            crate::download_net::release_public_resolution(&host);
        }
    }

    fn insert(self: &Rc<Self>, item: DownloadItem) -> DownloadItem {
        Self::claim_public_policy(&item);
        self.store.append(&item);
        self.persist_queue();
        self.changed();
        // Inside a batch the close kicks the scheduler once; a queue scan per insert is pure overhead.
        if self.batch.get() == 0 {
            self.start_next();
        }
        item
    }

    /// "Finished • {size}" detail with the size measured off the GTK thread.
    /// `measure_disk_usage` is a recursive scan that can stall the main loop on
    /// large folders, so the label paints immediately with `size` (the engine's
    /// figure, 0 for plain "Finished") and is refined when the measurement lands.
    /// A row that is gone by then simply never gets the update.
    fn set_finished_detail(&self, item: &DownloadItem, size: u64) {
        item.set_detail(if size > 0 {
            gettext("Finished • {size}").replace("{size}", &fmt_bytes(size))
        } else {
            gettext("Finished")
        });
        let weak = item.downgrade();
        let path = item.file_path();
        let sizing_guard = SizingGuard::new(Rc::clone(&self.sizing_inflight));
        glib::spawn_future_local(async move {
            let _sizing_guard = sizing_guard;
            let measured = crate::file_names::path_size_async(path)
                .await
                .filter(|m| *m > 0);
            let (Some(item), Some(measured)) = (weak.upgrade(), measured) else {
                return;
            };
            item.set_detail(gettext("Finished • {size}").replace("{size}", &fmt_bytes(measured)));
        });
    }

    fn insert_history(
        self: &Rc<Self>,
        stored: StoredItem,
        output_dir: Option<String>,
        existing: &[String],
    ) {
        let StoredItem {
            id: stored_id,
            url: raw_url,
            dest_dir: dir,
            filename: name,
            progress,
            video_source,
            group,
            ..
        } = stored;
        let Ok(url) = normalize_url(&raw_url) else {
            tracing::warn!("skipping history entry with bad URL");
            return;
        };
        if !sane_filename(&name) {
            tracing::warn!("skipping invalid history entry for {name}");
            return;
        }
        if !std::path::Path::new(&dir).is_absolute() {
            tracing::warn!("skipping history entry with relative destination");
            return;
        }
        let item = DownloadItem::new(self.claim_id_in(stored_id, existing), &url, &name, &dir);
        item.set_group(group);
        item.set_progress(progress.clamp(0.0, 1.0));
        item.set_status(DownloadStatus::Done);
        // Already trust-checked by the caller (must sit inside `dir`).
        if let Some(folder) = output_dir {
            item.set_output_dir(folder);
        }
        // Size off the final path, measured off-thread: the engine measured the
        // pre-rename one, and folders sum contents. Paints "Finished" now,
        // refines with the measured size when it lands.
        self.set_finished_detail(&item, 0);
        // Keep the row's media identity: a restored video page must not fall
        // back to the generic file icon (same page-URL guard as
        // `restore_existing`).
        if let Some(src) = video_source {
            let source_matches = video_source_matches_url(&src, &url);
            if source_matches {
                self.video_sources.borrow_mut().insert(item.id(), src);
            }
        }
        self.insert(item);
    }

    /// Whether finish/fail desktop notifications are enabled.
    pub fn notifications_enabled(&self) -> bool {
        self.settings.show_notifications()
    }

    /// Whether closing over active downloads notifies.
    pub fn background_notifications_enabled(&self) -> bool {
        self.settings.notify_background()
    }

    /// Configured folder, or the system Downloads folder when empty or
    /// relative (a relative dir would resolve against the launcher CWD).
    pub fn effective_download_dir(&self) -> String {
        let configured = self.settings.download_dir();
        if !configured.is_empty() && std::path::Path::new(&configured).is_absolute() {
            return configured;
        }
        glib::user_special_dir(glib::UserDirectory::Downloads)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/tmp".to_string())
    }

    /// Simultaneous-download limit (at least 1).
    pub fn max_concurrent(&self) -> usize {
        self.settings.max_concurrent().max(1) as usize
    }
}

impl DownloadManager {
    /// Populate the per-owner lookup cache once from the persisted job state.
    fn ensure_gallery_file_row_index(&self, owner_id: u64) -> bool {
        if self.gallery_file_row_index.borrow().contains_key(&owner_id) {
            return true;
        }
        let Some(state_indices) = self.gallery_jobs.borrow().get(&owner_id).map(|job| {
            job.file_rows
                .iter()
                .enumerate()
                .map(|(index, state)| (state.path.clone(), index))
                .collect::<HashMap<_, _>>()
        }) else {
            return false;
        };

        self.gallery_file_row_index
            .borrow_mut()
            .entry(owner_id)
            .or_insert_with(|| CollectionProjectionIndex {
                state_indices,
                rows: HashMap::new(),
                summary: None,
            });
        true
    }

    /// Create or refresh a transient read-only child row for one file in a
    /// gallery collection. The path must remain inside gallery-dl's assigned
    /// output directory; extractor-controlled paths are never accepted as
    /// arbitrary queue destinations.
    fn upsert_gallery_file_row(
        &self,
        owner: &DownloadItem,
        path: &str,
        status: DownloadStatus,
        progress: f64,
        detail: &str,
    ) {
        let Some(output_dir) = self
            .gallery_jobs
            .borrow()
            .get(&owner.id())
            .map(|job| job.dest_dir.clone())
        else {
            return;
        };
        let output_path = std::path::Path::new(path);
        let Some(relative) = output_path
            .strip_prefix(&output_dir)
            .ok()
            .filter(|relative| {
                !relative.as_os_str().is_empty()
                    && relative
                        .components()
                        .all(|component| matches!(component, std::path::Component::Normal(_)))
            })
        else {
            tracing::warn!(
                path = %path,
                output_dir = %output_dir.display(),
                "ignoring gallery-dl file event outside its assigned destination"
            );
            return;
        };
        let filename = relative.to_string_lossy().into_owned();
        let Some(group) = owner.group() else {
            tracing::warn!(
                gallery_id = owner.id(),
                "gallery file event has no collection identity"
            );
            return;
        };
        let progress = progress.clamp(0.0, 1.0);

        // Build the index once after restore, then update state and row by
        // output path without walking either the persisted Vec or GTK store
        // for every gallery-dl percentage line.
        let owner_id = owner.id();
        if !self.ensure_gallery_file_row_index(owner_id) {
            return;
        }
        let existing_state_index = self
            .gallery_file_row_index
            .borrow()
            .get(&owner_id)
            .and_then(|index| index.state_indices.get(path))
            .copied();

        // Keep the child projection in the single persisted GalleryJob. Its
        // row itself is transient and is omitted from StoredQueue.
        let state_index = {
            let mut jobs = self.gallery_jobs.borrow_mut();
            let Some(job) = jobs.get_mut(&owner_id) else {
                return;
            };
            let index = existing_state_index.unwrap_or_else(|| {
                let index = job.file_rows.len();
                job.file_rows.push(crate::gallery_argv::GalleryFileState {
                    path: path.to_string(),
                    status,
                    progress,
                });
                index
            });
            let Some(state) = job.file_rows.get_mut(index) else {
                tracing::warn!(
                    gallery_id = owner_id,
                    path,
                    state_index = index,
                    "gallery file state index is stale; ignoring the update"
                );
                return;
            };
            state.status = status;
            state.progress = progress;
            index
        };
        if existing_state_index.is_none()
            && let Some(index) = self.gallery_file_row_index.borrow_mut().get_mut(&owner_id)
        {
            index.state_indices.insert(path.to_string(), state_index);
        }

        if let Some(existing) = self
            .gallery_file_row_index
            .borrow()
            .get(&owner_id)
            .and_then(|index| index.rows.get(path))
            .cloned()
        {
            existing.set_progress(progress);
            existing.set_detail(detail.to_string());
            existing.set_status(status);
            return;
        }

        let child = DownloadItem::new(
            self.alloc_id(),
            &owner.url(),
            &filename,
            &output_dir.to_string_lossy(),
        );
        child.set_is_collection_child(true);
        child.set_gallery_file_path(path.to_string());
        child.set_group(Some(group));
        child.set_progress(progress);
        child.set_detail(detail.to_string());
        child.set_status(status);
        // Child rows describe the one gallery-dl process; they are not
        // independently scheduled or persisted as separate download jobs.
        self.store.append(&child);
        if let Some(index) = self.gallery_file_row_index.borrow_mut().get_mut(&owner_id) {
            index.rows.insert(path.to_string(), child);
        }
    }

    /// Recreate child projections from the GalleryJob payload after queue
    /// restore or Undo. A previously active file is queued/paused with its
    /// owner and will receive a fresh start event if gallery-dl resumes it.
    fn restore_gallery_file_rows(
        &self,
        owner: &DownloadItem,
        file_rows: &[crate::gallery_argv::GalleryFileState],
    ) {
        for state in file_rows {
            let status = restored_collection_child_status(
                CollectionProjectionKind::Gallery,
                owner.status(),
                state.status,
            );
            let detail = collection_child_status_detail(CollectionProjectionKind::Gallery, status);
            self.upsert_gallery_file_row(owner, &state.path, status, state.progress, &detail);
        }
        if !file_rows.is_empty() {
            self.changed();
        }
    }

    /// Re-queue transient child rows when a paused/failed gallery owner restarts.
    fn prepare_gallery_file_rows_for_attempt(&self, owner_id: u64) {
        let children = self
            .items()
            .filter(|item| {
                item.is_collection_child()
                    && item.group().is_some_and(|group| group.id() == owner_id)
                    && matches!(
                        item.status(),
                        DownloadStatus::Paused | DownloadStatus::Failed | DownloadStatus::Cancelled
                    )
            })
            .collect::<Vec<_>>();
        for child in children {
            child.set_status(DownloadStatus::Queued);
            child.set_progress(0.0);
            child.set_detail(gettext("Queued"));
            let path = child.gallery_file_path();
            if let Some(state) = self
                .gallery_jobs
                .borrow_mut()
                .get_mut(&owner_id)
                .and_then(|job| job.file_rows.iter_mut().find(|state| state.path == path))
            {
                state.status = DownloadStatus::Queued;
                state.progress = 0.0;
            }
        }
        if self.items().any(|item| {
            item.is_collection_child() && item.group().is_some_and(|group| group.id() == owner_id)
        }) {
            self.changed();
        }
    }

    /// Resolve files left active when the shared gallery-dl process pauses,
    /// cancels, succeeds, or fails.
    fn finish_gallery_file_rows(&self, owner_id: u64, status: DownloadStatus) {
        let children = self
            .items()
            .filter(|item| {
                item.is_collection_child()
                    && item.group().is_some_and(|group| group.id() == owner_id)
                    && item.status() == DownloadStatus::Downloading
            })
            .collect::<Vec<_>>();
        let mut updated_paths = Vec::new();
        for item in children {
            if status == DownloadStatus::Done {
                item.set_progress(1.0);
                item.set_detail(gettext("Finished"));
            } else {
                let detail =
                    collection_child_status_detail(CollectionProjectionKind::Gallery, status);
                item.set_detail(detail);
            }
            item.set_status(status);
            updated_paths.push((item.gallery_file_path(), item.progress()));
        }
        if let Some(job) = self.gallery_jobs.borrow_mut().get_mut(&owner_id) {
            for (path, progress) in updated_paths {
                if let Some(state) = job.file_rows.iter_mut().find(|state| state.path == path) {
                    state.status = status;
                    state.progress = progress;
                }
            }
        }
        if self.items().any(|item| {
            item.is_collection_child() && item.group().is_some_and(|group| group.id() == owner_id)
        }) {
            self.changed();
        }
    }

    /// Build the playlist-entry and child-row indexes once per owner.
    fn ensure_video_playlist_file_row_index(&self, owner_id: u64) -> bool {
        if self
            .video_playlist_file_row_index
            .borrow()
            .contains_key(&owner_id)
        {
            return true;
        }
        let Some((state_indices, total, done, progress_sum)) = self
            .video_sources
            .borrow()
            .get(&owner_id)
            .and_then(|source| match source {
                crate::media_types::VideoSource::Playlist { state, .. } => {
                    let mut indices = HashMap::with_capacity(state.files.len());
                    let mut done = 0usize;
                    let mut progress_sum = 0.0;
                    for (index, file) in state.files.iter().enumerate() {
                        indices.insert(file.id.clone(), index);
                        done += usize::from(file.status == DownloadStatus::Done);
                        progress_sum += file.progress.clamp(0.0, 1.0);
                    }
                    Some((indices, state.files.len(), done, progress_sum))
                }
                _ => None,
            })
        else {
            return false;
        };

        // One store scan per index build; all later progress events use the
        // row map instead of repeatedly downcasting every GObject in the list.
        let rows = self
            .items()
            .filter(|item| {
                item.is_collection_child()
                    && item.group().is_some_and(|group| group.id() == owner_id)
            })
            .filter_map(|item| {
                let file_id = item.collection_file_id();
                (!file_id.is_empty()).then_some((file_id, item))
            })
            .collect();

        self.video_playlist_file_row_index
            .borrow_mut()
            .entry(owner_id)
            .or_insert_with(|| CollectionProjectionIndex {
                state_indices,
                rows,
                summary: Some(CollectionProjectionSummary {
                    total,
                    done,
                    progress_sum,
                }),
            });
        true
    }

    fn update_video_playlist_index_summary(
        &self,
        owner_id: u64,
        old_status: DownloadStatus,
        old_progress: f64,
        new_status: DownloadStatus,
        new_progress: f64,
    ) {
        if let Some(summary) = self
            .video_playlist_file_row_index
            .borrow_mut()
            .get_mut(&owner_id)
            .and_then(|index| index.summary.as_mut())
        {
            summary.update(old_status, old_progress, new_status, new_progress);
        }
    }

    /// Update one persisted entry and its passive row using O(1) id lookups.
    fn upsert_video_playlist_file_row(
        &self,
        owner: &DownloadItem,
        file_id: &str,
        status: DownloadStatus,
        progress: f64,
        detail: &str,
        path: Option<&str>,
    ) {
        let owner_id = owner.id();
        if !self.ensure_video_playlist_file_row_index(owner_id) {
            return;
        }
        let Some(state_index) = self
            .video_playlist_file_row_index
            .borrow()
            .get(&owner_id)
            .and_then(|index| index.state_indices.get(file_id))
            .copied()
        else {
            tracing::warn!(
                owner_id,
                file_id,
                "ignoring yt-dlp playlist event for an unknown entry"
            );
            return;
        };

        let (
            page_url,
            file_title,
            dest_dir,
            title,
            audio_only,
            stored_path,
            old_status,
            old_progress,
            new_progress,
        ) = {
            let mut sources = self.video_sources.borrow_mut();
            let Some(crate::media_types::VideoSource::Playlist {
                audio_only, state, ..
            }) = sources.get_mut(&owner_id)
            else {
                return;
            };
            let Some(file) = state
                .files
                .get_mut(state_index)
                .filter(|file| file.id == file_id)
            else {
                tracing::warn!(
                    owner_id,
                    file_id,
                    state_index,
                    "playlist state index is stale; ignoring the update"
                );
                return;
            };
            let old_status = file.status;
            let old_progress = file.progress;
            file.status = status;
            file.progress = progress.clamp(0.0, 1.0);
            if let Some(path) = path.filter(|path| !path.is_empty()) {
                file.path = Some(path.to_string());
            }
            (
                file.page_url.clone(),
                file.title.clone(),
                state.dest_dir.clone(),
                state.title.clone(),
                *audio_only,
                file.path.clone(),
                old_status,
                old_progress,
                file.progress,
            )
        };
        self.update_video_playlist_index_summary(
            owner_id,
            old_status,
            old_progress,
            status,
            new_progress,
        );

        let group = owner
            .group()
            .or_else(|| crate::download_store::DownloadGroup::try_new(owner_id, &title));
        let filename = stored_path
            .as_deref()
            .and_then(|path| std::path::Path::new(path).file_name())
            .and_then(|name| name.to_str())
            .map(str::to_string)
            .unwrap_or_else(|| {
                let remux = crate::video_prefs::remux_video_active(&self.settings.remux_video());
                crate::video::default_video_filename(&file_title, audio_only, remux.as_deref())
            });

        let existing = self
            .video_playlist_file_row_index
            .borrow()
            .get(&owner_id)
            .and_then(|index| index.rows.get(file_id))
            .cloned();
        if let Some(existing) = existing {
            existing.set_filename(filename);
            existing.set_gallery_file_path(stored_path.unwrap_or_default());
            existing.set_progress(new_progress);
            existing.set_detail(detail.to_string());
            existing.set_status(status);
        } else {
            let child = DownloadItem::new(self.alloc_id(), &page_url, &filename, &dest_dir);
            child.set_is_collection_child(true);
            child.set_collection_file_id(file_id);
            child.set_gallery_file_path(stored_path.unwrap_or_default());
            child.set_group(group);
            child.set_progress(new_progress);
            child.set_detail(detail.to_string());
            child.set_status(status);
            self.store.append(&child);
            if let Some(index) = self
                .video_playlist_file_row_index
                .borrow_mut()
                .get_mut(&owner_id)
            {
                index.rows.insert(file_id.to_string(), child);
            }
        }
        self.refresh_video_playlist_summary(owner);
    }

    /// Rebuild passive child projections from persisted owner state after restore or Undo.
    fn restore_video_playlist_file_rows(
        &self,
        owner: &DownloadItem,
        files: &[crate::media_types::VideoPlaylistFile],
    ) {
        for file in files {
            let status = restored_collection_child_status(
                CollectionProjectionKind::Playlist,
                owner.status(),
                file.status,
            );
            let detail = collection_child_status_detail(CollectionProjectionKind::Playlist, status);
            self.upsert_video_playlist_file_row(
                owner,
                &file.id,
                status,
                file.progress,
                &detail,
                file.path.as_deref(),
            );
        }
        self.refresh_video_playlist_summary(owner);
    }

    /// Reset unfinished files for a collection retry; completed entries stay Done.
    fn prepare_video_playlist_file_rows_for_attempt(&self, owner: &DownloadItem) {
        let owner_id = owner.id();
        if !self.ensure_video_playlist_file_row_index(owner_id) {
            return;
        }
        let updates = {
            let mut sources = self.video_sources.borrow_mut();
            let Some(crate::media_types::VideoSource::Playlist { state, .. }) =
                sources.get_mut(&owner_id)
            else {
                return;
            };
            state
                .files
                .iter_mut()
                .filter(|file| file.status != DownloadStatus::Done)
                .map(|file| {
                    let old_status = file.status;
                    let old_progress = file.progress;
                    file.status = DownloadStatus::Queued;
                    file.progress = 0.0;
                    (file.id.clone(), old_status, old_progress)
                })
                .collect::<Vec<_>>()
        };
        for (file_id, old_status, old_progress) in updates {
            self.update_video_playlist_index_summary(
                owner_id,
                old_status,
                old_progress,
                DownloadStatus::Queued,
                0.0,
            );
            if let Some(child) = self
                .video_playlist_file_row_index
                .borrow()
                .get(&owner_id)
                .and_then(|index| index.rows.get(&file_id))
                .cloned()
            {
                child.set_progress(0.0);
                child.set_detail(gettext("Waiting in collection"));
                child.set_status(DownloadStatus::Queued);
            }
        }
        self.refresh_video_playlist_summary(owner);
    }

    /// Settle active playlist children when the one owning process pauses, fails, or is cancelled.
    fn finish_video_playlist_file_rows(&self, owner_id: u64, status: DownloadStatus) {
        if !self.ensure_video_playlist_file_row_index(owner_id) {
            return;
        }
        let updates = {
            let mut sources = self.video_sources.borrow_mut();
            let Some(crate::media_types::VideoSource::Playlist { state, .. }) =
                sources.get_mut(&owner_id)
            else {
                return;
            };
            state
                .files
                .iter_mut()
                .filter(|file| file.status == DownloadStatus::Downloading)
                .map(|file| {
                    let old_status = file.status;
                    let old_progress = file.progress;
                    file.status = status;
                    if status == DownloadStatus::Done {
                        file.progress = 1.0;
                    }
                    (file.id.clone(), old_status, old_progress, file.progress)
                })
                .collect::<Vec<_>>()
        };
        for (file_id, old_status, old_progress, new_progress) in updates {
            self.update_video_playlist_index_summary(
                owner_id,
                old_status,
                old_progress,
                status,
                new_progress,
            );
            if let Some(child) = self
                .video_playlist_file_row_index
                .borrow()
                .get(&owner_id)
                .and_then(|index| index.rows.get(&file_id))
                .cloned()
            {
                if status == DownloadStatus::Done {
                    child.set_progress(1.0);
                    child.set_detail(gettext("Finished"));
                } else {
                    child.set_detail(collection_child_status_detail(
                        CollectionProjectionKind::Playlist,
                        status,
                    ));
                }
                child.set_status(status);
            }
        }
        if let Some(owner) = self.find(owner_id) {
            self.refresh_video_playlist_summary(&owner);
        }
    }

    fn refresh_video_playlist_summary(&self, owner: &DownloadItem) {
        let owner_id = owner.id();
        if !self.ensure_video_playlist_file_row_index(owner_id) {
            return;
        }
        let Some(summary) = self
            .video_playlist_file_row_index
            .borrow()
            .get(&owner_id)
            .and_then(|index| index.summary)
        else {
            return;
        };
        owner.set_gallery_file_count(saturating_u32(summary.total));
        owner.set_gallery_files_done(saturating_u32(summary.done));
        owner.set_progress(summary.fraction());
        owner.set_detail(
            gettext("Playlist • {done}/{total} files")
                .replace("{done}", &summary.done.to_string())
                .replace("{total}", &summary.total.to_string()),
        );
    }

    /// Pause a running download, freeing its slot for the next queued item.
    pub fn pause(self: &Rc<Self>, id: u64) {
        if self.can_pause_live_capture(id) {
            // Native live workers commit each HLS/DASH update before observing
            // Pause. Keep the row paused while the pump releases its slot.
            if !self.stop_video_worker(id, crate::video::StopIntent::Pause) {
                self.changed();
                return;
            }
            if let Some(item) = self.find(id)
                && item.status() == DownloadStatus::Downloading
            {
                item.set_status(DownloadStatus::Paused);
                item.set_detail(gettext("Paused"));
            }
            self.persist_queue();
            self.changed();
            return;
        }
        // Non-live downloads retain their existing preserve-partials behavior.
        if !self.stop_engine(id, crate::video::StopIntent::Finish) {
            return;
        }
        if let Some(item) = self.find(id) {
            if crate::torrent::is_torrent(&item.url()) {
                crate::torrent::pause_download(id);
            }
            if item.status() == DownloadStatus::Downloading {
                if item.is_gallery() {
                    self.finish_gallery_file_rows(id, DownloadStatus::Paused);
                }
                if item.is_collection_owner() {
                    self.finish_video_playlist_file_rows(id, DownloadStatus::Paused);
                }
                item.set_status(DownloadStatus::Paused);
                item.set_detail(
                    gettext("Paused • {pct}%")
                        .replace("{pct}", &((item.progress() * 100.0) as u64).to_string()),
                );
            }
        }
        // Persist the bitmap too: a kill while paused must resume segmented.
        self.persist_queue();
        self.changed();
        self.start_next();
    }

    /// Re-queue a paused download.
    pub fn resume(self: &Rc<Self>, id: u64) {
        if let Some(item) = self.find(id)
            && item.status() == DownloadStatus::Paused
        {
            item.set_status(DownloadStatus::Queued);
            self.persist_queue();
            self.changed();
            self.start_next();
        }
    }

    /// Rename a download: the list label plus the file on disk when already fetched. Only completed and queued rows qualify: mid-transfer the engine owns the path, and torrent rows map to session outputs.
    ///
    /// # Errors
    /// Returns a display-ready message when the row cannot be renamed.
    pub fn rename_download(self: &Rc<Self>, id: u64, new_name: &str) -> Result<(), String> {
        let item = self.find(id).ok_or_else(|| gettext("Download not found"))?;
        if crate::torrent::is_torrent(&item.url()) {
            return Err(gettext("Renaming torrent downloads isn't supported"));
        }
        match item.status() {
            DownloadStatus::Done | DownloadStatus::Queued | DownloadStatus::Scheduled => {}
            DownloadStatus::Downloading => {
                return Err(gettext("Pause or wait for the download to finish first"));
            }
            _ => {
                return Err(gettext(
                    "Only completed or not-yet-started downloads can be renamed",
                ));
            }
        }
        // Cap at filesystem limits before the equality check: a >240-byte
        // name that shortens to the current filename must be a no-op, not
        // a rename_noreplace(old, old) -> EEXIST.
        let name = shorten_filename(new_name.trim());
        if name == item.filename() {
            return Ok(());
        }
        if name.is_empty() || !sane_filename(&name) {
            return Err(gettext("That isn't a valid file name"));
        }
        if item.status() == DownloadStatus::Done {
            let new_path = std::path::PathBuf::from(item.dest_dir().to_string()).join(&name);
            match rename_noreplace(&item.file_path(), &new_path) {
                Ok(()) => {}
                // Deleted behind our back: the label update below still applies.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(gettext("Could not rename {name}: {error}")
                        .replace("{name}", item.filename().as_str())
                        .replace("{error}", &e.to_string()));
                }
            }
        }
        let dest_dir = item.dest_dir();
        let renamed_dir = std::path::Path::new(&dest_dir).join(&name);
        item.set_filename(name);
        // Gallery rows: the stored job's dest_dir is the on-disk subfolder.
        // Keep it in sync with the rename so Delete's ownership check and a
        // later retry target the renamed folder, not the stale name.
        if item.is_gallery()
            && let Some(job) = self.gallery_jobs.borrow_mut().get_mut(&id)
        {
            job.dest_dir = renamed_dir;
        }
        self.persist_queue();
        self.changed();
        Ok(())
    }

    pub fn segments_of(&self, id: u64) -> Option<SegmentState> {
        self.segment_state.borrow().get(&id).cloned()
    }

    /// Whether the engine ever started this row (for Undo snapshots, which
    /// are built outside this module).
    pub(crate) fn has_started(&self, id: u64) -> bool {
        self.started.borrow().contains(&id)
    }

    /// Gallery job payload used by collection Undo.
    pub(crate) fn gallery_job(&self, id: u64) -> Option<crate::gallery_argv::GalleryJob> {
        self.gallery_jobs.borrow().get(&id).cloned()
    }

    /// Restore a group of removed rows after listing each distinct destination
    /// directory off GTK. One batch keeps persistence, UI refresh and scheduling
    /// to one update, while failures are collected for a single user-facing toast.
    pub(crate) async fn unremove_many_async(
        self: &Rc<Self>,
        snapshots: Vec<RemovedSnapshot>,
    ) -> Result<usize, String> {
        let total = snapshots.len();
        if total == 0 {
            return Ok(0);
        }

        let directories = snapshots
            .iter()
            .map(|snapshot| snapshot.dest_dir.clone())
            .collect::<HashSet<_>>();
        let listings = gio::spawn_blocking(move || {
            directories
                .into_iter()
                .map(|directory| {
                    let result =
                        crate::video_staging::try_dir_file_names(std::path::Path::new(&directory))
                            .map_err(|error| error.to_string());
                    (directory, result)
                })
                .collect::<HashMap<_, _>>()
        })
        .await
        .map_err(|error| {
            let message = format!("{error:?}");
            format_gettext_template(
                &gettext("Could not inspect Undo destinations: {error}"),
                &[("error", &message)],
            )
        })?;

        let _batch = self.batch_guard();
        let mut restored = 0;
        let mut failures = Vec::new();
        for snapshot in snapshots {
            match listings.get(&snapshot.dest_dir) {
                Some(Ok(existing)) => {
                    self.unremove_with_listing(snapshot, existing);
                    restored += 1;
                }
                Some(Err(error)) => failures.push((snapshot.filename, error.clone())),
                None => failures.push((
                    snapshot.filename,
                    gettext("destination listing was not available"),
                )),
            }
        }

        if failures.is_empty() {
            Ok(restored)
        } else {
            let (filename, error) = &failures[0];
            let restored_text = restored.to_string();
            let total_text = total.to_string();
            let failed_text = failures.len().to_string();
            Err(format_gettext_template(
                &gettext(
                    "Restored {restored} of {total} downloads; {failed} could not be restored. First failure for {filename}: {error}",
                ),
                &[
                    ("restored", &restored_text),
                    ("total", &total_text),
                    ("failed", &failed_text),
                    ("filename", filename),
                    ("error", error),
                ],
            ))
        }
    }

    /// Re-insert one previously removed download (Undo) after listing its
    /// destination off GTK. Collection actions use the batched entry point.
    pub(crate) async fn unremove_async(
        self: &Rc<Self>,
        snap: RemovedSnapshot,
    ) -> Result<DownloadItem, String> {
        let dir = snap.dest_dir.clone();
        let existing = gio::spawn_blocking(move || {
            crate::video_staging::try_dir_file_names(std::path::Path::new(&dir))
        })
        .await
        .map_err(|error| {
            let message = format!("{error:?}");
            format_gettext_template(
                &gettext("Could not inspect Undo destination: {error}"),
                &[("error", &message)],
            )
        })?
        .map_err(|error| {
            let message = error.to_string();
            format_gettext_template(
                &gettext("Could not inspect Undo destination: {error}"),
                &[("error", &message)],
            )
        })?;
        Ok(self.unremove_with_listing(snap, &existing))
    }

    /// Synchronous Undo for tests; production single-row callers use `unremove_async`.
    #[cfg(test)]
    pub fn unremove(self: &Rc<Self>, snap: RemovedSnapshot) -> DownloadItem {
        let existing = crate::video_staging::dir_file_names(std::path::Path::new(&snap.dest_dir));
        self.unremove_with_listing(snap, &existing)
    }

    /// Re-insert a previously removed download (Undo). Restored `Downloading`
    /// restarts as `Queued`; a restored bitmap resumes, a stale one is dropped
    /// at spawn.
    fn unremove_with_listing(
        self: &Rc<Self>,
        snap: RemovedSnapshot,
        existing: &[String],
    ) -> DownloadItem {
        let id = self.alloc_id_for_in(existing);
        let item = DownloadItem::new(id, &snap.url, &snap.filename, &snap.dest_dir);
        item.set_progress(snap.progress.clamp(0.0, 1.0));
        item.set_detail(snap.detail);
        item.set_status(match snap.status {
            DownloadStatus::Downloading => DownloadStatus::Queued,
            s => s,
        });
        item.set_output_dir(snap.output_dir);
        item.set_public_only(snap.public_only);
        let mut gallery_file_rows = None;
        let mut playlist_file_rows = None;
        if let Some(mut job) = snap.gallery_job {
            job.item_id = id;
            item.set_is_gallery(true);
            item.set_gallery_file_count(job.file_count.unwrap_or(0));
            item.set_gallery_files_done(
                job.file_rows
                    .iter()
                    .filter(|file| file.status == DownloadStatus::Done)
                    .count() as u32,
            );
            item.set_gallery_url(job.page_url.clone());
            item.set_group(
                crate::download_store::DownloadGroup::try_new(id, &job.title)
                    .or_else(|| crate::download_store::DownloadGroup::try_new(id, &snap.filename)),
            );
            gallery_file_rows = Some(job.file_rows.clone());
            self.gallery_jobs.borrow_mut().insert(id, job);
        } else {
            item.set_group(snap.group);
        }
        if let Some(st) = snap.segments {
            self.segment_state.borrow_mut().insert(item.id(), st);
        }
        // An Undo-restored row keeps its started mark under its new id: the
        // partial on disk is its own bytes, so the spawn gate resumes it
        // instead of failing it at DEST_EXISTS as a foreign file.
        if snap.started {
            self.started.borrow_mut().insert(item.id());
        }
        // Re-stage before insert (see `enqueue_video`): the persist carries it and `start_next` dispatches on it.
        if let Some(src) = snap.video_source {
            if let crate::media_types::VideoSource::Playlist { state, .. } = &src {
                item.set_is_collection_owner(true);
                item.set_group(crate::download_store::DownloadGroup::try_new(
                    id,
                    &state.title,
                ));
                item.set_gallery_url(item.url());
                item.set_gallery_file_count(saturating_u32(state.files.len()));
                item.set_gallery_files_done(saturating_u32(
                    state
                        .files
                        .iter()
                        .filter(|file| file.status == DownloadStatus::Done)
                        .count(),
                ));
                playlist_file_rows = Some(state.files.clone());
            }
            self.video_sources.borrow_mut().insert(item.id(), src);
        }
        // Undo bypasses intake dedupe, so consult the reservation: a discard still
        // tearing down owns this destination, and the finalizer's `wake_tx` wakeup
        // starts the row once the destination is free.
        if item.status() == DownloadStatus::Queued && self.dest_reserved(&item.file_path()) {
            item.set_status(DownloadStatus::Queued);
            Self::claim_public_policy(&item);
            self.store.append(&item);
            self.persist_queue();
            self.changed();
            if let Some(file_rows) = gallery_file_rows.as_deref() {
                self.restore_gallery_file_rows(&item, file_rows);
            }
            if let Some(files) = playlist_file_rows.as_deref() {
                self.restore_video_playlist_file_rows(&item, files);
            }
            return item;
        }
        let item = self.insert(item);
        if let Some(file_rows) = gallery_file_rows.as_deref() {
            self.restore_gallery_file_rows(&item, file_rows);
        }
        if let Some(files) = playlist_file_rows.as_deref() {
            self.restore_video_playlist_file_rows(&item, files);
        }
        item
    }

    /// Engine's real output folder for a torrent row: recorded at enqueue, else recomputed from the archive, else the row's own path.
    /// The recomputed leaf goes through the same guard as the write path; the
    /// recorded branch refuses a path that is itself a symlink. Either way, a
    /// symlink planted after the download finished can't divert the
    /// delete/trash onto its target.
    #[cfg(test)]
    fn torrent_folder(item: &DownloadItem) -> std::path::PathBuf {
        let dest_dir = item.dest_dir().to_string();
        let url = item.url().to_string();
        let output_dir = item.output_dir().to_string();
        let filename = item.filename().to_string();
        Self::torrent_folder_from_parts(&dest_dir, &url, &output_dir, &filename)
    }

    /// Resolve a torrent output folder from plain path metadata so the same
    /// guard can run on a blocking worker without moving a GTK object.
    fn torrent_folder_from_parts(
        dest_dir: &str,
        url: &str,
        recorded: &str,
        filename: &str,
    ) -> std::path::PathBuf {
        if !recorded.is_empty() {
            let path = std::path::PathBuf::from(recorded);
            // Refuse a recorded folder that is itself a symlink: swapped in
            // after the finish, cleanup could otherwise run through it.
            let is_link = std::fs::symlink_metadata(&path)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false);
            if !is_link {
                return path;
            }
        }
        let dest = std::path::PathBuf::from(dest_dir);
        crate::torrent::torrent_output_dir(&dest, url)
            .map(|folder| crate::torrent::guard_output_folder(&dest, folder))
            .unwrap_or_else(|| dest.join(filename))
    }

    /// Per-piece completion for the block map: segmented bitmap, torrent haves, or a prefix fill from byte progress. Empty when nothing is known.
    pub fn piece_bitmap(&self, id: u64) -> Vec<bool> {
        if let Some(st) = self.segment_state.borrow().get(&id) {
            return st.done.clone();
        }
        if let Some(have) = self.torrent_pieces.borrow().get(&id) {
            return have.clone();
        }
        let Some(item) = self.find(id) else {
            return Vec::new();
        };
        if crate::torrent::is_torrent(&item.url()) || item.progress() <= 0.0 {
            return Vec::new();
        }
        let filled = (item.progress().clamp(0.0, 1.0) * BLOCK_CELLS as f64) as usize;
        (0..BLOCK_CELLS).map(|i| i < filled).collect()
    }

    /// The folder Delete may trash for a gallery row.
    ///
    /// Returns `Some` only when the stored job proves Grab owns the folder:
    /// the recorded subfolder must be a direct child of the row's `dest_dir`,
    /// named exactly the row's filename. Anything else — legacy rows whose
    /// job points at the parent itself, a missing or mismatched record —
    /// yields `None` and Delete removes just the row.
    /// Drop every finished row (files stay on disk). Still-seeding torrents leave the session first. Returns the cleared count.
    pub fn clear_finished(self: &Rc<Self>) -> usize {
        let before = self.store.n_items();
        let visible_finished = self.finished_count();
        let finished_owners = self
            .owners()
            .filter(|item| item.status() == DownloadStatus::Done)
            .map(|item| item.id())
            .collect::<Vec<_>>();
        self.drop_finished_rows(&finished_owners);
        let removed_rows = (before - self.store.n_items()) as usize;
        if removed_rows > 0 {
            // Same hygiene as a restart: drop archives and selections no remaining row references.
            let referenced: std::collections::HashSet<String> = self
                .items()
                .map(|it| it.url().to_string())
                .filter(|u| crate::torrent::is_torrent(u))
                .collect();
            crate::torrent::sweep_archives(&referenced);
            crate::torrent::prune_selections(&referenced);
            self.persist_queue();
            self.changed();
        }
        visible_finished
    }

    /// Finished rows (for the clear-finished confirm and toast counts).
    pub fn finished_count(&self) -> usize {
        let items = self.items().collect::<Vec<_>>();
        let done_owner_ids = items
            .iter()
            .filter(|item| !item.is_collection_child() && item.status() == DownloadStatus::Done)
            .map(DownloadItem::id)
            .collect::<HashSet<_>>();
        let owners_with_children = items
            .iter()
            .filter(|item| item.is_collection_child())
            .filter_map(|item| item.group().map(|group| group.id()))
            .collect::<HashSet<_>>();

        let finished_owners = items
            .iter()
            .filter(|item| !item.is_collection_child() && item.status() == DownloadStatus::Done)
            .filter(|item| {
                !item.is_collection_parent() || !owners_with_children.contains(&item.id())
            })
            .count();
        let finished_children = items
            .iter()
            .filter(|item| item.is_collection_child() && item.status() == DownloadStatus::Done)
            .filter(|item| {
                item.group()
                    .is_some_and(|group| done_owner_ids.contains(&group.id()))
            })
            .count();

        finished_owners + finished_children
    }

    /// Snapshots of every finished row, in list order, for Clear Finished's Undo.
    pub fn finished_snapshots(&self) -> Vec<RemovedSnapshot> {
        self.owners()
            .filter(|it| it.status() == DownloadStatus::Done)
            .map(|item| RemovedSnapshot::capture(self, &item))
            .collect()
    }

    /// Drop finished owners and all their passive child rows with one
    /// ListStore walk. Destination files remain untouched.
    fn drop_finished_rows(&self, owner_ids: &[u64]) {
        let owner_ids = owner_ids.iter().copied().collect::<HashSet<_>>();
        if owner_ids.is_empty() {
            return;
        }

        let owner_items = self
            .owners()
            .filter(|item| owner_ids.contains(&item.id()))
            .map(|item| (item.id(), item))
            .collect::<HashMap<_, _>>();
        for item in owner_items.values() {
            if crate::torrent::is_torrent(&item.url()) {
                crate::torrent::forget_download(item.id());
            }
        }

        let released = owner_items
            .values()
            .filter(|item| item.public_only())
            .cloned()
            .collect::<Vec<_>>();
        let collection_owner_ids = owner_items
            .values()
            .filter(|item| item.is_collection_parent())
            .map(DownloadItem::id)
            .collect::<HashSet<_>>();

        for id in &owner_ids {
            self.video_sources.borrow_mut().remove(id);
            self.gallery_jobs.borrow_mut().remove(id);
            self.gallery_file_row_index.borrow_mut().remove(id);
            self.video_playlist_file_row_index.borrow_mut().remove(id);
            self.epoch.borrow_mut().remove(id);
        }

        let positions = (0..self.store.n_items())
            .filter_map(|position| {
                self.store
                    .item(position)
                    .and_downcast::<DownloadItem>()
                    .filter(|item| {
                        owner_ids.contains(&item.id())
                            || (item.is_collection_child()
                                && item.group().is_some_and(|group| {
                                    collection_owner_ids.contains(&group.id())
                                }))
                    })
                    .map(|_| position)
            })
            .collect::<Vec<_>>();
        for position in positions.into_iter().rev() {
            self.store.remove(position);
        }

        for item in released {
            Self::release_public_policy(&item);
        }
    }

    fn for_matching(
        self: &Rc<Self>,
        matches: impl Fn(DownloadStatus) -> bool,
        mut op: impl FnMut(&Rc<Self>, u64),
    ) {
        let ids: Vec<u64> = self
            .owners()
            .filter(|it| matches(it.status()))
            .map(|it| it.id())
            .collect();
        for id in ids {
            op(self, id);
        }
    }

    /// Whether `gen` is still the row's latest engine spawn.
    fn is_current(&self, id: u64, generation: u64) -> bool {
        self.epoch.borrow().get(&id).cloned().unwrap_or(0) == generation
    }

    /// Rows waiting queued (cached; see `queued`).
    pub fn queued_count(&self) -> usize {
        self.queued.get()
    }

    /// Whether any item is queued, downloading or paused.
    pub fn has_active(&self) -> bool {
        self.active_count() > 0
    }

    /// Rows `cancel_all` would touch (queued, downloading, paused).
    pub fn active_count(&self) -> usize {
        self.owners()
            .filter(|it| {
                matches!(
                    it.status(),
                    DownloadStatus::Queued | DownloadStatus::Downloading | DownloadStatus::Paused
                )
            })
            .count()
    }

    /// Whether anything is actually transferring (queued or downloading). Paused items don't count.
    pub fn has_transferring(&self) -> bool {
        self.any_status(|s| matches!(s, DownloadStatus::Queued | DownloadStatus::Downloading))
    }

    /// Whether any item failed or was cancelled.
    pub fn has_failed(&self) -> bool {
        self.any_status(|s| matches!(s, DownloadStatus::Failed | DownloadStatus::Cancelled))
    }

    /// Whether any item genuinely failed. User-cancelled rows need no error banner (Retry Failed still resurrects them via `has_failed`).
    pub fn has_errored(&self) -> bool {
        self.any_status(|s| matches!(s, DownloadStatus::Failed))
    }

    fn any_status(&self, pred: impl Fn(DownloadStatus) -> bool) -> bool {
        self.owners().any(|it| pred(it.status()))
    }

    /// Join bound for [`Self::shutdown`]: aborted engine tasks die at their next
    /// await and discard finalizers are local file sweeps, so a healthy quit
    /// joins in milliseconds — the bound only ever bites when something is
    /// already wedged, and waiting longer fixes nothing. On timeout the queue
    /// is still persisted and partial files are reconciled at the next launch,
    /// so the only thing a longer bound buys is a longer frozen quit.
    const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_millis(500);

    /// Abort running tasks and persist the queue for the next launch. The
    /// GTK close/Quit path uses begin_async_shutdown and finish_async_shutdown.
    pub fn shutdown(&self) {
        if self.shutdown_finished.get() || self.shutdown_started.replace(true) {
            return;
        }
        self.shutdown_with_timeout(Self::SHUTDOWN_JOIN_TIMEOUT);
        self.shutdown_finished.set(true);
    }

    /// Reserve asynchronous shutdown synchronously at the close/Quit event.
    /// This gates queued work and removes scheduler wakeups before the caller
    /// yields to the GLib context.
    pub(crate) fn begin_async_shutdown(&self) -> bool {
        if self.shutdown_finished.get() || self.shutdown_started.replace(true) {
            return false;
        }
        self.stop_scheduler();
        true
    }

    /// Finish a shutdown previously reserved by begin_async_shutdown.
    pub(crate) async fn finish_async_shutdown(self: &Rc<Self>) {
        if self.shutdown_finished.get() {
            return;
        }
        // Never wait indefinitely for startup restore during Quit. If it is
        // still pending, cancel its main-context task and preserve the saved
        // queue file below rather than snapshotting a partially applied model.
        // The loader's blocking worker may finish later, but the cancelled
        // future cannot apply its result after shutdown has been claimed.
        let restore_task = self.restore_task.borrow_mut().take();
        if self.restore_state.get() == RestoreState::Pending
            && let Some(task) = restore_task
        {
            tracing::warn!(
                "shutdown: cancelling in-progress queue restore; preserving saved queue"
            );
            task.abort();
        }
        self.draining.set(true);
        let handles: Vec<_> = self.running.borrow_mut().drain().map(|(_, h)| h).collect();
        let finals: Vec<PendingDiscard> =
            self.discards.borrow_mut().drain().map(|(_, p)| p).collect();
        let gates: Vec<_> = self.gates.borrow().values().cloned().collect();
        for handle in &handles {
            handle.abort();
        }
        for pending in &finals {
            if let Some(worker_abort) = &pending.worker_abort {
                worker_abort.abort();
            }
        }

        // Engine/finalizer joins and GTK-local pump joins share one deadline.
        // A pump may be waiting for a blocking filename rename, so it cannot
        // extend Quit beyond the same bound.
        let shutdown_deadline = Instant::now() + Self::SHUTDOWN_JOIN_TIMEOUT;
        let worker_join_timeout = shutdown_deadline.saturating_duration_since(Instant::now());
        let (joined_tx, joined_rx) = tokio::sync::oneshot::channel();
        tokio_rt().spawn(async move {
            let joined = tokio::time::timeout(worker_join_timeout, async {
                for handle in handles {
                    let _ = handle.await;
                }
                for pending in finals {
                    let _ = pending.finalizer.await;
                }
                for gate in gates {
                    gate.wait_for_rollbacks().await;
                }
            })
            .await
            .is_ok();
            let _ = joined_tx.send(joined);
        });
        let joined = joined_rx.await.unwrap_or(false);
        // A timed-out pump leaves its final filename unknown. Do not snapshot
        // the in-memory queue or truncate partials unless every pump has joined.
        let pumps: Vec<_> = self
            .pump_handles
            .borrow_mut()
            .drain()
            .map(|(_, handle)| handle)
            .collect();
        let pumps_joined = if pumps.is_empty() {
            true
        } else {
            wait_for_local_future(
                async move {
                    for pump in pumps {
                        let _ = pump.await;
                    }
                },
                shutdown_deadline.saturating_duration_since(Instant::now()),
            )
            .await
        };
        let persist_snapshot = joined && pumps_joined;
        if !persist_snapshot {
            tracing::warn!(
                "shutdown: async/pump join deadline elapsed; preserving the saved queue and skipping partial-file reconciliation"
            );
        } else {
            // Truncating a partially written segmented file is filesystem I/O;
            // keep it off GTK's main context just like the task joins above.
            let ids: Vec<u64> = self.segment_state.borrow().keys().cloned().collect();
            let truncations: Vec<_> = ids
                .iter()
                .filter_map(|id| {
                    let item = self.find(*id)?;
                    let state = self.segment_state.borrow().get(id).cloned()?;
                    Some((item.file_path(), state))
                })
                .collect();
            match tokio_rt()
                .spawn_blocking(move || {
                    for (path, state) in truncations {
                        truncate_to_prefix(&path, &state);
                    }
                })
                .await
            {
                Ok(()) => {
                    for id in ids {
                        if let Some(st) = self.segment_state.borrow_mut().get_mut(&id) {
                            st.forget_beyond_prefix();
                        }
                    }
                }
                Err(e) => {
                    // Keep the original bitmap if we could not reconcile the
                    // on-disk prefix; it is safer than persisting a false one.
                    tracing::warn!("shutdown: partial-file reconciliation worker failed: {e}");
                }
            }
        }
        self.persist_for_shutdown_async(persist_snapshot).await;
        self.shutdown_finished.set(true);
    }

    async fn persist_for_shutdown_async(&self, persist_snapshot: bool) {
        #[cfg(test)]
        if persist_snapshot {
            self.persist_for_shutdown();
        }
        #[cfg(not(test))]
        {
            if persist_snapshot && self.restore_state.get() == RestoreState::Ready {
                if let (Some(queue), Some(tx)) =
                    (self.snapshot_queue_inner(true), &*self.persist_tx.borrow())
                {
                    tx.send_replace(Some(queue));
                }
            } else if !persist_snapshot {
                tracing::warn!(
                    "shutdown: preserving the saved queue because an engine or GTK pump did not quiesce"
                );
            } else {
                tracing::warn!(
                    "shutdown: queue restore did not complete; preserving the saved queue file"
                );
            }
            drop(self.persist_tx.borrow_mut().take());
            let persist_task = { self.persist_task.borrow_mut().take() };
            if let Some(task) = persist_task {
                let (done_tx, done_rx) = tokio::sync::oneshot::channel();
                tokio_rt().spawn(async move {
                    let result =
                        tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
                    let _ = done_tx.send(matches!(result, Ok(Ok(()))));
                });
                match done_rx.await {
                    Ok(true) => {}
                    Ok(false) => tracing::warn!(
                        "shutdown: queue persistence failed or exceeded its deadline"
                    ),
                    Err(_) => {
                        tracing::warn!("shutdown: queue persistence task ended without a result")
                    }
                }
            }
        }
    }

    pub(crate) fn shutdown_with_timeout(&self, _timeout: Duration) {
        self.stop_scheduler();
        self.draining.set(true);
        let handles: Vec<_> = self.running.borrow_mut().drain().map(|(_, h)| h).collect();
        let finals: Vec<PendingDiscard> =
            self.discards.borrow_mut().drain().map(|(_, p)| p).collect();
        for handle in &handles {
            handle.abort();
        }
        for pending in &finals {
            if let Some(worker_abort) = &pending.worker_abort {
                worker_abort.abort();
            }
        }
        // This synchronous fallback is only for shutdown paths that bypass the
        // normal GLib async close flow. Do not block GTK joining workers or
        // truncating files. Preserve the bitmap as-is; the next launch can
        // reconcile stale partial lengths without risking a false bitmap.
        self.persist_for_shutdown();
    }
}

#[cfg(test)]
#[path = "download_tests.rs"]
mod tests;
