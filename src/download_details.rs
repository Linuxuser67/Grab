//! Per-row "Download Details" dialog behind the info button: static identity
//! rows plus live transfer rows. UI leaf module (gtk/adw).

use crate::download::DownloadManager;
use crate::media_types::VideoSource;
use adw::prelude::*;
use gettextrs::gettext;
use gtk4::prelude::*;
use libadwaita as adw;
use std::rc::Rc;

/// Download kind for the details dialog's Type row: torrents are detected
/// from the URL, media from the persisted video source, everything else is
/// a plain file. Pure for tests — the wording stays at the call site so
/// xgettext keeps extracting every label (same rule as `stop_copy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DownloadKind {
    Torrent,
    LiveVideo,
    Video,
    Audio,
    File,
}

pub(crate) fn download_kind(
    is_torrent: bool,
    source: Option<&VideoSource>,
    is_live: bool,
) -> DownloadKind {
    if is_torrent {
        return DownloadKind::Torrent;
    }
    match source {
        Some(VideoSource::Page {
            audio_only: true, ..
        }) => DownloadKind::Audio,
        Some(VideoSource::Page { .. }) if is_live => DownloadKind::LiveVideo,
        Some(VideoSource::Page { .. }) => DownloadKind::Video,
        _ => DownloadKind::File,
    }
}

/// Type-row wording for a classified download; gettext() here (not at call
/// sites) so xgettext extracts every msgid.
fn download_kind_label(kind: DownloadKind) -> String {
    match kind {
        DownloadKind::Torrent => gettext("Torrent"),
        DownloadKind::LiveVideo => gettext("Live video"),
        DownloadKind::Video => gettext("Video"),
        DownloadKind::Audio => gettext("Audio"),
        DownloadKind::File => gettext("File"),
    }
}

/// Right-aligned dimmed value label for a details-dialog row.
fn detail_value_label(text: &str) -> gtk4::Label {
    gtk4::Label::builder()
        .label(text)
        .halign(gtk4::Align::End)
        .valign(gtk4::Align::Center)
        .css_classes(["dimmed"])
        .build()
}

/// Whole-percent progress for the details dialog ("45%").
fn fmt_progress(frac: f64) -> String {
    format!("{}%", (frac.clamp(0.0, 1.0) * 100.0).round() as u64)
}

/// Show the details dialog for one row. Live rows update through the item's
/// notify signals holding only weak label refs — the item outlives the
/// dialog, so strong captures would leak the labels; there is no polling
/// timer.
pub(crate) fn show_download_details(
    manager: Rc<DownloadManager>,
    id: u64,
    anchor: &gtk4::ListBoxRow,
) {
    let Some(it) = manager.find(id) else {
        return;
    };
    let dialog = adw::Dialog::builder()
        .title(gettext("Download Details"))
        .build();
    dialog.set_content_width(420);

    let toolbar = adw::ToolbarView::new();
    let hb = adw::HeaderBar::new();
    let close_btn = gtk4::Button::builder()
        .label(gettext("_Close"))
        .use_underline(true)
        .build();
    hb.pack_end(&close_btn);
    toolbar.add_top_bar(&hb);

    let page = adw::PreferencesPage::new();
    toolbar.set_content(Some(&page));
    dialog.set_child(Some(&toolbar));
    crate::ui_util::close_on_click(&close_btn, &dialog);

    // Transfer group first: the live state is what the button is for.
    let transfer = adw::PreferencesGroup::new();
    page.add(&transfer);

    let status_row = adw::ActionRow::builder().title(gettext("Status")).build();
    let status_label = detail_value_label(&it.status().label());
    status_row.add_suffix(&status_label);
    transfer.add(&status_row);
    {
        let weak = status_label.downgrade();
        it.connect_status_notify(move |it| {
            if let Some(l) = weak.upgrade() {
                l.set_text(&it.status().label());
            }
        });
    }

    let progress_row = adw::ActionRow::builder().title(gettext("Progress")).build();
    let progress_label = detail_value_label(&fmt_progress(it.progress()));
    progress_row.add_suffix(&progress_label);
    transfer.add(&progress_row);
    {
        let weak = progress_label.downgrade();
        it.connect_progress_notify(move |it| {
            if let Some(l) = weak.upgrade() {
                l.set_text(&fmt_progress(it.progress()));
            }
        });
    }

    // The engine's live line (speed/ETA/size) or the failure message:
    // full-width, selectable and wrapping — the row caption ellipsizes it.
    let detail_row = adw::ActionRow::new();
    let detail_label = gtk4::Label::builder()
        .label(it.detail())
        .wrap(true)
        .wrap_mode(gtk4::pango::WrapMode::WordChar)
        .selectable(true)
        .xalign(0.0)
        .css_classes(["dimmed"])
        .build();
    detail_row.add_prefix(&detail_label);
    detail_row.set_visible(!it.detail().is_empty());
    transfer.add(&detail_row);
    {
        let weak_row = detail_row.downgrade();
        let weak_label = detail_label.downgrade();
        it.connect_detail_notify(move |it| {
            let text = it.detail();
            if let Some(l) = weak_label.upgrade() {
                l.set_text(&text);
            }
            if let Some(r) = weak_row.upgrade() {
                r.set_visible(!text.is_empty());
            }
        });
    }

    let file = adw::PreferencesGroup::new();
    page.add(&file);

    let url = it.url();
    let kind = download_kind(
        crate::torrent::is_torrent(url.as_str()) || crate::torrent::is_magnet(url.as_str()),
        manager.video_source(id).as_ref(),
        manager.is_live_video(id),
    );
    let type_row = adw::ActionRow::builder().title(gettext("Type")).build();
    type_row.add_suffix(&detail_value_label(&download_kind_label(kind)));
    file.add(&type_row);

    let name_row = adw::ActionRow::builder()
        .title(gettext("File name"))
        .build();
    let name_label = detail_value_label(&it.filename());
    name_label.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
    name_row.add_suffix(&name_label);
    file.add(&name_row);
    {
        let weak = name_label.downgrade();
        it.connect_filename_notify(move |it| {
            if let Some(l) = weak.upgrade() {
                l.set_text(&it.filename());
            }
        });
    }

    let url_row = adw::ActionRow::builder().title(gettext("URL")).build();
    let url_label = detail_value_label(url.as_str());
    url_label.set_hexpand(true);
    url_label.set_selectable(true);
    url_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    url_label.set_tooltip_text(Some(url.as_str()));
    url_row.add_suffix(&url_label);
    file.add(&url_row);

    let dest_row = adw::ActionRow::builder()
        .title(gettext("Save location"))
        .build();
    let dest_label = detail_value_label(it.dest_dir().as_str());
    dest_label.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
    dest_row.add_suffix(&dest_label);
    file.add(&dest_row);

    dialog.present(anchor.root().as_ref());
}
