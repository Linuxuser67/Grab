//! Application lifecycle: startup (actions once), activate (window),
//! open (URLs/files), shutdown (stop downloads, persist queue).

use crate::download::DownloadManager;
use crate::settings::AppSettings;
use crate::window;
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
    search_toggle: gtk4::ToggleButton,
    add_card: crate::inline_add::AddCard,
}

/// True for an http(s) URL pointing at a .torrent file (case-insensitive).
fn is_remote_torrent_url(uri: &url::Url) -> bool {
    matches!(uri.scheme(), "http" | "https")
        && uri
            .path()
            .rsplit('.')
            .next()
            .is_some_and(|e| e.eq_ignore_ascii_case("torrent"))
}

/// How long a remote .torrent fetch may take overall (headers + body).
/// Per-request, not on the shared client builder: a total timeout there
/// would kill slow legitimate downloads too.
const TORRENT_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// What went wrong fetching a remote .torrent.
#[derive(Debug, PartialEq, Eq)]
enum TorrentFetchError {
    Failed(String),
    TimedOut,
    TooLarge,
}

/// Fetch a remote .torrent's bytes with a streaming size cap. Separated from
/// the toast/picker plumbing so the timeout and the cap are unit-testable.
async fn fetch_remote_torrent_bytes(
    client: &reqwest::Client,
    url: &str,
    timeout: std::time::Duration,
) -> Result<Vec<u8>, TorrentFetchError> {
    // .torrent files are tiny; refuse absurd payloads before buffering them.
    const MAX_TORRENT_BYTES: u64 = 10 * 1024 * 1024;
    let failed = |e: reqwest::Error| {
        if e.is_timeout() {
            TorrentFetchError::TimedOut
        } else {
            TorrentFetchError::Failed(e.to_string())
        }
    };
    let resp = client
        .get(url)
        .timeout(timeout)
        .send()
        .await
        .map_err(failed)?;
    let resp = resp.error_for_status().map_err(failed)?;
    // Enforce the cap while streaming: content_length is advisory, so a
    // hostile endpoint must not be able to fill memory before we notice.
    let mut bytes = Vec::new();
    {
        use futures_util::StreamExt as _;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(failed)?;
            if (bytes.len() as u64) + (chunk.len() as u64) > MAX_TORRENT_BYTES {
                return Err(TorrentFetchError::TooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
    }
    Ok(bytes)
}

/// Fetch a remote .torrent URL and run it through the torrent intake, mirroring
/// the local .torrent file path: the picker for multi-file torrents, direct
/// enqueue otherwise.
async fn intake_remote_torrent(
    manager: Rc<DownloadManager>,
    toasts: Rc<adw::ToastOverlay>,
    add_card: crate::inline_add::AddCard,
    client: reqwest::Client,
    url: String,
    file_name: String,
) -> Routed {
    let fail = |msg: String| {
        toasts.add_toast(adw::Toast::new(&crate::ui_util::esc_markup(&msg)));
    };
    let fetch_url = url.clone();
    let fetch_client = client.clone();
    let fetch = crate::runtime::tokio_rt().spawn(async move {
        fetch_remote_torrent_bytes(&fetch_client, &fetch_url, TORRENT_FETCH_TIMEOUT).await
    });
    let fetched = match fetch.await {
        Ok(result) => result,
        Err(e) => Err(TorrentFetchError::Failed(format!(
            "torrent fetch task failed: {e}"
        ))),
    };
    let bytes = match fetched {
        Ok(b) => b,
        Err(TorrentFetchError::TooLarge) => {
            fail(gettext("That .torrent link is too large"));
            return Routed::Failed;
        }
        Err(TorrentFetchError::TimedOut) => {
            fail(format!(
                "{}: {}",
                gettext("Could not fetch that .torrent link"),
                gettext("timed out")
            ));
            return Routed::Failed;
        }
        Err(TorrentFetchError::Failed(e)) => {
            fail(format!(
                "{}: {e}",
                gettext("Could not fetch that .torrent link")
            ));
            return Routed::Failed;
        }
    };
    let stem = std::path::Path::new(&file_name)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_string());
    // Multi-file torrents offer per-file switches in the inline picker's
    // right-sliding page.
    match crate::torrent::torrent_file_list(&bytes) {
        Ok((_, entries)) if entries.len() > 1 => {
            add_card.open_torrent_picker(file_name, bytes, entries);
            // Picker opened, not queued: return OpenedCard (202) so the
            // extension keeps the browser copy until the user chooses.
            Routed::OpenedCard
        }
        Ok(_) => {
            if let Err(e) = manager.enqueue_torrent_file(bytes, &stem, None, None) {
                fail(e);
                return Routed::Failed;
            }
            Routed::Queued
        }
        Err(e) => {
            fail(e);
            Routed::Failed
        }
    }
}

/// Restore the original URL from a browser-extension handoff.
///
/// The extension sends `grab://<scheme>/<url-without-scheme>` (the `http://`
/// or `https://` prefix is carried as the first path segment because the
/// custom scheme replaces it); the desktop entry registers Grab as the
/// `x-scheme-handler/grab` handler. Older extension versions sent
/// `grab://<url-without-scheme>` with no scheme marker — those default to
/// https, the previous behavior. `grab://magnet:...` passes through as a
/// magnet link. Anything that is not a `grab:` URI is returned unchanged.
fn normalize_grab_uri(raw: &str) -> String {
    let Some(u) = raw
        .parse::<url::Url>()
        .ok()
        .filter(|u| u.scheme() == "grab")
    else {
        return raw.to_string();
    };
    // Magnet links: match the raw text, because parsing turns the ':' into
    // a query delimiter ("grab://magnet:?xt=..." parses with host "magnet").
    // .get(..7): byte index 7 may split a multi-byte char in a crafted URI.
    if let Some(rest) = raw.strip_prefix("grab://")
        && rest
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("magnet:"))
    {
        return format!("magnet:{}", &rest[7..]);
    }
    let Some(rest) = u.as_str().strip_prefix("grab://") else {
        return raw.to_string();
    };
    let (scheme, rest) = match rest.split_once('/') {
        Some(("http", r)) => ("http", r),
        Some(("https", r)) => ("https", r),
        _ => ("https", rest),
    };
    format!("{scheme}://{rest}")
}

