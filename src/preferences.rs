use adw::prelude::*;
use gettextrs::gettext;
use gtk4::gio;
use gtk4::prelude::*;
use libadwaita as adw;

/// Two-way ComboRow <-> GSettings sync via index/value mapping (plain bind can't map index vs string).
fn bind_combo_row(
    row: &adw::ComboRow,
    settings: &crate::settings::AppSettings,
    key: &'static str,
    get: impl Fn(&crate::settings::AppSettings) -> String,
    index_of: fn(&str) -> usize,
    value_of: fn(usize) -> &'static str,
) {
    row.set_selected(index_of(&get(settings)) as u32);
    {
        let row = row.downgrade();
        settings.connect_changed(Some(key), move |s, _| {
            if let Some(row) = row.upgrade() {
                row.set_selected(index_of(&s.string(key)) as u32);
            }
        });
    }
    row.connect_selected_notify({
        let s = settings.clone();
        move |row| {
            let _ = s.set_string(key, value_of(row.selected() as usize));
        }
    });
}

/// Flag bad rate text immediately (empty/`0` = unlimited); error icon + tooltip keeps it perceivable without color.
fn mark_rate_row(live: &adw::EntryRow) {
    let icon = gtk4::Image::from_icon_name("dialog-error-symbolic");
    icon.set_tooltip_text(Some(&gettext(
        "Invalid rate — use e.g. 500K, 2M, or leave empty for unlimited",
    )));
    icon.set_visible(false);
    live.add_suffix(&icon);
    let l = live.clone();
    let mark = move |row: &adw::EntryRow| {
        let t = row.text().to_string();
        let t = t.trim();
        let ok = t.is_empty() || t == "0" || crate::download_rate::parse_rate(t).is_some();
        if ok {
            l.remove_css_class("error");
            icon.set_visible(false);
        } else {
            l.add_css_class("error");
            icon.set_visible(true);
        }
    };
    mark(live);
    live.connect_changed(mark);
}

/// Flatpak-only row under "Cookies from Browser": when the picked profile is
/// unreachable in the sandbox, show the `flatpak override` command as a
/// copyable action row. Re-runs on dialog open and `COOKIES_BROWSER` changes;
/// overrides apply only at sandbox startup, so a companion row tells the user
/// to restart. Takes `&gio::Settings` because that is what `connect_changed`
/// hands the signal.
fn sync_cookies_override_row(
    row: &adw::ActionRow,
    restart_row: &adw::ActionRow,
    settings: &gio::Settings,
) {
    let value = settings.string(crate::settings::key::COOKIES_BROWSER);
    let command = if crate::video_tools::in_flatpak()
        && value != "none"
        && crate::video_tools::browser_profile_dir(&value).is_none()
    {
        crate::video_tools::browser_override_command(&value)
    } else {
        None
    };
    match command {
        Some(cmd) => {
            row.set_subtitle(&cmd);
            row.set_visible(true);
            restart_row.set_visible(true);
        }
        None => {
            row.set_visible(false);
            restart_row.set_visible(false);
        }
    }
}

/// Whether proxy mode selects manual setup; takes raw value for signal payload and getter.
fn is_manual_proxy(mode: &str) -> bool {
    mode == crate::download::PROXY_MODE_MANUAL
}

