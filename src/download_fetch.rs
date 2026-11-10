//! HTTP fetch engine: attempts, range probing, piece fetching, resume bitmaps.
//! The download manager spawns `run_download`; tests drive the pieces directly.

use crate::content_range::{ParsedContentRange, parse_content_range};
use crate::download_net::{DEFAULT_USER_AGENT, DownloadOptions};
use crate::download_pieces::{SegmentState, plan_pieces};
use crate::download_rate::{live_rate_limit, pace_chunk, parse_rate, progress_msg};
use crate::engine_msg::{DEST_EXISTS, EngineMsg};
use crate::file_names::{PIECE_MIN, percent_decode, piece_len, sane_filename};
use crate::runtime::lock_recover;
use futures_util::StreamExt as _;
use gettextrs::gettext;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Jittered exponential backoff for retryable failures: `capped_ms = min(60_000, 500 * 2^(failures-1))`,
/// full jitter via GLib's RNG (no `rand` dependency). `failures` is 1-based.
/// Worst case per episode ~1.5s (500ms + 1s across the two retries `tries` allows).
pub(crate) fn backoff_delay(failures: u32) -> Duration {
    // Cap the exponent before shifting: 500ms * 2^7 = 64s > 60s ceiling,
    // so higher exponents are indistinguishable after the min().
    let exponent = failures.saturating_sub(1).min(7);
    let capped_ms = std::cmp::min(60_000u64, 500u64 << exponent);
    let sleep_ms = glib::random_int_range(0, capped_ms as i32 + 1) as u64;
    Duration::from_millis(sleep_ms)
}

/// Forward a parseable Last-Modified header to the pump (at most one per HTTP response).
fn send_last_modified(
    tx: &tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    resp: &reqwest::Response,
) {
    if let Some(v) = resp.headers().get(reqwest::header::LAST_MODIFIED)
        && let Ok(s) = v.to_str()
        && let Ok(t) = httpdate::parse_http_date(s)
    {
        tx.send(EngineMsg::LastModified(t)).ok();
    }
}

