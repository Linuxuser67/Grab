//! Scheduled-download wakeups and due-item promotion.

use super::{DownloadManager, DownloadStatus};
use gtk4::glib;
use std::rc::Rc;

impl DownloadManager {
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
}
