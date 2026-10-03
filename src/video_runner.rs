//! Attempt orchestration: resolve, download legs, merge, live capture and HLS extraction.
//! Top of the video cluster: the engine drives `run_video_download` through the `video` facade.

use crate::attempt_gate::AttemptGate;
use crate::file_names::is_url_derived_name;
use crate::runtime::lock_recover;
use crate::video_argv::{
    VideoJob, apply_proxy_env, container_truth_name, fallback_to_live_edge, hls_download_argv,
    live_capture_argv, live_from_start_unsupported, live_remux_argv, merge_output_ext,
    playlist_scope_args, proxy_cli_args, unified_download_argv, unified_format_spec,
    unified_output_template, write_manifest,
};
use crate::video_plan::{StreamPlan, plan_streams};
use crate::video_probe::page_host;
use crate::video_progress::{
    PROGRESS_GRANULARITY, estimate_collapsed, grid_needs_rebuild, is_ytdlp_merge_line,
    last_error_line, last_log_line, leg_changed, parse_ytdlp_after_move, parse_ytdlp_template,
    piece_marks, trace_format_lines,
};
use crate::video_quality::default_video_filename;
use crate::video_spawn::{
    LiveScratchGuard, ProcessGroupGuard, discover_ytdlp_output, drain_stderr_to_tail,
    fetch_video_page, join_drain, reap_child, spawn_piped_ytdlp, ytdlp_command,
};
use crate::video_staging::{
    ResumePlan, ResumeQuery, VideoManifest, clean_dest_parts, clean_staging_files, collect_sidecar,
    dest_part_path, discover_unified_output, ensure_staging_dir_in, file_len, part_path,
    read_manifest, release_remux_lease, reserve_remux_temp, resume_plan, sidecar_path_for,
    staging_location_for_dest, sweep_partial_remuxes, sweep_staging_preserving_recordings,
};
use crate::video_tools::{
    VideoError, ensure_tool_versions, resolve_libraries, ytdlp_identity_args,
};
use crate::video_types::{FetchedVideo, VideoOutcome};
use gettextrs::gettext;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::oneshot;
use yt_dlp::model::Video;

/// What a stop means for the attempt it reaches. The worker applies the intent (only it knows whether it is capturing live); a bare oneshot could not say *how* to stop, so removal once delivered a file with no row behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopIntent {
    /// Stop and keep what is recorded (Stop, pause, cancel); a live capture adopts its partial and delivers it.
    Preserve,
    /// Stop and throw it away (row removal): reap the recorder and return without adopting, remuxing or delivering; scratch stays for the manager to reclaim after the task returns.
    Discard,
}

/// Run one attempt: resolve → download parts → merge → rename into place.
/// Returns the final size, or `None` when aborted (the pauser/canceller already set the row status).
///
/// # Errors
/// Returns a display-ready [`VideoError`]; the caller reports it as Failed.
pub async fn run_video_download(
    mut job: VideoJob,
    gate: &std::sync::Arc<AttemptGate>,
    mut abort: oneshot::Receiver<StopIntent>,
    tx: tokio::sync::mpsc::UnboundedSender<crate::engine_msg::EngineMsg>,
) -> Result<VideoOutcome, VideoError> {
    use crate::engine_msg::EngineMsg;

    // Stage beside the destination (same filesystem: atomic delivery, no tmpfs
    // pressure). Legacy tmp dirs for rows staged before the move resolve here
    // too, so a paused row keeps its resume data across the upgrade.
    let loc = staging_location_for_dest(&job.dest, job.item_id);
    // Keep the canonical path: `discover_unified_output` compares a canonicalized `after_move` against it (symlinked roots would otherwise fail closed).
    let staging = ensure_staging_dir_in(&loc.root, &loc.dir)?;
    let libs = resolve_libraries()?;
    let (yt_version, ff_version) = ensure_tool_versions(&libs).await?;
    // quickjs-ng is the JS runtime Grab pins for YouTube; make sure it's
    // installed before a YouTube spawn that may need to solve JS challenges.
    crate::video_tools::ensure_quickjs(&job.page_url).await?;
    tracing::debug!(
        item_id = job.item_id,
        host = %page_host(&job.page_url),
        quality = %job.quality,
        yt_dlp = %yt_version,
        ffmpeg = %ff_version,
        "starting video attempt"
    );
    // Stall timeout: a full-length merge on a slow CPU dwarfs any network timeout, so allow five minutes of silence, but a progressing download never trips it.
    let timeout = Duration::from_secs(300);
    let youtube_bin = libs.youtube.clone();
    let ffmpeg_bin = libs.ffmpeg.clone();
    let phase = |text: String| {
        tx.send(EngineMsg::Phase(text)).ok();
    };

    // Resolve with retries, always fresh: no cache backend, so expired format URLs never survive a retry. A staging manifest labels the re-resolve as a resume.
    let resuming = read_manifest(&staging, job.item_id).is_some();
    phase(if resuming {
        gettext("Resuming download…")
    } else if job.audio_only {
        gettext("Resolving audio…")
    } else {
        gettext("Resolving media…")
    });
    let mut video: Option<Video> = None;
    let mut playlist_index: Option<usize> = None;
    for attempt in 0u32..3 {
        match fetch_video_page(
            &youtube_bin,
            &job.page_url,
            &job.cookies_browser,
            Duration::from_secs(300),
            job.proxy.as_ref(),
            job.playlist_item_id.as_deref(),
        )
        .await
        {
            Ok(FetchedVideo::Single {
                video: v,
                playlist_index: index,
            }) => {
                video = Some(*v);
                playlist_index = index;
                break;
            }
            // No picked entry: the spawner expands the collection into per-item rows instead of failing it.
            Ok(FetchedVideo::Playlist(pl)) => return Ok(VideoOutcome::Expand(pl.into())),
            Err(e) if attempt + 1 < 3 => {
                tracing::info!("video resolve failed, retrying: {e}");
                tokio::time::sleep(Duration::from_secs(u64::from(attempt) + 1)).await;
            }
            Err(e) => return Err(e),
        }
    }
    let Some(video) = video else {
        return Err(VideoError::fetch("empty response"));
    };

    // Dialog-less rows skip the picker, so refresh live-ness from resolve metadata: without this a live row takes the HLS VOD path and never reaches capture. Flipping the job field keeps every downstream use consistent.
    if !job.is_live && video.is_live.unwrap_or(false) {
        job.is_live = true;
        tx.send(EngineMsg::LiveDetected).ok();
    }

    // Dialog-less rows have URL-stem names ("watch"): rename to the title default now that metadata is in. Only URL-derived names qualify; the pump dedupes at Finished.
    if let Some(current) = job.dest.file_name().and_then(|n| n.to_str())
        && is_url_derived_name(current, &job.page_url)
    {
        // Live rows capture through the dest name with a hardcoded mp4/m4a container: never suggest a remux extension there.
        let remux = if job.is_live {
            None
        } else {
            job.remux_video.as_deref()
        };
        let better = default_video_filename(&video.title, job.audio_only, remux);
        if better != current {
            tx.send(EngineMsg::SuggestName(better)).ok();
        }
    }

    // Select streams: newest codec first, best audio; older codecs stay as automatic fallback. Rejections degrade to absent here; the plan below decides between split, single-file and HLS. A pinned format id wins, falling back to the preset when it vanishes.
    let StreamPlan {
        video_sel,
        audio_sel,
        hls_sel,
    } = plan_streams(
        &video,
        &job.quality,
        job.audio_only,
        job.video_format_id.as_deref(),
        job.newest_codecs,
        job.item_id,
    );
    if let Some(hls) = hls_sel {
        // An abort during resolve is stop-before-start: for live rows nobody waits on a message, so report instead of going quiet (the pump tail would fail the row either way).
        if job.is_live && abort.try_recv().is_ok() {
            return Err(VideoError::interrupted());
        }
        tracing::debug!(
            item_id = job.item_id,
            page_host = %page_host(&job.page_url),
            height = ?hls.height,
            "downloading HLS variant",
        );
        // Live captures go through yt-dlp with a kill-safe MPEG-TS container (Stop is kill + adopt + remux); VOD captures use yt-dlp's standard HLS path.
        if job.is_live {
            // The extractor's canonical URL, falling back to the row's URL exactly as `VideoInfo::from` does, so all legs stamp the same provenance.
            let canonical = video
                .webpage_url
                .as_deref()
                .filter(|u| !u.is_empty())
                .unwrap_or(&job.page_url);
            return run_live_ytdlp(
                &youtube_bin,
                &ffmpeg_bin,
                &staging,
                &job,
                gate,
                canonical,
                &hls.format_id,
                abort,
                timeout,
                tx,
                playlist_index,
            )
            .await
            .map(|opt| opt.map_or(VideoOutcome::Aborted, VideoOutcome::Finished));
        }
        return run_hls_ytdlp(
            &youtube_bin,
            &ffmpeg_bin,
            &staging,
            &job,
            gate,
            &hls.format_id,
            abort,
            timeout,
            tx,
            playlist_index,
        )
        .await
        .map(|opt| opt.map_or(VideoOutcome::Aborted, VideoOutcome::Finished));
    }
    let Some(audio_sel) = audio_sel else {
        return Err(VideoError::unavailable_detail(&video.formats));
    };

    tracing::debug!(
        item_id = job.item_id,
        video = ?video_sel.as_ref().map(|s| s.format_id.as_str()),
        audio = %audio_sel.format_id,
        "formats selected"
    );

    // Retry discipline from the sidecar. Parts live beside the finished file, so every file check builds off the destination.
    let manifest = read_manifest(&staging, job.item_id);
    // Split rows merge video+audio (both sizes known or neither trusted); an adopted single's extractor size is the whole file. Anything else leaves the total unknown rather than understating it.
    let single = video_sel.is_none();
    let query = ResumeQuery {
        manifest: manifest.as_ref(),
        dest: &job.dest,
        staging: &staging,
        page_url: &job.page_url,
        quality: &job.quality,
        video: video_sel
            .as_ref()
            .map(|s| (s.format_id.as_str(), s.ext.as_str())),
        audio: (audio_sel.format_id.as_str(), audio_sel.ext.as_str()),
        total: if single {
            audio_sel.size
        } else {
            match (video_sel.as_ref().and_then(|s| s.size), audio_sel.size) {
                (Some(v), Some(a)) => Some(v + a),
                _ => None,
            }
        },
    };
    let plan = resume_plan(&query);
    match plan {
        ResumePlan::Finished => {
            return Ok(match file_len(&job.dest) {
                Some(n) => VideoOutcome::Finished(n),
                None => VideoOutcome::Aborted,
            });
        }
        ResumePlan::Fresh => {
            // Overwrite pre-flight (Parabolic parity): a finished file at `dest` means the atomic claim fails at the end, so refuse before a wasted download. This arm's cleanup drops our own shells too.
            if job.dest.exists() {
                clean_dest_parts(&job.dest);
                return Err(VideoError::exists());
            }
            // Clear this item's staging files (grab-<id>-*), not the dest dir itself: a previous attempt's detached writers may still hold old inodes, so unlink first. Only mismatches/oversize leftovers land here; same-selection resume never does.
            crate::video::clean_staging_files(&staging, job.item_id);
            // Dest-dir parts are Grab-namespaced, so a mismatch restarts clean instead of resuming into a foreign lookalike. The finished file itself is never touched.
            clean_dest_parts(&job.dest);
            // Record this attempt's selection up front: a pause from here on leaves a matchable sidecar, so the next attempt resumes instead of wiping.
            write_manifest(
                &staging,
                job.item_id,
                &VideoManifest {
                    page_url: job.page_url.clone(),
                    quality: job.quality.clone(),
                    video_format_id: video_sel.as_ref().map(|s| s.format_id.clone()),
                    video_ext: video_sel
                        .as_ref()
                        .map(|s| s.ext.clone())
                        .unwrap_or_default(),
                    audio_format_id: audio_sel.format_id.clone(),
                    audio_ext: audio_sel.ext.clone(),
                    final_bytes: None,
                },
            )
            .await?;
        }
        ResumePlan::Resume => {
            // Overwrite pre-flight, same as Fresh: anything at `dest` is foreign or stale, so refuse before a wasted download.
            if job.dest.exists() {
                clean_dest_parts(&job.dest);
                return Err(VideoError::exists());
            }
            phase(gettext("Resuming download…"));
        }
    }
    // One yt-dlp invocation downloads (and merges) the whole selection; the `-f` spec carries the planner pair plus the preset fallback, so yt-dlp itself retries stale ids.
    let (spec, _merging) = unified_format_spec(
        video_sel.as_ref().map(|s| s.format_id.as_str()),
        audio_sel.format_id.as_str(),
        &job.quality,
        job.audio_only,
    );
    let combined_total = query.total;
    // NOTE: Grab's speed limit applies to VOD legs via `--ratelimit`. Live capture runs unthrottled: capping an endless stream would fall behind the edge.
    run_unified_ytdlp(
        &youtube_bin,
        &ffmpeg_bin,
        &staging,
        &job,
        gate,
        &spec,
        video_sel.as_ref().map(|s| s.ext.as_str()),
        combined_total,
        &mut abort,
        timeout,
        tx,
        playlist_index,
    )
    .await
    .map(|opt| opt.map_or(VideoOutcome::Aborted, VideoOutcome::Finished))
}

