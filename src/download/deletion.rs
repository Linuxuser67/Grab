//! Download deletion and Trash operations.

use super::{DownloadItem, DownloadManager};
use gettextrs::gettext;
use gtk4::gio;
use gtk4::gio::prelude::*;
use std::rc::Rc;

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
}
