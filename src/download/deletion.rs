//! Download deletion and Trash operations.

use super::{DownloadItem, DownloadManager};
use gettextrs::gettext;
use gtk4::gio;
use gtk4::gio::prelude::*;
use gtk4::glib;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};

/// Shared ownership for rows and destinations claimed by Trash or Undo.
#[derive(Default)]
pub(super) struct OperationLocks {
    rows: std::collections::HashSet<u64>,
    paths: std::collections::HashSet<std::path::PathBuf>,
}

/// A cloneable lease: locks are released only after the last async or blocking
/// operation that owns the lease has finished.
#[derive(Clone)]
struct OperationLockLease {
    _inner: Arc<OperationLockLeaseInner>,
}

struct OperationLockLeaseInner {
    locks: Arc<Mutex<OperationLocks>>,
    row_ids: Vec<u64>,
    paths: Vec<std::path::PathBuf>,
    notify_on_release: bool,
}

impl Drop for OperationLockLeaseInner {
    fn drop(&mut self) {
        {
            let mut locks = self
                .locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for id in &self.row_ids {
                locks.rows.remove(id);
            }
            for path in &self.paths {
                locks.paths.remove(path);
            }
        }
        if self.notify_on_release {
            super::notify_operation_locks_changed(Arc::downgrade(&self.locks));
        }
    }
}

/// Keeps the model row alive until a successful Trash worker commits its result.
pub(crate) struct TrashOperationGuard {
    manager: Weak<DownloadManager>,
    id: u64,
    lease: OperationLockLease,
}

impl TrashOperationGuard {
    fn belongs_to(&self, manager: &Rc<DownloadManager>, id: u64) -> bool {
        self.id == id
            && self
                .manager
                .upgrade()
                .is_some_and(|owner| Rc::ptr_eq(&owner, manager))
    }

    fn worker_lease(&self) -> OperationLockLease {
        self.lease.clone()
    }
}

/// Holds Undo destinations while directory inspection and model insertion run.
pub(crate) struct UndoOperationGuard {
    _lease: OperationLockLease,
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

/// Apply the Trash result on GTK even when the original async waiter is dropped.
/// Keep the worker lease alive until the queue mutation has been committed.
fn schedule_trash_completion(
    locks: std::sync::Weak<Mutex<OperationLocks>>,
    id: u64,
    result: Result<(), String>,
    completion: tokio::sync::oneshot::Sender<Result<(), String>>,
    worker_lease: OperationLockLease,
) {
    glib::idle_add(move || {
        let _keep_lease_until_commit = &worker_lease;
        if result.is_ok()
            && let Some(expected_locks) = locks.upgrade()
        {
            super::MANAGER_WEAK.with(|slot| {
                if let Some(manager) = slot.borrow().as_ref().and_then(Weak::upgrade)
                    && Arc::ptr_eq(&manager.operation_locks, &expected_locks)
                {
                    manager.remove_inner(id);
                    manager.persist_queue();
                    manager.start_next();
                }
            });
        }
        let _ = completion.send(result);
        glib::ControlFlow::Break
    });
}

impl DownloadManager {
    /// Return every row transitively connected to a Trash destination through
    /// collection membership or an identical output path.
    fn trash_scope(&self, id: u64, item: &DownloadItem) -> Vec<u64> {
        let rows = self.items().collect::<Vec<_>>();
        let mut locked_ids = std::collections::HashSet::from([id]);
        let mut locked_paths = std::collections::HashSet::from([item.file_path()]);
        let mut locked_groups = std::collections::HashSet::new();
        let initial_group = if item.is_collection_parent() {
            Some(id)
        } else {
            item.group_id()
        };
        if let Some(group_id) = initial_group {
            locked_groups.insert(group_id);
        }

        // A duplicate-path row can pull in its own collection, which can
        // expose another path shared with a third collection. Expand paths,
        // groups, and rows together until the complete lock scope is stable.
        loop {
            let mut changed = false;
            for member in &rows {
                let member_group = if member.is_collection_parent() {
                    Some(member.id())
                } else {
                    member.group_id()
                };
                let member_path = member.file_path();
                let connected = locked_ids.contains(&member.id())
                    || member_group.is_some_and(|group_id| locked_groups.contains(&group_id))
                    || locked_paths.contains(&member_path);
                if !connected {
                    continue;
                }
                changed |= locked_ids.insert(member.id());
                changed |= locked_paths.insert(member_path);
                if let Some(group_id) = member_group {
                    changed |= locked_groups.insert(group_id);
                }
            }
            if !changed {
                break;
            }
        }

        let mut scope = locked_ids.into_iter().collect::<Vec<_>>();
        scope.sort_unstable();
        scope
    }

    fn claim_operation_locks(
        &self,
        locked_ids: Vec<u64>,
        locked_paths: Vec<std::path::PathBuf>,
        notify_on_release: bool,
        busy_message: String,
    ) -> Result<OperationLockLease, String> {
        let mut locks = self
            .operation_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if locked_ids.iter().any(|id| locks.rows.contains(id))
            || locked_paths.iter().any(|path| locks.paths.contains(path))
        {
            return Err(busy_message);
        }
        locks.rows.extend(locked_ids.iter().copied());
        locks.paths.extend(locked_paths.iter().cloned());
        Ok(OperationLockLease {
            _inner: Arc::new(OperationLockLeaseInner {
                locks: Arc::clone(&self.operation_locks),
                row_ids: locked_ids,
                paths: locked_paths,
                notify_on_release,
            }),
        })
    }

    /// Whether the row is currently owned by Trash or Undo.
    pub(crate) fn is_trash_blocked(&self, id: u64) -> bool {
        self.operation_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .rows
            .contains(&id)
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
        let lease = self.claim_operation_locks(
            locked_ids,
            locked_paths,
            true,
            gettext("This download is already being moved to Trash"),
        )?;
        self.changed();
        Ok(TrashOperationGuard {
            manager: Rc::downgrade(self),
            id,
            lease,
        })
    }

    /// Claim all Undo destinations before starting directory inspection.
    /// The lease also blocks Trash until model insertion has completed.
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
        let lease = self.claim_operation_locks(
            locked_ids,
            locked_paths,
            false,
            gettext("A download destination is busy with another operation"),
        )?;
        Ok(UndoOperationGuard { _lease: lease })
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

        // The worker owns the lease through its GTK completion callback. This
        // makes both the filesystem operation and its queue commit independent
        // of the lifetime of the future awaiting this method.
        let worker_lease = guard.worker_lease();
        let locks = Arc::downgrade(&self.operation_locks);
        let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
        crate::runtime::tokio_rt()
            .spawn_blocking(move || {
                if let Some(hook) = before_execute_hook {
                    hook();
                }
                let result = execute_delete_operation(operation);
                if result.is_ok() && torrent_url.is_some() {
                    crate::torrent::forget_download(id);
                }
                schedule_trash_completion(locks, id, result, completion_tx, worker_lease);
            })
            .await
            .map_err(|_| format!("Could not move {filename} to Trash: background task panicked"))?;

        completion_rx.await.map_err(|_| {
            format!("Could not move {filename} to Trash: completion was interrupted")
        })?
    }
}
