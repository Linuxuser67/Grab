//! gallery-dl download runner: spawn, parse progress, collect results.
//!
//! Spawns gallery-dl with the argv from [`gallery_argv`](crate::gallery_argv),
//! feeds the merged child output stream through [`gallery_progress`](crate::gallery_progress),
//! and reports via callbacks. Cancellation: drop the future; the child is
//! killed via ProcessGroupGuard.

use std::path::Path;
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::Command;

use crate::gallery_argv::{GalleryJob, download_argv_with_proxy};
use crate::gallery_progress::{GalleryProgress, ProgressLine, parse_progress_line};
use crate::gallery_tools::GalleryError;
use crate::video_spawn::ProcessGroupGuard;

fn gallery_event_nonce() -> Result<String, GalleryError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| {
        GalleryError::runtime(format!("couldn't generate gallery event nonce: {error}"))
    })?;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut nonce = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        nonce.push(char::from(HEX[(byte >> 4) as usize]));
        nonce.push(char::from(HEX[(byte & 0x0f) as usize]));
    }
    Ok(nonce)
}


/// Outcome of a gallery download.
#[derive(Debug)]
pub struct GalleryResult {
    /// Number of files completed by gallery-dl.
    pub files_completed: u32,
    /// Files skipped (already existed).
    pub skipped: u32,
    /// gallery-dl exit code, if it exited non-zero after partial success.
    /// `None` means clean success.
    pub exit_code: Option<i32>,
}

/// Total files handled (downloaded + skipped), for internal progress bookkeeping.
/// Skipped files were already on disk from a previous run.
pub fn done_count(files_completed: u32, skipped: u32) -> u32 {
    files_completed.saturating_add(skipped)
}

#[cfg(test)]
mod done_count_tests {
    use super::done_count;

    #[test]
    fn includes_downloaded_and_previously_skipped_files() {
        assert_eq!(done_count(5, 0), 5);
        assert_eq!(done_count(0, 3), 3);
        assert_eq!(done_count(2, 3), 5);
    }

    #[test]
    fn saturates_the_total_file_count() {
        assert_eq!(done_count(u32::MAX, 1), u32::MAX);
    }
}

/// Progress update from the runner.
#[derive(Debug, Clone)]
pub struct GalleryProgressUpdate {
    pub files_done: u32,
    /// Percentage reported for the current file, when its size is known.
    pub current_file_percent: Option<u8>,
    /// Full path announced by gallery-dl's start event, if a file is active.
    pub current_file_path: Option<String>,
    /// The stdout file event that caused this update. Progress-only updates use None.
    pub event: Option<ProgressLine>,
    /// Last completed file path for the aggregate gallery detail.
    pub last_completed_file: Option<String>,
}

/// Journal-safe stderr text: gallery-dl failure lines embed the media URL
/// with signed query tokens; the journal never gets them. User-facing error
/// text keeps the raw line.
/// Redacts the query/fragment of any `scheme://` URL in the text, scanning
/// token-wise between whitespace/quote/bracket boundaries instead of
/// `Url::parse`-ing whitespace-delimited words: quoted (`'https://x/?sig=…'`)
/// and bracketed (`(https://x/?sig=…)`) shapes fail `Url::parse`, which let
/// their signed tokens reach the journal in the clear. Non-URL text (no
/// `://`) passes through untouched, including punctuation.
fn redact_query_tokens(text: &str) -> String {
    let is_special =
        |b: u8| b.is_ascii_whitespace() || matches!(b, b'"' | b'\'' | b'(' | b')' | b'[' | b']');
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] < 0x80 && is_special(bytes[i]) {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }
        // Token: multibyte UTF-8 is never special, so these slice bounds
        // always sit on char boundaries.
        let start = i;
        while i < bytes.len() && !(bytes[i] < 0x80 && is_special(bytes[i])) {
            i += 1;
        }
        let token = &text[start..i];
        match token.find("://").and_then(|_| token.find(['?', '#'])) {
            Some(cut) => {
                out.push_str(&token[..cut]);
                out.push_str("(...)");
            }
            None => out.push_str(token),
        }
    }
    out
}

