//! Media-tool installation progress attached to the install button's bottom edge.
//!
//! The button uses GNOME Software's 2 px progress strip. Installers do not
//! expose byte-level progress, so the strip sweeps indeterminately.

use std::cell::{Cell, RefCell};
use std::time::Duration;

use gettextrs::gettext;
use gtk4::glib;
use gtk4::prelude::*;

const PROGRESS_CLASS: &str = "grab-install-progress";

const PROGRESS_CSS: &str = r#"
button.grab-install-progress {
  background-image: linear-gradient(to top, var(--accent-bg-color) 2px, transparent 2px);
  background-repeat: no-repeat;
  background-position: left bottom;
  background-size: 28% 100%;
  animation: grab-install-progress-sweep 1.25s ease-in-out infinite alternate;
  transition: none;
}

button.grab-install-progress:dir(rtl) {
  animation-direction: alternate-reverse;
}

@media (prefers-reduced-motion: reduce) {
  button.grab-install-progress {
    background-image: linear-gradient(
      to top,
      color-mix(in srgb, var(--accent-bg-color) 40%, transparent) 2px,
      transparent 2px
    );
    animation: none;
    background-size: 100% 100%;
  }
}

@keyframes grab-install-progress-sweep {
  from { background-position: left bottom; }
  to { background-position: right bottom; }
}
"#;

thread_local! {
    static RUNNING: Cell<bool> = const { Cell::new(false) };
    // GtkStyleProvider registration does not transfer ownership. Keep the
    // provider alive for the display for as long as the application runs.
    static PROGRESS_CSS_PROVIDER: RefCell<Option<gtk4::CssProvider>> =
        const { RefCell::new(None) };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SupportTool {
    YtDlp,
    GalleryDl,
    Deno,
}

impl SupportTool {
    fn from_id(id: &str) -> Option<Self> {
        if id.eq_ignore_ascii_case("yt-dlp") {
            Some(Self::YtDlp)
        } else if id.eq_ignore_ascii_case("gallery-dl") {
            Some(Self::GalleryDl)
        } else if id.eq_ignore_ascii_case("deno") {
            Some(Self::Deno)
        } else {
            None
        }
    }
}

fn includes_tool(selected: Option<&[SupportTool]>, tool: SupportTool) -> bool {
    selected.is_none_or(|selected| selected.contains(&tool))
}

/// Own one support-tool operation. Dropping it restores the button and clears
/// the process-wide guard, including when the async operation is cancelled or panics.
struct OperationGuard {
    button: gtk4::glib::WeakRef<gtk4::Button>,
    restore_label: String,
}

impl OperationGuard {
    fn acquire(button: &gtk4::Button) -> Option<Self> {
        let restore_label = button
            .label()
            .map(|label| label.to_string())
            .unwrap_or_default();
        let button = button.downgrade();
        RUNNING.with(|running| {
            if running.replace(true) {
                None
            } else {
                Some(Self {
                    button,
                    restore_label,
                })
            }
        })
    }
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        RUNNING.with(|running| running.set(false));
        if let Some(button) = self.button.upgrade() {
            finish(&button, &self.restore_label);
        }
    }
}

/// Whether this operation installs missing tools or updates existing ones.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ToolOperation {
    /// Install tools that are not present.
    Install,
    /// Update tools that are already present.
    Update,
}

/// A tool installed or updated during one support-tools operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledTool {
    /// Human-readable tool name.
    pub name: &'static str,
    /// Detected version after installation, if the version probe succeeded.
    pub version: Option<String>,
}

impl InstalledTool {
    /// Format the tool name and detected version for a Preferences subtitle.
    pub fn display_label(&self) -> String {
        match self.version.as_deref() {
            Some(version) if !version.trim().is_empty() => {
                format!("{} {}", self.name, version.trim())
            }
            _ => self.name.to_string(),
        }
    }
}

/// Summarize the completed tools, distinguishing upgrades from fresh installs.
pub fn summarize_changes(completed: &[InstalledTool], updated_names: &[String]) -> String {
    let mut installed = Vec::new();
    let mut updated = Vec::new();

    for tool in completed {
        let label = tool.display_label();
        if updated_names
            .iter()
            .any(|name| name.eq_ignore_ascii_case(tool.name))
        {
            updated.push(label);
        } else {
            installed.push(label);
        }
    }

    let mut parts = Vec::new();
    if !installed.is_empty() {
        parts.push(gettext("Installed: {list}").replace("{list}", &installed.join(", ")));
    }
    if !updated.is_empty() {
        parts.push(gettext("Updated: {list}").replace("{list}", &updated.join(", ")));
    }

    if parts.is_empty() {
        gettext("No support tools were changed")
    } else {
        parts.join(" • ")
    }
}

fn load_progress_css(provider: &gtk4::CssProvider) {
    provider.load_from_string(PROGRESS_CSS);
}

fn ensure_progress_css() {
    PROGRESS_CSS_PROVIDER.with(|stored| {
        if stored.borrow().is_some() {
            return;
        }
        let Some(display) = gtk4::gdk::Display::default() else {
            return;
        };

        let provider = gtk4::CssProvider::new();
        load_progress_css(&provider);
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        *stored.borrow_mut() = Some(provider);
    });
}

