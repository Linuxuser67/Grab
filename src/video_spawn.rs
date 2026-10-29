//! Spawn plumbing + fetch resolve: process-group spawns, pipe drains, tree kills, output discovery.
//! Consumed by the dialog/runner through the `video` facade.

use crate::video_argv::{apply_proxy_env, proxy_cli_args};
use crate::video_probe::{
    page_host, parse_playlist_json, parse_single_video, pick_playlist_entry,
    playlist_resolve_error, retarget_story_items, sanitize_video_json,
};
use crate::video_progress::{last_log_line, trace_format_lines};
use crate::video_tools::{VideoError, ensure_ytdlp_version};
use crate::video_types::{FetchedVideo, ProbeResult, VideoInfo};
use gettextrs::gettext;
use std::path::{Path, PathBuf};
use std::time::Duration;
use yt_dlp::model::Video;

/// Prepend Grab-managed tools to PATH without hiding system tools. gallery-dl may
/// delegate HLS/DASH transfers to yt-dlp, which otherwise is invisible in Flatpak.
pub(crate) fn prepend_user_lib_to_path(cmd: &mut tokio::process::Command) {
    let mut paths = vec![crate::video_tools::user_lib_dir()];
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    if let Ok(path) = std::env::join_paths(paths) {
        cmd.env("PATH", path);
    }
}

/// Propagate Grab's configured proxy to gallery-dl and delegated downloader processes.
pub(crate) fn apply_configured_proxy_env(
    cmd: &mut tokio::process::Command,
    proxy: Option<&crate::net_types::ResolvedProxy>,
) {
    if let Some(proxy) = proxy {
        cmd.env("HTTP_PROXY", &proxy.cli_url)
            .env("http_proxy", &proxy.cli_url)
            .env("HTTPS_PROXY", &proxy.cli_url)
            .env("https_proxy", &proxy.cli_url)
            .env("NO_PROXY", &proxy.no_proxy_env)
            .env("no_proxy", &proxy.no_proxy_env);
    }
}

const FETCH_TIMEOUT_SECS: u64 = 60;

/// Byte ceilings for drained child pipes (the timeout bounds time, not memory; stdout carries the JSON document).
const MAX_STDOUT_BYTES: u64 = 32 * 1024 * 1024;
const MAX_STDERR_BYTES: u64 = 1024 * 1024;

/// Drain one child pipe with a hard byte ceiling (overflow fails as QuotaExceeded, never a bigger buffer).
/// Always drains to EOF into a discard buffer so the child can never block on a full pipe.
/// Shared with the gallery probe (same bounded-drain policy).
pub(crate) async fn read_bounded(
    stream: impl tokio::io::AsyncRead + Unpin,
    limit: u64,
) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt as _;
    let mut buf = Vec::new();
    let mut reader = tokio::io::BufReader::new(stream);
    (&mut reader).take(limit + 1).read_to_end(&mut buf).await?;
    if buf.len() as u64 <= limit {
        return Ok(buf);
    }
    let mut discard = [0u8; 8192];
    while reader.read(&mut discard).await? != 0 {}
    Err(std::io::Error::new(
        std::io::ErrorKind::QuotaExceeded,
        "child output exceeded the size limit",
    ))
}

