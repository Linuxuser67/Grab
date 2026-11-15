use super::*;

impl DownloadManager {
    /// Finalize one successful engine message, including collision-safe name adoption.
    ///
    /// This runs on the GTK context. Any blocking filesystem work drops the
    /// thread-bound manager first, then re-upgrades and checks the attempt
    /// generation before touching queue state again.
    pub(super) async fn handle_finished_message(
        this: Rc<Self>,
        weak: std::rc::Weak<Self>,
        item: &DownloadItem,
        id: u64,
        generation: u64,
        size: u64,
        url: String,
    ) {
        if item.status() != DownloadStatus::Cancelled && item.status() != DownloadStatus::Paused {
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
                let Some(this) = weak.upgrade() else {
                    return;
                };
                if !this.is_current(id, generation) {
                    return;
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
                    return;
                }
                // If the current file is present, the collision path must
                // claim-then-move it. Keep GTK objects on this context and send
                // only plain paths/candidate names to the blocking worker.
                if existing.iter().any(|entry| entry == &current) {
                    match this.reserve_final_name_candidates(&dir, &name, &existing) {
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
                                    let moved_path = std::path::Path::new(&dir).join(&final_name);
                                    let rollback_old_path = old_path.clone();
                                    let rollback = gio::spawn_blocking(move || {
                                        rename_noreplace(&moved_path, &rollback_old_path)
                                    })
                                    .await;
                                    if !matches!(rollback, Ok(Ok(()))) {
                                        item.set_filename(final_name);
                                    }
                                }
                                return;
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
                                    let moved_path = std::path::Path::new(&dir).join(&final_name);
                                    let rollback_moved_path = moved_path.clone();
                                    let rollback_old_path = old_path.clone();
                                    drop(this);
                                    let rollback = gio::spawn_blocking(move || {
                                        rename_noreplace(&rollback_moved_path, &rollback_old_path)
                                    })
                                    .await;
                                    if !matches!(rollback, Ok(Ok(()))) && update_detached_path {
                                        item.set_filename(final_name);
                                        if row_removed {
                                            // Removal already reserved old_path. Keep the
                                            // new path until its discard finalizer finishes.
                                            claims.retain_path(&moved_path);
                                        }
                                    }
                                }
                                return;
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
                return;
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
            let sizing_guard =
                cleanup_torrent.then(|| crate::download::SizingGuard::new(Rc::clone(&this.sizing_inflight)));
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
                    // Keep the in-flight count balanced when this
                    // fire-and-forget future completes or is cancelled.
                    let _sizing_guard = sizing_guard;
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
                    if cleanup_torrent
                        && let (Ok(Some(size)), Some(manager), Some(item)) =
                            (measured_size, weak_manager.upgrade(), weak_item.upgrade())
                        && manager.find(id).is_some()
                    {
                        item.set_detail(
                            gettext("Finished • {size}").replace("{size}", &fmt_bytes(size)),
                        );
                    }
                });
            }
        }
    }
}
