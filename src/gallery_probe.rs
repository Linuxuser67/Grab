//! gallery-dl probe: JSON metadata extraction, content classification, routing.
//!
//! On paste, Grab spawns the yt-dlp probe (existing) and the gallery-dl probe
//! (here) concurrently. This module parses gallery-dl's `-j` output and decides
//! which mode the New Download card shows, per the routing table in the design.
//! The file picker's enumeration streams the same messages line by line
//! (`output.jsonl=true`), so the picker can open on the first item.

use std::path::Path;
use std::time::Duration;

use crate::gallery_tools::GalleryError;

/// Timeout for the gallery-dl probe (same as the yt-dlp probe).
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Upper bound for the file picker enumeration. The picker virtualizes its
/// rows, so the UI cost is independent of the count; the cap only bounds
/// gallery-dl's own listing walk and the buffered item list. `--range`
/// keeps gallery-dl itself from walking further.
pub(crate) const ENUMERATE_MAX_FILES: usize = 5000;

/// One file from `gallery-dl -j` output. Extractors emit wildly different
/// metadata keys; `filename`/`extension`/`category` are the reliable core,
/// everything else lands in `extra` for best-effort title derivation.
/// Some extractors omit `extension`/`category`, hence the defaults.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct GalleryItem {
    pub filename: String,
    #[serde(default)]
    pub extension: String,
    #[serde(default)]
    pub category: String,
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// gallery-dl `-j` message types (first array element).
/// Verified against gallery-dl 1.32.16: messages are `[type, ...]` arrays.
mod msg_type {
    /// Directory/context message: `[2, {metadata}]`.
    pub const DIRECTORY: i64 = 2;
    /// File message: `[3, "url", {metadata}]`.
    pub const FILE: i64 = 3;
    /// Error message: `[-1, {error, message}]`.
    pub const ERROR: i64 = -1;
}

/// Content type derived from the probe JSON's `extension` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GalleryContent {
    Video,
    Image,
    Unknown,
}

impl GalleryContent {
    pub fn from_extension(ext: &str) -> Self {
        let lower = ext.to_ascii_lowercase();
        if crate::gallery_argv::VIDEO_EXTENSIONS.contains(&lower.as_str()) {
            Self::Video
        } else if crate::gallery_argv::IMAGE_EXTENSIONS.contains(&lower.as_str()) {
            Self::Image
        } else {
            Self::Unknown
        }
    }
}

/// Whether a URL points directly to a media file (by extension). Used to skip
/// the gallery-dl probe when yt-dlp fails: a direct `.mp4` link isn't a gallery
/// page, so trying gallery-dl is wasted time.
pub fn url_is_direct_media(url: &str) -> bool {
    // Also cover audio extensions for direct downloads.
    const AUDIO: &[&str] = &[
        "mp3", "m4a", "aac", "ogg", "oga", "opus", "flac", "wav", "wma", "alac", "aiff",
    ];
    // Strip query/fragment, then take the path after the authority so a
    // host-only URL like https://example.mov isn't read as extension "mov".
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    let path = match without_query.split_once("://") {
        Some((_, rest)) => rest.split('/').skip(1).collect::<Vec<_>>().join("/"),
        None => without_query.to_string(),
    };
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    if ext.is_empty() || ext.contains('/') {
        return false;
    }
    !matches!(
        GalleryContent::from_extension(&ext),
        GalleryContent::Unknown
    ) || AUDIO.contains(&ext.as_str())
}

/// Where the New Download card should land after both probes complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// yt-dlp found video. Gallery toggle iff gallery-dl also found images.
    VideoYtDlp { gallery_toggle: bool },
    /// yt-dlp found nothing; gallery-dl found video (e.g. Instagram stories).
    VideoGalleryDl,
    /// yt-dlp found nothing; gallery-dl found images.
    Gallery,
    /// Neither probe found anything usable.
    None,
}

/// Sticky per-host mode preference: which mode the user picked last time
/// both were viable. Only honored when both modes are viable now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StickyMode {
    Video,
    Gallery,
}

/// Routing decision from both probe results.
///
/// `ytdlp_video`: whether the yt-dlp probe found a playable video.
/// `gallery_items`: gallery-dl `-j` items (`None` = probe failed/unsupported).
/// `host_sticky`: the user's per-host mode preference, if any.
pub fn route(
    ytdlp_video: bool,
    gallery_items: Option<&[GalleryItem]>,
    host_sticky: Option<StickyMode>,
) -> ProbeOutcome {
    // Sticky preference wins when both modes are viable; otherwise the table decides.
    let (has_video, has_images) = gallery_items
        .map(|items| {
            let mut video = false;
            let mut images = false;
            for item in items {
                match GalleryContent::from_extension(&item.extension) {
                    GalleryContent::Video => video = true,
                    GalleryContent::Image => images = true,
                    GalleryContent::Unknown => {}
                }
            }
            (video, images)
        })
        .unwrap_or((false, false));

    match (ytdlp_video, has_video, has_images) {
        (true, _, true) => {
            // Both viable: honor sticky, default Video.
            match host_sticky {
                Some(StickyMode::Gallery) => ProbeOutcome::Gallery,
                _ => ProbeOutcome::VideoYtDlp {
                    gallery_toggle: true,
                },
            }
        }
        (true, _, false) => ProbeOutcome::VideoYtDlp {
            gallery_toggle: false,
        },
        (false, true, _) => ProbeOutcome::VideoGalleryDl,
        (false, false, true) => ProbeOutcome::Gallery,
        (false, false, false) => ProbeOutcome::None,
    }
}

/// Route a probe using the original URL and whether yt-dlp resolved it as a
/// playlist. Instagram Highlights are collection-only URLs: when gallery-dl
/// confirms video entries, use its picker/download path so each picked item is
/// selected from gallery-dl's range rather than re-resolving the whole tray
/// through yt-dlp's Instagram story extractor.
pub(crate) fn route_for_url(
    url: &str,
    ytdlp_video: bool,
    ytdlp_playlist: bool,
    gallery_items: Option<&[GalleryItem]>,
    host_sticky: Option<StickyMode>,
) -> ProbeOutcome {
    let is_instagram_highlights = url::Url::parse(url).is_ok_and(|parsed| {
        matches!(
            parsed.host_str(),
            Some("instagram.com") | Some("www.instagram.com")
        ) && parsed.path_segments().is_some_and(|segments| {
            let parts: Vec<_> = segments.filter(|segment| !segment.is_empty()).collect();
            parts.len() == 3
                && parts[0] == "stories"
                && parts[1] == "highlights"
                && !parts[2].is_empty()
                && parts[2].chars().all(|c| c.is_ascii_digit())
        })
    });
    let has_gallery_video = gallery_items.is_some_and(|items| {
        items
            .iter()
            .any(|item| GalleryContent::from_extension(&item.extension) == GalleryContent::Video)
    });
    if is_instagram_highlights && ytdlp_playlist && has_gallery_video {
        ProbeOutcome::VideoGalleryDl
    } else {
        route(ytdlp_video, gallery_items, host_sticky)
    }
}