/// Fetch one page's raw `--dump-single-json`, then parse leniently. `flat_playlist` lists collection stubs; false runs full extraction.
pub(crate) async fn fetch_raw_dump_json(
    youtube_bin: &Path,
    url: &str,
    cookies_browser: &str,
    timeout: Duration,
    fetch_proxy: Option<&crate::net_types::ResolvedProxy>,
    flat_playlist: bool,
) -> Result<serde_json::Value, VideoError> {
    let mut args = vec!["--ignore-config".to_string(), "--no-progress".to_string()];
    if flat_playlist {
        args.push("--flat-playlist".to_string());
    }
    args.push("--dump-single-json".to_string());
    args.extend(proxy_cli_args(fetch_proxy));
    // Extension handoff (Cloudflare bypass): staged by the handoff receiver,
    // peeked here. The entry outlives the probe — the subtitle probe and the
    // media download spawn yt-dlp separately and need it too; the finished
    // download discards it. The tail ends with `-- <url>`, past which yt-dlp
    // reads positionals.
    args.extend(crate::extension_server::ytdlp_spawn_identity(
        cookies_browser,
        url,
    ));
    let guarded_proxy = guard_untrusted_args(&args)?;
    // Direct spawn with the same semantics (concurrent drain, timeout kill, stderr detail on failure).
    let mut cmd = ytdlp_command(youtube_bin).await;
    // Belt-and-braces with the ProcessGroupGuard below (which SIGKILLs the
    // process group on drop): if the drain is dropped, the child dies with
    // it either way. Download/capture spawns keep their guard-only shutdown
    // behavior; never set this via the shared ytdlp_command() builder.
    cmd.kill_on_drop(true);
    cmd.args(&args);
    apply_proxy_env(&mut cmd, fetch_proxy);
    apply_guarded_proxy_env(&mut cmd, guarded_proxy.as_deref());
    // Debug: log the full argv before spawning. as_std() captures everything
    // including ytdlp_command()'s --impersonate flag.
    crate::backend_debug::log_command(&cmd);
    let mut child = cmd.spawn().map_err(VideoError::fetch)?;
    let mut group = ProcessGroupGuard::new(&child);
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| VideoError::fetch("yt-dlp gave no output pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| VideoError::fetch("yt-dlp gave no log pipe"))?;
    // Drain both pipes concurrently (an unread full pipe would stall yt-dlp), each with a hard ceiling.
    fn drain(
        stream: impl tokio::io::AsyncRead + Unpin + Send + 'static,
        limit: u64,
    ) -> tokio::task::JoinHandle<std::io::Result<Vec<u8>>> {
        tokio::spawn(async move { read_bounded(stream, limit).await })
    }
    let out_task = drain(stdout, MAX_STDOUT_BYTES);
    let err_task = drain(stderr, MAX_STDERR_BYTES);
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => {
            group.disarm();
            status
        }
        Ok(Err(e)) => {
            reap_child(&mut child, &mut group).await;
            return Err(VideoError::fetch(&e));
        }
        Err(_) => {
            reap_child(&mut child, &mut group).await;
            return Err(VideoError::fetch(gettext("the lookup timed out")));
        }
    };
    // Drains join with a grace period (an orphaned grandchild holding the pipe would stall a bare await); only the ceiling gets the size message.
    let stdout = match join_drain(out_task).await {
        Some(Ok(buf)) => buf,
        Some(Err(e)) if e.kind() == std::io::ErrorKind::QuotaExceeded => {
            return Err(VideoError::fetch(gettext(
                "the lookup produced too much output",
            )));
        }
        _ => Vec::new(),
    };
    let stderr = join_drain(err_task)
        .await
        .and_then(|r| r.ok())
        .unwrap_or_default();
    // Debug: echo the probe's stderr (not live — the probe is short-lived,
    // so post-hoc is fine).
    crate::backend_debug::echo_stderr(&stderr);
    if !status.success() {
        let detail = last_log_line(&String::from_utf8_lossy(&stderr), "yt-dlp reported failure");
        return Err(VideoError::fetch(detail));
    }
    let value: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&stdout)).map_err(VideoError::fetch)?;
    Ok(value)
}

/// Strict single-video model for the download worker (full extraction; playlist output selects the picked entry or expands worker-side).
pub(crate) async fn fetch_video_page(
    youtube_bin: &Path,
    url: &str,
    cookies_browser: &str,
    timeout: Duration,
    fetch_proxy: Option<&crate::net_types::ResolvedProxy>,
    playlist_item_id: Option<&str>,
) -> Result<FetchedVideo, VideoError> {
    let value = fetch_raw_dump_json(
        youtube_bin,
        url,
        cookies_browser,
        timeout,
        fetch_proxy,
        false,
    )
    .await?;
    if let Some(playlist) = parse_playlist_json(&value, url) {
        if playlist_item_id.is_some() {
            if let Some((entry, playlist_index)) = pick_playlist_entry(&value, playlist_item_id) {
                return parse_single_video(entry).map(|v| FetchedVideo::Single {
                    video: Box::new(v),
                    playlist_index: Some(playlist_index),
                });
            }
            return Err(playlist_resolve_error(playlist_item_id));
        }
        return Ok(FetchedVideo::Playlist(playlist));
    }
    parse_single_video(value).map(|v| FetchedVideo::Single {
        video: Box::new(v),
        playlist_index: None,
    })
}

