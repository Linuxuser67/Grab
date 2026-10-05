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
use crate::download_row::DownloadItem;
use crate::download_store::DownloadStatus;
use crate::window_rows::{default_name_for, selection_action_bar};
use adw::prelude::*;
use gettextrs::{gettext, ngettext};
use glib::object::Cast;
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

/// One media-format option: an exact pinnable format from the probe, the
/// Automatic row (the global preference, no pin) when nothing is pinnable,
/// or the Audio only row (no pin, audio-only download).
#[derive(Clone)]
struct FormatOption {
    /// Row title: the pin label ("1080p"), "Automatic", or "Audio only".
    label: String,
    /// Resolved yt-dlp format id; `None` for the Automatic and Audio only rows.
    format_id: Option<String>,
    /// True for the Audio only row.
    audio_only: bool,
}

/// The video preview block inside the form: exactly one state shows at a
/// time (ready rows, tools row, or the centered error card), driven by the
/// probe below.
struct VideoStep {
    /// Action slot beside the URL entry: a homogeneous GtkStack holding the
    /// Add button and the lookup spinner. Swapping pages keeps the slot at
    /// the widest child's width, so a lookup never reallocates the URL row
    /// (no separate status line, no layout shift when a lookup starts).
    action_slot: gtk4::Stack,
    /// Revealer wrapping the action slot: the Add button / spinner slides
    /// in from the left (SlideRight) after the tick submits, pushing the
    /// gear and X buttons aside; it slides out on enqueue or card close.
    /// The slide uses GtkRevealer, the established API — no hand-rolled
    /// animation.
    action_revealer: gtk4::Revealer,
    /// The spinner page of the action slot: kept so the show/hide helpers
    /// can set and clear its accessible label.
    url_spinner: adw::Spinner,
    /// Slide-down revealer wrapping the preview block's PreferencesGroup:
    /// the block animates in/out instead of snapping.
    group_revealer: gtk4::Revealer,
    name: adw::EntryRow,
    revert: gtk4::Button,
    /// Media-format selector, filled per video on resolve: exact pinnable
    /// formats, tallest first (the preference preselects the closest row),
    /// a single Automatic row when nothing is pinnable, and always an
    /// Audio only row — the whole format decision lives in this one row.
    format: adw::ComboRow,
    /// Index-aligned with the combo's model. Reset on every resolve.
    options: Rc<RefCell<Vec<FormatOption>>>,
    tools: adw::ActionRow,
    error: adw::ActionRow,
    /// The form's Add button: icon-only until a format is picked, then a
    /// labeled "Add" pill (HIG: primary actions carry a text label).
    add_btn: gtk4::Button,
}

/// Reserve trailing text space inside the URL entry while the lookup
fn hide_video_step(v: &VideoStep) {
    // Slide the preview block closed; the child-revealed handler hides the
    // revealer once the animation finishes, so no dead spacing remains.
    v.group_revealer.set_reveal_child(false);
    // NOTE: the action revealer (Add button / spinner) is NOT touched here:
    // its visibility is owned by the submit/lookup flow (show_video_loading
    // reveals, close_card hides). Hiding it here would break show_video_ready,
    // which resets the step before showing the Add button.
    // Back to the Add button: the action slot keeps its width, so selecting
    // a page never reallocates the row. The spinner page is unmapped while
    // hidden, which stops its animation (no set_spinning on adw::Spinner).
    v.action_slot.set_visible_child_name("add");
    // Clear the accessible name so a screen reader doesn't re-read the
    // stale "Looking up…" set by show_video_loading.
    v.url_spinner
        .upcast_ref::<gtk4::Widget>()
        .update_property(&[gtk4::accessible::Property::Label("")]);
    v.name.set_visible(false);
    v.revert.set_visible(false);
    v.format.set_visible(false);
    v.tools.set_visible(false);
    v.error.set_visible(false);
    // No format picked in these states: back to the icon-only button.
    v.add_btn.set_label("");
    v.add_btn.set_icon_name("object-select-symbolic");
    v.add_btn.add_css_class("circular");
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
    // Name row and the tools/error subtitles keep their last text when
    // only hidden; clear them so nothing stale survives.
    step.name.set_text("");
    step.tools.set_subtitle("");
    step.error.set_subtitle("");
    hide_video_step(step);
    // Slide the action slot out: card is closing, gear + X slide back.
    step.action_revealer.set_reveal_child(false);
}

fn show_video_loading(v: &VideoStep) {
    hide_video_step(v);
    // Slide the action slot in (Add button / spinner pushes gear + X right),
    // then swap to the spinner: same reserved width, no shift.
    v.action_revealer.set_visible(true);
    v.action_revealer.set_reveal_child(true);
    v.action_slot.set_visible_child_name("spinner");
    // Screen-reader announcement: the spinner alone is silent.
    // `adw::Spinner` doesn't expose `update_property` directly; upcast to Widget.
    // A static label alone is never spoken: `announce` voices the state change.
    // (gtk-rs names GTK's polite tier `Medium`; there is no `Polite` variant.)
    let spinner = v.url_spinner.upcast_ref::<gtk4::Widget>();
    let looking_up = gettext("Looking up…");
    spinner.update_property(&[gtk4::accessible::Property::Label(&looking_up)]);
    spinner.announce(&looking_up, gtk4::AccessibleAnnouncementPriority::Medium);
}

fn show_video_ready(v: &VideoStep) {
    hide_video_step(v);
    // Ensure the action slot is visible: the fresh-preview path reaches here
    // without a show_video_loading (no new lookup to reveal it).
    v.action_revealer.set_visible(true);
    v.action_revealer.set_reveal_child(true);
    v.name.set_visible(true);
    v.revert.set_visible(true);
    v.format.set_visible(true);
    // Reveal after the rows are shown: set_visible(true) first so the
    // SlideDown has a mapped widget to animate (see slide_down_revealer).
    v.group_revealer.set_visible(true);
    v.group_revealer.set_reveal_child(true);
    // Format picked: the Add button earns its text label.
    v.add_btn.set_icon_name("");
    v.add_btn.remove_css_class("circular");
    v.add_btn.set_label(&gettext("Add"));
}

fn show_video_tools_missing(v: &VideoStep, message: &str) {
    hide_video_step(v);
    // Lookup failed: nothing to add. Slide the action slot out (the
    // child-revealed handler hides it once the animation finishes) — the
    // entry's tick stays for retry.
    v.action_revealer.set_reveal_child(false);
    v.tools.set_subtitle(message);
    v.tools.set_visible(true);
    // Reveal after the row is shown: set_visible(true) first so the
    // SlideDown has a mapped widget to animate (see slide_down_revealer).
    v.group_revealer.set_visible(true);
    v.group_revealer.set_reveal_child(true);
}

fn show_video_error(v: &VideoStep, message: &str) {
    hide_video_step(v);
    // Lookup failed: nothing to add. Slide the action slot out (the
    // child-revealed handler hides it once the animation finishes) — the
    // entry's tick stays for retry.
    v.action_revealer.set_reveal_child(false);
    v.error.set_subtitle(message);
    v.error.set_visible(true);
    // Reveal after the row is shown: set_visible(true) first so the
    // SlideDown has a mapped widget to animate (see slide_down_revealer).
    v.group_revealer.set_visible(true);
    v.group_revealer.set_reveal_child(true);
}