/// Run a gallery download.
///
/// - `job`: the gallery job (URL, dest, filter, cookies)
/// - `on_progress`: called for file completion, skips, and gallery-dl's
///   per-file percentage updates (not for log noise)
/// - Returns the downloaded files on success, or a GalleryError.
///
/// Cancellation: drop the future; the child and its whole process group are
/// SIGKILLed (own process group + `ProcessGroupGuard` + `kill_on_drop`).
#[cfg(test)]
pub async fn run_gallery_download(
    job: &GalleryJob,
    tool: &Path,
    cookies_file: Option<&Path>,
    user_agent: Option<&str>,
    on_progress: impl Fn(GalleryProgressUpdate),
) -> Result<GalleryResult, GalleryError> {
    run_gallery_download_with_proxy(job, tool, cookies_file, user_agent, None, on_progress).await
}

pub async fn run_gallery_download_with_proxy(
    job: &GalleryJob,
    tool: &Path,
    cookies_file: Option<&Path>,
    user_agent: Option<&str>,
    proxy: Option<&crate::net_types::ResolvedProxy>,
    on_progress: impl Fn(GalleryProgressUpdate),
) -> Result<GalleryResult, GalleryError> {
    let untrusted_handoff = url::Url::parse(&job.page_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .is_some_and(|host| crate::download_net::is_untrusted_handoff_host(&host));
    // Never let a user-configured proxy supersede or compete with the guarded
    // proxy for an untrusted browser handoff.
    if untrusted_handoff && proxy.is_some() {
        return Err(GalleryError::probe(
            "Cannot safely use a configured proxy for an untrusted browser handoff".to_string(),
        ));
    }
    let event_nonce = gallery_event_nonce()?;
    let argv = download_argv_with_proxy(
        tool,
        &job.page_url,
        &job.dest_dir,
        cookies_file,
        user_agent,
        job.filter,
        job.speed_limit,
        job.range.as_deref(),
        proxy,
        &event_nonce,
    );
    let guarded_proxy = if untrusted_handoff {
        crate::video_spawn::validate_guarded_proxy_args(&argv).map_err(GalleryError::probe)?;
        Some(crate::subprocess_proxy::ensure_started().map_err(GalleryError::probe)?)
    } else {
        None
    };

    // gallery-dl writes file lifecycle markers to stdout and byte progress
    // to stderr. Merge both streams in the child so the runner observes their
    // write order; separate pipes can deliver a fast file's final percentage
    // after its completion event and misattribute it to the next file.
    //
    // Arguments stay positional: the URL, cookie path and proxy flags are not
    // interpolated into the shell script or subject to shell expansion.
    let mut cmd = Command::new("/bin/sh");
    crate::video_spawn::prepend_user_lib_to_path(&mut cmd);
    cmd.arg("-c").arg("exec \"$@\" 2>&1").arg("grab-gallery-dl");
    cmd.args(&argv);
    crate::video_spawn::apply_configured_proxy_env(&mut cmd, proxy);
    crate::video_spawn::apply_guarded_proxy_env(&mut cmd, guarded_proxy.as_deref());
    // Python output must be flushed promptly so merged file and progress records
    // preserve the order observed by the runner.
    cmd.env("PYTHONUNBUFFERED", "1");
    // The fixture process uses the same run identity embedded in gallery-dl's
    // output templates; the nonce is not a credential and is not logged.
    cmd.env("GRAB_GALLERY_EVENT_NONCE", &event_nonce);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
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
        .ok_or_else(|| GalleryError::runtime("gallery-dl output not piped"))?;

    // stdout and stderr were redirected onto the same child pipe above.
    // Parse all events in that single ordered stream rather than trying to
    // reconstruct ordering between independently scheduled readers.
    const MAX_OUTPUT_RECORD_BYTES: usize = 64 * 1024;
    let mut reader = BufReader::new(stdout);
    let mut line_buf = Vec::new();
    let mut chunk = [0; 4096];
    let mut discard_line = false;
    let mut diagnostic_lines = Vec::<String>::new();

    let mut progress = GalleryProgress::new();
    let mut last_completed: Option<String> = None;

    let mut process_line = |line: String| {
        let parsed = parse_progress_line(&line, &event_nonce);
        match parsed {
            ProgressLine::Progress(percent) => {
                progress.feed(ProgressLine::Progress(percent));
                on_progress(GalleryProgressUpdate {
                    files_done: progress.files_done(),
                    current_file_percent: progress.current_file_percent,
                    current_file_path: progress.current_file_path.clone(),
                    event: None,
                    last_completed_file: last_completed.clone(),
                });
            }
            ProgressLine::Other(line) => {
                if !line.is_empty() {
                    crate::backend_debug::echo_line(&line);
                    diagnostic_lines.push(line);
                    if diagnostic_lines.len() > 20 {
                        diagnostic_lines.remove(0);
                    }
                }
            }
            event => {
                if let ProgressLine::Completed(path) = &event {
                    last_completed = Some(path.clone());
                }
                progress.feed(event.clone());
                on_progress(GalleryProgressUpdate {
                    files_done: progress.files_done(),
                    current_file_percent: progress.current_file_percent,
                    current_file_path: progress.current_file_path.clone(),
                    event: Some(event),
                    last_completed_file: last_completed.clone(),
                });
            }
        }
    };

    loop {
        let read = reader.read(&mut chunk).await.map_err(|error| {
            GalleryError::runtime(format!("couldn't read gallery-dl output: {error}"))
        })?;
        if read == 0 {
            break;
        }

        for &byte in &chunk[..read] {
            if matches!(byte, b'\r' | b'\n') {
                if !discard_line && !line_buf.is_empty() {
                    let line = String::from_utf8_lossy(&line_buf).into_owned();
                    process_line(line);
                }
                line_buf.clear();
                discard_line = false;
            } else if !discard_line {
                if line_buf.len() >= MAX_OUTPUT_RECORD_BYTES {
                    line_buf.clear();
                    discard_line = true;
                } else {
                    line_buf.push(byte);
                }
            }
        }
    }
    if !discard_line && !line_buf.is_empty() {
        process_line(String::from_utf8_lossy(&line_buf).into_owned());
    }
    // Release the closure's mutable borrows before reading the final counters.
    drop(process_line);

    let status = child
        .wait()
        .await
        .map_err(|e| GalleryError::runtime(format!("couldn't wait for gallery-dl: {e}")))?;
    // Reaped: disarm so Drop can't reach a recycled PGID (guard contract).
    guard.disarm();

    if !status.success() {
        let code = status.code();
        let stderr_tail = diagnostic_lines.join("\n");
        // Log a redacted tail (gallery-dl failure lines embed media URLs with
        // signed query tokens; the journal gets no query strings). The
        // user-facing message keeps the full tail (the dialog scrolls).
        tracing::warn!(stderr = %redact_query_tokens(&stderr_tail), "gallery-dl stderr tail");
        // Partial success: files were downloaded/skipped before the failure.
        // Return Ok with the exit code so the row shows Done with a warning.
        if progress.files_completed > 0 || progress.files_skipped > 0 {
            return Ok(GalleryResult {
                files_completed: progress.files_completed,
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
    classify_result(progress.files_completed, progress.files_skipped, None)
        .map_err(GalleryError::runtime)?;

    Ok(GalleryResult {
        files_completed: progress.files_completed,
        skipped: progress.files_skipped,
        exit_code: None,
    })
}

/// Classify a gallery-dl run result for the UI.
/// A non-clean exit with completed or skipped files remains a partial success.
pub(crate) fn classify_result(
    files_completed: u32,
    skipped: u32,
    exit_code: Option<i32>,
) -> Result<(), String> {
    // Clean exit with zero files and zero skipped: visible failure, not Done.
    if exit_code.is_none() && files_completed == 0 && skipped == 0 {
        return Err("gallery-dl found no files (check login or filter)".to_string());
    }
    Ok(())
}

/// Speed limit for gallery downloads, from settings.
/// Shared by the card enqueue and the video-fallback path.
pub fn gallery_speed_limit(settings: &crate::settings::AppSettings) -> Option<u64> {
    crate::download_rate::parse_rate(settings.speed_limit().trim())
}

#[cfg(test)]
mod gallery_event_nonce_tests {
    use super::gallery_event_nonce;

    #[test]
    fn nonce_is_hex_and_unique_per_run() {
        let first = gallery_event_nonce().expect("operating-system randomness");
        let second = gallery_event_nonce().expect("operating-system randomness");
        assert_eq!(first.len(), 32);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gallery_argv::GalleryFilter;
    use std::path::PathBuf;

    #[test]
    fn classify_result_clean_no_files_is_failure() {
        // Clean exit (None), zero files, zero skipped: visible failure.
        // Mutation: return Ok → assertion on Err fails.
        let err = classify_result(0, 0, None).unwrap_err();
        assert_eq!(err, "gallery-dl found no files (check login or filter)");
    }

    #[test]
    fn classify_result_with_files_is_ok() {
        assert!(classify_result(1, 0, None).is_ok());
    }

    #[test]
    fn classify_result_skipped_only_is_ok() {
        // All skipped (not downloaded): not a failure, just nothing new.
        assert!(classify_result(0, 3, None).is_ok());
    }

    #[test]
    fn classify_result_partial_with_exit_code_is_ok() {
        // Non-clean exit but files present: partial success (handled upstream).
        assert!(classify_result(1, 0, Some(4)).is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runner_preserves_order_of_start_progress_and_completion_events() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::sync::{Arc, Mutex};

        let dir = std::env::temp_dir().join(format!(
            "grab-runner-per-file-progress-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create runner fixture directory");
        let tool = dir.join("fake-gallery-dl.sh");
        let first = dir.join("one.jpg");
        let second = dir.join("two.jpg");
        let script = format!(
            "#!/bin/sh\n[ \"$PYTHONUNBUFFERED\" = \"1\" ] || exit 99\nprintf '%s\\n' \"@GRAB:$GRAB_GALLERY_EVENT_NONCE:START:{}\"\nprintf '50%%\\n' >&2\nprintf '%s\\n' \"@GRAB:$GRAB_GALLERY_EVENT_NONCE:DONE:{}\"\nprintf '%s\\n' \"@GRAB:$GRAB_GALLERY_EVENT_NONCE:SKIP:{}\"\n",
            first.display(),
            first.display(),
            second.display(),
        );
        std::fs::write(&tool, script).expect("write fake gallery-dl");
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755))
            .expect("make fake gallery-dl executable");

        let job = GalleryJob {
            item_id: 7,
            page_url: "https://example.com/gallery".to_string(),
            title: "Gallery".to_string(),
            file_count: Some(2),
            dest_dir: dir.clone(),
            filter: GalleryFilter::All,
            cookies_browser: "none".to_string(),
            speed_limit: None,
            range: None,
            file_rows: Vec::new(),
        };
        let updates = Arc::new(Mutex::new(Vec::<GalleryProgressUpdate>::new()));
        let captured = Arc::clone(&updates);
        let result = run_gallery_download(&job, &tool, None, None, move |update| {
            captured.lock().expect("progress updates lock").push(update);
        })
        .await
        .expect("fixture gallery download");

        assert_eq!(result.files_completed, 1);
        assert_eq!(result.skipped, 1);
        let first_path = first.to_string_lossy().into_owned();
        let second_path = second.to_string_lossy().into_owned();
        let updates = updates.lock().expect("progress updates lock");
        assert!(updates.iter().any(|update| {
            matches!(
                update.event.as_ref(),
                Some(ProgressLine::Started(path)) if path == &first_path
            ) && update.current_file_path.as_deref() == Some(first_path.as_str())
        }));
        assert!(updates.iter().any(|update| {
            update.event.is_none()
                && update.current_file_path.as_deref() == Some(first_path.as_str())
                && update.current_file_percent == Some(50)
        }));
        assert!(updates.iter().any(|update| {
            matches!(
                update.event.as_ref(),
                Some(ProgressLine::Skipped(path)) if path == &second_path
            )
        }));

        let started_at = updates
            .iter()
            .position(|update| matches!(update.event, Some(ProgressLine::Started(_))))
            .expect("start event");
        let progress_at = updates
            .iter()
            .position(|update| update.event.is_none() && update.current_file_percent == Some(50))
            .expect("per-file progress event");
        let completed_at = updates
            .iter()
            .position(|update| matches!(update.event, Some(ProgressLine::Completed(_))))
            .expect("completion event");
        let skipped_at = updates
            .iter()
            .position(|update| matches!(update.event, Some(ProgressLine::Skipped(_))))
            .expect("skip event");
        assert!(started_at < progress_at);
        assert!(progress_at < completed_at);
        assert!(completed_at < skipped_at);

        std::fs::remove_dir_all(&dir).ok();
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
            file_rows: Vec::new(),
        };
        let task =
            tokio::spawn(
                async move { run_gallery_download(&job, &tool, None, None, |_| {}).await },
            );

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
        // Quoted and bracketed contexts used to dodge the redaction because
        // Url::parse rejects the word with its delimiter; both are scanned
        // out now.
        assert_eq!(
            redact_query_tokens("'https://media.example/file?token=abc'"),
            "'https://media.example/file(...)'"
        );
        assert_eq!(
            redact_query_tokens("(https://media.example/file?token=abc)(more)"),
            "(https://media.example/file(...))(more)"
        );
        assert_eq!(
            redact_query_tokens("]https://media.example/file#frag[ok"),
            "]https://media.example/file(...)[ok"
        );
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
            file_rows: Vec::new(),
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
