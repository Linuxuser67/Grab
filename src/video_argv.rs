//! Video attempt inputs + argv builders (unified/HLS/live format-spec).
//! Consumed by the runner/engine through the `video` facade.

use crate::video_prefs::subtitle_cli_args;
use crate::video_quality::quality_height;
use crate::video_staging::{VideoManifest, dest_part_path, manifest_path, ytdlp_output_template};
use crate::video_tools::{VideoError, ffmpeg_location_dir, ytdlp_identity_args};
use std::path::{Path, PathBuf};

pub(crate) async fn write_manifest(
    dir: &Path,
    item_id: u64,
    manifest: &VideoManifest,
) -> Result<(), VideoError> {
    let text = serde_json::to_string_pretty(manifest).map_err(VideoError::staging)?;
    tokio::fs::write(manifest_path(dir, item_id), text)
        .await
        .map_err(VideoError::staging)
}

/// Inputs for one resolver-worker attempt (built on the main thread, no GTK).
#[derive(Debug, Clone)]
pub struct VideoJob {
    pub item_id: u64,
    pub page_url: String,
    /// Playlist entry id this row was picked from, if any (selected on re-resolve).
    pub playlist_item_id: Option<String>,
    pub quality: String,
    pub dest: PathBuf,
    /// Per-download speed cap in bytes/sec (`--ratelimit`); `None` = unlimited. Live rows never take it.
    pub speed_limit: Option<u64>,
    /// Stamp the finished file with the server's Last-Modified date (`--mtime`). Best-effort; never on live rows.
    pub keep_server_date: bool,
    /// Dialog-pinned video format id, if the user picked an exact format.
    pub video_format_id: Option<String>,
    /// Whether the page is currently live (a live capture keeps its partial on stop).
    pub is_live: bool,
    /// Record a live stream from its beginning (`--live-from-start`); live-only, no-op without DVR support.
    pub live_from_start: bool,
    /// Newest codecs first (AV1 over AVC1); false prefers compatible H.264.
    pub newest_codecs: bool,
    /// Dialog audio-only choice for this row (skips video selection, merging, subtitles).
    pub audio_only: bool,
    /// Raw browser-auth setting (`none` = off), resolved to `--cookies-from-browser` in the worker.
    pub cookies_browser: String,
    /// Subtitle language code for sidecars (`None` = off); missing language warns, never fails.
    pub subtitles: Option<String>,
    /// Mux downloaded subtitles into the finished file; live rows never take it.
    pub embed_subs: bool,
    /// Cut SponsorBlock sponsor segments; live rows never take it.
    pub sponsorblock_remove: bool,
    /// Mark SponsorBlock sponsor segments as chapters; live rows never take it.
    pub sponsorblock_mark: bool,
    /// Remux the finished file into another container (`None` = off; always `None` for audio-only/live).
    pub remux_video: Option<String>,
    /// Write chapter markers into the finished file; live rows never take it.
    pub embed_chapters: bool,
    /// Proxy resolved at spawn time (`None` = direct).
    pub proxy: Option<crate::net_types::ResolvedProxy>,
}

/// Fallback `-f` spec when the pinned id rotated since the dialog resolve. Pure.
pub(crate) fn part_fallback_spec(quality: &str, video_part: bool, prefer_audio: bool) -> String {
    if video_part {
        return match quality_height(quality) {
            Some(h) => format!("bv*[height<={h}]"),
            None => "bv*".to_string(),
        };
    }
    if prefer_audio {
        "ba/b".to_string()
    } else {
        "b".to_string()
    }
}

/// Allowlist extractor ids to selector-safe chars (a hostile id must not widen the `-f` set).
fn selector_id(id: &str) -> Option<&str> {
    let id = id.trim();
    (!id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ':'))
    .then_some(id)
}

