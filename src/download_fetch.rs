//! HTTP fetch engine: attempts, range probing, piece fetching, resume bitmaps.
//! The download manager spawns `run_download`; tests drive the pieces directly.

use crate::content_range::{ParsedContentRange, parse_content_range};
use crate::download_net::DownloadOptions;
use crate::download_pieces::{SegmentState, plan_pieces};
use crate::download_rate::{live_rate_limit, pace_chunk, parse_rate, progress_msg};
use crate::engine_msg::{DEST_EXISTS, EngineMsg};
use crate::file_names::{PIECE_MIN, piece_len, sane_filename};
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
    /// Browser-provided source page; absent for manually pasted links.
    pub(crate) referer: Option<String>,
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
            let mut claim = match DestinationClaim::capture(&ctx.dest).await {
                Ok(claim) => claim,
                Err(AttemptFail::Destination(problem)) => {
                    ctx.tx.send(EngineMsg::FailedDestination(problem)).ok();
                    return;
                }
                Err(error) => {
                    ctx.tx.send(EngineMsg::Failed(error.into_message())).ok();
                    return;
                }
            };
            single_loop(&ctx, &mut tries, None, &mut claim).await;
        }
        StartMode::Fresh => {
            let mut claim = DestinationClaim::default();
            // Single connection skips probing (the probe consumes single-use token URLs; one stream needs no total up front).
            if connections <= 1 {
                single_loop(&ctx, &mut tries, None, &mut claim).await;
                return;
            }
            match probe_ranges(
                &ctx.client,
                &ctx.url,
                &ctx.user_agent,
                ctx.referer.as_deref(),
                timeout,
            )
            .await
            {
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
                        single_loop(&ctx, &mut tries, Some(total), &mut claim).await;
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
                        if multi_loop(&ctx, total, None, connections, &mut tries, &mut claim).await
                        {
                            let mut single_tries = 3;
                            single_loop(&ctx, &mut single_tries, Some(total), &mut claim).await;
                        }
                    }
                }
                Err(_) => {
                    single_loop(&ctx, &mut tries, None, &mut claim).await;
                }
            }
        }
        StartMode::Resume(st) => {
            let total = st.total;
            let mut claim = match DestinationClaim::capture(&ctx.dest).await {
                Ok(claim) if claim.is_claimed() => claim,
                Ok(_) => {
                    ctx.tx
                        .send(EngineMsg::FailedDestination(
                            DestinationProblem::Disappeared,
                        ))
                        .ok();
                    return;
                }
                Err(AttemptFail::Destination(problem)) => {
                    ctx.tx.send(EngineMsg::FailedDestination(problem)).ok();
                    return;
                }
                Err(error) => {
                    ctx.tx.send(EngineMsg::Failed(error.into_message())).ok();
                    return;
                }
            };
            if multi_loop(&ctx, total, Some(st), connections, &mut tries, &mut claim).await {
                let mut single_tries = 3;
                single_loop(&ctx, &mut single_tries, Some(total), &mut claim).await;
            }
        }
    }
}

/// Single-stream fallback with retries. The identity is set only after a
/// fresh destination is exclusively created, or captured before resuming an
/// existing partial file. A retry cannot adopt a path that appeared meanwhile.
async fn report_changed_destination(ctx: &FetchCtx, claim: &mut DestinationClaim, message: String) {
    match reset_changed_destination(ctx, claim).await {
        Ok(()) => {
            ctx.tx
                .send(EngineMsg::FailedVersion {
                    error: message,
                    destination_reusable: true,
                })
                .ok();
        }
        Err(reset_error) => {
            tracing::warn!(
                error = %reset_error.into_message(),
                destination = %ctx.dest.display(),
                "could not safely reset the partial after the server object changed"
            );
            let error = format!(
                "{message}. Grab could not safely reset the partial file; remove it manually before retrying."
            );
            ctx.tx
                .send(EngineMsg::FailedVersion {
                    error,
                    destination_reusable: false,
                })
                .ok();
        }
    }
}

/// Reset only the inode captured by the current attempt. A pathname that has
/// disappeared or been replaced is never opened, truncated, or removed.
async fn reset_changed_destination(
    ctx: &FetchCtx,
    claim: &mut DestinationClaim,
) -> Result<(), AttemptFail> {
    if !claim.is_claimed() {
        return Err(AttemptFail::Destination(DestinationProblem::Disappeared));
    }
    let file = claim.open_for_writing(&ctx.dest).await?;
    file.set_len(0).await.map_err(|error| {
        AttemptFail::Retryable(format!("Could not reset the changed partial: {error}"))
    })?;
    file.sync_all().await.map_err(|error| {
        AttemptFail::Retryable(format!("Could not sync the reset partial: {error}"))
    })?;
    claim
        .verify_opened_file(&ctx.dest, &file, "reset partial")
        .await
}

/// Single-stream fallback with retries. The identity is set only after a
/// fresh destination is exclusively created, or captured before resuming an
/// existing partial file. A retry cannot adopt a path that appeared meanwhile.
async fn single_loop(
    ctx: &FetchCtx,
    tries: &mut i32,
    expected: Option<u64>,
    claim: &mut DestinationClaim,
) {
    let mut failures: u32 = 0;
    loop {
        match attempt_once(ctx, expected, claim).await {
            Ok(()) => {
                let size = tokio::fs::metadata(&ctx.dest)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0);
                crate::extension_server::discard_handoff_cookies_async(&ctx.url);
                ctx.tx.send(EngineMsg::Finished { size }).ok();
                return;
            }
            Err(failure) => {
                let message = match failure {
                    // A changed object invalidates the partial and any bitmap describing it.
                    AttemptFail::Changed(message) => {
                        report_changed_destination(ctx, claim, message).await;
                        return;
                    }
                    AttemptFail::Destination(problem) => {
                        ctx.tx.send(EngineMsg::FailedDestination(problem)).ok();
                        return;
                    }
                    AttemptFail::Retryable(e) | AttemptFail::Throttled(e) => e,
                };
                *tries -= 1;
                if *tries <= 0 {
                    ctx.tx.send(EngineMsg::Failed(message)).ok();
                    return;
                }
                // Retryable transport failures use capped jittered backoff to spread reconnects.
                failures += 1;
                tokio::time::sleep(backoff_delay(failures)).await;
            }
        }
    }
}

const FALLBACK_ACK_TIMEOUT: Duration = Duration::from_secs(30);