/// Desensitize the form's Add button while a lookup is in flight (a dead
/// button says so upfront). Every terminal state re-enables it.
fn set_lookup_add(cell: &Rc<RefCell<Option<gtk4::Button>>>, enabled: bool) {
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
            audio_only: false,
        });
    }
    if options.is_empty() {
        options.push(FormatOption {
            label: gettext("Automatic"),
            format_id: None,
            audio_only: false,
        });
    }
    options.push(FormatOption {
        label: gettext("Audio only"),
        format_id: None,
        audio_only: true,
    });
    let labels: Vec<&str> = options.iter().map(|o| o.label.as_str()).collect();
    step.format.set_model(Some(&gtk4::StringList::new(&labels)));
    // Options before selection: set_selected fires notify::selected, and the
    // handler reads the options vec.
    *step.options.borrow_mut() = options;
    // default_quality_index is over info.formats; options may be just
    // [Automatic] when nothing is pinnable, so clamp.
    let index = crate::video::default_quality_index(&info.formats, preferred)
        .min(step.options.borrow().len().saturating_sub(1));
    step.format.set_selected(index as u32);
}

/// Whether the combo's current pick is the Audio only row.
fn selected_audio_only(step: &VideoStep) -> bool {
    let selected = step.format.selected() as usize;
    step.options
        .borrow()
        .get(selected)
        .map(|o| o.audio_only)
        .unwrap_or(false)
}

