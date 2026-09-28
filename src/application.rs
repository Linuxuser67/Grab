//! Application lifecycle: startup (actions once), activate (window),
//! open (URLs/files), shutdown (stop downloads, persist queue).

use crate::download::DownloadManager;
use crate::settings::AppSettings;
use crate::window::{self, show_add_dialog};
use crate::window_rows::ngettext_count;
use crate::{APP_ID, preferences};
use adw::prelude::*;
use gettextrs::gettext;
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use std::cell::RefCell;
use std::rc::Rc;

struct State {
    manager: Rc<DownloadManager>,
    settings: AppSettings,
    toasts: Rc<adw::ToastOverlay>,
    window: adw::ApplicationWindow,
    search_bar: gtk4::SearchBar,
}

pub fn setup(app: &adw::Application) {
    let state: Rc<RefCell<Option<Rc<State>>>> = Rc::new(RefCell::new(None));

    {
        let st = Rc::clone(&state);
        app.connect_startup(move |app| {
            let settings = AppSettings::new();
            let store = gio::ListStore::new::<crate::download::DownloadItem>();
            let manager = DownloadManager::new(store, settings.clone());
            manager.restore_queue();

            let toasts = Rc::new(adw::ToastOverlay::new());
            register_actions(app, &st);
            let win = window::build_window(app, manager.clone(), settings.clone(), toasts.clone());
            st.borrow_mut().replace(Rc::new(State {
                manager: manager.clone(),
                settings,
                toasts,
                window: win.0,
                search_bar: win.1,
            }));

            app.set_accels_for_action("app.add-download", &["<Control>n"]);
            app.set_accels_for_action("app.search", &["<Control>f"]);
            app.set_accels_for_action("app.quit", &["<Control>q"]);
            app.set_accels_for_action("app.preferences", &["<Control>comma"]);
            app.set_accels_for_action("app.shortcuts", &["<Control>question"]);
        });
    }

    {
        let st = Rc::clone(&state);
        app.connect_activate(move |app| {
            app.withdraw_notification(window::BACKGROUND_NOTIF_ID);
            if let Some(s) = st.borrow().as_ref() {
                s.window.present();
            }
        });
    }

    {
        let st = Rc::clone(&state);
        app.connect_open(move |_, files, _| {
            let s = match st.borrow().as_ref().cloned() {
                Some(s) => s,
                None => return,
            };
            for f in files {
                // magnet: links arrive here when Grab is the system's magnet
                // handler; enqueue validates them like pasted links.
                if let Ok(uri) = f.uri().parse::<url::Url>()
                    && matches!(uri.scheme(), "http" | "https" | "magnet")
                {
                    // Video pages take the dialog path (pre-filled): plain
                    // enqueue would save the raw HTML page as a file.
                    if crate::video::is_video_page(uri.as_str()) {
                        show_add_dialog(s.manager.clone(), Some(uri.as_str()));
                        continue;
                    }
                    if let Err(e) = s.manager.enqueue(uri.as_str(), None, None) {
                        s.toasts.add_toast(adw::Toast::new(&e));
                    }
                    continue;
                }
                if let Some(path) = f.path() {
                    // .torrent files go to the torrent intake.
                    if path
                        .extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("torrent"))
                    {
                        let (manager, toasts) = (s.manager.clone(), s.toasts.clone());
                        let stem = path
                            .file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "download".to_string());
                        let file_name = path
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| format!("{stem}.torrent"));
                        glib::spawn_future_local(async move {
                            let bytes = gio::spawn_blocking(move || {
                                crate::torrent::read_torrent_bytes(&path)
                            })
                            .await
                            .ok()
                            .flatten();
                            let Some(bytes) = bytes else {
                                toasts.add_toast(adw::Toast::new(&gettext(
                                    "Could not read that .torrent file",
                                )));
                                return;
                            };
                            // Same file picker as the add dialog: multi-file
                            // torrents offer per-file switches.
                            match crate::torrent::torrent_file_list(&bytes) {
                                Ok((_, entries)) if entries.len() > 1 => {
                                    let dest =
                                        Rc::new(RefCell::new(manager.effective_download_dir()));
                                    window::show_torrent_files_dialog(
                                        manager, dest, None, file_name, bytes, entries,
                                    );
                                }
                                Ok(_) => {
                                    if let Err(e) =
                                        manager.enqueue_torrent_file(bytes, &stem, None, None)
                                    {
                                        toasts.add_toast(adw::Toast::new(&e));
                                    }
                                }
                                Err(e) => {
                                    toasts.add_toast(adw::Toast::new(&e));
                                }
                            }
                        });
                        continue;
                    }
                    // Only .torrent files open as files now; anything else
                    // explains itself instead of queuing garbage rows.
                    s.toasts.add_toast(adw::Toast::new(&gettext(
                        "Only .torrent files can be opened directly",
                    )));
                }
            }
            s.window.present();
        });
    }

    {
        let st = Rc::clone(&state);
        app.connect_shutdown(move |_| {
            if let Some(s) = st.borrow().as_ref() {
                s.manager.shutdown();
            }
        });
    }
}