/// Single-invocation `-f` spec for direct downloads, plus whether yt-dlp will merge. Pure.
pub(crate) fn unified_format_spec(
    video_id: Option<&str>,
    audio_id: &str,
    quality: &str,
    audio_only: bool,
) -> (String, bool) {
    let vfb = part_fallback_spec(quality, true, false);
    let afb = part_fallback_spec(quality, false, true);
    let aid = selector_id(audio_id);
    if audio_only {
        return match aid {
            Some(a) => (format!("{a}/ba/b"), false),
            None => ("ba/b".to_string(), false),
        };
    }
    match video_id {
        None => match aid {
            Some(a) => (format!("{a}/b"), false),
            None => ("b".to_string(), false),
        },
        Some(raw) => match (selector_id(raw), aid) {
            (Some(vid), Some(a)) => (format!("{vid}+{a}/{vfb}+{a}/{vfb}+{afb}"), true),
            _ => (format!("{vfb}+{afb}"), true),
        },
    }
}

/// Merge container: the video ext when supported, mp4 otherwise. Pure.
pub(crate) fn merge_output_ext(video_ext: &str) -> String {
    match video_ext.to_ascii_lowercase().as_str() {
        "mp4" | "webm" | "mkv" | "flv" | "ogg" => video_ext.to_ascii_lowercase(),
        _ => "mp4".to_string(),
    }
}

/// Containers the finished-name backstop may correct toward.
const TRUTHFUL_CONTAINERS: &[&str] = &["mp4", "webm", "mkv", "flv", "ogg", "m4a"];

/// Intake-name extensions Grab generates itself (the only ones the backstop second-guesses).
const GENERATED_INTAKE_EXTS: &[&str] = &["mp4", "m4a"];

/// Corrected row name when the discovered container disagrees with the intake name. Pure.
pub(crate) fn container_truth_name(dest: &Path, discovered: &Path) -> Option<String> {
    let found_ext = discovered.extension().and_then(|e| e.to_str())?;
    let dest_ext = dest.extension().and_then(|e| e.to_str())?;
    if found_ext.eq_ignore_ascii_case(dest_ext) {
        return None;
    }
    if !GENERATED_INTAKE_EXTS
        .iter()
        .any(|e| dest_ext.eq_ignore_ascii_case(e))
    {
        return None;
    }
    if !TRUTHFUL_CONTAINERS
        .iter()
        .any(|e| found_ext.eq_ignore_ascii_case(e))
    {
        return None;
    }
    let stem = dest.file_stem().and_then(|s| s.to_str())?;
    if stem.is_empty() {
        return None;
    }
    Some(format!("{stem}.{}", found_ext.to_ascii_lowercase()))
}

/// Stable `-o` template inside the row's staging dir (yt-dlp resumes its `.part` beside it).
pub(crate) fn unified_output_template(staging: &Path) -> PathBuf {
    staging.join("grab-media.%(ext)s")
}

/// Shared argv tail for the VOD builders: subtitles, proxy, identity/cookie args.
/// `job.subtitles` carries the already-resolved language (preferred if the
/// video offers it, else the English fallback) — see `resolve_subtitle_lang`.
fn push_vod_tail_args(args: &mut Vec<String>, job: &VideoJob) {
    if !job.audio_only
        && let Some(lang) = job.subtitles.as_deref()
    {
        args.extend(subtitle_cli_args(lang));
    }
    args.extend(proxy_cli_args(job.proxy.as_ref()));
    args.extend(ytdlp_identity_args(
        &job.cookies_browser,
        None,
        &job.page_url,
    ));
}

/// Playlist scoping for a yt-dlp download argv. A row picked from a playlist
/// re-resolves the collection URL, so it selects its entry by 1-based position:
/// `--no-playlist` would silently download the collection's first entry for
/// every picked row (N copies of story one). Plain rows keep `--no-playlist`.
pub(crate) fn playlist_scope_args(playlist_index: Option<usize>) -> Vec<String> {
    match playlist_index {
        Some(index) => vec!["--playlist-items".to_string(), (index + 1).to_string()],
        None => vec!["--no-playlist".to_string()],
    }
}

