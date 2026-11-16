//! Queue file persistence and immutable queue snapshots for the download manager.

use super::{
    DownloadManager, DownloadStatus, QUEUE_TMP_COUNTER, QUEUE_VERSION, RestoreState, StoredItem,
    StoredQueue,
};
#[cfg(not(test))]
use crate::runtime::tokio_rt;
use gtk4::glib;
use std::sync::atomic::Ordering;
#[cfg(not(test))]
use std::time::Duration;

impl DownloadManager {
    pub(super) fn queue_file() -> std::path::PathBuf {
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
    pub(super) fn sweep_stale_tmp_at(qf: &std::path::Path) {
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

    /// Move a broken queue entry aside without replacing an existing backup.
    ///
    /// # Errors
    ///
    /// Returns an error when no backup name can be claimed or the source cannot
    /// be moved. Callers must keep restore in the Failed state in that case.
    pub(super) fn quarantine_queue_at(queue_file: &std::path::Path) -> std::io::Result<()> {
        let base = queue_file.with_extension("json.bak");
        for index in 0..=99 {
            let target = if index == 0 {
                base.clone()
            } else {
                queue_file.with_extension(format!("json.bak.{index}"))
            };
            match crate::file_names::rename_noreplace_same_device(queue_file, &target) {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                // The file may have been removed between inspection and rename.
                // There is no longer a damaged queue to preserve.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "too many download queue backups; keeping the damaged queue in place",
        ))
    }

    pub(super) fn persist_queue(&self) {
        // Avoid overwriting the saved queue with an incomplete in-memory model
        // while the asynchronous startup restore is still reading it.
        if self.restore_state.get() != RestoreState::Ready {
            self.restore_dirty.set(true);
            return;
        }
        // watch::send_replace keeps only the latest value: rapid state changes
        // coalesce to a single write by the persist task.
        // Tests need determinism: they persist then immediately restore in a
        // new manager, so write synchronously in test builds.
        #[cfg(test)]
        if let Some(text) = self.serialize_queue() {
            if let Err(error) = Self::write_queue_file(&text) {
                tracing::error!(error = %error, "could not persist download queue");
            }
        }
        #[cfg(not(test))]
        if let (Some(queue), Some(tx)) =
            (self.snapshot_queue_inner(false), &*self.persist_tx.borrow())
        {
            tx.send_replace(Some(queue));
        }
    }

    /// Write the current GTK-owned queue snapshot and wait for its atomic file
    /// replacement and parent-directory sync to complete.
    ///
    /// # Errors
    ///
    /// Returns an error when restore is incomplete, the writer is unavailable,
    /// serialization fails, or the queue file cannot be durably replaced.
    pub(super) async fn persist_queue_and_wait(&self) -> std::io::Result<()> {
        if self.restore_state.get() != RestoreState::Ready {
            return Err(std::io::Error::other(
                "download queue restore has not completed",
            ));
        }
        let queue = self.snapshot_queue_inner(true).ok_or_else(|| {
            std::io::Error::other("could not snapshot the download queue")
        })?;
        #[cfg(test)]
        {
            let text = Self::serialize_snapshot(queue).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "could not serialize the download queue",
                )
            })?;
            Self::write_queue_file(&text)
        }
        #[cfg(not(test))]
        {
            let persist_tx = self
                .persist_tx
                .borrow()
                .as_ref()
                .cloned()
                .ok_or_else(|| std::io::Error::other("download queue writer is unavailable"))?;
            let flush_tx = self
                .persist_flush_tx
                .borrow()
                .as_ref()
                .cloned()
                .ok_or_else(|| std::io::Error::other("download queue flush channel is unavailable"))?;
            persist_tx.send_replace(Some(queue));
            let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
            flush_tx
                .send(ack_tx)
                .map_err(|_| std::io::Error::other("download queue writer stopped"))?;
            ack_rx
                .await
                .map_err(|_| std::io::Error::other("download queue writer dropped its flush result"))?
        }
    }

    /// Sole queue writer. Normal snapshots coalesce; explicit flush requests
    /// force the latest snapshot through the same writer and report durability.
    pub(crate) async fn persist_task_loop<T>(
        mut rx: tokio::sync::watch::Receiver<Option<T>>,
        mut flush_rx: tokio::sync::mpsc::UnboundedReceiver<
            tokio::sync::oneshot::Sender<std::io::Result<()>>,
        >,
        write: impl Fn(T) -> std::io::Result<()> + Clone + Send + 'static,
    ) where
        T: Clone + Send + 'static,
    {
        let mut watch_open = true;
        let mut flush_open = true;
        while watch_open || flush_open {
            tokio::select! {
                changed = rx.changed(), if watch_open => {
                    match changed {
                        Ok(()) => {
                            let snapshot = rx.borrow_and_update().clone();
                            if let Some(value) = snapshot {
                                let writer = write.clone();
                                match tokio::task::spawn_blocking(move || writer(value)).await {
                                    Ok(Ok(())) => {}
                                    Ok(Err(error)) => tracing::error!(
                                        error = %error,
                                        "could not persist download queue"
                                    ),
                                    Err(error) => tracing::error!(
                                        error = %error,
                                        "download queue writer task failed"
                                    ),
                                }
                            }
                        }
                        Err(_) => watch_open = false,
                    }
                }
                request = flush_rx.recv(), if flush_open => {
                    match request {
                        Some(ack) => {
                            let snapshot = rx.borrow_and_update().clone();
                            let result = match snapshot {
                                Some(value) => {
                                    let writer = write.clone();
                                    match tokio::task::spawn_blocking(move || writer(value)).await {
                                        Ok(result) => result,
                                        Err(error) => Err(std::io::Error::other(format!(
                                            "download queue writer task failed: {error}"
                                        ))),
                                    }
                                }
                                None => Err(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    "download queue snapshot is unavailable",
                                )),
                            };
                            let _ = ack.send(result);
                        }
                        None => flush_open = false,
                    }
                }
            }
        }
    }

    /// Final persist for shutdown: send the state, drop the sender, await the
    /// task. `watch::changed()` delivers the final value before the
    /// closed-channel Err, so the last write lands before we return.
    /// Tests write synchronously (no channel/task in test builds).
    pub(super) fn persist_for_shutdown(&self) {
        #[cfg(test)]
        if self.restore_state.get() == RestoreState::Ready
            && let Some(text) = self.serialize_queue_inner(true)
            && let Err(error) = Self::write_queue_file(&text)
        {
            tracing::error!(error = %error, "shutdown: could not persist download queue");
        }
        #[cfg(not(test))]
        {
            if self.restore_state.get() != RestoreState::Ready {
                tracing::warn!(
                    "shutdown: queue restore did not complete; preserving the saved queue file"
                );
            } else if let (Some(queue), Some(tx)) =
                (self.snapshot_queue_inner(true), &*self.persist_tx.borrow())
            {
                tx.send_replace(Some(queue));
            }
            drop(self.persist_tx.borrow_mut().take());
            drop(self.persist_flush_tx.borrow_mut().take());
            // This fallback runs after the main loop has exited, when blocking
            // cannot freeze GTK. Bound the wait so a stalled disk cannot hang exit.
            if let Some(task) = self.persist_task.borrow_mut().take() {
                let _ = tokio_rt()
                    .block_on(async { tokio::time::timeout(Duration::from_secs(5), task).await });
            }
        }
    }

    /// Build a thread-safe snapshot from GTK-owned rows. The expensive JSON
    /// encoding is done by the background persistence worker.
    pub(super) fn snapshot_queue_inner(&self, force: bool) -> Option<StoredQueue> {
        if !force && self.batch.get() > 0 {
            return None;
        }
        let mut items = Vec::new();
        for it in self.items() {
            // Collection children are passive UI projections of one
            // persisted owner job (gallery-dl or yt-dlp playlist).
            if it.is_collection_child() {
                continue;
            }
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
                let video_source = self.video_source(it.id()).filter(|source| {
                    matches!(
                        source,
                        crate::media_types::VideoSource::Page { .. }
                            | crate::media_types::VideoSource::Manifest { .. }
                            | crate::media_types::VideoSource::Playlist { .. }
                    )
                });
                let group = it.group();
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
                    group,
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
    pub(super) fn serialize_queue(&self) -> Option<String> {
        self.serialize_queue_inner(false)
    }

    /// Synchronous serialization is used only by deterministic test persistence.
    #[cfg(test)]
    pub(super) fn serialize_queue_inner(&self, force: bool) -> Option<String> {
        let queue = self.snapshot_queue_inner(force)?;
        Self::serialize_snapshot(queue)
    }

    pub(super) fn serialize_snapshot(queue: StoredQueue) -> Option<String> {
        match serde_json::to_string(&queue) {
            Ok(text) => Some(text),
            Err(error) => {
                tracing::error!("could not serialize download queue: {error}");
                None
            }
        }
    }

    pub(super) fn write_queue_snapshot(queue: StoredQueue) -> std::io::Result<()> {
        let text = Self::serialize_snapshot(queue).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "could not serialize the download queue",
            )
        })?;
        Self::write_queue_file(&text)
    }

    /// Write the serialized queue atomically and sync both the file and its
    /// containing directory before reporting success.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory, temporary file, write, sync, rename,
    /// or directory sync fails.
    pub(super) fn write_queue_file(text: &str) -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt as _;

        // Resolve and prepare the parent here: this function runs on the writer
        // task (or synchronously in unit tests), not from the normal GTK path.
        let queue_file = Self::queue_file();
        let parent = queue_file.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "download queue path has no parent directory",
            )
        })?;
        std::fs::create_dir_all(parent)?;

        // Temp name is unique per write (PID + monotonic counter): a pre-existing
        // symlink at this path cannot be predicted, and exclusive create refuses
        // to follow symlinks or overwrite an existing file.
        let tmp = queue_file.with_extension(format!(
            "json.tmp.{}.{}",
            std::process::id(),
            QUEUE_TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                // URLs can carry signed tokens: restrict to owner.
                .mode(0o600)
                .open(&tmp)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&tmp, &queue_file)?;
            std::fs::File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }
}