/// Aborts the wrapped task when dropped. Aborting an already-finished task is
/// a no-op, so no disarm bookkeeping is needed. [`fetch_video_infos`] keeps it
/// alive for the duration of the probe: a caller dropping the probe future
/// (card closed, lookup superseded) must not leave the yt-dlp worker — and
/// its child — probing on detached.
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Extract metadata for one URL (the page URL is what survives restarts, not the media URLs).
/// Returns the parsed result plus the raw `--dump-single-json` for single videos
/// (used for `--load-info-json` at download time to skip re-extraction).
/// Playlists return `None` (their entries are re-probed individually).
pub async fn fetch_video_infos(
    youtube_bin: PathBuf,
    url: String,
    cookies_browser: String,
    newest_first: bool,
    fetch_proxy: Option<crate::net_types::ResolvedProxy>,
    cancel: Option<tokio::sync::oneshot::Receiver<()>>,
) -> Result<(ProbeResult, Option<serde_json::Value>), VideoError> {
    let handle = crate::runtime::tokio_rt().spawn(async move {
        let total_started = std::time::Instant::now();
        let mut version_ms = 0u64;
        let mut deno_ms = 0u64;
        let mut metadata_lookup_ms = 0u64;
        let mut parse_ms = 0u64;
        let result: Result<(ProbeResult, Option<serde_json::Value>), VideoError> = async {
            let phase_started = std::time::Instant::now();
            let version_result = ensure_ytdlp_version(&youtube_bin).await;
            version_ms = phase_started.elapsed().as_millis() as u64;
            let yt_version = version_result?;
            tracing::info!(yt_dlp = %yt_version, url_host = %page_host(&url), "resolving video page");
            // Deno is Grab's JavaScript runtime for YouTube; make sure it's
            // installed before a YouTube spawn that may need to solve JS challenges.
            let phase_started = std::time::Instant::now();
            let deno_result = crate::video_tools::ensure_deno(&url).await;
            deno_ms = phase_started.elapsed().as_millis() as u64;
            deno_result?;
            let phase_started = std::time::Instant::now();
            let fetch_result = tokio::time::timeout(
                Duration::from_secs(FETCH_TIMEOUT_SECS),
                fetch_raw_dump_json(
                    &youtube_bin,
                    &url,
                    &cookies_browser,
                    Duration::from_secs(300),
                    fetch_proxy.as_ref(),
                    true,
                ),
            )
            .await;
            metadata_lookup_ms = phase_started.elapsed().as_millis() as u64;
            let mut value = match fetch_result {
                Ok(Ok(value)) => value,
                Ok(Err(e)) => return Err(e),
                Err(_) => return Err(VideoError::fetch(gettext("the lookup timed out"))),
            };
            let phase_started = std::time::Instant::now();
            if let Some(mut playlist) = parse_playlist_json(&value, &url) {
                retarget_story_items(&url, &mut playlist);
                parse_ms = phase_started.elapsed().as_millis() as u64;
                return Ok((ProbeResult::Playlist(playlist), None));
            }
            sanitize_video_json(&mut value);
            // Clone for --load-info-json: the parse below consumes `value`.
            // The JSON is ~100KB; one clone per probe is acceptable.
            let raw_json = value.clone();
            let parsed_video: Result<Video, VideoError> =
                serde_json::from_value(value).map_err(VideoError::fetch);
            parse_ms = phase_started.elapsed().as_millis() as u64;
            let video = parsed_video?;
            Ok((
                ProbeResult::Single(VideoInfo::from(&video, &url, newest_first)),
                Some(raw_json),
            ))
        }
        .await;
        tracing::info!(
            url_host = %page_host(&url),
            version_ms,
            deno_ms,
            metadata_lookup_ms,
            parse_ms,
            total_ms = total_started.elapsed().as_millis() as u64,
            result = if result.is_ok() { "ok" } else { "error" },
            "yt-dlp lookup timing"
        );
        result
    });
    // Dropping the caller's future aborts the worker: the yt-dlp child dies
    // with it (kill_on_drop) instead of probing on detached.
    let _abort_on_drop = AbortOnDrop(handle.abort_handle());
    await_probe(handle, cancel).await
}