fn begin(button: &gtk4::Button, operation: ToolOperation) {
    button.set_label(&gettext(match operation {
        ToolOperation::Install => "Installing…",
        ToolOperation::Update => "Updating…",
    }));
    button.set_sensitive(false);
    button.add_css_class(PROGRESS_CLASS);
}

fn finish(button: &gtk4::Button, restore_label: &str) {
    button.remove_css_class(PROGRESS_CLASS);
    button.set_label(restore_label);
    button.set_sensitive(true);
}

/// Briefly explain that another support-tool operation is already active
/// without mutating the label captured by a later install attempt.
fn note_busy(button: &gtk4::Button) {
    let previous_tooltip = button.tooltip_text().map(|tooltip| tooltip.to_string());
    let busy_tooltip = gettext("Another install is in progress…");
    button.set_tooltip_text(Some(&busy_tooltip));

    let button = button.downgrade();
    glib::timeout_add_local_once(Duration::from_secs(3), move || {
        if let Some(button) = button.upgrade()
            && button.tooltip_text().as_deref() == Some(busy_tooltip.as_str())
        {
            button.set_tooltip_text(previous_tooltip.as_deref());
        }
    });
}

/// Release the operation guard before invoking the caller's error callback.
fn report_failure(
    operation: OperationGuard,
    on_error: &impl Fn(String, Vec<InstalledTool>),
    error: String,
    completed: Vec<InstalledTool>,
) {
    drop(operation);
    on_error(error, completed);
}

