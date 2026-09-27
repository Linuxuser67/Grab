use crate::download::DownloadManager;
use adw::prelude::*;
use gettextrs::gettext;
use gtk4::prelude::*;
use gtk4::{gio, glib};
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

/// Facade: the add-panel flow lives in [`window_dialogs`](crate::window_dialogs)
/// now; these re-exports keep the in-tree `crate::window::X` paths working.
pub use crate::window_dialogs::{AddPanel, show_torrent_files_dialog};
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
) -> (adw::ApplicationWindow, gtk4::SearchBar, Rc<AddPanel>) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Grab")
        .default_width(settings.window_width().max(400))
        .default_height(settings.window_height().max(300))
        .build();

    // Persistent sidebar: the add form lives in the split view's sidebar on
    // the start side, with the filter navigation below it; the main UI is the
    // content. Toasts stay outermost and overlay both.
    let split = adw::NavigationSplitView::new();
    split.set_sidebar_position(gtk4::PackType::Start);
    split.set_min_sidebar_width(300.0);
    split.set_max_sidebar_width(340.0);
    let (add_panel, form_nav) = AddPanel::new(Rc::clone(&manager), &window, &split, &toasts);
    let sidebar_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    form_nav.set_vexpand(true);
    sidebar_box.append(&form_nav);
    let filter_separator = gtk4::Separator::new(gtk4::Orientation::Horizontal);
    sidebar_box.append(&filter_separator);
    // Filter navigation: the sidebar's HIG navigation pattern, one view per
    // download state. Replaces the search-bar status dropdown. Categories
    // hide when they have no downloads; the section itself is an ExpanderRow
    // (the HIG collapsible) and hides when there is nothing to filter.
    let filter_sel: Rc<Cell<u32>> = Rc::new(Cell::new(0));
    let filter_expander = adw::ExpanderRow::builder()
        .title(gettext("Status"))
        .subtitle(gettext("All"))
        .expanded(true)
        .build();
    let filter_names: Vec<String> = vec![
        gettext("All"),
        gettext("Downloading"),
        gettext("Paused"),
        gettext("Queued"),
        gettext("Done"),
        gettext("Failed"),
        gettext("Cancelled"),
    ];
    let filter_rows: Rc<Vec<(adw::ActionRow, gtk4::Image)>> = Rc::new(
        filter_names
            .iter()
            .map(|name| {
                let check = gtk4::Image::from_icon_name("object-select-symbolic");
                check.set_valign(gtk4::Align::Center);
                let row = adw::ActionRow::builder()
                    .title(name)
                    .activatable(true)
                    .build();
                row.add_prefix(&check);
                filter_expander.add_row(&row);
                (row, check)
            })
            .collect(),
    );
    sidebar_box.append(&filter_expander);
    // Echo the pick: checkmark on the selected row, subtitle on the expander.
    let refresh_filter_ui: Rc<dyn Fn()> = {
        let sel = Rc::clone(&filter_sel);
        let expander = filter_expander.clone();
        let rows = Rc::clone(&filter_rows);
        let names = filter_names.clone();
        Rc::new(move || {
            let s = sel.get() as usize;
            for (j, (_, check)) in rows.iter().enumerate() {
                check.set_visible(j == s);
            }
            if let Some(name) = names.get(s) {
                expander.set_subtitle(name);
            }
        })
    };
    refresh_filter_ui();
    // Narrow windows navigate between the sidebar and the list instead of
    // showing both; the sidebar gets its own headerbar there so the panel
    // can be collapsed back. Hidden on wide windows where the sidebar is
    // persistent.
    let sidebar_header = adw::HeaderBar::new();
    let sidebar_add = gtk4::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text(gettext("Hide Sidebar"))
        .build();
    sidebar_add.update_property(&[gtk4::accessible::Property::Label(&gettext("Hide Sidebar"))]);
    sidebar_header.pack_end(&sidebar_add);
    split
        .bind_property("collapsed", &sidebar_header, "visible")
        .sync_create()
        .build();
    {
        let sp = split.clone();
        sidebar_add.connect_clicked(move |_| sp.set_show_content(true));
    }
    let sidebar_toolbar = adw::ToolbarView::new();
    sidebar_toolbar.add_top_bar(&sidebar_header);
    sidebar_toolbar.set_content(Some(&sidebar_box));
    // NavigationSplitView only takes NavigationPage children.
    let sidebar_page = adw::NavigationPage::builder()
        .child(&sidebar_toolbar)
        .tag("sidebar")
        .build();
    split.set_sidebar(Some(&sidebar_page));
    // Narrow windows navigate between the sidebar and the content instead of
    // squeezing both side by side: below 800px the split collapses.
    {
        let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            800.0,
            adw::LengthUnit::Px,
        ));
        breakpoint.add_setter(&split, "collapsed", Some(&glib::Value::from(true)));
        window.add_breakpoint(breakpoint);
    }

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
    // Sidebar toggle for collapsed (narrow) windows: flips between the
    // sidebar and the download list. Hidden while both fit side by side.
    let sidebar_toggle = gtk4::ToggleButton::builder()
        .icon_name("sidebar-show-symbolic")
        .tooltip_text(gettext("Show Sidebar"))
        .build();
    sidebar_toggle.update_property(&[gtk4::accessible::Property::Label(&gettext("Show Sidebar"))]);
    header.pack_start(&sidebar_toggle);
    split
        .bind_property("collapsed", &sidebar_toggle, "visible")
        .sync_create()
        .build();
    // The toggle mirrors sidebar visibility (active = sidebar shown), so it
    // stays in sync when focus_form() reveals the sidebar too.
    split
        .bind_property("show-content", &sidebar_toggle, "active")
        .flags(
            glib::BindingFlags::BIDIRECTIONAL
                | glib::BindingFlags::SYNC_CREATE
                | glib::BindingFlags::INVERT_BOOLEAN,
        )
        .build();
    {
        split.connect_collapsed_notify(|s| {
            if !s.is_collapsed() {
                s.set_show_content(true);
            }
        });
    }
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
        let panel = Rc::clone(&add_panel);
        let sp = split.clone();
        add_btn.connect_clicked(move |_| {
            // In collapsed mode the + toggles the panel: collapse it when
            // it's showing, otherwise reveal and focus the form.
            if sp.is_collapsed() && !sp.shows_content() {
                sp.set_show_content(true);
            } else {
                panel.focus_form(None);
            }
        });
    }
    header.pack_start(&add_btn);

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
        let panel = Rc::clone(&add_panel);
        empty_add.connect_clicked(move |_| panel.focus_form(None));
    }
    stack.add_named(&empty, Some("empty"));
    let nomatch = adw::StatusPage::builder()
        .icon_name("system-search-symbolic")
        .title(gettext("No Downloads Match"))
        .description(gettext("Try a different search or filter"))
        .build();
    stack.add_named(&nomatch, Some("nomatch"));

    // Single filtered list: the sidebar navigation picks the state, so the
    // old Active / Queued / Downloaded sections flatten into one view.
    let list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
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

    /// Sidebar filter position for a status (0 = All); order must match the
    /// navigation list rows.
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

    // Search narrows the sidebar filter's view: non-matching rows are hidden
    // in sync().
    let query: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
    let search = gtk4::SearchEntry::builder()
        .placeholder_text(gettext("Search downloads"))
        .hexpand(true)
        .build();
    // HIG search pattern: a header toggle reveals a GtkSearchBar holding the
    // entry.
    let search_bar = gtk4::SearchBar::builder().show_close_button(true).build();
    search_bar.set_child(Some(&search));
    search_bar.connect_entry(&search);
    search_bar.set_key_capture_widget(Some(&window));
    search_toggle
        .bind_property("active", &search_bar, "search-mode-enabled")
        .bidirectional()
        .sync_create()
        .build();
    content.append(&list);

    let rows: Rc<RefCell<HashMap<u64, gtk4::ListBoxRow>>> = Rc::new(RefCell::new(HashMap::new()));
    let sync: Rc<dyn Fn()> = {
        let m = Rc::clone(&manager);
        let t = Rc::clone(&toasts);
        let r = Rc::clone(&rows);
        let add = add_btn.clone();
        let search_btn = search_toggle.clone();
        let s = stack.clone();
        let l = list.clone();
        let sp = split.clone();
        let query = Rc::clone(&query);
        let filter_sel = Rc::clone(&filter_sel);
        let filter_rows = Rc::clone(&filter_rows);
        let filter_expander = filter_expander.clone();
        let filter_separator = filter_separator.clone();
        let refresh_filter_ui = Rc::clone(&refresh_filter_ui);
        Rc::new(move || {
            let store = m.store();
            let q = query.borrow();
            // Count per category first: empty ones hide, and a selected
            // category that emptied falls back to All.
            let mut counts = [0u32; 7];
            for i in 0..store.n_items() {
                if let Some(it) = store
                    .item(i)
                    .and_downcast::<crate::download::DownloadItem>()
                {
                    counts[0] += 1;
                    counts[status_filter_index(it.status()) as usize] += 1;
                }
            }
            if filter_sel.get() != 0 && counts[filter_sel.get() as usize] == 0 {
                filter_sel.set(0);
                refresh_filter_ui();
            }
            let sel = filter_sel.get();
            for (j, (row, _)) in filter_rows.iter().enumerate() {
                row.set_visible(j == 0 && counts[0] > 0 || j > 0 && counts[j] > 0);
            }
            let has_downloads = counts[0] > 0;
            filter_expander.set_visible(has_downloads);
            filter_separator.set_visible(has_downloads);
            let mut present = std::collections::HashSet::new();
            let mut n_visible = 0;
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
                        n_visible += 1;
                    }
                    let existing = r.borrow().get(&it.id()).cloned();
                    let row = if let Some(row) = existing {
                        row
                    } else {
                        let row = build_row(&it, &m, &t);
                        r.borrow_mut().insert(it.id(), row.clone());
                        row
                    };
                    if !row.is_ancestor(&l) {
                        if let Some(old) = row.parent().and_downcast::<gtk4::ListBox>() {
                            old.remove(&row);
                        }
                        l.append(&row);
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
            let has_items = store.n_items() > 0;
            // The sidebar form is the add UI on wide windows; the header +
            // only shows while the sidebar is collapsed (narrow).
            add.set_visible(has_items && sp.is_collapsed());
            search_btn.set_visible(has_items);
            if !has_items {
                // List is gone, so nothing to search.
                search_btn.set_active(false);
            }
            s.set_visible_child_name(if !has_items {
                "empty"
            } else if n_visible == 0 {
                "nomatch"
            } else {
                "list"
            });
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
        // Activating a category picks the filter; on narrow windows it also
        // collapses the panel, returning to the list.
        let sync = Rc::clone(&sync);
        let sel = Rc::clone(&filter_sel);
        let refresh = Rc::clone(&refresh_filter_ui);
        let rows = Rc::clone(&filter_rows);
        let sp = split.clone();
        for (i, (row, _)) in rows.iter().enumerate() {
            let row = row.clone();
            let sync = Rc::clone(&sync);
            let sel = Rc::clone(&sel);
            let refresh = Rc::clone(&refresh);
            let sp = sp.clone();
            row.connect_activated(move |_| {
                sel.set(i as u32);
                refresh();
                sync();
                if sp.is_collapsed() {
                    sp.set_show_content(true);
                }
            });
        }
    }
    {
        let sync = Rc::clone(&sync);
        split.connect_collapsed_notify(move |_| sync());
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
    let content_page = adw::NavigationPage::builder()
        .child(&toolbar)
        .tag("content")
        .build();
    split.set_content(Some(&content_page));
    toasts.set_child(Some(&split));
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

    (window, search_bar, add_panel)
}

#[cfg(test)]
#[path = "window_tests.rs"]
mod tests;