/// Await `handle` unless `cancel` fires first (a caller cancelling the
/// probe): abort the worker so the yt-dlp child dies with it instead of
/// probing on detached.
async fn await_probe(
    handle: tokio::task::JoinHandle<Result<(ProbeResult, Option<serde_json::Value>), VideoError>>,
    cancel: Option<tokio::sync::oneshot::Receiver<()>>,
) -> Result<(ProbeResult, Option<serde_json::Value>), VideoError> {
    match cancel {
        Some(rx) => {
            // Pin: the abort arm needs `handle` after the select drops its
            // borrow of it.
            let mut handle = handle;
            tokio::select! {
                r = &mut handle => match r {
                    Ok(r) => r,
                    Err(e) => Err(VideoError::runtime(&e)),
                },
                _ = rx => {
                    handle.abort();
                    Err(VideoError::cancelled())
                }
            }
        }
        None => match handle.await {
            Ok(r) => r,
            Err(e) => Err(VideoError::runtime(&e)),
        },
    }
}

/// yt-dlp spawn with null stdin, piped outputs, and its own process group (so timeouts kill the whole tree).
/// The user lib dir heads PATH so yt-dlp can find Grab-installed Deno and
/// other managed tools during extractor execution.
pub(crate) async fn ytdlp_command(youtube_bin: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(youtube_bin);
    // Impersonation is user-selectable (Preferences → Advanced): yt-dlp's
    // docs make it opt-in and warn that forcing it on every request may hurt
    // speed and stability. The stored string resolves through the validated
    // value table (unknown values fall back to none), and the schema only
    // accepts the listed choices, so argv never carries an unvetted string.
    // Empty target = None = yt-dlp's documented default: decisive, because
    // it also skips the support probe entirely. Support is probed once per
    // (binary, mtime, size) and cached — a PATH-provided yt-dlp without
    // curl_cffi hard-fails on the flag, so never pass it blindly.
    let target = crate::settings::AppSettings::new().impersonate_browser();
    let target =
        crate::video_prefs::impersonate_value(crate::video_prefs::impersonate_index(&target));
    if !target.is_empty() && crate::video_tools::ytdlp_supports_impersonation(youtube_bin).await {
        cmd.arg("--impersonate").arg(target);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    prepend_user_lib_to_path(&mut cmd);
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    cmd
}

/// Validate that extractor argv has a URL and no explicit proxy override.
/// The authenticated proxy URL must stay out of argv because debug logging
/// records arguments; the child receives it through standard proxy variables.
pub(crate) fn validate_guarded_proxy_args(args: &[String]) -> Result<(), &'static str> {
    let Some(separator) = args.iter().position(|arg| arg == "--") else {
        return Err("Extractor arguments are missing the URL terminator");
    };
    if args.get(separator + 1).is_none() {
        return Err("Extractor arguments are missing the URL after the terminator");
    }
    let overrides_guarded_proxy = args[..separator].iter().any(|arg| {
        // Python option parsers accept both --option=value and unambiguous
        // long-option abbreviations. Reject any spelling that could select
        // --proxy before the positional URL terminator.
        let option = arg
            .split_once('=')
            .map_or(arg.as_str(), |(name, _value)| name);
        option == "--proxy" || (option.len() > 2 && "--proxy".starts_with(option))
    });
    if overrides_guarded_proxy {
        return Err("Cannot safely enforce public-address checks through a configured proxy");
    }
    Ok(())
}