/// How often a single-stream transfer re-checks that its destination path still
/// names the file it opened. Writes through the open descriptor keep succeeding
/// after an unlink, so without this the transfer only notices when the body ends.
const DESTINATION_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// Wait for the UI to confirm that the segmented file was truncated and the bitmap dropped.
/// A closed ack (the handler drops `ack` on failure or `break`) or a stall must stop the
/// worker: continuing single-stream after the row was failed leaves a detached writer behind.
async fn wait_for_fallback_ack(
    ack: &mut tokio::sync::mpsc::Receiver<Result<(), DestinationProblem>>,
    timeout: Duration,
) -> Option<Result<(), DestinationProblem>> {
    match tokio::time::timeout(timeout, ack.recv()).await {
        Ok(Some(result)) => Some(result),
        Ok(None) | Err(_) => None,
    }
}

/// Retry loop around segmented attempts (each retry refetches only still-missing pieces).
/// Returns true when throttling or an unsafe short prefix requires a fresh single-stream transfer.
/// The caller retains the identity so a single-stream fallback keeps the same ownership proof.
async fn multi_loop(
    ctx: &FetchCtx,
    total: u64,
    saved: Option<SegmentState>,
    max_workers: usize,
    tries: &mut i32,
    claim: &mut DestinationClaim,
) -> bool {
    let mut st = saved.unwrap_or_else(|| SegmentState::new(total));
    let mut failures: u32 = 0;
    loop {
        let result = attempt_multi(ctx, total, &mut st, max_workers, claim).await;

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
                let Some(expected) = claim.identity() else {
                    ctx.tx
                        .send(EngineMsg::FailedDestination(
                            DestinationProblem::Disappeared,
                        ))
                        .ok();
                    return false;
                };
                let (ack_tx, mut ack_rx) =
                    tokio::sync::mpsc::channel::<Result<(), DestinationProblem>>(1);
                ctx.tx
                    .send(EngineMsg::FallbackSingle {
                        expected,
                        ack: ack_tx,
                    })
                    .ok();
                // Wait for the UI thread to verify and truncate the claimed inode before single-stream resumes.
                match wait_for_fallback_ack(&mut ack_rx, FALLBACK_ACK_TIMEOUT).await {
                    Some(Ok(())) => return true,
                    Some(Err(problem)) => {
                        ctx.tx.send(EngineMsg::FailedDestination(problem)).ok();
                        return false;
                    }
                    None => return false,
                }
            }
            Err(AttemptFail::Changed(message)) => {
                report_changed_destination(ctx, claim, message).await;
                return false;
            }
            Err(AttemptFail::Destination(problem)) => {
                ctx.tx.send(EngineMsg::FailedDestination(problem)).ok();
                return false;
            }
            Err(AttemptFail::Retryable(e)) => {
                *tries -= 1;
                if *tries <= 0 {
                    let Some(expected) = claim.identity() else {
                        ctx.tx
                            .send(EngineMsg::FailedDestination(
                                DestinationProblem::Disappeared,
                            ))
                            .ok();
                        return false;
                    };
                    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
                    if ctx
                        .tx
                        .send(EngineMsg::TruncatePrefix {
                            expected,
                            ack: ack_tx,
                        })
                        .is_ok()
                    {
                        match tokio::time::timeout(FALLBACK_ACK_TIMEOUT, ack_rx).await {
                            Ok(Ok(Ok(true))) => {
                                // The bitmap was cleared and the claimed partial was reset to zero.
                                // Reuse the existing destination claim for a fresh single stream.
                                return true;
                            }
                            Ok(Ok(Err(problem))) => {
                                ctx.tx.send(EngineMsg::FailedDestination(problem)).ok();
                                return false;
                            }
                            _ => {}
                        }
                    }
                    ctx.tx.send(EngineMsg::Failed(e)).ok();
                    return false;
                }
                // Jittered backoff on retryable failures (Throttled and a short-prefix
                // reset have explicit UI acknowledgments; Changed/Destination are terminal).
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
    claim: &mut DestinationClaim,
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
    let output_file = ensure_sized(&ctx.dest, total, claim).await?;
    let queue: Arc<Mutex<VecDeque<(u64, u64, u64)>>> =
        Arc::new(Mutex::new(missing.into_iter().collect()));
    let failed = Arc::new(AtomicBool::new(false));
    let first_err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let destination_problem: Arc<Mutex<Option<DestinationProblem>>> = Arc::new(Mutex::new(None));
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
        let (ctx, wtx, queue, failed, first_err, destination_problem, throttled, changed) = (
            ctx,
            wtx.clone(),
            Arc::clone(&queue),
            Arc::clone(&failed),
            Arc::clone(&first_err),
            Arc::clone(&destination_problem),
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
                    Err(AttemptFail::Destination(problem)) => {
                        *lock_recover(&destination_problem) = Some(problem);
                        lock_recover(&first_err).get_or_insert(problem.message());
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
        let claim = &*claim;
        async move {
            let mut file = output_file;
            let mut downloaded = st.completed_bytes();
            ctx.tx.send(progress_msg(downloaded, Some(total))).ok();
            let pace_start = Instant::now();
            let mut paced: u64 = 0;
            let mut written: u64 = 0;
            let mut completed_indices = Vec::new();
            let mut unsynced_bytes = 0_u64;
            let mut last_rate_update = Instant::now();
            // Checkpoint four bitmap pieces at a time. Larger downloads use
            // larger pieces, avoiding a fixed 4 MiB sync cadence on huge files.
            // Progress is published only after a durable batch, so a pause
            // triggered by visible progress has a safe bitmap.
            let durable_batch_bytes = piece_len(total).saturating_mul(4);
            let mut rate = live_rate_limit();
            use tokio::io::{AsyncSeekExt as _, AsyncWriteExt as _};
            while let Some((offset, bytes, idx)) = wrx.recv().await {
                file.seek(std::io::SeekFrom::Start(offset))
                    .await
                    .map_err(|error| {
                        AttemptFail::Retryable(format!("Cannot write file: {error}"))
                    })?;
                file.write_all(&bytes).await.map_err(|error| {
                    AttemptFail::Retryable(format!("Cannot write file: {error}"))
                })?;
                downloaded += bytes.len() as u64;
                written += bytes.len() as u64;
                unsynced_bytes = unsynced_bytes.saturating_add(bytes.len() as u64);
                completed_indices.push(idx);
                pace_chunk(&mut paced, pace_start, rate, bytes.len()).await;
                if last_rate_update.elapsed() >= Duration::from_millis(50) {
                    rate = live_rate_limit();
                    last_rate_update = Instant::now();
                }
                if unsynced_bytes >= durable_batch_bytes {
                    file.flush().await.map_err(|error| {
                        AttemptFail::Retryable(format!("Cannot write file: {error}"))
                    })?;
                    file.sync_data().await.map_err(|error| {
                        AttemptFail::Retryable(format!("Cannot sync file: {error}"))
                    })?;
                    // Publish a batch only while both the opened descriptor and
                    // the destination path still identify the claimed regular file.
                    claim
                        .verify_opened_file(&dest, &file, "downloaded file")
                        .await?;
                    for idx in completed_indices.drain(..) {
                        st.mark(idx);
                        ctx.tx.send(EngineMsg::PieceDone(idx)).ok();
                    }
                    unsynced_bytes = 0;
                    ctx.tx.send(progress_msg(downloaded, Some(total))).ok();
                }
            }
            file.flush()
                .await
                .map_err(|error| AttemptFail::Retryable(format!("Cannot write file: {error}")))?;
            // Finish with sync_all after the last batch, then publish any short
            // final batch. Interrupted data not covered by a completed batch
            // remains absent from the resume bitmap and will be fetched again.
            file.sync_all()
                .await
                .map_err(|error| AttemptFail::Retryable(format!("Cannot sync file: {error}")))?;
            claim
                .verify_opened_file(&dest, &file, "downloaded file")
                .await?;
            for idx in completed_indices {
                st.mark(idx);
                ctx.tx.send(EngineMsg::PieceDone(idx)).ok();
            }
            ctx.tx.send(progress_msg(downloaded, Some(total))).ok();
            Ok::<u64, AttemptFail>(written)
        }
    };
    let (wres, _) = tokio::join!(writer, futures_util::future::join_all(workers));
    let written = wres?;
    if let Some(problem) = lock_recover(&destination_problem).take() {
        return Err(AttemptFail::Destination(problem));
    }
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
    claim: &mut DestinationClaim,
) -> Result<(), AttemptFail> {
    // At most one restart: a 416 may only trigger a single reset-and-retry.
    let mut restarted = false;
    loop {
        let start = match tokio::fs::symlink_metadata(&ctx.dest).await {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(AttemptFail::Destination(if claim.is_claimed() {
                    DestinationProblem::Replaced
                } else {
                    DestinationProblem::Exists
                }));
            }
            Ok(metadata) => {
                claim.verify_metadata(&metadata)?;
                metadata.len()
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if claim.is_claimed() {
                    return Err(AttemptFail::Destination(DestinationProblem::Disappeared));
                }
                0
            }
            // A stat failure is not an empty file: starting over would truncate bytes we may not own.
            Err(error) => {
                return Err(AttemptFail::Retryable(format!(
                    "Cannot inspect file: {error}"
                )));
            }
        };

        let mut req = stamp_request(
            ctx.client.get(&ctx.url),
            &ctx.user_agent,
            &ctx.url,
            ctx.referer.as_deref(),
        );
        if start > 0 {
            req = req.header("Range", format!("bytes={start}-"));
            // If-Range: the server returns 206 only if the object still
            // matches our validator; a 200 means it changed -> restart fresh
            // instead of splicing a different file onto our bytes.
            if let Some(v) = ctx.if_range.get() {
                req = req.header("If-Range", v);
            }
        }
        let resp = execute_with_timeout(&ctx.client, req, ctx.timeout)
            .await
            .map_err(AttemptFail::Retryable)?;
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
            let still_owned = claim.verify(&ctx.dest).await.is_ok();
            // "Already complete" only with proof: matching length, unchanged inode,
            // unchanged local size, and fully allocated bytes.
            let mut already_complete = start > 0
                && still_owned
                && claimed == Some(start)
                && local_len(&ctx.dest).await == Some(start)
                && !has_holes(&ctx.dest).await;
            if !already_complete && claimed.is_none() && start > 0 && !restarted {
                // Bare 416 with no usable Content-Range: confirm the length before resetting.
                let hreq = stamp_request(
                    ctx.client.head(&ctx.url),
                    &ctx.user_agent,
                    &ctx.url,
                    ctx.referer.as_deref(),
                );
                if let Ok(hresp) = execute_with_timeout(&ctx.client, hreq, ctx.timeout).await {
                    already_complete = hresp.status().is_success()
                        && hresp.content_length() == Some(start)
                        && claim.verify(&ctx.dest).await.is_ok()
                        && local_len(&ctx.dest).await == Some(start)
                        && !has_holes(&ctx.dest).await;
                }
            }
            if already_complete {
                // A complete-looking partial can come from an interrupted attempt.
                // Sync it before accepting a 416 as proof of completion.
                let file = claim.open_for_writing(&ctx.dest).await?;
                file.sync_all().await.map_err(|error| {
                    AttemptFail::Retryable(format!("Cannot sync file: {error}"))
                })?;
                claim.verify(&ctx.dest).await?;
                return Ok(());
            }
            if restarted {
                return Err(AttemptFail::Retryable(gettext(
                    "Server rejects range requests",
                )));
            }

            // Reset through the already-owned file descriptor. Never unlink a
            // pathname after an async/network wait: it may now name someone else's file.
            if claim.is_claimed() {
                let file = claim.open_for_writing(&ctx.dest).await?;
                file.set_len(0).await.map_err(|error| {
                    AttemptFail::Retryable(format!("Cannot reset partial download: {error}"))
                })?;
            } else if tokio::fs::symlink_metadata(&ctx.dest).await.is_ok() {
                return Err(AttemptFail::Destination(DestinationProblem::Exists));
            }
            restarted = true;
            continue;
        }
        if !status.is_success() {
            return Err(AttemptFail::Retryable(format!(
                "HTTP {}: {}",
                status.as_u16(),
                status.canonical_reason().unwrap_or("error")
            )));
        }
        send_last_modified(&ctx.tx, &resp);
        // Keep one validator per transfer generation: a matching If-Range response
        // must not replace it with metadata from a later response.
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
            // A server may ignore If-Range and serve 206 for a changed object.
            // Without this check the response would be appended to bytes from
            // the old version, silently splicing two different files together.
            if let Some(session) = ctx.if_range.get() {
                let (_, etag, last_modified) = response_validators(&resp);
                if validator_changed(session, etag.as_deref(), last_modified.as_deref()) {
                    return Err(AttemptFail::Changed(gettext("File changed on server")));
                }
            }
            // A 206 must start at our requested offset; accepting another range
            // would splice unrelated bytes into the owned partial file.
            let cr = resp
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok());
            if !accepts_content_range(cr, start, expected_total) {
                return Err(AttemptFail::Retryable(gettext(
                    "Server returned an unexpected range",
                )));
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
            return Err(AttemptFail::Retryable(gettext(
                "Server returned an unexpected file size",
            )));
        }
        if !partial
            && start > 0
            && let Some(length) = resp.content_length()
            && length < start
        {
            return Err(AttemptFail::Retryable(gettext(
                "Server restarted the download with a smaller file",
            )));
        }

        let mut file = if partial {
            claim.open_partial(&ctx.dest, start).await?
        } else {
            let was_claimed = claim.is_claimed();
            let file = claim.open_for_writing(&ctx.dest).await?;
            if was_claimed {
                file.set_len(0).await.map_err(|error| {
                    AttemptFail::Retryable(format!("Cannot reset partial download: {error}"))
                })?;
            }
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
        let mut last_destination_check = Instant::now();
        let mut rate = live_rate_limit();
        let mut empty_streak: u32 = 0;
        use tokio::io::AsyncWriteExt as _;
        loop {
            let chunk = match tokio::time::timeout(ctx.timeout, stream.next()).await {
                Ok(Some(Ok(chunk))) => chunk,
                Ok(Some(Err(error))) => {
                    return Err(AttemptFail::Retryable(format!(
                        "Download interrupted: {error}"
                    )));
                }
                Ok(None) => break,
                Err(_) => {
                    return Err(AttemptFail::Retryable(gettext(
                        "Stalled connection timed out",
                    )));
                }
            };
            if chunk.is_empty() {
                empty_streak += 1;
                if empty_streak > 32 {
                    return Err(AttemptFail::Retryable(gettext(
                        "Stalled connection timed out",
                    )));
                }
                continue;
            }
            empty_streak = 0;
            file.write_all(&chunk)
                .await
                .map_err(|error| AttemptFail::Retryable(format!("Cannot write file: {error}")))?;
            downloaded += chunk.len() as u64;
            if last_destination_check.elapsed() >= DESTINATION_CHECK_INTERVAL {
                // Deleting or replacing the path does not interrupt writes
                // through the open descriptor; stop once the claim is gone
                // instead of downloading the rest of the body into an
                // unlinked or foreign inode.
                claim.verify(&ctx.dest).await?;
                last_destination_check = Instant::now();
            }
            pace_chunk(&mut paced, pace_start, rate, chunk.len()).await;
            if last_sent.elapsed() >= Duration::from_millis(50) {
                rate = live_rate_limit();
                ctx.tx.send(progress_msg(downloaded, total)).ok();
                last_sent = Instant::now();
            }
        }
        file.flush()
            .await
            .map_err(|error| AttemptFail::Retryable(format!("Cannot write file: {error}")))?;
        // Single-stream attempts complete before the UI persists progress/bitmap;
        // make the bytes durable first so a crash cannot resurrect a lost tail.
        file.sync_all().await.map_err(|error| {
            // Keep the owned partial on storage errors. A 416 retry only completes it
            // after its own sync succeeds, so a failed sync cannot report success.
            AttemptFail::Retryable(format!("Cannot sync file: {error}"))
        })?;
        claim.verify(&ctx.dest).await?;
        return match total {
            Some(total) if downloaded != total => {
                Err(AttemptFail::Retryable(gettext("Incomplete download")))
            }
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

/// Shrink `path` to the longest completed piece prefix before single-stream resume.
///
/// If the bitmap overstates the bytes that survived on disk, reset the file to zero.
/// Non-regular destinations and filesystem errors remain hard failures.
#[derive(Debug)]
pub(crate) enum PrefixTruncation {
    CompletedPrefix,
    ResetFromZero(std::fs::File),
}

#[derive(Debug)]
pub(crate) enum PrefixTruncationError {
    Destination(DestinationProblem),
    Io(std::io::Error),
}

impl std::fmt::Display for PrefixTruncationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Destination(problem) => formatter.write_str(&problem.message()),
            Self::Io(error) => write!(formatter, "{error}"),
        }
    }
}

