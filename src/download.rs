use crate::engine_msg::{DEST_EXISTS, EngineMsg};
use crate::file_names::{
    dedupe_filename, filename_from_url, fmt_bytes, name_stem, path_size, piece_len,
    rename_noreplace, restrict_filename_ascii, sane_filename, shorten_filename,
};
use crate::runtime::tokio_rt;
use gettextrs::{gettext, ngettext};
use gtk4::gio::prelude::*;
use gtk4::{gio, glib};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

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

/// Create the download's parent dir, refusing a planted symlink instead of
/// following it. An existing parent is containment-checked *before* any
/// filesystem touch — the old create-then-check ran create_dir_all first,
/// which is a no-op on an existing path but needlessly touched the link —
/// while a missing parent is created, then verified (it can't harbor a link,
/// but a link can be swapped in for the name mid-call). `symlink_metadata`
/// sees even a dangling link, where `exists` would not.
///
/// A `create_dir_all` failure is `Err`, not a containment refusal, so the
/// caller reports it distinctly instead of blaming the download folder.
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
    /// Abort senders for resolver workers, by row. Signalled from pause/park/cancel so extractor streams stop promptly.
    video_abort: RefCell<HashMap<u64, tokio::sync::oneshot::Sender<crate::video::StopIntent>>>,
    /// Delivery decision per video attempt, by row. Manager and worker arbitrate on this one object.
    gates: RefCell<HashMap<u64, std::sync::Arc<AttemptGate>>>,
    /// Destinations with a discard in flight (exact path + stem key). Consulted by intake *and* `unremove`, which bypasses intake.
    reservations: std::sync::Arc<std::sync::Mutex<Reservations>>,
    /// Queue wakeups from worker threads: the discard finalizer sends here after releasing a reservation; `new()` re-runs `start_next()`.
    wake_tx: tokio::sync::mpsc::UnboundedSender<()>,
    /// Handle of the `wake_tx` receiver task. Kept so `Drop` can abort it (a task reaped on another thread trips glib's thread guard).
    wake_task: Cell<Option<glib::JoinHandle<()>>>,
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
}

/// One validated queue entry awaiting the restore apply phase (module level so a mid-loop failure never leaves half-spawned engines).
struct PendingRestore {
    item: StoredItem,
    output_dir: Option<String>,
    segments: Option<SegmentState>,
}

