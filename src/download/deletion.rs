//! Download deletion and Trash operations.

use super::{DownloadItem, DownloadManager};
use gettextrs::gettext;
use gtk4::gio;
use gtk4::gio::prelude::*;
use std::rc::{Rc, Weak};

/// Keeps all rows in one collection locked until the Trash worker and its
/// queue-model commit have both completed.
pub(crate) struct TrashOperationGuard {
    manager: Weak<DownloadManager>,
    id: u64,
    locked_ids: Vec<u64>,
    locked_paths: Vec<std::path::PathBuf>,
    active: bool,
}

impl TrashOperationGuard {
    fn belongs_to(&self, manager: &Rc<DownloadManager>, id: u64) -> bool {
        self.active
            && self.id == id
            && self
                .manager
                .upgrade()
                .is_some_and(|owner| Rc::ptr_eq(&owner, manager))
    }

    /// Commit the successful Trash operation while its row lock is still held.
    fn complete(mut self) {
        if let Some(manager) = self.manager.upgrade() {
            manager.remove_inner(self.id);
            manager.release_trash_scope(&self.locked_ids);
            manager.release_file_operation_paths(&self.locked_paths);
            self.active = false;
            manager.persist_queue();
            manager.changed();
            manager.start_next();
        } else {
            self.active = false;
        }
    }
}

impl Drop for TrashOperationGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Some(manager) = self.manager.upgrade() {
            manager.release_trash_scope(&self.locked_ids);
            manager.release_file_operation_paths(&self.locked_paths);
            self.active = false;
            manager.changed();
        } else {
            self.active = false;
        }
    }
}

/// Holds destination paths while an Undo restore is inspecting and re-inserting rows.
/// Trash claims the same paths, so the two asynchronous operations cannot overlap.
pub(crate) struct UndoOperationGuard {
    manager: Weak<DownloadManager>,
    locked_ids: Vec<u64>,
    locked_paths: Vec<std::path::PathBuf>,
}

