//! Spawn plumbing + fetch resolve: process-group spawns, pipe drains, tree kills, output discovery.
//! Consumed by the dialog/runner through the `video` facade.

use crate::video_argv::{apply_proxy_env, proxy_cli_args};
use crate::video_probe::{
    page_host, parse_playlist_json, parse_single_video, pick_playlist_entry,
    playlist_resolve_error, retarget_story_items, sanitize_video_json,
};
use crate::video_progress::{last_log_line, trace_format_lines};
use crate::video_staging::{ensure_staging_dir, staging_root};
use crate::video_tools::{VideoError, ensure_tool_versions, ytdlp_identity_args};
use crate::video_types::{FetchedVideo, ProbeResult, VideoInfo};
use gettextrs::gettext;
use std::path::{Path, PathBuf};
use std::time::Duration;
use yt_dlp::client::deps::Libraries;
use yt_dlp::model::Video;

/// How long one metadata extraction may take before it counts as failed (never park the dialog on its spinner).
const FETCH_TIMEOUT_SECS: u64 = 60;

/// Byte ceilings for drained child pipes (the timeout bounds time, not memory; stdout carries the JSON document).
const MAX_STDOUT_BYTES: u64 = 32 * 1024 * 1024;
const MAX_STDERR_BYTES: u64 = 1024 * 1024;

/// Drain one child pipe with a hard byte ceiling (overflow fails as QuotaExceeded, never a bigger buffer).
/// Always drains to EOF into a discard buffer so the child can never block on a full pipe.
async fn read_bounded(
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
    args.extend(ytdlp_identity_args(cookies_browser, None, url));
    // Direct spawn with the same semantics (concurrent drain, timeout kill, stderr detail on failure).
    let mut cmd = ytdlp_command(youtube_bin);
    cmd.args(&args);
    apply_proxy_env(&mut cmd, fetch_proxy);
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

/// Extract metadata for one URL (the page URL is what survives restarts, not the media URLs).
pub async fn fetch_video_infos(
    libs: Libraries,
    url: String,
    cookies_browser: String,
    newest_first: bool,
    fetch_proxy: Option<crate::net_types::ResolvedProxy>,
) -> Result<ProbeResult, VideoError> {
    let handle = crate::runtime::tokio_rt().spawn(async move {
        let (yt_version, _ff_version) = ensure_tool_versions(&libs).await?;
        tracing::info!(yt_dlp = %yt_version, url_host = %page_host(&url), "resolving video page");
        // quickjs-ng is the JS runtime Grab pins for YouTube; make sure it's
        // installed before a YouTube spawn that may need to solve JS challenges.
        crate::video_tools::ensure_quickjs(&url).await?;
        let out = staging_root();
        ensure_staging_dir(&out)?;
        let mut value = match tokio::time::timeout(
            Duration::from_secs(FETCH_TIMEOUT_SECS),
            fetch_raw_dump_json(
                &libs.youtube,
                &url,
                &cookies_browser,
                Duration::from_secs(300),
                fetch_proxy.as_ref(),
                true,
            ),
        )
        .await
        {
            Ok(Ok(value)) => value,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(VideoError::fetch(gettext("the lookup timed out")));
            }
        };
        // Stub entries would choke the strict video model.
        if let Some(mut playlist) = parse_playlist_json(&value, &url) {
            retarget_story_items(&url, &mut playlist);
            return Ok::<_, VideoError>(ProbeResult::Playlist(playlist));
        }
        sanitize_video_json(&mut value);
        let video: Video = serde_json::from_value(value).map_err(VideoError::fetch)?;
        Ok::<_, VideoError>(ProbeResult::Single(VideoInfo::from(
            &video,
            &url,
            newest_first,
        )))
    });
    match handle.await {
        Ok(r) => r,
        Err(e) => Err(VideoError::runtime(&e)),
    }
}

/// yt-dlp spawn with null stdin, piped outputs, and its own process group (so timeouts kill the whole tree).
/// The user lib dir heads PATH so yt-dlp's challenge solver finds Grab-installed
/// `qjs`; a system `qjs` elsewhere on PATH still works when Grab never installed one.
pub(crate) fn ytdlp_command(youtube_bin: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(youtube_bin);
    // Impersonate a real browser's TLS fingerprint where the binary supports
    // it (curl_cffi): some sites cut plain-HTTP-client streams off mid-flight.
    // Probed once per binary and cached — a PATH-provided yt-dlp without the
    // dependency hard-fails on the flag, so never pass it blindly.
    if crate::video_tools::ytdlp_supports_impersonation(youtube_bin) {
        cmd.arg("--impersonate").arg("chrome");
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let lib_dir = crate::video_tools::user_lib_dir();
    let path = std::env::var_os("PATH").map(|p| {
        let mut paths = vec![lib_dir];
        paths.extend(std::env::split_paths(&p));
        std::env::join_paths(paths).unwrap_or(p)
    });
    if let Some(path) = path {
        cmd.env("PATH", path);
    }
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    cmd
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
        let mut buf = [0u8; 4096];
        loop {
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
}
