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

    // Transfer state (status, progress, detail) lives in the list row itself;
    // the dialog only shows what the row doesn't: file metadata.
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
