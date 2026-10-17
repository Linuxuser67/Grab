//! Virtualized picker grid shared by the gallery, playlist and torrent
//! pickers: a `GtkGridView` over a `GioListStore` of `StringObject`s, with a
//! `GtkMultiSelection` as the single source of truth. Cost follows the
//! visible cells, not the entry count (the old WrapBox built one widget per
//! entry and enumeration was capped at 500 because of it).

use gtk4::prelude::*;
use gtk4::{gio, pango};
use std::rc::Rc;

struct Inner {
    store: gio::ListStore,
    selection: gtk4::MultiSelection,
    grid: gtk4::GridView,
}

/// Cloneable handle to the picker widgets (Rc inside, clone is cheap).
#[derive(Clone)]
pub(crate) struct PickerGrid {
    inner: Rc<Inner>,
}

/// Selected indices, ascending. O(n), called once per enqueue click.
fn selected_indices(selection: &gtk4::MultiSelection) -> Vec<usize> {
    (0..selection.n_items())
        .filter(|i| selection.is_selected(*i))
        .map(|i| i as usize)
        .collect()
}

/// Append titles, all selected: pickers are all-on by default and streamed
/// items must arrive selected too. One `select_range` for the batch, so one
/// signal, not one per entry.
fn append_titles(store: &gio::ListStore, selection: &gtk4::MultiSelection, titles: Vec<String>) {
    let n = titles.len() as u32;
    if n == 0 {
        return;
    }
    let start = store.n_items();
    let objs: Vec<gtk4::StringObject> = titles.iter().map(|t| gtk4::StringObject::new(t)).collect();
    // One splice = one items-changed signal for the whole batch.
    store.splice(start, 0, &objs[..]);
    selection.select_range(start, n, false);
}

impl PickerGrid {
    /// Grid over a fixed entry list, everything selected.
    pub(crate) fn new(titles: Vec<String>) -> Self {
        let this = Self::empty();
        this.append(titles);
        this
    }

    /// Empty grid; entries arrive later via `append` (streamed enumeration).
    pub(crate) fn empty() -> Self {
        let store = gio::ListStore::new::<gtk4::StringObject>();
        let selection = gtk4::MultiSelection::new(Some(store.clone()));
        let grid = build_grid(&selection);
        PickerGrid {
            inner: Rc::new(Inner {
                store,
                selection,
                grid,
            }),
        }
    }

    /// Append titles (each arrives selected), e.g. as enumeration streams.
    pub(crate) fn append(&self, titles: Vec<String>) {
        append_titles(&self.inner.store, &self.inner.selection, titles);
    }

    /// The widget to append into the picker page's box.
    pub(crate) fn widget(&self) -> &gtk4::GridView {
        &self.inner.grid
    }

    /// Entry count (not selection count).
    pub(crate) fn len(&self) -> usize {
        self.inner.store.n_items() as usize
    }

    /// Selected entry indices, ascending.
    pub(crate) fn selected_indices(&self) -> Vec<usize> {
        selected_indices(&self.inner.selection)
    }

    /// Unselect one entry (a queued row after a partial playlist import, so a
    /// retry submits only the remainder).
    pub(crate) fn unselect(&self, index: usize) {
        self.inner.selection.unselect_item(index as u32);
    }

