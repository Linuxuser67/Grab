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

fn begin(button: &gtk4::Button, operation: ToolOperation) -> String {
    let restore_label = button
        .label()
        .map(|label| label.to_string())
        .unwrap_or_default();
    button.set_label(&gettext(match operation {
        ToolOperation::Install => "Installing…",
        ToolOperation::Update => "Updating…",
    }));
    button.set_sensitive(false);
    button.add_css_class(PROGRESS_CLASS);
    restore_label
}

fn finish(button: &gtk4::Button, restore_label: &str) {
    button.remove_css_class(PROGRESS_CLASS);
    button.set_label(restore_label);
    button.set_sensitive(true);
}

/// Briefly explain that another support-tool operation is already active.
fn note_busy(button: &gtk4::Button) {
    let previous_label = button
        .label()
        .map(|label| label.to_string())
        .unwrap_or_default();
    button.set_label(&gettext("Another install is in progress…"));

    let button = button.downgrade();
    glib::timeout_add_local_once(Duration::from_secs(3), move || {
        if let Some(button) = button.upgrade()
            && button.label().as_deref()
                == Some(gettext("Another install is in progress…").as_str())
        {
            button.set_label(&previous_label);
        }
    });
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
    if RUNNING.with(Cell::get) {
        note_busy(button);
        return;
    }
    ensure_progress_css();
    RUNNING.with(|running| running.set(true));

    let restore_label = begin(button, operation);
    let button = button.clone();

    let include = |name: &str| {
        tools
            .as_ref()
            .map(|selected| selected.iter().any(|item| item.eq_ignore_ascii_case(name)))
            .unwrap_or(true)
    };
    let do_yt = include("yt-dlp");
    let do_gdl = include("gallery-dl");
    let do_deno = include("deno");

    glib::spawn_future_local(async move {
        let mut completed = Vec::with_capacity(3);

        if do_yt {
            on_stage(gettext("Installing yt-dlp…"));
            let path = match crate::video::install_ytdlp().await {
                Ok(path) => path,
                Err(error) => {
                    finish(&button, &restore_label);
                    RUNNING.with(|running| running.set(false));
                    on_error(error.to_string(), completed);
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
                    finish(&button, &restore_label);
                    RUNNING.with(|running| running.set(false));
                    on_error(error.to_string(), completed);
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
                    finish(&button, &restore_label);
                    RUNNING.with(|running| running.set(false));
                    on_error(error.to_string(), completed);
                    return;
                }
            };
            let version = crate::video_tools::tool_display_version(path, "--version").await;
            completed.push(InstalledTool {
                name: "Deno",
                version,
            });
        }

        finish(&button, &restore_label);
        RUNNING.with(|running| running.set(false));
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

        let restore_label = begin(&progress_button, ToolOperation::Update);
        assert!(progress_button.has_css_class(PROGRESS_CLASS));
        assert!(!progress_button.is_sensitive());
        assert_eq!(
            progress_button.label().as_deref(),
            Some(gettext("Updating…").as_str())
        );

        finish(&progress_button, &restore_label);
        assert!(!progress_button.has_css_class(PROGRESS_CLASS));
        assert!(progress_button.is_sensitive());
        assert_eq!(progress_button.label().as_deref(), Some("Install"));

        let install_button = gtk4::Button::with_label("Install");
        let restore_label = begin(&install_button, ToolOperation::Install);
        assert_eq!(
            install_button.label().as_deref(),
            Some(gettext("Installing…").as_str())
        );
        finish(&install_button, &restore_label);
        assert_eq!(install_button.label().as_deref(), Some("Install"));
    }
}
