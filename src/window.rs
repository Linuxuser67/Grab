use crate::download::DownloadManager;
use adw::prelude::*;
use gettextrs::gettext;
use gtk4::prelude::*;
use gtk4::{gio, glib};
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

/// Facade: the dialog flow lives in [`window_dialogs`](crate::window_dialogs)
/// now; these re-exports keep the in-tree `crate::window::X` paths working.
pub use crate::window_dialogs::{show_add_dialog, show_torrent_files_dialog};
/// Facade: row widgets live in [`window_rows`](crate::window_rows) now.
use crate::window_rows::build_row;
pub use crate::window_rows::launch_path;
use crate::window_rows::ngettext_count;

pub const BACKGROUND_NOTIF_ID: &str = "grab-background";

/// Suspend block held through the desktop portal. `sub` watches its Response so
/// a denial clears the hold instead of pretending to block. Both die with the
/// process, which also releases the lock server-side.
struct InhibitState {
    request: Option<String>,
    sub: Option<gio::SignalSubscription>,
    seq: u64,
    /// An Inhibit round trip is in flight. `request` stays None until its reply
    /// lands, so without this every sync during the round trip would fire a
    /// duplicate request whose path gets overwritten and never closed.
    pending: bool,
}

/// Ask the portal to block suspend. Stores the request only if still wanted
/// when the reply lands; anything failing leaves nothing held.
async fn request_inhibit(
    state: Rc<RefCell<InhibitState>>,
    manager: Rc<DownloadManager>,
    settings: crate::settings::AppSettings,
) {
    const PORTAL: &str = "org.freedesktop.portal.Desktop";
    const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";
    const SUSPEND: u32 = 4;
    // Every exit below clears `pending`: a stuck true silences all future
    // inhibits, leaving the machine unblocked forever.
    let clear_pending = |state: &Rc<RefCell<InhibitState>>| {
        state.borrow_mut().pending = false;
    };
    let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await else {
        clear_pending(&state);
        tracing::warn!("suspend block unavailable (no session bus)");
        return; // Headless/test: no session bus, nothing to block on.
    };
    let token = {
        let mut st = state.borrow_mut();
        st.seq += 1;
        format!("grab{}", st.seq)
    };
    let options = glib::VariantDict::new(None);
    options.insert("handle_token", token);
    options.insert("reason", gettext("Downloading files"));
    // Flags ride positionally (sua{sv}), not in the options dict: the portal
    // rejects the call otherwise.
    let params = glib::variant::ToVariant::to_variant(&(String::new(), SUSPEND, options.end()));
    let Ok(reply) = conn
        .call_future(
            Some(PORTAL),
            DESKTOP_PATH,
            "org.freedesktop.portal.Inhibit",
            "Inhibit",
            Some(&params),
            None,
            gio::DBusCallFlags::NONE,
            -1,
        )
        .await
    else {
        clear_pending(&state);
        tracing::warn!("suspend block request failed");
        return;
    };
    let path = (reply.n_children() == 1)
        .then(|| reply.child_value(0))
        .and_then(|v| v.str().map(String::from));
    let Some(path) = path else {
        clear_pending(&state);
        tracing::warn!("suspend block reply had no request path");
        return;
    };
    // The queue may have idled during the round trip: close at once.
    if !(settings.inhibit_suspend() && manager.has_transferring()) {
        release_inhibit(conn, path).await;
        clear_pending(&state);
        return;
    }
    let st2 = Rc::clone(&state);
    let sub = conn.subscribe_to_signal(
        None,
        Some("org.freedesktop.portal.Request"),
        Some("Response"),
        Some(&path),
        None,
        gio::DBusSignalFlags::NONE,
        move |sig| {
            let denied = sig.parameters.n_children() != 2
                || sig.parameters.child_value(0).get::<u32>() != Some(0);
            if denied {
                let mut st = st2.borrow_mut();
                if st.request.as_deref() == Some(sig.object_path) {
                    st.request = None;
                    drop(st.sub.take());
                }
            }
        },
    );
    let mut st = state.borrow_mut();
    st.request = Some(path.clone());
    st.sub = Some(sub);
    st.pending = false;
    tracing::info!("suspend block held ({path})");
}

