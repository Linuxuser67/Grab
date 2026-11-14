//! Download attempt preparation, backend execution and message-pump lifecycle.

use super::{
    DownloadManager, FinalNameReservations, ProbeJsonKey, RestoreState, ensure_contained_parent,
    format_gettext_template,
};
use crate::download_fetch::{FetchCtx, StartMode, run_download, truncate_to_prefix};
use crate::download_net::http_client_for;
use crate::download_pieces::SegmentState;
use crate::download_rate::{fmt_eta, format_amounts, parse_rate, publish_rate_limit};
use crate::download_row::DownloadItem;
use crate::download_store::DownloadStatus;
use crate::engine_msg::{DEST_EXISTS, EngineMsg};
use crate::file_names::{dedupe_filename, fmt_bytes, name_stem, rename_noreplace, sane_filename};
use crate::runtime::tokio_rt;
use crate::video::AttemptGate;
use gettextrs::{gettext, ngettext};
use gtk4::gio::prelude::*;
use gtk4::{gio, glib};
use std::rc::Rc;
use std::time::{Duration, Instant};

impl DownloadManager {
    pub(super) fn start_next(self: &Rc<Self>) {
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
        while self.running.borrow().len() + self.preparing.borrow().len() < self.max_concurrent() {
            let stopping = self
                .stopping
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let next = self
                .owners()
                // A discard still tearing down owns its destination: the row stays Queued until the reservation clears.
                .find(|it| {
                    it.status() == DownloadStatus::Queued
                        && !stopping.contains(&it.id())
                        && !self.dest_reserved(&it.file_path())
                });
            match next {
                Some(item) => {
                    if item.is_gallery() {
                        self.spawn_gallery(item);
                    } else if matches!(
                        self.video_source(item.id()),
                        Some(crate::media_types::VideoSource::Playlist { .. })
                    ) {
                        self.spawn_video_playlist(item);
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
            Some(
                crate::media_types::VideoSource::Page { .. }
                    | crate::media_types::VideoSource::Manifest { .. }
                    | crate::media_types::VideoSource::Playlist { .. }
            )
        );
        let segments = self.segment_state.borrow().get(&id).cloned();
        let started = self.started.borrow().contains(&id);
        // Reserve the target and a scheduler slot while filesystem validation is in flight.
        // This keeps another queued row or a rapid retry from claiming the same path,
        // and prevents async preparation from bypassing max_concurrent.
        self.reserve_dest(&dest);
        self.preparing.borrow_mut().insert(id);
        let weak = Rc::downgrade(self);
        let task_weak = weak.clone();
        let handle = glib::spawn_future_local(async move {
            async move {
                let worker_dest = dest.clone();
                let worker_dir = dest_dir.clone();
                let worker_recorded = item.output_dir().to_string();
                let worker_segments = segments.clone();
                let preparation = gio::spawn_blocking(move || {
                    let parent = worker_dest.parent();
                    if let Some(parent) = parent {
                        // Re-check containment on the worker immediately before touching the
                        // path: a symlink planted after intake must not redirect the write.
                        match ensure_contained_parent(parent, &worker_dir) {
                            Ok(true) => {}
                            Ok(false) => {
                                return Err(gettext("Download path escapes the download folder"));
                            }
                            Err(error) => {
                                return Err(gettext(
                                    "Could not create the download folder: {error}",
                                )
                                .replace("{error}", &error.to_string()));
                            }
                        }
                    }

                    if is_torrent {
                        let dir = std::path::PathBuf::from(&worker_dir);
                        let recorded_path = std::path::PathBuf::from(&worker_recorded);
                        let (output, output_dir) = if worker_recorded.is_empty() {
                            (dir.clone(), None)
                        } else {
                            // Re-guard the recorded folder at spawn time; a symlink may have
                            // been planted after intake. Never follow it outside the destination.
                            let guarded =
                                crate::torrent::guard_output_folder(&dir, recorded_path.clone());
                            let output_dir = (guarded != recorded_path)
                                .then(|| guarded.to_string_lossy().into_owned());
                            if output_dir.is_none() && guarded.parent() != Some(dir.as_path()) {
                                std::fs::create_dir_all(&guarded).map_err(|error| {
                                    gettext("Could not create the download folder: {error}")
                                        .replace("{error}", &error.to_string())
                                })?;
                            }
                            (guarded, output_dir)
                        };
                        if worker_recorded.is_empty() {
                            std::fs::create_dir_all(&dir).map_err(|error| {
                                gettext("Could not create the download folder: {error}")
                                    .replace("{error}", &error.to_string())
                            })?;
                        }
                        return Ok(PreparedSpawn::Torrent {
                            dest: output,
                            output_dir,
                        });
                    }
                    if is_video {
                        return Ok(PreparedSpawn::Video);
                    }

                    let mut drop_segments = false;
                    let mode = match worker_segments {
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
                        // Existing destination is refused for a fresh row; never overwrite a
                        // file that may belong to the user or another application.
                        None => match std::fs::metadata(&worker_dest) {
                            Err(_) => StartMode::Fresh,
                            Ok(_) if !started => {
                                return Err(DEST_EXISTS.to_string());
                            }
                            Ok(_) => StartMode::Single,
                        },
                    };
                    Ok(PreparedSpawn::Plain {
                        mode,
                        drop_segments,
                    })
                })
                .await;

                let Some(manager) = weak.upgrade() else {
                    return;
                };
                manager.preparing.borrow_mut().remove(&id);
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

                let prepared = match preparation {
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
                if matches!(
                    &prepared,
                    PreparedSpawn::Plain {
                        drop_segments: true,
                        ..
                    }
                ) {
                    manager.segment_state.borrow_mut().remove(&id);
                    manager.persist_queue();
                }
                manager.spawn_prepared(current, prepared);
            }
            .await;
            if let Some(manager) = task_weak.upgrade() {
                manager.preparation_handles.borrow_mut().remove(&id);
            }
        });
        // Insert after creating the task, before the main context can poll it.
        if let Some(old) = self.preparation_handles.borrow_mut().insert(id, handle) {
            old.abort();
        }
    }

    fn spawn_prepared(self: &Rc<Self>, item: DownloadItem, prepared: PreparedSpawn) {
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
            self.start_next();
            return;
        }
        publish_rate_limit(&self.settings);
        let url = item.url().to_string();
        let mode = match prepared {
            PreparedSpawn::Torrent { dest, output_dir } => {
                if let Some(output_dir) = output_dir {
                    item.set_output_dir(output_dir);
                }
                return self.spawn_torrent(item, url, dest);
            }
            PreparedSpawn::Video => match self.video_source(item.id()) {
                Some(crate::media_types::VideoSource::Page {
                    page_url,
                    quality,
                    audio_only,
                    is_live,
                    video_format_id,
                    playlist_item_id,
                    ..
                }) => {
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
                Some(crate::media_types::VideoSource::Manifest { source }) => {
                    return self.spawn_manifest(item, source);
                }
                Some(crate::media_types::VideoSource::Playlist { .. }) => {
                    return self.spawn_video_playlist(item);
                }
                _ => {
                    let detail = gettext("Could not prepare video source");
                    item.set_detail(detail);
                    item.set_status(DownloadStatus::Failed);
                    self.changed();
                    self.start_next();
                    return;
                }
            },
            PreparedSpawn::Plain { mode, .. } => mode,
        };
        let connections = (opts.connections.max(1) as usize).min(16);
        let timeout = Duration::from_secs(30);
        // Restore the visible fraction from the saved bitmap before starting the
        // resumed engine; preparation runs off-thread but UI state remains main-thread-owned.
        if let StartMode::Resume(state) = &mode {
            let fraction =
                (state.completed_bytes() as f64 / state.total.max(1) as f64).clamp(0.0, 1.0);
            item.set_progress(fraction);
        }
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
                self.start_next();
                return;
            }
        };
        let client = match http_client_for(proxy.as_ref()) {
            Ok(c) => c,
            Err(e) => {
                item.set_detail(e);
                item.set_status(DownloadStatus::Failed);
                self.changed();
                self.start_next();
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
        let resuming = matches!(&mode, StartMode::Resume(_));
        let handle = tokio_rt().spawn(run_download(ctx, connections, mode));
        self.running.borrow_mut().insert(item.id(), handle);
        // The engine owns the destination from here: later spawns resume it rather than refusing as foreign.
        // Persist immediately: a force-close before the next periodic save must not lose the ownership claim.
        self.started.borrow_mut().insert(item.id());
        self.persist_queue();
        item.set_status(DownloadStatus::Downloading);
        // Fresh attempts pulse while connecting; a segmented resume keeps its
        // restored completed-byte fraction visible instead of flashing back to zero.
        if !resuming {
            item.set_progress(0.0);
        }
        let host = url::Url::parse(&item.url())
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
            .unwrap_or_default();
        item.set_detail(if resuming {
            gettext("Resuming download…")
        } else if host.is_empty() {
            gettext("Starting…")
        } else {
            gettext("Connecting to {host}…").replace("{host}", &host)
        });
        self.changed();

        let item_id = item.id();
        self.pump(item, item_id, generation, rx);
    }

    /// Claim one of the pre-reserved candidates without replacing an existing file.
    /// The candidate list is computed on the GTK thread; this worker receives only paths and strings.
    pub(super) fn adopt_final_name_candidates(
        old_path: &std::path::Path,
        dir: &std::path::Path,
        candidates: &[String],
    ) -> Option<String> {
        for candidate in candidates {
            let target = dir.join(candidate);
            if old_path == target {
                return Some(candidate.clone());
            }
            match rename_noreplace(old_path, &target) {
                Ok(()) => return Some(candidate.clone()),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => return None,
            }
        }
        None
    }

    /// Whether a finished-name candidate is taken (file on disk, reserved stem, or live row holding it). Used by the Finished claim loop.
    pub(super) fn is_name_taken(&self, dir: &str, existing: &[String], n: &str) -> bool {
        existing.iter().any(|entry| entry == n)
            || crate::video_staging::stem_reserved_in(existing, name_stem(n))
            // A discard still tearing down owns this destination and its
            // stem: the finalizer's sweep (and the orphan removal when the
            // commit won) must not meet a row that claimed the name meanwhile.
            || self.dest_reserved(&std::path::Path::new(dir).join(n))
            || self.items()
                .any(|it| it.dest_dir() == dir && it.filename() == n)
    }

    /// Reserve up to eight collision-free names before yielding the main context to the filesystem worker.
    pub(super) fn reserve_final_name_candidates(
        &self,
        dir: &str,
        wanted: &str,
        existing: &[String],
    ) -> Option<(Vec<String>, FinalNameReservations)> {
        let row_names = self
            .items()
            .map(|item| (item.dest_dir().to_string(), item.filename().to_string()))
            .collect::<Vec<_>>();
        let reservations = std::sync::Arc::clone(&self.reservations);
        let mut reserved = reservations.lock().ok()?;
        let mut candidates = Vec::with_capacity(8);
        let directory = std::path::Path::new(dir);

        for _ in 0..8 {
            let candidate = dedupe_filename(wanted, |name| {
                existing.iter().any(|entry| entry == name)
                    || crate::video_staging::stem_reserved_in(existing, name_stem(name))
                    || row_names
                        .iter()
                        .any(|(row_dir, row_name)| row_dir == dir && row_name == name)
                    || reserved.contains(&directory.join(name))
                    || candidates.iter().any(|claimed| claimed == name)
            });
            candidates.push(candidate);
        }

        let paths = candidates
            .iter()
            .map(|candidate| directory.join(candidate))
            .collect::<Vec<_>>();
        for path in &paths {
            reserved.insert(path);
        }
        drop(reserved);

        Some((
            candidates,
            FinalNameReservations {
                reservations,
                paths,
            },
        ))
    }

    /// Drain one engine's message channel into its row. Shared by the HTTP and torrent engines.
    pub(super) fn pump(
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
            let mut last_playlist_progress_refresh: Option<Instant> = None;
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
                            // Drop the strong manager reference across the blocking directory
                            // listing. Re-upgrade into the arm's continuing binding afterward.
                            let (this, pending, existing) = if let Some(name) = pending {
                                // Re-apply the final name policy before yielding; idempotent on finalized names.
                                let name = this.finalize_filename(&name);
                                let current = item.filename().to_string();
                                let dir = item.dest_dir().to_string();
                                let listing_dir = dir.clone();
                                drop(this);
                                let existing = match gio::spawn_blocking(move || {
                                    crate::video_staging::dir_file_names(std::path::Path::new(
                                        &listing_dir,
                                    ))
                                })
                                .await
                                {
                                    Ok(names) => names,
                                    Err(error) => {
                                        tracing::warn!(error = ?error, dir = %dir, "finished-name directory listing failed");
                                        Vec::new()
                                    }
                                };
                                let Some(this) = weak.upgrade() else {
                                    done = true;
                                    break;
                                };
                                if !this.is_current(id, generation) {
                                    done = true;
                                    break;
                                }
                                (this, Some((name, current, dir)), Some(existing))
                            } else {
                                (this, None, None)
                            };
                            if let Some((name, current, dir)) = pending {
                                let existing = existing.unwrap_or_default();
                                // The main context was yielded while the directory was read. Do not finalize a row that was
                                // paused/cancelled, removed, or replaced while the worker ran.
                                if item.status() == DownloadStatus::Cancelled
                                    || item.status() == DownloadStatus::Paused
                                {
                                    done = true;
                                    break;
                                }
                                // If the current file is present, the collision path must
                                // claim-then-move it. Keep GTK objects on this context and send
                                // only plain paths/candidate names to the blocking worker.
                                if existing.iter().any(|entry| entry == &current) {
                                    match this.reserve_final_name_candidates(&dir, &name, &existing)
                                    {
                                        Some((candidates, mut claims)) => {
                                            let old_path = item.file_path();
                                            let worker_old_path = old_path.clone();
                                            let worker_dir = std::path::PathBuf::from(dir.as_str());
                                            #[cfg(test)]
                                            let after_move_hook =
                                                this.final_name_after_move_hook.borrow_mut().take();
                                            drop(this);
                                            let worker_result = gio::spawn_blocking(move || {
                                                let adopted = Self::adopt_final_name_candidates(
                                                    &worker_old_path,
                                                    &worker_dir,
                                                    &candidates,
                                                );
                                                #[cfg(test)]
                                                if let Some(hook) = after_move_hook {
                                                    hook();
                                                }
                                                adopted
                                            })
                                            .await;
                                            let adopted = match worker_result {
                                                Ok(name) => name,
                                                Err(error) => {
                                                    tracing::warn!(
                                                        item_id = id,
                                                        error = ?error,
                                                        "finished-name rename worker failed"
                                                    );
                                                    None
                                                }
                                            };
                                            let Some(this) = weak.upgrade() else {
                                                // The manager is gone. Restore the original path
                                                // when possible rather than leaving an untracked move.
                                                if let Some(final_name) = adopted {
                                                    let moved_path = std::path::Path::new(&dir)
                                                        .join(&final_name);
                                                    let rollback_old_path = old_path.clone();
                                                    let rollback = gio::spawn_blocking(move || {
                                                        rename_noreplace(
                                                            &moved_path,
                                                            &rollback_old_path,
                                                        )
                                                    })
                                                    .await;
                                                    if !matches!(rollback, Ok(Ok(()))) {
                                                        item.set_filename(final_name);
                                                    }
                                                }
                                                done = true;
                                                break;
                                            };
                                            let current_attempt = this.is_current(id, generation);
                                            let stopped = matches!(
                                                item.status(),
                                                DownloadStatus::Cancelled | DownloadStatus::Paused
                                            );
                                            if !current_attempt || stopped {
                                                // Preserve the pre-yield path on removal/pause
                                                // when possible. If an external file took it,
                                                // retain the moved path for discard cleanup.
                                                let row_removed = this.find(id).is_none();
                                                let update_detached_path = stopped || row_removed;
                                                if let Some(final_name) = adopted {
                                                    let moved_path = std::path::Path::new(&dir)
                                                        .join(&final_name);
                                                    let rollback_moved_path = moved_path.clone();
                                                    let rollback_old_path = old_path.clone();
                                                    drop(this);
                                                    let rollback = gio::spawn_blocking(move || {
                                                        rename_noreplace(
                                                            &rollback_moved_path,
                                                            &rollback_old_path,
                                                        )
                                                    })
                                                    .await;
                                                    if !matches!(rollback, Ok(Ok(())))
                                                        && update_detached_path
                                                    {
                                                        item.set_filename(final_name);
                                                        if row_removed {
                                                            // Removal already reserved old_path. Keep the
                                                            // new path until its discard finalizer finishes.
                                                            claims.retain_path(&moved_path);
                                                        }
                                                    }
                                                }
                                                done = true;
                                                break;
                                            }
                                            if let Some(final_name) = adopted
                                                && final_name != current
                                            {
                                                item.set_filename(final_name);
                                            }
                                        }
                                        None => {
                                            tracing::warn!(
                                                item_id = id,
                                                "could not reserve a finished filename candidate; keeping the current name"
                                            );
                                        }
                                    }
                                } else {
                                    // The engine no longer has the old path; only update the
                                    // suggested name using the original in-memory dedupe policy.
                                    let final_name = dedupe_filename(&name, |candidate| {
                                        this.is_name_taken(&dir, &existing, candidate)
                                    });
                                    if final_name != current {
                                        item.set_filename(final_name);
                                    }
                                }
                            }
                            let Some(this) = weak.upgrade() else {
                                done = true;
                                break;
                            };
                            // Torrent stats report the complete metainfo size, including files
                            // the user deselected. Their bytes are removed below, so measure the
                            // output only after cleanup. Other engines' size remains authoritative.
                            let cleanup_torrent = crate::torrent::is_torrent(&url);
                            if cleanup_torrent {
                                item.set_detail(gettext("Finished"));
                            } else {
                                // The engine measured the pre-rename path; the on-disk size
                                // refines this after final-name adoption.
                                this.set_finished_detail(&item, size);
                            }
                            // Server-date stamping and torrent directory cleanup can perform filesystem
                            // metadata reads and recursive walks; snapshot plain inputs and defer them.
                            let mtime = this.server_mtime.borrow_mut().remove(&id);
                            let keep_server_date = this.settings.keep_server_date();
                            // Snapshot the filter before dropping its row-scoped state.
                            let cleanup_selection = cleanup_torrent
                                .then(|| crate::torrent::get_selection(&url))
                                .flatten();
                            let finished_path = item.file_path();
                            let torrent_dest_dir = item.dest_dir().to_string();
                            let torrent_output_dir = item.output_dir().to_string();
                            let torrent_filename = item.filename().to_string();
                            let torrent_sizing = Rc::clone(&this.sizing_inflight);
                            if cleanup_torrent {
                                torrent_sizing.set(torrent_sizing.get() + 1);
                            }
                            let weak_manager = Rc::downgrade(&this);
                            let weak_item = item.downgrade();
                            item.set_progress(1.0);
                            item.set_status(DownloadStatus::Done);
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
                                    let measured_size = gio::spawn_blocking(move || {
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
                                            crate::torrent::cleanup_unselected_and_measure(
                                                &folder,
                                                &worker_url,
                                                cleanup_selection.as_deref(),
                                            )
                                        } else {
                                            None
                                        }
                                    })
                                    .await;
                                    if cleanup_torrent {
                                        torrent_sizing.set(torrent_sizing.get().saturating_sub(1));
                                        if let (Ok(Some(size)), Some(manager), Some(item)) = (
                                            measured_size,
                                            weak_manager.upgrade(),
                                            weak_item.upgrade(),
                                        ) && manager.find(id).is_some()
                                        {
                                            item.set_detail(
                                                gettext("Finished • {size}")
                                                    .replace("{size}", &fmt_bytes(size)),
                                            );
                                        }
                                    }
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
                        let weak = Rc::downgrade(&this);
                        drop(this);
                        let (added, total, skipped_by_host_limit) =
                            Self::expand_playlist_rows_async(
                                weak.clone(),
                                id,
                                generation,
                                &item,
                                &pl,
                            )
                            .await;
                        let Some(this) = weak.upgrade() else {
                            done = true;
                            break;
                        };
                        if !this.is_current(id, generation) {
                            done = true;
                            break;
                        }
                        let skipped_detail = || {
                            ngettext(
                                "{skipped} item skipped because a public-host protection limit was reached",
                                "{skipped} items skipped because a public-host protection limit was reached",
                                skipped_by_host_limit as u32,
                            )
                            .replace("{skipped}", &skipped_by_host_limit.to_string())
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
                        }
                        done = true;
                        break;
                    }
                    EngineMsg::ManifestFallback(error) => {
                        this.pending_names.borrow_mut().remove(&id);
                        this.server_mtime.borrow_mut().remove(&id);
                        if matches!(
                            item.status(),
                            DownloadStatus::Cancelled | DownloadStatus::Paused
                        ) {
                            done = true;
                            break;
                        }

                        tracing::debug!(
                            item_id = id,
                            reason = %crate::backend_debug::redact_query_tokens(&error),
                            "native manifest is unsupported; falling back to yt-dlp"
                        );

                        // Remove only files carrying this row's native-manifest
                        // ownership marker before switching the row to yt-dlp.
                        let directory = std::path::PathBuf::from(item.dest_dir());
                        let manifest_id = id;
                        let cleanup = gio::spawn_blocking(move || {
                            crate::video_staging::clean_manifest_staging_files(
                                &directory,
                                manifest_id,
                            );
                        });
                        drop(this);
                        if let Err(error) = cleanup.await {
                            tracing::warn!(
                                item_id = id,
                                error = ?error,
                                "native manifest staging cleanup failed before yt-dlp fallback"
                            );
                        }
                        let Some(this) = weak.upgrade() else {
                            done = true;
                            break;
                        };
                        if !this.is_current(id, generation) {
                            done = true;
                            break;
                        }
                        if matches!(
                            item.status(),
                            DownloadStatus::Cancelled | DownloadStatus::Paused
                        ) {
                            done = true;
                            break;
                        }

                        this.video_sources.borrow_mut().insert(
                            id,
                            crate::media_types::VideoSource::Page {
                                page_url: url.clone(),
                                media_url: None,
                                expires_at: None,
                                quality: this.settings.video_quality(),
                                audio_only: false,
                                is_live: false,
                                video_format_id: None,
                                playlist_item_id: None,
                                backend: None,
                            },
                        );
                        item.set_detail(
                            gettext("Native stream failed ({error}); trying yt-dlp…")
                                .replace("{error}", &error),
                        );
                        item.set_progress(0.0);
                        item.set_status(DownloadStatus::Queued);
                        this.persist_queue();
                        done = true;
                        break;
                    }
                    EngineMsg::ManifestRetryable(error) => {
                        this.pending_names.borrow_mut().remove(&id);
                        this.server_mtime.borrow_mut().remove(&id);
                        if matches!(
                            item.status(),
                            DownloadStatus::Cancelled | DownloadStatus::Paused
                        ) {
                            done = true;
                            break;
                        }

                        // Preserve the native source and completed staging resources.
                        // DownloadManager::retry will re-enter the native runner.
                        item.set_detail(
                            gettext("Temporary stream failure: {error}. Retry to resume.")
                                .replace("{error}", &error),
                        );
                        item.set_status(DownloadStatus::Failed);
                        this.torrent_pieces.borrow_mut().remove(&id);
                        this.persist_queue();
                        this.notify_finished(&item, Err(error));
                        this.changed();
                        done = true;
                        break;
                    }
                    EngineMsg::Failed(e) => {
                        if item.is_collection_owner() {
                            this.finish_video_playlist_file_rows(id, DownloadStatus::Failed);
                        }
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
                        drop(this);
                        let _ =
                            gio::spawn_blocking(move || std::fs::remove_file(worker_path)).await;
                        let Some(this) = weak.upgrade() else {
                            done = true;
                            break;
                        };
                        this.release_dest(&invalid_path);
                        if !this.is_current(id, generation) {
                            done = true;
                            break;
                        }
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
                        drop(this);
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
                    EngineMsg::PlaylistFile(event) => match *event {
                        crate::engine_msg::PlaylistFileEvent::Started {
                            file_id,
                            filename: _filename,
                        } => {
                            if item.is_collection_owner()
                                && item.status() == DownloadStatus::Downloading
                            {
                                this.upsert_video_playlist_file_row(
                                    &item,
                                    &file_id,
                                    DownloadStatus::Downloading,
                                    0.0,
                                    &gettext("Downloading"),
                                    None,
                                );
                                this.persist_queue();
                                this.changed();
                                last_playlist_progress_refresh = Some(Instant::now());
                            }
                        }
                        crate::engine_msg::PlaylistFileEvent::Progress {
                            file_id,
                            downloaded,
                            total,
                        } => {
                            if item.is_collection_owner()
                                && item.status() == DownloadStatus::Downloading
                                && last_playlist_progress_refresh
                                    .is_none_or(|last| last.elapsed() >= Duration::from_millis(100))
                            {
                                let progress = total
                                    .filter(|total| *total > 0)
                                    .map(|total| (downloaded as f64 / total as f64).clamp(0.0, 1.0))
                                    .unwrap_or(0.0);
                                let detail = total
                                    .filter(|total| *total > 0)
                                    .map(|total| {
                                        format!(
                                            "{} • {} / {}",
                                            (progress * 100.0) as u64,
                                            fmt_bytes(downloaded),
                                            fmt_bytes(total)
                                        )
                                    })
                                    .unwrap_or_else(|| fmt_bytes(downloaded));
                                this.upsert_video_playlist_file_row(
                                    &item,
                                    &file_id,
                                    DownloadStatus::Downloading,
                                    progress,
                                    &detail,
                                    None,
                                );
                                this.changed();
                                last_playlist_progress_refresh = Some(Instant::now());
                            }
                        }
                        crate::engine_msg::PlaylistFileEvent::Finished {
                            file_id,
                            path,
                            size,
                        } => {
                            if item.is_collection_owner() {
                                this.upsert_video_playlist_file_row(
                                    &item,
                                    &file_id,
                                    DownloadStatus::Done,
                                    1.0,
                                    &gettext("Finished • {size}")
                                        .replace("{size}", &fmt_bytes(size)),
                                    Some(&path),
                                );
                                this.persist_queue();
                                this.changed();
                                last_playlist_progress_refresh = Some(Instant::now());
                            }
                        }
                        crate::engine_msg::PlaylistFileEvent::Failed { file_id, error } => {
                            if item.is_collection_owner() {
                                this.upsert_video_playlist_file_row(
                                    &item,
                                    &file_id,
                                    DownloadStatus::Failed,
                                    0.0,
                                    &error,
                                    None,
                                );
                                this.persist_queue();
                                this.changed();
                                last_playlist_progress_refresh = Some(Instant::now());
                            }
                        }
                    },
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
                            drop(this);
                            let _ = gio::spawn_blocking(move || {
                                truncate_to_prefix(&worker_path, &state)
                            })
                            .await;
                            let Some(this) = weak.upgrade() else {
                                done = true;
                                break;
                            };
                            this.release_dest(&path);
                            if !this.is_current(id, generation) {
                                done = true;
                                break;
                            }
                            this.start_next();
                        }
                    }
                    EngineMsg::FallbackSingle { ack } => {
                        // Keep the bitmap persisted until the sparse/segmented file is
                        // actually truncated. A crash during the worker must still restore
                        // with the bitmap rather than append to a file with holes.
                        let segment_state = { this.segment_state.borrow().get(&id).cloned() };
                        if let Some(state) = segment_state {
                            let path = item.file_path();
                            this.reserve_dest(&path);
                            let worker_path = path.clone();
                            drop(this);
                            let truncated = gio::spawn_blocking(move || {
                                truncate_to_prefix(&worker_path, &state)
                            })
                            .await
                            .is_ok();
                            let Some(this) = weak.upgrade() else {
                                done = true;
                                break;
                            };
                            this.release_dest(&path);
                            if !this.is_current(id, generation) {
                                done = true;
                                break;
                            }
                            if truncated {
                                this.segment_state.borrow_mut().remove(&id);
                                this.persist_queue();
                                this.start_next();
                                drop(this);
                                ack.send(()).await.ok();
                            } else {
                                tracing::warn!(path = %path.display(), "could not truncate segmented file before single-stream fallback");
                                // Dropping ack tells the engine that fallback cannot safely proceed.
                                done = true;
                                break;
                            }
                        } else {
                            this.segment_state.borrow_mut().remove(&id);
                            this.persist_queue();
                            drop(this);
                            ack.send(()).await.ok();
                        }
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
                            Some(
                                crate::media_types::VideoSource::Page { .. }
                                    | crate::media_types::VideoSource::Manifest { .. }
                            )
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
                    EngineMsg::LiveFinalizing => {
                        if item.status() == DownloadStatus::Downloading {
                            this.live_rows.borrow_mut().remove(&id);
                            item.set_detail(gettext("Finalizing recording…"));
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
    pub(super) fn spawn_torrent(
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
                    item.set_detail(e.to_string());
                    item.set_status(DownloadStatus::Failed);
                    self.changed();
                    self.start_next();
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
                self.start_next();
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
        let is_file = crate::torrent::is_torrent_url(&magnet);
        let source = if is_file {
            match crate::torrent::archive_path_for_url(&magnet) {
                Some(path) => crate::torrent::TorrentSource::File(path),
                None => {
                    item.set_detail(gettext("Torrent file is missing from the archive"));
                    item.set_status(DownloadStatus::Failed);
                    self.changed();
                    self.start_next();
                    return;
                }
            }
        } else {
            crate::torrent::TorrentSource::Magnet(magnet)
        };
        // Source validation can fail before an engine or pump exists. Do not
        // register an attempt generation or channel until the source is ready.
        let id = item.id();
        let generation = self.epoch.borrow().get(&id).cloned().unwrap_or(0) + 1;
        self.epoch.borrow_mut().insert(id, generation);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
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

/// Validated worker preparation. The enum makes impossible combinations (such as a
/// torrent without an output directory or a plain row without a start mode) unrepresentable.
enum PreparedSpawn {
    Torrent {
        dest: std::path::PathBuf,
        output_dir: Option<String>,
    },
    Video,
    Plain {
        mode: StartMode,
        drop_segments: bool,
    },
}

/// Video-page spawn inputs, bundled so the field list doesn't trip clippy's too-many-arguments lint.
pub(super) struct SpawnVideoParams {
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
    pub(super) fn spawn_video(self: &Rc<Self>, params: SpawnVideoParams) {
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
        let opts = DownloadOptions::from_settings(&self.settings);
        // Validate before registering the attempt so a failed configuration
        // cannot leave a stale live-row marker, gate, or abort sender behind.
        let proxy = match opts.proxy_config() {
            Ok(proxy) => proxy,
            Err(error) => {
                item.set_detail(error);
                item.set_status(DownloadStatus::Failed);
                self.changed();
                self.start_next();
                return;
            }
        };

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

    /// Spawn the one yt-dlp process owned by a playlist collection. The
    /// parent owns the scheduler slot and pause/cancel/retry; child rows are
    /// only progress projections updated by its queue pump.
    fn spawn_video_playlist(self: &Rc<Self>, item: DownloadItem) {
        let id = item.id();
        let (page_url, quality, audio_only, state) = match self.video_source(id) {
            Some(crate::media_types::VideoSource::Playlist {
                page_url,
                quality,
                audio_only,
                state,
            }) => (page_url, quality, audio_only, state),
            _ => {
                item.set_detail(gettext("Could not prepare playlist source"));
                item.set_status(DownloadStatus::Failed);
                self.changed();
                self.start_next();
                return;
            }
        };
        let opts = DownloadOptions::from_settings(&self.settings);
        let proxy = match opts.proxy_config() {
            Ok(proxy) => proxy,
            Err(error) => {
                item.set_detail(error);
                item.set_status(DownloadStatus::Failed);
                self.changed();
                self.start_next();
                return;
            }
        };

        self.prepare_video_playlist_file_rows_for_attempt(&item);
        let generation = self.epoch.borrow().get(&id).copied().unwrap_or(0) + 1;
        self.epoch.borrow_mut().insert(id, generation);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (abort_tx, abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
        let gate = AttemptGate::new();
        self.gates.borrow_mut().insert(id, gate.clone());
        self.video_abort.borrow_mut().insert(id, abort_tx);
        self.live_rows.borrow_mut().remove(&id);

        let job = crate::video::VideoJob {
            item_id: id,
            page_url: page_url.clone(),
            playlist_item_id: None,
            quality,
            audio_only,
            dest: item.file_path(),
            speed_limit: parse_rate(opts.limit_rate.as_str()),
            keep_server_date: self.settings.keep_server_date(),
            video_format_id: None,
            is_live: false,
            live_from_start: false,
            newest_codecs: self.settings.video_codec_newest(),
            cookies_browser: self.settings.cookies_browser(),
            subtitles: if audio_only {
                None
            } else {
                crate::video_prefs::subtitle_lang_active(&self.settings.subtitle_language())
            },
            embed_subs: self.settings.embed_subs(),
            sponsorblock_remove: self.settings.sponsorblock_remove(),
            sponsorblock_mark: self.settings.sponsorblock_mark(),
            remux_video: if audio_only {
                None
            } else {
                crate::video_prefs::remux_video_active(&self.settings.remux_video())
            },
            embed_chapters: self.settings.embed_chapters(),
            proxy,
            info_json: None,
        };
        let handle = tokio_rt().spawn(async move {
            let worker_tx = tx.clone();
            match crate::video_runner::run_video_playlist_download(
                job, state, &gate, abort_rx, worker_tx,
            )
            .await
            {
                Ok(crate::video::VideoOutcome::Finished(size)) => {
                    tx.send(EngineMsg::Finished { size }).ok();
                }
                Ok(crate::video::VideoOutcome::Aborted) => {}
                Ok(crate::video::VideoOutcome::Expand(_)) => {
                    tx.send(EngineMsg::Failed(gettext(
                        "A playlist collection unexpectedly resolved to another collection",
                    )))
                    .ok();
                }
                Err(error) => {
                    tracing::warn!(
                        item_id = id,
                        error = %error,
                        "yt-dlp playlist collection failed"
                    );
                    tx.send(EngineMsg::Failed(error.to_string())).ok();
                }
            }
        });

        self.running.borrow_mut().insert(id, handle);
        item.set_status(DownloadStatus::Downloading);
        item.set_detail(gettext("Resolving playlist…"));
        self.changed();
        self.pump(item, id, generation, rx);
    }

    /// Spawn one native HLS/DASH attempt through the shared queue pump.
    fn spawn_manifest(
        self: &Rc<Self>,
        item: DownloadItem,
        source: crate::media_types::ManifestSource,
    ) {
        let opts = DownloadOptions::from_settings(&self.settings);
        let proxy = match opts.proxy_config() {
            Ok(proxy) => proxy,
            Err(error) => {
                item.set_detail(error);
                item.set_status(DownloadStatus::Failed);
                self.changed();
                self.start_next();
                return;
            }
        };

        let source_host = url::Url::parse(&source.url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned));
        if source_host
            .as_deref()
            .is_some_and(crate::download_net::is_untrusted_handoff_host)
            && let Err(error) = crate::subprocess_proxy::validate_proxy_compatibility()
        {
            item.set_detail(error);
            item.set_status(DownloadStatus::Failed);
            self.changed();
            self.start_next();
            return;
        }

        let id = item.id();
        let generation = self.epoch.borrow().get(&id).copied().unwrap_or(0) + 1;
        self.epoch.borrow_mut().insert(id, generation);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (abort_tx, abort_rx) = tokio::sync::oneshot::channel::<crate::video::StopIntent>();
        let gate = AttemptGate::new();
        self.gates.borrow_mut().insert(id, gate.clone());
        self.video_abort.borrow_mut().insert(id, abort_tx);
        self.live_rows.borrow_mut().remove(&id);

        let job = crate::manifest_runner::ManifestJob {
            item_id: id,
            source,
            dest: item.file_path(),
            proxy,
            cookies_browser: opts.cookies_browser.clone(),
            connections: opts.connections.max(1) as usize,
            embed_subs: self.settings.embed_subs(),
        };
        let handle = tokio_rt().spawn(async move {
            match crate::manifest_runner::run_manifest_download(job, gate, abort_rx, tx.clone())
                .await
            {
                Ok(crate::manifest_runner::ManifestOutcome::Finished(size)) => {
                    tx.send(EngineMsg::Finished { size }).ok();
                }
                Ok(crate::manifest_runner::ManifestOutcome::Paused) => {}
                Ok(crate::manifest_runner::ManifestOutcome::Aborted) => {}
                Err(error) => {
                    tracing::warn!(
                        item_id = id,
                        error = %error,
                        "native manifest attempt did not complete"
                    );
                    tx.send(error.into_engine_message()).ok();
                }
            }
        });
        self.running.borrow_mut().insert(id, handle);
        let resuming = self.started.borrow().contains(&id);
        self.started.borrow_mut().insert(id);
        self.persist_queue();
        item.set_status(DownloadStatus::Downloading);
        item.set_progress(0.0);
        item.set_detail(if resuming {
            gettext("Resuming stream download…")
        } else {
            gettext("Resolving stream manifest…")
        });
        self.changed();
        self.pump(item, id, generation, rx);
    }
}

impl DownloadManager {
    /// Spawn one gallery-dl process for the collection owner.
    /// Progress and file lifecycle events are bridged to the GTK main thread;
    /// the handle owns collection-wide pause, cancel and concurrency control.
    fn spawn_gallery(self: &Rc<Self>, item: DownloadItem) {
        self.prepare_gallery_file_rows_for_attempt(item.id());
        item.set_status(DownloadStatus::Downloading);
        // Clear previous error log on (re)start.
        item.set_error_log(String::new());
        self.changed();

        // Get the job (carries the user's filter). Kept in the map for retry.
        let job = match self.gallery_jobs.borrow().get(&item.id()).cloned() {
            Some(j) => j,
            None => {
                // Detail before status: the status notify fires synchronously.
                item.set_detail(gettext("Gallery job not found"));
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
            let (handoff_path, handoff_ua) =
                crate::extension_server::live_handoff_identity(&job.page_url)
                    .map_or((None, None), |(path, ua)| (Some(path), ua));
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
                crate::extension_server::discard_handoff_cookies_async(&page_url);
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
            let mut last_progress_refresh = Instant::now();
            while let Some(msg) = rx.recv().await {
                match msg {
                    GalleryMsg::Progress(update) => {
                        let done = update.files_done;
                        let file_progress = update
                            .current_file_percent
                            .map(|percent| format!(" · {percent}%"))
                            .unwrap_or_default();
                        item_c.set_gallery_files_done(done);

                        // The collection UI is backed by transient child rows
                        // keyed by the exact output path announced by gallery-dl.
                        match update.event.as_ref() {
                            Some(crate::gallery_progress::ProgressLine::Started(path)) => {
                                this.upsert_gallery_file_row(
                                    &item_c,
                                    path,
                                    DownloadStatus::Downloading,
                                    0.0,
                                    &gettext("Downloading"),
                                );
                            }
                            Some(crate::gallery_progress::ProgressLine::Completed(path)) => {
                                this.upsert_gallery_file_row(
                                    &item_c,
                                    path,
                                    DownloadStatus::Done,
                                    1.0,
                                    &gettext("Finished"),
                                );
                            }
                            Some(crate::gallery_progress::ProgressLine::Skipped(path)) => {
                                this.upsert_gallery_file_row(
                                    &item_c,
                                    path,
                                    DownloadStatus::Done,
                                    1.0,
                                    &gettext("Already exists"),
                                );
                            }
                            _ => {}
                        }
                        if update.event.is_none()
                            && let Some(path) = update.current_file_path.as_deref()
                        {
                            this.upsert_gallery_file_row(
                                &item_c,
                                path,
                                DownloadStatus::Downloading,
                                update
                                    .current_file_percent
                                    .map(|percent| f64::from(percent) / 100.0)
                                    .unwrap_or(0.0),
                                &update
                                    .current_file_percent
                                    .map(|percent| format!("{percent}%"))
                                    .unwrap_or_else(|| gettext("Downloading")),
                            );
                        }

                        // Show the last completed filename in the owner detail.
                        let file_part = update
                            .last_completed_file
                            .as_ref()
                            .and_then(|p| std::path::Path::new(p).file_name()?.to_str())
                            .map(|n| format!(" • {n}"))
                            .unwrap_or_default();
                        if let Some(fraction) = crate::gallery_progress::progress_fraction(
                            done,
                            update.current_file_percent,
                            total,
                        ) {
                            item_c.set_progress(fraction);
                            let done_text = done.to_string();
                            let total_text = total.to_string();
                            item_c.set_detail(format_gettext_template(
                                &gettext("Gallery \u{2022} {done}/{total} files{file_progress}{file_part}"),
                                &[
                                    ("done", &done_text),
                                    ("total", &total_text),
                                    ("file_progress", &file_progress),
                                    ("file_part", &file_part),
                                ],
                            ));
                        } else {
                            item_c.set_progress(0.0);
                            let done_text = done.to_string();
                            item_c.set_detail(format_gettext_template(
                                &gettext("Gallery \u{2022} {done} files{file_progress}{file_part}"),
                                &[
                                    ("done", &done_text),
                                    ("file_progress", &file_progress),
                                    ("file_part", &file_part),
                                ],
                            ));
                        }
                        // Row and owner properties are updated immediately.
                        // Recount and rebuild the grouped view at most 10 Hz;
                        // lifecycle events still refresh synchronously.
                        if update.event.is_some()
                            || last_progress_refresh.elapsed() >= Duration::from_millis(100)
                        {
                            this.changed();
                            last_progress_refresh = Instant::now();
                        }
                        // Persist file identity/status transitions, not every
                        // high-frequency byte-progress update.
                        if update.event.is_some() {
                            this.persist_queue();
                        }
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
                                    crate::gallery_runner::done_count(r.files_completed, r.skipped);
                                let detail = if r.exit_code.is_some() {
                                    gettext("Gallery \u{2022} some failed")
                                } else {
                                    gettext("Gallery")
                                };
                                item_c.set_gallery_files_done(done);
                                let progress = if r.exit_code.is_some() {
                                    crate::gallery_progress::progress_fraction(
                                        done,
                                        None,
                                        item_c.gallery_file_count(),
                                    )
                                    .unwrap_or_else(|| item_c.progress())
                                } else {
                                    1.0
                                };
                                item_c.set_progress(progress);
                                item_c.set_detail(detail);
                                // Partial failure (exit_code.is_some()) is marked
                                // Failed so it's retryable via retry_failed().
                                // Clean success is Done.
                                if r.exit_code.is_some() {
                                    this.finish_gallery_file_rows(
                                        item_c.id(),
                                        DownloadStatus::Failed,
                                    );
                                    item_c.set_status(DownloadStatus::Failed);
                                } else {
                                    this.finish_gallery_file_rows(
                                        item_c.id(),
                                        DownloadStatus::Done,
                                    );
                                    item_c.set_status(DownloadStatus::Done);
                                }
                                tracing::info!(
                                    files = r.files_completed,
                                    skipped = r.skipped,
                                    "gallery download complete"
                                );
                            }
                            Err(e) => {
                                // Set detail/error_log BEFORE status: the status
                                // notify fires synchronously and opens the dialog,
                                // which reads these fields.
                                let summary = e.lines().next().unwrap_or(&e).trim();
                                item_c.set_detail(format_gettext_template(
                                    &gettext("Gallery failed: {summary}"),
                                    &[("summary", summary)],
                                ));
                                item_c.set_error_log(e.clone());
                                this.finish_gallery_file_rows(item_c.id(), DownloadStatus::Failed);
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
}
