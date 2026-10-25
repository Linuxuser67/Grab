use crate::engine_msg::{DEST_EXISTS, EngineMsg};
use crate::file_names::{
    dedupe_filename, filename_from_url, fmt_bytes, name_stem, piece_len, rename_noreplace,
    restrict_filename_ascii, sane_filename, shorten_filename,
};
use crate::runtime::tokio_rt;
use gettextrs::{gettext, ngettext};
use gtk4::gio::prelude::*;
use gtk4::{gio, glib};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

/// Counter for unique queue temp-file names. Combined with the PID, this makes
/// temp paths unique across writers in the same process.
static QUEUE_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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
use crate::download_pieces::SegmentState;
/// Facade: segmented-piece math lives in [`download_pieces`](crate::download_pieces)
/// now (no re-exports: the engine consumes it here, tests import it directly).
use crate::download_pieces::{BLOCK_CELLS, MAX_SEGMENTED_TOTAL};
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

/// A removal whose worker is still tearing down.
struct PendingDiscard {
    /// Aborts the *worker*: aborting the finalizer would drop this handle and detach the task instead of stopping it.
    worker_abort: tokio::task::AbortHandle,
    finalizer: tokio::task::JoinHandle<()>,
}

#[derive(Debug)]
enum DeleteOperation {
    Gallery {
        target: Option<std::path::PathBuf>,
        filename: String,
    },
    Torrent {
        url: String,
        dest_dir: std::path::PathBuf,
        output_dir: String,
        filename: String,
    },
    File {
        path: std::path::PathBuf,
        sidecars: Vec<std::path::PathBuf>,
        filename: String,
    },
}

/// Trash one path. Missing files are harmless: a completed row may have been
/// deleted externally since it was last refreshed.
fn trash_path(path: &std::path::Path, display_name: &str) -> Result<(), String> {
    match gio::File::for_path(path).trash(gio::Cancellable::NONE) {
        Ok(()) => Ok(()),
        Err(e) if e.kind::<gio::IOErrorEnum>() == Some(gio::IOErrorEnum::NotFound) => Ok(()),
        Err(e) => Err(format!("Could not move {display_name} to Trash: {e}")),
    }
}