/// Report the response's resume validator (ETag preferred, Last-Modified
/// fallback) so the item stores it for If-Range on resume. Sent when the
/// response carries a new validator generation — not on 206 resume hits
/// where the validator is already known (a mid-download change must not
/// poison the stored value). The handler overwrites any stale validator.
fn send_validator(tx: &tokio::sync::mpsc::UnboundedSender<EngineMsg>, resp: &reqwest::Response) {
    let etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let last_modified = resp
        .headers()
        .get(reqwest::header::LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    // Only report if at least one validator is present; absent headers
    // leave any stored validator alone (don't clear on a bare response).
    if etag.is_some() || last_modified.is_some() {
        tx.send(EngineMsg::Validator(Box::new(
            crate::engine_msg::ValidatorInfo {
                etag,
                last_modified,
            },
        )))
        .ok();
    }
}

/// First recorded worker error, or a generic interruption message (callers keep their own `AttemptFail` variant).
fn take_first_err(first_err: &Mutex<Option<String>>) -> String {
    lock_recover(first_err)
        .take()
        .unwrap_or_else(|| gettext("Download interrupted"))
}

/// Shared inputs for one download's engine task.
pub(crate) struct FetchCtx {
    pub(crate) client: reqwest::Client,
    pub(crate) url: String,
    pub(crate) dest: std::path::PathBuf,
    pub(crate) opts: DownloadOptions,
    /// Attempt-scoped browser cookie jar, resolved once per attempt.
    pub(crate) cookies: Option<std::sync::Arc<reqwest::cookie::Jar>>,
    /// Browser UA from the extension handoff (cf_clearance is UA-bound).
    /// Falls back to DEFAULT_USER_AGENT when no handoff.
    pub(crate) user_agent: String,
    pub(crate) timeout: Duration,
    pub(crate) tx: tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    /// Session validator for If-Range: ETag preferred, Last-Modified fallback.
    /// Set from the stored validator (resume) or the first response (fresh).
    /// All requests in the session use it; OnceLock ensures first-wins.
    pub(crate) if_range: std::sync::Arc<std::sync::OnceLock<String>>,
}

pub(crate) async fn run_download(mut ctx: FetchCtx, connections: usize, mode: StartMode) {
    let timeout = Duration::from_secs(30);
    let mut tries = 3;
    // Resolve the staged jar and its UA together, so a stale/missing/unreadable
    // jar can never leave its UA paired with cookies exported from the live browser.
    if let Some((path, handoff_ua)) = crate::extension_server::live_handoff_identity(&ctx.url)
        && let Ok(text) = tokio::fs::read_to_string(&path).await
    {
        let (jar, count) = crate::cookies::jar_from_export(&text);
        if count > 0 {
            ctx.cookies = Some(jar);
            if let Some(user_agent) = handoff_ua {
                ctx.user_agent = user_agent;
            }
        }
    }
    // One export per attempt (jars are cheap next to downloads).
    if ctx.cookies.is_none()
        && !ctx.opts.cookies_browser.is_empty()
        && ctx.opts.cookies_browser != "none"
    {
        let youtube_bin = crate::video::resolve_libraries()
            .map(|libs| libs.youtube)
            .ok();
        if let Some(bin) = youtube_bin {
            let browser = ctx.opts.cookies_browser.clone();
            let page_url = ctx.url.clone();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
            match tokio::time::timeout_at(deadline, async {
                crate::video_tools::ensure_deno(&page_url).await?;
                Ok::<_, crate::video_tools::VideoError>(
                    crate::cookies::jar_for_browser(&browser, &bin, &page_url).await,
                )
            })
            .await
            {
                Ok(Ok(jar)) => ctx.cookies = jar,
                Ok(Err(error)) => {
                    tracing::warn!(error = %error, "browser-cookie export skipped after runtime setup failure");
                }
                Err(_) => {
                    tracing::warn!(
                        "browser-cookie export exceeded its 120-second end-to-end deadline"
                    );
                }
            }
        }
    }
    // With a jar, swap in a provider-backed client: reqwest re-applies
    // in-scope cookies on every redirect hop, while a stamped Cookie header
    // would be stripped on a cross-host redirect. The provider is read-only;
    // the shared jar stays a browser snapshot.
    if let Some(jar) = ctx.cookies.clone() {
        let proxy = match ctx.opts.proxy_config() {
            Ok(p) => p,
            Err(e) => {
                ctx.tx.send(EngineMsg::Failed(e)).ok();
                return;
            }
        };
        match crate::download_net::http_client_with_jar(proxy.as_ref(), jar) {
            Ok(client) => ctx.client = client,
            Err(e) => {
                ctx.tx.send(EngineMsg::Failed(e)).ok();
                return;
            }
        }
    }
    match mode {
        StartMode::Single => {
            let mut expected_identity = match destination_identity(&ctx.dest).await {
                Ok(identity) => identity,
                Err(error) => {
                    ctx.tx.send(EngineMsg::Failed(error)).ok();
                    return;
                }
            };
            single_loop(&ctx, &mut tries, None, &mut expected_identity).await;
        }
        StartMode::Fresh => {
            let mut expected_identity = None;
            // Single connection skips probing (the probe consumes single-use token URLs; one stream needs no total up front).
            if connections <= 1 {
                single_loop(&ctx, &mut tries, None, &mut expected_identity).await;
                return;
            }
            match probe_ranges(&ctx.client, &ctx.url, timeout).await {
                Ok((total, validator)) => {
                    // The probe captured the session validator before any worker started.
                    // Persist it for cross-session resume, and use it for If-Range.
                    // Don't overwrite a resume validator: the stored one is for the
                    // existing partial file, the probe is for a fresh download.
                    if ctx.if_range.get().is_none() {
                        // Send Validator for persistence: the item's etag/last_modified
                        // must be populated, or a pause/restart loses the validator.
                        ctx.tx
                            .send(EngineMsg::Validator(Box::new(validator.clone())))
                            .ok();
                        // Derive If-Range from the same value.
                        let if_range_val = validator
                            .etag
                            .as_deref()
                            .filter(|e| !e.starts_with("W/"))
                            .map(|s| s.to_string())
                            .or_else(|| validator.last_modified.clone());
                        if let Some(v) = if_range_val {
                            let _ = ctx.if_range.set(v);
                        }
                    }
                    if plan_pieces(total, connections).is_empty() {
                        single_loop(&ctx, &mut tries, Some(total), &mut expected_identity).await;
                    } else {
                        let (ack_tx, mut ack_rx) = tokio::sync::mpsc::channel::<()>(1);
                        if ctx
                            .tx
                            .send(EngineMsg::SegmentsInit { total, ack: ack_tx })
                            .is_ok()
                        {
                            // Bounded: a consumer that never acks (tests, torn-down row) must not hang the engine.
                            let _ =
                                tokio::time::timeout(Duration::from_secs(2), ack_rx.recv()).await;
                        }
                        if multi_loop(
                            &ctx,
                            total,
                            None,
                            connections,
                            &mut tries,
                            &mut expected_identity,
                        )
                        .await
                        {
                            let mut single_tries = 3;
                            single_loop(
                                &ctx,
                                &mut single_tries,
                                Some(total),
                                &mut expected_identity,
                            )
                            .await;
                        }
                    }
                }
                Err(_) => {
                    single_loop(&ctx, &mut tries, None, &mut expected_identity).await;
                }
            }
        }
        StartMode::Resume(st) => {
            let total = st.total;
            let mut expected_identity = match destination_identity(&ctx.dest).await {
                Ok(Some(identity)) => Some(identity),
                Ok(None) => {
                    ctx.tx
                        .send(EngineMsg::Failed(gettext("The partial download file is missing")))
                        .ok();
                    return;
                }
                Err(error) => {
                    ctx.tx.send(EngineMsg::Failed(error)).ok();
                    return;
                }
            };
            if multi_loop(
                &ctx,
                total,
                Some(st),
                connections,
                &mut tries,
                &mut expected_identity,
            )
            .await
            {
                let mut single_tries = 3;
                single_loop(
                    &ctx,
                    &mut single_tries,
                    Some(total),
                    &mut expected_identity,
                )
                .await;
            }
        }
    }
}

/// Single-stream fallback with retries. The identity is set only after a
/// fresh destination is exclusively created, or captured before resuming an
/// existing partial file. A retry cannot adopt a path that appeared meanwhile.
async fn single_loop(
    ctx: &FetchCtx,
    tries: &mut i32,
    expected: Option<u64>,
    expected_identity: &mut Option<FileIdentity>,
) {
    let mut failures: u32 = 0;
    loop {
        match attempt_once(ctx, expected, expected_identity).await {
            Ok(()) => {
                let size = tokio::fs::metadata(&ctx.dest)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0);
                crate::extension_server::discard_handoff_cookies_async(&ctx.url);
                ctx.tx.send(EngineMsg::Finished { size }).ok();
                return;
            }
            Err(e) => {
                if e == DEST_EXISTS {
                    ctx.tx.send(EngineMsg::Failed(e)).ok();
                    return;
                }
                *tries -= 1;
                if *tries <= 0 {
                    ctx.tx.send(EngineMsg::Failed(e)).ok();
                    return;
                }
                failures += 1;
                tokio::time::sleep(backoff_delay(failures)).await;
            }
        }
    }
}

/// Retry loop around segmented attempts (each retry refetches only still-missing pieces).
/// Returns true when the server throttled parallel connections (caller continues single-stream).
/// The caller retains the identity so a single-stream fallback keeps the same ownership proof.
async fn multi_loop(
    ctx: &FetchCtx,
    total: u64,
    saved: Option<SegmentState>,
    max_workers: usize,
    tries: &mut i32,
    expected_identity: &mut Option<FileIdentity>,
) -> bool {
    let mut st = saved.unwrap_or_else(|| SegmentState::new(total));
    let mut failures: u32 = 0;
    loop {
        let result = attempt_multi(
            ctx,
            total,
            &mut st,
            max_workers,
            expected_identity,
        )
        .await;

        match result {
            Ok(()) => {
                let size = tokio::fs::metadata(&ctx.dest)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0);
                crate::extension_server::discard_handoff_cookies_async(&ctx.url);
                ctx.tx.send(EngineMsg::Finished { size }).ok();
                return false;
            }
            Err(AttemptFail::Throttled(_)) => {
                let (ack_tx, mut ack_rx) = tokio::sync::mpsc::channel::<()>(1);
                ctx.tx.send(EngineMsg::FallbackSingle { ack: ack_tx }).ok();
                // Wait for the UI thread to truncate + drop the bitmap before single-stream resumes.
                let _ = ack_rx.recv().await;
                return true;
            }
            Err(AttemptFail::Changed(e)) => {
                // The probed object is gone: retries can never succeed, so fail terminally and let a later retry start fresh.
                ctx.tx.send(EngineMsg::FailedVersion(e)).ok();
                return false;
            }
            Err(AttemptFail::Retryable(e)) => {
                // A foreign destination is terminal, not something to retry
                // by reopening or modifying the path.
                if e == DEST_EXISTS {
                    ctx.tx.send(EngineMsg::Failed(e)).ok();
                    return false;
                }

                *tries -= 1;
                if *tries <= 0 {
                    ctx.tx.send(EngineMsg::TruncatePrefix).ok();
                    ctx.tx.send(EngineMsg::Failed(e)).ok();
                    return false;
                }
                // Jittered backoff on retryable failures (Throttled has its own
                // UI-ack handshake, Changed/DEST_EXISTS are terminal).
                failures += 1;
                tokio::time::sleep(backoff_delay(failures)).await;
            }
        }
    }
}

