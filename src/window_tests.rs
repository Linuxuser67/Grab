use crate::download::DownloadStatus;
use crate::inline_add::{fmt_item_duration, playlist_count_label};
use crate::media_types::PlaylistKind;
use crate::window_rows::{
    PulseTick, RowMedia, StopCopy, media_icon_failed, pulse_tick, row_media, should_pulse,
    stop_copy,
};
use gtk4::glib::object::Cast as _;
use gtk4::prelude::*;

#[test]
fn close_hides_only_for_background_transfers_not_during_shutdown() {
    use crate::window::should_hide_to_background;

    assert!(should_hide_to_background(true, false));
    assert!(!should_hide_to_background(false, false));
    assert!(
        !should_hide_to_background(true, true),
        "async shutdown must bypass the background-hide handler"
    );
}

#[test]
fn stop_copy_distinguishes_a_live_capture_from_a_discard() {
    // Stop inverts by row: normal downloads discard, live captures keep the recording.
    // Shipped both as "Cancel" with no feedback, so live users couldn't tell it was safe.
    assert_eq!(
        stop_copy(false),
        StopCopy::Cancel,
        "a normal download is discarded, so the discard verb is right"
    );
    assert_eq!(
        stop_copy(true),
        StopCopy::FinishRecording,
        "a live capture is kept, so the copy must not read as a discard"
    );
}

#[test]
fn should_pulse_covers_row_states() {
    use DownloadStatus::*;
    // Resolving: active with no fraction — the reported hang.
    assert!(should_pulse(Downloading, false, 0.0));
    // Determinate transfer renders fractions, never pulses.
    assert!(!should_pulse(Downloading, false, 0.5));
    assert!(!should_pulse(Downloading, false, 1.0));
    // Live captures pulse for the whole capture regardless of fraction.
    assert!(should_pulse(Downloading, true, 0.0));
    assert!(should_pulse(Downloading, true, 0.9));
    // Non-downloading rows never animate: queued, paused, and terminal.
    assert!(!should_pulse(Queued, false, 0.0));
    assert!(!should_pulse(Paused, false, 0.0));
    assert!(!should_pulse(Paused, true, 0.0));
    assert!(!should_pulse(Done, true, 0.0));
    assert!(!should_pulse(Failed, false, 0.0));
    assert!(!should_pulse(Cancelled, false, 0.0));
}

#[test]
fn should_pulse_locks_non_finite_and_negative_edges() {
    use DownloadStatus::*;
    // Negative fractions still read as "no fraction" — pulses.
    assert!(should_pulse(Downloading, false, -0.5));
    // NaN never satisfies `<= 0.0`, so it renders determinate (no pulse).
    assert!(!should_pulse(Downloading, false, f64::NAN));
    // Infinite progress is not "no fraction" — no pulse.
    assert!(!should_pulse(Downloading, false, f64::INFINITY));
    // Non-active rows never pulse regardless of fraction.
    assert!(!should_pulse(Queued, true, 0.0));
    assert!(!should_pulse(Done, false, f64::NAN));
}

/// The row pulse timer used to run until the widget died, so every finished
/// row kept waking the main loop 8x a second forever. It may only stop on a
/// state the row can never leave.
#[test]
fn the_pulse_timer_stops_only_where_the_row_cannot_come_back() {
    use DownloadStatus::*;
    use PulseTick::*;
    // Done is one-way for a given widget: no command moves a finished row
    // back to Queued, so the tick has nothing left to drive.
    assert_eq!(pulse_tick(Done, false, 1.0), Stop);
    assert_eq!(pulse_tick(Done, true, 0.0), Stop);
    // Failed and Cancelled are NOT safe to stop on: `retry` revives both by
    // mutating this very item back to Queued, so a stopped timer would
    // freeze the retried row's bar — the hang the tick exists to prevent.
    assert_eq!(pulse_tick(Failed, false, 0.5), Idle);
    assert_eq!(pulse_tick(Cancelled, false, 0.5), Idle);
    assert_eq!(pulse_tick(Paused, false, 0.0), Idle);
    assert_eq!(pulse_tick(Queued, false, 0.0), Idle);
    // Rows that can still animate keep animating.
    assert_eq!(pulse_tick(Downloading, false, 0.0), Pulse);
    assert_eq!(pulse_tick(Downloading, true, 0.9), Pulse);
    // An active row with a real fraction renders determinate: Pulse or Idle,
    // never Stop — the only input separating the two on an active row.
    assert_eq!(pulse_tick(Downloading, false, 0.5), Idle);
}

