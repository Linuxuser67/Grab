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
//! Intake behavior is unchanged from the old dialog: debounced probing with
//! generation freshness and twin suppression, the unlisted-URL and
//! direct-file fallbacks, the Drive fallback, clipboard prefill, probe
//! cancellation on close, preferred-quality preselection, and the
//! select-all/none pickers.

use crate::download::DownloadManager;
use crate::window_rows::{default_name_for, error_label, selection_action_bar};
use adw::prelude::*;
use gettextrs::{gettext, ngettext};
use gtk4::prelude::*;
use gtk4::{gio, glib};
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
/// current generation (Enter while the debounced lookup is still in flight is
/// the usual trigger). The marker carries the kick's unlisted-probe flag: an
/// explicit Enter kick probes unlisted URLs, a different resolve from a typing
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

/// Wire a picker's selection bar to its checkboxes: the confirm action counts
/// the live selection (`count_label` builds its text — msgids differ per
/// picker) and Select All/None flip every checkbox.
fn wire_selection_bar(
    checks: &[gtk4::CheckButton],
    select_all_btn: &gtk4::Button,
    select_none_btn: &gtk4::Button,
    add_btn: &gtk4::Button,
    count_label: impl Fn(usize) -> String + 'static,
) {
    {
        let checks: Vec<gtk4::CheckButton> = checks.to_vec();
        let add_btn = add_btn.clone();
        let refresh = Rc::new({
            let checks = checks.clone();
            move || {
                let n = checks.iter().filter(|c| c.is_active()).count();
                add_btn.set_label(&count_label(n));
            }
        });
        for check in &checks {
            let refresh = refresh.clone();
            check.connect_toggled(move |_| refresh());
        }
        refresh();
    }
    {
        let checks: Vec<gtk4::CheckButton> = checks.to_vec();
        select_all_btn.connect_clicked(move |_| {
            for c in &checks {
                c.set_active(true);
            }
        });
    }
    {
        let checks: Vec<gtk4::CheckButton> = checks.to_vec();
        select_none_btn.connect_clicked(move |_| {
            for c in &checks {
                c.set_active(false);
            }
        });
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
    /// Lookup spinner, floating over the URL entry's trailing edge.
    url_spinner: gtk4::Spinner,
    /// The URL entry the spinner floats over: toggles the `url-lookup`
    /// class that reserves its trailing text space while it is visible.
    url_entry: gtk4::Entry,
    group: adw::PreferencesGroup,
    name: adw::EntryRow,
    revert: gtk4::Button,
    /// ComboRow-looking media-format selector: title/subtitle on the left, the
    /// current pick on the right; opens in-flow and pushes the rows below down.
    format: adw::ExpanderRow,
    format_value: gtk4::Label,
    /// Index-aligned with the expander's option rows. Reset on every resolve.
    options: Rc<RefCell<Vec<FormatOption>>>,
    /// Index into `options` of the current pick.
    selected: Rc<Cell<usize>>,
    /// Option rows and their checkmarks, index-aligned with `options`.
    option_rows: Rc<RefCell<Vec<adw::ActionRow>>>,
    option_checks: Rc<RefCell<Vec<gtk4::Image>>>,
    audio: adw::SwitchRow,
    tools: adw::ActionRow,
    error: adw::ActionRow,
}

/// Reserve trailing text space inside the URL entry while the lookup
/// spinner floats over it. Installed once per display; the `url-lookup`
/// class is toggled with the spinner's visibility, and padding lives
/// inside the entry's allocation so toggling it moves no sibling.
fn ensure_url_lookup_css() {
    static INSTALLED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    INSTALLED.get_or_init(|| {
        let css = gtk4::CssProvider::new();
        css.load_from_string("entry.url-lookup { padding-inline-end: 32px; }");
        if let Some(display) = gtk4::gdk::Display::default() {
            gtk4::style_context_add_provider_for_display(
                &display,
                &css,
                gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });
}

fn hide_video_step(v: &VideoStep) {
    v.group.set_visible(false);
    v.url_spinner.set_spinning(false);
    v.url_spinner.set_visible(false);
    v.url_entry.remove_css_class("url-lookup");
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
    // Drop the format option rows (mirrors `rebuild_format_options`) and
    // the current pick.
    for row in step.option_rows.borrow().iter() {
        step.format.remove(row);
    }
    step.option_rows.borrow_mut().clear();
    step.option_checks.borrow_mut().clear();
    step.options.borrow_mut().clear();
    step.selected.set(0);
    step.format_value.set_text("");
    step.format.set_expanded(false);
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
    v.url_spinner.set_spinning(true);
    v.url_spinner.set_visible(true);
    v.url_entry.add_css_class("url-lookup");
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
fn set_lookup_add(cell: &Rc<RefCell<Option<gtk4::Button>>>, enabled: bool) {
    if let Some(b) = cell.borrow().as_ref() {
        b.set_sensitive(enabled);
    }
}

/// Apply the picked format: move the checkmark, show the pick on the
/// ComboRow-looking row, collapse the options (combo behavior).
fn select_format(step: &VideoStep, index: usize) {
    let options = step.options.borrow();
    let Some(pick) = options.get(index) else {
        return;
    };
    step.selected.set(index);
    for (i, check) in step.option_checks.borrow().iter().enumerate() {
        check.set_visible(i == index);
    }
    step.format_value.set_text(&pick.label);
    step.format.set_expanded(false);
}

/// Rebuild the format options from a fresh resolve (tallest first, the
/// preference preselects the closest row) or a single Automatic row when
/// nothing is pinnable. Selection resets — a pin must never carry over.
fn rebuild_format_options(step: &Rc<VideoStep>, info: &crate::video::VideoInfo, preferred: &str) {
    {
        let mut rows = step.option_rows.borrow_mut();
        let mut checks = step.option_checks.borrow_mut();
        for row in rows.iter() {
            step.format.remove(row);
        }
        rows.clear();
        checks.clear();
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
        for (i, opt) in options.iter().enumerate() {
            let check = gtk4::Image::from_icon_name("object-select-symbolic");
            let row = adw::ActionRow::builder()
                .title(&*opt.label)
                .activatable(true)
                .build();
            row.add_suffix(&check);
            {
                let step = Rc::clone(step);
                row.connect_activate(move |_| select_format(&step, i));
            }
            step.format.add_row(&row);
            rows.push(row);
            checks.push(check);
        }
        *step.options.borrow_mut() = options;
    }
    select_format(
        step,
        crate::video::default_quality_index(&info.formats, preferred),
    );
}

#[allow(clippy::too_many_arguments)]
fn submit_probed_single(
    manager: &Rc<DownloadManager>,
    dest: &Rc<RefCell<String>>,
    close_card: &Rc<dyn Fn()>,
    step: &Rc<VideoStep>,
    lookup_add: &Rc<RefCell<Option<gtk4::Button>>>,
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
    let selected = step.selected.get();
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
    lookup_add: &Rc<RefCell<Option<gtk4::Button>>>,
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
    lookup_add: &Rc<RefCell<Option<gtk4::Button>>>,
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
        .css_classes(["dimmed", "caption"])
        .valign(gtk4::Align::Center)
        .build();
    header.append(&back);
    header.append(&title_label);
    header.append(&count_label);
    header
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

    let page = adw::PreferencesPage::new();
    let count = playlist.items.len();
    // No group title: the count lives in the compact header above, so one
    // title level suffices. The truncation notice stays as the description.
    let group = adw::PreferencesGroup::new();
    if crate::video_probe::playlist_truncated(&playlist) {
        group.set_description(Some(
            &gettext("Showing the first {n} of {total}")
                .replace("{n}", &count.to_string())
                .replace("{total}", &playlist.total.to_string()),
        ));
    }
    page.add(&group);

    let mut checks = Vec::new();
    for item in &playlist.items {
        let check = gtk4::CheckButton::builder().active(true).build();
        check.update_property(&[gtk4::accessible::Property::Label(&item.title)]);
        // Compact single-line rows: the duration sits as a dimmed suffix
        // instead of a subtitle, so more items fit without scrolling.
        let row = adw::ActionRow::builder()
            .title(&*item.title)
            .activatable(true)
            .build();
        if let Some(d) = item.duration {
            let dur = gtk4::Label::builder()
                .label(fmt_item_duration(d))
                .css_classes(["dimmed", "caption"])
                .valign(gtk4::Align::Center)
                .build();
            row.add_suffix(&dur);
        }
        row.add_prefix(&check);
        {
            let check = check.clone();
            row.connect_activate(move |_| {
                check.set_active(!check.is_active());
            });
        }
        checks.push(check);
        group.add(&row);
    }
    let error_label = error_label(&group);

    // Scrolled: big playlists must not size the card off-screen, but the capped
    // natural height lets it grow and shrink with the item count instead of
    // keeping the last size.
    let scrolled = gtk4::ScrolledWindow::builder()
        .child(&page)
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

    // The action counts the live selection (see `wire_selection_bar`).
    wire_selection_bar(&checks, &select_all_btn, &select_none_btn, &add_btn, |n| {
        ngettext("_Queue {} item", "_Queue {} items", n as u32).replace("{}", &n.to_string())
    });

    {
        let close_card = close_card.clone();
        add_btn.connect_clicked(move |_| {
            let chosen: Vec<(usize, &crate::media_types::PlaylistItem)> = playlist
                .items
                .iter()
                .enumerate()
                .filter(|(i, _)| checks[*i].is_active())
                .collect();
            if chosen.is_empty() {
                error_label.set_text(&gettext("Select at least one item"));
                error_label.set_visible(true);
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
                // Rows already queued stay queued on a partial failure: uncheck them so a
                // retry submits only the remainder (dedupe is by filename).
                checks[*i].set_active(false);
            }
            if let Some(e) = failed {
                error_label.set_text(&e);
                error_label.set_visible(true);
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

    let page = adw::PreferencesPage::new();
    // The file count lives in the compact header; the group needs no title.
    let file_count = ngettext("{} file", "{} files", entries.len() as u32)
        .replace("{}", &entries.len().to_string());
    let group = adw::PreferencesGroup::new();
    page.add(&group);

    // HIG selection, not settings: a switch means "a setting is on", a checkbox
    // means "this item is picked". Clicking a row toggles its checkbox.
    let mut checks = Vec::new();
    for e in &entries {
        let check = gtk4::CheckButton::builder().active(true).build();
        check.update_property(&[gtk4::accessible::Property::Label(&e.display_path)]);
        // Same compact single-line rows as the playlist picker: the size
        // sits as a dimmed suffix instead of a subtitle.
        let row = adw::ActionRow::builder()
            .title(&e.display_path)
            .activatable(true)
            .build();
        let size = gtk4::Label::builder()
            .label(crate::file_names::fmt_bytes(e.length))
            .css_classes(["dimmed", "caption"])
            .valign(gtk4::Align::Center)
            .build();
        row.add_suffix(&size);
        row.add_prefix(&check);
        {
            let check = check.clone();
            row.connect_activate(move |_| {
                check.set_active(!check.is_active());
            });
        }
        checks.push(check);
        group.add(&row);
    }
    let error_label = error_label(&group);

    // Same capped scrolled window as the playlist picker: big torrents must
    // not size the card off-screen.
    let scrolled = gtk4::ScrolledWindow::builder()
        .child(&page)
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
    wire_selection_bar(&checks, &select_all_btn, &select_none_btn, &add_btn, |n| {
        ngettext("_Add {} file", "_Add {} files", n as u32).replace("{}", &n.to_string())
    });

    {
        let close_card = close_card.clone();
        add_btn.connect_clicked(move |_| {
            let selected: Vec<usize> = checks
                .iter()
                .enumerate()
                .filter(|(_, c)| c.is_active())
                .map(|(i, _)| i)
                .collect();
            if selected.is_empty() {
                error_label.set_text(&gettext("Select at least one file"));
                error_label.set_visible(true);
                return;
            }
            // All on means no filter: pass None, not every index.
            let only = (selected.len() < checks.len()).then_some(selected);
            enqueue_and_close(
                &dest_dir,
                &close_card,
                |d| manager.enqueue_torrent_file(bytes.clone(), &file_name, d, only),
                |e| {
                    error_label.set_text(e);
                    error_label.set_visible(true);
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

    // No in-card title: the card only opens from explicit "New Download"
    // affordances (+, Ctrl+N, the empty-state pill), so restating it is
    // redundant. The navigation page below keeps the accessible name.
    // Dismissal lives in the URL row with the other actions — an inline
    // card has no window controls.
    let cancel_btn = gtk4::Button::builder()
        .icon_name("window-close-symbolic")
        .css_classes(["flat", "circular"])
        .tooltip_text(gettext("Cancel"))
        .build();
    cancel_btn.update_property(&[gtk4::accessible::Property::Label(&gettext("Cancel"))]);

    let nav = adw::NavigationView::new();
    card.append(&nav);
    revealer.set_child(Some(&card));

    // Form page: URL row (entry + Add), the video preview block, then the
    // file / torrent / destination rows.
    let form = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    form.set_margin_top(6);
    form.set_margin_bottom(12);
    form.set_margin_start(12);
    form.set_margin_end(12);

    let url_entry = gtk4::Entry::builder()
        .placeholder_text(gettext("Paste a download link"))
        .hexpand(true)
        .build();
    url_entry.set_input_purpose(gtk4::InputPurpose::Url);
    let add_btn = gtk4::Button::builder()
        .label(gettext("_Add Download"))
        .use_underline(true)
        .css_classes(["suggested-action"])
        .build();
    // Gear toggle for the download options: the HIG settings icon
    // (emblem-system-symbolic), bound to the options revealer below.
    let opts_toggle = gtk4::ToggleButton::builder()
        .icon_name("emblem-system-symbolic")
        .tooltip_text(gettext("Download options"))
        .build();
    opts_toggle.update_property(&[gtk4::accessible::Property::Label(&gettext(
        "Download options",
    ))]);
    let url_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    url_box.append(&url_overlay);
    url_box.append(&add_btn);
    url_box.append(&opts_toggle);
    url_box.append(&cancel_btn);
    form.append(&url_box);

    // Download options live in a revealer directly under the URL row: the
    // card opens compact, one tap on the gear reveals file name, torrent,
    // and destination inline.
    let opts_revealer = gtk4::Revealer::builder()
        .transition_type(gtk4::RevealerTransitionType::SlideDown)
        .reveal_child(false)
        .build();
    opts_toggle
        .bind_property("active", &opts_revealer, "reveal-child")
        .bidirectional()
        .sync_create()
        .build();
    form.append(&opts_revealer);

    // Video preview block: hidden until a lookup runs; exactly one state shows.
    let video_group = adw::PreferencesGroup::new();
    video_group.set_visible(false);
    // The lookup spinner lives inside the URL entry (browser-address-bar
    // style): no separate status line for the transient loading state, and
    // no layout shift when a lookup starts. GtkEntry has no add_suffix, so
    // the spinner floats over the entry's trailing edge in a GtkOverlay;
    // the entry reserves trailing text space via the `url-lookup` class
    // (toggled with the spinner) so text never slides underneath it. The
    // accessible label carries the "Looking up…" text the spinner replaces
    // visually.
    ensure_url_lookup_css();
    let url_spinner = gtk4::Spinner::new();
    url_spinner.update_property(&[gtk4::accessible::Property::Label(&gettext("Looking up…"))]);
    url_spinner.set_halign(gtk4::Align::End);
    url_spinner.set_valign(gtk4::Align::Center);
    url_spinner.set_margin_end(10);
    url_spinner.set_visible(false);
    let url_overlay = gtk4::Overlay::new();
    url_overlay.set_hexpand(true);
    url_overlay.set_child(Some(&url_entry));
    url_overlay.add_overlay(&url_spinner);
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
    // single Automatic row when nothing is pinnable. ComboRow-looking: the
    // title/subtitle on the left, the current pick on the right.
    let format_value = gtk4::Label::new(None);
    let video_format = adw::ExpanderRow::builder()
        .title(gettext("Media format"))
        .subtitle(gettext("Uses your preferred quality"))
        .build();
    video_format.add_suffix(&format_value);
    video_group.add(&video_format);
    let video_audio = adw::SwitchRow::builder()
        .title(gettext("Audio only"))
        .subtitle(gettext("Skip the video track"))
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
        url_entry: url_entry.clone(),
        group: video_group.clone(),
        name: video_name,
        revert: video_revert_btn,
        format: video_format,
        format_value,
        options: Rc::new(RefCell::new(Vec::new())),
        selected: Rc::new(Cell::new(0)),
        option_rows: Rc::new(RefCell::new(Vec::new())),
        option_checks: Rc::new(RefCell::new(Vec::new())),
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
        .css_classes(["dimmed", "caption"])
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
    // resolved URL and probe result. The submit path kicks while the
    // debounced keystroke lookup may still be in flight; without the
    // marker both spawn yt-dlp and the loser's result is discarded by
    // the generation guard anyway.
    let probe = Rc::new(RefCell::new(ProbeState::default()));
    // The form's Add button, desensitized while a lookup is in flight (a
    // dead button says so upfront). Every terminal state re-enables it.
    let lookup_add: Rc<RefCell<Option<gtk4::Button>>> = Rc::new(RefCell::new(None));
    lookup_add.replace(Some(add_btn.clone()));
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
            // Clearing the URL fires the changed handler: it hides the step
            // again and spawns a stale debounce the generation bump discards.
            url_entry.set_text("");
            while nav.visible_page_tag().as_deref() != Some("form") {
                if !nav.pop() {
                    break;
                }
            }
        })
    };

    // Video resolve machinery: debounced metadata lookup that never blocks the
    // main loop. The probe state's generation drops stale completions while
    // the user keeps typing; every async touch re-checks the generation.
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
            // running for the current generation (Enter while the debounced
            // lookup is still in flight is the usual trigger). The twin's
            // result would lose the generation race anyway — don't spawn a
            // second yt-dlp. The marker carries the owning kick's generation
            // so a stale marker — its resolve already doomed by a generation
            // bump — never suppresses a re-kick for the same URL. It also
            // carries the kick's unlisted-probe flag: an explicit Enter kick
            // probes unlisted URLs, a different resolve from a typing kick.
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
                // Unlisted links probe only on explicit kicks (submit, retry), never while
                // typing. Non-HTTP schemes never probe: magnets have their own flows.
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

    // Debounced auto-lookup while typing (600 ms idle); Enter submits
    // immediately through the path above.
    {
        let probe = Rc::clone(&probe);
        let kick = kick_video.clone();
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
            // Sync skeleton: leaving video-land (or editing a resolved URL) hides the stale
            // step at once; the debounced kick refills it. `fresh` is deliberately the
            // stricter canonical compare: a mismatch is always safe to hide.
            if !crate::video::is_video_page(&text) || !fresh {
                hide_video_step(&step2);
                if !fresh {
                    probe.borrow_mut().info.take();
                }
            }
            let my = probe.borrow_mut().bump_generation();
            let (probe_b, kick_b) = (probe.clone(), kick.clone());
            glib::spawn_future_local(async move {
                glib::timeout_future(std::time::Duration::from_millis(600)).await;
                if probe_b.borrow().generation != my {
                    return;
                }
                kick_b(false);
            });
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
            // Dropped/opened URLs land here pre-filled: setting the text fires the same changed →
            // debounce → lookup chain as typing, so video pages resolve through the media pipeline.
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
    fn explicit_unlisted_kick_is_not_suppressed_by_typing_kick() {
        // A debounced typing kick (probe_unlisted=false) in flight must not
        // suppress an explicit Enter/retry kick (probe_unlisted=true) for the
        // same URL: the explicit kick's unlisted probe is a different
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
        generation += 1; // typed A
        generation += 1; // kicked A
        let marker = Some(("https://youtu.be/a".to_string(), generation, false));
        generation += 1; // typed B (debounced kick skipped)
        generation += 1; // typed A again
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
