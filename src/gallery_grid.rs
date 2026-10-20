//! Gallery picker grid: a recycling `GtkGridView` over `GtkMultiSelection`.
//!
//! The pill layout this replaces materialized one widget per
//! entry for the picker's whole life; galleries stream in unbounded, so cost
//! would be linear in entries. `GtkGridView` reuses a handful of cells whatever
//! the enumeration produced, and `GtkMultiSelection` adds the Ctrl+A,
//! rubber-band and arrow-key selection pills never had.
//!
//! One cell is a mirrored `CheckButton` (the visual state) plus an
//! ellipsized `Label` filling the rest of the cell. Columns cap at three:
//! `GtkGridView` sizes columns from the cell's *minimum* width and clamps
//! to its 7-column default, which sliced the picker into slivers and
//! ellipsized every title to a few characters. Columns always divide the
//! row width, so fewer of them just make each column wider. Selection state
//! lives in `GtkMultiSelection`; the check only mirrors it in `bind` and
//! through one `selection-changed` watch per realized cell. Titles are set
//! in `bind` from the `StringObject` — plain text, since titles are
//! untrusted and may carry `&`, `<`, RTL and bidi-control characters.

use gtk4::glib::object::ObjectType;
use gtk4::prelude::*;
use gtk4::{gdk, glib, pango};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// (weak add button, count label formatter) once wired — the add button
/// shows the selected count.
type CountSink = (glib::WeakRef<gtk4::Button>, Rc<dyn Fn(usize) -> String>);

struct Inner {
    /// Titles in entry order; the store position is the entry index, which
    /// `selected_indices` maps to a `--range` spec.
    store: gtk4::StringList,
    selection: gtk4::MultiSelection,
    scrolled: gtk4::ScrolledWindow,
    count: RefCell<Option<CountSink>>,
}

/// The scrolled grid grows with small selections and scrolls after this cap.
/// 420px leaves the card compact for a handful of entries while keeping a
/// useful number visible in long galleries, playlists, and torrent pickers.
const MAX_PICKER_HEIGHT: i32 = 420;

/// Cloneable handle to the picker widgets (Rc inside, clone is cheap).
#[derive(Clone)]
pub(crate) struct GalleryGrid {
    inner: Rc<Inner>,
}

/// Selected count straight from the model; no mirror of the state is kept.
fn selected_count(selection: &gtk4::MultiSelection) -> usize {
    selection.selection().size() as usize
}

/// Refresh the wired count label from the model.
fn inner_count_bump(inner: &Inner) {
    if let Some((add_btn, count_label)) = inner.count.borrow().as_ref()
        && let Some(add_btn) = add_btn.upgrade()
    {
        add_btn.set_label(&count_label(selected_count(&inner.selection)));
    }
}

/// The bound position of a row's item, or INVALID when the row is not
/// currently presenting an entry (detached, recycled or before `bind`).
fn item_position(item: &glib::WeakRef<gtk4::ListItem>) -> u32 {
    item.upgrade()
        .map_or(gtk4::INVALID_LIST_POSITION, |li| li.position())
}

/// Flip one entry through the model (the click and Enter-activation path);
/// the rest of the selection is untouched. Plain arrow keys keep
/// `GtkListBase`'s select-and-move semantics (Up/Down move the focus and
/// select only it, resetting a multi-selection) — accepted behavior for the
/// list; the multi-select paths are clicks, the rubber band, Ctrl+A and
/// Space on the focused cell.
fn toggle_entry(selection: &gtk4::MultiSelection, position: u32) {
    if position == gtk4::INVALID_LIST_POSITION {
        return;
    }
    if selection.is_selected(position) {
        selection.unselect_item(position);
    } else {
        selection.select_item(position, false);
    }
}