/// One segmented attempt: N workers pull pieces off a shared queue while one writer sequences them to disk.
async fn attempt_multi(
    ctx: &FetchCtx,
    total: u64,
    st: &mut SegmentState,
    max_workers: usize,
    expected_identity: &mut Option<FileIdentity>,
) -> Result<(), AttemptFail> {
    if st.total != total {
        return Err(AttemptFail::Retryable(gettext("File changed on server")));
    }
    let missing: Vec<(u64, u64, u64)> = st.missing();
    if missing.is_empty() {
        return Ok(());
    }
    let expect_bytes: u64 = missing.iter().map(|(_, s, e)| e - s + 1).sum();
    // Size without truncating: completed pieces are already on disk.
    let output_file = ensure_sized(&ctx.dest, total, expected_identity).await?;
    let queue: Arc<Mutex<VecDeque<(u64, u64, u64)>>> =
        Arc::new(Mutex::new(missing.into_iter().collect()));
    let failed = Arc::new(AtomicBool::new(false));
    let first_err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let throttled = Arc::new(AtomicBool::new(false));
    let changed = Arc::new(AtomicBool::new(false));
    // Bound in-flight BYTES, not pieces (big pieces would pin hundreds of MB on a slow disk).
    let depth = ((8 * PIECE_MIN) / piece_len(total)).clamp(2, 8) as usize;
    let (wtx, wrx) = tokio::sync::mpsc::channel::<(u64, Vec<u8>, u64)>(depth);
    // Never exceed the configured connections: extra range requests are what throttled hosts punish.
    let n_workers = lock_recover(&queue)
        .len()
        .min(max_workers.max(1))
        .clamp(1, 16);
    let mut workers = Vec::with_capacity(n_workers);
    for _ in 0..n_workers {
        let (ctx, wtx, queue, failed, first_err, throttled, changed) = (
            ctx,
            wtx.clone(),
            Arc::clone(&queue),
            Arc::clone(&failed),
            Arc::clone(&first_err),
            Arc::clone(&throttled),
            Arc::clone(&changed),
        );
        workers.push(async move {
            loop {
                if failed.load(Ordering::SeqCst) {
                    break;
                }
                let piece = lock_recover(&queue).pop_front();
                let Some((idx, s, e)) = piece else { break };
                match fetch_piece(ctx, s, e, total).await {
                    Ok(bytes) => {
                        if wtx.send((s, bytes, idx)).await.is_err() {
                            break; // Writer gone; attempt is over.
                        }
                    }
                    Err(AttemptFail::Throttled(msg)) => {
                        lock_recover(&first_err).get_or_insert(msg);
                        throttled.store(true, Ordering::SeqCst);
                        failed.store(true, Ordering::SeqCst);
                        break;
                    }
                    Err(AttemptFail::Changed(msg)) => {
                        lock_recover(&first_err).get_or_insert(msg);
                        changed.store(true, Ordering::SeqCst);
                        failed.store(true, Ordering::SeqCst);
                        break;
                    }
                    Err(AttemptFail::Retryable(msg)) => {
                        lock_recover(&first_err).get_or_insert(msg);
                        failed.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }
        });
    }
    drop(wtx);
    let writer = {
        let mut wrx = wrx;
        let dest = ctx.dest.clone();
        let st = &mut *st;
        async move {
            let mut file = output_file;
            let mut downloaded = st.completed_bytes();
            ctx.tx.send(progress_msg(downloaded, Some(total))).ok();
            let pace_start = Instant::now();
            let mut paced: u64 = 0;
            let mut last_sent = Instant::now();
            let mut written: u64 = 0;
            let mut rate = live_rate_limit();
            use tokio::io::{AsyncSeekExt as _, AsyncWriteExt as _};
            while let Some((offset, bytes, idx)) = wrx.recv().await {
                file.seek(std::io::SeekFrom::Start(offset))
                    .await
                    .map_err(|e| format!("Cannot write file: {e}"))?;
                file.write_all(&bytes)
                    .await
                    .map_err(|e| format!("Cannot write file: {e}"))?;
                downloaded += bytes.len() as u64;
                written += bytes.len() as u64;
                ctx.tx.send(EngineMsg::PieceDone(idx)).ok();
                st.mark(idx);
                pace_chunk(&mut paced, pace_start, rate, bytes.len()).await;
                // ~20fps row updates.
                if last_sent.elapsed() >= Duration::from_millis(50) {
                    rate = live_rate_limit();
                    ctx.tx.send(progress_msg(downloaded, Some(total))).ok();
                    last_sent = Instant::now();
                }
            }
            file.flush()
                .await
                .map_err(|e| format!("Cannot write file: {e}"))?;
            // Verify the path still references the file being written: a
            // swap during transfer must not go unnoticed.
            let fd_metadata = file
                .metadata()
                .await
                .map_err(|e| format!("Cannot inspect downloaded file: {e}"))?;
            let path_metadata = tokio::fs::symlink_metadata(&dest)
                .await
                .map_err(|e| format!("Cannot verify download destination: {e}"))?;
            if path_metadata.file_type().is_symlink()
                || file_identity(&fd_metadata) != file_identity(&path_metadata)
            {
                return Err("Destination file was replaced during download".to_string());
            }
            ctx.tx.send(progress_msg(downloaded, Some(total))).ok();
            Ok::<u64, String>(written)
        }
    };
    let (wres, _) = tokio::join!(writer, futures_util::future::join_all(workers));
    let written = wres.map_err(AttemptFail::Retryable)?;
    if changed.load(Ordering::SeqCst) {
        return Err(AttemptFail::Changed(take_first_err(&first_err)));
    }
    if throttled.load(Ordering::SeqCst) {
        return Err(AttemptFail::Throttled(take_first_err(&first_err)));
    }
    if failed.load(Ordering::SeqCst) {
        return Err(AttemptFail::Retryable(take_first_err(&first_err)));
    }
    if written != expect_bytes {
        return Err(AttemptFail::Retryable(gettext("Incomplete download")));
    }
    Ok(())
}

/// True when the file has unallocated (sparse) regions: a later append-at-EOF resume must not inherit holes.
pub(crate) async fn has_holes(path: &std::path::Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .map(|metadata| {
            use std::os::unix::fs::MetadataExt;
            metadata.blocks().saturating_mul(512) < metadata.len()
        })
        .unwrap_or(false)
}

/// Current local length, or `None` when it cannot be read: an offset from an earlier stat is then untrustworthy.
async fn local_len(path: &std::path::Path) -> Option<u64> {
    tokio::fs::metadata(path)
        .await
        .map(|metadata| metadata.len())
        .ok()
}

async fn attempt_once(
    ctx: &FetchCtx,
    expected_total: Option<u64>,
    expected_identity: &mut Option<FileIdentity>,
) -> Result<(), String> {
    // At most one restart: a 416 may only trigger a single reset-and-retry.
    let mut restarted = false;
    loop {
        let start = match tokio::fs::symlink_metadata(&ctx.dest).await {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(DEST_EXISTS.to_string());
            }
            Ok(metadata) => {
                let actual_identity = file_identity(&metadata);
                match *expected_identity {
                    Some(expected) if expected == actual_identity => metadata.len(),
                    Some(_) => {
                        return Err("Destination file was replaced during download".to_string());
                    }
                    None => return Err(DEST_EXISTS.to_string()),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if expected_identity.is_some() {
                    return Err("Destination file disappeared during download".to_string());
                }
                0
            }
            // A stat failure is not an empty file: starting over would truncate bytes we may not own.
            Err(error) => return Err(format!("Cannot inspect file: {error}")),
        };

        let mut req = stamp_request(ctx.client.get(&ctx.url), &ctx.user_agent, &ctx.url);
        if start > 0 {
            req = req.header("Range", format!("bytes={start}-"));
            // If-Range: the server returns 206 only if the object still
            // matches our validator; a 200 means it changed -> restart fresh
            // instead of splicing a different file onto our bytes.
            if let Some(v) = ctx.if_range.get() {
                req = req.header("If-Range", v);
            }
        }
        let resp = execute_with_timeout(&ctx.client, req, ctx.timeout).await?;
        let status = resp.status();
        if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            let claimed = resp
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .and_then(parse_content_range)
                .and_then(|range| match range {
                    ParsedContentRange::Unsatisfied { complete_length } => Some(complete_length),
                    ParsedContentRange::Satisfied { .. } => None,
                });
            let still_owned = match *expected_identity {
                Some(expected) => verify_destination_identity(&ctx.dest, expected).await.is_ok(),
                None => false,
            };
            // "Already complete" only with proof: matching length, unchanged inode,
            // unchanged local size, and fully allocated bytes.
            if start > 0
                && still_owned
                && claimed == Some(start)
                && local_len(&ctx.dest).await == Some(start)
                && !has_holes(&ctx.dest).await
            {
                return Ok(());
            }
            if claimed.is_none() && start > 0 && !restarted {
                // Bare 416 with no usable Content-Range: confirm the length before resetting.
                let hreq = stamp_request(ctx.client.head(&ctx.url), &ctx.user_agent, &ctx.url);
                if let Ok(hresp) = execute_with_timeout(&ctx.client, hreq, ctx.timeout).await
                    && hresp.status().is_success()
                    && hresp.content_length() == Some(start)
                    && verify_destination_identity(
                        &ctx.dest,
                        expected_identity.ok_or_else(|| DEST_EXISTS.to_string())?,
                    )
                    .await
                    .is_ok()
                    && local_len(&ctx.dest).await == Some(start)
                    && !has_holes(&ctx.dest).await
                {
                    return Ok(());
                }
            }
            if restarted {
                return Err(gettext("Server rejects range requests"));
            }

            // Reset through the already-owned file descriptor. Never unlink a
            // pathname after an async/network wait: it may now name someone else's file.
            if let Some(expected) = *expected_identity {
                let file = open_owned_destination(&ctx.dest, expected).await?;
                file.set_len(0)
                    .await
                    .map_err(|error| format!("Cannot reset partial download: {error}"))?;
            } else if tokio::fs::symlink_metadata(&ctx.dest).await.is_ok() {
                return Err(DEST_EXISTS.to_string());
            }
            restarted = true;
            continue;
        }
        if !status.is_success() {
            return Err(format!(
                "HTTP {}: {}",
                status.as_u16(),
                status.canonical_reason().unwrap_or("error")
            ));
        }
        send_last_modified(&ctx.tx, &resp);
        let if_range_matched =
            ctx.if_range.get().is_some() && status == reqwest::StatusCode::PARTIAL_CONTENT;
        if !if_range_matched {
            send_validator(&ctx.tx, &resp);
        }
        if ctx.if_range.get().is_none() {
            response_validator(&resp).map(|v| ctx.if_range.set(v));
        }

        let partial = start > 0 && status == reqwest::StatusCode::PARTIAL_CONTENT;
        if partial {
            let cr = resp
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok());
            if !accepts_content_range(cr, start, expected_total) {
                return Err(gettext("Server returned an unexpected range"));
            }
        }
        let total = response_total(
            resp.content_length(),
            resp.headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok()),
            partial,
            start,
        );
        if rejects_unexpected_restart(partial, start, expected_total, resp.content_length()) {
            return Err(gettext("Server returned an unexpected file size"));
        }
        if !partial
            && start > 0
            && let Some(length) = resp.content_length()
            && length < start
        {
            return Err(gettext("Server restarted the download with a smaller file"));
        }

        let mut file = if partial {
            let expected = expected_identity.ok_or_else(|| DEST_EXISTS.to_string())?;
            open_owned_partial(&ctx.dest, start, expected).await?
        } else if let Some(expected) = *expected_identity {
            let file = open_owned_destination(&ctx.dest, expected).await?;
            file.set_len(0)
                .await
                .map_err(|error| format!("Cannot reset partial download: {error}"))?;
            file
        } else {
            let file = match open_nofollow(
                |options| {
                    options.write(true).create_new(true);
                },
                &ctx.dest,
            )
            .await
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Err(DEST_EXISTS.to_string());
                }
                Err(error) => return Err(format!("Cannot write file: {error}")),
            };
            let metadata = file
                .metadata()
                .await
                .map_err(|error| format!("Cannot inspect downloaded file: {error}"))?;
            let identity = file_identity(&metadata);
            *expected_identity = Some(identity);
            verify_destination_identity(&ctx.dest, identity).await?;
            file
        };

        if !partial
            && let Some(name) = resp
                .headers()
                .get(reqwest::header::CONTENT_DISPOSITION)
                .and_then(|v| v.to_str().ok())
                .and_then(filename_from_content_disposition)
        {
            ctx.tx.send(EngineMsg::SuggestName(name)).ok();
        }
        let mut downloaded = if partial { start } else { 0 };
        ctx.tx.send(progress_msg(downloaded, total)).ok();
        let mut stream = resp.bytes_stream();
        let pace_start = Instant::now();
        let mut paced: u64 = 0;
        let mut last_sent = Instant::now();
        let mut rate = live_rate_limit();
        let mut empty_streak: u32 = 0;
        use tokio::io::AsyncWriteExt as _;
        loop {
            let chunk = match tokio::time::timeout(ctx.timeout, stream.next()).await {
                Ok(Some(Ok(chunk))) => chunk,
                Ok(Some(Err(error))) => return Err(format!("Download interrupted: {error}")),
                Ok(None) => break,
                Err(_) => return Err(gettext("Stalled connection timed out")),
            };
            if chunk.is_empty() {
                empty_streak += 1;
                if empty_streak > 32 {
                    return Err(gettext("Stalled connection timed out"));
                }
                continue;
            }
            empty_streak = 0;
            file.write_all(&chunk)
                .await
                .map_err(|error| format!("Cannot write file: {error}"))?;
            downloaded += chunk.len() as u64;
            pace_chunk(&mut paced, pace_start, rate, chunk.len()).await;
            if last_sent.elapsed() >= Duration::from_millis(50) {
                rate = live_rate_limit();
                ctx.tx.send(progress_msg(downloaded, total)).ok();
                last_sent = Instant::now();
            }
        }
        file.flush()
            .await
            .map_err(|error| format!("Cannot write file: {error}"))?;
        return match total {
            Some(total) if downloaded != total => Err(gettext("Incomplete download")),
            _ => Ok(()),
        };
    }
}
/// Re-apply both torrent speed caps from settings (editing one key must never clear the other).
pub(crate) fn apply_torrent_limits(s: &crate::settings::AppSettings) {
    crate::torrent::apply_live_limits(
        parse_rate(s.speed_limit().trim()),
        parse_rate(s.torrent_upload_limit().trim()),
    );
}