#[test]
fn fmt_item_duration_covers_minute_hour_boundaries() {
    assert_eq!(fmt_item_duration(0), "0:00");
    assert_eq!(fmt_item_duration(59), "0:59");
    assert_eq!(fmt_item_duration(60), "1:00");
    assert_eq!(fmt_item_duration(61), "1:01");
    assert_eq!(fmt_item_duration(3599), "59:59");
    assert_eq!(fmt_item_duration(3600), "1:00:00");
    assert_eq!(fmt_item_duration(3661), "1:01:01");
}

#[test]
fn fmt_item_duration_clamps_negative_to_zero() {
    assert_eq!(fmt_item_duration(-1), "0:00");
    assert_eq!(fmt_item_duration(i64::MIN), "0:00");
}

#[test]
fn playlist_count_label_covers_kinds_singular_plural() {
    assert_eq!(playlist_count_label(PlaylistKind::Stories, 1), "1 story");
    assert_eq!(playlist_count_label(PlaylistKind::Stories, 3), "3 stories");
    assert_eq!(
        playlist_count_label(PlaylistKind::Highlights, 1),
        "1 highlight"
    );
    assert_eq!(
        playlist_count_label(PlaylistKind::Highlights, 2),
        "2 highlights"
    );
    assert_eq!(playlist_count_label(PlaylistKind::Playlist, 1), "1 item");
    assert_eq!(playlist_count_label(PlaylistKind::Playlist, 5), "5 items");
}

#[test]
fn playlist_count_label_zero_uses_plural() {
    assert_eq!(playlist_count_label(PlaylistKind::Stories, 0), "0 stories");
    assert_eq!(
        playlist_count_label(PlaylistKind::Highlights, 0),
        "0 highlights"
    );
    assert_eq!(playlist_count_label(PlaylistKind::Playlist, 0), "0 items");
}

#[test]
fn row_media_live_wins_over_everything() {
    // A live audio-only torrent page is still a live capture first.
    assert_eq!(
        row_media(true, true, true, true, false, "f.mp4"),
        RowMedia::Live
    );
}

#[test]
fn row_media_audio_beats_torrent_and_page() {
    assert_eq!(
        row_media(false, true, true, true, false, "f.mp4"),
        RowMedia::Audio
    );
}

#[test]
fn row_media_torrent_beats_plain_video_page() {
    assert_eq!(
        row_media(false, false, true, true, false, "f.mp4"),
        RowMedia::Torrent
    );
}

#[test]
fn row_media_video_page_beats_plain_file() {
    assert_eq!(
        row_media(false, false, false, true, false, "f.mp4"),
        RowMedia::Video
    );
}

#[test]
fn row_media_gallery_takes_priority() {
    // Gallery flag wins over all other types.
    assert_eq!(
        row_media(true, true, true, true, true, "f.mp4"),
        RowMedia::Gallery
    );
    assert_eq!(
        row_media(false, false, false, false, true, "f.bin"),
        RowMedia::Gallery
    );
}

#[test]
fn row_media_plain_direct_falls_back_to_file() {
    assert_eq!(
        row_media(false, false, false, false, false, "f.bin"),
        RowMedia::File
    );
}

#[test]
fn row_media_gif_classified_as_image() {
    assert_eq!(
        row_media(false, false, false, false, false, "animation.gif"),
        RowMedia::Image
    );
    assert_eq!(
        row_media(false, false, false, false, false, "photo.PNG"),
        RowMedia::Image
    );
}

#[test]
fn row_media_direct_video_file() {
    assert_eq!(
        row_media(false, false, false, false, false, "movie.mkv"),
        RowMedia::VideoFile
    );
}

