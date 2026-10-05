//! HTTP fetch engine: attempts, range probing, piece fetching, resume bitmaps.
//! The download manager spawns `run_download`; tests drive the pieces directly.

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
    // One export per attempt (jars are cheap next to downloads).
    if !ctx.opts.cookies_browser.is_empty() && ctx.opts.cookies_browser != "none" {
        let youtube_bin = crate::video::resolve_libraries()
            .map(|libs| libs.youtube)
            .ok();
        if let Some(bin) = youtube_bin {
            ctx.cookies =
                crate::cookies::jar_for_browser(&ctx.opts.cookies_browser, &bin, &ctx.url).await;
        }
    }
    match mode {
        StartMode::Single => {
            single_loop(&ctx, &mut tries, None, false).await;
        }
        StartMode::Fresh => {
            // Single connection skips probing (the probe consumes single-use token URLs; one stream needs no total up front).
            if connections <= 1 {
                single_loop(&ctx, &mut tries, None, true).await;
                return;
            }
            match probe_ranges(&ctx.client, &ctx.url, ctx.cookies.as_ref(), timeout).await {
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
                        single_loop(&ctx, &mut tries, Some(total), true).await;
                    } else {
                        ctx.tx.send(EngineMsg::SegmentsInit { total }).ok();
                        if multi_loop(&ctx, total, None, connections, &mut tries).await {
                            let mut single_tries = 3;
                            single_loop(&ctx, &mut single_tries, Some(total), false).await;
                        }
                    }
                }
                Err(_) => {
                    single_loop(&ctx, &mut tries, None, true).await;
                }
            }
        }
        StartMode::Resume(st) => {
            let total = st.total;
            if multi_loop(&ctx, total, Some(st), connections, &mut tries).await {
                let mut single_tries = 3;
                single_loop(&ctx, &mut single_tries, Some(total), false).await;
            }
        }
    }
}

/// Single-stream fallback with retries (`expected` rejects a restarted 200 with a disagreeing length; `claim` fails fast on foreign files).
async fn single_loop(ctx: &FetchCtx, tries: &mut i32, expected: Option<u64>, claim: bool) {
    // Only the first attempt may claim a missing file; later bytes are ours.
    let mut claim = claim;
    let mut failures: u32 = 0;
    loop {
        match attempt_once(ctx, expected, claim).await {
            Ok(()) => {
                let size = tokio::fs::metadata(&ctx.dest)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0);
                ctx.tx.send(EngineMsg::Finished { size }).ok();
                return;
            }
            Err(e) => {
                // A taken path never frees on retry: fail at once so the row surfaces the error instead of requeueing.
                if e == DEST_EXISTS {
                    ctx.tx.send(EngineMsg::Failed(e)).ok();
                    return;
                }
                claim = false;
                *tries -= 1;
                if *tries <= 0 {
                    ctx.tx.send(EngineMsg::Failed(e)).ok();
                    return;
                }
                // Jittered backoff on retryable failures (DEST_EXISTS is terminal, handled above).
                failures += 1;
                tokio::time::sleep(backoff_delay(failures)).await;
            }
        }
    }
}

