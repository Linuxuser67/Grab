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
use gettextrs::gettext;
use gtk4::prelude::*;
use libadwaita as adw;
use std::cell::RefCell;
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
    // gallery-dl needs the cookie export first (handoff jar preferred); the
    // export runs while yt-dlp is already probing.
    let mut gallery_fut = Box::pin(async {
        let handoff_path = crate::extension_server::handoff_cookie_path(&url);
        let handoff_ua = crate::extension_server::handoff_user_agent(&url);
        let probe_cookies = if handoff_path.is_none() {
            if ctx.browser.is_empty() || ctx.browser == "none" {
                None
            } else if let Some(ytdlp) =
                crate::video_tools::find_in_dirs("yt-dlp", &crate::video_tools::tool_search_dirs())
            {
                crate::cookies::export_cookies_to_file(
                    &ytdlp,
                    ctx.browser.as_str(),
                    &url,
                    std::time::Duration::from_secs(30),
                )
                .await
            } else {
                None
            }
        } else {
            None
        };
        let cookies_path =
            handoff_path.or_else(|| probe_cookies.as_ref().map(|c| c.path().to_path_buf()));
        let r = crate::gallery_probe::probe_gallery(
            &ctx.tool,
            &url,
            cookies_path.as_deref(),
            handoff_ua.as_deref(),
        )
        .await;
        // The exported jar must outlive the probe.
        drop(probe_cookies);
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
