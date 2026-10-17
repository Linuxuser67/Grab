//! Gallery UI helpers for the inline New Download card.
//!
//! Pure move from `inline_add.rs` (no behavior change): the gallery preview
//! (`show_gallery_ready`, `refresh_gallery_info`) and the gallery-dl video
//! fallback submit (`submit_gallery_video`).

use crate::download::DownloadManager;
use crate::inline_add::{
    VideoStep, enqueue_and_close, hide_video_step, set_lookup_add, show_video_error,
    sync_entry_icon,
};
use adw::prelude::*;
use gettextrs::{gettext, ngettext};
use gtk4::prelude::*;
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Show the gallery preview: title, file count, and filenames.
/// Hides the video-specific rows (format selector).
pub(crate) fn show_gallery_ready(
    v: &VideoStep,
    card: crate::gallery_card::GalleryCard,
    title: &str,
) {
    hide_video_step(v);
    v.looking_up.set(false);
    v.is_gallery.set(true);
    // Store the card first so the filter handler can access it.
    *v.gallery_card.borrow_mut() = Some(card);
    // Reset filter to All.
    v.gallery_filter.set_active_name(Some("all"));
    refresh_gallery_info(v, title);
    v.gallery_info.set_visible(true);
    v.gallery_filter_wrap.set_visible(true);
    // Reveal the block.
    v.action_revealer.set_visible(true);
    v.action_revealer.set_reveal_child(true);
    v.group_revealer.set_visible(true);
    v.group_revealer.set_reveal_child(true);
    // The Add button earns its text label.
    v.add_btn.set_icon_name("");
    v.add_btn.remove_css_class("circular");
    v.add_btn.set_label(&gettext("Add"));
    sync_entry_icon(v);
}

/// Refresh the gallery info row from the current card state.
pub(crate) fn refresh_gallery_info(v: &VideoStep, title: &str) {
    if let Some(card) = v.gallery_card.borrow().as_ref() {
        let filter = *card.filter.borrow();
        let total = card.total(filter);
        let count_text = crate::gallery_card::display_count(
            card.file_count(),
            5, // probe cap (--range 1-5)
            total,
        );
        let preview = card.preview_filenames().join(", ");
        let subtitle = format!("{count_text} • {preview}");
        v.gallery_info.set_title(title);
        v.gallery_info.set_subtitle(&subtitle);
    }
}

/// Submit a video URL that yt-dlp can't handle but gallery-dl can.
/// Queues it as a gallery row (option A); the title comes from the gallery probe.
#[allow(clippy::too_many_arguments)]
pub(crate) fn submit_gallery_video(
    manager: &Rc<DownloadManager>,
    dest: &Rc<RefCell<String>>,
    close_card: &Rc<dyn Fn()>,
    url: &str,
    items: &[crate::gallery_probe::GalleryItem],
    scheduled_at: Option<i64>,
    step: &Rc<VideoStep>,
    lookup_add: &Rc<RefCell<Option<gtk4::Button>>>,
) {
    let title = items
        .first()
        .map(|i| crate::gallery_probe::probe_title(i, url))
        .unwrap_or_else(|| url.to_string());
    let settings = manager.settings();
    let cookies_browser = settings.cookies_browser().to_string();
    let speed_limit = crate::gallery_runner::gallery_speed_limit(settings);
    enqueue_and_close(
        manager,
        dest,
        close_card,
        scheduled_at,
        |d| {
            let job = crate::gallery_argv::GalleryJob {
                // Placeholder; enqueue_gallery assigns the real row ID.
                item_id: 0,
                page_url: url.to_string(),
                title: title.clone(),
                // Probe items are capped at 5; don't report a wrong count.
                file_count: None,
                dest_dir: std::path::PathBuf::from(d.unwrap_or(".")),
                // Default to All: a video fallback may still yield images.
                filter: crate::gallery_argv::GalleryFilter::All,
                cookies_browser,
                speed_limit,
                range: None,
            };
            manager.enqueue_gallery(&job)
        },
        |e| {
            show_video_error(step, e);
            set_lookup_add(lookup_add, true);
        },
    );
}
/// Outcome of the dual-backend probe race. The caller (inline_add.rs) owns
/// all UI; this only orchestrates the two probes.
pub(crate) enum DualProbeOutcome {
    /// gallery-dl found actionable items (an image gallery and/or video);
    /// the yt-dlp probe was cancelled and its result is ignored. Route the
    /// items with ytdlp_video=false.
    GalleryWin(Vec<crate::gallery_probe::GalleryItem>),
    /// gallery-dl found nothing usable; yt-dlp's result decides. The gallery
    /// error is Some only when `should_surface` accepted it — shown on the
    /// final error row when yt-dlp also failed.
    YtDlpFallback {
        gallery_error: Option<crate::gallery_tools::GalleryError>,
        ytdlp: Result<
            (crate::video::ProbeResult, Option<serde_json::Value>),
            crate::video_tools::VideoError,
        >,
    },
    /// A newer lookup started while probing: show nothing.
    Stale,
}