/// Verify that the destination pathname still resolves to the inode claimed by
/// the running transfer. The open descriptor may remain valid after replacement.
pub(crate) fn verify_destination_identity(
    path: &std::path::Path,
    expected: FileIdentity,
) -> Result<(), PrefixTruncationError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || file_identity(&metadata) != expected =>
        {
            Err(PrefixTruncationError::Destination(
                DestinationProblem::Replaced,
            ))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(
            PrefixTruncationError::Destination(DestinationProblem::Disappeared),
        ),
        Err(error) => Err(PrefixTruncationError::Io(error)),
    }
}

fn truncate_to_prefix_inner(
    path: &std::path::Path,
    st: &SegmentState,
    expected: Option<FileIdentity>,
) -> Result<PrefixTruncation, PrefixTruncationError> {
    let prefix = st.prefix_len();
    let file = std_open_nofollow(
        |options| {
            options.write(true);
        },
        path,
    )
    .map_err(|error| match expected {
        Some(_) if error.kind() == std::io::ErrorKind::NotFound => {
            PrefixTruncationError::Destination(DestinationProblem::Disappeared)
        }
        Some(_) if error.raw_os_error() == Some(libc::ELOOP) => {
            PrefixTruncationError::Destination(DestinationProblem::Replaced)
        }
        _ => PrefixTruncationError::Io(error),
    })?;
    let metadata = file.metadata().map_err(PrefixTruncationError::Io)?;
    if !metadata.is_file() {
        return Err(match expected {
            Some(_) => PrefixTruncationError::Destination(DestinationProblem::Replaced),
            None => PrefixTruncationError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "segmented download destination is not a regular file",
            )),
        });
    }
    if let Some(expected) = expected {
        if file_identity(&metadata) != expected {
            return Err(PrefixTruncationError::Destination(
                DestinationProblem::Replaced,
            ));
        }
        verify_destination_identity(path, expected)?;
    }
    if metadata.len() < prefix {
        // Return the still-open descriptor without modifying it. The caller
        // must durably clear the bitmap before truncating, so a crash between
        // those operations cannot resurrect a bitmap for bytes that were lost.
        return Ok(PrefixTruncation::ResetFromZero(file));
    }
    if metadata.len() > prefix {
        if let Err(error) = file.set_len(prefix) {
            if let Some(expected) = expected {
                verify_destination_identity(path, expected)?;
            }
            return Err(PrefixTruncationError::Io(error));
        }
        // The caller clears the durable bitmap only after this sync succeeds.
        if let Err(error) = file.sync_all() {
            if let Some(expected) = expected {
                verify_destination_identity(path, expected)?;
            }
            return Err(PrefixTruncationError::Io(error));
        }
    }
    if let Some(expected) = expected {
        verify_destination_identity(path, expected)?;
    }
    Ok(PrefixTruncation::CompletedPrefix)
}

