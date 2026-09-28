//! Add-dialog flow: the new-download dialog, video probe steps, playlist
//! display and submit paths. UI module (gtk/adw + leaves + engines): the
//! window builder opens it, the playlist picker reuses the count label.

use crate::download::DownloadManager;
use crate::window_rows::{default_name_for, error_label, selection_action_bar};
use adw::prelude::*;
use gettextrs::{gettext, ngettext};
use gtk4::prelude::*;
use gtk4::{gio, glib};
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Owns the add dialog's in-flight lookup marker (`video_inflight`) for one
/// resolve kick. The marker is the kicked URL paired with the kick's
/// generation and its unlisted-probe flag. Every exit path of the kick
/// future — early return or landed result — drops the guard, clearing the
/// marker only when this kick's generation is still current, so a stale
/// generation never clears a newer kick's marker.
struct InflightGuard {
    inflight: Rc<RefCell<Option<(String, u64, bool)>>>,
    generation: Rc<Cell<u64>>,
    my: u64,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if self.generation.get() == self.my {
            self.inflight.take();
        }
    }
}

/// Twin suppression for the add dialog's resolve kick: true when a resolve
/// for this exact URL is already running *for the current generation* *with
/// the same unlisted-probe flag*. A stale generation's marker must not
/// suppress a re-kick — its result will lose the generation race and be
/// discarded, so suppressing on the URL alone would leave the dialog with
/// no preview and every retry suppressed until the URL text changes.
/// Likewise an explicit Enter/retry kick (which probes unlisted URLs) is a
/// different resolve from a debounced typing kick and must not be
/// suppressed by it.
fn inflight_suppresses(
    inflight: &Option<(String, u64, bool)>,
    url: &str,
    generation: u64,
    probe_unlisted: bool,
) -> bool {
    matches!(inflight, Some((u, g, p)) if u.as_str() == url && *g == generation && *p == probe_unlisted)
}

/// Present on the active window when there is one, standalone otherwise.
fn present_dialog(dialog: &adw::Dialog) {
    let win = gio::Application::default()
        .and_downcast::<adw::Application>()
        .and_then(|app| app.active_window());
    dialog.present(win.as_ref());
}

/// Widgets of the New Download dialog's video step (details page), managed as
/// one unit: exactly one state visible at a time. The group header itself
/// carries the video identity (title + page URL).
struct VideoStep {
    status: adw::ActionRow,
    group: adw::PreferencesGroup,
    name: adw::EntryRow,
    revert: gtk4::Button,
    quality: adw::ComboRow,
    audio: adw::SwitchRow,
    tools: adw::ActionRow,
    error: adw::ActionRow,
}

/// Queue one probed video from the Add dialog and close it. Shared by the
/// listed-video and unlisted-link branches: both queue the canonical page, never
/// the typed link (which may redirect to an HTML page the plain engine would save
/// as a file); intake errors surface on the details page with Add re-enabled.
#[allow(clippy::too_many_arguments)]
fn submit_probed_single(
    manager: &Rc<DownloadManager>,
    dest: &Rc<RefCell<String>>,
    dialog: &glib::WeakRef<adw::Dialog>,
    step: &Rc<VideoStep>,
    formats: &Rc<RefCell<Vec<Option<String>>>>,
    lookup_add: &Rc<RefCell<Option<gtk4::Button>>>,
    v: &crate::video::VideoInfo,
) {
    let typed = step.name.text().trim().to_string();
    let audio_only = step.audio.is_active();
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
    // pin) falls back to the global preference. Combo rows and formats share one order.
    let selected = step.quality.selected() as usize;
    let format_id = formats.borrow().get(selected).cloned().flatten();
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
    match manager.enqueue_video(
        &v.page_url,
        Some(&dest.borrow()),
        name,
        crate::media_types::VideoChoices {
            quality,
            audio_only,
            video_format_id: format_id,
            is_live: v.is_live,
            playlist_item_id: None,
        },
    ) {
        Ok(_) => {
            if let Some(d) = dialog.upgrade() {
                d.close();
            }
        }
        Err(e) => {
            show_video_error(step, &e);
            set_lookup_add(lookup_add, true);
        }
    }
}

/// Dispatch a fresh preview to its submit path: singles queue with their pinned
/// format, collections open the item picker. The caller's freshness gate stays.
#[allow(clippy::too_many_arguments)]
fn submit_probe(
    manager: &Rc<DownloadManager>,
    dest: &Rc<RefCell<String>>,
    dialog: &glib::WeakRef<adw::Dialog>,
    step: &Rc<VideoStep>,
    formats: &Rc<RefCell<Vec<Option<String>>>>,
    lookup_add: &Rc<RefCell<Option<gtk4::Button>>>,
    nav: &adw::NavigationView,
    probe: crate::video::ProbeResult,
) {
    match probe {
        crate::video::ProbeResult::Single(v) => {
            submit_probed_single(manager, dest, dialog, step, formats, lookup_add, &v);
        }
        crate::video::ProbeResult::Playlist(pl) => {
            // Collections queue through the item picker: one row per chosen
            // entry, each re-resolving its own page at download time.
            push_playlist_items_page(
                nav,
                manager.clone(),
                dest.clone(),
                dialog.clone(),
                pl,
                step.audio.is_active(),
            );
        }
    }
}

/// Report a failed plain-queue fallback on the details page: drop the stale probe, log, show the error, re-enable Add.
fn fallback_plain_failed(
    info: &Rc<RefCell<Option<crate::video::ProbeResult>>>,
    step: &Rc<VideoStep>,
    lookup_add: &Rc<RefCell<Option<gtk4::Button>>>,
    url: &str,
    error: &str,
) {
    info.borrow_mut().take();
    tracing::warn!(
        host = %crate::video_probe::page_host(url),
        error = %error,
        "plain fallback failed"
    );
    show_video_error(step, error);
    set_lookup_add(lookup_add, true);
}

/// Queue a probed link as a plain file and close the dialog: the fallback when
/// extraction finds no playable media on an unlisted page. `Err` when plain intake rejects the URL.
fn queue_plain(
    manager: &Rc<DownloadManager>,
    dest: &Rc<RefCell<String>>,
    dialog: &glib::WeakRef<adw::Dialog>,
    file_row: &adw::EntryRow,
    url: &str,
) -> Result<(), String> {
    let typed = file_row.text().trim().to_string();
    let name = (!typed.is_empty()).then_some(typed);
    manager.enqueue(url, Some(&dest.borrow()), name.as_deref())?;
    if let Some(d) = dialog.upgrade() {
        d.close();
    }
    Ok(())
}

