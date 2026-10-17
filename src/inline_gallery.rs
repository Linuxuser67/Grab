//! Gallery probe orchestration for the inline New Download card.
//!
//! The dual-backend probe race (`dual_probe`) and unified gallery auth
//! (`GalleryAuth`, `resolve_gallery_auth`). All UI stays in `inline_add.rs`;
//! this module has no dependency on it (the sentrux structure check forbids
//! the cycle).

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
    // Note: staleness (user pasted a new URL mid-probe) is checked by the
    // caller, which owns the probe state.
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
