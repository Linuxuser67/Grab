//! Download cancellation, worker teardown and discard cleanup.

use super::{
    AttemptGate, DownloadItem, DownloadManager, DownloadStatus, PendingDiscard, Stop, wake_queue,
};
use gtk4::gio::prelude::*;
use gtk4::{gio, glib};
use std::collections::HashSet;
use std::rc::Rc;

/// Remove a discarded download's owned files, preserving native ownership records on error.
pub(super) fn cleanup_discarded_video_files(dest: &std::path::Path, id: u64) -> Result<(), String> {
    if let Some(dest_dir) = dest.parent() {
        if let Some(staging) = crate::video_staging::manifest_staging_dir_for_cleanup(dest_dir, id)?
        {
            crate::manifest_destination::remove_planned_parts(&staging, dest_dir, id)?;
        }
        crate::video::clean_staging_files(dest_dir, id);
        // Native/live remux temporaries are lease-owned scratch, not resumable
        // recording data. Reclaim them only after the attempt's tracked
        // blocking workers have finished.
        crate::video_staging::sweep_partial_remuxes(dest_dir, id);
    }
    // Legacy destination-side part names have no reliable ownership marker.
    // Keep them rather than deleting user files based on a filename pattern.
    Ok(())
}

/// Reclaim the removed row's filesystem scratch on Tokio's blocking pool.
/// The caller retains the destination reservation until this future completes.
async fn cleanup_discarded_files(dest: std::path::PathBuf, id: u64, remove_delivered_orphan: bool) {
    let worker_dest = dest.clone();
    if let Err(error) = crate::runtime::tokio_rt()
        .spawn_blocking(move || {
            if let Err(error) = cleanup_discarded_video_files(&worker_dest, id) {
                tracing::warn!(
                    item_id = id,
                    error = %error,
                    "discard cleanup failed; preserving native manifest ownership metadata"
                );
            }
            if remove_delivered_orphan {
                // The worker committed after its row was removed. This is the
                // narrowly scoped orphan case; normal finished files survive.
                let _ = std::fs::remove_file(&worker_dest);
            }
        })
        .await
    {
        tracing::warn!(item_id = id, error = ?error, "discard cleanup worker failed");
    }
}

impl DownloadManager {
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
    pub(super) fn stop_video_worker(&self, id: u64, intent: crate::video::StopIntent) -> bool {
        self.video_abort
            .borrow_mut()
            .remove(&id)
            .is_some_and(|stop| stop.send(intent).is_ok())
    }