/// True when an opened URI should land on the New Download card (pre-filled)
/// instead of enqueueing directly: any http(s) URL that isn't obviously a
/// direct file. The card probes it with yt-dlp exactly like a pasted URL, so
/// extractor-supported pages work with no per-site list; direct files skip
/// the probe delay. Stream manifests aren't direct files, so they land here
/// too (enqueueing would save the manifest XML as a file).
fn handoff_goes_to_card(uri: &url::Url) -> bool {
    // Manifests are called out explicitly: they must keep routing to the card
    // even if their extensions ever land in the direct-file list.
    crate::video::is_http_url(uri.as_str())
        && (!crate::video::is_direct_file_url(uri.as_str())
            || crate::video::is_stream_manifest_url(uri.as_str()))
}

/// True for `grab:` handoffs that would enqueue on their own: plain files,
/// magnets, and remote `.torrent` fetches. The card is its own confirmation
/// (its Add button) — handoffs landing there skip the dialog.
fn grab_handoff_needs_confirm(uri: &url::Url) -> bool {
    !handoff_goes_to_card(uri)
}

/// Whether a `grab:` handoff shows the "Add this download?" dialog: for
/// handoffs that would enqueue on their own when auto-add is off (the toggle
/// opts out of the explicit-OK protection) — and always for links to this
/// computer or a private network, since any web page can fire a `grab:` link
/// at the LAN.
fn handoff_should_confirm(via_grab: bool, uri: &url::Url, auto_add: bool) -> bool {
    let private = uri
        .host_str()
        .is_some_and(crate::download_net::is_local_or_private_host);
    via_grab && (private || (grab_handoff_needs_confirm(uri) && !auto_add))
}

/// `grab:` handoffs bypass the intake's URL normalization, so re-apply its
/// userinfo rejection here: credentials must never be sent to a server as
/// Basic auth on a torrent fetch.
fn check_remote_torrent_uri(uri: &url::Url) -> Result<(), String> {
    if !uri.username().is_empty() || uri.password().is_some() {
        return Err(gettext("URLs with a username/password are not supported"));
    }
    Ok(())
}

/// Route one opened URI: remote .torrent fetch, video card pre-fill, or plain
/// enqueue. Extracted from `connect_open` so the extension-handoff confirm
/// dialog can run it after the user approves.
/// What `route_open_uri` did with a URI.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Routed {
    /// Enqueued, or an async intake was started.
    Queued,
    /// Only the inline card was opened; nothing is downloading yet.
    OpenedCard,
    /// Rejected; the user already saw a toast.
    Failed,
}

async fn route_open_uri(
    manager: Rc<DownloadManager>,
    toasts: Rc<adw::ToastOverlay>,
    add_card: crate::inline_add::AddCard,
    settings: AppSettings,
    uri: url::Url,
) -> Routed {
    // Remote .torrent files: fetch the bytes and run the
    // torrent intake instead of saving the .torrent itself.
    if is_remote_torrent_url(&uri) {
        if let Err(e) = check_remote_torrent_uri(&uri) {
            toasts.add_toast(adw::Toast::new(&crate::ui_util::esc_markup(&e)));
            return Routed::Failed;
        }
        let url = uri.as_str().to_string();
        // Decode percent-encoding and sanitize: the raw path would keep
        // `My%20Show.torrent` as the stem `My%20Show`.
        let file_name = crate::file_names::filename_from_url(uri.as_str());
        // The fetch honors the proxy settings like any other download.
        let proxy =
            match crate::download_net::DownloadOptions::from_settings(&settings).proxy_config() {
                Ok(p) => p,
                Err(e) => {
                    toasts.add_toast(adw::Toast::new(&crate::ui_util::esc_markup(&e)));
                    return Routed::Failed;
                }
            };
        let client = match crate::download_net::http_client_for(proxy.as_ref()) {
            Ok(c) => c,
            Err(e) => {
                toasts.add_toast(adw::Toast::new(&crate::ui_util::esc_markup(&e)));
                return Routed::Failed;
            }
        };
        // Fetch the torrent first; the intake returns the appropriate Routed
        // outcome (Queued/OpenedCard/Failed). The extension deletes the browser
        // copy only on 200 (Queued).
        return intake_remote_torrent(manager, toasts, add_card, client, url, file_name).await;
    }
    // Anything over http(s) that isn't obviously a direct file takes the
    // inline card path (pre-filled): the card probes it with yt-dlp exactly
    // like a pasted URL. Plain enqueue would save a raw page — or a manifest
    // — as a file.
    if handoff_goes_to_card(&uri) {
        add_card.open(Some(uri.as_str().to_string()));
        return Routed::OpenedCard;
    }
    match manager.enqueue(uri.as_str(), None, None) {
        Ok(_) => Routed::Queued,
        Err(e) => {
            toasts.add_toast(adw::Toast::new(&crate::ui_util::esc_markup(&e)));
            Routed::Failed
        }
    }
}