/// Desensitize the details-page Add button while a lookup resolves. No-op until
/// the button exists (see `lookup_add`); every terminal state re-enables it.
fn set_lookup_add(cell: &Rc<RefCell<Option<gtk4::Button>>>, enabled: bool) {
    if let Some(button) = cell.borrow().as_ref() {
        button.set_sensitive(enabled);
    }
}

fn hide_video_step(v: &VideoStep) {
    v.status.set_visible(false);
    v.name.set_visible(false);
    v.revert.set_visible(false);
    v.quality.set_visible(false);
    v.audio.set_visible(false);
    v.tools.set_visible(false);
    v.error.set_visible(false);
}

fn show_video_loading(v: &VideoStep) {
    hide_video_step(v);
    v.status.set_visible(true);
}

fn show_video_ready(v: &VideoStep) {
    hide_video_step(v);
    v.name.set_visible(true);
    v.revert.set_visible(true);
    v.quality.set_visible(true);
    v.audio.set_visible(true);
}

fn show_video_tools_missing(v: &VideoStep, message: &str) {
    hide_video_step(v);
    v.tools.set_subtitle(message);
    v.tools.set_visible(true);
}

fn show_video_error(v: &VideoStep, message: &str) {
    hide_video_step(v);
    v.error.set_subtitle(message);
    v.error.set_visible(true);
}

/// Item-count label for a probed collection, kind-aware ("3 stories").
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

/// Playlist probe state: the group header carries the collection identity (title +
/// item count). The name row and format picker stay hidden (renames and pins don't
/// apply across items); the audio switch stays visible and seeds every queued item.
fn show_video_playlist(v: &VideoStep, pl: &crate::media_types::PlaylistInfo) {
    hide_video_step(v);
    v.group
        .set_title(glib::markup_escape_text(&pl.title).as_str());
    let mut desc = format!(
        "{} • {}",
        playlist_count_label(pl.kind, pl.items.len()),
        glib::markup_escape_text(&pl.page_url)
    );
    if crate::video_probe::playlist_truncated(pl) {
        desc.push_str(" • ");
        desc.push_str(
            &gettext("Showing the first {n} of {total}")
                .replace("{n}", &pl.items.len().to_string())
                .replace("{total}", &pl.total.to_string()),
        );
    }
    v.group.set_description(Some(&desc));
    v.audio.set_visible(true);
}

/// The add dialog's "Torrent file" row: pick a .torrent, offer the per-file
/// switches for multi-file torrents, queue singles directly.
fn wire_torrent_picker(
    torrent_btn: &gtk4::Button,
    manager: Rc<DownloadManager>,
    dest_dir: Rc<RefCell<String>>,
    dialog: glib::WeakRef<adw::Dialog>,
    error_label: gtk4::Label,
) {
    torrent_btn.connect_clicked(move |_| {
        let m = manager.clone();
        let dd = dest_dir.clone();
        let dialog = dialog.clone();
        let error_label = error_label.clone();
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
                    error_label.set_text(&gettext("Could not read that .torrent file"));
                    error_label.set_visible(true);
                    return;
                }
            };
            let (_tname, entries) = match crate::torrent::torrent_file_list(&bytes) {
                Ok(v) => v,
                Err(e) => {
                    error_label.set_text(&e);
                    error_label.set_visible(true);
                    return;
                }
            };
            if entries.len() <= 1 {
                match m.enqueue_torrent_file(bytes, &name, Some(&dd.borrow()), None) {
                    Ok(_) => {
                        if let Some(d) = dialog.upgrade() {
                            d.close();
                        }
                    }
                    Err(e) => {
                        error_label.set_text(&e);
                        error_label.set_visible(true);
                    }
                }
                return;
            }
            show_torrent_files_dialog(m, dd, Some(dialog), name, bytes, entries);
        });
    });
}