/// Apply the loopback proxy when the initial page URL is a marked browser handoff.
pub(crate) fn guard_untrusted_args(args: &[String]) -> Result<Option<String>, VideoError> {
    let Some(separator) = args.iter().position(|arg| arg == "--") else {
        return Err(VideoError::fetch(
            "Extractor arguments are missing the URL terminator",
        ));
    };
    let Some(page_url) = args.get(separator + 1) else {
        return Err(VideoError::fetch(
            "Extractor arguments are missing the URL after the terminator",
        ));
    };
    let Some(host) = url::Url::parse(page_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
    else {
        return Ok(None);
    };
    if !crate::download_net::is_untrusted_handoff_host(&host) {
        return Ok(None);
    }
    validate_guarded_proxy_args(args).map_err(VideoError::fetch)?;
    let proxy = crate::subprocess_proxy::ensure_started().map_err(VideoError::fetch)?;
    Ok(Some(proxy))
}

/// Route extractor and external-downloader requests through the guarded proxy.
/// The authenticated URL stays in standard environment variables, never argv.
pub(crate) fn apply_guarded_proxy_env(cmd: &mut tokio::process::Command, proxy: Option<&str>) {
    if let Some(proxy) = proxy {
        cmd.env("NO_PROXY", "")
            .env("no_proxy", "")
            .env("HTTP_PROXY", proxy)
            .env("http_proxy", proxy)
            .env("HTTPS_PROXY", proxy)
            .env("https_proxy", proxy);
    }
}

/// Spawn a `ytdlp_command` child and take its pipes.
pub(crate) fn spawn_piped_ytdlp(
    mut cmd: tokio::process::Command,
) -> Result<
    (
        tokio::process::Child,
        tokio::process::ChildStdout,
        tokio::process::ChildStderr,
    ),
    VideoError,
> {
    // Debug: log the full argv before spawning. as_std() gives the program
    // and args that were built via ytdlp_command() + cmd.args(argv).
    crate::backend_debug::log_command(&cmd);
    let mut child = cmd.spawn().map_err(VideoError::runtime)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| VideoError::runtime("yt-dlp gave no output pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| VideoError::runtime("yt-dlp gave no log pipe"))?;
    Ok((child, stdout, stderr))
}

/// Drain a child's stderr in the background: trace lines live, keep the last 8 KiB for error detail.
pub(crate) fn drain_stderr_to_tail(
    stderr: tokio::process::ChildStderr,
) -> tokio::task::JoinHandle<String> {
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt as _;
        let mut reader = tokio::io::BufReader::new(stderr);
        let mut tail = Vec::new();
        let mut pending = String::new();
        // Redact complete stderr lines, not arbitrary read chunks: URLs and
        // their query tokens can span reads. Drop overlong debug lines as a
        // unit rather than risk printing a token fragment after the cap.
        const MAX_DEBUG_LINE: usize = 64 * 1024;
        let mut debug_line = Vec::new();
        let mut discard_debug_line = false;
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if crate::backend_debug::enabled() {
                        for &byte in &buf[..n] {
                            if byte == b'\n' {
                                if discard_debug_line {
                                    crate::backend_debug::echo_stderr(
                                        b"[grab-backend] stderr line omitted (over limit)\n",
                                    );
                                } else {
                                    debug_line.push(byte);
                                    crate::backend_debug::echo_stderr(&debug_line);
                                }
                                debug_line.clear();
                                discard_debug_line = false;
                            } else if !discard_debug_line {
                                if debug_line.len() >= MAX_DEBUG_LINE {
                                    debug_line.clear();
                                    discard_debug_line = true;
                                } else {
                                    debug_line.push(byte);
                                }
                            }
                        }
                    }
                    trace_format_lines(&mut pending, &buf[..n]);
                    tail.extend_from_slice(&buf[..n]);
                    if tail.len() > 8192 {
                        tail.drain(..tail.len() - 8192);
                    }
                }
            }
        }
        if crate::backend_debug::enabled() && !discard_debug_line && !debug_line.is_empty() {
            crate::backend_debug::echo_stderr(&debug_line);
        }
        String::from_utf8_lossy(&tail).into_owned()
    })
}