/// Destructive confirm dialog: Cancel/confirm responses, destructive styling,
/// Cancel as default and close. The body runs only on explicit confirmation,
/// since dialogs sit open while the queue may change.
fn destructive_confirm(
    parent: &impl gtk4::glib::object::IsA<gtk4::Widget>,
    heading: &str,
    body: &str,
    confirm_label: &str,
    on_confirm: impl Fn() + 'static,
) {
    let dialog = adw::AlertDialog::builder()
        .heading(heading)
        .body(body)
        .build();
    dialog.add_response("cancel", &gettext("Cancel"));
    dialog.add_response("confirm", confirm_label);
    dialog.set_response_appearance("confirm", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.connect_response(None, move |_, response| {
        if response == "confirm" {
            on_confirm();
        }
    });
    dialog.present(Some(parent));
}

fn register_actions(app: &adw::Application, st: &Rc<RefCell<Option<Rc<State>>>>) {
    let entries = [
        {
            let st = Rc::clone(st);
            gio::ActionEntry::builder("add-download")
                .activate(move |_, _, _| {
                    if let Some(s) = st.borrow().as_ref() {
                        show_add_dialog(s.manager.clone(), None);
                    }
                })
                .build()
        },
        {
            let st = Rc::clone(st);
            gio::ActionEntry::builder("search")
                .activate(move |_, _, _| {
                    if let Some(s) = st.borrow().as_ref() {
                        let on = !s.search_bar.is_search_mode();
                        s.search_bar.set_search_mode(on);
                    }
                })
                .build()
        },
        {
            let st = Rc::clone(st);
            gio::ActionEntry::builder("cancel-all")
                .activate(move |_, _, _| {
                    let Some(s) = st.borrow().as_ref().cloned() else {
                        return;
                    };
                    let n = s.manager.active_count();
                    if n == 0 {
                        return;
                    }
                    let body = ngettext_count(
                        "This will cancel the active download.",
                        "This will cancel {n} active downloads.",
                        n,
                    );
                    let manager = s.manager.clone();
                    let toasts = s.toasts.clone();
                    destructive_confirm(
                        &s.window,
                        &gettext("Cancel All Downloads?"),
                        &body,
                        &gettext("Cancel All"),
                        move || {
                            // Count at confirm time: the queue may have
                            // changed while the dialog sat open.
                            let n = manager.active_count();
                            manager.cancel_all();
                            let toast = adw::Toast::new(&ngettext_count(
                                "Cancelled download",
                                "Cancelled {n} downloads",
                                n,
                            ));
                            toasts.add_toast(toast);
                        },
                    );
                })
                .build()
        },
        {
            let st = Rc::clone(st);
            gio::ActionEntry::builder("retry-failed")
                .activate(move |_, _, _| {
                    if let Some(s) = st.borrow().as_ref() {
                        let n = s.manager.retry_failed();
                        if n > 0 {
                            s.toasts.add_toast(adw::Toast::new(&ngettext_count(
                                "Retrying failed download",
                                "Retrying {n} failed downloads",
                                n,
                            )));
                        }
                    }
                })
                .build()
        },
        {
            let st = Rc::clone(st);
            gio::ActionEntry::builder("clear-finished")
                .activate(move |_, _, _| {
                    let Some(s) = st.borrow().as_ref().cloned() else {
                        return;
                    };
                    let n = s.manager.finished_count();
                    if n == 0 {
                        return;
                    }
                    // Records only: files stay on disk, so a destructive
                    // confirm must still spell the scope out.
                    let body = ngettext_count(
                        "This will remove the finished download from the list. The file stays on disk.",
                        "This will remove {n} finished downloads from the list. The files stay on disk.",
                        n,
                    );
                    let manager = s.manager.clone();
                    let toasts = s.toasts.clone();
                    destructive_confirm(
                        &s.window,
                        &gettext("Clear Finished Downloads?"),
                        &body,
                        &gettext("Clear Finished"),
                        move || {
                            let snapshots = manager.finished_snapshots();
                            let n = manager.clear_finished();
                            if n == 0 {
                                return;
                            }
                            let toast = adw::Toast::new(&ngettext_count(
                                "Cleared finished download",
                                "Cleared {n} finished downloads",
                                n,
                            ));
                            toast.set_button_label(Some(&gettext("Undo")));
                            let m2 = manager.clone();
                            toast.connect_button_clicked(move |_| {
                                for snap in snapshots.clone() {
                                    m2.unremove(snap);
                                }
                            });
                            toasts.add_toast(toast);
                        },
                    );
                })
                .build()
        },
        {
            let st = Rc::clone(st);
            gio::ActionEntry::builder("open-folder")
                .activate(move |_, _, _| {
                    if let Some(s) = st.borrow().as_ref() {
                        let dir = s.manager.effective_download_dir();
                        window::launch_path(std::path::Path::new(&dir), &s.toasts, false);
                    }
                })
                .build()
        },
        {
            let st = Rc::clone(st);
            gio::ActionEntry::builder("preferences")
                .activate(move |_, _, _| {
                    if let Some(s) = st.borrow().as_ref().cloned() {
                        preferences::show(&s.window, &s.settings);
                    }
                })
                .build()
        },
        {
            let st = Rc::clone(st);
            gio::ActionEntry::builder("about")
                .activate(move |_, _, _| {
                    if let Some(s) = st.borrow().as_ref() {
                        // from_appdata aborts on a missing resource, so only
                        // use it when the catalog is registered: About must
                        // never crash the app.
                        const METAINFO: &str = "/io/github/houssemko/Grab/metainfo.xml";
                        let registered =
                            gio::resources_lookup_data(METAINFO, gio::ResourceLookupFlags::NONE)
                                .is_ok();
                        let about = if registered {
                            // Name/version/notes come from the metainfo
                            // catalog; icon and license can't, so literal.
                            let about = adw::AboutDialog::from_appdata(
                                METAINFO,
                                Some(env!("GRAB_VERSION")),
                            );
                            about.set_application_icon(APP_ID);
                            about.set_license_type(gtk4::License::Gpl30Only);
                            about
                        } else {
                            let about = adw::AboutDialog::new();
                            about.set_application_name("Grab");
                            about
                        };
                        about.present(Some(&s.window));
                    }
                })
                .build()
        },
        {
            let st = Rc::clone(st);
            gio::ActionEntry::builder("shortcuts")
                .activate(move |_, _, _| {
                    if let Some(s) = st.borrow().as_ref() {
                        let dialog = adw::ShortcutsDialog::new();
                        let section =
                            adw::ShortcutsSection::new(Some(&gettext("Downloads") as &str));
                        section.add(adw::ShortcutsItem::new(
                            &gettext("New Download"),
                            "<Control>n",
                        ));
                        section.add(adw::ShortcutsItem::new(&gettext("Rename"), "F2"));
                        // Plain items: these actions have no accelerators,
                        // and from_action would imply a keybinding.
                        section.add(adw::ShortcutsItem::new(&gettext("Cancel All"), ""));
                        section.add(adw::ShortcutsItem::new(&gettext("Retry Failed"), ""));
                        section.add(adw::ShortcutsItem::from_action(
                            &gettext("Search"),
                            "app.search",
                        ));
                        dialog.add(section);
                        let section2 =
                            adw::ShortcutsSection::new(Some(&gettext("General") as &str));
                        section2.add(adw::ShortcutsItem::from_action(
                            &gettext("Preferences"),
                            "app.preferences",
                        ));
                        section2.add(adw::ShortcutsItem::new(&gettext("Quit"), "<Control>q"));
                        dialog.add(section2);
                        dialog.present(Some(&s.window));
                    }
                })
                .build()
        },
        gio::ActionEntry::builder("quit")
            .activate(|app: &adw::Application, _, _| app.quit())
            .build(),
        gio::ActionEntry::builder("present")
            .activate(|app: &adw::Application, _, _| app.activate())
            .build(),
    ];
    app.add_action_entries(entries);
}

#[cfg(test)]
mod tests {
    /// Every metainfo `<release version="...">` entry, in file order.
    fn metainfo_release_versions(xml: &str) -> Vec<String> {
        let marker = "<release version=\"";
        let mut out = Vec::new();
        let mut rest = xml;
        while let Some(start) = rest.find(marker) {
            rest = &rest[start + marker.len()..];
            let end = rest.find('"').expect("release version closes");
            out.push(rest[..end].to_string());
            rest = &rest[end..];
        }
        assert!(!out.is_empty(), "metainfo has releases");
        out
    }

    /// One dot-separated prerelease identifier, ordered per semver: numeric
    /// identifiers compare by value and sort below alphanumeric ones, so
    /// `beta.10` outranks `beta.2` (a whole-string comparison gets this
    /// wrong: `'1' < '2'`).
    #[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
    enum PreId {
        Num(u64),
        Str(String),
    }

    /// Version tuple for comparison (numeric parts, stable flag, prerelease
    /// identifiers last).
    fn version_key(v: &str) -> (u32, u32, u32, bool, Vec<PreId>) {
        let (core, suffix) = match v.split_once(['-', '+']) {
            Some((c, s)) => (c, s),
            None => (v, ""),
        };
        let mut parts = core.split('.').map(|p| p.parse().unwrap_or(0));
        // A stable release outranks its own pre-releases: without the flag,
        // `4.4.0` and `4.4.0-beta.1` tie and `max_by_key` keeps the beta,
        // so beta history alongside a stable entry would fail the test below.
        // The identifiers break ties between pre-releases of the same core
        // (`4.4.4-beta.2` outranks `4.4.4-beta.1`); without them the older
        // beta wins the tie and the test below fails.
        let stable = suffix.is_empty();
        // `stable` already outranks any prerelease at the tuple level, and a
        // stable-vs-stable comparison never consults the identifiers, so the
        // empty suffix keeps its single empty identifier with no effect.
        let pre = suffix
            .split('.')
            .map(|id| {
                if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
                    PreId::Num(id.parse().unwrap_or(u64::MAX))
                } else {
                    PreId::Str(id.to_owned())
                }
            })
            .collect();
        (
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
            stable,
            pre,
        )
    }

    /// The About dialog (`from_appdata`) shows the newest metainfo release as
    /// the app version: a Cargo bump without a matching entry ships a stale one.
    #[test]
    fn metainfo_newest_release_matches_package_version() {
        let xml = include_str!("../data/io.github.houssemko.Grab.metainfo.xml.in");
        let newest = metainfo_release_versions(xml)
            .iter()
            .max_by_key(|v| version_key(v))
            .expect("at least one release")
            .clone();
        assert_eq!(newest, env!("CARGO_PKG_VERSION"));
    }

    /// The newest entry must also be first: `from_appdata` reads the leading
    /// `<release>`, so an out-of-order file shows the wrong version.
    #[test]
    fn metainfo_lists_newest_release_first() {
        let xml = include_str!("../data/io.github.houssemko.Grab.metainfo.xml.in");
        let versions = metainfo_release_versions(xml);
        assert_eq!(
            versions.first().map(String::as_str),
            Some(env!("CARGO_PKG_VERSION"))
        );
    }

    /// Prerelease identifiers compare per semver: numeric ones by value, and
    /// below alphanumeric ones. A whole-string suffix comparison orders
    /// `beta.10` below `beta.2` (`'1' < '2'`).
    #[test]
    fn version_key_orders_prerelease_identifiers_numerically() {
        assert!(version_key("4.4.4-beta.10") > version_key("4.4.4-beta.2"));
        assert!(version_key("4.4.4-beta.2") > version_key("4.4.4-beta.1"));
        assert!(version_key("0.2.1-alpha.13") > version_key("0.2.1-alpha.5"));
        assert!(version_key("4.4.4") > version_key("4.4.4-beta.99"));
        assert!(version_key("4.4.4-beta.1") > version_key("4.4.3"));
        assert!(version_key("4.4.4-beta.1") < version_key("4.4.4-beta.1.1"));
    }
}