/// Retry loop around segmented attempts (each retry refetches only still-missing pieces).
/// Returns true when the server throttled parallel connections (caller continues single-stream).
async fn multi_loop(
    ctx: &FetchCtx,
    total: u64,
    saved: Option<SegmentState>,
    max_workers: usize,
    tries: &mut i32,
) -> bool {
    let mut st = saved.unwrap_or_else(|| SegmentState::new(total));
    let mut failures: u32 = 0;
    loop {
        match attempt_multi(ctx, total, &mut st, max_workers).await {
            Ok(()) => {
                let size = tokio::fs::metadata(&ctx.dest)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0);
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
    ensure_sized(&ctx.dest, total).await?;
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
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .open(&dest)
                .await
                .map_err(|e| format!("Cannot write file: {e}"))?;
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
pub(crate) fn has_holes(path: &std::path::Path) -> bool {
    std::fs::metadata(path)
        .map(|m| {
            use std::os::unix::fs::MetadataExt;
            m.blocks().saturating_mul(512) < m.len()
        })
        .unwrap_or(false)
}

async fn attempt_once(
    ctx: &FetchCtx,
    expected_total: Option<u64>,
    // True only for the first attempt of a fresh single-stream run (a foreign file must fail, not truncate).
    claim: bool,
) -> Result<(), String> {
    // At most one restart: a 416 may only trigger a single delete-and-retry.
    let mut restarted = false;
    loop {
        let start = tokio::fs::metadata(&ctx.dest)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        let mut req = stamp_request(
            ctx.client.get(&ctx.url),
            DEFAULT_USER_AGENT,
            ctx.cookies.as_ref(),
            &ctx.url,
        );
        if start > 0 {
            req = req.header("Range", format!("bytes={start}-"));
            // If-Range: the server returns 206 only if the object still
            // matches our validator; a 200 means it changed → restart fresh
            // instead of splicing a different file onto our bytes.
            if let Some(v) = ctx.if_range.get() {
                req = req.header("If-Range", v);
            }
        }
        let resp = execute_with_timeout(&ctx.client, req, ctx.timeout).await?;
        let status = resp.status();
        if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            // "Already complete" only with proof: matching length AND fully allocated bytes (a sparse full-size file must never take this shortcut).
            let claimed = resp
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.split('/').next_back())
                .and_then(|t| t.parse::<u64>().ok());
            // A length match alone proves nothing: the allocation check above rules out holes.
            if start > 0 && claimed == Some(start) && !has_holes(&ctx.dest) {
                return Ok(());
            }
            if claimed.is_none() && start > 0 && !restarted {
                // Bare 416 with no usable Content-Range: confirm the length before deleting anything that might be complete.
                let hreq = stamp_request(
                    ctx.client.head(&ctx.url),
                    DEFAULT_USER_AGENT,
                    ctx.cookies.as_ref(),
                    &ctx.url,
                );
                if let Ok(hresp) = execute_with_timeout(&ctx.client, hreq, ctx.timeout).await
                    && hresp.status().is_success()
                    && hresp.content_length() == Some(start)
                    && !has_holes(&ctx.dest)
                {
                    return Ok(());
                }
            }
            if restarted {
                return Err(gettext("Server rejects range requests"));
            }
            let _ = std::fs::remove_file(&ctx.dest);
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
        // Capture validator once per generation: if we sent If-Range and got
        // 206, the validator already matches — don't overwrite it (a mid-download
        // change would otherwise poison the stored value). On 200 (fresh or
        // If-Range mismatch), capture the new generation's validator.
        let if_range_matched =
            ctx.if_range.get().is_some() && status == reqwest::StatusCode::PARTIAL_CONTENT;
        if !if_range_matched {
            send_validator(&ctx.tx, &resp);
        }
        // Capture the session validator for single-stream retries: the first
        // response sets it (first-wins via OnceLock), and all retries send it
        // as If-Range. This closes the splice hole for fresh single-stream
        // downloads.
        // Set the session validator once (first-wins); ignore if already set.
        if ctx.if_range.get().is_none() {
            response_validator(&resp).map(|v| ctx.if_range.set(v));
        }
        let partial = start > 0 && status == reqwest::StatusCode::PARTIAL_CONTENT;
        if claim && start > 0 && !partial {
            return Err(DEST_EXISTS.to_string());
        }
        // A 206 must carry a Content-Range starting at our offset: a proxy or
        // server answering for the wrong offset would otherwise be appended at
        // the wrong position, silently corrupting the file. The segmented path
        // (`fetch_piece`) already validates this; match it here.
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
        // A smaller full restart than our bytes is a different file (login wall, throttle page): keep the partial bytes.
        if !partial
            && start > 0
            && let Some(l) = resp.content_length()
            && l < start
        {
            return Err(gettext("Server restarted the download with a smaller file"));
        }
        let mut file = if partial {
            open_nofollow(tokio::fs::OpenOptions::new().append(true), &ctx.dest)
                .await
                .map_err(|e| format!("Cannot write file: {e}"))?
        } else if claim {
            // Retries truncate our own bytes (see the `claim` parameter).
            match tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&ctx.dest)
                .await
            {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Err(DEST_EXISTS.to_string());
                }
                Err(e) => return Err(format!("Cannot write file: {e}")),
            }
        } else {
            open_nofollow(
                tokio::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true),
                &ctx.dest,
            )
            .await
            .map_err(|e| format!("Cannot write file: {e}"))?
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
        // Empty chunks never end a chunked stream: a run of them is a stalled connection, not data.
        let mut empty_streak: u32 = 0;
        use tokio::io::AsyncWriteExt as _;
        loop {
            let chunk = match tokio::time::timeout(ctx.timeout, stream.next()).await {
                Ok(Some(Ok(c))) => c,
                Ok(Some(Err(e))) => return Err(format!("Download interrupted: {e}")),
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
                .map_err(|e| format!("Cannot write file: {e}"))?;
            downloaded += chunk.len() as u64;
            pace_chunk(&mut paced, pace_start, rate, chunk.len()).await;
            // ~20fps row updates.
            if last_sent.elapsed() >= Duration::from_millis(50) {
                rate = live_rate_limit();
                ctx.tx.send(progress_msg(downloaded, total)).ok();
                last_sent = Instant::now();
            }
        }
        file.flush()
            .await
            .map_err(|e| format!("Cannot write file: {e}"))?;
        return match total {
            Some(t) if downloaded != t => Err(gettext("Incomplete download")),
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
        && let Ok(f) = std::fs::OpenOptions::new().write(true).open(path)
    {
        let _ = f.set_len(prefix);
    }
}

/// Open for writing without truncating (truncating here would corrupt resumed pieces the bitmap claims as done).
/// Open a file refusing to follow symlinks: a link swapped in between attempts
/// must not divert a resume-append or retry-truncate onto an arbitrary file.
/// Returns ELOOP if the final component is a symlink.
async fn open_nofollow(
    options: &mut tokio::fs::OpenOptions,
    path: &std::path::Path,
) -> std::io::Result<tokio::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    options.custom_flags(libc::O_NOFOLLOW).open(path).await
}

async fn ensure_sized(dest: &std::path::Path, total: u64) -> Result<(), AttemptFail> {
    let file = open_nofollow(tokio::fs::OpenOptions::new().write(true).create(true), dest)
        .await
        .map_err(|e| AttemptFail::Retryable(format!("Cannot write file: {e}")))?;
    if file.metadata().await.map(|m| m.len()).unwrap_or(u64::MAX) != total
        && let Err(e) = file.set_len(total).await
    {
        use std::io::ErrorKind::{FileTooLarge, StorageFull};
        // No room for full-size staging: downgrade to single-stream (which preallocates nothing).
        let msg = format!("Cannot write file: {e}");
        let storage = matches!(e.kind(), StorageFull | FileTooLarge);
        return Err(if storage {
            AttemptFail::Throttled(msg)
        } else {
            AttemptFail::Retryable(msg)
        });
    }
    Ok(())
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

/// Stamp one outbound request like a browser (UA, cookies); single choke point for all requests.
/// Note: no Referer is sent. The client builder disables reqwest's automatic
/// Referer (it leaks URLs/tokens), and a manually-stamped Referer would persist
/// across redirects (reqwest doesn't strip custom headers), leaking the origin
/// to cross-origin targets. Hotlink guards requiring Referer are not supported.
pub(crate) fn stamp_request(
    mut req: reqwest::RequestBuilder,
    user_agent: &str,
    cookies: Option<&std::sync::Arc<reqwest::cookie::Jar>>,
    url: &str,
) -> reqwest::RequestBuilder {
    if !user_agent.trim().is_empty() {
        req = req.header("User-Agent", user_agent.trim());
    }
    if let Some(jar) = cookies
        && let Some(cookie) = crate::cookies::cookie_header_for(jar, url)
    {
        req = req.header("Cookie", cookie);
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
    cookies: Option<&std::sync::Arc<reqwest::cookie::Jar>>,
    timeout: Duration,
) -> Result<(u64, crate::engine_msg::ValidatorInfo), String> {
    let req = stamp_request(
        client.get(url).header("Range", "bytes=0-0"),
        DEFAULT_USER_AGENT,
        cookies,
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
    let total = parse_content_range(value.unwrap_or_default())
        .filter(|(s, _, _)| *s == 0)
        .map(|(_, _, t)| t)
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

/// Parse `Content-Range: bytes <start>-<end>/<total>` (a wrong range pins all chunks to one file version).
pub(crate) fn parse_content_range(value: &str) -> Option<(u64, u64, u64)> {
    let (range, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let (start, end, total) = (
        start.parse::<u64>().ok()?,
        end.parse::<u64>().ok()?,
        total.parse::<u64>().ok()?,
    );
    (total > 0 && end >= start).then_some((start, end, total))
}

/// Total size for a response (`None` only when neither header says; then EOF is the only signal).
pub(crate) fn response_total(
    content_length: Option<u64>,
    content_range: Option<&str>,
    partial: bool,
    start: u64,
) -> Option<u64> {
    content_length
        .map(|t| if partial { t + start } else { t })
        .or_else(|| {
            if !partial {
                return None;
            }
            content_range
                .and_then(parse_content_range)
                .filter(|(s, _, _)| *s == start)
                .map(|(_, _, t)| t)
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
    content_range
        .and_then(parse_content_range)
        .is_some_and(|(s, _, t)| s == start && expected_total.is_none_or(|e| e == t))
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
        let req = stamp_request(builder, DEFAULT_USER_AGENT, ctx.cookies.as_ref(), &ctx.url);
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
            let mismatch = response_validator(&resp)
                .map(|rv| rv != *session_validator)
                .unwrap_or(false);
            if mismatch {
                return Err(Changed(gettext("File changed on server")));
            }
        }
        let cr = resp
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok());
        match parse_content_range(cr.unwrap_or_default()) {
            Some((s, e, t)) if s == start && e == end && t == total => {}
            Some((_, _, t)) if t != total => {
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