    /// Stop the engine for `id` without touching the row. Live rows receive the requested intent and complete through the pump; returns whether the engine was actually stopped.
    pub(super) fn stop_engine(&self, id: u64, intent: crate::video::StopIntent) -> bool {
        self.stop_video_worker(id, intent);
        if self.live_rows.borrow().contains(&id) {
            return false;
        }
        if let Some(handle) = self.running.borrow_mut().remove(&id) {
            // Do not let a quick resume/retry start a second worker for this
            // row until the aborted task has actually been dropped. The row
            // does not occupy a global slot while it stops, so other queued
            // downloads can proceed.
            let stopping = std::sync::Arc::clone(&self.stopping);
            stopping
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(id);
            handle.abort();
            // Join aborted workers on Tokio rather than creating a GLib-local
            // continuation. The latter is thread-bound and can be reaped by a
            // different test thread when the main context is reused.
            crate::runtime::tokio_rt().spawn(async move {
                let _ = handle.await;
                stopping
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id);
                wake_queue();
            });
        }
        true
    }

    /// Stop the engine for `id`, keeping file, bitmap and progress, and mark it queued. Live rows only signal (see `stop_engine`).
    pub(super) fn park(&self, id: u64) {
        // Live rows only signal: the worker finalizes and its message completes the row.
        if !self.stop_engine(id, crate::video::StopIntent::Finish) {
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
    pub(super) fn preempt_excess(&self) {
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
        if let Some(item) = self.find(id) {
            if item.is_gallery() {
                self.finish_gallery_file_rows(id, DownloadStatus::Cancelled);
            }
            if item.is_collection_owner() {
                self.finish_video_playlist_file_rows(id, DownloadStatus::Cancelled);
            }
        }
        self.persist_queue();
        self.changed();
        self.start_next();
    }

    /// Reclaim a removed video row's scratch, but only once its worker has actually stopped. Removal cannot sweep inline: a worker mid-teardown can recreate what a sweep removed, so unlinking first just loses the race. Claiming the gate first makes deferring safe: only a worker this finalizer awaited may have committed an orphan `dest` file.
    /// Engine invariant: a finished file is never deleted. The `remove_file` below is the single exception — a commit that won the race against its row's discard, leaving an orphan no row can own.
    pub(super) fn finish_discard(
        &self,
        id: u64,
        dest: std::path::PathBuf,
        gate: std::sync::Arc<AttemptGate>,
    ) {
        let _ = gate.discard();
        // Staging files live visibly in the dest dir as `grab-<id>-*`; no subfolder.
        // `self` (Rc, !Send) cannot go to the tokio runtime: carry reservations as an `Arc` clone.
        // Queue wakeups go through `wake_queue()` (thread-safe, no manager handle needed).
        let reservations = std::sync::Arc::clone(&self.reservations);
        // A final-name claim may be awaiting GIO's blocking pool. Wait for the
        // pump before cleanup, then use the row's final path rather than the
        // stale filename captured when Remove was clicked.
        let pump = self.pump_handles.borrow_mut().remove(&id);
        let pump_item = self.find(id);
        let pump_dest = dest.clone();
        let (pump_path_tx, pump_path_rx) = tokio::sync::oneshot::channel();
        if let Some(pump) = pump {
            glib::spawn_future_local(async move {
                let _ = pump.await;
                let final_dest = pump_item
                    .map(|item| item.file_path())
                    .unwrap_or(pump_dest.clone());
                let _ = pump_path_tx.send(final_dest);
            });
        } else {
            let final_dest = pump_item
                .map(|item| item.file_path())
                .unwrap_or_else(|| dest.clone());
            let _ = pump_path_tx.send(final_dest);
        }
        match self.running.borrow_mut().remove(&id) {
            Some(handle) => {
                let worker_abort = handle.abort_handle();
                let finalizer = crate::runtime::tokio_rt().spawn(async move {
                    let _ = handle.await;
                    let final_dest = pump_path_rx.await.unwrap_or_else(|_| dest.clone());
                    // A cancelled publisher may have detached its blocking rollback.
                    // Do not inspect delivery until that filesystem work has completed.
                    gate.wait_for_rollbacks().await;
                    let remove_delivered_orphan = gate.was_delivered();
                    cleanup_discarded_files(final_dest.clone(), id, remove_delivered_orphan).await;
                    if let Ok(mut r) = reservations.lock() {
                        r.remove(&dest);
                        if final_dest != dest {
                            r.remove(&final_dest);
                        }
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
                        worker_abort: Some(worker_abort),
                        finalizer,
                    },
                );
            }
            // No task to wait for: sweep the scratch, never the finished file
            // (no orphan is possible without a worker in flight). Run the
            // cleanup off GTK too; a large staging dir must not stall the UI.
            None => {
                let finalizer = crate::runtime::tokio_rt().spawn(async move {
                    let final_dest = pump_path_rx.await.unwrap_or_else(|_| dest.clone());
                    cleanup_discarded_files(final_dest.clone(), id, false).await;
                    if let Ok(mut r) = reservations.lock() {
                        r.remove(&dest);
                        if final_dest != dest {
                            r.remove(&final_dest);
                        }
                    }
                    wake_queue();
                });
                self.discards.borrow_mut().insert(
                    id,
                    PendingDiscard {
                        worker_abort: None,
                        finalizer,
                    },
                );
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
                let intent = match stop {
                    Stop::Preserve => crate::video::StopIntent::Finish,
                    Stop::Discard => crate::video::StopIntent::Discard,
                };
                self.stop_engine(id, intent);
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

    /// Reset one failed/cancelled row without persisting or waking the scheduler.
    fn requeue_failed_item(&self, item: &DownloadItem) -> bool {
        if !matches!(
            item.status(),
            DownloadStatus::Failed | DownloadStatus::Cancelled
        ) {
            return false;
        }
        let id = item.id();
        // A kept segment bitmap resumes where it left off, so leave the progress bar there.
        if !self.segment_state.borrow().contains_key(&id) {
            item.set_progress(0.0);
        }
        // Gallery downloads: gallery-dl skips existing files on retry
        // (file-level resume). Reset the UI counter; the runner
        // reports newly completed files via progress updates.
        let playlist_owner = matches!(
            self.video_source(id),
            Some(crate::media_types::VideoSource::Playlist { .. })
        );
        if item.is_gallery() {
            item.set_gallery_files_done(0);
            item.set_progress(0.0);
        } else if playlist_owner {
            self.refresh_video_playlist_summary(item);
        }
        if !playlist_owner {
            item.set_detail(String::new());
        }
        item.set_status(DownloadStatus::Queued);
        true
    }

    /// Re-queue a failed or cancelled download.
    pub fn retry(self: &Rc<Self>, id: u64) {
        if let Some(item) = self.find(id)
            && self.requeue_failed_item(&item)
        {
            self.persist_queue();
            self.changed();
            self.start_next();
        }
    }

    /// Remove every row in a collection from the list with one queue update.
    pub fn remove_many(self: &Rc<Self>, ids: &[u64]) -> usize {
        let mut removed = 0;
        for &id in ids {
            if self.find(id).is_none() {
                continue;
            }
            self.remove_inner(id);
            removed += 1;
        }
        if removed > 0 {
            self.persist_queue();
            self.changed();
            self.start_next();
        }
        removed
    }

    /// Remove a row from the queue. Non-video partials remain available to Undo;
    /// page-video and native-manifest staging is discarded after its worker stops,
    /// so Undo re-resolves or restarts those attempts from the beginning.
    pub fn remove(self: &Rc<Self>, id: u64) {
        self.remove_inner(id);
        self.persist_queue();
        self.changed();
        self.start_next();
    }

    /// Remove one row without publishing an intermediate queue state. Bulk
    /// collection removal calls this for every member, then persists once.
    pub(super) fn remove_inner(self: &Rc<Self>, id: u64) {
        let _ = self.cancel_inner(id, true, Stop::Discard);
        if self
            .find(id)
            .is_some_and(|item| item.is_collection_parent())
        {
            self.remove_collection_child_rows(&HashSet::from([id]));
        }
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
            crate::extension_server::discard_handoff_cookies_async(&job.page_url);
        }
        self.gallery_jobs.borrow_mut().remove(&id);
        self.gallery_file_row_index.borrow_mut().remove(&id);
        self.video_playlist_file_row_index.borrow_mut().remove(&id);
        let released = self.find(id).filter(|item| item.public_only());
        if let Some(pos) = self.position_of(id) {
            self.store.remove(pos);
        }
        if let Some(item) = released {
            Self::release_public_policy(&item);
        }
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
                let owner = m.find(id);
                let gallery_owner = owner.as_ref().is_some_and(|item| item.is_gallery());
                let playlist_owner = owner
                    .as_ref()
                    .is_some_and(|item| item.is_collection_owner());
                if let Some(path) = m.cancel_inner(id, false, Stop::Preserve) {
                    partials_to_remove.push(path);
                }
                if gallery_owner {
                    m.finish_gallery_file_rows(id, DownloadStatus::Cancelled);
                }
                if playlist_owner {
                    m.finish_video_playlist_file_rows(id, DownloadStatus::Cancelled);
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
}