#[allow(clippy::too_many_arguments)]
fn submit_probed_single(
    manager: &Rc<DownloadManager>,
    dest: &Rc<RefCell<String>>,
    close_card: &Rc<dyn Fn()>,
    step: &Rc<VideoStep>,
    lookup_add: &Rc<RefCell<Option<gtk4::Button>>>,
    v: &crate::video::VideoInfo,
    scheduled_at: Option<i64>,
) {
    let typed = step.name.text().trim().to_string();
    let audio_only = selected_audio_only(step);
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
        manager,
        dest,
        close_card,
        scheduled_at,
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
    scheduled_at: Option<i64>,
) {
    match probe {
        crate::video::ProbeResult::Single(v) => {
            submit_probed_single(
                manager,
                dest,
                close_card,
                step,
                lookup_add,
                &v,
                scheduled_at,
            );
        }
        crate::video::ProbeResult::Playlist(pl) => {
            // Collections queue through the item picker: one row per chosen
            // entry, each re-resolving its own page at download time. No
            // format choice here — pins don't apply across items.
            push_playlist_items_page(
                nav,
                manager.clone(),
                dest.clone(),
                close_card.clone(),
                pl,
                scheduled_at,
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
/// Apply a scheduled timestamp to a freshly enqueued item. Future timestamps
/// become Scheduled; past ones (the 12:00 default is often stale by Add time)
/// queue immediately, mirroring restore_existing's overdue handling.
fn apply_scheduled_at(item: &DownloadItem, scheduled_at: Option<i64>) {
    if let Some(ts) = scheduled_at {
        let now = glib::DateTime::now_local()
            .map(|dt| dt.to_unix())
            .unwrap_or(0);
        if ts > now {
            item.set_scheduled_at(ts);
            item.set_status(DownloadStatus::Scheduled);
        }
    }
}

/// The destination borrow lives only for the enqueue call: a `dest.borrow()`
/// temporary in a `match` scrutinee would live into the arms, and
/// `close_card()` re-borrows the same cell mutably to reset the destination —
/// panicking with "RefCell already borrowed" on every successful Add.
fn enqueue_and_close(
    manager: &Rc<DownloadManager>,
    dest: &Rc<RefCell<String>>,
    close_card: &Rc<dyn Fn()>,
    scheduled_at: Option<i64>,
    enqueue: impl FnOnce(Option<&str>) -> Result<DownloadItem, String>,
    on_err: impl FnOnce(&str),
) {
    // Batch guard defers start_next until the Scheduled status is set:
    // without it, the still-Queued item would spawn immediately.
    let _guard = manager.batch_guard();
    let result = {
        let d = dest.borrow();
        enqueue(Some(&d))
    };
    match result {
        Ok(item) => {
            apply_scheduled_at(&item, scheduled_at);
            close_card()
        }
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
    scheduled_at: Option<i64>,
) -> Result<(), String> {
    // Batch guard defers start_next until the Scheduled status is set.
    let _guard = manager.batch_guard();
    let typed = file_row.text().trim().to_string();
    let name = (!typed.is_empty()).then_some(typed);
    let item = manager.enqueue(url, Some(&dest.borrow()), name.as_deref())?;
    apply_scheduled_at(&item, scheduled_at);
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
            // emit_clicked bypasses sensitivity; skip while the import is in flight.
            if add_btn.is_sensitive() {
                add_btn.emit_clicked();
            }
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

/// A picker grid: `GtkFlowBox` as the wrapping container, cells as toggle
/// pills with the simple Button API. All entries start active, matching the
/// old checked-by-default rows. Returns the box and the pills for the caller
/// to wire.
///
/// `homogeneous(true)` gives every pill an equal share of the row width, so
/// full rows fill the available space instead of leaving ragged gaps. A
/// partial final row keeps its empty slots (stock `GtkFlowBox` behavior —
/// its allocator reuses the same cell size for every row); stretching the
/// leftovers would need a custom layout manager.
fn picker_list(entries: Rc<Vec<(String, String)>>) -> (gtk4::FlowBox, Vec<gtk4::ToggleButton>) {
    // Flowing grid of toggle pills: the simple Button API
    // (set_active/is_active/toggled), native selected styling, no selection
    // model. FlowBox is only the wrapping container.
    let flowbox = gtk4::FlowBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .homogeneous(true)
        // Four-column cap: without it homogeneous pills flow to the toolkit
        // default on wide cards. Min stays default so narrow cards degrade.
        .max_children_per_line(4)
        .column_spacing(12)
        .row_spacing(12)
        .valign(gtk4::Align::Start)
        .build();
    let mut buttons = Vec::with_capacity(entries.len());
    for (title, _) in entries.iter() {
        // Titles are untrusted (video titles carry `&`, `<`, …): Button label
        // is plain text, never markup.
        let btn = gtk4::ToggleButton::with_label(title);
        btn.set_active(true);
        // Overlong titles ellipsize instead of forcing the FlowBox into
        // degenerate width measurements (gtk_widget_measure for_size
        // criticals): the pill keeps a sane minimum width. The char cap is
        // what makes homogeneous sizing + ellipsis interact — without it one
        // overlong title stretches every pill and ellipsis never fires.
        if let Some(label) = btn.child().and_downcast::<gtk4::Label>() {
            label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            label.set_max_width_chars(24);
        }
        flowbox.append(&btn);
        buttons.push(btn);
    }
    (flowbox, buttons)
}

/// Wire the pickers' bottom action bar to the toggle pills: the action counts
/// the live selection, Select All/None drive the buttons.
fn wire_list_selection_bar(
    buttons: &[gtk4::ToggleButton],
    select_all_btn: &gtk4::Button,
    select_none_btn: &gtk4::Button,
    add_btn: &gtk4::Button,
    count_label: impl Fn(usize) -> String + 'static,
) {
    let buttons = buttons.to_vec();
    let refresh = Rc::new({
        let buttons = buttons.clone();
        let add_btn = add_btn.clone();
        move || {
            let n = buttons.iter().filter(|b| b.is_active()).count();
            add_btn.set_label(&count_label(n));
        }
    });
    for btn in &buttons {
        let refresh = Rc::clone(&refresh);
        btn.connect_toggled(move |_| refresh());
    }
    refresh();
    {
        let buttons = buttons.clone();
        select_all_btn.connect_clicked(move |_| {
            for b in &buttons {
                b.set_active(true);
            }
        });
    }
    {
        let buttons = buttons.clone();
        select_none_btn.connect_clicked(move |_| {
            for b in &buttons {
                b.set_active(false);
            }
        });
    }
}

/// Selected indices of picker toggle pills, ascending.
fn list_selected(buttons: &[gtk4::ToggleButton]) -> Vec<usize> {
    buttons
        .iter()
        .enumerate()
        .filter(|(_, b)| b.is_active())
        .map(|(i, _)| i)
        .collect()
}

fn push_playlist_items_page(
    nav: &adw::NavigationView,
    manager: Rc<DownloadManager>,
    dest_dir: Rc<RefCell<String>>,
    close_card: Rc<dyn Fn()>,
    playlist: crate::media_types::PlaylistInfo,
    scheduled_at: Option<i64>,
) {
    // Same guard as the video step: don't stack a second picker while one is
    // already visible.
    if nav.visible_page_tag().as_deref() == Some("playlist") {
        return;
    }

    let count = playlist.items.len();
    // Title over a dimmed duration. The truncation
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
    let (flowbox, picks) = picker_list(Rc::clone(&entries));

    let list_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(6)
        .build();
    // HIG padding between the grid and the scrolled viewport edges.
    list_box.set_margin_top(12);
    list_box.set_margin_bottom(12);
    list_box.set_margin_start(12);
    list_box.set_margin_end(12);
    if crate::video_probe::playlist_truncated(&playlist) {
        let notice = gtk4::Label::builder()
            .label(
                gettext("Showing the first {n} of {total}")
                    .replace("{n}", &count.to_string())
                    .replace("{total}", &playlist.total.to_string()),
            )
            .css_classes(["dimmed", "caption"])
            .halign(gtk4::Align::Start)
            .build();
        list_box.append(&notice);
    }
    list_box.append(&flowbox);
    // The error caption lives under the list, like the old row list.
    let error_caption = gtk4::Label::builder()
        .label("")
        .css_classes(["error", "caption"])
        .halign(gtk4::Align::Start)
        .visible(false)
        .build();
    list_box.append(&error_caption);

    // Scrolled: big playlists must not size the card off-screen, but the capped
    // natural height lets it grow and shrink with the item count. valign=Start
    // (not the Fill default): the ToolbarView would otherwise stretch the
    // scrolled window to the full content height, leaving empty space below
    // a short list.
    let scrolled = gtk4::ScrolledWindow::builder()
        .child(&list_box)
        .propagate_natural_height(true)
        .max_content_height(480)
        .vexpand(false)
        .valign(gtk4::Align::Start)
        .hscrollbar_policy(gtk4::PolicyType::Never)
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

    // The action counts the live selection (see `wire_list_selection_bar`).
    wire_list_selection_bar(&picks, &select_all_btn, &select_none_btn, &add_btn, |n| {
        ngettext("_Queue {} item", "_Queue {} items", n as u32).replace("{}", &n.to_string())
    });

    {
        let close_card = close_card.clone();
        let picks = picks.clone();
        let add_btn_click = add_btn.clone();
        add_btn_click.connect_clicked(move |_| {
            // Guard against re-entry: Enter emits clicked directly, bypassing
            // the sensitivity check, so a second press while the import is in
            // flight would enqueue duplicates.
            if !add_btn.is_sensitive() {
                return;
            }
            let picked: Vec<usize> = list_selected(&picks);
            let picked_set: std::collections::HashSet<usize> = picked.into_iter().collect();
            // Owned clones for the async import below.
            let chosen: Vec<(usize, crate::media_types::PlaylistItem)> = playlist
                .items
                .iter()
                .enumerate()
                .filter(|(i, _)| picked_set.contains(i))
                .map(|(i, item)| (i, item.clone()))
                .collect();
            if chosen.is_empty() {
                error_caption.set_text(&gettext("Select at least one item"));
                error_caption.set_visible(true);
                return;
            }
            // Multiple items from one collection share a titled subfolder,
            // torrent-style; a lone item keeps the flat behavior. The base
            // dir is pure path manipulation; the subdir creation and readdir
            // can stall on network mounts, so they run off the GTK thread.
            let base_dir = manager.resolve_dir(Some(&dest_dir.borrow()));
            let multi = chosen.len() > 1;
            let title = playlist.title.clone();
            // Disable the button while the import is in flight: a second
            // click would see the picks still active and enqueue duplicates.
            add_btn.set_sensitive(false);
            let manager = manager.clone();
            let picks = picks.clone();
            let error_caption = error_caption.clone();
            let close_card = close_card.clone();
            let page_url = playlist.page_url.clone();
            let add_btn = add_btn.clone();
            glib::spawn_future_local(async move {
                // One blocking call for subdir creation + readdir.
                let (dir, existing) = match gio::spawn_blocking(move || {
                    let dir = if multi {
                        crate::file_names::collection_subdir(&base_dir, &title)
                    } else {
                        base_dir
                    };
                    let existing = crate::video_staging::dir_file_names(std::path::Path::new(&dir));
                    (dir, existing)
                })
                .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("playlist import dir setup failed: {e:?}");
                        error_caption.set_text(&gettext("Could not prepare the download folder"));
                        error_caption.set_visible(true);
                        add_btn.set_sensitive(true);
                        return;
                    }
                };
                // One persist for the whole import, not one per row. Taken
                // after the await: holding it across the readdir would suppress
                // every other persist/UI refresh/scheduler kick in the app.
                let _batch = manager.batch_guard();
                // Story segments are addressable as their own pages: queue those so each row
                // re-resolves its own segment instead of the tray (tray + format ids would
                // download the first segment once per row). Attempted unconditionally:
                // highlights and non-story URLs return None and keep the tray.
                let mut failed: Option<String> = None;
                for (i, item) in &chosen {
                    let item_page_url = crate::video_probe::story_segment_url(&page_url, &item.id)
                        .unwrap_or_else(|| item.page_url.clone());
                    let settings = manager.settings();
                    let name = default_name_for(settings, &item.title, false);
                    match manager.enqueue_video_staged(
                        &item_page_url,
                        &dir,
                        Some(&name),
                        crate::media_types::VideoChoices {
                            quality: manager.settings().video_quality(),
                            audio_only: false,
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
                        Ok(enqueued) => apply_scheduled_at(&enqueued, scheduled_at),
                        Err(e) => {
                            failed = Some(e);
                            break;
                        }
                    }
                    // Rows already queued stay queued on a partial failure: unselect
                    // them so a retry submits only the remainder (dedupe is by
                    // filename).
                    picks[*i].set_active(false);
                }
                if let Some(e) = failed {
                    error_caption.set_text(&e);
                    error_caption.set_visible(true);
                    add_btn.set_sensitive(true);
                    return;
                }
                // Complete success collapses the whole New Download card; a partial failure stays
                // on the picker so the remaining rows (unchecked above) can be retried.
                close_card();
            });
        });
    }

    nav.push(&picker_page);
}

/// Multi-file .torrent intake as a right-sliding card page: one switch per
/// file, all on by default. The selection feeds rqbit's `only_files` at add
/// time (no live setter), so it must be chosen here.
#[allow(clippy::too_many_arguments)]
fn push_torrent_picker_page(
    nav: &adw::NavigationView,
    manager: Rc<DownloadManager>,
    dest_dir: Rc<RefCell<String>>,
    close_card: Rc<dyn Fn()>,
    file_name: String,
    bytes: Vec<u8>,
    entries: Vec<crate::torrent::TorrentFileEntry>,
    scheduled_at: Rc<Cell<Option<i64>>>,
) {
    if nav.visible_page_tag().as_deref() == Some("torrent") {
        return;
    }

    // The file count lives in the compact header; the group needs no title.
    let file_count = ngettext("{} file", "{} files", entries.len() as u32)
        .replace("{}", &entries.len().to_string());
    // Like the playlist picker: path over a dimmed size.
    let entry_count = entries.len();
    let list_entries: Rc<Vec<(String, String)>> = Rc::new(
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
    let (flowbox, picks) = picker_list(list_entries);

    let list_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(6)
        .build();
    // HIG padding between the grid and the scrolled viewport edges.
    list_box.set_margin_top(12);
    list_box.set_margin_bottom(12);
    list_box.set_margin_start(12);
    list_box.set_margin_end(12);
    list_box.append(&flowbox);
    let error_caption = gtk4::Label::builder()
        .label("")
        .css_classes(["error", "caption"])
        .halign(gtk4::Align::Start)
        .visible(false)
        .build();
    list_box.append(&error_caption);

    // Same capped scrolled window as the playlist picker: big torrents must
    // not size the card off-screen. valign=Start (not the Fill default): the
    // ToolbarView would otherwise stretch the scrolled window to the full
    // content height, leaving empty space below a short list.
    let scrolled = gtk4::ScrolledWindow::builder()
        .child(&list_box)
        .propagate_natural_height(true)
        .max_content_height(480)
        .vexpand(false)
        .valign(gtk4::Align::Start)
        .hscrollbar_policy(gtk4::PolicyType::Never)
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
    wire_list_selection_bar(&picks, &select_all_btn, &select_none_btn, &add_btn, |n| {
        ngettext("_Add {} file", "_Add {} files", n as u32).replace("{}", &n.to_string())
    });

    {
        let close_card = close_card.clone();
        let picks = picks.clone();
        add_btn.connect_clicked(move |_| {
            let selected: Vec<usize> = list_selected(&picks);
            if selected.is_empty() {
                error_caption.set_text(&gettext("Select at least one file"));
                error_caption.set_visible(true);
                return;
            }
            // All on means no filter: pass None, not every index.
            let only = (selected.len() < entry_count).then_some(selected);
            enqueue_and_close(
                &manager,
                &dest_dir,
                &close_card,
                scheduled_at.get(),
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
    torrent_row: adw::ActionRow,
    scheduled_at: Rc<Cell<Option<i64>>>,
) {
    let nav = nav.clone();
    let scheduled_at_c = Rc::clone(&scheduled_at);
    torrent_btn.connect_clicked(move |_| {
        let m = manager.clone();
        let dd = dest_dir.clone();
        let close_card = close_card.clone();
        let nav = nav.clone();
        let torrent_row = torrent_row.clone();
        let scheduled_at = Rc::clone(&scheduled_at_c);
        // A fresh pick clears the previous row-level error.
        clear_field_error(&torrent_row);
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
                    set_field_error(&torrent_row, &gettext("Could not read that .torrent file"));
                    return;
                }
            };
            let (_tname, entries) = match crate::torrent::torrent_file_list(&bytes) {
                Ok(v) => v,
                Err(e) => {
                    set_field_error(&torrent_row, &e);
                    return;
                }
            };
            if entries.len() <= 1 {
                enqueue_and_close(
                    &m,
                    &dd,
                    &close_card,
                    scheduled_at.get(),
                    |d| m.enqueue_torrent_file(bytes, &name, d, None),
                    |e| {
                        set_field_error(&torrent_row, e);
                    },
                );
                return;
            }
            push_torrent_picker_page(&nav, m, dd, close_card, name, bytes, entries, scheduled_at);
        });
    });
}

/// Validation error state lives on the field itself: the error class +
/// tooltip for sighted users, and an accessible description so screen readers
/// announce it too (tooltips are never announced). Set and cleared together —
/// a stale description must never outlive the visible error.
fn set_field_error(field: &impl IsA<gtk4::Widget>, message: &str) {
    let field = field.upcast_ref::<gtk4::Widget>();
    field.add_css_class("error");
    field.set_tooltip_text(Some(message));
    field.update_property(&[gtk4::accessible::Property::Description(message)]);
}

fn clear_field_error(field: &impl IsA<gtk4::Widget>) {
    let field = field.upcast_ref::<gtk4::Widget>();
    field.remove_css_class("error");
    field.set_tooltip_text(None);
    field.update_property(&[gtk4::accessible::Property::Description("")]);
}

fn show_video_playlist(v: &VideoStep, _pl: &crate::media_types::PlaylistInfo) {
    // Playlists carry no format choice (pins don't apply across items), so
    // the preview block stays hidden — but the probe resolved, so Add earns
    // its label like a single video; tapping it opens the title picker.
    hide_video_step(v);
    v.action_revealer.set_visible(true);
    v.action_revealer.set_reveal_child(true);
    v.add_btn.set_icon_name("");
    v.add_btn.remove_css_class("circular");
    v.add_btn.set_label(&gettext("Add"));
}

/// The New Download card's open state, decoupled from the widgets: every
/// flip notifies one listener, so the header `+` toggle mirrors closes
/// from Escape, a successful add, or the toggle itself — not just its own
/// clicks. Widget-free so the notify-on-flip protocol is unit-testable
/// (widget creation segfaults headless, so CI can't cover it there).
/// Fired on every open-state flip with the new state.
type OnFlip = Rc<dyn Fn(bool)>;

#[derive(Default)]
struct OpenState {
    open: Cell<bool>,
    on_flip: RefCell<Option<OnFlip>>,
}

impl OpenState {
    /// Set the state; the listener fires only on an actual flip, so a
    /// redundant open (focus grab on an already-open card) stays silent.
    fn set(&self, open: bool) {
        if self.open.get() == open {
            return;
        }
        self.open.set(open);
        if let Some(cb) = self.on_flip.borrow().as_ref() {
            cb(open);
        }
    }

    fn is_open(&self) -> bool {
        self.open.get()
    }

    /// The single state mirror (the header toggle). Replaces any previous.
    fn set_on_flip(&self, cb: impl Fn(bool) + 'static) {
        *self.on_flip.borrow_mut() = Some(Rc::new(cb));
    }
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
    state: Rc<OpenState>,
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
    /// open one (which resets it, like the header toggle/Escape).
    pub fn toggle(&self) {
        (self.toggle)()
    }

    /// Whether the card is currently revealed.
    pub fn is_open(&self) -> bool {
        self.state.is_open()
    }

    /// Mirror the card's open state onto the header `+` toggle: fires on
    /// every flip, whichever path caused it.
    pub fn set_on_state_changed(&self, cb: impl Fn(bool) + 'static) {
        self.state.set_on_flip(cb);
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

/// A SlideDown revealer that owns its visibility: starts hidden, and hides
/// Itself once the slide-up finishes (a collapsed revealer keeps occupying
/// the parent box's spacing otherwise). Callers reveal with
/// `set_visible(true)` + `set_reveal_child(true)` and collapse with
/// `set_reveal_child(false)`; the child-revealed handler below does the
/// rest. Used by the options, preview-block, and schedule sections (vertical
/// slides) and the URL-row action slot (horizontal slide).
fn slide_revealer(transition: gtk4::RevealerTransitionType) -> gtk4::Revealer {
    let revealer = gtk4::Revealer::builder()
        .transition_type(transition)
        .reveal_child(false)
        .build();
    revealer.set_visible(false);
    revealer.connect_child_revealed_notify(|r| {
        r.set_visible(r.is_child_revealed());
    });
    revealer
}

/// Slide-down variant of [`slide_revealer`] for the vertical sections.
fn slide_down_revealer() -> gtk4::Revealer {
    slide_revealer(gtk4::RevealerTransitionType::SlideDown)
}

/// Build the inline New Download card. The returned [`AddCard`] owns the
/// widget and the open/toggle entry points; the card starts collapsed.
pub fn build_add_card(manager: Rc<DownloadManager>) -> AddCard {
    let open_state = Rc::new(OpenState::default());
    // Shared scheduled-download timestamp: set by the schedule picker UI,
    // read by every enqueue path. None = start immediately.
    let scheduled_at: Rc<Cell<Option<i64>>> = Rc::new(Cell::new(None));

    // Card chrome: a slide-down revealer so the card animates in under the
    // header; collapsed it takes no space.
    let revealer = gtk4::Revealer::builder()
        .transition_type(gtk4::RevealerTransitionType::SlideDown)
        .reveal_child(false)
        .build();
    let card = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    // 12px: matches the download list's original window margins.
    card.set_margin_top(12);
    card.set_margin_bottom(12);
    card.set_margin_start(12);
    card.set_margin_end(12);
    // No .card class: each AdwPreferencesGroup below renders as its own
    // flush card (rows touch the group's edge, 12px internal row margins
    // match the app's card padding). The 6px margins give the window
    // spacing.

    // No in-card title: the card only opens from explicit "New Download"
    // affordances (+, Ctrl+N, the empty-state pill), so restating it is
    // redundant. The navigation page below keeps the accessible name.
    // Dismissal is the header `+` toggle (now stateful) and Escape — the
    // card has no window controls, and a fourth URL-row action was noise
    // next to Add/options.

    let nav = adw::NavigationView::new();
    // HIG form sizing: cap the card at 600px on wide windows instead of
    // stretching the URL field with the window. AdwClamp hands the child
    // the full width below the threshold, so narrow windows are untouched.
    let clamp = adw::Clamp::builder().maximum_size(600).build();
    clamp.set_child(Some(&nav));
    card.append(&clamp);
    revealer.set_child(Some(&card));

    // Form page: URL row (entry + Add), the video preview block, then the
    // file / torrent / destination rows. Each AdwPreferencesGroup is its
    // own card; the 12px form spacing separates the cards (HIG).
    let form = gtk4::Box::new(gtk4::Orientation::Vertical, 12);

    // URL bar: plain GtkEntry (entry styling, 6px corners) with the action
    // buttons beside it — AdwEntryRow would render card styling (12px).
    // The tick lives inside the entry as the secondary icon (the established
    // GtkEntry icon API): it submits the URL for lookup. The Add button /
    // spinner slides in beside the entry after submit, via a GtkRevealer.
    let url_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    let url_entry = gtk4::Entry::builder()
        .placeholder_text(gettext("Paste a download link"))
        .hexpand(true)
        .build();
    url_entry.set_input_purpose(gtk4::InputPurpose::Url);
    // Tick icon inside the entry: appears only when there's text to submit
    // (the changed handler below adds/removes it). Icon-press submits like
    // Enter. Entry starts empty, so no icon initially.
    url_bar.append(&url_entry);
    // Action slot: a homogeneous GtkStack swapping the Add button with the
    // lookup spinner, wrapped in a revealer. The slot keeps the widest
    // child's width, so starting a lookup never reallocates the URL row.
    // The revealer slides the slot in from the left (SlideRight) after the
    // tick submits, pushing the gear and X buttons aside; it slides out on
    // enqueue or card close. The swap itself is instant: a slide transition
    // on every lookup would distract from the entry being read.
    let url_spinner = adw::Spinner::new();
    url_spinner.set_valign(gtk4::Align::Center);
    // Add button: appears only after the lookup finishes (or for direct
    // files, after the tick submits). Labeled pill once a format is picked;
    // icon-only checkmark otherwise. Colored rounded HIG button.
    let add_btn = gtk4::Button::builder()
        .icon_name("object-select-symbolic")
        .tooltip_text(gettext("Add download"))
        .css_classes(["suggested-action", "circular"])
        .valign(gtk4::Align::Center)
        .build();
    add_btn.update_property(&[gtk4::accessible::Property::Label(&gettext("Add download"))]);
    let action_slot = gtk4::Stack::builder()
        .hhomogeneous(true)
        .transition_type(gtk4::StackTransitionType::None)
        .valign(gtk4::Align::Center)
        .build();
    action_slot.add_named(&add_btn, Some("add"));
    action_slot.add_named(&url_spinner, Some("spinner"));
    // The revealer uses the shared slide helper: hidden (no spacing) until
    // revealed, and collapses back when the slide-out finishes.
    let action_revealer = slide_revealer(gtk4::RevealerTransitionType::SlideRight);
    action_revealer.set_child(Some(&action_slot));
    url_bar.append(&action_revealer);
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
    url_bar.append(&opts_toggle);
    // Dismissal X: closes the card (the header `+` toggle and Escape do the
    // same; the X is the in-row affordance).
    let cancel_btn = gtk4::Button::builder()
        .icon_name("window-close-symbolic")
        .css_classes(["flat", "circular"])
        .tooltip_text(gettext("Cancel"))
        .valign(gtk4::Align::Center)
        .build();
    cancel_btn.update_property(&[gtk4::accessible::Property::Label(&gettext("Cancel"))]);
    url_bar.append(&cancel_btn);
    form.append(&url_bar);

    // Download options live in a revealer directly under the URL row: the
    // card opens compact, one tap on the gear reveals file name, torrent,
    // and destination inline.
    let opts_revealer = slide_down_revealer();
    // Explicit handler (not a property binding): visible comes first so
    // the slide-down still animates — a reveal set while hidden would
    // just snap open — and the revealer is only hidden once the slide-up
    // finishes, since hiding it eagerly would kill that animation too.
    {
        let revealer = opts_revealer.clone();
        opts_toggle.connect_toggled(move |toggle| {
            let active = toggle.is_active();
            if active {
                revealer.set_visible(true);
            }
            revealer.set_reveal_child(active);
        });
    }
    form.append(&opts_revealer);

    // Video preview block: hidden until a lookup runs; exactly one state shows.
    // HIG AdwPreferencesGroup: title/description are built-in, rows get the
    // 12px internal margins natively.
    let video_group = adw::PreferencesGroup::new();
    let video_name = adw::EntryRow::builder().title(gettext("File name")).build();
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
    let video_tools = adw::ActionRow::builder()
        .title(gettext("Support tools"))
        // Subtitles carry raw tool errors: never parse them as Pango markup.
        .use_markup(false)
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
    // Probe-error state: an ActionRow in the group (4.4.4 pattern) — the
    // message goes in the subtitle, Retry is a suffix.
    // Subtitles carry raw tool errors: never parse them as Pango markup.
    let video_error = adw::ActionRow::builder()
        .title(gettext("Couldn't load the media preview"))
        .use_markup(false)
        .build();
    let video_retry_btn = gtk4::Button::builder()
        .label(gettext("Retry"))
        .valign(gtk4::Align::Center)
        .build();
    video_error.add_suffix(&video_retry_btn);
    video_error.set_visible(false);
    video_group.add(&video_error);
    // The preview block slides down like the options section: the revealer
    // owns show/hide.
    let video_revealer = slide_down_revealer();
    video_revealer.set_child(Some(&video_group));
    let step = Rc::new(VideoStep {
        action_slot,
        action_revealer: action_revealer.clone(),
        url_spinner: url_spinner.clone(),
        group_revealer: video_revealer.clone(),
        name: video_name,
        revert: video_revert_btn,
        format: video_format,
        options: Rc::new(RefCell::new(Vec::new())),
        tools: video_tools,
        error: video_error,
        add_btn: add_btn.clone(),
    });
    // Card-local choices: the format is initialized from Preferences (not
    // bound). Exact picks are per lookup, so nothing persists here.
    form.append(&video_revealer);

    // Download options: HIG AdwPreferencesGroup, no header — the rows
    // speak for themselves. Rows get the 12px internal margins natively.
    let group = adw::PreferencesGroup::new();
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

    // Schedule: switch row reveals date/time pickers. The timestamp is shared
    // with the enqueue paths via the `scheduled_at` cell defined at the top
    // of `build_add_card`.
    let schedule_switch = adw::SwitchRow::builder()
        .title(gettext("Schedule download"))
        .subtitle(gettext("Start at a specific time"))
        .build();
    group.add(&schedule_switch);

    // Revealed schedule rows live in their own PreferencesGroup: AdwActionRow
    // and AdwSpinRow must be placed in a GtkListBox (which PreferencesGroup
    // provides), not a plain GtkBox — Adwaita warns otherwise.
    let schedule_group = adw::PreferencesGroup::new();
    let schedule_revealer = slide_down_revealer();
    schedule_revealer.set_child(Some(&schedule_group));

    // Date picker: plain Button (matches the "Choose…" buttons) toggling a
    // popover with GtkCalendar (HIG: no text entry for dates). MenuButton was
    // tried but its dropdown arrow made the row visually inconsistent.
    let calendar = gtk4::Calendar::new();
    let date_popover = gtk4::Popover::new();
    date_popover.set_child(Some(&calendar));
    let date_btn = gtk4::Button::builder()
        .label(gettext("Choose date…"))
        .valign(gtk4::Align::Center)
        .build();
    {
        let popover = date_popover.clone();
        let btn = date_btn.clone();
        date_btn.connect_clicked(move |_| {
            popover.set_parent(&btn);
            popover.popup();
        });
    }
    let date_row = adw::ActionRow::builder().title(gettext("Date")).build();
    date_row.add_suffix(&date_btn);
    date_row.set_activatable_widget(Some(&date_btn));
    schedule_group.add(&date_row);

    // Time pickers: hour/minute spin rows (HIG: SpinRow for numbers).
    let hour_spin = adw::SpinRow::builder()
        .title(gettext("Hour"))
        .adjustment(&gtk4::Adjustment::new(12.0, 0.0, 23.0, 1.0, 5.0, 0.0))
        .build();
    let minute_spin = adw::SpinRow::builder()
        .title(gettext("Minute"))
        .adjustment(&gtk4::Adjustment::new(0.0, 0.0, 59.0, 1.0, 5.0, 0.0))
        .build();
    schedule_group.add(&hour_spin);
    schedule_group.add(&minute_spin);

    // Update the shared timestamp when date/time changes or the switch toggles.
    {
        let calendar_c = calendar.clone();
        let hour_spin_c = hour_spin.clone();
        let minute_spin_c = minute_spin.clone();
        let date_btn_c = date_btn.clone();
        let scheduled_at_c = Rc::clone(&scheduled_at);
        let update: Rc<dyn Fn()> = Rc::new(move || {
            let dt = calendar_c.date();
            let hour = hour_spin_c.value() as i32;
            let minute = minute_spin_c.value() as i32;
            // Build a local DateTime from the calendar date + spin time.
            if let Ok(scheduled) = glib::DateTime::from_local(
                dt.year(),
                dt.month(),
                dt.day_of_month(),
                hour,
                minute,
                0.0,
            ) {
                let ts = scheduled.to_unix();
                scheduled_at_c.set(Some(ts));
                // Untranslated format: translators must not touch `%` verbs,
                // and the label must show the picked time, not just the date.
                date_btn_c.set_label(&scheduled.format("%Y-%m-%d %H:%M").unwrap_or_default());
            }
        });
        {
            let update = Rc::clone(&update);
            let popover_c = date_popover.clone();
            calendar.connect_day_selected(move |_| {
                update();
                popover_c.popdown();
            });
        }
        {
            let update = Rc::clone(&update);
            hour_spin.connect_changed(move |_| update());
        }
        {
            let update = Rc::clone(&update);
            minute_spin.connect_changed(move |_| update());
        }
        let scheduled_at_c2 = Rc::clone(&scheduled_at);
        let schedule_revealer_c = schedule_revealer.clone();
        schedule_switch.connect_active_notify(move |sw| {
            let active = sw.is_active();
            if active {
                // Visible before revealing so the slide-down still animates.
                schedule_revealer_c.set_visible(true);
            }
            schedule_revealer_c.set_reveal_child(active);
            if active {
                update();
            } else {
                scheduled_at_c2.set(None);
            }
        });
    }
    // The schedule rows are their own card under the options: separate
    // PreferencesGroups get the 12px box spacing instead of touching.
    // Stays inside opts_revealer so the gear toggle collapses it together
    // with the other options.
    let opts_box = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    opts_box.append(&group);
    opts_box.append(&schedule_revealer);
    // The whole schedule section hides when the preference is off: a hidden
    // switch can't be toggled, so no new scheduled downloads can be created
    // while the scheduler is disabled. Weak settings ref: settings must not
    // keep the card alive.
    {
        let settings_w = manager.settings().downgrade();
        let switch_c = schedule_switch.clone();
        let revealer_c = schedule_revealer.clone();
        let scheduled_at_c = Rc::clone(&scheduled_at);
        let sync = Rc::new(move || {
            if let Some(s) = settings_w.upgrade() {
                let enabled = crate::settings::AppSettings::from(s).scheduled_downloads_enabled();
                switch_c.set_visible(enabled);
                // Only visible while the switch is on: a collapsed revealer
                // would otherwise leave a dead 12px gap under the card.
                revealer_c.set_visible(enabled && switch_c.is_active());
                if !enabled {
                    scheduled_at_c.set(None);
                    switch_c.set_active(false);
                }
            }
        });
        sync();
        let sync_c = Rc::clone(&sync);
        manager.settings().connect_changed(
            Some(crate::settings::key::ENABLE_SCHEDULED_DOWNLOADS),
            move |_, _| sync_c(),
        );
    }
    opts_revealer.set_child(Some(&opts_box));

    // Validation errors live on the fields themselves (error class +
    // tooltip + accessible description), so there is no error caption under
    // the URL bar: the form never shifts when an error appears or clears.
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
    let lookup_add: Rc<RefCell<Option<gtk4::Button>>> = Rc::new(RefCell::new(None));
    lookup_add.replace(Some(add_btn.clone()));
    let dest_dir = Rc::new(RefCell::new(manager.effective_download_dir()));

    // Collapse the card and reset the form to a fresh state, like closing the
    // old dialog: reopening always starts clean, including the destination.
    let close_card: Rc<dyn Fn()> = {
        let revealer = revealer.clone();
        let open_state = Rc::clone(&open_state);
        let probe = Rc::clone(&probe);
        let url_entry = url_entry.clone();
        let file_row = file_row.clone();
        let step = Rc::clone(&step);
        let torrent_row = torrent_row.clone();
        let lookup_add = Rc::clone(&lookup_add);
        let nav = nav.clone();
        let dest_dir = Rc::clone(&dest_dir);
        let dest_label = dest_label.clone();
        let opts_revealer = opts_revealer.clone();
        let default_dir = manager.effective_download_dir();
        let scheduled_at = Rc::clone(&scheduled_at);
        let schedule_switch = schedule_switch.clone();
        Rc::new(move || {
            open_state.set(false);
            // Cancel any in-flight probe and drop its state; the
            // generation bump discards the stale completion.
            probe.borrow_mut().reset();
            revealer.set_reveal_child(false);
            reset_video_step(&step);
            clear_field_error(&url_entry);
            clear_field_error(&torrent_row);
            set_lookup_add(&lookup_add, true);
            file_row.set_text("");
            dest_dir.replace(default_dir.clone());
            dest_label.set_text(&default_dir);
            // The options reopen collapsed with the default destination,
            // like every other row of the fresh form.
            opts_revealer.set_reveal_child(false);
            // Reset the schedule picker: a stale timestamp must not leak
            // into the next download.
            scheduled_at.set(None);
            schedule_switch.set_active(false);
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

    // X button dismisses the card, like the header toggle and Escape.
    {
        let close = Rc::clone(&close_card);
        cancel_btn.connect_clicked(move |_| close());
    }

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
        let scheduled_at_kick = Rc::clone(&scheduled_at);
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
                scheduled_at_b,
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
                scheduled_at_kick.clone(),
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
                    // A fresh playlist resolve hides the format picker (pins
                    // don't apply across items); a single shows it — unless
                    // auto-add is on, which submits single videos immediately
                    // with the pre-selected quality.
                    match probe_b.borrow().info.clone() {
                        Some(crate::video::ProbeResult::Playlist(pl)) => {
                            show_video_playlist(&step_b, &pl)
                        }
                        Some(crate::video::ProbeResult::Single(v)) => {
                            if settings_b.auto_add_downloads() {
                                submit_probed_single(
                                    &manager_b,
                                    &dest_b,
                                    &close_b,
                                    &step_b,
                                    &lookup_add_b,
                                    &v,
                                    scheduled_at_b.get(),
                                );
                            } else {
                                show_video_ready(&step_b);
                            }
                        }
                        None => show_video_ready(&step_b),
                    }
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
                            match queue_plain(
                                &manager_b,
                                &dest_b,
                                &close_b,
                                &file_b,
                                &url,
                                scheduled_at_b.get(),
                            ) {
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
                            match queue_plain(
                                &manager_b,
                                &dest_b,
                                &close_b,
                                &file_b,
                                &direct,
                                scheduled_at_b.get(),
                            ) {
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
                            match queue_plain(
                                &manager_b,
                                &dest_b,
                                &close_b,
                                &file_b,
                                &url,
                                scheduled_at_b.get(),
                            ) {
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
                                // Seed the file name once: an explicit name wins, else
                                // the title default. Never clobbers an edit here.
                                if step_b.name.text().trim().is_empty() {
                                    let typed = file_b.text().trim().to_string();
                                    let base = if typed.is_empty() {
                                        // Seeded before the format rebuild;
                                        // audio-only is always off by design
                                        // at seed time.
                                        default_name_for(&settings_b, &v.title, false)
                                    } else {
                                        typed
                                    };
                                    step_b.name.set_text(&base);
                                }
                                probe_b.borrow_mut().last_ok = url;
                                rebuild_format_options(&step_b, &v, &settings_b.video_quality());
                                // Store a copy for retry; auto-add submits
                                // immediately with the pre-selected quality.
                                probe_b.borrow_mut().info =
                                    Some(crate::video::ProbeResult::Single(v.clone()));
                                if settings_b.auto_add_downloads() {
                                    submit_probed_single(
                                        &manager_b,
                                        &dest_b,
                                        &close_b,
                                        &step_b,
                                        &lookup_add_b,
                                        &v,
                                        scheduled_at_b.get(),
                                    );
                                } else {
                                    show_video_ready(&step_b);
                                }
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
        let close_card = close_card.clone();
        let probe = Rc::clone(&probe);
        let step2 = step.clone();
        let kick = kick_video.clone();
        let lookup_add_submit = lookup_add.clone();
        let nav2 = nav.clone();
        let scheduled_at = Rc::clone(&scheduled_at);
        Rc::new(move |from_activate: bool| {
            let fail = |message: &str| {
                // Form-validation pattern: the error state lives on the
                // entry itself, not in a caption underneath it.
                set_field_error(&url_entry, message);
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
                        scheduled_at.get(),
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
                                scheduled_at.get(),
                            );
                        }
                        Some(crate::video::ProbeResult::Playlist(pl)) => {
                            push_playlist_items_page(
                                &nav2,
                                m.clone(),
                                dd.clone(),
                                close_card.clone(),
                                pl,
                                scheduled_at.get(),
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
                &m,
                &dd,
                &close_card,
                scheduled_at.get(),
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
    // Tick icon inside the entry submits like Enter.
    {
        let s = submit.clone();
        url_entry.connect_icon_press(move |_, pos| {
            if pos == gtk4::EntryIconPosition::Secondary {
                s(false);
            }
        });
    }

    // Sync the form skeleton while typing. The lookup itself never fires
    // on its own: pasting or editing only updates the form, and the resolve
    // starts when Add Download (or Enter) is pressed — unless auto-add is
    // on, in which case a pasted/typed URL auto-starts the lookup (debounced)
    // and the download follows automatically on resolve.
    {
        let probe = Rc::clone(&probe);
        let step2 = step.clone();
        let file_row2 = file_row.clone();
        let submit_auto = submit.clone();
        let settings_auto = manager.settings().clone();
        let debounce: Rc<std::cell::RefCell<Option<glib::SourceId>>> =
            Rc::new(std::cell::RefCell::new(None));
        // Only auto-trigger for URLs that look complete: a parseable
        // http(s) URL with a dotted host, or a magnet link. This avoids
        // firing on partial input while the user is still typing.
        let url_looks_complete = |text: &str| -> bool {
            if text.starts_with("magnet:?") {
                return true;
            }
            url::Url::parse(text).is_ok_and(|u| {
                matches!(u.scheme(), "http" | "https")
                    && u.host_str().is_some_and(|h| h.contains('.'))
            })
        };
        url_entry.connect_changed(move |row| {
            clear_field_error(row);
            let text = row.text().trim().to_string();
            // Auto-add: debounce the lookup so it fires after the user
            // pauses typing/pasting, not on every keystroke.
            if settings_auto.auto_add_downloads() && url_looks_complete(&text) {
                if let Some(id) = debounce.borrow_mut().take() {
                    id.remove();
                }
                let s = submit_auto.clone();
                let id =
                    glib::timeout_add_local(std::time::Duration::from_millis(800), move || {
                        s(true);
                        glib::ControlFlow::Break
                    });
                *debounce.borrow_mut() = Some(id);
            } else if let Some(id) = debounce.borrow_mut().take() {
                id.remove();
            }
            // Tick icon appears only when there's a URL to submit: add it
            // on first text, remove it when cleared.
            let has_text = !text.is_empty();
            let icon_shown = row.icon_name(gtk4::EntryIconPosition::Secondary).is_some();
            if has_text != icon_shown {
                if has_text {
                    row.set_icon_from_icon_name(
                        gtk4::EntryIconPosition::Secondary,
                        Some("object-select-symbolic"),
                    );
                    row.set_icon_tooltip_text(
                        gtk4::EntryIconPosition::Secondary,
                        Some(&gettext("Look up")),
                    );
                } else {
                    row.set_icon_from_icon_name(gtk4::EntryIconPosition::Secondary, None);
                }
            }
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
                // Editing the URL stales the preview: slide the Add button
                // out too (the tick re-submits for a fresh lookup).
                step2.action_revealer.set_reveal_child(false);
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
        torrent_row.clone(),
        Rc::clone(&scheduled_at),
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
        let (name, step_c, probe) = (step.name.clone(), Rc::clone(&step), Rc::clone(&probe));
        let settings = manager.settings().clone();
        step.revert.connect_clicked(move |_| {
            let st = probe.borrow();
            if let Some(p) = st.info.as_ref() {
                name.set_text(&default_name_for(
                    &settings,
                    p.title(),
                    selected_audio_only(&step_c),
                ));
                name.grab_focus();
            }
        });
    }
    // Changing the format pick re-seeds an untouched name: the resolve-time
    // seed ran under the other mode, so without this the row keeps a
    // video-container name for an audio download (or vice versa). An edited
    // name is never clobbered.
    {
        let (name, step_c, probe) = (step.name.clone(), Rc::clone(&step), Rc::clone(&probe));
        let settings = manager.settings().clone();
        step.format.connect_selected_notify(move |_| {
            let st = probe.borrow();
            if let Some(p) = st.info.as_ref() {
                let active = selected_audio_only(&step_c);
                let current = name.text().to_string();
                if current.trim().is_empty()
                    || current == default_name_for(&settings, p.title(), !active)
                {
                    name.set_text(&default_name_for(&settings, p.title(), active));
                }
            }
        });
    }

    // Escape collapses the card.
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

    // Open/toggle entry points.
    let reveal = {
        let revealer = revealer.clone();
        let open_state = Rc::clone(&open_state);
        Rc::new(move || {
            open_state.set(true);
            revealer.set_reveal_child(true);
        })
    };
    let open = {
        let reveal = Rc::clone(&reveal);
        let open_state = Rc::clone(&open_state);
        let url_entry = url_entry.clone();
        Rc::new(move |initial_url: Option<String>| {
            let already = open_state.is_open();
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
        let open_state = Rc::clone(&open_state);
        let close_card = Rc::clone(&close_card);
        Rc::new(move || {
            if open_state.is_open() {
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
        let scheduled_at = Rc::clone(&scheduled_at);
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
                    Rc::clone(&scheduled_at),
                );
            },
        )
    };

    AddCard {
        widget: revealer.upcast(),
        open,
        toggle,
        open_torrent_picker,
        state: Rc::clone(&open_state),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_probe() -> crate::video::ProbeResult {
        crate::video::ProbeResult::Single(crate::video::VideoInfo {
            title: "Test title".into(),
            duration: None,
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
        //
        // SAFETY: single-threaded setup phase (cargo runs with --test-threads=1).
        unsafe {
            std::env::set_var("GSETTINGS_SCHEMA_DIR", env!("GRAB_SCHEMA_DIR"));
            std::env::set_var("GSETTINGS_BACKEND", "memory");
        }
        let manager = Rc::new(DownloadManager::new(
            gio::ListStore::new::<DownloadItem>(),
            crate::settings::AppSettings::new(),
        ));
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
            &manager,
            &dest,
            &close,
            None,
            |d| {
                seen2.replace(d.unwrap_or("?").to_string());
                Ok::<DownloadItem, String>(DownloadItem::new(
                    1,
                    "https://example.com/f",
                    "f",
                    "/dl",
                ))
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

    /// The header `+` toggle mirrors the card through this protocol: every
    /// flip notifies exactly once, whatever path caused it (toggle click,
    /// Escape, successful enqueue).
    #[test]
    fn open_state_notifies_only_on_flip() {
        let st = OpenState::default();
        let seen = Rc::new(RefCell::new(Vec::new()));
        {
            let seen = Rc::clone(&seen);
            st.set_on_flip(move |open| seen.borrow_mut().push(open));
        }
        assert!(!st.is_open());
        st.set(true);
        assert!(st.is_open());
        // Redundant set: no second notification.
        st.set(true);
        st.set(false);
        assert!(!st.is_open());
        st.set(true);
        assert_eq!(*seen.borrow(), vec![true, false, true]);
    }
}