/// Shutdown reconciliation has no live worker claim, so it can only use the
/// persisted queue's path/bitmap pairing.
pub(crate) fn truncate_to_prefix(
    path: &std::path::Path,
    st: &SegmentState,
) -> std::io::Result<PrefixTruncation> {
    truncate_to_prefix_inner(path, st, None).map_err(|error| match error {
        PrefixTruncationError::Io(error) => error,
        PrefixTruncationError::Destination(problem) => std::io::Error::other(problem.message()),
    })
}

/// Live fallback may only truncate the inode captured by the current attempt.
pub(crate) fn truncate_to_prefix_owned(
    path: &std::path::Path,
    st: &SegmentState,
    expected: FileIdentity,
) -> Result<PrefixTruncation, PrefixTruncationError> {
    truncate_to_prefix_inner(path, st, Some(expected))
}

/// Reset an already-open claimed inode and verify that the pathname still names it.
pub(crate) fn truncate_open_file_to_zero_owned(
    file: std::fs::File,
    path: &std::path::Path,
    expected: FileIdentity,
) -> Result<(), PrefixTruncationError> {
    let metadata = file.metadata().map_err(PrefixTruncationError::Io)?;
    if !metadata.is_file() || file_identity(&metadata) != expected {
        return Err(PrefixTruncationError::Destination(
            DestinationProblem::Replaced,
        ));
    }
    if let Err(error) = file.set_len(0) {
        verify_destination_identity(path, expected)?;
        return Err(PrefixTruncationError::Io(error));
    }
    if let Err(error) = file.sync_all() {
        verify_destination_identity(path, expected)?;
        return Err(PrefixTruncationError::Io(error));
    }
    verify_destination_identity(path, expected)
}