    /// Wire the bottom action bar: live count label plus Select All/None.
    pub(crate) fn wire_selection_bar(
        &self,
        select_all_btn: &gtk4::Button,
        select_none_btn: &gtk4::Button,
        add_btn: &gtk4::Button,
        count_label: impl Fn(usize) -> String + 'static,
    ) {
        let count_label = Rc::new(count_label);
        // First paint: no selection-changed signal has fired yet.
        let n = self.inner.selection.selection().size() as usize;
        add_btn.set_label(&count_label(n));
        {
            let count_label = Rc::clone(&count_label);
            // Weak, not strong: the button's own click handler holds the grid,
            // so a strong reference here would close a cycle and keep the
            // whole page alive after the picker window is popped.
            let add_btn = add_btn.downgrade();
            // The selection model is the single source of truth: one signal
            // per change, one label write; no per-pill bookkeeping.
            self.inner
                .selection
                .connect_selection_changed(move |selection, _, _| {
                    let Some(add_btn) = add_btn.upgrade() else {
                        return;
                    };
                    let n = selection.selection().size() as usize;
                    add_btn.set_label(&count_label(n));
                });
        }
        {
            let selection = self.inner.selection.clone();
            select_all_btn.connect_clicked(move |_| {
                selection.select_all();
            });
        }
        {
            let selection = self.inner.selection.clone();
            select_none_btn.connect_clicked(move |_| {
                selection.unselect_all();
            });
        }
    }
}

fn build_grid(selection: &gtk4::MultiSelection) -> gtk4::GridView {
    let factory = gtk4::SignalListItemFactory::new();
    let setup_selection = selection.clone();
    factory.connect_setup(move |_, list_item| {
        let list_item = list_item.downcast_ref::<gtk4::ListItem>().unwrap();
        // Rows are not selectable by the user: clicks belong to the pills.
        // Without this, the view's own click gesture would apply
        // MultiSelection's replace-the-whole-selection semantics to every
        // click that lands on the row instead of a pill.
        list_item.set_selectable(false);
        // The pill's label is ours: ellipsize once here, not on every bind.
        let label = gtk4::Label::new(None);
        label.set_ellipsize(pango::EllipsizeMode::End);
        label.set_max_width_chars(24);
        let btn = gtk4::ToggleButton::new();
        btn.set_child(Some(&label));
        // The view mirrors `ListItem:selected` from the selection model on
        // every bind, so this one-time binding covers recycling, Select
        // All/None and streamed appends. Both objects share the cell's
        // lifetime: nothing to tear down, nothing accumulates on rebind.
        list_item
            .bind_property("selected", &btn, "active")
            .sync_create()
            .build();
        let weak_selection = setup_selection.downgrade();
        let weak_item = list_item.downgrade();
        btn.connect_toggled(move |btn| {
            let (Some(selection), Some(list_item)) =
                (weak_selection.upgrade(), weak_item.upgrade())
            else {
                return;
            };
            // Unbound (recycled) cells have no position to write.
            if list_item.item().is_none() {
                return;
            }
            let pos = list_item.position();
            if btn.is_active() == selection.is_selected(pos) {
                // The binding synced the pill from the model: nothing to
                // write back.
                return;
            }
            if btn.is_active() {
                selection.select_item(pos, false);
            } else {
                selection.unselect_item(pos);
            }
        });
        list_item.set_child(Some(&btn));
    });
    factory.connect_bind(|_, list_item| {
        let list_item = list_item.downcast_ref::<gtk4::ListItem>().unwrap();
        let title = list_item
            .item()
            .and_downcast::<gtk4::StringObject>()
            .expect("store holds StringObjects")
            .string();
        let btn = list_item
            .child()
            .and_downcast::<gtk4::ToggleButton>()
            .unwrap();
        // Titles are untrusted (they carry `&`, `<`, …): Button label is
        // plain text, never markup.
        if let Some(label) = btn.child().and_downcast::<gtk4::Label>() {
            label.set_label(&title);
        }
    });
    // Pills are the only selection UI: rubberband selection writes straight
    // to the model (it does not respect `selectable`), so a stray drag would
    // rewrite the choice across whole rows.
    // ponytail: GridView cells are uniform (the widest title sets the width)
    // and reflow by available width; the WrapBox sized each pill to its own
    // title. Pin max_columns here if beta shows the wrap is off.
    gtk4::GridView::builder()
        .enable_rubberband(false)
        .model(selection)
        .factory(&factory)
        .build()
}

