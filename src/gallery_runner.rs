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
#[allow(dead_code)] // Fields populated for API completeness; row UI uses files_completed for now.
pub struct GalleryProgressUpdate {
    pub files_completed: u32,
    pub files_total: Option<u32>,
    pub bytes_downloaded: u64,
    pub bytes_total: Option<u64>,
    /// Last completed file path (gallery-dl doesn't name the in-progress file).
    pub last_completed_file: Option<String>,
}

/// Run a gallery download.
///
/// - `job`: the gallery job (URL, dest, filter, cookies)
/// - `on_progress`: called for meaningful progress updates (file completed,
///   skipped, or byte progress — not for log noise)
/// - Returns the downloaded files on success, or a GalleryError.
///
/// Cancellation: drop the future; the child is killed via ProcessGroupGuard.
pub async fn run_gallery_download(
    job: &GalleryJob,
    tool: &Path,
    cookies_file: Option<&Path>,
    on_progress: impl Fn(GalleryProgressUpdate),
) -> Result<GalleryResult, GalleryError> {
    let argv = download_argv(
        tool,
        &job.page_url,
        &job.dest_dir,
        cookies_file,
        job.filter,
        job.speed_limit,
    );

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    // Pipe stdout for progress parsing; pipe stderr for failure context
    // (drained in a separate task so gallery-dl can't block on a full pipe).
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| GalleryError::runtime(format!("couldn't spawn gallery-dl: {e}")))?;
    // Kill the child if the future is dropped (user cancel, app quit).
    let _guard = ProcessGroupGuard::new(&child);

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
                files_total: job.file_count,
                // Byte progress disabled (downloader.http.progress=null);
                // kept for the video-fallback path until B4 resolves it.
                bytes_downloaded: 0,
                bytes_total: None,
                last_completed_file: last_completed.clone(),
            });
        }
    }

    let status = child
        .wait()
        .await
        .map_err(|e| GalleryError::runtime(format!("couldn't wait for gallery-dl: {e}")))?;

    // The child has exited, so the stderr pipe is at EOF; awaiting the drain
    // guarantees the ring buffer has the complete tail.
    let _ = drain_handle.await;

    if !status.success() {
        let code = status.code();
        let stderr_tail = stderr_lines.lock().unwrap().join("\n");
        // Partial success: files were downloaded/skipped before the failure.
        // Return Ok with the exit code so the row shows Done with a warning.
        if !files.is_empty() || progress.files_skipped > 0 {
            return Ok(GalleryResult {
                files,
                skipped: progress.files_skipped,
                exit_code: code,
            });
        }
        // Total failure: include the stderr tail and a mapped message.
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
            "{msg} (exit {}): {stderr_tail}",
            code.unwrap_or(-1)
        )));
    }

    Ok(GalleryResult {
        files,
        skipped: progress.files_skipped,
        exit_code: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gallery_argv::GalleryFilter;

    #[test]
    fn done_label_all_new() {
        assert_eq!(done_label(5, 0), "Gallery \u{2022} 5 files");
    }

    #[test]
    fn done_label_all_skipped() {
        assert_eq!(done_label(0, 3), "Gallery \u{2022} 3 files");
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
        // Queue store round-trip.
        let job = GalleryJob {
            item_id: 42,
            page_url: "https://example.com/gallery".to_string(),
            title: "Test Gallery".to_string(),
            file_count: Some(10),
            dest_dir: PathBuf::from("/tmp/dest"),
            filter: GalleryFilter::ImagesOnly,
            cookies_browser: "none".to_string(),
            speed_limit: None,
        };
        let json = serde_json::to_string(&job).unwrap();
        let back: GalleryJob = serde_json::from_str(&json).unwrap();
        assert_eq!(back.item_id, 42);
        assert_eq!(back.filter, GalleryFilter::ImagesOnly);
        assert_eq!(back.title, "Test Gallery");
        assert_eq!(back.file_count, Some(10));
    }
}