/// One direct download through a single yt-dlp invocation: Grab claims the output into place (EXDEV-safe, no clobber) and wipes staging.
/// Returns the finished size, or `None` on user abort (the caller stays quiet).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_unified_ytdlp(
    youtube_bin: &Path,
    ffmpeg_bin: &Path,
    staging: &Path,
    job: &VideoJob,
    gate: &std::sync::Arc<AttemptGate>,
    spec: &str,
    video_ext: Option<&str>,
    total: Option<u64>,
    abort: &mut oneshot::Receiver<StopIntent>,
    timeout: Duration,
    tx: tokio::sync::mpsc::UnboundedSender<crate::engine_msg::EngineMsg>,
    playlist_index: Option<usize>,
) -> Result<Option<u64>, VideoError> {
    use crate::engine_msg::EngineMsg;
    // Split rows merge; adopted singles download one file, nothing to merge.
    let merging = video_ext.is_some();
    let merge_ext = video_ext.map(merge_output_ext).unwrap_or_default();
    let out_template = unified_output_template(staging);
    // Resolve the subtitle language against what the video actually offers
    // (preferred, else English, else none) before the media argv is built.
    // An abort here stops the download; a probe failure just drops subtitles.
    let mut job = job.clone();
    job.subtitles = match resolve_subtitle_lang(youtube_bin, &job, abort, playlist_index).await {
        Ok(lang) => lang,
        Err(()) => return Ok(None),
    };
    let argv = unified_download_argv(
        &job,
        spec,
        merging,
        &merge_ext,
        ffmpeg_bin,
        &out_template,
        playlist_index,
    );
    // Single-counter progress with the pump's granularity gate; `done` is capped against the metadata total so the bar never passes 100%.
    let sent = Arc::new(AtomicU64::new(0));
    let report = {
        let sent = Arc::clone(&sent);
        let tx = tx.clone();
        Arc::new(move |done: u64, _: u64| {
            let shown = match total {
                Some(t) if t > 0 => done.min(t),
                _ => done,
            };
            let prev = sent.load(Ordering::Relaxed);
            if shown.saturating_sub(prev) >= PROGRESS_GRANULARITY
                || total.is_some_and(|t| t > 0 && shown >= t)
            {
                sent.store(shown, Ordering::Relaxed);
                tx.send(EngineMsg::Progress {
                    downloaded: shown,
                    total,
                    uploaded: 0,
                    upload_bps: 0,
                })
                .ok();
            }
        })
    };
    let (done, after_move) = run_ytdlp_attempt(
        youtube_bin,
        &argv,
        report,
        Some(Arc::new({
            let tx = tx.clone();
            move || {
                tx.send(EngineMsg::Phase(gettext("Merging…"))).ok();
            }
        })),
        job.proxy.as_ref(),
        abort,
        timeout,
    )
    .await?;
    let Some(()) = done else {
        return Ok(None);
    };
    let final_tmp = discover_unified_output(staging, after_move.as_deref());
    let Some(final_tmp) = final_tmp else {
        return Err(VideoError::part_failed("no output file produced"));
    };
    // Measure reality, not the plan: a zero-byte "completed" download must fail now, or the row sits Done and empty forever.
    if file_len(&final_tmp) == Some(0) {
        return Err(VideoError::part_failed("empty stream"));
    }
    // Container-truth backstop: the intake name assumes mp4 (or the remux target), so a native webm merge under a stale extension claims under the truer name. Same stem, so stem reservations hold; the pump dedupes at Finished.
    if let Some(truer) = container_truth_name(&job.dest, &final_tmp) {
        tx.send(EngineMsg::SuggestName(truer)).ok();
    }
    // Atomic claim into place (EXDEV-safe, no clobber). The linearization point: either this wins and the row is still here, or the removal already won.
    if !gate.try_commit() {
        // Lost the race: clear this item's staging files only, never the dest dir.
        clean_staging_files(staging, job.item_id);
        return Ok(None);
    }
    match crate::file_names::rename_noreplace(&final_tmp, &job.dest) {
        Ok(()) => {
            gate.mark_delivered();
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(VideoError::exists());
        }
        Err(e) => return Err(VideoError::combine(&e)),
    }
    // Best-effort subtitle sidecar beside the discovered output (video rows only; skipped when embedding, since tracks are muxed in).
    if !job.audio_only
        && !job.embed_subs
        && let Some(lang) = job.subtitles.as_deref()
    {
        collect_sidecar(&sidecar_path_for(&final_tmp, lang), &job.dest, lang);
    }
    // Sweep legacy dest-dir parts, so pre-migration rows (or foreign lookalikes the Fresh arm never saw) don't sit beside the finished file forever.
    clean_dest_parts(&job.dest);
    // Record the finished size so a later retry adopts the file.
    let final_bytes = file_len(&job.dest);
    if let Some(mut m) = read_manifest(staging, job.item_id) {
        m.final_bytes = final_bytes;
        let _ = write_manifest(staging, job.item_id, &m).await;
    }
    sweep_staging_preserving_recordings(staging, job.item_id);
    // Staging is the dest dir itself: never remove it or its parent.
    Ok(Some(final_bytes.unwrap_or(0)))
}