impl Drop for UndoOperationGuard {
    fn drop(&mut self) {
        if let Some(manager) = self.manager.upgrade() {
            manager.release_trash_scope(&self.locked_ids);
            manager.release_file_operation_paths(&self.locked_paths);
            manager.changed();
        }
    }
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
                    if crate::torrent::trash_torrent_contents(&path, &url)
                        .map_err(|error| error.to_string())?
                    {
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

impl DownloadManager {
    /// Return every row that must be locked together for a Trash operation.
    /// Collection owners, children, and siblings share one serialization scope.
    fn trash_scope(&self, id: u64, item: &DownloadItem) -> Vec<u64> {
        let mut scope = vec![id];
        let group_id = if item.is_collection_parent() {
            Some(id)
        } else {
            item.group().map(|group| group.id())
        };
        if let Some(group_id) = group_id {
            scope.push(group_id);
        }
        let destination = item.file_path();
        scope.extend(
            self.items()
                .filter(|member| {
                    group_id.is_some_and(|group_id| {
                        member.id() == group_id
                            || member.group().is_some_and(|group| group.id() == group_id)
                    }) || member.file_path() == destination
                })
                .map(|member| member.id()),
        );
        scope.sort_unstable();
        scope.dedup();
        scope
    }

    fn release_trash_scope(&self, ids: &[u64]) {
        let mut active = self.trash_in_progress.borrow_mut();
        for id in ids {
            active.remove(id);
        }
    }

    fn release_file_operation_paths(&self, paths: &[std::path::PathBuf]) {
        let mut active_paths = self.file_operations_in_progress.borrow_mut();
        for path in paths {
            active_paths.remove(path);
        }
    }

    /// Whether the row or its output path is currently owned by another operation.
    pub(crate) fn is_trash_blocked(&self, id: u64) -> bool {
        self.trash_in_progress.borrow().contains(&id)
    }

    /// Claim the row synchronously, before its filesystem operation is spawned.
    pub(crate) fn begin_trash(self: &Rc<Self>, id: u64) -> Result<TrashOperationGuard, String> {
        let item = self.find(id).ok_or_else(|| gettext("Download not found"))?;
        let locked_ids = self.trash_scope(id, &item);
        let mut locked_paths = self
            .items()
            .filter(|row| locked_ids.binary_search(&row.id()).is_ok())
            .map(|row| row.file_path())
            .collect::<Vec<_>>();
        locked_paths.sort_unstable();
        locked_paths.dedup();
        {
            let mut active = self.trash_in_progress.borrow_mut();
            let mut active_paths = self.file_operations_in_progress.borrow_mut();
            if locked_ids
                .iter()
                .any(|locked_id| active.contains(locked_id))
                || locked_paths.iter().any(|path| active_paths.contains(path))
            {
                return Err(gettext("This download is already being moved to Trash"));
            }
            active.extend(locked_ids.iter().copied());
            active_paths.extend(locked_paths.iter().cloned());
        }
        self.changed();
        Ok(TrashOperationGuard {
            manager: Rc::downgrade(self),
            id,
            locked_ids,
            locked_paths,
            active: true,
        })
    }

    /// Claim all Undo destinations before starting directory inspection. The
    /// guard holds them through model insertion, serializing Undo with Trash.
    pub(crate) fn begin_undo(
        self: &Rc<Self>,
        paths: &[std::path::PathBuf],
    ) -> Result<UndoOperationGuard, String> {
        let mut locked_paths = paths.to_vec();
        locked_paths.sort_unstable();
        locked_paths.dedup();
        let mut locked_ids = self
            .items()
            .filter(|item| locked_paths.binary_search(&item.file_path()).is_ok())
            .map(|item| item.id())
            .collect::<Vec<_>>();
        locked_ids.sort_unstable();
        locked_ids.dedup();
        {
            let mut active_ids = self.trash_in_progress.borrow_mut();
            let mut active_paths = self.file_operations_in_progress.borrow_mut();
            if locked_ids.iter().any(|id| active_ids.contains(id))
                || locked_paths.iter().any(|path| active_paths.contains(path))
            {
                return Err(gettext(
                    "A download destination is busy with another operation",
                ));
            }
            active_ids.extend(locked_ids.iter().copied());
            active_paths.extend(locked_paths.iter().cloned());
        }
        self.changed();
        Ok(UndoOperationGuard {
            manager: Rc::downgrade(self),
            locked_ids,
            locked_paths,
        })
    }

    pub(super) fn gallery_trash_target(
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

    /// Test-only convenience wrapper for exercising the complete Trash worker.
    #[cfg(test)]
    pub async fn delete_download_async(self: &Rc<Self>, id: u64) -> Result<(), String> {
        let guard = self.begin_trash(id)?;
        self.delete_download_with_guard(id, guard).await
    }

    /// Run a Trash operation after its row and collection scope were claimed
    /// synchronously by the UI. The guard remains alive across the blocking
    /// worker and is only released after the queue row is removed.
    pub(crate) async fn delete_download_with_guard(
        self: &Rc<Self>,
        id: u64,
        guard: TrashOperationGuard,
    ) -> Result<(), String> {
        if !guard.belongs_to(self, id) {
            return Err(gettext("Trash operation is no longer valid"));
        }
        let item = self.find(id).ok_or_else(|| gettext("Download not found"))?;
        let operation = self.prepare_delete_operation(id, &item);
        let torrent_url = match &operation {
            DeleteOperation::Torrent { url, .. } => Some(url.clone()),
            _ => None,
        };
        let filename = item.filename().to_string();
        #[cfg(test)]
        let before_execute_hook = self.trash_before_execute_hook.borrow_mut().take();
        #[cfg(not(test))]
        let before_execute_hook: Option<std::sync::Arc<dyn Fn() + Send + Sync>> = None;

        gio::spawn_blocking(move || {
            if let Some(hook) = before_execute_hook {
                hook();
            }
            execute_delete_operation(operation)
        })
        .await
        .map_err(|_| format!("Could not move {filename} to Trash: background task panicked"))??;

        // This only schedules teardown on Tokio; retain the original ordering
        // and do it only after trash/archive operations have succeeded.
        if torrent_url.is_some() {
            crate::torrent::forget_download(id);
        }
        guard.complete();
        Ok(())
    }
}
