//! Shared install-progress popover for Grab-managed media tools.
//! Indeterminate pulsing bars (no byte progress); spinner becomes check/error per stage.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gettextrs::gettext;
use gtk4::glib::{self, ControlFlow};
use gtk4::prelude::*;
use libadwaita as adw;

// One install in flight + its popover (main-thread thread_local, no locking); later clicks re-open, failures retry.
thread_local! {
    static RUNNING: Cell<bool> = const { Cell::new(false) };
    static SESSION: RefCell<Option<gtk4::Popover>> = const { RefCell::new(None) };
}

/// One tool's row: indicator, name, status, pulsing bar, error label shown only on failure.
struct ToolRow {
    root: gtk4::Box,
    /// Idle/spinner/status icon: exactly one child visible, so swaps never
    /// move the text column and no visibility juggling is needed.
    stack: gtk4::Stack,
    icon: gtk4::Image,
    status: gtk4::Label,
    bar: gtk4::ProgressBar,
    error: gtk4::Label,
    /// Which bar the shared pulse driver ticks; set/cleared as stages start and finish.
    active: Rc<RefCell<Option<gtk4::ProgressBar>>>,
}

impl ToolRow {
    /// Build one row; `bar_label` is the accessible name (status label already announces state).
    fn new(name: &str, bar_label: &str, active: &Rc<RefCell<Option<gtk4::ProgressBar>>>) -> Self {
        // AdwSpinner animates while mapped; the stack unmaps hidden children,
        // so it runs only while the "spinner" child is visible.
        let stack = gtk4::Stack::new();
        stack.set_valign(gtk4::Align::Center);
        stack.add_named(
            &gtk4::Box::new(gtk4::Orientation::Horizontal, 0),
            Some("idle"),
        );
        stack.add_named(&adw::Spinner::new(), Some("spinner"));
        let icon = gtk4::Image::from_icon_name("emblem-ok-symbolic");
        icon.set_pixel_size(16);
        stack.add_named(&icon, Some("icon"));

        let name_label = gtk4::Label::new(Some(name));
        name_label.set_halign(gtk4::Align::Start);
        name_label.add_css_class("heading");
        let status = gtk4::Label::new(Some(&gettext("Waiting…")));
        status.set_halign(gtk4::Align::Start);
        status.add_css_class("caption");
        status.add_css_class("dimmed");
        let titles = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        titles.set_hexpand(true);
        titles.append(&name_label);
        titles.append(&status);

        let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        header.append(&stack);
        header.append(&titles);

        let bar = gtk4::ProgressBar::new();
        bar.update_property(&[gtk4::accessible::Property::Label(bar_label)]);

        let error = gtk4::Label::new(None);
        error.set_wrap(true);
        error.set_max_width_chars(32);
        error.set_halign(gtk4::Align::Start);
        error.set_visible(false);

        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        root.append(&header);
        root.append(&bar);
        root.append(&error);

        Self {
            root,
            stack,
            icon,
            status,
            bar,
            error,
            active: active.clone(),
        }
    }

    fn widget(&self) -> gtk4::Box {
        self.root.clone()
    }

    /// Mark the row active: spinner runs and its bar becomes the pulse target.
    fn set_downloading(&self) {
        self.stack.set_visible_child_name("spinner");
        // A new attempt clears the previous failure's tint.
        self.icon.remove_css_class("error");
        self.status.set_text(&gettext("Downloading…"));
        *self.active.borrow_mut() = Some(self.bar.clone());
    }

    /// Mark the row done: checkmark, full bar, probed version when available.
    fn set_installed(&self, version: Option<String>) {
        *self.active.borrow_mut() = None;
        self.stack.set_visible_child_name("icon");
        // Success is never red, even after a retried failure.
        self.icon.remove_css_class("error");
        self.bar.set_fraction(1.0);
        let text = match version {
            Some(v) => gettext("Installed • {version}").replace("{version}", &v),
            None => gettext("Installed"),
        };
        self.status.set_text(&text);
    }

    /// Mark failed: error icon + wrapping text (not color alone); popover stays open, caller re-enables Install for retry.
    fn set_failed(&self, message: &str) {
        *self.active.borrow_mut() = None;
        self.icon.set_icon_name(Some("dialog-error-symbolic"));
        // Red is the failure signal; the "Failed" text keeps color from being the only signal.
        self.icon.add_css_class("error");
        self.stack.set_visible_child_name("icon");
        self.bar.set_visible(false);
        self.status.set_text(&gettext("Failed"));
        self.error.set_text(message);
        self.error.set_visible(true);
    }
}