/// Time since yt-dlp last wrote a stdout line: the stall watchdog's clock.
fn stall_elapsed(last_progress: &std::sync::Mutex<std::time::Instant>) -> std::time::Duration {
    lock_recover(last_progress).elapsed()
}

/// Wait for the child: bounded by the stall deadline while downloading, and by
/// a generous wall-clock once the merge starts. yt-dlp captures ffmpeg's
/// output instead of forwarding it, so a merge is silent on both streams — a
/// stall deadline there would kill legitimate long merges, but a truly hung
/// ffmpeg must not park the row forever either.
pub(crate) async fn await_child(
    child: &mut tokio::process::Child,
    merging: bool,
    remaining: Duration,
    merge_timeout: Duration,
) -> Result<Result<std::process::ExitStatus, std::io::Error>, tokio::time::error::Elapsed> {
    if merging {
        tokio::time::timeout(merge_timeout, child.wait()).await
    } else {
        tokio::time::timeout(remaining, child.wait()).await
    }
}

/// One yt-dlp spawn: parse template progress, collect the log tail, capture `--print after_move:filepath`. `Ok((None, _))` is a user abort (the caller stays quiet). `on_merge` fires once on the first merge line. `timeout` is a stall budget, not a wall clock: any stdout line resets it, so only silence kills the attempt. The stall deadline gives way to a generous merge wall-clock once the merge starts (see `await_child`).
#[allow(clippy::too_many_arguments)]
async fn run_ytdlp_attempt(
    youtube_bin: &Path,
    argv: &[String],
    report: std::sync::Arc<dyn Fn(u64, u64) + Send + Sync>,
    on_merge: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    proxy: Option<&crate::net_types::ResolvedProxy>,
    abort: &mut oneshot::Receiver<StopIntent>,
    timeout: Duration,
) -> Result<(Option<()>, Option<String>), VideoError> {
    use tokio::io::AsyncBufReadExt as _;
    let mut cmd = ytdlp_command(youtube_bin);
    cmd.args(argv);
    apply_proxy_env(&mut cmd, proxy);
    let (mut child, stdout, stderr) = spawn_piped_ytdlp(cmd)?;
    let mut group = ProcessGroupGuard::new(&child);
    // Stall watchdog, not a wall clock: any stdout line proves yt-dlp is alive, so a progressing download never trips the timeout: only silence does. The merge phase uses a generous wall-clock instead of the stall deadline (see `await_child`).
    let last_progress = std::sync::Arc::new(std::sync::Mutex::new(std::time::Instant::now()));
    let last_progress_p = last_progress.clone();
    // Shared with the watchdog loop: once the merge starts the stall deadline
    // no longer applies (see `await_child`).
    let merging = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let merging_p = merging.clone();
    let progress = tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        // `downloaded_bytes` resets per leg, so bank each leg's max on its `finished` line and report the running sum; the caller caps against its metadata total.
        let (mut banked, mut leg_max, mut total) = (0u64, 0u64, 0u64);
        let mut after_move = None::<String>;
        let mut merged = false;
        while let Ok(Some(line)) = lines.next_line().await {
            *lock_recover(&last_progress_p) = std::time::Instant::now();
            if !merged && is_ytdlp_merge_line(&line) {
                merged = true;
                merging_p.store(true, std::sync::atomic::Ordering::SeqCst);
                if let Some(cb) = on_merge.as_ref() {
                    cb();
                }
            }
            if after_move.is_none()
                && let Some(path) = parse_ytdlp_after_move(&line)
            {
                after_move = Some(path.to_string());
            }
            if let Some(p) = parse_ytdlp_template(&line) {
                if p.finished {
                    banked += p.downloaded.unwrap_or(0).max(leg_max);
                    leg_max = 0;
                } else {
                    if let Some(d) = p.downloaded {
                        leg_max = leg_max.max(d);
                    }
                    if let Some(t) = p.total {
                        total = total.max(t);
                    }
                    if total > 0 {
                        report(banked + leg_max, total);
                    }
                }
            }
        }
        after_move
    });
    let logs = tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(stderr);
        let mut tail = Vec::new();
        let mut pending = String::new();
        let mut buf = [0u8; 4096];
        loop {
            use tokio::io::AsyncReadExt as _;
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    trace_format_lines(&mut pending, &buf[..n]);
                    tail.extend_from_slice(&buf[..n]);
                    if tail.len() > 8192 {
                        tail.drain(..tail.len() - 8192);
                    }
                }
            }
        }
        String::from_utf8_lossy(&tail).into_owned()
    });
    let status = loop {
        // Each wait runs only until the stall deadline; a progress line pushes the deadline out, so a progressing download is never killed. Once merging, `await_child` swaps the stall deadline for the merge wall-clock. The snapshot only
        // sizes this wait; the kill decision re-reads the live flag.
        let merging_snapshot = merging.load(std::sync::atomic::Ordering::SeqCst);
        let remaining = timeout.saturating_sub(stall_elapsed(&last_progress));
        tokio::select! {
            biased;
            _ = &mut *abort => {
                reap_child(&mut child, &mut group).await;
                progress.abort();
                logs.abort();
                return Ok((None, None));
            }
            waited = await_child(&mut child, merging_snapshot, remaining, MERGE_WALL_CLOCK) => match waited {
                Ok(Ok(status)) => {
                    // The leader is reaped, so release the PGID: holding it across the drain joins would risk the OS recycling it onto another group.
                    group.disarm();
                    break status;
                }
                Ok(Err(e)) => {
                    reap_child(&mut child, &mut group).await;
                    progress.abort();
                    logs.abort();
                    return Err(VideoError::runtime(&e));
                }
                // The merge marker can land while `await_child` is parked on a
                // pre-merge snapshot: re-read the live flag before killing, or a
                // silent merge dies exactly one budget after its marker line.
                Err(_)
                    if stall_elapsed(&last_progress) >= timeout
                        && !merging.load(std::sync::atomic::Ordering::SeqCst) =>
                {
                    reap_child(&mut child, &mut group).await;
                    progress.abort();
                    logs.abort();
                    return Err(VideoError::part_failed("stalled"));
                }
                // The merge wall-clock expired while a merge was genuinely
                // underway (the wait started after the merge marker and the
                // live flag still agrees): reap the hung merge and fail the
                // part instead of parking the row forever. A merge that
                // finished in the same instant still counts.
                Err(_)
                    if merging_snapshot
                        && merging.load(std::sync::atomic::Ordering::SeqCst) =>
                {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            group.disarm();
                            break status;
                        }
                        _ => {
                            reap_child(&mut child, &mut group).await;
                            progress.abort();
                            logs.abort();
                            return Err(VideoError::part_failed("merge timed out"));
                        }
                    }
                }
                // Progress landed mid-wait, or the merge started while parked:
                // loop back and re-arm (the next wait uses the merge wall-clock
                // once merging).
                Err(_) => {}
            },
        }
    };
    let after_move = progress.await.unwrap_or_default();
    let log_tail = logs.await.unwrap_or_default();
    if !status.success() {
        let detail = last_log_line(&log_tail, "yt-dlp reported failure");
        return Err(VideoError::part_failed(detail));
    }
    Ok((Some(()), after_move))
}

