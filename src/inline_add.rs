//! Inline New Download card: the whole add flow (link probing, the media
//! format selector, the playlist and torrent pickers) as a card pinned under
//! the header instead of a standalone dialog.
//!
//! The header `+` button (and the empty-state button, the `add-download`
//! action, and application-open URLs) reveal one card at the top of the main
//! window; the queue stays usable underneath. The card holds an
//! [`adw::NavigationView`]: the form page plus right-sliding playlist and
//! multi-file torrent picker pages (the gallery picker page is built in
//! inline_gallery.rs). Cancel, Escape, or a successful enqueue
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
use crate::gallery_grid::GalleryGrid;
use crate::window_rows::{
    apply_scheduled_at, default_name_for, picker_enter_confirms, picker_header,
    selection_action_bar,
};
use adw::prelude::*;
use gettextrs::{gettext, ngettext};
use glib::object::Cast;
use gtk4::prelude::*;
use gtk4::{gio, glib};
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

/// Tokio detaches a task when its JoinHandle is dropped. Own the abort handle
/// as well, so aborting the enclosing GLib lookup doesn't leave dual probes
/// running (and holding child processes) for a URL the user has already changed.
struct AbortTaskOnDrop(tokio::task::AbortHandle);

impl Drop for AbortTaskOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Where a submit came from: Enter, the Add button or tick icon, or auto-add.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SubmitSource {
    /// Enter in the URL entry.
    Enter,
    /// The Add button or the entry's tick icon.
    Button,
    /// The debounced submit that auto-add fires after a lookup.
    Auto,
}

/// Multi-file torrent picker opener: file name, raw bytes, parsed entries.
type TorrentPickerOpener = Rc<dyn Fn(String, Vec<u8>, Vec<crate::torrent::TorrentFileEntry>)>;