#[test]
fn row_media_direct_audio_file() {
    assert_eq!(
        row_media(false, false, false, false, false, "song.flac"),
        RowMedia::AudioFile
    );
}

#[test]
fn row_media_document_and_archive() {
    assert_eq!(
        row_media(false, false, false, false, false, "paper.pdf"),
        RowMedia::Document
    );
    assert_eq!(
        row_media(false, false, false, false, false, "data.zip"),
        RowMedia::Archive
    );
}

#[test]
fn row_media_icon_names_are_real_adwaita_symbolic_names() {
    // Each name checked against /usr/share/icons/Adwaita/symbolic: a missing
    // name would render a broken-image icon in every row.
    assert_eq!(RowMedia::Live.icon_name(), "media-record-symbolic");
    assert_eq!(RowMedia::Audio.icon_name(), "audio-headphones-symbolic");
    assert_eq!(RowMedia::Torrent.icon_name(), "network-workgroup-symbolic");
    assert_eq!(RowMedia::Video.icon_name(), "video-display-symbolic");
    assert_eq!(RowMedia::Gallery.icon_name(), "folder-pictures-symbolic");
    assert_eq!(RowMedia::Image.icon_name(), "image-x-generic-symbolic");
    assert_eq!(RowMedia::VideoFile.icon_name(), "video-x-generic-symbolic");
    assert_eq!(RowMedia::AudioFile.icon_name(), "audio-x-generic-symbolic");
    assert_eq!(RowMedia::Document.icon_name(), "application-pdf-symbolic");
    assert_eq!(RowMedia::Archive.icon_name(), "package-x-generic-symbolic");
    assert_eq!(
        RowMedia::File.icon_name(),
        "application-octet-stream-symbolic"
    );
}

#[test]
fn media_icon_failed_reserves_red_for_failure() {
    use DownloadStatus::*;
    // Red is the failure signal: only a failed download tints its icon.
    assert!(media_icon_failed(Failed));
    // Every other state — live capture included — stays neutral.
    for s in [Queued, Scheduled, Downloading, Paused, Done, Cancelled] {
        assert!(!media_icon_failed(s), "{s:?} must not take the error tint");
    }
}

/// The downloads stack is hidden while a picker page is visible. If a queue
/// update lands while hidden and its usual manager callback is missed, going
/// back to the form must reconcile the model instead of leaving the queue blank
/// until the next launch. Run under Xvfb with the UI-test command in ci.yml.
#[test]
#[ignore = "needs a display; run under Xvfb (see ci.yml)"]
fn queue_row_resynced_after_picker_returns_to_form() {
    // SAFETY: UI tests run serially; no other thread reads these variables
    // while this process-global setup runs.
    unsafe {
        std::env::set_var("GSETTINGS_SCHEMA_DIR", env!("GRAB_SCHEMA_DIR"));
        std::env::set_var("GSETTINGS_BACKEND", "memory");
    }
    gtk4::init().expect("gtk::init (run under Xvfb)");

    let app = libadwaita::Application::builder()
        .application_id("io.github.linuxuser67.Grab.UiTest")
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(None::<&gtk4::gio::Cancellable>)
        .expect("register UI test application before building its window");
    let settings = crate::settings::AppSettings::new();
    let manager = crate::download::DownloadManager::new(
        gtk4::gio::ListStore::new::<crate::download::DownloadItem>(),
        settings.clone(),
    );
    let toasts = std::rc::Rc::new(libadwaita::ToastOverlay::new());
    let (window, _, add_card) =
        crate::window::build_window(&app, manager.clone(), settings, toasts);
    let list_clamp = find_widget_by_name(window.upcast_ref(), "download-list-clamp")
        .expect("the download sections should be inside the HIG width clamp");
    assert!(list_clamp.is::<libadwaita::Clamp>());
    assert_eq!(
        list_clamp.property::<i32>("maximum-size"),
        800,
        "the filter and download rows should share a centered 800px column"
    );
    window.present();
    drain_gtk_events();

    let nav = add_card.navigation();
    let picker_child = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    let picker_page = libadwaita::NavigationPage::builder()
        .tag("gallery-picker")
        .title("Gallery")
        .child(&picker_child)
        .build();
    nav.push(&picker_page);
    drain_gtk_events();
    assert_eq!(nav.visible_page_tag().as_deref(), Some("gallery-picker"));

    // Simulate the exceptional timing: the model gets its persisted row while
    // the queue stack is hidden, but no manager change callback reaches sync().
    let item =
        crate::download::DownloadItem::new(1, "https://example.com/gallery", "Gallery", "/tmp");
    item.set_is_gallery(true);
    manager.store().append(&item);
    assert_eq!(count_queue_rows(window.upcast_ref()), 0);

    assert!(nav.pop());
    drain_gtk_events();
    assert_eq!(nav.visible_page_tag().as_deref(), Some("form"));
    assert_eq!(
        count_queue_rows(window.upcast_ref()),
        1,
        "the hidden queue should be rebuilt when the form returns"
    );
    window.destroy();
}