/// Remux a stopped live capture into place. ffmpeg writes `<dest>.part` and the result is renamed to `dest` only on success: a bare `final.<n>.<ext>` is always worth keeping, a `.part` never is.
pub(crate) async fn remux_live_capture(
    ffmpeg_bin: &Path,
    ts_path: &Path,
    dest: &Path,
    audio_only: bool,
    timeout: Duration,
    page_url: &str,
) -> Result<(), VideoError> {
    let mut partial = dest.as_os_str().to_os_string();
    partial.push(".part");
    let partial = PathBuf::from(partial);
    // Two attempts: with the bsf, then bare. The loop always exits via break.
    let mut with_bsf = true;
    loop {
        // ffmpeg runs without `-y` and refuses an existing output, so the bare retry must not inherit the first attempt's partial.
        let _ = tokio::fs::remove_file(&partial).await;
        let mut cmd = tokio::process::Command::new(ffmpeg_bin);
        cmd.args(live_remux_argv(
            ts_path, &partial, audio_only, with_bsf, page_url,
        ));
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        #[cfg(unix)]
        {
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().map_err(VideoError::runtime)?;
        let mut group = ProcessGroupGuard::new(&child);
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| VideoError::runtime("ffmpeg gave no log pipe"))?;
        let logs = drain_stderr_to_tail(stderr);
        let status = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) => {
                // The leader is reaped, so release the PGID: holding it across the drain joins would risk the OS recycling it onto another group.
                group.disarm();
                status
            }
            Ok(Err(e)) => {
                reap_child(&mut child, &mut group).await;
                join_drain(logs).await;
                let _ = tokio::fs::remove_file(&partial).await;
                return Err(VideoError::runtime(&e));
            }
            Err(_) => {
                reap_child(&mut child, &mut group).await;
                join_drain(logs).await;
                let _ = tokio::fs::remove_file(&partial).await;
                return Err(VideoError::part_failed("timed out finalizing"));
            }
        };
        let log_tail = join_drain(logs).await.unwrap_or_default();
        if status.success() {
            // Only now is this a recording: a crash before this point leaves a `.part`, which no sweep has to protect.
            break match tokio::fs::rename(&partial, dest).await {
                Ok(()) => Ok(()),
                Err(e) => {
                    let _ = tokio::fs::remove_file(&partial).await;
                    Err(VideoError::runtime(&e))
                }
            };
        }
        let detail = last_log_line(&log_tail, "ffmpeg reported failure");
        if with_bsf {
            tracing::debug!(error = %detail, "live remux without bsf, retrying bare");
            with_bsf = false;
            continue;
        }
        let _ = tokio::fs::remove_file(&partial).await;
        break Err(VideoError::combine(detail));
    }
}

/// Why a live capture ended, which decides what scratch is redundant. Raw media and the staging dir are tracked separately: conflating them deletes a finished recording on the one exit where both are the user's only copy.
#[derive(Clone, Copy)]
enum Exit {
    /// The remuxed file was claimed at dest: shell and emptied staging dir are both redundant.
    Delivered,
    /// Dest was claimed mid-capture; the row fails Parabolic-style, so shell and remux temp are both redundant.
    DestClaimed,
    /// Reaping the recorder failed, so no remux was attempted: the raw shell may hold bytes the user wants.
    CaptureWaitFailed,
    /// The remux failed, so the raw shell is the only usable copy of the capture.
    RemuxFailed,
    /// The recorder exited nonzero on its own: the raw shell may hold a partial
    /// capture worth salvaging, but the row fails instead of adopting it as finished.
    RecorderFailed,
    /// The rename failed unexpectedly: keep both the raw shell and this attempt's completed remux.
    RenameFailed,
    /// Nothing was recorded, so there is nothing to salvage.
    NothingRecorded,
    /// The row was removed mid-finalize: discard both the completed remux and the raw shell (no row left to own them).
    Discarded,
}

/// What a terminal exit does with the row's staging directory.
#[derive(Clone, Copy)]
enum Staging {
    /// Remove this attempt's own remux temp, then drop the dir only if empty. Never recursive: a sibling temp is an earlier attempt's completed recording.
    Sweep,
    /// Leave this attempt's temp in place: it is the completed recording the final rename could not place.
    Keep,
}

/// Salvage exits keep the raw shell inside the hidden staging dir: name it in
/// the failure message so the recording stays findable.
fn salvage_note(staging: &Path) -> String {
    format!(
        " {}",
        gettext("(recording kept in {dir})").replace("{dir}", &staging.display().to_string())
    )
}

/// Reclaim one live capture's scratch on any terminal exit. The `.ytdl` state file is always removed: a killed capture never cleans it, and a stale one would resume fragment N against a wiped shell (corrupt recording). Staging is never swept recursively: a sibling temp is an earlier attempt's completed recording. Best-effort throughout: a sweep racing a vanished file is a no-op, never worth failing a row over.
async fn sweep_live_capture(
    out: &Path,
    part: &Path,
    state: &Path,
    _staging: &Path,
    final_tmp: Option<&Path>,
    staging_mode: Staging,
    exit: Exit,
) {
    if matches!(
        exit,
        Exit::Delivered | Exit::DestClaimed | Exit::NothingRecorded
    ) {
        let _ = tokio::fs::remove_file(out).await;
        let _ = tokio::fs::remove_file(part).await;
    }
    let _ = tokio::fs::remove_file(state).await;
    if let Staging::Sweep = staging_mode
        && let Some(path) = final_tmp
    {
        let _ = tokio::fs::remove_file(path).await;
        release_remux_lease(path);
    }
    // Staging is the dest dir itself: never remove it. The item's
    // `grab-<id>-*` files were already swept above; the dir stays.
}

/// Reap the recorder, and *only then* reclaim its scratch. The order is the contract: sweeping first could delete a file the recorder is still writing. Pinned by controlled futures in `video_runner_tests.rs`; the sweep is a closure so it cannot even be constructed before the reap completes.
async fn reap_then_sweep<F, S, G>(reap: F, sweep: S)
where
    F: std::future::Future<Output = ()>,
    S: FnOnce() -> G,
    G: std::future::Future<Output = ()>,
{
    reap.await;
    sweep().await;
}

/// Wall-clock bound for the subtitle language probe: a single info-JSON fetch.
/// Best-effort — on timeout the download simply gets no subtitles.
const SUBTITLE_PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// Bound for draining the probe's stdout after a successful wait: the child's
/// pipe can outlive it (a grandchild inheriting the descriptor), and an
/// unbounded drain would stall the download on an otherwise fine probe.
const SUBTITLE_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Drain the probe's stdout with a deadline; on expiry abort the drain task so
/// it can't linger on a pipe held open by a grandchild, then `None`.
async fn drain_with_timeout(
    mut drain: tokio::task::JoinHandle<Vec<u8>>,
    bound: Duration,
) -> Option<Vec<u8>> {
    match tokio::time::timeout(bound, &mut drain).await {
        Ok(bytes) => Some(bytes.unwrap_or_default()),
        Err(_) => {
            drain.abort();
            let _ = drain.await;
            None
        }
    }
}

/// Wall-clock bound for the merge phase: merges are local ffmpeg work and
/// typically stream-copies finishing in minutes, so an hour is generous even
/// on slow CPUs — while a truly hung ffmpeg no longer parks the row forever.
const MERGE_WALL_CLOCK: Duration = Duration::from_secs(3600);

