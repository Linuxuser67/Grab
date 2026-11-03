use crate::download::DownloadManager;
use adw::prelude::*;
use gettextrs::gettext;
use glib::variant::ToVariant;
use gtk4::prelude::*;
use gtk4::{gio, glib};
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

/// Facade: row widgets live in [`window_rows`](crate::window_rows) now.
use crate::window_rows::build_row;
pub use crate::window_rows::launch_path;
use crate::window_rows::ngettext_count;

pub const BACKGROUND_NOTIF_ID: &str = "grab-background";

/// Remove the empty icon slot from the wide/header view switcher.
///
/// AdwViewSwitcher always builds its buttons with an image and a label. The
/// page icons are cleared while wide, so collapse the corresponding box gap
/// only for this header switcher. The bottom ViewSwitcherBar is unaffected.
fn ensure_queue_view_switcher_css() {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    let Some(display) = gtk4::gdk::Display::default() else {
        return;
    };
    if ONCE.set(()).is_ok() {
        let provider = gtk4::CssProvider::new();
        provider.load_from_bytes(&glib::Bytes::from_static(
            b"#download-status-view-switcher viewswitcher > button.toggle > stack > box.wide { border-spacing: 0; }",
        ));
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

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
    // rejects the call otherwise. Build the tuple explicitly: the
    // tuple-ToVariant impl can wrap the dict's Variant in an extra
    // variant layer, producing (suv) instead of (sua{sv}).
    let params = glib::Variant::tuple_from_iter([
        String::new().to_variant(),
        SUSPEND.to_variant(),
        options.end(),
    ]);
    let reply = match conn
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
    {
        Ok(reply) => reply,
        Err(e) => {
            clear_pending(&state);
            tracing::warn!("suspend block request failed: {e}");
            return;
        }
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
        // Build explicitly (see Inhibit above): tuple-ToVariant can wrap
        // the dict's Variant in an extra layer.
        let params = glib::Variant::tuple_from_iter([String::new().to_variant(), options.end()]);
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

/// Hide on ordinary close while downloads transfer, but let an explicit
/// asynchronous shutdown finish closing the window.
pub(crate) fn should_hide_to_background(has_transferring: bool, shutting_down: bool) -> bool {
    has_transferring && !shutting_down
}

pub fn build_window(
    app: &adw::Application,
    manager: Rc<DownloadManager>,
    settings: crate::settings::AppSettings,
    toasts: Rc<adw::ToastOverlay>,
) -> (
    adw::ApplicationWindow,
    gtk4::ToggleButton,
    crate::inline_add::AddCard,
) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Grab")
        .default_width(settings.window_width().max(400))
        .default_height(settings.window_height().max(300))
        .width_request(360)
        .height_request(300)
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
    header.set_widget_name("main-header");
    // Inline New Download card, pinned under the header above the list (and
    // the empty state). The queue stays usable underneath it.
    let add_card = crate::inline_add::build_add_card(Rc::clone(&manager));
    // HIG search: the toggle button lives in the header; the search bar
    // slides down below it (GtkSearchBar's built-in animation), sized to
    // match the inline New Download card (12px margins).
    let search = gtk4::SearchEntry::builder()
        .placeholder_text(gettext("Search downloads"))
        .hexpand(true)
        .build();
    // In a PreferencesGroup like the torrent/save cards: same 12px internal
    // padding, same card styling, automatic alignment.
    let search_close = gtk4::Button::builder()
        .icon_name("window-close-symbolic")
        .css_classes(["flat", "circular"])
        .valign(gtk4::Align::Center)
        .tooltip_text(gettext("Close search"))
        .build();
    let search_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    search_row.append(&search);
    search_row.append(&search_close);
    let search_group = adw::PreferencesGroup::new();
    search_group.add(&search_row);
    let search_bar = gtk4::SearchBar::builder().child(&search_group).build();
    // Same 600px clamp as the New Download card.
    let search_clamp = adw::Clamp::builder()
        .maximum_size(600)
        .margin_start(12)
        .margin_end(12)
        .build();
    search_clamp.set_child(Some(&search_bar));

    let menu = gio::Menu::new();
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
        .primary(true)
        .menu_model(&menu)
        .tooltip_text(gettext("Main Menu"))
        .build();
    menu_btn.set_widget_name("main-menu-button");
    menu_btn.update_property(&[gtk4::accessible::Property::Label(&gettext("Main Menu"))]);
    // Keep the primary menu on the leading edge, as in the original header layout.
    header.pack_start(&menu_btn);

    let search_toggle = gtk4::ToggleButton::builder()
        .icon_name("system-search-symbolic")
        .tooltip_text(gettext("Search (Ctrl+F)"))
        .build();
    search_toggle.set_widget_name("search-downloads-button");
    search_toggle.update_property(&[gtk4::accessible::Property::Label(&gettext("Search"))]);

    // Stateful toggle: reflects the card's open state (set_on_state_changed
    // mirrors Escape / successful-add / toggle closes too), so the button
    // itself is the dismissal affordance — no separate close button needed.
    let add_btn = gtk4::ToggleButton::builder()
        .icon_name("list-add-symbolic")
        .css_classes(["suggested-action"])
        .tooltip_text(gettext("New Download (Ctrl+N)"))
        .build();
    add_btn.set_widget_name("add-download-button");
    add_btn.update_property(&[gtk4::accessible::Property::Label(&gettext("New Download"))]);
    {
        let card = add_card.clone();
        add_btn.connect_toggled(move |btn| {
            // Guard: state-changed callbacks land here via set_active, and
            // set_active to the current value emits nothing, so this only
            // fires on a genuine user flip — no feedback loop.
            if btn.is_active() != card.is_open() {
                card.toggle();
            }
        });
    }
    {
        let weak = add_btn.downgrade();
        add_card.set_on_state_changed(move |open| {
            if let Some(btn) = weak.upgrade() {
                btn.set_active(open);
            }
        });
    }
    // Keep the primary New Download action on the trailing edge.
    header.pack_end(&add_btn);
    header.pack_start(&search_toggle);

    let content_stack = gtk4::Stack::new();
    content_stack.set_widget_name("queue-content-stack");
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
        let card = add_card.clone();
        empty_add.connect_clicked(move |_| card.toggle());
    }
    content_stack.add_named(&empty, Some("empty"));

    // The queue filters are views in one AdwViewStack. This lets libadwaita
    // provide the same adaptive header/bottom navigation used by GNOME Software.
    let query: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
    let status_sel: Rc<Cell<u32>> = Rc::new(Cell::new(0));
    let narrow_layout = Rc::new(Cell::new(false));
    let has_items = Rc::new(Cell::new(false));
    let queue_view_visible = Rc::new(Cell::new(true));

    let view_stack = adw::ViewStack::new();
    view_stack.set_widget_name("download-status-stack");
    // The one shared queue scroller moves between page containers on selection.
    view_stack.set_enable_transitions(false);
    let view_switcher = adw::ViewSwitcher::new();
    view_switcher.set_widget_name("download-status-view-switcher");
    ensure_queue_view_switcher_css();
    view_switcher.set_stack(Some(&view_stack));
    view_switcher.set_policy(adw::ViewSwitcherPolicy::Wide);
    header.set_title_widget(Some(&view_switcher));

    let switcher_bar = adw::ViewSwitcherBar::new();
    switcher_bar.set_widget_name("download-status-view-switcher-bar");
    switcher_bar.set_stack(Some(&view_stack));

    fn section_list(title: &str) -> (gtk4::Box, gtk4::ListBox) {
        let label = gtk4::Label::builder()
            .label(title)
            .halign(gtk4::Align::Start)
            .css_classes(["heading"])
            .build();
        // HIG: .boxed-list-separate presents each custom row as its own
        // card — the style class provides the card padding natively, no
        // manual margins on the row content.
        let list = gtk4::ListBox::builder()
            .selection_mode(gtk4::SelectionMode::None)
            .css_classes(["boxed-list-separate"])
            .build();
        let section = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        section.append(&label);
        section.append(&list);
        (section, list)
    }
    let (active_section, active_list) = section_list(&gettext("Active"));
    let (queued_section, queued_list) = section_list(&gettext("Queued"));
    let (downloaded_section, downloaded_list) = section_list(&gettext("Downloaded"));
    active_section.set_widget_name("download-section-active");
    queued_section.set_widget_name("download-section-queued");
    downloaded_section.set_widget_name("download-section-downloaded");
    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    // HIG: keep the status filter and all download sections in one
    // centered column on wide windows. AdwClamp shrinks with the available
    // width, so tiled and narrow windows retain their existing layout.
    const DOWNLOAD_LIST_MAX_WIDTH: i32 = 800;
    let list_clamp = adw::Clamp::builder()
        .maximum_size(DOWNLOAD_LIST_MAX_WIDTH)
        .build();
    list_clamp.set_widget_name("download-list-clamp");
    list_clamp.set_child(Some(&content));
    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .child(&list_clamp)
        .build();
    scroll.set_widget_name("download-list-scroll");

    // Share one scroller and its row widgets across every view. The selected
    // page owns the scroller, so row signals and progress sources are not duplicated.
    let all_page = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    all_page.set_widget_name("status-page-all");
    let active_page = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    active_page.set_widget_name("status-page-active");
    let queued_page = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    queued_page.set_widget_name("status-page-queued");
    let downloaded_page = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    downloaded_page.set_widget_name("status-page-downloaded");

    let all_stack_page = view_stack.add_titled(&all_page, Some("all"), &gettext("All"));
    let active_stack_page = view_stack.add_titled(&active_page, Some("active"), &gettext("Active"));
    let queued_stack_page = view_stack.add_titled(&queued_page, Some("queued"), &gettext("Queued"));
    let downloaded_stack_page =
        view_stack.add_titled(&downloaded_page, Some("downloaded"), &gettext("Downloaded"));
    all_page.append(&scroll);
    view_stack.set_visible_child_name("all");

    // Four destinations need room for their wide labels. 550sp is the
    // libadwaita recommendation for a four-page view switcher.
    let breakpoint = adw::Breakpoint::new(
        adw::BreakpointCondition::parse("max-width: 550sp")
            .expect("the static view-switcher breakpoint is valid"),
    );
    {
        let narrow = Rc::clone(&narrow_layout);
        let header = header.clone();
        let switcher = view_switcher.clone();
        let switcher_bar = switcher_bar.clone();
        let has_items = Rc::clone(&has_items);
        let queue_visible = Rc::clone(&queue_view_visible);
        let all_page = all_stack_page.clone();
        let active_page = active_stack_page.clone();
        let queued_page = queued_stack_page.clone();
        let downloaded_page = downloaded_stack_page.clone();
        breakpoint.connect_apply(move |_| {
            narrow.set(true);
            all_page.set_icon_name(Some("view-list-symbolic"));
            active_page.set_icon_name(Some("content-loading-symbolic"));
            queued_page.set_icon_name(Some("task-due-symbolic"));
            downloaded_page.set_icon_name(Some("folder-download-symbolic"));
            // Clearing the title widget makes AdwHeaderBar show the window
            // title ("Grab"), while the same stack is exposed by the bottom bar.
            header.set_title_widget(None::<&gtk4::Widget>);
            let show_navigation = has_items.get() && queue_visible.get();
            switcher.set_visible(show_navigation);
            switcher_bar.set_reveal(show_navigation);
        });
    }
    {
        let narrow = Rc::clone(&narrow_layout);
        let header = header.clone();
        let switcher = view_switcher.clone();
        let switcher_bar = switcher_bar.clone();
        let has_items = Rc::clone(&has_items);
        let queue_visible = Rc::clone(&queue_view_visible);
        let all_page = all_stack_page.clone();
        let active_page = active_stack_page.clone();
        let queued_page = queued_stack_page.clone();
        let downloaded_page = downloaded_stack_page.clone();
        breakpoint.connect_unapply(move |_| {
            narrow.set(false);
            all_page.set_icon_name(None);
            active_page.set_icon_name(None);
            queued_page.set_icon_name(None);
            downloaded_page.set_icon_name(None);
            switcher_bar.set_reveal(false);
            let show_navigation = has_items.get() && queue_visible.get();
            switcher.set_visible(show_navigation);
            if show_navigation {
                header.set_title_widget(Some(&switcher));
            } else {
                header.set_title_widget(None::<&gtk4::Widget>);
            }
        });
    }
    window.add_breakpoint(breakpoint);
    content_stack.add_named(&view_stack, Some("list"));
    content_stack.set_visible_child_name("empty");
    view_stack.set_vexpand(true);

    fn is_done(it: &crate::download::DownloadItem) -> bool {
        it.status() == crate::download::DownloadStatus::Done
    }
    fn is_queued(it: &crate::download::DownloadItem) -> bool {
        it.status() == crate::download::DownloadStatus::Queued
    }
    /// View-stack bucket for a status (0 = All); order matches the pages.
    /// Active deliberately covers everything in flight or stalled —
    /// including the terminal Failed/Cancelled states, so one bucket shows
    /// anything still needing attention. Queued is the waiting, Downloaded
    /// the finished.
    fn status_filter_bucket(s: crate::download::DownloadStatus) -> u32 {
        use crate::download::DownloadStatus::*;
        match s {
            Downloading | Paused | Failed | Cancelled => 1,
            Queued | Scheduled => 2,
            Done => 3,
        }
    }

    // The toggle shows/hides the search bar below the header (HIG pattern);
    // Escape in the entry closes it too.
    {
        let bar = search_bar.clone();
        let entry = search.clone();
        search_toggle.connect_toggled(move |btn| {
            bar.set_search_mode(btn.is_active());
            if btn.is_active() {
                entry.grab_focus();
            }
        });
    }
    {
        let toggle = search_toggle.clone();
        let esc = gtk4::EventControllerKey::new();
        esc.connect_key_pressed(move |_, keyval, _, _| {
            if keyval == gtk4::gdk::Key::Escape {
                toggle.set_active(false);
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        search.add_controller(esc);
    }
    // Custom close button exits search mode via the toggle.
    {
        let toggle = search_toggle.clone();
        search_close.connect_clicked(move |_| {
            toggle.set_active(false);
        });
    }
    // Keep the toggle in sync when Escape exits search mode.
    {
        let toggle = search_toggle.clone();
        search_bar.connect_search_mode_enabled_notify(move |bar| {
            if !bar.is_search_mode() {
                toggle.set_active(false);
            }
        });
    }
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
        let s = content_stack.clone();
        let l_active = active_list.clone();
        let l_queued = queued_list.clone();
        let l_downloaded = downloaded_list.clone();
        let sec_active = active_section.clone();
        let sec_queued = queued_section.clone();
        let sec_downloaded = downloaded_section.clone();
        let query = Rc::clone(&query);
        let status_sel = Rc::clone(&status_sel);
        let header = header.clone();
        let view_switcher = view_switcher.clone();
        let switcher_bar = switcher_bar.clone();
        let narrow = Rc::clone(&narrow_layout);
        let has_items_state = Rc::clone(&has_items);
        let queue_visible = Rc::clone(&queue_view_visible);
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
                    let mut shown = sel == 0 || status_filter_bucket(it.status()) == sel;
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
            has_items_state.set(has_items);
            // The header holds the primary action once there are rows; the empty
            // state supplies its own call to action when the queue is empty.
            add.set_visible(has_items);
            search_btn.set_visible(has_items && queue_visible.get());
            let show_navigation = has_items && queue_visible.get();
            view_switcher.set_visible(show_navigation);
            if narrow.get() {
                header.set_title_widget(None::<&gtk4::Widget>);
                switcher_bar.set_reveal(show_navigation);
            } else {
                switcher_bar.set_reveal(false);
                if show_navigation {
                    header.set_title_widget(Some(&view_switcher));
                } else {
                    header.set_title_widget(None::<&gtk4::Widget>);
                }
            }
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
        let status_sel = Rc::clone(&status_sel);
        let scroll = scroll.clone();
        let all_page = all_page.clone();
        let active_page = active_page.clone();
        let queued_page = queued_page.clone();
        let downloaded_page = downloaded_page.clone();
        view_stack.connect_visible_child_name_notify(move |stack| {
            let Some(name) = stack.visible_child_name() else {
                return;
            };
            let (bucket, page) = match name.as_str() {
                "active" => (1, &active_page),
                "queued" => (2, &queued_page),
                "downloaded" => (3, &downloaded_page),
                _ => (0, &all_page),
            };
            status_sel.set(bucket);
            if let Some(parent) = scroll.parent().and_downcast::<gtk4::Box>() {
                parent.remove(&scroll);
            }
            page.append(&scroll);
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
            if should_hide_to_background(m.has_transferring(), m.is_shutting_down()) {
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
    toolbar.add_top_bar(&search_clamp);
    toolbar.add_top_bar(&banner);
    toolbar.add_bottom_bar(&switcher_bar);
    // The add card's gallery/playlist picker pages can be much taller than
    // a toolbar. Keeping it as a top bar can produce an invalid `for_size`
    // measurement when the window is probed at its minimum size. Keep the
    // toolbar bars compact and place the card in the content area instead.
    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    content.append(add_card.widget());
    content_stack.set_vexpand(true);
    // A picker page is a selection task that needs height: while one is
    // visible, the card takes the content area. The queue gets its space
    // back as soon as the form page returns (pop, Escape, or card close).
    {
        let content_stack = content_stack.clone();
        let sync = Rc::clone(&sync);
        let queue_visible = Rc::clone(&queue_view_visible);
        let view_switcher = view_switcher.clone();
        let switcher_bar = switcher_bar.clone();
        let header = header.clone();
        let narrow = Rc::clone(&narrow_layout);
        let search_toggle = search_toggle.clone();
        add_card
            .navigation()
            .connect_notify_local(Some("visible-page"), move |nav, _| {
                let picker = nav
                    .visible_page_tag()
                    .is_some_and(|tag| tag.as_str() != "form");
                queue_visible.set(!picker);
                if picker {
                    view_switcher.set_visible(false);
                    switcher_bar.set_reveal(false);
                    if !narrow.get() {
                        header.set_title_widget(None::<&gtk4::Widget>);
                    }
                    search_toggle.set_visible(false);
                    search_toggle.set_active(false);
                }
                content_stack.set_visible(!picker);
                if !picker {
                    // Let the navigation transition settle before reconciling.
                    // Queue changes can land while the stack is hidden, and the
                    // first visible allocation must see the latest model.
                    let content_stack = content_stack.clone();
                    let sync = Rc::clone(&sync);
                    glib::idle_add_local_once(move || {
                        if content_stack.is_visible() {
                            sync();
                            content_stack.queue_resize();
                        }
                    });
                }
            });
    }
    content.append(&content_stack);
    toolbar.set_content(Some(&content));
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
                && !m.is_shutting_down()
                && !m.has_transferring()
                && w.upgrade().is_some_and(|win| !win.is_visible());
            if idle_hidden && let Some(app) = app_weak.upgrade() {
                app.withdraw_notification(BACKGROUND_NOTIF_ID);
                // Reuse the action's async shutdown path even when the final
                // background transfer finishes while the window is hidden.
                app.activate_action("quit", None);
            }
        });
        manager.set_on_change({
            let hook = Rc::clone(&hook);
            move || hook()
        });
        hook();
    }

    (window, search_toggle, add_card)
}

#[cfg(test)]
#[path = "window_tests.rs"]
mod tests;