/// Whether a probe error should be surfaced to the user instead of falling
/// through to the plain-download path. Auth/HTTP errors mean the URL needs
/// login; "unsupported URL" and timeouts mean gallery-dl just can't handle it.
pub fn should_surface(e: &GalleryError) -> bool {
    match e {
        GalleryError::Probe(msg) => {
            msg.contains("AuthenticationError")
                || msg.contains("AuthorizationError")
                || msg.contains("AuthRequired")
                || msg.contains("ChallengeError")
                || msg.contains("HttpError")
        }
        // Install/runtime errors (spawn failures, timeouts) fall through.
        _ => false,
    }
}

/// Best-effort title from probe metadata. First non-empty wins:
/// `title` → `user`/`author` → `category` → URL-derived filename.
pub fn probe_title(item: &GalleryItem, url: &str) -> String {
    let str_field = |key: &str| {
        item.extra
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    if let Some(t) = str_field("title") {
        return t.to_string();
    }
    if let Some(u) = str_field("user").or_else(|| str_field("author")) {
        return u.to_string();
    }
    if !item.category.trim().is_empty() {
        return item.category.trim().to_string();
    }
    // Fallback: filename from URL (percent-decoded, validated).
    crate::file_names::filename_from_url(url)
}

/// Run the gallery-dl probe: `gallery-dl --config-ignore -j --no-download
/// --range 1-5 <url>`. Document mode, not jsonl: the probe isn't streamed,
/// and jsonl mode drops a fatal extraction error's `[-1]` message (see
/// `enumerate_gallery_files_streamed`).
///
/// Returns `Ok(Some(items))` on success, `Ok(None)` when the URL is unsupported
/// (exit 64) or yields no files. `Err` on timeout, spawn failure, or any other
/// non-zero exit — the card then shows "couldn't preview" but Add stays enabled
/// (optimistic path).
pub async fn probe_gallery(
    tool: &Path,
    url: &str,
    cookies_file: Option<&Path>,
    user_agent: Option<&str>,
) -> Result<Option<Vec<GalleryItem>>, GalleryError> {
    let mut cmd = tokio::process::Command::new(tool);
    // A timed-out probe must not leave gallery-dl running: dropping the
    // timed `cmd.output()` future in `run_to_completion` drops the child,
    // and with kill_on_drop it is SIGKILLed instead of orphaned.
    cmd.kill_on_drop(true);
    cmd.arg("--config-ignore")
        .arg("-j")
        .arg("--no-download")
        .arg("--range")
        .arg("1-5")
        // Don't let gallery-dl write back to Grab's staged cookies file.
        .arg("-o")
        .arg("extractor.cookies-update=false");
    if let Some(cookies) = cookies_file {
        cmd.arg("-C").arg(cookies);
    }
    if let Some(ua) = user_agent {
        cmd.arg("--user-agent").arg(ua);
    }
    // Instagram throttling for extraction requests (probe doesn't download).
    for arg in crate::gallery_argv::instagram_throttle_args(url, false) {
        cmd.arg(arg);
    }
    // `--` last: guards against URLs starting with `-` being parsed as options.
    cmd.arg("--").arg(url);
    let output = run_to_completion(cmd, PROBE_TIMEOUT, "gallery-dl probe").await?;

    // Exit 32 (v1.30.0+) / 64 (older) = NoExtractorError: unsupported URL,
    // not a failure. v1.30.0 folded NoExtractorError (64) into InputError (32).
    if matches!(output.status.code(), Some(32) | Some(64)) {
        return Ok(None);
    }
    if !output.status.success() {
        return Err(GalleryError::probe(exit_message(
            "gallery-dl probe",
            &output.status,
        )));
    }
    let items = parse_document(&output.stdout)?;
    if items.is_empty() {
        return Ok(None);
    }
    Ok(Some(items))
}

/// Run a gallery-dl command to completion under a wall-clock bound. Spawn
/// failures are install errors; a timeout is a probe error.
async fn run_to_completion(
    mut cmd: tokio::process::Command,
    timeout: Duration,
    what: &str,
) -> Result<std::process::Output, GalleryError> {
    // Debug: log the full argv before spawning.
    crate::backend_debug::log_command(&cmd);
    tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| GalleryError::probe(format!("{what} timed out")))?
        .map_err(|e| GalleryError::install(format!("couldn't spawn gallery-dl: {e}")))
}

/// Parse a complete `gallery-dl -j` document (one JSON array of messages).
fn parse_document(stdout: &[u8]) -> Result<Vec<GalleryItem>, GalleryError> {
    let messages: Vec<serde_json::Value> = serde_json::from_slice(stdout)
        .map_err(|e| GalleryError::probe(format!("couldn't parse gallery-dl output: {e}")))?;
    parse_probe_messages(&messages)
}

/// The message for a non-zero exit, shared by every gallery-dl run path.
fn exit_message(what: &str, status: &std::process::ExitStatus) -> String {
    format!("{what} failed (exit {})", status.code().unwrap_or(-1))
}

/// Base argv for the file enumeration (before auth, filter, and URL).
/// `jsonl` selects line-framed streaming output; document mode (jsonl off)
/// is the fallback that keeps fatal errors visible.
fn enumerate_base_argv(jsonl: bool) -> Vec<String> {
    let mut argv = vec![
        "--config-ignore".to_string(),
        "-j".to_string(),
        "--no-download".to_string(),
        // Bound gallery-dl's walk (the picker itself virtualizes rows).
        // Indices stay 1-based, so range_spec remains correct.
        "--range".to_string(),
        format!("1-{ENUMERATE_MAX_FILES}"),
        "-o".to_string(),
        "extractor.cookies-update=false".to_string(),
    ];
    if jsonl {
        // One JSON message per line, flushed as it arrives (see parse_jsonl_line).
        argv.push("-o".to_string());
        argv.push("output.jsonl=true".to_string());
    }
    argv
}