/// Inter-cell gaps live in CSS on the item node: `GtkListItem` is not a
/// widget, so margins can't be set from Rust, and themes paint the chip on
/// the `gridview > child` node which spans the whole cell — inner-box
/// margins only inset content inside the chip (beta.16 report). 3px on all
/// sides = 6px gap between cells, the 6px-per-step GNOME spacing rhythm.
/// Loaded once per display on first grid creation.
fn ensure_gap_css() {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if ONCE.set(()).is_ok() {
        let provider = gtk4::CssProvider::new();
        provider.load_from_bytes(&glib::Bytes::from_static(
            b"gridview > child { margin: 3px; }",
        ));
        gtk4::style_context_add_provider_for_display(
            &gdk::Display::default().expect("grid gap CSS: no display"),
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

impl GalleryGrid {
    /// Empty grid; entries arrive later via `append` (streamed enumeration).
    pub(crate) fn new() -> Self {
        ensure_gap_css();
        let store = gtk4::StringList::new(&[]);
        let selection = gtk4::MultiSelection::new(Some(store.clone()));
        // Let the grid request its natural height for small selections. The
        // cap makes long collections scroll inside the picker rather than
        // stretching the whole card to the window's available height.
        let scrolled = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(MAX_PICKER_HEIGHT)
            .build();

        let factory = gtk4::SignalListItemFactory::new();
        // Selection watches live on the long-lived model; teardown disconnects
        // each cell's watch so a view that recreates cells doesn't accumulate
        // dead handlers.
        let selection_handlers: Rc<RefCell<HashMap<usize, glib::SignalHandlerId>>> =
            Rc::new(RefCell::new(HashMap::new()));
        // Per realized cell: one row of widgets, a toggle gesture and one
        // selection-changed watch (disconnected at teardown).
        {
            let selection = selection.clone();
            let handlers = Rc::clone(&selection_handlers);
            factory.connect_setup(move |_, object| {
                let list_item = object
                    .downcast_ref::<gtk4::ListItem>()
                    .expect("setup handler gets a ListItem");
                let label = gtk4::Label::builder()
                    .ellipsize(pango::EllipsizeMode::End)
                    .max_width_chars(24)
                    .xalign(0.0)
                    .hexpand(true)
                    .build();
                // Selection state lives in the model; the check only mirrors
                // it, so pointer events pass through to the cell unchanged
                // and clicks land on the gesture below.
                let check = gtk4::CheckButton::new();
                check.set_can_target(false);
                check.set_focusable(false);
                // The margins keep focus rings from clipping at the scroll
                // edges.
                let cell = gtk4::Box::builder()
                    .orientation(gtk4::Orientation::Horizontal)
                    .spacing(6)
                    .margin_top(2)
                    .margin_bottom(2)
                    .focusable(true)
                    .build();
                cell.append(&check);
                cell.append(&label);
                list_item.set_child(Some(&cell));

                let item = glib::WeakRef::<gtk4::ListItem>::new();
                item.set(Some(list_item));
                let check_weak = glib::WeakRef::<gtk4::CheckButton>::new();
                check_weak.set(Some(&check));

                // One press toggles the cell through the model and claims
                // the sequence, so GridView's own click (a selection
                // replacement) never gets it; a rubber band can only start
                // from empty space, since a claimed sequence can't begin a
                // drag. Presses with n_press > 1 only claim — a stray
                // double-click must not replace the whole selection.
                let gesture = gtk4::GestureClick::new();
                {
                    let selection = selection.clone();
                    let item = item.clone();
                    gesture.connect_pressed(move |gesture, n_press, _, _| {
                        gesture.set_state(gtk4::EventSequenceState::Claimed);
                        if n_press > 1 {
                            return;
                        }
                        toggle_entry(&selection, item_position(&item));
                    });
                }
                cell.add_controller(gesture);

                // Watches the model so Ctrl+A, the rubber band, keyboard and
                // button actions refresh the visible check, not just the
                // toggled cell. One watch per realized cell; detached rows
                // (weak item gone or position INVALID) are skipped.
                {
                    let item = item.clone();
                    let check_weak = check_weak.clone();
                    let id = selection.connect_selection_changed(move |model, _, _| {
                        if let Some(li) = item.upgrade()
                            && let Some(check) = check_weak.upgrade()
                        {
                            let position = li.position();
                            if position != gtk4::INVALID_LIST_POSITION {
                                check.set_active(model.is_selected(position));
                            }
                        }
                    });
                    handlers
                        .borrow_mut()
                        .insert(list_item.as_ptr() as usize, id);
                }
            });
        }

        // Titles (text, tooltip, accessible name) are set here, explicitly:
        // no expression watches, no lifetime dependencies on how long the
        // bind runs.
        {
            let selection = selection.clone();
            factory.connect_bind(move |_, object| {
                let list_item = object
                    .downcast_ref::<gtk4::ListItem>()
                    .expect("bind handler gets a ListItem");
                let Some(title) = list_item
                    .item()
                    .and_downcast::<gtk4::StringObject>()
                    .map(|o| o.string())
                else {
                    return;
                };
                let cell = list_item
                    .child()
                    .and_downcast::<gtk4::Box>()
                    .expect("gallery cell: setup built a Box child");
                let check = cell
                    .first_child()
                    .and_downcast::<gtk4::CheckButton>()
                    .expect("gallery cell: first child is the check button");
                let label = check
                    .next_sibling()
                    .and_downcast::<gtk4::Label>()
                    .expect("gallery cell: label follows the check button");
                label.set_text(&title);
                label.set_tooltip_text(Some(title.as_str()));
                cell.update_property(&[gtk4::accessible::Property::Label(title.as_str())]);
                check.set_active(selection.is_selected(list_item.position()));
            });
        }

        {
            let selection = selection.clone();
            let handlers = Rc::clone(&selection_handlers);
            factory.connect_teardown(move |_, object| {
                let Some(list_item) = object.downcast_ref::<gtk4::ListItem>() else {
                    return;
                };
                if let Some(id) = handlers.borrow_mut().remove(&(list_item.as_ptr() as usize)) {
                    selection.disconnect(id);
                }
            });
        }

        let view = gtk4::GridView::builder()
            .model(&selection)
            .factory(&factory)
            // Three columns max; the default 7 split the width into
            // slivers and ellipsized every title to a few characters.
            .max_columns(3)
            .enable_rubberband(true)
            // The click gesture does the selecting; activation makes no
            // sense in a picker.
            .single_click_activate(false)
            .build();

        // Return/ISO/KP over the grid surface is consumed by the focused
        // item widget's own activate binding before any BUBBLE-phase
        // controller could see it — so the view's `activate` signal is the
        // live Enter path, and it ends up toggling like the click does.
        // Space keeps its item-widget toggle binding, which changes the
        // selection straight through the model. `single_click_activate` is
        // off and the cell gesture denies double-click sequences, so
        // activation is keyboard-only here.
        {
            let selection = selection.clone();
            view.connect_activate(move |_, position| toggle_entry(&selection, position));
        }

        scrolled.set_child(Some(&view));

        GalleryGrid {
            inner: Rc::new(Inner {
                store,
                selection,
                scrolled,
                count: RefCell::new(None),
            }),
        }
    }

    /// Append titles (each arrives selected), e.g. as enumeration streams.
    pub(crate) fn append(&self, titles: &[String]) {
        if titles.is_empty() {
            return;
        }
        let start = self.inner.store.n_items();
        let additions: Vec<&str> = titles.iter().map(String::as_str).collect();
        self.inner.store.splice(start, 0, &additions);
        // The rest of the selection is untouched: a batch already streaming
        // in must not reset the user's current pick.
        self.inner
            .selection
            .select_range(start, titles.len() as u32, false);
    }

    /// The widget to append into the picker page's box.
    pub(crate) fn widget(&self) -> &gtk4::ScrolledWindow {
        &self.inner.scrolled
    }

    /// Entry count (not selection count).
    pub(crate) fn len(&self) -> usize {
        self.inner.store.n_items() as usize
    }

    /// Selected entry indices, ascending, in store order == enumeration
    /// order (the shifted-index caveat is documented at the `--range`
    /// mapping in `inline_gallery`).
    pub(crate) fn selected_indices(&self) -> Vec<usize> {
        let bitset = self.inner.selection.selection();
        (0..bitset.size())
            .map(|i| bitset.nth(i as u32) as usize)
            .collect()
    }

    /// Unselect one entry (a queued row after a partial playlist import, so
    /// a retry submits only the remainder).
    pub(crate) fn unselect(&self, index: usize) {
        if (index as u32) < self.inner.store.n_items() {
            self.inner.selection.unselect_item(index as u32);
        }
    }

    /// A selection-state consumer hook: called for every selection change
    /// (clicks, Ctrl+A, the rubber band, keyboard, the buttons).
    pub(crate) fn connect_selection_changed(&self, f: impl Fn(usize) + 'static) {
        // Weak from here on: the handler is stored on `selection`, which
        // `Inner` owns — a strong `Rc<Inner>` capture would leak the whole
        // page subtree per picker open.
        let inner = Rc::downgrade(&self.inner);
        self.inner
            .selection
            .connect_selection_changed(move |_, _, _| {
                if let Some(inner) = inner.upgrade() {
                    f(selected_count(&inner.selection));
                }
            });
    }

    /// Wire the bottom action bar: live selected-count label plus Select
    /// All/None.
    pub(crate) fn wire_selection_bar(
        &self,
        select_all_btn: &gtk4::Button,
        select_none_btn: &gtk4::Button,
        add_btn: &gtk4::Button,
        count_label: impl Fn(usize) -> String + 'static,
    ) {
        let add_btn_weak = glib::WeakRef::new();
        add_btn_weak.set(Some(add_btn));
        let count_label = Rc::new(count_label);
        *self.inner.count.borrow_mut() = Some((add_btn_weak, count_label));

        // The queued count follows every selection change (weak capture —
        // the handler list lives on the selection inside Inner).
        {
            let inner = Rc::downgrade(&self.inner);
            self.connect_selection_changed(move |_| {
                if let Some(inner) = inner.upgrade() {
                    inner_count_bump(&inner);
                }
            });
        }
        // First paint: no selection-changed has fired for existing entries.
        inner_count_bump(&self.inner);
        {
            let this = self.clone();
            select_all_btn.connect_clicked(move |_| this.select_all());
        }
        {
            let this = self.clone();
            select_none_btn.connect_clicked(move |_| this.select_none());
        }
    }

    /// Select every entry.
    pub(crate) fn select_all(&self) {
        self.inner.selection.select_all();
    }

    /// Unselect every entry.
    pub(crate) fn select_none(&self) {
        self.inner.selection.unselect_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A display is needed to realize cells: the tests are `#[ignore]`d for
    // the plain run; CI runs them under Xvfb (the "UI tests (Xvfb)" step in
    // ci.yml), which also greps the log for Gtk-CRITICAL floods.
    #[test]
    #[ignore = "needs a display; run under Xvfb (see ci.yml)"]
    fn picker_grid_gallery_selection() {
        gtk4::init().expect("gtk::init (run under Xvfb)");
        selection_semantics();
        append_keeps_existing_selection();
        empty_grid();
        cells_recycle_and_count_follows();
    }

    fn selection_semantics() {
        let grid = GalleryGrid::new();
        grid.append(&[
            "item-0".to_string(),
            "item-1".to_string(),
            "item-2".to_string(),
        ]);
        // All-on default for streamed batches.
        assert_eq!(grid.selected_indices(), vec![0, 1, 2]);
        // A single unselect (the click path) keeps the neighbors; the
        // out-of-range guard of the wrapper is a no-op.
        grid.unselect(0);
        grid.inner.selection.unselect_item(1);
        grid.unselect(usize::MAX);
        assert_eq!(grid.selected_indices(), vec![2]);
        grid.inner.selection.select_item(1, false);
        assert_eq!(grid.selected_indices(), vec![1, 2]);
    }

    fn append_keeps_existing_selection() {
        let grid = GalleryGrid::new();
        grid.append(&["item-0".to_string(), "item-1".to_string()]);
        grid.inner.selection.unselect_item(0);
        grid.append(&["late".to_string()]);
        // The new entry arrives selected; the existing choice is untouched.
        assert_eq!(grid.selected_indices(), vec![1, 2]);
    }

    fn empty_grid() {
        let grid = GalleryGrid::new();
        // Small pickers hug the entries; long ones scroll within a bounded
        // viewport rather than claiming the full page height.
        assert!(!grid.widget().property::<bool>("vexpand"));
        assert!(grid.widget().propagates_natural_height());
        assert_eq!(grid.widget().max_content_height(), MAX_PICKER_HEIGHT);
        assert_eq!(grid.len(), 0);
        assert!(grid.selected_indices().is_empty());
        grid.append(&["only".to_string()]);
        assert_eq!(grid.selected_indices(), vec![0]);
        grid.select_none();
        assert!(grid.selected_indices().is_empty());
        grid.select_all();
        assert_eq!(grid.selected_indices(), vec![0]);
    }

    /// Stress the recycling point with 10,000 entries: realized cells must
    /// stay bounded and the queued count must follow the complete model.
    fn cells_recycle_and_count_follows() {
        let grid = GalleryGrid::new();
        let titles: Vec<String> = (0..10_000).map(|i| format!("item-{i}")).collect();
        grid.append(&titles);
        assert_eq!(grid.len(), 10_000);

        let add_btn = gtk4::Button::new();
        let select_all = gtk4::Button::new();
        let select_none = gtk4::Button::new();
        grid.wire_selection_bar(&select_all, &select_none, &add_btn, |n| {
            format!("{n} selected")
        });
        assert_eq!(add_btn.label().as_deref(), Some("10000 selected"));

        // The picker page's nesting: captions live outside the grid's
        // scroll area, and the grid keeps its natural height up to its cap.
        let page = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        page.set_vexpand(true);
        page.append(grid.widget());
        let window = gtk4::Window::new();
        window.set_default_size(480, 600);
        window.set_child(Some(&page));
        window.present();
        settle();

        let cells = collect_check_buttons(grid.widget().upcast_ref::<gtk4::Widget>());
        println!("smoke: {} cells realized for 10000 entries", cells.len());
        assert!(
            !cells.is_empty(),
            "cells must be realized for the visible rows"
        );
        assert!(
            cells.len() < 10_000,
            "GridView must recycle: {} cells for 10000 entries",
            cells.len()
        );
        let bound_titles: Vec<String> = cells
            .iter()
            .filter_map(|check| {
                check
                    .parent()
                    .and_downcast::<gtk4::Box>()?
                    .first_child()
                    .and_then(|w| w.next_sibling())
                    .and_downcast::<gtk4::Label>()
                    .map(|label| label.text().to_string())
            })
            .collect();
        assert!(
            bound_titles.iter().any(|t| t.starts_with("item-")),
            "a realized cell must show its bound title: {bound_titles:?}"
        );

        // Three columns max, stretched across the row: at this 480px window
        // each cell is ~160px and the label keeps most of it. The 7-column
        // default squeezed the label into a sliver of its own cell.
        let first_cell = cells[0]
            .parent()
            .and_downcast::<gtk4::Box>()
            .expect("a realized cell box");
        let first_label = first_cell
            .first_child()
            .and_then(|w| w.next_sibling())
            .and_downcast::<gtk4::Label>()
            .expect("a realized cell with a title label");
        assert!(
            first_label.width() * 2 > first_cell.width(),
            "the title keeps most of its cell: {} of {}",
            first_label.width(),
            first_cell.width()
        );

        // Click-equivalent path (the model toggle the gesture performs); the
        // queued count follows.
        grid.inner.selection.unselect_item(0);
        settle();
        assert_eq!(add_btn.label().as_deref(), Some("9999 selected"));

        // Button path: Select All/None through the model.
        select_none.emit_clicked();
        settle();
        assert_eq!(add_btn.label().as_deref(), Some("0 selected"));
        assert!(grid.selected_indices().is_empty());
        select_all.emit_clicked();
        settle();
        assert_eq!(add_btn.label().as_deref(), Some("300 selected"));

        // A late batch arrives selected with an existing pick untouched:
        // item-0 is out, the two late items are in.
        grid.inner.selection.unselect_item(0);
        grid.append(&["late-a".to_string(), "late-b".to_string()]);
        settle();
        assert_eq!(grid.len(), 302);
        assert_eq!(add_btn.label().as_deref(), Some("301 selected"));
        assert!(!grid.selected_indices().contains(&0));
    }

    /// Pump the main loop for a bounded time: enough for layout to settle,
    /// but it can never hang CI.
    fn settle() {
        let ctx = glib::MainContext::default();
        for _ in 0..200 {
            // Bounded too: a source that stays ready without draining would
            // otherwise spin here forever.
            for _ in 0..1000 {
                if !ctx.pending() {
                    break;
                }
                ctx.iteration(false);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// Every CheckButton in the widget tree, in tree order (one per
    /// realized cell).
    fn collect_check_buttons(widget: &gtk4::Widget) -> Vec<gtk4::CheckButton> {
        let mut out = Vec::new();
        if let Some(btn) = widget.downcast_ref::<gtk4::CheckButton>() {
            out.push(btn.clone());
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            out.extend(collect_check_buttons(&c));
            child = c.next_sibling();
        }
        out
    }
}