/// Reset a previously validated no-follow descriptor after its resume bitmap was cleared.
///
/// The caller owns the ordering: persist the empty bitmap before invoking this function.
pub(crate) fn truncate_open_file_to_zero(file: std::fs::File) -> std::io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "segmented download destination is not a regular file",
        ));
    }
    file.set_len(0)?;
    file.sync_all()
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
        .open(path)
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
) -> std::io::Result<tokio::fs::File> {
    let file = open_nofollow(
        |o| {
            o.append(true);
        },
        path,
    )
    .await?;
    // The stat happened before the request: a local append/truncate in that window would shift every byte.
    let at = file.metadata().await?.len();
    if at != start {
        return Err(std::io::Error::other(gettext(
            "The partial file changed during resume",
        )));
    }
    Ok(file)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileIdentity {
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

const DESTINATION_REPLACED: &str = "Destination file was replaced during download";
const DESTINATION_DISAPPEARED: &str = "Destination file disappeared during download";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DestinationProblem {
    Exists,
    Disappeared,
    Replaced,
}

impl DestinationProblem {
    pub(crate) fn message(self) -> String {
        match self {
            Self::Exists => DEST_EXISTS.to_string(),
            Self::Disappeared => DESTINATION_DISAPPEARED.to_string(),
            Self::Replaced => DESTINATION_REPLACED.to_string(),
        }
    }
}

fn destination_open_error(error: std::io::Error, claimed: bool) -> AttemptFail {
    if claimed && error.kind() == std::io::ErrorKind::NotFound {
        AttemptFail::Destination(DestinationProblem::Disappeared)
    } else if claimed && error.raw_os_error() == Some(libc::ELOOP) {
        AttemptFail::Destination(DestinationProblem::Replaced)
    } else {
        AttemptFail::Retryable(format!("Cannot write file: {error}"))
    }
}

/// Proof that a transfer owns the destination inode. Fresh transfers begin
/// unclaimed and may acquire ownership only through an exclusive create_new.
/// This stateful claim is not `Copy`: an implicit copy could fork its ownership state.
#[derive(Clone, Debug, Default)]
struct DestinationClaim {
    identity: Option<FileIdentity>,
}

impl DestinationClaim {
    async fn capture(path: &std::path::Path) -> Result<Self, AttemptFail> {
        Ok(Self {
            identity: destination_identity(path).await?,
        })
    }

    fn is_claimed(&self) -> bool {
        self.identity.is_some()
    }

    fn identity(&self) -> Option<FileIdentity> {
        self.identity
    }

    #[cfg(test)]
    fn from_identity(identity: FileIdentity) -> Self {
        Self {
            identity: Some(identity),
        }
    }

    fn verify_metadata(&self, metadata: &std::fs::Metadata) -> Result<(), AttemptFail> {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(AttemptFail::Destination(if self.is_claimed() {
                DestinationProblem::Replaced
            } else {
                DestinationProblem::Exists
            }));
        }
        match self.identity {
            Some(expected) if file_identity(metadata) == expected => Ok(()),
            Some(_) => Err(AttemptFail::Destination(DestinationProblem::Replaced)),
            None => Err(AttemptFail::Destination(DestinationProblem::Exists)),
        }
    }

    async fn verify(&self, path: &std::path::Path) -> Result<(), AttemptFail> {
        let expected = self
            .identity
            .ok_or(AttemptFail::Destination(DestinationProblem::Exists))?;
        match tokio::fs::symlink_metadata(path).await {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                Err(AttemptFail::Destination(DestinationProblem::Replaced))
            }
            Ok(metadata) if file_identity(&metadata) == expected => Ok(()),
            Ok(_) => Err(AttemptFail::Destination(DestinationProblem::Replaced)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(AttemptFail::Destination(DestinationProblem::Disappeared))
            }
            Err(error) => Err(AttemptFail::Retryable(format!(
                "Cannot inspect file: {error}"
            ))),
        }
    }

    async fn verify_opened_file(
        &self,
        path: &std::path::Path,
        file: &tokio::fs::File,
        context: &str,
    ) -> Result<(), AttemptFail> {
        let metadata = file.metadata().await.map_err(|error| {
            AttemptFail::Retryable(format!("Could not inspect {context}: {error}"))
        })?;
        self.verify_metadata(&metadata)?;
        self.verify(path).await
    }

    /// Open the claimed inode without following symlinks, or exclusively
    /// create a fresh destination and claim its identity before returning.
    async fn open_for_writing(
        &mut self,
        path: &std::path::Path,
    ) -> Result<tokio::fs::File, AttemptFail> {
        let was_claimed = self.is_claimed();
        if was_claimed {
            self.verify(path).await?;
        }
        let file = open_nofollow(
            move |options| {
                options.write(true);
                if !was_claimed {
                    options.create_new(true);
                }
            },
            path,
        )
        .await
        .map_err(|error| {
            if !was_claimed && error.kind() == std::io::ErrorKind::AlreadyExists {
                AttemptFail::Destination(DestinationProblem::Exists)
            } else {
                destination_open_error(error, was_claimed)
            }
        })?;
        let metadata = file.metadata().await.map_err(|error| {
            AttemptFail::Retryable(format!("Cannot inspect downloaded file: {error}"))
        })?;
        if !metadata.is_file() {
            return Err(AttemptFail::Destination(if was_claimed {
                DestinationProblem::Replaced
            } else {
                DestinationProblem::Exists
            }));
        }
        let actual = file_identity(&metadata);
        match self.identity {
            Some(expected) if expected != actual => {
                return Err(AttemptFail::Destination(DestinationProblem::Replaced));
            }
            None => self.identity = Some(actual),
            _ => {}
        }
        self.verify_opened_file(path, &file, "downloaded file")
            .await?;
        Ok(file)
    }

    /// Resume an append only when the requested offset and opened inode still
    /// match the captured destination claim.
    async fn open_partial(
        &self,
        path: &std::path::Path,
        start: u64,
    ) -> Result<tokio::fs::File, AttemptFail> {
        if !self.is_claimed() {
            return Err(AttemptFail::Destination(DestinationProblem::Exists));
        }
        let file = open_partial(path, start)
            .await
            .map_err(|error| destination_open_error(error, true))?;
        self.verify_opened_file(path, &file, "partial download")
            .await?;
        Ok(file)
    }
}