/// The enumeration command: base argv, auth, filter, throttle. `jsonl` picks
/// line-framed streaming output over the document form.
fn enumerate_command(
    tool: &Path,
    url: &str,
    cookies_file: Option<&Path>,
    user_agent: Option<&str>,
    filter: crate::gallery_argv::GalleryFilter,
    jsonl: bool,
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(tool);
    // See probe_gallery: a dropped enumeration future must kill the child.
    cmd.kill_on_drop(true);
    for arg in enumerate_base_argv(jsonl) {
        cmd.arg(arg);
    }
    if let Some(cookies) = cookies_file {
        cmd.arg("-C").arg(cookies);
    }
    if let Some(ua) = user_agent {
        cmd.arg("--user-agent").arg(ua);
    }
    // Mirror the download's filter so --range indices match.
    let mut filter_argv = Vec::new();
    crate::gallery_argv::push_filter(&mut filter_argv, filter);
    for arg in filter_argv {
        cmd.arg(arg);
    }
    for arg in crate::gallery_argv::instagram_throttle_args(url, false) {
        cmd.arg(arg);
    }
    cmd.arg("--").arg(url);
    cmd
}

/// Wall-clock bound for the document-mode enumeration. It buffers the whole
/// output itself, so the streamed path's idle bound doesn't apply.
const ENUMERATE_TIMEOUT: Duration = Duration::from_secs(120);

/// Non-streamed enumeration in document mode: the whole output arrives as one
/// pretty-printed JSON array, the fatal error's `[-1]` message included. The
/// streamed path falls back to this when it collected nothing, because jsonl
/// mode swallows that message (see `enumerate_gallery_files_streamed`).
pub(crate) async fn enumerate_gallery_files(
    tool: &Path,
    url: &str,
    cookies_file: Option<&Path>,
    user_agent: Option<&str>,
    filter: crate::gallery_argv::GalleryFilter,
) -> Result<Vec<GalleryItem>, GalleryError> {
    let cmd = enumerate_command(tool, url, cookies_file, user_agent, filter, false);
    let output = run_to_completion(cmd, ENUMERATE_TIMEOUT, "gallery-dl enumerate").await?;
    if !output.status.success() {
        return Err(GalleryError::probe(exit_message(
            "gallery-dl enumerate",
            &output.status,
        )));
    }
    parse_document(&output.stdout)
}

/// Idle bound between lines and the overall wall-clock bound for the
/// streamed enumeration. Large galleries throttle between items, so the
/// idle bound is generous; the total bound still stops a pathological run.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const STREAM_TOTAL_TIMEOUT: Duration = Duration::from_secs(600);