/// Shrink `path` to the longest completed piece prefix (a single-stream resume appends at EOF, so holes must go). Only shrinks.
pub(crate) fn truncate_to_prefix(path: &std::path::Path, st: &SegmentState) {
    let prefix = st.prefix_len();
    if let Ok(md) = std::fs::metadata(path)
        && md.len() > prefix
        && let Ok(f) = std_open_nofollow(
            |o| {
                o.write(true);
            },
            path,
        )
    {
        let _ = f.set_len(prefix);
    }
}

/// Open for writing without truncating (truncating here would corrupt resumed pieces the bitmap claims as done).
/// Open a file refusing to follow symlinks: a link swapped in between attempts
/// must not divert a resume-append or retry-truncate onto an arbitrary file.
/// Returns ELOOP if the final component is a symlink.
fn std_open_nofollow(
    configure: impl FnOnce(&mut std::fs::OpenOptions),
    path: &std::path::Path,
) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut opts = std::fs::OpenOptions::new();
    configure(&mut opts);
    opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
}

async fn open_nofollow(
    configure: impl FnOnce(&mut std::fs::OpenOptions) + Send + 'static,
    path: &std::path::Path,
) -> std::io::Result<tokio::fs::File> {
    // Keep O_NOFOLLOW while moving the synchronous open off the Tokio worker.
    let path = path.to_path_buf();
    let file = crate::runtime::tokio_rt()
        .spawn_blocking(move || std_open_nofollow(configure, &path))
        .await
        .map_err(std::io::Error::other)??;
    Ok(tokio::fs::File::from_std(file))
}

