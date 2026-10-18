//! Gallery probe orchestration for the inline New Download card.
//!
//! The dual-backend probe (`dual_probe`), unified gallery auth
//! (`GalleryAuth`, `resolve_gallery_auth`) and the gallery file picker page
//! (`push_gallery_picker_page`). Step-bound UI stays in `inline_add.rs`; this
//! module has no dependency on it (the sentrux structure check forbids the
//! cycle). The picker's shared widgets come from `window_rows`; the entry
//! grid from `picker_grid`.

use crate::download::DownloadManager;
use crate::picker_grid::PickerGrid;
use crate::window_rows::{
    apply_scheduled_at, picker_enter_confirms, picker_header, selection_action_bar,
};
use gettextrs::{gettext, ngettext};
use gtk4::prelude::*;
use libadwaita as adw;
use std::cell::RefCell;
use std::rc::Rc;

/// Outcome of the dual-backend probe. The caller (inline_add.rs) checks
/// staleness, routes the real results with `gallery_probe::route_for_collection`, and owns
/// all UI; this only orchestrates the two probes.
pub(crate) enum DualProbeOutcome {
    /// Both probes completed. The caller derives the `ytdlp_video` route()
    /// input (see [`ytdlp_found_video`]) and routes the gallery items.
    Complete {
        gallery: Result<
            Option<Vec<crate::gallery_probe::GalleryItem>>,
            crate::gallery_tools::GalleryError,
        >,
        ytdlp: Result<
            (crate::video::ProbeResult, Option<serde_json::Value>),
            crate::video_tools::VideoError,
        >,
    },
}

/// Inputs for [`dual_probe`].
pub(crate) struct DualProbeCtx<'a> {
    pub url: &'a str,
    pub browser: String,
    pub newest_first: bool,
    pub proxy: Option<crate::net_types::ResolvedProxy>,
    pub libs: yt_dlp::client::deps::Libraries,
    pub tool: std::path::PathBuf,
}

/// Did yt-dlp find video worth showing? The `ytdlp_video` input of
/// `gallery_probe::route_for_collection`. Pure for tests.
pub(crate) fn ytdlp_found_video(
    result: &Result<
        (crate::video::ProbeResult, Option<serde_json::Value>),
        crate::video_tools::VideoError,
    >,
) -> bool {
    let Ok((probe, _)) = result else {
        return false;
    };
    matches!(
        probe,
        crate::video::ProbeResult::Single(_) | crate::video::ProbeResult::Playlist(_)
    )
}