/// Release a held portal block. Fire-and-forget: the lock dies with the bus
/// connection anyway.
async fn release_inhibit(conn: gio::DBusConnection, path: String) {
    let _ = conn
        .call_future(
            Some("org.freedesktop.portal.Desktop"),
            &path,
            "org.freedesktop.portal.Request",
            "Close",
            None,
            None,
            gio::DBusCallFlags::NONE,
            -1,
        )
        .await;
}

/// Tell the desktop we keep running without windows (Background portal): the
/// cross-desktop way to survive window close on strict desktops, and what lists
/// Grab in the system's background-apps settings. A denial changes nothing;
/// no autostart is requested, so relaunch stays the user's choice.
fn request_background() {
    glib::spawn_future_local(async move {
        let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await else {
            return;
        };
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let token = format!(
            "grabbg{}",
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let options = glib::VariantDict::new(None);
        options.insert("handle_token", token);
        options.insert(
            "reason",
            gettext("Downloads continue in the background after the window is closed"),
        );
        options.insert("autostart", false);
        options.insert("background", true);
        let params = glib::variant::ToVariant::to_variant(&(String::new(), options.end()));
        let _ = conn
            .call_future(
                Some("org.freedesktop.portal.Desktop"),
                "/org/freedesktop/portal/desktop",
                "org.freedesktop.portal.Background",
                "RequestBackground",
                Some(&params),
                None,
                gio::DBusCallFlags::NONE,
                -1,
            )
            .await;
    });
}

pub fn build_window(
    app: &adw::Application,
    manager: Rc<DownloadManager>,
    settings: crate::settings::AppSettings,
    toasts: Rc<adw::ToastOverlay>,
) -> (adw::ApplicationWindow, gtk4::SearchBar) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Grab")
        .default_width(settings.window_width().max(400))
        .default_height(settings.window_height().max(300))
        .build();

    settings
        .bind(crate::settings::key::WINDOW_WIDTH, &window, "default-width")
        .build();
    settings
        .bind(
            crate::settings::key::WINDOW_HEIGHT,
            &window,
            "default-height",
        )
        .build();

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(
        &gettext("Grab"),
        &gettext("Download Manager"),
    )));

    let menu = gio::Menu::new();
    menu.append(Some(&gettext("New Download")), Some("app.add-download"));
    let section = gio::Menu::new();
    section.append(Some(&gettext("Cancel All")), Some("app.cancel-all"));
    section.append(Some(&gettext("Retry Failed")), Some("app.retry-failed"));
    section.append(Some(&gettext("Clear Finished")), Some("app.clear-finished"));
    section.append(
        Some(&gettext("Open Download Folder")),
        Some("app.open-folder"),
    );
    menu.append_section(None, &section);
    let section2 = gio::Menu::new();
    section2.append(Some(&gettext("Preferences")), Some("app.preferences"));
    section2.append(Some(&gettext("Keyboard Shortcuts")), Some("app.shortcuts"));
    section2.append(Some(&gettext("About")), Some("app.about"));
    menu.append_section(None, &section2);
    let menu_btn = gtk4::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .tooltip_text(gettext("Main Menu"))
        .build();
    menu_btn.update_property(&[gtk4::accessible::Property::Label(&gettext("Main Menu"))]);
    header.pack_start(&menu_btn);

    let search_toggle = gtk4::ToggleButton::builder()
        .icon_name("system-search-symbolic")
        .tooltip_text(gettext("Search (Ctrl+F)"))
        .build();
    search_toggle.update_property(&[gtk4::accessible::Property::Label(&gettext("Search"))]);
    header.pack_end(&search_toggle);

    let add_btn = gtk4::Button::builder()
        .icon_name("list-add-symbolic")
        .css_classes(["suggested-action"])
        .tooltip_text(gettext("New Download (Ctrl+N)"))
        .build();
    add_btn.update_property(&[gtk4::accessible::Property::Label(&gettext("New Download"))]);
    {
        let m = Rc::clone(&manager);
        add_btn.connect_clicked(move |_| show_add_dialog(m.clone(), None));
    }
    header.pack_end(&add_btn);

    let stack = adw::ViewStack::new();
    let empty = adw::StatusPage::builder()
        .icon_name("folder-download-symbolic")
        .title(gettext("No Downloads Yet"))
        .description(gettext("Add a download to get started"))
        .build();
    let empty_add = gtk4::Button::builder()
        .label(gettext("New Download"))
        .css_classes(["pill", "suggested-action"])
        .halign(gtk4::Align::Center)
        .build();
    empty.set_child(Some(&empty_add));
    {
        let m = Rc::clone(&manager);
        empty_add.connect_clicked(move |_| show_add_dialog(m.clone(), None));
    }
    stack.add_named(&empty, Some("empty"));

    fn section_list(title: &str) -> (gtk4::Box, gtk4::ListBox) {
        let label = gtk4::Label::builder()
            .label(title)
            .halign(gtk4::Align::Start)
            .css_classes(["heading"])
            .build();
        let list = gtk4::ListBox::builder()
            .selection_mode(gtk4::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let section = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        section.append(&label);
        section.append(&list);
        (section, list)
    }
    let (active_section, active_list) = section_list(&gettext("Active"));
    let (queued_section, queued_list) = section_list(&gettext("Queued"));
    let (downloaded_section, downloaded_list) = section_list(&gettext("Downloaded"));
    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .child(&content)
        .build();
    stack.add_named(&scroll, Some("list"));

    fn is_done(it: &crate::download::DownloadItem) -> bool {
        it.status() == crate::download::DownloadStatus::Done
    }
    fn is_queued(it: &crate::download::DownloadItem) -> bool {
        it.status() == crate::download::DownloadStatus::Queued
    }
    /// DropDown position for a status (0 = All); order must match the model.
    fn status_filter_index(s: crate::download::DownloadStatus) -> u32 {
        use crate::download::DownloadStatus::*;
        match s {
            Downloading => 1,
            Paused => 2,
            Queued => 3,
            Done => 4,
            Failed => 5,
            Cancelled => 6,
        }
    }

    // Search + status filter above the sections: non-matching rows are hidden
    // in sync(), and empty sections collapse as usual.
    let query: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
    let status_sel: Rc<Cell<u32>> = Rc::new(Cell::new(0));
    let search = gtk4::SearchEntry::builder()
        .placeholder_text(gettext("Search downloads"))
        .hexpand(true)
        .build();
    let status_names = gtk4::StringList::new(&[]);
    for name in [
        gettext("All"),
        gettext("Downloading"),
        gettext("Paused"),
        gettext("Queued"),
        gettext("Done"),
        gettext("Failed"),
        gettext("Cancelled"),
    ] {
        status_names.append(&name);
    }
    let status_drop = gtk4::DropDown::builder()
        .model(&status_names)
        .selected(0)
        .valign(gtk4::Align::Center)
        .build();
    status_drop.update_property(&[gtk4::accessible::Property::Label(&gettext(
        "Filter by status",
    ))]);
    // HIG search pattern: a header toggle reveals a GtkSearchBar that may also
    // hold extra widgets like the status filter.
    let search_bar = gtk4::SearchBar::builder().show_close_button(true).build();
    let filter_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    filter_box.append(&search);
    filter_box.append(&status_drop);
    search_bar.set_child(Some(&filter_box));
    search_bar.connect_entry(&search);
    search_bar.set_key_capture_widget(Some(&window));
    search_toggle
        .bind_property("active", &search_bar, "search-mode-enabled")
        .bidirectional()
        .sync_create()
        .build();
    content.append(&active_section);
    content.append(&queued_section);
    content.append(&downloaded_section);

    let rows: Rc<RefCell<HashMap<u64, gtk4::ListBoxRow>>> = Rc::new(RefCell::new(HashMap::new()));
    let sync: Rc<dyn Fn()> = {
        let m = Rc::clone(&manager);
        let t = Rc::clone(&toasts);
        let r = Rc::clone(&rows);
        let add = add_btn.clone();
        let search_btn = search_toggle.clone();
        let s = stack.clone();
        let l_active = active_list.clone();
        let l_queued = queued_list.clone();
        let l_downloaded = downloaded_list.clone();
        let sec_active = active_section.clone();
        let sec_queued = queued_section.clone();
        let sec_downloaded = downloaded_section.clone();
        let query = Rc::clone(&query);
        let status_sel = Rc::clone(&status_sel);
        Rc::new(move || {
            let store = m.store();
            let q = query.borrow();
            let sel = status_sel.get();
            let mut present = std::collections::HashSet::new();
            let mut n_active = 0;
            let mut n_queued = 0;
            let mut n_downloaded = 0;
            for i in 0..store.n_items() {
                if let Some(it) = store
                    .item(i)
                    .and_downcast::<crate::download::DownloadItem>()
                {
                    present.insert(it.id());
                    let mut shown = sel == 0 || status_filter_index(it.status()) == sel;
                    if shown && !q.is_empty() && !it.filename().to_lowercase().contains(q.as_str())
                    {
                        shown = false;
                    }
                    if shown {
                        if is_done(&it) {
                            n_downloaded += 1;
                        } else if is_queued(&it) {
                            n_queued += 1;
                        } else {
                            n_active += 1;
                        }
                    }
                    let existing = r.borrow().get(&it.id()).cloned();
                    let row = if let Some(row) = existing {
                        row
                    } else {
                        let row = build_row(&it, &m, &t);
                        r.borrow_mut().insert(it.id(), row.clone());
                        row
                    };
                    let target = if is_done(&it) {
                        &l_downloaded
                    } else if is_queued(&it) {
                        &l_queued
                    } else {
                        &l_active
                    };
                    if !row.is_ancestor(target) {
                        if let Some(old) = row.parent().and_downcast::<gtk4::ListBox>() {
                            old.remove(&row);
                        }
                        target.append(&row);
                    }
                    row.set_visible(shown);
                }
            }
            let stale: Vec<u64> = r
                .borrow()
                .keys()
                .filter(|id| !present.contains(id))
                .cloned()
                .collect();
            for id in stale {
                if let Some(row) = r.borrow_mut().remove(&id)
                    && let Some(old) = row.parent().and_downcast::<gtk4::ListBox>()
                {
                    old.remove(&row);
                }
            }
            sec_active.set_visible(n_active > 0);
            sec_queued.set_visible(n_queued > 0);
            sec_downloaded.set_visible(n_downloaded > 0);
            let has_items = store.n_items() > 0;
            // Header + duplicates the empty-state pill, so show it only with the list.
            add.set_visible(has_items);
            search_btn.set_visible(has_items);
            if !has_items {
                // List is gone, so nothing to search.
                search_btn.set_active(false);
            }
            s.set_visible_child_name(if has_items { "list" } else { "empty" });
        })
    };

    {
        let sync = Rc::clone(&sync);
        let q = Rc::clone(&query);
        search.connect_search_changed(move |s| {
            *q.borrow_mut() = s.text().to_lowercase();
            sync();
        });
    }
    {
        let sync = Rc::clone(&sync);
        let sel = Rc::clone(&status_sel);
        status_drop.connect_selected_notify(move |d| {
            sel.set(d.selected());
            sync();
        });
    }

    // hidden window keeps its widget tree (~MBs) while headless; destroy+rebuild if that ever matters.
    let ever_shown = Rc::new(Cell::new(false));
    {
        let m = Rc::clone(&manager);
        window.connect_close_request(move |win| {
            // Only hide to background while bytes are actually moving; paused
            // items (or none) quit normally, since "continues in the
            // background" would be a lie with nothing transferring.
            if m.has_transferring() {
                win.set_visible(false);
                request_background();
                if m.background_notifications_enabled()
                    && let Some(app) = gio::Application::default()
                {
                    let n =
                        gio::Notification::new(&gettext("Downloads continue in the background"));
                    n.set_default_action_and_target_value("app.present", None);
                    app.send_notification(Some(BACKGROUND_NOTIF_ID), &n);
                }
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
    }
    {
        let armed = Rc::clone(&ever_shown);
        window.connect_map(move |_| armed.set(true));
    }

    let banner = adw::Banner::new(&gettext("Some downloads failed"));
    banner.set_button_label(Some(&gettext("Retry Failed")));
    {
        let m = Rc::clone(&manager);
        let t = Rc::clone(&toasts);
        banner.connect_button_clicked(move |_| {
            let n = m.retry_failed();
            if n > 0 {
                t.add_toast(adw::Toast::new(&ngettext_count(
                    "Retrying failed download",
                    "Retrying {n} failed downloads",
                    n,
                )));
            }
        });
    }
    banner.set_revealed(false);

    // Sleep inhibition through the desktop portal (`org.freedesktop.portal.Inhibit`,
    // flag 4 = suspend): the cross-desktop path, sandbox-safe with no extra
    // permissions. GtkApplication's inhibit only speaks to GNOME SessionManager,
    // so KDE/Sway would silently never block.
    let inhibit = Rc::new(RefCell::new(InhibitState {
        request: None,
        sub: None,
        seq: 0,
        pending: false,
    }));
    let sync_inhibit: Rc<dyn Fn()> = {
        let m = Rc::clone(&manager);
        let s = settings.clone();
        let st = Rc::clone(&inhibit);
        Rc::new(move || {
            let want = s.boolean("inhibit-suspend") && m.has_transferring();
            // Claim the in-flight marker synchronously: without it, every sync
            // during the D-Bus round trip would fire a duplicate request whose
            // path the later reply overwrites and never closes.
            let launch = {
                let mut st = st.borrow_mut();
                if want && st.request.is_none() && !st.pending {
                    st.pending = true;
                    true
                } else {
                    false
                }
            };
            if launch {
                let (st2, m2, s2) = (Rc::clone(&st), Rc::clone(&m), s.clone());
                glib::spawn_future_local(async move {
                    request_inhibit(st2, m2, s2).await;
                });
            } else if !want {
                // Separate statements: the first borrow must end first, or
                // RefCell panics on release.
                let path = st.borrow_mut().request.take();
                drop(st.borrow_mut().sub.take());
                if let Some(path) = path {
                    glib::spawn_future_local(async move {
                        // Bus lookup stays async: a stalled portal must never
                        // stall the main loop from inside a change hook.
                        let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await else {
                            return;
                        };
                        release_inhibit(conn, path).await;
                    });
                }
            }
        })
    };
    {
        let inhibit = Rc::clone(&sync_inhibit);
        settings.connect_changed(Some("inhibit-suspend"), move |_, _| inhibit());
    }

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.add_top_bar(&search_bar);
    toolbar.add_top_bar(&banner);
    toolbar.set_content(Some(&stack));
    toasts.set_child(Some(&toolbar));
    window.set_content(Some(toasts.as_ref()));

    // Flip a named app action on/off; silently skips a missing one.
    let set_action = |app: &adw::Application, name: &str, enabled: bool| {
        if let Some(a) = app.lookup_action(name).and_downcast::<gio::SimpleAction>() {
            a.set_enabled(enabled);
        }
    };
    {
        let app_weak = app.downgrade();
        let m = Rc::clone(&manager);
        let sync = Rc::clone(&sync);
        let w = window.downgrade();
        let armed = Rc::clone(&ever_shown);
        let inhibit = Rc::clone(&sync_inhibit);
        let hook: Rc<dyn Fn()> = Rc::new(move || {
            sync();
            inhibit();
            if let Some(app) = app_weak.upgrade() {
                set_action(&app, "cancel-all", m.has_active());
                set_action(&app, "retry-failed", m.has_failed());
                set_action(&app, "clear-finished", m.finished_count() > 0);
            }
            banner.set_revealed(m.has_errored());
            // Same predicate as close-request: quit only when nothing is
            // transferring, since paused rows persist across launches and
            // counting them would strand a hidden zombie.
            let idle_hidden = armed.get()
                && !m.has_transferring()
                && w.upgrade().is_some_and(|win| !win.is_visible());
            if idle_hidden && let Some(app) = app_weak.upgrade() {
                app.withdraw_notification(BACKGROUND_NOTIF_ID);
                app.quit();
            }
        });
        manager.set_on_change({
            let hook = Rc::clone(&hook);
            move || hook()
        });
        hook();
    }

    (window, search_bar)
}

#[cfg(test)]
#[path = "window_tests.rs"]
mod tests;