/// yt-dlp argv for one unified direct download into a staging temp. Pure.
#[allow(clippy::too_many_arguments)]
pub(crate) fn unified_download_argv(
    job: &VideoJob,
    spec: &str,
    merging: bool,
    merge_ext: &str,
    ffmpeg_bin: &Path,
    out: &Path,
    playlist_index: Option<usize>,
) -> Vec<String> {
    let mut args = vec!["--ignore-config".to_string()];
    args.extend(playlist_scope_args(playlist_index));
    args.extend([
        "--newline".to_string(),
        "--progress".to_string(),
        "--progress-template".to_string(),
        YTDLP_PROGRESS_TEMPLATE.to_string(),
        // Embed metadata on every leg so the video id survives in tags (not the live leg: it remuxes after capture).
        "--embed-metadata".to_string(),
        "-f".to_string(),
        spec.to_string(),
        "-o".to_string(),
        out.to_string_lossy().into_owned(),
        "--ffmpeg-location".to_string(),
        ffmpeg_location_dir(ffmpeg_bin),
        "--print".to_string(),
        "after_move:filepath".to_string(),
    ]);
    if let Some(limit) = job.speed_limit {
        args.push("--ratelimit".to_string());
        args.push(limit.to_string());
    }
    if job.keep_server_date {
        args.push("--mtime".to_string());
    }
    if job.audio_only {
        // Native audio containers vary by codec, so extraction targets m4a rather than the dest name.
        args.push("--extract-audio".to_string());
        args.push("--audio-format".to_string());
        args.push("m4a".to_string());
    } else if merging {
        args.push("--merge-output-format".to_string());
        args.push(merge_ext.to_string());
        if merge_ext == "mp4" {
            // moov atom to the front so merged mp4s start playback without a full scan.
            args.push("--postprocessor-args".to_string());
            args.push(crate::video_tools::MERGER_FASTSTART_ARGS.to_string());
        }
    }
    if job.embed_subs {
        args.push("--embed-subs".to_string());
    }
    if job.sponsorblock_remove {
        args.push("--sponsorblock-remove".to_string());
        args.push("sponsor".to_string());
    }
    if job.sponsorblock_mark {
        args.push("--sponsorblock-mark".to_string());
        args.push("sponsor".to_string());
    }
    if job.embed_chapters {
        args.push("--embed-chapters".to_string());
    }
    if let Some(fmt) = job.remux_video.as_deref() {
        args.push("--remux-video".to_string());
        args.push(fmt.to_string());
    }
    push_vod_tail_args(&mut args, job);
    args
}

/// yt-dlp `-f` spec for one HLS attempt over the page URL (muxed files win over lower splits).
pub(crate) fn hls_format_spec(quality: &str, pinned: Option<&str>) -> String {
    if let Some(id) = pinned.and_then(selector_id) {
        return format!("{id}+ba/b");
    }
    match quality_height(quality) {
        Some(h) => format!("bv*[height<={h}]+ba/b"),
        None => "bv*+ba/b".to_string(),
    }
}

/// yt-dlp argv for a live capture: kill-safe MPEG-TS, endless fragment retries bounded by our timeout. Pure.
pub(crate) fn live_capture_argv(
    job: &VideoJob,
    hls_format_id: &str,
    out: &Path,
    playlist_index: Option<usize>,
) -> Vec<String> {
    let mut args = vec!["--ignore-config".to_string()];
    args.extend(playlist_scope_args(playlist_index));
    args.extend([
        "--newline".to_string(),
        "--progress".to_string(),
        "--progress-template".to_string(),
        YTDLP_PROGRESS_TEMPLATE.to_string(),
        "-f".to_string(),
        hls_format_spec(&job.quality, Some(hls_format_id)),
        "--hls-use-mpegts".to_string(),
        "--fragment-retries".to_string(),
        "infinite".to_string(),
        "-o".to_string(),
        ytdlp_output_template(out),
        // LL-HLS timestamp hygiene for yt-dlp's ffmpeg downloader: split A/V
        // manifests drift when the broadcaster drops frames; the flags keep
        // the tracks aligned without re-encoding. Scoped `ffmpeg:` so native
        // HLS downloads ignore them. Must sit before the `--` URL terminator
        // below: everything after `--` is a positional URL, so a flag placed
        // past it becomes an extra "URL" and yt-dlp aborts with "Fixed output
        // name but more than one file to download".
        "--downloader-args".to_string(),
        crate::video_tools::LIVE_DOWNLOADER_ARGS.to_string(),
    ]);
    if job.is_live && job.live_from_start {
        args.push("--live-from-start".to_string());
    }
    args.extend(proxy_cli_args(job.proxy.as_ref()));
    args.extend(ytdlp_identity_args(
        &job.cookies_browser,
        None,
        &job.page_url,
    ));
    args
}