/// Capture an existing regular destination before a resume request. A fresh
/// download receives `None` and must create its first file with create_new.
async fn destination_identity(path: &std::path::Path) -> Result<Option<FileIdentity>, AttemptFail> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(AttemptFail::Destination(DestinationProblem::Exists))
        }
        Ok(metadata) => Ok(Some(file_identity(&metadata))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AttemptFail::Retryable(format!(
            "Cannot inspect file: {error}"
        ))),
    }
}

async fn ensure_sized(
    dest: &std::path::Path,
    total: u64,
    claim: &mut DestinationClaim,
) -> Result<tokio::fs::File, AttemptFail> {
    let file = claim.open_for_writing(dest).await?;
    let metadata = file
        .metadata()
        .await
        .map_err(|error| AttemptFail::Retryable(format!("Cannot inspect file: {error}")))?;
    if metadata.len() != total
        && let Err(error) = file.set_len(total).await
    {
        let message = format!("Cannot write file: {error}");
        // A full disk is not server throttling: switching to single-stream cannot
        // free space. Keep it retryable so the row fails with the storage message.
        return Err(AttemptFail::Retryable(message));
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

/// Produce a redirect-safe Referer using only a scheme, host, and port.
/// This matches browsers' `strict-origin-when-cross-origin` default and
/// avoids leaking a source page's path or query, which may carry tokens.
/// The HTTP client may follow redirects without giving Grab a chance to rewrite
/// request headers at each hop, so a full source-page URL is never attached.
fn request_referer(target: &str, source: Option<&str>) -> Option<String> {
    let Some(source) = source else {
        return crate::referrer::origin_only(target);
    };
    let source_url = crate::referrer::parse_source(source)?;
    let target_url = url::Url::parse(target).ok()?;
    if source_url.scheme() == "https" && target_url.scheme() == "http" {
        return None;
    }
    crate::referrer::origin_only(source_url.as_str())
}

#[cfg(test)]
mod request_referer_tests {
    use super::request_referer;

    #[test]
    fn same_origin_uses_only_the_source_origin() {
        assert_eq!(
            request_referer(
                "https://example.test/download/file",
                Some("https://example.test/watch?id=42#player"),
            ),
            Some("https://example.test/".to_string())
        );
    }

    #[tokio::test]
    async fn redirect_does_not_forward_source_page_path_or_query() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        async fn read_headers(stream: &mut tokio::net::TcpStream) -> String {
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    stream.read(&mut buffer),
                )
                .await
                .expect("request headers arrive")
                .expect("read request headers");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            String::from_utf8(request).expect("HTTP request is UTF-8")
        }

        let first_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind initial origin");
        let first_address = first_listener.local_addr().expect("initial origin address");
        let redirected_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind redirected origin");
        let redirected_address = redirected_listener
            .local_addr()
            .expect("redirected origin address");

        let first_server = tokio::spawn(async move {
            let (mut stream, _) = first_listener.accept().await.expect("initial request");
            let request = read_headers(&mut stream).await;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://{redirected_address}/resource\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .expect("write redirect response");
            request
        });
        let redirected_server = tokio::spawn(async move {
            let (mut stream, _) = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                redirected_listener.accept(),
            )
            .await
            .expect("redirected request arrives")
            .expect("accept redirected request");
            let request = read_headers(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .expect("write final response");
            request
        });

        let client =
            crate::download_net::http_client_for(None, None).expect("build guarded client");
        let target = format!("http://{first_address}/media");
        let source = format!("http://{first_address}/watch?token=private");
        let request =
            super::stamp_request(client.get(&target), "Grab-Test-UA", &target, Some(&source));
        let response =
            super::execute_with_timeout(&client, request, std::time::Duration::from_secs(3))
                .await
                .expect("follow safe redirect");
        assert!(response.status().is_success());

        let initial_request = first_server.await.expect("initial server task");
        let redirected_request = redirected_server.await.expect("redirect server task");
        for request in [&initial_request, &redirected_request] {
            let referer = request
                .lines()
                .find(|line| line.to_ascii_lowercase().starts_with("referer:"))
                .expect("request has a Referer header");
            assert_eq!(
                referer.split_once(':').expect("header separator").1.trim(),
                format!("http://{first_address}/"),
                "only the source origin may survive an automatic redirect"
            );
            assert!(!request.contains("token=private"));
            assert!(!request.contains("/watch"));
        }
    }

    #[test]
    fn cross_origin_sends_source_origin_only() {
        assert_eq!(
            request_referer(
                "https://cdn.example.test/file",
                Some("https://www.example.test/watch?id=private"),
            ),
            Some("https://www.example.test/".to_string())
        );
    }

    #[test]
    fn downgrade_and_invalid_source_suppress_referer() {
        assert_eq!(
            request_referer(
                "http://cdn.example.test/file",
                Some("https://www.example.test/watch"),
            ),
            None
        );
        assert_eq!(
            request_referer("https://cdn.example.test/file", Some("javascript:alert(1)"),),
            None
        );
        assert_eq!(
            request_referer(
                "https://cdn.example.test/file",
                Some("https://user:pass@example.test/watch"),
            ),
            None
        );
        assert_eq!(
            request_referer("https://cdn.example.test/file", Some(&"x".repeat(2049)),),
            None
        );
    }
}