/// Open a partial download for append only if it still holds exactly the `start` bytes the Range offset was computed from.
pub(crate) async fn open_partial(
    path: &std::path::Path,
    start: u64,
) -> Result<tokio::fs::File, String> {
    let file = open_nofollow(
        |o| {
            o.append(true);
        },
        path,
    )
    .await
    .map_err(|e| format!("Cannot write file: {e}"))?;
    // The stat happened before the request: a local append/truncate in that window would shift every byte.
    let at = file
        .metadata()
        .await
        .map_err(|e| format!("Cannot write file: {e}"))?
        .len();
    if at != start {
        return Err(gettext("The partial file changed during resume"));
    }
    Ok(file)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
}

fn file_identity(metadata: &std::fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt as _;

    FileIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    }
}

/// Capture an existing regular destination before a resume request. A fresh
/// download receives `None` and must create its first file with create_new.
async fn destination_identity(
    path: &std::path::Path,
) -> Result<Option<FileIdentity>, String> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(DEST_EXISTS.to_string())
        }
        Ok(metadata) => Ok(Some(file_identity(&metadata))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("Cannot inspect file: {error}")),
    }
}

async fn verify_destination_identity(
    path: &std::path::Path,
    expected: FileIdentity,
) -> Result<(), String> {
    match destination_identity(path).await? {
        Some(actual) if actual == expected => Ok(()),
        Some(_) => Err("Destination file was replaced during download".to_string()),
        None => Err("Destination file disappeared during download".to_string()),
    }
}

/// Open an already-owned destination without truncating it, then verify both
/// the descriptor and the current path before allowing writes.
async fn open_owned_destination(
    path: &std::path::Path,
    expected: FileIdentity,
) -> Result<tokio::fs::File, String> {
    let file = open_nofollow(|options| { options.write(true); }, path)
        .await
        .map_err(|error| format!("Cannot write file: {error}"))?;
    let metadata = file
        .metadata()
        .await
        .map_err(|error| format!("Cannot inspect downloaded file: {error}"))?;
    if !metadata.is_file() || file_identity(&metadata) != expected {
        return Err("Destination file was replaced during download".to_string());
    }
    verify_destination_identity(path, expected).await?;
    Ok(file)
}

/// Append to a resumed destination only after the opened inode matches its
/// pre-request identity and the size still matches the requested range offset.
async fn open_owned_partial(
    path: &std::path::Path,
    start: u64,
    expected: FileIdentity,
) -> Result<tokio::fs::File, String> {
    let file = open_partial(path, start).await?;
    let metadata = file
        .metadata()
        .await
        .map_err(|error| format!("Cannot inspect partial download: {error}"))?;
    if !metadata.is_file() || file_identity(&metadata) != expected {
        return Err("Destination file was replaced during download".to_string());
    }
    verify_destination_identity(path, expected).await?;
    Ok(file)
}