/// Install all tools when no filter is provided, or only selected tool names.
///
/// The button is owned by its GTK parent; this function clones the button into
/// the main-context task, so callers do not need a separate progress controller.
/// The stage callback updates the row subtitle, the error callback receives
/// completed tools before a failure, and the success callback receives all
/// completed tools.
pub fn run(
    button: &gtk4::Button,
    operation: ToolOperation,
    on_stage: impl Fn(String) + 'static,
    on_error: impl Fn(String, Vec<InstalledTool>) + 'static,
    on_success: impl Fn(Vec<InstalledTool>) + 'static,
    tools: Option<Vec<String>>,
) {
    let Some(operation_guard) = OperationGuard::acquire(button) else {
        note_busy(button);
        return;
    };
    ensure_progress_css();
    begin(button, operation);

    let selected_tools = tools.map(|names| {
        names
            .iter()
            .filter_map(|name| SupportTool::from_id(name))
            .collect::<Vec<_>>()
    });
    let do_yt = includes_tool(selected_tools.as_deref(), SupportTool::YtDlp);
    let do_gdl = includes_tool(selected_tools.as_deref(), SupportTool::GalleryDl);
    let do_deno = includes_tool(selected_tools.as_deref(), SupportTool::Deno);

    glib::spawn_future_local(async move {
        let mut completed = Vec::with_capacity(3);

        if do_yt {
            on_stage(gettext("Installing yt-dlp…"));
            let path = match crate::video::install_ytdlp().await {
                Ok(path) => path,
                Err(error) => {
                    report_failure(operation_guard, &on_error, error.to_string(), completed);
                    return;
                }
            };
            let version = crate::video_tools::tool_display_version(path, "--version").await;
            completed.push(InstalledTool {
                name: "yt-dlp",
                version,
            });
        }

        if do_gdl {
            on_stage(gettext("Installing gallery-dl…"));
            let path = match crate::runtime::tokio_rt()
                .spawn(async move { crate::gallery_tools::install_gallerydl().await })
                .await
                .ok()
                .unwrap_or_else(|| Err(crate::gallery_tools::GalleryError::install("spawn failed")))
            {
                Ok(path) => path,
                Err(error) => {
                    report_failure(operation_guard, &on_error, error.to_string(), completed);
                    return;
                }
            };

            let version = crate::runtime::tokio_rt()
                .spawn(async move {
                    crate::gallery_tools::gallerydl_display_version(&path)
                        .await
                        .ok()
                })
                .await
                .ok()
                .flatten();
            completed.push(InstalledTool {
                name: "gallery-dl",
                version,
            });
        }

        if do_deno {
            on_stage(gettext("Installing Deno…"));
            let path = match crate::video::install_deno().await {
                Ok(path) => path,
                Err(error) => {
                    report_failure(operation_guard, &on_error, error.to_string(), completed);
                    return;
                }
            };
            let version = crate::video_tools::tool_display_version(path, "--version").await;
            completed.push(InstalledTool {
                name: "Deno",
                version,
            });
        }

        drop(operation_guard);
        on_success(completed);
    });
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::*;

    #[test]
    fn installed_tool_summary_includes_the_detected_version() {
        let tool = InstalledTool {
            name: "yt-dlp",
            version: Some("2026.11.23".to_string()),
        };
        assert_eq!(tool.display_label(), "yt-dlp 2026.11.23");
    }

    #[test]
    fn installed_tool_summary_keeps_the_tool_when_version_probe_fails() {
        let tool = InstalledTool {
            name: "gallery-dl",
            version: None,
        };
        assert_eq!(tool.display_label(), "gallery-dl");
    }

    #[test]
    fn support_tool_ids_are_parsed_into_known_variants() {
        assert_eq!(SupportTool::from_id("yt-dlp"), Some(SupportTool::YtDlp));
        assert_eq!(
            SupportTool::from_id("GALLERY-DL"),
            Some(SupportTool::GalleryDl)
        );
        assert_eq!(SupportTool::from_id("Deno"), Some(SupportTool::Deno));
        assert_eq!(SupportTool::from_id("unknown"), None);
    }

    #[test]
    fn support_tool_filter_includes_all_by_default_and_only_selected_tools() {
        assert!(includes_tool(None, SupportTool::YtDlp));
        assert!(includes_tool(None, SupportTool::GalleryDl));
        let selected = [SupportTool::Deno];
        assert!(!includes_tool(Some(&selected), SupportTool::YtDlp));
        assert!(!includes_tool(Some(&selected), SupportTool::GalleryDl));
        assert!(includes_tool(Some(&selected), SupportTool::Deno));
        assert!(!includes_tool(Some(&[]), SupportTool::Deno));
    }

    #[test]
    fn change_summary_distinguishes_new_installs_from_updates() {
        let completed = [
            InstalledTool {
                name: "yt-dlp",
                version: Some("2026.11.23".to_string()),
            },
            InstalledTool {
                name: "Deno",
                version: Some("2.6.0".to_string()),
            },
        ];
        let summary = summarize_changes(&completed, &["deno".to_string()]);
        assert!(summary.contains("Installed: yt-dlp 2026.11.23"));
        assert!(summary.contains("Updated: Deno 2.6.0"));
    }

    #[test]
    #[ignore = "needs a display; run under Xvfb (see ci.yml)"]
    fn progress_strip_is_attached_to_the_button_and_removed_on_finish() {
        gtk4::init().expect("gtk::init (run under Xvfb)");

        let parse_errors = Rc::new(RefCell::new(Vec::new()));
        let provider = gtk4::CssProvider::new();
        let recorded_errors = parse_errors.clone();
        provider.connect_parsing_error(move |_, _, error| {
            recorded_errors.borrow_mut().push(error.to_string());
        });
        load_progress_css(&provider);
        assert!(
            parse_errors.borrow().is_empty(),
            "progress CSS must parse without GTK errors: {:?}",
            parse_errors.borrow()
        );

        let progress_button = gtk4::Button::with_label("Install");
        ensure_progress_css();
        PROGRESS_CSS_PROVIDER.with(|stored| {
            assert!(
                stored.borrow().is_some(),
                "the display must retain the CSS provider after setup"
            );
        });
        assert!(!progress_button.has_css_class(PROGRESS_CLASS));

        let operation = OperationGuard::acquire(&progress_button)
            .expect("the first support-tool operation should acquire the guard");
        begin(&progress_button, ToolOperation::Update);
        assert!(progress_button.has_css_class(PROGRESS_CLASS));
        assert!(!progress_button.is_sensitive());
        assert_eq!(
            progress_button.label().as_deref(),
            Some(gettext("Updating…").as_str())
        );

        drop(operation);
        assert!(!progress_button.has_css_class(PROGRESS_CLASS));
        assert!(progress_button.is_sensitive());
        assert_eq!(progress_button.label().as_deref(), Some("Install"));
        let next_operation = OperationGuard::acquire(&progress_button)
            .expect("dropping the previous operation must release RUNNING");
        drop(next_operation);

        let panic_button = gtk4::Button::with_label("Install");
        let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let operation = OperationGuard::acquire(&panic_button)
                .expect("the operation should start before the callback panics");
            begin(&panic_button, ToolOperation::Update);
            let _operation = operation;
            panic!("simulate a callback panic");
        }));
        assert!(panic_result.is_err());
        assert!(panic_button.is_sensitive());
        assert!(!panic_button.has_css_class(PROGRESS_CLASS));
        assert_eq!(panic_button.label().as_deref(), Some("Install"));
        let after_panic =
            OperationGuard::acquire(&panic_button).expect("unwinding must release RUNNING");
        drop(after_panic);

        let install_button = gtk4::Button::with_label("Install");
        let operation = OperationGuard::acquire(&install_button).expect("operation guard");
        begin(&install_button, ToolOperation::Install);
        assert_eq!(
            install_button.label().as_deref(),
            Some(gettext("Installing…").as_str())
        );
        drop(operation);
        assert_eq!(install_button.label().as_deref(), Some("Install"));

        let busy_button = gtk4::Button::with_label("Install");
        busy_button.set_tooltip_text(Some("Existing help"));
        let previous_label = busy_button.label();
        note_busy(&busy_button);
        assert_eq!(busy_button.label(), previous_label);
        assert_eq!(
            busy_button.tooltip_text().as_deref(),
            Some(gettext("Another install is in progress…").as_str())
        );
    }
}
