//! Gallery UI helpers for the inline New Download card.
//!
//! Pure move from `inline_add.rs` (no behavior change): the gallery preview
//! (`show_gallery_ready`, `refresh_gallery_info`) and the gallery-dl video
//! fallback submit (`submit_gallery_video`).

use crate::download::DownloadManager;
use crate::inline_add::{
    VideoStep, enqueue_and_close, hide_video_step, set_lookup_add, show_video_error,
    sync_entry_icon,
};
use adw::prelude::*;
use gettextrs::gettext;
use gtk4::prelude::*;
use libadwaita as adw;
use std::cell::RefCell;
use std::rc::Rc;

/// Show the gallery preview: title, file count, and filenames.
/// Hides the video-specific rows (format selector).
pub(crate) fn show_gallery_ready(
    v: &VideoStep,
    card: crate::gallery_card::GalleryCard,
    title: &str,
) {
    hide_video_step(v);
    v.looking_up.set(false);
    v.is_gallery.set(true);
    // Store the card first so the filter handler can access it.
    *v.gallery_card.borrow_mut() = Some(card);
    // Reset filter to All.
    v.gallery_filter.set_active_name(Some("all"));
    refresh_gallery_info(v, title);
    v.gallery_info.set_visible(true);
    v.gallery_filter_wrap.set_visible(true);
    // Reveal the block.
    v.action_revealer.set_visible(true);
    v.action_revealer.set_reveal_child(true);
    v.group_revealer.set_visible(true);
    v.group_revealer.set_reveal_child(true);
    // The Add button earns its text label.
    v.add_btn.set_icon_name("");
    v.add_btn.remove_css_class("circular");
    v.add_btn.set_label(&gettext("Add"));
    sync_entry_icon(v);
}

/// Refresh the gallery info row from the current card state.
pub(crate) fn refresh_gallery_info(v: &VideoStep, title: &str) {
    if let Some(card) = v.gallery_card.borrow().as_ref() {
        let count = card.file_count();
        let preview = card.preview_filenames().join(", ");
        let subtitle = if count > 5 {
            format!("{} files • {}, +{} more", count, preview, count - 5)
        } else {
            format!("{} files • {}", count, preview)
        };
        v.gallery_info.set_title(title);
        v.gallery_info.set_subtitle(&subtitle);
    }
}

/// Submit a video URL that yt-dlp can't handle but gallery-dl can.
/// Queues it as a normal video row with the GalleryDl backend; the runner
/// branches on the backend. The title comes from the gallery probe.
pub(crate) fn submit_gallery_video(
    manager: &Rc<DownloadManager>,
    dest: &Rc<RefCell<String>>,
    close_card: &Rc<dyn Fn()>,
    url: &str,
    items: &[crate::gallery_probe::GalleryItem],
    scheduled_at: Option<i64>,
    step: &Rc<VideoStep>,
    lookup_add: &Rc<RefCell<Option<gtk4::Button>>>,
) {
    let title = items
        .first()
        .map(|i| crate::gallery_probe::probe_title(i, url))
        .unwrap_or_else(|| url.to_string());
    let settings = manager.settings();
    enqueue_and_close(
        manager,
        dest,
        close_card,
        scheduled_at,
        |d| {
            manager.enqueue_video(
                url,
                d,
                Some(&title),
                crate::media_types::VideoChoices {
                    quality: settings.video_quality(),
                    audio_only: false,
                    video_format_id: None,
                    is_live: false,
                    playlist_item_id: None,
                    backend: crate::video::VideoBackend::GalleryDl,
                },
            )
        },
        |e| {
            show_video_error(step, e);
            set_lookup_add(lookup_add, true);
        },
    );
}