/// Staged media-tool installation under a shared popover anchored at `button`.
/// Rows go Waiting → Downloading → Installed; failure marks its row and leaves the popover open, success lingers briefly.
/// Button stays sensitive: in-flight clicks re-open, post-failure clicks retry; `on_error`/`on_success` report to caller.
pub fn run(
    button: &gtk4::Button,
    on_error: impl Fn(String) + 'static,
    on_success: impl Fn() + 'static,
    // Tools to install/update. None = all (fresh install). Some(list) = only
    // those (update flow: popup shows only what's actually downloading).
    tools: Option<Vec<String>>,
) {
    let btn = button.clone();

    // Already running: re-open its popover at the clicked button.
    if RUNNING.with(|r| r.get()) {
        SESSION.with(|s| {
            if let Some(pop) = s.borrow().as_ref() {
                pop.popdown();
                pop.unparent();
                pop.set_parent(&btn);
                pop.popup();
            }
        });
        return;
    }
    // Failed popover still open; this click retires it for a fresh retry.
    SESSION.with(|s| {
        if let Some(pop) = s.take() {
            pop.popdown();
        }
    });
    RUNNING.with(|r| r.set(true));

    let active: Rc<RefCell<Option<gtk4::ProgressBar>>> = Rc::new(RefCell::new(None));
    // Whether to include a tool: None = all (fresh install), Some(list) = only listed.
    let include = |name: &str| {
        tools
            .as_ref()
            .map(|t| t.iter().any(|n| n == name))
            .unwrap_or(true)
    };
    let do_yt = include("yt-dlp");
    let do_gdl = include("gallery-dl");
    let do_deno = include("deno");

    // Order: yt-dlp, gallery-dl (under yt-dlp), then Deno.
    let yt = ToolRow::new("yt-dlp", &gettext("yt-dlp install progress"), &active);
    let gdl = ToolRow::new(
        "gallery-dl",
        &gettext("gallery-dl install progress"),
        &active,
    );
    let deno = ToolRow::new("Deno", &gettext("deno install progress"), &active);

    let rows = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    // HIG card padding: 12, matching the rest of the app (not 18).
    rows.set_margin_top(12);
    rows.set_margin_bottom(12);
    rows.set_margin_start(12);
    rows.set_margin_end(12);
    if do_yt {
        rows.append(&yt.widget());
    }
    if do_gdl {
        rows.append(&gdl.widget());
    }
    if do_deno {
        rows.append(&deno.widget());
    }
    let pop = gtk4::Popover::new();
    pop.set_child(Some(&rows));
    pop.set_parent(&btn);
    pop.popup();
    // Kept alive so later clicks re-open this popover instead of starting a new install.
    SESSION.with(|s| *s.borrow_mut() = Some(pop.clone()));

    glib::spawn_future_local(async move {
        // Pulse driver ticks the active row. Started here (before any tool)
        // so it runs regardless of which tools are included.
        {
            let active = active.clone();
            glib::timeout_add_local(Duration::from_millis(100), move || {
                if let Some(bar) = active.borrow().as_ref() {
                    bar.pulse();
                    ControlFlow::Continue
                } else {
                    ControlFlow::Break
                }
            });
        }
        if do_yt {
            yt.set_downloading();
            let yt_path = match crate::video::install_ytdlp().await {
                Ok(path) => path,
                Err(e) => {
                    let message = e.to_string();
                    yt.set_failed(&message);
                    RUNNING.with(|r| r.set(false));
                    on_error(message);
                    return;
                }
            };
            let version = crate::video_tools::tool_display_version(yt_path, "--version").await;
            yt.set_installed(version);
        }
        if do_gdl {
            // gallery-dl for image galleries and fallback video; installed up front
            // so gallery pastes never wait on a lazy install.
            gdl.set_downloading();
            // Run on Tokio worker (not GTK main) to avoid UI freeze.
            let gdl_path = match crate::runtime::tokio_rt()
                .spawn(async move { crate::gallery_tools::install_gallerydl().await })
                .await
                .ok()
                .unwrap_or_else(|| Err(crate::gallery_tools::GalleryError::install("spawn failed")))
            {
                Ok(path) => path,
                Err(e) => {
                    let message = e.to_string();
                    gdl.set_failed(&message);
                    RUNNING.with(|r| r.set(false));
                    on_error(message);
                    return;
                }
            };
            let version = crate::runtime::tokio_rt()
                .spawn(async move {
                    crate::gallery_tools::gallerydl_display_version(&gdl_path)
                        .await
                        .ok()
                })
                .await
                .ok()
                .flatten();
            gdl.set_installed(version);
        }
        if do_deno {
            // Deno is Grab's JavaScript runtime for YouTube; installed up
            // front so YouTube spawns never wait on a lazy install.
            deno.set_downloading();
            let deno_path = match crate::video::install_deno().await {
                Ok(path) => path,
                Err(e) => {
                    let message = e.to_string();
                    deno.set_failed(&message);
                    RUNNING.with(|r| r.set(false));
                    on_error(message);
                    return;
                }
            };
            let version = crate::video_tools::tool_display_version(deno_path, "--version").await;
            deno.set_installed(version);
        }

        // Linger on checkmarks so completion registers, then close.
        glib::timeout_future(Duration::from_millis(1200)).await;
        pop.popdown();
        SESSION.with(|s| {
            s.take();
        });
        RUNNING.with(|r| r.set(false));
        on_success();
    });
}