async fn ensure_sized(
    dest: &std::path::Path,
    total: u64,
    expected_identity: &mut Option<FileIdentity>,
) -> Result<tokio::fs::File, AttemptFail> {
    use std::io::ErrorKind::{AlreadyExists, FileTooLarge, StorageFull};

    // Unclaimed destinations must be atomically created. Once an inode has
    // been recorded, retries may reopen only that same inode without truncating
    // it during the open itself.
    let file_result = if expected_identity.is_some() {
        open_nofollow(|options| { options.write(true); }, dest).await
    } else {
        open_nofollow(
            |options| { options.write(true).create_new(true); },
            dest,
        )
        .await
    };
    let file = file_result.map_err(|error| {
        if expected_identity.is_none() && error.kind() == AlreadyExists {
            AttemptFail::Retryable(DEST_EXISTS.to_string())
        } else {
            AttemptFail::Retryable(format!("Cannot write file: {error}"))
        }
    })?;

    let metadata = file
        .metadata()
        .await
        .map_err(|error| AttemptFail::Retryable(format!("Cannot inspect file: {error}")))?;
    if !metadata.is_file() {
        return Err(AttemptFail::Retryable(DEST_EXISTS.to_string()));
    }
    let actual_identity = file_identity(&metadata);
    match *expected_identity {
        Some(expected) if expected != actual_identity => {
            return Err(AttemptFail::Retryable(
                "Destination file was replaced during download".to_string(),
            ));
        }
        None => *expected_identity = Some(actual_identity),
        _ => {}
    }
    verify_destination_identity(dest, actual_identity)
        .await
        .map_err(AttemptFail::Retryable)?;

    if metadata.len() != total
        && let Err(error) = file.set_len(total).await
    {
        let message = format!("Cannot write file: {error}");
        return Err(if matches!(error.kind(), StorageFull | FileTooLarge) {
            AttemptFail::Throttled(message)
        } else {
            AttemptFail::Retryable(message)
        });
    }

    Ok(file)
}

/// How the engine task should start this download.
#[derive(Debug)]
pub(crate) enum StartMode {
    /// Classic single stream (small/unknown size, no range support, or contiguous partial).
    Single,
    /// Fresh file: probe range support, then go multi or single.
    Fresh,
    /// Segmented resume from the in-memory bitmap.
    Resume(SegmentState),
}

/// Origin (scheme + host + port, no path/query) for a Referer header.
/// Browsers send `strict-origin-when-cross-origin` by default; Grab matches
/// it. An origin-only Referer satisfies hotlink guards that check the origin
/// without leaking the page path or query (which may carry tokens).
fn origin_referer(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    let host = u.host_str()?;
    let port = match (u.scheme(), u.port()) {
        ("http", Some(80)) | ("http", None) => String::new(),
        ("https", Some(443)) | ("https", None) => String::new(),
        (_, Some(p)) => format!(":{p}"),
        (_, None) => String::new(),
    };
    Some(format!("{}://{host}{port}/", u.scheme()))
}

/// Stamp one outbound request like a browser (User-Agent + origin-only
/// Referer); single choke point for all requests. No Cookie: cookie-bearing
/// downloads use a client with the jar as provider (see `http_client_with_jar`),
/// which re-applies in-scope cookies on every hop.
pub(crate) fn stamp_request(
    mut req: reqwest::RequestBuilder,
    user_agent: &str,
    url: &str,
) -> reqwest::RequestBuilder {
    if !user_agent.trim().is_empty() {
        req = req.header("User-Agent", user_agent.trim());
    }
    if let Some(referer) = origin_referer(url) {
        req = req.header("Referer", referer);
    }
    req
}

/// Execute one request under a timeout (one consistent stall string; build/transport errors keep their own text).
async fn execute_with_timeout(
    client: &reqwest::Client,
    req: reqwest::RequestBuilder,
    timeout: Duration,
) -> Result<reqwest::Response, String> {
    let built = req.build().map_err(|e| e.to_string())?;
    match tokio::time::timeout(timeout, client.execute(built)).await {
        Ok(Ok(r)) => Ok(r),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(gettext("Connection timed out")),
    }
}

/// True only when the response carries the same kind of validator as the
/// session one and it differs. The kind follows the value's shape: an
/// IMF-fixdate (always ends in "GMT") is a Last-Modified, anything else is an
/// ETag, quoted or not (some servers send them bare). Comparing across kinds
/// or against a weak ETag would report a false change and delete a good
/// partial; a response without the matching kind never reads as a change.
pub(crate) fn validator_changed(
    session: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> bool {
    let candidate = if session.ends_with("GMT") {
        last_modified
    } else {
        etag.filter(|e| !e.starts_with("W/"))
    };
    candidate.is_some_and(|c| c != session)
}

/// Extract the validator from a response: strong ETag preferred, Last-Modified
/// fallback. Weak ETags are skipped (never valid for If-Range).
/// Returns the If-Range value to send.
fn response_validator(resp: &reqwest::Response) -> Option<String> {
    response_validators(resp).0
}

/// Extract both validators from a response for persistence.
/// Returns (if_range_value, etag, last_modified).
fn response_validators(
    resp: &reqwest::Response,
) -> (Option<String>, Option<String>, Option<String>) {
    let etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let last_modified = resp
        .headers()
        .get(reqwest::header::LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    // If-Range value: strong ETag preferred, Last-Modified fallback.
    let if_range = etag
        .as_deref()
        .filter(|e| !e.starts_with("W/"))
        .map(|s| s.to_string())
        .or_else(|| last_modified.clone());
    (if_range, etag, last_modified)
}

/// One `Range: bytes=0-0` round trip: proves range support AND yields the total. Any error falls back to single-stream.
/// Returns the total and the validators for persistence.
async fn probe_ranges(
    client: &reqwest::Client,
    url: &str,
    timeout: Duration,
) -> Result<(u64, crate::engine_msg::ValidatorInfo), String> {
    let req = stamp_request(
        client.get(url).header("Range", "bytes=0-0"),
        DEFAULT_USER_AGENT,
        url,
    );
    let resp = execute_with_timeout(client, req, timeout).await?;
    if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(gettext("Range requests not supported"));
    }
    let value = resp
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok());
    let total = value
        .and_then(parse_content_range)
        .and_then(|range| match range {
            ParsedContentRange::Satisfied {
                first_byte: 0,
                complete_length: Some(total),
                ..
            } => Some(total),
            ParsedContentRange::Satisfied { .. } | ParsedContentRange::Unsatisfied { .. } => None,
        })
        .ok_or_else(|| gettext("Bad Content-Range"))?;
    // Capture the validators from the probe: this becomes the session validator
    // for all piece workers, and is persisted for cross-session resume.
    let (_, etag, last_modified) = response_validators(&resp);
    let validator = crate::engine_msg::ValidatorInfo {
        etag,
        last_modified,
    };
    Ok((total, validator))
}