/// Inputs for [`dual_probe`].
pub(crate) struct DualProbeCtx<'a> {
    pub url: &'a str,
    pub browser: String,
    pub newest_first: bool,
    pub proxy: Option<crate::net_types::ResolvedProxy>,
    pub libs: yt_dlp::client::deps::Libraries,
    pub tool: std::path::PathBuf,
    pub probe: &'a Rc<RefCell<crate::inline_add::ProbeState>>,
    pub generation: u64,
}

/// Did gallery-dl find anything worth showing on its own? Images and/or
/// video items (route() → Gallery | VideoGalleryDl). An all-unknown item
/// list, an empty find, or a probe error means "not won": yt-dlp decides.
/// Pure for tests.
pub(crate) fn gallery_won(
    result: &Result<
        Option<Vec<crate::gallery_probe::GalleryItem>>,
        crate::gallery_tools::GalleryError,
    >,
) -> bool {
    match result {
        Ok(Some(items)) => {
            let outcome = crate::gallery_probe::route(false, Some(items.as_slice()), None);
            matches!(
                outcome,
                crate::gallery_probe::ProbeOutcome::Gallery
                    | crate::gallery_probe::ProbeOutcome::VideoGalleryDl
            )
        }
        _ => false,
    }
}

/// Auth for a gallery-dl spawn: cookie jar path + user agent. The handoff
/// jar (from the browser extension) is preferred; otherwise cookies are
/// exported from the configured browser (up to 30s). `_staged` keeps an
/// exported jar alive as long as the auth is held.
pub(crate) struct GalleryAuth {
    pub cookies_path: Option<std::path::PathBuf>,
    pub user_agent: Option<String>,
    _staged: Option<crate::cookies::StagedCookies>,
}

/// Resolve gallery-dl auth for a URL: handoff jar first, browser export as
/// fallback. Runs on the worker (not GTK main); the export can take 30s.
pub(crate) async fn resolve_gallery_auth(url: &str, browser: &str) -> GalleryAuth {
    let handoff_path = crate::extension_server::handoff_cookie_path(url);
    let handoff_ua = crate::extension_server::handoff_user_agent(url);
    let staged = if handoff_path.is_none() {
        if browser.is_empty() || browser == "none" {
            None
        } else if let Some(ytdlp) =
            crate::video_tools::find_in_dirs("yt-dlp", &crate::video_tools::tool_search_dirs())
        {
            crate::cookies::export_cookies_to_file(
                &ytdlp,
                browser,
                url,
                std::time::Duration::from_secs(30),
            )
            .await
        } else {
            None
        }
    } else {
        None
    };
    let cookies_path = handoff_path.or_else(|| staged.as_ref().map(|c| c.path().to_path_buf()));
    GalleryAuth {
        cookies_path,
        user_agent: handoff_ua,
        _staged: staged,
    }
}

