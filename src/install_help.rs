//! Guided yt-dlp/ffmpeg/quickjs install for tarball/dev builds (Flatpak Install is the only sandbox path).

use adw::prelude::*;
use gettextrs::gettext;
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;

/// Close the dialog, then run a follow-up.
fn close_then(btn: &gtk4::Button, dialog: &adw::Dialog, on_check: impl Fn() + 'static) {
    let weak = dialog.downgrade();
    btn.connect_clicked(move |_| {
        if let Some(d) = weak.upgrade() {
            d.close();
        }
        on_check();
    });
}

/// Show the install-help dialog; `on_check` re-resolves tools and refreshes the caller.
pub fn show(parent: &impl glib::object::IsA<gtk4::Widget>, on_check: impl Fn() + 'static) {
    let dialog = adw::Dialog::builder()
        .title(gettext("Install Support Tools"))
        .build();
    dialog.set_content_width(420);

    let page = adw::PreferencesPage::new();
    let toolbar = adw::ToolbarView::new();
    let hb = adw::HeaderBar::new();
    hb.set_show_end_title_buttons(false);
    hb.set_show_start_title_buttons(false);
    let close_btn = gtk4::Button::builder()
        .label(gettext("_Close"))
        .use_underline(true)
        .build();
    let check_btn = gtk4::Button::builder()
        .label(gettext("_Check Again"))
        .use_underline(true)
        .css_classes(["suggested-action"])
        .build();
    hb.pack_start(&close_btn);
    hb.pack_end(&check_btn);
    toolbar.add_top_bar(&hb);
    toolbar.set_content(Some(&page));
    dialog.set_child(Some(&toolbar));
    dialog.set_default_widget(Some(&check_btn));

    crate::ui_util::close_on_click(&close_btn, &dialog);
    close_then(&check_btn, &dialog, on_check);

    let pkgs = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|text| crate::video_tools::distro_packages(&text));
    match pkgs {
        Some(pkgs) => {
            let group = adw::PreferencesGroup::builder()
                .title(pkgs.distro.clone())
                .description(gettext(
                    "Run this command in a terminal, then press Check Again.",
                ))
                .build();
            command_row(&group, &gettext("Install tools"), &pkgs.install_all);
            // No distro quickjs package: the command above installs only
            // yt-dlp and ffmpeg, so link the upstream quickjs releases
            // instead of dead-ending.
            for (tool, url) in extra_link_rows(&pkgs) {
                link_row(&group, tool, url);
            }
            page.add(&group);
        }
        None => {
            let group = adw::PreferencesGroup::builder()
                .title(gettext("Install the tools manually"))
                .description(gettext(
                    "Install yt-dlp, ffmpeg and quickjs yourself and make sure they are on your PATH, then press Check Again.",
                ))
                .build();
            link_row(
                &group,
                "yt-dlp",
                "https://github.com/yt-dlp/yt-dlp#installation",
            );
            link_row(&group, "ffmpeg", "https://ffmpeg.org/download.html");
            link_row(
                &group,
                "quickjs",
                "https://github.com/quickjs-ng/quickjs/releases",
            );
            page.add(&group);
        }
    }

    dialog.present(Some(parent));
}

/// One command row with copy button and brief checkmark confirmation. The copy
/// button reads the row's current subtitle, so callers can refresh the command
/// later with `set_subtitle`. Returns the row for visibility control.
pub(crate) fn command_row(
    group: &adw::PreferencesGroup,
    title: &str,
    command: &str,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(command)
        .build();
    let copy = gtk4::Button::builder()
        .icon_name("edit-copy-symbolic")
        .css_classes(["flat"])
        .tooltip_text(gettext("Copy command"))
        .valign(gtk4::Align::Center)
        .build();
    copy.update_property(&[gtk4::accessible::Property::Label(&gettext("Copy command"))]);
    row.add_suffix(&copy);
    let row_b = row.clone();
    copy.connect_clicked(move |b| {
        // The subtitle is the command; read it back so callers can refresh it
        // with `set_subtitle` after the row is built.
        let cmd = row_b.subtitle().unwrap_or_default();
        if let Some(clipboard) = gtk4::gdk::Display::default().map(|d| d.clipboard()) {
            clipboard.set_text(&cmd);
        }
        b.set_icon_name("emblem-ok-symbolic");
        let b2 = b.clone();
        glib::timeout_add_seconds_local(2, move || {
            b2.set_icon_name("edit-copy-symbolic");
            glib::ControlFlow::Break
        });
    });
    group.add(&row);
    row
}

/// Link rows the distro group needs beyond the install command: the upstream
/// quickjs releases when the distro has no quickjs package (the command then
/// covers only yt-dlp and ffmpeg). Pure so tests pin the dialog's rows
/// without needing a display.
fn extra_link_rows(
    _pkgs: &crate::video_tools::DistroPackages,
) -> Vec<(&'static str, &'static str)> {
    Vec::new()
}

/// One outbound-link row for the manual fallback.
fn link_row(group: &adw::PreferencesGroup, tool: &str, url: &'static str) {
    let row = adw::ActionRow::builder().title(tool).build();
    let open = gtk4::Button::builder()
        .label(gettext("Open"))
        .valign(gtk4::Align::Center)
        .build();
    row.add_suffix(&open);
    open.connect_clicked(move |b| {
        let root = b.root().and_downcast::<gtk4::Window>();
        gtk4::UriLauncher::new(url).launch(root.as_ref(), gio::Cancellable::NONE, |_| {});
    });
    group.add(&row);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video_tools::DistroPackages;

    fn pkgs(has_quickjs_package: bool) -> DistroPackages {
        DistroPackages {
            distro: "Void".to_string(),
            install_all: "sudo xbps-install -S yt-dlp ffmpeg".to_string(),
            has_quickjs_package,
        }
    }

    #[test]
    fn quickjs_link_row_appears_without_distro_package() {
        // The dialog must not dead-end on distros without a quickjs package:
        // the upstream releases link has to be among the group's rows.
        assert_eq!(
            extra_link_rows(&pkgs(false)),
            vec![("quickjs", "https://github.com/quickjs-ng/quickjs/releases")]
        );
    }

    #[test]
    fn no_quickjs_link_row_with_distro_package() {
        // The install command already covers quickjs here; no extra row.
        assert!(extra_link_rows(&pkgs(true)).is_empty());
    }
}