/// ffmpeg argv remuxing a stopped live capture into the finished file (stream-copy + faststart). Pure.
/// `page_url` is stamped into `comment`: the live leg never gets `--embed-metadata`, so this is its only provenance.
/// `dest` is the `.part` remux target: ffmpeg can't guess a container from that name, so `-f`
/// is pinned from the real extension underneath it.
pub(crate) fn live_remux_argv(
    ts_path: &Path,
    dest: &Path,
    audio_only: bool,
    with_bsf: bool,
    page_url: &str,
) -> Vec<String> {
    let mut argv = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "warning".to_string(),
        "-nostats".to_string(),
        "-i".to_string(),
        ts_path.to_string_lossy().into_owned(),
    ];
    if audio_only {
        argv.extend(["-map".to_string(), "0:a?".to_string()]);
    } else {
        argv.extend(["-map".to_string(), "0".to_string()]);
    }
    argv.extend([
        "-dn".to_string(),
        "-ignore_unknown".to_string(),
        "-c".to_string(),
        "copy".to_string(),
    ]);
    // TS AAC needs the ADTS-to-ASC fixup (mirrors yt-dlp's own FixupM3u8); anything else remuxes on retry.
    if with_bsf {
        argv.extend(["-bsf:a".to_string(), "aac_adtstoasc".to_string()]);
    }
    let page_url = page_url.trim();
    if !page_url.is_empty() {
        argv.extend(["-metadata".to_string(), format!("comment={page_url}")]);
    }
    argv.extend([
        "-f".to_string(),
        remux_muxer(dest).to_string(),
        "-movflags".to_string(),
        "+faststart".to_string(),
        "--".to_string(),
        dest.to_string_lossy().into_owned(),
    ]);
    argv
}

/// ffmpeg muxer for a live remux target. The target wears the `.part` crash-debris suffix,
/// which maps to no container, so the real extension is read from underneath it. Anything
/// unrecognized falls back to mp4: live captures are mp4/m4a by construction. Pure.
fn remux_muxer(dest: &Path) -> &'static str {
    let under = dest
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.strip_suffix(".part").unwrap_or(n))
        .unwrap_or("");
    let ext = under.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        // No m4a muxer in ffmpeg: the mp4 muxer writes it.
        "mp4" | "m4a" => "mp4",
        "webm" => "webm",
        "mkv" => "matroska",
        "flv" => "flv",
        "ogg" => "ogg",
        _ => "mp4",
    }
}

/// Whether a no-output from-start attempt retries once from the live edge (never discards partials, never overrides Stop). Pure.
pub(crate) fn fallback_to_live_edge(
    is_live: bool,
    live_from_start: bool,
    aborted: bool,
    already_retried: bool,
) -> bool {
    is_live && live_from_start && !aborted && !already_retried
}

/// Whether yt-dlp's stderr carries its distinctive `--live-from-start`
/// rejection: the flag was passed but the live stream has no formats
/// downloadable from the start (yt-dlp raises it during format selection, so
/// nothing is ever recorded). Pure; matched on yt-dlp's wording, not the
/// exit code.
pub(crate) fn live_from_start_unsupported(log: &str) -> bool {
    log.lines().any(|l| {
        l.contains("--live-from-start")
            && l.contains("no formats that can be downloaded from the start")
    })
}

