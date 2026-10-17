//! Gallery probe orchestration for the inline New Download card.
//!
//! The dual-backend probe (`dual_probe`) and unified gallery auth
//! (`GalleryAuth`, `resolve_gallery_auth`). All UI stays in `inline_add.rs`;
//! this module has no dependency on it (the sentrux structure check forbids
//! the cycle).

/// Outcome of the dual-backend probe. The caller (inline_add.rs) checks
/// staleness, routes the real results with `gallery_probe::route`, and owns
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
/// `gallery_probe::route`. Pure for tests.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ytdlp_found_video_cases() {
        // A single video counts as "found video".
        assert!(ytdlp_found_video(&Ok((
            crate::video::ProbeResult::Single(crate::video::VideoInfo {
                title: "T".into(),
                duration: None,
                page_url: "https://example.com/p".into(),
                expires_at: None,
                formats: Box::default(),
                is_live: false,
                fetchable: true,
            }),
            None,
        ))));
        // A playlist counts too (it plays through the picker).
        assert!(ytdlp_found_video(&Ok((
            crate::video::ProbeResult::Playlist(crate::media_types::PlaylistInfo {
                id: "p".into(),
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