/// Streamed gallery enumeration: gallery-dl runs with `output.jsonl=true`,
/// so every flushed line is one message and items are pushed to `items_tx`
/// as they arrive — the picker can open on the first one. Returns the number
/// of items delivered. Dropping the future kills gallery-dl (`kill_on_drop`);
/// a closed receiver stops the stream early. An empty stream re-runs in
/// document mode: jsonl drops a fatal error's `[-1]` message, so only the
/// fallback keeps such failures visible.
pub(crate) async fn enumerate_gallery_files_streamed(
    tool: &Path,
    url: &str,
    cookies_file: Option<&Path>,
    user_agent: Option<&str>,
    filter: crate::gallery_argv::GalleryFilter,
    items_tx: tokio::sync::mpsc::UnboundedSender<GalleryItem>,
) -> Result<usize, GalleryError> {
    // Same line-streaming shape as the yt-dlp runner (see video_runner.rs).
    use tokio::io::AsyncBufReadExt as _;

    let mut cmd = enumerate_command(tool, url, cookies_file, user_agent, filter, true);
    // stdout is the line stream; stderr is kept as a short tail for the
    // error text, drained concurrently so gallery-dl never blocks on a full pipe.
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // Debug: log the full argv before spawning.
    crate::backend_debug::log_command(&cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| GalleryError::install(format!("couldn't spawn gallery-dl: {e}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| GalleryError::install("gallery-dl enumerate: no stdout".to_string()))?;
    let (stderr_tail, stderr_drain) = spawn_stderr_tail(child.stderr.take());
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let mut reader = ProbeReader::default();
    let mut count = 0usize;
    let deadline = tokio::time::Instant::now() + STREAM_TOTAL_TIMEOUT;
    loop {
        // Wake at the idle bound or the total deadline, whichever is first.
        let wake = (tokio::time::Instant::now() + STREAM_IDLE_TIMEOUT).min(deadline);
        let line = match tokio::time::timeout_at(wake, lines.next_line()).await {
            Err(_) if tokio::time::Instant::now() >= deadline => {
                return Err(GalleryError::probe(
                    "gallery-dl enumerate timed out".to_string(),
                ));
            }
            Err(_) => {
                return Err(GalleryError::probe(
                    "gallery-dl enumerate stalled (no output)".to_string(),
                ));
            }
            Ok(Err(e)) => {
                return Err(GalleryError::probe(format!(
                    "couldn't read gallery-dl output: {e}"
                )));
            }
            Ok(Ok(None)) => break,
            Ok(Ok(Some(line))) => line,
        };
        // Blank lines carry no message.
        let Some(m) = parse_jsonl_line(&line)? else {
            continue;
        };
        if let Some(item) = reader.feed(&m) {
            // A closed receiver means the UI is gone: stop early.
            if items_tx.send(item).is_err() {
                return Ok(count);
            }
            count += 1;
        }
    }
    // EOF: the child should exit promptly — it just closed stdout.
    let status = tokio::time::timeout(STREAM_IDLE_TIMEOUT, child.wait())
        .await
        .map_err(|_| GalleryError::probe("gallery-dl enumerate timed out".to_string()))?
        .map_err(|e| GalleryError::install(format!("gallery-dl enumerate failed: {e}")))?;
    // Stabilize the final error tail after process exit. A grandchild may
    // inherit stderr, so never wait for EOF without a bound.
    let stderr_abort = stderr_drain.abort_handle();
    if tokio::time::timeout(STREAM_IDLE_TIMEOUT, stderr_drain)
        .await
        .is_err()
    {
        stderr_abort.abort();
    }
    if !status.success() {
        return Err(GalleryError::probe(format!(
            "{}{}",
            exit_message("gallery-dl enumerate", &status),
            stderr_summary(&stderr_tail)
        )));
    }
    reader.finish()?;
    // gallery-dl's jsonl mode drops the final `[-1]` message on a fatal
    // extraction error and still exits 0 (DataJob.run only appends it to the
    // buffer that jsonl mode no longer dumps). An empty stream is therefore
    // ambiguous — genuinely no files, or a swallowed error — so re-run once
    // in document mode, the only form where that error reaches us, and
    // forward whatever it found. Cost: empty galleries walk twice.
    if count == 0 && !items_tx.is_closed() {
        let batched = enumerate_gallery_files(tool, url, cookies_file, user_agent, filter);
        for item in batched.await? {
            if items_tx.send(item).is_err() {
                break;
            }
            count += 1;
        }
    }
    Ok(count)
}

/// Last few stderr lines of a streamed run, drained in the background.
type StderrTail = std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>;

const STDERR_TAIL_LINES: usize = 8;

fn spawn_stderr_tail(
    stderr: Option<tokio::process::ChildStderr>,
) -> (StderrTail, tokio::task::JoinHandle<()>) {
    use tokio::io::AsyncBufReadExt as _;
    let tail: StderrTail = Default::default();
    let sink = tail.clone();
    let handle = tokio::spawn(async move {
        if let Some(stderr) = stderr {
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut t = sink.lock().unwrap_or_else(|p| p.into_inner());
                if t.len() == STDERR_TAIL_LINES {
                    t.pop_front();
                }
                t.push_back(line);
            }
        }
    });
    (tail, handle)
}

/// `": line | line"` for the error text, or empty when stderr said nothing.
fn stderr_summary(tail: &StderrTail) -> String {
    let t = tail.lock().unwrap_or_else(|p| p.into_inner());
    if t.is_empty() {
        return String::new();
    }
    format!(": {}", t.iter().cloned().collect::<Vec<_>>().join(" | "))
}

/// Parse `gallery-dl -o output.jsonl=true` output: one JSON message array
/// per line, blank lines ignored. A malformed line is an error — silently
/// skipping one would shift every later index away from what `--range`
/// expects on the download side.
fn parse_jsonl_line(line: &str) -> Result<Option<serde_json::Value>, GalleryError> {
    if line.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(line)
        .map(Some)
        .map_err(|e| GalleryError::probe(format!("couldn't parse gallery-dl output: {e}")))
}

/// Streaming twin of `parse_probe_messages`: feed messages one at a time
/// and items come out as they arrive. Directory context and the pending
/// error carry across feeds exactly like the whole-vector parse.
#[derive(Default)]
struct ProbeReader {
    /// Count of file messages seen, including unreadable ones: gallery-dl
    /// counts every file in `--range`, so placeholders must hold a slot.
    file_index: usize,
    /// Directory context from the most recent type-2 message; file items
    /// inherit its title/user when they don't carry their own.
    dir_ctx: Option<serde_json::Map<String, serde_json::Value>>,
    /// A `-1` error doesn't discard files already seen; kept here and only
    /// surfaced by `finish` when nothing was collected.
    pending_err: Option<GalleryError>,
    got_items: bool,
}

/// Stand-in for a file message whose metadata is missing or unreadable.
/// Keeps its position in the picker so `--range` indices stay aligned.
fn placeholder_item(url: Option<&str>, n: usize) -> GalleryItem {
    let tail = url
        .and_then(|u| u.split(['?', '#']).next())
        .and_then(|u| u.rsplit('/').next())
        .filter(|s| !s.is_empty());
    let filename = tail
        .map(str::to_string)
        .unwrap_or_else(|| format!("file {n}"));
    let extension = filename
        .rsplit_once('.')
        .map(|(_, e)| e.to_string())
        .unwrap_or_default();
    GalleryItem {
        filename,
        extension,
        category: String::new(),
        extra: serde_json::Map::new(),
    }
}

impl ProbeReader {
    /// Feed one message; returns the file item it carried, if any.
    ///
    /// Message types (first array element): 2 = directory context, 3 = file,
    /// -1 = error.
    fn feed(&mut self, m: &serde_json::Value) -> Option<GalleryItem> {
        match m.get(0).and_then(|v| v.as_i64()) {
            Some(msg_type::FILE) => {
                self.file_index += 1;
                let url = m.get(1).and_then(|v| v.as_str());
                let mut item: GalleryItem = match m.get(2).cloned().map(serde_json::from_value) {
                    Some(Ok(item)) => item,
                    _ => {
                        tracing::warn!(
                            url = ?url,
                            "gallery-dl probe file message unreadable; keeping a placeholder"
                        );
                        placeholder_item(url, self.file_index)
                    }
                };
                // Inherit directory title/user when the file lacks them.
                if let Some(dir) = self.dir_ctx.as_ref() {
                    for key in ["title", "user", "author"] {
                        if !item.extra.contains_key(key)
                            && let Some(v) = dir.get(key)
                        {
                            item.extra.insert(key.to_string(), v.clone());
                        }
                    }
                }
                // Stash the file URL from m[1] for potential use.
                if let Some(url) = m.get(1).and_then(|v| v.as_str()) {
                    item.extra.insert(
                        "_gallery_dl_url".to_string(),
                        serde_json::Value::String(url.to_string()),
                    );
                }
                self.got_items = true;
                Some(item)
            }
            Some(msg_type::DIRECTORY) => {
                if let Some(obj) = m.get(1).and_then(|v| v.as_object()) {
                    self.dir_ctx = Some(obj.clone());
                }
                None
            }
            Some(msg_type::ERROR) => {
                let err = m
                    .get(1)
                    .and_then(|v| v.get("error"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let msg = m
                    .get(1)
                    .and_then(|v| v.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                // Don't discard usable items: store the error and let
                // `finish` decide, exactly like the batched parse.
                self.pending_err = Some(GalleryError::probe(format!("gallery-dl: {err}: {msg}")));
                None
            }
            _ => {
                tracing::trace!(message = ?m, "gallery-dl probe unknown message");
                None
            }
        }
    }

    /// Same verdict as the batched parse: items win over a pending error.
    fn finish(self) -> Result<(), GalleryError> {
        match (self.got_items, self.pending_err) {
            (true, Some(e)) => {
                tracing::warn!(error = %e, "gallery-dl probe had error but returned items");
                Ok(())
            }
            (false, Some(e)) => Err(e),
            _ => Ok(()),
        }
    }
}

/// Parse `gallery-dl -j` message arrays into file items.
///
/// Message types (first array element): 2 = directory context, 3 = file,
/// -1 = error. Directory metadata is inherited by file items lacking
/// `title`/`user`/`author`. Returns `Err` on a type -1 error message.
fn parse_probe_messages(messages: &[serde_json::Value]) -> Result<Vec<GalleryItem>, GalleryError> {
    let mut reader = ProbeReader::default();
    let mut items = Vec::new();
    for m in messages {
        if let Some(item) = reader.feed(m) {
            items.push(item);
        }
    }
    reader.finish()?;
    Ok(items)
}

/// Count files without downloading: `gallery-dl --config-ignore -g --no-download <url>`.
/// Returns the line count of direct file URLs, or `None` on any failure
/// (the card then shows an indeterminate count).
pub async fn count_gallery_files(
    tool: &Path,
    url: &str,
    cookies_file: Option<&Path>,
    user_agent: Option<&str>,
    filter: crate::gallery_argv::GalleryFilter,
) -> Option<u32> {
    let argv = crate::gallery_argv::count_argv(tool, url, cookies_file, user_agent, filter);
    let mut cmd = tokio::process::Command::new(&argv[0]);
    // See probe_gallery: a timed-out count must not orphan gallery-dl.
    cmd.kill_on_drop(true);
    cmd.args(&argv[1..]);
    // Debug: log the full argv before spawning.
    crate::backend_debug::log_command(&cmd);
    let output = tokio::time::timeout(PROBE_TIMEOUT, cmd.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let count = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();
    Some(count as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn run_timeout_is_a_probe_error() {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("5");
        let err = run_to_completion(cmd, Duration::from_millis(50), "gallery-dl probe")
            .await
            .unwrap_err();
        assert!(matches!(err, GalleryError::Probe(ref m) if m == "gallery-dl probe timed out"));
    }

    #[tokio::test]
    async fn run_spawn_failure_is_an_install_error() {
        let cmd = tokio::process::Command::new("/nonexistent/gallery-dl");
        let err = run_to_completion(cmd, Duration::from_secs(5), "gallery-dl probe")
            .await
            .unwrap_err();
        assert!(matches!(err, GalleryError::Install(_)));
    }

    #[test]
    fn enumerate_argv_is_range_bounded() {
        // The enumeration must stay --range-bounded: the cap bounds
        // gallery-dl's walk (the picker itself virtualizes rows).
        let argv = enumerate_base_argv(true);
        let range_pos = argv.iter().position(|a| a == "--range");
        assert!(range_pos.is_some(), "argv: {argv:?}");
        assert_eq!(
            argv[range_pos.unwrap() + 1],
            format!("1-{ENUMERATE_MAX_FILES}"),
            "argv: {argv:?}"
        );
    }

    #[test]
    fn argv_requests_jsonl_output() {
        // One JSON message per line, flushed as it arrives: the streaming
        // enumerate and the picker's incremental appends read this format.
        let argv = enumerate_base_argv(true);
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "-o" && w[1] == "output.jsonl=true"),
            "argv: {argv:?}"
        );
        // The fallback must stay in document mode: jsonl is what swallows
        // the `[-1]` error the fallback exists to surface.
        let argv = enumerate_base_argv(false);
        let jsonl = argv
            .windows(2)
            .any(|w| w[0] == "-o" && w[1] == "output.jsonl=true");
        assert!(!jsonl, "argv: {argv:?}");
    }

    #[test]
    fn jsonl_lines_parse_like_a_batched_document() {
        let jsonl = concat!(
            "[2, {\"category\": \"instagram\"}]\n",
            "\n",
            "[3, \"https://example.com/1.jpg\", {\"filename\": \"1.jpg\"}]\n",
            "   \n",
            "[3, \"https://example.com/2.jpg\", {\"filename\": \"2.jpg\"}]\n",
            "[-1, {\"error\": \"HttpError\", \"message\": \"404\"}]\n"
        );
        let messages: Vec<serde_json::Value> = jsonl
            .lines()
            .filter_map(|l| parse_jsonl_line(l).unwrap())
            .collect();
        // Blank/whitespace-only lines are skipped; message order is kept.
        assert_eq!(messages.len(), 4);
        let items = parse_probe_messages(&messages).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].filename, "2.jpg");
    }

    #[test]
    fn jsonl_garbage_line_is_an_error() {
        // A skipped line would shift every later index away from the
        // --range the download side applies, so this must fail loudly.
        let jsonl = "[3, \"https://example.com/1.jpg\", {\"filename\": \"1.jpg\"}]\nnot json\n";
        assert!(jsonl.lines().any(|l| parse_jsonl_line(l).is_err()));
        assert!(parse_jsonl_line("   ").unwrap().is_none());
    }

    #[test]
    fn unreadable_file_message_keeps_its_range_slot() {
        // gallery-dl counts this message in --range. Dropping it would shift
        // every later pick onto the wrong file, so it must become a placeholder.
        let messages: Vec<serde_json::Value> = vec![
            serde_json::json!([3, "https://example.com/a", {"filename": "a"}]),
            serde_json::json!([3, "https://example.com/dir/b.jpg"]),
            serde_json::json!([3, "https://example.com/c", {"filename": 5}]),
            serde_json::json!([3, "https://example.com/d", {"filename": "d"}]),
        ];
        let items = parse_probe_messages(&messages).unwrap();
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].filename, "a");
        assert_eq!(items[1].filename, "b.jpg");
        assert_eq!(items[1].extension, "jpg");
        assert_eq!(items[2].filename, "c");
        assert_eq!(items[3].filename, "d");
    }

    #[test]
    fn probe_reader_carries_dir_context_and_error_across_feeds() {
        // Same semantics as the batched parse, but messages arrive one at a
        // time: the directory context must carry over, and an error after
        // items must not discard them.
        let json = r#"[
            [2, {"user": "someone"}],
            [3, "https://example.com/1.jpg", {"filename": "1.jpg"}],
            [-1, {"error": "HttpError", "message": "404"}]
        ]"#;
        let messages: Vec<serde_json::Value> = serde_json::from_str(json).unwrap();
        let mut reader = ProbeReader::default();
        assert!(reader.feed(&messages[0]).is_none());
        let item = reader.feed(&messages[1]).unwrap();
        assert_eq!(
            item.extra.get("user").and_then(|v| v.as_str()),
            Some("someone")
        );
        assert!(reader.feed(&messages[2]).is_none());
        // Items exist, so the trailing error is logged, not surfaced.
        assert!(reader.finish().is_ok());
    }

    #[test]
    fn probe_reader_error_without_items_fails_finish() {
        let json = r#"[[-1, {"error": "AuthenticationError", "message": "login required"}]]"#;
        let messages: Vec<serde_json::Value> = serde_json::from_str(json).unwrap();
        let mut reader = ProbeReader::default();
        assert!(reader.feed(&messages[0]).is_none());
        assert!(reader.finish().is_err());
    }

    /// Streaming: a fake tool that prints jsonl messages must deliver items
    /// as they arrive, in order, and report the count.
    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn streamed_enumerate_delivers_items_in_order() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("grab-stream-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-gallery-dl.sh");
        std::fs::write(
            &script,
            concat!(
                "#!/bin/sh\n",
                "printf '%s\\n' '[2, {\"user\": \"someone\"}]'\n",
                "printf '%s\\n' '[3, \"https://example.com/1.jpg\", {\"filename\": \"1.jpg\"}]'\n",
                "printf '%s\\n' '[3, \"https://example.com/2.jpg\", {\"filename\": \"2.jpg\"}]'\n",
                "exit 0\n",
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let count = enumerate_gallery_files_streamed(
            &script,
            "https://example.com/",
            None,
            None,
            crate::gallery_argv::GalleryFilter::All,
            tx,
        )
        .await
        .unwrap();
        assert_eq!(count, 2);
        let mut names = Vec::new();
        while let Ok(item) = rx.try_recv() {
            names.push(item.filename);
        }
        assert_eq!(names, ["1.jpg", "2.jpg"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Streaming: items must reach the receiver while the child is still
    /// running — the picker opens on the first one, it does not wait for
    /// the whole listing.
    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn streamed_enumerate_yields_items_before_exit() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("grab-stream-early-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-gallery-dl.sh");
        std::fs::write(
            &script,
            concat!(
                "#!/bin/sh\n",
                "printf '%s\\n' '[3, \"https://example.com/1.jpg\", {\"filename\": \"1.jpg\"}]'\n",
                // Holds the process open: buffer-until-exit would time out.
                "sleep 30\n",
                "exit 0\n",
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = tokio::spawn(async move {
            enumerate_gallery_files_streamed(
                &script,
                "https://example.com/",
                None,
                None,
                crate::gallery_argv::GalleryFilter::All,
                tx,
            )
            .await
        });
        let first = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("first item must arrive before the child exits")
            .expect("stream must yield an item");
        assert_eq!(first.filename, "1.jpg");
        // Cancelling drops the future, which kills the sleeping child.
        handle.abort();
        let _ = handle.await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A non-zero exit after items were delivered must surface the error
    /// while the receiver keeps the partial list: the picker shows what was
    /// listed and stays usable.
    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn streamed_enumerate_keeps_delivered_items_on_failed_exit() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("grab-stream-exit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-gallery-dl.sh");
        std::fs::write(
            &script,
            concat!(
                "#!/bin/sh\n",
                "printf '%s\\n' '[3, \"https://example.com/1.jpg\", {\"filename\": \"1.jpg\"}]'\n",
                "exit 1\n",
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let result = enumerate_gallery_files_streamed(
            &script,
            "https://example.com/",
            None,
            None,
            crate::gallery_argv::GalleryFilter::All,
            tx,
        )
        .await;
        assert!(result.is_err(), "non-zero exit must be an error");
        let item = rx.try_recv().expect("item delivered before the failure");
        assert_eq!(item.filename, "1.jpg");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A silent stream — jsonl drops the fatal error's `[-1]` message — must
    /// re-run in document mode and forward the items it finds.
    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn streamed_enumerate_falls_back_to_document_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("grab-fallback-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-gallery-dl.sh");
        std::fs::write(
            &script,
            concat!(
                "#!/bin/sh\n",
                // Streamed run: nothing, like a swallowed error, exit 0.
                "case \"$*\" in\n",
                "  *output.jsonl=true*) exit 0;;\n",
                "esac\n",
                // Document run: one file message.
                "printf '%s' '[[3, \"https://example.com/1.jpg\", {\"filename\": \"1.jpg\"}]]'\n",
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let count = enumerate_gallery_files_streamed(
            &script,
            "https://example.com/",
            None,
            None,
            crate::gallery_argv::GalleryFilter::All,
            tx,
        )
        .await
        .unwrap();
        assert_eq!(count, 1);
        let item = rx
            .try_recv()
            .expect("fallback items must reach the receiver");
        assert_eq!(item.filename, "1.jpg");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The fallback's error must propagate: it is exactly the error the
    /// streamed run lost, so returning `Ok(0)` would hide the failure.
    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn streamed_enumerate_reports_fallback_error() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("grab-fallback-err-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-gallery-dl.sh");
        std::fs::write(
            &script,
            concat!(
                "#!/bin/sh\n",
                "case \"$*\" in\n",
                "  *output.jsonl=true*) exit 0;;\n",
                "esac\n",
                "printf '%s' '[[-1, {\"error\": \"HttpError\", \"message\": \"404\"}]]'\n",
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let result = enumerate_gallery_files_streamed(
            &script,
            "https://example.com/",
            None,
            None,
            crate::gallery_argv::GalleryFilter::All,
            tx,
        )
        .await;
        assert!(result.is_err(), "a swallowed error must surface, not Ok(0)");
        assert!(rx.try_recv().is_err(), "no items were ever listed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn should_surface_auth_errors() {
        assert!(should_surface(&GalleryError::probe(
            "gallery-dl: AuthenticationError: login required"
        )));
        assert!(should_surface(&GalleryError::probe(
            "gallery-dl: AuthorizationError: forbidden"
        )));
        assert!(should_surface(&GalleryError::probe(
            "gallery-dl: AuthRequired: account credentials required"
        )));
        assert!(should_surface(&GalleryError::probe(
            "gallery-dl: ChallengeError: captcha"
        )));
        assert!(should_surface(&GalleryError::probe(
            "gallery-dl: HttpError: 404"
        )));
    }

    #[test]
    fn should_not_surface_other_errors() {
        assert!(!should_surface(&GalleryError::probe(
            "gallery-dl: NoExtractorError: unsupported URL"
        )));
        assert!(!should_surface(&GalleryError::probe(
            "gallery-dl: NotFoundError: nothing found"
        )));
        assert!(!should_surface(&GalleryError::install("timeout")));
        assert!(!should_surface(&GalleryError::runtime("spawn failed")));
    }

    fn item(extension: &str) -> GalleryItem {
        GalleryItem {
            filename: format!("file.{extension}"),
            extension: extension.to_string(),
            category: "test".to_string(),
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn content_from_extension() {
        assert_eq!(GalleryContent::from_extension("mp4"), GalleryContent::Video);
        assert_eq!(GalleryContent::from_extension("MP4"), GalleryContent::Video);
        assert_eq!(
            GalleryContent::from_extension("webm"),
            GalleryContent::Video
        );
        assert_eq!(GalleryContent::from_extension("flv"), GalleryContent::Video);
        assert_eq!(GalleryContent::from_extension("wmv"), GalleryContent::Video);
        assert_eq!(GalleryContent::from_extension("mpg"), GalleryContent::Video);
        assert_eq!(GalleryContent::from_extension("jpg"), GalleryContent::Image);
        assert_eq!(
            GalleryContent::from_extension("JPEG"),
            GalleryContent::Image
        );
        assert_eq!(
            GalleryContent::from_extension("webp"),
            GalleryContent::Image
        );
        assert_eq!(
            GalleryContent::from_extension("heic"),
            GalleryContent::Image
        );
        assert_eq!(
            GalleryContent::from_extension("HEIF"),
            GalleryContent::Image
        );
        assert_eq!(
            GalleryContent::from_extension("xyz"),
            GalleryContent::Unknown
        );
    }

    #[test]
    fn url_is_direct_media_check() {
        assert!(url_is_direct_media("https://example.com/video.mp4"));
        assert!(url_is_direct_media("https://example.com/photo.HEIC"));
        assert!(url_is_direct_media(
            "https://example.com/song.mp3?token=abc#frag"
        ));
        assert!(!url_is_direct_media("https://example.com/page.html"));
        assert!(!url_is_direct_media("https://example.com/noext"));
        assert!(!url_is_direct_media("https://example.com/"));
        // Host-only URL: .mov is a TLD, not a file extension.
        assert!(!url_is_direct_media("https://example.mov"));
        assert!(!url_is_direct_media("https://example.mov/"));
    }

    #[test]
    fn route_instagram_highlights_playlist_prefers_gallerydl_videos() {
        let highlights = "https://www.instagram.com/stories/highlights/18119089357372404/";
        let videos = vec![item("mp4"), item("webm")];
        assert_eq!(
            route_for_url(highlights, true, true, Some(&videos), None),
            ProbeOutcome::VideoGalleryDl
        );

        // A single video is not overridden, and regular playlists remain yt-dlp's.
        assert_eq!(
            route_for_url(highlights, true, false, Some(&videos), None),
            ProbeOutcome::VideoYtDlp {
                gallery_toggle: false
            }
        );
        assert_eq!(
            route_for_url(
                "https://www.youtube.com/playlist?list=PL123",
                true,
                true,
                Some(&videos),
                None,
            ),
            ProbeOutcome::VideoYtDlp {
                gallery_toggle: false
            }
        );

        // Do not pick gallery-dl if its probe did not find any video entries.
        let images = vec![item("jpg")];
        assert_eq!(
            route_for_url(highlights, true, true, Some(&images), None),
            ProbeOutcome::VideoYtDlp {
                gallery_toggle: true
            }
        );
    }

    #[test]
    fn route_ytdlp_wins_with_gallery_toggle() {
        let items = vec![item("jpg")];
        assert_eq!(
            route(true, Some(&items), None),
            ProbeOutcome::VideoYtDlp {
                gallery_toggle: true
            }
        );
    }

    #[test]
    fn route_ytdlp_wins_no_toggle_without_images() {
        let items = vec![item("mp4")];
        assert_eq!(
            route(true, Some(&items), None),
            ProbeOutcome::VideoYtDlp {
                gallery_toggle: false
            }
        );
        assert_eq!(
            route(true, None, None),
            ProbeOutcome::VideoYtDlp {
                gallery_toggle: false
            }
        );
    }

    #[test]
    fn route_gallerydl_video_fallback() {
        // Instagram stories: yt-dlp finds nothing, gallery-dl finds video.
        let items = vec![item("mp4")];
        assert_eq!(
            route(false, Some(&items), None),
            ProbeOutcome::VideoGalleryDl
        );
    }

    #[test]
    fn route_gallery_images() {
        let items = vec![item("jpg"), item("png")];
        assert_eq!(route(false, Some(&items), None), ProbeOutcome::Gallery);
    }

    #[test]
    fn route_nothing() {
        assert_eq!(route(false, None, None), ProbeOutcome::None);
        let unknown = vec![item("xyz")];
        assert_eq!(route(false, Some(&unknown), None), ProbeOutcome::None);
    }

    #[test]
    fn route_sticky_host_preference() {
        let images = vec![item("jpg")];
        // Sticky gallery wins when both viable.
        assert_eq!(
            route(true, Some(&images), Some(StickyMode::Gallery)),
            ProbeOutcome::Gallery
        );
        // Sticky video → default Video with toggle.
        assert_eq!(
            route(true, Some(&images), Some(StickyMode::Video)),
            ProbeOutcome::VideoYtDlp {
                gallery_toggle: true
            }
        );
        // No sticky → default Video.
        assert_eq!(
            route(true, Some(&images), None),
            ProbeOutcome::VideoYtDlp {
                gallery_toggle: true
            }
        );
        // Sticky never overrides an unviable mode: yt-dlp found nothing,
        // so Video is not an option even with a Video preference.
        assert_eq!(
            route(false, Some(&images), Some(StickyMode::Video)),
            ProbeOutcome::Gallery
        );
    }

    #[test]
    fn route_mixed_content() {
        // Order-independent: video + image → Video with gallery toggle.
        let mixed = vec![item("mp4"), item("jpg")];
        assert_eq!(
            route(true, Some(&mixed), None),
            ProbeOutcome::VideoYtDlp {
                gallery_toggle: true
            }
        );
        let mixed_rev = vec![item("jpg"), item("mp4")];
        assert_eq!(
            route(true, Some(&mixed_rev), None),
            ProbeOutcome::VideoYtDlp {
                gallery_toggle: true
            }
        );
        // yt-dlp found nothing: video wins over images for the fallback.
        assert_eq!(
            route(false, Some(&mixed), None),
            ProbeOutcome::VideoGalleryDl
        );
    }

    #[test]
    fn probe_title_prefers_title_field() {
        let mut extra = serde_json::Map::new();
        extra.insert(
            "title".to_string(),
            serde_json::Value::String("My Gallery".to_string()),
        );
        extra.insert(
            "user".to_string(),
            serde_json::Value::String("someuser".to_string()),
        );
        let item = GalleryItem {
            filename: "f.jpg".to_string(),
            extension: "jpg".to_string(),
            category: "instagram".to_string(),
            extra,
        };
        assert_eq!(
            probe_title(&item, "https://example.com/p/123"),
            "My Gallery"
        );
    }

    #[test]
    fn probe_title_falls_back_to_url() {
        // Empty category and no metadata → URL-derived filename.
        let item = GalleryItem {
            filename: "f.jpg".to_string(),
            extension: "jpg".to_string(),
            category: String::new(),
            extra: serde_json::Map::new(),
        };
        assert_eq!(
            probe_title(&item, "https://example.com/p/sunset.jpg"),
            "sunset.jpg"
        );
    }

    #[test]
    fn probe_json_parses_real_array_format() {
        // Real `gallery-dl -j` output: [2, {dir}] then [3, "url", {meta}].
        let json = r#"[
            [2, {"category": "instagram", "subcategory": "posts", "user": "someuser"}],
            [3, "https://example.com/12345.jpg", {
                "filename": "12345.jpg",
                "extension": "jpg",
                "category": "instagram",
                "title": "Sunset"
            }]
        ]"#;
        let messages: Vec<serde_json::Value> = serde_json::from_str(json).unwrap();
        let items = parse_probe_messages(&messages).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].filename, "12345.jpg");
        assert_eq!(
            GalleryContent::from_extension(&items[0].extension),
            GalleryContent::Image
        );
        assert_eq!(probe_title(&items[0], "https://example.com/"), "Sunset");
        // File URL stashed from m[1].
        assert_eq!(
            items[0]
                .extra
                .get("_gallery_dl_url")
                .and_then(|v| v.as_str()),
            Some("https://example.com/12345.jpg")
        );
    }

    #[test]
    fn probe_json_inherits_directory_user() {
        // File without user inherits from the directory message.
        let json = r#"[
            [2, {"category": "instagram", "user": "someuser"}],
            [3, "https://example.com/12345.jpg", {
                "filename": "12345.jpg",
                "extension": "jpg"
            }]
        ]"#;
        let messages: Vec<serde_json::Value> = serde_json::from_str(json).unwrap();
        let items = parse_probe_messages(&messages).unwrap();
        assert_eq!(items.len(), 1);
        // extension/category default when omitted (S7).
        assert_eq!(items[0].extension, "jpg");
        assert_eq!(items[0].category, "");
        assert_eq!(probe_title(&items[0], "https://example.com/"), "someuser");
    }

    #[test]
    fn probe_json_error_message_returns_err() {
        // Type -1 error message becomes GalleryError::Probe.
        let json = r#"[
            [-1, {"error": "AuthenticationError", "message": "login required"}]
        ]"#;
        let messages: Vec<serde_json::Value> = serde_json::from_str(json).unwrap();
        let err = parse_probe_messages(&messages).unwrap_err();
        assert!(matches!(err, GalleryError::Probe(_)));
        assert!(err.to_string().contains("AuthenticationError"));
    }

    #[test]
    fn probe_error_after_files_returns_items() {
        // A -1 error after files doesn't discard them.
        let json = r#"[
            [3, "https://example.com/1.jpg", {"filename": "1.jpg", "extension": "jpg"}],
            [3, "https://example.com/2.jpg", {"filename": "2.jpg", "extension": "jpg"}],
            [-1, {"error": "HttpError", "message": "404"}]
        ]"#;
        let messages: Vec<serde_json::Value> = serde_json::from_str(json).unwrap();
        let items = parse_probe_messages(&messages).unwrap();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn probe_json_route_gallery_for_images() {
        // End-to-end: real-format messages route to Gallery.
        let json = r#"[
            [2, {"category": "test"}],
            [3, "https://example.com/a.jpg", {"filename": "a.jpg", "extension": "jpg"}],
            [3, "https://example.com/b.png", {"filename": "b.png", "extension": "png"}]
        ]"#;
        let messages: Vec<serde_json::Value> = serde_json::from_str(json).unwrap();
        let items = parse_probe_messages(&messages).unwrap();
        assert_eq!(route(false, Some(&items), None), ProbeOutcome::Gallery);
    }

    /// Regression: probe_gallery must work from a thread without a Tokio
    /// runtime (e.g. GTK main thread). Uses block_on via the internal
    /// static runtime; a nonexistent tool should give a proper error,
    /// not a "no reactor running" panic.
    #[test]
    fn probe_gallery_works_without_tokio_runtime() {
        let result = std::thread::spawn(|| {
            // No Tokio runtime on this thread.
            assert!(tokio::runtime::Handle::try_current().is_err());
            crate::runtime::tokio_rt().block_on(probe_gallery(
                std::path::Path::new("/nonexistent/gallery-dl"),
                "https://example.com/",
                None,
                None,
            ))
        })
        .join()
        .expect("thread panicked");
        // Should be an Err (couldn't spawn), not a panic.
        assert!(result.is_err());
    }

    /// Regression: count_gallery_files must work from a thread without
    /// a Tokio runtime.
    #[test]
    fn count_gallery_files_works_without_tokio_runtime() {
        let result = std::thread::spawn(|| {
            assert!(tokio::runtime::Handle::try_current().is_err());
            crate::runtime::tokio_rt().block_on(count_gallery_files(
                std::path::Path::new("/nonexistent/gallery-dl"),
                "https://example.com/",
                None,
                None,
                crate::gallery_argv::GalleryFilter::All,
            ))
        })
        .join()
        .expect("thread panicked");
        // Should be None (failure), not a panic.
        assert_eq!(result, None);
    }

    /// Regression: dropping the probe future (as `timeout()` does on expiry)
    /// must kill the gallery-dl child instead of orphaning it. The fake tool
    /// records its own PID so the test can check the process is really gone.
    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn probe_kills_child_when_future_dropped() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("grab-kill-drop-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-gallery-dl.sh");
        let pidfile = dir.join("child.pid");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho $$ > {}\nexec sleep 60\n",
                pidfile.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Drop the probe future quickly, like `timeout()` does on expiry.
        let _ = tokio::time::timeout(
            Duration::from_millis(300),
            probe_gallery(&script, "https://example.com/", None, None),
        )
        .await;

        // Wait for the child to have started, then require it to die.
        let pid: u32 = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&pidfile)
                    && let Ok(pid) = text.trim().parse()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("fake tool never started");
        let gone = tokio::time::timeout(Duration::from_secs(5), async {
            while std::path::Path::new(&format!("/proc/{pid}")).exists() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .is_ok();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(gone, "probe child (pid {pid}) survived the dropped future");
    }
}