/// Race gallery-dl against yt-dlp for one URL. gallery-dl's success ends the
/// race: the yt-dlp probe is cancelled (its child SIGKILLed) and its result
/// ignored — no steps after that. Otherwise yt-dlp's result decides.
///
/// Cookies: the handoff jar is preferred; the browser export (up to 30s)
/// runs only for gallery-dl, which needs a cookie file, while yt-dlp is
/// already probing (it uses --cookies-from-browser).
pub(crate) async fn dual_probe(ctx: DualProbeCtx<'_>) -> DualProbeOutcome {
    type YtDlpProbe = Result<
        (crate::video::ProbeResult, Option<serde_json::Value>),
        crate::video_tools::VideoError,
    >;
    type GalleryProbe =
        Result<Option<Vec<crate::gallery_probe::GalleryItem>>, crate::gallery_tools::GalleryError>;

    let url = ctx.url.to_string();
    // Cookie export for gallery-dl only, on the worker (not GTK main).
    // yt-dlp probes with a cancel handle: gallery-dl winning the race aborts
    // it instead of leaving it to finish detached. yt-dlp starts immediately:
    // it uses --cookies-from-browser, not the exported file below.
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    let mut ytdlp_fut = Box::pin(crate::video::fetch_video_infos(
        ctx.libs,
        url.clone(),
        ctx.browser.clone(),
        ctx.newest_first,
        ctx.proxy,
        Some(cancel_rx),
    ));
    // gallery-dl needs its auth first (handoff jar preferred); the export
    // runs while yt-dlp is already probing.
    let mut gallery_fut = Box::pin(async {
        let auth = resolve_gallery_auth(&url, &ctx.browser).await;
        let r = crate::gallery_probe::probe_gallery(
            &ctx.tool,
            &url,
            auth.cookies_path.as_deref(),
            auth.user_agent.as_deref(),
        )
        .await;
        // The auth (and its staged jar) must outlive the probe.
        drop(auth);
        r
    });

    let (gallery_result, ytdlp_result, won): (GalleryProbe, Option<YtDlpProbe>, bool) = tokio::select! {
        g = &mut gallery_fut => {
            let won = gallery_won(&g);
            let y = if won {
                // gallery-dl won: stop yt-dlp, ignore its result. The
                // cancel makes fetch_video_infos abort its worker, so
                // the yt-dlp child dies instead of probing on.
                let _ = cancel_tx.send(());
                let _ = ytdlp_fut.await;
                None
            } else {
                Some(ytdlp_fut.await)
            };
            (g, y, won)
        }
        y = &mut ytdlp_fut => {
            // yt-dlp finished first: nothing to cancel, but gallery-dl's
            // success still wins per the caller's rule.
            let g = gallery_fut.await;
            let won = gallery_won(&g);
            (g, Some(y), won)
        }
    };
    // Discard stale results: the user may have pasted a new URL meanwhile.
    if ctx.probe.borrow().generation() != ctx.generation {
        return DualProbeOutcome::Stale;
    }
    if won {
        let items = gallery_result
            .expect("gallery_won implies Ok(Some)")
            .expect("gallery_won implies Some");
        return DualProbeOutcome::GalleryWin(items);
    }
    let gallery_error = match &gallery_result {
        Err(e) if crate::gallery_probe::should_surface(e) => Some(e.clone()),
        _ => None,
    };
    DualProbeOutcome::YtDlpFallback {
        gallery_error,
        ytdlp: ytdlp_result.expect("ytdlp awaited on every fallthrough path"),
    }
}