/// Lowercase hex SHA-256 of a pasted link: what the "paste once" check stores
/// instead of the link (dconf is plain text and often synced or backed up).
fn paste_fingerprint(url: &str) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(url.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Starts the next queued link once the finished lookup's marker is cleared.
/// The start is deferred to an idle callback so it never re-enters the task
/// that is still unwinding, and it is skipped while a ready preview or picker
/// waits on the user (the queued links stay queued instead of replacing it).
struct DrainPending {
    probe: Rc<RefCell<ProbeState>>,
    pending: Rc<RefCell<VecDeque<String>>>,
    start: Rc<dyn Fn(String)>,
}

impl Drop for DrainPending {
    fn drop(&mut self) {
        if self.pending.borrow().is_empty() {
            return;
        }
        let (probe, pending, start) = (
            Rc::clone(&self.probe),
            Rc::clone(&self.pending),
            Rc::clone(&self.start),
        );
        glib::idle_add_local_once(move || {
            let busy = {
                let st = probe.borrow();
                st.inflight.is_some() || st.info.is_some()
            };
            if busy {
                return;
            }
            let next = pending.borrow_mut().pop_front();
            if let Some(next) = next {
                start(next);
            }
        });
    }
}

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
pub(crate) struct ProbeState {
    generation: u64,
    inflight: Option<(String, u64, bool)>,
    last_ok: String,
    info: Option<crate::video::ProbeResult>,
}

impl ProbeState {
    /// Current generation: a probe started under an older generation is stale.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Start a resolve for `url`: returns the new generation, or `None`
    /// when a probe is already in flight (the user must wait for the
    /// current lookup to finish before starting a new one).
    fn kick(&mut self, url: String, probe_unlisted: bool) -> Option<u64> {
        if inflight_suppresses(&self.inflight, &url, self.generation, probe_unlisted) {
            return None;
        }
        let my = self.generation + 1;
        self.generation = my;
        self.inflight = Some((url, my, probe_unlisted));
        Some(my)
    }

    /// Twin check without starting: is a resolve already running?
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

/// Suppression: a probe is already in flight for the current generation. Any
/// new kick (same URL, different URL, or different probe mode) must wait —
/// a second probe would race the shared ProbeState and Add could grab the
/// first probe's title. A stale marker (older generation) never suppresses.
fn inflight_suppresses(
    marker: &Option<(String, u64, bool)>,
    _url: &str,
    generation: u64,
    _probe_unlisted: bool,
) -> bool {
    marker.as_ref().is_some_and(|(_, g, _)| *g == generation)
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

/// Seconds as M:SS / H:MM:SS. Test-only since subtitles were dropped from
/// playlist rows (window_tests keeps the formatting pinned).
#[cfg(test)]
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
pub(crate) struct FormatOption {
    /// Row title: the pin label ("1080p"), "Automatic", or "Audio only".
    label: String,
    /// Resolved yt-dlp format id; `None` for the Automatic and Audio only rows.
    format_id: Option<String>,
    /// True for the Audio only row.
    audio_only: bool,
}

/// The Video ⇄ Gallery switcher's Gallery side: the probe's items and the
/// URL they came from.
pub(crate) type ToggleItems = (Vec<crate::gallery_probe::GalleryItem>, String);

/// The video preview block inside the form: exactly one state shows at a
/// time (ready rows, tools row, or the centered error card), driven by the
/// probe below.
pub(crate) struct VideoStep {
    /// The URL entry itself: its secondary icon is the lookup state machine
    /// (tick → spinner → none), shared with the helpers below.
    pub(crate) url_entry: gtk4::Entry,
    /// Spinner paintable for the entry's secondary icon slot: GTK4 entries
    /// take paintables, not widgets, as icons. Animated by the entry's own
    /// frame clock (the established AdwSpinnerPaintable API).
    pub(crate) spinner_paintable: adw::SpinnerPaintable,
    /// A lookup is in flight for the current text. Any edit stales it (the
    /// probe generation bump discards its completion), so the changed
    /// handler clears this and the icon follows.
    pub(crate) looking_up: Cell<bool>,
    /// Revealer wrapping the Add button: it slides in from the left
    /// (SlideRight) once the lookup resolves, pushing the gear and X
    /// buttons aside; it slides out on enqueue or card close. The slide
    /// uses GtkRevealer, the established API — no hand-rolled animation.
    pub(crate) action_revealer: gtk4::Revealer,
    /// Slide-down revealer wrapping the preview block's PreferencesGroup:
    /// the block animates in/out instead of snapping.
    pub(crate) group_revealer: gtk4::Revealer,
    pub(crate) name: adw::EntryRow,
    pub(crate) revert: gtk4::Button,
    /// Media-format selector, filled per video on resolve: exact pinnable
    /// formats, tallest first (the preference preselects the closest row),
    /// a single Automatic row when nothing is pinnable, and always an
    /// Audio only row — the whole format decision lives in this one row.
    pub(crate) format: adw::ComboRow,
    /// Index-aligned with the combo's model. Reset on every resolve.
    pub(crate) options: Rc<RefCell<Vec<FormatOption>>>,
    pub(crate) tools: adw::ActionRow,
    pub(crate) error: adw::ActionRow,
    /// Gallery info row: shown when the probe finds an image gallery.
    /// Title is "Gallery", subtitle is "N files • preview...".
    pub(crate) gallery_info: adw::ActionRow,
    /// Gallery filter toggle (All/Images/Videos). Shown with gallery_info.
    pub(crate) gallery_filter: adw::ToggleGroup,
    /// Wrapper for the filter toggle (for visibility control).
    pub(crate) gallery_filter_wrap: gtk4::Box,
    /// Media-type switcher (Video ⇄ Gallery), the established AdwToggleGroup:
    /// shown only when both backends are viable for the current decode
    /// (yt-dlp video + gallery-dl images).
    pub(crate) mode_toggle: adw::ToggleGroup,
    /// Wrapper for the switcher (visibility control + accessible label).
    pub(crate) mode_toggle_wrap: gtk4::Box,
    /// Items backing the switcher's Gallery side: Some only while the choice
    /// is live. Cleared when a lookup starts and on reset.
    pub(crate) toggle_items: Rc<RefCell<Option<ToggleItems>>>,
    /// True when the current preview is a gallery (not a video).
    pub(crate) is_gallery: Rc<Cell<bool>>,
    /// The gallery card state (filter, items) when is_gallery is true.
    pub(crate) gallery_card: Rc<RefCell<Option<crate::gallery_card::GalleryCard>>>,
    /// "Select files…" button: opens the full-enumeration picker.
    /// Suffix of gallery_info; shows/hides with the row.
    pub(crate) gallery_select_btn: gtk4::Button,
    /// Opener for the gallery picker, set at probe completion (where nav,
    /// manager, dest, etc. are in scope). The button invokes it if set.
    /// The nested Rc/RefCell/Option is the wiring cost of connecting the
    /// button (wired at VideoStep construction) to context available later.
    #[allow(clippy::type_complexity)]
    pub(crate) gallery_select_opener: Rc<RefCell<Option<Rc<dyn Fn()>>>>,
    /// Abort handle for the in-flight gallery file enumeration (picker).
    /// Aborted when the card closes, a new lookup starts, or the opener is
    /// invoked again — killing gallery-dl instead of letting it probe on.
    pub(crate) enumerate_abort: Rc<RefCell<Option<tokio::task::AbortHandle>>>,
    /// Generation of the enumeration above: a completed-but-superseded task
    /// stays quiet instead of flashing "Could not list files".
    pub(crate) enumerate_gen: Cell<u64>,
    /// Handle of the in-flight video lookup task (`kick_video` spawns it).
    /// Aborted when a newer kick or an edit supersedes it, or the card
    /// closes: dropping the task kills its probe children (kill_on_drop)
    /// instead of letting them run out their timeouts on a dead URL.
    pub(crate) lookup_abort: Rc<RefCell<Option<glib::JoinHandle<()>>>>,
    /// The form's Add button: icon-only until a format is picked, then a
    /// labeled "Add" pill (HIG: primary actions carry a text label).
    pub(crate) add_btn: gtk4::Button,
}

/// The URL entry's secondary icon: exactly one state shows at a time.
/// The tick submits the URL; while looking up it morphs into a spinner in
/// place (browser-address-bar style); once the Add button appears beside
/// the entry — or the preview block shows an error/tools state with its
/// own recovery button — the entry carries no icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryIcon {
    Tick,
    Spinner,
    None,
}

/// Derive the entry icon from the card state. Pure so it's unit-testable
/// (widget creation segfaults headless, so CI can't cover the GTK calls).
fn entry_icon_for(has_text: bool, looking_up: bool, add_shown: bool, blocked: bool) -> EntryIcon {
    if looking_up {
        EntryIcon::Spinner
    } else if blocked {
        EntryIcon::None
    } else if has_text && !add_shown {
        EntryIcon::Tick
    } else {
        EntryIcon::None
    }
}

/// Records `text` as the latest entry text; false when it repeats. GTK emits
/// `changed` even when `set_text` writes the identical string (verified on
/// 4.24), so a programmatic write (handoff, queued drain) would otherwise read
/// as an edit: it wiped a resolved collection and re-armed auto-add, running
/// the lookup again.
fn text_is_new(last: &mut String, text: &str) -> bool {
    if *last == text {
        return false;
    }
    last.clear();
    last.push_str(text);
    true
}

/// True when `text` is a settled edit that invalidates a live lookup for
/// `last`. Empty is not decidable synchronously: GTK emits it transiently
/// while replacing text, so a real clear is handled by the 0 ms `apply`.
fn edit_invalidates_lookup(last: &str, text: &str, looking_up: bool) -> bool {
    looking_up && !text.is_empty() && text != last
}

/// Flip the lookup state and lock editing while it resolves. An accidental
/// keystroke would otherwise edit the URL, and any edit bumps the generation
/// and aborts the probe. `set_editable` (not `set_sensitive`) keeps the text
/// readable, selectable and copyable; the spinner cancels in place, and
/// Escape or the card's X still close.
fn set_looking_up(v: &VideoStep, looking_up: bool) {
    v.looking_up.set(looking_up);
    v.url_entry.set_editable(!looking_up);
}

/// Apply the derived icon state to the URL entry.
pub(crate) fn sync_entry_icon(v: &VideoStep) {
    let icon = entry_icon_for(
        !v.url_entry.text().trim().is_empty(),
        v.looking_up.get(),
        v.action_revealer.reveals_child(),
        v.error.is_visible() || v.tools.is_visible(),
    );
    let pos = gtk4::EntryIconPosition::Secondary;
    match icon {
        EntryIcon::Tick => {
            v.url_entry
                .set_icon_from_icon_name(pos, Some("object-select-symbolic"));
            v.url_entry.set_icon_activatable(pos, true);
            v.url_entry
                .set_icon_tooltip_text(pos, Some(&gettext("Look up")));
        }
        EntryIcon::Spinner => {
            v.url_entry
                .set_icon_from_paintable(pos, Some(&v.spinner_paintable));
            // The entry is locked while it resolves, so the spinner is the
            // cancel affordance (closing the card would lose the URL).
            v.url_entry.set_icon_activatable(pos, true);
            v.url_entry
                .set_icon_tooltip_text(pos, Some(&gettext("Cancel lookup")));
        }
        EntryIcon::None => {
            v.url_entry
                .set_icon_from_paintable(pos, None::<&gtk4::gdk::Paintable>);
            v.url_entry.set_icon_tooltip_text(pos, None);
        }
    }
}

pub(crate) fn hide_video_step(v: &VideoStep) {
    // Slide the preview block closed; the child-revealed handler hides the
    // revealer once the animation finishes, so no dead spacing remains.
    v.group_revealer.set_reveal_child(false);
    // NOTE: the action revealer (Add button) is NOT touched here: its
    // visibility is owned by the submit/lookup flow (show_video_ready
    // reveals, close_card hides). Hiding it here would break
    // show_video_ready, which resets the step before showing Add.
    // NOTE: the entry icon is NOT touched here either: the caller owns the
    // lookup state (show_video_loading sets the spinner, the terminal
    // states clear it) and sync_entry_icon derives the icon from it.
    v.name.set_visible(false);
    v.revert.set_visible(false);
    v.format.set_visible(false);
    v.tools.set_visible(false);
    v.error.set_visible(false);
    v.gallery_info.set_visible(false);
    v.gallery_filter_wrap.set_visible(false);
    v.mode_toggle_wrap.set_visible(false);
    v.is_gallery.set(false);
    v.gallery_card.borrow_mut().take();
    // NOTE: the Add button keeps its labeled-pill appearance while sliding
    // out: the revealer is only ever revealed after show_video_ready /
    // show_video_playlist set the label, so the icon-only form is never
    // visible — resetting it here would flash a blue tick mid-slide.
}

/// Abort the in-flight gallery file enumeration, if any. The generation
/// bump makes the superseded task stay quiet instead of flashing an error.
pub(crate) fn abort_enumeration(step: &VideoStep) {
    step.enumerate_gen
        .set(step.enumerate_gen.get().wrapping_add(1));
    if let Some(handle) = step.enumerate_abort.borrow_mut().take() {
        handle.abort();
    }
}

/// Abort the in-flight video lookup, if any: superseding it (a newer kick,
/// an edit that stales it, the card closing) drops the lookup task and its
/// probe children instead of letting them run out their timeouts. Safe from
/// inside the task (close paths): glib defers the source destruction until
/// the current dispatch returns.
pub(crate) fn abort_lookup(step: &VideoStep) {
    if let Some(handle) = step.lookup_abort.borrow_mut().take() {
        handle.abort();
    }
}

/// Clear the video preview block back to a pristine state: `close_card`
/// calls this so a non-video add after a video leaves no dead probe state
/// in memory. Hiding alone is not enough — the name row keeps its text,
/// the format rows keep their widgets, and the tools/error rows keep
/// their subtitles.
fn reset_video_step(step: &VideoStep) {
    // No enumeration or lookup survives a close: kill gallery-dl and the
    // probe children instead of letting them run on for a dead card.
    abort_enumeration(step);
    abort_lookup(step);
    // Drop the format options and the current pick.
    step.options.borrow_mut().clear();
    step.format.set_model(Some(&gtk4::StringList::new(&[])));
    // Name row and the tools/error subtitles keep their last text when
    // only hidden; clear them so nothing stale survives.
    step.name.set_text("");
    step.tools.set_subtitle("");
    step.error.set_subtitle("");
    hide_video_step(step);
    // The media-type choice does not survive a close either.
    arm_mode_toggle(step, None);
    // No lookup survives a close: clear the flag (and unlock the entry)
    // before the entry text is cleared below (its changed handler would
    // otherwise keep the spinner).
    set_looking_up(step, false);
    sync_entry_icon(step);
    // Slide the Add button out: card is closing, gear + X slide back.
    step.action_revealer.set_reveal_child(false);
}

/// Reset the Video ⇄ Gallery switcher and arm its Gallery side: `items` is
/// Some when the decode has both a yt-dlp video and gallery-dl images
/// (route() → VideoYtDlp { gallery_toggle: true }). The active name resets
/// to Video while the items are absent, so the notify that fires on the
/// reset finds an unarmed switcher and is ignored.
fn arm_mode_toggle(step: &VideoStep, items: Option<ToggleItems>) {
    step.toggle_items.borrow_mut().take();
    step.mode_toggle.set_active_name(Some("video"));
    if let Some(items) = items {
        *step.toggle_items.borrow_mut() = Some(items);
    }
}

fn show_video_loading(v: &VideoStep) {
    hide_video_step(v);
    // A new lookup supersedes any in-flight file enumeration: kill it.
    abort_enumeration(v);
    // A new lookup supersedes the previous media-type choice; the switcher
    // re-arms at completion when both backends are viable.
    v.toggle_items.borrow_mut().take();
    // The tick morphs into a spinner in place (browser-address-bar style):
    // no separate indicator beside the entry, no layout shift.
    set_looking_up(v, true);
    sync_entry_icon(v);
    // Screen-reader announcement: the spinner alone is silent. Announced on
    // the entry itself — no persistent label change, the entry keeps its name.
    // A static label alone is never spoken: `announce` voices the state change.
    // (gtk-rs names GTK's polite tier `Medium`; there is no `Polite` variant.)
    let looking_up = gettext("Looking up…");
    v.url_entry
        .announce(&looking_up, gtk4::AccessibleAnnouncementPriority::Medium);
}

fn show_video_ready(v: &VideoStep) {
    hide_video_step(v);
    set_looking_up(v, false);
    // Ensure the action revealer is visible: the fresh-preview path reaches
    // here without a show_video_loading (no new lookup to reveal it).
    v.action_revealer.set_visible(true);
    v.action_revealer.set_reveal_child(true);
    v.name.set_visible(true);
    v.revert.set_visible(true);
    v.format.set_visible(true);
    // The switcher shows while both sides exist for this decode.
    v.mode_toggle_wrap
        .set_visible(v.toggle_items.borrow().is_some());
    // Reveal after the rows are shown: set_visible(true) first so the
    // SlideDown has a mapped widget to animate (see slide_down_revealer).
    v.group_revealer.set_visible(true);
    v.group_revealer.set_reveal_child(true);
    // Format picked: the Add button earns its text label.
    v.add_btn.set_icon_name("");
    v.add_btn.remove_css_class("circular");
    v.add_btn.set_label(&gettext("Add"));
    sync_entry_icon(v);
}

fn show_video_tools_missing(v: &VideoStep, message: &str) {
    hide_video_step(v);
    set_looking_up(v, false);
    // Lookup failed: nothing to add. Slide the Add button out (the
    // child-revealed handler hides it once the animation finishes); the
    // entry carries no icon — Install in the tools row owns recovery.
    v.action_revealer.set_reveal_child(false);
    v.tools.set_subtitle(message);
    v.tools.set_visible(true);
    // Reveal after the row is shown: set_visible(true) first so the
    // SlideDown has a mapped widget to animate (see slide_down_revealer).
    v.group_revealer.set_visible(true);
    v.group_revealer.set_reveal_child(true);
    sync_entry_icon(v);
}

pub(crate) fn show_video_error(v: &VideoStep, message: &str) {
    hide_video_step(v);
    set_looking_up(v, false);
    // Lookup failed: nothing to add. Slide the Add button out (the
    // child-revealed handler hides it once the animation finishes); the
    // entry carries no icon — Retry in the error row owns recovery.
    v.action_revealer.set_reveal_child(false);
    v.error.set_subtitle(message);
    v.error.set_visible(true);
    // Reveal after the row is shown: set_visible(true) first so the
    // SlideDown has a mapped widget to animate (see slide_down_revealer).
    v.group_revealer.set_visible(true);
    v.group_revealer.set_reveal_child(true);
    sync_entry_icon(v);
}

/// Desensitize the form's Add button while a lookup is in flight (a dead
/// button says so upfront). Every terminal state re-enables it.
pub(crate) fn set_lookup_add(cell: &Rc<RefCell<Option<gtk4::Button>>>, enabled: bool) {
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
    let submitted_url = step.url_entry.text().to_string();
    if !crate::download_net::inherit_public_resolution(&submitted_url, &v.page_url) {
        show_video_error(
            step,
            &gettext("The resolved media page could not be verified as a public destination."),
        );
        set_lookup_add(lookup_add, true);
        return;
    }
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
            // Preserve the untrusted handoff policy if yt-dlp canonicalized
            // the collection URL to another hostname.
            let submitted_url = step.url_entry.text().to_string();
            if !crate::download_net::inherit_public_resolution(&submitted_url, &pl.page_url) {
                show_video_error(
                    step,
                    &gettext(
                        "The resolved collection could not be verified as a public destination.",
                    ),
                );
                set_lookup_add(lookup_add, true);
                return;
            }
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

/// The destination borrow lives only for the enqueue call: a `dest.borrow()`
/// temporary in a `match` scrutinee would live into the arms, and
/// `close_card()` re-borrows the same cell mutably to reset the destination —
/// panicking with "RefCell already borrowed" on every successful Add.
pub(crate) fn enqueue_and_close(
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
    url: &str,
    scheduled_at: Option<i64>,
) -> Result<(), String> {
    // Batch guard defers start_next until the Scheduled status is set.
    let _guard = manager.batch_guard();
    let item = manager.enqueue(url, Some(&dest.borrow()), None)?;
    apply_scheduled_at(&item, scheduled_at);
    close_card();
    Ok(())
}

fn push_playlist_items_page(
    nav: &adw::NavigationView,
    manager: Rc<DownloadManager>,
    dest_dir: Rc<RefCell<String>>,
    close_card: Rc<dyn Fn()>,
    playlist: crate::media_types::PlaylistInfo,
    scheduled_at: Option<i64>,
) {
    // Only the form opens a picker: auto-add can resolve a playlist while the
    // user is already in the gallery or torrent picker, and a second page
    // would stack on it.
    if !crate::window_rows::form_is_visible(nav) {
        return;
    }

    let count = playlist.items.len();
    // Titles; the truncation notice sits under the header as a dimmed
    // caption.
    let titles = playlist
        .items
        .iter()
        .map(|item| item.title.to_string())
        .collect::<Vec<_>>();
    let grid = GalleryGrid::new();
    grid.append(&titles);

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
    // Small selections use their natural height; long lists scroll within
    // the grid's capped viewport. Captions stay outside the scroll area.
    list_box.append(grid.widget());
    // The error caption lives under the list, like the old row list.
    let error_caption = gtk4::Label::builder()
        .label("")
        .css_classes(["error", "caption"])
        .halign(gtk4::Align::Start)
        .visible(false)
        .build();
    list_box.append(&error_caption);

    let toolbar = adw::ToolbarView::new();
    // Compact header instead of the auto AdwHeaderBar: back + title + count
    // on one tight row, no 48px of empty chrome around a centered title.
    let (header, _count_label) = picker_header(
        nav,
        &playlist.title,
        &playlist_count_label(playlist.kind, count),
    );
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&list_box));
    let (action_bar, select_all_btn, select_none_btn, add_btn) = selection_action_bar();
    toolbar.add_bottom_bar(&action_bar);
    let picker_page = adw::NavigationPage::builder()
        .tag("playlist")
        .title(&*playlist.title)
        .child(&toolbar)
        .build();
    picker_enter_confirms(&picker_page, &add_btn);

    // The action counts the live selection (see `GalleryGrid::wire_selection_bar`).
    grid.wire_selection_bar(&select_all_btn, &select_none_btn, &add_btn, |n| {
        ngettext("_Queue {} item", "_Queue {} items", n as u32).replace("{}", &n.to_string())
    });

    // Preserve this collection's accepted hosts across partial picker imports.
    // Otherwise repeatedly clicking Queue could reset the cap and eventually
    // register every host from a hostile playlist.
    let guarded_child_hosts = Rc::new(RefCell::new(std::collections::HashSet::new()));

    {
        let guarded_child_hosts = guarded_child_hosts.clone();
        let close_card = close_card.clone();
        let grid = grid.clone();
        let add_btn_click = add_btn.clone();
        add_btn_click.connect_clicked(move |_| {
            // Guard against re-entry: Enter emits clicked directly, bypassing
            // the sensitivity check, so a second press while the import is in
            // flight would enqueue duplicates.
            if !add_btn.is_sensitive() {
                return;
            }
            let picked: Vec<usize> = grid.selected_indices();
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
            // The folder belongs to the collection, so retain it even when
            // only one entry is selected from a multi-entry playlist.
            let multi = playlist.total > 1;
            let title = playlist.title.clone();
            // Disable the button while the import is in flight: a second
            // click would see the selection unchanged and enqueue duplicates.
            add_btn.set_sensitive(false);
            let manager = manager.clone();
            let grid = grid.clone();
            let error_caption = error_caption.clone();
            let close_card = close_card.clone();
            let page_url = playlist.page_url.clone();
            let add_btn = add_btn.clone();
            // The GTK clicked handler is an `Fn` and may run repeatedly;
            // clone the shared collection-host state for each import future.
            let guarded_child_hosts = guarded_child_hosts.clone();
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
                let mut skipped_by_host_limit = 0usize;
                for (i, item) in &chosen {
                    let item_page_url = crate::video_probe::story_segment_url(&page_url, &item.id)
                        .unwrap_or_else(|| item.page_url.clone());
                    let inheritance = {
                        let mut seen_hosts = guarded_child_hosts.borrow_mut();
                        crate::download_net::inherit_public_resolution_for_collection(
                            &page_url,
                            &item_page_url,
                            &mut seen_hosts,
                        )
                    };
                    match inheritance {
                        Ok(()) => {}
                        Err(crate::download_net::CollectionHostError::TooManyDistinctHosts) => {
                            skipped_by_host_limit += 1;
                            // Keep the process-wide policy bounded across retries.
                            // Leave the row selected so the user can deselect it
                            // after reading the explanation below the picker.
                            continue;
                        }
                        Err(crate::download_net::CollectionHostError::UnsafeDestination) => {
                            failed = Some(gettext(
                                "A selected item was rejected because its destination could not be verified as public.",
                            ));
                            break;
                        }
                    }
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
                    grid.unselect(*i);
                }
                if let Some(e) = failed {
                    let message = if skipped_by_host_limit > 0 {
                        format!(
                            "{e} {}",
                            ngettext(
                                "{n} item was also skipped because the collection exceeded the limit of {limit} distinct hosts.",
                                "{n} items were also skipped because the collection exceeded the limit of {limit} distinct hosts.",
                                skipped_by_host_limit as u32,
                            )
                            .replace("{n}", &skipped_by_host_limit.to_string())
                            .replace(
                                "{limit}",
                                &crate::download_net::MAX_GUARDED_CHILD_HOSTS_PER_COLLECTION
                                    .to_string(),
                            )
                        )
                    } else {
                        e
                    };
                    error_caption.set_text(&message);
                    error_caption.set_visible(true);
                    add_btn.set_sensitive(true);
                    return;
                }
                if skipped_by_host_limit > 0 {
                    error_caption.set_text(
                        &ngettext(
                            "Skipped {n} selected item because the collection exceeded the limit of {limit} distinct hosts.",
                            "Skipped {n} selected items because the collection exceeded the limit of {limit} distinct hosts.",
                            skipped_by_host_limit as u32,
                        )
                        .replace("{n}", &skipped_by_host_limit.to_string())
                        .replace(
                            "{limit}",
                            &crate::download_net::MAX_GUARDED_CHILD_HOSTS_PER_COLLECTION
                                .to_string(),
                        ),
                    );
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
    if !crate::window_rows::form_is_visible(nav) {
        return;
    }

    // The file count lives in the compact header; the group needs no title.
    let file_count = ngettext("{} file", "{} files", entries.len() as u32)
        .replace("{}", &entries.len().to_string());
    // Rows carry the display path; the old pill loop never rendered the
    // size sub either.
    let titles = entries
        .iter()
        .map(|e| e.display_path.clone())
        .collect::<Vec<_>>();
    let grid = GalleryGrid::new();
    grid.append(&titles);

    let list_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(6)
        .build();
    // HIG padding between the grid and the scrolled viewport edges.
    list_box.set_margin_top(12);
    list_box.set_margin_bottom(12);
    list_box.set_margin_start(12);
    list_box.set_margin_end(12);
    // Same natural-height, capped-scrolling behavior as the playlist picker;
    // captions stay outside the scroll area.
    list_box.append(grid.widget());
    let error_caption = gtk4::Label::builder()
        .label("")
        .css_classes(["error", "caption"])
        .halign(gtk4::Align::Start)
        .visible(false)
        .build();
    list_box.append(&error_caption);

    let toolbar = adw::ToolbarView::new();
    // Same compact header as the playlist picker: back + file name + count.
    let (header, _count_label) = picker_header(nav, &file_name, &file_count);
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&list_box));
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
    grid.wire_selection_bar(&select_all_btn, &select_none_btn, &add_btn, |n| {
        ngettext("_Add {} file", "_Add {} files", n as u32).replace("{}", &n.to_string())
    });

    {
        let close_card = close_card.clone();
        let grid = grid.clone();
        add_btn.connect_clicked(move |_| {
            let selected: Vec<usize> = grid.selected_indices();
            if selected.is_empty() {
                error_caption.set_text(&gettext("Select at least one file"));
                error_caption.set_visible(true);
                return;
            }
            // All on means no filter: pass None, not every index.
            let only = (selected.len() < grid.len()).then_some(selected);
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
    set_looking_up(v, false);
    v.action_revealer.set_visible(true);
    v.action_revealer.set_reveal_child(true);
    v.add_btn.set_icon_name("");
    v.add_btn.remove_css_class("circular");
    v.add_btn.set_label(&gettext("Add"));
    sync_entry_icon(v);
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
    nav: adw::NavigationView,
    open: Rc<dyn Fn(Option<String>)>,
    toggle: Rc<dyn Fn()>,
    open_torrent_picker: TorrentPickerOpener,
    state: Rc<OpenState>,
}

impl AddCard {
    /// The card widget, placed above the view stack in the window content.
    pub fn widget(&self) -> &gtk4::Widget {
        &self.widget
    }

    /// The card's navigation stack (form plus picker pages), so the window
    /// can react to a picker page becoming visible.
    pub fn navigation(&self) -> &adw::NavigationView {
        &self.nav
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
    // Tick icon inside the entry: managed by sync_entry_icon (tick while
    // idle with text, spinner paintable while looking up, none otherwise).
    // Icon-press submits like Enter. Entry starts empty, so no icon initially.
    url_bar.append(&url_entry);
    // Spinner paintable for the entry's secondary icon slot, animated by
    // the entry's own frame clock.
    let spinner_paintable = adw::SpinnerPaintable::new(Some(&url_entry));
    // Add button: appears only after the lookup finishes. Labeled pill once
    // a format is picked; icon-only checkmark otherwise. Colored rounded
    // HIG button.
    let add_btn = gtk4::Button::builder()
        .icon_name("object-select-symbolic")
        .tooltip_text(gettext("Add download"))
        .css_classes(["suggested-action", "circular"])
        .valign(gtk4::Align::Center)
        .build();
    add_btn.update_property(&[gtk4::accessible::Property::Label(&gettext("Add download"))]);
    // The revealer slides the Add button in from the left (SlideRight) once
    // the lookup resolves, pushing the gear and X buttons aside; it slides
    // out on enqueue or card close. Hidden (no spacing) until revealed, and
    // collapses back when the slide-out finishes.
    let action_revealer = slide_revealer(gtk4::RevealerTransitionType::SlideRight);
    action_revealer.set_child(Some(&add_btn));
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
    // Media-type switcher (Video ⇄ Gallery): the established AdwToggleGroup,
    // same pattern as the gallery filter below. Hidden until both backends
    // are viable for a decode; the wrapper carries the accessible label
    // (AdwToggleGroup doesn't implement GtkAccessible).
    let mode_toggle = adw::ToggleGroup::new();
    for (name, label) in [("video", gettext("Video")), ("gallery", gettext("Gallery"))] {
        let toggle = adw::Toggle::builder().name(name).label(&label).build();
        mode_toggle.add(toggle);
    }
    mode_toggle.set_active_name(Some("video"));
    // The wrapper owns visibility; the group stays visible inside it.
    let mode_toggle_wrap = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    mode_toggle_wrap
        .update_property(&[gtk4::accessible::Property::Label(&gettext("Preview type"))]);
    mode_toggle_wrap.append(&mode_toggle);
    mode_toggle_wrap.set_visible(false);
    video_group.add(&mode_toggle_wrap);
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
    // Gallery info row: hidden by default, shown when probe finds an image gallery.
    let gallery_info = adw::ActionRow::builder()
        .title(gettext("Gallery"))
        .use_markup(false)
        .build();
    // "Select files…" opener: suffix button on the gallery row. Runs a full
    // enumeration on demand, then pushes the checklist picker.
    let gallery_select_btn = gtk4::Button::builder()
        .label(gettext("Select files…"))
        .valign(gtk4::Align::Center)
        .css_classes(["flat"])
        .build();
    gallery_info.add_suffix(&gallery_select_btn);
    gallery_info.set_visible(false);
    video_group.add(&gallery_info);
    // Gallery filter toggle: All / Images / Videos. Hidden by default.
    let gallery_filter = adw::ToggleGroup::new();
    for (name, label) in [
        ("all", gettext("All")),
        ("images", gettext("Images")),
        ("videos", gettext("Videos")),
    ] {
        let toggle = adw::Toggle::builder().name(name).label(&label).build();
        gallery_filter.add(toggle);
    }
    gallery_filter.set_active_name(Some("all"));
    // Span the section width: the segmented control reads as one control
    // across the card instead of a floating pill row.
    gallery_filter.set_hexpand(true);
    // The wrapper owns visibility; the group stays visible inside it. (Phase
    // 10 hid the group here and never re-showed it: the filter never rendered.)
    // AdwToggleGroup doesn't implement GtkAccessible; the label goes on a
    // wrapper so screen readers announce the filter's purpose.
    let filter_wrap = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    // HIG spacing: the count subtitle above and this filter are separate
    // blocks, so raise the filter above the boxed list's bare row separator.
    filter_wrap.set_margin_top(6);
    filter_wrap.set_hexpand(true);
    filter_wrap.update_property(&[gtk4::accessible::Property::Label(&gettext(
        "Filter gallery by type",
    ))]);
    filter_wrap.append(&gallery_filter);
    filter_wrap.set_visible(false);
    video_group.add(&filter_wrap);
    // The preview block slides down like the options section: the revealer
    // owns show/hide.
    let video_revealer = slide_down_revealer();
    video_revealer.set_child(Some(&video_group));
    let step = Rc::new(VideoStep {
        url_entry: url_entry.clone(),
        spinner_paintable,
        looking_up: Cell::new(false),
        action_revealer: action_revealer.clone(),
        group_revealer: video_revealer.clone(),
        name: video_name,
        revert: video_revert_btn,
        format: video_format,
        options: Rc::new(RefCell::new(Vec::new())),
        tools: video_tools,
        error: video_error,
        gallery_info,
        gallery_filter,
        gallery_filter_wrap: filter_wrap,
        mode_toggle,
        mode_toggle_wrap,
        toggle_items: Rc::new(RefCell::new(None)),
        is_gallery: Rc::new(Cell::new(false)),
        gallery_card: Rc::new(RefCell::new(None)),
        gallery_select_btn: gallery_select_btn.clone(),
        gallery_select_opener: Rc::new(RefCell::new(None)),
        enumerate_abort: Rc::new(RefCell::new(None)),
        enumerate_gen: Cell::new(0),
        lookup_abort: Rc::new(RefCell::new(None)),
        add_btn: add_btn.clone(),
    });
    // "Select files…" invokes the opener set below (where the picker context
    // lives). Connected once; the opener is set once at build time.
    {
        let opener = step.gallery_select_opener.clone();
        step.gallery_select_btn.connect_clicked(move |_| {
            if let Some(open) = opener.borrow().as_ref() {
                open();
            }
        });
    }
    // Gallery filter toggle: update the card's filter and refresh the info row.
    // Connected once; reads the current card from the Rc.
    // On filter change, re-count if not cached (async), then refresh.
    {
        let card_rc = step.gallery_card.clone();
        let info = step.gallery_info.clone();
        step.gallery_filter
            .connect_active_name_notify(move |toggle| {
                let filter = match toggle.active_name().as_deref() {
                    Some("images") => crate::gallery_argv::GalleryFilter::ImagesOnly,
                    Some("videos") => crate::gallery_argv::GalleryFilter::VideosOnly,
                    _ => crate::gallery_argv::GalleryFilter::All,
                };
                // Check cache first (synchronous).
                let (needs_count, url) = {
                    if let Some(card_ref) = card_rc.borrow().as_ref() {
                        *card_ref.filter.borrow_mut() = filter;
                        if card_ref.is_count_cached(filter) {
                            // Cached (success or failure): refresh immediately.
                            let count_text = crate::gallery_card::display_count(
                                card_ref.file_count(),
                                5,
                                card_ref.total(filter),
                            );
                            info.set_subtitle(&count_text);
                            (false, String::new())
                        } else {
                            (true, card_ref.url().to_string())
                        }
                    } else {
                        (false, String::new())
                    }
                };
                // Not cached: count async, cache, refresh.
                if needs_count {
                    let card_rc = card_rc.clone();
                    let info = info.clone();
                    glib::spawn_future_local(async move {
                        let url_for_count = url.clone();
                        let filter_c = filter;
                        // Read settings before spawn: AppSettings is !Send.
                        let count_settings = crate::settings::AppSettings::new();
                        let browser = count_settings.cookies_browser();
                        let proxy =
                            crate::download::DownloadOptions::from_settings(&count_settings)
                                .proxy_config();
                        // Run auth + count on Tokio worker (not GTK main).
                        let count_result = crate::runtime::tokio_rt()
                            .spawn(async move {
                                let proxy = match proxy {
                                    Ok(proxy) => proxy,
                                    Err(e) => {
                                        tracing::warn!(error = %e, "gallery count proxy invalid");
                                        return None;
                                    }
                                };
                                // Unified auth: handoff jar first, browser
                                // export as fallback (was handoff-only: auth-
                                // gated galleries could count zero here but
                                // download fine).
                                let auth = crate::inline_gallery::resolve_gallery_auth(
                                    &url_for_count,
                                    browser.as_str(),
                                )
                                .await;
                                if let Ok(tool) = crate::gallery_argv::resolve_tool() {
                                    crate::gallery_probe::count_gallery_files_with_proxy(
                                        &tool,
                                        &url_for_count,
                                        auth.cookies_path.as_deref(),
                                        auth.user_agent.as_deref(),
                                        filter_c,
                                        proxy.as_ref(),
                                    )
                                    .await
                                } else {
                                    None
                                }
                            })
                            .await
                            .ok()
                            .flatten();
                        if let Some(card_ref) = card_rc.borrow().as_ref()
                            // Only update if the card still shows the same URL.
                            // The user may have changed the URL while the count was running.
                            && card_ref.url() == url
                        {
                            match count_result {
                                Some(n) => {
                                    card_ref.set_total(filter, n);
                                    // Refresh only if the filter hasn't changed again.
                                    if *card_ref.filter.borrow() == filter {
                                        let count_text = crate::gallery_card::display_count(
                                            card_ref.file_count(),
                                            5,
                                            Some(n),
                                        );
                                        info.set_subtitle(&count_text);
                                    }
                                }
                                None => {
                                    // Cache the failure so we don't re-stage cookies on every toggle.
                                    card_ref.set_total_failed(filter);
                                }
                            }
                        }
                        // `cookies` (StagedCookies) drops here, deleting the file.
                    });
                }
            });
    }
    // Media-type switcher: flipping to Gallery builds the card from the
    // already-probed items (no second gallery-dl run); flipping back shows
    // the yt-dlp preview from the stored probe. Programmatic active-name
    // resets run with the items absent and are ignored here.
    {
        let step2 = step.clone();
        step.mode_toggle.connect_active_name_notify(move |toggle| {
            let state = step2.toggle_items.borrow().clone();
            let Some((items, url)) = state else {
                return;
            };
            if toggle.active_name().as_deref() != Some("gallery") {
                show_video_ready(&step2);
                return;
            }
            let title = crate::gallery_probe::collection_title(&items, &url);
            let card = crate::gallery_card::GalleryCard::new(
                items,
                Some(crate::gallery_probe::StickyMode::Gallery),
                url,
            );
            show_gallery_ready(&step2, card, &title);
        });
    }
    // Card-local choices: the format is initialized from Preferences (not
    // bound). Exact picks are per lookup, so nothing persists here.
    form.append(&video_revealer);

    // Download options: HIG AdwPreferencesGroup, no header — the rows
    // speak for themselves. Rows get the 12px internal margins natively.
    let group = adw::PreferencesGroup::new();

    // Torrent and save location sit behind the gear toggle: the common
    // case is a bare URL, so the card opens compact.
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

    // Date picker: MenuButton opens a popover with GtkCalendar (HIG: no text
    // entry for dates). GTK/libadwaita provide no stock date picker, so this
    // composes native primitives — MenuButton, not a hand-wired Button+Popover.
    let calendar = gtk4::Calendar::new();
    let date_popover = gtk4::Popover::new();
    date_popover.set_child(Some(&calendar));
    let date_btn = gtk4::MenuButton::builder()
        .label(gettext("Choose date…"))
        .popover(&date_popover)
        .valign(gtk4::Align::Center)
        .build();
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
            calendar.connect_day_selected(move |_| update());
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
    // Queued URL from context menu/share while a probe is in flight: the
    // open() handler stores it here, and the probe completion auto-starts it.
    let pending_url: Rc<RefCell<VecDeque<String>>> = Rc::new(RefCell::new(VecDeque::new()));
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

    // Gallery "Select files…" opener: set once at build time.
    *step.gallery_select_opener.borrow_mut() = Some(make_gallery_picker_opener(
        step.clone(),
        &nav,
        manager.clone(),
        dest_dir.clone(),
        close_card.clone(),
        scheduled_at.clone(),
    ));

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
        let settings2 = manager.settings().clone();
        let lookup_add_kick = lookup_add.clone();
        let manager_kick = manager.clone();
        let dest_kick = dest_dir.clone();
        let close_kick = close_card.clone();
        let scheduled_at_kick = Rc::clone(&scheduled_at);
        let pending_kick = Rc::clone(&pending_url);
        let nav_kick = nav.clone();
        Rc::new(move |probe_unlisted: bool| {
            // Twin suppression: a resolve for this URL is already running for the
            // current generation (Add pressed twice is the usual trigger) — don't
            // spawn a second yt-dlp, the twin would lose the generation race anyway.
            // The marker carries the owning generation so a stale marker never
            // suppresses a re-kick, plus the unlisted-probe flag.
            let url = url_entry2.text().trim().to_string();
            let Some(my) = probe.borrow_mut().kick(url, probe_unlisted) else {
                return;
            };
            let (
                probe_b,
                step_b,
                url_b,
                settings_b,
                lookup_add_b,
                manager_b,
                dest_b,
                close_b,
                scheduled_at_b,
                pending_b,
                nav_b,
            ) = (
                probe.clone(),
                step2.clone(),
                url_entry2.clone(),
                settings2.clone(),
                lookup_add_kick.clone(),
                manager_kick.clone(),
                dest_kick.clone(),
                close_kick.clone(),
                scheduled_at_kick.clone(),
                pending_kick.clone(),
                nav_kick.clone(),
            );
            let lookup = glib::spawn_future_local(async move {
                // Declared BEFORE the guard so it drops AFTER it (reverse
                // declaration order): every exit, early returns included, starts
                // the next queued link with the in-flight marker already clear.
                let _drain = DrainPending {
                    probe: Rc::clone(&probe_b),
                    pending: Rc::clone(&pending_b),
                    start: {
                        let url_entry = url_b.clone();
                        Rc::new(move |next: String| {
                            url_entry.set_text(&next);
                            // Submit one idle later: the changed handler settles
                            // on a 0 ms timeout, and submitting in this turn reads
                            // the previous URL's state while the deferred edit
                            // aborts the kick.
                            let entry = url_entry.clone();
                            glib::idle_add_local_once(move || {
                                entry.activate();
                            });
                        })
                    },
                };
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
                // Dual-backend probe: both tools run for non-direct-media
                // URLs and `route_for_collection()` decides over both real
                // results. A gallery-dl hit no longer cancels yt-dlp: the
                // user's video/gallery choice needs yt-dlp's result too.
                // Skipped for direct media URLs (not gallery pages).
                let browser = settings_b.cookies_browser();
                let newest_first = settings_b.video_codec_newest();
                // Key the probe-JSON cache by the probe's conditions: the
                // download reuses the JSON only under identical ones. The
                // download side keys on the normalized page URL, so this key
                // must match exactly: typed URLs that normalize (bare
                // domains, trailing slash, host case) otherwise miss the
                // cache silently.
                let key_url = crate::download::normalize_url(&url).unwrap_or_else(|_| url.clone());
                let probe_json_key: crate::download::ProbeJsonKey = (
                    key_url,
                    browser.clone(),
                    proxy.as_ref().map(|p| p.cli_url.clone()),
                );
                // Gallery items for the Video ⇄ Gallery switcher, armed in
                // the Single arm below when route() says both are viable.
                let mut toggle_items: Option<ToggleItems> = None;
                let (ytdlp_result, gallery_error) =
                    if !crate::gallery_probe::url_is_direct_media(&url)
                        && let Ok(tool) = crate::gallery_argv::resolve_tool()
                    {
                        // This lookup is polled by GLib, but gallery-dl and browser-cookie
                        // export use tokio::process and must run inside Tokio's runtime.
                        let probe_url = url.clone();
                        let probe_browser = browser.clone();
                        let probe_proxy = proxy.clone();
                        let probe_libs = libs.clone();
                        let probe_handle = crate::runtime::tokio_rt().spawn(async move {
                            crate::inline_gallery::dual_probe(crate::inline_gallery::DualProbeCtx {
                                url: &probe_url,
                                browser: probe_browser,
                                newest_first,
                                proxy: probe_proxy,
                                libs: probe_libs,
                                tool,
                            })
                            .await
                        });
                        // If the parent GLib lookup is superseded, this future is
                        // dropped. A bare JoinHandle would detach the Tokio task;
                        // abort it so both yt-dlp and gallery-dl probe children wind down.
                        let _abort_probe_on_drop = AbortTaskOnDrop(probe_handle.abort_handle());
                        let probe_outcome = match probe_handle.await {
                            Ok(outcome) => outcome,
                            Err(e) => {
                                tracing::error!(
                                    error = %e,
                                    "dual-backend probe task failed"
                                );
                                // Don't overwrite the UI if this lookup is stale.
                                if probe_b.borrow().generation() != my {
                                    return;
                                }
                                let message =
                                    gettext("The lookup failed unexpectedly. Please try again.");
                                show_video_error(&step_b, &message);
                                set_lookup_add(&lookup_add_b, true);
                                return;
                            }
                        };
                        // Discard stale results: the user may have pasted a new URL
                        // meanwhile.
                        if probe_b.borrow().generation() != my {
                            return;
                        }
                        match probe_outcome {
                            crate::inline_gallery::DualProbeOutcome::Complete {
                                gallery,
                                ytdlp,
                            } => {
                                // Failure-path error precedence: a
                                // surfaced gallery error is preferred, but
                                // only when yt-dlp found nothing.
                                let ytdlp_video = crate::inline_gallery::ytdlp_found_video(&ytdlp);
                                let gallery_error = crate::inline_gallery::preferred_probe_error(
                                    ytdlp_video,
                                    &gallery,
                                );
                                let ytdlp_playlist_entries = match &ytdlp {
                                    Ok((crate::video::ProbeResult::Playlist(pl), _)) => pl.total,
                                    _ => 0,
                                };
                                let routed = crate::gallery_probe::route_for_collection(
                                    ytdlp_video,
                                    ytdlp_playlist_entries,
                                    gallery.as_ref().ok().and_then(|items| items.as_deref()),
                                    None,
                                );
                                match routed {
                                    crate::gallery_probe::ProbeOutcome::VideoYtDlp {
                                        gallery_toggle,
                                    } => {
                                        if gallery_toggle {
                                            let items = gallery.ok().flatten();
                                            toggle_items = items.map(|i| (i, url.clone()));
                                        }
                                        (ytdlp, gallery_error)
                                    }
                                    crate::gallery_probe::ProbeOutcome::VideoGalleryDl
                                    | crate::gallery_probe::ProbeOutcome::Gallery => {
                                        // Both outcomes contain gallery-dl entries.
                                        // Keep queue creation behind the full file picker:
                                        // the probe is only a preview and may be capped.
                                        let items = gallery.ok().flatten().unwrap_or_default();
                                        let card = crate::gallery_card::GalleryCard::new(
                                            items,
                                            Some(crate::gallery_probe::StickyMode::Gallery),
                                            url.clone(),
                                        );
                                        let title = crate::gallery_probe::collection_title(
                                            &card.items,
                                            &url,
                                        );
                                        tracing::info!(
                                            files = card.file_count(),
                                            sticky = ?card.sticky_mode(),
                                            "gallery-dl items await user selection"
                                        );
                                        show_gallery_ready(&step_b, card, &title);
                                        // Every terminal state re-enables Add.
                                        set_lookup_add(&lookup_add_b, true);
                                        // With auto-add this lookup *is* the
                                        // submit: a gallery needs a selection,
                                        // so open the picker instead of
                                        // stopping on the inline row.
                                        if settings_b.auto_add_downloads() {
                                            let opener =
                                                step_b.gallery_select_opener.borrow().clone();
                                            if let Some(open) = opener {
                                                open();
                                            }
                                        }
                                        return;
                                    }
                                    crate::gallery_probe::ProbeOutcome::None => {
                                        (ytdlp, gallery_error)
                                    }
                                }
                            }
                        }
                    } else {
                        (
                            crate::video::fetch_video_infos(
                                libs,
                                url.clone(),
                                browser,
                                newest_first,
                                proxy,
                                None,
                            )
                            .await,
                            None,
                        )
                    };
                match ytdlp_result {
                    Err(e) => {
                        if probe_b.borrow().generation != my {
                            return;
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
                        // Plain fallback for yt-dlp "unsupported url" on non-video
                        // pages (e.g. direct .pdf/.zip links). gallery-dl was
                        // already tried above and found nothing.
                        let plain_fallback_ok = !crate::video::is_video_page(&url)
                            && e.to_string().to_lowercase().contains("unsupported url");
                        if plain_fallback_ok {
                            match queue_plain(
                                &manager_b,
                                &dest_b,
                                &close_b,
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
                        probe_b.borrow_mut().info.take();
                        // Prefer a surfaced gallery-dl error (e.g. login
                        // required): it's more actionable than yt-dlp's.
                        let msg = gallery_error
                            .as_ref()
                            .map(|ge| ge.to_string())
                            .unwrap_or_else(|| e.to_string());
                        tracing::warn!(
                            host = %crate::video_probe::page_host(&url),
                            error = %msg,
                            "video preview failed"
                        );
                        show_video_error(&step_b, &msg);
                        set_lookup_add(&lookup_add_b, true);
                    }
                    Ok((probe, raw_json)) => {
                        if probe_b.borrow().generation != my {
                            return;
                        }
                        // Cache the raw info JSON for --load-info-json at download time.
                        // Single videos only; playlists re-probe per entry.
                        if let Some(json) = raw_json {
                            // The probe canonically retargets some pages
                            // (youtu.be/… → …/watch?v=…) and enqueue stores the
                            // probed page URL, which the download keys the cache on.
                            // Fill that key too, else short links and other
                            // retargets miss the cache and re-extract.
                            manager_b.cache_probe_json(probe_json_key.clone(), json.clone());
                            let final_url = crate::download::normalize_url(probe.page_url())
                                .unwrap_or_else(|_| probe.page_url().to_string());
                            if final_url != probe_json_key.0 {
                                let retarget_key = (
                                    final_url,
                                    probe_json_key.1.clone(),
                                    probe_json_key.2.clone(),
                                );
                                manager_b.cache_probe_json(retarget_key, json);
                            }
                        }
                        // Resolved but nothing playable, and not a listed video
                        // page: same plain fallback as above.
                        if !probe.fetchable() && !crate::video::is_video_page(&url) {
                            match queue_plain(
                                &manager_b,
                                &dest_b,
                                &close_b,
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
                                // Seed the file name once from the title default.
                                // Never clobbers an edit here.
                                if step_b.name.text().trim().is_empty() {
                                    // Seeded before the format rebuild;
                                    // audio-only is always off by design
                                    // at seed time.
                                    let base = default_name_for(&settings_b, &v.title, false);
                                    step_b.name.set_text(&base);
                                }
                                probe_b.borrow_mut().last_ok = url;
                                rebuild_format_options(&step_b, &v, &settings_b.video_quality());
                                // Store a copy for retry; auto-add submits
                                // immediately with the pre-selected quality.
                                probe_b.borrow_mut().info =
                                    Some(crate::video::ProbeResult::Single(v.clone()));
                                // Both backends were viable (route() armed
                                // the items): show the Video ⇄ Gallery
                                // switcher with the preview.
                                arm_mode_toggle(&step_b, toggle_items.take());
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
                                // Auto-add's lookup is the submit: a playlist
                                // needs a selection, so open its picker too.
                                if settings_b.auto_add_downloads() {
                                    push_playlist_items_page(
                                        &nav_b,
                                        manager_b.clone(),
                                        dest_b.clone(),
                                        close_b.clone(),
                                        pl,
                                        scheduled_at_b.get(),
                                    );
                                }
                            }
                        }
                    }
                }
            });
            // This kick supersedes any earlier lookup: its generation is
            // stale now, so abort it — the dropped task kills the probe
            // children instead of letting them run out their timeouts.
            if let Some(prev) = step2.lookup_abort.borrow_mut().replace(lookup) {
                prev.abort();
            }
        })
    };

    // One submit path for the Add button and Enter: video pages go through the
    // media pipeline (a matching preview is required so the row stores the
    // resolved page, not a stale URL); everything else keeps direct enqueue.
    let submit = {
        let m = manager.clone();
        let dd = dest_dir.clone();
        let url_entry = url_entry.clone();
        let close_card = close_card.clone();
        let probe = Rc::clone(&probe);
        let step2 = step.clone();
        let kick = kick_video.clone();
        let lookup_add_submit = lookup_add.clone();
        let nav2 = nav.clone();
        let scheduled_at = Rc::clone(&scheduled_at);
        Rc::new(move |source: SubmitSource| {
            let from_activate = matches!(source, SubmitSource::Enter | SubmitSource::Auto);
            let fail = |message: &str| {
                // Form-validation pattern: the error state lives on the
                // entry itself, not in a caption underneath it.
                set_field_error(&url_entry, message);
            };
            // A gallery opens the file picker on every submit, auto-add included,
            // so nothing is queued unseen.
            if step2.is_gallery.get() {
                let open = step2.gallery_select_opener.borrow().clone();
                if let Some(open) = open {
                    open();
                }
                return;
            }
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
            enqueue_and_close(
                &m,
                &dd,
                &close_card,
                scheduled_at.get(),
                |d| m.enqueue(&url, d, None),
                fail,
            );
        })
    };
    {
        let s = submit.clone();
        add_btn.connect_clicked(move |_| s(SubmitSource::Button));
    }
    {
        let s = submit.clone();
        url_entry.connect_activate(move |_| s(SubmitSource::Enter));
    }
    // Tick icon inside the entry submits like Enter; the spinner cancels the
    // running lookup (the entry is locked while it resolves).
    {
        let s = submit.clone();
        let step = step.clone();
        let probe = Rc::clone(&probe);
        url_entry.connect_icon_press(move |_, pos| {
            if pos != gtk4::EntryIconPosition::Secondary {
                return;
            }
            if step.looking_up.get() {
                probe.borrow_mut().bump_generation();
                abort_lookup(&step);
                set_looking_up(&step, false);
                sync_entry_icon(&step);
            } else {
                s(SubmitSource::Button);
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
        let submit_auto = submit.clone();
        let settings_auto = manager.settings().clone();
        let debounce: Rc<std::cell::RefCell<Option<glib::SourceId>>> =
            Rc::new(std::cell::RefCell::new(None));
        let last_text: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
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
        // GTK emits `changed` twice for a replace (clear, then insert). The
        // 0 ms coalescing below drops the transient "", the sync handler
        // only invalidates on a settled non-empty change, and the guard then
        // ignores an identical re-set: none may count as an edit that wipes
        // the resolved collection and re-arms auto-add.
        let entry = url_entry.clone();
        let last_text_sync = Rc::clone(&last_text);
        let probe_sync = Rc::clone(&probe);
        let step_sync = Rc::clone(&step2);
        let apply = Rc::new(move || {
            let text = entry.text().trim().to_string();
            if !text_is_new(&mut last_text.borrow_mut(), &text) {
                return;
            }
            // The sync handler skips the transient empty; a settled clear
            // still invalidates the lookup (0 ms later).
            if text.is_empty() && step2.looking_up.get() {
                probe.borrow_mut().bump_generation();
                abort_lookup(&step2);
            }
            clear_field_error(&entry);
            // Auto-add: debounce the lookup so it fires after the user
            // pauses typing/pasting, not on every keystroke.
            if settings_auto.auto_add_downloads() && url_looks_complete(&text) {
                if let Some(id) = debounce.borrow_mut().take() {
                    id.remove();
                }
                let s = submit_auto.clone();
                let debounce_c = Rc::clone(&debounce);
                let id =
                    glib::timeout_add_local(std::time::Duration::from_millis(800), move || {
                        // Clear the stored ID first: after Break the source is
                        // gone, and a later `changed` (e.g. programmatic
                        // set_text) must not remove this dead ID — that was a
                        // hard abort ("Source ID was not found").
                        *debounce_c.borrow_mut() = None;
                        s(SubmitSource::Auto);
                        glib::ControlFlow::Break
                    });
                *debounce.borrow_mut() = Some(id);
            } else if let Some(id) = debounce.borrow_mut().take() {
                id.remove();
            }
            // A probed preview counts as video mode while its canonical URL
            // matches the entry text.
            let fresh = probe
                .borrow()
                .info
                .as_ref()
                .is_some_and(|p| p.page_url() == text);
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
            // Any edit clears the lookup-in-flight state, and the icon
            // follows: the tick returns for non-empty text, the spinner
            // and the icon-less states clear.
            set_looking_up(&step2, false);
            sync_entry_icon(&step2);
        });
        let scheduled: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
        url_entry.connect_changed(move |entry| {
            // A re-set of the settled text (handoff, queued drain) is not an
            // edit: aborting here would kill the live lookup while `apply`
            // ignores the identical text, leaving the spinner up and the
            // entry locked. Empty is skipped because GTK emits it transiently
            // during a replace; `apply` aborts a real clear.
            let text = entry.text().trim().to_string();
            let looking_up = step_sync.looking_up.get();
            if edit_invalidates_lookup(last_text_sync.borrow().as_str(), &text, looking_up) {
                // Stale *now*: a probe completing this turn must not slip past
                // the edit and (with auto-add) queue the URL being replaced.
                // Aborting here also stops the probes instead of letting them
                // read to completion for a URL the user left.
                probe_sync.borrow_mut().bump_generation();
                abort_lookup(&step_sync);
                // The abort drops the task that would clear the lock; reset
                // here so an edit that settles back to `last_text` can't
                // leave the spinner up and the entry locked.
                set_looking_up(&step_sync, false);
                sync_entry_icon(&step_sync);
            }
            if let Some(id) = scheduled.borrow_mut().take() {
                id.remove();
            }
            let apply = apply.clone();
            let scheduled_c = Rc::clone(&scheduled);
            let id = glib::timeout_add_local(std::time::Duration::from_millis(0), move || {
                *scheduled_c.borrow_mut() = None;
                apply();
                glib::ControlFlow::Break
            });
            *scheduled.borrow_mut() = Some(id);
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
                None, // Fresh install: show all tools.
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
        let probe = Rc::clone(&probe);
        let pending_url = Rc::clone(&pending_url);
        // The last URL the clipboard auto-paste inserted: a stale clipboard
        // link pastes once, not on every fresh open. Persisted so a restart
        // doesn't forget and re-paste the stale link.
        // Only a digest is kept: a clipboard link can carry a token, and
        // "same link as last time" needs nothing more.
        let last_auto_paste: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new({
            let saved = manager.settings().last_auto_paste();
            if saved.is_empty() { None } else { Some(saved) }
        }));
        Rc::new(move |initial_url: Option<String>| {
            let already = open_state.is_open();
            reveal();
            // Dropped/opened URLs land here pre-filled: setting the text syncs the form, and the
            // lookup itself starts on Add/Enter like any other entry.
            // If a probe is already in flight, queue the URL: it auto-starts
            // when the current probe finishes (see the completion handler).
            // Also queue if the entry holds an unprobed URL: replacing it
            // would lose the first link.
            if let Some(raw) = initial_url {
                if let Ok(normalized) = crate::download::normalize_url(raw.trim()) {
                    let st = probe.borrow();
                    let busy = st.inflight.is_some();
                    let has_unprobed = !busy
                        && st.info.is_none()
                        && !url_entry.text().trim().is_empty()
                        && url_entry.text().trim() != normalized;
                    drop(st);
                    if busy || has_unprobed {
                        // FIFO: rapid sends must not overwrite each other.
                        let mut queue = pending_url.borrow_mut();
                        if !queue.contains(&normalized) {
                            queue.push_back(normalized);
                        }
                        return;
                    }
                    url_entry.set_text(&normalized);
                }
            } else if !already && url_entry.text().trim().is_empty() {
                // Single clipboard read per fresh open; no watch, no polling.
                let url_entry = url_entry.clone();
                let last_auto_paste = Rc::clone(&last_auto_paste);
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
                        // Paste once: skip a link this auto-paste already inserted.
                        let fingerprint = paste_fingerprint(&normalized);
                        if last_auto_paste.borrow().as_deref() == Some(fingerprint.as_str()) {
                            return;
                        }
                        url_entry.set_text(&normalized);
                        *last_auto_paste.borrow_mut() = Some(fingerprint.clone());
                        // Persist so a restart doesn't re-paste the stale link.
                        // The manager Rc isn't available in this async block;
                        // settings are global via the default AppSettings.
                        crate::settings::AppSettings::new().set_last_auto_paste(&fingerprint);
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
        nav: nav.clone(),
        open,
        toggle,
        open_torrent_picker,
        state: Rc::clone(&open_state),
    }
}

/// Build the gallery "Select files…" opener: reads the current card state
/// (URL, filter, title) at click time, runs the bounded enumeration in the
/// background, then pushes the checklist picker.
fn make_gallery_picker_opener(
    step: Rc<VideoStep>,
    nav: &adw::NavigationView,
    manager: Rc<DownloadManager>,
    dest_dir: Rc<RefCell<String>>,
    close_card: Rc<dyn Fn()>,
    scheduled_at: Rc<Cell<Option<i64>>>,
) -> Rc<dyn Fn()> {
    let step2 = step.clone();
    let nav2 = nav.clone();
    let manager2 = manager.clone();
    let dest2 = dest_dir.clone();
    let close2 = close_card.clone();
    let scheduled2 = scheduled_at.clone();
    Rc::new(move || {
        let (url, filter, title) = match step2.gallery_card.borrow().as_ref() {
            Some(card) => (
                card.url().to_string(),
                *card.filter.borrow(),
                step2.gallery_info.title().to_string(),
            ),
            None => return,
        };
        // A previous enumeration is superseded: abort it (kills
        // gallery-dl) instead of letting it probe on.
        abort_enumeration(&step2);
        let my_gen = step2.enumerate_gen.get();
        // Disable the button and show a loading label while enumerating;
        // the picker replaces it on success.
        let loading_label = gettext("Loading…");
        let orig_label = step2.gallery_select_btn.label().unwrap_or_default();
        step2.gallery_select_btn.set_sensitive(false);
        step2.gallery_select_btn.set_label(&loading_label);
        let step3 = step2.clone();
        let nav3 = nav2.clone();
        let manager3 = manager2.clone();
        let dest3 = dest2.clone();
        let close3 = close2.clone();
        let scheduled3 = scheduled2.clone();
        let picker_settings = crate::settings::AppSettings::new();
        let browser3 = picker_settings.cookies_browser();
        let proxy3 =
            crate::download::DownloadOptions::from_settings(&picker_settings).proxy_config();
        glib::spawn_future_local(async move {
            let restore_btn = || {
                step3.gallery_select_btn.set_sensitive(true);
                step3.gallery_select_btn.set_label(&orig_label);
            };
            // Surface enumeration failures on the gallery row instead of
            // only logging: the user clicked a button and deserves feedback.
            let show_error = |msg: &str| {
                step3.gallery_info.set_subtitle(msg);
            };
            let proxy3 = match proxy3 {
                Ok(proxy) => proxy,
                Err(e) => {
                    restore_btn();
                    show_error(&e);
                    return;
                }
            };
            let tool = match crate::gallery_argv::resolve_tool() {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("gallery picker: no gallery-dl: {e}");
                    restore_btn();
                    show_error(&gettext(
                        "Could not list files: gallery-dl is not installed",
                    ));
                    return;
                }
            };
            let url_c = url.clone();
            let (items_tx, mut items_rx) = tokio::sync::mpsc::unbounded_channel();
            let handle = crate::runtime::tokio_rt().spawn(async move {
                // Unified auth: handoff jar first, browser export as
                // fallback (was handoff-only: auth-gated galleries could
                // list nothing here but download fine).
                let auth =
                    crate::inline_gallery::resolve_gallery_auth(&url_c, browser3.as_str()).await;
                let r = crate::gallery_probe::enumerate_gallery_files_streamed_with_proxy(
                    &tool,
                    &url_c,
                    auth.cookies_path.as_deref(),
                    auth.user_agent.as_deref(),
                    filter,
                    items_tx,
                    proxy3.as_ref(),
                )
                .await;
                drop(auth);
                r
            });
            *step3.enumerate_abort.borrow_mut() = Some(handle.abort_handle());
            // Fill the picker as items arrive: it opens on the first one, so
            // a huge gallery never blocks the UI on the full listing.
            let mut page: Option<crate::inline_gallery::GalleryPickerStream> = None;
            let mut batch: Vec<crate::gallery_probe::GalleryItem> = Vec::new();
            while let Some(item) = items_rx.recv().await {
                batch.push(item);
                // Coalesce whatever else already arrived into one append:
                // one items-changed and one relayout per burst.
                while let Ok(next) = items_rx.try_recv() {
                    batch.push(next);
                }
                if page.is_none() {
                    // Superseded (newer click, card close, new lookup):
                    // stay quiet — the newer task owns the UI now.
                    if step3.enumerate_gen.get() != my_gen {
                        handle.abort();
                        return;
                    }
                    // Stale guard: the card may have been closed or a new
                    // URL pasted while enumerating; don't push a picker for
                    // the old gallery onto the new card's nav stack. The
                    // picker replaces the card's loading button from here on.
                    let still_current = step3
                        .gallery_card
                        .borrow()
                        .as_ref()
                        .map(|card| card.url() == url)
                        .unwrap_or(false);
                    if !still_current {
                        handle.abort();
                        return;
                    }
                    restore_btn();
                    page = match crate::inline_gallery::push_gallery_picker_page(
                        &nav3,
                        manager3.clone(),
                        dest3.clone(),
                        close3.clone(),
                        scheduled3.get(),
                        title.clone(),
                        url.clone(),
                        filter,
                        crate::gallery_grid::GalleryGrid::new(),
                    ) {
                        Some(page) => Some(page),
                        // Another picker already owns the screen: the
                        // enumeration has nowhere to land — drop out.
                        None => {
                            handle.abort();
                            return;
                        }
                    };
                }
                // Items buffered before a supersede must not land on the page.
                if step3.enumerate_gen.get() != my_gen {
                    handle.abort();
                    break;
                }
                if let Some(page) = page.as_ref() {
                    page.append(&batch);
                }
                batch.clear();
            }
            let result = handle.await;
            // Superseded: the newer task owns the card, so stay quiet — but
            // a picker for this gallery already on screen must still be
            // enabled, or nothing ever un-gates its action bar.
            if step3.enumerate_gen.get() != my_gen {
                if let Some(page) = page.as_ref() {
                    page.mark_loaded();
                }
                return;
            }
            match result {
                Ok(Ok(_)) => match page {
                    Some(page) => page.mark_loaded(),
                    None => {
                        // Stream ended without a single item.
                        restore_btn();
                        show_error(&gettext("No files found"));
                    }
                },
                Ok(Err(e)) => {
                    tracing::warn!("gallery picker: enumeration failed: {e:?}");
                    match page {
                        // Partial list: keep it selectable, surface the
                        // failure under the entries.
                        Some(page) => page.show_error(&gettext("Could not list files")),
                        None => {
                            restore_btn();
                            show_error(&gettext("Could not list files"));
                        }
                    }
                }
                Err(e) => {
                    // Aborted or panicked: the abort path returns above
                    // via the generation guard, so this is a real failure.
                    tracing::warn!("gallery picker: enumeration task failed: {e:?}");
                    match page {
                        Some(page) => page.show_error(&gettext("Could not list files")),
                        None => {
                            restore_btn();
                            show_error(&gettext("Could not list files"));
                        }
                    }
                }
            }
        });
    })
}

fn show_gallery_ready(v: &VideoStep, card: crate::gallery_card::GalleryCard, title: &str) {
    hide_video_step(v);
    set_looking_up(v, false);
    v.is_gallery.set(true);
    // Store the card first so the filter handler can access it.
    *v.gallery_card.borrow_mut() = Some(card);
    // A superseded enumeration can leave the button on "Loading…"; every
    // resolved gallery row gets a working opener.
    v.gallery_select_btn.set_sensitive(true);
    v.gallery_select_btn.set_label(&gettext("Select files…"));
    // Reset filter to All.
    v.gallery_filter.set_active_name(Some("all"));
    refresh_gallery_info(v, title);
    v.gallery_info.set_visible(true);
    v.gallery_filter_wrap.set_visible(true);
    // The switcher is live while both sides are stored; flipping to Video
    // restores the yt-dlp preview.
    v.mode_toggle_wrap
        .set_visible(v.toggle_items.borrow().is_some());
    // Reveal the block.
    v.action_revealer.set_visible(true);
    v.action_revealer.set_reveal_child(true);
    v.group_revealer.set_visible(true);
    v.group_revealer.set_reveal_child(true);
    // The Add button earns its text label.
    v.add_btn.set_icon_name("");
    v.add_btn.remove_css_class("circular");
    v.add_btn.set_label(&gettext("Add"));
    sync_entry_icon(v);
}

/// Refresh the gallery info row from the current card state.
fn refresh_gallery_info(v: &VideoStep, title: &str) {
    if let Some(card) = v.gallery_card.borrow().as_ref() {
        let filter = *card.filter.borrow();
        let total = card.total(filter);
        let count_text = crate::gallery_card::display_count(card.file_count(), 5, total);
        v.gallery_info.set_title(title);
        v.gallery_info.set_subtitle(&count_text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_text_is_ignored() {
        // The UI coalesces GTK's clear+insert pair before this guard runs;
        // this pins the guard itself: an identical re-set is not an edit, so
        // a resolved collection and the armed auto-add survive it.
        let mut last = String::new();
        assert!(text_is_new(&mut last, "https://example.com/gallery"));
        assert!(!text_is_new(&mut last, "https://example.com/gallery"));
        assert!(text_is_new(&mut last, "https://example.com/video"));
        // Clearing is a real edit: close resets the form.
        assert!(text_is_new(&mut last, ""));
        assert!(!text_is_new(&mut last, ""));
    }

    #[test]
    fn identical_reset_keeps_the_live_lookup() {
        let last = "https://example.com/video";
        assert!(!edit_invalidates_lookup(last, last, true));
        // GTK's transient clear half of a replace is not a settled edit.
        assert!(!edit_invalidates_lookup(last, "", true));
    }

    #[test]
    fn a_different_text_invalidates_the_live_lookup() {
        let last = "https://example.com/a";
        assert!(edit_invalidates_lookup(last, "https://example.com/b", true));
        assert!(!edit_invalidates_lookup(
            last,
            "https://example.com/b",
            false
        ));
    }

    #[test]
    fn entry_icon_lookup_wins_over_everything() {
        // While a lookup is in flight the tick morphs into a spinner in
        // place, regardless of text, Add visibility, or error state.
        assert_eq!(entry_icon_for(true, true, false, false), EntryIcon::Spinner);
        assert_eq!(entry_icon_for(true, true, true, false), EntryIcon::Spinner);
        assert_eq!(entry_icon_for(false, true, false, true), EntryIcon::Spinner);
    }

    #[test]
    fn entry_icon_tick_only_when_idle_with_text_and_no_add() {
        // The tick submits: idle, non-empty, and no Add button yet.
        assert_eq!(entry_icon_for(true, false, false, false), EntryIcon::Tick);
        // Empty entry: nothing to submit.
        assert_eq!(entry_icon_for(false, false, false, false), EntryIcon::None);
    }

    #[test]
    fn entry_icon_cleared_once_add_appears() {
        // Once Add appears beside the entry, the entry carries no icon —
        // the button owns the action.
        assert_eq!(entry_icon_for(true, false, true, false), EntryIcon::None);
        assert_eq!(entry_icon_for(false, false, true, false), EntryIcon::None);
    }

    #[test]
    fn entry_icon_cleared_on_error_or_tools_state() {
        // On lookup failure the error row's Retry (or the tools row's
        // Install) owns recovery: no entry icon.
        assert_eq!(entry_icon_for(true, false, false, true), EntryIcon::None);
        assert_eq!(entry_icon_for(true, false, true, true), EntryIcon::None);
    }

    fn dummy_probe() -> crate::video::ProbeResult {
        crate::video::ProbeResult::Single(crate::video::VideoInfo {
            title: "Test title".into(),
            page_url: "https://youtu.be/x".into(),
            formats: Box::new([]),
            is_live: false,
            fetchable: true,
        })
    }

    #[tokio::test]
    async fn abort_task_on_drop_aborts_nested_tokio_task() {
        let child = tokio::spawn(std::future::pending::<()>());
        let guard = AbortTaskOnDrop(child.abort_handle());
        drop(guard);
        let error = child.await.expect_err("guard must abort the child task");
        assert!(error.is_cancelled());
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
        // Any in-flight probe suppresses a new kick: same URL, different
        // URL, or different probe mode — all must wait for the lookup.
        assert!(st.kick("https://youtu.be/a".to_string(), false).is_none());
        assert!(st.kick("https://youtu.be/b".to_string(), false).is_none());
        assert!(st.kick("https://youtu.be/a".to_string(), true).is_none());
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
        // A different URL is also suppressed while a probe is in flight.
        assert!(inflight_suppresses(&marker, "https://youtu.be/b", 2, false));
        // No marker, no suppression.
        assert!(!inflight_suppresses(&None, "https://youtu.be/a", 2, false));
    }

    #[test]
    fn explicit_unlisted_kick_is_not_suppressed_by_plain_kick() {
        // An explicit unlisted kick is also suppressed while a plain probe
        // is in flight for the current generation: any second probe would
        // race the shared state.
        let marker = Some(("https://youtu.be/a".to_string(), 2, false));
        assert!(inflight_suppresses(&marker, "https://youtu.be/a", 2, true));
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
