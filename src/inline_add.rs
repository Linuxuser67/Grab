//! Inline New Download card: the whole add flow (link probing, the media
//! format selector, the playlist and torrent pickers) as a card pinned under
//! the header instead of a standalone dialog.
//!
//! The header `+` button (and the empty-state button, the `add-download`
//! action, and application-open URLs) reveal one card at the top of the main
//! window; the queue stays usable underneath. The card holds an
//! [`adw::NavigationView`]: the form page plus right-sliding playlist and
//! multi-file torrent picker pages. Cancel, Escape, or a successful enqueue
//! collapses the card and resets the form, so reopening always starts fresh.
//!
//! Intake behavior is unchanged from the old dialog: probing with generation
//! freshness and twin suppression, the unlisted-URL and direct-file
//! fallbacks, the Drive fallback, clipboard prefill, probe cancellation on
//! close, preferred-quality preselection, and the select-all/none pickers.
//! The probe never fires on its own: pasting or typing only syncs the form,
//! and the lookup starts when Add Download (or Enter) is pressed.

use crate::download::DownloadManager;
use crate::window_rows::{default_name_for, selection_action_bar};
use adw::prelude::*;
use gettextrs::{gettext, ngettext};
use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Multi-file torrent picker opener: file name, raw bytes, parsed entries.
type TorrentPickerOpener = Rc<dyn Fn(String, Vec<u8>, Vec<crate::torrent::TorrentFileEntry>)>;

/// Owns the in-flight probe marker: every exit clears it for the owning
/// generation, so a stale kick's marker never suppresses a re-kick.
struct InflightGuard {
    probe: Rc<RefCell<ProbeState>>,
    my: u64,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.probe.borrow_mut().finish(self.my);
    }
}

/// Pure resolve state for the video probe pipeline: generation counter,
/// in-flight marker, last resolved URL and probe result. GTK-free, so it
/// unit-tests without a display; shared behind one `Rc<RefCell<_>>`.
#[derive(Default)]
struct ProbeState {
    generation: u64,
    inflight: Option<(String, u64, bool)>,
    last_ok: String,
    info: Option<crate::video::ProbeResult>,
}

impl ProbeState {
    /// Start a resolve for `url`: returns the new generation, or `None`
    /// when a twin resolve for this exact URL is already running for the
    /// current generation (suppressed — the twin's result would lose the
    /// generation race anyway).
    fn kick(&mut self, url: String, probe_unlisted: bool) -> Option<u64> {
        if inflight_suppresses(&self.inflight, &url, self.generation, probe_unlisted) {
            return None;
        }
        let my = self.generation + 1;
        self.generation = my;
        self.inflight = Some((url, my, probe_unlisted));
        Some(my)
    }

    /// Twin check without starting: is a resolve for this exact URL already
    /// running for the current generation?
    fn is_inflight(&self, url: &str, probe_unlisted: bool) -> bool {
        inflight_suppresses(&self.inflight, url, self.generation, probe_unlisted)
    }

    /// Clear the in-flight marker when it belongs to `my` generation; a
    /// stale generation leaves a newer kick's marker alone.
    fn finish(&mut self, my: u64) {
        if self
            .inflight
            .as_ref()
            .is_some_and(|(_, g, _)| *g == self.generation && *g == my)
        {
            self.inflight = None;
        }
    }