/// yt-dlp argv for one VOD HLS capture (planner-pinned variant id, merge/extract, proxy/identity). Pure.
#[allow(clippy::too_many_arguments)]
pub(crate) fn hls_download_argv(
    job: &VideoJob,
    hls_format_id: &str,
    ffmpeg_bin: &Path,
    dest: &Path,
    playlist_index: Option<usize>,
) -> Vec<String> {
    let out_template = dest_part_path(dest, "hls", "%(ext)s");
    let mut args = vec!["--ignore-config".to_string()];
    args.extend(playlist_scope_args(playlist_index));
    args.extend([
        "--newline".to_string(),
        "--progress".to_string(),
        "--progress-template".to_string(),
        YTDLP_PROGRESS_TEMPLATE.to_string(),
        // As in the unified builder: metadata on every leg.
        "--embed-metadata".to_string(),
        "-f".to_string(),
        hls_format_spec(&job.quality, Some(hls_format_id)),
        "-o".to_string(),
        ytdlp_output_template(&out_template),
        "--ffmpeg-location".to_string(),
        ffmpeg_location_dir(ffmpeg_bin),
        "--print".to_string(),
        "after_move:filepath".to_string(),
    ]);
    if let Some(limit) = job.speed_limit {
        args.push("--ratelimit".to_string());
        args.push(limit.to_string());
    }
    if job.keep_server_date {
        args.push("--mtime".to_string());
    }
    if job.audio_only {
        args.push("--extract-audio".to_string());
        args.push("--audio-format".to_string());
        args.push("m4a".to_string());
    } else {
        args.push("--merge-output-format".to_string());
        args.push("mp4".to_string());
        // moov atom to the front so merged mp4s start playback without a full scan.
        args.push("--postprocessor-args".to_string());
        args.push(crate::video_tools::MERGER_FASTSTART_ARGS.to_string());
    }
    if job.embed_subs {
        args.push("--embed-subs".to_string());
    }
    if job.sponsorblock_remove {
        args.push("--sponsorblock-remove".to_string());
        args.push("sponsor".to_string());
    }
    if job.sponsorblock_mark {
        args.push("--sponsorblock-mark".to_string());
        args.push("sponsor".to_string());
    }
    if job.embed_chapters {
        args.push("--embed-chapters".to_string());
    }
    if let Some(fmt) = job.remux_video.as_deref() {
        args.push("--remux-video".to_string());
        args.push(fmt.to_string());
    }
    push_vod_tail_args(&mut args, job);
    args
}
/// `--proxy` argv for one yt-dlp spawn (empty when direct; proxies carry no userinfo).
pub(crate) fn proxy_cli_args(proxy: Option<&crate::net_types::ResolvedProxy>) -> Vec<String> {
    match proxy.map(|p| p.cli_url.clone()) {
        Some(url) => vec!["--proxy".to_string(), url],
        None => Vec::new(),
    }
}

/// NO_PROXY env for one yt-dlp spawn (set only when proxied).
pub(crate) fn apply_proxy_env(
    cmd: &mut tokio::process::Command,
    proxy: Option<&crate::net_types::ResolvedProxy>,
) {
    if let Some(p) = proxy
        && !p.no_proxy_env.is_empty()
    {
        cmd.env("NO_PROXY", &p.no_proxy_env)
            .env("no_proxy", &p.no_proxy_env);
    }
}
/// Machine-readable progress lines (`[Grab];`-prefixed so after_move parsing never mistakes one for a filepath).
pub(crate) const YTDLP_PROGRESS_TEMPLATE: &str = "[Grab];%(progress.status)s;%(progress.downloaded_bytes)s;%(progress.total_bytes)s;%(progress.total_bytes_estimate)s;%(progress.speed)s;%(progress.eta)s";
