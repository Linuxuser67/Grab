//! Gallery probe orchestration for the inline New Download card.
//!
//! The dual-backend probe (`dual_probe`), unified gallery auth
//! (`GalleryAuth`, `resolve_gallery_auth`) and the gallery file picker page
//! (`push_gallery_picker_page`). Step-bound UI stays in `inline_add.rs`; this
//! module has no dependency on it (the sentrux structure check forbids the
//! cycle). The picker's shared widgets come from `window_rows`; the entry
//! grid from `gallery_grid`.

use crate::download::DownloadManager;
use crate::gallery_grid::GalleryGrid;
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
    pub youtube_bin: std::path::PathBuf,
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

/// Authentication source for gallery-dl. The browser-extension handoff jar
/// wins (and preserves its paired user-agent); otherwise gallery-dl reads the
/// selected browser profile directly, avoiding a second yt-dlp export process.
pub(crate) struct GalleryAuth {
    pub cookies_path: Option<std::path::PathBuf>,
    pub cookies_browser: Option<String>,
    pub user_agent: Option<String>,
}

/// Resolve auth without spawning a subprocess or staging cookies.
pub(crate) fn resolve_gallery_auth(url: &str, browser: &str) -> GalleryAuth {
    // The jar and browser user-agent are one identity. Resolve them under one
    // store lookup and one freshness decision so an eviction or freshness
    // boundary cannot pair cookies with a UA from a different handoff.
    let (cookies_path, user_agent) =
        match crate::extension_server::live_handoff_identity(url, browser) {
            Some((path, user_agent)) => (Some(path), user_agent),
            None => (None, None),
        };
    let cookies_browser = if cookies_path.is_none() && !browser.is_empty() && browser != "none" {
        Some(browser.to_string())
    } else {
        None
    };
    GalleryAuth {
        cookies_path,
        cookies_browser,
        user_agent,
    }
}

/// Probe both backends for one URL, concurrently: yt-dlp's video check and
/// gallery-dl's item listing. Both results go back to the caller, which
/// routes them with the established `route()`. A gallery-dl hit no longer
/// cancels yt-dlp: the user's video/gallery choice needs yt-dlp's result
/// too, so a lookup takes the slower probe's time, not the faster one's.
///
/// Cookies: yt-dlp's source (handoff jar or --cookies-from-browser) needs no
/// export step, so it starts immediately; gallery-dl resolves its auth first
/// (`resolve_gallery_auth`).
pub(crate) async fn dual_probe(ctx: DualProbeCtx<'_>) -> DualProbeOutcome {
    let url = ctx.url.to_string();
    // yt-dlp starts immediately: it uses the handoff jar or
    // --cookies-from-browser, not the exported file below.
    let ytdlp_fut = crate::video::fetch_video_infos(
        ctx.youtube_bin,
        url.clone(),
        ctx.browser.clone(),
        ctx.newest_first,
        ctx.proxy.clone(),
        None,
    );
    // Resolve a handoff jar or browser selector without spawning an export
    // process, so gallery-dl and yt-dlp can probe concurrently.
    let gallery_fut = async {
        let auth = resolve_gallery_auth(&url, &ctx.browser);
        let r = crate::gallery_probe::probe_gallery_with_proxy(
            &ctx.tool,
            &url,
            auth.cookies_path.as_deref(),
            auth.cookies_browser.as_deref(),
            auth.user_agent.as_deref(),
            ctx.proxy.as_ref(),
        )
        .await;
        // Keep the chosen cookie source alive until the probe finishes.
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
    grid: GalleryGrid,
    page_url: String,
    count_label: gtk4::Label,
    error_caption: gtk4::Label,
    add_btn: gtk4::Button,
    select_all_btn: gtk4::Button,
    select_none_btn: gtk4::Button,
}

impl GalleryPickerStream {
    /// Append one burst of streamed items and update the running count.
    pub(crate) fn append(&self, items: &[crate::gallery_probe::GalleryItem]) {
        let titles = items
            .iter()
            .map(|item| {
                let item_url = item
                    .extra
                    .get("_gallery_dl_url")
                    .and_then(|value| value.as_str())
                    .unwrap_or(&self.page_url);
                crate::gallery_probe::probe_title(item, item_url)
            })
            .collect::<Vec<_>>();
        self.grid.append(&titles);
        self.count_label
            .set_label(&header_count_text(self.grid.len()));
    }

    /// Stream finished: enable the action bar.
    pub(crate) fn mark_loaded(&self) {
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

/// Gallery file picker as a right-sliding card page: one toggle cell per
/// file (all on by default), mirroring the picker pages. The selection
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
    grid: GalleryGrid,
) -> Option<GalleryPickerStream> {
    // Only the form opens a picker: another picker (playlist, torrent) may
    // already own the card.
    if !crate::window_rows::form_is_visible(nav) {
        return None;
    }
    let page_url = url.clone();

    let list_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(6)
        .build();
    list_box.set_margin_top(12);
    list_box.set_margin_bottom(12);
    list_box.set_margin_start(12);
    list_box.set_margin_end(12);
    // The grid keeps its natural height for small selections and caps its
    // viewport for long galleries; the rest of the page stays content-sized.
    list_box.append(grid.widget());
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
        add_btn_click.connect_clicked(move |_| {
            if !add_btn_guard.is_sensitive() {
                return;
            }
            // Selection is positional: the indices are computed against the
            // enumeration snapshot and become a gallery-dl `--range` on the
            // queued job. A gallery that gained files in between shifts that
            // mapping. (Queueing the selected file URLs themselves would be
            // the robust fix and needs a design decision.)
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
                    // Don't destroy the currently emitting button/page in its
                    // own clicked callback. close_card already pops back to form;
                    // defer that teardown until the signal has unwound.
                    let close = close_card.clone();
                    gtk4::glib::idle_add_local_once(move || close());
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
        page_url,
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