/// Execute a prepared delete without touching any GTK object. This job may
/// include directory enumeration, torrent metadata reads, and cross-volume
/// trash moves, all of which must stay off the GTK main thread.
fn execute_delete_operation(operation: DeleteOperation) -> Result<(), String> {
    match operation {
        DeleteOperation::Gallery { target, filename } => {
            if let Some(path) = target
                && path.is_dir()
            {
                trash_path(&path, &filename)?;
            }
        }
        DeleteOperation::Torrent {
            url,
            dest_dir,
            output_dir,
            filename,
        } => {
            let has_archive = crate::torrent::has_archive(&url);
            // Resolve the real output while archive metadata still exists.
            let single_path = if has_archive {
                crate::torrent::single_file_path(&dest_dir, &url)
            } else {
                None
            };
            let folder = if single_path.is_none() {
                Some(DownloadManager::torrent_folder_from_parts(
                    &dest_dir.to_string_lossy(),
                    &url,
                    &output_dir,
                    &filename,
                ))
            } else {
                None
            };

            if let Some(path) = single_path {
                // Archived single-file torrents use their metadata name.
                trash_path(&path, &filename)?;
            } else if let Some(path) = folder {
                if path.is_file() {
                    trash_path(&path, &filename)?;
                } else if has_archive {
                    // Preserve unrelated files in the folder; only trash
                    // paths owned by the torrent metadata.
                    if crate::torrent::trash_torrent_contents(&path, &url)? {
                        trash_path(&path, &filename)?;
                    }
                } else {
                    // A magnet without an archive cannot prove folder ownership.
                    tracing::info!(
                        "magnet delete: leaving folder in place (no archive to enumerate): {}",
                        path.display()
                    );
                }
            }
            // Archive deletion can touch disk too, so it belongs to this job.
            crate::torrent::delete_archive_for_url(&url);
        }
        DeleteOperation::File {
            path,
            sidecars,
            filename,
        } => {
            trash_path(&path, &filename)?;
            // Subtitle sidecars are best-effort: failure to trash a secondary
            // file must not turn a successful primary delete into a failure.
            for sidecar in sidecars {
                let display_name = sidecar.display().to_string();
                if let Err(e) = trash_path(&sidecar, &display_name) {
                    tracing::warn!(
                        sidecar = %sidecar.display(),
                        error = %e,
                        "subtitle sidecar left behind"
                    );
                }
            }
        }
    }
    Ok(())
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
    /// Pump task handles by download ID, so tests can wait for pump completion (a `spawn_future_local` task reaped on another thread trips glib's thread guard).
    pub(crate) pump_handles: RefCell<HashMap<u64, glib::JoinHandle<()>>>,
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
    /// Abort senders for resolver workers, by row. Signalled from pause/park/cancel so extractor streams stop promptly.
    video_abort: RefCell<HashMap<u64, tokio::sync::oneshot::Sender<crate::video::StopIntent>>>,
    /// Delivery decision per video attempt, by row. Manager and worker arbitrate on this one object.
    gates: RefCell<HashMap<u64, std::sync::Arc<AttemptGate>>>,
    /// Destinations with a discard in flight (exact path + stem key). Consulted by intake *and* `unremove`, which bypasses intake.
    reservations: std::sync::Arc<std::sync::Mutex<Reservations>>,
    /// Finalizers reclaiming a removed row's scratch, by row. Each keeps the worker abort handle: aborting the finalizer would detach the worker, leaving yt-dlp unsupervised (#180).
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
    persist_tx: RefCell<Option<tokio::sync::watch::Sender<Option<String>>>>,
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

struct RestoreSweepPlan {
    referenced: HashSet<String>,
    live: HashSet<u64>,
    dest_dirs: HashSet<String>,
    selections: Vec<(String, Vec<usize>)>,
    session_keep: Option<HashSet<String>>,
}

/// One validated queue entry awaiting the restore apply phase (module level so a mid-loop failure never leaves half-spawned engines).
struct PendingRestore {
    item: StoredItem,
    output_dir: Option<String>,
    segments: Option<SegmentState>,
    gallery_job: Option<crate::gallery_argv::GalleryJob>,
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
        // Abort any pump tasks: they're `spawn_future_local`, so a stale one
        // reaped on another thread trips glib's thread guard. Tests wait for
        // pump completion via `drain_engine` (which checks `pump_handles`),
        // so in practice none are pending here; this is a backstop.
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
                match serde_json::to_string(&queue) {
                    Ok(text) => Self::write_queue_file(&text),
                    Err(error) => tracing::error!("could not serialize download queue: {error}"),
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
            pump_handles: RefCell::new(HashMap::new()),
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
        self.queued.set(
            self.items()
                .filter(|it| it.status() == DownloadStatus::Queued)
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

    /// Fresh id that lands on no existing staging dir: the allocator must skip
    /// leftover `grab-<id>-*` files in dest dirs and legacy tmp dirs, or a new row
    /// would resume (or sweep) a stranger's scratch.
    fn alloc_id_for(&self, dest_dir: &std::path::Path) -> u64 {
        loop {
            let id = self.alloc_id();
            if !crate::video::staging_occupied(dest_dir, id) {
                return id;
            }
        }
    }

    /// Reuse a restored row's persisted id where possible, else allocate. The id is the staging key, so a fresh number would point at the wrong `grab-<id>-*` files and risk colliding with a later row.
    fn claim_id(&self, stored: Option<u64>, dest_dir: &std::path::Path) -> u64 {
        let Some(id) = stored else {
            return self.alloc_id_for(dest_dir);
        };
        // Already owned in this session: a hand-edited/duplicated queue must never share one staging directory.
        if self.find(id).is_some() || self.epoch.borrow().contains_key(&id) {
            return self.alloc_id_for(dest_dir);
        }
        // Keep the allocator ahead of every adopted id so a new row never reuses a restored row's number.
        if id >= self.next_id.get() {
            self.next_id.set(id + 1);
        }
        id
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

    /// Delete a segmented partial away from GTK. Reserve its exact path until
    /// unlink completes so an immediate retry cannot recreate it mid-cleanup.
    fn schedule_partial_cleanup(self: &Rc<Self>, path: std::path::PathBuf) {
        self.reserve_dest(&path);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let worker_path = path.clone();
            match gio::spawn_blocking(move || std::fs::remove_file(worker_path)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
                Ok(Err(e)) => tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "could not remove cancelled segmented partial"
                ),
                Err(e) => tracing::warn!(
                    path = %path.display(),
                    error = ?e,
                    "partial cleanup worker failed"
                ),
            }
            if let Some(manager) = weak.upgrade() {
                manager.release_dest(&path);
                manager.start_next();
            }
        });
    }

    /// Whether the row is currently capturing a live stream (stop-and-keep applies).
    pub fn is_live_video(&self, id: u64) -> bool {
        self.live_rows.borrow().contains(&id)
    }

    /// Explicit absolute dest, else the effective download dir.
    pub(crate) fn resolve_dir(&self, dest_dir: Option<&str>) -> String {
        // Explicit destinations must be absolute (relative dirs would resolve against the launcher CWD and fail the sandbox).
        dest_dir
            .filter(|s| !s.is_empty() && std::path::Path::new(s).is_absolute())
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.effective_download_dir())
    }

    /// Validate and queue a download, starting it when a slot is free.
    ///
    /// Names dedupe against live rows and reservations only: an on-disk file
    /// never renames the intake. The engine refuses a fresh row whose
    /// destination already exists (Parabolic-style), so duplicates surface as
    /// an error instead of scattering ` (1)` copies.
    ///
    /// # Errors
    /// Returns a display-ready message when the URL or filename is invalid.
    pub fn enqueue(
        self: &Rc<Self>,
        url: &str,
        dest_dir: Option<&str>,
        filename: Option<&str>,
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
        let item = DownloadItem::new(
            self.alloc_id_for(std::path::Path::new(&dir)),
            &url,
            &name,
            &dir,
        );
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
        let file_name = file_name.to_string();
        let dir = self.resolve_dir(dest_dir);
        let worker_file_name = file_name.clone();
        let worker_dir = dir.clone();
        let (pseudo, base, existing) = gio::spawn_blocking(move || {
            // Parse metadata before archiving so invalid input never leaves an archive behind.
            let base = crate::torrent::intake_plan(&bytes).map(|(base, _multi)| base);
            let pseudo = crate::torrent::archive_torrent_file(&worker_file_name, &bytes)?;
            let existing =
                crate::video_staging::dir_file_names(std::path::Path::new(&worker_dir));
            Ok::<_, String>((pseudo, base, existing))
        })
        .await
        .map_err(|error| format!("Could not prepare torrent file: {error:?}"))??;

        if let Some(selection) = only_files {
            crate::torrent::stage_selection(&pseudo, selection);
        }
        let stub = crate::torrent::stub_name_for_file(&file_name);
        let item = self.enqueue(&pseudo, dest_dir, Some(&stub))?;

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
        Ok(item)
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
    pub async fn enqueue_video_async(
        self: &Rc<Self>,
        page_url: &str,
        dest_dir: Option<&str>,
        filename: Option<&str>,
        choices: crate::media_types::VideoChoices,
    ) -> Result<DownloadItem, String> {
        let dir = self.resolve_dir(dest_dir);
        let listing_dir = dir.clone();
        let existing = gio::spawn_blocking(move || {
            crate::video_staging::dir_file_names(std::path::Path::new(&listing_dir))
        })
        .await
        .map_err(|error| format!("Could not inspect download folder: {error:?}"))?;
        self.enqueue_video_staged(page_url, &dir, filename, choices, &existing)
    }

    /// Enqueue a gallery download (image gallery via gallery-dl).
    /// Creates a DownloadItem with gallery flags set; the actual download
    /// is driven by the gallery runner (phase 12).
    /// Enqueue a gallery after listing its destination off GTK.
    pub async fn enqueue_gallery_async(
        self: &Rc<Self>,
        job: &crate::gallery_argv::GalleryJob,
    ) -> Result<DownloadItem, String> {
        let url = normalize_url(&job.page_url)?;
        let dir = self.resolve_dir(Some(&job.dest_dir.to_string_lossy()));
        let title = crate::file_names::sanitize_folder_name(&job.title);
        let listing_dir = dir.clone();
        let existing = gio::spawn_blocking(move || {
            crate::video_staging::dir_file_names(std::path::Path::new(&listing_dir))
        })
        .await
        .map_err(|error| format!("Could not inspect gallery destination: {error:?}"))?;

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
        let item = DownloadItem::new(
            self.alloc_id_for(std::path::Path::new(&dir)),
            &url,
            &unique_title,
            &dir,
        );
        item.set_is_gallery(true);
        item.set_gallery_file_count(job.file_count.unwrap_or(0));
        item.set_gallery_files_done(0);
        item.set_gallery_url(url.clone());
        item.set_detail(format!("Gallery • {} files", job.file_count.unwrap_or(0)));
        let gallery_dir = std::path::Path::new(&dir).join(&unique_title);
        let mut stored_job = job.clone();
        stored_job.dest_dir = gallery_dir;
        stored_job.item_id = item.id();
        self.gallery_jobs.borrow_mut().insert(item.id(), stored_job);
        Ok(self.insert(item))
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
        let item = DownloadItem::new(
            self.alloc_id_for(std::path::Path::new(&dir)),
            &url,
            &name,
            dir,
        );
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

    /// Queue one row per playlist entry for collection-shaped resolves with no picked entry. Returns (added, reported total). One persist for the whole import.
    /// Queue collection children after creating/listing their destination off GTK.
    async fn expand_playlist_rows_async(
        self: &Rc<Self>,
        id: u64,
        item: &DownloadItem,
        pl: &crate::media_types::PlaylistInfo,
    ) -> (usize, usize, usize) {
        let (quality, audio_only) = match self.video_source(id) {
            Some(crate::media_types::VideoSource::Page {
                quality,
                audio_only,
                ..
            }) => (quality, audio_only),
            _ => return (0, pl.total, 0),
        };
        let base_dir = self.resolve_dir(Some(&item.dest_dir()));
        let multi = pl.total > 1;
        let title = pl.title.clone();
        let prepared = gio::spawn_blocking(move || {
            let dest_dir = if multi {
                crate::file_names::collection_subdir(&base_dir, &title)
            } else {
                base_dir
            };
            let existing =
                crate::video_staging::dir_file_names(std::path::Path::new(&dest_dir));
            (dest_dir, existing)
        })
        .await;
        let (dest_dir, existing) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                tracing::warn!(
                    error = ?error,
                    parent_id = id,
                    "playlist import directory preparation failed"
                );
                return (0, pl.total, 0);
            }
        };

        // The main context yielded while the folder was prepared. Do not
        // enqueue stale children after pause/cancel/remove.
        let Some(current) = self.find(id) else {
            return (0, pl.total, 0);
        };
        if matches!(
            current.status(),
            DownloadStatus::Cancelled | DownloadStatus::Paused
        ) {
            return (0, pl.total, 0);
        }

        let _batch = self.batch_guard();
        let mut guarded_child_hosts = HashSet::new();
        let mut skipped_by_host_limit = 0usize;
        let mut added = 0usize;
        for entry in &pl.items {
            let Some(url) =
                crate::video_probe::expand_child_target(&item.url(), &pl.page_url, entry)
            else {
                continue;
            };
            match crate::download_net::inherit_public_resolution_for_collection(
                &item.url(),
                &url,
                &mut guarded_child_hosts,
            ) {
                Ok(()) => {}
                Err(crate::download_net::CollectionHostError::TooManyDistinctHosts) => {
                    skipped_by_host_limit += 1;
                    continue;
                }
                Err(crate::download_net::CollectionHostError::UnsafeDestination) => continue,
            }
            let remux = crate::video_prefs::remux_video_active(&self.settings.remux_video());
            let name =
                crate::video::default_video_filename(&entry.title, audio_only, remux.as_deref());
            if self
                .enqueue_video_staged(
                    &url,
                    &dest_dir,
                    Some(&name),
                    crate::media_types::VideoChoices {
                        quality: quality.clone(),
                        audio_only,
                        video_format_id: None,
                        is_live: false,
                        playlist_item_id: None,
                    },
                    &existing,
                )
                .is_ok()
            {
                added += 1;
            }
        }
        (added, pl.total, skipped_by_host_limit)
    }

    /// Re-queue one persisted entry, preserving its intent (paused/failed stay). Names restore verbatim to keep resume identity.
    /// The video-page source is staged *before* insert (see `enqueue_video`).
    ///
    /// # Errors
    /// Returns a display-ready message when the stored entry is invalid.
    pub fn restore_existing(self: &Rc<Self>, stored: &StoredItem) -> Result<DownloadItem, String> {
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
            self.claim_id(*stored_id, std::path::Path::new(dest_dir)),
            &url,
            &filename,
            dest_dir,
        );
        item.set_public_only(public_only);
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
        if let Some(src) = video_source {
            let matches = matches!(src, crate::media_types::VideoSource::Page { page_url, .. } if *page_url == url);
            if matches {
                let audio_only = matches!(
                    src,
                    crate::media_types::VideoSource::Page {
                        audio_only: true,
                        ..
                    }
                );
                item.set_detail(pending_resolve_detail(audio_only));
                self.video_sources
                    .borrow_mut()
                    .insert(item.id(), src.clone());
            } else {
                tracing::warn!("dropping video source with mismatched page URL");
            }
        }
        // A row the engine started in a past session resumes its own
        // partial; only a never-started row refuses an existing dest.
        // Legacy queue files (started == None) keep the old resume behavior.
        if stored.started.unwrap_or(true) {
            self.started.borrow_mut().insert(item.id());
        }
        Ok(self.insert(item))
    }

    /// Restore a gallery row from a validated stored entry + parsed job.
    /// Mirrors `enqueue_gallery` but reuses the stored id and status.
    pub fn restore_existing_gallery(
        self: &Rc<Self>,
        stored: &StoredItem,
        job: Option<crate::gallery_argv::GalleryJob>,
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
        let id = self.claim_id(stored.id, std::path::Path::new(&stored.dest_dir));
        job.item_id = id;
        let item = DownloadItem::new(id, &url, &title, &stored.dest_dir);
        item.set_public_only(public_only);
        item.set_is_gallery(true);
        let file_count = job.file_count.unwrap_or(0);
        item.set_gallery_file_count(file_count);
        // Done rows show the full count; others start at 0.
        let files_done = if stored.status == DownloadStatus::Done {
            file_count
        } else {
            0
        };
        item.set_gallery_files_done(files_done);
        item.set_gallery_url(url.clone());
        // Done rows show the finished count; others mirror enqueue_gallery's
        // total-count format (done_label(0) would read "Gallery • 0 files").
        if stored.status == DownloadStatus::Done {
            item.set_detail(crate::gallery_runner::done_label(files_done as usize, 0));
        } else {
            item.set_detail(format!("Gallery • {file_count} files"));
        }
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
        self.gallery_jobs.borrow_mut().insert(id, job);
        if stored.started.unwrap_or(true) {
            self.started.borrow_mut().insert(id);
        }
        Ok(self.insert(item))
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

    /// Drop the row's policy reference. The host stays registered so an open
    /// card or another row on the same host keeps its protection.
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
        let inflight = Rc::clone(&self.sizing_inflight);
        inflight.set(inflight.get() + 1);
        glib::spawn_future_local(async move {
            let measured = crate::file_names::path_size_async(path)
                .await
                .filter(|m| *m > 0);
            // Balance the counter before any early return: the row may be
            // gone or the path missing, but the task still finished.
            inflight.set(inflight.get() - 1);
            let (Some(item), Some(measured)) = (weak.upgrade(), measured) else {
                return;
            };
            item.set_detail(gettext("Finished • {size}").replace("{size}", &fmt_bytes(measured)));
        });
    }

    fn insert_history(
        self: &Rc<Self>,
        url: String,
        dir: String,
        name: String,
        progress: f64,
        output_dir: Option<String>,
        video_source: Option<crate::media_types::VideoSource>,
    ) {
        let Ok(url) = normalize_url(&url) else {
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
        let item = DownloadItem::new(
            self.alloc_id_for(std::path::Path::new(&dir)),
            &url,
            &name,
            &dir,
        );
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
        // Restored duplicates collapse too: the last Done row per URL wins.
        self.drop_finished_duplicates(&url, item.id());
        // Keep the row's media identity: a restored video page must not fall
        // back to the generic file icon (same page-URL guard as
        // `restore_existing`).
        if let Some(src) = video_source
            && matches!(src, crate::media_types::VideoSource::Page { ref page_url, .. } if *page_url == url)
        {
            self.video_sources.borrow_mut().insert(item.id(), src);
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

    fn start_next(self: &Rc<Self>) {
        // Startup restore runs asynchronously in production. Don't start a newly
        // queued URL before persisted active rows have been inserted, or we can
        // briefly exceed the configured concurrency limit.
        if self.restore_state.get() != RestoreState::Ready || self.shutdown_started.get() {
            return;
        }
        // Prune finished discard finalizers so the registry doesn't grow per removed row.
        self.discards
            .borrow_mut()
            .retain(|_, pending| !pending.finalizer.is_finished());
        while self.running.borrow().len() < self.max_concurrent() {
            let next = self
                .items()
                // A discard still tearing down owns its destination: the row stays Queued until the reservation clears.
                .find(|it| {
                    it.status() == DownloadStatus::Queued && !self.dest_reserved(&it.file_path())
                });
            match next {
                Some(item) => {
                    if item.is_gallery() {
                        self.spawn_gallery(item);
                    } else {
                        self.spawn(item);
                    }
                }
                None => break,
            }
        }
    }

    fn spawn(self: &Rc<Self>, item: DownloadItem) {
        let id = item.id();
        let dest = item.file_path();
        let dest_dir = item.dest_dir().to_string();
        let url = item.url().to_string();
        let is_torrent = crate::torrent::is_torrent(&url);
        let is_video = matches!(
            self.video_source(id),
            Some(crate::media_types::VideoSource::Page { .. })
        );
        let segments = self.segment_state.borrow().get(&id).cloned();
        let started = self.started.borrow().contains(&id);
        // Reserve the target while filesystem validation is in flight. This
        // keeps another queued row or a rapid retry from claiming the same path.
        self.reserve_dest(&dest);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let worker_dest = dest.clone();
            let worker_dir = dest_dir.clone();
            let worker_recorded = item.output_dir().to_string();
            let worker_segments = segments.clone();
            let preparation = gio::spawn_blocking(move || {
                let parent = worker_dest.parent();
                if let Some(parent) = parent {
                    match ensure_contained_parent(parent, &worker_dir) {
                        Ok(true) => {}
                        Ok(false) => {
                            return Err(gettext("Download path escapes the download folder"));
                        }
                        Err(error) => {
                            return Err(
                                gettext("Could not create the download folder: {error}")
                                    .replace("{error}", &error.to_string()),
                            );
                        }
                    }
                }

                let mut torrent_dest = None;
                let mut torrent_output_dir = None;
                if is_torrent {
                    let dir = std::path::PathBuf::from(&worker_dir);
                    let recorded_path = std::path::PathBuf::from(&worker_recorded);
                    let output = if worker_recorded.is_empty() {
                        dir.clone()
                    } else {
                        let guarded =
                            crate::torrent::guard_output_folder(&dir, recorded_path.clone());
                        if guarded != recorded_path {
                            torrent_output_dir = Some(guarded.to_string_lossy().into_owned());
                        } else if guarded.parent() != Some(dir.as_path()) {
                            std::fs::create_dir_all(&guarded).map_err(|error| {
                                gettext("Could not create the download folder: {error}")
                                    .replace("{error}", &error.to_string())
                            })?;
                        }
                        guarded
                    };
                    if worker_recorded.is_empty() {
                        std::fs::create_dir_all(&dir).map_err(|error| {
                            gettext("Could not create the download folder: {error}")
                                .replace("{error}", &error.to_string())
                        })?;
                    }
                    torrent_dest = Some(output);
                }

                let mut drop_segments = false;
                let mode = if is_torrent || is_video {
                    None
                } else {
                    Some(match worker_segments {
                        Some(state) => {
                            // Drop an invalid bitmap and stale partial together. Do both on
                            // the worker so resume validation does not block GTK.
                            let len = std::fs::metadata(&worker_dest)
                                .map(|metadata| metadata.len())
                                .unwrap_or(0);
                            if len < state.prefix_len() || len > state.total {
                                let _ = std::fs::remove_file(&worker_dest);
                                drop_segments = true;
                                StartMode::Fresh
                            } else {
                                StartMode::Resume(state)
                            }
                        }
                        None => match std::fs::metadata(&worker_dest) {
                            Err(_) => StartMode::Fresh,
                            Ok(_) if !started => {
                                return Err(DEST_EXISTS.to_string());
                            }
                            Ok(_) => StartMode::Single,
                        },
                    })
                };
                Ok((mode, drop_segments, torrent_dest, torrent_output_dir))
            })
            .await;

            let Some(manager) = weak.upgrade() else {
                return;
            };
            manager.release_dest(&dest);
            let Some(current) = manager.find(id) else {
                manager.start_next();
                return;
            };
            if current.status() != DownloadStatus::Queued || current.file_path() != dest {
                manager.start_next();
                return;
            }
            // A cancel/retry can happen while the blocking task runs. If any input
            // changed, discard its answer and let the scheduler take a fresh snapshot.
            let current_segments = manager.segment_state.borrow().get(&id).cloned();
            let same_segments = match (&segments, &current_segments) {
                (None, None) => true,
                (Some(before), Some(after)) => {
                    before.total == after.total && before.done == after.done
                }
                _ => false,
            };
            if !same_segments || manager.started.borrow().contains(&id) != started {
                manager.start_next();
                return;
            }

            let (mode, drop_segments, torrent_dest, torrent_output_dir) = match preparation {
                Ok(Ok(prepared)) => prepared,
                Ok(Err(detail)) => {
                    current.set_detail(detail.clone());
                    current.set_status(DownloadStatus::Failed);
                    manager.changed();
                    manager.notify_finished(&current, Err(detail));
                    manager.start_next();
                    return;
                }
                Err(error) => {
                    let detail = gettext("Could not prepare download destination: {error}")
                        .replace("{error}", &format!("{error:?}"));
                    current.set_detail(detail.clone());
                    current.set_status(DownloadStatus::Failed);
                    manager.changed();
                    manager.notify_finished(&current, Err(detail));
                    manager.start_next();
                    return;
                }
            };
            if drop_segments {
                manager.segment_state.borrow_mut().remove(&id);
                manager.persist_queue();
            }
            manager.spawn_prepared(current, mode, torrent_dest, torrent_output_dir);
        });
    }

    fn spawn_prepared(
        self: &Rc<Self>,
        item: DownloadItem,
        mode: Option<StartMode>,
        torrent_dest: Option<std::path::PathBuf>,
        torrent_output_dir: Option<String>,
    ) {
        let opts = DownloadOptions::from_settings(&self.settings);
        // Re-stash per attempt: a stale Last-Modified must never apply when the new run's server sends none.
        self.server_mtime.borrow_mut().remove(&item.id());
        let dest = item.file_path();
        // Fail fast on junk; later edits apply without re-queueing.
        let limit = opts.limit_rate.trim();
        if !limit.is_empty() && limit != "0" && parse_rate(limit).is_none() {
            item.set_detail(gettext("Invalid speed limit: {limit}").replace("{limit}", limit));
            item.set_status(DownloadStatus::Failed);
            self.changed();
            return;
        }
        publish_rate_limit(&self.settings);
        let url = item.url().to_string();
        if crate::torrent::is_torrent(&url) {
            let Some(dest) = torrent_dest else {
                item.set_detail(gettext("Could not prepare the torrent output folder"));
                item.set_status(DownloadStatus::Failed);
                self.changed();
                return;
            };
            if let Some(output_dir) = torrent_output_dir {
                item.set_output_dir(output_dir);
            }
            return self.spawn_torrent(item, url, dest);
        }
        if let Some(crate::media_types::VideoSource::Page {
            page_url,
            quality,
            audio_only,
            is_live,
            video_format_id,
            playlist_item_id,
            ..
        }) = self.video_source(item.id())
        {
            return self.spawn_video(SpawnVideoParams {
                item,
                page_url,
                quality,
                audio_only,
                video_format_id,
                is_live,
                playlist_item_id,
            });
        }
        let connections = (opts.connections.max(1) as usize).min(16);
        let timeout = Duration::from_secs(30);
        // A saved bitmap means non-contiguous pieces: only a segmented resume is correct.
        let mode = mode.unwrap_or(StartMode::Fresh);
        let generation = self.epoch.borrow().get(&item.id()).cloned().unwrap_or(0) + 1;
        self.epoch.borrow_mut().insert(item.id(), generation);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        // Invalid manual proxy fails the row loudly: never silently route direct around an explicit user demand.
        let proxy = match opts.proxy_config() {
            Ok(proxy) => proxy,
            Err(e) => {
                item.set_detail(e);
                item.set_status(DownloadStatus::Failed);
                self.changed();
                return;
            }
        };
        let client = match http_client_for(proxy.as_ref()) {
            Ok(c) => c,
            Err(e) => {
                item.set_detail(e);
                item.set_status(DownloadStatus::Failed);
                self.changed();
                return;
            }
        };
        let ctx = FetchCtx {
            client,
            url: url.clone(),
            dest,
            opts,
            if_range: {
                let lock = std::sync::Arc::new(std::sync::OnceLock::new());
                let etag = item.etag();
                // If-Range requires a strong validator: weak ETags (W/"…")
                // never match, so skip them and fall back to Last-Modified.
                // If both are absent (weak ETag with no Last-Modified), no
                // If-Range is sent and resume is unvalidated, as before.
                let validator = if !etag.is_empty() && !etag.starts_with("W/") {
                    Some(etag.to_string())
                } else {
                    let lm = item.last_modified();
                    (!lm.is_empty()).then(|| lm.to_string())
                };
                if let Some(v) = validator {
                    let _ = lock.set(v);
                }
                lock
            },
            cookies: None,
            // If a handoff jar is accepted at attempt start, run_download also
            // applies its matching UA. Otherwise use the default with live-browser cookies.
            user_agent: crate::download_net::DEFAULT_USER_AGENT.to_string(),
            timeout,
            tx,
        };
        let handle = tokio_rt().spawn(run_download(ctx, connections, mode));
        self.running.borrow_mut().insert(item.id(), handle);
        // The engine owns the destination from here: later spawns resume it rather than refusing as foreign.
        // Persist immediately: a force-close before the next periodic save must not lose the ownership claim.
        self.started.borrow_mut().insert(item.id());
        self.persist_queue();
        item.set_status(DownloadStatus::Downloading);
        // Attempts start indeterminate: clear any stale fraction so the row pulses through the connecting phase.
        item.set_progress(0.0);
        let host = url::Url::parse(&item.url())
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
            .unwrap_or_default();
        item.set_detail(if host.is_empty() {
            gettext("Starting…")
        } else {
            gettext("Connecting to {host}…").replace("{host}", &host)
        });
        self.changed();

        let item_id = item.id();
        self.pump(item, item_id, generation, rx);
    }

    /// Whether a finished-name candidate is taken (file on disk, reserved stem, or live row holding it). Used by the Finished claim loop.
    fn is_name_taken(&self, dir: &str, existing: &[String], n: &str) -> bool {
        existing.iter().any(|entry| entry == n)
            || crate::video_staging::stem_reserved_in(existing, name_stem(n))
            // A discard still tearing down owns this destination and its
            // stem: the finalizer's sweep (and the orphan removal when the
            // commit won) must not meet a row that claimed the name meanwhile.
            || self.dest_reserved(&std::path::Path::new(dir).join(n))
            || self.items()
                .any(|it| it.dest_dir() == dir && it.filename() == n)
    }

    /// Drain one engine's message channel into its row. Shared by the HTTP and torrent engines.
    fn pump(
        self: &Rc<Self>,
        item: DownloadItem,
        id: u64,
        generation: u64,
        mut rx: tokio::sync::mpsc::UnboundedReceiver<EngineMsg>,
    ) {
        // Weak ref: a strong Rc here would keep the manager alive past its
        // owner's drop, leaving a thread-bound `spawn_future_local` task to be
        // polled on a later thread's context iteration — tripping glib's
        // thread guard. When the manager is gone the pump has nothing to
        // update, so it exits.
        let weak = Rc::downgrade(self);
        // Speed baseline: resumes seed `downloaded` with pre-existing bytes, which lifetime-average math would report as fantasy GB/s.
        let mut base: Option<(u64, Instant)> = None;
        // Set on Finished/Failed. If the channel closes first the engine died without reporting: fail the row instead of stranding it.
        let mut done = false;
        // Phase owns the detail text while set (torrent init: metadata/peers).
        // Progress ticks preserve it until bytes flow, then resume normal detail.
        let mut phase_owned = false;
        // Row URL and file filter are fixed at intake: hoist both out of the per-tick path.
        let url = item.url().to_string();
        let sel_suffix = match crate::torrent::get_selection(&url) {
            Some(sel) => format!(" • {} files", sel.len()),
            None => String::new(),
        };
        // Track the pump handle so tests can wait for pump completion: a `spawn_future_local` task reaped on another thread trips glib's thread guard.
        let handle = glib::spawn_future_local(async move {
            while let Some(msg) = rx.recv().await {
                let Some(this) = weak.upgrade() else {
                    break;
                };
                // Stale spawn: its reports would drag the bar backwards and its tail would fail the row or steal the new engine's handle.
                if !this.is_current(id, generation) {
                    break;
                }
                match msg {
                    EngineMsg::Progress {
                        downloaded,
                        total,
                        uploaded,
                        upload_bps,
                    } => {
                        if item.status() != DownloadStatus::Downloading {
                            continue;
                        }
                        // Phase owns the detail text until bytes flow; then
                        // Progress resumes normal detail.
                        if phase_owned && downloaded > 0 {
                            phase_owned = false;
                        }
                        let (d0, tb) = match base {
                            Some((d0, tb)) if downloaded >= d0 => (d0, tb),
                            _ => {
                                let b = (downloaded, Instant::now());
                                base = Some(b);
                                b
                            }
                        };
                        let bps = downloaded.saturating_sub(d0) as f64
                            / Instant::now().duration_since(tb).as_secs_f64().max(0.001);
                        let speed = format!("{}/s", fmt_bytes(bps as u64));
                        // Torrent upload counters (HTTP rows send zeros, so their labels stay unchanged).
                        let up_suffix = if uploaded > 0 || upload_bps > 0 {
                            let ratio = total.filter(|&t| t > 0).map_or_else(String::new, |t| {
                                format!(" · ratio {:.1}", uploaded as f64 / t as f64)
                            });
                            gettext(" • ↑ {upspeed}/s · {up} up{ratio}")
                                .replace("{upspeed}", &fmt_bytes(upload_bps))
                                .replace("{up}", &fmt_bytes(uploaded))
                                .replace("{ratio}", &ratio)
                        } else {
                            String::new()
                        };
                        // Torrent rows with a file filter show the count (hoisted above: fixed for the row's lifetime).
                        match total {
                            Some(t) if t > 0 => {
                                let frac = (downloaded as f64 / t as f64).clamp(0.0, 1.0);
                                item.set_progress(frac);
                                if !phase_owned {
                                    let eta = if bps > 0.0 {
                                        fmt_eta((t.saturating_sub(downloaded) as f64 / bps) as u64)
                                    } else {
                                        "—".to_string()
                                    };
                                    item.set_detail(
                                        gettext("{pct}% ({amounts}) • {speed} • About {eta} left{filter}{up}")
                                            .replace("{pct}", &((frac * 100.0) as u64).to_string())
                                            .replace("{amounts}", &format_amounts(downloaded, t))
                                            .replace("{speed}", &speed)
                                            .replace("{eta}", &eta)
                                            .replace("{filter}", &sel_suffix)
                                            .replace("{up}", &up_suffix),
                                    );
                                }
                            }
                            _ => {
                                if !phase_owned {
                                    item.set_detail(
                                        gettext("{done} • {speed}{filter}{up}")
                                            .replace("{done}", &fmt_bytes(downloaded))
                                            .replace("{speed}", &speed)
                                            .replace("{filter}", &sel_suffix)
                                            .replace("{up}", &up_suffix),
                                    );
                                }
                            }
                        }
                    }
                    EngineMsg::Finished { size } => {
                        if item.status() != DownloadStatus::Cancelled
                            && item.status() != DownloadStatus::Paused
                        {
                            // A pause keeps its pending name: resumed single-stream attempts never re-suggest.
                            let pending = this.pending_names.borrow_mut().remove(&id);
                            if let Some(name) = pending {
                                // Re-apply the final name policy: adoption is the last gate before dedupe/rename. Idempotent on finalized names.
                                let name = this.finalize_filename(&name);
                                let current = item.filename().to_string();
                                let dir = item.dest_dir().to_string();
                                let listing_dir = dir.clone();
                                let existing = match gio::spawn_blocking(move || {
                                    crate::video_staging::dir_file_names(std::path::Path::new(&listing_dir))
                                })
                                .await
                                {
                                    Ok(names) => names,
                                    Err(error) => {
                                        tracing::warn!(error = ?error, dir = %dir, "finished-name directory listing failed");
                                        Vec::new()
                                    }
                                };
                                // The main context was yielded while the directory was read. Do not finalize a row that was
                                // paused/cancelled, removed, or replaced while the worker ran.
                                if item.status() == DownloadStatus::Cancelled
                                    || item.status() == DownloadStatus::Paused
                                {
                                    done = true;
                                    break;
                                }
                                let taken = |n: &str| this.is_name_taken(&dir, &existing, n);
                                // Claim-then-move so a file appearing between dedupe and rename is never clobbered.
                                let old_path = item.file_path();
                                let mut final_name = dedupe_filename(&name, taken);
                                let mut moved = !existing.iter().any(|entry| entry == &current);
                                for _ in 0..8 {
                                    if moved
                                        || old_path == std::path::Path::new(&dir).join(&final_name)
                                    {
                                        break;
                                    }
                                    match rename_noreplace(
                                        &old_path,
                                        &std::path::Path::new(&dir).join(&final_name),
                                    ) {
                                        Ok(()) => {
                                            moved = true;
                                        }
                                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                                            final_name = dedupe_filename(&name, taken);
                                        }
                                        Err(_) => break,
                                    }
                                }
                                if moved && final_name != current {
                                    item.set_filename(final_name);
                                }
                            }
                            // Size off the final path, measured off-thread (see
                            // `set_finished_detail`): the engine measured the
                            // pre-rename one. Paints the engine's size now,
                            // refines with the measured size when it lands.
                            this.set_finished_detail(&item, size);
                            // Server-date stamping and torrent directory cleanup can perform filesystem
                            // metadata reads and recursive walks; snapshot plain inputs and defer them.
                            let mtime = this.server_mtime.borrow_mut().remove(&id);
                            let keep_server_date = this.settings.keep_server_date();
                            let cleanup_torrent = crate::torrent::is_torrent(&url)
                                && !this.settings.torrent_seed_finished();
                            let finished_path = item.file_path();
                            let torrent_dest_dir = item.dest_dir().to_string();
                            let torrent_output_dir = item.output_dir().to_string();
                            let torrent_filename = item.filename().to_string();
                            item.set_progress(1.0);
                            item.set_status(DownloadStatus::Done);
                            // One finished record per URL (Parabolic parity): re-downloads replace instead of stacking.
                            this.drop_finished_duplicates(&url, id);
                            this.segment_state.borrow_mut().remove(&id);
                            this.torrent_pieces.borrow_mut().remove(&id);
                            if cleanup_torrent {
                                // The intake filter is spent: keeping it would pin the index list for a done row.
                                crate::torrent::drop_selection(&url);
                            }
                            if this.find(id).is_some() {
                                this.notify_finished(&item, Ok(()));
                            }
                            if (keep_server_date && mtime.is_some()) || cleanup_torrent {
                                let worker_url = url.clone();
                                glib::spawn_future_local(async move {
                                    let _ = gio::spawn_blocking(move || {
                                        if keep_server_date
                                            && let Some(time) = mtime
                                            && let Ok(file) = std::fs::File::open(&finished_path)
                                        {
                                            let _ = file.set_modified(time);
                                        }
                                        if cleanup_torrent {
                                            let folder = DownloadManager::torrent_folder_from_parts(
                                                &torrent_dest_dir,
                                                &worker_url,
                                                &torrent_output_dir,
                                                &torrent_filename,
                                            );
                                            if folder.is_dir() {
                                                crate::torrent::cleanup_unselected(&folder, &worker_url);
                                            }
                                        }
                                    })
                                    .await;
                                });
                            }
                        }
                        done = true;
                        break;
                    }
                    EngineMsg::ExpandPlaylist(pl) => {
                        // Pause/cancel during resolve must not enqueue: the carrier stays put and nothing new appears.
                        if item.status() == DownloadStatus::Cancelled
                            || item.status() == DownloadStatus::Paused
                        {
                            done = true;
                            break;
                        }
                        // Playlist expansion runs on the main thread where queueing is legal; the new rows are the feedback.
                        let (added, total, skipped_by_host_limit) =
                            this.expand_playlist_rows_async(id, &item, &pl).await;
                        let skipped_detail = || {
                            ngettext(
                                "{skipped} item skipped because the collection exceeded the limit of {limit} distinct hosts",
                                "{skipped} items skipped because the collection exceeded the limit of {limit} distinct hosts",
                                skipped_by_host_limit as u32,
                            )
                            .replace("{skipped}", &skipped_by_host_limit.to_string())
                            .replace(
                                "{limit}",
                                &crate::download_net::MAX_GUARDED_CHILD_HOSTS_PER_COLLECTION
                                    .to_string(),
                            )
                        };
                        if added == 0 {
                            let e = if skipped_by_host_limit > 0 {
                                format!(
                                    "{}; {}",
                                    crate::video_probe::playlist_resolve_error(None),
                                    skipped_detail()
                                )
                            } else {
                                crate::video_probe::playlist_resolve_error(None).to_string()
                            };
                            if item.status() != DownloadStatus::Cancelled
                                && item.status() != DownloadStatus::Paused
                            {
                                item.set_detail(e.clone());
                                item.set_status(DownloadStatus::Failed);
                                this.torrent_pieces.borrow_mut().remove(&id);
                                this.notify_finished(&item, Err(e));
                            }
                        } else if item.status() != DownloadStatus::Cancelled
                            && item.status() != DownloadStatus::Paused
                        {
                            item.set_status(DownloadStatus::Done);
                            let detail = ngettext(
                                "Expanded into {added} of {total} item",
                                "Expanded into {added} of {total} items",
                                added as u32,
                            )
                            .replace("{added}", &added.to_string())
                            .replace("{total}", &total.to_string());
                            item.set_detail(if skipped_by_host_limit > 0 {
                                format!("{detail}; {}", skipped_detail())
                            } else {
                                detail
                            });
                            item.set_progress(1.0);
                            this.pending_names.borrow_mut().remove(&id);
                            this.server_mtime.borrow_mut().remove(&id);
                            // Like Finished: an older Done carrier for the URL leaves so re-expansions replace instead of stacking.
                            this.drop_finished_duplicates(&url, id);
                        }
                        done = true;
                        break;
                    }
                    EngineMsg::Failed(e) => {
                        // A failed download keeps its URL-derived name.
                        this.pending_names.borrow_mut().remove(&id);
                        this.server_mtime.borrow_mut().remove(&id);
                        // DEST_EXISTS (a foreign file at our path) fails the row:
                        // Parabolic-style, duplicates surface instead of
                        // requeueing under a fresh name.
                        if item.status() != DownloadStatus::Cancelled
                            && item.status() != DownloadStatus::Paused
                        {
                            // Set detail BEFORE status: the status notify
                            // pops the failure dialog, which reads detail.
                            item.set_detail(e.clone());
                            item.set_status(DownloadStatus::Failed);
                            this.torrent_pieces.borrow_mut().remove(&id);
                            this.notify_finished(&item, Err(e));
                        }
                        done = true;
                        break;
                    }
                    EngineMsg::FailedVersion(e) => {
                        // Server bytes changed mid-download: the resume bitmap describes a dead version, so drop it and re-probe fresh.
                        // Clear the validator too: retrying with the dead ETag/Last-Modified would send a stale If-Range,
                        // get 200 on every piece, and fail again permanently.
                        // Explicitly delete the partial file: its bytes belong to the dead version and must not survive
                        // into a retry (previously this was only emergent via 416/hole handling).
                        let invalid_path = item.file_path();
                        this.segment_state.borrow_mut().remove(&id);
                        this.pending_names.borrow_mut().remove(&id);
                        item.set_etag(String::new());
                        item.set_last_modified(String::new());
                        this.persist_queue();
                        this.reserve_dest(&invalid_path);
                        let worker_path = invalid_path.clone();
                        let _ = gio::spawn_blocking(move || std::fs::remove_file(worker_path)).await;
                        this.release_dest(&invalid_path);
                        this.start_next();
                        if item.status() != DownloadStatus::Cancelled
                            && item.status() != DownloadStatus::Paused
                        {
                            // Set detail BEFORE status: the status notify
                            // pops the failure dialog, which reads detail.
                            item.set_detail(e.clone());
                            item.set_status(DownloadStatus::Failed);
                            this.notify_finished(&item, Err(e));
                        }
                        done = true;
                        break;
                    }
                    EngineMsg::SegmentsInit { total, ack } => {
                        this.segment_state
                            .borrow_mut()
                            .insert(id, SegmentState::new(total));
                        // Queue the bitmap before the engine preallocates the
                        // full-size file (it waits for this ack): a crash in
                        // between would otherwise restore a full-size file with
                        // no bitmap, and a 416 would pass it off as complete on
                        // filesystems that zero-fill instead of leaving holes.
                        this.persist_queue();
                        ack.send(()).await.ok();
                    }
                    EngineMsg::PieceDone(idx) => {
                        if let Some(st) = this.segment_state.borrow_mut().get_mut(&id) {
                            st.mark(idx);
                        }
                    }
                    EngineMsg::TorrentPieces(have) => {
                        this.torrent_pieces.borrow_mut().insert(id, have);
                    }
                    EngineMsg::Phase(detail) => {
                        if item.status() == DownloadStatus::Downloading {
                            item.set_detail(detail);
                            phase_owned = true;
                        }
                    }
                    EngineMsg::TruncatePrefix => {
                        // Commit the matching bitmap first: Undo/retry during the worker must never see a
                        // prefix-truncated file paired with bits beyond the first gap.
                        let state = {
                            let mut states = this.segment_state.borrow_mut();
                            states.get_mut(&id).map(|st| {
                                st.forget_beyond_prefix();
                                st.clone()
                            })
                        };
                        if let Some(state) = state {
                            this.persist_queue();
                            let path = item.file_path();
                            this.reserve_dest(&path);
                            let worker_path = path.clone();
                            let _ = gio::spawn_blocking(move || {
                                truncate_to_prefix(&worker_path, &state)
                            })
                            .await;
                            this.release_dest(&path);
                            this.start_next();
                        }
                    }
                    EngineMsg::FallbackSingle { ack } => {
                        // Drop and persist the bitmap before yielding. The worker owns only the
                        // file mutation; a retry/Undo waits on the path reservation until it completes.
                        let state = this.segment_state.borrow_mut().remove(&id);
                        this.persist_queue();
                        if let Some(state) = state {
                            let path = item.file_path();
                            this.reserve_dest(&path);
                            let worker_path = path.clone();
                            let _ = gio::spawn_blocking(move || {
                                truncate_to_prefix(&worker_path, &state)
                            })
                            .await;
                            this.release_dest(&path);
                            this.start_next();
                        }
                        ack.send(()).await.ok();
                    }
                    EngineMsg::SuggestName(name) => {
                        // Stash the server-advertised name for adoption at Finished:
                        // moving mid-transfer desyncs the engine (stale size reads,
                        // split files), so only fresh single-stream attempts suggest.
                        if item.status() != DownloadStatus::Downloading {
                            continue;
                        }
                        let current = item.filename().to_string();
                        if name == current || !sane_filename(&name) {
                            continue;
                        }
                        // Container truth from video engines: same stem, truer extension. Gated on video rows so plain-engine behavior stays bit-identical.
                        let is_video_row = matches!(
                            this.video_source(id),
                            Some(crate::media_types::VideoSource::Page { .. })
                        );
                        if is_video_row
                            && name.contains('.')
                            && name_stem(&name) == name_stem(&current)
                            && name != current
                        {
                            // Routed through the shared final policy so the opt-in ASCII fold covers container-truth names.
                            let name = this.finalize_filename(&name);
                            this.pending_names.borrow_mut().insert(id, name);
                            continue;
                        }
                        // Chromium parity: adopt when current is a placeholder/extensionless, or the server name is shorter with an extension.
                        let placeholder = current == "index.html" || !current.contains('.');
                        if !placeholder && !(name.contains('.') && name.len() < current.len()) {
                            continue;
                        }
                        let name = this.finalize_filename(&name);
                        if name == current || !sane_filename(&name) {
                            continue;
                        }
                        this.pending_names.borrow_mut().insert(id, name);
                    }
                    EngineMsg::LastModified(t) => {
                        // Latest attempt wins; applied at Finished when keep-server-date is on.
                        this.server_mtime.borrow_mut().insert(id, t);
                    }
                    EngineMsg::Validator(info) => {
                        // Resume validator for If-Range, captured once per download
                        // generation. Overwrites both fields: a stale Last-Modified
                        // must not survive when the new response has only an ETag.
                        item.set_etag(info.etag.unwrap_or_default());
                        item.set_last_modified(info.last_modified.unwrap_or_default());
                    }
                    EngineMsg::LiveDetected => {
                        // Downloading only: a stop/pause during resolve must not leave a stale live_rows member (it would skip staging cleanup and take live signal paths).
                        if item.status() == DownloadStatus::Downloading {
                            this.live_rows.borrow_mut().insert(id);
                        }
                    }
                }
            }
            // Post-loop cleanup: the manager may have been dropped while the
            // pump was pending; only clean up if it's still alive.
            if let Some(this) = weak.upgrade() {
                // Superseded pump future: touch nothing, especially not the new engine's handle in `running`.
                if !this.is_current(id, generation) {
                    return;
                }
                this.running.borrow_mut().remove(&id);
                this.video_abort.borrow_mut().remove(&id);
                this.live_rows.borrow_mut().remove(&id);
                if this.draining.get() {
                    // Draining: this is the current pump (not superseded), so
                    // remove its handle before returning — otherwise
                    // `drain_engine` spins to its 15 s timeout after shutdown.
                    this.pump_handles.borrow_mut().remove(&id);
                    return;
                }
                if !done && item.status() == DownloadStatus::Downloading {
                    item.set_detail(gettext("Download interrupted"));
                    item.set_status(DownloadStatus::Failed);
                    this.notify_finished(&item, Err(gettext("Download interrupted")));
                }
                this.persist_queue();
                this.changed();
                this.start_next();
            }
            // Pump exiting: remove the handle so `drain_engine` knows it's done.
            // (Stale pumps return early above without removing: a stale pump
            // implies a successor overwrote the entry, so removing by id would
            // delete the successor's handle.)
            if let Some(this) = weak.upgrade() {
                this.pump_handles.borrow_mut().remove(&id);
            }
        });
        // Abort-after-insert: a superseded handle lingers until its next message
        // if merely dropped. Abort AFTER insert so the stale-tail `is_current`
        // skip still protects the new entry from the old pump's cleanup.
        if let Some(old) = self.pump_handles.borrow_mut().insert(id, handle) {
            old.abort();
        }
    }

    /// Spawn the torrent engine for a magnet row. Mirrors `spawn`'s contract so pause/cancel/retry and the stale-pump guard keep working.
    fn spawn_torrent(
        self: &Rc<Self>,
        item: DownloadItem,
        magnet: String,
        dest: std::path::PathBuf,
    ) {
        let recorded = item.output_dir().to_string();
        let settings = &self.settings;
        let seed_finished = settings.torrent_seed_finished();
        let peer_limit: Option<usize> = None; // rqbit default
        let download_bps = parse_rate(settings.speed_limit().trim());
        let upload_bps = parse_rate(settings.torrent_upload_limit().trim());
        let trackers: Option<Vec<String>> = None;
        // A malformed blocklist URL fails loudly: silently torrenting without it would betray the user's intent.
        let blocklist_url =
            match crate::torrent::blocklist_url_of(&settings.torrent_blocklist_url()) {
                Ok(url) => url,
                Err(e) => {
                    // Set detail BEFORE status: the status notify pops the
                    // failure dialog, which reads detail.
                    item.set_detail(e);
                    item.set_status(DownloadStatus::Failed);
                    self.changed();
                    return;
                }
            };
        // Invalid manual proxy fails loudly: no silent direct torrent while the user asked for a tunnel.
        let proxy = match DownloadOptions::from_settings(settings).proxy_config() {
            Ok(proxy) => proxy,
            Err(e) => {
                item.set_detail(e);
                item.set_status(DownloadStatus::Failed);
                self.changed();
                return;
            }
        };
        // SOCKS5 takes over TCP peers + HTTP trackers; no listen port, so UPnP has nothing to forward: hardcoded off.
        // Otherwise, listen only when seeding is enabled; UPnP is a separate opt-in.
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
            trackers,
            proxy.as_ref(),
        );
        let id = item.id();
        let generation = self.epoch.borrow().get(&id).cloned().unwrap_or(0) + 1;
        self.epoch.borrow_mut().insert(id, generation);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let is_file = crate::torrent::is_torrent_url(&magnet);
        let source = if is_file {
            match crate::torrent::archive_path_for_url(&magnet) {
                Some(path) => crate::torrent::TorrentSource::File(path),
                None => {
                    item.set_detail(gettext("Torrent file is missing from the archive"));
                    item.set_status(DownloadStatus::Failed);
                    self.changed();
                    return;
                }
            }
        } else {
            crate::torrent::TorrentSource::Magnet(magnet)
        };
        let only_files = crate::torrent::get_selection(&item.url().to_string());
        // Intake-recorded collision subfolders (file torrents) are already final; magnets use their recorded dir.
        let dest_is_final = !recorded.is_empty() && is_file;
        let handle = tokio_rt().spawn(crate::torrent::run_torrent(crate::torrent::TorrentJob {
            id,
            source,
            dest,
            seed_finished,
            seed_ratio: settings.torrent_seed_ratio(),
            seed_time_min: settings.torrent_seed_time(),
            dht: net.dht,
            peer_limit,
            download_bps,
            upload_bps,
            listen_port: net.listen_port,
            upnp: net.upnp,
            trackers: net.trackers,
            socks_proxy: net.socks_proxy,
            blocklist_url,
            lsd: net.lsd,
            only_files,
            dest_is_final,
            tx,
        }));
        self.running.borrow_mut().insert(id, handle);
        item.set_status(DownloadStatus::Downloading);
        // Attempts start indeterminate (see spawn): clear stale fractions through "Starting torrent…".
        item.set_progress(0.0);
        item.set_detail(gettext("Starting torrent…"));
        self.changed();
        self.pump(item, id, generation, rx);
    }
}

