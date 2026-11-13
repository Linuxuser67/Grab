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
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

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
    let (cookies_path, user_agent) = match crate::extension_server::live_handoff_identity(url) {
        Some((path, user_agent)) => (Some(path), user_agent),
        None => (None, None),
    };
    let cookies_browser = if cookies_path.is_none() {
        let spec = crate::video_tools::gallery_cookies_browser_spec(browser);
        if spec.is_none() && !browser.is_empty() && browser != "none" {
            tracing::warn!(
                browser,
                "gallery-dl cannot read this browser profile directly; browser cookies will be unavailable for this probe"
            );
        }
        spec
    } else {
        None
    };
    GalleryAuth {
        cookies_path,
        cookies_browser,
        user_agent,
    }
}

/// Successful gallery-dl previews are reused for the same page and credentials.
/// The probe spawns a subprocess (2-7 s); the yt-dlp half already caches its
/// metadata, so without this a repeat preview still waits on gallery-dl.
const GALLERY_PREVIEW_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const GALLERY_PREVIEW_CACHE_CAPACITY: usize = 32;

#[derive(Clone, PartialEq, Eq, Hash)]
struct GalleryPreviewCacheKey {
    url: String,
    cookies_browser: Option<String>,
    handoff_cookie_hash: Option<[u8; 32]>,
    user_agent: Option<String>,
    proxy: Option<String>,
}

type GalleryPreviewItems = Option<Vec<crate::gallery_probe::GalleryItem>>;
type GalleryPreviewCacheMap = HashMap<GalleryPreviewCacheKey, (Instant, GalleryPreviewItems)>;
type GalleryPreviewCache = Mutex<GalleryPreviewCacheMap>;

static GALLERY_PREVIEW_CACHE: LazyLock<GalleryPreviewCache> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Cache identity for one gallery-dl preview. Cookie jars are keyed by content
/// so a re-handoff of the same credentials reuses the result despite a new
/// staged path; a browser profile is keyed by the setting, like the manifest
/// preview cache.
fn gallery_preview_cache_key(
    raw_url: &str,
    auth: &GalleryAuth,
    proxy: Option<&crate::net_types::ResolvedProxy>,
) -> Option<GalleryPreviewCacheKey> {
    let mut url = url::Url::parse(raw_url).ok()?;
    url.set_fragment(None);
    if crate::video_cache::has_known_rotating_auth_query(&url) {
        return None;
    }
    let handoff_cookie_hash = match auth.cookies_path.as_deref() {
        Some(path) => Some(crate::extension_server::handoff_cookie_hash(path)?),
        None => None,
    };
    Some(GalleryPreviewCacheKey {
        url: url.to_string(),
        cookies_browser: auth.cookies_browser.clone(),
        handoff_cookie_hash,
        user_agent: auth.user_agent.clone(),
        proxy: proxy.map(|resolved| resolved.cli_url.clone()),
    })
}

/// `Some(None)` is a cached "no items" result; `None` is a miss.
fn gallery_preview_cache_get(key: &GalleryPreviewCacheKey) -> Option<GalleryPreviewItems> {
    let mut cache = GALLERY_PREVIEW_CACHE.lock().ok()?;
    let expired = cache
        .get(key)
        .is_some_and(|(cached_at, _)| cached_at.elapsed() >= GALLERY_PREVIEW_CACHE_TTL);
    if expired {
        cache.remove(key);
        return None;
    }
    cache.get(key).map(|(_, items)| items.clone())
}

fn gallery_preview_cache_insert(key: GalleryPreviewCacheKey, items: &GalleryPreviewItems) {
    let Ok(mut cache) = GALLERY_PREVIEW_CACHE.lock() else {
        return;
    };
    insert_gallery_preview(&mut cache, key, items, Instant::now());
}

