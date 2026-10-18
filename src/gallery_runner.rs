//! gallery-dl download runner: spawn, parse progress, collect results.
//!
//! Spawns gallery-dl with the argv from [`gallery_argv`](crate::gallery_argv),
//! feeds stdout lines through [`gallery_progress`](crate::gallery_progress),
//! and reports via callbacks. Cancellation: drop the future; the child is
//! killed via ProcessGroupGuard.

use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::gallery_argv::{GalleryJob, download_argv};
use crate::gallery_progress::{GalleryProgress, ProgressLine, parse_progress_line};
use crate::gallery_tools::GalleryError;
use crate::video_spawn::ProcessGroupGuard;

/// Outcome of a gallery download.
#[derive(Debug)]
pub struct GalleryResult {
    /// Full paths of downloaded files (in completion order).
    pub files: Vec<PathBuf>,
    /// Files skipped (already existed).
    pub skipped: u32,
    /// gallery-dl exit code, if it exited non-zero but produced files.
    /// `None` means clean success.
    pub exit_code: Option<i32>,
}

/// Total files handled (downloaded + skipped) for display.
/// Skipped files were already on disk from a previous run.
pub fn done_count(files: usize, skipped: u32) -> u32 {
    files as u32 + skipped
}

/// Detail label for a gallery row: "Gallery • 1 file" / "Gallery • N files".
pub fn done_label(files: usize, skipped: u32) -> String {
    let done = done_count(files, skipped);
    if done == 1 {
        "Gallery \u{2022} 1 file".to_string()
    } else {
        format!("Gallery \u{2022} {done} files")
    }
}

/// Progress update from the runner.
#[derive(Debug, Clone)]
pub struct GalleryProgressUpdate {
    pub files_completed: u32,
    /// Last completed file path (gallery-dl doesn't name the in-progress file).
    pub last_completed_file: Option<String>,
}