/// Single policy gate for an http(s)/magnet URI that arrived from outside the
/// window: browser-extension POSTs and `grab:` links (`via_grab`) come from
/// pages the user does not control; system-handler opens do not.
///
/// - Handoffs that would enqueue on their own show the confirm dialog first,
///   unless the auto-add toggle opted out of that protection.
/// - A handoff that skips the dialog has no explicit user approval behind it,
///   so its host must resolve to public addresses only: a name pointing at
///   127.0.0.1 or the LAN is refused at connect time.
///
/// `decision` is set for extension handoffs that asked to be told the outcome
/// (`/add?confirm=1`): it is answered `true` once Grab has taken the URL over
/// and `false` when the user declines, so the extension cancels the browser's
/// own download only after Grab really has it. Dropping it unanswered counts
/// as declined.
fn dispatch_handoff(
    s: &State,
    uri: url::Url,
    via_grab: bool,
    decision: Option<tokio::sync::oneshot::Sender<crate::extension_server::HandoffOutcome>>,
) {
    let (manager, toasts, add_card, settings) = (
        s.manager.clone(),
        s.toasts.clone(),
        s.add_card.clone(),
        s.settings.clone(),
    );
    let requires_confirmation =
        handoff_should_confirm(via_grab, &uri, settings.auto_add_downloads());
    // Only a browser handoff that skips confirmation is untrusted. Confirmed
    // handoffs and user-entered URLs retain the normal proxy/resolver behavior.
    if via_grab
        && !requires_confirmation
        && let Some(host) = uri.host_str()
        && !crate::download_net::is_local_or_private_host(host)
    {
        // Upstream proxies resolve targets themselves, bypassing Grab's DNS
        // filter for both extractor and direct-file requests. Refuse every
        // unconfirmed handoff while such a proxy is configured.
        if let Err(e) = crate::subprocess_proxy::validate_proxy_compatibility() {
            toasts.add_toast(adw::Toast::new(&crate::ui_util::esc_markup(&e)));
            answer_handoff(decision, crate::extension_server::HandoffOutcome::Failed);
            return;
        }
        if !crate::download_net::require_public_resolution(host) {
            toasts.add_toast(adw::Toast::new(&gettext(
                "This browser handoff cannot be accepted safely: the safe-host limit was reached.",
            )));
            answer_handoff(decision, crate::extension_server::HandoffOutcome::Failed);
            return;
        }
    }
    if !requires_confirmation {
        // Route async (remote torrent fetch needs to complete before ack).
        glib::spawn_future_local(async move {
            let routed = route_open_uri(manager, toasts, add_card, settings, uri).await;
            answer_handoff(decision, routed.into());
        });
        return;
    }
    // Validate before the dialog: the user should approve the canonical URL,
    // not a raw string that enqueue would reject (overlong, userinfo, etc.).
    let canonical = match crate::download_intake::normalize_url(uri.as_str()) {
        Ok(u) => u,
        Err(e) => {
            toasts.add_toast(adw::Toast::new(&crate::ui_util::esc_markup(&e)));
            answer_handoff(decision, crate::extension_server::HandoffOutcome::Failed);
            return;
        }
    };
    let shown: url::Url = canonical.parse().unwrap_or_else(|_| uri.clone());
    // Enqueue the URL the dialog displays, not the raw one.
    let approved = shown.clone();
    // The dialog callbacks are `Fn`, so the one-shot sender sits in a shared
    // slot that the first answer empties. Closing the dialog some other way
    // drops the slot, and with it the sender: the extension sees a decline.
    let decision = std::rc::Rc::new(RefCell::new(decision));
    let on_cancel_decision = std::rc::Rc::clone(&decision);
    confirm_download(
        &s.window,
        &shown,
        move || {
            // Route first, then answer: the extension deletes the browser
            // copy only if Grab really queued it.
            let manager_c = manager.clone();
            let toasts_c = toasts.clone();
            let add_card_c = add_card.clone();
            let settings_c = settings.clone();
            let approved_c = approved.clone();
            let decision_c = decision.clone();
            glib::spawn_future_local(async move {
                let routed =
                    route_open_uri(manager_c, toasts_c, add_card_c, settings_c, approved_c).await;
                answer_handoff(decision_c.borrow_mut().take(), routed.into());
            });
        },
        move || {
            answer_handoff(
                on_cancel_decision.borrow_mut().take(),
                crate::extension_server::HandoffOutcome::Declined,
            )
        },
    );
}