fn drain_gtk_events() {
    let context = gtk4::glib::MainContext::default();
    for _ in 0..50 {
        if !context.pending() {
            break;
        }
        context.iteration(false);
    }
}

fn find_widget_by_name(widget: &gtk4::Widget, name: &str) -> Option<gtk4::Widget> {
    if widget.widget_name().as_str() == name {
        return Some(widget.clone());
    }

    let mut child = widget.first_child();
    while let Some(current) = child {
        if let Some(found) = find_widget_by_name(&current, name) {
            return Some(found);
        }
        child = current.next_sibling();
    }
    None
}

fn count_queue_rows(widget: &gtk4::Widget) -> usize {
    if let Some(list) = widget.downcast_ref::<gtk4::ListBox>() {
        let is_queue_section = list
            .parent()
            .and_downcast::<gtk4::Box>()
            .and_then(|section| section.first_child())
            .and_downcast::<gtk4::Label>()
            .is_some_and(|label| {
                [
                    gettextrs::gettext("Active"),
                    gettextrs::gettext("Queued"),
                    gettextrs::gettext("Done"),
                ]
                .iter()
                .any(|title| title == label.text().as_str())
            });
        if is_queue_section {
            let mut rows = 0;
            let mut child = list.first_child();
            while let Some(current) = child {
                rows += usize::from(current.downcast_ref::<gtk4::ListBoxRow>().is_some());
                child = current.next_sibling();
            }
            return rows;
        }
    }

    let mut count = 0;
    let mut child = widget.first_child();
    while let Some(current) = child {
        count += count_queue_rows(&current);
        child = current.next_sibling();
    }
    count
}

/// Rows the selected filter actually shows; hidden rows stay parented in their
/// section, so `is_visible` is what the filter toggles.
fn count_visible_rows(widget: &gtk4::Widget) -> usize {
    let mut count = usize::from(
        widget
            .downcast_ref::<gtk4::ListBoxRow>()
            .is_some_and(|row| row.is_visible()),
    );
    let mut child = widget.first_child();
    while let Some(current) = child {
        count += count_visible_rows(&current);
        child = current.next_sibling();
    }
    count
}