/// Signal a process group, ignoring whether it still exists.
fn signal_group(pid: libc::pid_t) {
    // Unconditional: the group outlives its leader (ffmpeg grandchildren), and the pid-reuse race needs churn no desktop hits.
    // SAFETY: constant signal number; ESRCH (raced exit) is harmless.
    unsafe {
        libc::killpg(pid, libc::SIGKILL);
    }
}

/// SIGKILL a spawned downloader and its ffmpeg group (one killpg, so ffmpeg is never orphaned mid-merge).
pub(crate) fn kill_tree(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        signal_group(pid as libc::pid_t);
    }
}

/// Kills the process group on drop (covers task-abort mid-await, where async cleanup never runs).
/// Disarm as soon as the child is reaped: a recycled PGID would otherwise SIGKILL an unrelated group.
/// Runners must disarm in their own wait arm too; the seven spawn sites each carry a guard plus a disarm.
pub(crate) struct ProcessGroupGuard {
    pid: Option<libc::pid_t>,
}

impl ProcessGroupGuard {
    /// `None` when the child was already reaped (no group left to signal).
    pub(crate) fn new(child: &tokio::process::Child) -> Self {
        Self {
            pid: child.id().map(|pid| pid as libc::pid_t),
        }
    }

    /// Give up ownership: the child has been reaped, so its group id is no longer ours to signal.
    pub(crate) fn disarm(&mut self) {
        self.pid = None;
    }

    /// The process group id this guard signals.
    pub(crate) fn pgid(&self) -> Option<libc::pid_t> {
        self.pid
    }

    /// Whether the guard would still signal on drop (test-only: catches guards outliving their reap).
    #[cfg(test)]
    pub(crate) fn is_armed(&self) -> bool {
        self.pid.is_some()
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid {
            signal_group(pid);
        }
    }
}

/// Removes a live capture's yt-dlp state file on drop (task-abort cleanup; state file only, never staging or media).
/// No disarm needed (terminal exits already sweep it); sync `std::fs` because `Drop` cannot await.
pub(crate) struct LiveScratchGuard {
    state: std::path::PathBuf,
}

impl LiveScratchGuard {
    pub(crate) fn new(state: &std::path::Path) -> Self {
        Self {
            state: state.to_path_buf(),
        }
    }
}

impl Drop for LiveScratchGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.state);
    }
}

/// `kill_tree`, wait for the reap, then disarm the guard.
/// Deliberately unbounded: proceeding on a merely signalled child risks remuxing a file still being written (do not add a timeout here).
pub(crate) async fn reap_child(child: &mut tokio::process::Child, group: &mut ProcessGroupGuard) {
    kill_tree(child);
    let _ = child.wait().await;
    group.disarm();
}

