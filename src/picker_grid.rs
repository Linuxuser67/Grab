//! Picker pill grid shared by the gallery, playlist and torrent pickers: an
//! `AdwWrapBox` of `GtkToggleButton`s, one per entry, each sized to its own
//! text (children wrap like words in a label). Cost follows the entry count,
//! so both enumeration paths stay bounded: playlists at
//! `MAX_PLAYLIST_ITEMS`, galleries at `ENUMERATE_MAX_FILES` (the previously
//! shipped caps before the recycled GridView experiment).
//!
//! The pills are the selection state: active == selected. No
//! `GtkMultiSelection` underneath — with per-text pills there is no
//! recycling left to mirror it to.

use adw::prelude::*;
use gtk4::prelude::*;
use gtk4::{glib, pango};
use std::cell::RefCell;
use std::rc::Rc;

struct Inner {
    /// One pill per entry, in entry order; index == entry index.
    pills: RefCell<Vec<gtk4::ToggleButton>>,
    wrap: adw::WrapBox,
    /// (weak add button, count label formatter) once wired.
    count: RefCell<Option<(glib::WeakRef<gtk4::Button>, Rc<dyn Fn(usize) -> String>)>>,
}

/// Cloneable handle to the picker widgets (Rc inside, clone is cheap).
#[derive(Clone)]
pub(crate) struct PickerGrid {
    inner: Rc<Inner>,
}

fn pill_count(pills: &[gtk4::ToggleButton]) -> usize {
    pills.iter().filter(|b| b.is_active()).count()
}

/// Refresh the wired count label from the pills.
fn inner_count_bump(inner: &Inner) {
    if let Some((add_btn, count_label)) = inner.count.borrow().as_ref() {
        if let Some(add_btn) = add_btn.upgrade() {
            let n = pill_count(&inner.pills.borrow());
            add_btn.set_label(&count_label(n));
        }
    }
}

/// One pill, all-on default: pickers are all-on and streamed items must
/// arrive selected. `set_active` happens before the toggled handler is
/// connected, so construction emits nothing; the count bumps once per
/// batch in `append`.
fn pill(title: &str) -> gtk4::ToggleButton {
    let label = gtk4::Label::builder()
        .label(title)
        .ellipsize(pango::EllipsizeMode::End)
        .max_width_chars(24)
        .build();
    let btn = gtk4::ToggleButton::builder()
        .child(&label)
        .active(true)
        .build();
    // Titles are untrusted (they carry `&`, `<`, …): Button label is plain
    // text, never markup.
    btn.set_tooltip_text(Some(title));
    btn
}

impl PickerGrid {
    /// Pills over a fixed entry list, everything selected.
    pub(crate) fn new(titles: Vec<String>) -> Self {
        let this = Self::empty();
        this.append(titles);
        this
    }

    /// Empty picker; entries arrive later via `append` (streamed enumeration).
    pub(crate) fn empty() -> Self {
        let wrap = adw::WrapBox::builder()
            .child_spacing(6)
            .line_spacing(6)
            .build();
        PickerGrid {
            inner: Rc::new(Inner {
                pills: RefCell::new(Vec::new()),
                wrap,
                count: RefCell::new(None),
            }),
        }
    }

    /// Append titles (each arrives selected), e.g. as enumeration streams.
    pub(crate) fn append(&self, titles: Vec<String>) {
        let new: Vec<gtk4::ToggleButton> = titles
            .iter()
            .map(|t| {
                let btn = pill(t);
                let inner = self.inner.clone();
                btn.connect_toggled(move |_| inner_count_bump(&inner));
                btn
            })
            .collect();
        for btn in &new {
            self.inner.wrap.append(btn);
        }
        self.inner.pills.borrow_mut().extend(new);
        inner_count_bump(&self.inner);
    }

    /// The widget to append into the picker page's box.
    pub(crate) fn widget(&self) -> &adw::WrapBox {
        &self.inner.wrap
    }

    /// Entry count (not selection count).
    pub(crate) fn len(&self) -> usize {
        self.inner.pills.borrow().len()
    }

    /// Selected entry indices, ascending.
    pub(crate) fn selected_indices(&self) -> Vec<usize> {
        let pills = self.inner.pills.borrow();
        (0..pills.len()).filter(|&i| pills[i].is_active()).collect()
    }

    /// Unselect one entry (a queued row after a partial playlist import, so a
    /// retry submits only the remainder).
    pub(crate) fn unselect(&self, index: usize) {
        if let Some(b) = self.inner.pills.borrow().get(index) {
            // Not guarded: the toggled handler bumps the count, which is
            // exactly what this path wants.
            b.set_active(false);
        }
    }

    /// Wire the bottom action bar: live count label plus Select All/None.
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

        // First paint: no toggled signal has fired yet.
        inner_count_bump(&self.inner);
        {
            let this = self.clone();
            select_all_btn.connect_clicked(move |_| {
                let pills = this.inner.pills.borrow();
                for btn in pills.iter() {
                    if !btn.is_active() {
                        // Each change fires toggled -> count bump.
                        btn.set_active(true);
                    }
                }
            });
        }
        {
            let this = self.clone();
            select_none_btn.connect_clicked(move |_| {
                let pills = this.inner.pills.borrow();
                for btn in pills.iter() {
                    if btn.is_active() {
                        btn.set_active(false);
                    }
                }
            });
        }
    }
}