/// Queue status navigation uses libadwaita's adaptive view-switcher pattern.
/// The selected page owns the shared queue scroller; no row widgets are duplicated.
#[test]
#[ignore = "needs a display; run under Xvfb (see ci.yml)"]
fn status_views_use_gnome_adaptive_view_switcher() {
    // SAFETY: UI tests run serially; no other thread reads these variables
    // while this process-global setup runs.
    unsafe {
        std::env::set_var("GSETTINGS_SCHEMA_DIR", env!("GRAB_SCHEMA_DIR"));
        std::env::set_var("GSETTINGS_BACKEND", "memory");
    }
    gtk4::init().expect("gtk::init (run under Xvfb)");

    let app = libadwaita::Application::builder()
        .application_id("io.github.linuxuser67.Grab.AdaptiveViewSwitcherTest")
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(None::<&gtk4::gio::Cancellable>)
        .expect("register test application before building its window");

    for (width, narrow) in [(460, true), (960, false)] {
        let settings = crate::settings::AppSettings::new();
        let manager = crate::download::DownloadManager::new(
            gtk4::gio::ListStore::new::<crate::download::DownloadItem>(),
            settings.clone(),
        );
        let queued = crate::download::DownloadItem::new(
            1,
            "https://example.com/queued",
            "queued.bin",
            "/tmp",
        );
        let active = crate::download::DownloadItem::new(
            2,
            "https://example.com/active",
            "active.bin",
            "/tmp",
        );
        active.set_status(crate::download::DownloadStatus::Paused);
        let scheduled = crate::download::DownloadItem::new(
            3,
            "https://example.com/scheduled",
            "scheduled.bin",
            "/tmp",
        );
        scheduled.set_status(crate::download::DownloadStatus::Scheduled);
        let downloaded = crate::download::DownloadItem::new(
            4,
            "https://example.com/downloaded",
            "downloaded.bin",
            "/tmp",
        );
        downloaded.set_status(crate::download::DownloadStatus::Done);
        manager.store().append(&queued);
        manager.store().append(&active);
        manager.store().append(&scheduled);
        manager.store().append(&downloaded);
        let toasts = std::rc::Rc::new(libadwaita::ToastOverlay::new());
        let (window, _, add_card) = crate::window::build_window(&app, manager, settings, toasts);
        window.set_default_size(width, 500);
        window.present();
        drain_gtk_events();

        assert_eq!(
            window.title().as_deref(),
            Some("Grab"),
            "the window title must be the application name without a subtitle"
        );
        let header = find_widget_by_name(window.upcast_ref(), "main-header")
            .expect("main header exists")
            .downcast::<libadwaita::HeaderBar>()
            .expect("main header is an AdwHeaderBar");
        assert_eq!(
            header.title_widget().is_some(),
            !narrow,
            "wide mode owns the header title widget; narrow mode restores the window title"
        );
        if !narrow {
            assert_eq!(
                header
                    .title_widget()
                    .expect("wide header contains the view switcher")
                    .widget_name()
                    .as_str(),
                "download-status-view-switcher"
            );
        }

        let switcher_bar =
            find_widget_by_name(window.upcast_ref(), "download-status-view-switcher-bar")
                .expect("bottom AdwViewSwitcherBar exists")
                .downcast::<libadwaita::ViewSwitcherBar>()
                .expect("bottom navigation uses AdwViewSwitcherBar");
        assert_eq!(
            switcher_bar.reveals(),
            narrow,
            "the bottom navigation is revealed only at narrow widths"
        );

        let view_stack = find_widget_by_name(window.upcast_ref(), "download-status-stack")
            .expect("queue AdwViewStack exists")
            .downcast::<libadwaita::ViewStack>()
            .expect("status navigation is backed by AdwViewStack");
        for (name, expected_icon) in [
            ("all", "view-list-symbolic"),
            ("active", "content-loading-symbolic"),
            ("downloaded", "folder-download-symbolic"),
        ] {
            let child = view_stack
                .child_by_name(name)
                .expect("each status view is registered");
            assert_eq!(
                view_stack.page(&child).icon_name().as_deref(),
                Some(expected_icon),
                "the {name} tab keeps its symbolic icon in both navigation layouts"
            );
            if name == "downloaded" {
                assert_eq!(
                    view_stack.page(&child).title().as_deref(),
                    Some(gettextrs::gettext("Done").as_str()),
                    "the completed-download tab is labelled Done"
                );
            }
        }

        let scroll = find_widget_by_name(window.upcast_ref(), "download-list-scroll")
            .expect("the shared queue scroller exists");
        let active_section = find_widget_by_name(window.upcast_ref(), "download-section-active")
            .expect("active downloads section exists");
        let queued_section = find_widget_by_name(window.upcast_ref(), "download-section-queued")
            .expect("queued downloads section exists");
        let downloaded_section =
            find_widget_by_name(window.upcast_ref(), "download-section-downloaded")
                .expect("downloaded downloads section exists");
        let downloaded_heading = downloaded_section
            .first_child()
            .and_downcast::<gtk4::Label>()
            .expect("Done section heading exists");
        let expected_done_title = gettextrs::gettext("Done");
        assert_eq!(
            downloaded_heading.text().as_str(),
            expected_done_title.as_str()
        );

        for name in ["active", "downloaded", "all"] {
            view_stack.set_visible_child_name(name);
            drain_gtk_events();
            assert_eq!(
                view_stack.visible_child_name().as_deref(),
                Some(name),
                "the view switcher changes the selected status page"
            );
            let expected_parent = format!("status-page-{name}");
            assert_eq!(
                scroll
                    .parent()
                    .expect("the scroller is attached to the selected page")
                    .widget_name()
                    .to_string(),
                expected_parent,
                "the single queue scroller follows the selected page"
            );
            assert_eq!(
                active_section.is_visible(),
                name == "active" || name == "all",
                "active rows are shown only in the Active and All views"
            );
            assert_eq!(
                queued_section.is_visible(),
                name == "active" || name == "all",
                "queued rows join the Active filter and stay in the All view"
            );
            assert_eq!(
                downloaded_section.is_visible(),
                name == "downloaded" || name == "all",
                "downloaded rows are shown only in the Downloaded and All views"
            );
            // Paused and scheduled rows live in the Active section, queued rows in
            // their own; every page must show exactly the rows it filters in.
            let expected_rows = match name {
                "active" => (2, 1, 0),
                "downloaded" => (0, 0, 1),
                _ => (2, 1, 1),
            };
            assert_eq!(
                (
                    count_visible_rows(&active_section),
                    count_visible_rows(&queued_section),
                    count_visible_rows(&downloaded_section),
                ),
                expected_rows,
                "the {name} view shows exactly its own rows"
            );
        }

        let add_button = find_widget_by_name(window.upcast_ref(), "add-download-button")
            .expect("New Download remains in the header");
        assert_eq!(
            add_button
                .next_sibling()
                .map(|widget| widget.widget_name().to_string())
                .as_deref(),
            Some("main-menu-button"),
            "the primary menu is immediately beside the window controls, after New Download"
        );

        let search_bar = find_widget_by_name(window.upcast_ref(), "download-search-bar")
            .expect("the stock GTK search bar is present")
            .downcast::<gtk4::SearchBar>()
            .expect("queue search uses GtkSearchBar");
        assert!(
            search_bar
                .child()
                .is_some_and(|child| child.is::<gtk4::SearchEntry>()),
            "GtkSearchBar contains GtkSearchEntry directly rather than a custom card"
        );
        assert!(
            search_bar.property::<bool>("show-close-button"),
            "the stock GtkSearchBar close control is enabled"
        );
        let search_toggle = find_widget_by_name(window.upcast_ref(), "search-downloads-button")
            .expect("header search toggle exists")
            .downcast::<gtk4::ToggleButton>()
            .expect("search action is a toggle");
        search_bar.set_search_mode(true);
        drain_gtk_events();
        assert!(search_toggle.is_active(), "the header toggle follows an enabled search mode");
        search_bar.set_search_mode(false);
        drain_gtk_events();
        assert!(!search_toggle.is_active(), "closing search clears the header toggle");

        let picker_child = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        let picker_page = libadwaita::NavigationPage::builder()
            .tag("adaptive-view-switcher-test-picker")
            .title("Picker")
            .child(&picker_child)
            .build();
        add_card.navigation().push(&picker_page);
        drain_gtk_events();
        assert!(
            !switcher_bar.reveals(),
            "queue navigation is hidden while a picker page is visible"
        );
        assert!(
            header.title_widget().is_none(),
            "the queue view switcher leaves the header while a picker is visible"
        );
        assert!(
            !find_widget_by_name(window.upcast_ref(), "search-downloads-button")
                .expect("search button exists")
                .is_visible(),
            "queue search is hidden while a picker is visible"
        );

        assert!(
            add_card.navigation().pop(),
            "the picker page should return to the download form"
        );
        drain_gtk_events();
        assert_eq!(
            header.title_widget().is_some(),
            !narrow,
            "wide header navigation is restored after leaving the picker"
        );
        assert_eq!(
            switcher_bar.reveals(),
            narrow,
            "narrow bottom navigation is restored after leaving the picker"
        );

        window.destroy();
        drain_gtk_events();
    }
}