/// Wait until no process remains in `pgid`, bounded (`false` = treat a writer as possibly still present).
#[cfg(target_os = "linux")]
pub(crate) async fn await_group_quiescence(pgid: i32, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        // SAFETY: signal 0 performs error checking only; a constant signal.
        let alive = unsafe { libc::killpg(pgid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        if !alive {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) async fn await_group_quiescence(_pgid: i32, _timeout: std::time::Duration) -> bool {
    true
}

/// Join a pipe-drain with a grace period (orphaned grandchildren can hold pipes open; falls back to abort).
pub(crate) async fn join_drain<T>(task: tokio::task::JoinHandle<T>) -> Option<T> {
    let abort = task.abort_handle();
    tokio::select! {
        biased;
        done = task => done.ok(),
        _ = tokio::time::sleep(Duration::from_secs(5)) => {
            abort.abort();
            None
        }
    }
}

/// The finished file of one yt-dlp attempt: the `after_move` path when trustworthy, else the largest non-temp file.
pub(crate) fn discover_ytdlp_output(dest: &Path, after_move: Option<&str>) -> Option<PathBuf> {
    let dir = dest.parent().unwrap_or_else(|| Path::new(""));
    // Canonicalize dir too: a symlinked download dir would otherwise defeat
    // the fast-path comparison below (canonical vs non-canonical).
    let dir_canonical = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let stem = dest
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let prefix = format!("{stem}.hls.");
    if let Some(path) = after_move
        && let Ok(canonical) = std::fs::canonicalize(path)
        && canonical.starts_with(&dir_canonical)
        && canonical.is_file()
        && let Some(name) = canonical.file_name().and_then(|n| n.to_str())
        && name.starts_with(&prefix)
    {
        return Some(canonical);
    }
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_file()
                && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                    n.starts_with(&prefix)
                        // Subtitle sidecars share the prefix but are never the media output.
                        && !matches!(
                            p.extension().and_then(|e| e.to_str()),
                            Some("part" | "ytdl" | "temp" | "tmp" | "frag" | "srt")
                        )
                })
        })
        .max_by_key(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guarded_proxy_validation_leaves_credentials_out_of_argv_and_rejects_overrides() {
        let target = "https://video.example.invalid/watch";
        let args = vec![
            "--ignore-config".to_string(),
            "--".to_string(),
            target.to_string(),
        ];
        let original = args.clone();
        validate_guarded_proxy_args(&args).expect("argv can use the guarded proxy");
        assert_eq!(args, original, "credentials must stay out of argv");

        // Reject the exact option, --option=value, and abbreviations accepted
        // by Python's long-option parsers before the positional terminator.
        for option in [
            "--proxy",
            "--proxy=http://example-proxy.invalid",
            "--prox",
            "--prox=http://example-proxy.invalid",
            "--pro",
            "--p",
        ] {
            let conflict = vec![
                option.to_string(),
                "--".to_string(),
                target.to_string(),
            ];
            assert!(
                validate_guarded_proxy_args(&conflict).is_err(),
                "unguarded proxy spelling must be rejected: {option}"
            );
        }
        let conflict_with_separate_value = vec![
            "--proxy".to_string(),
            "http://example-proxy.invalid".to_string(),
            "--".to_string(),
            target.to_string(),
        ];
        assert!(validate_guarded_proxy_args(&conflict_with_separate_value).is_err());
        assert!(validate_guarded_proxy_args(&["--ignore-config".to_string()]).is_err());
    }

    #[test]
    fn guarded_argv_without_url_terminator_fails_closed() {
        let args = vec!["--ignore-config".to_string()];
        assert!(guard_untrusted_args(&args).is_err());
    }

    #[test]
    fn unmarked_user_entered_url_keeps_its_arguments() {
        let url = format!(
            "https://user-entered-{}.example.invalid/watch",
            std::process::id()
        );
        let args = vec!["--ignore-config".to_string(), "--".to_string(), url];
        let original = args.clone();
        assert!(
            guard_untrusted_args(&args)
                .expect("ordinary user-entered URLs need no guarded proxy")
                .is_none()
        );
        assert_eq!(args, original);
    }

    #[test]
    fn guarded_proxy_environment_clears_bypasses_and_covers_external_downloaders() {
        use std::ffi::OsStr;

        let proxy = "http://grab:0123456789abcdef@127.0.0.1:43123";
        let mut command = tokio::process::Command::new("ffmpeg");
        apply_guarded_proxy_env(&mut command, Some(proxy));
        let envs: Vec<_> = command
            .as_std()
            .get_envs()
            .map(|(key, value)| (key.to_os_string(), value.map(ToOwned::to_owned)))
            .collect();
        let get = |name: &str| {
            envs.iter()
                .find(|(key, _)| key == OsStr::new(name))
                .and_then(|(_, value)| value.as_deref())
        };
        assert_eq!(get("NO_PROXY"), Some(OsStr::new("")));
        assert_eq!(get("no_proxy"), Some(OsStr::new("")));
        assert_eq!(get("HTTP_PROXY"), Some(OsStr::new(proxy)));
        assert_eq!(get("http_proxy"), Some(OsStr::new(proxy)));
        assert_eq!(get("HTTPS_PROXY"), Some(OsStr::new(proxy)));
        assert_eq!(get("https_proxy"), Some(OsStr::new(proxy)));
    }

    #[test]
    fn fetch_video_infos_reports_cancelled_when_pre_cancelled() {
        // A caller cancelling before the probe starts: the probe must
        // report Cancelled (not run the pipeline, not a generic error).
        let (tx, rx) = tokio::sync::oneshot::channel();
        tx.send(()).expect("receiver alive");
        let youtube_bin = std::path::PathBuf::from("/nonexistent/yt-dlp");
        let r = crate::runtime::tokio_rt().block_on(fetch_video_infos(
            youtube_bin,
            "https://example.com/video".to_string(),
            String::new(),
            false,
            None,
            Some(rx),
        ));
        assert!(
            matches!(r, Err(VideoError::Cancelled)),
            "a pre-cancelled probe must report Cancelled, got {r:?}"
        );
    }

    #[test]
    fn bounded_drain_passes_small_streams() {
        let rt = crate::runtime::tokio_rt();
        let data = vec![7u8; 1024];
        let out = rt
            .block_on(read_bounded(std::io::Cursor::new(data.clone()), 2048))
            .expect("a stream under the limit must pass through");
        assert_eq!(out, data);
    }

    #[test]
    fn bounded_drain_accepts_exactly_at_limit() {
        let rt = crate::runtime::tokio_rt();
        let out = rt
            .block_on(read_bounded(std::io::Cursor::new(vec![9u8; 64]), 64))
            .expect("a stream at exactly the limit must pass through");
        assert_eq!(out.len(), 64);
    }

    #[test]
    fn bounded_drain_rejects_oversize_stream() {
        let rt = crate::runtime::tokio_rt();
        let err = rt
            .block_on(read_bounded(std::io::Cursor::new(vec![9u8; 65]), 64))
            .expect_err("a stream past the limit must fail, not grow");
        assert_eq!(err.kind(), std::io::ErrorKind::QuotaExceeded);
    }

    #[test]
    fn bounded_drain_keeps_draining_after_quota() {
        let rt = crate::runtime::tokio_rt();
        rt.block_on(async {
            use tokio::io::AsyncWriteExt as _;
            let (mut writer, reader) = tokio::io::duplex(8192);
            let write_task = tokio::spawn(async move {
                let chunk = vec![7u8; 8192];
                for _ in 0..32 {
                    writer.write_all(&chunk).await.unwrap();
                }
                writer.shutdown().await.unwrap();
            });
            let err = read_bounded(reader, 64)
                .await
                .expect_err("a stream past the limit must fail");
            assert_eq!(err.kind(), std::io::ErrorKind::QuotaExceeded);
            tokio::time::timeout(std::time::Duration::from_secs(10), write_task)
                .await
                .expect("the drain must not stall the writer")
                .unwrap();
        });
    }

    #[test]
    fn abort_on_drop_cancels_a_running_worker() {
        let rt = crate::runtime::tokio_rt();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let worker = rt.spawn(async move {
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        });
        let guard = AbortOnDrop(worker.abort_handle());
        rt.block_on(started_rx).expect("worker starts");
        drop(guard);
        let err = rt
            .block_on(worker)
            .expect_err("the guard's abort must cancel the running worker");
        assert!(err.is_cancelled(), "got {err:?}");
    }

    #[test]
    fn abort_on_drop_leaves_a_finished_worker_alone() {
        let rt = crate::runtime::tokio_rt();
        let worker = rt.spawn(async { 7u32 });
        let guard = AbortOnDrop(worker.abort_handle());
        assert_eq!(rt.block_on(worker).expect("worker completes"), 7);
        // Aborting a finished task is a no-op, not a cancellation.
        drop(guard);
    }
}