/// Journal-safe stderr text: gallery-dl failure lines embed the media URL
/// with signed query tokens, so swap URL words for their path stem — same
/// convention as `video_probe::page_host` (journal never gets query strings).
/// User-facing error text keeps the raw line.
fn redact_query_tokens(text: &str) -> String {
    text.split_whitespace()
        .map(|word| {
            if url::Url::parse(word).is_ok()
                && let Some(cut) = word.find(['?', '#'])
            {
                format!("{}(...)", &word[..cut])
            } else {
                word.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Run a gallery download.
///
/// - `job`: the gallery job (URL, dest, filter, cookies)
/// - `on_progress`: called for meaningful progress updates (file completed,
///   skipped, or byte progress — not for log noise)
/// - Returns the downloaded files on success, or a GalleryError.
///
/// Cancellation: drop the future; the child and its whole process group are
/// SIGKILLed (own process group + `ProcessGroupGuard` + `kill_on_drop`).
pub async fn run_gallery_download(
    job: &GalleryJob,
    tool: &Path,
    cookies_file: Option<&Path>,
    user_agent: Option<&str>,
    on_progress: impl Fn(GalleryProgressUpdate),
) -> Result<GalleryResult, GalleryError> {
    let argv = download_argv(
        tool,
        &job.page_url,
        &job.dest_dir,
        cookies_file,
        user_agent,
        job.filter,
        job.speed_limit,
        job.range.as_deref(),
    );

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    // Pipe stdout for progress parsing; pipe stderr for failure context
    // (drained in a separate task so gallery-dl can't block on a full pipe).
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    // Own process group + kill-on-drop: cancel/abort/shutdown drops this
    // future, and only a group kill reaches gallery-dl's grandchildren too
    // (mirrors ytdlp_command; a bare killpg on the raw pid is an ESRCH
    // no-op when the child never became a group leader).
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    cmd.kill_on_drop(true);

    // Debug: log the full argv before spawning.
    crate::backend_debug::log_command(&cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| GalleryError::runtime(format!("couldn't spawn gallery-dl: {e}")))?;
    // Kill the child's group if the future is dropped (user cancel, app quit).
    let mut guard = ProcessGroupGuard::new(&child);

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| GalleryError::runtime("gallery-dl stdout not piped"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| GalleryError::runtime("gallery-dl stderr not piped"))?;

    // Drain stderr into a ring buffer (last 20 lines) in the background.
    // The JoinHandle is awaited after the child exits so the tail is complete.
    let stderr_lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let stderr_lines_clone = stderr_lines.clone();
    let drain_handle = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            // Debug: echo stderr live. The line is still captured below
            // for the failure tail.
            crate::backend_debug::echo_line(&line);
            let mut buf = stderr_lines_clone.lock().unwrap();
            buf.push(line);
            if buf.len() > 20 {
                buf.remove(0);
            }
        }
    });

    let mut lines = BufReader::new(stdout).lines();

    let mut progress = GalleryProgress::new();
    let mut files = Vec::new();
    // Last completed file path (gallery-dl doesn't name the file in progress lines).
    let mut last_completed: Option<String> = None;

    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|e| GalleryError::runtime(format!("couldn't read gallery-dl output: {e}")))?
    {
        let parsed = parse_progress_line(&line);
        // Only fire the callback for meaningful updates, not log noise.
        let meaningful = matches!(
            parsed,
            ProgressLine::Completed(_) | ProgressLine::Skipped(_)
        );
        if let ProgressLine::Completed(path) = &parsed {
            last_completed = Some(path.clone());
        }
        if let Some(path) = progress.feed(parsed) {
            files.push(PathBuf::from(path));
        }
        if meaningful {
            on_progress(GalleryProgressUpdate {
                files_completed: progress.files_completed,
                last_completed_file: last_completed.clone(),
            });
        }
    }

    let status = child
        .wait()
        .await
        .map_err(|e| GalleryError::runtime(format!("couldn't wait for gallery-dl: {e}")))?;
    // Reaped: disarm so Drop can't reach a recycled PGID (guard contract).
    guard.disarm();

    // The child has exited, so the stderr pipe is at EOF; awaiting the drain
    // guarantees the ring buffer has the complete tail.
    let _ = drain_handle.await;

    if !status.success() {
        let code = status.code();
        let stderr_tail = stderr_lines.lock().unwrap().join("\n");
        // Log a redacted tail (gallery-dl failure lines embed media URLs with
        // signed query tokens; the journal gets no query strings). The
        // user-facing message keeps the full tail (the dialog scrolls).
        tracing::warn!(stderr = %redact_query_tokens(&stderr_tail), "gallery-dl stderr tail");
        // Partial success: files were downloaded/skipped before the failure.
        // Return Ok with the exit code so the row shows Done with a warning.
        if !files.is_empty() || progress.files_skipped > 0 {
            return Ok(GalleryResult {
                files,
                skipped: progress.files_skipped,
                exit_code: code,
            });
        }
        // Total failure: include the full stderr tail and a mapped message.
        // gallery-dl exit codes are bit flags:
        // 1=unspecified, 2=cmdline, 4=http, 8=challenge,
        // 16=auth, 32=input, 128=os. Check the most specific bits first.
        let msg = match code.unwrap_or(0) {
            c if c & 128 != 0 => "gallery-dl: OS error",
            c if c & 16 != 0 => "gallery-dl: authentication failed",
            c if c & 8 != 0 => "gallery-dl: challenge error (Cloudflare/DDoS-Guard)",
            c if c & 32 != 0 => "gallery-dl: invalid input",
            c if c & 4 != 0 => "gallery-dl: HTTP error",
            c if c & 2 != 0 => "gallery-dl: invalid command-line arguments",
            c if c & 1 != 0 => "gallery-dl: unspecified error",
            _ => "gallery-dl failed",
        };
        return Err(GalleryError::runtime(format!(
            "{msg} (exit {}):\n{stderr_tail}",
            code.unwrap_or(-1)
        )));
    }

    // Classify the result: clean exit with zero files is a visible failure.
    let files =
        classify_result(files, progress.files_skipped, None).map_err(GalleryError::runtime)?;

    Ok(GalleryResult {
        files,
        skipped: progress.files_skipped,
        exit_code: None,
    })
}

/// Classify a gallery-dl run result for the UI.
/// Returns the user-facing outcome: Ok(files) for success, Err(msg) for failure.
pub(crate) fn classify_result(
    files: Vec<PathBuf>,
    skipped: u32,
    exit_code: Option<i32>,
) -> Result<Vec<PathBuf>, String> {
    // Clean exit with zero files and zero skipped: visible failure, not Done.
    if exit_code.is_none() && files.is_empty() && skipped == 0 {
        return Err("gallery-dl found no files (check login or filter)".to_string());
    }
    Ok(files)
}