impl Drop for DownloadManager {
    fn drop(&mut self) {
        // Destroy the discard-wakeup task now: a `spawn_future_local` task is
        // only reaped when the main loop next polls it, so a stale one would be
        // dispatched by a later test's main-loop iteration running on another
        // thread, tripping glib's thread guard.
        if let Some(handle) = self.wake_task.take() {
            handle.abort();
        }
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

/// Queue + engine owner: persists the queue, spawns downloads, notifies the UI.
impl DownloadManager {
    /// Create a manager over `store`; call [`DownloadManager::restore_queue`] once.
    pub fn new(store: gio::ListStore, settings: crate::settings::AppSettings) -> Rc<Self> {
        let (wake_tx, mut wake_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let this = Rc::new(Self {
            store,
            settings,
            running: RefCell::new(HashMap::new()),
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
            video_abort: RefCell::new(HashMap::new()),
            gates: RefCell::new(HashMap::new()),
            reservations: std::sync::Arc::new(std::sync::Mutex::new(Reservations::default())),
            wake_tx,
            wake_task: Cell::new(None),
            discards: RefCell::new(HashMap::new()),
            live_rows: RefCell::new(std::collections::HashSet::new()),
            draining: Cell::new(false),
        });
        // Queue wakeups from worker threads (see `wake_tx`). Weak ref: the loop must not keep the manager alive.
        {
            let weak = Rc::downgrade(&this);
            let handle = glib::spawn_future_local(async move {
                while wake_rx.recv().await.is_some() {
                    if let Some(m) = weak.upgrade() {
                        // Never start rows while tearing down: shutdown awaits discard finalizers, each of which sends a wakeup.
                        if !m.draining.get() {
                            m.start_next();
                        }
                    } else {
                        break;
                    }
                }
            });
            // The task must die with the manager (see `Drop`): a stale local task reaped on another thread trips glib's thread guard.
            this.wake_task.set(Some(handle));
        }
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

    /// Recount queued rows and refresh the UI. Deferred while a batch is
    /// open, exactly like `persist_queue`: the recount and `finished_count`
    /// walk the whole store, the window hook walks it again in `sync()` plus
    /// once per status predicate, and every step is a GObject ref, a downcast
    /// and a property read per element. Per row that adds up, so a 500-entry
    /// import did it 500 times over. Every batch opener already ends with its
    /// own refresh — the [`BatchGuard`] close here, `restore_queue` and the picker
    /// at the close of their loops — so nothing is lost by waiting.
    fn changed(&self) {
        if self.batch.get() > 0 {
            return;
        }
        self.queued.set(
            self.items()
                .filter(|it| it.status() == DownloadStatus::Queued)
                .count(),
        );
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
    /// leftover `<dest>/.grab-video/<id>/` and legacy tmp dirs, or a new row
    /// would resume (or sweep) a stranger's scratch.
    fn alloc_id_for(&self, dest_dir: &std::path::Path) -> u64 {
        loop {
            let id = self.alloc_id();
            if !crate::video::staging_occupied(dest_dir, id) {
                return id;
            }
        }
    }

    /// Reuse a restored row's persisted id where possible, else allocate. The id is the staging key, so a fresh number would point at the wrong `<dest>/.grab-video/<id>/` and risk colliding with a later row.
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

    /// Enqueue a download to start at a future Unix timestamp. The item gets
    /// `Scheduled` status (skipped by `start_next`) until the scheduler queues it.
    ///
    /// # Errors
    /// Returns a display-ready message when the URL is invalid or the timestamp is not in the future.
    pub fn enqueue_scheduled(
        self: &Rc<Self>,
        url: &str,
        dest_dir: Option<&str>,
        filename: Option<&str>,
        scheduled_at: i64,
    ) -> Result<DownloadItem, String> {
        let now = glib::DateTime::now_local()
            .map(|dt| dt.to_unix())
            .unwrap_or(0);
        if scheduled_at <= now {
            return Err(gettext("Scheduled time must be in the future"));
        }
        // Batch guard defers start_next until the status is Scheduled:
        // without it, insert() would spawn the still-Queued item.
        let _guard = self.batch_guard();
        let item = self.enqueue(url, dest_dir, filename)?;
        item.set_scheduled_at(scheduled_at);
        item.set_status(DownloadStatus::Scheduled);
        Ok(item)
    }

    /// Intake for .torrent files: archive the bytes, then enqueue the
    /// pseudo-URL like any other download (stub from the file stem, real
    /// name arrives with metadata via SuggestName).
    ///
    /// # Errors
    /// Returns a display-ready message when the bytes are not a valid torrent.
    pub fn enqueue_torrent_file(
        self: &Rc<Self>,
        bytes: Vec<u8>,
        file_name: &str,
        dest_dir: Option<&str>,
        only_files: Option<Vec<usize>>,
    ) -> Result<DownloadItem, String> {
        let pseudo = crate::torrent::archive_torrent_file(file_name, &bytes)?;
        if let Some(sel) = only_files {
            crate::torrent::stage_selection(&pseudo, sel);
        }
        let stub = crate::torrent::stub_name_for_file(file_name);
        let item = self.enqueue(&pseudo, dest_dir, Some(&stub))?;
        // The engine writes with overwrite:true, so record a deduped subfolder the engine then uses as-is.
        if let Some((base, _)) = crate::torrent::intake_plan(&bytes) {
            let dir = self.resolve_dir(dest_dir);
            if std::path::Path::new(&dir).join(&base).exists() {
                let name = dedupe_filename(&base, |n| {
                    let p = std::path::Path::new(&dir).join(n);
                    p.exists()
                        // Only video-row finished names reserve stems (see plain `enqueue`).
                        || self.items()
                            .any(|it| it.output_dir() == p.to_string_lossy())
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
    pub fn enqueue_video(
        self: &Rc<Self>,
        page_url: &str,
        dest_dir: Option<&str>,
        filename: Option<&str>,
        choices: crate::media_types::VideoChoices,
    ) -> Result<DownloadItem, String> {
        // One readdir per intake.
        let dir = self.resolve_dir(dest_dir);
        let existing = crate::video_staging::dir_file_names(std::path::Path::new(&dir));
        self.enqueue_video_staged(page_url, &dir, filename, choices, &existing)
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
            },
        );
        Ok(self.insert(item))
    }

    /// Queue one row per playlist entry for collection-shaped resolves with no picked entry. Returns (added, reported total). One persist for the whole import.
    fn expand_playlist_rows(
        self: &Rc<Self>,
        id: u64,
        item: &DownloadItem,
        pl: &crate::media_types::PlaylistInfo,
    ) -> (usize, usize) {
        let (quality, audio_only) = match self.video_source(id) {
            Some(crate::media_types::VideoSource::Page {
                quality,
                audio_only,
                ..
            }) => (quality, audio_only),
            _ => return (0, pl.total),
        };
        let dest_dir = self.resolve_dir(Some(&item.dest_dir()));
        let _batch = self.batch_guard();
        // Multi-item collections land in a titled subfolder, torrent-style; a
        // lone item keeps the flat behavior.
        let dest_dir = if pl.items.len() > 1 {
            crate::file_names::collection_subdir(&dest_dir, &pl.title)
        } else {
            dest_dir
        };
        // One readdir for the whole expansion instead of one per entry.
        let existing = crate::video_staging::dir_file_names(std::path::Path::new(&dest_dir));
        let mut added = 0;
        for entry in &pl.items {
            let Some(url) =
                crate::video_probe::expand_child_target(&item.url(), &pl.page_url, entry)
            else {
                continue;
            };
            // Live streams queued from a playlist take the VOD path; each child re-resolves its own page.
            if self
                .enqueue_video_staged(
                    &url,
                    &dest_dir,
                    None,
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
        // Reported total, not attempted ("500 of 600").
        (added, pl.total)
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
        // Resumed rows requeue; only settled rows keep their status.
        // Scheduled rows keep their status and timestamp; overdue ones queue immediately.
        let now = glib::DateTime::now_local()
            .map(|dt| dt.to_unix())
            .unwrap_or(0);
        let scheduled_at = stored.scheduled_at.unwrap_or(0);
        item.set_scheduled_at(scheduled_at);
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

    fn insert(self: &Rc<Self>, item: DownloadItem) -> DownloadItem {
        self.store.append(&item);
        self.persist_queue();
        self.changed();
        // Inside a batch the close kicks the scheduler once; a queue scan per insert is pure overhead.
        if self.batch.get() == 0 {
            self.start_next();
        }
        item
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
        // Size off the final path: the engine measured the pre-rename one. Folders sum contents.
        let size = path_size(&item.file_path()).unwrap_or(0);
        item.set_detail(if size > 0 {
            gettext("Finished • {size}").replace("{size}", &fmt_bytes(size))
        } else {
            gettext("Finished")
        });
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
                Some(item) => self.spawn(item),
                None => break,
            }
        }
    }

    fn spawn(self: &Rc<Self>, item: DownloadItem) {
        let opts = DownloadOptions::from_settings(&self.settings);
        // Re-stash per attempt: a stale Last-Modified must never apply when the new run's server sends none.
        self.server_mtime.borrow_mut().remove(&item.id());
        let dest = item.file_path();
        if let Some(parent) = dest.parent() {
            // A planted symlink in any parent component would divert the
            // download outside the destination: refuse instead of following
            // it. (Filenames can't contain separators, so the parent is the
            // destination itself today; this pins the invariant.)
            // A mkdir failure is reported distinctly: it is not a containment
            // refusal, so "escapes the download folder" would mislead.
            let blocked = match ensure_contained_parent(parent, &item.dest_dir()) {
                Ok(true) => None,
                Ok(false) => Some(gettext("Download path escapes the download folder")),
                Err(e) => Some(
                    gettext("Could not create the download folder: {error}")
                        .replace("{error}", &e.to_string()),
                ),
            };
            if let Some(detail) = blocked {
                item.set_detail(detail.clone());
                item.set_status(DownloadStatus::Failed);
                self.changed();
                self.notify_finished(&item, Err(detail));
                return;
            }
        }
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
            return self.spawn_torrent(item, url);
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
        let mode = match self.segment_state.borrow().get(&item.id()).cloned() {
            Some(st) => {
                // Drop the bitmap if the file no longer holds the completed prefix (deleted/truncated/replaced while paused).
                let len = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
                if len < st.prefix_len() || len > st.total {
                    self.segment_state.borrow_mut().remove(&item.id());
                    let _ = std::fs::remove_file(&dest);
                    StartMode::Fresh
                } else {
                    let frac =
                        (st.completed_bytes() as f64 / st.total.max(1) as f64).clamp(0.0, 1.0);
                    item.set_progress(frac);
                    StartMode::Resume(st)
                }
            }
            None => match std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0) {
                0 => StartMode::Fresh,
                // Parabolic-style dedup: a row that never started refuses an
                // existing destination instead of truncating or renaming it.
                // Restored/retried rows carry `started` and resume their own
                // partials below (progress alone can't tell: a crash before
                // the first tick restores at 0.0 with bytes on disk).
                _ if !self.started.borrow().contains(&item.id()) => {
                    item.set_detail(DEST_EXISTS.to_string());
                    item.set_status(DownloadStatus::Failed);
                    self.changed();
                    self.notify_finished(&item, Err(DEST_EXISTS.to_string()));
                    return;
                }
                _ => StartMode::Single,
            },
        };
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
        let ctx = FetchCtx {
            client: http_client_for(proxy.as_ref()),
            url,
            dest,
            opts,
            cookies: None,
            timeout,
            tx,
        };
        let handle = tokio_rt().spawn(run_download(ctx, connections, mode));
        self.running.borrow_mut().insert(item.id(), handle);
        // The engine owns the destination from here: later spawns resume it rather than refusing as foreign.
        self.started.borrow_mut().insert(item.id());
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
        std::path::Path::new(dir).join(n).exists()
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
        let this = Rc::clone(self);
        // Speed baseline: resumes seed `downloaded` with pre-existing bytes, which lifetime-average math would report as fantasy GB/s.
        let mut base: Option<(u64, Instant)> = None;
        // Set on Finished/Failed. If the channel closes first the engine died without reporting: fail the row instead of stranding it.
        let mut done = false;
        // Row URL and file filter are fixed at intake: hoist both out of the per-tick path.
        let url = item.url().to_string();
        let sel_suffix = match crate::torrent::get_selection(&url) {
            Some(sel) => format!(" • {} files", sel.len()),
            None => String::new(),
        };
        glib::spawn_future_local(async move {
            while let Some(msg) = rx.recv().await {
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
                            _ => {
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
                                let existing = crate::video_staging::dir_file_names(
                                    std::path::Path::new(&dir),
                                );
                                let taken = |n: &str| this.is_name_taken(&dir, &existing, n);
                                // Claim-then-move so a file appearing between dedupe and rename is never clobbered.
                                let old_path = item.file_path();
                                let mut final_name = dedupe_filename(&name, taken);
                                let mut moved = !old_path.exists();
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
                            // Size off the final path (see `insert_history`): the engine measured the pre-rename one.
                            let size = path_size(&item.file_path()).unwrap_or(size);
                            // Server file date, when asked: best-effort, never fails the row.
                            let mtime = this.server_mtime.borrow_mut().remove(&id);
                            if this.settings.keep_server_date()
                                && let Some(t) = mtime
                                && let Ok(f) = std::fs::File::open(item.file_path())
                            {
                                let _ = f.set_modified(t);
                            }
                            item.set_progress(1.0);
                            item.set_status(DownloadStatus::Done);
                            item.set_detail(if size > 0 {
                                gettext("Finished • {size}").replace("{size}", &fmt_bytes(size))
                            } else {
                                gettext("Finished")
                            });
                            // One finished record per URL (Parabolic parity): re-downloads replace instead of stacking.
                            this.drop_finished_duplicates(&url, id);
                            this.segment_state.borrow_mut().remove(&id);
                            this.torrent_pieces.borrow_mut().remove(&id);
                            // Filtered torrents: drop untoggled files now that every selected byte is on disk. Skipped while seeding (serving still reads those spans).
                            if crate::torrent::is_torrent(&url)
                                && !this.settings.torrent_seed_finished()
                            {
                                let folder = Self::torrent_folder(&item);
                                if folder.is_dir() {
                                    crate::torrent::cleanup_unselected(&folder, &url);
                                }
                                // The intake filter is spent: keeping it would pin the index list for a done row.
                                crate::torrent::drop_selection(&url);
                            }
                            this.notify_finished(&item, Ok(()));
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
                        let (added, total) = this.expand_playlist_rows(id, &item, &pl);
                        if added == 0 {
                            let e = crate::video_probe::playlist_resolve_error(None);
                            if item.status() != DownloadStatus::Cancelled
                                && item.status() != DownloadStatus::Paused
                            {
                                item.set_detail(e.to_string());
                                item.set_status(DownloadStatus::Failed);
                                this.torrent_pieces.borrow_mut().remove(&id);
                                this.notify_finished(&item, Err(e.to_string()));
                            }
                        } else if item.status() != DownloadStatus::Cancelled
                            && item.status() != DownloadStatus::Paused
                        {
                            item.set_status(DownloadStatus::Done);
                            item.set_detail(
                                ngettext(
                                    "Expanded into {added} of {total} item",
                                    "Expanded into {added} of {total} items",
                                    added as u32,
                                )
                                .replace("{added}", &added.to_string())
                                .replace("{total}", &total.to_string()),
                            );
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
                        this.segment_state.borrow_mut().remove(&id);
                        this.pending_names.borrow_mut().remove(&id);
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
                    EngineMsg::SegmentsInit { total } => {
                        this.segment_state
                            .borrow_mut()
                            .insert(id, SegmentState::new(total));
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
                        }
                    }
                    EngineMsg::TruncatePrefix => {
                        if let Some(st) = this.segment_state.borrow_mut().get_mut(&id) {
                            truncate_to_prefix(&item.file_path(), st);
                            // The file lost everything past the prefix; the bitmap must forget it too or a later resume skips missing pieces.
                            st.forget_beyond_prefix();
                        }
                    }
                    EngineMsg::FallbackSingle { ack } => {
                        // Server throttled parallel connections: shrink to the completed prefix and forget the bitmap on the main thread. The engine waits for this ack before appending at EOF.
                        if let Some(st) = this.segment_state.borrow().get(&id) {
                            truncate_to_prefix(&item.file_path(), st);
                        }
                        this.segment_state.borrow_mut().remove(&id);
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
                    EngineMsg::LiveDetected => {
                        // Downloading only: a stop/pause during resolve must not leave a stale live_rows member (it would skip staging cleanup and take live signal paths).
                        if item.status() == DownloadStatus::Downloading {
                            this.live_rows.borrow_mut().insert(id);
                        }
                    }
                }
            }
            // Superseded pump future: touch nothing, especially not the new engine's handle in `running`.
            if !this.is_current(id, generation) {
                return;
            }
            this.running.borrow_mut().remove(&id);
            this.video_abort.borrow_mut().remove(&id);
            this.live_rows.borrow_mut().remove(&id);
            if this.draining.get() {
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
        });
    }

    /// Spawn the torrent engine for a magnet row. Mirrors `spawn`'s contract so pause/cancel/retry and the stale-pump guard keep working.
    fn spawn_torrent(self: &Rc<Self>, item: DownloadItem, magnet: String) {
        let dir = std::path::PathBuf::from(item.dest_dir().to_string());
        // Magnets recorded their subfolder at enqueue; archived torrents recompute theirs from metadata.
        let recorded = item.output_dir().to_string();
        let dest = if recorded.is_empty() {
            let _ = std::fs::create_dir_all(&dir);
            dir.clone()
        } else {
            // Re-guard the recorded subfolder at spawn: a symlink planted
            // after intake (a wide window for queued torrents) would
            // otherwise divert the engine's writes. A squatted name dedupes
            // to a fresh folder, like collection subfolders.
            let recorded_path = std::path::PathBuf::from(&recorded);
            let guarded = crate::torrent::guard_output_folder(&dir, recorded_path.clone());
            if guarded != recorded_path {
                item.set_output_dir(guarded.to_string_lossy().into_owned());
            } else if guarded.parent() != Some(dir.as_path()) {
                // Not under the dest dir (shouldn't happen: every recorded
                // path is dir-joined): keep the old ensure-exists behavior.
                let _ = std::fs::create_dir_all(&guarded);
            }
            guarded
        };
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
        let net = crate::torrent::plan_torrent_net(
            settings.torrent_dht(),
            settings.torrent_lsd(),
            0,
            false,
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
        let job = crate::video::VideoJob {
            item_id: id,
            page_url,
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
        let name = new_name.trim();
        if name == item.filename() {
            return Ok(());
        }
        if name.is_empty() || !sane_filename(name) {
            return Err(gettext("That isn't a valid file name"));
        }
        if item.status() == DownloadStatus::Done {
            let new_path = std::path::PathBuf::from(item.dest_dir().to_string()).join(name);
            match rename_noreplace(&item.file_path(), &new_path) {
                Ok(()) => {}
                // Deleted behind our back: the label update below still applies.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(format!("Could not rename {}: {e}", item.filename()));
                }
            }
        }
        item.set_filename(name);
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
        self.cancel_inner(id, false, Stop::Preserve);
        self.persist_queue();
        self.changed();
        self.start_next();
    }

    /// Reclaim a removed video row's scratch, but only once its worker has actually stopped. Removal cannot sweep inline: a worker mid-teardown can recreate what a sweep removed, so unlinking first just loses the race. Claiming the gate first makes deferring safe: only a worker this finalizer awaited may have committed an orphan `dest` file.
    /// Engine invariant: a finished file is never deleted. The `remove_file` below is the single exception — a commit that won the race against its row's discard, leaving an orphan no row can own.
    fn finish_discard(&self, id: u64, dest: std::path::PathBuf, gate: std::sync::Arc<AttemptGate>) {
        let _ = gate.discard();
        // Resolved against the destination: current staging lives beside it,
        // legacy tmp dirs for rows staged before the move resolve the same way.
        let staging = crate::video::staging_location_for_dest(&dest, id);
        // `self` (Rc, !Send) cannot go to the tokio runtime: carry reservations as an `Arc` clone and wake the queue through the `Send` channel.
        let reservations = std::sync::Arc::clone(&self.reservations);
        let wake_tx = self.wake_tx.clone();
        match self.running.borrow_mut().remove(&id) {
            Some(handle) => {
                let worker_abort = handle.abort_handle();
                let finalizer = crate::runtime::tokio_rt().spawn(async move {
                    let _ = handle.await;
                    crate::video::clean_staging_in(&staging.root, &staging.dir);
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
                    wake_tx.send(()).ok();
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
                crate::video::clean_staging_in(&staging.root, &staging.dir);
                crate::video::clean_dest_parts(&dest);
                self.release_dest(&dest);
                // Same wakeup as the async finalizer above; already on the main thread.
                self.wake_tx.send(()).ok();
            }
        }
    }

    fn cancel_inner(&self, id: u64, keep_partial: bool, stop: Stop) {
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
        if let Some(item) = self.find(id) {
            // Segmented partials have holes: without the bitmap cancel drops the file — unless the row may come back via Undo.
            if had_segments && !keep_partial {
                let _ = std::fs::remove_file(item.file_path());
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
        self.cancel_inner(id, true, Stop::Discard);
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
        if let Some(pos) = self.position_of(id) {
            self.store.remove(pos);
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
        let recorded = item.output_dir().to_string();
        if !recorded.is_empty() {
            let path = std::path::PathBuf::from(recorded);
            // Refuse a recorded folder that is itself a symlink: swapped in
            // after the finish, `cleanup_unselected`'s `is_dir` check follows
            // the link and the deletes run through it onto the target.
            // `symlink_metadata` sees the link itself (even when dangling);
            // fall through to the guarded recompute instead of trusting it.
            let is_link = std::fs::symlink_metadata(&path)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false);
            if !is_link {
                return path;
            }
        }
        let dest = std::path::PathBuf::from(item.dest_dir().to_string());
        crate::torrent::torrent_output_dir(&dest, &item.url())
            .map(|folder| crate::torrent::guard_output_folder(&dest, folder))
            .unwrap_or_else(|| item.file_path())
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

    /// Move the downloaded file to Trash, then remove the row.
    ///
    /// # Errors
    /// Returns a display-ready message when trashing fails.
    pub fn delete_download(self: &Rc<Self>, id: u64) -> Result<(), String> {
        let item = self.find(id).ok_or_else(|| gettext("Download not found"))?;
        // Torrent rows: drop the session entry and archive, drop the row, then Trash the real files (they live in the recorded/recomputed folder, never the stub path).
        if crate::torrent::is_torrent(&item.url()) {
            let path = Self::torrent_folder(&item);
            crate::torrent::forget_download(id);
            crate::torrent::delete_archive_for_url(&item.url());
            self.remove(id);
            return match gio::File::for_path(path).trash(gio::Cancellable::NONE) {
                Ok(()) => Ok(()),
                Err(e) if e.kind::<gio::IOErrorEnum>() == Some(gio::IOErrorEnum::NotFound) => {
                    Ok(())
                }
                Err(e) => Err(format!("Could not move {} to Trash: {e}", item.filename())),
            };
        }
        match gio::File::for_path(item.file_path()).trash(gio::Cancellable::NONE) {
            Ok(()) => {}
            Err(e) if e.kind::<gio::IOErrorEnum>() == Some(gio::IOErrorEnum::NotFound) => {}
            Err(e) => return Err(format!("Could not move {} to Trash: {e}", item.filename())),
        }
        // Collected subtitle sidecars travel with the video: trash every `<stem>.<lang>.srt`. Plain rows never wrote sidecars — gate on the staged source like remove().
        if self.video_sources.borrow().contains_key(&id) {
            for lang in crate::video_prefs::subtitle_content_languages() {
                let sidecar = crate::video_staging::sidecar_path_for(&item.file_path(), lang);
                match gio::File::for_path(&sidecar).trash(gio::Cancellable::NONE) {
                    Ok(()) => {}
                    Err(e) if e.kind::<gio::IOErrorEnum>() == Some(gio::IOErrorEnum::NotFound) => {}
                    Err(e) => tracing::warn!(
                        sidecar = %sidecar.display(),
                        error = %e,
                        "subtitle sidecar left behind"
                    ),
                }
            }
        }
        self.remove(id);
        Ok(())
    }

    /// Cancel every queued, downloading or paused item.
    pub fn cancel_all(self: &Rc<Self>) {
        // One persist at the end and no per-row start_next: cancel() would briefly spawn the next queued row only to cancel it right after.
        self.for_matching(
            |s| {
                matches!(
                    s,
                    DownloadStatus::Queued | DownloadStatus::Downloading | DownloadStatus::Paused
                )
            },
            |m, id| m.cancel_inner(id, false, Stop::Preserve),
        );
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
        self.epoch.borrow_mut().remove(&id);
        if let Some(pos) = self.position_of(id) {
            self.store.remove(pos);
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
        let _ = std::fs::create_dir_all(&dir);
        dir.join("queue.json")
    }

    /// Move a broken queue file aside (`queue.json.bak`) so the next persist starts fresh.
    fn quarantine_queue() {
        let bak = Self::queue_file().with_extension("json.bak");
        if let Err(e) = std::fs::rename(Self::queue_file(), &bak) {
            tracing::error!("could not quarantine download queue: {e}");
        }
    }

    fn persist_queue(&self) {
        if self.batch.get() > 0 {
            return;
        }
        let mut items = Vec::new();
        for it in self.items() {
            // Cancelled rows carry no intent: never persisted.
            if it.status() != DownloadStatus::Cancelled {
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
                });
            }
        }
        let data = StoredQueue {
            version: QUEUE_VERSION,
            items,
        };
        let text = match serde_json::to_string_pretty(&data) {
            Ok(text) => text,
            Err(e) => {
                tracing::error!("could not serialize download queue: {e}");
                return;
            }
        };
        let tmp = Self::queue_file().with_extension("json.tmp");
        let write_tmp = || -> std::io::Result<()> {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            Ok(())
        };
        match write_tmp() {
            Ok(()) => {
                if let Err(e) = std::fs::rename(&tmp, Self::queue_file()) {
                    tracing::error!("could not replace download queue: {e}");
                    return;
                }
                if let Some(parent) = Self::queue_file().parent()
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
        Some(PendingRestore {
            item,
            output_dir,
            segments,
        })
    }

    /// Load the persisted queue (cap: 1000 items / 10 MB), then resume. Unusable files move to `queue.json.bak`, never deleted.
    pub fn restore_queue(self: &Rc<Self>) {
        // Pre-persisted-id queues restore with fresh ids, which must not collide with a staging dir another row still occupies.
        if let Some(highest) = crate::video::highest_staging_index()
            && highest >= self.next_id.get()
        {
            self.next_id.set(highest + 1);
        }
        if Self::queue_file().exists() {
            const MAX_QUEUE_BYTES: u64 = 10_000_000;
            const MAX_QUEUE_ITEMS: usize = 1000;
            if std::fs::metadata(Self::queue_file())
                .map(|m| m.len() > MAX_QUEUE_BYTES)
                .unwrap_or(true)
            {
                tracing::warn!("quarantining oversized download queue");
                Self::quarantine_queue();
                return;
            };
            let Ok(text) = std::fs::read_to_string(Self::queue_file()) else {
                return;
            };
            let Ok(queue) = serde_json::from_str::<StoredQueue>(&text) else {
                tracing::warn!("quarantining unreadable download queue");
                Self::quarantine_queue();
                return;
            };
            if queue.version == 0 || queue.version > QUEUE_VERSION {
                tracing::warn!("quarantining download queue version {}", queue.version);
                Self::quarantine_queue();
                return;
            }
            // Over-cap queues keep every resumable item first, then the newest history.
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
            {
                let _batch = self.batch_guard();
                // Two phases: collect all restore decisions first, then apply — a mid-loop failure can no longer leave half-spawned engines.
                let mut pending = Vec::with_capacity(items.len());
                for item in items {
                    if let Some(p) = Self::validate_stored_item(item) {
                        pending.push(p);
                    }
                }
                for p in pending {
                    match p.item.status {
                        DownloadStatus::Done => {
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
                        status => {
                            // Re-stage the intake file selection BEFORE restore_existing: insert → start_next → spawn_torrent reads it synchronously, so staging after is too late.
                            if let Some(sel) = p.item.selected_files.clone()
                                && let Ok(url) = normalize_url(&p.item.url)
                            {
                                crate::torrent::stage_selection(&url, sel);
                            }
                            let mut restored_item = p.item.clone();
                            restored_item.status = status;
                            restored_item.segments = p.segments;
                            match self.restore_existing(&restored_item) {
                                Ok(restored) => {
                                    // Re-attach the recorded engine folder.
                                    if let Some(dir) = p.output_dir.clone() {
                                        restored.set_output_dir(dir);
                                    }
                                }
                                Err(e) => tracing::warn!("skipping queue entry: {e}"),
                            }
                        }
                    }
                }
            } // Guard drops: persist once, refresh the UI, kick the scheduler.
            // Drop archives and staged selections no row references anymore; sweep session orphans whose rows vanished in a crash.
            let referenced: std::collections::HashSet<String> = self
                .items()
                .map(|it| it.url().to_string())
                .filter(|u| crate::torrent::is_torrent_url(u))
                .collect();
            crate::torrent::sweep_archives(&referenced);
            // Same for staged file selections (a future re-add stages fresh at intake).
            crate::torrent::prune_selections(&referenced);
            // Crash/kill leftovers: only restored rows reuse their staging ids, so a
            // numeric staging dir with no live row can never resume. Reclaimed
            // here, after restore and before any worker starts; completed
            // `final.*` recordings are preserved (the user's only copy).
            let live: std::collections::HashSet<u64> = self.items().map(|it| it.id()).collect();
            // Legacy tmp root first, then every destination's own staging root:
            // `<dest>/.grab-video` holds one numeric dir per item that staged
            // there, reclaimed with the same keep-set (ids are session-unique).
            crate::video::sweep_orphan_staging(&live);
            let mut dest_dirs = std::collections::HashSet::new();
            for it in self.items() {
                dest_dirs.insert(it.dest_dir());
            }
            for dest_dir in dest_dirs {
                crate::video::sweep_dest_staging(std::path::Path::new(&dest_dir), &live);
            }
            // No session yet: the sweep below would no-op, so skip the store walk.
            if crate::torrent::session_handle().is_some() {
                let keep: std::collections::HashSet<String> = self
                    .items()
                    .filter(|it| {
                        it.status() != DownloadStatus::Done && crate::torrent::is_torrent(&it.url())
                    })
                    .filter_map(|it| crate::torrent::info_hash_for_url(&it.url()))
                    .collect();
                tokio_rt().spawn(async move {
                    crate::torrent::sweep_session_orphans(&keep).await;
                });
            }
        }
    }

    /// Start the schedule checker: every 30s, queue any scheduled downloads whose time has arrived.
    /// Uses `timeout_add_seconds_local` (main thread, no worker needed — the check is a cheap store scan).
    pub fn start_scheduler(self: &Rc<Self>) {
        let manager = Rc::clone(self);
        glib::timeout_add_seconds_local(30, move || {
            manager.check_scheduled();
            glib::ControlFlow::Continue
        });
    }

    /// Queue scheduled downloads whose time has arrived. Called by the scheduler timer.
    pub(crate) fn check_scheduled(self: &Rc<Self>) {
        let now = glib::DateTime::now_local()
            .map(|dt| dt.to_unix())
            .unwrap_or(0);
        let mut due = false;
        for item in self.items() {
            if item.status() == DownloadStatus::Scheduled && item.scheduled_at() <= now {
                item.set_status(DownloadStatus::Queued);
                item.set_scheduled_at(0);
                due = true;
            }
        }
        if due {
            self.changed();
            self.persist_queue();
            self.start_next();
        }
    }

    /// Abort running tasks and persist the queue for the next launch.
    pub fn shutdown(&self) {
        self.draining.set(true);
        let handles: Vec<_> = self.running.borrow_mut().drain().map(|(_, h)| h).collect();
        // Discard workers first through their retained abort handles: aborting a finalizer would detach its worker instead of stopping it. Finalizers are awaited (not aborted) so cleanup still runs.
        let finals: Vec<PendingDiscard> =
            self.discards.borrow_mut().drain().map(|(_, p)| p).collect();
        for handle in handles.iter() {
            handle.abort();
        }
        for pending in &finals {
            pending.worker_abort.abort();
        }
        // Engine tasks touch only the tokio runtime, so joining them here is prompt and deadlock-free.
        tokio_rt().block_on(async {
            for handle in handles {
                let _ = handle.await;
            }
            for pending in finals {
                let _ = pending.finalizer.await;
            }
        });
        let ids: Vec<u64> = self.segment_state.borrow().keys().cloned().collect();
        for id in ids {
            if let Some(item) = self.find(id)
                && let Some(st) = self.segment_state.borrow_mut().get_mut(&id)
            {
                truncate_to_prefix(&item.file_path(), st);
                st.forget_beyond_prefix();
            }
        }
        // Persist BEFORE returning: pump tails exit silently once draining is set, so this is the only persist that matters.
        self.persist_queue();
    }
}

#[cfg(test)]
#[path = "download_tests.rs"]
mod tests;