/// The picker pages' nesting: the grid goes straight into a capped scrolled
/// window (the list-widget docs' placement, so cells stay virtualized), and
/// the captions stay outside so only the entries scroll. valign=Start (not
/// the Fill default): the ToolbarView would otherwise stretch the scrolled
/// window to the full content height, leaving empty space below a short list.
pub(crate) fn scrolled_grid(grid: &gtk4::GridView) -> gtk4::ScrolledWindow {
    gtk4::ScrolledWindow::builder()
        .child(grid)
        .propagate_natural_height(true)
        .max_content_height(480)
        .vexpand(false)
        .valign(gtk4::Align::Start)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk4::glib;

    /// Pill text now lives on the child label (see `build_grid`).
    fn pill_title(b: &gtk4::ToggleButton) -> Option<String> {
        b.child()
            .and_downcast::<gtk4::Label>()
            .map(|l| l.label().to_string())
    }

    // `GtkMultiSelection::new` asserts that GTK is initialized, so the test
    // here needs a display and is `#[ignore]`d: CI runs it under Xvfb (the
    // "UI tests (Xvfb)" step in ci.yml), the plain run skips it.
    fn store_and_selection(n: usize) -> (gio::ListStore, gtk4::MultiSelection) {
        let store = gio::ListStore::new::<gtk4::StringObject>();
        let selection = gtk4::MultiSelection::new(Some(store.clone()));
        append_titles(
            &store,
            &selection,
            (0..n).map(|i| format!("item-{i}")).collect(),
        );
        (store, selection)
    }

    // libtest gives every `#[test]` its own thread, and `gtk4::init` may run
    // only once per process (a later call panics "GTK from two different
    // threads"), so all scenarios share this one entry point and one thread.
    #[test]
    #[ignore = "needs a display; run under Xvfb (see ci.yml)"]
    fn picker_grid() {
        gtk4::init().expect("gtk::init (run under Xvfb)");
        selection_semantics();
        append_keeps_existing_selection();
        empty_store();
        ui_smoke();
    }

    fn selection_semantics() {
        let (_, selection) = store_and_selection(3);
        // All-on default comes from append_titles.
        assert_eq!(selected_indices(&selection), vec![0, 1, 2]);
        selection.unselect_all();
        assert!(selected_indices(&selection).is_empty());
        // Pill clicks must add/remove without clearing the rest.
        selection.select_item(1, false);
        assert_eq!(selected_indices(&selection), vec![1]);
        selection.select_item(2, false);
        assert_eq!(selected_indices(&selection), vec![1, 2]);
        selection.unselect_item(1);
        assert_eq!(selected_indices(&selection), vec![2]);
        selection.select_all();
        assert_eq!(selected_indices(&selection), vec![0, 1, 2]);
    }

    fn append_keeps_existing_selection() {
        let (store, selection) = store_and_selection(2);
        selection.unselect_item(0);
        append_titles(&store, &selection, vec!["late".to_string()]);
        // The new entry arrives selected; the existing choice is untouched.
        assert_eq!(selected_indices(&selection), vec![1, 2]);
    }

    fn empty_store() {
        let (store, selection) = store_and_selection(0);
        assert_eq!(store.n_items(), 0);
        selection.select_all();
        assert!(selected_indices(&selection).is_empty());
    }

    /// UI smoke: realized cells bind both ways, and the capped layout keeps
    /// a long list from expanding (i.e. it stays virtualized).
    fn ui_smoke() {
        let titles: Vec<String> = (0..5).map(|i| format!("item-{i}")).collect();
        let grid = PickerGrid::new(titles);
        let add_btn = gtk4::Button::new();
        let select_all = gtk4::Button::new();
        let select_none = gtk4::Button::new();
        grid.wire_selection_bar(&select_all, &select_none, &add_btn, |n| {
            format!("{n} selected")
        });
        assert_eq!(add_btn.label().as_deref(), Some("5 selected"));

        // The picker pages' nesting: captions around a scrolled window
        // wrapped directly around the grid (the list-widget docs' placement,
        // so cells stay virtualized).
        let scrolled = scrolled_grid(grid.widget());
        let page = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        page.append(&gtk4::Label::new(Some("note")));
        page.append(&scrolled);
        page.append(&gtk4::Label::new(Some("caption")));
        let window = gtk4::Window::new();
        window.set_default_size(480, 600);
        window.set_child(Some(&page));
        window.present();
        settle();

        let buttons = collect_toggle_buttons(grid.widget().upcast_ref::<gtk4::Widget>());
        assert!(!buttons.is_empty(), "grid must realize visible cells");
        assert!(buttons.iter().all(|b| b.is_active()), "all-on default");
        // A handful of titles must not stretch the card to the 480 cap.
        println!("smoke: 5-entry scrolled height = {}", scrolled.height());
        assert!(scrolled.height() > 0, "card height must be measured");
        assert!(
            scrolled.height() < 480,
            "5 files should not stretch the card: h={}",
            scrolled.height()
        );

        // Click path: pill -> selection model -> count sink.
        let first = buttons
            .iter()
            .find(|b| pill_title(b).as_deref() == Some("item-0"))
            .expect("item-0 pill realized");
        first.set_active(false);
        settle();
        assert_eq!(grid.selected_indices(), vec![1, 2, 3, 4]);
        assert_eq!(add_btn.label().as_deref(), Some("4 selected"));

        // Button path (Select All/None -> selection model -> ListItem:selected
        // -> pills), the real action-bar wiring.
        select_none.emit_clicked();
        settle();
        assert!(
            collect_toggle_buttons(grid.widget().upcast_ref::<gtk4::Widget>())
                .iter()
                .all(|b| !b.is_active()),
            "unselected items must unset visible pills"
        );
        assert_eq!(add_btn.label().as_deref(), Some("0 selected"));
        select_all.emit_clicked();
        settle();
        assert_eq!(add_btn.label().as_deref(), Some("5 selected"));

        // A long list must cap at the scrolled window and keep virtualizing
        // instead of expanding every cell.
        grid.append((5..300).map(|i| format!("item-{i}")).collect());
        settle();
        println!("smoke: 300-entry scrolled height = {}", scrolled.height());
        assert_eq!(add_btn.label().as_deref(), Some("300 selected"));
        assert!(scrolled.height() > 0, "card height must be measured");
        assert!(
            scrolled.height() <= 480,
            "the scrolled window must cap the grid: h={}",
            scrolled.height()
        );
        assert!(
            collect_toggle_buttons(grid.widget().upcast_ref::<gtk4::Widget>()).len() < 300,
            "only visible cells may be realized"
        );

        // Recycling: scrolling rebinds realized cells to new positions, the
        // case the toggled-position guard exists for. Scroll to the bottom
        // and check a rebound pill both shows and writes the right index.
        let vadj = scrolled.vadjustment();
        vadj.set_value(vadj.upper());
        settle();
        let last = collect_toggle_buttons(grid.widget().upcast_ref::<gtk4::Widget>())
            .into_iter()
            .find(|b| pill_title(b).as_deref() == Some("item-299"))
            .expect("bottom row must realize after scrolling");
        assert!(last.is_active(), "rebound cell must show item-299 selected");
        last.set_active(false);
        settle();
        assert_eq!(add_btn.label().as_deref(), Some("299 selected"));
        assert!(!grid.selected_indices().contains(&299));
    }

    /// Pump the main loop for a bounded time: enough for the view to lay out
    /// and bind cells, but it can never hang CI.
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

    /// Every ToggleButton in the widget tree, in tree order.
    fn collect_toggle_buttons(widget: &gtk4::Widget) -> Vec<gtk4::ToggleButton> {
        let mut out = Vec::new();
        if let Some(btn) = widget.downcast_ref::<gtk4::ToggleButton>() {
            out.push(btn.clone());
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            out.extend(collect_toggle_buttons(&c));
            child = c.next_sibling();
        }
        out
    }
}
