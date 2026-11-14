//! Queue decoding, validation, restoration and recovery for the download manager.

#[cfg(not(test))]
use super::RestoreTaskGuard;
use super::{DownloadManager, DownloadStatus, RestoreOutcome, RestoreState};
use crate::download_intake::normalize_url;
use crate::download_pieces::MAX_SEGMENTED_TOTAL;
use crate::download_store::{QUEUE_VERSION, StoredItem, StoredQueue};
use crate::file_names::{piece_len, sane_filename};
use crate::runtime::tokio_rt;
#[cfg(not(test))]
use gtk4::glib;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

struct RestoreSweepPlan {
    referenced: HashSet<String>,
    live: HashSet<u64>,
    dest_dirs: HashSet<String>,
    state_keep: HashSet<(String, u64)>,
    selections: Vec<(String, Vec<usize>)>,
    session_urls: Option<Vec<String>>,
}

/// One validated queue entry awaiting the restore apply phase.
pub(super) struct PendingRestore {
    pub(super) item: StoredItem,
    pub(super) output_dir: Option<String>,
    pub(super) segments: Option<crate::download_pieces::SegmentState>,
    pub(super) gallery_job: Option<crate::gallery_argv::GalleryJob>,
}

impl DownloadManager {
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
        let item_url = match normalize_url(&item.url) {
            Ok(url) => url,
            Err(_) => {
                tracing::warn!("skipping queue entry with bad URL");
                return None;
            }
        };
        if let Some(crate::media_types::VideoSource::Manifest { source }) =
            item.video_source.as_ref()
            && (!crate::manifest_intake::valid_source(source)
                || !normalize_url(&source.url).is_ok_and(|url| url == item_url))
        {
            tracing::warn!("skipping queue entry with invalid native manifest source");
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
                        file_rows: Vec::new(),
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

    /// Find restore rows whose id allocation may need a staging-name scan.
    /// Normal rows with unique persisted ids need no directory listing.
    pub(super) fn restore_dirs_requiring_id_listing(
        &self,
        pending: &[PendingRestore],
    ) -> HashSet<String> {
        let mut used_ids: HashSet<u64> = self.items().map(|item| item.id()).collect();
        used_ids.extend(self.epoch.borrow().keys().copied());
        pending
            .iter()
            .filter_map(|entry| match entry.item.id {
                Some(id) if used_ids.insert(id) => None,
                _ => Some(entry.item.dest_dir.clone()),
            })
            .collect()
    }

    /// Read destination directories needed for fallback IDs.
    ///
    /// Missing destinations are empty; other listing failures are returned so
    /// an ID is never selected from an incomplete directory snapshot.
    /// Production callers run this on a blocking worker, not on GTK.
    pub(super) fn list_restore_destinations(
        directories: HashSet<String>,
    ) -> std::io::Result<HashMap<String, Vec<String>>> {
        directories
            .into_iter()
            .map(|dir| {
                let path = std::path::Path::new(&dir);
                let names = crate::video_staging::try_dir_file_names(path).map_err(|error| {
                    std::io::Error::new(
                        error.kind(),
                        format!(
                            "cannot list restore destination {}: {error}",
                            path.display()
                        ),
                    )
                })?;
                Ok((dir, names))
            })
            .collect()
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
                let directories = self.restore_dirs_requiring_id_listing(&pending);
                let existing_by_dir = match Self::list_restore_destinations(directories) {
                    Ok(existing) => existing,
                    Err(error) => {
                        tracing::error!(
                            "download queue destination listing failed: {error}; preserving the saved queue"
                        );
                        self.finish_restore(RestoreOutcome::Failed);
                        return;
                    }
                };
                let plan = self.apply_restored_queue(pending, &existing_by_dir);
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
                        let directories = manager.restore_dirs_requiring_id_listing(&pending);
                        let prepared = tokio_rt()
                            .spawn_blocking(move || {
                                Self::list_restore_destinations(directories)
                                    .map(|existing_by_dir| (pending, existing_by_dir))
                            })
                            .await;
                        let (pending, existing_by_dir) = match prepared {
                            Ok(Ok(prepared)) => prepared,
                            Ok(Err(error)) => {
                                tracing::error!(
                                    "download queue destination listing failed: {error}; preserving the saved queue"
                                );
                                manager.finish_restore(RestoreOutcome::Failed);
                                completion.finished = true;
                                return;
                            }
                            Err(error) => {
                                tracing::error!(
                                    "download queue destination listing task failed: {error}; preserving the saved queue"
                                );
                                manager.finish_restore(RestoreOutcome::Failed);
                                completion.finished = true;
                                return;
                            }
                        };
                        // The main context yielded while legacy/duplicate ids were checked.
                        if manager.restore_should_abort() {
                            manager.finish_restore(RestoreOutcome::Failed);
                            completion.finished = true;
                            return;
                        }
                        let plan = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            manager.apply_restored_queue(pending, &existing_by_dir)
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

    pub(super) fn restore_should_abort(&self) -> bool {
        self.shutdown_started.get() || self.draining.get() || self.shutdown_finished.get()
    }

    /// Shared restore finalization: all test and production paths release intake
    /// waiters through the same state transition and keep failed restores gated.
    pub(super) fn finish_restore(self: &Rc<Self>, outcome: RestoreOutcome) {
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
    fn apply_restored_queue(
        self: &Rc<Self>,
        pending: Vec<PendingRestore>,
        existing_by_dir: &HashMap<String, Vec<String>>,
    ) -> RestoreSweepPlan {
        let mut selections = Vec::new();
        {
            let _batch = self.batch_guard();
            // JSON parsing, validation and any necessary listings completed off-thread.
            // GObject construction and model mutation stay on this context.
            for p in pending {
                let existing = existing_by_dir
                    .get(&p.item.dest_dir)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                match p.item.status {
                    DownloadStatus::Done => {
                        if p.gallery_job.is_some() {
                            let mut restored_item = p.item.clone();
                            restored_item.segments = p.segments;
                            match self.restore_existing_gallery_with_listing(
                                &restored_item,
                                p.gallery_job,
                                existing,
                            ) {
                                Ok(_) => {}
                                Err(e) => tracing::warn!("skipping queue entry: {e}"),
                            }
                        } else {
                            self.insert_history(p.item, p.output_dir, existing);
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
                            self.restore_existing_gallery_with_listing(
                                &restored_item,
                                Some(job),
                                existing,
                            )
                        } else {
                            self.restore_existing_with_listing(&restored_item, existing)
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
            .filter(|u| crate::torrent::is_torrent(u))
            .collect();
        // Only rows that can still resume own staging: a Done row has
        // delivered, so anything it left in its dest dir is debris.
        let live: HashSet<u64> = self
            .items()
            .filter(|it| it.status() != DownloadStatus::Done)
            .map(|it| it.id())
            .collect();
        let dest_dirs: HashSet<String> = self.items().map(|it| it.dest_dir()).collect();
        // State manifests belong to rows that can still resume. Done rows have
        // delivered; anything they leave behind is crash debris.
        let state_keep: HashSet<(String, u64)> = self
            .items()
            .filter(|it| it.status() != DownloadStatus::Done)
            .map(|it| (it.dest_dir(), it.id()))
            .collect();
        let session_urls = crate::torrent::session_handle().is_some().then(|| {
            self.items()
                .filter(|it| {
                    it.status() != DownloadStatus::Done && crate::torrent::is_torrent(&it.url())
                })
                .map(|it| it.url().to_string())
                .collect()
        });
        RestoreSweepPlan {
            referenced,
            live,
            dest_dirs,
            state_keep,
            selections,
            session_urls,
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
        crate::video::sweep_state_staging(&plan.state_keep);
        // Hashing reads each torrent archive: do it here, on the blocking pool,
        // not while building the plan on the GTK thread.
        plan.session_urls.map(|urls| {
            urls.into_iter()
                .filter_map(|url| crate::torrent::info_hash_for_url(&url))
                .collect()
        })
    }

    fn schedule_session_orphan_sweep(keep: Option<HashSet<String>>) {
        if let Some(keep) = keep {
            tokio_rt().spawn(async move {
                crate::torrent::sweep_session_orphans(&keep).await;
            });
        }
    }
}