/// Speed limit for gallery downloads, from settings.
/// Shared by the card enqueue and the video-fallback path.
pub fn gallery_speed_limit(settings: &crate::settings::AppSettings) -> Option<u64> {
    crate::download_rate::parse_rate(settings.speed_limit().trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gallery_argv::GalleryFilter;

    #[test]
    fn classify_result_clean_no_files_is_failure() {
        // Clean exit (None), zero files, zero skipped: visible failure.
        // Mutation: return Ok → assertion on Err fails.
        let err = classify_result(vec![], 0, None).unwrap_err();
        assert_eq!(err, "gallery-dl found no files (check login or filter)");
    }

    #[test]
    fn classify_result_with_files_is_ok() {
        let files = vec![PathBuf::from("/tmp/a.jpg")];
        assert!(classify_result(files, 0, None).is_ok());
    }

    #[test]
    fn classify_result_skipped_only_is_ok() {
        // All skipped (not downloaded): not a failure, just nothing new.
        assert!(classify_result(vec![], 3, None).is_ok());
    }

    #[test]
    fn classify_result_partial_with_exit_code_is_ok() {
        // Non-clean exit but files present: partial success (handled upstream).
        let files = vec![PathBuf::from("/tmp/a.jpg")];
        assert!(classify_result(files, 0, Some(4)).is_ok());
    }

    #[test]
    fn done_label_all_new() {
        assert_eq!(done_label(5, 0), "Gallery \u{2022} 5 files");
    }

    #[test]
    fn done_label_all_skipped() {
        assert_eq!(done_label(0, 3), "Gallery \u{2022} 3 files");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_kills_the_download_process_group() {
        // run_gallery_download must own a process group: dropping the future
        // (cancel/abort/shutdown) kills the child and everything it spawned —
        // the guard's killpg is a no-op unless the child became a group
        // leader. A fake tool writes a heartbeat file every 200 ms; after
        // the abort the heartbeat must freeze.
        let dir = std::env::temp_dir().join(format!("grab-runner-cancel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = dir.join("fake-gallery-dl.sh");
        let log = dir.join("heartbeat");
        std::fs::write(
            &tool,
            format!(
                "#!/bin/sh\nfor _ in $(seq 1 600); do echo tick >> '{}'; sleep 0.2; done\n",
                log.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let job = GalleryJob {
            item_id: 1,
            page_url: "https://example.com/x".to_string(),
            title: "x".to_string(),
            file_count: None,
            dest_dir: dir.clone(),
            filter: GalleryFilter::All,
            cookies_browser: "none".to_string(),
            speed_limit: None,
            range: None,
        };
        let task = tokio::spawn(async move {
            run_gallery_download(&job, &tool, None, None, |_| {}).await
        });

        // In flight: the heartbeat file exists and freshens.
        let started = std::time::Instant::now();
        while !log.exists() {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(5),
                "fake gallery-dl never wrote its first heartbeat"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;

        // Cancel like the DownloadManager does: abort the runner future.
        task.abort();
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let after = std::fs::metadata(&log).unwrap().modified().unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let settled = std::fs::metadata(&log).unwrap().modified().unwrap();
        assert_eq!(
            after, settled,
            "heartbeat must freeze once the future is dropped (process-group kill); \
             a surviving child would freshen it every 200 ms"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stderr_url_tokens_are_redacted_for_the_journal() {
        let raw = "[instagram][warning] 404 https://media.example/file?token=abc#frag plain";
        assert_eq!(
            redact_query_tokens(raw),
            "[instagram][warning] 404 https://media.example/file(...) plain"
        );
        // Words that are not URLs keep their glyph bytes.
        assert_eq!(redact_query_tokens("a?b#c"), "a?b#c");
        // Plain text passes through untouched.
        assert_eq!(redact_query_tokens("no urls here"), "no urls here");
    }

    #[test]
    fn done_label_mixed() {
        assert_eq!(done_label(2, 3), "Gallery \u{2022} 5 files");
    }

    #[test]
    fn done_label_singular() {
        assert_eq!(done_label(1, 0), "Gallery \u{2022} 1 file");
        assert_eq!(done_label(0, 1), "Gallery \u{2022} 1 file");
    }

    #[test]
    fn gallery_job_serializes() {
        // Queue store round-trip. item_id is #[serde(skip)]: restore assigns
        // a fresh id via claim_id, so it round-trips as 0.
        let job = GalleryJob {
            item_id: 42,
            page_url: "https://example.com/gallery".to_string(),
            title: "Test Gallery".to_string(),
            file_count: Some(10),
            dest_dir: PathBuf::from("/tmp/dest"),
            filter: GalleryFilter::ImagesOnly,
            cookies_browser: "none".to_string(),
            speed_limit: None,
            range: None,
        };
        let json = serde_json::to_string(&job).unwrap();
        // item_id skipped on serialize.
        assert!(!json.contains("item_id"));
        let back: GalleryJob = serde_json::from_str(&json).unwrap();
        assert_eq!(back.item_id, 0);
        assert_eq!(back.filter, GalleryFilter::ImagesOnly);
        assert_eq!(back.title, "Test Gallery");
        assert_eq!(back.file_count, Some(10));
    }
}