/// Total size for a response (`None` only when neither header says; then EOF is the only signal).
pub(crate) fn response_total(
    content_length: Option<u64>,
    content_range: Option<&str>,
    partial: bool,
    start: u64,
) -> Option<u64> {
    content_length
        .and_then(|t| {
            if partial {
                t.checked_add(start)
            } else {
                Some(t)
            }
        })
        .or_else(|| {
            if !partial {
                return None;
            }
            match content_range.and_then(parse_content_range) {
                Some(ParsedContentRange::Satisfied {
                    first_byte,
                    complete_length,
                    ..
                }) if first_byte == start => complete_length,
                Some(ParsedContentRange::Satisfied { .. })
                | Some(ParsedContentRange::Unsatisfied { .. })
                | None => None,
            }
        })
}

/// A resumed range answered with a full 200 must still be the same object (a disagreeing length means login wall, not our file).
pub(crate) fn rejects_unexpected_restart(
    partial: bool,
    start: u64,
    expected: Option<u64>,
    content_length: Option<u64>,
) -> bool {
    !partial && start > 0 && matches!((expected, content_length), (Some(t), Some(l)) if l != t)
}

/// Whether a 206 response's Content-Range is acceptable: must start at our
/// offset and (if we know it) match the expected total. A missing, unparseable,
/// or mismatched range is rejected — appending it would silently corrupt the file.
pub(crate) fn accepts_content_range(
    content_range: Option<&str>,
    start: u64,
    expected_total: Option<u64>,
) -> bool {
    matches!(
        content_range.and_then(parse_content_range),
        Some(ParsedContentRange::Satisfied {
            first_byte,
            complete_length: Some(total),
            ..
        }) if first_byte == start && expected_total.is_none_or(|expected| expected == total)
    )
}

/// Why a piece or segmented attempt failed (throttled = downgrade to single-stream; changed = fail terminally, never retry).
pub(crate) enum AttemptFail {
    Retryable(String),
    Throttled(String),
    Changed(String),
}

/// Fetch one `[start, end]` piece, retrying stalls (verifies the Content-Range total; never suggests server filenames).
pub(crate) async fn fetch_piece(
    ctx: &FetchCtx,
    start: u64,
    end: u64,
    total: u64,
) -> Result<Vec<u8>, AttemptFail> {
    let timeout = ctx.timeout;
    use AttemptFail::{Changed, Retryable, Throttled};
    let mut last_err = Retryable(gettext("Empty response"));
    for _ in 0..PIECE_TRIES {
        let mut builder = ctx
            .client
            .get(&ctx.url)
            .header("Range", format!("bytes={start}-{end}"));
        // If-Range: a changed object returns 200 instead of 206, which the
        // Content-Range check below turns into a Changed failure.
        if let Some(v) = ctx.if_range.get() {
            builder = builder.header("If-Range", v);
        }
        let req = stamp_request(builder, &ctx.user_agent, &ctx.url);
        let resp = match execute_with_timeout(&ctx.client, req, timeout).await {
            Ok(r) => r,
            Err(e) => {
                last_err = Retryable(e);
                continue;
            }
        };
        send_last_modified(&ctx.tx, &resp);
        // Capture once: if we sent If-Range and got 206, the validator is
        // already known — don't let a mid-download change poison it.
        if ctx.if_range.get().is_none() {
            send_validator(&ctx.tx, &resp);
        }
        if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            let code = resp.status().as_u16();
            // If-Range mismatch: the object changed → fail terminally, never retry a dead version.
            if code == 200 && ctx.if_range.get().is_some() {
                return Err(Changed(gettext("File changed on server")));
            }
            // Per-IP limits speak 403/429/503 (509 on some hosts): downgrade at once instead of burning retries.
            if matches!(code, 403 | 429 | 503) || code == 509 {
                return Err(Throttled(format!("Range rejected: HTTP {code}")));
            }
            last_err = Retryable(format!("Range rejected: HTTP {code}"));
            continue;
        }
        // 206 with a different validator than our session: the object changed
        // mid-download but the server didn't honor If-Range (returned 206
        // anyway). Treat as Changed to avoid splicing versions.
        if let Some(session_validator) = ctx.if_range.get() {
            let (_, etag, last_modified) = response_validators(&resp);
            if validator_changed(session_validator, etag.as_deref(), last_modified.as_deref()) {
                return Err(Changed(gettext("File changed on server")));
            }
        }
        let cr = resp
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok());
        match parse_content_range(cr.unwrap_or_default()) {
            Some(ParsedContentRange::Satisfied {
                first_byte,
                last_byte,
                complete_length: Some(response_total),
            }) if first_byte == start && last_byte == end && response_total == total => {}
            Some(ParsedContentRange::Satisfied {
                complete_length: Some(response_total),
                ..
            }) if response_total != total => {
                return Err(Changed(gettext("File changed on server")));
            }
            // Unparseable or wrong range: the host ignores ranges, so downgrade instead of retrying to Failed.
            _ => {
                return Err(Throttled(gettext("Server ignored range request")));
            }
        }
        let mut body = Vec::with_capacity((end - start + 1).min(2 * piece_len(total)) as usize);
        let mut stream = resp.bytes_stream();
        // Silence deadline, not a total one: slow-but-progressing pieces survive, stalled ones fail after 10x timeout.
        let quiet_limit = timeout.saturating_mul(10);
        let mut last_progress = Instant::now();
        let failed: Option<AttemptFail> = loop {
            if last_progress.elapsed() >= quiet_limit {
                break Some(Retryable(gettext("Piece stalled")));
            }
            let idle = tokio::time::sleep(quiet_limit.saturating_sub(last_progress.elapsed()));
            tokio::pin!(idle);
            tokio::select! {
                _ = &mut idle => {
                    break Some(Retryable(gettext("Piece stalled")))
                }
                next = tokio::time::timeout(timeout, stream.next()) => match next {
                    Ok(Some(Ok(c))) => {
                        if body.len() + c.len() > (end - start + 1) as usize {
                            break Some(Retryable(gettext("Server sent too much data")));
                        }
                        body.extend_from_slice(&c);
                        // Only real data resets the silence clock (keep-alives must not mask stalls).
                        if !c.is_empty() {
                            last_progress = Instant::now();
                        }
                    }
                    Ok(Some(Err(e))) => {
                        break Some(Retryable(format!("Download interrupted: {e}")))
                    }
                    Ok(None) => break None,
                    Err(_) => {
                        break Some(Retryable(gettext("Stalled connection timed out")))
                    }
                },
            }
        };
        if let Some(e) = failed {
            last_err = e;
            continue;
        }
        // A truncated stream must never record as a done piece (zeros on disk would be skipped on every later resume).
        if body.len() as u64 != end - start + 1 {
            last_err = Retryable(gettext("Incomplete piece"));
            continue;
        }
        return Ok(body);
    }
    Err(last_err)
}
/// Filename from a `Content-Disposition` header (RFC 6266): prefers `filename*`, falls back to `filename`, strips directories.
pub fn filename_from_content_disposition(value: &str) -> Option<String> {
    fn basename(raw: &str) -> &str {
        raw.trim()
            .trim_matches('"')
            .trim()
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or("")
            .trim()
    }
    /// Split on `;` but not inside double quotes: `filename="a;b.zip"`
    /// is one parameter, not two.
    fn split_params(value: &str) -> Vec<&str> {
        let mut parts = Vec::new();
        let mut start = 0;
        let mut in_quotes = false;
        let mut escaped = false;
        for (i, c) in value.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            match c {
                '\\' if in_quotes => escaped = true,
                '"' => in_quotes = !in_quotes,
                ';' if !in_quotes => {
                    parts.push(value[start..i].trim());
                    start = i + 1;
                }
                _ => {}
            }
        }
        parts.push(value[start..].trim());
        parts
    }
    let mut fallback = None;
    for part in split_params(value) {
        let Some((pname, pval)) = part.split_once('=') else {
            continue;
        };
        if pname.trim().eq_ignore_ascii_case("filename*") {
            let data = pval.split('\'').next_back().unwrap_or("").trim();
            let name = percent_decode(basename(data));
            if sane_filename(&name) {
                return Some(name);
            }
        } else if pname.trim().eq_ignore_ascii_case("filename") && fallback.is_none() {
            let name = basename(pval);
            if sane_filename(name) {
                fallback = Some(name.to_string());
            }
        }
    }
    fallback
}