/// Video-page spawn inputs, bundled so the field list doesn't trip clippy's too-many-arguments lint.
struct SpawnVideoParams {
    item: DownloadItem,
    page_url: String,
    quality: String,
    audio_only: bool,
    video_format_id: Option<String>,
    is_live: bool,
    playlist_item_id: Option<String>,
}

impl DownloadManager {
    /// Spawn the resolver worker for a video-page row. Mirrors `spawn`'s contract; its abort sender lets pause/cancel stop extractor streams promptly.
    fn spawn_video(self: &Rc<Self>, params: SpawnVideoParams) {
        let SpawnVideoParams {
            item,
            page_url,
            quality,
            audio_only,
            video_format_id,
            is_live,
            playlist_item_id,
        } = params;
        let id = item.id();
        let generation = self.epoch.borrow().get(&id).cloned().unwrap_or(0) + 1;
        self.epoch.borrow_mut().insert(id, generation);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (abort_tx, abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
        self.gates.borrow_mut().insert(id, AttemptGate::new());
        let gate = self
            .gate_for(id)
            .expect("spawn just created the attempt gate");
        // Overwrite any stale sender: its task is dead or guarded stale.
        self.video_abort.borrow_mut().insert(id, abort_tx);
        // Live rows finalize in the worker on stop: track them so pause/cancel/park signal without aborting or presetting.
        if is_live {
            self.live_rows.borrow_mut().insert(id);
        } else {
            self.live_rows.borrow_mut().remove(&id);
        }
        let opts = DownloadOptions::from_settings(&self.settings);
        // Invalid manual proxy fails the row loudly before any network: never silently route direct.
        let proxy = match opts.proxy_config() {
            Ok(proxy) => proxy,
            Err(e) => {
                item.set_detail(e);
                item.set_status(DownloadStatus::Failed);
                self.changed();
                return;
            }
        };
        // Probe JSON is only reusable under the conditions it was probed with.
        let probe_json_key: ProbeJsonKey = (
            page_url.clone(),
            self.settings.cookies_browser(),
            proxy.as_ref().map(|p| p.cli_url.clone()),
        );
        let job = crate::video::VideoJob {
            item_id: id,
            page_url: page_url.clone(),
            playlist_item_id,
            quality,
            audio_only,
            dest: item.file_path(),
            // Shared throttle, parsed once here; empty/0/invalid means unlimited (the preferences row flags junk live).
            speed_limit: parse_rate(opts.limit_rate.as_str()),
            keep_server_date: self.settings.keep_server_date(),
            video_format_id,
            is_live,
            live_from_start: self.settings.live_from_start(),
            newest_codecs: self.settings.video_codec_newest(),
            cookies_browser: self.settings.cookies_browser(),
            // Audio-only rows never subtitle or remux (no video leg exists): resolve to `None` here rather than gating at every use.
            subtitles: if audio_only {
                None
            } else {
                crate::video_prefs::subtitle_lang_active(&self.settings.subtitle_language())
            },
            embed_subs: self.settings.embed_subs(),
            sponsorblock_remove: self.settings.sponsorblock_remove(),
            sponsorblock_mark: self.settings.sponsorblock_mark(),
            // Note the enqueue→spawn gap: a pref change on a queued row can disagree with the dialog-time row name on the extension.
            remux_video: if audio_only {
                None
            } else {
                crate::video_prefs::remux_video_active(&self.settings.remux_video())
            },
            embed_chapters: self.settings.embed_chapters(),
            proxy,
            // Take the probe's raw info JSON if cached (single videos only).
            // Lets the download use --load-info-json instead of re-extracting.
            info_json: self.take_probe_json(&probe_json_key),
        };
        let handle = tokio_rt().spawn(async move {
            let worker_tx = tx.clone();
            match crate::video::run_video_download(job, &gate, abort_rx, worker_tx).await {
                Ok(crate::video::VideoOutcome::Finished(size)) => {
                    tx.send(EngineMsg::Finished { size }).ok();
                }
                // Aborted: the pauser/canceller already set the row status, so send nothing and let the pump tail no-op.
                Ok(crate::video::VideoOutcome::Aborted) => {}
                Ok(crate::video::VideoOutcome::Expand(pl)) => {
                    // Handed to the pump: queueing needs the manager, which the worker task must never touch (Rc is main-thread only).
                    tx.send(EngineMsg::ExpandPlaylist(pl)).ok();
                }
                Err(e) => {
                    tracing::warn!(item_id = id, error = %e.to_string(), "video attempt failed");
                    tx.send(EngineMsg::Failed(e.to_string())).ok();
                }
            }
        });
        self.running.borrow_mut().insert(id, handle);
        item.set_status(DownloadStatus::Downloading);
        // A parked/paused/failed row keeps its fraction: the new attempt is a resume. Attempts start indeterminate (see spawn).
        let resuming = item.progress() > 0.0;
        item.set_progress(0.0);
        item.set_detail(if resuming {
            gettext("Resuming download…")
        } else if audio_only {
            gettext("Resolving audio…")
        } else {
            gettext("Resolving media…")
        });
        self.changed();
        self.pump(item, id, generation, rx);
    }

    /// Spawn a gallery download via gallery-dl.
    /// Runs on the tokio runtime (the runner needs tokio::process); progress
    /// is bridged to the glib main thread via a channel. The handle is
    /// registered in `running` for cancel/concurrency/shutdown.
    fn spawn_gallery(self: &Rc<Self>, item: DownloadItem) {
        item.set_status(DownloadStatus::Downloading);
        // Clear previous error log on (re)start.
        item.set_error_log(String::new());
        self.changed();

        // Get the job (carries the user's filter). Kept in the map for retry.
        let job = match self.gallery_jobs.borrow().get(&item.id()).cloned() {
            Some(j) => j,
            None => {
                // Detail before status: the status notify fires synchronously.
                item.set_detail("Gallery job not found".to_string());
                item.set_status(DownloadStatus::Failed);
                self.changed();
                self.start_next();
                return;
            }
        };

        let proxy = match DownloadOptions::from_settings(&self.settings).proxy_config() {
            Ok(proxy) => proxy,
            Err(e) => {
                item.set_detail(e);
                item.set_status(DownloadStatus::Failed);
                self.changed();
                self.start_next();
                return;
            }
        };

        // Channel for progress/finished messages (tokio -> glib).
        #[derive(Debug)]
        enum GalleryMsg {
            Progress(crate::gallery_runner::GalleryProgressUpdate),
            Finished(Result<crate::gallery_runner::GalleryResult, String>),
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<GalleryMsg>();

        // Spawn the download on the tokio runtime.
        // Cookies are resolved at spawn time (not enqueue time): the staged
        // file is ephemeral, so a path stored at enqueue would be stale on retry.
        let page_url = job.page_url.clone();
        let handle = tokio_rt().spawn(async move {
            let tool = match crate::gallery_argv::resolve_tool() {
                Ok(t) => t,
                Err(e) => {
                    let _ = tx.send(GalleryMsg::Finished(Err(e)));
                    return;
                }
            };
            // Stage cookies inside the task (async context).
            // Prefer the extension's staged cookies ("Download with Grab");
            // fall back to browser export. Same order as the probe.
            let handoff_path = crate::extension_server::live_handoff_cookie_path(
                &job.page_url,
                &job.cookies_browser,
            );
            let handoff_ua =
                crate::extension_server::handoff_user_agent(&job.page_url, &job.cookies_browser);
            let cookies_file: Option<crate::cookies::StagedCookies> = if handoff_path.is_some()
                || job.cookies_browser.is_empty()
                || job.cookies_browser == "none"
            {
                None
            } else if let Some(ytdlp) =
                crate::video_tools::find_in_dirs("yt-dlp", &crate::video_tools::tool_search_dirs())
            {
                // One deadline covers first-run Deno provisioning plus the
                // cookie-export subprocess.
                let browser = job.cookies_browser.clone();
                let page_url = job.page_url.clone();
                let deadline = tokio::time::Instant::now()
                    + std::time::Duration::from_secs(30);
                match tokio::time::timeout_at(deadline, async {
                    crate::video_tools::ensure_deno(&page_url).await?;
                    let remaining =
                        deadline.saturating_duration_since(tokio::time::Instant::now());
                    if remaining.is_zero() {
                        return Ok::<_, crate::video_tools::VideoError>(None);
                    }
                    Ok(crate::cookies::export_cookies_to_file(
                        &ytdlp, &browser, &page_url, remaining,
                    )
                    .await)
                })
                .await
                {
                    Ok(Ok(cookies)) => cookies,
                    Ok(Err(error)) => {
                        tracing::warn!(error = %error, "gallery browser-cookie export skipped after runtime setup failure");
                        None
                    }
                    Err(_) => {
                        tracing::warn!(
                            "gallery browser-cookie export exceeded its 30-second end-to-end deadline"
                        );
                        None
                    }
                }
            } else {
                None
            };
            let result = crate::gallery_runner::run_gallery_download_with_proxy(
                &job,
                &tool,
                handoff_path
                    .as_deref()
                    .or_else(|| cookies_file.as_ref().map(|c| c.path())),
                handoff_ua.as_deref(),
                proxy.as_ref(),
                |update| {
                    let _ = tx.send(GalleryMsg::Progress(update));
                },
            )
            .await;
            // `cookies_file` (StagedCookies) drops at block end, deleting the
            // file. Pause/cancel drops the task, so no leak.
            // Discard handoff cookies only on clean success (exit_code.is_none()).
            // Partial success (exit_code.is_some()) retains them for retry.
            if matches!(&result, Ok(r) if r.exit_code.is_none()) {
                crate::extension_server::discard_handoff_cookies(&page_url);
            }
            let _ = tx.send(GalleryMsg::Finished(result.map_err(|e| e.to_string())));
        });

        // Register for cancel/concurrency/shutdown.
        self.running.borrow_mut().insert(item.id(), handle);

        // Glib-side: recv messages and update the UI.
        let this = self.clone();
        let item_c = item.clone();
        let total = item.gallery_file_count();
        gtk4::glib::spawn_future_local(async move {
            while let Some(msg) = rx.recv().await {
                match msg {
                    GalleryMsg::Progress(update) => {
                        let done = update.files_completed;
                        item_c.set_gallery_files_done(done);
                        // Show the last completed filename if available.
                        let file_part = update
                            .last_completed_file
                            .as_ref()
                            .and_then(|p| std::path::Path::new(p).file_name()?.to_str())
                            .map(|n| format!(" • {n}"))
                            .unwrap_or_default();
                        if total > 0 && done <= total {
                            item_c.set_progress((done as f64 / total as f64).min(1.0));
                            item_c.set_detail(format!(
                                "Gallery \u{2022} {done}/{total} files{file_part}"
                            ));
                        } else {
                            // Total unknown or the probe count was wrong (done >
                            // total): reset to indeterminate instead of keeping
                            // a stale fraction. progress <= 0 pulses the bar
                            // (should_pulse) and the a11y label reads 0%.
                            item_c.set_progress(0.0);
                            item_c.set_detail(format!("Gallery \u{2022} {done} files{file_part}"));
                        }
                        this.changed();
                    }
                    GalleryMsg::Finished(result) => {
                        // Remove from running registry.
                        this.running.borrow_mut().remove(&item_c.id());
                        match result {
                            Ok(r) => {
                                // Set detail BEFORE status: the status notify
                                // fires synchronously and opens the dialog,
                                // which reads the detail (same as Err below).
                                let done =
                                    crate::gallery_runner::done_count(r.files.len(), r.skipped);
                                let detail = if r.exit_code.is_some() {
                                    format!(
                                        "{} \u{2022} some failed",
                                        crate::gallery_runner::done_label(r.files.len(), r.skipped)
                                    )
                                } else {
                                    crate::gallery_runner::done_label(r.files.len(), r.skipped)
                                };
                                item_c.set_gallery_files_done(done);
                                item_c.set_progress(1.0);
                                item_c.set_detail(detail);
                                // Partial failure (exit_code.is_some()) is marked
                                // Failed so it's retryable via retry_failed().
                                // Clean success is Done.
                                if r.exit_code.is_some() {
                                    item_c.set_status(DownloadStatus::Failed);
                                } else {
                                    item_c.set_status(DownloadStatus::Done);
                                }
                                // One finished record per URL (Parabolic parity):
                                // done here (not at enqueue) so a failed
                                // re-download keeps the earlier good record.
                                this.drop_finished_duplicates(&item_c.gallery_url(), item_c.id());
                                tracing::info!(
                                    files = r.files.len(),
                                    skipped = r.skipped,
                                    "gallery download complete"
                                );
                            }
                            Err(e) => {
                                // Set detail/error_log BEFORE status: the status
                                // notify fires synchronously and opens the dialog,
                                // which reads these fields.
                                let summary = e.lines().next().unwrap_or(&e).trim();
                                item_c.set_detail(format!("Gallery failed: {summary}"));
                                item_c.set_error_log(e.clone());
                                item_c.set_status(DownloadStatus::Failed);
                                tracing::error!(error = %e, "gallery download failed");
                            }
                        }
                        this.changed();
                        this.persist_queue();
                        this.start_next();
                        break;
                    }
                }
            }
        });
    }

    fn notify_finished(&self, item: &DownloadItem, result: Result<(), String>) {
        if !self.notifications_enabled() {
            return;
        }
        if let Some(app) = gio::Application::default() {
            let ok = result.is_ok();
            let n = gio::Notification::new(&if ok {
                gettext("Download finished")
            } else {
                gettext("Download failed")
            });
            let mut body = if ok {
                item.filename().to_string()
            } else {
                format!(
                    "{} → {}",
                    item.filename(),
                    item.file_path().to_string_lossy()
                )
            };
            if let Err(h) = result {
                body.push_str(&format!("\n{h}"));
            }
            n.set_body(Some(&body));
            n.set_default_action_and_target_value("app.present", None);
            app.send_notification(Some(&format!("dl-{}", item.id())), &n);
        }
    }

    /// Pause a running download, freeing its slot for the next queued item.
    pub fn pause(self: &Rc<Self>, id: u64) {
        // Live captures finalize in the worker; the pump tail frees the slot on completion.
        if !self.stop_engine(id) {
            return;
        }
        if let Some(item) = self.find(id) {
            if crate::torrent::is_torrent(&item.url()) {
                crate::torrent::pause_download(id);
            }
            if item.status() == DownloadStatus::Downloading {
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

    /// Send a downloading/paused row to the back of the queue, keeping progress and partial file.
    pub fn defer(self: &Rc<Self>, id: u64) {
        let Some(item) = self.find(id) else {
            return;
        };
        if !matches!(
            item.status(),
            DownloadStatus::Downloading | DownloadStatus::Paused
        ) {
            return;
        }
        self.park(id);
        self.move_to_back(id);
        self.persist_queue();
        self.changed();
        self.start_next();
    }

    /// Signal the worker for `id` to stop, with the intent that decides what stopping means. Only the worker knows whether it is capturing live.
    fn stop_video_worker(&self, id: u64, intent: crate::video::StopIntent) {
        if let Some(stop) = self.video_abort.borrow_mut().remove(&id) {
            let _ = stop.send(intent);
        }
    }

    /// Stop the engine for `id` without touching the row. Live rows only get signalled (the worker finalizes and its Finished drives the row); returns whether the engine was actually stopped.
    fn stop_engine(&self, id: u64) -> bool {
        self.stop_video_worker(id, crate::video::StopIntent::Preserve);
        if self.live_rows.borrow().contains(&id) {
            return false;
        }
        if let Some(handle) = self.running.borrow().get(&id) {
            handle.abort();
        }
        self.running.borrow_mut().remove(&id);
        true
    }

    /// Stop the engine for `id`, keeping file, bitmap and progress, and mark it queued. Live rows only signal (see `stop_engine`).
    fn park(&self, id: u64) {
        // Live rows only signal: the worker finalizes and its message completes the row.
        if !self.stop_engine(id) {
            return;
        }
        if let Some(item) = self.find(id) {
            if crate::torrent::is_torrent(&item.url()) {
                crate::torrent::pause_download(id);
            }
            if matches!(
                item.status(),
                DownloadStatus::Downloading | DownloadStatus::Paused
            ) {
                item.set_status(DownloadStatus::Queued);
                item.set_detail(item.status().label());
            }
        }
    }

    fn move_to_back(&self, id: u64) {
        if let Some(pos) = self.position_of(id)
            && let Some(obj) = self.store.item(pos)
        {
            self.store.remove(pos);
            self.store.append(&obj);
        }
    }

    /// Park running rows past the shrunk limit; lowest ids keep their slots.
    fn preempt_excess(&self) {
        let max = self.max_concurrent();
        let mut ids: Vec<u64> = self.running.borrow().keys().cloned().collect();
        if ids.len() <= max {
            return;
        }
        ids.sort_unstable();
        for id in ids.into_iter().skip(max) {
            self.park(id);
        }
        self.persist_queue();
        self.changed();
    }

    /// Cancel a download; retry with [`DownloadManager::retry`].
    pub fn cancel(self: &Rc<Self>, id: u64) {
        if let Some(path) = self.cancel_inner(id, false, Stop::Preserve) {
            self.schedule_partial_cleanup(path);
        }
        self.persist_queue();
        self.changed();
        self.start_next();
    }

    /// Reclaim a removed video row's scratch, but only once its worker has actually stopped. Removal cannot sweep inline: a worker mid-teardown can recreate what a sweep removed, so unlinking first just loses the race. Claiming the gate first makes deferring safe: only a worker this finalizer awaited may have committed an orphan `dest` file.
    /// Engine invariant: a finished file is never deleted. The `remove_file` below is the single exception — a commit that won the race against its row's discard, leaving an orphan no row can own.
    fn finish_discard(&self, id: u64, dest: std::path::PathBuf, gate: std::sync::Arc<AttemptGate>) {
        let _ = gate.discard();
        // Staging files live visibly in the dest dir as `grab-<id>-*`; no subfolder.
        // `self` (Rc, !Send) cannot go to the tokio runtime: carry reservations as an `Arc` clone.
        // Queue wakeups go through `wake_queue()` (thread-safe, no manager handle needed).
        let reservations = std::sync::Arc::clone(&self.reservations);
        match self.running.borrow_mut().remove(&id) {
            Some(handle) => {
                let worker_abort = handle.abort_handle();
                let finalizer = crate::runtime::tokio_rt().spawn(async move {
                    let _ = handle.await;
                    if let Some(dest_dir) = dest.parent() {
                        crate::video::clean_staging_files(dest_dir, id);
                    }
                    crate::video::clean_dest_parts(&dest);
                    if gate.was_delivered() {
                        // The commit won the race, so this file is the attempt's own orphan and the row is gone: the one sanctioned exception to never deleting a finished file.
                        let _ = std::fs::remove_file(&dest);
                    }
                    if let Ok(mut r) = reservations.lock() {
                        r.remove(&dest);
                    }
                    // The destination is free: wake the queue on the main
                    // thread so a row parked by `unremove()` starts now
                    // instead of waiting for an unrelated `start_next()`.
                    wake_queue();
                });
                // Retain the worker's abort beside the finalizer: aborting the finalizer would detach the worker instead of stopping it.
                self.discards.borrow_mut().insert(
                    id,
                    PendingDiscard {
                        worker_abort,
                        finalizer,
                    },
                );
            }
            // No task to wait for: sweep the scratch, never the finished file (no orphan is possible without a worker in flight).
            None => {
                if let Some(dest_dir) = dest.parent() {
                    crate::video::clean_staging_files(dest_dir, id);
                }
                crate::video::clean_dest_parts(&dest);
                self.release_dest(&dest);
                // Same wakeup as the async finalizer above; already on the main thread.
                wake_queue();
            }
        }
    }

    fn cancel_inner(&self, id: u64, keep_partial: bool, stop: Stop) -> Option<std::path::PathBuf> {
        // Live rows keep what's recorded (Stop, not Cancel): signal the worker and skip the abort plus the Cancelled preset, so its Finished still lands. Removal is the one caller that discards.
        let live = self.live_rows.borrow().contains(&id);
        // Only a *video* worker can be told to discard: plain rows have no `video_abort` sender, so a Discard-only path would leak the engine and its slot.
        let video = self.video_sources.borrow().contains_key(&id);
        match stop {
            Stop::Discard if video => {
                self.stop_video_worker(id, crate::video::StopIntent::Discard);
                // The pump tail early-returns once `remove` drops the epoch entry, so clear the marker here or a later row reusing the id takes live paths.
                self.live_rows.borrow_mut().remove(&id);
            }
            // Preserve and non-video Discard stop the engine the same way (see `stop_engine`).
            _ => {
                self.stop_engine(id);
            }
        }
        self.pending_names.borrow_mut().remove(&id);
        let had_segments = self.segment_state.borrow_mut().remove(&id).is_some();
        self.torrent_pieces.borrow_mut().remove(&id);
        let mut partial_to_remove = None;
        if let Some(item) = self.find(id) {
            // Segmented partials have holes: without the bitmap cancel drops the file — unless the row may come back via Undo.
            // Return the path so callers can reserve it and unlink off the GTK thread.
            if had_segments && !keep_partial {
                partial_to_remove = Some(item.file_path());
            }
            // Unfinished torrent rows drop their session entry but keep partial files, so cancel/retry and remove/Undo resume.
            if crate::torrent::is_torrent(&item.url()) && item.status() != DownloadStatus::Done {
                crate::torrent::forget_download(id);
            }
            // A removed row's status is moot, and a live one must keep Downloading so its Finished still lands.
            // A finished row is terminal: no command revives it, so Preserve must not clobber Done —
            // otherwise retry() would revive it in place with a stopped pulse timer.
            if stop == Stop::Preserve && !live && item.status() != DownloadStatus::Done {
                item.set_status(DownloadStatus::Cancelled);
                item.set_detail(item.status().label());
            }
        }
        partial_to_remove
    }

    /// Re-queue a failed or cancelled download.
    pub fn retry(self: &Rc<Self>, id: u64) {
        if let Some(item) = self.find(id) {
            match item.status() {
                DownloadStatus::Failed | DownloadStatus::Cancelled => {
                    // A kept segment bitmap resumes where it left off, so leave the progress bar there.
                    if !self.segment_state.borrow().contains_key(&id) {
                        item.set_progress(0.0);
                    }
                    // Gallery downloads: gallery-dl skips existing files on retry
                    // (file-level resume). Reset the UI counter; the runner
                    // reports newly completed files via progress updates.
                    if item.is_gallery() {
                        item.set_gallery_files_done(0);
                        item.set_progress(0.0);
                    }
                    item.set_detail(String::new());
                    item.set_status(DownloadStatus::Queued);
                    self.persist_queue();
                    self.changed();
                    self.start_next();
                }
                _ => {}
            }
        }
    }

    /// Cancel and drop a row; restore with [`DownloadManager::unremove`]. The partial file is kept so Undo can resume.
    pub fn remove(self: &Rc<Self>, id: u64) {
        let _ = self.cancel_inner(id, true, Stop::Discard);
        self.epoch.borrow_mut().remove(&id);
        self.started.borrow_mut().remove(&id);
        // Video rows defer the sweep to `finish_discard`, which waits for the worker: a finalize path still inside its rename would otherwise deliver a file for a row that no longer exists.
        if self.video_sources.borrow().contains_key(&id) {
            let dest = self.find(id).map(|i| i.file_path()).unwrap_or_default();
            // Reserve first: between here and the finalizer's sweep a new
            // row (or Undo) must not claim this destination, or the
            // stem-wide sweep would delete the new row's files.
            self.reserve_dest(&dest);
            // Spawned attempts always have a gate; a never-spawned row gets a fresh one so the reclaim path stays uniform.
            let gate = self
                .gate_for(id)
                .unwrap_or_else(crate::video::AttemptGate::new);
            self.gates.borrow_mut().remove(&id);
            self.finish_discard(id, dest, gate);
        }
        // The snapshot carries the source for Undo; the live map drops it
        // with the row (cancel keeps it, remove doesn't).
        self.video_sources.borrow_mut().remove(&id);
        // Gallery: discard handoff cookies on removal (failed rows keep them
        // for retry, but removal means the user is done with this URL).
        if let Some(job) = self.gallery_jobs.borrow().get(&id) {
            crate::extension_server::discard_handoff_cookies(&job.page_url);
        }
        self.gallery_jobs.borrow_mut().remove(&id);
        let released = self.find(id).filter(|item| item.public_only());
        if let Some(pos) = self.position_of(id) {
            self.store.remove(pos);
        }
        if let Some(item) = released {
            Self::release_public_policy(&item);
        }
        self.persist_queue();
        self.changed();
        self.start_next();
    }

    /// Segment bitmap snapshot for Undo: cloned before [`DownloadManager::remove`] drops the row's live bitmap.
    pub fn segments_of(&self, id: u64) -> Option<SegmentState> {
        self.segment_state.borrow().get(&id).cloned()
    }

    /// Whether the engine ever started this row (for Undo snapshots, which
    /// are built outside this module).
    pub(crate) fn has_started(&self, id: u64) -> bool {
        self.started.borrow().contains(&id)
    }

    /// Re-insert a previously removed download (Undo). Restored `Downloading` restarts as `Queued`; a restored bitmap resumes, a stale one is dropped at spawn.
    pub fn unremove(self: &Rc<Self>, snap: RemovedSnapshot) -> DownloadItem {
        let item = DownloadItem::new(
            self.alloc_id_for(std::path::Path::new(&snap.dest_dir)),
            &snap.url,
            &snap.filename,
            &snap.dest_dir,
        );
        item.set_progress(snap.progress.clamp(0.0, 1.0));
        item.set_detail(snap.detail);
        item.set_status(match snap.status {
            DownloadStatus::Downloading => DownloadStatus::Queued,
            s => s,
        });
        item.set_output_dir(snap.output_dir);
        item.set_public_only(snap.public_only);
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
            return item;
        }
        self.insert(item.clone());
        item
    }

    /// Engine's real output folder for a torrent row: recorded at enqueue, else recomputed from the archive, else the row's own path.
    /// The recomputed leaf goes through the same guard as the write path; the
    /// recorded branch refuses a path that is itself a symlink. Either way, a
    /// symlink planted after the download finished can't divert the
    /// delete/trash onto its target.
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
    fn gallery_trash_target(
        item: &DownloadItem,
        job: &crate::gallery_argv::GalleryJob,
    ) -> Option<std::path::PathBuf> {
        let dest_dir = item.dest_dir();
        let parent = std::path::Path::new(&dest_dir);
        let filename = item.filename();
        // All three must hold: a distinct subfolder, directly inside the
        // row's dest_dir, named exactly the row's filename. This rules out
        // legacy rows (job points at the parent itself), `..` escapes, and
        // records that drifted to a different folder.
        if job.dest_dir.as_path() == parent {
            return None;
        }
        if job.dest_dir.parent() != Some(parent) {
            return None;
        }
        if job.dest_dir.file_name() != Some(std::ffi::OsStr::new(&filename)) {
            return None;
        }
        Some(job.dest_dir.clone())
    }

    /// Prepare immutable inputs for deletion while the model is on GTK.
    /// Directory checks and torrent archive parsing are deferred to the worker.
    fn prepare_delete_operation(&self, id: u64, item: &DownloadItem) -> DeleteOperation {
        let filename = item.filename().to_string();
        if item.is_gallery() {
            let target = self
                .gallery_jobs
                .borrow()
                .get(&id)
                .and_then(|job| Self::gallery_trash_target(item, job));
            // Legacy gallery rows without an ownership proof still remove
            // only their queue row; they never trash the shared parent.
            return DeleteOperation::Gallery { target, filename };
        }

        let url = item.url().to_string();
        if crate::torrent::is_torrent(&url) {
            return DeleteOperation::Torrent {
                url,
                dest_dir: std::path::PathBuf::from(item.dest_dir().to_string()),
                output_dir: item.output_dir().to_string(),
                filename,
            };
        }

        let path = item.file_path();
        let sidecars = if self.video_sources.borrow().contains_key(&id) {
            crate::video_prefs::subtitle_content_languages()
                .map(|lang| crate::video_staging::sidecar_path_for(&path, lang))
                .collect()
        } else {
            Vec::new()
        };
        DeleteOperation::File {
            path,
            sidecars,
            filename,
        }
    }

    /// Move downloaded files to Trash on a blocking worker, then update the
    /// queue model on GTK. Errors are display-ready.
    pub async fn delete_download_async(self: &Rc<Self>, id: u64) -> Result<(), String> {
        let item = self.find(id).ok_or_else(|| gettext("Download not found"))?;
        let operation = self.prepare_delete_operation(id, &item);
        let torrent_url = match &operation {
            DeleteOperation::Torrent { url, .. } => Some(url.clone()),
            _ => None,
        };
        let filename = item.filename().to_string();

        gio::spawn_blocking(move || execute_delete_operation(operation))
            .await
            .map_err(|_| {
                format!("Could not move {filename} to Trash: background task panicked")
            })??;

        // This only schedules teardown on Tokio; retain the original ordering
        // and do it only after trash/archive operations have succeeded.
        if torrent_url.is_some() {
            crate::torrent::forget_download(id);
        }
        self.remove(id);
        Ok(())
    }

    /// Cancel every queued, downloading or paused item.
    pub fn cancel_all(self: &Rc<Self>) {
        // One persist at the end and no per-row start_next: cancel() would briefly spawn the next queued row only to cancel it right after.
        let mut partials_to_remove = Vec::new();
        self.for_matching(
            |s| {
                matches!(
                    s,
                    DownloadStatus::Queued | DownloadStatus::Downloading | DownloadStatus::Paused
                )
            },
            |m, id| {
                if let Some(path) = m.cancel_inner(id, false, Stop::Preserve) {
                    partials_to_remove.push(path);
                }
            },
        );
        for path in partials_to_remove {
            self.schedule_partial_cleanup(path);
        }
        self.persist_queue();
        self.changed();
        self.start_next();
    }

    /// Retry every failed/cancelled row. Returns the retried count for
    /// the caller's toast.
    pub fn retry_failed(self: &Rc<Self>) -> usize {
        let mut n = 0;
        self.for_matching(
            |s| matches!(s, DownloadStatus::Failed | DownloadStatus::Cancelled),
            |m, id| {
                m.retry(id);
                n += 1;
            },
        );
        n
    }

    /// Drop every finished row (files stay on disk). Still-seeding torrents leave the session first. Returns the cleared count.
    pub fn clear_finished(self: &Rc<Self>) -> usize {
        let before = self.store.n_items();
        self.for_matching(
            |s| matches!(s, DownloadStatus::Done),
            |m, id| m.drop_finished_row(id),
        );
        let n = (before - self.store.n_items()) as usize;
        if n > 0 {
            // Same hygiene as a restart: drop archives and selections no remaining row references.
            let referenced: std::collections::HashSet<String> = self
                .items()
                .map(|it| it.url().to_string())
                .filter(|u| crate::torrent::is_torrent_url(u))
                .collect();
            crate::torrent::sweep_archives(&referenced);
            crate::torrent::prune_selections(&referenced);
            self.persist_queue();
            self.changed();
        }
        n
    }

    /// Finished rows (for the clear-finished confirm and toast counts).
    pub fn finished_count(&self) -> usize {
        self.items()
            .filter(|it| it.status() == DownloadStatus::Done)
            .count()
    }

    /// Snapshots of every finished row, in list order, for Clear Finished's Undo.
    pub fn finished_snapshots(&self) -> Vec<RemovedSnapshot> {
        self.items()
            .filter(|it| it.status() == DownloadStatus::Done)
            .map(|it| RemovedSnapshot {
                url: it.url().to_string(),
                dest_dir: it.dest_dir().to_string(),
                filename: it.filename().to_string(),
                status: it.status(),
                progress: it.progress(),
                detail: it.detail().to_string(),
                output_dir: it.output_dir().to_string(),
                segments: self.segments_of(it.id()),
                video_source: self.video_source(it.id()),
                started: self.has_started(it.id()),
                public_only: it.public_only(),
            })
            .collect()
    }

    /// Drop finished rows for `url` other than `keep_id`: one finished record per URL (Parabolic parity). Store-only; callers persist.
    pub(crate) fn drop_finished_duplicates(&self, url: &str, keep_id: u64) {
        let Ok(key) = normalize_url(url) else {
            return;
        };
        let ids: Vec<u64> = self
            .items()
            .filter(|it| {
                it.status() == DownloadStatus::Done
                    && it.id() != keep_id
                    && normalize_url(&it.url()).is_ok_and(|u| u == key)
            })
            .map(|it| it.id())
            .collect();
        for id in ids {
            self.drop_finished_row(id);
        }
    }

    /// Drop one finished row: files stay on disk. Unlike `remove` there is nothing to cancel, snapshot or clean.
    fn drop_finished_row(&self, id: u64) {
        if let Some(item) = self.find(id)
            && crate::torrent::is_torrent(&item.url())
        {
            crate::torrent::forget_download(id);
        }
        self.video_sources.borrow_mut().remove(&id);
        self.gallery_jobs.borrow_mut().remove(&id);
        self.epoch.borrow_mut().remove(&id);
        let released = self.find(id).filter(|item| item.public_only());
        if let Some(pos) = self.position_of(id) {
            self.store.remove(pos);
        }
        if let Some(item) = released {
            Self::release_public_policy(&item);
        }
    }

    fn for_matching(
        self: &Rc<Self>,
        matches: impl Fn(DownloadStatus) -> bool,
        mut op: impl FnMut(&Rc<Self>, u64),
    ) {
        let ids: Vec<u64> = self
            .items()
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
        self.items()
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
        self.items().any(|it| pred(it.status()))
    }

    fn queue_file() -> std::path::PathBuf {
        // Test seam only: release builds always use the real location, so a crafted launcher environment can never redirect queue state.
        if cfg!(debug_assertions)
            && let Some(p) = std::env::var_os("GRAB_QUEUE_FILE")
        {
            return std::path::PathBuf::from(p);
        }
        let mut dir = glib::user_data_dir();
        dir.push("grab");
        dir.join("queue.json")
    }

    /// Remove stale `queue.json.tmp.*` files left by crashed writes.
    fn sweep_stale_tmp_at(qf: &std::path::Path) {
        let Some(dir) = qf.parent() else { return };
        let Some(name) = qf.file_name().and_then(|n| n.to_str()) else {
            return;
        };
        let prefix = format!("{name}.tmp.");
        // This process's own temp files (`<prefix><pid>.<n>`) may be mid-write.
        let own = format!("{prefix}{}.", std::process::id());
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                if entry
                    .file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with(&prefix) && !n.starts_with(&own))
                {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }

    /// Move a broken queue file aside so the next persist starts fresh.
    /// Uses the first free `queue.json.bak`, `queue.json.bak.1`, ... slot —
    /// never clobbers a previous backup, never deletes.
    fn quarantine_queue_at(queue_file: &std::path::Path) {
        let base = queue_file.with_extension("json.bak");
        let mut target = base.clone();
        let mut n = 0;
        while target.exists() {
            n += 1;
            target = queue_file.with_extension(format!("json.bak.{n}"));
            if n > 99 {
                tracing::error!("too many queue backups; keeping broken queue in place");
                return;
            }
        }
        if let Err(e) = std::fs::rename(queue_file, &target) {
            tracing::error!("could not quarantine download queue: {e}");
        }
    }

    fn persist_queue(&self) {
        // Avoid overwriting the saved queue with an incomplete in-memory model
        // while the asynchronous startup restore is still reading it.
        if self.restore_state.get() != RestoreState::Ready {
            self.restore_dirty.set(true);
            return;
        }
        // `watch::send_replace` keeps only the latest value: rapid state changes
        // coalesce to a single write by the persist task.
        // Tests need determinism: they persist then immediately restore in a
        // new manager, so write synchronously in test builds.
        #[cfg(test)]
        if let Some(text) = self.serialize_queue() {
            Self::write_queue_file(&text);
        }
        #[cfg(not(test))]
        if let (Some(queue), Some(tx)) = (
            self.snapshot_queue_inner(false),
            &*self.persist_tx.borrow(),
        ) {
            tx.send_replace(Some(queue));
        }
    }

    /// The persist task body: watches for new queue states and writes them.
    /// Extracted for testing: `watch::changed()` delivers the final value before
    /// the closed-channel Err, so dropping the sender after a final send guarantees
    /// the last write lands.
    pub(crate) async fn persist_task_loop<T>(
        mut rx: tokio::sync::watch::Receiver<Option<T>>,
        write: impl Fn(T) + Clone + Send + 'static,
    )
    where
        T: Clone + Send + 'static,
    {
        while rx.changed().await.is_ok() {
            let Some(value) = rx.borrow_and_update().clone() else {
                continue;
            };
            // Queue serialization and disk I/O must not block GTK or the async runtime.
            let write = write.clone();
            let _ = tokio::task::spawn_blocking(move || write(value)).await;
        }
    }

    /// Final persist for shutdown: send the state, drop the sender, await the
    /// task. `watch::changed()` delivers the final value before the
    /// closed-channel Err, so the last write lands before we return.
    /// Tests write synchronously (no channel/task in test builds).
    fn persist_for_shutdown(&self) {
        #[cfg(test)]
        if self.restore_state.get() == RestoreState::Ready
            && let Some(text) = self.serialize_queue_inner(true)
        {
            Self::write_queue_file(&text);
        }
        #[cfg(not(test))]
        {
            if self.restore_state.get() != RestoreState::Ready {
                tracing::warn!(
                    "shutdown: queue restore did not complete; preserving the saved queue file"
                );
            } else if let (Some(queue), Some(tx)) = (
                self.snapshot_queue_inner(true),
                &*self.persist_tx.borrow(),
            ) {
                tx.send_replace(Some(queue));
            }
            drop(self.persist_tx.borrow_mut().take());
            // This is the synchronous fallback reached only when the normal
            // close/Quit path could not run its async shutdown. Detach the writer
            // instead of blocking GTK; the ordinary path awaits it in
            // persist_for_shutdown_async before the application quits.
            drop(self.persist_task.borrow_mut().take());
        }
    }

    /// Build a thread-safe snapshot from GTK-owned rows. The expensive JSON
    /// encoding is done by the background persistence worker.
    fn snapshot_queue_inner(&self, force: bool) -> Option<StoredQueue> {
        if !force && self.batch.get() > 0 {
            return None;
        }
        let mut items = Vec::new();
        for it in self.items() {
            // Cancelled rows carry no intent: never persisted.
            if it.status() != DownloadStatus::Cancelled {
                // Gallery rows need their job to restore; a missing job is a
                // bug — skip the row rather than resurrecting it as a plain
                // direct download (the old phase-11b hazard).
                let gallery = if it.is_gallery() {
                    match self.gallery_jobs.borrow().get(&it.id()) {
                        Some(job) => match serde_json::to_value(job) {
                            Ok(v) => Some(v),
                            Err(e) => {
                                tracing::warn!("skipping gallery row: job serialize failed: {e}");
                                None
                            }
                        },
                        None => {
                            tracing::warn!("skipping gallery row with no job entry");
                            None
                        }
                    }
                } else {
                    None
                };
                // Skip gallery rows that failed to serialize their job.
                if it.is_gallery() && gallery.is_none() {
                    continue;
                }
                let segments = self.segment_state.borrow().get(&it.id()).cloned();
                let selected_files = crate::torrent::get_selection(&it.url().to_string());
                // Empty means "no folder tracked": omit it so old files stay clean and old app versions keep reading new ones.
                let output_dir = it.output_dir().to_string();
                let output_dir = (!output_dir.is_empty()).then_some(output_dir);
                let video_source = self
                    .video_source(it.id())
                    .filter(|s| matches!(s, crate::media_types::VideoSource::Page { .. }));
                items.push(StoredItem {
                    id: Some(it.id()),
                    url: it.url().to_string(),
                    dest_dir: it.dest_dir().to_string(),
                    filename: it.filename().to_string(),
                    status: it.status(),
                    progress: it.progress(),
                    segments,
                    selected_files,
                    output_dir,
                    video_source,
                    started: Some(self.started.borrow().contains(&it.id())),
                    scheduled_at: {
                        let ts = it.scheduled_at();
                        (ts > 0).then_some(ts)
                    },
                    etag: {
                        let e = it.etag();
                        (!e.is_empty()).then_some(e.to_string())
                    },
                    last_modified: {
                        let lm = it.last_modified();
                        (!lm.is_empty()).then_some(lm.to_string())
                    },
                    public_only: Some(it.public_only()),
                    gallery,
                });
            }
        }
        Some(StoredQueue {
            version: QUEUE_VERSION,
            items,
        })
    }

    /// Test helper for assertions over the serialized queue representation.
    #[cfg(test)]
    fn serialize_queue(&self) -> Option<String> {
        self.serialize_queue_inner(false)
    }

    /// Synchronous serialization is used only by deterministic test persistence.
    #[cfg(test)]
    fn serialize_queue_inner(&self, force: bool) -> Option<String> {
        let queue = self.snapshot_queue_inner(force)?;
        Self::serialize_snapshot(queue)
    }

    fn serialize_snapshot(queue: StoredQueue) -> Option<String> {
        match serde_json::to_string(&queue) {
            Ok(text) => Some(text),
            Err(error) => {
                tracing::error!("could not serialize download queue: {error}");
                None
            }
        }
    }


    /// Write the serialized queue atomically: temp file + sync_all + rename +
    /// dir sync. The persist task is the sole writer, so writes are ordered by
    /// construction. Runs via `spawn_blocking`, never on the async runtime or
    /// the UI thread.
    fn write_queue_file(text: &str) {
        // Resolve and prepare the parent here: this function runs on the writer
        // task (or synchronously in unit tests), not from the normal GTK path.
        let queue_file = Self::queue_file();
        if let Some(parent) = queue_file.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            tracing::error!("could not create download queue directory: {e}");
            return;
        }
        // Temp name is unique per write (PID + monotonic counter): a pre-existing
        // symlink at this path cannot be predicted, and `create_new` refuses to
        // follow symlinks or overwrite an existing file.
        let tmp = queue_file.with_extension(format!(
            "json.tmp.{}.{}",
            std::process::id(),
            QUEUE_TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let write_tmp = || -> std::io::Result<()> {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt as _;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                // URLs can carry signed tokens: restrict to owner.
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            Ok(())
        };
        match write_tmp() {
            Ok(()) => {
                if let Err(e) = std::fs::rename(&tmp, &queue_file) {
                    tracing::error!("could not replace download queue: {e}");
                    let _ = std::fs::remove_file(&tmp);
                    return;
                }
                if let Some(parent) = queue_file.parent()
                    && let Ok(dir) = std::fs::File::open(parent)
                {
                    let _ = dir.sync_all();
                }
            }
            Err(e) => tracing::error!("could not persist download queue: {e}"),
        }
    }

    /// Validate one persisted queue entry for restore. Invalid entries warn-skip; valid ones collect for the apply phase. Pure: never inserts or spawns.
    fn validate_stored_item(item: StoredItem) -> Option<PendingRestore> {
        // The recorded engine folder is only trusted when it sits directly inside the row's own dest.
        let output_dir = item.output_dir.clone().filter(|dir| {
            let folder = std::path::PathBuf::from(dir);
            folder.is_absolute()
                && folder
                    .parent()
                    .is_some_and(|p| p == std::path::Path::new(&item.dest_dir))
        });
        // Mirror restore_existing's cheap validations now, so the apply phase below cannot fail partway and strand engines.
        if normalize_url(&item.url).is_err() {
            tracing::warn!("skipping queue entry with bad URL");
            return None;
        }
        if !sane_filename(&item.filename) {
            tracing::warn!(
                "skipping queue entry: Invalid filename in queue: {}",
                item.filename
            );
            return None;
        }
        if !std::path::Path::new(&item.dest_dir).is_absolute() {
            tracing::warn!(
                "skipping queue entry: Invalid destination in queue: {}",
                item.dest_dir
            );
            return None;
        }
        // A stored bitmap resumes segmented; misshapen ones are dropped (single-stream fallback stays correct).
        let segments = match &item.segments {
            Some(s)
                if s.total > 0
                    && s.total <= MAX_SEGMENTED_TOTAL
                    && s.done.len() == s.total.div_ceil(piece_len(s.total)) as usize =>
            {
                Some(s.clone())
            }
            _ => None,
        };
        // Gallery payload: parse per-entry so one bad shape can't quarantine
        // the whole queue. Unparseable → skip the entry, never fall through
        // to a plain row.
        let gallery_job = match &item.gallery {
            Some(v) => match serde_json::from_value::<crate::gallery_argv::GalleryJob>(v.clone()) {
                Ok(mut job) => {
                    // Overwrite identity from the stored item, not the payload.
                    job.page_url = item.url.clone();
                    // The payload's dest_dir is the gallery subfolder enqueue
                    // stored: parent + row filename — the only record of the
                    // uniquifying -2 suffix. A payload matching that shape is
                    // trustworthy; anything else (legacy rows, drifted
                    // records) is rebuilt from the validated filename.
                    let folder = std::path::Path::new(&item.dest_dir).join(&item.filename);
                    if job.dest_dir != folder {
                        job.dest_dir = folder;
                    }
                    Some(job)
                }
                Err(e) => {
                    tracing::warn!("skipping gallery entry: unparseable job: {e}");
                    return None;
                }
            },
            None => {
                // Legacy: pre-B4 rows stored `backend: "gallerydl"` on the
                // video source. Convert to a gallery job (filter All, no count).
                if let Some(crate::media_types::VideoSource::Page {
                    page_url,
                    backend: Some(backend),
                    ..
                }) = &item.video_source
                    && backend == "gallerydl"
                {
                    tracing::info!("converting legacy gallerydl row to gallery");
                    Some(crate::gallery_argv::GalleryJob {
                        item_id: 0, // overwritten on restore
                        page_url: page_url.clone(),
                        title: item.filename.clone(),
                        file_count: None,
                        dest_dir: std::path::PathBuf::from(&item.dest_dir),
                        filter: crate::gallery_argv::GalleryFilter::All,
                        cookies_browser: "none".to_string(),
                        speed_limit: None,
                        range: None,
                    })
                } else {
                    None
                }
            }
        };
        Some(PendingRestore {
            item,
            output_dir,
            segments,
            gallery_job,
        })
    }

    /// Read and validate the saved queue away from GTK's main context.
    /// Only ListStore/model mutation is applied on the main context; disk sweeps
    /// are sent to Tokio's blocking pool before restore becomes Ready.
    pub fn restore_queue(self: &Rc<Self>) {
        let queue_file = Self::queue_file();
        self.restore_ready.send_replace(false);
        self.restore_state.set(RestoreState::Pending);
        #[cfg(test)]
        {
            // Test fixtures keep restore synchronous, but share finalization and
            // the same sweep implementation with production.
            let pending = Self::load_restore_queue(queue_file);
            if let Some(pending) = pending {
                let plan = self.apply_restored_queue(pending);
                let keep = Self::run_restore_sweeps(plan);
                Self::schedule_session_orphan_sweep(keep);
            }
            self.finish_restore(RestoreOutcome::Succeeded);
        }
        #[cfg(not(test))]
        {
            let weak = Rc::downgrade(self);
            let task = glib::spawn_future_local(async move {
                let mut completion = RestoreTaskGuard {
                    manager: weak.clone(),
                    finished: false,
                };
                let loaded = tokio_rt()
                    .spawn_blocking(move || Self::load_restore_queue(queue_file))
                    .await;
                let Some(manager) = weak.upgrade() else {
                    return;
                };
                // Close/Quit claims shutdown synchronously. Do not apply even a
                // successfully decoded queue after that claim: preserve its file.
                if manager.restore_should_abort() {
                    manager.finish_restore(RestoreOutcome::Failed);
                    completion.finished = true;
                    return;
                }
                match loaded {
                    Ok(Some(pending)) => {
                        manager.restore_dirty.set(false);
                        let plan = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            manager.apply_restored_queue(pending)
                        }));
                        let plan = match plan {
                            Ok(plan) => plan,
                            Err(payload) => {
                                let message = payload
                                    .downcast_ref::<String>()
                                    .map(String::as_str)
                                    .or_else(|| payload.downcast_ref::<&str>().copied())
                                    .unwrap_or("unknown panic");
                                tracing::error!(
                                    "download queue restore apply failed: {message}; preserving the saved queue"
                                );
                                manager.finish_restore(RestoreOutcome::Failed);
                                completion.finished = true;
                                return;
                            }
                        };
                        // Destination directories may be network mounts. Keep
                        // traversal/deletion off the GTK main context and do not
                        // release the queue start gate until cleanup has finished.
                        let swept = tokio_rt()
                            .spawn_blocking(move || Self::run_restore_sweeps(plan))
                            .await;
                        let keep = match swept {
                            Ok(keep) => keep,
                            Err(e) => {
                                tracing::error!(
                                    "download queue restore cleanup failed: {e}; preserving the saved queue"
                                );
                                manager.finish_restore(RestoreOutcome::Failed);
                                completion.finished = true;
                                return;
                            }
                        };
                        // Shutdown may have begun while the blocking sweeps ran.
                        if manager.restore_should_abort() {
                            manager.finish_restore(RestoreOutcome::Failed);
                            completion.finished = true;
                            return;
                        }
                        Self::schedule_session_orphan_sweep(keep);
                        manager.finish_restore(RestoreOutcome::Succeeded);
                    }
                    Ok(None) => {
                        manager.finish_restore(RestoreOutcome::Succeeded);
                    }
                    Err(e) => {
                        tracing::error!(
                            "download queue restore worker failed: {e}; preserving the saved queue"
                        );
                        manager.finish_restore(RestoreOutcome::Failed);
                        completion.finished = true;
                        return;
                    }
                }
                completion.finished = true;
            });
            self.restore_task.replace(Some(task));
        }
    }

    /// Wait until persisted rows have been applied to the model. Multiple
    /// app-level intake paths may wait concurrently without consuming a shared handle.
    pub async fn wait_for_restore(&self) {
        let mut ready = self.restore_ready.subscribe();
        loop {
            if *ready.borrow() {
                return;
            }
            if ready.changed().await.is_err() {
                return;
            }
        }
    }

    /// Whether the manager has begun or completed shutdown.
    pub fn is_shutting_down(&self) -> bool {
        self.shutdown_started.get() || self.draining.get() || self.shutdown_finished.get()
    }

    /// Restore failed after startup began; callers must not enqueue or persist
    /// over the last known-good queue until the user restarts or recovers it.
    pub fn restore_failed(&self) -> bool {
        self.restore_state.get() == RestoreState::Failed
    }

    fn restore_should_abort(&self) -> bool {
        self.shutdown_started.get() || self.draining.get() || self.shutdown_finished.get()
    }

    /// Shared restore finalization: all test and production paths release intake
    /// waiters through the same state transition and keep failed restores gated.
    fn finish_restore(self: &Rc<Self>, outcome: RestoreOutcome) {
        match outcome {
            RestoreOutcome::Succeeded => {
                self.restore_state.set(RestoreState::Ready);
                if self.restore_dirty.replace(false) {
                    self.persist_queue();
                }
                self.start_next();
            }
            RestoreOutcome::Failed => {
                self.restore_state.set(RestoreState::Failed);
            }
        }
        self.restore_ready.send_replace(true);
    }

    /// File reading, size/version caps, JSON parsing, and pure stored-item
    /// validation run on a blocking worker. No GTK/GObject values are created.
    fn load_restore_queue(queue_file: std::path::PathBuf) -> Option<Vec<PendingRestore>> {
        Self::sweep_stale_tmp_at(&queue_file);
        if !queue_file.exists() {
            return None;
        }

        const MAX_QUEUE_BYTES: u64 = 10_000_000;
        const MAX_QUEUE_ITEMS: usize = 1000;
        if std::fs::metadata(&queue_file)
            .map(|m| m.len() > MAX_QUEUE_BYTES)
            .unwrap_or(true)
        {
            tracing::warn!("quarantining oversized download queue");
            Self::quarantine_queue_at(&queue_file);
            return None;
        }
        let Ok(text) = std::fs::read_to_string(&queue_file) else {
            tracing::warn!("quarantining unreadable download queue");
            Self::quarantine_queue_at(&queue_file);
            return None;
        };
        let Ok(queue) = serde_json::from_str::<StoredQueue>(&text) else {
            tracing::warn!("quarantining unreadable download queue");
            Self::quarantine_queue_at(&queue_file);
            return None;
        };
        if queue.version == 0 || queue.version > QUEUE_VERSION {
            tracing::warn!("quarantining download queue version {}", queue.version);
            Self::quarantine_queue_at(&queue_file);
            return None;
        }

        // Preserve the existing policy: resumable entries first, then newest history.
        let mut items = queue.items;
        if items.len() > MAX_QUEUE_ITEMS {
            tracing::warn!(
                "truncating download queue ({} items, keeping active first)",
                items.len()
            );
            let (mut active, done): (Vec<StoredItem>, Vec<StoredItem>) = items
                .into_iter()
                .partition(|it| !matches!(it.status, DownloadStatus::Done));
            active.truncate(MAX_QUEUE_ITEMS);
            let skip = done.len().saturating_sub(MAX_QUEUE_ITEMS - active.len());
            items = active
                .into_iter()
                .chain(done.into_iter().skip(skip))
                .collect();
        }
        Some(
            items
                .into_iter()
                .filter_map(Self::validate_stored_item)
                .collect(),
        )
    }

    /// Apply validated rows to GTK's model and snapshot cleanup keep-sets.
    /// Filesystem traversal/deletion happens later on a blocking worker.
    fn apply_restored_queue(self: &Rc<Self>, pending: Vec<PendingRestore>) -> RestoreSweepPlan {
        let mut selections = Vec::new();
        {
            let _batch = self.batch_guard();
            // JSON parsing and per-row validation completed off-thread.
            // GObject construction and model mutation stay on this context.
            for p in pending {
                match p.item.status {
                    DownloadStatus::Done => {
                        if p.gallery_job.is_some() {
                            let mut restored_item = p.item.clone();
                            restored_item.segments = p.segments;
                            match self.restore_existing_gallery(&restored_item, p.gallery_job) {
                                Ok(_) => {}
                                Err(e) => tracing::warn!("skipping queue entry: {e}"),
                            }
                        } else {
                            let video_source = p.item.video_source.clone();
                            self.insert_history(
                                p.item.url,
                                p.item.dest_dir,
                                p.item.filename,
                                p.item.progress,
                                p.output_dir,
                                video_source,
                            );
                        }
                    }
                    status => {
                        // Restore staging on the blocking worker after model
                        // insertion. The start gate remains closed until then.
                        if let Some(sel) = p.item.selected_files.clone()
                            && let Ok(url) = normalize_url(&p.item.url)
                        {
                            selections.push((url, sel));
                        }
                        let mut restored_item = p.item.clone();
                        restored_item.status = status;
                        restored_item.segments = p.segments;
                        let result = if let Some(job) = p.gallery_job {
                            self.restore_existing_gallery(&restored_item, Some(job))
                        } else {
                            self.restore_existing(&restored_item)
                        };
                        match result {
                            Ok(restored) => {
                                if let Some(dir) = p.output_dir.clone() {
                                    restored.set_output_dir(dir);
                                }
                            }
                            Err(e) => tracing::warn!("skipping queue entry: {e}"),
                        }
                    }
                }
            }
        } // BatchGuard performs the consolidated persist/UI refresh/start attempt.

        // Build immutable keep-sets while ListStore/GObjects are available on GTK.
        let referenced: HashSet<String> = self
            .items()
            .map(|it| it.url().to_string())
            .filter(|u| crate::torrent::is_torrent_url(u))
            .collect();
        let live: HashSet<u64> = self.items().map(|it| it.id()).collect();
        let dest_dirs: HashSet<String> = self.items().map(|it| it.dest_dir()).collect();
        let session_keep = crate::torrent::session_handle().is_some().then(|| {
            self.items()
                .filter(|it| {
                    it.status() != DownloadStatus::Done && crate::torrent::is_torrent(&it.url())
                })
                .filter_map(|it| crate::torrent::info_hash_for_url(&it.url()))
                .collect()
        });
        RestoreSweepPlan {
            referenced,
            live,
            dest_dirs,
            selections,
            session_keep,
        }
    }

    fn run_restore_sweeps(plan: RestoreSweepPlan) -> Option<HashSet<String>> {
        for (url, selected_files) in plan.selections {
            crate::torrent::stage_selection(&url, selected_files);
        }
        crate::torrent::sweep_archives(&plan.referenced);
        crate::torrent::prune_selections(&plan.referenced);
        for dest_dir in plan.dest_dirs {
            crate::video::sweep_dest_staging(std::path::Path::new(&dest_dir), &plan.live);
        }
        crate::cookies::sweep_cookie_staging();
        plan.session_keep
    }

    fn schedule_session_orphan_sweep(keep: Option<HashSet<String>>) {
        if let Some(keep) = keep {
            tokio_rt().spawn(async move {
                crate::torrent::sweep_session_orphans(&keep).await;
            });
        }
    }

    /// Arm a one-shot wake for the earliest scheduled row. A one-minute
    /// fallback handles wall-clock changes; ordinary due items wake at their
    /// own deadline instead of waiting for a fixed 30-second polling interval.
    pub fn start_scheduler(self: &Rc<Self>) {
        if self.shutdown_started.get()
            || self.shutdown_finished.get()
            || !self.settings.scheduled_downloads_enabled()
        {
            return;
        }
        if self.scheduler_refresh.borrow().is_none() {
            let weak = Rc::downgrade(self);
            self.scheduler_refresh.replace(Some(Box::new(move || {
                if let Some(manager) = weak.upgrade() {
                    manager.arm_scheduler_timer();
                }
            })));
        }
        self.arm_scheduler_timer();
    }

    fn arm_scheduler_timer(self: &Rc<Self>) {
        if let Some(id) = self.scheduler_source.take() {
            id.remove();
        }
        if !self.settings.scheduled_downloads_enabled() {
            return;
        }
        let Some(next_due) = self
            .items()
            .filter(|item| item.status() == DownloadStatus::Scheduled)
            .map(|item| item.scheduled_at())
            .min()
        else {
            return;
        };

        let now = glib::DateTime::now_local()
            .map(|dt| dt.to_unix())
            .unwrap_or(0);
        const CLOCK_CHANGE_RECHECK_SECS: i64 = 60;
        let delay = next_due
            .saturating_sub(now)
            .clamp(1, CLOCK_CHANGE_RECHECK_SECS) as u32;
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_seconds_local_once(delay, move || {
            if let Some(manager) = weak.upgrade() {
                manager.scheduler_source.borrow_mut().take();
                manager.check_scheduled();
                manager.arm_scheduler_timer();
            }
        });
        self.scheduler_source.replace(Some(id));
    }

    /// Stop the schedule checker entirely: remove its source and change hook.
    pub fn stop_scheduler(&self) {
        if let Some(id) = self.scheduler_source.take() {
            id.remove();
        }
        self.scheduler_refresh.borrow_mut().take();
    }

    /// Start or stop the scheduler to match the scheduling preference.
    pub fn sync_scheduler(self: &Rc<Self>) {
        if self.shutdown_started.get() || self.shutdown_finished.get() {
            self.stop_scheduler();
        } else if self.settings.scheduled_downloads_enabled() {
            self.start_scheduler();
        } else {
            self.stop_scheduler();
        }
    }

    /// Queue scheduled downloads whose time has arrived.
    pub(crate) fn check_scheduled(self: &Rc<Self>) {
        let now = glib::DateTime::now_local()
            .map(|dt| dt.to_unix())
            .unwrap_or(0);
        let due_items: Vec<_> = self
            .items()
            .filter(|item| item.status() == DownloadStatus::Scheduled && item.scheduled_at() <= now)
            .collect();
        if due_items.is_empty() {
            return;
        }

        // Batch the due rows so persistence, UI refresh, and scheduler re-arm
        // happen once. Dropping BatchGuard also wakes the queue.
        let _batch = self.batch_guard();
        for item in due_items {
            item.set_status(DownloadStatus::Queued);
            item.set_scheduled_at(0);
        }
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
        for handle in &handles {
            handle.abort();
        }
        for pending in &finals {
            pending.worker_abort.abort();
        }

        let (joined_tx, joined_rx) = tokio::sync::oneshot::channel();
        tokio_rt().spawn(async move {
            let joined = tokio::time::timeout(Self::SHUTDOWN_JOIN_TIMEOUT, async {
                for handle in handles {
                    let _ = handle.await;
                }
                for pending in finals {
                    let _ = pending.finalizer.await;
                }
            })
            .await
            .is_ok();
            let _ = joined_tx.send(joined);
        });
        let joined = joined_rx.await.unwrap_or(false);
        if !joined {
            tracing::warn!(
                "shutdown: async join deadline elapsed; persisting without truncating partial files"
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
        self.persist_for_shutdown_async().await;
        self.shutdown_finished.set(true);
    }

    async fn persist_for_shutdown_async(&self) {
        #[cfg(test)]
        {
            self.persist_for_shutdown();
        }
        #[cfg(not(test))]
        {
            if self.restore_state.get() == RestoreState::Ready {
                if let (Some(queue), Some(tx)) =
                    (self.snapshot_queue_inner(true), &*self.persist_tx.borrow())
                {
                    tx.send_replace(Some(queue));
                }
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
            pending.worker_abort.abort();
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