/// The gallery-dl error worth showing on the failure path, if any: a
/// surfaced error (login, challenge, HTTP) is more actionable than
/// yt-dlp's, but only when yt-dlp found nothing to show — a video result
/// always stands over a gallery-dl complaint.
pub(crate) fn preferred_probe_error(
    ytdlp_video: bool,
    gallery: &Result<
        Option<Vec<crate::gallery_probe::GalleryItem>>,
        crate::gallery_tools::GalleryError,
    >,
) -> Option<crate::gallery_tools::GalleryError> {
    if ytdlp_video {
        return None;
    }
    match gallery {
        Err(e) if crate::gallery_probe::should_surface(e) => Some(e.clone()),
        _ => None,
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

/// Probe both backends for one URL, concurrently: yt-dlp's video check and
/// gallery-dl's item listing. Both results go back to the caller, which
/// routes them with the established `route()`. A gallery-dl hit no longer
/// cancels yt-dlp: the user's video/gallery choice needs yt-dlp's result
/// too, so a lookup takes the slower probe's time, not the faster one's.
///
/// Cookies: yt-dlp probes with --cookies-from-browser, so it starts
/// immediately; gallery-dl resolves its auth first (`resolve_gallery_auth`).
pub(crate) async fn dual_probe(ctx: DualProbeCtx<'_>) -> DualProbeOutcome {
    let url = ctx.url.to_string();
    // yt-dlp starts immediately: it uses --cookies-from-browser, not the
    // exported file below.
    let ytdlp_fut = crate::video::fetch_video_infos(
        ctx.libs,
        url.clone(),
        ctx.browser.clone(),
        ctx.newest_first,
        ctx.proxy,
        None,
    );
    // gallery-dl needs its auth first (handoff jar preferred); the export
    // runs while yt-dlp is already probing.
    let gallery_fut = async {
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
    };

    let (ytdlp, gallery) = tokio::join!(ytdlp_fut, gallery_fut);
    DualProbeOutcome::Complete { gallery, ytdlp }
}

/// The picker header's "{n} files" text; shared by the initial paint and
/// the streamed count updates.
fn header_count_text(n: usize) -> String {
    ngettext("{} file", "{} files", n as u32).replace("{}", &n.to_string())
}

/// Handles into a live gallery-picker page, so a streamed enumeration can
/// fill it in: the count in the header, the error caption, and the gated
/// action bar.
pub(crate) struct GalleryPickerStream {
    grid: PickerGrid,
    count_label: gtk4::Label,
    error_caption: gtk4::Label,
    add_btn: gtk4::Button,
    select_all_btn: gtk4::Button,
    select_none_btn: gtk4::Button,
}

impl GalleryPickerStream {
    /// Append one burst of streamed items and update the running count.
    pub(crate) fn append(&self, items: &[crate::gallery_probe::GalleryItem]) {
        self.grid
            .append(items.iter().map(|item| item.filename.clone()).collect());
        self.count_label
            .set_label(&header_count_text(self.grid.len()));
    }

    /// Stream finished: reveal the truncation notice when the cap was hit
    /// and enable the action bar.
    pub(crate) fn mark_loaded(&self) {
        let n = self.grid.len();
        if n >= crate::gallery_probe::ENUMERATE_MAX_FILES {
            self.count_label.set_label(&format!(
                "{} \u{2022} {}",
                header_count_text(n),
                gettext("Showing the first {} files").replace("{}", &n.to_string())
            ));
        }
        self.add_btn.set_sensitive(true);
        self.select_all_btn.set_sensitive(true);
        self.select_none_btn.set_sensitive(true);
    }

    /// Stream failed after items arrived: keep the partial list selectable
    /// and surface the message.
    pub(crate) fn show_error(&self, message: &str) {
        self.error_caption.set_label(message);
        self.error_caption.set_visible(true);
        self.add_btn.set_sensitive(true);
        self.select_all_btn.set_sensitive(true);
        self.select_none_btn.set_sensitive(true);
    }
}

/// Gallery file picker as a right-sliding card page: one toggle pill per
/// file (all on by default), mirroring the playlist picker. The selection
/// becomes a gallery-dl `--range` spec on the queued job.
///
/// The grid arrives empty and fills from a streamed enumeration (see
/// `GalleryPickerStream`); `None` means another picker page already owns
/// the screen, so the caller should drop its enumeration.
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
    grid: PickerGrid,
) -> Option<GalleryPickerStream> {
    // Don't stack a second picker while one is already visible.
    if nav.visible_page_tag().as_deref() == Some("gallery-picker") {
        return None;
    }

    let list_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(6)
        .build();
    list_box.set_margin_top(12);
    list_box.set_margin_bottom(12);
    list_box.set_margin_start(12);
    list_box.set_margin_end(12);
    // Selection is positional: the indices are computed against the
    // enumeration snapshot and become a gallery-dl `--range` on the queued
    // job. A gallery that gained files in between shifts that mapping, so
    // say so up front. (Queueing the selected file URLs themselves would be
    // the robust fix and needs a design decision.)
    let positional_note = gtk4::Label::builder()
        .label(gettext(
            "Selected by position; reopen this picker if the gallery changed",
        ))
        .css_classes(["dimmed", "caption"])
        .halign(gtk4::Align::Start)
        .build();
    list_box.append(&positional_note);
    // Only the entries scroll; the captions stay outside (see scrolled_grid).
    let scrolled = crate::picker_grid::scrolled_grid(grid.widget());
    list_box.append(&scrolled);
    let error_caption = gtk4::Label::builder()
        .label("")
        .css_classes(["error", "caption"])
        .halign(gtk4::Align::Start)
        .visible(false)
        .build();
    list_box.append(&error_caption);

    let toolbar = adw::ToolbarView::new();
    let (header, count_label) = picker_header(nav, &title, &header_count_text(grid.len()));
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&list_box));
    let (action_bar, select_all_btn, select_none_btn, add_btn) = selection_action_bar();
    toolbar.add_bottom_bar(&action_bar);
    let picker_page = adw::NavigationPage::builder()
        .tag("gallery-picker")
        .title(&title)
        .child(&toolbar)
        .build();
    picker_enter_confirms(&picker_page, &add_btn);

    // Items are still streaming in: nothing may be queued until the stream
    // ends (`mark_loaded`/`show_error` enable the bar), so a partial list
    // can't be submitted by mistake.
    add_btn.set_sensitive(false);
    select_all_btn.set_sensitive(false);
    select_none_btn.set_sensitive(false);

    grid.wire_selection_bar(&select_all_btn, &select_none_btn, &add_btn, |n| {
        ngettext("_Queue {} file", "_Queue {} files", n as u32).replace("{}", &n.to_string())
    });

    {
        let grid = grid.clone();
        let error_caption = error_caption.clone();
        let add_btn_click = add_btn.clone();
        let add_btn_guard = add_btn.clone();
        let nav_pop = nav.clone();
        add_btn_click.connect_clicked(move |_| {
            if !add_btn_guard.is_sensitive() {
                return;
            }
            let selected = grid.selected_indices();
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
            add_btn_guard.set_sensitive(false);
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
                    apply_scheduled_at(&item, scheduled_at);
                    nav_pop.pop();
                    close_card();
                }
                Err(e) => {
                    error_caption.set_text(&e);
                    error_caption.set_visible(true);
                    add_btn_guard.set_sensitive(true);
                }
            }
        });
    }

    nav.push(&picker_page);
    Some(GalleryPickerStream {
        grid,
        count_label,
        error_caption,
        add_btn,
        select_all_btn,
        select_none_btn,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ytdlp_found_video_cases() {
        // A single video counts as "found video".
        assert!(ytdlp_found_video(&Ok((
            crate::video::ProbeResult::Single(crate::video::VideoInfo {
                title: "T".into(),
                page_url: "https://example.com/p".into(),
                formats: Box::default(),
                is_live: false,
                fetchable: true,
            }),
            None,
        ))));
        // A playlist counts too (it plays through the picker).
        assert!(ytdlp_found_video(&Ok((
            crate::video::ProbeResult::Playlist(crate::media_types::PlaylistInfo {
                title: "T".into(),
                page_url: "https://example.com/p".into(),
                kind: crate::media_types::PlaylistKind::Playlist,
                total: 0,
                items: Box::default(),
            }),
            None,
        ))));
        // A probe error found nothing.
        assert!(!ytdlp_found_video(&Err(
            crate::video_tools::VideoError::Message("nope".into())
        )));
    }

    #[test]
    fn preferred_probe_error_cases() {
        // A surfaced gallery error wins when yt-dlp found nothing.
        let surfaced = Err(crate::gallery_tools::GalleryError::probe("HttpError: 403"));
        assert!(preferred_probe_error(false, &surfaced).is_some());
        // A yt-dlp video stands over a gallery-dl complaint.
        assert!(preferred_probe_error(true, &surfaced).is_none());
        // Non-surfaced gallery errors fall through to yt-dlp's error.
        let unsupported = Err(crate::gallery_tools::GalleryError::probe("unsupported url"));
        assert!(preferred_probe_error(false, &unsupported).is_none());
    }
}