fn insert_gallery_preview(
    cache: &mut GalleryPreviewCacheMap,
    key: GalleryPreviewCacheKey,
    items: &GalleryPreviewItems,
    now: Instant,
) {
    // Empty and absent listings are negative results: do not let a transient
    // empty preview suppress a later lookup for the same page.
    if items.as_ref().is_none_or(Vec::is_empty) {
        return;
    }

    cache.retain(|_, (cached_at, _)| {
        now.saturating_duration_since(*cached_at) < GALLERY_PREVIEW_CACHE_TTL
    });
    if cache.len() >= GALLERY_PREVIEW_CACHE_CAPACITY && !cache.contains_key(&key) {
        let oldest_key = cache
            .iter()
            .min_by_key(|(_, (cached_at, _))| *cached_at)
            .map(|(cached_key, _)| cached_key.clone());
        if let Some(oldest_key) = oldest_key {
            cache.remove(&oldest_key);
        }
    }
    cache.insert(key, (now, items.clone()));
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
    let total_started = std::time::Instant::now();
    let url = ctx.url.to_string();
    let host = crate::video_probe::page_host(&url);
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
        let gallery_started = Instant::now();
        let auth = resolve_gallery_auth(&url, &ctx.browser);
        let cache_key = gallery_preview_cache_key(&url, &auth, ctx.proxy.as_ref());
        let cached = cache_key.as_ref().and_then(gallery_preview_cache_get);
        let r = if let Some(items) = cached {
            tracing::debug!(url_host = %host, "gallery-dl preview cache hit");
            Ok(items)
        } else {
            let r = crate::gallery_probe::probe_gallery_with_proxy(
                &ctx.tool,
                &url,
                auth.cookies_path.as_deref(),
                auth.cookies_browser.as_deref(),
                auth.user_agent.as_deref(),
                ctx.proxy.as_ref(),
            )
            .await;
            // Errors may be transient and Retry must actually retry. Re-hash
            // after the probe: a replaced or removed jar must not store its
            // result under the old credentials' key.
            let unchanged = gallery_preview_cache_key(&url, &auth, ctx.proxy.as_ref()) == cache_key;
            if let Ok(items) = &r
                && let Some(key) = cache_key
                && unchanged
            {
                gallery_preview_cache_insert(key, items);
            }
            r
        };
        // Keep the chosen cookie source alive until the probe finishes.
        drop(auth);
        tracing::info!(
            url_host = %host,
            gallery_ms = gallery_started.elapsed().as_millis() as u64,
            result = if r.is_ok() { "ok" } else { "error" },
            "gallery-dl lookup timing"
        );
        r
    };

    let (ytdlp, gallery) = tokio::join!(ytdlp_fut, gallery_fut);
    tracing::info!(
        url_host = %host,
        dual_probe_total_ms = total_started.elapsed().as_millis() as u64,
        gallery_result = if gallery.is_ok() { "ok" } else { "error" },
        ytdlp_result = if ytdlp.is_ok() { "ok" } else { "error" },
        "dual media probe timing"
    );
    DualProbeOutcome::Complete { gallery, ytdlp }
}

/// Handles into a live gallery-picker page, so streamed enumeration can
/// fill the grid and update the error caption and gated action bar.
pub(crate) struct GalleryPickerStream {
    grid: GalleryGrid,
    page_url: String,
    error_caption: gtk4::Label,
    add_btn: gtk4::Button,
    select_all_btn: gtk4::Button,
    select_none_btn: gtk4::Button,
}

