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
}

/// Progress update from the runner.
#[derive(Debug, Clone)]
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
    on_progress: impl Fn(GalleryProgressUpdate),
) -> Result<GalleryResult, GalleryError> {
    let argv = download_argv(
        tool,
        &job.page_url,
        &job.dest_dir,
        job.cookies_file.as_deref(),
        job.filter,
    );

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    // Pipe stdout for progress parsing; stderr inherits (gallery-dl logs there).
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::inherit());

    let mut child = cmd
        .spawn()
        .map_err(|e| GalleryError::runtime(format!("couldn't spawn gallery-dl: {e}")))?;
    // Kill the child if the future is dropped (user cancel, app quit).
    let _guard = ProcessGroupGuard::new(&child);

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| GalleryError::runtime("gallery-dl stdout not piped"))?;
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
            ProgressLine::Completed(_) | ProgressLine::Skipped(_) | ProgressLine::Progress { .. }
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
                bytes_downloaded: progress.bytes_downloaded,
                bytes_total: progress.bytes_total,
                last_completed_file: last_completed.clone(),
            });
        }
    }

    let status = child
        .wait()
        .await
        .map_err(|e| GalleryError::runtime(format!("couldn't wait for gallery-dl: {e}")))?;

    if !status.success() {
        return Err(GalleryError::runtime(format!(
            "gallery-dl failed (exit {})",
            status.code().unwrap_or(-1)
        )));
    }

    Ok(GalleryResult {
        files,
        skipped: progress.files_skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gallery_argv::GalleryFilter;

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
            cookies_file: None,
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