/// The picker pages' nesting: the pills go straight into a capped scrolled
/// window, and the captions stay outside so only the entries scroll. The
/// WrapBox reports its honest wrapped height, so propagate-natural grows and
/// shrinks the card with the entry count. valign=Start (not the Fill
/// default): the ToolbarView would otherwise stretch the scrolled window to
/// the full content height, leaving empty space below a short list.
pub(crate) fn scrolled_grid<W: IsA<gtk4::Widget>>(grid: &W) -> gtk4::ScrolledWindow {
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

    /// Pill text lives on the child label.
    fn pill_title(b: &gtk4::ToggleButton) -> Option<String> {
        b.child()
            .and_downcast::<gtk4::Label>()
            .map(|l| l.label().to_string())
    }

    // Pills need a display for layout: the tests are `#[ignore]`d for the
    // plain run; CI runs them under Xvfb (the "UI tests (Xvfb)" step in
    // ci.yml), which also greps the log for Gtk-CRITICAL floods.
    #[test]
    #[ignore = "needs a display; run under Xvfb (see ci.yml)"]
    fn picker_grid() {
        gtk4::init().expect("gtk::init (run under Xvfb)");
        selection_semantics();
        append_keeps_existing_selection();
        empty_grid();
        pills_fit_their_text();
        ui_smoke();
    }

    fn selection_semantics() {
        let grid = PickerGrid::new(vec![
            "item-0".to_string(),
            "item-1".to_string(),
            "item-2".to_string(),
        ]);
        // All-on default.
        assert_eq!(grid.selected_indices(), vec![0, 1, 2]);
        // Toggling adds/removes without clearing the rest.
        grid.unselect(0);
        grid.unselect(1);
        assert_eq!(grid.selected_indices(), vec![2]);
        let pills = grid.inner.pills.borrow();
        pills[1].set_active(true);
        assert_eq!(grid.selected_indices(), vec![1, 2]);
        drop(pills);
    }

    fn append_keeps_existing_selection() {
        let grid = PickerGrid::new(vec!["item-0".to_string(), "item-1".to_string()]);
        grid.unselect(0);
        grid.append(vec!["late".to_string()]);
        // The new entry arrives selected; the existing choice is untouched.
        assert_eq!(grid.selected_indices(), vec![1, 2]);
    }

    fn empty_grid() {
        let grid = PickerGrid::empty();
        assert_eq!(grid.len(), 0);
        assert!(grid.selected_indices().is_empty());
        grid.append(vec!["only".to_string()]);
        assert_eq!(grid.selected_indices(), vec![0]);
    }

    /// The feature the pickers exist for: a pill is sized to its own text,
    /// so a short entry gives a short pill and a long one a wide pill.
    fn pills_fit_their_text() {
        let short = PickerGrid::new(vec!["abc".to_string()]);
        let long = PickerGrid::new(vec![
            "a-very-long-entry-title-that-keeps-going-and-going".to_string(),
        ]);
        let sw_short = scrolled_grid(short.widget());
        let sw_long = scrolled_grid(long.widget());
        let page = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        page.append(&sw_short);
        page.append(&sw_long);
        let window = gtk4::Window::new();
        window.set_default_size(480, 600);
        window.set_child(Some(&page));
        window.present();
        settle();
        let width = |p: &PickerGrid| p.inner.pills.borrow()[0].width();
        let short_w = width(&short);
        let long_w = width(&long);
        assert!(short_w > 0, "pill must be allocated");
        assert!(
            long_w > short_w + 40,
            "pills must fit their text: short={short_w} long={long_w}"
        );
    }

    /// UI smoke: pills realize per entry (one widget each — the cost the
    /// enumeration caps bound), the count sink follows clicks, the scrolled
    /// window caps the height, and late appends keep the bar honest.
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
        // wrapped directly around the pills.
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
        assert_eq!(buttons.len(), 5, "one pill per entry");
        assert!(buttons.iter().all(|b| b.is_active()), "all-on default");
        // A handful of titles must not stretch the card to the 480 cap.
        println!("smoke: 5-entry scrolled height = {}", scrolled.height());
        assert!(scrolled.height() > 0, "card height must be measured");
        assert!(
            scrolled.height() < 480,
            "5 files should not stretch the card: h={}",
            scrolled.height()
        );

        // Click path: pill -> count sink.
        let first = buttons
            .iter()
            .find(|b| pill_title(b).as_deref() == Some("item-0"))
            .expect("item-0 pill realized");
        first.set_active(false);
        settle();
        assert_eq!(grid.selected_indices(), vec![1, 2, 3, 4]);
        assert_eq!(add_btn.label().as_deref(), Some("4 selected"));

        // Button path (Select All/None -> pills), the real action-bar wiring.
        select_none.emit_clicked();
        settle();
        assert!(
            collect_toggle_buttons(grid.widget().upcast_ref::<gtk4::Widget>())
                .iter()
                .all(|b| !b.is_active()),
            "unselected items must unset pills"
        );
        assert_eq!(add_btn.label().as_deref(), Some("0 selected"));
        select_all.emit_clicked();
        settle();
        assert_eq!(add_btn.label().as_deref(), Some("5 selected"));

        // A long list must cap at the scrolled window instead of expanding
        // the card to content size.
        grid.append((5..300).map(|i| format!("item-{i}")).collect());
        settle();
        println!("smoke: 300-entry scrolled height = {}", scrolled.height());
        assert_eq!(add_btn.label().as_deref(), Some("300 selected"));
        assert!(scrolled.height() > 0, "card height must be measured");
        assert!(
            scrolled.height() <= 480,
            "the scrolled window must cap the wrap: h={}",
            scrolled.height()
        );

        // Bottom entries still toggle once materialized in the scrolled view.
        let vadj = scrolled.vadjustment();
        vadj.set_value(vadj.upper() / 2.0);
        settle();
        let buttons = collect_toggle_buttons(grid.widget().upcast_ref::<gtk4::Widget>());
        let any_late = buttons.last().expect("late pills exist");
        any_late.set_active(false);
        settle();
        assert_eq!(add_btn.label().as_deref(), Some("299 selected"));
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