/// Per-piece fetch attempts before a worker gives up on it.
const PIECE_TRIES: u32 = 3;


#[cfg(test)]
mod destination_ownership_tests {
    use super::{FetchCtx, FileIdentity, attempt_once, ensure_sized, single_loop};
    use crate::download_net::{DEFAULT_USER_AGENT, DownloadOptions};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn test_context(
        url: String,
        dest: std::path::PathBuf,
        tx: tokio::sync::mpsc::UnboundedSender<crate::engine_msg::EngineMsg>,
    ) -> FetchCtx {
        FetchCtx {
            client: reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("create test HTTP client"),
            url,
            dest,
            opts: DownloadOptions::default(),
            cookies: None,
            user_agent: DEFAULT_USER_AGENT.to_string(),
            timeout: Duration::from_secs(3),
            tx,
            if_range: std::sync::Arc::new(std::sync::OnceLock::new()),
        }
    }

    async fn read_request(stream: &mut tokio::net::TcpStream) {
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut chunk).await.expect("read request");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
        }
    }

    #[tokio::test]
    async fn a_retry_does_not_claim_a_destination_created_after_a_preopen_failure() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let address = listener.local_addr().expect("listener address");
        let directory = tempfile::tempdir().expect("create destination directory");
        let destination = directory.path().join("download.bin");
        let server_destination = destination.clone();

        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.expect("first request");
            read_request(&mut first).await;
            first
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .expect("write first response");
            drop(first);
            tokio::fs::write(&server_destination, b"foreign sentinel")
                .await
                .expect("create a foreign destination between attempts");

            if let Ok(Ok((mut second, _))) =
                tokio::time::timeout(Duration::from_secs(2), listener.accept()).await
            {
                read_request(&mut second).await;
                second
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\nConnection: close\r\n\r\nreplacement!!")
                    .await
                    .expect("write second response");
            }
        });

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let context = test_context(format!("http://{address}/download.bin"), destination.clone(), tx);
        let mut tries = 3;
        let mut expected_identity: Option<FileIdentity> = None;
        single_loop(&context, &mut tries, None, &mut expected_identity).await;
        server.await.expect("server task");

        let bytes = tokio::fs::read(&destination).await.expect("read destination");
        assert_eq!(bytes, b"foreign sentinel");
        assert!(expected_identity.is_none(), "a failed pre-open attempt must not acquire ownership");
    }

    #[tokio::test]
    async fn segmented_retry_refuses_to_resize_a_replaced_destination() {
        let directory = tempfile::tempdir().expect("create destination directory");
        let destination = directory.path().join("download.bin");
        tokio::fs::write(&destination, b"original owned bytes")
            .await
            .expect("write original");
        let original = tokio::fs::symlink_metadata(&destination)
            .await
            .expect("inspect original");
        use std::os::unix::fs::MetadataExt as _;
        let expected = FileIdentity {
            dev: original.dev(),
            ino: original.ino(),
        };

        let replacement = directory.path().join("replacement.tmp");
        tokio::fs::write(&replacement, b"foreign sentinel")
            .await
            .expect("write replacement");
        tokio::fs::rename(&replacement, &destination)
            .await
            .expect("replace the destination atomically");

        let mut expected_identity = Some(expected);
        let result = ensure_sized(&destination, 1024, &mut expected_identity).await;
        assert!(result.is_err(), "a replacement inode must not be adopted or resized");
        assert_eq!(
            tokio::fs::read(&destination).await.expect("read replacement"),
            b"foreign sentinel"
        );
    }

    #[tokio::test]
    async fn attempt_once_does_not_truncate_an_unowned_preexisting_path() {
        let directory = tempfile::tempdir().expect("create destination directory");
        let destination = directory.path().join("download.bin");
        tokio::fs::write(&destination, b"foreign sentinel")
            .await
            .expect("write foreign file");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let context = test_context("http://127.0.0.1:1/download.bin".to_string(), destination.clone(), tx);
        let mut identity = None;
        let error = attempt_once(&context, None, &mut identity)
            .await
            .expect_err("an unowned file must be rejected before networking");
        assert_eq!(error, crate::engine_msg::DEST_EXISTS);
        assert_eq!(
            tokio::fs::read(&destination).await.expect("read destination"),
            b"foreign sentinel"
        );
    }
}
