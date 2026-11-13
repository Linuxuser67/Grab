use crate::download::DownloadManager;
use adw::prelude::*;
use gettextrs::gettext;
use glib::variant::ToVariant;
use gtk4::prelude::*;
use gtk4::{gio, glib};
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// Facade: row widgets live in [`window_rows`](crate::window_rows) now.
use crate::window_rows::build_row;
pub use crate::window_rows::launch_path;
use crate::window_rows::ngettext_count;

#[derive(Clone, Copy, PartialEq, Eq)]
struct RowPlacement {
    section: u32,
    group_id: Option<u64>,
}

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

fn apply_navigation_state(
    narrow_layout: bool,
    has_items: bool,
    queue_visible: bool,
    header: &adw::HeaderBar,
    view_switcher: &adw::ViewSwitcher,
    switcher_bar: &adw::ViewSwitcherBar,
    search_toggle: &gtk4::ToggleButton,
) {
    let show_navigation = has_items && queue_visible;
    view_switcher.set_visible(show_navigation);
    switcher_bar.set_reveal(narrow_layout && show_navigation);
    search_toggle.set_visible(show_navigation);
    if narrow_layout || !show_navigation {
        header.set_title_widget(None::<&gtk4::Widget>);
    } else {
        header.set_title_widget(Some(view_switcher));
    }
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
    // Use GTK's standard GNOME search bar: it reveals below the header
    // and captures unhandled window typing; the header toggle dismisses it.
    let search = gtk4::SearchEntry::builder()
        .placeholder_text(gettext("Search downloads"))
        .hexpand(true)
        .build();
    let search_bar = gtk4::SearchBar::builder().child(&search).build();
    search_bar.set_widget_name("download-search-bar");
    search_bar.connect_entry(&search);
    search_bar.set_key_capture_widget(Some(&window));
    search_bar.set_margin_top(12);
    search_bar.set_margin_bottom(12);
    search_bar.set_margin_start(12);
    search_bar.set_margin_end(12);

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
    section2.append(Some(&gettext("About Grab")), Some("app.about"));
    menu.append_section(None, &section2);
    let menu_btn = gtk4::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .primary(true)
        .menu_model(&menu)
        .tooltip_text(gettext("Main Menu"))
        .build();
    menu_btn.set_widget_name("main-menu-button");
    menu_btn.update_property(&[gtk4::accessible::Property::Label(&gettext("Main Menu"))]);
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
    // GtkHeaderBar prepends pack_end children: the menu is nearest the
    // window controls, with New Download immediately to its left.
    header.pack_end(&menu_btn);
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
    let (downloaded_section, downloaded_list) = section_list(&gettext("Done"));
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
    let downloaded_page = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    downloaded_page.set_widget_name("status-page-downloaded");

    view_stack.add_titled_with_icon(
        &all_page,
        Some("all"),
        &gettext("All"),
        "view-list-symbolic",
    );
    view_stack.add_titled_with_icon(
        &active_page,
        Some("active"),
        &gettext("Active"),
        "content-loading-symbolic",
    );
    view_stack.add_titled_with_icon(
        &downloaded_page,
        Some("downloaded"),
        &gettext("Done"),
        "folder-download-symbolic",
    );
    all_page.append(&scroll);
    view_stack.set_visible_child_name("all");

    // Three destinations need room for their wide labels; 550sp also leaves
    // headroom for longer translated labels.
    let breakpoint = adw::Breakpoint::new(
        adw::BreakpointCondition::parse("max-width: 550sp")
            .expect("the static view-switcher breakpoint is valid"),
    );
    {
        let narrow = Rc::clone(&narrow_layout);
        let header = header.clone();
        let switcher = view_switcher.clone();
        let switcher_bar = switcher_bar.clone();
        let search_toggle = search_toggle.clone();
        let has_items = Rc::clone(&has_items);
        let queue_visible = Rc::clone(&queue_view_visible);
        breakpoint.connect_apply(move |_| {
            narrow.set(true);
            // The header title is cleared on narrow layouts; navigation moves to the bottom bar.
            apply_navigation_state(
                true,
                has_items.get(),
                queue_visible.get(),
                &header,
                &switcher,
                &switcher_bar,
                &search_toggle,
            );
        });
    }
    {
        let narrow = Rc::clone(&narrow_layout);
        let header = header.clone();
        let switcher = view_switcher.clone();
        let switcher_bar = switcher_bar.clone();
        let search_toggle = search_toggle.clone();
        let has_items = Rc::clone(&has_items);
        let queue_visible = Rc::clone(&queue_view_visible);
        breakpoint.connect_unapply(move |_| {
            narrow.set(false);
            apply_navigation_state(
                false,
                has_items.get(),
                queue_visible.get(),
                &header,
                &switcher,
                &switcher_bar,
                &search_toggle,
            );
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
    /// Active deliberately covers everything not finished — in flight, waiting,
    /// stalled and the terminal Failed/Cancelled states — so one bucket shows
    /// anything still needing attention. Downloaded is the finished bucket.
    /// The All view still groups waiting items under their own Queued section.
    fn status_filter_bucket(s: crate::download::DownloadStatus) -> u32 {
        use crate::download::DownloadStatus::*;
        match s {
            Downloading | Paused | Queued | Scheduled | Failed | Cancelled => 1,
            Done => 2,
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
    // Keep the header toggle synchronized whether search is opened by the
    // button, type-to-search, Ctrl+F, or GTK's built-in close/Escape behavior.
    {
        let toggle = search_toggle.clone();
        search_bar.connect_search_mode_enabled_notify(move |bar| {
            toggle.set_active(bar.is_search_mode());
        });
    }
    content.append(&active_section);
    content.append(&queued_section);
    content.append(&downloaded_section);

    fn detach_download_row(row: &gtk4::ListBoxRow) {
        if let Some(parent) = row.parent().and_downcast::<gtk4::ListBox>() {
            parent.remove(row);
        }
    }

    fn section_bucket(status: crate::download::DownloadStatus) -> u32 {
        if status == crate::download::DownloadStatus::Done {
            2
        } else if status == crate::download::DownloadStatus::Queued {
            3
        } else {
            1
        }
    }

    let rows: Rc<RefCell<HashMap<u64, gtk4::ListBoxRow>>> = Rc::new(RefCell::new(HashMap::new()));
    let group_rows: Rc<RefCell<HashMap<(u64, u32), crate::window_rows::DownloadGroupRow>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let row_placements: Rc<RefCell<HashMap<u64, RowPlacement>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let sync: Rc<dyn Fn()> = {
        let m = Rc::clone(&manager);
        let t = Rc::clone(&toasts);
        let r = Rc::clone(&rows);
        let groups = Rc::clone(&group_rows);
        let placements = Rc::clone(&row_placements);
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
            let items = (0..store.n_items())
                .filter_map(|index| {
                    store
                        .item(index)
                        .and_downcast::<crate::download::DownloadItem>()
                })
                .collect::<Vec<_>>();

            // Collection owners are the only persisted/scheduled rows.
            // Their file rows are passive projections grouped under the
            // owner's status section, regardless of each child's own state.
            let collection_owner_ids = items
                .iter()
                .filter(|item| item.is_collection_child())
                .filter_map(|item| item.group().map(|group| group.id()))
                .collect::<HashSet<_>>();
            let collection_owners = items
                .iter()
                .filter(|item| item.is_collection_parent())
                .map(|item| (item.id(), item.clone()))
                .collect::<HashMap<_, _>>();
            let effective_status = |item: &crate::download::DownloadItem| {
                if item.is_collection_child() {
                    item.group()
                        .and_then(|group| collection_owners.get(&group.id()))
                        .map_or(item.status(), |owner| owner.status())
                } else {
                    item.status()
                }
            };
            let effective_bucket =
                |item: &crate::download::DownloadItem| section_bucket(effective_status(item));

            let mut group_sizes = HashMap::<u64, usize>::new();
            for item in &items {
                if item.is_collection_parent() && collection_owner_ids.contains(&item.id()) {
                    continue;
                }
                if let Some(group) = item.group() {
                    *group_sizes.entry(group.id()).or_default() += 1;
                }
            }
            let is_group_member = |item: &crate::download::DownloadItem, group_id: u64| {
                item.is_collection_child()
                    || group_sizes.get(&group_id).is_some_and(|count| *count > 1)
            };
            let mut all_groups = HashMap::<u64, Vec<crate::download::DownloadItem>>::new();
            for item in &items {
                if item.is_collection_parent() && collection_owner_ids.contains(&item.id()) {
                    continue;
                }
                if let Some(group) = item
                    .group()
                    .filter(|group| is_group_member(item, group.id()))
                {
                    all_groups.entry(group.id()).or_default().push(item.clone());
                }
            }

            let mut present = HashSet::with_capacity(items.len());
            let mut grouped = HashMap::<(u64, u32), Vec<crate::download::DownloadItem>>::new();
            let mut ordered_groups = Vec::new();
            let mut seen_groups = HashSet::new();
            let mut visible_groups = HashSet::new();
            let mut counted_collections = HashSet::new();
            let mut n_active = 0;
            let mut n_queued = 0;
            let mut n_downloaded = 0;

            for item in &items {
                // Represent an owner with one expandable collection header,
                // never a second ordinary download row.
                if item.is_collection_parent() && collection_owner_ids.contains(&item.id()) {
                    continue;
                }
                present.insert(item.id());
                let bucket = effective_bucket(item);
                let status = effective_status(item);
                let group_key = item
                    .group()
                    .filter(|group| is_group_member(item, group.id()))
                    .map(|group| (group.id(), bucket));

                let mut shown = sel == 0 || status_filter_bucket(status) == sel;
                if shown && !q.is_empty() && !item.filename().to_lowercase().contains(q.as_str()) {
                    shown = false;
                }
                if shown {
                    if item.is_collection_child() {
                        if let Some(group) = item.group()
                            && counted_collections.insert(group.id())
                        {
                            if status == crate::download_store::DownloadStatus::Done {
                                n_downloaded += 1;
                            } else if matches!(
                                status,
                                crate::download_store::DownloadStatus::Queued
                                    | crate::download_store::DownloadStatus::Scheduled
                            ) {
                                n_queued += 1;
                            } else {
                                n_active += 1;
                            }
                        }
                    } else if is_done(item) {
                        n_downloaded += 1;
                    } else if is_queued(item) {
                        n_queued += 1;
                    } else {
                        n_active += 1;
                    }
                }

                if let Some(key) = group_key {
                    grouped.entry(key).or_default().push(item.clone());
                    if seen_groups.insert(key) {
                        ordered_groups.push(key);
                    }
                    if shown {
                        visible_groups.insert(key);
                    }
                }
            }

            // Build each collection header in exactly one status section.
            // Child statuses stay visible inside it; the owner determines
            // where the collection appears globally.
            for key in &ordered_groups {
                let Some(members) = grouped.get(key) else {
                    continue;
                };
                let title = members
                    .first()
                    .and_then(crate::download::DownloadItem::group)
                    .map(|group| group.title().to_string())
                    .unwrap_or_default();
                let group = {
                    let mut groups = groups.borrow_mut();
                    groups
                        .entry(*key)
                        .or_insert_with(|| {
                            crate::window_rows::build_download_group_row(&title, &m, &t)
                        })
                        .clone()
                };
                let all_members = all_groups.get(&key.0).map(Vec::as_slice).unwrap_or(&[]);
                group.update(members, all_members, &m);
                group.row.set_visible(visible_groups.contains(key));
            }

            for (key, group) in groups.borrow().iter() {
                if !grouped.contains_key(key) {
                    group.update(&[], &[], &m);
                    group.row.set_visible(false);
                }
            }

            for item in &items {
                if item.is_collection_parent() && collection_owner_ids.contains(&item.id()) {
                    continue;
                }
                let bucket = effective_bucket(item);
                let status = effective_status(item);
                let target = match bucket {
                    2 => &l_downloaded,
                    3 => &l_queued,
                    _ => &l_active,
                };
                let group_key = item
                    .group()
                    .filter(|group| is_group_member(item, group.id()))
                    .map(|group| (group.id(), bucket));
                let location = RowPlacement {
                    section: bucket,
                    group_id: group_key.map(|(group_id, _)| group_id),
                };

                let row = if let Some(row) = r.borrow().get(&item.id()).cloned() {
                    row
                } else {
                    let row = build_row(item, &m, &t);
                    r.borrow_mut().insert(item.id(), row.clone());
                    row
                };

                if placements.borrow().get(&item.id()) != Some(&location) {
                    detach_download_row(&row);
                    if let Some(key) = group_key {
                        if let Some(group) = groups.borrow().get(&key).cloned() {
                            if group.row.parent().is_none() {
                                target.append(&group.row);
                            }
                            group.row.add_row(&row);
                        } else {
                            target.append(&row);
                        }
                    } else {
                        target.append(&row);
                    }
                    placements.borrow_mut().insert(item.id(), location);
                }

                let mut shown = sel == 0 || status_filter_bucket(status) == sel;
                if shown && !q.is_empty() && !item.filename().to_lowercase().contains(q.as_str()) {
                    shown = false;
                }
                row.set_visible(shown);
            }

            let stale: Vec<u64> = r
                .borrow()
                .keys()
                .filter(|id| !present.contains(id))
                .copied()
                .collect();
            for id in stale {
                if let Some(row) = r.borrow_mut().remove(&id) {
                    detach_download_row(&row);
                }
                placements.borrow_mut().remove(&id);
            }

            let live_group_ids = grouped
                .keys()
                .map(|(group_id, _)| *group_id)
                .collect::<HashSet<_>>();
            let stale_groups = groups
                .borrow()
                .keys()
                .filter(|(group_id, _)| !live_group_ids.contains(group_id))
                .copied()
                .collect::<Vec<_>>();
            for key in stale_groups {
                if let Some(group) = groups.borrow_mut().remove(&key)
                    && let Some(parent) = group.row.parent().and_downcast::<gtk4::ListBox>()
                {
                    parent.remove(&group.row);
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
            apply_navigation_state(
                narrow.get(),
                has_items,
                queue_visible.get(),
                &header,
                &view_switcher,
                &switcher_bar,
                &search_btn,
            );
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
        let downloaded_page = downloaded_page.clone();
        view_stack.connect_visible_child_name_notify(move |stack| {
            let Some(name) = stack.visible_child_name() else {
                return;
            };
            let (bucket, page) = match name.as_str() {
                "active" => (1, &active_page),
                "downloaded" => (2, &downloaded_page),
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
                t.add_toast(crate::ui_util::plain_toast(&ngettext_count(
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
        let has_items = Rc::clone(&has_items);
        let search_toggle = search_toggle.clone();
        add_card
            .navigation()
            .connect_notify_local(Some("visible-page"), move |nav, _| {
                let picker = nav
                    .visible_page_tag()
                    .is_some_and(|tag| tag.as_str() != "form");
                queue_visible.set(!picker);
                apply_navigation_state(
                    narrow.get(),
                    has_items.get(),
                    queue_visible.get(),
                    &header,
                    &view_switcher,
                    &switcher_bar,
                    &search_toggle,
                );
                if picker {
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