pub fn show(
    parent: &impl gtk4::glib::object::IsA<gtk4::Widget>,
    settings: &crate::settings::AppSettings,
) {
    let dialog = adw::PreferencesDialog::builder()
        .title(gettext("Preferences"))
        .build();

    // Advanced page keeps main pages approachable; every advanced row keeps its safe default.
    let advanced_page = adw::PreferencesPage::builder()
        .title(gettext("Advanced"))
        .icon_name("applications-engineering-symbolic")
        .build();

    let advanced_general_group = adw::PreferencesGroup::builder()
        .title(gettext("General"))
        .build();
    let advanced_media_group = adw::PreferencesGroup::builder()
        .title(gettext("Media"))
        .build();
    let page = adw::PreferencesPage::builder()
        .title(gettext("Downloads"))
        .icon_name("folder-download-symbolic")
        .build();

    let dest_group = adw::PreferencesGroup::builder()
        .title(gettext("Destination"))
        .build();
    let current = settings.download_dir();
    let shown = if current.is_empty() {
        gettext("(System Downloads folder)")
    } else {
        current
    };
    let dest_label = gtk4::Label::builder()
        .label(&shown)
        .css_classes(["dimmed", "caption"])
        .ellipsize(gtk4::pango::EllipsizeMode::Middle)
        .hexpand(true)
        .halign(gtk4::Align::Start)
        .build();
    let dest_btn = gtk4::Button::builder()
        .label(gettext("Choose…"))
        .tooltip_text(gettext("Choose download folder"))
        .valign(gtk4::Align::Center)
        .build();
    let reset_btn = gtk4::Button::builder()
        .icon_name("edit-clear-symbolic")
        .css_classes(["flat"])
        .tooltip_text(gettext("Use system default"))
        .valign(gtk4::Align::Center)
        .build();
    reset_btn.update_property(&[gtk4::accessible::Property::Label(&gettext(
        "Use system default",
    ))]);
    let dest_row = adw::ActionRow::builder()
        .title(gettext("Download folder"))
        .build();
    dest_row.add_suffix(&dest_label);
    dest_row.add_suffix(&reset_btn);
    dest_row.add_suffix(&dest_btn);
    dest_group.add(&dest_row);
    let restrict_filenames = adw::SwitchRow::builder()
        .title(gettext("Restrict filenames to ASCII"))
        .subtitle(gettext("Use only ASCII characters in filenames"))
        .build();
    restrict_filenames.set_tooltip_text(Some(&gettext(
        "Avoids broken names on USB drives, network shares and older systems",
    )));
    settings
        .bind(
            crate::settings::key::RESTRICT_FILENAMES,
            &restrict_filenames,
            "active",
        )
        .build();
    advanced_general_group.add(&restrict_filenames);
    {
        let s = settings.clone();
        let l = dest_label.clone();
        let root = parent.root().and_downcast::<gtk4::Window>();
        dest_btn.connect_clicked(move |_| {
            let chooser = gtk4::FileDialog::builder()
                .title(gettext("Choose download folder"))
                .accept_label(gettext("Select Folder"))
                .build();
            let s2 = s.clone();
            let l2 = l.clone();
            chooser.select_folder(root.as_ref(), gio::Cancellable::NONE, move |res| {
                if let Ok(f) = res
                    && let Some(p) = f.path()
                {
                    let dir = p.to_string_lossy().into_owned();
                    if s2
                        .set_string(crate::settings::key::DOWNLOAD_DIR, &dir)
                        .is_ok()
                    {
                        l2.set_text(&dir);
                    }
                }
            });
        });
    }
    {
        let s = settings.clone();
        let l = dest_label.clone();
        reset_btn.connect_clicked(move |_| {
            if s.set_string(crate::settings::key::DOWNLOAD_DIR, "").is_ok() {
                l.set_text(&gettext("(System Downloads folder)"));
            }
        });
    }

    let net_page = adw::PreferencesPage::builder()
        .title(gettext("Network"))
        .icon_name("network-wired-symbolic")
        .build();

    let net_group = adw::PreferencesGroup::builder()
        .title(gettext("Network and Queue"))
        .build();

    let concurrent = adw::SpinRow::builder()
        .title(gettext("Simultaneous downloads"))
        .adjustment(&gtk4::Adjustment::new(3.0, 1.0, 10.0, 1.0, 1.0, 0.0))
        .build();
    settings
        .bind(crate::settings::key::MAX_CONCURRENT, &concurrent, "value")
        .build();
    net_group.add(&concurrent);

    let connections = adw::SpinRow::builder()
        .title(gettext("Connections per download"))
        .subtitle(gettext("Parallel connections for large files"))
        .adjustment(&gtk4::Adjustment::new(4.0, 1.0, 16.0, 1.0, 1.0, 0.0))
        .build();
    connections.set_tooltip_text(Some(&gettext(
        "Files under ~16 MB always use one connection",
    )));
    settings
        .bind(crate::settings::key::CONNECTIONS, &connections, "value")
        .build();
    net_group.add(&connections);

    let limit = adw::EntryRow::builder()
        .title(gettext("Speed limit"))
        .build();
    limit.set_tooltip_text(Some(&gettext(
        "Per download, e.g. 500K, 2M; empty means unlimited",
    )));
    limit.set_input_purpose(gtk4::InputPurpose::FreeForm);
    settings
        .bind(crate::settings::key::SPEED_LIMIT, &limit, "text")
        .build();
    mark_rate_row(&limit);
    net_group.add(&limit);

    let keep_date = adw::SwitchRow::builder()
        .title(gettext("Keep original file dates"))
        .subtitle(gettext("Use the server's date instead of download time"))
        .build();
    keep_date.set_tooltip_text(Some(&gettext(
        "Handy for archives; otherwise files are dated when you downloaded them",
    )));
    settings
        .bind(crate::settings::key::KEEP_SERVER_DATE, &keep_date, "active")
        .build();
    advanced_general_group.add(&keep_date);

    let proxy_mode = adw::ComboRow::builder()
        .title(gettext("Proxy"))
        .subtitle(gettext("Torrents follow SOCKS5 proxies only"))
        .build();
    {
        let labels = crate::download::proxy_mode_labels();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        proxy_mode.set_model(Some(&gtk4::StringList::new(&refs)));
    }
    bind_combo_row(
        &proxy_mode,
        settings,
        crate::settings::key::PROXY_MODE,
        crate::settings::AppSettings::proxy_mode,
        crate::download::proxy_mode_index,
        crate::download::proxy_mode_value,
    );
    net_group.add(&proxy_mode);
    let proxy_group = adw::PreferencesGroup::builder()
        .title(gettext("Manual proxy"))
        .build();
    let proxy_type = adw::ComboRow::builder().title(gettext("Type")).build();
    {
        let labels = crate::download::proxy_type_labels();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        proxy_type.set_model(Some(&gtk4::StringList::new(&refs)));
    }
    bind_combo_row(
        &proxy_type,
        settings,
        crate::settings::key::PROXY_TYPE,
        crate::settings::AppSettings::proxy_type,
        crate::download::proxy_type_index,
        crate::download::proxy_type_value,
    );
    proxy_group.add(&proxy_type);
    let proxy_host = adw::EntryRow::builder().title(gettext("Host")).build();
    settings
        .bind(crate::settings::key::PROXY_HOST, &proxy_host, "text")
        .build();
    proxy_group.add(&proxy_host);
    let proxy_port = adw::SpinRow::builder()
        .title(gettext("Port"))
        .adjustment(&gtk4::Adjustment::new(9050.0, 1.0, 65535.0, 1.0, 10.0, 0.0))
        .build();
    settings
        .bind(crate::settings::key::PROXY_PORT, &proxy_port, "value")
        .build();
    proxy_group.add(&proxy_port);
    // Manual rows exist only in manual mode; group hides otherwise.
    fn sync_proxy_group(group: &gtk4::glib::WeakRef<adw::PreferencesGroup>, manual: bool) {
        if let Some(group) = group.upgrade() {
            group.set_visible(manual);
        }
    }
    {
        let group = proxy_group.downgrade();
        settings.connect_changed(Some(crate::settings::key::PROXY_MODE), move |s, _| {
            sync_proxy_group(
                &group,
                is_manual_proxy(s.string(crate::settings::key::PROXY_MODE).as_str()),
            );
        });
    }
    proxy_mode.connect_selected_notify({
        let s = settings.clone();
        let group = proxy_group.downgrade();
        move |_row| {
            sync_proxy_group(&group, is_manual_proxy(&s.proxy_mode()));
        }
    });
    sync_proxy_group(
        &proxy_group.downgrade(),
        is_manual_proxy(&settings.proxy_mode()),
    );
    net_page.add(&proxy_group);

    let notif_group = adw::PreferencesGroup::builder()
        .title(gettext("Notifications"))
        .build();
    let notif = adw::SwitchRow::builder()
        .title(gettext("Notify when downloads finish"))
        .build();
    settings
        .bind(crate::settings::key::SHOW_NOTIFICATIONS, &notif, "active")
        .build();
    notif_group.add(&notif);
    let bg_notif = adw::SwitchRow::builder()
        .title(gettext("Background download notifications"))
        .subtitle(gettext("When closing with downloads still running"))
        .build();
    settings
        .bind(crate::settings::key::NOTIFY_BACKGROUND, &bg_notif, "active")
        .build();
    notif_group.add(&bg_notif);

    let power_group = adw::PreferencesGroup::builder()
        .title(gettext("Power"))
        .build();
    let inhibit = adw::SwitchRow::builder()
        .title(gettext("Prevent sleep during downloads"))
        .subtitle(gettext(
            "Keep the computer awake while downloads are active",
        ))
        .build();
    settings
        .bind(crate::settings::key::INHIBIT_SUSPEND, &inhibit, "active")
        .build();
    power_group.add(&inhibit);

    let auto_add = adw::SwitchRow::builder()
        .title(gettext("Add downloads automatically"))
        .subtitle(gettext(
            "Pasted links start looking up on their own; single videos download after lookup",
        ))
        .build();
    auto_add.set_tooltip_text(Some(&gettext(
        "Links opened from other apps also start without asking, except links to local or private addresses",
    )));
    settings
        .bind(
            crate::settings::key::AUTO_ADD_DOWNLOADS,
            &auto_add,
            "active",
        )
        .build();
    dest_group.add(&auto_add);

    page.add(&dest_group);
    let sched_group = adw::PreferencesGroup::builder()
        .title(gettext("Scheduling"))
        .build();
    let sched = adw::SwitchRow::builder()
        .title(gettext("Scheduled downloads"))
        .subtitle(gettext("Allow downloads to start at a specific time"))
        .build();
    settings
        .bind(
            crate::settings::key::ENABLE_SCHEDULED_DOWNLOADS,
            &sched,
            "active",
        )
        .build();
    sched_group.add(&sched);
    page.add(&sched_group);
    page.add(&power_group);
    page.add(&notif_group);
    dialog.add(&page);

    net_page.add(&net_group);

    let torrent_page = adw::PreferencesPage::builder()
        .title(gettext("Torrent"))
        .icon_name("emblem-shared-symbolic")
        .build();

    let share_group = adw::PreferencesGroup::builder()
        .title(gettext("Sharing"))
        .build();
    let seed = adw::SwitchRow::builder()
        .title(gettext("Seed finished downloads"))
        .build();
    seed.set_tooltip_text(Some(&gettext(
        "Keep sharing the finished file with other downloaders",
    )));
    settings
        .bind(crate::settings::key::TORRENT_SEED_FINISHED, &seed, "active")
        .build();
    share_group.add(&seed);
    let seed_ratio = adw::SpinRow::builder()
        .title(gettext("Seed to ratio"))
        .subtitle(gettext(
            "Stop seeding at this upload ratio. 0 means unlimited.",
        ))
        .adjustment(&gtk4::Adjustment::new(0.0, 0.0, 100.0, 0.1, 1.0, 0.0))
        .digits(1)
        .build();
    seed_ratio.set_tooltip_text(Some(&gettext(
        "Upload relative to download size; 2.0 means twice as much up as down",
    )));
    settings
        .bind(
            crate::settings::key::TORRENT_SEED_RATIO,
            &seed_ratio,
            "value",
        )
        .build();
    seed.bind_property("active", &seed_ratio, "sensitive")
        .sync_create()
        .build();
    share_group.add(&seed_ratio);
    let seed_time = adw::SpinRow::builder()
        .title(gettext("Seed time limit"))
        .subtitle(gettext(
            "Stop seeding this many minutes after finishing. 0 means unlimited.",
        ))
        .adjustment(&gtk4::Adjustment::new(0.0, 0.0, 43200.0, 5.0, 60.0, 0.0))
        .build();
    seed_time.set_tooltip_text(Some(&gettext(
        "Counts from the moment your download finished",
    )));
    settings
        .bind(crate::settings::key::TORRENT_SEED_TIME, &seed_time, "value")
        .build();
    seed.bind_property("active", &seed_time, "sensitive")
        .sync_create()
        .build();
    share_group.add(&seed_time);
    let listen_port = adw::SpinRow::builder()
        .title(gettext("Listen port"))
        .subtitle(gettext(
            "Port for incoming connections. 0 disables listening. 1024+ recommended; binds all interfaces. Takes effect on restart.",
        ))
        .adjustment(&gtk4::Adjustment::new(0.0, 0.0, 65535.0, 1.0, 10.0, 0.0))
        .build();
    listen_port.set_tooltip_text(Some(&gettext(
        "Required for effective seeding behind NAT; enable UPnP below to forward it automatically",
    )));
    settings
        .bind(
            crate::settings::key::TORRENT_LISTEN_PORT,
            &listen_port,
            "value",
        )
        .build();
    seed.bind_property("active", &listen_port, "sensitive")
        .sync_create()
        .build();
    share_group.add(&listen_port);
    let upnp = adw::SwitchRow::builder()
        .title(gettext("UPnP port forwarding"))
        .subtitle(gettext(
            "Ask the router to forward the listen port. Only applies when a listen port is set. Takes effect on restart.",
        ))
        .build();
    upnp.set_tooltip_text(Some(&gettext(
        "Lets the router forward the port automatically; without it, incoming connections fail behind NAT",
    )));
    settings
        .bind(crate::settings::key::TORRENT_UPNP, &upnp, "active")
        .build();
    seed.bind_property("active", &upnp, "sensitive")
        .sync_create()
        .build();
    share_group.add(&upnp);

    let torrent_net_group = adw::PreferencesGroup::builder()
        .title(gettext("Connectivity"))
        .build();
    let dht = adw::SwitchRow::builder()
        .title(gettext("Use DHT"))
        .subtitle(gettext(
            "Find peers without trackers. Takes effect on restart.",
        ))
        .build();
    dht.set_tooltip_text(Some(&gettext(
        "Lets magnet links and trackerless torrents find peers",
    )));
    settings
        .bind(crate::settings::key::TORRENT_DHT, &dht, "active")
        .build();
    torrent_net_group.add(&dht);
    let lsd = adw::SwitchRow::builder()
        .title(gettext("Find peers on the local network"))
        .subtitle(gettext(
            "Discover peers on your local network. Applies when the torrent engine first starts.",
        ))
        .build();
    settings
        .bind(crate::settings::key::TORRENT_LSD, &lsd, "active")
        .build();
    torrent_net_group.add(&lsd);
    let upload_limit = adw::EntryRow::builder()
        .title(gettext("Upload speed limit"))
        .build();
    upload_limit.set_tooltip_text(Some(&gettext(
        "Per torrent, e.g. 500K, 2M; empty means unlimited",
    )));
    upload_limit.set_input_purpose(gtk4::InputPurpose::FreeForm);
    settings
        .bind(
            crate::settings::key::TORRENT_UPLOAD_LIMIT,
            &upload_limit,
            "text",
        )
        .build();
    mark_rate_row(&upload_limit);
    torrent_net_group.add(&upload_limit);
    let blocklist = adw::EntryRow::builder()
        .title(gettext("Peer blocklist"))
        .build();
    blocklist.set_tooltip_text(Some(&gettext(
        "URL of a peer blocklist (eMule ipfilter.dat format); empty means disabled. Fetched directly, bypassing any proxy.",
    )));
    blocklist.set_input_purpose(gtk4::InputPurpose::Url);
    settings
        .bind(
            crate::settings::key::TORRENT_BLOCKLIST_URL,
            &blocklist,
            "text",
        )
        .build();
    {
        let icon = gtk4::Image::from_icon_name("dialog-error-symbolic");
        icon.set_tooltip_text(Some(&gettext("Invalid URL")));
        icon.set_visible(false);
        blocklist.add_suffix(&icon);
        let l = blocklist.clone();
        let mark = move |row: &adw::EntryRow| {
            let t = row.text().to_string();
            let ok = crate::torrent::blocklist_url_of(t.trim()).is_ok();
            if ok {
                l.remove_css_class("error");
                icon.set_visible(false);
            } else {
                l.add_css_class("error");
                icon.set_visible(true);
            }
        };
        mark(&blocklist);
        blocklist.connect_changed(mark);
    }
    torrent_net_group.add(&blocklist);
    torrent_page.add(&torrent_net_group);

    // Video pages resolve via yt-dlp tools; this page holds defaults plus tool setup.
    fn tool_first_line(binary: &std::path::Path, version_arg: &str) -> Option<String> {
        crate::video_tools::probe_first_line(
            binary,
            version_arg,
            crate::video_tools::TOOL_PROBE_TIMEOUT,
        )
    }
    fn tool_version(binary: &std::path::Path, version_arg: &str) -> Option<String> {
        let first = tool_first_line(binary, version_arg)?;
        // ffmpeg prints a whole sentence ("ffmpeg version n9.0.1 ..."):
        // keep the version token so the row stays readable.
        if binary
            .file_name()
            .is_some_and(|n| n.to_string_lossy() == "ffmpeg")
            && let Some(token) = first.split_whitespace().nth(2)
        {
            return Some(format!("ffmpeg {token}"));
        }
        Some(first)
    }
    /// Installed tool versions for the tools row: `(yt-dlp, ffmpeg, quickjs?)`;
    /// `None` when the core tools are missing, falling back to the binary path
    /// if `--version` fails. quickjs is `None` when no `qjs` is on hand — it is
    /// only provisioned for YouTube.
    fn installed_tool_versions() -> Option<(String, String, Option<String>)> {
        crate::video::resolve_libraries().ok().map(|libs| {
            let yt = tool_version(&libs.youtube, "--version")
                .map(|v| format!("yt-dlp {v}"))
                .unwrap_or_else(|| libs.youtube.display().to_string());
            let ff = tool_version(&libs.ffmpeg, "-version")
                .unwrap_or_else(|| libs.ffmpeg.display().to_string());
            let qjs = crate::video_tools::find_quickjs()
                .and_then(|p| tool_version(&p, "--version").map(|v| format!("quickjs {v}")));
            (yt, ff, qjs)
        })
    }
    /// What the tools-row button does: Install when tools are missing, Check to
    /// probe each tool's upstream for a newer release, Update once one is
    /// known. The check-then-act shape keeps a permanent Update button off
    /// fresh installs while leaving on-demand updates one click away.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum ToolAction {
        Install,
        Check,
        Update,
    }
    fn paint_tools_state(
        row: &adw::ActionRow,
        btn: &gtk4::Button,
        action: &std::rc::Rc<std::cell::Cell<ToolAction>>,
        probed: Option<(String, String, Option<String>)>,
    ) {
        match probed {
            Some((yt, ff, qjs)) => {
                let mut subtitle = gettext("Ready • {yt} • {ff}")
                    .replace("{yt}", &yt)
                    .replace("{ff}", &ff);
                if let Some(qjs) = qjs {
                    subtitle.push_str(&format!(" • {qjs}"));
                }
                row.set_subtitle(&subtitle);
                btn.set_label(&gettext("Check for Updates"));
                action.set(ToolAction::Check);
            }
            None => {
                row.set_subtitle(&gettext("Not installed"));
                btn.set_label(&gettext("Install"));
                action.set(ToolAction::Install);
            }
        }
    }
    fn refresh_video_tools(
        row: &adw::ActionRow,
        btn: &gtk4::Button,
        spin: &adw::Spinner,
        action: &std::rc::Rc<std::cell::Cell<ToolAction>>,
    ) {
        spin.set_visible(false);
        // Probe off the GTK thread (three subprocesses), then paint.
        let (row, btn, action) = (row.clone(), btn.clone(), action.clone());
        gtk4::glib::spawn_future_local(async move {
            let probed = gio::spawn_blocking(installed_tool_versions)
                .await
                .ok()
                .flatten();
            paint_tools_state(&row, &btn, &action, probed);
        });
    }
    let video_page = adw::PreferencesPage::builder()
        .title(gettext("Media"))
        .icon_name("video-display-symbolic")
        .build();
    let video_quality_group = adw::PreferencesGroup::builder()
        .title(gettext("Quality"))
        .build();
    let video_labels = crate::video::quality_labels();
    let video_refs: Vec<&str> = video_labels.iter().map(String::as_str).collect();
    let video_quality = adw::ComboRow::builder()
        .title(gettext("Preferred quality"))
        .subtitle(gettext("Used for new media downloads"))
        .model(&gtk4::StringList::new(&video_refs))
        .build();
    bind_combo_row(
        &video_quality,
        settings,
        crate::settings::key::VIDEO_QUALITY,
        crate::settings::AppSettings::video_quality,
        crate::media_types::quality_index,
        crate::media_types::quality_value,
    );
    video_quality_group.add(&video_quality);
    let codec_labels = crate::video::codec_priority_labels();
    let codec_refs: Vec<&str> = codec_labels.iter().map(String::as_str).collect();
    let video_codec = adw::ComboRow::builder()
        .title(gettext("Preferred video codec"))
        .subtitle(gettext("Newest codecs first, or widest playback"))
        .model(&gtk4::StringList::new(&codec_refs))
        .build();
    video_codec.set_tooltip_text(Some(&gettext(
        "Ranks available formats; the closest match wins when your pick isn't offered",
    )));
    bind_combo_row(
        &video_codec,
        settings,
        crate::settings::key::VIDEO_CODEC_PRIORITY,
        crate::settings::AppSettings::video_codec_priority,
        crate::video::codec_priority_index,
        crate::video::codec_priority_value,
    );
    advanced_media_group.add(&video_codec);
    let subtitle_labels = crate::video::subtitle_language_labels();
    let subtitle_refs: Vec<&str> = subtitle_labels.iter().map(String::as_str).collect();
    let subtitle_lang = adw::ComboRow::builder()
        .title(gettext("Subtitles"))
        .subtitle(gettext("Download subtitles beside the video"))
        .model(&gtk4::StringList::new(&subtitle_refs))
        .build();
    bind_combo_row(
        &subtitle_lang,
        settings,
        crate::settings::key::SUBTITLE_LANGUAGE,
        crate::settings::AppSettings::subtitle_language,
        crate::video::subtitle_language_index,
        crate::video::subtitle_language_value,
    );
    video_quality_group.add(&subtitle_lang);
    let video_auth_group = adw::PreferencesGroup::builder()
        .title(gettext("Authentication"))
        .description(gettext("Age gates and member-only pages"))
        .build();
    let browser_labels = crate::video::cookies_browser_labels();
    let browser_refs: Vec<&str> = browser_labels.iter().map(String::as_str).collect();
    let browser_row = adw::ComboRow::builder()
        .title(gettext("Cookies from Browser"))
        .subtitle(gettext("Reads this browser's profile directly"))
        .model(&gtk4::StringList::new(&browser_refs))
        .build();
    bind_combo_row(
        &browser_row,
        settings,
        crate::settings::key::COOKIES_BROWSER,
        crate::settings::AppSettings::cookies_browser,
        crate::video::cookies_browser_index,
        crate::video::cookies_browser_value,
    );
    video_auth_group.add(&browser_row);
    let cookies_override_row =
        crate::install_help::command_row(&video_auth_group, &gettext("Grant browser access"), "");
    cookies_override_row.set_visible(false);
    // Overrides apply at sandbox startup: a running app can never observe the
    // grant, so no live re-check — the companion row says to restart instead.
    let cookies_restart_row = adw::ActionRow::builder()
        .title(gettext("Restart Grab after running the command"))
        .activatable(false)
        .build();
    cookies_restart_row.set_visible(false);
    video_auth_group.add(&cookies_restart_row);
    sync_cookies_override_row(&cookies_override_row, &cookies_restart_row, settings);
    {
        let grant = cookies_override_row.downgrade();
        let restart = cookies_restart_row.downgrade();
        settings.connect_changed(Some(crate::settings::key::COOKIES_BROWSER), move |s, _| {
            if let (Some(grant), Some(restart)) = (grant.upgrade(), restart.upgrade()) {
                sync_cookies_override_row(&grant, &restart, s);
            }
        });
    }
    let video_post_group = adw::PreferencesGroup::builder()
        .title(gettext("Post-processing"))
        .description(gettext("Applied while finishing downloads"))
        .build();
    let embed_subs = adw::SwitchRow::builder()
        .title(gettext("Embed subtitles"))
        .subtitle(gettext("Mux downloaded subtitles into the video file"))
        .build();
    embed_subs.set_tooltip_text(Some(&gettext(
        "Needs subtitles enabled under Media → Subtitles; does nothing without them",
    )));
    settings
        .bind(crate::settings::key::EMBED_SUBS, &embed_subs, "active")
        .build();
    video_post_group.add(&embed_subs);
    let sponsorblock = adw::SwitchRow::builder()
        .title(gettext("Remove sponsored segments"))
        .subtitle(gettext("Cut SponsorBlock-flagged sponsor segments"))
        .build();
    sponsorblock.set_tooltip_text(Some(&gettext(
        "Works where SponsorBlock has data, mostly YouTube",
    )));
    settings
        .bind(
            crate::settings::key::SPONSORBLOCK_REMOVE,
            &sponsorblock,
            "active",
        )
        .build();
    video_post_group.add(&sponsorblock);
    let sponsorblock_mark = adw::SwitchRow::builder()
        .title(gettext("Mark sponsored segments"))
        .subtitle(gettext(
            "Tag SponsorBlock-flagged sponsor segments as chapters",
        ))
        .build();
    sponsorblock_mark.set_tooltip_text(Some(&gettext(
        "Lets you skip sponsors with chapter navigation instead of cutting them",
    )));
    settings
        .bind(
            crate::settings::key::SPONSORBLOCK_MARK,
            &sponsorblock_mark,
            "active",
        )
        .build();
    // Removing segments makes marking them moot: grey the Mark row out
    // while Remove is on.
    sponsorblock
        .bind_property("active", &sponsorblock_mark, "sensitive")
        .invert_boolean()
        .sync_create()
        .build();
    video_post_group.add(&sponsorblock_mark);
    let embed_chapters = adw::SwitchRow::builder()
        .title(gettext("Embed chapters"))
        .subtitle(gettext("Write chapter markers into the finished file"))
        .build();
    embed_chapters.set_tooltip_text(Some(&gettext(
        "Players with chapter support let you jump between sections",
    )));
    settings
        .bind(
            crate::settings::key::EMBED_CHAPTERS,
            &embed_chapters,
            "active",
        )
        .build();
    video_post_group.add(&embed_chapters);
    // ComboRow holds an index, GSettings a string: sync both ways by hand
    // (same pattern as the subtitle language row).
    let remux_labels = crate::video::remux_video_labels();
    let remux_refs: Vec<&str> = remux_labels.iter().map(String::as_str).collect();
    let remux_row = adw::ComboRow::builder()
        .title(gettext("Remux video"))
        .subtitle(gettext(
            "Change the finished file's container without re-encoding",
        ))
        .model(&gtk4::StringList::new(&remux_refs))
        .build();
    remux_row.set_tooltip_text(Some(&gettext(
        "Useful when a player or editor dislikes the original container",
    )));
    bind_combo_row(
        &remux_row,
        settings,
        crate::settings::key::REMUX_VIDEO,
        crate::settings::AppSettings::remux_video,
        crate::video::remux_video_index,
        crate::video::remux_video_value,
    );
    video_post_group.add(&remux_row);
    let video_live_group = adw::PreferencesGroup::builder()
        .title(gettext("Live"))
        .description(gettext("Live stream recording"))
        .build();
    let live_from_start = adw::SwitchRow::builder()
        .title(gettext("Live from start"))
        .subtitle(gettext("Record live streams from the beginning"))
        .build();
    live_from_start.set_tooltip_text(Some(&gettext(
        "Needs the site to keep a replay; otherwise recording starts from the live edge",
    )));
    settings
        .bind(
            crate::settings::key::LIVE_FROM_START,
            &live_from_start,
            "active",
        )
        .build();
    video_live_group.add(&live_from_start);
    let video_tools_group = adw::PreferencesGroup::builder()
        .title(gettext("Support tools"))
        .description(gettext("yt-dlp and ffmpeg resolve media pages"))
        .build();
    let video_tools_row = adw::ActionRow::builder()
        .title(gettext("Media support tools"))
        // Subtitles carry version strings and raw install errors: plain text.
        .use_markup(false)
        .build();
    let video_tools_btn = gtk4::Button::builder().valign(gtk4::Align::Center).build();
    video_tools_row.set_activatable_widget(Some(&video_tools_btn));
    video_tools_row.add_suffix(&video_tools_btn);
    let video_tools_spin = adw::Spinner::new();
    video_tools_spin.set_visible(false);
    video_tools_row.add_suffix(&video_tools_spin);
    video_tools_group.add(&video_tools_row);
    // Probe off the main thread: spawning cold binaries can jank startup.
    // The row shows Checking until versions (or absence) resolve.
    let tool_action = std::rc::Rc::new(std::cell::Cell::new(ToolAction::Check));
    {
        let (row, btn, spin) = (
            video_tools_row.clone(),
            video_tools_btn.clone(),
            video_tools_spin.clone(),
        );
        let dialog_weak = dialog.downgrade();
        let action = tool_action.clone();
        video_tools_row.set_subtitle(&gettext("Checking…"));
        video_tools_btn.set_sensitive(false);
        video_tools_spin.set_visible(true);
        gtk4::glib::spawn_future_local(async move {
            let probed = gio::spawn_blocking(installed_tool_versions)
                .await
                .ok()
                .flatten();
            if dialog_weak.upgrade().is_none() {
                return;
            }
            spin.set_visible(false);
            paint_tools_state(&row, &btn, &action, probed);
            btn.set_sensitive(true);
        });
    }
    {
        let (row, btn, spin) = (
            video_tools_row.clone(),
            video_tools_btn.clone(),
            video_tools_spin.clone(),
        );
        let dialog_weak = dialog.downgrade();
        let action = tool_action.clone();
        // Outside Flatpak the button guides through self-install; the
        // automatic download stays Flatpak-only.
        if !crate::video_tools::in_flatpak() {
            btn.set_label(&gettext("How to Install"));
            btn.set_tooltip_text(Some(&gettext("Show terminal install instructions")));
        }
        let settings_b = settings.clone();
        video_tools_btn.connect_clicked(move |_| {
            if !crate::video_tools::in_flatpak() {
                let (row_b, btn_b, spin_b) = (row.clone(), btn.clone(), spin.clone());
                let dialog_b = dialog_weak.clone();
                let action_b = action.clone();
                crate::install_help::show(&btn, move || {
                    if dialog_b.upgrade().is_none() {
                        return;
                    }
                    refresh_video_tools(&row_b, &btn_b, &spin_b, &action_b);
                });
                return;
            }
            if action.get() == ToolAction::Check {
                // One user-initiated probe; see `ToolAction` for the Check→Update shape.
                let (row_b, btn_b, spin_b) = (row.clone(), btn.clone(), spin.clone());
                let dialog_b = dialog_weak.clone();
                let action_b = action.clone();
                let settings_c = settings_b.clone();
                btn.set_sensitive(false);
                spin.set_visible(true);
                row.set_subtitle(&gettext("Checking for updates…"));
                gtk4::glib::spawn_future_local(async move {
                    // The update probe cannot go through the proxy: a proxied check
                    // would leak the machine IP to GitHub, so skip loudly.
                    let proxied = crate::download::DownloadOptions::from_settings(&settings_c)
                        .proxy_config()
                        .map(|p| p.is_some())
                        .unwrap_or(false);
                    if proxied {
                        if dialog_b.upgrade().is_none() {
                            return;
                        }
                        spin_b.set_visible(false);
                        row_b.set_subtitle(&gettext(
                            "Update checks are skipped while a proxy is configured",
                        ));
                        btn_b.set_sensitive(true);
                        return;
                    }
                    // Probe installed versions off-thread, then ask each tool's
                    // upstream for its latest release tag: yt-dlp/yt-dlp,
                    // boul2gom/ffmpeg-builds, quickjs-ng/quickjs.
                    let installed = gio::spawn_blocking(|| {
                        crate::video::resolve_libraries().ok().map(|libs| {
                            let yt = tool_first_line(&libs.youtube, "--version");
                            let ff = tool_first_line(&libs.ffmpeg, "-version");
                            let qjs = crate::video_tools::find_quickjs()
                                .and_then(|p| tool_first_line(&p, "--version"));
                            (yt, ff, qjs)
                        })
                    })
                    .await
                    .ok()
                    .flatten();
                    // Probe the three upstreams concurrently: three sequential
                    // round-trips would triple the wait and burn three times
                    // the unauthenticated rate limit per click.
                    let (yt_tag, ff_tag, qjs_tag) = tokio::join!(
                        crate::video::latest_ytdlp_tag(),
                        crate::video::latest_ffmpeg_tag(),
                        crate::video::latest_quickjs_tag()
                    );
                    if dialog_b.upgrade().is_none() {
                        return;
                    }
                    spin_b.set_visible(false);
                    let mut updates: Vec<String> = Vec::new();
                    let mut current: Vec<String> = Vec::new();
                    let mut indeterminate = installed.is_none();
                    if let Some((yt, ff, qjs)) = installed {
                        match (yt.as_deref(), yt_tag.as_deref()) {
                            (Some(installed), Some(tag))
                                if crate::video_tools::ytdlp_update_available(installed, tag) =>
                            {
                                updates.push(format!(
                                    "yt-dlp {} → {}",
                                    installed.trim(),
                                    tag.trim()
                                ));
                            }
                            (Some(installed), Some(_)) => {
                                current.push(format!("yt-dlp {}", installed.trim()));
                            }
                            _ => indeterminate = true,
                        }
                        match (ff.as_deref(), ff_tag.as_deref()) {
                            (Some(first_line), Some(tag)) => {
                                let token = crate::video_tools::ffmpeg_version_token(first_line)
                                    .unwrap_or(first_line);
                                if crate::video_tools::tool_update_available(token, tag) {
                                    updates.push(format!("ffmpeg {} → {}", token, tag.trim()));
                                } else {
                                    current.push(format!("ffmpeg {token}"));
                                }
                            }
                            _ => indeterminate = true,
                        }
                        // quickjs is optional (provisioned for YouTube only):
                        // judged when installed, never blocks the check.
                        match (qjs.as_deref(), qjs_tag.as_deref()) {
                            (Some(installed), Some(tag))
                                if crate::video_tools::tool_update_available(installed, tag) =>
                            {
                                updates.push(format!(
                                    "quickjs {} → {}",
                                    installed.trim(),
                                    tag.trim()
                                ));
                            }
                            (Some(installed), Some(_)) => {
                                current.push(format!("quickjs {}", installed.trim()));
                            }
                            (None, _) => {}
                            _ => indeterminate = true,
                        }
                    }
                    if !updates.is_empty() {
                        row_b.set_subtitle(
                            &gettext("Update available: {list}")
                                .replace("{list}", &updates.join(", ")),
                        );
                        btn_b.set_label(&gettext("Update"));
                        action_b.set(ToolAction::Update);
                    } else if !indeterminate {
                        row_b.set_subtitle(
                            &gettext("Up to date • {list}").replace("{list}", &current.join(" • ")),
                        );
                        btn_b.set_label(&gettext("Check for Updates"));
                        action_b.set(ToolAction::Check);
                    } else {
                        row_b.set_subtitle(&gettext("Couldn't check for updates"));
                        btn_b.set_label(&gettext("Check for Updates"));
                        action_b.set(ToolAction::Check);
                    }
                    btn_b.set_sensitive(true);
                });
                return;
            }
            let (row_err, row_ok) = (row.clone(), row.clone());
            let (dialog_err, dialog_ok) = (dialog_weak.clone(), dialog_weak.clone());
            let (btn_b, spin_b, action_b) = (btn.clone(), spin.clone(), action.clone());
            crate::install_progress::run(
                &btn,
                move |err| {
                    if dialog_err.upgrade().is_none() {
                        return;
                    }
                    row_err.set_subtitle(&err);
                },
                move || {
                    if dialog_ok.upgrade().is_none() {
                        return;
                    }
                    refresh_video_tools(&row_ok, &btn_b, &spin_b, &action_b);
                },
            );
        });
    }
    video_page.add(&video_quality_group);
    video_page.add(&video_auth_group);
    video_page.add(&video_tools_group);
    dialog.add(&video_page);
    // In-page group order: the General/Network/Media/Torrent taxonomy, keeping
    // each family's related groups adjacent.
    advanced_page.add(&advanced_general_group);
    advanced_page.add(&advanced_media_group);
    advanced_page.add(&video_post_group);
    advanced_page.add(&video_live_group);
    advanced_page.add(&share_group);
    // Page order: Downloads, Media, Torrent, Network, Advanced (Advanced last).
    dialog.add(&torrent_page);
    dialog.add(&net_page);
    dialog.add(&advanced_page);
    dialog.present(Some(parent));
}