/// Pick the subtitle language to request from the video's available subtitle
/// languages (lowercase info-JSON `subtitles`/`automatic_captions` keys):
/// the first candidate the video offers, accepting a region variant (`en`
/// matches `en-us`). Returns the concrete offered key so `--sub-langs`
/// matches exactly. Pure.
pub(crate) fn pick_subtitle_lang(
    available: &std::collections::HashSet<String>,
    candidates: &[&str],
) -> Option<String> {
    for cand in candidates {
        if available.contains(*cand) {
            return Some(cand.to_string());
        }
        if let Some(hit) = available.iter().find(|a| {
            a.len() > cand.len()
                && a.starts_with(*cand)
                && matches!(a.as_bytes()[cand.len()], b'-' | b'_')
        }) {
            return Some(hit.clone());
        }
    }
    None
}

/// Available subtitle languages for one video: the lowercase keys of the
/// info-JSON `subtitles` and `automatic_captions` maps. Pure.
fn available_subtitle_langs(info: &serde_json::Value) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for key in ["subtitles", "automatic_captions"] {
        if let Some(map) = info.get(key).and_then(|v| v.as_object()) {
            out.extend(map.keys().map(|k| k.to_ascii_lowercase()));
        }
    }
    out
}

/// Resolve the subtitle language for the media command: the preferred
/// language if the video offers it, else English if offered, else no
/// subtitles. The probe is one best-effort info fetch — any failure
/// (network, HTTP 429, unparsable output) yields `None`, so a subtitle
/// outage can never sink the media download. `Err(())` means the user
/// aborted mid-probe: the caller must stop.
pub(crate) async fn resolve_subtitle_lang(
    youtube_bin: &Path,
    job: &VideoJob,
    abort: &mut oneshot::Receiver<StopIntent>,
    playlist_index: Option<usize>,
) -> Result<Option<String>, ()> {
    let pref = match job.subtitles.as_deref() {
        Some(p) => p.to_ascii_lowercase(),
        None => return Ok(None),
    };
    let mut argv = vec!["--ignore-config".to_string()];
    // A picked row probes the collection URL: scope to its entry, or the
    // subtitles come from the tray's first entry while the media is another.
    argv.extend(playlist_scope_args(playlist_index));
    argv.extend(["--skip-download".to_string(), "--dump-json".to_string()]);
    argv.extend(proxy_cli_args(job.proxy.as_ref()));
    argv.extend(ytdlp_identity_args(
        &job.cookies_browser,
        None,
        &job.page_url,
    ));
    let mut cmd = ytdlp_command(youtube_bin);
    cmd.args(&argv);
    apply_proxy_env(&mut cmd, job.proxy.as_ref());
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            tracing::warn!(error = %e, "subtitle probe couldn't start; downloading without subtitles");
            return Ok(None);
        }
    };
    // Drain stdout concurrently with the wait: the probe's info JSON can
    // exceed the pipe buffer (many subtitle tracks), and a child blocked on
    // a full pipe never exits — waiting first would stall to the timeout and
    // drop the subtitles. Take the pipe before the select (`wait_with_output`
    // moves the child, which `select!` forbids alongside `kill`).
    let stdout = child.stdout.take();
    let drain = tokio::spawn(async move {
        let mut out_bytes = Vec::new();
        if let Some(mut pipe) = stdout {
            use tokio::io::AsyncReadExt as _;
            let _ = pipe.read_to_end(&mut out_bytes).await;
        }
        out_bytes
    });
    let status = tokio::select! {
        biased;
        _ = &mut *abort => {
            drain.abort();
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(());
        }
        res = tokio::time::timeout(SUBTITLE_PROBE_TIMEOUT, child.wait()) => res,
    };
    if !matches!(status, Ok(Ok(s)) if s.success()) {
        drain.abort();
        let _ = child.kill().await;
        let _ = child.wait().await;
        tracing::warn!("subtitle probe failed; downloading without subtitles");
        return Ok(None);
    };
    // The drain is bounded: the child's stdout can stay open after it exits
    // (a grandchild inheriting the pipe), and an unbounded `drain.await`
    // would stall the download on an otherwise successful probe. 5 s is
    // generous — the info JSON is small and fully written by now.
    let out_bytes = match drain_with_timeout(drain, SUBTITLE_DRAIN_TIMEOUT).await {
        Some(bytes) => bytes,
        None => {
            tracing::warn!("subtitle probe drain timed out; downloading without subtitles");
            return Ok(None);
        }
    };
    let info: serde_json::Value = match serde_json::from_slice(&out_bytes) {
        Ok(v) => v,
        Err(_) => {
            tracing::warn!(
                "subtitle probe returned unparsable info; downloading without subtitles"
            );
            return Ok(None);
        }
    };
    let available = available_subtitle_langs(&info);
    let lang = pick_subtitle_lang(&available, &[&pref, "en"]);
    if lang.is_none() {
        tracing::debug!("video offers no subtitles in the preferred language or English");
    }
    Ok(lang)
}