/// Tell a waiting extension request the handoff outcome.
fn answer_handoff(
    decision: Option<tokio::sync::oneshot::Sender<crate::extension_server::HandoffOutcome>>,
    outcome: crate::extension_server::HandoffOutcome,
) {
    if let Some(tx) = decision {
        // The request may have timed out already; nobody is left to tell.
        let _ = tx.send(outcome);
    }
}

/// Map the internal routing result to the handoff outcome.
impl From<Routed> for crate::extension_server::HandoffOutcome {
    fn from(routed: Routed) -> Self {
        match routed {
            Routed::Queued => crate::extension_server::HandoffOutcome::Queued,
            Routed::OpenedCard => crate::extension_server::HandoffOutcome::CardOpened,
            Routed::Failed => crate::extension_server::HandoffOutcome::Failed,
        }
    }
}

/// Process files opened through GApplication after startup has installed
/// the state and the queue manager has restored its saved rows.
fn handle_open_files(s: Rc<State>, files: Vec<gio::File>) {
    glib::spawn_future_local(async move {
        s.manager.wait_for_restore().await;
        if s.manager.is_shutting_down() {
            return;
        }
        if s.manager.restore_failed() {
            s.toasts.add_toast(adw::Toast::new(&gettext(
                "Could not process opened files because the saved queue could not be restored.",
            )));
            return;
        }
        for f in files {
            // grab: links arrive here from the browser extension; restore
            // the original URL so the normal routing below applies.
            let via_grab = f
                .uri()
                .parse::<url::Url>()
                .is_ok_and(|u| u.scheme() == "grab");
            let uri_text = normalize_grab_uri(&f.uri());
            // magnet: links arrive here when Grab is the system's magnet
            // handler; enqueue validates them like pasted links.
            if let Ok(uri) = uri_text.parse::<url::Url>()
                && matches!(uri.scheme(), "http" | "https" | "magnet")
            {
                dispatch_handoff(&s, uri, via_grab, None);
                continue;
            }
            if let Some(path) = f.path() {
                // .torrent files go to the torrent intake.
                if path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("torrent"))
                {
                    let (manager, toasts, add_card) =
                        (s.manager.clone(), s.toasts.clone(), s.add_card.clone());
                    let stem = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "download".to_string());
                    let file_name = path
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| format!("{stem}.torrent"));
                    glib::spawn_future_local(async move {
                        let bytes =
                            gio::spawn_blocking(move || crate::torrent::read_torrent_bytes(&path))
                                .await
                                .ok()
                                .flatten();
                        let Some(bytes) = bytes else {
                            toasts.add_toast(adw::Toast::new(&gettext(
                                "Could not read that .torrent file",
                            )));
                            return;
                        };
                        // Multi-file torrents offer per-file switches in the
                        // inline picker's right-sliding page.
                        match crate::torrent::torrent_file_list(&bytes) {
                            Ok((_, entries)) if entries.len() > 1 => {
                                add_card.open_torrent_picker(file_name, bytes, entries);
                            }
                            Ok(_) => {
                                if let Err(e) =
                                    manager.enqueue_torrent_file(bytes, &stem, None, None)
                                {
                                    toasts.add_toast(adw::Toast::new(&crate::ui_util::esc_markup(
                                        &e,
                                    )));
                                }
                            }
                            Err(e) => {
                                toasts.add_toast(adw::Toast::new(&crate::ui_util::esc_markup(&e)));
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

/// Route startup-open requests once the application state exists; retain them otherwise.
/// Keeping this decision in one place makes early D-Bus/URI delivery testable.
fn dispatch_open_files_or_defer(
    state: Option<Rc<State>>,
    pending: &RefCell<Vec<gio::File>>,
    files: Vec<gio::File>,
) {
    if let Some(state) = state {
        handle_open_files(state, files);
    } else {
        pending.borrow_mut().extend(files);
    }
}

fn take_pending_open_files(pending: &RefCell<Vec<gio::File>>) -> Vec<gio::File> {
    std::mem::take(&mut *pending.borrow_mut())
}

/// Atomically claim the one permitted orderly-shutdown attempt.
/// Ordinary close and explicit Quit share this gate to avoid racing persistence.
pub(crate) fn begin_shutdown_once(
    manager: &DownloadManager,
    shutdown_requested: &std::cell::Cell<bool>,
) -> bool {
    !shutdown_requested.replace(true) && manager.begin_async_shutdown()
}

pub fn setup(app: &adw::Application) {
    let state: Rc<RefCell<Option<Rc<State>>>> = Rc::new(RefCell::new(None));
    // GApplication can deliver open requests before startup has installed State.
    // Retain them and dispatch once the main window and queue manager exist.
    let pending_open_files: Rc<RefCell<Vec<gio::File>>> = Rc::new(RefCell::new(Vec::new()));
    // Set synchronously before scheduling shutdown work so close and explicit
    // Quit cannot race each other before shutdown_async is first polled.
    let shutdown_requested = Rc::new(std::cell::Cell::new(false));

    {
        let st = Rc::clone(&state);
        let pending_open_files = Rc::clone(&pending_open_files);
        let shutdown_requested = Rc::clone(&shutdown_requested);
        app.connect_startup(move |app| {
            // Browser extension integration via HTTP (see extension_server):
            // the server forwards URLs here, and we enqueue them like
            // pasted links.
            let settings = AppSettings::new();
            let store = gio::ListStore::new::<crate::download::DownloadItem>();
            let manager = DownloadManager::new(store, settings.clone());
            manager.restore_queue();
            manager.start_scheduler();

            let toasts = Rc::new(adw::ToastOverlay::new());
            register_actions(app, &st, &shutdown_requested);
            let win = window::build_window(app, manager.clone(), settings.clone(), toasts.clone());
            win.2.widget().set_sensitive(false);
            st.borrow_mut().replace(Rc::new(State {
                manager: manager.clone(),
                settings,
                toasts: toasts.clone(),
                window: win.0.clone(),
                search_toggle: win.1,
                add_card: win.2.clone(),
            }));
            {
                let manager = manager.clone();
                let card = win.2.clone();
                let toasts = toasts.clone();
                glib::spawn_future_local(async move {
                    manager.wait_for_restore().await;
                    if manager.restore_failed() {
                        toasts.add_toast(adw::Toast::new(&gettext(
                            "Could not restore the saved queue. The original queue file was preserved; restart Grab to try again.",
                        )));
                    } else if !manager.is_shutting_down() {
                        card.widget().set_sensitive(true);
                    }
                });
            }
            let pending = take_pending_open_files(&pending_open_files);
            if !pending.is_empty()
                && let Some(s) = st.borrow().as_ref().cloned()
            {
                handle_open_files(s, pending);
            }

            // The normal close path must not block GTK while joining tasks or
            // flushing queue state. Stop the first close, wait asynchronously,
            // then replay the close once shutdown is complete.
            let manager_weak = Rc::downgrade(&manager);
            let closing = Rc::new(std::cell::Cell::new(false));
            let close_ready = Rc::new(std::cell::Cell::new(false));
            win.0.connect_close_request({
                let closing = closing.clone();
                let close_ready = close_ready.clone();
                let shutdown_requested = shutdown_requested.clone();
                move |window| {
                    if close_ready.get() {
                        return glib::Propagation::Proceed;
                    }
                    if closing.replace(true) {
                        return glib::Propagation::Stop;
                    }
                    let Some(manager) = manager_weak.upgrade() else {
                        return glib::Propagation::Proceed;
                    };
                    if !begin_shutdown_once(&manager, shutdown_requested.as_ref()) {
                        return glib::Propagation::Stop;
                    }
                    window.set_sensitive(false);
                    let window = window.clone();
                    let closing_done = closing.clone();
                    let ready = close_ready.clone();
                    glib::MainContext::default().spawn_local(async move {
                        manager.finish_async_shutdown().await;
                        ready.set(true);
                        closing_done.set(false);
                        window.close();
                    });
                    glib::Propagation::Stop
                }
            });

            // Extension HTTP server -> download queue bridge. Routes through
            // the same confirm dialog and route_open_uri as grab:// handoffs.
            let bridge_state = st.clone();
            // Bounded: a flood of handoffs gets 429 from the server instead of
            // piling up confirm dialogs or memory here.
            let (ext_tx, mut ext_rx) =
                tokio::sync::mpsc::channel::<crate::extension_server::Handoff>(crate::extension_server::QUEUE_DEPTH);
            crate::runtime::tokio_rt().spawn(crate::extension_server::serve(ext_tx));
            glib::spawn_future_local(async move {
                while let Some(handoff) = ext_rx.recv().await {
                    // Early `continue`s below drop `decision`: a decline.
                    let (url_text, decision) = (handoff.url, handoff.decision);
                    let s = match bridge_state.borrow().as_ref().cloned() {
                        Some(s) => s,
                        None => continue,
                    };
                    s.manager.wait_for_restore().await;
                    if s.manager.is_shutting_down() {
                        answer_handoff(decision, crate::extension_server::HandoffOutcome::Failed);
                        continue;
                    }
                    if s.manager.restore_failed() {
                        s.toasts.add_toast(adw::Toast::new(&gettext(
                            "Browser links cannot be queued because the saved queue could not be restored.",
                        )));
                        answer_handoff(decision, crate::extension_server::HandoffOutcome::Failed);
                        continue;
                    }
                    // The extension sends the real URL (it unwraps grab:// in JS).
                    // The text is not echoed: it is attacker-controllable.
                    let Ok(uri) = url_text.parse::<url::Url>() else {
                        s.toasts.add_toast(adw::Toast::new(&gettext(
                            "Couldn't read the link sent by the browser extension",
                        )));
                        continue;
                    };
                    if !matches!(uri.scheme(), "http" | "https" | "magnet") {
                        continue;
                    }
                    dispatch_handoff(&s, uri, true, decision);
                }
            });

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
        let pending = Rc::clone(&pending_open_files);
        app.connect_open(move |_, files, _| {
            let state_now = st.borrow().as_ref().cloned();
            dispatch_open_files_or_defer(state_now, &pending, files.to_vec());
        });
    }

    {
        let st = Rc::clone(&state);
        app.connect_shutdown(move |_| {
            if let Some(s) = st.borrow().as_ref() {
                s.manager.shutdown();
            }
            // Staged extension cookies only live in memory; delete their files.
            crate::extension_server::purge_handoff_cookies();
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

/// Confirm dialog for browser-extension handoffs. Any web page can fire
/// `grab:` links, so handoffs that would enqueue on their own (plain files,
/// magnets, remote `.torrent` fetches) only proceed after an explicit OK —
/// the body shows the URL so the user sees what they are approving. Video
/// pages and stream manifests skip this: the New Download card's Add button
/// is already the confirmation.
fn confirm_download(
    parent: &impl gtk4::glib::object::IsA<gtk4::Widget>,
    uri: &url::Url,
    on_confirm: impl Fn() + 'static,
    on_cancel: impl Fn() + 'static,
) {
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Add this download?"))
        .body(crate::ui_util::esc_markup(uri.as_str()))
        .build();
    dialog.add_response("cancel", &gettext("Cancel"));
    dialog.add_response("download", &gettext("Download"));
    dialog.set_response_appearance("download", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.connect_response(None, move |_, response| {
        if response == "download" {
            on_confirm();
        } else {
            on_cancel();
        }
    });
    dialog.present(Some(parent));
}

fn register_actions(
    app: &adw::Application,
    st: &Rc<RefCell<Option<Rc<State>>>>,
    shutdown_requested: &Rc<std::cell::Cell<bool>>,
) {
    let entries = [
        {
            let st = Rc::clone(st);
            gio::ActionEntry::builder("add-download")
                .activate(move |_, _, _| {
                    // Defer past the menu popover's close so the card's
                    // slide-down doesn't collide with it and looks janky.
                    let st = Rc::clone(&st);
                    glib::timeout_add_local_once(
                        std::time::Duration::from_millis(120),
                        move || {
                            if let Some(s) = st.borrow().as_ref() {
                                s.add_card.toggle();
                            }
                        },
                    );
                })
                .build()
        },
        {
            let st = Rc::clone(st);
            gio::ActionEntry::builder("search")
                .activate(move |_, _, _| {
                    if let Some(s) = st.borrow().as_ref() {
                        s.search_toggle.set_active(!s.search_toggle.is_active());
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
                        const METAINFO: &str = "/io/github/linuxuser67/Grab/metainfo.xml";
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
        {
            let st = Rc::clone(st);
            let shutdown_requested = Rc::clone(shutdown_requested);
            gio::ActionEntry::builder("quit")
                .activate(move |app: &adw::Application, _, _| {
                    // Explicit Quit must not pass through close-request's
                    // "hide to background" behavior used for ordinary closes.
                    if let Some(s) = st.borrow().as_ref().cloned() {
                        // Claim shutdown synchronously, before yielding to the
                        // GLib context, so a concurrent close cannot start a
                        // second task that quits before persistence finishes.
                        if !begin_shutdown_once(&s.manager, shutdown_requested.as_ref()) {
                            return;
                        }
                        let app = app.clone();
                        glib::MainContext::default().spawn_local(async move {
                            s.window.set_sensitive(false);
                            s.manager.finish_async_shutdown().await;
                            app.quit();
                        });
                    } else {
                        app.quit();
                    }
                })
                .build()
        },
        gio::ActionEntry::builder("present")
            .activate(|app: &adw::Application, _, _| app.activate())
            .build(),
    ];
    app.add_action_entries(entries);
}

#[cfg(test)]
mod tests {
    use super::{
        TorrentFetchError, check_remote_torrent_uri, dispatch_open_files_or_defer,
        fetch_remote_torrent_bytes, grab_handoff_needs_confirm, handoff_should_confirm,
        is_remote_torrent_url, normalize_grab_uri, take_pending_open_files,
    };
    use gettextrs::gettext;
    use gtk4::prelude::*;

    #[test]
    fn open_requests_before_state_installation_are_retained_and_drained() {
        let pending = std::cell::RefCell::new(Vec::new());
        dispatch_open_files_or_defer(
            None,
            &pending,
            vec![gio::File::for_uri("grab://example.com/first.zip")],
        );
        dispatch_open_files_or_defer(
            None,
            &pending,
            vec![gio::File::for_uri("magnet:?xt=urn:btih:startup-intake")],
        );

        assert_eq!(pending.borrow().len(), 2);
        let drained = take_pending_open_files(&pending);
        assert_eq!(
            drained.iter().map(|file| file.uri()).collect::<Vec<_>>(),
            vec![
                "grab://example.com/first.zip".to_string(),
                "magnet:?xt=urn:btih:startup-intake".to_string(),
            ]
        );
        assert!(pending.borrow().is_empty());
    }

    #[test]
    fn remote_torrent_url_detection() {
        let yes = [
            "https://example.com/ubuntu.torrent",
            "https://example.com/x/UBUNTU.TORRENT?a=1",
            "http://example.com:8080/a/b.torrent",
            "grab://example.com/x.torrent",
        ];
        for raw in yes {
            let normalized = normalize_grab_uri(raw);
            let uri = normalized.parse::<url::Url>().unwrap();
            assert!(is_remote_torrent_url(&uri), "input: {raw}");
        }
        let no = [
            "https://example.com/ubuntu.iso",
            "https://example.com/download?file=x.torrent",
            "https://example.com/torrent/x",
            "magnet:?xt=urn:btih:abc",
        ];
        for raw in no {
            let uri = raw.parse::<url::Url>().unwrap();
            assert!(!is_remote_torrent_url(&uri), "input: {raw}");
        }
    }

    #[test]
    fn grab_uri_restores_https_scheme() {
        assert_eq!(
            normalize_grab_uri("grab://example.com/file.zip"),
            "https://example.com/file.zip"
        );
    }

    #[test]
    fn grab_uri_keeps_query_and_fragment() {
        assert_eq!(
            normalize_grab_uri("grab://example.com/watch?v=abc&t=42#frag"),
            "https://example.com/watch?v=abc&t=42#frag"
        );
    }

    #[test]
    fn grab_uri_keeps_port_and_path() {
        assert_eq!(
            normalize_grab_uri("grab://example.com:8080/a/b?x=1"),
            "https://example.com:8080/a/b?x=1"
        );
    }

    #[test]
    fn non_grab_uris_pass_through_unchanged() {
        for raw in [
            "https://example.com/file.zip",
            "http://example.com/file.zip",
            "magnet:?xt=urn:btih:abc",
            "file:///home/user/x.torrent",
            "not a uri at all",
        ] {
            assert_eq!(normalize_grab_uri(raw), raw, "input: {raw}");
        }
    }

    #[test]
    fn bare_grab_scheme_never_panics() {
        // Produces an unparseable URL that the open handler ignores.
        let out = normalize_grab_uri("grab://");
        assert!(out.parse::<url::Url>().is_err());
    }

    #[test]
    fn grab_uri_preserves_http_scheme() {
        // The original scheme is irretrievably lost if the extension strips
        // it: an http:// original must not silently become https://.
        assert_eq!(
            normalize_grab_uri("grab://http/example.com/file.zip"),
            "http://example.com/file.zip"
        );
    }

    #[test]
    fn grab_uri_preserves_https_scheme() {
        assert_eq!(
            normalize_grab_uri("grab://https/example.com/file.zip"),
            "https://example.com/file.zip"
        );
    }

    #[test]
    fn grab_uri_passes_magnet_through() {
        assert_eq!(
            normalize_grab_uri("grab://magnet:?xt=urn:btih:abc123"),
            "magnet:?xt=urn:btih:abc123"
        );
        // Scheme match is case-insensitive; the rest is untouched.
        assert_eq!(
            normalize_grab_uri("grab://MAGNET:?xt=urn:btih:abc123"),
            "magnet:?xt=urn:btih:abc123"
        );
    }

    #[test]
    fn grab_uri_unicode_prefix_never_panics() {
        // Byte index 7 splits the multi-byte 'é': must not panic, just miss
        // the magnet prefix and fall through to the https default.
        let out = normalize_grab_uri("grab://xxxxxx\u{e9}yyy");
        assert_eq!(out, "https://xxxxxx%C3%A9yyy");
    }

    #[test]
    fn remote_torrent_rejects_userinfo() {
        // grab: handoffs bypass the intake's normalization, so credentials
        // must be rejected here before they reach the HTTP client.
        for raw in [
            "https://user:pass@example.com/x.torrent",
            "https://:pass@example.com/x.torrent",
        ] {
            let uri = raw.parse::<url::Url>().unwrap();
            assert_eq!(
                check_remote_torrent_uri(&uri),
                Err(gettext("URLs with a username/password are not supported")),
                "input: {raw}"
            );
        }
        let ok = "https://example.com/x.torrent".parse::<url::Url>().unwrap();
        assert_eq!(check_remote_torrent_uri(&ok), Ok(()));
    }

    #[test]
    fn grab_handoff_confirm_matrix() {
        // These would enqueue on their own: the dialog must gate them.
        for raw in [
            "https://example.com/file.zip",
            "http://example.com/file.zip",
            "magnet:?xt=urn:btih:abc123",
            "https://example.com/x.torrent",
        ] {
            let uri = raw.parse::<url::Url>().unwrap();
            assert!(grab_handoff_needs_confirm(&uri), "input: {raw}");
        }
        // Video pages and stream manifests land on the New Download card,
        // whose Add button is already the user's confirmation.
        for raw in [
            "https://www.youtube.com/watch?v=x",
            "https://example.com/stream.m3u8",
            // Unlisted pages take the card too: it probes them with yt-dlp
            // exactly like a pasted URL, no per-site list involved.
            "https://example.com/room/some_stream",
            "https://example.com/get?id=123",
        ] {
            let uri = raw.parse::<url::Url>().unwrap();
            assert!(!grab_handoff_needs_confirm(&uri), "input: {raw}");
        }
    }

    /// Read one HTTP request's headers, then hand the socket back. Exiting
    /// with the request unread makes the kernel RST the connection, which
    /// would fail the fetch with "error sending request" instead of the
    /// intended outcome.
    fn read_request_headers(stream: &mut std::net::TcpStream) {
        use std::io::Read as _;
        let mut req = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    req.extend_from_slice(&buf[..n]);
                    if req.windows(4).any(|w| w == b"\r\n\r\n") {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    }

    fn test_client() -> reqwest::Client {
        reqwest::Client::builder().build().unwrap()
    }

    #[tokio::test]
    async fn torrent_fetch_returns_small_body() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::Write as _;
            let (mut stream, _) = listener.accept().unwrap();
            read_request_headers(&mut stream);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ntiny"
            )
            .ok();
        });
        let out = fetch_remote_torrent_bytes(
            &test_client(),
            &format!("http://{addr}/x.torrent"),
            std::time::Duration::from_secs(30),
        )
        .await;
        assert_eq!(out, Ok(b"tiny".to_vec()));
    }

    #[tokio::test]
    async fn torrent_fetch_times_out_on_tarpit() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::Read as _;
            let (mut stream, _) = listener.accept().unwrap();
            read_request_headers(&mut stream);
            // Headers consumed, then silence: the client's per-request
            // timeout must fire instead of hanging forever. The client
            // drops the connection when it times out, so read until EOF
            // instead of sleeping — the thread exits with the test
            // instead of lingering for the old 30s sleep.
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .ok();
            let mut buf = [0u8; 1024];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
        });
        let out = fetch_remote_torrent_bytes(
            &test_client(),
            &format!("http://{addr}/x.torrent"),
            std::time::Duration::from_millis(300),
        )
        .await;
        assert_eq!(out, Err(TorrentFetchError::TimedOut));
    }

    #[tokio::test]
    async fn torrent_fetch_rejects_oversize_stream() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::Write as _;
            let (mut stream, _) = listener.accept().unwrap();
            read_request_headers(&mut stream);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: 11534336\r\nConnection: close\r\n\r\n"
            )
            .ok();
            // 11 MiB of zeros; the client must bail at the 10 MiB cap.
            let zeros = [0u8; 65536];
            for _ in 0..176 {
                if stream.write_all(&zeros).is_err() {
                    break;
                }
            }
        });
        let out = fetch_remote_torrent_bytes(
            &test_client(),
            &format!("http://{addr}/x.torrent"),
            std::time::Duration::from_secs(30),
        )
        .await;
        assert_eq!(out, Err(TorrentFetchError::TooLarge));
    }

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
        // AppStream uses '~' for prereleases (5.6.7~beta.3 < 5.6.7); Cargo uses '-'.
        let (core, suffix) = match v.split_once(['-', '+', '~']) {
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
        let xml = include_str!("../data/io.github.linuxuser67.Grab.metainfo.xml.in");
        let newest = metainfo_release_versions(xml)
            .iter()
            .max_by_key(|v| version_key(v))
            .expect("at least one release")
            .clone();
        // Normalize AppStream's '~' to Cargo's '-': 5.6.8~beta.1 == 5.6.8-beta.1.
        assert_eq!(newest.replace('~', "-"), env!("CARGO_PKG_VERSION"));
    }

    /// The newest entry must also be first: `from_appdata` reads the leading
    /// `<release>`, so an out-of-order file shows the wrong version.
    #[test]
    fn metainfo_lists_newest_release_first() {
        let xml = include_str!("../data/io.github.linuxuser67.Grab.metainfo.xml.in");
        let versions = metainfo_release_versions(xml);
        // Normalize AppStream's '~' to Cargo's '-'.
        assert_eq!(
            versions.first().map(|s| s.replace('~', "-")).as_deref(),
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

    #[test]
    fn handoff_should_confirm_respects_auto_add() {
        // Direct file via grab: needs confirm by default...
        let direct = "https://example.com/file.zip".parse::<url::Url>().unwrap();
        assert!(handoff_should_confirm(true, &direct, false));
        // ...but auto-add skips the dialog.
        // Mutation: drop the `!auto_add` → this assertion fails.
        assert!(!handoff_should_confirm(true, &direct, true));
        // Non-grab handoffs never confirm (not extension-driven).
        assert!(!handoff_should_confirm(false, &direct, false));
        // Video pages go to the card (its Add button is the confirmation).
        let video = "https://example.com/watch?v=1".parse::<url::Url>().unwrap();
        assert!(!handoff_should_confirm(true, &video, false));
    }

    #[test]
    fn handoff_private_host_always_confirms() {
        // A grab: link at the LAN always asks, even with auto-add on:
        // any web page can fire grab: links.
        // Mutation: drop the `private ||` → these assertions fail.
        for raw in [
            "http://192.168.1.1/file.zip",
            "http://10.0.0.9:8080/file.zip",
            "http://localhost/file.zip",
            "http://printer.local/file.zip",
            "http://[::1]/file.zip",
        ] {
            let uri = raw.parse::<url::Url>().unwrap();
            assert!(handoff_should_confirm(true, &uri, true), "input: {raw}");
        }
        // Public hosts keep the old behavior: auto-add skips the dialog.
        let public = "https://example.com/file.zip".parse::<url::Url>().unwrap();
        assert!(!handoff_should_confirm(true, &public, true));
        // Magnets have no host: unchanged.
        let magnet = "magnet:?xt=urn:btih:da39a3ee5e6b4b0d3255bfef95601890afd80709"
            .parse::<url::Url>()
            .unwrap();
        assert!(handoff_should_confirm(true, &magnet, false));
        assert!(!handoff_should_confirm(true, &magnet, true));
    }
}