    /// Bump the generation so in-flight resolves go stale.
    fn bump_generation(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    /// Collapse/close: cancel in-flight work and drop all probe state, so a
    /// non-video add after a video leaves no dead probe state behind.
    fn reset(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.inflight = None;
        self.last_ok.clear();
        self.info = None;
    }
}

/// Twin suppression: a resolve for this exact URL is already running for the
/// current generation (Add pressed twice while the lookup is in flight is
/// the usual trigger). The marker carries the kick's unlisted-probe flag: an
/// explicit Add/Enter kick probes unlisted URLs, a different resolve from
/// kick, so it is never suppressed by one.
fn inflight_suppresses(
    marker: &Option<(String, u64, bool)>,
    url: &str,
    generation: u64,
    probe_unlisted: bool,
) -> bool {
    marker
        .as_ref()
        .is_some_and(|(u, g, p)| u == url && *g == generation && *p == probe_unlisted)
}

/// Item-count label for a probed collection, kind-aware ("3 stories"). The
/// playlist picker reuses the count label; the error stays a plain caption.
pub(crate) fn playlist_count_label(kind: crate::media_types::PlaylistKind, count: usize) -> String {
    let template = match kind {
        crate::media_types::PlaylistKind::Stories => {
            ngettext("{} story", "{} stories", count as u32)
        }
        crate::media_types::PlaylistKind::Highlights => {
            ngettext("{} highlight", "{} highlights", count as u32)
        }
        crate::media_types::PlaylistKind::Playlist => ngettext("{} item", "{} items", count as u32),
    };
    template.replace("{}", &count.to_string())
}

/// Seconds as M:SS / H:MM:SS for picker subtitles.
pub(crate) fn fmt_item_duration(secs: i64) -> String {
    let secs = secs.max(0) as u64;
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// One media-format option: an exact pinnable format from the probe, or the
/// Automatic row (the global preference, no pin) when nothing is pinnable.
#[derive(Clone)]
struct FormatOption {
    /// Row title: the pin label ("1080p") or "Automatic".
    label: String,
    /// Resolved yt-dlp format id; `None` for the Automatic row.
    format_id: Option<String>,
}

/// The video preview block inside the form: exactly one state row shows at a
/// time, driven by the probe below.
struct VideoStep {
    /// Lookup spinner, in the URL entry's suffix slot (browser-address-bar
    /// style): no separate status line, no layout shift when a lookup starts.
    url_spinner: adw::Spinner,
    group: adw::PreferencesGroup,
    name: adw::EntryRow,
    revert: gtk4::Button,
    /// Media-format selector, filled per video on resolve: exact pinnable
    /// formats, tallest first (the preference preselects the closest row), or a
    /// single Automatic row when nothing is pinnable.
    format: adw::ComboRow,
    /// Index-aligned with the combo's model. Reset on every resolve.
    options: Rc<RefCell<Vec<FormatOption>>>,
    audio: adw::SwitchRow,
    tools: adw::ActionRow,
    error: adw::ActionRow,
}

/// Reserve trailing text space inside the URL entry while the lookup
fn hide_video_step(v: &VideoStep) {
    v.group.set_visible(false);
    // adw::Spinner animates while mapped; hiding stops it (no set_spinning).
    v.url_spinner.set_visible(false);
    v.name.set_visible(false);
    v.revert.set_visible(false);
    v.format.set_visible(false);
    v.audio.set_visible(false);
    v.tools.set_visible(false);
    v.error.set_visible(false);
}

/// Clear the video preview block back to a pristine state: `close_card`
/// calls this so a non-video add after a video leaves no dead probe state
/// in memory. Hiding alone is not enough — the name row keeps its text,
/// the format rows keep their widgets, and the tools/error rows keep
/// their subtitles.
fn reset_video_step(step: &VideoStep) {
    // Drop the format options and the current pick.
    step.options.borrow_mut().clear();
    step.format.set_model(Some(&gtk4::StringList::new(&[])));
    step.audio.set_active(false);
    // Name row and the tools/error subtitles keep their last text when
    // only hidden; clear them so nothing stale survives.
    step.name.set_text("");
    step.tools.set_subtitle("");
    step.error.set_subtitle("");
    hide_video_step(step);
}

fn show_video_loading(v: &VideoStep) {
    hide_video_step(v);
    v.url_spinner.set_visible(true);
}

fn show_video_ready(v: &VideoStep) {
    hide_video_step(v);
    v.group.set_visible(true);
    v.name.set_visible(true);
    v.revert.set_visible(true);
    v.format.set_visible(true);
    v.audio.set_visible(true);
}

fn show_video_tools_missing(v: &VideoStep, message: &str) {
    hide_video_step(v);
    v.group.set_visible(true);
    v.tools.set_subtitle(message);
    v.tools.set_visible(true);
}

fn show_video_error(v: &VideoStep, message: &str) {
    hide_video_step(v);
    v.group.set_visible(true);
    v.error.set_subtitle(message);
    v.error.set_visible(true);
}

/// Desensitize the form's Add button while a lookup is in flight (a dead
/// button says so upfront). Every terminal state re-enables it.
fn set_lookup_add(cell: &Rc<RefCell<Option<adw::EntryRow>>>, enabled: bool) {
    if let Some(b) = cell.borrow().as_ref() {
        b.set_sensitive(enabled);
    }
}

/// Rebuild the format options from a fresh resolve (tallest first, the
/// preference preselects the closest row) or a single Automatic row when
/// nothing is pinnable. Selection resets — a pin must never carry over.
fn rebuild_format_options(step: &Rc<VideoStep>, info: &crate::video::VideoInfo, preferred: &str) {
    let mut options = Vec::new();
    for opt in &info.formats {
        options.push(FormatOption {
            label: opt.label.to_string(),
            format_id: Some(opt.id.to_string()),
        });
    }
    if options.is_empty() {
        options.push(FormatOption {
            label: gettext("Automatic"),
            format_id: None,
        });
    }
    let labels: Vec<&str> = options.iter().map(|o| o.label.as_str()).collect();
    step.format.set_model(Some(&gtk4::StringList::new(&labels)));
    // default_quality_index is over info.formats; options may be just
    // [Automatic] when nothing is pinnable, so clamp.
    let index = crate::video::default_quality_index(&info.formats, preferred)
        .min(options.len().saturating_sub(1));
    step.format.set_selected(index as u32);
    *step.options.borrow_mut() = options;
}

#[allow(clippy::too_many_arguments)]
fn submit_probed_single(
    manager: &Rc<DownloadManager>,
    dest: &Rc<RefCell<String>>,
    close_card: &Rc<dyn Fn()>,
    step: &Rc<VideoStep>,
    lookup_add: &Rc<RefCell<Option<adw::EntryRow>>>,
    v: &crate::video::VideoInfo,
) {
    let typed = step.name.text().trim().to_string();
    let audio_only = step.audio.is_active();
    // Default name from the video title and id; the intake sanitizes it.
    let settings = manager.settings();
    let auto = typed
        .is_empty()
        .then(|| default_name_for(settings, &v.title, audio_only));
    let name = if typed.is_empty() {
        auto.as_deref()
    } else {
        Some(typed.as_str())
    };
    // Exact picks pin the format with its height as fallback, so a dropped pin still
    // degrades to the chosen height; audio-only rows drop the pin, and Automatic (no
    // pin) falls back to the global preference. Options and rows share one order.
    let selected = step.format.selected() as usize;
    let format_id = step
        .options
        .borrow()
        .get(selected)
        .and_then(|o| o.format_id.clone());
    let format_id = if audio_only { None } else { format_id };
    let quality = match format_id.clone() {
        Some(id) => v
            .formats
            .iter()
            .find(|opt| id.as_str() == &*opt.id)
            .map(|opt| crate::video::quality_for_height(opt.height).to_string())
            .unwrap_or_else(|| manager.settings().video_quality()),
        None => manager.settings().video_quality(),
    };
    enqueue_and_close(
        dest,
        close_card,
        |d| {
            manager.enqueue_video(
                &v.page_url,
                d,
                name,
                crate::media_types::VideoChoices {
                    quality,
                    audio_only,
                    video_format_id: format_id,
                    is_live: v.is_live,
                    playlist_item_id: None,
                },
            )
        },
        |e| {
            show_video_error(step, e);
            set_lookup_add(lookup_add, true);
        },
    );
}

/// Dispatch a fresh preview to its submit path: singles queue with their pinned
/// format, collections open the item picker. The caller's freshness gate stays.
#[allow(clippy::too_many_arguments)]
fn submit_probe(
    manager: &Rc<DownloadManager>,
    dest: &Rc<RefCell<String>>,
    close_card: &Rc<dyn Fn()>,
    step: &Rc<VideoStep>,
    lookup_add: &Rc<RefCell<Option<adw::EntryRow>>>,
    nav: &adw::NavigationView,
    probe: crate::video::ProbeResult,
) {
    match probe {
        crate::video::ProbeResult::Single(v) => {
            submit_probed_single(manager, dest, close_card, step, lookup_add, &v);
        }
        crate::video::ProbeResult::Playlist(pl) => {
            // Collections queue through the item picker: one row per chosen
            // entry, each re-resolving its own page at download time. Pins
            // don't apply across items, so the form's Audio only switch is
            // the quality control here.
            push_playlist_items_page(
                nav,
                manager.clone(),
                dest.clone(),
                close_card.clone(),
                pl,
                step.audio.is_active(),
            );
        }
    }
}

/// Report a failed plain-queue fallback on the form: drop the stale probe, log, show the error, re-enable Add.
fn fallback_plain_failed(
    probe: &Rc<RefCell<ProbeState>>,
    step: &Rc<VideoStep>,
    lookup_add: &Rc<RefCell<Option<adw::EntryRow>>>,
    url: &str,
    error: &str,
) {
    probe.borrow_mut().info.take();
    tracing::warn!(
        host = %crate::video_probe::page_host(url),
        error = %error,
        "plain fallback failed"
    );
    show_video_error(step, error);
    set_lookup_add(lookup_add, true);
}

/// Enqueue with the card's destination, then collapse the card on success.
///
/// The destination borrow lives only for the enqueue call: a `dest.borrow()`
/// temporary in a `match` scrutinee would live into the arms, and
/// `close_card()` re-borrows the same cell mutably to reset the destination —
/// panicking with "RefCell already borrowed" on every successful Add.
fn enqueue_and_close<T>(
    dest: &Rc<RefCell<String>>,
    close_card: &Rc<dyn Fn()>,
    enqueue: impl FnOnce(Option<&str>) -> Result<T, String>,
    on_err: impl FnOnce(&str),
) {
    let result = {
        let d = dest.borrow();
        enqueue(Some(&d))
    };
    match result {
        Ok(_) => close_card(),
        Err(e) => on_err(&e),
    }
}

/// Queue a probed link as a plain file and collapse the card: the fallback when
/// extraction finds no playable media on an unlisted page. `Err` when plain intake rejects the URL.
fn queue_plain(
    manager: &Rc<DownloadManager>,
    dest: &Rc<RefCell<String>>,
    close_card: &Rc<dyn Fn()>,
    file_row: &adw::EntryRow,
    url: &str,
) -> Result<(), String> {
    let typed = file_row.text().trim().to_string();
    let name = (!typed.is_empty()).then_some(typed);
    manager.enqueue(url, Some(&dest.borrow()), name.as_deref())?;
    close_card();
    Ok(())
}

/// Enter confirms a picker page's action: the card-scoped counterpart of the
/// old dialog's default widget. Rows and buttons keep Enter for their own
/// activation — they stop the event before it bubbles up here — so this only
/// fires when focus is on the page background.
fn picker_enter_confirms(page: &adw::NavigationPage, add_btn: &gtk4::Button) {
    let key = gtk4::EventControllerKey::new();
    let add_btn = add_btn.clone();
    key.connect_key_pressed(move |_, keyval, _, _| {
        if keyval == gtk4::gdk::Key::Return || keyval == gtk4::gdk::Key::KP_Enter {
            add_btn.emit_clicked();
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    page.add_controller(key);
}

/// Compact picker header: back + title + dimmed count on one tight row.
/// The auto `AdwHeaderBar` left the centered title floating in 48px of
/// chrome with nothing else in it; a picker embedded in a card earns a
/// denser row. Back pops the navigation page (Esc still collapses the
/// whole card); the selection actions stay in the bottom action bar.
fn picker_header(nav: &adw::NavigationView, title: &str, count: &str) -> gtk4::Box {
    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    header.set_margin_top(6);
    header.set_margin_bottom(6);
    header.set_margin_start(6);
    header.set_margin_end(12);
    let back = gtk4::Button::builder()
        .icon_name("go-previous-symbolic")
        .css_classes(["flat", "circular"])
        .tooltip_text(gettext("Back"))
        .valign(gtk4::Align::Center)
        .build();
    back.update_property(&[gtk4::accessible::Property::Label(&gettext("Back"))]);
    {
        let nav = nav.clone();
        back.connect_clicked(move |_| {
            nav.pop();
        });
    }
    let title_label = gtk4::Label::builder()
        .label(title)
        .css_classes(["heading"])
        .halign(gtk4::Align::Start)
        .valign(gtk4::Align::Center)
        .hexpand(true)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    let count_label = gtk4::Label::builder()
        .label(count)
        .css_classes(["dim-label", "caption"])
        .valign(gtk4::Align::Center)
        .build();
    header.append(&back);
    header.append(&title_label);
    header.append(&count_label);
    header
}

/// Column count for a picker grid, from the entry count: max 4 columns,
/// no empty cells. Picks the divisor (≤4) that makes the grid most
/// square — 3→3×1, 4→2×2, 8→4×2, 9→3×3, 12→4×3. Ties prefer more
/// columns (fewer rows). Fixed per picker (set via min/max-columns) —
/// selecting never reflows.
/// Prime counts >4 use a single column — the only way to avoid gaps.
fn picker_columns(count: usize) -> u32 {
    if count == 0 {
        return 1;
    }
    let mut best = 1;
    let mut best_score = usize::MAX;
    for c in 1..=4 {
        if count.is_multiple_of(c) {
            let rows = count / c;
            // Most square wins; ties prefer more columns (fewer rows).
            let score = rows.abs_diff(c) * 100 - c;
            if score < best_score {
                best_score = score;
                best = c;
            }
        }
    }
    best as u32
}

/// A picker grid (HIG `GtkGridView` with `GtkMultiSelection` and
/// `AdwActionRow` cells): click toggles selection, no checkboxes. Columns
/// adapt to the item count so there are no empty trailing cells. All
/// entries start selected, matching the old checked-by-default rows.
/// Returns the view and its selection model for the caller to wire.
fn picker_grid(entries: Rc<Vec<(String, String)>>) -> (gtk4::GridView, gtk4::MultiSelection) {
    // HIG selection outline: selected cells get an accent-colored outline.
    // Uses the theme's @accent_color — no hardcoded colors. Installed once.
    static INSTALL_CSS: std::sync::Once = std::sync::Once::new();
    INSTALL_CSS.call_once(|| {
        let css = gtk4::CssProvider::new();
        css.load_from_string(
            "gridview child:selected .picker-cell { \
               outline: 2px solid @accent_color; \
               outline-offset: -2px; \
               border-radius: 12px; \
             } \
             .new-download-card { \
               padding: 12px 12px 0 12px; \
             }",
        );
        gtk4::style_context_add_provider_for_display(
            &gdk::Display::default().expect("no display"),
            &css,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    });

    let store = gio::ListStore::new::<gtk4::StringObject>();
    for (title, _) in entries.iter() {
        store.append(&gtk4::StringObject::new(title));
    }
    let selection = gtk4::MultiSelection::new(Some(store));
    selection.select_all();

    let factory = gtk4::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        // HIG: AdwActionRow for title/subtitle — no hand-rolled Box+Labels.
        // The picker-cell class gets an accent outline when selected (CSS).
        let row = adw::ActionRow::builder().activatable(true).build();
        row.add_css_class("picker-cell");
        item.set_child(Some(&row));
    });
    {
        let entries = Rc::clone(&entries);
        factory.connect_bind(move |_, item| {
            let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
            let (title, subtitle) = &entries[item.position() as usize];
            let row = item.child().and_downcast::<adw::ActionRow>().unwrap();
            row.set_title(title);
            row.set_subtitle(subtitle);
        });
    }

    let columns = picker_columns(entries.len());
    let grid = gtk4::GridView::builder()
        .model(&selection)
        .factory(&factory)
        .min_columns(columns)
        .max_columns(columns)
        .build();
    (grid, selection)
}

/// Wire the pickers' bottom action bar to a grid's multi-selection: the
/// action counts the live selection, Select All/None drive the model.
fn wire_grid_selection_bar(
    selection: &gtk4::MultiSelection,
    select_all_btn: &gtk4::Button,
    select_none_btn: &gtk4::Button,
    add_btn: &gtk4::Button,
    count_label: impl Fn(usize) -> String + 'static,
) {
    {
        let selection = selection.clone();
        let add_btn = add_btn.clone();
        let refresh = Rc::new({
            let selection = selection.clone();
            move || {
                add_btn.set_label(&count_label(selection.selection().size() as usize));
            }
        });
        selection.connect_selection_changed({
            let refresh = refresh.clone();
            move |_, _, _| refresh()
        });
        refresh();
    }
    {
        let selection = selection.clone();
        select_all_btn.connect_clicked(move |_| {
            selection.select_all();
        });
    }
    {
        let selection = selection.clone();
        select_none_btn.connect_clicked(move |_| {
            selection.unselect_all();
        });
    }
}

/// Selected positions of a picker grid's multi-selection, ascending.
fn grid_selected(selection: &gtk4::MultiSelection, item_count: usize) -> Vec<usize> {
    (0..item_count)
        .filter(|&i| selection.is_selected(i as u32))
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn push_playlist_items_page(
    nav: &adw::NavigationView,
    manager: Rc<DownloadManager>,
    dest_dir: Rc<RefCell<String>>,
    close_card: Rc<dyn Fn()>,
    playlist: crate::media_types::PlaylistInfo,
    audio_only: bool,
) {
    // Same guard as the video step: don't stack a second picker while one is
    // already visible.
    if nav.visible_page_tag().as_deref() == Some("playlist") {
        return;
    }

    let count = playlist.items.len();
    // Multi-column grid: title over a dimmed duration. The truncation
    // notice sits under the header as a dimmed caption.
    let entries: Rc<Vec<(String, String)>> = Rc::new(
        playlist
            .items
            .iter()
            .map(|item| {
                (
                    item.title.to_string(),
                    item.duration.map(fmt_item_duration).unwrap_or_default(),
                )
            })
            .collect(),
    );
    let (grid, selection) = picker_grid(Rc::clone(&entries));

    let list_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(6)
        .build();
    if crate::video_probe::playlist_truncated(&playlist) {
        let notice = gtk4::Label::builder()
            .label(
                gettext("Showing the first {n} of {total}")
                    .replace("{n}", &count.to_string())
                    .replace("{total}", &playlist.total.to_string()),
            )
            .css_classes(["dim-label", "caption"])
            .halign(gtk4::Align::Start)
            .build();
        list_box.append(&notice);
    }
    list_box.append(&grid);
    // The error caption lives under the grid, like the old row list.
    let error_caption = gtk4::Label::builder()
        .label("")
        .css_classes(["error", "caption"])
        .halign(gtk4::Align::Start)
        .visible(false)
        .build();
    list_box.append(&error_caption);

    // Scrolled: big playlists must not size the card off-screen, but the capped
    // natural height lets it grow and shrink with the item count instead of
    // keeping the last size.
    let scrolled = gtk4::ScrolledWindow::builder()
        .child(&list_box)
        .vexpand(true)
        .propagate_natural_height(true)
        .max_content_height(480)
        .build();

    let toolbar = adw::ToolbarView::new();
    // Compact header instead of the auto AdwHeaderBar: back + title + count
    // on one tight row, no 48px of empty chrome around a centered title.
    toolbar.add_top_bar(&picker_header(
        nav,
        &playlist.title,
        &playlist_count_label(playlist.kind, count),
    ));
    toolbar.set_content(Some(&scrolled));
    let (action_bar, select_all_btn, select_none_btn, add_btn) = selection_action_bar();
    toolbar.add_bottom_bar(&action_bar);
    let picker_page = adw::NavigationPage::builder()
        .tag("playlist")
        .title(&*playlist.title)
        .child(&toolbar)
        .build();
    picker_enter_confirms(&picker_page, &add_btn);

    // The action counts the live selection (see `wire_grid_selection_bar`).
    wire_grid_selection_bar(
        &selection,
        &select_all_btn,
        &select_none_btn,
        &add_btn,
        |n| ngettext("_Queue {} item", "_Queue {} items", n as u32).replace("{}", &n.to_string()),
    );

    {
        let close_card = close_card.clone();
        let selection = selection.clone();
        add_btn.connect_clicked(move |_| {
            let picked: Vec<usize> = grid_selected(&selection, count);
            let picked_set: std::collections::HashSet<usize> = picked.into_iter().collect();
            let chosen: Vec<(usize, &crate::media_types::PlaylistItem)> = playlist
                .items
                .iter()
                .enumerate()
                .filter(|(i, _)| picked_set.contains(i))
                .collect();
            if chosen.is_empty() {
                error_caption.set_text(&gettext("Select at least one item"));
                error_caption.set_visible(true);
                return;
            }
            // One persist for the whole import, not one per row.
            let _batch = manager.batch_guard();
            // Multiple items from one collection share a titled subfolder,
            // torrent-style; a lone item keeps the flat behavior.
            let dir = manager.resolve_dir(Some(&dest_dir.borrow()));
            let dir = if chosen.len() > 1 {
                crate::file_names::collection_subdir(&dir, &playlist.title)
            } else {
                dir
            };
            // One readdir for the whole import instead of one per row.
            let existing = crate::video_staging::dir_file_names(std::path::Path::new(&dir));
            // Story segments are addressable as their own pages: queue those so each row
            // re-resolves its own segment instead of the tray (tray + format ids would
            // download the first segment once per row). Attempted unconditionally:
            // highlights and non-story URLs return None and keep the tray.
            let mut failed: Option<String> = None;
            for (i, item) in &chosen {
                let page_url = crate::video_probe::story_segment_url(&playlist.page_url, &item.id)
                    .unwrap_or_else(|| item.page_url.clone());
                let settings = manager.settings();
                let name = default_name_for(settings, &item.title, audio_only);
                if let Err(e) = manager.enqueue_video_staged(
                    &page_url,
                    &dir,
                    Some(&name),
                    crate::media_types::VideoChoices {
                        quality: manager.settings().video_quality(),
                        audio_only,
                        video_format_id: None,
                        // Live streams queued from a playlist take the VOD
                        // path; the worker re-resolves each item page anyway.
                        is_live: false,
                        // Remember the picked entry as fallback: story rows normally carry
                        // segment pages and never need it, but highlights — and anything
                        // unparseable at pick time — re-resolve the tray by this id.
                        playlist_item_id: Some(item.id.to_string()),
                    },
                    &existing,
                ) {
                    failed = Some(e);
                    break;
                }
                // Rows already queued stay queued on a partial failure: unselect
                // them so a retry submits only the remainder (dedupe is by
                // filename).
                selection.unselect_item(*i as u32);
            }
            if let Some(e) = failed {
                error_caption.set_text(&e);
                error_caption.set_visible(true);
                return;
            }
            // Complete success collapses the whole New Download card; a partial failure stays
            // on the picker so the remaining rows (unchecked above) can be retried.
            close_card();
        });
    }

    nav.push(&picker_page);
}

/// Multi-file .torrent intake as a right-sliding card page: one switch per
/// file, all on by default. The selection feeds rqbit's `only_files` at add
/// time (no live setter), so it must be chosen here.
fn push_torrent_picker_page(
    nav: &adw::NavigationView,
    manager: Rc<DownloadManager>,
    dest_dir: Rc<RefCell<String>>,
    close_card: Rc<dyn Fn()>,
    file_name: String,
    bytes: Vec<u8>,
    entries: Vec<crate::torrent::TorrentFileEntry>,
) {
    if nav.visible_page_tag().as_deref() == Some("torrent") {
        return;
    }

    // The file count lives in the compact header; the group needs no title.
    let file_count = ngettext("{} file", "{} files", entries.len() as u32)
        .replace("{}", &entries.len().to_string());
    // Multi-column grid like the playlist picker: path over a dimmed size.
    let entry_count = entries.len();
    let grid_entries: Rc<Vec<(String, String)>> = Rc::new(
        entries
            .iter()
            .map(|e| {
                (
                    e.display_path.clone(),
                    crate::file_names::fmt_bytes(e.length),
                )
            })
            .collect(),
    );
    let (grid, selection) = picker_grid(grid_entries);

    let list_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(6)
        .build();
    list_box.append(&grid);
    let error_caption = gtk4::Label::builder()
        .label("")
        .css_classes(["error", "caption"])
        .halign(gtk4::Align::Start)
        .visible(false)
        .build();
    list_box.append(&error_caption);

    // Same capped scrolled window as the playlist picker: big torrents must
    // not size the card off-screen.
    let scrolled = gtk4::ScrolledWindow::builder()
        .child(&list_box)
        .vexpand(true)
        .propagate_natural_height(true)
        .max_content_height(480)
        .build();

    let toolbar = adw::ToolbarView::new();
    // Same compact header as the playlist picker: back + file name + count.
    toolbar.add_top_bar(&picker_header(nav, &file_name, &file_count));
    toolbar.set_content(Some(&scrolled));
    // HIG selection mode: the selection's actions live in a bottom
    // action bar, not the header. Back pops the page (cancels).
    let (action_bar, select_all_btn, select_none_btn, add_btn) = selection_action_bar();
    toolbar.add_bottom_bar(&action_bar);
    let picker_page = adw::NavigationPage::builder()
        .tag("torrent")
        .title(&file_name)
        .child(&toolbar)
        .build();
    picker_enter_confirms(&picker_page, &add_btn);

    // Same as the playlist picker: the action counts the live selection.
    wire_grid_selection_bar(
        &selection,
        &select_all_btn,
        &select_none_btn,
        &add_btn,
        |n| ngettext("_Add {} file", "_Add {} files", n as u32).replace("{}", &n.to_string()),
    );

    {
        let close_card = close_card.clone();
        let selection = selection.clone();
        add_btn.connect_clicked(move |_| {
            let selected: Vec<usize> = grid_selected(&selection, entry_count);
            if selected.is_empty() {
                error_caption.set_text(&gettext("Select at least one file"));
                error_caption.set_visible(true);
                return;
            }
            // All on means no filter: pass None, not every index.
            let only = (selected.len() < entry_count).then_some(selected);
            enqueue_and_close(
                &dest_dir,
                &close_card,
                |d| manager.enqueue_torrent_file(bytes.clone(), &file_name, d, only),
                |e| {
                    error_caption.set_text(e);
                    error_caption.set_visible(true);
                },
            );
        });
    }

    nav.push(&picker_page);
}

fn wire_torrent_picker(
    torrent_btn: &gtk4::Button,
    manager: Rc<DownloadManager>,
    dest_dir: Rc<RefCell<String>>,
    close_card: Rc<dyn Fn()>,
    nav: &adw::NavigationView,
    error_label: gtk4::Label,
) {
    let nav = nav.clone();
    torrent_btn.connect_clicked(move |_| {
        let m = manager.clone();
        let dd = dest_dir.clone();
        let close_card = close_card.clone();
        let nav = nav.clone();
        let error_label = error_label.clone();
        glib::spawn_future_local(async move {
            let filter = gtk4::FileFilter::new();
            filter.set_name(Some(&gettext("Torrent files")));
            filter.add_mime_type("application/x-bittorrent");
            filter.add_pattern("*.torrent");
            let filters = gio::ListStore::new::<gtk4::FileFilter>();
            filters.append(&filter);
            let picker = gtk4::FileDialog::builder()
                .title(gettext("Choose torrent file"))
                .accept_label(gettext("Add Torrent"))
                .filters(&filters)
                .build();
            let Ok(file) = picker.open_future(None::<&gtk4::Window>).await else {
                return; // dismissed
            };
            let name = file
                .basename()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "download.torrent".to_string());
            let bytes = match file
                .path()
                .and_then(|p| crate::torrent::read_torrent_bytes(&p))
            {
                Some(b) => b,
                None => {
                    error_label.set_text(&gettext("Could not read that .torrent file"));
                    error_label.set_visible(true);
                    return;
                }
            };
            let (_tname, entries) = match crate::torrent::torrent_file_list(&bytes) {
                Ok(v) => v,
                Err(e) => {
                    error_label.set_text(&e);
                    error_label.set_visible(true);
                    return;
                }
            };
            if entries.len() <= 1 {
                enqueue_and_close(
                    &dd,
                    &close_card,
                    |d| m.enqueue_torrent_file(bytes, &name, d, None),
                    |e| {
                        error_label.set_text(e);
                        error_label.set_visible(true);
                    },
                );
                return;
            }
            push_torrent_picker_page(&nav, m, dd, close_card, name, bytes, entries);
        });
    });
}

fn show_video_playlist(v: &VideoStep, pl: &crate::media_types::PlaylistInfo) {
    hide_video_step(v);
    v.group.set_visible(true);
    v.group
        .set_title(glib::markup_escape_text(&pl.title).as_str());
    let mut desc = format!(
        "{} • {}",
        playlist_count_label(pl.kind, pl.items.len()),
        glib::markup_escape_text(&pl.page_url)
    );
    if crate::video_probe::playlist_truncated(pl) {
        desc.push_str(" • ");
        desc.push_str(
            &gettext("Showing the first {n} of {total}")
                .replace("{n}", &pl.items.len().to_string())
                .replace("{total}", &pl.total.to_string()),
        );
    }
    v.group.set_description(Some(&desc));
    v.audio.set_visible(true);
}

/// Handle for the inline New Download card: the widget to pin under the
/// header plus the open/toggle entry points the header button, the
/// empty-state button, the `add-download` action, and application-open URLs
/// all share. Cheap to clone; all state lives in the captured `Rc`s.
#[derive(Clone)]
pub struct AddCard {
    widget: gtk4::Widget,
    open: Rc<dyn Fn(Option<String>)>,
    toggle: Rc<dyn Fn()>,
    open_torrent_picker: TorrentPickerOpener,
}

impl AddCard {
    /// The card widget, pinned under the header as a toolbar top bar.
    pub fn widget(&self) -> &gtk4::Widget {
        &self.widget
    }

    /// Reveal the card, pre-filling `initial_url` when given (drops and
    /// Open With). An already-open card only gains focus — no reset;
    /// collapse always resets, so a reopened card starts fresh.
    pub fn open(&self, initial_url: Option<String>) {
        (self.open)(initial_url)
    }

    /// Shared toggle for every New Download affordance (header `+`,
    /// Ctrl+N, the empty-state pill): reveal a fresh card, or retract the
    /// open one (which resets it, like Cancel/Escape).
    pub fn toggle(&self) {
        (self.toggle)()
    }

    /// Push the multi-file torrent picker for an already-read .torrent
    /// (drag-and-drop / Open With), revealing the card first.
    pub fn open_torrent_picker(
        &self,
        file_name: String,
        bytes: Vec<u8>,
        entries: Vec<crate::torrent::TorrentFileEntry>,
    ) {
        (self.open_torrent_picker)(file_name, bytes, entries)
    }
}

/// Build the inline New Download card. The returned [`AddCard`] owns the
/// widget and the open/toggle entry points; the card starts collapsed.
pub fn build_add_card(manager: Rc<DownloadManager>) -> AddCard {
    let is_open = Rc::new(Cell::new(false));

    // Card chrome: a slide-down revealer so the card animates in under the
    // header; collapsed it takes no space.
    let revealer = gtk4::Revealer::builder()
        .transition_type(gtk4::RevealerTransitionType::SlideDown)
        .reveal_child(false)
        .build();
    let card = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    card.set_margin_top(12);
    card.set_margin_start(12);
    card.set_margin_end(12);
    card.add_css_class("card");
    card.add_css_class("new-download-card");

    // No in-card title: the card only opens from explicit "New Download"
    // affordances (+, Ctrl+N, the empty-state pill), so restating it is
    // redundant. The navigation page below keeps the accessible name.
    // Dismissal lives in the URL row with the other actions — an inline
    // card has no window controls.
    let cancel_btn = gtk4::Button::builder()
        .icon_name("window-close-symbolic")
        .css_classes(["flat", "circular"])
        .tooltip_text(gettext("Cancel"))
        .valign(gtk4::Align::Center)
        .build();
    cancel_btn.update_property(&[gtk4::accessible::Property::Label(&gettext("Cancel"))]);

    let nav = adw::NavigationView::new();
    card.append(&nav);
    revealer.set_child(Some(&card));

    // Form page: URL row (entry + Add), the video preview block, then the
    // file / torrent / destination rows. No margins — the .card CSS class
    // provides the HIG 12px inner padding; the card's outer margins give
    // the spacing from the window.
    let form = gtk4::Box::new(gtk4::Orientation::Vertical, 12);

    // URL form: HIG AdwPreferencesGroup → AdwEntryRow. The Add action is
    // a persistent ✓ suffix button (not the built-in apply button, which
    // hides when the text hasn't changed); spinner, gear, close follow.
    // (No Page wrapper — the Page adds dialog margins.)
    let url_group = adw::PreferencesGroup::new();
    url_group.set_hexpand(true);
    let url_entry = adw::EntryRow::builder()
        .title(gettext("Paste a download link"))
        .activates_default(true)
        .build();
    url_entry.set_input_purpose(gtk4::InputPurpose::Url);
    let url_spinner = adw::Spinner::new();
    url_spinner.set_visible(false);
    url_entry.add_suffix(&url_spinner);
    // Persistent Add button: stays visible so a second press confirms
    // after the preview loads.
    let add_btn = gtk4::Button::builder()
        .icon_name("object-select-symbolic")
        .tooltip_text(gettext("Add download"))
        .css_classes(["flat"])
        .valign(gtk4::Align::Center)
        .build();
    add_btn.update_property(&[gtk4::accessible::Property::Label(&gettext("Add download"))]);
    url_entry.add_suffix(&add_btn);
    url_group.add(&url_entry);
    // Gear toggle for the download options: the HIG settings icon
    // (emblem-system-symbolic), bound to the options revealer below.
    let opts_toggle = gtk4::ToggleButton::builder()
        .icon_name("emblem-system-symbolic")
        .tooltip_text(gettext("Download options"))
        .valign(gtk4::Align::Center)
        .build();
    opts_toggle.update_property(&[gtk4::accessible::Property::Label(&gettext(
        "Download options",
    ))]);
    url_entry.add_suffix(&opts_toggle);
    url_entry.add_suffix(&cancel_btn);
    form.append(&url_group);

    // Download options live in a revealer directly under the URL row: the
    // card opens compact, one tap on the gear reveals file name, torrent,
    // and destination inline.
    let opts_revealer = gtk4::Revealer::builder()
        .transition_type(gtk4::RevealerTransitionType::SlideDown)
        .reveal_child(false)
        .visible(false)
        .build();
    opts_toggle
        .bind_property("active", &opts_revealer, "reveal-child")
        .bidirectional()
        .sync_create()
        .build();
    // Hidden when collapsed so the form's Box spacing doesn't leave a
    // gap under the URL row.
    opts_toggle
        .bind_property("active", &opts_revealer, "visible")
        .sync_create()
        .build();
    form.append(&opts_revealer);

    // Video preview block: hidden until a lookup runs; exactly one state shows.
    let video_group = adw::PreferencesGroup::new();
    video_group.set_visible(false);
    let video_name = adw::EntryRow::builder()
        .title(gettext("File name"))
        .activates_default(false)
        .build();
    let video_revert_btn = gtk4::Button::builder()
        .icon_name("edit-undo-symbolic")
        .css_classes(["flat"])
        .tooltip_text(gettext("Revert to Title"))
        .valign(gtk4::Align::Center)
        .build();
    video_revert_btn.update_property(&[gtk4::accessible::Property::Label(&gettext(
        "Revert to Title",
    ))]);
    video_name.add_suffix(&video_revert_btn);
    video_group.add(&video_name);
    // Media-format selector, filled per video on resolve: exact pinnable
    // formats, tallest first (the preference preselects the closest row), or a
    // single Automatic row when nothing is pinnable.
    let video_format = adw::ComboRow::builder()
        .title(gettext("Media format"))
        .build();
    video_group.add(&video_format);
    let video_audio = adw::SwitchRow::builder()
        .title(gettext("Audio only"))
        .build();
    video_group.add(&video_audio);
    let video_tools = adw::ActionRow::builder()
        .title(gettext("Support tools"))
        .build();
    let video_install_btn = gtk4::Button::builder()
        .label(gettext("Install"))
        .tooltip_text(gettext("Download the yt-dlp support tools"))
        .valign(gtk4::Align::Center)
        .build();
    // Whole-row click hits Install (same pattern as the torrent row below).
    video_tools.set_activatable_widget(Some(&video_install_btn));
    video_tools.add_suffix(&video_install_btn);
    video_group.add(&video_tools);
    let video_error = adw::ActionRow::builder()
        .title(gettext("Couldn't load the media preview"))
        .build();
    let video_retry_btn = gtk4::Button::builder()
        .label(gettext("Retry"))
        .valign(gtk4::Align::Center)
        .build();
    video_error.add_suffix(&video_retry_btn);
    video_group.add(&video_error);
    let step = Rc::new(VideoStep {
        url_spinner: url_spinner.clone(),
        group: video_group.clone(),
        name: video_name,
        revert: video_revert_btn,
        format: video_format,
        options: Rc::new(RefCell::new(Vec::new())),
        audio: video_audio,
        tools: video_tools,
        error: video_error,
    });
    // Card-local choices: the format is initialized from Preferences (not
    // bound); audio-only is always off by design — no global preference
    // exists. Exact picks are per lookup, so nothing persists here.
    {
        let format = step.format.clone();
        step.audio.connect_active_notify(move |sw| {
            format.set_sensitive(!sw.is_active());
        });
    }
    form.append(&video_group);

    let group = adw::PreferencesGroup::new();
    group.set_title(&gettext("Download options"));
    let file_row = adw::EntryRow::builder()
        .title(gettext("File name (optional)"))
        .text("")
        .build();
    group.add(&file_row);

    // Torrent and save location sit behind the gear toggle: the common
    // case is a URL plus an optional file name, so the card opens compact.
    let torrent_btn = gtk4::Button::builder()
        .label(gettext("Choose…"))
        .tooltip_text(gettext("Choose a .torrent file"))
        .valign(gtk4::Align::Center)
        .build();
    let torrent_row = adw::ActionRow::builder()
        .title(gettext("Torrent file"))
        .activatable_widget(&torrent_btn)
        .build();
    torrent_row.add_suffix(&torrent_btn);
    group.add(&torrent_row);

    let dest_label = gtk4::Label::builder()
        .label(manager.effective_download_dir())
        .halign(gtk4::Align::Start)
        .ellipsize(gtk4::pango::EllipsizeMode::Middle)
        .css_classes(["dim-label", "caption"])
        .hexpand(true)
        .build();
    let dest_btn = gtk4::Button::builder()
        .label(gettext("Choose…"))
        .tooltip_text(gettext("Choose download folder"))
        .valign(gtk4::Align::Center)
        .build();
    let dest_row = adw::ActionRow::builder().title(gettext("Save to")).build();
    dest_row.add_suffix(&dest_label);
    dest_row.add_suffix(&dest_btn);
    dest_row.set_activatable_widget(Some(&dest_btn));
    group.add(&dest_row);
    opts_revealer.set_child(Some(&group));

    // Form-level error caption sits outside the options revealer so a failed
    // Add stays visible while the options are collapsed.
    let form_error = gtk4::Label::builder()
        .label("")
        .css_classes(["error", "caption"])
        .halign(gtk4::Align::Start)
        .visible(false)
        .build();
    form.append(&form_error);

    let form_page = adw::NavigationPage::builder()
        .tag("form")
        .title(gettext("New Download"))
        .can_pop(false)
        .child(&form)
        .build();
    nav.push(&form_page);

    // Shared probe state: generation counter, in-flight marker, last
    // resolved URL and probe result. A second Add press while a lookup is
    // still in flight is suppressed by the marker; without it both would
    // spawn yt-dlp and the loser's result would be discarded by the
    // generation guard anyway.
    let probe = Rc::new(RefCell::new(ProbeState::default()));
    // The form's Add button, desensitized while a lookup is in flight (a
    // dead button says so upfront). Every terminal state re-enables it.
    let lookup_add: Rc<RefCell<Option<adw::EntryRow>>> = Rc::new(RefCell::new(None));
    lookup_add.replace(Some(url_entry.clone()));
    let dest_dir = Rc::new(RefCell::new(manager.effective_download_dir()));

    // Collapse the card and reset the form to a fresh state, like closing the
    // old dialog: reopening always starts clean, including the destination.
    let close_card: Rc<dyn Fn()> = {
        let revealer = revealer.clone();
        let is_open = Rc::clone(&is_open);
        let probe = Rc::clone(&probe);
        let url_entry = url_entry.clone();
        let file_row = file_row.clone();
        let step = Rc::clone(&step);
        let form_error = form_error.clone();
        let lookup_add = Rc::clone(&lookup_add);
        let nav = nav.clone();
        let dest_dir = Rc::clone(&dest_dir);
        let dest_label = dest_label.clone();
        let opts_revealer = opts_revealer.clone();
        let default_dir = manager.effective_download_dir();
        Rc::new(move || {
            is_open.set(false);
            // Cancel any in-flight probe and drop its state; the
            // generation bump discards the stale completion.
            probe.borrow_mut().reset();
            revealer.set_reveal_child(false);
            reset_video_step(&step);
            url_entry.remove_css_class("error");
            form_error.set_visible(false);
            set_lookup_add(&lookup_add, true);
            file_row.set_text("");
            dest_dir.replace(default_dir.clone());
            dest_label.set_text(&default_dir);
            // The options reopen collapsed with the default destination,
            // like every other row of the fresh form.
            opts_revealer.set_reveal_child(false);
            // Clearing the URL fires the changed handler, which hides the
            // step again; the generation bump keeps it from touching probe
            // state.
            url_entry.set_text("");
            while nav.visible_page_tag().as_deref() != Some("form") {
                if !nav.pop() {
                    break;
                }
            }
        })
    };

    // Video resolve machinery: metadata lookup that never blocks the main
    // loop, started only by an explicit Add/Enter press. The probe state's
    // generation drops stale completions; every async touch re-checks the
    // generation.
    let kick_video = {
        let probe = Rc::clone(&probe);
        let step2 = step.clone();
        let url_entry2 = url_entry.clone();
        let file_row2 = file_row.clone();
        let settings2 = manager.settings().clone();
        let lookup_add_kick = lookup_add.clone();
        let manager_kick = manager.clone();
        let dest_kick = dest_dir.clone();
        let close_kick = close_card.clone();
        Rc::new(move |probe_unlisted: bool| {
            // Twin suppression: a resolve for this exact URL is already
            // running for the current generation (Add pressed twice while
            // the lookup is still in flight is the usual trigger). The twin's
            // result would lose the generation race anyway — don't spawn a
            // second yt-dlp. The marker carries the owning kick's generation
            // so a stale marker — its resolve already doomed by a generation
            // bump — never suppresses a re-kick for the same URL. It also
            // carries the kick's unlisted-probe flag: an explicit Add/Enter
            // kick probes unlisted URLs.
            let url = url_entry2.text().trim().to_string();
            let Some(my) = probe.borrow_mut().kick(url, probe_unlisted) else {
                return;
            };
            let (
                probe_b,
                step_b,
                url_b,
                settings_b,
                file_b,
                lookup_add_b,
                manager_b,
                dest_b,
                close_b,
            ) = (
                probe.clone(),
                step2.clone(),
                url_entry2.clone(),
                settings2.clone(),
                file_row2.clone(),
                lookup_add_kick.clone(),
                manager_kick.clone(),
                dest_kick.clone(),
                close_kick.clone(),
            );
            glib::spawn_future_local(async move {
                // Owns the in-flight marker: every exit below clears it for
                // this generation (a stale generation leaves a newer marker).
                let _guard = InflightGuard {
                    probe: Rc::clone(&probe_b),
                    my,
                };
                let url = url_b.text().trim().to_string();
                // Unlisted links probe only on explicit kicks (Add/Enter,
                // retry). Non-HTTP schemes never probe: magnets have their
                // own flows.
                let probing = probe_unlisted
                    && !crate::video::is_video_page(&url)
                    && crate::video::is_http_url(&url);
                if url.is_empty() || (!crate::video::is_video_page(&url) && !probing) {
                    // A stale probe for another URL must not linger: editing to a fresh
                    // URL would show the old preview.
                    let stale = {
                        let st = probe_b.borrow();
                        !crate::video::preview_fresh(&st.info, st.last_ok.as_str(), &url)
                    };
                    if stale {
                        hide_video_step(&step_b);
                        probe_b.borrow_mut().info.take();
                        set_lookup_add(&lookup_add_b, true);
                    }
                    return;
                }
                let fresh = {
                    let st = probe_b.borrow();
                    crate::video::preview_fresh(&st.info, st.last_ok.as_str(), &url)
                };
                if fresh {
                    show_video_ready(&step_b);
                    set_lookup_add(&lookup_add_b, true);
                    return;
                }
                // Fast local tools check first: missing tools show Install
                // with no spinner round-trip.
                let libs = match crate::video::resolve_libraries() {
                    Ok(libs) => libs,
                    Err(e) => {
                        if probe_b.borrow().generation != my {
                            return;
                        }
                        probe_b.borrow_mut().info.take();
                        show_video_tools_missing(&step_b, &e.to_string());
                        set_lookup_add(&lookup_add_b, true);
                        return;
                    }
                };
                show_video_loading(&step_b);
                set_lookup_add(&lookup_add_b, false);
                // Invalid manual proxy fails the lookup loudly, matching
                // the row behavior: no silent direct extraction.
                let proxy = match crate::download::DownloadOptions::from_settings(&settings_b)
                    .proxy_config()
                {
                    Ok(proxy) => proxy,
                    Err(e) => {
                        if probe_b.borrow().generation != my {
                            return;
                        }
                        show_video_error(&step_b, &e);
                        set_lookup_add(&lookup_add_b, true);
                        return;
                    }
                };
                match crate::video::fetch_video_infos(
                    libs,
                    url.clone(),
                    settings_b.cookies_browser(),
                    settings_b.video_codec_newest(),
                    proxy,
                )
                .await
                {
                    Err(e) => {
                        if probe_b.borrow().generation != my {
                            return;
                        }
                        // Probed links fall back to today's outcome (queue the file directly)
                        // only when extraction says unsupported; transient failures keep
                        // the error row with retry.
                        if !crate::video::is_video_page(&url)
                            && e.to_string().to_lowercase().contains("unsupported url")
                        {
                            match queue_plain(&manager_b, &dest_b, &close_b, &file_b, &url) {
                                Ok(()) => return,
                                Err(pe) => {
                                    fallback_plain_failed(
                                        &probe_b,
                                        &step_b,
                                        &lookup_add_b,
                                        &url,
                                        &pe,
                                    );
                                    return;
                                }
                            }
                        }
                        // Drive serves videos and plain files behind the same share URLs, but
                        // yt-dlp's Drive extractor is playback-API-only: PDFs, docs and zips
                        // fail it with HTTP 400. Those fall back to a direct export download;
                        // anything else keeps the error row with retry.
                        if let Some(direct) = crate::video::drive_direct_url(&url)
                            && {
                                let msg = e.to_string().to_ascii_lowercase();
                                msg.contains("400") || msg.contains("bad request")
                            }
                        {
                            match queue_plain(&manager_b, &dest_b, &close_b, &file_b, &direct) {
                                Ok(()) => return,
                                Err(pe) => {
                                    probe_b.borrow_mut().info.take();
                                    tracing::warn!(
                                        host = %crate::video_probe::page_host(&url),
                                        error = %pe.to_string(),
                                        "drive direct fallback failed"
                                    );
                                    show_video_error(&step_b, &pe);
                                    set_lookup_add(&lookup_add_b, true);
                                    return;
                                }
                            }
                        }
                        probe_b.borrow_mut().info.take();
                        tracing::warn!(
                            host = %crate::video_probe::page_host(&url),
                            error = %e.to_string(),
                            "video preview failed"
                        );
                        show_video_error(&step_b, &e.to_string());
                        set_lookup_add(&lookup_add_b, true);
                    }
                    Ok(probe) => {
                        if probe_b.borrow().generation != my {
                            return;
                        }
                        // Resolved but nothing playable, and not a listed video
                        // page: same plain fallback as above.
                        if !probe.fetchable() && !crate::video::is_video_page(&url) {
                            match queue_plain(&manager_b, &dest_b, &close_b, &file_b, &url) {
                                Ok(()) => return,
                                Err(pe) => {
                                    fallback_plain_failed(
                                        &probe_b,
                                        &step_b,
                                        &lookup_add_b,
                                        &url,
                                        &pe,
                                    );
                                    return;
                                }
                            }
                        }
                        match probe {
                            crate::video::ProbeResult::Single(v) => {
                                // Group header carries the identity (title + page); rows
                                // below carry the choices. Both sinks parse Pango markup:
                                // URLs carry `&`, titles anything.
                                let desc =
                                    match v.duration_string.as_deref().filter(|s| !s.is_empty()) {
                                        Some(d) => format!(
                                            "{} • {}",
                                            glib::markup_escape_text(&v.page_url),
                                            glib::markup_escape_text(d)
                                        ),
                                        None => glib::markup_escape_text(&v.page_url).to_string(),
                                    };
                                step_b
                                    .group
                                    .set_title(glib::markup_escape_text(&v.title).as_str());
                                step_b.group.set_description(Some(&desc));
                                // Seed the file name once: an explicit name wins, else
                                // the title default. Never clobbers an edit here.
                                if step_b.name.text().trim().is_empty() {
                                    let typed = file_b.text().trim().to_string();
                                    let base = if typed.is_empty() {
                                        default_name_for(
                                            &settings_b,
                                            &v.title,
                                            step_b.audio.is_active(),
                                        )
                                    } else {
                                        typed
                                    };
                                    step_b.name.set_text(&base);
                                }
                                probe_b.borrow_mut().last_ok = url;
                                rebuild_format_options(&step_b, &v, &settings_b.video_quality());
                                probe_b.borrow_mut().info =
                                    Some(crate::video::ProbeResult::Single(v));
                                show_video_ready(&step_b);
                                set_lookup_add(&lookup_add_b, true);
                            }
                            crate::video::ProbeResult::Playlist(pl) => {
                                if pl.items.is_empty() {
                                    probe_b.borrow_mut().info.take();
                                    show_video_error(
                                        &step_b,
                                        &gettext("No items found in this playlist"),
                                    );
                                    set_lookup_add(&lookup_add_b, true);
                                    return;
                                }
                                probe_b.borrow_mut().last_ok = url;
                                probe_b.borrow_mut().info =
                                    Some(crate::video::ProbeResult::Playlist(pl.clone()));
                                show_video_playlist(&step_b, &pl);
                                set_lookup_add(&lookup_add_b, true);
                            }
                        }
                    }
                }
            });
        })
    };

    // One submit path for the Add button and Enter: video pages go through the
    // media pipeline (a matching preview is required so the row stores the
    // resolved page, not a stale URL); everything else keeps direct enqueue.
    let submit = {
        let m = manager.clone();
        let dd = dest_dir.clone();
        let url_entry = url_entry.clone();
        let file_row = file_row.clone();
        let form_error = form_error.clone();
        let close_card = close_card.clone();
        let probe = Rc::clone(&probe);
        let step2 = step.clone();
        let kick = kick_video.clone();
        let lookup_add_submit = lookup_add.clone();
        let nav2 = nav.clone();
        Rc::new(move |from_activate: bool| {
            let fail = |message: &str| {
                form_error.set_text(message);
                form_error.set_visible(true);
                url_entry.add_css_class("error");
            };
            let url = url_entry.text().trim().to_string();
            if crate::video::is_video_page(&url) {
                // Same freshness gate as the kick skip: the stored page URL is
                // canonicalized, so only the round-trip key (which text was resolved) decides.
                let ready = {
                    let st = probe.borrow();
                    if crate::video::preview_fresh(&st.info, st.last_ok.as_str(), &url) {
                        st.info.clone()
                    } else {
                        None
                    }
                };
                match ready {
                    Some(probe) => submit_probe(
                        &m,
                        &dd,
                        &close_card,
                        &step2,
                        &lookup_add_submit,
                        &nav2,
                        probe,
                    ),
                    None => {
                        // A tap while the lookup is still running: don't stack
                        // a twin resolve, say so instead.
                        if probe.borrow().is_inflight(&url, false) {
                            show_video_error(
                                &step2,
                                &gettext(
                                    "Still looking up the media — wait for the preview, then add.",
                                ),
                            );
                        } else {
                            kick(false);
                        }
                    }
                }
                return;
            }
            // Unlisted http(s) links get one probe for a video path, unless they are obviously
            // direct files (the plain engine downloads those better, with no probe delay). A
            // fresh preview queues like a listed video page (canonical page, never the typed
            // link); anything else skips straight to the plain intake below.
            if crate::video::is_http_url(&url) && !crate::video::is_direct_file_url(&url) {
                let fresh = {
                    let st = probe.borrow();
                    crate::video::preview_fresh(&st.info, st.last_ok.as_str(), &url)
                };
                if !fresh {
                    // Resubmit while already probing: say so instead of
                    // stacking a twin resolve.
                    if probe.borrow().is_inflight(&url, true) {
                        show_video_error(
                            &step2,
                            &gettext(
                                "Still looking up the media — wait for the preview, then add.",
                            ),
                        );
                    } else {
                        kick(true);
                    }
                } else {
                    // Fresh preview on a link that probed video-shaped (e.g. a dai.ly short
                    // link): queue it like a listed video page — without this the press fell
                    // through to a bare return. preview_fresh implies Single or Playlist, so
                    // this is exhaustive.
                    let cached = probe.borrow().info.clone();
                    match cached {
                        Some(crate::video::ProbeResult::Single(v)) => {
                            submit_probed_single(
                                &m,
                                &dd,
                                &close_card,
                                &step2,
                                &lookup_add_submit,
                                &v,
                            );
                        }
                        Some(crate::video::ProbeResult::Playlist(pl)) => {
                            push_playlist_items_page(
                                &nav2,
                                m.clone(),
                                dd.clone(),
                                close_card.clone(),
                                pl,
                                step2.audio.is_active(),
                            );
                        }
                        None => {}
                    }
                }
                return;
            }
            // Enter on an empty field stays silent (stray Enter); the Add
            // button surfaces the intake error instead.
            if url.is_empty() && from_activate {
                return;
            }
            let fname = file_row.text().trim().to_string();
            enqueue_and_close(
                &dd,
                &close_card,
                |d| {
                    m.enqueue(
                        &url,
                        d,
                        if fname.is_empty() {
                            None
                        } else {
                            Some(fname.as_str())
                        },
                    )
                },
                fail,
            );
        })
    };
    {
        let s = submit.clone();
        add_btn.connect_clicked(move |_| s(false));
    }
    {
        let s = submit.clone();
        url_entry.connect_activate(move |_| s(true));
    }

    // Sync the form skeleton while typing. The lookup itself never fires
    // on its own: pasting or editing only updates the form, and the resolve
    // starts when Add Download (or Enter) is pressed.
    {
        let probe = Rc::clone(&probe);
        let step2 = step.clone();
        let file_row2 = file_row.clone();
        let form_error2 = form_error.clone();
        url_entry.connect_changed(move |row| {
            form_error2.set_visible(false);
            row.remove_css_class("error");
            let text = row.text().trim().to_string();
            // The direct-only file row hides in video mode (the preview has its own
            // name row); a non-empty entry is not lost — the resolve seeds the video name
            // from it. A probed preview counts as video mode while its canonical URL matches.
            let fresh = probe
                .borrow()
                .info
                .as_ref()
                .is_some_and(|p| p.page_url() == text);
            file_row2.set_visible(!(crate::video::is_video_page(&text) || fresh));
            // Leaving video-land (or editing a resolved URL) hides the stale
            // step at once; `fresh` is deliberately the stricter canonical
            // compare: a mismatch is always safe to hide.
            if !crate::video::is_video_page(&text) || !fresh {
                hide_video_step(&step2);
                if !fresh {
                    probe.borrow_mut().info.take();
                }
            }
            // Editing mid-lookup stales the in-flight resolve so its
            // completion is discarded.
            probe.borrow_mut().bump_generation();
        });
    }

    // Destination chooser.
    {
        let dd = Rc::clone(&dest_dir);
        let dl = dest_label.clone();
        dest_btn.connect_clicked(move |b| {
            let chooser = gtk4::FileDialog::builder()
                .title(gettext("Choose download folder"))
                .accept_label(gettext("Select Folder"))
                .build();
            let root = b.root().and_downcast::<gtk4::Window>();
            let dd2 = Rc::clone(&dd);
            let dl2 = dl.clone();
            chooser.select_folder(root.as_ref(), gio::Cancellable::NONE, move |res| {
                if let Ok(f) = res
                    && let Some(p) = f.path()
                {
                    let s = p.to_string_lossy().into_owned();
                    dl2.set_text(&s);
                    *dd2.borrow_mut() = s;
                }
            });
        });
    }

    wire_torrent_picker(
        &torrent_btn,
        manager.clone(),
        dest_dir.clone(),
        close_card.clone(),
        &nav,
        form_error.clone(),
    );

    // Install / retry / revert / audio-mode wiring.
    {
        let step2 = step.clone();
        let kick = kick_video.clone();
        let btn = video_install_btn.clone();
        // Outside Flatpak there is no bundled binary and host packages can't
        // be installed from here: guide through self-install instead.
        if !crate::video_tools::in_flatpak() {
            btn.set_label(&gettext("How to Install"));
            btn.set_tooltip_text(Some(&gettext("Show terminal install instructions")));
        }
        video_install_btn.connect_clicked(move |_| {
            if !crate::video_tools::in_flatpak() {
                let kick_b = kick.clone();
                crate::install_help::show(&btn, move || kick_b(true));
                return;
            }
            // Flatpak: staged auto-install under the shared progress popover anchored at
            // the button (HIG: feedback lives with its control; stages stand in for the
            // percentages that don't exist).
            let (step_b, kick_b) = (step2.clone(), kick.clone());
            crate::install_progress::run(
                &btn,
                move |err| {
                    step_b.tools.set_subtitle(&err);
                },
                move || {
                    // Re-probe, don't just refresh: an unlisted URL that led
                    // here for missing tools has no preview yet.
                    kick_b(true);
                },
            );
        });
    }
    {
        let kick = kick_video.clone();
        video_retry_btn.connect_clicked(move |_| kick(true));
    }
    // One-click restore of the title default (audio-aware, like submit).
    {
        let (name, audio, probe) = (step.name.clone(), step.audio.clone(), Rc::clone(&probe));
        let settings = manager.settings().clone();
        step.revert.connect_clicked(move |_| {
            let st = probe.borrow();
            if let Some(p) = st.info.as_ref() {
                name.set_text(&default_name_for(&settings, p.title(), audio.is_active()));
                name.grab_focus();
            }
        });
    }
    // Toggling the mode re-seeds an untouched name: the resolve-time seed ran under
    // the other mode, so without this the row keeps a video-container name for an
    // audio download (or vice versa). An edited name is never clobbered.
    {
        let (name, audio, probe) = (step.name.clone(), step.audio.clone(), Rc::clone(&probe));
        let settings = manager.settings().clone();
        audio.connect_active_notify(move |sw| {
            let st = probe.borrow();
            if let Some(p) = st.info.as_ref() {
                let active = sw.is_active();
                let current = name.text().to_string();
                if current.trim().is_empty()
                    || current == default_name_for(&settings, p.title(), !active)
                {
                    name.set_text(&default_name_for(&settings, p.title(), active));
                }
            }
        });
    }

    // Escape collapses the card; Cancel does the same.
    {
        let key = gtk4::EventControllerKey::new();
        let close_card = close_card.clone();
        key.connect_key_pressed(move |_, keyval, _, _| {
            if keyval == gtk4::gdk::Key::Escape {
                close_card();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        card.add_controller(key);
    }
    {
        let close_card = close_card.clone();
        cancel_btn.connect_clicked(move |_| close_card());
    }

    // Open/toggle entry points.
    let reveal = {
        let revealer = revealer.clone();
        let is_open = Rc::clone(&is_open);
        Rc::new(move || {
            is_open.set(true);
            revealer.set_reveal_child(true);
        })
    };
    let open = {
        let reveal = Rc::clone(&reveal);
        let is_open = Rc::clone(&is_open);
        let url_entry = url_entry.clone();
        Rc::new(move |initial_url: Option<String>| {
            let already = is_open.get();
            reveal();
            // Dropped/opened URLs land here pre-filled: setting the text syncs the form, and the
            // lookup itself starts on Add/Enter like any other entry.
            if let Some(raw) = initial_url {
                if let Ok(normalized) = crate::download::normalize_url(raw.trim()) {
                    url_entry.set_text(&normalized);
                }
            } else if !already && url_entry.text().trim().is_empty() {
                // Single clipboard read per fresh open; no watch, no polling.
                let url_entry = url_entry.clone();
                glib::spawn_future_local(async move {
                    let clipboard = gtk4::gdk::Display::default().map(|d| d.clipboard());
                    let Some(clipboard) = clipboard else { return };
                    let Ok(Some(text)) = clipboard.read_text_future().await else {
                        return;
                    };
                    if !url_entry.text().trim().is_empty() {
                        return;
                    }
                    let pasted = text.trim().to_string();
                    if let Ok(normalized) = crate::download::normalize_url(&pasted) {
                        url_entry.set_text(&normalized);
                    }
                });
            }
            // Keyboard-first: focus lands in the URL field so typing starts a
            // download with no tab stops.
            url_entry.grab_focus();
        })
    };
    let toggle = {
        let open = Rc::clone(&open);
        let is_open = Rc::clone(&is_open);
        let close_card = Rc::clone(&close_card);
        Rc::new(move || {
            if is_open.get() {
                close_card();
            } else {
                open(None);
            }
        })
    };
    let open_torrent_picker = {
        let reveal = Rc::clone(&reveal);
        let nav = nav.clone();
        let manager = manager.clone();
        let dest_dir = Rc::clone(&dest_dir);
        let close_card = Rc::clone(&close_card);
        Rc::new(
            move |file_name: String,
                  bytes: Vec<u8>,
                  entries: Vec<crate::torrent::TorrentFileEntry>| {
                reveal();
                push_torrent_picker_page(
                    &nav,
                    manager.clone(),
                    dest_dir.clone(),
                    close_card.clone(),
                    file_name,
                    bytes,
                    entries,
                );
            },
        )
    };

    AddCard {
        widget: revealer.upcast(),
        open,
        toggle,
        open_torrent_picker,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_columns_adapts_without_empty_cells() {
        // Max 4 columns, most-square grid, no empty cells: 3→3×1,
        // 4→2×2, 8→4×2, 9→3×3, 12→4×3.
        assert_eq!(picker_columns(0), 1);
        assert_eq!(picker_columns(1), 1);
        assert_eq!(picker_columns(2), 2);
        assert_eq!(picker_columns(3), 3);
        assert_eq!(picker_columns(4), 2);
        assert_eq!(picker_columns(5), 1); // prime: single column
        assert_eq!(picker_columns(6), 3);
        assert_eq!(picker_columns(7), 1); // prime: single column
        assert_eq!(picker_columns(8), 4);
        assert_eq!(picker_columns(9), 3);
        assert_eq!(picker_columns(12), 4);
        // No empty cells: count is always divisible by columns.
        for n in 1..=50 {
            let c = picker_columns(n) as usize;
            assert_eq!(n % c, 0, "n={n} cols={c} leaves empty cells");
        }
    }

    fn dummy_probe() -> crate::video::ProbeResult {
        crate::video::ProbeResult::Single(crate::video::VideoInfo {
            title: "Test title".into(),
            duration: None,
            duration_string: None,
            page_url: "https://youtu.be/x".into(),
            expires_at: None,
            formats: Box::new([]),
            is_live: false,
            fetchable: true,
        })
    }

    #[test]
    fn inflight_guard_clears_only_for_current_generation() {
        let probe = Rc::new(RefCell::new(ProbeState::default()));

        // The owning generation clears the marker on drop.
        let my = probe
            .borrow_mut()
            .kick("https://youtu.be/x".to_string(), false)
            .expect("first kick runs");
        drop(InflightGuard {
            probe: probe.clone(),
            my,
        });
        assert!(probe.borrow().inflight.is_none());

        // A stale generation leaves a newer kick's marker alone.
        let my2 = probe
            .borrow_mut()
            .kick("https://youtu.be/y".to_string(), true)
            .expect("second kick runs");
        drop(InflightGuard {
            probe: probe.clone(),
            my: my2 - 1,
        });
        assert_eq!(
            probe.borrow().inflight.clone(),
            Some(("https://youtu.be/y".to_string(), my2, true))
        );
    }

    #[test]
    fn kick_suppresses_twin_for_current_generation() {
        let mut st = ProbeState::default();
        let my = st
            .kick("https://youtu.be/a".to_string(), false)
            .expect("first kick runs");
        assert_eq!(my, 1);
        // Twin: same URL, generation and probe mode — suppressed.
        assert!(st.kick("https://youtu.be/a".to_string(), false).is_none());
        // A different URL is never a twin.
        assert!(st.kick("https://youtu.be/b".to_string(), false).is_some());
        // Same URL but the other probe mode is a different resolve.
        assert!(st.kick("https://youtu.be/a".to_string(), true).is_some());
    }

    #[test]
    fn reset_clears_probe_state_and_bumps_generation() {
        let mut st = ProbeState::default();
        st.kick("https://youtu.be/a".to_string(), false);
        st.last_ok = "https://youtu.be/a".to_string();
        st.info = Some(dummy_probe());
        let prev_gen = st.generation;

        st.reset();

        assert_eq!(st.generation, prev_gen.wrapping_add(1));
        assert!(
            st.inflight.is_none(),
            "reset must clear the in-flight marker"
        );
        assert!(st.last_ok.is_empty());
        assert!(st.info.is_none());
    }

    #[test]
    fn reset_is_idempotent() {
        let mut st = ProbeState::default();
        st.kick("https://youtu.be/a".to_string(), false);
        st.reset();
        let prev_gen = st.generation;

        // A second reset on the already-clean state keeps it clean.
        st.reset();

        assert_eq!(st.generation, prev_gen.wrapping_add(1));
        assert!(st.inflight.is_none());
        assert!(st.last_ok.is_empty());
        assert!(st.info.is_none());
    }

    #[test]
    fn twin_kick_for_current_generation_is_suppressed() {
        let marker = Some(("https://youtu.be/a".to_string(), 2, false));
        assert!(inflight_suppresses(&marker, "https://youtu.be/a", 2, false));
        // A different URL is never a twin.
        assert!(!inflight_suppresses(
            &marker,
            "https://youtu.be/b",
            2,
            false
        ));
        // No marker, no suppression.
        assert!(!inflight_suppresses(&None, "https://youtu.be/a", 2, false));
    }

    #[test]
    fn explicit_unlisted_kick_is_not_suppressed_by_plain_kick() {
        // A plain kick (probe_unlisted=false) in flight must not suppress an
        // explicit unlisted Add/Enter/retry kick (probe_unlisted=true) for
        // the same URL: the explicit kick's unlisted probe is a different
        // resolve, and suppressing it would show a preview without the
        // unlisted formats the user explicitly asked for.
        let marker = Some(("https://youtu.be/a".to_string(), 2, false));
        assert!(!inflight_suppresses(&marker, "https://youtu.be/a", 2, true));
        // Identical kicks still suppress: no double yt-dlp.
        let marker = Some(("https://youtu.be/a".to_string(), 2, true));
        assert!(inflight_suppresses(&marker, "https://youtu.be/a", 2, true));
    }

    #[test]
    fn stale_inflight_marker_does_not_suppress_rekick() {
        // A -> B -> A: the first A kick's marker goes stale when typing bumps
        // the generation, its result is discarded, and its guard leaves the
        // marker (a newer generation is current). The re-kick for A must
        // still spawn — suppressing on the URL alone would wedge the card
        // with no preview and every retry suppressed.
        let mut generation = 0u64;
        generation += 1; // edited to A
        generation += 1; // kicked A
        let marker = Some(("https://youtu.be/a".to_string(), generation, false));
        generation += 1; // edited to B (no kick on edit)
        generation += 1; // edited back to A
        assert!(!inflight_suppresses(
            &marker,
            "https://youtu.be/a",
            generation,
            false
        ));
    }

    #[test]
    fn enqueue_and_close_releases_dest_borrow_before_close() {
        // Regression: every Add path used to run
        // `match enqueue(..., Some(&dest.borrow())) { Ok(_) => close_card(), ... }`.
        // The borrow temporary in a match scrutinee lives into the arms, so
        // close_card()'s mutable re-borrow (it resets the destination) panicked
        // with "RefCell already borrowed" and aborted the app on every
        // successful video, plain, and torrent Add.
        let dest = Rc::new(RefCell::new("/dl".to_string()));
        let closed = Rc::new(std::cell::Cell::new(false));
        let closed2 = Rc::clone(&closed);
        let dest2 = Rc::clone(&dest);
        let close: Rc<dyn Fn()> = Rc::new(move || {
            dest2.replace("/default".to_string());
            closed2.set(true);
        });
        let seen = Rc::new(RefCell::new(String::new()));
        let seen2 = Rc::clone(&seen);
        enqueue_and_close(
            &dest,
            &close,
            |d| {
                seen2.replace(d.unwrap_or("?").to_string());
                Ok::<(), String>(())
            },
            |_| panic!("enqueue reported success"),
        );
        assert!(closed.get(), "card closes after a successful enqueue");
        assert_eq!(seen.borrow().as_str(), "/dl");
        assert_eq!(
            dest.borrow().as_str(),
            "/default",
            "close ran with the destination borrow released"
        );
    }
}