/// File-growth watcher for live captures: announces "Recording…" once the
/// `.part` shell or the final output has bytes (live captures often emit no
/// progress for long stretches). Capped at ~10 min of silence — a dead
/// capture's own timeouts fire instead.
pub(crate) fn spawn_recording_watcher(
    tx: tokio::sync::mpsc::UnboundedSender<crate::engine_msg::EngineMsg>,
    shell: std::path::PathBuf,
    out: std::path::PathBuf,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        for _ in 0..1200 {
            let mut bytes = 0u64;
            for p in [&shell, &out] {
                bytes = bytes.max(tokio::fs::metadata(p).await.map(|m| m.len()).unwrap_or(0));
            }
            if bytes > 0 {
                tx.send(crate::engine_msg::EngineMsg::Phase(gettext("Recording…")))
                    .ok();
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
}

/// Stall signal for live captures: the recorder can go quiet on stdout for
/// long stretches while still writing, so file growth also resets the stall
/// budget. Polls the `.part` shell and the final output; any growth proves
/// the capture is alive. Runs until aborted — owned by a guard per attempt,
/// like the recording watcher.
fn spawn_growth_watcher(
    last_progress: std::sync::Arc<std::sync::Mutex<std::time::Instant>>,
    shell: std::path::PathBuf,
    out: std::path::PathBuf,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut max = 0u64;
        loop {
            let mut bytes = 0u64;
            for p in [&shell, &out] {
                bytes = bytes.max(tokio::fs::metadata(p).await.map(|m| m.len()).unwrap_or(0));
            }
            if bytes > max {
                max = bytes;
                *lock_recover(&last_progress) = std::time::Instant::now();
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    })
}

/// Owns a recording watcher's `JoinHandle` and aborts it on drop: the live
/// retry loop must never leave a watcher behind — a surviving watcher keeps
/// polling for up to ~10 min and double-announces "Recording…" into the
/// retry. Drop covers every attempt exit (`continue`, `break`, `return`).
/// The handle stays private: every watcher must be owned by the guard, so the
/// abort-on-drop invariant can't be bypassed by holding the raw `JoinHandle`.
pub(crate) struct RecordingWatcherGuard(tokio::task::JoinHandle<()>);

impl RecordingWatcherGuard {
    pub(crate) fn new(handle: tokio::task::JoinHandle<()>) -> Self {
        Self(handle)
    }
}

impl Drop for RecordingWatcherGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// One live capture through the yt-dlp binary. The MPEG-TS container keeps every kill point playable, so Stop is kill, adopt and remux. Stalled captures yield their partial; an empty capture fails. `timeout` is a stall budget, not a wall clock: any stdout line or output-file growth resets it, so a healthy multi-hour stream never trips it — only silence kills the capture.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_live_ytdlp(
    youtube_bin: &Path,
    ffmpeg_bin: &Path,
    staging: &Path,
    job: &VideoJob,
    gate: &std::sync::Arc<AttemptGate>,
    page_url: &str,
    hls_format_id: &str,
    mut abort: oneshot::Receiver<StopIntent>,
    timeout: Duration,
    tx: tokio::sync::mpsc::UnboundedSender<crate::engine_msg::EngineMsg>,
    playlist_index: Option<usize>,
) -> Result<Option<u64>, VideoError> {
    use crate::engine_msg::EngineMsg;
    use tokio::io::AsyncBufReadExt as _;
    // Fresh capture: a crashed run's live file must never be resumed into (append-only stream) nor adopted as an empty fresh capture.
    let ext = if job.audio_only { "m4a" } else { "mp4" };
    // Capture inside the row's staging dir: the `.part` shell stays hidden while
    // recording, and the file-growth watcher announces "Recording…" off this path.
    let out = part_path(staging, job.item_id, "live", ext);
    // Overwrite pre-flight (Parabolic parity): refuse before recording; the row fails instead of requeueing. Also reclaims pre-upgrade dest-dir scratch for this stem (live parts used to sit beside the finished file); the finished file at dest is left be.
    if job.dest.exists() {
        clean_dest_parts(&job.dest);
        return Err(VideoError::exists());
    }
    tokio::fs::create_dir_all(staging)
        .await
        .map_err(VideoError::staging)?;
    // Reclaim partial remuxes from attempts that died mid-ffmpeg: worthless by construction (a completed one is renamed), so this bounds crash litter without risking a real recording.
    sweep_partial_remuxes(staging);
    // At most two attempts: the from-start capture, then — only if it recorded nothing and wasn't stopped — one retry from the live edge.
    let part = out.with_extension(format!("{ext}.part"));
    let state = out.with_extension(format!("{ext}.ytdl"));
    // Covers the await windows a shutdown can cancel, so the state file does not outlive the app. Held purely for its `Drop`.
    let _scratch = LiveScratchGuard::new(&state);
    let mut downgraded: Option<VideoJob> = None;
    let src = 'attempt: loop {
        let attempt: &VideoJob = downgraded.as_ref().unwrap_or(job);
        // Fresh shell per attempt: a stale output or state file must never survive into a retry, or yt-dlp resumes fragment N against a deleted shell (corrupt recording).
        let _ = tokio::fs::remove_file(&out).await;
        let _ = tokio::fs::remove_file(&part).await;
        let _ = tokio::fs::remove_file(&state).await;
        let mut cmd = ytdlp_command(youtube_bin);
        // The builder argv ends with `-- <page URL>`: nothing may be appended
        // after it — anything past `--` becomes a positional URL.
        cmd.args(live_capture_argv(
            attempt,
            hls_format_id,
            &out,
            playlist_index,
        ));
        apply_proxy_env(&mut cmd, job.proxy.as_ref());
        let (mut child, stdout, stderr) = spawn_piped_ytdlp(cmd)?;
        // Without this guard a shutdown orphans the recorder (and the ffmpeg it may have started) still writing to the capture.
        let mut group = ProcessGroupGuard::new(&child);
        let tx_p = tx.clone();
        // Recording indicator: live captures often emit no progress for long
        // stretches, so announce once the output file has bytes. The guard
        // aborts the watcher when this attempt ends (retry, success, or
        // failure) — a leaked watcher would double-announce into the retry.
        // yt-dlp records into the `.part` shell and only renames at the end:
        // the shell is what grows during capture; the final path covers a
        // capture that finalized instantly.
        let _watcher = RecordingWatcherGuard::new(spawn_recording_watcher(
            tx.clone(),
            part.clone(),
            out.clone(),
        ));
        // Stall watchdog, not a wall clock (VOD parity): the budget measures
        // silence, not capture age. Reset by any stdout line below and by
        // output-file growth from the watcher; only a truly silent capture
        // trips it.
        let last_progress = std::sync::Arc::new(std::sync::Mutex::new(std::time::Instant::now()));
        let last_progress_p = last_progress.clone();
        let _growth = RecordingWatcherGuard::new(spawn_growth_watcher(
            last_progress.clone(),
            part.clone(),
            out.clone(),
        ));
        let progress = tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            let mut have = 0u64;
            // Announce once recording is confirmed (same "Recording…" the file watcher sends, so whichever fires first wins).
            let mut announced = false;
            while let Ok(Some(line)) = lines.next_line().await {
                // Any stdout line proves the recorder is alive.
                *lock_recover(&last_progress_p) = std::time::Instant::now();
                if let Some(p) = parse_ytdlp_template(&line) {
                    if !announced {
                        announced = true;
                        tx_p.send(EngineMsg::Phase(gettext("Recording…"))).ok();
                    }
                    if let Some(d) = p.downloaded {
                        have = have.max(d);
                    }
                    tx_p.send(EngineMsg::Progress {
                        downloaded: have,
                        total: None,
                        uploaded: 0,
                        upload_bps: 0,
                    })
                    .ok();
                }
            }
            have
        });
        let logs = drain_stderr_to_tail(stderr);
        // Numeric group id for the quiescence wait on the discard path: `reap_child` disarms the guard, so read it while still armed.
        let pgid = group.pgid();
        // `aborted` gates the live-edge retry below; `&mut abort` keeps the receiver usable for the second attempt.
        // Stall watchdog, not a wall clock: each wait runs only until the
        // stall deadline; a stdout line or output growth pushes the deadline
        // out, so a progressing capture is never killed. Only true silence
        // trips it, and the partial is still adopted below.
        let (aborted, discarded) = loop {
            let remaining = timeout.saturating_sub(stall_elapsed(&last_progress));
            tokio::select! {
                biased;
                intent = &mut abort => {
                    reap_child(&mut child, &mut group).await;
                    break match intent {
                        Ok(StopIntent::Preserve) => (true, false),
                        Ok(StopIntent::Discard) => (true, true),
                        Err(_) => {
                            // No sender remains to authorise anything: fail closed. Claim the gate so the pre-rename commit below cannot deliver either.
                            let _ = gate.discard();
                            (true, true)
                        }
                    };
                }
                waited = tokio::time::timeout(remaining, child.wait()) => {
                    match waited {
                        Ok(Ok(status)) => {
                            group.disarm();
                            if !status.success() {
                                progress.abort();
                                let log_tail = join_drain(logs).await.unwrap_or_default();
                                // A from-start attempt the site can't honor fails
                                // fast with a distinctive error and nothing
                                // recorded: retry once from the live edge instead
                                // of failing the row (same fallback as the
                                // startup miss below).
                                if live_from_start_unsupported(&log_tail)
                                    && fallback_to_live_edge(
                                        attempt.is_live,
                                        attempt.live_from_start,
                                        false,
                                        downgraded.is_some(),
                                    )
                                {
                                    tx.send(EngineMsg::Phase(gettext(
                                        "\"Live from start\" isn't available for this stream — recording from the live edge…",
                                    )))
                                    .ok();
                                    let mut edge = job.clone();
                                    edge.live_from_start = false;
                                    downgraded = Some(edge);
                                    continue 'attempt;
                                }
                                // The recorder exited on its own with a failure: adopting
                                // its partial as Finished would claim a capture that never
                                // really ran (e.g. ffmpeg choking on the playlist seconds
                                // in). Fail loudly with yt-dlp's own error line instead;
                                // the raw shell is kept for salvage, only scratch is swept.
                                let detail = last_error_line(&log_tail, "recorder failed");
                                let detail =
                                    format!("{detail}{}", salvage_note(staging));
                                sweep_live_capture(
                                    &out,
                                    &part,
                                    &state,
                                    staging,
                                    None,
                                    Staging::Sweep,
                                    Exit::RecorderFailed,
                                )
                                .await;
                                return Err(VideoError::part_failed(detail));
                            }
                            break (false, false);
                        }
                        Ok(Err(e)) => {
                            // Reap, then reclaim — never the other way round. The
                            // sweep keeps a finished recording, taking only scratch.
                            progress.abort();
                            logs.abort();
                            reap_then_sweep(
                                reap_child(&mut child, &mut group),
                                || {
                                    sweep_live_capture(
                                        &out,
                                        &part,
                                        &state,
                                        staging,
                                        None,
                                        Staging::Sweep,
                                        Exit::CaptureWaitFailed,
                                    )
                                },
                            )
                            .await;
                            return Err(VideoError::runtime(format!(
                                "{e}{}",
                                salvage_note(staging)
                            )));
                        }
                        // The deadline fired but progress landed during the
                        // wait: the budget resets, so wait again instead of
                        // killing a live capture on a race.
                        Err(_) if stall_elapsed(&last_progress) < timeout => {}
                        Err(_) => {
                            // A stalled live capture still yields what it got.
                            reap_child(&mut child, &mut group).await;
                            break (false, false);
                        }
                    }
                }
            }
        };
        let _ = join_drain(progress).await;
        // A discard is not a stop. With no row left to deliver to, the finalize
        // path below must not run: adopting and remuxing would place a file at
        // a destination with no row behind it. Only the direct child is reaped
        // here; group descendants may still be writing (quiescence wait below).
        // The scratch is deliberately left too: the manager reclaims it only
        // after this task returns -- sweeping from inside a running task is the
        // race this avoids.
        if discarded {
            logs.abort();
            // The guard only *signalled* the group, so a descendant may still be
            // writing when the manager reclaims the scratch: wait for the group
            // and report rather than assume. Five seconds, not the attempt
            // timeout -- the reap already killed everything real, so this is
            // grace for stragglers only, and the attempt timeout would park
            // the discard path (and a runtime thread) for minutes.
            if let Some(pgid) = pgid
                && !crate::video_spawn::await_group_quiescence(
                    pgid,
                    std::time::Duration::from_secs(5),
                )
                .await
            {
                tracing::warn!(
                    "recorder process group did not quiesce; reclaiming anyway with a \
                     writer possibly still present"
                );
            }
            return Ok(None);
        }
        let log_tail = join_drain(logs).await.unwrap_or_default();
        // Whatever stopped the capture — stop, stall, stream end or crash —
        // adopt what landed: MPEG-TS needs no finalizing, and yt-dlp renames
        // the `.part` shell on clean completion, so prefer the finished name.
        let src = [out.clone(), part.clone()]
            .into_iter()
            .find(|p| file_len(p).is_some_and(|n| n > 0));
        let Some(src) = src else {
            // From-start attempt that never got going: retry once from the live
            // edge instead of failing the row, and say so. Only a startup miss
            // qualifies — a mid-capture failure keeps its error, so partial
            // recordings are never discarded. Staging is untouched (nothing was
            // recorded); the terminal path below sweeps it.
            if fallback_to_live_edge(
                attempt.is_live,
                attempt.live_from_start,
                aborted,
                downgraded.is_some(),
            ) {
                tx.send(EngineMsg::Phase(gettext(
                    "\"Live from start\" isn't available for this stream — recording from the live edge…",
                )))
                .ok();
                let mut edge = job.clone();
                edge.live_from_start = false;
                downgraded = Some(edge);
                continue;
            }
            // Nothing was recorded, so there is no media to salvage —
            // drop whatever shells and state yt-dlp left behind.
            sweep_live_capture(
                &out,
                &part,
                &state,
                staging,
                None,
                Staging::Sweep,
                Exit::NothingRecorded,
            )
            .await;
            // Startup failure: surface yt-dlp's line, not a generic miss.
            let detail = log_tail
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("nothing recorded")
                .trim()
                .to_string();
            return Err(VideoError::part_failed(detail));
        };
        break src;
    };
    tx.send(EngineMsg::Phase(gettext("Finalizing…"))).ok();
    let final_tmp = match reserve_remux_temp(staging, ext) {
        Ok(path) => path,
        // No claimable slot: leave the recorded shell for salvage rather
        // than risk sharing one. This is the pre-existing behaviour.
        Err(e) => {
            sweep_live_capture(
                &out,
                &part,
                &state,
                staging,
                None,
                Staging::Sweep,
                Exit::RemuxFailed,
            )
            .await;
            return Err(e);
        }
    };
    if let Err(e) = remux_live_capture(
        ffmpeg_bin,
        &src,
        &final_tmp,
        job.audio_only,
        timeout,
        page_url,
    )
    .await
    {
        // The remux never materialized, so the recorded shell is the user's only
        // copy: keep it for salvage, drop only the scratch around it.
        let note = salvage_note(staging);
        sweep_live_capture(
            &out,
            &part,
            &state,
            staging,
            Some(&final_tmp),
            Staging::Sweep,
            Exit::RemuxFailed,
        )
        .await;
        return Err(e.with_suffix(note));
    }
    // The linearization point. `try_commit` is a CAS, so there is no window
    // between deciding and acting for a concurrent removal to slip into:
    // either this wins and the row is still here, or the removal already
    // won and there is nothing to deliver.
    if !gate.try_commit() {
        sweep_live_capture(
            &out,
            &part,
            &state,
            staging,
            Some(&final_tmp),
            Staging::Sweep,
            Exit::Discarded,
        )
        .await;
        return Ok(None);
    }
    match crate::file_names::rename_noreplace(&final_tmp, &job.dest) {
        Ok(()) => {
            gate.mark_delivered();
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // The name was claimed mid-capture; the row fails Parabolic-style
            // instead of recording again, so the shell is redundant.
            sweep_live_capture(
                &out,
                &part,
                &state,
                staging,
                Some(&final_tmp),
                Staging::Sweep,
                Exit::DestClaimed,
            )
            .await;
            return Err(VideoError::exists());
        }
        Err(e) => {
            // An unexpected rename failure (permissions, I/O) leaves the shell
            // and the completed remux as the user's only copies: keep both
            // (see `sweep_live_capture` on why Staging::Keep).
            let note = salvage_note(staging);
            sweep_live_capture(
                &out,
                &part,
                &state,
                staging,
                None,
                Staging::Keep,
                Exit::RenameFailed,
            )
            .await;
            return Err(VideoError::combine(format!("{e}{note}")));
        }
    }
    // Claimed: the remuxed file is at its final name, so the shell and
    // the state file are both redundant now.
    sweep_live_capture(
        &out,
        &part,
        &state,
        staging,
        Some(&final_tmp),
        Staging::Sweep,
        Exit::Delivered,
    )
    .await;
    Ok(file_len(&job.dest))
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_hls_ytdlp(
    youtube_bin: &Path,
    ffmpeg_bin: &Path,
    staging: &Path,
    job: &VideoJob,
    gate: &std::sync::Arc<AttemptGate>,
    hls_format_id: &str,
    mut abort: oneshot::Receiver<StopIntent>,
    timeout: Duration,
    tx: tokio::sync::mpsc::UnboundedSender<crate::engine_msg::EngineMsg>,
    playlist_index: Option<usize>,
) -> Result<Option<u64>, VideoError> {
    use crate::engine_msg::EngineMsg;
    use tokio::io::AsyncBufReadExt as _;
    tokio::fs::create_dir_all(staging)
        .await
        .map_err(VideoError::staging)?;
    // Overwrite pre-flight, same as Fresh: `rename_noreplace` never clobbers, so refuse early.
    if job.dest.exists() {
        return Err(VideoError::exists());
    }
    // Resolve the subtitle language against what the video actually offers
    // (preferred, else English, else none) before the media argv is built.
    // An abort here stops the download; a probe failure just drops subtitles.
    let mut job = job.clone();
    job.subtitles = match resolve_subtitle_lang(youtube_bin, &job, &mut abort, playlist_index).await
    {
        Ok(lang) => lang,
        Err(()) => return Ok(None),
    };
    let mut cmd = ytdlp_command(youtube_bin);
    cmd.args(hls_download_argv(
        &job,
        hls_format_id,
        ffmpeg_bin,
        &job.dest,
        playlist_index,
    ));
    apply_proxy_env(&mut cmd, job.proxy.as_ref());
    let (mut child, stdout, stderr) = spawn_piped_ytdlp(cmd)?;
    let mut group = ProcessGroupGuard::new(&child);
    // Progress lines may land on either stream depending on version;
    // parse both, collect the log tail for failure diagnostics.
    let tx_p = tx.clone();
    // Stall watchdog, not a wall clock: any stdout line proves yt-dlp is alive, so a progressing download never trips the timeout: only silence does. The merge phase uses a generous wall-clock instead of the stall deadline (see `await_child`).
    let last_progress = std::sync::Arc::new(std::sync::Mutex::new(std::time::Instant::now()));
    let last_progress_p = last_progress.clone();
    // Shared with the watchdog loop: once the merge starts the stall deadline
    // no longer applies (see `await_child`).
    let merging = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let merging_p = merging.clone();
    let progress = tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        let (mut max_dl, mut max_total, mut marked) = (0u64, None, 0u64);
        // `grid_total` is what the live block grid was built for, `leg_have` the
        // bytes within the current leg: `max_dl` stays monotonic across legs for
        // the bar, while `leg_have` resets so a second leg's map starts empty.
        let (mut grid_total, mut leg_have) = (None::<u64>, 0u64);
        let mut after_move = None::<String>;
        let mut merged = false;
        while let Ok(Some(line)) = lines.next_line().await {
            *lock_recover(&last_progress_p) = std::time::Instant::now();
            if is_ytdlp_merge_line(&line) && !merged {
                merged = true;
                merging_p.store(true, std::sync::atomic::Ordering::SeqCst);
                tx_p.send(EngineMsg::Phase(gettext("Merging…"))).ok();
            } else if let Some(path) = parse_ytdlp_after_move(&line) {
                after_move = Some(path.to_string());
            } else if let Some(p) = parse_ytdlp_template(&line) {
                if let Some(t) = p.total {
                    if leg_changed(max_total, max_dl, t, p.downloaded) {
                        // New format leg (video→audio): fresh grid and a
                        // leg-relative byte basis (see `leg_changed`).
                        tx_p.send(EngineMsg::SegmentsInit { total: t }).ok();
                        marked = 0;
                        leg_have = 0;
                        grid_total = Some(t);
                    } else {
                        // Same file, revised total: rebuild the grid on
                        // growth (refined-up estimate) or on a sharp drop
                        // (an estimate spike collapsed — the sticky max was
                        // phantom; see `estimate_collapsed`). The collapse is
                        // only adopted when the new total can still contain
                        // what we've downloaded: adopting a total below the
                        // downloaded bytes would flood the grid to a false
                        // 100% (see hls_map_survives_estimate_wobble).
                        let collapsed = estimate_collapsed(max_total, t)
                            && p.downloaded.is_some_and(|d| t >= d);
                        if grid_needs_rebuild(grid_total, t) || collapsed {
                            tx_p.send(EngineMsg::SegmentsInit { total: t }).ok();
                            grid_total = Some(t);
                            if collapsed {
                                max_total = Some(t);
                            }
                            let len = crate::file_names::piece_len(t);
                            marked = 0;
                            if let Some(count) = leg_have.checked_div(len) {
                                for idx in 0..count {
                                    tx_p.send(EngineMsg::PieceDone(idx)).ok();
                                    marked += 1;
                                }
                            }
                        }
                    }
                    max_total = Some(t.max(max_total.unwrap_or(0)));
                }
                if let Some(d) = p.downloaded
                    && let Some(grid) = grid_total
                    && grid > 0
                {
                    // Marks align with the displayed grid, not the running max:
                    // the grid may lag, and past its end the row drops them.
                    leg_have = leg_have.max(d.min(grid));
                    let have = max_dl.max(d.min(max_total.unwrap_or(grid)));
                    max_dl = have;
                    for idx in
                        piece_marks(crate::file_names::piece_len(grid), &mut marked, leg_have)
                    {
                        tx_p.send(EngineMsg::PieceDone(idx)).ok();
                    }
                }
                tx_p.send(EngineMsg::Progress {
                    downloaded: max_dl,
                    total: max_total,
                    uploaded: 0,
                    upload_bps: 0,
                })
                .ok();
            }
        }
        (max_dl, max_total, after_move)
    });
    let logs = drain_stderr_to_tail(stderr);
    // Stall watchdog, not a wall clock: `timeout` is the silence budget. Each
    // wait runs only until the stall deadline; a progress line pushes the
    // deadline out, so a progressing download is never killed. Once merging,
    // `await_child` swaps the stall deadline for the merge wall-clock. The snapshot only sizes
    // each wait; the kill decision re-reads the live flag.
    let status = loop {
        let merging_snapshot = merging.load(std::sync::atomic::Ordering::SeqCst);
        let remaining = timeout.saturating_sub(stall_elapsed(&last_progress));
        tokio::select! {
            biased;
            _ = &mut abort => {
                reap_child(&mut child, &mut group).await;
                progress.abort();
                logs.abort();
                sweep_staging_preserving_recordings(staging, job.item_id);
                // Staging is the dest dir itself: never remove it or its parent.
                return Ok(None);
            }
            waited = await_child(&mut child, merging_snapshot, remaining, MERGE_WALL_CLOCK) => match waited {
                Ok(Ok(status)) => {
                    // The leader is reaped, so release the PGID: holding it across the drain joins would risk the OS recycling it onto another group.
                    group.disarm();
                    break status;
                }
                Ok(Err(e)) => {
                    reap_child(&mut child, &mut group).await;
                    progress.abort();
                    logs.abort();
                    return Err(VideoError::runtime(&e));
                }
                // The merge marker can land while `await_child` is parked on a
                // pre-merge snapshot: re-read the live flag before killing, or a
                // silent merge dies exactly one budget after its marker line.
                Err(_)
                    if stall_elapsed(&last_progress) >= timeout
                        && !merging.load(std::sync::atomic::Ordering::SeqCst) =>
                {
                    reap_child(&mut child, &mut group).await;
                    progress.abort();
                    logs.abort();
                    return Err(VideoError::part_failed("stalled"));
                }
                // The merge wall-clock expired while a merge was genuinely
                // underway (the wait started after the merge marker and the
                // live flag still agrees): reap the hung merge and fail the
                // part instead of parking the row forever. A merge that
                // finished in the same instant still counts.
                Err(_)
                    if merging_snapshot
                        && merging.load(std::sync::atomic::Ordering::SeqCst) =>
                {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            group.disarm();
                            break status;
                        }
                        _ => {
                            reap_child(&mut child, &mut group).await;
                            progress.abort();
                            logs.abort();
                            return Err(VideoError::part_failed("merge timed out"));
                        }
                    }
                }
                // Progress landed mid-wait, or the merge started while parked:
                // loop back and re-arm (the next wait uses the merge wall-clock
                // once merging).
                Err(_) => {}
            },
        }
    };
    let (mut _downloaded, _total, after_move) = progress.await.unwrap_or_default();
    let log_tail = logs.await.unwrap_or_default();
    if !status.success() {
        let detail = last_log_line(&log_tail, "yt-dlp reported failure");
        return Err(VideoError::part_failed(detail));
    }
    let final_tmp = discover_ytdlp_output(&job.dest, after_move.as_deref());
    let Some(final_tmp) = final_tmp else {
        return Err(VideoError::part_failed("no output file produced"));
    };
    // Same container-truth backstop as the unified path: `--remux-video` is
    // honored here too, so a remux pref changed mid-queue would claim under a
    // stale name. (Live rows need none: ext is fixed and live never remuxes.)
    if let Some(truer) = container_truth_name(&job.dest, &final_tmp) {
        tx.send(EngineMsg::SuggestName(truer)).ok();
    }
    // Atomic claim into place (EXDEV-safe, no clobber), at the linearization
    // point as on the live leg. The part shells beside the finished file are
    // ours to sweep: no rename means no delivery happened.
    if !gate.try_commit() {
        clean_dest_parts(&job.dest);
        // Lost the race: clear this item's staging files only, never the dest dir.
        clean_staging_files(staging, job.item_id);
        return Ok(None);
    }
    match crate::file_names::rename_noreplace(&final_tmp, &job.dest) {
        Ok(()) => {
            gate.mark_delivered();
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(VideoError::exists());
        }
        Err(e) => return Err(VideoError::combine(&e)),
    }
    // Best-effort subtitle sidecar: `-o` is the `hls` part template, so collect
    // `<stem>.hls.<lang>.srt` beside the finished file (outside the part
    // namespace, so retries and row removal keep it). When embedding, the part
    // sidecar is deleted outright: it lives in the dest dir, so no staging wipe
    // reaches it.
    if !job.embed_subs
        && let Some(lang) = job.subtitles.as_deref()
    {
        collect_sidecar(
            &dest_part_path(&job.dest, "hls", &format!("{lang}.srt")),
            &job.dest,
            lang,
        );
    } else if let Some(lang) = job.subtitles.as_deref() {
        let _ =
            tokio::fs::remove_file(dest_part_path(&job.dest, "hls", &format!("{lang}.srt"))).await;
    }
    sweep_staging_preserving_recordings(staging, job.item_id);
    // Staging is the dest dir itself: never remove it or its parent.
    Ok(file_len(&job.dest))
}

#[cfg(test)]
#[path = "video_runner_tests.rs"]
mod tests;