/// New-download dialog, optionally pre-filled (drag-and-drop / Open With hands a
/// URL in; the normal lookup flow then takes over, so drops never bypass the
/// media pipeline).
pub fn show_add_dialog(manager: Rc<DownloadManager>, initial_url: Option<&str>) {
    let dialog = adw::Dialog::builder()
        .title(gettext("New Download"))
        .build();
    dialog.set_follows_content_size(true);
    dialog.set_content_width(420);

    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::new();
    page.add(&group);

    let url_row = adw::EntryRow::builder()
        .title(gettext("URL"))
        .text("")
        .show_apply_button(true)
        .activates_default(true)
        .build();
    url_row.set_input_purpose(gtk4::InputPurpose::Url);
    group.add(&url_row);

    let file_row = adw::EntryRow::builder()
        .title(gettext("File name (optional)"))
        .text("")
        .activates_default(true)
        .build();
    group.add(&file_row);

    // Video step (details navigation page): preview rows for video-page
    // URLs. All hidden until a lookup runs; exactly one state shows.
    let video_group = adw::PreferencesGroup::new();
    let video_status = adw::ActionRow::builder()
        .title(gettext("Looking up media…"))
        .build();
    let video_spinner = adw::Spinner::new();
    video_status.add_suffix(&video_spinner);
    video_status.set_visible(false);
    video_group.add(&video_status);
    let video_name = adw::EntryRow::builder()
        .title(gettext("File name"))
        .activates_default(false)
        .build();
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
    video_name.set_visible(false);
    video_revert_btn.set_visible(false);
    video_group.add(&video_name);
    // Format picker, filled per video on resolve: exact pinnable formats,
    // tallest first (the preference preselects the closest row). Starts with
    // and returns to a single Automatic row — global preference, no pin.
    let video_quality = adw::ComboRow::builder()
        .title(gettext("Media format"))
        .subtitle(gettext("Uses your preferred quality"))
        .model(&gtk4::StringList::new(&[gettext("Automatic").as_str()]))
        .build();
    video_quality.set_visible(false);
    video_group.add(&video_quality);
    let video_audio = adw::SwitchRow::builder()
        .title(gettext("Audio only"))
        .subtitle(gettext("Skip the video track"))
        .build();
    video_audio.set_visible(false);
    video_group.add(&video_audio);
    let video_tools = adw::ActionRow::builder()
        .title(gettext("Support tools"))
        .build();
    let video_install_btn = gtk4::Button::builder()
        .label(gettext("Install"))
        .tooltip_text(gettext("Download the yt-dlp support tools"))
        .valign(gtk4::Align::Center)
        .build();
    // Whole-row click hits Install (same pattern as the torrent row below).
    video_tools.set_activatable_widget(Some(&video_install_btn));
    video_tools.add_suffix(&video_install_btn);
    video_tools.set_visible(false);
    video_group.add(&video_tools);
    let video_error = adw::ActionRow::builder()
        .title(gettext("Couldn't load the media preview"))
        .build();
    let video_retry_btn = gtk4::Button::builder()
        .label(gettext("Retry"))
        .valign(gtk4::Align::Center)
        .build();
    video_error.add_suffix(&video_retry_btn);
    video_error.set_visible(false);
    video_group.add(&video_error);
    let step = Rc::new(VideoStep {
        status: video_status,
        group: video_group,
        name: video_name,
        revert: video_revert_btn,
        quality: video_quality,
        audio: video_audio,
        tools: video_tools,
        error: video_error,
    });
    // Dialog-local choices: quality is initialized from Preferences (not
    // bound); audio-only is always off by design — no global preference
    // exists. Exact picks are per lookup, so nothing persists here.
    step.quality.set_selected(0);
    // Audio-only is per-download only (see above); the quality row is moot while on.
    step.audio.set_active(false);
    {
        let q = step.quality.clone();
        step.audio.connect_active_notify(move |sw| {
            q.set_sensitive(!sw.is_active());
        });
    }

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
    group.add(&dest_row);

    let dest_dir = Rc::new(RefCell::new(manager.effective_download_dir()));
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

    let error_label = error_label(&group);
    {
        let el = error_label.clone();
        let ur = url_row.clone();
        url_row.connect_changed(move |_| {
            el.set_visible(false);
            ur.remove_css_class("error");
        });
    }

    // Video resolve machinery: debounced metadata lookup that never blocks the
    // main loop. `video_generation` drops stale completions while the user keeps
    // typing; every async touch re-checks the dialog is still open.
    let video_generation = Rc::new(Cell::new(0u64));
    let video_last_ok = Rc::new(RefCell::new(String::new()));
    let video_info = Rc::new(RefCell::new(None::<crate::video::ProbeResult>));
    // Index-aligned with the format combo rows: exact format ids, or a
    // single `None` for the Automatic row. Reset on every resolve.
    let format_ids: Rc<RefCell<Vec<Option<String>>>> = Rc::new(RefCell::new(vec![None]));
    // Set while the submit path re-arms the apply tick: the changed handler must
    // ignore that synthetic edit, or every failed Enter-submit would re-resolve.
    let video_quiet = Rc::new(Cell::new(false));
    // URL a resolve is currently running for, if any. The submit path kicks
    // while the debounced keystroke lookup may still be in flight; without
    // this both spawn yt-dlp and the loser's result is discarded by the
    // generation guard anyway.
    let video_inflight: Rc<RefCell<Option<(String, u64, bool)>>> = Rc::new(RefCell::new(None));
    // The details-page Add button, desensitized while a lookup is in flight (a
    // dead button says so upfront). Populated once the button exists.
    let lookup_add: Rc<RefCell<Option<gtk4::Button>>> = Rc::new(RefCell::new(None));
    let kick_video = {
        let generation = video_generation.clone();
        let last_ok = video_last_ok.clone();
        let info = video_info.clone();
        let step2 = step.clone();
        let url_row2 = url_row.clone();
        let file_row2 = file_row.clone();
        let dialog_weak = dialog.downgrade();
        let formats_kick = format_ids.clone();
        let settings2 = manager.settings().clone();
        let lookup_add_kick = lookup_add.clone();
        let manager_kick = manager.clone();
        let dest_kick = dest_dir.clone();
        let inflight = video_inflight.clone();
        Rc::new(move |probe_unlisted: bool| {
            // Twin suppression: a resolve for this exact URL is already
            // running for the current generation (Enter while the debounced
            // lookup is still in flight is the usual trigger). The twin's
            // result would lose the generation race anyway — don't spawn a
            // second yt-dlp. The marker carries the owning kick's generation
            // so a stale marker — its resolve already doomed by a generation
            // bump — never suppresses a re-kick for the same URL. It also
            // carries the kick's unlisted-probe flag: an explicit Enter kick
            // probes unlisted URLs, a different resolve from a typing kick.
            let url = url_row2.text().trim().to_string();
            if inflight_suppresses(&inflight.borrow(), &url, generation.get(), probe_unlisted) {
                return;
            }
            let my = generation.get() + 1;
            generation.set(my);
            inflight.replace(Some((url, my, probe_unlisted)));
            let (
                generation_b,
                last_b,
                info_b,
                step_b,
                url_b,
                dialog_b,
                settings_b,
                file_b,
                formats_b,
                lookup_add_b,
                manager_b,
                dest_b,
                inflight_b,
            ) = (
                generation.clone(),
                last_ok.clone(),
                info.clone(),
                step2.clone(),
                url_row2.clone(),
                dialog_weak.clone(),
                settings2.clone(),
                file_row2.clone(),
                formats_kick.clone(),
                lookup_add_kick.clone(),
                manager_kick.clone(),
                dest_kick.clone(),
                inflight.clone(),
            );
            glib::spawn_future_local(async move {
                // Owns the in-flight marker: every exit below clears it for
                // this generation (a stale generation leaves a newer marker).
                let _guard = InflightGuard {
                    inflight: inflight_b,
                    generation: generation_b.clone(),
                    my,
                };
                if dialog_b.upgrade().is_none() {
                    return;
                }
                let url = url_b.text().trim().to_string();
                // Unlisted links probe only on explicit kicks (submit, retry), never while
                // typing. Non-HTTP schemes never probe: magnets have their own flows.
                let probing = probe_unlisted
                    && !crate::video::is_video_page(&url)
                    && crate::video::is_http_url(&url);
                if url.is_empty() || (!crate::video::is_video_page(&url) && !probing) {
                    // A stale probe for another URL must not linger: navigating back
                    // with a fresh typed URL would show the old preview.
                    if !crate::video::preview_fresh(
                        &info_b.borrow(),
                        last_b.borrow().as_str(),
                        &url,
                    ) {
                        hide_video_step(&step_b);
                        info_b.borrow_mut().take();
                        set_lookup_add(&lookup_add_b, true);
                    }
                    return;
                }
                if crate::video::preview_fresh(&info_b.borrow(), last_b.borrow().as_str(), &url) {
                    show_video_ready(&step_b);
                    set_lookup_add(&lookup_add_b, true);
                    return;
                }
                // Fast local tools check first: missing tools show Install
                // with no spinner round-trip.
                let libs = match crate::video::resolve_libraries() {
                    Ok(libs) => libs,
                    Err(e) => {
                        if dialog_b.upgrade().is_none() || generation_b.get() != my {
                            return;
                        }
                        info_b.borrow_mut().take();
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
                        if dialog_b.upgrade().is_none() || generation_b.get() != my {
                            return;
                        }
                        show_video_error(&step_b, &e);
                        set_lookup_add(&lookup_add_b, true);
                        return;
                    }
                };
                match crate::video::fetch_video_infos(
                    libs,
                    url.clone(),
                    settings_b.cookies_browser(),
                    settings_b.video_codec_newest(),
                    proxy,
                )
                .await
                {
                    Err(e) => {
                        if dialog_b.upgrade().is_none() || generation_b.get() != my {
                            return;
                        }
                        // Probed links fall back to today's outcome (queue the file directly)
                        // only when extraction says unsupported; transient failures keep
                        // the error row with retry.
                        if !crate::video::is_video_page(&url)
                            && e.to_string().to_lowercase().contains("unsupported url")
                        {
                            match queue_plain(&manager_b, &dest_b, &dialog_b, &file_b, &url) {
                                Ok(()) => return,
                                Err(pe) => {
                                    fallback_plain_failed(
                                        &info_b,
                                        &step_b,
                                        &lookup_add_b,
                                        &url,
                                        &pe,
                                    );
                                    return;
                                }
                            }
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
                            match queue_plain(&manager_b, &dest_b, &dialog_b, &file_b, &direct) {
                                Ok(()) => return,
                                Err(pe) => {
                                    info_b.borrow_mut().take();
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
                        info_b.borrow_mut().take();
                        tracing::warn!(
                            host = %crate::video_probe::page_host(&url),
                            error = %e.to_string(),
                            "video preview failed"
                        );
                        show_video_error(&step_b, &e.to_string());
                        set_lookup_add(&lookup_add_b, true);
                    }
                    Ok(probe) => {
                        if dialog_b.upgrade().is_none() || generation_b.get() != my {
                            return;
                        }
                        // Resolved but nothing playable, and not a listed video
                        // page: same plain fallback as above.
                        if !probe.fetchable() && !crate::video::is_video_page(&url) {
                            match queue_plain(&manager_b, &dest_b, &dialog_b, &file_b, &url) {
                                Ok(()) => return,
                                Err(pe) => {
                                    fallback_plain_failed(
                                        &info_b,
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
                                let desc =
                                    match v.duration_string.as_deref().filter(|s| !s.is_empty()) {
                                        Some(d) => format!(
                                            "{} • {}",
                                            glib::markup_escape_text(&v.page_url),
                                            glib::markup_escape_text(d)
                                        ),
                                        None => glib::markup_escape_text(&v.page_url).to_string(),
                                    };
                                step_b
                                    .group
                                    .set_title(glib::markup_escape_text(&v.title).as_str());
                                step_b.group.set_description(Some(&desc));
                                // Seed the file name once: an explicit page-1 name wins,
                                // else the title default. Never clobbers an edit here.
                                if step_b.name.text().trim().is_empty() {
                                    let typed = file_b.text().trim().to_string();
                                    let base = if typed.is_empty() {
                                        default_name_for(
                                            &settings_b,
                                            &v.title,
                                            step_b.audio.is_active(),
                                        )
                                    } else {
                                        typed
                                    };
                                    step_b.name.set_text(&base);
                                }
                                *last_b.borrow_mut() = url;
                                // Rebuild the format picker from this resolve (tallest first,
                                // preference preselects the closest row) or a single Automatic
                                // row when nothing is pinnable. Selection resets — a pin must
                                // never carry over.
                                let mut labels = Vec::new();
                                let mut ids: Vec<Option<String>> = Vec::new();
                                for opt in &v.formats {
                                    labels.push(opt.label.to_string());
                                    ids.push(Some(opt.id.to_string()));
                                }
                                if labels.is_empty() {
                                    labels.push(gettext("Automatic"));
                                    ids.push(None);
                                }
                                let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
                                step_b
                                    .quality
                                    .set_model(Some(&gtk4::StringList::new(&refs)));
                                *formats_b.borrow_mut() = ids;
                                step_b
                                    .quality
                                    .set_selected(crate::video::default_quality_index(
                                        &v.formats,
                                        &settings_b.video_quality(),
                                    ) as u32);
                                *info_b.borrow_mut() = Some(crate::video::ProbeResult::Single(v));
                                show_video_ready(&step_b);
                                set_lookup_add(&lookup_add_b, true);
                            }
                            crate::video::ProbeResult::Playlist(pl) => {
                                if pl.items.is_empty() {
                                    info_b.borrow_mut().take();
                                    show_video_error(
                                        &step_b,
                                        &gettext("No items found in this playlist"),
                                    );
                                    set_lookup_add(&lookup_add_b, true);
                                    return;
                                }
                                *last_b.borrow_mut() = url;
                                *info_b.borrow_mut() =
                                    Some(crate::video::ProbeResult::Playlist(pl.clone()));
                                show_video_playlist(&step_b, &pl);
                                set_lookup_add(&lookup_add_b, true);
                            }
                        }
                    }
                }
            });
        })
    };
    // Debounced auto-lookup while typing (600 ms idle); Enter applies
    // immediately through the submit path below.
    {
        let generation = video_generation.clone();
        let kick = kick_video.clone();
        let dialog_weak = dialog.downgrade();
        let step2 = step.clone();
        let info2 = video_info.clone();
        let quiet = video_quiet.clone();
        let file_row2 = file_row.clone();
        url_row.connect_changed(move |row| {
            // Synthetic edit from the apply re-arm below: ignore it.
            if quiet.get() {
                return;
            }
            let text = row.text().trim().to_string();
            // The direct-only file row hides in video mode (the details page has its own
            // name row); a non-empty entry is not lost — the resolve seeds the video name
            // from it. A probed preview counts as video mode while its canonical URL matches.
            let fresh = info2
                .borrow()
                .as_ref()
                .is_some_and(|p| p.page_url() == text);
            file_row2.set_visible(!(crate::video::is_video_page(&text) || fresh));
            // Sync skeleton: leaving video-land (or editing a resolved URL) hides the stale
            // step at once; the debounced kick refills it. `fresh` is deliberately the
            // stricter canonical compare: a mismatch is always safe to hide.
            if !crate::video::is_video_page(&text) || !fresh {
                hide_video_step(&step2);
                if !fresh {
                    info2.borrow_mut().take();
                }
            }
            let my = generation.get() + 1;
            generation.set(my);
            let (generation_b, kick_b, dialog_b) =
                (generation.clone(), kick.clone(), dialog_weak.clone());
            glib::spawn_future_local(async move {
                glib::timeout_future(std::time::Duration::from_millis(600)).await;
                if dialog_b.upgrade().is_none() || generation_b.get() != my {
                    return;
                }
                kick_b(false);
            });
        });
    }
    {
        let step2 = step.clone();
        let kick = kick_video.clone();
        let dialog_weak = dialog.downgrade();
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
            let (dialog_err, dialog_ok) = (dialog_weak.clone(), dialog_weak.clone());
            crate::install_progress::run(
                &btn,
                move |err| {
                    if dialog_err.upgrade().is_none() {
                        return;
                    }
                    step_b.tools.set_subtitle(&err);
                },
                move || {
                    if dialog_ok.upgrade().is_none() {
                        return;
                    }
                    // Re-probe, don't just refresh: an unlisted URL that led
                    // here for missing tools has no preview yet.
                    kick_b(true);
                },
            );
        });
    }
    {
        let kick = kick_video.clone();
        video_retry_btn.connect_clicked(move |_| kick(true));
    }
    // One-click restore of the title default (audio-aware, like submit).
    {
        let (name, audio, info) = (step.name.clone(), step.audio.clone(), video_info.clone());
        let settings = manager.settings().clone();
        step.revert.connect_clicked(move |_| {
            if let Some(p) = info.borrow().as_ref() {
                name.set_text(&default_name_for(&settings, p.title(), audio.is_active()));
                name.grab_focus();
            }
        });
    }
    // Toggling the mode re-seeds an untouched name: the resolve-time seed ran under
    // the other mode, so without this the row keeps a video-container name for an
    // audio download (or vice versa). An edited name is never clobbered.
    {
        let (name, audio, info) = (step.name.clone(), step.audio.clone(), video_info.clone());
        let settings = manager.settings().clone();
        audio.connect_active_notify(move |sw| {
            if let Some(p) = info.borrow().as_ref() {
                let active = sw.is_active();
                let current = name.text().to_string();
                if current.trim().is_empty()
                    || current == default_name_for(&settings, p.title(), !active)
                {
                    name.set_text(&default_name_for(&settings, p.title(), active));
                }
            }
        });
    }

    wire_torrent_picker(
        &torrent_btn,
        manager.clone(),
        dest_dir.clone(),
        dialog.downgrade(),
        error_label.clone(),
    );

    let toolbar = adw::ToolbarView::new();
    let hb = adw::HeaderBar::new();
    hb.set_show_start_title_buttons(false);
    hb.set_show_end_title_buttons(false);
    let cancel_btn = gtk4::Button::builder()
        .label(gettext("_Cancel"))
        .use_underline(true)
        .build();
    let add_btn = gtk4::Button::builder()
        .label(gettext("_Add Download"))
        .use_underline(true)
        .css_classes(["suggested-action"])
        .build();
    hb.pack_start(&cancel_btn);
    hb.pack_end(&add_btn);
    toolbar.add_top_bar(&hb);
    toolbar.set_content(Some(&page));

    // Details page: the video step lives here behind an explicit Continue, so the final
    // Add is unreachable without a resolved preview. The back button is the nav view's.
    let video_page = adw::PreferencesPage::new();
    video_page.add(&step.group);
    let video_toolbar = adw::ToolbarView::new();
    let video_hb = adw::HeaderBar::new();
    // No WM title buttons either end: close paths are the nav back button and Esc, like
    // the sibling dialog headers (which keep an explicit Cancel instead).
    video_hb.set_show_start_title_buttons(false);
    video_hb.set_show_end_title_buttons(false);
    let final_add_btn = gtk4::Button::builder()
        .label(gettext("_Add Download"))
        .use_underline(true)
        .css_classes(["suggested-action"])
        .build();
    video_hb.pack_end(&final_add_btn);
    video_toolbar.add_top_bar(&video_hb);
    video_toolbar.set_content(Some(&video_page));
    // The kick above drives this button's sensitivity (see `lookup_add`).
    lookup_add.replace(Some(final_add_btn.clone()));

    let entry_nav_page = adw::NavigationPage::builder()
        .tag("entry")
        .title(gettext("New Download"))
        .can_pop(false)
        .child(&toolbar)
        .build();
    let video_nav_page = adw::NavigationPage::builder()
        .tag("video")
        .title(gettext("Media Details"))
        .child(&video_toolbar)
        .build();
    let nav = adw::NavigationView::new();
    nav.push(&entry_nav_page);

    dialog.set_child(Some(&nav));
    dialog.set_default_widget(Some(&add_btn));

    crate::ui_util::close_on_click(&cancel_btn, &dialog);
    // One submit path for the Add button and URL apply: video pages go through the Page
    // intake (a matching preview is required so the row stores the resolved page, not a
    // stale URL); everything else keeps the direct enqueue.
    let submit = {
        let m = manager.clone();
        let dd = dest_dir.clone();
        let url_row = url_row.clone();
        let file_row = file_row.clone();
        let error_label = error_label.clone();
        let dialog_weak = dialog.downgrade();
        let info = video_info.clone();
        let last_ok = video_last_ok.clone();
        let step2 = step.clone();
        let kick = kick_video.clone();
        let quiet = video_quiet.clone();
        let nav2 = nav.clone();
        let video_nav_page2 = video_nav_page.clone();
        let formats = format_ids.clone();
        let lookup_add_submit = lookup_add.clone();
        move |rearm_apply: bool| {
            let fail = |message: &str| {
                error_label.set_text(message);
                error_label.set_visible(true);
                url_row.add_css_class("error");
                if rearm_apply {
                    // libadwaita hides its apply tick before emitting `apply`; touch
                    // the text while focused to re-arm it. Quiet so the video changed
                    // handler doesn't treat this as a user edit.
                    quiet.set(true);
                    let current = url_row.text().to_string();
                    url_row.grab_focus();
                    url_row.set_text("");
                    url_row.set_text(&current);
                    quiet.set(false);
                }
            };
            let close = || {
                if let Some(d) = dialog_weak.upgrade() {
                    d.close();
                }
            };
            let url = url_row.text().trim().to_string();
            if crate::video::is_video_page(&url) {
                // Structural guarantee: the final Add lives on the details page, so from
                // the entry page a video URL only advances — it can never queue without
                // its preview.
                if nav2.visible_page_tag().as_deref() != Some("video") {
                    nav2.push(&video_nav_page2);
                    step2.name.grab_focus();
                    kick(false);
                    return;
                }
                // Same freshness gate as the kick skip above: the stored page URL is
                // canonicalized, so only the round-trip key (which text was resolved) decides.
                let ready =
                    if crate::video::preview_fresh(&info.borrow(), last_ok.borrow().as_str(), &url)
                    {
                        info.borrow().clone()
                    } else {
                        None
                    };
                match ready {
                    Some(probe) => submit_probe(
                        &m,
                        &dd,
                        &dialog_weak,
                        &step2,
                        &formats,
                        &lookup_add_submit,
                        &nav2,
                        probe,
                    ),
                    None => {
                        kick(true);
                        show_video_error(
                            &step2,
                            &gettext(
                                "Still looking up the media — wait for the preview, then add.",
                            ),
                        );
                    }
                }
                return;
            }
            // Unlisted http(s) links get one probe for a video path, unless they are obviously
            // direct files (the plain engine downloads those better, with no probe delay). A
            // fresh preview queues like a listed video page (canonical page, never the typed
            // link); anything else skips straight to the plain intake below.
            if crate::video::is_http_url(&url) && !crate::video::is_direct_file_url(&url) {
                if nav2.visible_page_tag().as_deref() != Some("video") {
                    nav2.push(&video_nav_page2);
                    step2.name.grab_focus();
                    kick(true);
                } else if !crate::video::preview_fresh(
                    &info.borrow(),
                    last_ok.borrow().as_str(),
                    &url,
                ) {
                    // Resubmit while already probing: re-kick so the user
                    // gets feedback instead of silence.
                    kick(true);
                    show_video_error(
                        &step2,
                        &gettext("Still looking up the media — wait for the preview, then add."),
                    );
                } else {
                    // Fresh preview on a link that probed video-shaped (e.g. a dai.ly short
                    // link): queue it like a listed video page — without this the press fell
                    // through to a bare return. preview_fresh implies Single or Playlist, so
                    // this is exhaustive.
                    match info.borrow().clone() {
                        Some(crate::video::ProbeResult::Single(v)) => {
                            submit_probed_single(
                                &m,
                                &dd,
                                &dialog_weak,
                                &step2,
                                &formats,
                                &lookup_add_submit,
                                &v,
                            );
                        }
                        Some(crate::video::ProbeResult::Playlist(pl)) => {
                            push_playlist_items_page(
                                &nav2,
                                m.clone(),
                                dd.clone(),
                                dialog_weak.clone(),
                                pl,
                                step2.audio.is_active(),
                            );
                        }
                        None => {}
                    }
                }
                return;
            }
            // Apply on an empty field stays silent (stray Enter); the Add
            // button surfaces the intake error instead.
            if url.is_empty() && rearm_apply {
                return;
            }
            let fname = file_row.text().trim().to_string();
            match m.enqueue(
                &url,
                Some(&dd.borrow()),
                if fname.is_empty() {
                    None
                } else {
                    Some(fname.as_str())
                },
            ) {
                Ok(_) => close(),
                Err(e) => fail(&e),
            }
        }
    };
    let submit = Rc::new(submit);
    {
        let s = submit.clone();
        add_btn.connect_clicked(move |_| s(false));
    }
    {
        let s = submit.clone();
        url_row.connect_apply(move |_| s(true));
    }
    // Details page actions run the same submit: on this page a video URL
    // takes the enqueue branch; anything else falls through to direct.
    {
        let s = submit.clone();
        final_add_btn.connect_clicked(move |_| s(false));
    }
    {
        let s = submit.clone();
        step.name.connect_apply(move |_| s(false));
    }
    // Contextual verb: the entry button continues to details for video and ambiguous http(s)
    // links (probed), and queues obvious direct files plus anything else straight away.
    {
        let b = add_btn.clone();
        let ur = url_row.clone();
        ur.connect_changed(move |row| {
            let text = row.text().trim().to_string();
            if crate::video::is_video_page(&text)
                || (crate::video::is_http_url(&text) && !crate::video::is_direct_file_url(&text))
            {
                b.set_label(&gettext("_Continue"));
            } else {
                b.set_label(&gettext("_Add Download"));
            }
        });
    }

    present_dialog(&dialog);

    // Dropped/opened URLs land here pre-filled: setting the text fires the same changed →
    // debounce → lookup chain as typing, so video pages resolve through the media pipeline.
    if let Some(url) = initial_url.map(str::trim).filter(|u| !u.is_empty())
        && let Ok(normalized) = crate::download::normalize_url(url)
    {
        url_row.set_text(&normalized);
    }

    // Keyboard-first: focus lands in the URL field so typing starts a
    // download with no tab stops (same pattern as the rename dialog).
    url_row.grab_focus();

    // single clipboard read per dialog open; no watch, no polling.
    {
        let url_row = url_row.clone();
        let dialog_weak = dialog.downgrade();
        glib::spawn_future_local(async move {
            let clipboard = gtk4::gdk::Display::default().map(|d| d.clipboard());
            let Some(clipboard) = clipboard else { return };
            let Ok(Some(text)) = clipboard.read_text_future().await else {
                return;
            };
            if dialog_weak.upgrade().is_none() {
                return;
            }
            if !url_row.text().trim().is_empty() {
                return;
            }
            let pasted = text.trim().to_string();
            if let Ok(normalized) = crate::download::normalize_url(&pasted) {
                url_row.set_text(&normalized);
            }
        });
    }
}
/// Seconds as M:SS / H:MM:SS for picker subtitles.
pub(crate) fn fmt_item_duration(secs: i64) -> String {
    let secs = secs.max(0) as u64;
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Wire a picker's selection bar to its checkboxes: the confirm action counts the live
/// selection (`count_label` builds its text — msgids differ per picker) and Select All/None
/// flip every box. With nothing selected the action shows the error label.
fn wire_selection_bar(
    checks: &[gtk4::CheckButton],
    select_all_btn: &gtk4::Button,
    select_none_btn: &gtk4::Button,
    add_btn: &gtk4::Button,
    count_label: impl Fn(usize) -> String + 'static,
) {
    let refresh = Rc::new({
        let add_btn = add_btn.clone();
        let checks = checks.to_vec();
        move || {
            let n = checks.iter().filter(|c| c.is_active()).count();
            add_btn.set_label(&count_label(n));
        }
    });
    for check in checks {
        let refresh = refresh.clone();
        check.connect_toggled(move |_| refresh());
    }
    {
        let checks = checks.to_vec();
        select_all_btn.connect_clicked(move |_| {
            for c in &checks {
                c.set_active(true);
            }
        });
    }
    {
        let checks = checks.to_vec();
        select_none_btn.connect_clicked(move |_| {
            for c in &checks {
                c.set_active(false);
            }
        });
    }
    refresh();
}

/// Item picker for probed playlists, stories and highlights, mirroring
/// [`show_torrent_files_dialog`]: one checkbox per entry, all checked by default
/// (HIG selection). Each chosen item becomes its own queue row through the Page
/// intake, so formats resolve per item at download time; quality follows the
/// global preference (pins don't survive across items) and the dialog's audio-only
/// choice applies to every row. It is a page in the New Download navigation stack
/// (tag "playlist"), not a standalone dialog: one window, one close path, the
/// navigation header's Back button.
fn push_playlist_items_page(
    nav: &adw::NavigationView,
    manager: Rc<DownloadManager>,
    dest_dir: Rc<RefCell<String>>,
    parent: glib::WeakRef<adw::Dialog>,
    playlist: crate::media_types::PlaylistInfo,
    audio_only: bool,
) {
    // Same guard as the video page: don't stack a second picker while one is
    // already visible.
    if nav.visible_page_tag().as_deref() == Some("playlist") {
        return;
    }

    let page = adw::PreferencesPage::new();
    let count = playlist.items.len();
    let group = adw::PreferencesGroup::builder()
        .title(playlist_count_label(playlist.kind, count))
        .build();
    if crate::video_probe::playlist_truncated(&playlist) {
        group.set_description(Some(
            &gettext("Showing the first {n} of {total}")
                .replace("{n}", &count.to_string())
                .replace("{total}", &playlist.total.to_string()),
        ));
    }
    page.add(&group);

    let mut checks = Vec::new();
    for item in &playlist.items {
        let check = gtk4::CheckButton::builder().active(true).build();
        check.update_property(&[gtk4::accessible::Property::Label(&item.title)]);
        let row = adw::ActionRow::builder()
            .title(&*item.title)
            .activatable(true)
            .build();
        if let Some(d) = item.duration {
            row.set_subtitle(&fmt_item_duration(d));
        }
        row.add_prefix(&check);
        {
            let check = check.clone();
            row.connect_activate(move |_| {
                check.set_active(!check.is_active());
            });
        }
        checks.push(check);
        group.add(&row);
    }
    let error_label = error_label(&group);

    // Scrolled: big playlists must not size the dialog off-screen, but the capped natural
    // height lets it grow and shrink with the item count instead of keeping the last size.
    let scrolled = gtk4::ScrolledWindow::builder()
        .child(&page)
        .vexpand(true)
        .propagate_natural_height(true)
        .max_content_height(480)
        .build();

    let toolbar = adw::ToolbarView::new();
    let hb = adw::HeaderBar::new();
    // No WM title buttons either end and no explicit Cancel, like the Media Details page: the
    // nav header's Back button (and Esc) close the picker, selection actions live per HIG.
    hb.set_show_start_title_buttons(false);
    hb.set_show_end_title_buttons(false);
    toolbar.add_top_bar(&hb);
    toolbar.set_content(Some(&scrolled));
    let (action_bar, select_all_btn, select_none_btn, add_btn) = selection_action_bar();
    toolbar.add_bottom_bar(&action_bar);
    let picker_page = adw::NavigationPage::builder()
        .tag("playlist")
        .title(&*playlist.title)
        .child(&toolbar)
        .build();

    // The action counts the live selection (see `wire_selection_bar`).
    wire_selection_bar(&checks, &select_all_btn, &select_none_btn, &add_btn, |n| {
        ngettext("_Queue {} item", "_Queue {} items", n as u32).replace("{}", &n.to_string())
    });

    {
        let parent_weak = parent.clone();
        add_btn.connect_clicked(move |_| {
            let chosen: Vec<(usize, &crate::media_types::PlaylistItem)> = playlist
                .items
                .iter()
                .enumerate()
                .filter(|(i, _)| checks[*i].is_active())
                .collect();
            if chosen.is_empty() {
                error_label.set_text(&gettext("Select at least one item"));
                error_label.set_visible(true);
                return;
            }
            // One persist for the whole import, not one per row.
            let _batch = manager.batch_guard();
            // Multiple items from one collection share a titled subfolder,
            // torrent-style; a lone item keeps the flat behavior.
            let dir = manager.resolve_dir(Some(&dest_dir.borrow()));
            let dir = if chosen.len() > 1 {
                crate::file_names::collection_subdir(&dir, &playlist.title)
            } else {
                dir
            };
            // One readdir for the whole import instead of one per row.
            let existing = crate::video_staging::dir_file_names(std::path::Path::new(&dir));
            // Story segments are addressable as their own pages: queue those so each row
            // re-resolves its own segment instead of the tray (tray + format ids would
            // download the first segment once per row). Attempted unconditionally:
            // highlights and non-story URLs return None and keep the tray.
            let mut failed: Option<String> = None;
            for (i, item) in &chosen {
                let page_url = crate::video_probe::story_segment_url(&playlist.page_url, &item.id)
                    .unwrap_or_else(|| item.page_url.clone());
                let settings = manager.settings();
                let name = default_name_for(settings, &item.title, audio_only);
                if let Err(e) = manager.enqueue_video_staged(
                    &page_url,
                    &dir,
                    Some(&name),
                    crate::media_types::VideoChoices {
                        quality: manager.settings().video_quality(),
                        audio_only,
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
                    failed = Some(e);
                    break;
                }
                // Rows already queued stay queued on a partial failure: uncheck them so a
                // retry submits only the remainder (dedupe is by filename).
                checks[*i].set_active(false);
            }
            if let Some(e) = failed {
                error_label.set_text(&e);
                error_label.set_visible(true);
                return;
            }
            // Complete success closes the whole New Download dialog; a partial failure stays
            // on the picker so the remaining rows (unchecked above) can be retried.
            if let Some(p) = parent_weak.upgrade() {
                p.close();
            }
        });
    }

    // Enter queues the selection while the picker is up; the dialog's previous
    // default widget is restored when the page is popped.
    if let Some(p) = parent.upgrade() {
        let prev_default = p.default_widget();
        p.set_default_widget(Some(&add_btn));
        let parent_weak = parent.clone();
        nav.connect_popped(move |_, popped| {
            if popped.tag().as_deref() == Some("playlist")
                && let Some(p) = parent_weak.upgrade()
            {
                p.set_default_widget(prev_default.as_ref());
            }
        });
    }

    nav.push(&picker_page);
}
/// Multi-file .torrent intake: one switch per file, all on by default. The selection
/// feeds rqbit's `only_files` at add time (no live setter), so it must be chosen here.
pub fn show_torrent_files_dialog(
    manager: Rc<DownloadManager>,
    dest_dir: Rc<RefCell<String>>,
    parent: Option<glib::WeakRef<adw::Dialog>>,
    file_name: String,
    bytes: Vec<u8>,
    entries: Vec<crate::torrent::TorrentFileEntry>,
) {
    let dialog = adw::Dialog::builder().title(&file_name).build();
    dialog.set_follows_content_size(true);
    dialog.set_content_width(420);

    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder()
        .title(gettext("Files"))
        .description(
            ngettext("{} file", "{} files", entries.len() as u32)
                .replace("{}", &entries.len().to_string()),
        )
        .build();
    page.add(&group);

    // HIG selection, not settings: a switch means "a setting is on", a checkbox
    // means "this item is picked". Clicking a row toggles its checkbox.
    let mut checks = Vec::new();
    for e in &entries {
        let check = gtk4::CheckButton::builder().active(true).build();
        check.update_property(&[gtk4::accessible::Property::Label(&e.display_path)]);
        let row = adw::ActionRow::builder()
            .title(&e.display_path)
            .subtitle(crate::file_names::fmt_bytes(e.length))
            .activatable(true)
            .build();
        row.add_prefix(&check);
        {
            let check = check.clone();
            row.connect_activate(move |_| {
                check.set_active(!check.is_active());
            });
        }
        checks.push(check);
        group.add(&row);
    }
    let error_label = error_label(&group);

    let toolbar = adw::ToolbarView::new();
    let hb = adw::HeaderBar::new();
    hb.set_show_start_title_buttons(false);
    hb.set_show_end_title_buttons(false);
    let cancel_btn = gtk4::Button::builder()
        .label(gettext("_Cancel"))
        .use_underline(true)
        .build();
    hb.pack_start(&cancel_btn);
    toolbar.add_top_bar(&hb);
    toolbar.set_content(Some(&page));
    // HIG selection mode: the selection's actions live in a bottom
    // action bar, not the header.
    let (action_bar, select_all_btn, select_none_btn, add_btn) = selection_action_bar();
    toolbar.add_bottom_bar(&action_bar);
    dialog.set_child(Some(&toolbar));
    dialog.set_default_widget(Some(&add_btn));

    // Same as the playlist picker: the action counts the live selection.
    wire_selection_bar(&checks, &select_all_btn, &select_none_btn, &add_btn, |n| {
        ngettext("_Add {} file", "_Add {} files", n as u32).replace("{}", &n.to_string())
    });

    {
        let dialog_weak = dialog.downgrade();
        cancel_btn.connect_clicked(move |_| {
            if let Some(d) = dialog_weak.upgrade() {
                d.close();
            }
        });
    }
    {
        let dialog_weak = dialog.downgrade();
        add_btn.connect_clicked(move |_| {
            let selected: Vec<usize> = checks
                .iter()
                .enumerate()
                .filter(|(_, c)| c.is_active())
                .map(|(i, _)| i)
                .collect();
            if selected.is_empty() {
                error_label.set_text(&gettext("Select at least one file"));
                error_label.set_visible(true);
                return;
            }
            // All on means no filter: pass None, not every index.
            let only = (selected.len() < checks.len()).then_some(selected);
            match manager.enqueue_torrent_file(
                bytes.clone(),
                &file_name,
                Some(&dest_dir.borrow()),
                only,
            ) {
                Ok(_) => {
                    if let Some(d) = dialog_weak.upgrade() {
                        d.close();
                    }
                    if let Some(p) = parent.as_ref().and_then(|w| w.upgrade()) {
                        p.close();
                    }
                }
                Err(e) => {
                    error_label.set_text(&e);
                    error_label.set_visible(true);
                }
            }
        });
    }

    // No gtk Window parent exists here (invoked from an adw::Dialog): present
    // standalone like the no-window fallback above.
    dialog.present(None::<&gtk4::Window>);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inflight_guard_clears_only_for_current_generation() {
        let inflight: Rc<RefCell<Option<(String, u64, bool)>>> = Rc::new(RefCell::new(None));
        let generation = Rc::new(Cell::new(1u64));

        // The owning generation clears the marker on drop.
        inflight.replace(Some(("https://youtu.be/x".to_string(), 1, false)));
        drop(InflightGuard {
            inflight: inflight.clone(),
            generation: generation.clone(),
            my: 1,
        });
        assert!(inflight.borrow().is_none());

        // A stale generation leaves a newer kick's marker alone.
        inflight.replace(Some(("https://youtu.be/y".to_string(), 2, true)));
        generation.set(2);
        drop(InflightGuard {
            inflight: inflight.clone(),
            generation: generation.clone(),
            my: 1,
        });
        assert_eq!(
            inflight.borrow().clone(),
            Some(("https://youtu.be/y".to_string(), 2, true))
        );
    }

    #[test]
    fn twin_kick_for_current_generation_is_suppressed() {
        let marker = Some(("https://youtu.be/a".to_string(), 2, false));
        assert!(inflight_suppresses(&marker, "https://youtu.be/a", 2, false));
        // A different URL is never a twin.
        assert!(!inflight_suppresses(
            &marker,
            "https://youtu.be/b",
            2,
            false
        ));
        // No marker, no suppression.
        assert!(!inflight_suppresses(&None, "https://youtu.be/a", 2, false));
    }

    #[test]
    fn explicit_unlisted_kick_is_not_suppressed_by_typing_kick() {
        // A debounced typing kick (probe_unlisted=false) in flight must not
        // suppress an explicit Enter/retry kick (probe_unlisted=true) for the
        // same URL: the explicit kick's unlisted probe is a different
        // resolve, and suppressing it would show a preview without the
        // unlisted formats the user explicitly asked for.
        let marker = Some(("https://youtu.be/a".to_string(), 2, false));
        assert!(!inflight_suppresses(&marker, "https://youtu.be/a", 2, true));
        // Identical kicks still suppress: no double yt-dlp.
        let marker = Some(("https://youtu.be/a".to_string(), 2, true));
        assert!(inflight_suppresses(&marker, "https://youtu.be/a", 2, true));
    }

    #[test]
    fn stale_inflight_marker_does_not_suppress_rekick() {
        // A -> B -> A: the first A kick's marker goes stale when typing bumps
        // the generation, its result is discarded, and its guard leaves the
        // marker (a newer generation is current). The re-kick for A must
        // still spawn — suppressing on the URL alone would wedge the dialog
        // with no preview and every retry suppressed.
        let mut generation = 0u64;
        generation += 1; // typed A
        generation += 1; // kicked A
        let marker = Some(("https://youtu.be/a".to_string(), generation, false));
        generation += 1; // typed B (debounced kick skipped)
        generation += 1; // typed A again
        assert!(!inflight_suppresses(
            &marker,
            "https://youtu.be/a",
            generation,
            false
        ));
    }
}