/// Gallery file picker as a right-sliding card page: one toggle pill per
/// file (all on by default), mirroring the playlist picker. The selection
/// becomes a gallery-dl `--range` spec on the queued job.
#[allow(clippy::too_many_arguments)]
pub(crate) fn push_gallery_picker_page(
    nav: &adw::NavigationView,
    manager: Rc<DownloadManager>,
    dest_dir: Rc<RefCell<String>>,
    close_card: Rc<dyn Fn()>,
    scheduled_at: Option<i64>,
    title: String,
    url: String,
    filter: crate::gallery_argv::GalleryFilter,
    items: Vec<crate::gallery_probe::GalleryItem>,
) {
    // Don't stack a second picker while one is already visible.
    if nav.visible_page_tag().as_deref() == Some("gallery-picker") {
        return;
    }
    let count = items.len();
    // The enumeration is capped (see ENUMERATE_MAX_FILES): say so when we
    // hit the cap, so the user knows the list is truncated.
    let capped = count >= crate::gallery_probe::ENUMERATE_MAX_FILES;
    let entries: Rc<Vec<(String, String)>> = Rc::new(
        items
            .iter()
            .map(|item| {
                let sub = if item.extension.is_empty() {
                    item.category.clone()
                } else {
                    item.extension.clone()
                };
                (item.filename.clone(), sub)
            })
            .collect(),
    );
    let (wrapbox, picks) = crate::inline_add::picker_list(Rc::clone(&entries));

    let list_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(6)
        .build();
    list_box.set_margin_top(12);
    list_box.set_margin_bottom(12);
    list_box.set_margin_start(12);
    list_box.set_margin_end(12);
    list_box.append(&wrapbox);
    let error_caption = gtk4::Label::builder()
        .label("")
        .css_classes(["error", "caption"])
        .halign(gtk4::Align::Start)
        .visible(false)
        .build();
    list_box.append(&error_caption);

    let scrolled = gtk4::ScrolledWindow::builder()
        .child(&list_box)
        .propagate_natural_height(true)
        .max_content_height(480)
        .vexpand(false)
        .valign(gtk4::Align::Start)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .build();

    let toolbar = adw::ToolbarView::new();
    let count_text =
        ngettext("{} file", "{} files", count as u32).replace("{}", &count.to_string());
    let count_text = if capped {
        format!(
            "{} \u{2022} {}",
            count_text,
            gettext("Showing the first {} files").replace("{}", &count.to_string())
        )
    } else {
        count_text
    };
    toolbar.add_top_bar(&crate::inline_add::picker_header(nav, &title, &count_text));
    toolbar.set_content(Some(&scrolled));
    let (action_bar, select_all_btn, select_none_btn, add_btn) =
        crate::window_rows::selection_action_bar();
    toolbar.add_bottom_bar(&action_bar);
    let picker_page = adw::NavigationPage::builder()
        .tag("gallery-picker")
        .title(&title)
        .child(&toolbar)
        .build();
    crate::inline_add::picker_enter_confirms(&picker_page, &add_btn);

    crate::inline_add::wire_list_selection_bar(
        &picks,
        &select_all_btn,
        &select_none_btn,
        &add_btn,
        |n| ngettext("_Queue {} file", "_Queue {} files", n as u32).replace("{}", &n.to_string()),
    );

    {
        let picks = picks.clone();
        let error_caption = error_caption.clone();
        let add_btn_click = add_btn.clone();
        let nav_pop = nav.clone();
        add_btn_click.connect_clicked(move |_| {
            if !add_btn.is_sensitive() {
                return;
            }
            let selected = crate::inline_add::list_selected(&picks);
            if selected.is_empty() {
                error_caption.set_text(&gettext("Select at least one file"));
                error_caption.set_visible(true);
                return;
            }
            let range = match crate::gallery_argv::range_spec(&selected) {
                Some(spec) => spec,
                None => {
                    error_caption.set_text(&gettext("Select at least one file"));
                    error_caption.set_visible(true);
                    return;
                }
            };
            // Disable while the enqueue runs: a second click would enqueue duplicates.
            add_btn.set_sensitive(false);
            let settings = manager.settings();
            let base_job = crate::gallery_argv::GalleryJob {
                item_id: 0, // enqueue_gallery assigns the real row ID.
                page_url: url.clone(),
                title: title.clone(),
                file_count: Some(selected.len() as u32),
                dest_dir: std::path::PathBuf::from("."),
                filter,
                cookies_browser: settings.cookies_browser().to_string(),
                speed_limit: crate::gallery_runner::gallery_speed_limit(settings),
                range: Some(range),
            };
            // Batch guard defers start_next until the Scheduled status is set.
            let _guard = manager.batch_guard();
            let mut job = base_job.clone();
            job.dest_dir = std::path::PathBuf::from(dest_dir.borrow().clone());
            match manager.enqueue_gallery(&job) {
                Ok(item) => {
                    crate::inline_add::apply_scheduled_at(&item, scheduled_at);
                    nav_pop.pop();
                    close_card();
                }
                Err(e) => {
                    error_caption.set_text(&e);
                    error_caption.set_visible(true);
                    add_btn.set_sensitive(true);
                }
            }
        });
    }

    nav.push(&picker_page);
}