impl GalleryPickerStream {
    /// Append one burst of streamed items to the picker grid.
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
#[expect(
    clippy::too_many_arguments,
    reason = "the picker page builder threads live GUI state from the enumeration caller"
)]
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
    let (header, _) = picker_header(nav, &title, "");
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
                file_rows: Vec::new(),
            };
            let manager = Rc::clone(&manager);
            let dest_dir = Rc::clone(&dest_dir);
            let close_card = Rc::clone(&close_card);
            let error_caption = error_caption.clone();
            let add_btn_guard = add_btn_guard.clone();
            gtk4::glib::spawn_future_local(async move {
                let mut job = base_job;
                job.dest_dir = std::path::PathBuf::from(dest_dir.borrow().clone());
                match manager.enqueue_gallery_async_batched(&job).await {
                    Ok((item, guard)) => {
                        apply_scheduled_at(&item, scheduled_at);
                        // Defer picker teardown until the clicked signal unwinds.
                        let close = close_card.clone();
                        gtk4::glib::idle_add_local_once(move || close());
                        drop(guard);
                    }
                    Err(e) => {
                        error_caption.set_text(&e);
                        error_caption.set_visible(true);
                        add_btn_guard.set_sensitive(true);
                    }
                }
            });
        });
    }

    nav.push(&picker_page);
    Some(GalleryPickerStream {
        grid,
        page_url,
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
    fn gallery_auth_resolves_the_extension_cookie_jar_and_paired_user_agent() {
        let url = "https://gallery-handoff.example.test/collection";
        let cookies = [crate::extension_server::Cookie {
            name: "session".to_string(),
            value: "fixture".to_string(),
            domain: None,
            host_only: None,
            path: None,
            secure: None,
            expiration_date: None,
        }];
        let path = crate::extension_server::write_handoff_cookies(url, &cookies)
            .expect("stage extension cookie fixture");
        crate::extension_server::store_handoff_cookies(
            url,
            path.clone(),
            Some("Grab-Test-UA".to_string()),
        );

        let auth = resolve_gallery_auth(url, "none");
        assert_eq!(auth.cookies_path.as_deref(), Some(path.as_path()));
        assert_eq!(auth.user_agent.as_deref(), Some("Grab-Test-UA"));
        assert!(auth.cookies_browser.is_none());

        crate::extension_server::discard_handoff_cookies(url);
    }

    fn cache_auth(
        cookies_path: Option<std::path::PathBuf>,
        cookies_browser: Option<&str>,
        user_agent: Option<&str>,
    ) -> GalleryAuth {
        GalleryAuth {
            cookies_path,
            cookies_browser: cookies_browser.map(str::to_string),
            user_agent: user_agent.map(str::to_string),
        }
    }

    #[test]
    fn gallery_preview_cache_key_separates_urls_auth_and_proxies() {
        let url = "https://gallery-cache.example.test/collection#top";
        let anonymous = cache_auth(None, None, None);
        let base = gallery_preview_cache_key(url, &anonymous, None).expect("anonymous key");
        assert_eq!(
            gallery_preview_cache_key(
                "https://gallery-cache.example.test/collection",
                &anonymous,
                None,
            ),
            Some(base.clone()),
            "the fragment is not part of the page identity"
        );
        let other_url = "https://gallery-cache.example.test/other";
        assert_ne!(
            gallery_preview_cache_key(other_url, &anonymous, None),
            Some(base.clone())
        );
        assert_ne!(
            gallery_preview_cache_key(url, &cache_auth(None, Some("firefox"), None), None),
            Some(base.clone()),
            "a browser profile must not reuse an anonymous probe"
        );
        assert_ne!(
            gallery_preview_cache_key(url, &cache_auth(None, None, Some("Grab-Test-UA")), None),
            Some(base.clone()),
            "the paired user agent is part of the identity"
        );
        let proxy = crate::net_types::ResolvedProxy {
            proxies: Vec::new(),
            cli_url: "socks5h://127.0.0.1:9050".to_string(),
            no_proxy_env: String::new(),
        };
        assert_ne!(
            gallery_preview_cache_key(url, &anonymous, Some(&proxy)),
            Some(base),
            "a proxy can change what the extractor returns"
        );
    }

    #[test]
    fn gallery_preview_cache_hashes_jar_contents_and_rejects_signed_pages() {
        let directory = tempfile::tempdir().expect("temporary cookie directory");
        let first = directory.path().join("first.jar");
        let second = directory.path().join("second.jar");
        let contents = b"# Netscape HTTP Cookie File\n.example.test\tTRUE\t/\tFALSE\t0\tsid\tv\n";
        std::fs::write(&first, contents).expect("first jar");
        std::fs::write(&second, contents).expect("second jar");
        let url = "https://gallery-cache.example.test/collection";

        let first_key = gallery_preview_cache_key(url, &cache_auth(Some(first), None, None), None)
            .expect("handoff key");
        let second_key =
            gallery_preview_cache_key(url, &cache_auth(Some(second.clone()), None, None), None)
                .expect("handoff key");
        assert_eq!(
            first_key, second_key,
            "a new staged path with the same cookies must reuse the probe"
        );

        std::fs::write(&second, b"# Netscape HTTP Cookie File\n").expect("changed jar");
        assert_ne!(
            gallery_preview_cache_key(url, &cache_auth(Some(second), None, None), None)
                .expect("handoff key"),
            first_key,
            "changed cookies must invalidate the probe"
        );

        assert!(
            gallery_preview_cache_key(
                "https://gallery-cache.example.test/collection?token=secret",
                &cache_auth(None, None, None),
                None,
            )
            .is_none(),
            "signed page URLs must not be retained"
        );
    }

    #[test]
    fn gallery_preview_cache_reuses_successes_and_expires_entries() {
        let key = GalleryPreviewCacheKey {
            url: "https://gallery-cache.example.test/reuse".to_string(),
            cookies_browser: None,
            handoff_cookie_hash: None,
            user_agent: None,
            proxy: None,
        };
        let items = Some(vec![crate::gallery_probe::GalleryItem {
            filename: "one.jpg".to_string(),
            extension: "jpg".to_string(),
            category: "image".to_string(),
            extra: serde_json::Map::new(),
        }]);
        gallery_preview_cache_insert(key.clone(), &items);
        let cached = gallery_preview_cache_get(&key).expect("cache hit");
        assert_eq!(cached.as_ref().map(|items| items.len()), Some(1));

        GALLERY_PREVIEW_CACHE.lock().expect("cache lock").insert(
            key.clone(),
            (
                Instant::now() - GALLERY_PREVIEW_CACHE_TTL - Duration::from_secs(1),
                items,
            ),
        );
        assert!(gallery_preview_cache_get(&key).is_none());
    }

    #[test]
    fn gallery_preview_cache_skips_negative_results_and_evicts_oldest_entry() {
        let mut cache = GalleryPreviewCacheMap::new();
        let key = |name: &str| GalleryPreviewCacheKey {
            url: format!("https://gallery-cache.example.test/{name}"),
            cookies_browser: None,
            handoff_cookie_hash: None,
            user_agent: None,
            proxy: None,
        };
        let no_items: GalleryPreviewItems = None;
        let empty_items: GalleryPreviewItems = Some(Vec::new());
        let start = Instant::now();
        let empty_key = key("empty");
        insert_gallery_preview(&mut cache, empty_key.clone(), &no_items, start);
        insert_gallery_preview(&mut cache, empty_key.clone(), &empty_items, start);
        assert!(!cache.contains_key(&empty_key));

        let items: GalleryPreviewItems = Some(vec![crate::gallery_probe::GalleryItem {
            filename: "one.jpg".to_string(),
            extension: "jpg".to_string(),
            category: "image".to_string(),
            extra: serde_json::Map::new(),
        }]);
        for index in 0..GALLERY_PREVIEW_CACHE_CAPACITY {
            let inserted_at = start + Duration::from_secs(index as u64);
            insert_gallery_preview(
                &mut cache,
                key(&format!("item-{index}")),
                &items,
                inserted_at,
            );
        }
        assert_eq!(cache.len(), GALLERY_PREVIEW_CACHE_CAPACITY);

        let overflow_key = key("overflow");
        let inserted_at = start + Duration::from_secs(GALLERY_PREVIEW_CACHE_CAPACITY as u64);
        insert_gallery_preview(&mut cache, overflow_key.clone(), &items, inserted_at);

        assert_eq!(cache.len(), GALLERY_PREVIEW_CACHE_CAPACITY);
        assert!(!cache.contains_key(&key("item-0")));
        assert!(cache.contains_key(&key("item-1")));
        assert!(cache.contains_key(&overflow_key));
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