/// Stamp one outbound request like a browser (User-Agent + origin-only
/// Referer); single choke point for all requests. No Cookie: cookie-bearing
/// downloads use a client with the jar as provider (see `http_client_with_jar`),
/// which re-applies in-scope cookies on every hop.
pub(crate) fn stamp_request(
    mut req: reqwest::RequestBuilder,
    user_agent: &str,
    url: &str,
    source_referer: Option<&str>,
) -> reqwest::RequestBuilder {
    if !user_agent.trim().is_empty() {
        req = req.header("User-Agent", user_agent.trim());
    }
    if let Some(referer) = request_referer(url, source_referer) {
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
    user_agent: &str,
    referer: Option<&str>,
    timeout: Duration,
) -> Result<(u64, crate::engine_msg::ValidatorInfo), String> {
    let req = stamp_request(
        client.get(url).header("Range", "bytes=0-0"),
        user_agent,
        url,
        referer,
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

/// Why an attempt failed. Destination ownership failures never enter the retry budget.
#[derive(Debug)]
pub(crate) enum AttemptFail {
    Retryable(String),
    Throttled(String),
    Changed(String),
    Destination(DestinationProblem),
}

impl AttemptFail {
    fn into_message(self) -> String {
        match self {
            Self::Retryable(message) | Self::Throttled(message) | Self::Changed(message) => message,
            Self::Destination(problem) => problem.message(),
        }
    }
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
        let req = stamp_request(builder, &ctx.user_agent, &ctx.url, ctx.referer.as_deref());
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
            // RFC 5987: charset'language'percent-encoded-value. Honour the
            // common Latin-1 charset instead of decoding every value as UTF-8.
            let extended_value = pval.trim().trim_matches('"');
            let mut fields = extended_value.splitn(3, '\'');
            let charset = fields.next().unwrap_or("").trim().to_ascii_lowercase();
            let _language = fields.next();
            let value = fields.next().unwrap_or("").trim();
            let bytes: Vec<u8> = percent_encoding::percent_decode_str(basename(value)).collect();
            let name = match charset.as_str() {
                "iso-8859-1" | "iso8859-1" | "latin1" => {
                    bytes.into_iter().map(char::from).collect()
                }
                // Empty charset defaults to UTF-8; unknown charsets get a
                // lossy UTF-8 best effort (std has no general charset decoder).
                _ => String::from_utf8_lossy(&bytes).into_owned(),
            };
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
    use super::{
        AttemptFail, DestinationClaim, DestinationProblem, FetchCtx, FileIdentity,
        PrefixTruncation, PrefixTruncationError, attempt_once, ensure_sized,
        reset_changed_destination, single_loop, truncate_open_file_to_zero, truncate_to_prefix,
        truncate_to_prefix_owned, wait_for_fallback_ack,
    };
    use crate::download_net::{DEFAULT_USER_AGENT, DownloadOptions};
    use crate::download_pieces::SegmentState;
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
            referer: None,
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
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
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
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\nConnection: close\r\n\r\nreplacement!!",
                    )
                    .await
                    .expect("write second response");
            }
        });

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let context = test_context(
            format!("http://{address}/download.bin"),
            destination.clone(),
            tx,
        );
        let mut tries = 3;
        let mut claim = DestinationClaim::default();
        single_loop(&context, &mut tries, None, &mut claim).await;
        server.await.expect("server task");

        let bytes = tokio::fs::read(&destination)
            .await
            .expect("read destination");
        assert_eq!(bytes, b"foreign sentinel");
        assert!(
            !claim.is_claimed(),
            "a failed pre-open attempt must not acquire ownership"
        );
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

        let mut claim = DestinationClaim::from_identity(expected);
        let result = ensure_sized(&destination, 1024, &mut claim).await;
        assert!(
            matches!(
                result,
                Err(AttemptFail::Destination(DestinationProblem::Replaced))
            ),
            "a replacement inode must be a terminal destination failure"
        );
        assert_eq!(
            tokio::fs::read(&destination)
                .await
                .expect("read replacement"),
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
        let context = test_context(
            "http://127.0.0.1:1/download.bin".to_string(),
            destination.clone(),
            tx,
        );
        let mut claim = DestinationClaim::default();
        let error = attempt_once(&context, None, &mut claim)
            .await
            .expect_err("an unowned file must be rejected before networking");
        assert!(
            matches!(error, AttemptFail::Destination(DestinationProblem::Exists)),
            "expected a terminal typed ownership failure, got {error:?}"
        );
        assert_eq!(
            tokio::fs::read(&destination)
                .await
                .expect("read destination"),
            b"foreign sentinel"
        );
    }

    #[tokio::test]
    async fn resume_rejects_a_206_for_a_changed_object() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept request");
            read_request(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 4-9/10\r\nContent-Length: 6\r\nETag: \"server-etag-v2\"\r\nConnection: close\r\n\r\nNEWDAT",
                )
                .await
                .expect("write 206 response");
        });

        let directory = tempfile::tempdir().expect("create destination directory");
        let destination = directory.path().join("download.bin");
        tokio::fs::write(&destination, b"old-")
            .await
            .expect("write owned partial");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let context = test_context(
            format!("http://{address}/download.bin"),
            destination.clone(),
            tx,
        );
        let _ = context.if_range.set("\"client-etag-v1\"".to_string());
        let mut claim = DestinationClaim::capture(&destination)
            .await
            .expect("capture owned partial");

        let error = attempt_once(&context, None, &mut claim)
            .await
            .expect_err("a 206 from a changed object must fail");
        assert!(
            matches!(error, AttemptFail::Changed(_)),
            "expected Changed, got {error:?}"
        );
        server.await.expect("server task");
        assert_eq!(
            tokio::fs::read(&destination)
                .await
                .expect("read destination"),
            b"old-",
            "the changed response must not be spliced onto the owned partial"
        );
        reset_changed_destination(&context, &mut claim)
            .await
            .expect("reset only the claimed partial inode");
        assert!(
            tokio::fs::read(&destination)
                .await
                .expect("read reset destination")
                .is_empty(),
            "the changed partial must be empty before the UI clears its resume metadata"
        );
    }

    #[tokio::test]
    async fn changed_version_reset_never_truncates_a_replacement_inode() {
        let directory = tempfile::tempdir().expect("create destination directory");
        let destination = directory.path().join("download.bin");
        tokio::fs::write(&destination, b"old partial")
            .await
            .expect("write owned partial");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let context = test_context(
            "http://127.0.0.1:1/download.bin".to_string(),
            destination.clone(),
            tx,
        );
        let mut claim = DestinationClaim::capture(&destination)
            .await
            .expect("capture owned partial");

        let replacement = directory.path().join("replacement.bin");
        tokio::fs::write(&replacement, b"foreign sentinel")
            .await
            .expect("write replacement file");
        tokio::fs::rename(&replacement, &destination)
            .await
            .expect("replace destination path");

        let error = reset_changed_destination(&context, &mut claim)
            .await
            .expect_err("replacement inode must not be reset");
        assert!(
            matches!(
                error,
                AttemptFail::Destination(DestinationProblem::Replaced)
            ),
            "expected a typed replacement failure, got {error:?}"
        );
        assert_eq!(
            tokio::fs::read(&destination)
                .await
                .expect("read replacement"),
            b"foreign sentinel"
        );
    }

    #[test]
    fn truncation_syncs_only_the_completed_piece_prefix() {
        let directory = tempfile::tempdir().expect("create destination directory");
        let destination = directory.path().join("download.bin");
        let mut state = SegmentState::new(64 * 1024 * 1024);
        state.mark(0);
        let prefix = state.prefix_len();
        let original_len = prefix + 4096;
        std::fs::write(
            &destination,
            vec![0xa5; usize::try_from(original_len).expect("test file size fits usize")],
        )
        .expect("write segmented destination");

        assert!(matches!(
            truncate_to_prefix(&destination, &state).expect("truncate to completed prefix"),
            PrefixTruncation::CompletedPrefix
        ));

        let bytes = std::fs::read(&destination).expect("read truncated destination");
        assert_eq!(
            u64::try_from(bytes.len()).expect("file size fits u64"),
            prefix
        );
        assert!(bytes.iter().all(|byte| *byte == 0xa5));
    }

    #[test]
    fn truncation_identifies_a_file_shorter_than_the_completed_prefix() {
        let directory = tempfile::tempdir().expect("create destination directory");
        let destination = directory.path().join("download.bin");
        let mut state = SegmentState::new(64 * 1024 * 1024);
        state.mark(0);
        let prefix = state.prefix_len();
        std::fs::write(
            &destination,
            vec![0xa5; usize::try_from(prefix - 1).expect("test file size fits usize")],
        )
        .expect("write short destination");

        let reset_file =
            match truncate_to_prefix(&destination, &state).expect("inspect short partial") {
                PrefixTruncation::ResetFromZero(file) => file,
                PrefixTruncation::CompletedPrefix => panic!("short file must request a zero reset"),
            };
        assert_eq!(
            std::fs::metadata(&destination)
                .expect("inspect before reset")
                .len(),
            prefix - 1
        );
        truncate_open_file_to_zero(reset_file).expect("reset unsafe partial");
        assert_eq!(
            std::fs::metadata(&destination)
                .expect("inspect reset destination")
                .len(),
            0
        );
    }

    #[tokio::test]
    async fn fallback_ack_requires_an_explicit_success() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<(), DestinationProblem>>(1);
        tx.send(Ok(())).await.expect("send fallback ack");
        assert_eq!(
            wait_for_fallback_ack(&mut rx, Duration::from_secs(1)).await,
            Some(Ok(()))
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<(), DestinationProblem>>(1);
        tx.send(Err(DestinationProblem::Replaced))
            .await
            .expect("send destination error");
        assert!(matches!(
            wait_for_fallback_ack(&mut rx, Duration::from_secs(1)).await,
            Some(Err(DestinationProblem::Replaced))
        ));

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<(), DestinationProblem>>(1);
        drop(tx);
        assert_eq!(
            wait_for_fallback_ack(&mut rx, Duration::from_secs(1)).await,
            None,
            "a closed ack must stop the worker, not continue single-stream"
        );

        let (_tx, mut rx) = tokio::sync::mpsc::channel::<Result<(), DestinationProblem>>(1);
        assert_eq!(
            wait_for_fallback_ack(&mut rx, Duration::from_millis(10)).await,
            None,
            "a stalled ack must not hang the worker"
        );
    }

    #[tokio::test]
    async fn claimed_prefix_truncation_rejects_a_replaced_destination() {
        let directory = tempfile::tempdir().expect("create destination directory");
        let destination = directory.path().join("download.bin");
        let mut state = SegmentState::new(64 * 1024 * 1024);
        state.mark(0);
        let prefix = state.prefix_len();
        std::fs::write(
            &destination,
            vec![0xa5; usize::try_from(prefix + 4096).expect("file size fits usize")],
        )
        .expect("write owned partial");

        let claim = DestinationClaim::capture(&destination)
            .await
            .expect("capture destination claim");
        let expected = claim.identity().expect("claim has an inode");

        let replacement = directory.path().join("replacement.bin");
        std::fs::write(&replacement, b"foreign sentinel").expect("write replacement");
        std::fs::rename(&replacement, &destination).expect("replace destination path");

        let result = truncate_to_prefix_owned(&destination, &state, expected);
        assert!(matches!(
            result,
            Err(PrefixTruncationError::Destination(
                DestinationProblem::Replaced
            ))
        ));
        assert_eq!(
            std::fs::read(&destination).expect("read replacement"),
            b"foreign sentinel",
            "fallback must never truncate a replacement inode"
        );
    }

    #[cfg(unix)]
    #[test]
    fn truncation_never_follows_a_destination_symlink() {
        let directory = tempfile::tempdir().expect("create destination directory");
        let target = directory.path().join("target.bin");
        let link = directory.path().join("download.bin");
        let mut state = SegmentState::new(64 * 1024 * 1024);
        state.mark(0);
        let original_len = state.prefix_len() + 4096;
        std::fs::write(
            &target,
            vec![0xa5; usize::try_from(original_len).expect("test file size fits usize")],
        )
        .expect("write target");
        std::os::unix::fs::symlink(&target, &link).expect("create destination symlink");

        assert!(truncate_to_prefix(&link, &state).is_err());
        assert_eq!(
            std::fs::metadata(&target)
                .expect("inspect symlink target")
                .len(),
            original_len
        );
    }
}