/// Build the gallery "Select files…" opener: reads the current card state
/// (URL, filter, title) at click time, runs the bounded enumeration in the
/// background, then pushes the checklist picker.
pub(crate) fn make_gallery_picker_opener(
    step: Rc<VideoStep>,
    nav: &adw::NavigationView,
    manager: Rc<DownloadManager>,
    dest_dir: Rc<RefCell<String>>,
    close_card: Rc<dyn Fn()>,
    scheduled_at: Rc<Cell<Option<i64>>>,
) -> Rc<dyn Fn()> {
    let step2 = step.clone();
    let nav2 = nav.clone();
    let manager2 = manager.clone();
    let dest2 = dest_dir.clone();
    let close2 = close_card.clone();
    let scheduled2 = scheduled_at.clone();
    Rc::new(move || {
        let (url, filter, title) = match step2.gallery_card.borrow().as_ref() {
            Some(card) => (
                card.url().to_string(),
                *card.filter.borrow(),
                step2.gallery_info.title().to_string(),
            ),
            None => return,
        };
        // A previous enumeration is superseded: abort it (kills
        // gallery-dl) instead of letting it probe on.
        crate::inline_add::abort_enumeration(&step2);
        let my_gen = step2.enumerate_gen.get();
        // Disable the button and show a loading label while enumerating;
        // the picker replaces it on success.
        let loading_label = gettext("Loading…");
        let orig_label = step2.gallery_select_btn.label().unwrap_or_default();
        step2.gallery_select_btn.set_sensitive(false);
        step2.gallery_select_btn.set_label(&loading_label);
        let step3 = step2.clone();
        let nav3 = nav2.clone();
        let manager3 = manager2.clone();
        let dest3 = dest2.clone();
        let close3 = close2.clone();
        let scheduled3 = scheduled2.clone();
        let browser3 = crate::settings::AppSettings::new().cookies_browser();
        glib::spawn_future_local(async move {
            let restore_btn = || {
                step3.gallery_select_btn.set_sensitive(true);
                step3.gallery_select_btn.set_label(&orig_label);
            };
            // Surface enumeration failures on the gallery row instead of
            // only logging: the user clicked a button and deserves feedback.
            let show_error = |msg: &str| {
                step3.gallery_info.set_subtitle(msg);
            };
            let tool = match crate::gallery_argv::resolve_tool() {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("gallery picker: no gallery-dl: {e}");
                    restore_btn();
                    show_error(&gettext(
                        "Could not list files: gallery-dl is not installed",
                    ));
                    return;
                }
            };
            let url_c = url.clone();
            let handle = crate::runtime::tokio_rt().spawn(async move {
                // Unified auth: handoff jar first, browser export as
                // fallback (was handoff-only: auth-gated galleries could
                // list nothing here but download fine).
                let auth =
                    crate::inline_gallery::resolve_gallery_auth(&url_c, browser3.as_str()).await;
                let r = crate::gallery_probe::enumerate_gallery_files(
                    &tool,
                    &url_c,
                    auth.cookies_path.as_deref(),
                    auth.user_agent.as_deref(),
                    filter,
                )
                .await;
                drop(auth);
                r
            });
            *step3.enumerate_abort.borrow_mut() = Some(handle.abort_handle());
            let result = handle.await;
            // Superseded (newer click, card close, new lookup): stay
            // quiet — the newer task owns the UI now.
            if step3.enumerate_gen.get() != my_gen {
                return;
            }
            // Stale guard: the card may have been closed or a new URL
            // pasted while enumerating; don't push a picker for the old
            // gallery onto the new card's nav stack. Restore the button
            // first: it's a shared VideoStep widget, and its default
            // state is valid for whatever lookup is current.
            restore_btn();
            let still_current = step3
                .gallery_card
                .borrow()
                .as_ref()
                .map(|card| card.url() == url)
                .unwrap_or(false);
            if !still_current {
                return;
            }
            match result {
                Ok(Ok(items)) if !items.is_empty() => {
                    push_gallery_picker_page(
                        &nav3,
                        manager3,
                        dest3,
                        close3,
                        scheduled3.get(),
                        title,
                        url,
                        filter,
                        items,
                    );
                }
                Ok(Ok(_)) => {
                    show_error(&gettext("No files found"));
                }
                Ok(Err(e)) => {
                    tracing::warn!("gallery picker: enumeration failed: {e:?}");
                    show_error(&gettext("Could not list files"));
                }
                Err(e) => {
                    // Aborted or panicked: the abort path returns above
                    // via the generation guard, so this is a real failure.
                    tracing::warn!("gallery picker: enumeration task failed: {e:?}");
                    show_error(&gettext("Could not list files"));
                }
            }
        });
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(extension: &str) -> crate::gallery_probe::GalleryItem {
        crate::gallery_probe::GalleryItem {
            filename: format!("file.{extension}"),
            extension: extension.to_string(),
            category: "test".to_string(),
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn gallery_won_cases() {
        // Images: gallery-dl won, yt-dlp is cancelled.
        assert!(gallery_won(&Ok(Some(vec![item("jpg")]))));
        // Video: gallery-dl won (submits via the gallery-dl backend).
        assert!(gallery_won(&Ok(Some(vec![item("mp4")]))));
        // Mixed images+video: won.
        assert!(gallery_won(&Ok(Some(vec![item("jpg"), item("mp4")]))));
        // Unknown-only items: not won, yt-dlp decides.
        assert!(!gallery_won(&Ok(Some(vec![item("xyz")]))));
        // Empty item list: not won.
        assert!(!gallery_won(&Ok(Some(vec![]))));
        // Empty find: not won.
        assert!(!gallery_won(&Ok(None)));
        // Probe error: not won.
        assert!(!gallery_won(&Err(
            crate::gallery_tools::GalleryError::probe("nope")
        )));
    }
}
