//! Native HLS/DASH segment transfer, resume and container finalization.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use thiserror::Error;

use futures_util::{StreamExt as _, TryStreamExt as _};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
use url::Url;

use crate::content_range::{ParsedContentRange, parse_content_range};
use crate::engine_msg::EngineMsg;
use crate::manifest::{
    HlsLiveAccumulator, HlsPlan, HlsResource, HlsResourceKind, ManifestByteRange,
    dash::{DashPresentation, DashRepresentationPlan, DashResource, DashResourceKind},
};
use crate::manifest_destination::{TrackFileFormat, TrackKind};
use crate::manifest_intake::ManifestLookup;
#[cfg(test)]
use crate::manifest_intake::NativeManifest;
use crate::media_types::{
    ManifestContainer, ManifestKind, ManifestRepresentationKey, ManifestSource,
};

mod dash_live;
mod hls_live;

#[cfg(test)]
use dash_live::{
    apply_dash_live_snapshot, dash_live_track_bytes, load_dash_live_track,
    load_dash_live_track_for_destination, read_dash_live_representation_key,
    validate_dash_live_state, write_dash_live_state,
};
use dash_live::{dash_live_state_exists, run_live_dash_tracks};
use hls_live::{
    LiveHlsTrackRun, ensure_live_hls_marker, read_live_hls_marker, restore_local_live_hls_tracks,
    run_live_hls_tracks,
};
#[cfg(test)]
use hls_live::{apply_live_hls_snapshot, load_live_hls_track, write_live_playlist};

/// Upper bound for one media resource, independent of a server-supplied length.
const MAX_RESOURCE_BYTES: u64 = crate::manifest_writer::MAX_RESOURCE_BYTES;

pub(super) fn staged_resource_size_ok(
    size: u64,
    range: Option<ManifestByteRange>,
    is_key: bool,
) -> bool {
    size > 0
        && size <= MAX_RESOURCE_BYTES
        && range.is_none_or(|range| size == range.length)
        && (!is_key || size == 16)
}
/// Maximum encrypted SAMPLE-AES segment size accepted by the in-memory decoder.
/// The decoder and MPEG-TS preflight both need whole-segment buffers, so this
/// limit is intentionally much smaller than the general staged-resource cap.
const MAX_SAMPLE_AES_SEGMENT_BYTES: usize = 32 * 1024 * 1024;
/// Bound one in-memory SAMPLE-AES decrypt so malformed input cannot pin a worker indefinitely.
const SAMPLE_AES_DECRYPT_TIMEOUT: Duration = Duration::from_secs(30);
/// Aggregate resource ceiling, matching the regular segmented downloader's 1 TiB trust limit.
const MAX_MANIFEST_DOWNLOAD_BYTES: u64 = crate::manifest_types::MAX_MANIFEST_TOTAL_BYTES;
/// Idle budget for receiving a response body; progressing transfers are not wall-clock limited.
const RESOURCE_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound parallel manifest requests to the same range as the regular HTTP engine.
const MAX_RESOURCE_CONNECTIONS: usize = 16;
/// Keep resources within each HLS rendition serial; separate audio/video renditions run concurrently.
const HLS_STATIC_RESOURCE_CONNECTIONS: usize = 1;
/// Keep only a small diagnostic tail from FFmpeg.
const FFMPEG_LOG_LINES: usize = 32;
/// Bound native manifest muxing, including a user-requested Finish while FFmpeg is running.
const MANIFEST_MUX_TIMEOUT: Duration = Duration::from_secs(3600);
/// Hard upper bound for an incremental VOD mux, including the time spent waiting for segments.
const INCREMENTAL_HLS_MUX_TIMEOUT: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Upper bound on the persisted dynamic DASH segment index.
const MAX_DASH_LIVE_SEGMENTS: usize = crate::manifest_types::MAX_MANIFEST_SEGMENTS;
/// Bound for a serialized dynamic DASH resume index.
const MAX_DASH_LIVE_STATE_BYTES: usize = 16 * 1024 * 1024;

/// Shared network, concurrency, and progress-reporting state for live renditions.
#[derive(Clone, Copy)]
struct LiveManifestTransfer<'a> {
    client: &'a reqwest::Client,
    concurrency: usize,
    tx: &'a tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    downloaded: &'a Arc<AtomicU64>,
}

/// Destination paths and row identity for one live DASH capture.
#[derive(Clone, Copy)]
struct DashLiveStorage<'a> {
    destination_dir: &'a Path,
    destination: &'a Path,
    item_id: u64,
    video_dir: &'a Path,
    audio_dir: &'a Path,
}

/// Selected live DASH representations for one capture.
#[derive(Clone, Copy)]
struct DashLiveSelection<'a> {
    representation_key: &'a str,
    audio_key: Option<&'a str>,
    video_plan: Option<&'a DashRepresentationPlan>,
    audio_plan: Option<&'a DashRepresentationPlan>,
}

/// Require room for a second copy of the staged media while FFmpeg muxes it.
fn check_manifest_mux_space(
    available_bytes: u64,
    staged_bytes: u64,
) -> Result<(), ManifestRunError> {
    if available_bytes < staged_bytes {
        return Err(ManifestRunError::Failed(format!(
            "Not enough free disk space to finalize this stream (about {staged_bytes} bytes are needed; {available_bytes} bytes are available)"
        )));
    }
    Ok(())
}

/// Check the destination filesystem, using space available to this unprivileged process.
fn ensure_manifest_mux_space(
    destination_dir: &Path,
    staged_bytes: u64,
) -> Result<(), ManifestRunError> {
    let stats = rustix::fs::statvfs(destination_dir).map_err(|error| {
        ManifestRunError::Failed(format!(
            "Could not check available disk space at {}: {error}",
            destination_dir.display()
        ))
    })?;
    let available_bytes = stats.f_bavail.checked_mul(stats.f_frsize).ok_or_else(|| {
        ManifestRunError::Failed("The available disk-space byte count overflowed".to_string())
    })?;
    check_manifest_mux_space(available_bytes, staged_bytes)
}

/// Check both the persistent segment filesystem and the final download filesystem.
fn ensure_manifest_mux_spaces(
    staging_dir: &Path,
    destination_dir: &Path,
    staged_bytes: u64,
) -> Result<(), ManifestRunError> {
    ensure_manifest_mux_space(staging_dir, staged_bytes)?;
    ensure_manifest_mux_space(destination_dir, staged_bytes)
}

/// Estimate the remaining incremental HLS output from bytes fetched so far.
///
/// Stream-copy muxing adds container overhead. Reserve 10% above staged
/// resource bytes, then subtract bytes already written to the destination part.
fn incremental_hls_remaining_output_bytes(staged_bytes: u64, output_bytes: u64) -> u64 {
    let estimated_output_bytes = staged_bytes.saturating_add(staged_bytes.div_ceil(10));
    estimated_output_bytes.saturating_sub(output_bytes)
}

/// Check destination free space as incremental HLS resources become available.
///
/// This is called after each resource completes. The first media resource is
/// checked before its growing playlist is published to FFmpeg, so FFmpeg does
/// not start without an output-space estimate. Later checks account for output
/// bytes already written to the visible destination part.
async fn ensure_incremental_hls_output_space(
    destination_dir: &Path,
    output_part: &Path,
    staged_bytes: u64,
) -> Result<(), ManifestRunError> {
    let metadata = tokio::fs::symlink_metadata(output_part)
        .await
        .map_err(|error| {
            manifest_storage_error("Could not inspect the incremental HLS output part", error)
        })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ManifestRunError::Failed(
            "The incremental HLS output part is not a regular file".to_string(),
        ));
    }
    let required_bytes = incremental_hls_remaining_output_bytes(staged_bytes, metadata.len());
    ensure_manifest_mux_space(destination_dir, required_bytes)
}

fn manifest_storage_error(context: &str, error: std::io::Error) -> ManifestRunError {
    let storage_exhausted = rustix::io::Errno::from_io_error(&error).is_some_and(|errno| {
        errno == rustix::io::Errno::NOSPC || errno == rustix::io::Errno::DQUOT
    });
    if storage_exhausted {
        ManifestRunError::Failed(
            "Not enough free disk space or storage quota to save this stream".to_string(),
        )
    } else {
        ManifestRunError::Failed(format!("{context}: {error}"))
    }
}

/// Reject a response whose declared length exceeds the requested byte range.
fn byte_range_content_length_exceeds(
    range: crate::manifest::ManifestByteRange,
    content_length: Option<u64>,
) -> bool {
    content_length.is_some_and(|content_length| content_length > range.length)
}

/// Reject a byte-range chunk before it is written if it exceeds the requested extent.
fn byte_range_chunk_exceeds(
    range: crate::manifest::ManifestByteRange,
    written: u64,
    chunk_len: usize,
) -> bool {
    match u64::try_from(chunk_len) {
        Ok(chunk_len) => chunk_len > range.length.saturating_sub(written),
        Err(_) => true,
    }
}

fn ffmpeg_log_indicates_storage_full(log: &str) -> bool {
    let log = log.to_ascii_lowercase();
    log.contains("no space left on device") || log.contains("disk quota exceeded")
}

/// Classify mux failures as terminal so staged resources remain available for retry.
fn subtitle_mux_unsupported(log: &str) -> bool {
    let log = log.to_ascii_lowercase();
    [
        "matches no streams",
        "unknown decoder",
        "decoder not found",
        "unknown encoder",
        "encoder not found",
        "could not find tag for codec",
        "subtitle codec",
        "unsupported codec",
        "codec is not supported",
    ]
    .iter()
    .any(|needle| log.contains(needle))
}

/// Whether FFmpeg rejected the input presentation rather than its output destination.
fn ffmpeg_rejected_input(log: &str) -> bool {
    let log = log.to_ascii_lowercase();
    log.contains("invalid data found when processing input")
        || log.contains("moov atom not found")
        || log.contains("ebml header parsing failed")
        || log.contains("error reading header")
        || log.contains("could not find codec parameters for stream")
}

/// Classify a failed FFmpeg pass: unsupported source media falls back to yt-dlp,
/// while local output/storage failures remain terminal and preserve staged inputs.
fn mux_failure_error(log: &str) -> ManifestRunError {
    if ffmpeg_log_indicates_storage_full(log) {
        return ManifestRunError::Failed(
            "Not enough free disk space to finalize this stream".to_string(),
        );
    }
    if ffmpeg_rejected_input(log) {
        return ManifestRunError::Unsupported(
            "FFmpeg could not demux the selected stream".to_string(),
        );
    }

    let detail = log
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("FFmpeg could not mux the stream");
    ManifestRunError::Failed(format!("FFmpeg could not mux the stream: {detail}"))
}

/// Inputs retained by the queue; URLs for individual segments are resolved again on every attempt.
#[derive(Clone, Debug)]
pub(crate) struct ManifestJob {
    pub(crate) item_id: u64,
    pub(crate) source: ManifestSource,
    pub(crate) referer: Option<String>,
    pub(crate) dest: PathBuf,
    pub(crate) proxy: Option<crate::net_types::ResolvedProxy>,
    pub(crate) cookies_browser: String,
    pub(crate) connections: usize,
    pub(crate) embed_subs: bool,
}

/// Classifies native manifest failures so transient transfers do not discard staged resources.
#[must_use]
#[derive(Debug)]
pub(crate) enum ManifestRunError {
    Unsupported(String),
    Retryable(String),
    Failed(String),
}

impl fmt::Display for ManifestRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Unsupported(message) | Self::Retryable(message) | Self::Failed(message) => {
                message
            }
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for ManifestRunError {}

/// Failures while validating manifest-derived resource hosts.
#[derive(Debug, Error, PartialEq, Eq)]
enum ManifestChildUrlError {
    #[error("The stream manifest exceeds the limit of {0} distinct resource hosts")]
    TooManyDistinctHosts(usize),
    #[error("Grab reached its limit for simultaneously protected public download hosts")]
    PolicyCapReached,
    #[error("The stream manifest references a local or private network resource")]
    UnsafeDestination,
}

/// Failures while creating or validating a per-track staging directory.
#[derive(Debug, Error)]
enum ManifestTrackDirError {
    #[error("Could not inspect manifest staging: {0}")]
    Inspect(#[source] std::io::Error),
    #[error("Manifest staging path is not a real directory")]
    NotDirectory,
    #[error("Could not create manifest staging: {0}")]
    Create(#[source] std::io::Error),
}

impl From<ManifestChildUrlError> for ManifestRunError {
    fn from(error: ManifestChildUrlError) -> Self {
        Self::Failed(error.to_string())
    }
}

impl From<ManifestTrackDirError> for ManifestRunError {
    fn from(error: ManifestTrackDirError) -> Self {
        Self::Failed(error.to_string())
    }
}

impl ManifestRunError {
    pub(crate) fn into_engine_message(self) -> EngineMsg {
        match self {
            Self::Unsupported(error) => EngineMsg::ManifestFallback(error),
            Self::Retryable(error) => EngineMsg::ManifestRetryable(error),
            Self::Failed(error) => EngineMsg::Failed(error),
        }
    }
}

impl From<String> for ManifestRunError {
    fn from(error: String) -> Self {
        Self::Failed(error)
    }
}

impl From<crate::manifest_intake::ManifestIntakeError> for ManifestRunError {
    fn from(error: crate::manifest_intake::ManifestIntakeError) -> Self {
        match error {
            crate::manifest_intake::ManifestIntakeError::Unsupported(error) => {
                Self::Unsupported(error)
            }
            crate::manifest_intake::ManifestIntakeError::Retryable(error) => Self::Retryable(error),
            crate::manifest_intake::ManifestIntakeError::Failed(error) => Self::Failed(error),
        }
    }
}

/// Persist an unrecoverable live-capture decision before returning from the capture path.
/// The caller supplies bytes for the retained video track only; audio is discarded
/// when continuity can no longer be established.
async fn persist_unrecoverable_live_capture(
    staging_dir: &Path,
    protocol: ManifestKind,
    reason: &str,
    staged_bytes: u64,
    downloaded: &AtomicU64,
    tx: &tokio::sync::mpsc::UnboundedSender<EngineMsg>,
) -> Result<(), ManifestRunError> {
    crate::live_capture_state::persist(staging_dir, protocol, reason, staged_bytes)
        .await
        .map_err(|error| ManifestRunError::Failed(error.to_string()))?;
    downloaded.store(staged_bytes, Ordering::Relaxed);
    tx.send(progress_message(staged_bytes)).ok();
    Ok(())
}

/// An origin-only identifier safe for diagnostics. It deliberately omits
/// user-info, path, query and fragment, all of which may contain credentials.
fn diagnostic_url(url: &Url) -> String {
    let mut origin = format!(
        "{}://{}",
        url.scheme(),
        url.host_str().unwrap_or("<unknown>")
    );
    if let Some(port) = url.port() {
        origin.push_str(&format!(":{port}"));
    }
    origin
}

/// Result of one native manifest attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ManifestOutcome {
    Finished(u64),
    Paused,
    Aborted,
}

#[derive(Clone, Debug)]
struct Resource {
    url: Url,
    range: Option<ManifestByteRange>,
    filename: String,
    key: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SubtitleFormat {
    #[default]
    Text,
    Fmp4,
}

enum Tracks {
    Hls {
        video: Box<HlsPlan>,
        audio: Option<Box<HlsPlan>>,
        subtitle: Option<Box<HlsPlan>>,
        subtitle_language: Option<String>,
        subtitle_format: SubtitleFormat,
    },
    Dash {
        video: Box<DashRepresentationPlan>,
        audio: Option<Box<DashRepresentationPlan>>,
        subtitle: Option<Box<DashRepresentationPlan>>,
        subtitle_language: Option<String>,
    },
    DashLiveOffline {
        has_audio: bool,
    },
}

impl Tracks {
    fn supports_incremental_hls_vod_track(plan: &HlsPlan) -> bool {
        !plan.is_live
            && plan.end_list
            && plan.low_latency.is_none()
            && (plan.is_destination_assemblable_candidate()
                || plan.is_incremental_clear_vod_candidate()
                || plan.is_incremental_aes128_mpegts_vod_candidate())
    }

    fn uses_incremental_hls_vod(&self) -> bool {
        let Self::Hls {
            video,
            audio,
            subtitle: None,
            ..
        } = self
        else {
            return false;
        };

        let video_incremental = video.is_incremental_aes128_mpegts_vod_candidate()
            || video.is_incremental_clear_vod_candidate();
        if audio.is_none() {
            return video_incremental;
        }

        let video_supported = Self::supports_incremental_hls_vod_track(video);
        let audio_incremental = audio.as_deref().is_some_and(|plan| {
            plan.is_incremental_clear_vod_candidate()
                || plan.is_incremental_aes128_mpegts_vod_candidate()
        });
        let audio_supported = audio
            .as_deref()
            .is_none_or(Self::supports_incremental_hls_vod_track);

        video_supported && audio_supported && (video_incremental || audio_incremental)
    }

    fn uses_destination_parts(&self) -> bool {
        match self {
            Self::Hls { video, audio, .. } => {
                video.is_destination_assemblable_candidate()
                    && audio
                        .as_deref()
                        .is_none_or(HlsPlan::is_destination_assemblable_candidate)
            }
            Self::Dash { .. } => true,
            Self::DashLiveOffline { .. } => false,
        }
    }
}

#[derive(Debug)]
struct LiveHlsTrack {
    /// Effective post-redirect playlist URL used for resource resolution and polling.
    playlist_url: Url,
    inherited_variables: Box<[(String, String)]>,
    directory: PathBuf,
    accumulator: HlsLiveAccumulator,
    snapshot_at: Instant,
    last_full_resource_verification: Instant,
    ended: bool,
    next_poll_at: Option<Instant>,
    target_duration: u64,
    low_latency: Option<crate::manifest::HlsLowLatencyPlan>,
    destination_part: Option<LiveHlsDestination>,
}

impl LiveHlsTrack {
    fn part_target(&self) -> Option<Duration> {
        self.low_latency.as_ref()?.part_target
    }

    fn blocking_reload(&self) -> Option<(u64, Option<u64>)> {
        self.low_latency.as_ref()?.blocking_reload
    }

    fn can_skip_until(&self) -> Option<Duration> {
        self.low_latency.as_ref()?.can_skip_until
    }

    fn rendition_reports(&self) -> &[crate::manifest::hls_ll::HlsRenditionReport] {
        self.low_latency
            .as_ref()
            .map_or(&[], |state| state.rendition_reports.as_ref())
    }

    fn set_blocking_reload(&mut self, target: Option<(u64, Option<u64>)>) {
        if let Some(state) = self.low_latency.as_mut() {
            state.blocking_reload = target;
        }
    }
}

#[derive(Clone, Debug)]
struct LiveHlsDestination {
    staging: PathBuf,
    destination: PathBuf,
    item_id: u64,
    track: TrackKind,
    filename: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LiveHlsCaptureOutcome {
    Ended,
    Paused,
    Finish,
    FinishVideoOnly,
    Unrecoverable,
    Discard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LiveHlsOutput {
    LocalPlaylist,
    DestinationParts,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LiveHlsCapture {
    outcome: LiveHlsCaptureOutcome,
    output: LiveHlsOutput,
}

impl LiveHlsCapture {
    fn new(outcome: LiveHlsCaptureOutcome, output: LiveHlsOutput) -> Self {
        Self { outcome, output }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResourceFetchOutcome {
    Complete,
    Stopped(crate::video::StopIntent),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LiveHlsSnapshotOutcome {
    Committed,
    RetryScheduled,
    Stopped(crate::video::StopIntent),
    ContinuityLost,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct DashLiveSegment {
    number: u64,
    filename: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct DashLiveState {
    version: u32,
    representation_key: ManifestRepresentationKey,
    initialization: Option<String>,
    segments: Vec<DashLiveSegment>,
}

impl DashLiveState {
    fn new(representation_key: &str) -> Self {
        Self {
            version: 1,
            representation_key: ManifestRepresentationKey::from(representation_key),
            initialization: None,
            segments: Vec::new(),
        }
    }

    fn resource_names(&self) -> impl Iterator<Item = &str> {
        self.initialization.iter().map(String::as_str).chain(
            self.segments
                .iter()
                .map(|segment| segment.filename.as_str()),
        )
    }

    fn next_number(&self) -> Option<u64> {
        self.segments
            .last()
            .and_then(|segment| segment.number.checked_add(1))
    }
}

#[derive(Clone, Debug)]
struct DashLiveDestination {
    destination: PathBuf,
    item_id: u64,
    track: TrackKind,
}

#[derive(Debug)]
struct DashLiveTrack {
    directory: PathBuf,
    state: DashLiveState,
    track_filename: &'static str,
    // Verified once on load; a non-empty prefix is immutable across refreshes.
    committed_resource_count: usize,
    committed_bytes: u64,
    destination_part: Option<DashLiveDestination>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DashLiveUpdateOutcome {
    Committed,
    ContinuityLost,
    Stopped(crate::video::StopIntent),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DashLiveCaptureOutcome {
    Ended,
    Paused,
    Finish,
    FinishVideoOnly,
    Unrecoverable,
    Discard,
}

fn discard_unrecoverable_dash_capture_without_video(
    outcome: DashLiveCaptureOutcome,
    has_video_segments: bool,
) -> DashLiveCaptureOutcome {
    if outcome == DashLiveCaptureOutcome::Unrecoverable && !has_video_segments {
        DashLiveCaptureOutcome::Discard
    } else {
        outcome
    }
}

#[derive(Debug)]
struct DashLiveCapture {
    outcome: DashLiveCaptureOutcome,
    video_resources: Vec<String>,
    audio_resources: Option<Vec<String>>,
}

/// Resolve the complete FFmpeg toolchain before creating staging or fetching resources.
fn resolve_manifest_ffmpeg(dirs: &[PathBuf]) -> Result<PathBuf, ManifestRunError> {
    let ffmpeg_dir = crate::video_tools::toolchain_dir_in(dirs).ok_or_else(|| {
        ManifestRunError::Failed(
            "FFmpeg and ffprobe are required for native stream downloads".to_string(),
        )
    })?;
    Ok(ffmpeg_dir.join("ffmpeg"))
}

/// Result of preparing a native manifest attempt, including an early user stop.
enum ManifestPreparationOutcome {
    Ready(Box<ManifestPreparation>),
    Stopped(crate::video::StopIntent),
}

async fn await_manifest_operation_or_stop<T>(
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
    operation: impl std::future::Future<Output = T>,
) -> Result<T, crate::video::StopIntent> {
    tokio::select! {
        biased;
        intent = &mut *abort => Err(crate::video::stop_intent_or_pause(intent)),
        result = operation => Ok(result),
    }
}

/// State resolved before any manifest resources are transferred.
struct ManifestPreparation {
    staging: PathBuf,
    dest_dir: PathBuf,
    output_part: PathBuf,
    video_dir: PathBuf,
    audio_dir: PathBuf,
    client: reqwest::Client,
    tracks: Tracks,
    guarded_child_hosts: crate::download_net::PublicResolutionSet,
    has_audio: bool,
    live_hls: bool,
    live_dash: bool,
    finish_requested: bool,
    concurrency: usize,
    downloaded: Arc<AtomicU64>,
}

/// Recover staged state, resolve the selected tracks, and prepare the transfer client.
async fn prepare_manifest_attempt(
    job: &ManifestJob,
    tx: &tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
) -> Result<ManifestPreparationOutcome, ManifestRunError> {
    let dest_dir = job
        .dest
        .parent()
        .ok_or_else(|| {
            ManifestRunError::Failed("The download destination has no parent directory".to_string())
        })?
        .to_path_buf();
    let parent = dest_dir.clone();
    let output_container = manifest_output_container(&job.dest)?;
    let item_id = job.item_id;
    let staging = tokio::task::spawn_blocking(move || {
        crate::video_staging::ensure_manifest_staging_dir(&parent, item_id)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("Could not prepare manifest staging: {error}"))??;

    // Bind every staged resource and interrupted output to the exact persisted
    // selection and output settings before touching any existing staging data.
    let recovery_fingerprint = crate::manifest_recovery::fingerprint(
        &job.source,
        output_container.extension(),
        job.embed_subs,
    )
    .map_err(|error| ManifestRunError::Failed(error.to_string()))?;
    crate::manifest_recovery::validate_or_initialize(&staging, recovery_fingerprint)
        .await
        .map_err(|error| ManifestRunError::Failed(error.to_string()))?;
    let item_id = job.item_id;
    let destination_path = job.dest.clone();
    let has_audio = job.source.audio_key.is_some();
    let has_subtitles = job.source.subtitle_key.is_some();
    let staging_for_plan = staging.clone();
    tokio::task::spawn_blocking(move || {
        crate::manifest_destination::ensure_plan(
            &staging_for_plan,
            &destination_path,
            item_id,
            has_audio,
            has_subtitles,
        )
    })
    .await
    .map_err(|error| format!("Could not prepare destination-side manifest files: {error}"))?
    .map_err(ManifestRunError::Failed)?;

    // Reclaim either supported container left by an interrupted attempt only
    // after validating both the state-dir owner marker and the recovery plan.
    for extension in ["mp4", "mkv", "ts"] {
        remove_regular_file_if_present(&staging.join(format!("output.part.{extension}"))).await?;
    }
    let output_part = staging.join(format!("output.part.{}", output_container.extension()));
    let video_dir = staging.join("video");
    ensure_track_dir(&video_dir).await?;
    let audio_dir = staging.join("audio");
    let live_marker_exists = read_live_hls_marker(&staging)
        .await
        .map_err(ManifestRunError::Failed)?;
    let unrecoverable_state = crate::live_capture_state::read(&staging)
        .await
        .map_err(|error| ManifestRunError::Failed(error.to_string()))?;
    if let Some(state) = unrecoverable_state.as_ref()
        && state.protocol != job.source.kind
    {
        return Err(ManifestRunError::Failed(
            "The unrecoverable live-capture marker belongs to a different manifest protocol"
                .to_string(),
        ));
    }
    let mut finish_marker_bytes = read_live_finish_marker(&staging)
        .await
        .map_err(ManifestRunError::Failed)?;
    if let Some(state) = unrecoverable_state.as_ref() {
        if finish_marker_bytes.is_none() {
            ensure_live_finish_marker(&staging, state.staged_bytes)
                .await
                .map_err(ManifestRunError::Failed)?;
            finish_marker_bytes = Some(state.staged_bytes);
        }
        tracing::warn!(
            item_id = job.item_id,
            protocol = ?state.protocol,
            reason = %state.reason,
            "resuming finalization of an unrecoverable live capture without polling its source"
        );
    }
    let finish_requested = finish_marker_bytes.is_some();
    let dash_live_requested = job.source.kind == ManifestKind::Dash
        && job
            .source
            .representation_key
            .as_str()
            .starts_with("dash-live-");
    // A crash may occur after the unrecoverable marker is synced but before
    // the stale audio index is removed. Enforce the video-only decision before
    // restoring either protocol's local track state.
    if let Some(state) = unrecoverable_state.as_ref() {
        match state.protocol {
            ManifestKind::Hls => {
                remove_regular_file_if_present(&audio_dir.join("local.m3u8"))
                    .await
                    .map_err(ManifestRunError::Failed)?;
            }
            ManifestKind::Dash => {
                remove_regular_file_if_present(&audio_dir.join(DASH_LIVE_STATE_NAME))
                    .await
                    .map_err(ManifestRunError::Failed)?;
            }
        }
    }
    let mut offline_tracks =
        if (finish_requested || live_marker_exists) && job.source.kind == ManifestKind::Hls {
            restore_local_live_hls_tracks(&job.source.url, &video_dir, &audio_dir).await?
        } else if dash_live_requested && dash_live_state_exists(&video_dir).await? {
            let has_audio = dash_live_state_exists(&audio_dir).await?;
            Some(Tracks::DashLiveOffline { has_audio })
        } else {
            None
        };
    if finish_requested && offline_tracks.is_none() {
        return Err(ManifestRunError::Failed(
            "The finished live recording has no committed staging data".to_string(),
        ));
    }

    let resolved = match await_manifest_operation_or_stop(
        abort,
        resolve_manifest_tracks(job, &mut offline_tracks, finish_requested),
    )
    .await
    {
        Ok(result) => result?,
        Err(crate::video::StopIntent::Finish) if offline_tracks.is_some() => {
            // Re-arm Finish for the live finalization path so it can persist
            // the marker with the actual staged-byte count before finalizing.
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let _ = sender.send(crate::video::StopIntent::Finish);
            *abort = receiver;
            resolve_manifest_tracks(job, &mut offline_tracks, true).await?
        }
        Err(intent) => return Ok(ManifestPreparationOutcome::Stopped(intent)),
    };
    let ResolvedManifestTracks {
        client,
        tracks,
        guarded_child_hosts,
        has_audio,
    } = resolved;
    if has_audio {
        ensure_track_dir(&audio_dir).await?;
    }

    let live_source = matches!(
        &tracks,
        Tracks::Hls { video, audio, .. }
            if video.is_live || audio.as_ref().is_some_and(|track| track.is_live)
    );
    let live_hls = matches!(&tracks, Tracks::Hls { .. }) && (live_source || live_marker_exists);
    let live_dash = dash_live_requested
        && matches!(
            &tracks,
            Tracks::Dash { .. } | Tracks::DashLiveOffline { .. }
        );
    if live_source {
        ensure_live_hls_marker(&staging)
            .await
            .map_err(ManifestRunError::Failed)?;
    } else if live_marker_exists && !matches!(&tracks, Tracks::Hls { .. }) {
        return Err(ManifestRunError::Failed(
            "A live HLS staging marker belongs to a non-HLS queue item".to_string(),
        ));
    }
    if !finish_requested && (live_hls || live_dash) {
        tx.send(EngineMsg::LiveDetected).ok();
    }
    tx.send(EngineMsg::Phase(if finish_requested || live_hls {
        "Finalizing live stream…".to_string()
    } else if live_source || live_dash {
        "Recording live stream…".to_string()
    } else {
        "Downloading stream segments…".to_string()
    }))
    .ok();
    let concurrency = job.connections.clamp(1, MAX_RESOURCE_CONNECTIONS);
    let downloaded = Arc::new(AtomicU64::new(finish_marker_bytes.unwrap_or(0)));

    Ok(ManifestPreparationOutcome::Ready(Box::new(
        ManifestPreparation {
            staging,
            dest_dir,
            output_part,
            video_dir,
            audio_dir,
            client,
            tracks,
            guarded_child_hosts,
            has_audio,
            live_hls,
            live_dash,
            finish_requested,
            concurrency,
            downloaded,
        },
    )))
}

/// Resolved manifest tracks and the network client used to transfer them.
struct ResolvedManifestTracks {
    client: reqwest::Client,
    tracks: Tracks,
    guarded_child_hosts: crate::download_net::PublicResolutionSet,
    has_audio: bool,
}

/// Resolve selected tracks, falling back to committed live resources when the source is gone.
async fn resolve_manifest_tracks(
    job: &ManifestJob,
    offline_tracks: &mut Option<Tracks>,
    finish_requested: bool,
) -> Result<ResolvedManifestTracks, ManifestRunError> {
    let (client, force_offline) = if finish_requested {
        (reqwest::Client::new(), true)
    } else {
        match crate::manifest_intake::client_for_manifest(
            &job.source.url,
            &job.cookies_browser,
            job.proxy.as_ref(),
            job.referer
                .as_deref()
                .map(|source| crate::referrer::RefererContext {
                    source,
                    target: &job.source.url,
                }),
        )
        .await
        {
            Ok(client) => (client, false),
            Err(error) if offline_tracks.is_some() => {
                tracing::warn!(
                    item_id = job.item_id,
                    error = %error,
                    "live stream source is unavailable; finalizing the committed local capture"
                );
                (reqwest::Client::new(), true)
            }
            Err(error) => {
                return Err(ManifestRunError::Failed(format!(
                    "Could not configure manifest networking: {error}"
                )));
            }
        }
    };

    let mut guarded_child_hosts = crate::download_net::PublicResolutionSet::default();
    let tracks = if force_offline {
        offline_tracks.take().ok_or_else(|| {
            ManifestRunError::Failed(
                "The saved live HLS recording is no longer available".to_string(),
            )
        })?
    } else {
        match crate::manifest_intake::lookup_with_client(&client, &job.source.url).await {
            Ok(Some(lookup)) if lookup.kind() == job.source.kind => {
                match resolve_tracks(&client, lookup, &job.source, &mut guarded_child_hosts).await {
                    Ok(tracks) => tracks,
                    Err(error) if offline_tracks.is_some() => {
                        tracing::warn!(
                            item_id = job.item_id,
                            error = %error,
                            "live stream source no longer resolves; finalizing the committed local capture"
                        );
                        offline_tracks.take().ok_or_else(|| {
                            ManifestRunError::Failed(
                                "The saved live HLS recording is no longer available".to_string(),
                            )
                        })?
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok(Some(_)) | Ok(None) => {
                if let Some(tracks) = offline_tracks.take() {
                    tracing::warn!(
                        item_id = job.item_id,
                        "live stream source changed or is no longer supported; finalizing the committed local capture"
                    );
                    tracks
                } else {
                    return Err(ManifestRunError::Unsupported(
                        "The stream manifest is no longer supported".to_string(),
                    ));
                }
            }
            Err(error) if offline_tracks.is_some() => {
                tracing::warn!(
                    item_id = job.item_id,
                    error = %error,
                    "live HLS source is unavailable; finalizing the committed local capture"
                );
                offline_tracks.take().ok_or_else(|| {
                    ManifestRunError::Failed(
                        "The saved live HLS recording is no longer available".to_string(),
                    )
                })?
            }
            Err(error) => return Err(error.into()),
        }
    };
    let has_audio = match &tracks {
        Tracks::Hls { audio, .. } => audio.is_some(),
        Tracks::Dash { audio, .. } => audio.is_some(),
        Tracks::DashLiveOffline { has_audio } => *has_audio,
    };
    Ok(ResolvedManifestTracks {
        client,
        tracks,
        guarded_child_hosts,
        has_audio,
    })
}

/// Inputs for downloading and assembling the selected non-live manifest tracks.
struct ManifestStaticTransfer<'a> {
    client: &'a reqwest::Client,
    tracks: &'a Tracks,
    destination_parts: bool,
    staging: &'a Path,
    destination: &'a Path,
    item_id: u64,
    video_dir: &'a Path,
    audio_dir: &'a Path,
    concurrency: usize,
    tx: &'a tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    downloaded: &'a Arc<AtomicU64>,
    abort: &'a mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
}

/// Common fields shared by every selected static manifest track transfer.
struct ManifestStaticTrackContext<'a> {
    client: &'a reqwest::Client,
    staging: &'a Path,
    destination: &'a Path,
    item_id: u64,
    tx: &'a tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    downloaded: &'a Arc<AtomicU64>,
}

impl ManifestStaticTrackContext<'_> {
    fn hls_track<'a>(
        &'a self,
        plan: &'a HlsPlan,
        directory: &'a Path,
        filename: &'a str,
        track: TrackKind,
        abort: &'a mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
    ) -> ManifestHlsTrackTransfer<'a> {
        ManifestHlsTrackTransfer {
            client: self.client,
            staging: self.staging,
            plan,
            directory,
            filename,
            destination: self.destination,
            item_id: self.item_id,
            track,
            tx: self.tx,
            downloaded: self.downloaded,
            abort,
        }
    }

    fn dash_track<'a>(
        &'a self,
        plan: &'a DashRepresentationPlan,
        directory: &'a Path,
        filename: &'a str,
        track: TrackKind,
        connections: usize,
        abort: &'a mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
    ) -> ManifestDashTrackTransfer<'a> {
        ManifestDashTrackTransfer {
            client: self.client,
            staging: self.staging,
            plan,
            directory,
            filename,
            destination: self.destination,
            item_id: self.item_id,
            track,
            connections,
            tx: self.tx,
            downloaded: self.downloaded,
            abort,
        }
    }
}

/// Keep both track errors: terminal failures win, then unsupported tracks
/// trigger the yt-dlp fallback before a retry of a native pair that cannot work.
fn combine_parallel_track_errors(
    video: ManifestRunError,
    audio: ManifestRunError,
) -> ManifestRunError {
    let message = format!("video track: {video}; audio track: {audio}");
    if matches!(&video, ManifestRunError::Failed(_))
        || matches!(&audio, ManifestRunError::Failed(_))
    {
        ManifestRunError::Failed(message)
    } else if matches!(&video, ManifestRunError::Unsupported(_))
        || matches!(&audio, ManifestRunError::Unsupported(_))
    {
        ManifestRunError::Unsupported(message)
    } else {
        ManifestRunError::Retryable(message)
    }
}

/// Run a static video/audio track pair concurrently and forward stop intent to both.
async fn transfer_parallel_manifest_tracks<Video, Audio>(
    video: Video,
    audio: Audio,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
    video_abort: tokio::sync::oneshot::Sender<crate::video::StopIntent>,
    audio_abort: tokio::sync::oneshot::Sender<crate::video::StopIntent>,
) -> Result<bool, ManifestRunError>
where
    Video: std::future::Future<Output = Result<bool, ManifestRunError>>,
    Audio: std::future::Future<Output = Result<bool, ManifestRunError>>,
{
    tokio::pin!(video);
    tokio::pin!(audio);
    let stop_intent = async { crate::video::stop_intent_or_pause((&mut *abort).await) };
    tokio::pin!(stop_intent);

    let mut video_abort = Some(video_abort);
    let mut audio_abort = Some(audio_abort);
    let mut video_finished = false;
    let mut audio_finished = false;

    loop {
        if video_finished && audio_finished {
            return Ok(true);
        }

        tokio::select! {
            biased;
            intent = &mut stop_intent => {
                if let Some(sender) = video_abort.take() {
                    let _ = sender.send(intent);
                }
                if let Some(sender) = audio_abort.take() {
                    let _ = sender.send(intent);
                }
                if video_finished {
                    let _ = (&mut audio).await;
                } else if audio_finished {
                    let _ = (&mut video).await;
                } else {
                    let _ = tokio::join!(&mut video, &mut audio);
                }
                return Ok(false);
            }
            result = &mut video, if !video_finished => {
                video_finished = true;
                if !matches!(&result, Ok(true)) {
                    let video_error = result.err();
                    let audio_error = if !audio_finished {
                        if let Some(sender) = audio_abort.take() {
                            let _ = sender.send(crate::video::StopIntent::Pause);
                        }
                        (&mut audio).await.err()
                    } else {
                        None
                    };
                    return match (video_error, audio_error) {
                        (Some(video_error), Some(audio_error)) => {
                            Err(combine_parallel_track_errors(video_error, audio_error))
                        }
                        (Some(error), None) | (None, Some(error)) => Err(error),
                        (None, None) => Ok(false),
                    };
                }
            }
            result = &mut audio, if !audio_finished => {
                audio_finished = true;
                if !matches!(&result, Ok(true)) {
                    let audio_error = result.err();
                    let video_error = if !video_finished {
                        if let Some(sender) = video_abort.take() {
                            let _ = sender.send(crate::video::StopIntent::Pause);
                        }
                        (&mut video).await.err()
                    } else {
                        None
                    };
                    return match (video_error, audio_error) {
                        (Some(video_error), Some(audio_error)) => {
                            Err(combine_parallel_track_errors(video_error, audio_error))
                        }
                        (Some(error), None) | (None, Some(error)) => Err(error),
                        (None, None) => Ok(false),
                    };
                }
            }
        }
    }
}

/// Split a multi-track DASH request budget without exceeding its configured total.
fn split_manifest_track_connections(connections: usize) -> (usize, usize) {
    let total = connections.clamp(2, MAX_RESOURCE_CONNECTIONS);
    (total.div_ceil(2), total / 2)
}

#[cfg(test)]
mod manifest_track_connection_tests {
    use super::*;

    #[test]
    fn split_track_connection_budget_is_positive_and_bounded() {
        for configured in 2..=MAX_RESOURCE_CONNECTIONS {
            let (video, audio) = split_manifest_track_connections(configured);
            assert!(video >= 1);
            assert!(audio >= 1);
            assert_eq!(video + audio, configured);
        }

        assert_eq!(
            split_manifest_track_connections(MAX_RESOURCE_CONNECTIONS + 1),
            (MAX_RESOURCE_CONNECTIONS / 2, MAX_RESOURCE_CONNECTIONS / 2)
        );
    }
}

#[cfg(test)]
mod parallel_manifest_track_tests {
    use super::*;

    #[tokio::test]
    async fn static_track_pair_polls_video_and_audio_concurrently() {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let video_barrier = Arc::clone(&barrier);
        let audio_barrier = Arc::clone(&barrier);
        let video = async move {
            video_barrier.wait().await;
            Ok(true)
        };
        let audio = async move {
            audio_barrier.wait().await;
            Ok(true)
        };
        let (_keep_stop_sender, mut abort) = tokio::sync::oneshot::channel();
        let (video_abort, _video_abort_receiver) = tokio::sync::oneshot::channel();
        let (audio_abort, _audio_abort_receiver) = tokio::sync::oneshot::channel();

        assert!(
            transfer_parallel_manifest_tracks(video, audio, &mut abort, video_abort, audio_abort,)
                .await
                .expect("both track transfers should complete")
        );
    }

    #[tokio::test]
    async fn static_track_pair_broadcasts_the_original_stop_intent() {
        let (stop_sender, mut abort) = tokio::sync::oneshot::channel();
        let (video_abort, video_abort_receiver) = tokio::sync::oneshot::channel();
        let (audio_abort, audio_abort_receiver) = tokio::sync::oneshot::channel();
        let video = async move {
            assert_eq!(
                video_abort_receiver
                    .await
                    .expect("video stop should be forwarded"),
                crate::video::StopIntent::Finish
            );
            Ok(false)
        };
        let audio = async move {
            assert_eq!(
                audio_abort_receiver
                    .await
                    .expect("audio stop should be forwarded"),
                crate::video::StopIntent::Finish
            );
            Ok(false)
        };

        let transfer =
            transfer_parallel_manifest_tracks(video, audio, &mut abort, video_abort, audio_abort);
        let (result, sent) = tokio::join!(transfer, async move {
            stop_sender.send(crate::video::StopIntent::Finish)
        });

        assert!(sent.is_ok(), "the manager stop signal should be accepted");
        assert!(!result.expect("stopped track transfers should not fail"));
    }

    #[tokio::test]
    async fn static_track_pair_stops_the_peer_when_one_track_fails() {
        let (_keep_stop_sender, mut abort) = tokio::sync::oneshot::channel();
        let (video_abort, _video_abort_receiver) = tokio::sync::oneshot::channel();
        let (audio_abort, audio_abort_receiver) = tokio::sync::oneshot::channel();
        let video = async { Err(ManifestRunError::Failed("video track failed".to_string())) };
        let audio = async move {
            assert_eq!(
                audio_abort_receiver
                    .await
                    .expect("peer stop should be forwarded"),
                crate::video::StopIntent::Pause
            );
            Ok(false)
        };

        let result =
            transfer_parallel_manifest_tracks(video, audio, &mut abort, video_abort, audio_abort)
                .await;

        assert!(matches!(
            result,
            Err(ManifestRunError::Failed(message)) if message == "video track failed"
        ));
    }

    #[tokio::test]
    async fn static_track_pair_prefers_video_unsupported_over_audio_retryable() {
        let (_keep_stop_sender, mut abort) = tokio::sync::oneshot::channel();
        let (video_abort, _video_abort_receiver) = tokio::sync::oneshot::channel();
        let (audio_abort, audio_abort_receiver) = tokio::sync::oneshot::channel();
        let video = async {
            Err(ManifestRunError::Unsupported(
                "video format is unsupported".to_string(),
            ))
        };
        let audio = async move {
            assert_eq!(
                audio_abort_receiver.await.expect("peer pause is forwarded"),
                crate::video::StopIntent::Pause
            );
            Err(ManifestRunError::Retryable(
                "audio request timed out".to_string(),
            ))
        };

        let result =
            transfer_parallel_manifest_tracks(video, audio, &mut abort, video_abort, audio_abort)
                .await;

        assert!(matches!(
            result,
            Err(ManifestRunError::Unsupported(message))
                if message.contains("video format is unsupported")
                    && message.contains("audio request timed out")
        ));
    }

    #[tokio::test]
    async fn static_track_pair_prefers_audio_unsupported_over_video_retryable() {
        let (_keep_stop_sender, mut abort) = tokio::sync::oneshot::channel();
        let (video_abort, video_abort_receiver) = tokio::sync::oneshot::channel();
        let (audio_abort, _audio_abort_receiver) = tokio::sync::oneshot::channel();
        let video = async move {
            assert_eq!(
                video_abort_receiver.await.expect("peer pause is forwarded"),
                crate::video::StopIntent::Pause
            );
            Err(ManifestRunError::Retryable(
                "video request timed out".to_string(),
            ))
        };
        let audio = async {
            Err(ManifestRunError::Unsupported(
                "audio representation is unsupported".to_string(),
            ))
        };

        let result =
            transfer_parallel_manifest_tracks(video, audio, &mut abort, video_abort, audio_abort)
                .await;

        assert!(matches!(
            result,
            Err(ManifestRunError::Unsupported(message))
                if message.contains("video request timed out")
                    && message.contains("audio representation is unsupported")
        ));
    }

    #[tokio::test]
    async fn static_track_pair_preserves_video_error_after_audio_fails() {
        let (_keep_stop_sender, mut abort) = tokio::sync::oneshot::channel();
        let (video_abort, video_abort_receiver) = tokio::sync::oneshot::channel();
        let (audio_abort, _audio_abort_receiver) = tokio::sync::oneshot::channel();
        let video = async move {
            assert_eq!(
                video_abort_receiver.await.expect("peer pause is forwarded"),
                crate::video::StopIntent::Pause
            );
            Err(ManifestRunError::Failed(
                "video staging write failed".to_string(),
            ))
        };
        let audio = async {
            Err(ManifestRunError::Unsupported(
                "audio representation is unsupported".to_string(),
            ))
        };

        let result =
            transfer_parallel_manifest_tracks(video, audio, &mut abort, video_abort, audio_abort)
                .await;

        assert!(matches!(
            result,
            Err(ManifestRunError::Failed(message))
                if message.contains("video staging write failed")
                    && message.contains("audio representation is unsupported")
        ));
    }
}

/// Transfer selected static HLS/DASH resources and assemble their local track inputs.
///
/// Returns `false` when the attempt was stopped before all selected tracks were staged.
async fn transfer_static_manifest_tracks(
    context: ManifestStaticTransfer<'_>,
) -> Result<bool, ManifestRunError> {
    let ManifestStaticTransfer {
        client,
        tracks,
        destination_parts,
        staging,
        destination,
        item_id,
        video_dir,
        audio_dir,
        concurrency,
        tx,
        downloaded,
        abort,
    } = context;
    let track_context = ManifestStaticTrackContext {
        client,
        staging,
        destination,
        item_id,
        tx,
        downloaded,
    };
    match tracks {
        Tracks::Hls {
            video,
            audio,
            subtitle,
            ..
        } => {
            if destination_parts {
                if let Some(audio) = audio {
                    let (video_abort_sender, mut video_abort) = tokio::sync::oneshot::channel();
                    let (audio_abort_sender, mut audio_abort) = tokio::sync::oneshot::channel();
                    let video_transfer = fetch_and_write_hls_track(track_context.hls_track(
                        video.as_ref(),
                        video_dir,
                        TrackKind::Video.part_filename(TrackFileFormat::Ts),
                        TrackKind::Video,
                        &mut video_abort,
                    ));
                    let audio_transfer = fetch_and_write_hls_track(track_context.hls_track(
                        audio.as_ref(),
                        audio_dir,
                        TrackKind::Audio.part_filename(TrackFileFormat::Ts),
                        TrackKind::Audio,
                        &mut audio_abort,
                    ));
                    if !transfer_parallel_manifest_tracks(
                        video_transfer,
                        audio_transfer,
                        abort,
                        video_abort_sender,
                        audio_abort_sender,
                    )
                    .await?
                    {
                        return Ok(false);
                    }
                } else if !fetch_and_write_hls_track(track_context.hls_track(
                    video.as_ref(),
                    video_dir,
                    TrackKind::Video.part_filename(TrackFileFormat::Ts),
                    TrackKind::Video,
                    abort,
                ))
                .await?
                {
                    return Ok(false);
                }
            } else {
                if let Some(audio) = audio {
                    let (video_abort_sender, mut video_abort) = tokio::sync::oneshot::channel();
                    let (audio_abort_sender, mut audio_abort) = tokio::sync::oneshot::channel();
                    let video_transfer = fetch_resources(
                        client,
                        hls_resources(video.as_ref()),
                        video_dir,
                        HLS_STATIC_RESOURCE_CONNECTIONS,
                        tx,
                        downloaded,
                        &mut video_abort,
                    );
                    let audio_transfer = fetch_resources(
                        client,
                        hls_resources(audio.as_ref()),
                        audio_dir,
                        HLS_STATIC_RESOURCE_CONNECTIONS,
                        tx,
                        downloaded,
                        &mut audio_abort,
                    );
                    if !transfer_parallel_manifest_tracks(
                        video_transfer,
                        audio_transfer,
                        abort,
                        video_abort_sender,
                        audio_abort_sender,
                    )
                    .await?
                    {
                        return Ok(false);
                    }
                } else if !fetch_resources(
                    client,
                    hls_resources(video.as_ref()),
                    video_dir,
                    HLS_STATIC_RESOURCE_CONNECTIONS,
                    tx,
                    downloaded,
                    abort,
                )
                .await?
                {
                    return Ok(false);
                }

                write_playlist(video_dir, video.as_ref()).await?;
                if !decrypt_hls_sample_aes_segments(video_dir, video.as_ref(), abort).await? {
                    return Ok(false);
                }
                if let Some(audio) = audio {
                    write_playlist(audio_dir, audio).await?;
                    if !decrypt_hls_sample_aes_segments(audio_dir, audio, abort).await? {
                        return Ok(false);
                    }
                }
            }
            if let Some(subtitle) = subtitle {
                let subtitle_dir = staging.join("subtitles");
                ensure_track_dir(&subtitle_dir).await?;
                if !fetch_resources(
                    client,
                    hls_resources(subtitle),
                    &subtitle_dir,
                    HLS_STATIC_RESOURCE_CONNECTIONS,
                    tx,
                    downloaded,
                    abort,
                )
                .await?
                {
                    return Ok(false);
                }
                write_playlist(&subtitle_dir, subtitle).await?;
                if !decrypt_hls_sample_aes_segments(&subtitle_dir, subtitle, abort).await? {
                    return Ok(false);
                }
            }
        }
        Tracks::Dash {
            video,
            audio,
            subtitle,
            ..
        } => {
            if let Some(audio) = audio {
                if concurrency == 1 {
                    if !fetch_and_write_dash_track(track_context.dash_track(
                        video.as_ref(),
                        video_dir,
                        TrackKind::Video.part_filename(TrackFileFormat::Mp4),
                        TrackKind::Video,
                        concurrency,
                        abort,
                    ))
                    .await?
                    {
                        return Ok(false);
                    }
                    if !fetch_and_write_dash_track(track_context.dash_track(
                        audio,
                        audio_dir,
                        TrackKind::Audio.part_filename(TrackFileFormat::Mp4),
                        TrackKind::Audio,
                        concurrency,
                        abort,
                    ))
                    .await?
                    {
                        return Ok(false);
                    }
                } else {
                    // Share the configured request budget evenly across both active tracks.
                    let (video_connections, audio_connections) =
                        split_manifest_track_connections(concurrency);
                    let (video_abort_sender, mut video_abort) = tokio::sync::oneshot::channel();
                    let (audio_abort_sender, mut audio_abort) = tokio::sync::oneshot::channel();
                    let video_transfer = fetch_and_write_dash_track(track_context.dash_track(
                        video.as_ref(),
                        video_dir,
                        TrackKind::Video.part_filename(TrackFileFormat::Mp4),
                        TrackKind::Video,
                        video_connections,
                        &mut video_abort,
                    ));
                    let audio_transfer = fetch_and_write_dash_track(track_context.dash_track(
                        audio,
                        audio_dir,
                        TrackKind::Audio.part_filename(TrackFileFormat::Mp4),
                        TrackKind::Audio,
                        audio_connections,
                        &mut audio_abort,
                    ));
                    if !transfer_parallel_manifest_tracks(
                        video_transfer,
                        audio_transfer,
                        abort,
                        video_abort_sender,
                        audio_abort_sender,
                    )
                    .await?
                    {
                        return Ok(false);
                    }
                }
            } else if !fetch_and_write_dash_track(track_context.dash_track(
                video.as_ref(),
                video_dir,
                TrackKind::Video.part_filename(TrackFileFormat::Mp4),
                TrackKind::Video,
                concurrency,
                abort,
            ))
            .await?
            {
                return Ok(false);
            }
            if let Some(subtitle) = subtitle {
                let subtitle_dir = staging.join("subtitles");
                ensure_track_dir(&subtitle_dir).await?;
                if !subtitle.has_zero_origin_fragmented_subtitle_timeline() {
                    return Err(ManifestRunError::Unsupported(
                        "Native fragmented DASH subtitle extraction requires a single zero-origin Period with no presentation-time offset"
                            .to_string(),
                    ));
                }
                let codecs = subtitle.representation.codecs.clone().unwrap_or_default();
                let extractor: fn(
                    &Path,
                    &Path,
                    &str,
                )
                    -> Result<bool, crate::dash_subtitle::DashSubtitleError> =
                    if crate::dash_subtitle::is_wvtt_codec(&codecs) {
                        crate::dash_subtitle::extract_to_vtt
                    } else if crate::dash_subtitle_stpp::is_stpp_codec(&codecs) {
                        crate::dash_subtitle_stpp::extract_to_vtt
                    } else {
                        return Err(ManifestRunError::Unsupported(
                            "Native DASH subtitle extraction currently supports WVTT and STPP only"
                                .to_string(),
                        ));
                    };
                if !fetch_and_write_dash_track(track_context.dash_track(
                    subtitle,
                    &subtitle_dir,
                    TrackKind::Subtitles.part_filename(TrackFileFormat::Mp4),
                    TrackKind::Subtitles,
                    concurrency,
                    abort,
                ))
                .await?
                {
                    return Ok(false);
                }
                let source = if destination_parts {
                    crate::manifest_destination::part_path_async(
                        staging.to_path_buf(),
                        destination.to_path_buf(),
                        item_id,
                        TrackKind::Subtitles,
                    )
                    .await
                    .map_err(ManifestRunError::Failed)?
                } else {
                    subtitle_dir.join(TrackKind::Subtitles.part_filename(TrackFileFormat::Mp4))
                };
                let output = staging
                    .join("subtitles")
                    .join(TrackKind::Subtitles.part_filename(TrackFileFormat::Vtt));
                match tokio::task::spawn_blocking(move || extractor(&source, &output, &codecs))
                    .await
                {
                    Ok(Ok(_)) => {}
                    Ok(Err(crate::dash_subtitle::DashSubtitleError::Unsupported(message))) => {
                        return Err(ManifestRunError::Unsupported(message.to_string()));
                    }
                    Ok(Err(crate::dash_subtitle::DashSubtitleError::UnsupportedMp4(message))) => {
                        return Err(ManifestRunError::Unsupported(message));
                    }
                    Ok(Err(crate::dash_subtitle::DashSubtitleError::Failed(message))) => {
                        return Err(ManifestRunError::Failed(message));
                    }
                    Err(error) => {
                        return Err(ManifestRunError::Failed(format!(
                            "DASH subtitle extraction task failed: {error}"
                        )));
                    }
                }
            }
        }
        Tracks::DashLiveOffline { .. } => {
            return Err(ManifestRunError::Failed(
                "A restored live DASH item bypassed the native live capture path".to_string(),
            ));
        }
    }
    Ok(true)
}

/// Result of preparing a live capture for the shared finalization phase.
enum LiveCapturePreparation {
    Ready {
        has_audio: bool,
        finish_stop_sender: Option<tokio::sync::oneshot::Sender<crate::video::StopIntent>>,
        destination_parts: bool,
    },
    Stopped(ManifestOutcome),
}

/// Inputs for capturing a live HLS presentation and preparing its staged playlists.
struct ManifestLiveHlsFinalization<'a> {
    transfer: LiveManifestTransfer<'a>,
    source_url: &'a str,
    dest_dir: &'a Path,
    destination: &'a Path,
    item_id: u64,
    staging: &'a Path,
    finish_only: bool,
    video: &'a HlsPlan,
    audio: Option<&'a HlsPlan>,
    video_dir: &'a Path,
    audio_dir: &'a Path,
    has_audio: bool,
    guarded_child_hosts: &'a mut crate::download_net::PublicResolutionSet,
    abort: &'a mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
}

async fn cleanup_manifest_destination_part_if_present(
    staging: &Path,
    destination: &Path,
    item_id: u64,
    track_directory: &Path,
    track: TrackKind,
    filename: &str,
) -> Result<(), ManifestRunError> {
    let part = crate::manifest_destination::part_path_async(
        staging.to_path_buf(),
        destination.to_path_buf(),
        item_id,
        track,
    )
    .await
    .map_err(ManifestRunError::Failed)?;
    match tokio::fs::symlink_metadata(&part).await {
        Ok(_) => {
            crate::manifest_destination::verify_owned_part_async(
                staging.to_path_buf(),
                destination.to_path_buf(),
                item_id,
                track,
            )
            .await
            .map_err(ManifestRunError::Failed)?;
            crate::manifest_writer::cleanup_destination_part(
                staging,
                track_directory,
                destination,
                item_id,
                track,
                filename,
            )
            .await
            .map_err(ManifestRunError::Failed)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(ManifestRunError::Failed(format!(
                "Could not inspect the live HLS destination part: {error}"
            )));
        }
    }
    Ok(())
}

async fn cleanup_static_destination_parts_after_fallback(
    tracks: &Tracks,
    staging: &Path,
    destination: &Path,
    item_id: u64,
    video_dir: &Path,
    audio_dir: &Path,
) -> Result<(), ManifestRunError> {
    match tracks {
        Tracks::Hls { audio, .. } => {
            cleanup_manifest_destination_part_if_present(
                staging,
                destination,
                item_id,
                video_dir,
                TrackKind::Video,
                TrackKind::Video.part_filename(TrackFileFormat::Ts),
            )
            .await?;
            if audio.is_some() {
                cleanup_manifest_destination_part_if_present(
                    staging,
                    destination,
                    item_id,
                    audio_dir,
                    TrackKind::Audio,
                    TrackKind::Audio.part_filename(TrackFileFormat::Ts),
                )
                .await?;
            }
        }
        Tracks::Dash {
            audio, subtitle, ..
        } => {
            cleanup_manifest_destination_part_if_present(
                staging,
                destination,
                item_id,
                video_dir,
                TrackKind::Video,
                TrackKind::Video.part_filename(TrackFileFormat::Mp4),
            )
            .await?;
            if audio.is_some() {
                cleanup_manifest_destination_part_if_present(
                    staging,
                    destination,
                    item_id,
                    audio_dir,
                    TrackKind::Audio,
                    TrackKind::Audio.part_filename(TrackFileFormat::Mp4),
                )
                .await?;
            }
            if subtitle.is_some() {
                let subtitle_directory = staging.join("subtitles");
                cleanup_manifest_destination_part_if_present(
                    staging,
                    destination,
                    item_id,
                    &subtitle_directory,
                    TrackKind::Subtitles,
                    TrackKind::Subtitles.part_filename(TrackFileFormat::Mp4),
                )
                .await?;
            }
        }
        Tracks::DashLiveOffline { .. } => {}
    }
    Ok(())
}

/// Capture live HLS tracks, persist the finish decision, and prepare local playlists.
///
/// The returned stop sender keeps the replacement receiver open while FFmpeg finalizes.
async fn prepare_live_hls_finalization(
    context: ManifestLiveHlsFinalization<'_>,
) -> Result<LiveCapturePreparation, ManifestRunError> {
    let ManifestLiveHlsFinalization {
        transfer,
        source_url,
        dest_dir,
        destination,
        item_id,
        staging,
        finish_only,
        video,
        audio,
        video_dir,
        audio_dir,
        mut has_audio,
        guarded_child_hosts,
        abort,
    } = context;

    let capture = run_live_hls_tracks(LiveHlsTrackRun {
        transfer,
        source_url,
        destination_dir: dest_dir,
        destination,
        item_id,
        staging,
        video_plan: video,
        audio_plan: audio,
        finish_only,
        video_dir,
        audio_dir,
        guarded_child_hosts: &mut *guarded_child_hosts,
        abort: &mut *abort,
    })
    .await?;

    let destination_parts = capture.output == LiveHlsOutput::DestinationParts;
    let has_destination_audio = audio.is_some() && has_audio;
    let mut finish_stop_sender = None;
    match capture.outcome {
        LiveHlsCaptureOutcome::Ended => {}
        LiveHlsCaptureOutcome::Paused => {
            return Ok(LiveCapturePreparation::Stopped(ManifestOutcome::Paused));
        }
        LiveHlsCaptureOutcome::Finish
        | LiveHlsCaptureOutcome::FinishVideoOnly
        | LiveHlsCaptureOutcome::Unrecoverable => {
            if !staged_regular_file_exists(&video_dir.join("local.m3u8")).await {
                return Ok(LiveCapturePreparation::Stopped(ManifestOutcome::Aborted));
            }
            if matches!(
                capture.outcome,
                LiveHlsCaptureOutcome::FinishVideoOnly | LiveHlsCaptureOutcome::Unrecoverable
            ) {
                if destination_parts && has_destination_audio {
                    cleanup_manifest_destination_part_if_present(
                        staging,
                        destination,
                        item_id,
                        audio_dir,
                        TrackKind::Audio,
                        TrackKind::Audio.part_filename(TrackFileFormat::Ts),
                    )
                    .await?;
                }
                remove_regular_file_if_present(&audio_dir.join("local.m3u8"))
                    .await
                    .map_err(ManifestRunError::Failed)?;
                has_audio = false;
            }
            let (sender, receiver) = tokio::sync::oneshot::channel();
            finish_stop_sender = Some(sender);
            *abort = receiver;
        }
        LiveHlsCaptureOutcome::Discard => {
            return Ok(LiveCapturePreparation::Stopped(ManifestOutcome::Aborted));
        }
    }

    if !staged_regular_file_exists(&video_dir.join("local.m3u8")).await {
        return Ok(LiveCapturePreparation::Stopped(ManifestOutcome::Aborted));
    }
    if has_audio && !staged_regular_file_exists(&audio_dir.join("local.m3u8")).await {
        has_audio = false;
    }
    if destination_parts && has_destination_audio && !has_audio {
        cleanup_manifest_destination_part_if_present(
            staging,
            destination,
            item_id,
            audio_dir,
            TrackKind::Audio,
            TrackKind::Audio.part_filename(TrackFileFormat::Ts),
        )
        .await?;
    }

    if destination_parts {
        loop {
            match hls_live::complete_saved_live_hls_destination_parts(
                hls_live::SavedLiveHlsDestinationParts {
                    source_url,
                    staging,
                    destination,
                    item_id,
                    video_dir,
                    audio_dir,
                    has_audio,
                },
                abort,
            )
            .await?
            {
                None => break,
                Some(crate::video::StopIntent::Pause) => {
                    return Ok(LiveCapturePreparation::Stopped(ManifestOutcome::Paused));
                }
                Some(crate::video::StopIntent::Discard) => {
                    return Ok(LiveCapturePreparation::Stopped(ManifestOutcome::Aborted));
                }
                Some(crate::video::StopIntent::Finish) => {
                    let (sender, receiver) = tokio::sync::oneshot::channel();
                    finish_stop_sender = Some(sender);
                    *abort = receiver;
                }
            }
        }
    }

    ensure_live_finish_marker(staging, transfer.downloaded.load(Ordering::Relaxed))
        .await
        .map_err(ManifestRunError::Failed)?;
    transfer.tx.send(EngineMsg::LiveFinalizing).ok();

    Ok(LiveCapturePreparation::Ready {
        has_audio,
        finish_stop_sender,
        destination_parts,
    })
}

/// Inputs for capturing live DASH tracks and preparing their local track files.
struct ManifestLiveDashFinalization<'a> {
    source_url: &'a str,
    transfer: LiveManifestTransfer<'a>,
    storage: DashLiveStorage<'a>,
    selection: DashLiveSelection<'a>,
    staging: &'a Path,
    guarded_child_hosts: &'a mut crate::download_net::PublicResolutionSet,
    abort: &'a mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
}

/// Capture live DASH resources and prepare local track files for shared finalization.
async fn prepare_live_dash_finalization(
    context: ManifestLiveDashFinalization<'_>,
) -> Result<LiveCapturePreparation, ManifestRunError> {
    let ManifestLiveDashFinalization {
        source_url,
        transfer,
        storage,
        selection,
        staging,
        guarded_child_hosts,
        abort,
    } = context;

    let capture = run_live_dash_tracks(
        &transfer,
        source_url,
        storage,
        selection,
        guarded_child_hosts,
        abort,
    )
    .await?;

    let mut finish_stop_sender = None;
    match capture.outcome {
        DashLiveCaptureOutcome::Discard => {
            return Ok(LiveCapturePreparation::Stopped(ManifestOutcome::Aborted));
        }
        DashLiveCaptureOutcome::Paused => {
            return Ok(LiveCapturePreparation::Stopped(ManifestOutcome::Paused));
        }
        DashLiveCaptureOutcome::Finish
        | DashLiveCaptureOutcome::FinishVideoOnly
        | DashLiveCaptureOutcome::Unrecoverable => {
            let (sender, receiver) = tokio::sync::oneshot::channel();
            finish_stop_sender = Some(sender);
            *abort = receiver;
        }
        DashLiveCaptureOutcome::Ended => {}
    }

    if !capture
        .video_resources
        .iter()
        .any(|name| name.starts_with("dash-live-segment-"))
    {
        return Ok(LiveCapturePreparation::Stopped(ManifestOutcome::Aborted));
    }
    let has_audio = if matches!(
        capture.outcome,
        DashLiveCaptureOutcome::FinishVideoOnly | DashLiveCaptureOutcome::Unrecoverable
    ) {
        // The shared finalizer skips audio when has_audio is false. Remove its
        // owned destination part now, while the destination plan and inode
        // ownership record are still available in XDG staging.
        crate::manifest_destination::remove_owned_part_async(
            staging.to_path_buf(),
            storage.destination_dir.to_path_buf(),
            storage.item_id,
            TrackKind::Audio,
        )
        .await
        .map_err(ManifestRunError::Failed)?;
        remove_regular_file_if_present(&storage.audio_dir.join(DASH_LIVE_STATE_NAME))
            .await
            .map_err(ManifestRunError::Failed)?;
        false
    } else {
        capture.audio_resources.as_ref().is_some_and(|resources| {
            resources
                .iter()
                .any(|name| name.starts_with("dash-live-segment-"))
        })
    };

    dash_live::ensure_live_dash_destination_parts(
        selection.representation_key,
        selection.audio_key,
        storage.video_dir,
        storage.audio_dir,
        storage.destination,
        storage.item_id,
        has_audio,
    )
    .await?;

    ensure_live_finish_marker(staging, transfer.downloaded.load(Ordering::Relaxed))
        .await
        .map_err(ManifestRunError::Failed)?;
    transfer.tx.send(EngineMsg::LiveFinalizing).ok();

    Ok(LiveCapturePreparation::Ready {
        has_audio,
        finish_stop_sender,
        destination_parts: true,
    })
}

/// Run one HLS or static-DASH attempt without automatic resource retries.
///
/// # Errors
/// Returns a classified error. Only unsupported presentations use yt-dlp fallback;
/// transient transfer errors preserve completed resources for a later user-initiated resume.
pub(crate) async fn run_manifest_download(
    job: ManifestJob,
    gate: Arc<crate::attempt_gate::AttemptGate>,
    mut abort: tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
    tx: tokio::sync::mpsc::UnboundedSender<EngineMsg>,
) -> Result<ManifestOutcome, ManifestRunError> {
    let ffmpeg = resolve_manifest_ffmpeg(&crate::video_tools::ffmpeg_search_dirs())?;
    let ManifestPreparation {
        staging,
        dest_dir,
        output_part,
        video_dir,
        audio_dir,
        client,
        mut tracks,
        mut guarded_child_hosts,
        mut has_audio,
        live_hls,
        live_dash,
        finish_requested,
        concurrency,
        downloaded,
    } = match prepare_manifest_attempt(&job, &tx, &mut abort).await? {
        ManifestPreparationOutcome::Ready(preparation) => *preparation,
        ManifestPreparationOutcome::Stopped(crate::video::StopIntent::Pause) => {
            return Ok(ManifestOutcome::Paused);
        }
        ManifestPreparationOutcome::Stopped(
            crate::video::StopIntent::Finish | crate::video::StopIntent::Discard,
        ) => return Ok(ManifestOutcome::Aborted),
    };
    let mut _finish_stop_sender = None;
    if !live_hls && !live_dash && tracks.uses_incremental_hls_vod() {
        // Preserve AES-128's established configured concurrency. Complex clear
        // VOD stays sequential, matching the ordinary static-HLS resource path.
        let connections = match &tracks {
            Tracks::Hls {
                video, audio: None, ..
            } if video.is_incremental_aes128_mpegts_vod_candidate() => job.connections,
            _ => 1,
        };
        let result = run_incremental_hls_vod(ManifestIncrementalHlsVod {
            job: &job,
            gate,
            abort,
            tx: &tx,
            staging: &staging,
            dest_dir: &dest_dir,
            video_dir: &video_dir,
            audio_dir: &audio_dir,
            tracks,
            ffmpeg,
            downloaded,
            connections,
            client: &client,
        })
        .await;
        if matches!(&result, Err(ManifestRunError::Unsupported(_))) {
            cleanup_manifest_destination_part_if_present(
                &staging,
                &job.dest,
                job.item_id,
                &video_dir,
                TrackKind::Video,
                TrackKind::Video.part_filename(TrackFileFormat::Ts),
            )
            .await?;
        }
        return result;
    }
    let static_destination_parts = !live_hls && !live_dash && tracks.uses_destination_parts();
    let mut live_destination_parts = live_dash;
    if live_hls {
        let Tracks::Hls { video, audio, .. } = &mut tracks else {
            return Err(ManifestRunError::Failed(
                "Live HLS capture requires HLS media tracks".to_string(),
            ));
        };
        match prepare_live_hls_finalization(ManifestLiveHlsFinalization {
            transfer: LiveManifestTransfer {
                client: &client,
                concurrency,
                tx: &tx,
                downloaded: &downloaded,
            },
            source_url: &job.source.url,
            dest_dir: &dest_dir,
            destination: &job.dest,
            item_id: job.item_id,
            staging: &staging,
            finish_only: finish_requested,
            video: video.as_ref(),
            audio: audio.as_deref(),
            video_dir: &video_dir,
            audio_dir: &audio_dir,
            has_audio,
            guarded_child_hosts: &mut guarded_child_hosts,
            abort: &mut abort,
        })
        .await?
        {
            LiveCapturePreparation::Ready {
                has_audio: live_has_audio,
                finish_stop_sender,
                destination_parts,
            } => {
                has_audio = live_has_audio;
                live_destination_parts = destination_parts;
                _finish_stop_sender = finish_stop_sender;
            }
            LiveCapturePreparation::Stopped(outcome) => return Ok(outcome),
        }
    } else if finish_requested {
        // Older live DASH captures stored their completed tracks in XDG staging.
        // Reopen through the journal so they are migrated into destination parts.
        if live_dash {
            dash_live::ensure_live_dash_destination_parts(
                job.source.representation_key.as_str(),
                job.source.audio_key.as_deref(),
                &video_dir,
                &audio_dir,
                &job.dest,
                job.item_id,
                has_audio,
            )
            .await?;
        }
    } else if live_dash {
        let (video_plan, audio_plan) = match &tracks {
            Tracks::Dash { video, audio, .. } => (Some(video.as_ref()), audio.as_deref()),
            Tracks::DashLiveOffline { .. } => (None, None),
            Tracks::Hls { .. } => {
                return Err(ManifestRunError::Failed(
                    "A live DASH queue item resolved to HLS tracks".to_string(),
                ));
            }
        };
        match prepare_live_dash_finalization(ManifestLiveDashFinalization {
            source_url: &job.source.url,
            transfer: LiveManifestTransfer {
                client: &client,
                concurrency,
                tx: &tx,
                downloaded: &downloaded,
            },
            storage: DashLiveStorage {
                destination_dir: &dest_dir,
                destination: &job.dest,
                item_id: job.item_id,
                video_dir: &video_dir,
                audio_dir: &audio_dir,
            },
            selection: DashLiveSelection {
                representation_key: job.source.representation_key.as_str(),
                audio_key: job.source.audio_key.as_deref(),
                video_plan,
                audio_plan,
            },
            staging: &staging,
            guarded_child_hosts: &mut guarded_child_hosts,
            abort: &mut abort,
        })
        .await?
        {
            LiveCapturePreparation::Ready {
                has_audio: live_has_audio,
                finish_stop_sender,
                destination_parts,
            } => {
                has_audio = live_has_audio;
                live_destination_parts = destination_parts;
                _finish_stop_sender = finish_stop_sender;
            }
            LiveCapturePreparation::Stopped(outcome) => return Ok(outcome),
        }
    } else {
        match transfer_static_manifest_tracks(ManifestStaticTransfer {
            client: &client,
            tracks: &tracks,
            destination_parts: static_destination_parts,
            staging: &staging,
            destination: &job.dest,
            item_id: job.item_id,
            video_dir: &video_dir,
            audio_dir: &audio_dir,
            concurrency,
            tx: &tx,
            downloaded: &downloaded,
            abort: &mut abort,
        })
        .await
        {
            Ok(true) => {}
            Ok(false) => return Ok(ManifestOutcome::Aborted),
            Err(error @ ManifestRunError::Unsupported(_)) if static_destination_parts => {
                cleanup_static_destination_parts_after_fallback(
                    &tracks,
                    &staging,
                    &job.dest,
                    job.item_id,
                    &video_dir,
                    &audio_dir,
                )
                .await?;
                return Err(error);
            }
            Err(error) => return Err(error),
        }
    }
    finalize_manifest_download(ManifestFinalization {
        job: &job,
        destination_parts: live_destination_parts || static_destination_parts,
        mode: ManifestFinalizationMode::MuxStagedTracks,
        gate,
        abort,
        tx: &tx,
        staging: &staging,
        dest_dir: &dest_dir,
        video_dir: &video_dir,
        audio_dir: &audio_dir,
        output_part,
        tracks,
        has_audio,
        ffmpeg,
        downloaded: &downloaded,
    })
    .await
}

struct ManifestIncrementalHlsVod<'a> {
    job: &'a ManifestJob,
    gate: Arc<crate::attempt_gate::AttemptGate>,
    abort: tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
    tx: &'a tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    staging: &'a Path,
    dest_dir: &'a Path,
    video_dir: &'a Path,
    audio_dir: &'a Path,
    tracks: Tracks,
    ffmpeg: PathBuf,
    downloaded: Arc<AtomicU64>,
    connections: usize,
    client: &'a reqwest::Client,
}

fn send_manifest_stop_intent(
    sender: &mut Option<tokio::sync::oneshot::Sender<crate::video::StopIntent>>,
    intent: crate::video::StopIntent,
) {
    if let Some(sender) = sender.take() {
        let _ = sender.send(intent);
    }
}

struct ManifestIncrementalHlsTrackFetch {
    index: usize,
    client: reqwest::Client,
    plan: Arc<HlsPlan>,
    directory: PathBuf,
    destination_dir: PathBuf,
    output_part: PathBuf,
    tx: tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    downloaded: Arc<AtomicU64>,
    connections: usize,
}

struct ManifestIncrementalHlsFetchWorker {
    index: usize,
    first_segment_ready: Option<tokio::sync::oneshot::Receiver<()>>,
    complete_ready: Option<tokio::sync::oneshot::Receiver<()>>,
    abort_sender: Option<tokio::sync::oneshot::Sender<crate::video::StopIntent>>,
}

fn spawn_incremental_hls_track_fetch(
    fetchers: &mut tokio::task::JoinSet<(usize, Result<bool, ManifestRunError>)>,
    context: ManifestIncrementalHlsTrackFetch,
) -> ManifestIncrementalHlsFetchWorker {
    let ManifestIncrementalHlsTrackFetch {
        index,
        client,
        plan,
        directory,
        destination_dir,
        output_part,
        tx,
        downloaded,
        connections,
    } = context;
    let (abort_sender, mut abort) = tokio::sync::oneshot::channel();
    let (first_segment_sender, first_segment_ready) = tokio::sync::oneshot::channel();
    let (complete_sender, complete_ready) = tokio::sync::oneshot::channel();

    fetchers.spawn(async move {
        let result = fetch_hls_resources_incrementally(IncrementalHlsFetch {
            client: &client,
            plan: &plan,
            directory: &directory,
            destination_dir: &destination_dir,
            output_part: &output_part,
            tx: &tx,
            downloaded: downloaded.as_ref(),
            abort: &mut abort,
            connections,
            first_segment_ready: first_segment_sender,
            complete_ready: complete_sender,
        })
        .await;
        (index, result)
    });

    ManifestIncrementalHlsFetchWorker {
        index,
        first_segment_ready: Some(first_segment_ready),
        complete_ready: Some(complete_ready),
        abort_sender: Some(abort_sender),
    }
}

struct ManifestCompleteHlsRemux<'a> {
    ffmpeg: &'a Path,
    video_playlist: &'a Path,
    audio_playlist: Option<&'a Path>,
    staging: &'a Path,
    destination: &'a Path,
    item_id: u64,
    part_path: &'a Path,
    container: ManifestContainer,
    timestamp_policy: InputTimestampPolicy,
    abort: &'a mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
}

fn completed_incremental_hls_mux(
    result: Result<Result<ManifestMuxOutcome, ManifestRunError>, tokio::task::JoinError>,
) -> Result<ManifestMuxOutcome, ManifestRunError> {
    match result {
        Ok(Ok(outcome)) => Ok(outcome),
        Ok(Err(error)) => Err(error),
        Err(error) => Err(ManifestRunError::Failed(format!(
            "FFmpeg HLS mux task failed: {error}"
        ))),
    }
}

async fn remux_complete_hls_vod_part(
    context: ManifestCompleteHlsRemux<'_>,
) -> Result<ManifestMuxOutcome, ManifestRunError> {
    use tokio::io::AsyncSeekExt as _;

    let ManifestCompleteHlsRemux {
        ffmpeg,
        video_playlist,
        audio_playlist,
        staging,
        destination,
        item_id,
        part_path,
        container,
        timestamp_policy,
        abort,
    } = context;
    let mut output_file = crate::manifest_destination::open_owned_part_async(
        staging.to_path_buf(),
        destination.to_path_buf(),
        item_id,
        TrackKind::Video,
    )
    .await
    .map_err(ManifestRunError::Failed)?;
    output_file.set_len(0).await.map_err(|error| {
        manifest_storage_error("Could not reset the incomplete HLS output part", error)
    })?;
    output_file
        .seek(std::io::SeekFrom::Start(0))
        .await
        .map_err(|error| manifest_storage_error("Could not rewind the HLS output part", error))?;
    output_file.sync_all().await.map_err(|error| {
        manifest_storage_error("Could not persist the reset HLS output part", error)
    })?;

    run_ffmpeg_with_offsets_timeout(
        ffmpeg,
        FfmpegInput {
            path: video_playlist,
            timestamp_offset: None,
        },
        audio_playlist.map(|path| FfmpegInput {
            path,
            timestamp_offset: None,
        }),
        ManifestMuxOptions {
            timestamp_policy,
            timeout: Some(INCREMENTAL_HLS_MUX_TIMEOUT),
        },
        SubtitleOutput {
            input: None,
            embed: false,
            sidecar: None,
            format: SubtitleFormat::Text,
        },
        FfmpegOutput::OwnedPart {
            file: output_file.into_std().await,
            container,
            label: part_path.to_path_buf(),
        },
        abort,
    )
    .await
}

async fn run_incremental_hls_vod(
    context: ManifestIncrementalHlsVod<'_>,
) -> Result<ManifestOutcome, ManifestRunError> {
    use tokio::io::AsyncSeekExt as _;

    let ManifestIncrementalHlsVod {
        job,
        gate,
        mut abort,
        tx,
        staging,
        dest_dir,
        video_dir,
        audio_dir,
        tracks,
        ffmpeg,
        downloaded,
        connections,
        client,
    } = context;
    tx.send(EngineMsg::Phase("Downloading HLS VOD…".to_string()))
        .ok();
    let Tracks::Hls {
        video,
        audio,
        subtitle: None,
        ..
    } = tracks
    else {
        return Err(ManifestRunError::Failed(
            "Incremental HLS VOD requires video and optional audio media playlists".to_string(),
        ));
    };

    let part_path = crate::manifest_destination::part_path_async(
        staging.to_path_buf(),
        job.dest.clone(),
        job.item_id,
        TrackKind::Video,
    )
    .await
    .map_err(ManifestRunError::Failed)?;
    let mut output_file = crate::manifest_destination::open_owned_part_async(
        staging.to_path_buf(),
        job.dest.clone(),
        job.item_id,
        TrackKind::Video,
    )
    .await
    .map_err(ManifestRunError::Failed)?;
    output_file.set_len(0).await.map_err(|error| {
        manifest_storage_error("Could not reset the owned HLS output part", error)
    })?;
    output_file
        .seek(std::io::SeekFrom::Start(0))
        .await
        .map_err(|error| manifest_storage_error("Could not rewind the HLS output part", error))?;
    output_file.sync_all().await.map_err(|error| {
        manifest_storage_error("Could not persist the empty HLS output part", error)
    })?;

    let video = Arc::from(video);
    let audio = audio.map(Arc::from);
    let video_playlist = video_dir.join("local.m3u8");
    let audio_playlist = audio.as_ref().map(|_| audio_dir.join("local.m3u8"));
    let timestamp_policy = if audio.is_some() {
        // Separate HLS renditions share a source presentation timestamp clock.
        InputTimestampPolicy::Preserve
    } else {
        InputTimestampPolicy::Normalize
    };

    let mut fetchers = tokio::task::JoinSet::<(usize, Result<bool, ManifestRunError>)>::new();
    let mut workers = Vec::with_capacity(if audio.is_some() { 2 } else { 1 });
    workers.push(spawn_incremental_hls_track_fetch(
        &mut fetchers,
        ManifestIncrementalHlsTrackFetch {
            index: 0,
            client: client.clone(),
            plan: Arc::clone(&video),
            directory: video_dir.to_path_buf(),
            destination_dir: dest_dir.to_path_buf(),
            output_part: part_path.clone(),
            tx: tx.clone(),
            downloaded: Arc::clone(&downloaded),
            connections,
        },
    ));
    if let Some(audio_plan) = audio.as_ref() {
        workers.push(spawn_incremental_hls_track_fetch(
            &mut fetchers,
            ManifestIncrementalHlsTrackFetch {
                index: 1,
                client: client.clone(),
                plan: Arc::clone(audio_plan),
                directory: audio_dir.to_path_buf(),
                destination_dir: dest_dir.to_path_buf(),
                output_part: part_path.clone(),
                tx: tx.clone(),
                downloaded: Arc::clone(&downloaded),
                // Keep each rendition sequential. The video and audio tracks
                // themselves are fetched concurrently.
                connections: 1,
            },
        ));
    }

    let track_count = workers.len();
    let mut fetch_results = std::iter::repeat_with(|| None)
        .take(track_count)
        .collect::<Vec<Option<Result<bool, ManifestRunError>>>>();
    let mut first_segment_events = futures_util::stream::FuturesUnordered::new();
    for worker in &mut workers {
        let index = worker.index;
        let receiver = worker
            .first_segment_ready
            .take()
            .expect("each incremental HLS track has one first-segment receiver");
        first_segment_events.push(async move { (index, receiver.await) });
    }
    let mut first_segment_ready = vec![false; track_count];
    let mut fetch_abort_tx = Some(Vec::new());
    for worker in &mut workers {
        if let Some(sender) = worker.abort_sender.take() {
            fetch_abort_tx
                .as_mut()
                .expect("fetch abort senders are initialized")
                .push(sender);
        }
    }
    let (ffmpeg_abort_sender, mut ffmpeg_abort) = tokio::sync::oneshot::channel();
    let mut ffmpeg_abort_tx = Some(ffmpeg_abort_sender);
    let mut abort_done = false;
    let mut stop_intent = None;

    // Do not start FFmpeg until every selected rendition has published a
    // complete first segment and its growing local playlist snapshot.
    while first_segment_ready.iter().any(|ready| !ready) {
        tokio::select! {
            biased;
            stop = &mut abort, if !abort_done => {
                abort_done = true;
                let intent = crate::video::stop_intent_or_pause(stop);
                stop_intent = Some(intent);
                if intent != crate::video::StopIntent::Finish {
                    if let Some(senders) = fetch_abort_tx.as_mut() {
                        for sender in senders.drain(..) {
                            let _ = sender.send(intent);
                        }
                    }
                    send_manifest_stop_intent(&mut ffmpeg_abort_tx, intent);
                }
            }
            ready = futures_util::StreamExt::next(&mut first_segment_events),
                if !first_segment_events.is_empty() =>
            {
                if let Some((index, result)) = ready
                    && result.is_ok()
                {
                    first_segment_ready[index] = true;
                }
            }
            joined = fetchers.join_next(), if !fetchers.is_empty() => {
                match joined {
                    Some(Ok((index, Ok(true)))) => {
                        fetch_results[index] = Some(Ok(true));
                        first_segment_ready[index] = true;
                    }
                    Some(Ok((_, Ok(false)))) => {
                        return Ok(
                            match stop_intent.unwrap_or(crate::video::StopIntent::Pause) {
                                crate::video::StopIntent::Pause | crate::video::StopIntent::Finish => {
                                    ManifestOutcome::Paused
                                }
                                crate::video::StopIntent::Discard => ManifestOutcome::Aborted,
                            },
                        );
                    }
                    Some(Ok((_, Err(error)))) => return Err(error),
                    Some(Err(error)) => {
                        return Err(ManifestRunError::Failed(format!(
                            "Incremental HLS track fetch task failed: {error}"
                        )));
                    }
                    None => {}
                }
            }
        }
    }
    if !abort_done && let Some(intent) = pending_manifest_stop_intent(&mut abort) {
        abort_done = true;
        stop_intent = Some(intent);
    }
    if let Some(intent @ (crate::video::StopIntent::Pause | crate::video::StopIntent::Discard)) =
        stop_intent
    {
        return Ok(match intent {
            crate::video::StopIntent::Pause => ManifestOutcome::Paused,
            crate::video::StopIntent::Discard => ManifestOutcome::Aborted,
            crate::video::StopIntent::Finish => unreachable!(),
        });
    }

    let output_container = manifest_output_container(&job.dest)?;
    let ffmpeg_path = ffmpeg.clone();
    let ffmpeg_video_playlist = video_playlist.clone();
    let ffmpeg_audio_playlist = audio_playlist.clone();
    let output_label = part_path.clone();
    let output_file = output_file.into_std().await;
    let mut ffmpeg_task = tokio::spawn(async move {
        run_ffmpeg_with_offsets_timeout(
            &ffmpeg_path,
            FfmpegInput {
                path: &ffmpeg_video_playlist,
                timestamp_offset: None,
            },
            ffmpeg_audio_playlist.as_deref().map(|path| FfmpegInput {
                path,
                timestamp_offset: None,
            }),
            ManifestMuxOptions {
                timestamp_policy,
                timeout: Some(INCREMENTAL_HLS_MUX_TIMEOUT),
            },
            SubtitleOutput {
                input: None,
                embed: false,
                sidecar: None,
                format: SubtitleFormat::Text,
            },
            FfmpegOutput::OwnedPart {
                file: output_file,
                container: output_container,
                label: output_label,
            },
            &mut ffmpeg_abort,
        )
        .await
    });

    let mut ffmpeg_result = None;
    let mut ffmpeg_exited_before_complete_playlist = false;
    while fetch_results.iter().any(Option::is_none) || ffmpeg_result.is_none() {
        tokio::select! {
            biased;
            stop = &mut abort, if !abort_done => {
                abort_done = true;
                let intent = crate::video::stop_intent_or_pause(stop);
                stop_intent = Some(intent);
                if intent != crate::video::StopIntent::Finish {
                    if let Some(senders) = fetch_abort_tx.as_mut() {
                        for sender in senders.drain(..) {
                            let _ = sender.send(intent);
                        }
                    }
                    send_manifest_stop_intent(&mut ffmpeg_abort_tx, intent);
                }
            }
            joined = fetchers.join_next(), if !fetchers.is_empty() => {
                match joined {
                    Some(Ok((index, result))) => {
                        let should_stop = !matches!(&result, Ok(true));
                        fetch_results[index] = Some(result);
                        if should_stop {
                            if let Some(senders) = fetch_abort_tx.as_mut() {
                                for sender in senders.drain(..) {
                                    let _ = sender.send(crate::video::StopIntent::Pause);
                                }
                            }
                            send_manifest_stop_intent(&mut ffmpeg_abort_tx, crate::video::StopIntent::Pause);
                        }
                    }
                    Some(Err(error)) => {
                        let message = format!("Incremental HLS track fetch task failed: {error}");
                        for result in &mut fetch_results {
                            if result.is_none() {
                                *result = Some(Err(ManifestRunError::Failed(message.clone())));
                            }
                        }
                        fetchers.abort_all();
                        if let Some(senders) = fetch_abort_tx.as_mut() {
                            for sender in senders.drain(..) {
                                let _ = sender.send(crate::video::StopIntent::Pause);
                            }
                        }
                        send_manifest_stop_intent(&mut ffmpeg_abort_tx, crate::video::StopIntent::Pause);
                    }
                    None => {}
                }
            }
            result = &mut ffmpeg_task, if ffmpeg_result.is_none() => {
                ffmpeg_result = Some(result);
                match &ffmpeg_result {
                    Some(Ok(Ok(ManifestMuxOutcome::Finished)))
                        if fetch_results.iter().any(Option::is_none) =>
                    {
                        // FFmpeg can finish against a growing snapshot before
                        // either rendition has committed its complete playlist.
                        // Remux both complete playlists before publication.
                        ffmpeg_exited_before_complete_playlist = true;
                    }
                    Some(Ok(Err(_))) | Some(Err(_)) => {
                        if let Some(senders) = fetch_abort_tx.as_mut() {
                            for sender in senders.drain(..) {
                                let _ = sender.send(crate::video::StopIntent::Pause);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    if !abort_done && let Some(intent) = pending_manifest_stop_intent(&mut abort) {
        abort_done = true;
        stop_intent = Some(intent);
    }
    if let Some(intent @ (crate::video::StopIntent::Pause | crate::video::StopIntent::Discard)) =
        stop_intent
    {
        return Ok(match intent {
            crate::video::StopIntent::Pause => ManifestOutcome::Paused,
            crate::video::StopIntent::Discard => ManifestOutcome::Aborted,
            crate::video::StopIntent::Finish => unreachable!(),
        });
    }
    for result in &mut fetch_results {
        if matches!(result, Some(Err(_)))
            && let Some(Err(error)) = result.take()
        {
            return Err(error);
        }
    }
    let mux_outcome = completed_incremental_hls_mux(ffmpeg_result.ok_or_else(|| {
        ManifestRunError::Failed("The incremental HLS FFmpeg task did not finish".to_string())
    })?)?;
    if fetch_results
        .iter()
        .any(|result| matches!(result, Some(Ok(false)) | None))
    {
        return Ok(
            match stop_intent.unwrap_or(crate::video::StopIntent::Pause) {
                crate::video::StopIntent::Pause | crate::video::StopIntent::Finish => {
                    ManifestOutcome::Paused
                }
                crate::video::StopIntent::Discard => ManifestOutcome::Aborted,
            },
        );
    }
    if mux_outcome != ManifestMuxOutcome::Finished {
        return Ok(match mux_outcome {
            ManifestMuxOutcome::Paused => ManifestOutcome::Paused,
            ManifestMuxOutcome::Aborted => ManifestOutcome::Aborted,
            ManifestMuxOutcome::Finished => unreachable!(),
        });
    }
    for worker in &mut workers {
        let Some(receiver) = worker.complete_ready.take() else {
            return Err(ManifestRunError::Failed(
                "An HLS rendition lost its completion receiver".to_string(),
            ));
        };
        if receiver.await.is_err() {
            return Err(ManifestRunError::Failed(
                "An HLS rendition completed without publishing its complete VOD playlist"
                    .to_string(),
            ));
        }
    }

    match mux_outcome {
        ManifestMuxOutcome::Finished if ffmpeg_exited_before_complete_playlist => {
            tracing::warn!(
                item_id = job.item_id,
                audio = audio.is_some(),
                "FFmpeg exited on a growing HLS VOD snapshot; remuxing the completed playlists"
            );
            let remux_outcome = if abort_done {
                // Finish was already consumed by the fetch/mux supervisor.
                // Re-arm it for the completion-only remux.
                let (finish_sender, mut finish_abort) = tokio::sync::oneshot::channel();
                let _ = finish_sender.send(crate::video::StopIntent::Finish);
                remux_complete_hls_vod_part(ManifestCompleteHlsRemux {
                    ffmpeg: &ffmpeg,
                    video_playlist: &video_playlist,
                    audio_playlist: audio_playlist.as_deref(),
                    staging,
                    destination: &job.dest,
                    item_id: job.item_id,
                    part_path: &part_path,
                    container: output_container,
                    timestamp_policy: if audio.is_some() {
                        InputTimestampPolicy::Preserve
                    } else {
                        InputTimestampPolicy::Normalize
                    },
                    abort: &mut finish_abort,
                })
                .await?
            } else {
                remux_complete_hls_vod_part(ManifestCompleteHlsRemux {
                    ffmpeg: &ffmpeg,
                    video_playlist: &video_playlist,
                    audio_playlist: audio_playlist.as_deref(),
                    staging,
                    destination: &job.dest,
                    item_id: job.item_id,
                    part_path: &part_path,
                    container: output_container,
                    timestamp_policy: if audio.is_some() {
                        InputTimestampPolicy::Preserve
                    } else {
                        InputTimestampPolicy::Normalize
                    },
                    abort: &mut abort,
                })
                .await?
            };
            match remux_outcome {
                ManifestMuxOutcome::Finished => {}
                ManifestMuxOutcome::Paused => return Ok(ManifestOutcome::Paused),
                ManifestMuxOutcome::Aborted => return Ok(ManifestOutcome::Aborted),
            }
        }
        ManifestMuxOutcome::Finished => {}
        ManifestMuxOutcome::Paused => return Ok(ManifestOutcome::Paused),
        ManifestMuxOutcome::Aborted => return Ok(ManifestOutcome::Aborted),
    }

    let video = Arc::try_unwrap(video).map_err(|_| {
        ManifestRunError::Failed("incremental HLS video plan still has active readers".to_string())
    })?;
    let audio = audio
        .map(|plan| {
            Arc::try_unwrap(plan).map_err(|_| {
                ManifestRunError::Failed(
                    "incremental HLS audio plan still has active readers".to_string(),
                )
            })
        })
        .transpose()?;
    let has_audio = audio.is_some();
    let (finalizer_abort_sender, finalizer_abort) = if abort_done {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        (Some(sender), receiver)
    } else {
        (None, abort)
    };
    let result = finalize_manifest_download(ManifestFinalization {
        job,
        // The output part is already a single muxed file. No per-track
        // destination parts were created by the incremental worker.
        destination_parts: false,
        mode: ManifestFinalizationMode::PublishPreMuxedDestinationPart,
        gate,
        abort: finalizer_abort,
        tx,
        staging,
        dest_dir,
        video_dir,
        audio_dir,
        output_part: part_path,
        tracks: Tracks::Hls {
            video: Box::new(video),
            audio: audio.map(Box::new),
            subtitle: None,
            subtitle_language: None,
            subtitle_format: SubtitleFormat::Text,
        },
        has_audio,
        ffmpeg,
        downloaded: downloaded.as_ref(),
    })
    .await;
    drop(finalizer_abort_sender);
    result
}

/// Mux the staged tracks, validate the output, and publish it without clobbering.
///
/// The staging data is retained on every failure before publication so a later
/// user-initiated resume can reuse completed resources.
#[derive(Clone, Copy)]
enum ManifestFinalizationMode {
    MuxStagedTracks,
    PublishPreMuxedDestinationPart,
}

struct ManifestFinalization<'a> {
    job: &'a ManifestJob,
    destination_parts: bool,
    mode: ManifestFinalizationMode,
    gate: Arc<crate::attempt_gate::AttemptGate>,
    abort: tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
    tx: &'a tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    staging: &'a Path,
    dest_dir: &'a Path,
    video_dir: &'a Path,
    audio_dir: &'a Path,
    output_part: PathBuf,
    tracks: Tracks,
    has_audio: bool,
    ffmpeg: PathBuf,
    downloaded: &'a AtomicU64,
}

/// Mux the staged tracks, validate the output, and publish it without clobbering.
///
/// The staging data is retained on every failure before publication so the queue
/// can retry without downloading already completed resources.
fn pending_manifest_stop_intent(
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
) -> Option<crate::video::StopIntent> {
    match abort.try_recv() {
        Err(tokio::sync::oneshot::error::TryRecvError::Empty) => None,
        result => Some(crate::video::stop_intent_or_pause(result)),
    }
}

#[cfg(test)]
mod incremental_hls_mux_result_tests {
    use super::*;

    #[test]
    fn ffmpeg_failure_is_preserved_for_the_download_attempt() {
        let result = completed_incremental_hls_mux(Ok(Err(ManifestRunError::Failed(
            "synthetic FFmpeg mux failure".to_string(),
        ))));

        assert!(matches!(
            result,
            Err(ManifestRunError::Failed(message))
                if message == "synthetic FFmpeg mux failure"
        ));
    }

    #[test]
    fn completed_ffmpeg_outcome_is_preserved() {
        assert!(matches!(
            completed_incremental_hls_mux(Ok(Ok(ManifestMuxOutcome::Finished))),
            Ok(ManifestMuxOutcome::Finished)
        ));
    }
}

#[cfg(test)]
mod manifest_finalization_stop_tests {
    use super::*;

    #[test]
    fn empty_stop_channel_does_not_interrupt_finalization() {
        let (_sender, mut receiver) = tokio::sync::oneshot::channel();
        assert_eq!(pending_manifest_stop_intent(&mut receiver), None);
    }

    #[test]
    fn pending_stop_intents_are_preserved() {
        for intent in [
            crate::video::StopIntent::Pause,
            crate::video::StopIntent::Finish,
            crate::video::StopIntent::Discard,
        ] {
            let (sender, mut receiver) = tokio::sync::oneshot::channel();
            sender.send(intent).expect("queue stop intent");
            assert_eq!(pending_manifest_stop_intent(&mut receiver), Some(intent));
        }
    }

    #[test]
    fn closed_stop_channel_defaults_to_pause() {
        let (sender, mut receiver) = tokio::sync::oneshot::channel();
        drop(sender);
        assert_eq!(
            pending_manifest_stop_intent(&mut receiver),
            Some(crate::video::StopIntent::Pause)
        );
    }
}

async fn finalize_manifest_download(
    context: ManifestFinalization<'_>,
) -> Result<ManifestOutcome, ManifestRunError> {
    let ManifestFinalization {
        job,
        destination_parts,
        mode,
        gate,
        mut abort,
        tx,
        staging,
        dest_dir,
        video_dir,
        audio_dir,
        output_part,
        tracks,
        has_audio,
        ffmpeg,
        downloaded,
    } = context;
    match pending_manifest_stop_intent(&mut abort) {
        Some(crate::video::StopIntent::Pause) => return Ok(ManifestOutcome::Paused),
        Some(crate::video::StopIntent::Discard) => return Ok(ManifestOutcome::Aborted),
        Some(crate::video::StopIntent::Finish) | None => {}
    }
    tx.send(EngineMsg::Phase(
        if matches!(
            mode,
            ManifestFinalizationMode::PublishPreMuxedDestinationPart
        ) {
            "Finalizing downloaded stream…".to_string()
        } else {
            "Merging audio and video…".to_string()
        },
    ))
    .ok();

    let has_separate_hls_audio = has_audio && matches!(&tracks, Tracks::Hls { audio: Some(_), .. });
    let has_local_hls_renditions = has_separate_hls_audio
        && staged_regular_file_exists(&video_dir.join("local.m3u8")).await
        && staged_regular_file_exists(&audio_dir.join("local.m3u8")).await;
    if has_separate_hls_audio && !destination_parts && !has_local_hls_renditions {
        return Err(ManifestRunError::Failed(
            "The staged HLS audio or video rendition playlist is missing".to_string(),
        ));
    }
    // Separate HLS rendition inputs must retain their common source PTS clock.
    // The same applies to raw destination parts assembled from those renditions.
    let timestamp_policy = if has_separate_hls_audio {
        InputTimestampPolicy::Preserve
    } else {
        InputTimestampPolicy::Normalize
    };
    let video_input = if destination_parts {
        crate::manifest_destination::part_path_async(
            staging.to_path_buf(),
            job.dest.clone(),
            job.item_id,
            TrackKind::Video,
        )
        .await
        .map_err(ManifestRunError::Failed)?
    } else {
        match &tracks {
            Tracks::Hls { .. } => video_dir.join("local.m3u8"),
            Tracks::Dash { .. } | Tracks::DashLiveOffline { .. } => {
                video_dir.join(TrackKind::Video.part_filename(TrackFileFormat::Mp4))
            }
        }
    };
    let audio_input = if has_audio {
        Some(if destination_parts {
            crate::manifest_destination::part_path_async(
                staging.to_path_buf(),
                job.dest.clone(),
                job.item_id,
                TrackKind::Audio,
            )
            .await
            .map_err(ManifestRunError::Failed)?
        } else {
            match job.source.kind {
                ManifestKind::Hls => audio_dir.join("local.m3u8"),
                ManifestKind::Dash => {
                    audio_dir.join(TrackKind::Audio.part_filename(TrackFileFormat::Mp4))
                }
            }
        })
    } else {
        None
    };
    let subtitle_input = match &tracks {
        Tracks::Hls {
            subtitle: Some(_), ..
        } => Some(staging.join("subtitles/local.m3u8")),
        Tracks::Dash {
            subtitle: Some(_), ..
        } => {
            let path = staging
                .join("subtitles")
                .join(TrackKind::Subtitles.part_filename(TrackFileFormat::Vtt));
            match inspect_staged_regular_file(&path).await {
                Ok(Some(_)) => Some(path),
                Ok(None) => None,
                Err(StagedFileError::NotRegularFile) => {
                    return Err(ManifestRunError::Failed(
                        "The normalized DASH subtitle staging path is not a regular file"
                            .to_string(),
                    ));
                }
                Err(StagedFileError::Io(error)) => {
                    return Err(ManifestRunError::Failed(format!(
                        "Could not inspect normalized DASH subtitle staging: {error}"
                    )));
                }
            }
        }
        _ => None,
    };
    if matches!(
        mode,
        ManifestFinalizationMode::PublishPreMuxedDestinationPart
    ) && !destination_parts
    {
        // Incremental muxing writes the final media stream directly into the
        // owned video part; there are no separate audio/video track parts.
        crate::manifest_destination::verify_owned_part_async(
            staging.to_path_buf(),
            job.dest.clone(),
            job.item_id,
            TrackKind::Video,
        )
        .await
        .map_err(ManifestRunError::Failed)?;
    }
    if destination_parts {
        for (track, should_verify) in [
            (TrackKind::Video, true),
            (TrackKind::Audio, has_audio),
            (
                TrackKind::Subtitles,
                matches!(
                    &tracks,
                    Tracks::Dash {
                        subtitle: Some(_),
                        ..
                    }
                ),
            ),
        ] {
            if should_verify {
                crate::manifest_destination::verify_owned_part_async(
                    staging.to_path_buf(),
                    job.dest.clone(),
                    job.item_id,
                    track,
                )
                .await
                .map_err(ManifestRunError::Failed)?;
            }
        }
    }
    let subtitle_language = match &tracks {
        Tracks::Hls {
            subtitle_language, ..
        }
        | Tracks::Dash {
            subtitle_language, ..
        } => subtitle_language.as_deref(),
        Tracks::DashLiveOffline { .. } => None,
    };
    let subtitle_language = subtitle_language
        .filter(|language| {
            !language.is_empty()
                && language.len() <= 35
                && language
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
        .unwrap_or("und");
    let subtitle_format = match &tracks {
        Tracks::Hls {
            subtitle: Some(_),
            subtitle_format,
            ..
        } => *subtitle_format,
        _ => SubtitleFormat::Text,
    };
    let output_container = manifest_output_container(&job.dest)?;
    // Text WebVTT playlists can be normalized to the destination container.
    // fMP4 subtitle playlists retain their source codec in MP4 instead of
    // being decoded to SRT or converted to mov_text.
    let embed_subtitles =
        embed_manifest_subtitles(job.embed_subs, output_container, subtitle_format);
    let subtitle_sidecar = if subtitle_input.is_some() && !embed_subtitles {
        Some(if subtitle_format == SubtitleFormat::Fmp4 {
            crate::video_staging::sidecar_path_for_extension(
                &output_part,
                subtitle_language,
                crate::video_staging::FMP4_SUBTITLE_SIDECAR_EXTENSION,
            )
        } else {
            crate::video_staging::sidecar_path_for(&output_part, subtitle_language)
        })
    } else {
        None
    };
    if let Some(sidecar) = subtitle_sidecar.as_deref() {
        remove_regular_file_if_present(sidecar)
            .await
            .map_err(ManifestRunError::Failed)?;
    }
    if matches!(mode, ManifestFinalizationMode::MuxStagedTracks) {
        let staged_bytes = downloaded.load(Ordering::Relaxed);
        // FFmpeg writes beside the staged segments, so check both the XDG state
        // filesystem (staged inputs plus mux output) and destination filesystem
        // (the no-clobber cross-device publication fallback).
        ensure_manifest_mux_spaces(staging, dest_dir, staged_bytes)?;
    }
    // shortcut: the incremental mode has no space preflight here — its output grows
    // in the owned part while segments are still arriving, and the final size is not
    // known before fetch. ENOSPC surfaces as a storage error (mux_failure_error /
    // manifest_storage_error) and the owned part is reset on retry; preflight only
    // when encrypted segment sizes become available in the plan.
    let (video_timestamp_offset, audio_timestamp_offset) = match &tracks {
        Tracks::Dash { video, audio, .. } => (
            video.input_timestamp_offset.ffmpeg_argument(),
            audio
                .as_ref()
                .and_then(|plan| plan.input_timestamp_offset.ffmpeg_argument()),
        ),
        _ => (None, None),
    };
    if matches!(mode, ManifestFinalizationMode::MuxStagedTracks) {
        match run_ffmpeg_with_offsets(
            &ffmpeg,
            FfmpegInput {
                path: &video_input,
                timestamp_offset: video_timestamp_offset.as_deref(),
            },
            audio_input.as_deref().map(|path| FfmpegInput {
                path,
                timestamp_offset: audio_timestamp_offset.as_deref(),
            }),
            timestamp_policy,
            SubtitleOutput {
                input: subtitle_input.as_deref(),
                embed: embed_subtitles,
                sidecar: subtitle_sidecar.as_deref(),
                format: subtitle_format,
            },
            &output_part,
            &mut abort,
        )
        .await?
        {
            ManifestMuxOutcome::Finished => {}
            ManifestMuxOutcome::Paused => return Ok(ManifestOutcome::Paused),
            ManifestMuxOutcome::Aborted => return Ok(ManifestOutcome::Aborted),
        }
    }

    // Validate and size the muxed file before the delivery commit. After the
    // no-clobber rename succeeds, no later I/O failure may turn a delivered
    // download into a retry/fallback.
    let output_file = tokio::fs::File::open(&output_part)
        .await
        .map_err(|error| manifest_storage_error("Could not open the finalized stream", error))?;
    output_file
        .sync_all()
        .await
        .map_err(|error| manifest_storage_error("Could not flush the finalized stream", error))?;
    drop(output_file);
    let output_metadata = tokio::fs::symlink_metadata(&output_part)
        .await
        .map_err(|error| manifest_storage_error("Could not inspect the finalized stream", error))?;
    if output_metadata.file_type().is_symlink()
        || !output_metadata.is_file()
        || output_metadata.len() == 0
    {
        return Err(ManifestRunError::Failed(
            "FFmpeg did not produce a regular, non-empty media file".to_string(),
        ));
    }
    let size = output_metadata.len();

    match crate::manifest_publish::publish(&output_part, &job.dest, &gate).await {
        Ok(crate::manifest_publish::ManifestPublishOutcome::Aborted) => {
            return Ok(ManifestOutcome::Aborted);
        }
        Ok(crate::manifest_publish::ManifestPublishOutcome::Published) => {
            crate::extension_server::discard_handoff_cookies_async(&job.source.url);
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(ManifestRunError::Failed(
                crate::engine_msg::DEST_EXISTS.to_string(),
            ));
        }
        Err(error) => {
            return Err(manifest_storage_error(
                "Could not publish the finished stream into place",
                error,
            ));
        }
    }
    let mut destination_parts_cleaned = true;
    if matches!(
        mode,
        ManifestFinalizationMode::PublishPreMuxedDestinationPart
    ) && !destination_parts
        && let Err(error) = crate::manifest_destination::remove_owned_part_async(
            staging.to_path_buf(),
            job.dest.clone(),
            job.item_id,
            TrackKind::Video,
        )
        .await
    {
        destination_parts_cleaned = false;
        tracing::warn!(
            item_id = job.item_id,
            error = %error,
            "could not clean up the owned pre-muxed HLS destination part after publication"
        );
    }
    if destination_parts {
        let (video_filename, audio_filename) = if matches!(&tracks, Tracks::Hls { .. }) {
            (
                TrackKind::Video.part_filename(TrackFileFormat::Ts),
                TrackKind::Audio.part_filename(TrackFileFormat::Ts),
            )
        } else {
            (
                TrackKind::Video.part_filename(TrackFileFormat::Mp4),
                TrackKind::Audio.part_filename(TrackFileFormat::Mp4),
            )
        };
        let has_destination_subtitles = matches!(
            &tracks,
            Tracks::Dash {
                subtitle: Some(_),
                ..
            }
        );
        let subtitle_directory = staging.join("subtitles");
        for (track, filename, track_directory, should_clean) in [
            (TrackKind::Video, video_filename, video_dir, true),
            (TrackKind::Audio, audio_filename, audio_dir, has_audio),
            (
                TrackKind::Subtitles,
                TrackKind::Subtitles.part_filename(TrackFileFormat::Mp4),
                subtitle_directory.as_path(),
                has_destination_subtitles,
            ),
        ] {
            if should_clean
                && let Err(error) = crate::manifest_writer::cleanup_destination_part(
                    staging,
                    track_directory,
                    &job.dest,
                    job.item_id,
                    track,
                    filename,
                )
                .await
            {
                destination_parts_cleaned = false;
                tracing::warn!(
                    item_id = job.item_id,
                    track = ?track,
                    error = %error,
                    "could not clean up an owned manifest destination part after publication"
                );
            }
        }
    }

    if let Some(sidecar) = subtitle_sidecar.as_deref() {
        if subtitle_format == SubtitleFormat::Fmp4 {
            crate::video_staging::collect_sidecar_with_extension(
                sidecar,
                &job.dest,
                subtitle_language,
                crate::video_staging::FMP4_SUBTITLE_SIDECAR_EXTENSION,
            );
        } else {
            crate::video_staging::collect_sidecar(sidecar, &job.dest, subtitle_language);
        }
    }
    let parent = job.dest.parent().map(Path::to_path_buf);
    if let Some(parent) = parent {
        if destination_parts_cleaned {
            let id = job.item_id;
            let _ = tokio::task::spawn_blocking(move || {
                crate::video_staging::clean_manifest_staging_files(&parent, id);
            })
            .await;
        } else {
            tracing::warn!(
                item_id = job.item_id,
                "retaining XDG recovery metadata because a destination part could not be safely cleaned"
            );
        }
    }
    Ok(ManifestOutcome::Finished(size))
}

async fn resolve_tracks(
    client: &reqwest::Client,
    lookup: ManifestLookup,
    source: &ManifestSource,
    guarded_child_hosts: &mut crate::download_net::PublicResolutionSet,
) -> Result<Tracks, ManifestRunError> {
    use crate::manifest_plan::ManifestDownloadPlan;

    let plan = crate::manifest_plan::download_plan(lookup, source)
        .map_err(ManifestRunError::Unsupported)?;
    match plan {
        ManifestDownloadPlan::HlsMedia(video) => {
            guard_hls_resources(&source.url, &video, guarded_child_hosts)?;
            Ok(Tracks::Hls {
                video,
                audio: None,
                subtitle: None,
                subtitle_language: None,
                subtitle_format: SubtitleFormat::Text,
            })
        }
        ManifestDownloadPlan::HlsMaster(plan) => {
            let crate::manifest_plan::HlsMasterDownloadPlan {
                video_url,
                audio_url,
                subtitle_url,
                subtitle_language,
                inherited_variables,
            } = *plan;
            guard_manifest_child_url(&source.url, &video_url, guarded_child_hosts)?;
            if let Some(audio_url) = audio_url.as_ref() {
                guard_manifest_child_url(&source.url, audio_url, guarded_child_hosts)?;
            }
            let video = crate::manifest_intake::lookup_hls_media_with_variables(
                client,
                video_url.as_str(),
                &inherited_variables,
            )
            .await?;
            guard_hls_resources(&source.url, &video, guarded_child_hosts)?;
            let subtitle = match subtitle_url {
                Some(url) => {
                    guard_manifest_child_url(&source.url, &url, guarded_child_hosts)?;
                    let plan = crate::manifest_intake::lookup_hls_media_with_variables(
                        client,
                        url.as_str(),
                        &inherited_variables,
                    )
                    .await?;
                    if plan.is_live {
                        return Err(ManifestRunError::Unsupported(
                            "Native live HLS subtitle renditions are not supported yet".to_string(),
                        ));
                    }
                    guard_hls_resources(&source.url, &plan, guarded_child_hosts)?;
                    Some(Box::new(plan))
                }
                None => None,
            };
            let audio = match audio_url {
                Some(url) => {
                    let plan = crate::manifest_intake::lookup_hls_media_with_variables(
                        client,
                        url.as_str(),
                        &inherited_variables,
                    )
                    .await?;
                    guard_hls_resources(&source.url, &plan, guarded_child_hosts)?;
                    Some(plan)
                }
                None => None,
            };
            if subtitle.is_some()
                && (video.is_live || audio.as_ref().is_some_and(|track| track.is_live))
            {
                return Err(ManifestRunError::Unsupported(
                    "Native live HLS subtitle renditions are not supported yet".to_string(),
                ));
            }
            let subtitle_format = subtitle
                .as_deref()
                .map(hls_subtitle_format)
                .unwrap_or(SubtitleFormat::Text);
            Ok(Tracks::Hls {
                video: Box::new(video),
                audio: audio.map(Box::new),
                subtitle,
                subtitle_language,
                subtitle_format,
            })
        }
        ManifestDownloadPlan::Dash {
            video,
            audio,
            subtitle,
        } => {
            for resource in video.resources.iter() {
                guard_manifest_child_url(&source.url, &resource.url, guarded_child_hosts)?;
            }
            if let Some(audio) = audio.as_ref() {
                for resource in audio.resources.iter() {
                    guard_manifest_child_url(&source.url, &resource.url, guarded_child_hosts)?;
                }
            }
            if let Some(subtitle) = subtitle.as_ref() {
                for resource in subtitle.resources.iter() {
                    guard_manifest_child_url(&source.url, &resource.url, guarded_child_hosts)?;
                }
            }
            let subtitle_language = subtitle
                .as_ref()
                .and_then(|track| track.representation.language.clone());
            Ok(Tracks::Dash {
                video,
                audio,
                subtitle,
                subtitle_language,
            })
        }
    }
}

/// Apple HLS fMP4 subtitle renditions include an initialization map.
fn hls_subtitle_format(plan: &HlsPlan) -> SubtitleFormat {
    if plan.output_container() == Some(ManifestContainer::Mp4) {
        SubtitleFormat::Fmp4
    } else {
        SubtitleFormat::Text
    }
}

/// Whether the selected subtitle rendition can be embedded in the chosen container.
fn embed_manifest_subtitles(
    embed_requested: bool,
    output_container: ManifestContainer,
    subtitle_format: SubtitleFormat,
) -> bool {
    embed_requested
        && output_container != ManifestContainer::Ts
        && (subtitle_format == SubtitleFormat::Text || output_container == ManifestContainer::Mp4)
}

fn guard_hls_resources(
    parent_url: &str,
    plan: &HlsPlan,
    seen_hosts: &mut crate::download_net::PublicResolutionSet,
) -> Result<(), ManifestChildUrlError> {
    for resource in plan.resources.iter() {
        guard_manifest_child_url(parent_url, &resource.url, seen_hosts)?;
    }
    Ok(())
}

/// Extend the browser-handoff DNS policy to every host discovered in a native
/// presentation. The same collection cap is used by playlist/gallery imports.
fn guard_manifest_child_url(
    parent_url: &str,
    child_url: &Url,
    seen_hosts: &mut crate::download_net::PublicResolutionSet,
) -> Result<(), ManifestChildUrlError> {
    match seen_hosts.inherit(parent_url, child_url.as_str()) {
        Ok(()) => Ok(()),
        Err(crate::download_net::CollectionHostError::TooManyDistinctHosts) => {
            Err(ManifestChildUrlError::TooManyDistinctHosts(
                crate::download_net::MAX_GUARDED_CHILD_HOSTS_PER_COLLECTION,
            ))
        }
        Err(crate::download_net::CollectionHostError::PolicyCapReached) => {
            Err(ManifestChildUrlError::PolicyCapReached)
        }
        Err(crate::download_net::CollectionHostError::UnsafeDestination) => {
            Err(ManifestChildUrlError::UnsafeDestination)
        }
    }
}

fn hls_resources(plan: &HlsPlan) -> Vec<Resource> {
    hls_resources_from(&plan.resources)
}

fn hls_resources_from(resources: &[HlsResource]) -> Vec<Resource> {
    resources
        .iter()
        .map(|resource| Resource {
            url: resource.url.clone(),
            range: resource.range,
            filename: resource.filename.clone(),
            key: resource.kind == HlsResourceKind::Key,
        })
        .collect()
}

fn dash_resources(plan: &DashRepresentationPlan) -> Vec<Resource> {
    dash_resources_from(&plan.resources)
}

fn dash_resources_from(resources: &[DashResource]) -> Vec<Resource> {
    resources
        .iter()
        .map(|resource| Resource {
            url: resource.url.clone(),
            range: resource.range,
            filename: resource.filename.clone(),
            key: false,
        })
        .collect()
}

async fn ensure_track_dir(path: &Path) -> Result<(), ManifestTrackDirError> {
    match tokio::fs::create_dir(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = tokio::fs::symlink_metadata(path)
                .await
                .map_err(ManifestTrackDirError::Inspect)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(ManifestTrackDirError::NotDirectory);
            }
            Ok(())
        }
        Err(error) => Err(ManifestTrackDirError::Create(error)),
    }
}

const LIVE_HLS_MARKER_NAME: &str = "live-hls.capture";
const LIVE_HLS_MARKER_CONTENT: &[u8] = b"grab-live-hls-v1\n";
const LIVE_FINISH_MARKER_NAME: &str = "live-capture.finished";
const LIVE_FINISH_MARKER_VERSION: &str = "grab-live-finish-v1";

#[derive(Debug)]
pub(super) enum StagedFileError {
    NotRegularFile,
    Io(std::io::Error),
}

pub(super) async fn inspect_staged_regular_file(
    path: &Path,
) -> Result<Option<std::fs::Metadata>, StagedFileError> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(StagedFileError::NotRegularFile)
        }
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(StagedFileError::Io(error)),
    }
}

/// Read the durable finish intent and committed-byte count for a live capture.
async fn read_live_finish_marker(directory: &Path) -> Result<Option<u64>, String> {
    let marker = directory.join(LIVE_FINISH_MARKER_NAME);
    let Some(metadata) =
        inspect_staged_regular_file(&marker)
            .await
            .map_err(|error| match error {
                StagedFileError::NotRegularFile => {
                    "The live finish marker is not a supported regular file".to_string()
                }
                StagedFileError::Io(error) => {
                    format!("Could not inspect live finish marker: {error}")
                }
            })?
    else {
        return Ok(None);
    };
    if metadata.len() > 128 {
        return Err("The live finish marker is not a supported regular file".to_string());
    }
    let contents = tokio::fs::read(&marker)
        .await
        .map_err(|error| format!("Could not read live finish marker: {error}"))?;
    let text = std::str::from_utf8(&contents)
        .map_err(|_| "The live finish marker is not valid UTF-8".to_string())?;
    let mut lines = text.lines();
    if lines.next() != Some(LIVE_FINISH_MARKER_VERSION) {
        return Err("The live finish marker has an unsupported version".to_string());
    }
    let staged_bytes = lines
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| "The live finish marker has an invalid byte count".to_string())?;
    if lines.next().is_some() {
        return Err("The live finish marker contains unexpected data".to_string());
    }
    Ok(Some(staged_bytes))
}

/// Persist the finish decision before finalization so retries never resume capture.
async fn ensure_live_finish_marker(directory: &Path, staged_bytes: u64) -> Result<(), String> {
    if let Some(existing_bytes) = read_live_finish_marker(directory).await? {
        return if existing_bytes == staged_bytes {
            Ok(())
        } else {
            Err("The live finish marker byte count changed between attempts".to_string())
        };
    }

    let contents = format!("{LIVE_FINISH_MARKER_VERSION}\n{staged_bytes}\n");
    write_staged_file_atomic(
        directory,
        "live-capture.finished.part",
        LIVE_FINISH_MARKER_NAME,
        contents.as_bytes(),
        StagedPublish::NoReplace,
    )
    .await?;
    sync_staging_directory(directory).await
}

async fn staged_regular_file_exists(path: &Path) -> bool {
    tokio::fs::symlink_metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
}

async fn sync_staging_directory(directory: &Path) -> Result<(), String> {
    let handle = tokio::fs::File::open(directory)
        .await
        .map_err(|error| format!("Could not open staging directory for sync: {error}"))?;
    handle
        .sync_all()
        .await
        .map_err(|error| format!("Could not sync staging directory: {error}"))
}

#[derive(Clone, Copy)]
pub(super) enum StagedPublish {
    Replace,
    NoReplace,
}

pub(super) async fn write_staged_file_atomic(
    directory: &Path,
    temporary_name: &str,
    target_name: &str,
    bytes: &[u8],
    publish: StagedPublish,
) -> Result<(), String> {
    let temporary = directory.join(temporary_name);
    let target = directory.join(target_name);
    remove_regular_file_if_present(&temporary).await?;

    let result = async {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .await
            .map_err(|error| format!("Could not create staged file {target_name}: {error}"))?;
        file.write_all(bytes)
            .await
            .map_err(|error| format!("Could not write staged file {target_name}: {error}"))?;
        file.sync_all()
            .await
            .map_err(|error| format!("Could not flush staged file {target_name}: {error}"))?;
        drop(file);

        match publish {
            StagedPublish::Replace => {
                match tokio::fs::symlink_metadata(&target).await {
                    Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                        return Err(format!("Staged target {target_name} is not a regular file"));
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!(
                            "Could not inspect staged target {target_name}: {error}"
                        ));
                    }
                }
                tokio::fs::rename(&temporary, &target)
                    .await
                    .map_err(|error| {
                        format!("Could not publish staged file {target_name}: {error}")
                    })
            }
            StagedPublish::NoReplace => crate::file_names::rename_noreplace(&temporary, &target)
                .map_err(|error| format!("Could not publish staged file {target_name}: {error}")),
        }
    }
    .await;

    if result.is_err() {
        let _ = remove_regular_file_if_present(&temporary).await;
    }
    result
}

struct IncrementalHlsFetch<'fetch> {
    client: &'fetch reqwest::Client,
    plan: &'fetch HlsPlan,
    directory: &'fetch Path,
    destination_dir: &'fetch Path,
    output_part: &'fetch Path,
    tx: &'fetch tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    downloaded: &'fetch AtomicU64,
    abort: &'fetch mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
    connections: usize,
    first_segment_ready: tokio::sync::oneshot::Sender<()>,
    complete_ready: tokio::sync::oneshot::Sender<()>,
}

async fn fetch_hls_resources_incrementally(
    context: IncrementalHlsFetch<'_>,
) -> Result<bool, ManifestRunError> {
    let IncrementalHlsFetch {
        client,
        plan,
        directory,
        destination_dir,
        output_part,
        tx,
        downloaded,
        abort,
        connections,
        first_segment_ready,
        complete_ready,
    } = context;

    /// Cap atomic playlist rewrites while keeping the visible output responsive for long VODs.
    const MAX_PLAYLIST_SNAPSHOTS: usize = 512;

    let resources = hls_resources(plan);
    if resources.len() != plan.resources.len() {
        return Err(ManifestRunError::Failed(
            "The localized HLS resource plan changed while preparing incremental playback"
                .to_string(),
        ));
    }
    let total_segments = plan.segment_count();
    if total_segments == 0 {
        return Err(ManifestRunError::Failed(
            "The selected HLS VOD playlist contains no media segments".to_string(),
        ));
    }
    let batch_size = total_segments.div_ceil(MAX_PLAYLIST_SNAPSHOTS).max(1);
    let resource_count = resources.len();
    let mut completed = vec![false; resource_count];
    let key_indices: Vec<_> = plan
        .resources
        .iter()
        .enumerate()
        .filter_map(|(index, resource)| {
            (resource.kind == crate::manifest::HlsResourceKind::Key).then_some(index)
        })
        .collect();
    let mut next_to_commit = 0usize;
    let mut completed_segments = 0usize;
    let mut last_snapshot_segments = 0usize;
    let mut first_segment_ready = Some(first_segment_ready);

    let result = async {
        let jobs = futures_util::stream::iter(resources.into_iter().enumerate().map(
            |(index, resource)| async move {
                fetch_resource(client, resource, directory, tx, downloaded)
                    .await
                    .map(|_| index)
            },
        ))
        .buffer_unordered(connections.clamp(1, MAX_RESOURCE_CONNECTIONS));
        tokio::pin!(jobs);
        let mut stop_pending = true;

        loop {
            tokio::select! {
                biased;
                stop = &mut *abort, if stop_pending => {
                    stop_pending = false;
                    match crate::video::stop_intent_or_pause(stop) {
                        crate::video::StopIntent::Finish => {}
                        crate::video::StopIntent::Pause | crate::video::StopIntent::Discard => {
                            break Ok(false);
                        }
                    }
                }
                result = jobs.next() => {
                    let Some(result) = result else {
                        break Ok(true);
                    };
                    let index = result?;
                    ensure_incremental_hls_output_space(
                        destination_dir,
                        output_part,
                        downloaded.load(Ordering::Relaxed),
                    )
                    .await?;
                    completed[index] = true;
                    while next_to_commit < resource_count && completed[next_to_commit] {
                        if plan.resources[next_to_commit].kind == crate::manifest::HlsResourceKind::Segment {
                            completed_segments += 1;
                        }
                        next_to_commit += 1;
                    }
                    let all_keys_ready = key_indices.iter().all(|index| completed[*index]);
                    if all_keys_ready
                        && completed_segments > last_snapshot_segments
                        && (last_snapshot_segments == 0
                            || completed_segments - last_snapshot_segments >= batch_size)
                    {
                        let bytes = plan
                            .local_playlist_prefix_bytes(
                                completed_segments,
                                crate::manifest::HlsPlaylistFinality::Growing,
                            )
                            .map_err(|error| ManifestRunError::Failed(error.to_string()))?;
                        write_staged_file_atomic(
                            directory,
                            "local.m3u8.next",
                            "local.m3u8",
                            &bytes,
                            StagedPublish::Replace,
                        )
                        .await
                        .map_err(ManifestRunError::Failed)?;
                        last_snapshot_segments = completed_segments;
                        if let Some(sender) = first_segment_ready.take() {
                            let _ = sender.send(());
                        }
                    }
                }
            }
        }
    }
    .await;

    match result {
        Ok(false) => {
            tx.send(progress_message(downloaded.load(Ordering::Relaxed)))
                .ok();
            return Ok(false);
        }
        Err(error) => {
            tx.send(progress_message(downloaded.load(Ordering::Relaxed)))
                .ok();
            return Err(error);
        }
        Ok(true) => {}
    }

    if completed_segments != total_segments {
        return Err(ManifestRunError::Failed(
            "The incremental HLS transfer did not commit every planned media segment".to_string(),
        ));
    }
    if pending_manifest_stop_intent(abort).is_some_and(|intent| {
        matches!(
            intent,
            crate::video::StopIntent::Pause | crate::video::StopIntent::Discard
        )
    }) {
        tx.send(progress_message(downloaded.load(Ordering::Relaxed)))
            .ok();
        return Ok(false);
    }

    let bytes = plan
        .local_playlist_prefix_bytes(
            total_segments,
            crate::manifest::HlsPlaylistFinality::Complete,
        )
        .map_err(|error| ManifestRunError::Failed(error.to_string()))?;
    write_staged_file_atomic(
        directory,
        "local.m3u8.next",
        "local.m3u8",
        &bytes,
        StagedPublish::Replace,
    )
    .await
    .map_err(ManifestRunError::Failed)?;
    let _ = complete_ready.send(());
    Ok(true)
}

async fn write_playlist(directory: &Path, plan: &HlsPlan) -> Result<(), String> {
    let bytes = plan
        .local_playlist_prefix_bytes(
            plan.segment_count(),
            crate::manifest::HlsPlaylistFinality::Complete,
        )
        .map_err(|error| error.to_string())?;
    write_staged_file_atomic(
        directory,
        "local.m3u8.part",
        "local.m3u8",
        &bytes,
        StagedPublish::Replace,
    )
    .await
}

/// Read a staged SAMPLE-AES resource off the async executor with a dedicated
/// bound for data that must be held in memory by the decryptor and preflight.
async fn read_sample_aes_resource(
    path: &Path,
    description: &'static str,
    max_bytes: usize,
) -> Result<Vec<u8>, ManifestRunError> {
    let path = path.to_path_buf();
    let result = tokio::task::spawn_blocking(move || {
        crate::bounded_file::read_regular_file_bounded(&path, max_bytes)
    })
    .await
    .map_err(|error| {
        ManifestRunError::Failed(format!("The {description} read worker failed: {error}"))
    })?;

    result.map_err(|error| match error {
        crate::bounded_file::BoundedFileError::NotRegularFile => {
            ManifestRunError::Failed(format!("The {description} is not a regular file"))
        }
        crate::bounded_file::BoundedFileError::TooLarge => ManifestRunError::Unsupported(format!(
            "The {description} exceeds its {max_bytes}-byte SAMPLE-AES memory limit"
        )),
        crate::bounded_file::BoundedFileError::Io(source) => {
            ManifestRunError::Failed(format!("Could not read {description}: {source}"))
        }
    })
}

/// Decrypt identity SAMPLE-AES MPEG-TS segments with iori-ssa, the engine used by VSD,
/// then remove the corresponding key tags from the local playlist.
async fn decrypt_hls_sample_aes_segments(
    directory: &Path,
    plan: &HlsPlan,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
) -> Result<bool, ManifestRunError> {
    let playlist_bytes = plan
        .local_playlist_bytes()
        .map_err(|error| ManifestRunError::Failed(error.to_string()))?;
    let mut playlist = m3u8_rs::parse_media_playlist_res(&playlist_bytes).map_err(|error| {
        ManifestRunError::Failed(format!(
            "Could not parse the localized HLS playlist: {error:?}"
        ))
    })?;
    let mut changed = false;

    for (index, segment) in playlist.segments.iter_mut().enumerate() {
        let Some(key_tag) = segment
            .key
            .as_ref()
            .filter(|key| key.method == m3u8_rs::KeyMethod::SampleAES)
        else {
            continue;
        };

        let key_uri = key_tag.uri.as_deref().ok_or_else(|| {
            ManifestRunError::Unsupported("The HLS SAMPLE-AES key URI is missing".to_string())
        })?;
        let key_path = local_hls_resource_path(directory, key_uri)?;
        let key_bytes =
            read_sample_aes_resource(&key_path, "staged HLS SAMPLE-AES key", 16).await?;
        let key: [u8; 16] = key_bytes.as_slice().try_into().map_err(|_| {
            ManifestRunError::Unsupported("The HLS identity key is not 16 bytes".to_string())
        })?;

        let sequence_offset = u64::try_from(index).map_err(|_| {
            ManifestRunError::Failed("The HLS media sequence overflowed".to_string())
        })?;
        let media_sequence = playlist
            .media_sequence
            .checked_add(sequence_offset)
            .ok_or_else(|| {
                ManifestRunError::Failed("The HLS media sequence overflowed".to_string())
            })?;
        let iv = hls_sample_aes_iv(key_tag.iv.as_deref(), media_sequence)?;

        let segment_uri = segment.uri.clone();
        let segment_path = local_hls_resource_path(directory, &segment_uri)?;
        let encrypted = read_sample_aes_resource(
            &segment_path,
            "staged HLS SAMPLE-AES segment",
            MAX_SAMPLE_AES_SEGMENT_BYTES,
        )
        .await?;
        let encrypted_size = encrypted.len();

        let decrypted = tokio::select! {
            biased;
            _ = &mut *abort => return Ok(false),
            result = tokio::time::timeout(
                SAMPLE_AES_DECRYPT_TIMEOUT,
                tokio::task::spawn_blocking(move || {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        validate_iori_sample_aes_segment(&encrypted)?;
                        let mut decrypted = Vec::with_capacity(encrypted.len());
                        iori_ssa::decrypt(
                            std::io::Cursor::new(encrypted.as_slice()),
                            &mut decrypted,
                            key,
                            iv,
                        )
                        .map_err(|error| error.to_string())?;
                        Ok::<Vec<u8>, String>(decrypted)
                    }))
                }),
            ) => {
                let worker = result.map_err(|_| {
                    ManifestRunError::Unsupported(format!(
                        "iori-ssa timed out while processing HLS SAMPLE-AES segment {segment_uri}"
                    ))
                })?;
                worker.map_err(|error| {
                    ManifestRunError::Failed(format!(
                        "The HLS SAMPLE-AES worker failed for {segment_uri}: {error}"
                    ))
                })?
            }
        };
        let decrypted = match decrypted {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(error)) => {
                return Err(ManifestRunError::Unsupported(format!(
                    "iori-ssa cannot safely process HLS SAMPLE-AES segment {segment_uri}: {error}"
                )));
            }
            Err(_) => {
                return Err(ManifestRunError::Unsupported(format!(
                    "iori-ssa panicked while processing HLS SAMPLE-AES segment {segment_uri}"
                )));
            }
        };

        if decrypted.len() != encrypted_size {
            return Err(ManifestRunError::Unsupported(
                "iori-ssa returned an incomplete HLS SAMPLE-AES segment".to_string(),
            ));
        }
        // The encrypted input has already passed the bounded SAMPLE-AES/PES
        // preflight. Let FFmpeg validate the complete decrypted playlist after
        // all segments are localized; individual segments may need demux context.

        // Write each decrypted occurrence to a distinct staged file. Keep
        // encrypted source resources untouched until the whole playlist succeeds.
        let decrypted_uri = format!("sample-aes-decrypted-{index}.bin");
        local_hls_resource_path(directory, &decrypted_uri)?;
        write_staged_file_atomic(
            directory,
            &format!("{decrypted_uri}.part"),
            &decrypted_uri,
            &decrypted,
            StagedPublish::Replace,
        )
        .await
        .map_err(ManifestRunError::Failed)?;

        segment.uri = decrypted_uri;
        segment.key = None;
        changed = true;
    }

    if changed {
        let mut rewritten = Vec::new();
        playlist.write_to(&mut rewritten).map_err(|error| {
            ManifestRunError::Failed(format!(
                "Could not serialize the decrypted HLS playlist: {error}"
            ))
        })?;
        write_staged_file_atomic(
            directory,
            "local.m3u8.sample-aes.part",
            "local.m3u8",
            &rewritten,
            StagedPublish::Replace,
        )
        .await
        .map_err(ManifestRunError::Failed)?;
        sync_staging_directory(directory)
            .await
            .map_err(ManifestRunError::Failed)?;
    }

    Ok(true)
}

/// Validate the MPEG-TS packet/PES structure before handing untrusted bytes to
/// iori-ssa's SAMPLE-AES parser. Use the same mpeg2ts release as iori-ssa so
/// the guard and consumer cannot disagree about packet/PES reassembly.
fn validate_iori_sample_aes_segment(bytes: &[u8]) -> Result<(), String> {
    use mpeg2ts::{
        es::StreamType,
        ts::{ReadTsPacket, TsPacketReader, TsPayload},
    };

    let mut reader = TsPacketReader::new(std::io::Cursor::new(bytes));
    let mut stream_types = std::collections::HashMap::<u16, StreamType>::new();
    let mut aac_pes = std::collections::HashMap::<u16, Vec<u8>>::new();

    while let Some(packet) = reader
        .read_ts_packet()
        .map_err(|error| format!("invalid MPEG-TS packet: {error:?}"))?
    {
        let pid = packet.header.pid.as_u16();
        let flush = matches!(
            &packet.payload,
            Some(TsPayload::Pat(_) | TsPayload::Pmt(_) | TsPayload::Section(_))
        );

        match packet.payload {
            Some(TsPayload::Pmt(pmt)) => {
                for stream in pmt.es_info {
                    stream_types.insert(stream.elementary_pid.as_u16(), stream.stream_type);
                }
            }
            Some(TsPayload::Pes(pes)) => {
                if let Some(previous) = aac_pes.remove(&pid) {
                    validate_iori_adts_pes(&previous)?;
                }
                if stream_types.get(&pid).is_some_and(|stream_type| {
                    matches!(
                        *stream_type,
                        StreamType::AdtsAac | StreamType::AdtsAacWithAes128Cbc
                    )
                }) {
                    aac_pes.insert(pid, pes.data.to_vec());
                }
            }
            Some(TsPayload::Raw(payload)) => {
                if let Some(data) = aac_pes.get_mut(&pid) {
                    data.extend_from_slice(&payload);
                } else if stream_types.get(&pid).is_some_and(|stream_type| {
                    matches!(
                        *stream_type,
                        StreamType::AdtsAac | StreamType::AdtsAacWithAes128Cbc
                    )
                }) {
                    return Err("AAC PES continuation has no preceding PES header".to_string());
                }
            }
            _ => {}
        }

        if flush && let Some(data) = aac_pes.remove(&pid) {
            validate_iori_adts_pes(&data)?;
        }
    }

    for data in aac_pes.into_values() {
        validate_iori_adts_pes(&data)?;
    }

    Ok(())
}

fn validate_iori_adts_pes(mut data: &[u8]) -> Result<(), String> {
    while !data.is_empty() {
        if data.len() < 7 || data[0] != 0xff || data[1] & 0xf6 != 0xf0 {
            return Err("malformed ADTS frame header".to_string());
        }

        let frame_length = (usize::from(data[3] & 0x03) << 11)
            | (usize::from(data[4]) << 3)
            | usize::from(data[5] >> 5);
        let header_length = if data[1] & 0x01 == 0 { 9 } else { 7 };
        if frame_length < header_length || frame_length > data.len() {
            return Err("ADTS frame length is invalid or incomplete".to_string());
        }
        data = &data[frame_length..];
    }

    Ok(())
}

fn local_hls_resource_path(directory: &Path, uri: &str) -> Result<PathBuf, ManifestRunError> {
    let mut components = Path::new(uri).components();
    if !matches!(components.next(), Some(std::path::Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(ManifestRunError::Unsupported(
            "The localized HLS playlist contains an unsafe resource path".to_string(),
        ));
    }
    Ok(directory.join(uri))
}

fn hls_sample_aes_iv(iv: Option<&str>, media_sequence: u64) -> Result<[u8; 16], ManifestRunError> {
    let Some(iv) = iv else {
        return Ok(u128::from(media_sequence).to_be_bytes());
    };
    let hex = iv
        .strip_prefix("0x")
        .or_else(|| iv.strip_prefix("0X"))
        .unwrap_or(iv);
    if hex.len() != 32 {
        return Err(ManifestRunError::Unsupported(
            "The HLS SAMPLE-AES IV is not 128 bits".to_string(),
        ));
    }
    let mut parsed = [0_u8; 16];
    for (index, byte) in parsed.iter_mut().enumerate() {
        let high = hls_hex_nibble(hex.as_bytes()[index * 2]).ok_or_else(|| {
            ManifestRunError::Unsupported("The HLS SAMPLE-AES IV is invalid".to_string())
        })?;
        let low = hls_hex_nibble(hex.as_bytes()[index * 2 + 1]).ok_or_else(|| {
            ManifestRunError::Unsupported("The HLS SAMPLE-AES IV is invalid".to_string())
        })?;
        *byte = (high << 4) | low;
    }
    Ok(parsed)
}

fn hls_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

async fn fetch_resources(
    client: &reqwest::Client,
    resources: Vec<Resource>,
    directory: &Path,
    connections: usize,
    tx: &tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    downloaded: &Arc<AtomicU64>,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
) -> Result<bool, ManifestRunError> {
    match fetch_resources_with_stop(
        client,
        resources,
        directory,
        connections,
        tx,
        downloaded,
        abort,
    )
    .await?
    {
        ResourceFetchOutcome::Complete => Ok(true),
        ResourceFetchOutcome::Stopped(_) => Ok(false),
    }
}

async fn fetch_resources_with_stop(
    client: &reqwest::Client,
    resources: Vec<Resource>,
    directory: &Path,
    connections: usize,
    tx: &tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    downloaded: &Arc<AtomicU64>,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
) -> Result<ResourceFetchOutcome, ManifestRunError> {
    let result = {
        let jobs = futures_util::stream::iter(resources.into_iter().map(|resource| {
            let client = client.clone();
            let directory = directory.to_path_buf();
            let tx = tx.clone();
            let downloaded = Arc::clone(downloaded);
            async move { fetch_resource(&client, resource, &directory, &tx, &downloaded).await }
        }))
        .buffer_unordered(connections.clamp(1, MAX_RESOURCE_CONNECTIONS));
        tokio::pin!(jobs);
        tokio::select! {
            biased;
            stop = &mut *abort => ResourceFetchResult::Stopped(
                crate::video::stop_intent_or_pause(stop),
            ),
            result = jobs.try_for_each(|_| async { Ok(()) }) => {
                result.map_or_else(
                    ResourceFetchResult::Failed,
                    |_| ResourceFetchResult::Complete,
                )
            }
        }
    };

    // The scoped stream has dropped its remaining futures and refunded their
    // partial byte reservations before the corrected total is sent.
    match result {
        ResourceFetchResult::Complete => Ok(ResourceFetchOutcome::Complete),
        ResourceFetchResult::Stopped(intent) => {
            tx.send(progress_message(downloaded.load(Ordering::Relaxed)))
                .ok();
            Ok(ResourceFetchOutcome::Stopped(intent))
        }
        ResourceFetchResult::Failed(error) => {
            tx.send(progress_message(downloaded.load(Ordering::Relaxed)))
                .ok();
            Err(error)
        }
    }
}

enum ResourceFetchResult {
    Complete,
    Stopped(crate::video::StopIntent),
    Failed(ManifestRunError),
}

fn reserve_download_bytes(downloaded: &AtomicU64, bytes: u64) -> Result<u64, String> {
    downloaded
        .try_update(Ordering::AcqRel, Ordering::Relaxed, |current| {
            current
                .checked_add(bytes)
                .filter(|total| *total <= MAX_MANIFEST_DOWNLOAD_BYTES)
        })
        .map(|previous| previous + bytes)
        .map_err(|_| {
            "The stream presentation exceeds Grab's 1 TiB native download limit".to_string()
        })
}

struct ResourceByteReservation<'a> {
    downloaded: &'a AtomicU64,
    reserved: u64,
    committed: bool,
}

impl<'a> ResourceByteReservation<'a> {
    fn new(downloaded: &'a AtomicU64) -> Self {
        Self {
            downloaded,
            reserved: 0,
            committed: false,
        }
    }

    fn reserve(&mut self, bytes: u64) -> Result<u64, String> {
        let next_reserved = self
            .reserved
            .checked_add(bytes)
            .ok_or_else(|| "The reserved stream resource byte count overflowed".to_string())?;
        let total = reserve_download_bytes(self.downloaded, bytes)?;
        self.reserved = next_reserved;
        Ok(total)
    }

    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for ResourceByteReservation<'_> {
    fn drop(&mut self) {
        if !self.committed && self.reserved > 0 {
            self.downloaded.fetch_sub(self.reserved, Ordering::AcqRel);
        }
    }
}

async fn fetch_resource(
    client: &reqwest::Client,
    resource: Resource,
    directory: &Path,
    tx: &tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    downloaded: &AtomicU64,
) -> Result<u64, ManifestRunError> {
    let result = fetch_resource_once(client, &resource, directory, tx, downloaded).await;
    if result.is_err() {
        // The failed attempt's reservation has been dropped; publish the rolled-back total.
        tx.send(progress_message(downloaded.load(Ordering::Relaxed)))
            .ok();
    }
    result
}

async fn fetch_resource_once(
    client: &reqwest::Client,
    resource: &Resource,
    directory: &Path,
    tx: &tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    downloaded: &AtomicU64,
) -> Result<u64, ManifestRunError> {
    if !safe_resource_filename(&resource.filename) {
        return Err(ManifestRunError::Failed(
            "The stream manifest contains an unsafe resource name".to_string(),
        ));
    }
    let target = directory.join(&resource.filename);
    match inspect_staged_regular_file(&target).await {
        Ok(Some(metadata)) => {
            if staged_resource_size_ok(metadata.len(), resource.range, resource.key) {
                let size = metadata.len();
                let total = reserve_download_bytes(downloaded, size)?;
                tx.send(progress_message(total)).ok();
                return Ok(size);
            }
            tokio::fs::remove_file(&target).await.map_err(|error| {
                manifest_storage_error("Could not reset an incomplete manifest resource", error)
            })?;
        }
        Ok(None) => {}
        Err(StagedFileError::NotRegularFile) => {
            return Err(ManifestRunError::Failed(
                "A manifest resource path is not a regular file".to_string(),
            ));
        }
        Err(StagedFileError::Io(_)) => {}
    }

    let part = directory.join(format!("{}.part", resource.filename));
    remove_regular_file_if_present(&part).await?;
    let mut request = client.get(resource.url.clone());
    if let Some(range) = resource.range {
        let end = range
            .offset
            .checked_add(range.length)
            .and_then(|value| value.checked_sub(1))
            .ok_or_else(|| "The stream manifest contains an invalid byte range".to_string())?;
        request = request.header(
            reqwest::header::RANGE,
            format!("bytes={}-{end}", range.offset),
        );
    }
    let response = tokio::time::timeout(RESOURCE_IDLE_TIMEOUT, request.send())
        .await
        .map_err(|_| {
            ManifestRunError::Retryable("Timed out while requesting a stream segment".to_string())
        })?
        .map_err(|error| {
            ManifestRunError::Retryable(format!(
                "Could not retrieve a stream segment: {}",
                error.without_url()
            ))
        })?;
    let status = response.status();
    if let Some(range) = resource.range {
        validate_byte_range_response(
            status,
            response
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|value| value.to_str().ok()),
            range,
            "a byte-range segment",
        )?;
    } else if status == reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(ManifestRunError::Unsupported(
            "The stream server returned partial content without a requested byte range".to_string(),
        ));
    } else if !status.is_success() {
        return Err(resource_status_error(status));
    }
    let content_length = response.content_length();
    if content_length.is_some_and(|length| length > MAX_RESOURCE_BYTES) {
        return Err(ManifestRunError::Unsupported(
            "A stream segment exceeds the size limit".to_string(),
        ));
    }
    if let Some(range) = resource.range
        && byte_range_content_length_exceeds(range, content_length)
    {
        return Err(ManifestRunError::Unsupported(
            "The stream server returned more bytes than the requested range".to_string(),
        ));
    }
    if resource.key && content_length.is_some_and(|length| length != 16) {
        return Err(ManifestRunError::Unsupported(
            "The HLS identity key is not 16 bytes".to_string(),
        ));
    }

    let mut reservation = ResourceByteReservation::new(downloaded);
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part)
        .await
        .map_err(|error| manifest_storage_error("Could not create a segment file", error))?;
    let mut stream = response.bytes_stream();
    let mut size = 0_u64;
    let pace_start = Instant::now();
    let mut paced = 0_u64;
    let mut last_progress = Instant::now();
    loop {
        let next = tokio::time::timeout(RESOURCE_IDLE_TIMEOUT, stream.next())
            .await
            .map_err(|_| {
                ManifestRunError::Retryable("A stream segment stopped making progress".to_string())
            })?;
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk.map_err(|error| {
            ManifestRunError::Retryable(format!(
                "A stream segment transfer failed: {}",
                error.without_url()
            ))
        })?;
        if resource
            .range
            .is_some_and(|range| byte_range_chunk_exceeds(range, size, chunk.len()))
        {
            return Err(ManifestRunError::Unsupported(
                "The stream server returned more bytes than the requested range".to_string(),
            ));
        }
        let chunk_len = u64::try_from(chunk.len()).map_err(|_| {
            ManifestRunError::Unsupported("A stream segment exceeds the size limit".to_string())
        })?;
        size = size
            .checked_add(chunk_len)
            .filter(|size| *size <= MAX_RESOURCE_BYTES)
            .ok_or_else(|| {
                ManifestRunError::Unsupported("A stream segment exceeds the size limit".to_string())
            })?;
        if resource.key && size > 16 {
            return Err(ManifestRunError::Unsupported(
                "The HLS identity key is not 16 bytes".to_string(),
            ));
        }
        file.write_all(&chunk)
            .await
            .map_err(|error| manifest_storage_error("Could not save a stream segment", error))?;
        crate::download_rate::pace_chunk(
            &mut paced,
            pace_start,
            crate::download_rate::live_rate_limit(),
            chunk.len(),
        )
        .await;
        let total = reservation.reserve(chunk.len() as u64)?;
        if last_progress.elapsed() >= Duration::from_millis(100) {
            tx.send(progress_message(total)).ok();
            last_progress = Instant::now();
        }
    }
    if let Some(range) = resource.range
        && size < range.length
    {
        return Err(ManifestRunError::Retryable(
            "The stream server ended a byte-range segment before all bytes arrived".to_string(),
        ));
    }
    if size == 0 || (resource.key && size != 16) {
        return Err(ManifestRunError::Unsupported(if resource.key {
            "The HLS identity key is not 16 bytes".to_string()
        } else {
            "The stream server returned an empty segment".to_string()
        }));
    }
    file.sync_all()
        .await
        .map_err(|error| manifest_storage_error("Could not flush a stream segment", error))?;
    drop(file);
    crate::file_names::rename_noreplace(&part, &target)
        .map_err(|error| manifest_storage_error("Could not publish a stream segment", error))?;
    reservation.commit();
    tx.send(progress_message(downloaded.load(Ordering::Relaxed)))
        .ok();
    Ok(size)
}

fn is_retryable_resource_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

fn resource_status_error(status: reqwest::StatusCode) -> ManifestRunError {
    let message = format!(
        "The stream server returned HTTP {} for a segment",
        status.as_u16()
    );
    if is_retryable_resource_status(status) {
        ManifestRunError::Retryable(message)
    } else {
        ManifestRunError::Unsupported(message)
    }
}

fn progress_message(downloaded: u64) -> EngineMsg {
    EngineMsg::Progress {
        downloaded,
        total: None,
        uploaded: 0,
        upload_bps: 0,
    }
}

fn safe_resource_filename(filename: &str) -> bool {
    crate::file_names::is_single_component(filename) && !filename.contains('\\')
}

/// Preserve the HTTP response that caused a manifest byte-range request to fail.
///
/// Authentication/authorization responses such as 403 are distinct from an
/// origin that ignored the Range header (200) or returned a malformed 206.
/// Transient statuses retain the existing retry classification.
fn validate_byte_range_response(
    status: reqwest::StatusCode,
    content_range: Option<&str>,
    range: ManifestByteRange,
    resource_description: &str,
) -> Result<(), ManifestRunError> {
    let last_byte = range
        .offset
        .checked_add(range.length)
        .and_then(|end| end.checked_sub(1))
        .ok_or_else(|| {
            ManifestRunError::Unsupported("The manifest contains an invalid byte range".to_string())
        })?;
    let requested = format!("bytes {}-{last_byte}", range.offset);

    if is_retryable_resource_status(status) {
        return Err(ManifestRunError::Retryable(format!(
            "The stream server returned HTTP {} for {resource_description} ({requested})",
            status.as_u16()
        )));
    }
    if status != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(ManifestRunError::Unsupported(format!(
            "The stream server returned HTTP {} for {resource_description}; expected HTTP 206 for {requested}",
            status.as_u16()
        )));
    }
    if !content_range_matches(content_range, range) {
        let detail = if content_range.is_none() {
            "a missing"
        } else {
            "an invalid or mismatched"
        };
        return Err(ManifestRunError::Unsupported(format!(
            "The stream server returned HTTP 206 with {detail} Content-Range header for {resource_description} ({requested})"
        )));
    }
    Ok(())
}

fn content_range_matches(header: Option<&str>, range: ManifestByteRange) -> bool {
    let Some(ParsedContentRange::Satisfied {
        first_byte,
        last_byte,
        complete_length: Some(total),
    }) = header.and_then(parse_content_range)
    else {
        return false;
    };

    range
        .offset
        .checked_add(range.length)
        .is_some_and(|exclusive_end| {
            first_byte == range.offset && last_byte == exclusive_end - 1 && total >= exclusive_end
        })
}

fn manifest_output_container(path: &Path) -> Result<ManifestContainer, ManifestRunError> {
    ManifestContainer::from_path(path).ok_or_else(|| {
        ManifestRunError::Failed(
            "Native HLS/DASH output must use an MP4, MKV, or TS filename".to_string(),
        )
    })
}

const DASH_LIVE_STATE_NAME: &str = "live-dash-state.json";
const DASH_LIVE_STATE_VERSION: u32 = 1;

struct ManifestDashTrackTransfer<'a> {
    client: &'a reqwest::Client,
    staging: &'a Path,
    plan: &'a DashRepresentationPlan,
    directory: &'a Path,
    filename: &'a str,
    destination: &'a Path,
    item_id: u64,
    track: TrackKind,
    connections: usize,
    tx: &'a tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    downloaded: &'a Arc<AtomicU64>,
    abort: &'a mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
}

async fn fetch_and_write_dash_track(
    context: ManifestDashTrackTransfer<'_>,
) -> Result<bool, ManifestRunError> {
    let ManifestDashTrackTransfer {
        client,
        staging,
        plan,
        directory,
        filename,
        destination,
        item_id,
        track,
        connections,
        tx,
        downloaded,
        abort,
    } = context;
    let resources = dash_resources(plan);
    fetch_and_write_ordered_track(ManifestOrderedTrackTransfer {
        client,
        staging,
        resources,
        directory,
        filename,
        destination,
        item_id,
        track,
        connections,
        tx,
        downloaded,
        abort,
    })
    .await
}

struct ManifestHlsTrackTransfer<'a> {
    client: &'a reqwest::Client,
    staging: &'a Path,
    plan: &'a HlsPlan,
    directory: &'a Path,
    filename: &'a str,
    destination: &'a Path,
    item_id: u64,
    track: TrackKind,
    tx: &'a tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    downloaded: &'a Arc<AtomicU64>,
    abort: &'a mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
}

async fn fetch_and_write_hls_track(
    context: ManifestHlsTrackTransfer<'_>,
) -> Result<bool, ManifestRunError> {
    let ManifestHlsTrackTransfer {
        client,
        staging,
        plan,
        directory,
        filename,
        destination,
        item_id,
        track,
        tx,
        downloaded,
        abort,
    } = context;
    let resources = hls_resources(plan);
    let resource_names = resources
        .iter()
        .map(|resource| resource.filename.clone())
        .collect::<Vec<_>>();
    // Stage resources in playlist order. FFmpeg validates the complete track
    // with its initialization context during finalization.
    let (writer, committed_bytes) =
        crate::manifest_writer::OrderedTrackWriter::open_destination_part(
            resource_names,
            directory,
            staging,
            filename,
            destination,
            item_id,
            track,
        )
        .await
        .map_err(ManifestRunError::Failed)?;
    if committed_bytes > 0 {
        let total = reserve_download_bytes(downloaded, committed_bytes)
            .map_err(ManifestRunError::Failed)?;
        tx.send(progress_message(total)).ok();
    }
    let Some(mut writer) = writer else {
        return Ok(true);
    };

    for resource in resources.into_iter().skip(writer.next_resource_index()) {
        match fetch_hls_resource_to_writer(client, &resource, &mut writer, tx, downloaded, abort)
            .await
        {
            Ok(true) => {}
            Ok(false) => return Ok(false),
            Err(error) => {
                if matches!(&error, ManifestRunError::Unsupported(_)) {
                    drop(writer);
                    crate::manifest_writer::cleanup_destination_part(
                        staging,
                        directory,
                        destination,
                        item_id,
                        track,
                        filename,
                    )
                    .await
                    .map_err(ManifestRunError::Failed)?;
                }
                return Err(error);
            }
        }
    }
    writer.finish().await.map_err(ManifestRunError::Failed)?;
    Ok(true)
}

/// Fetch exactly one static HLS resource into the owned destination part.
async fn fetch_hls_resource_to_writer(
    client: &reqwest::Client,
    resource: &Resource,
    writer: &mut crate::manifest_writer::OrderedTrackWriter,
    tx: &tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    downloaded: &AtomicU64,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
) -> Result<bool, ManifestRunError> {
    let mut reservation = ResourceByteReservation::new(downloaded);
    let result = async {
        if resource.key {
            return Err(ManifestRunError::Unsupported(
                "The selected HLS track contains an encrypted resource".to_string(),
            ));
        }

        let mut request = client.get(resource.url.clone());
        if let Some(range) = resource.range {
            if range.length == 0 || range.length > MAX_RESOURCE_BYTES {
                return Err(ManifestRunError::Unsupported(
                    "An HLS byte range is outside the accepted size limit".to_string(),
                ));
            }
            let end = range
                .offset
                .checked_add(range.length)
                .and_then(|exclusive_end| exclusive_end.checked_sub(1))
                .ok_or_else(|| {
                    ManifestRunError::Unsupported("The HLS byte range is invalid".to_string())
                })?;
            request = request.header(
                reqwest::header::RANGE,
                format!("bytes={}-{end}", range.offset),
            );
        }

        let response = tokio::select! {
            biased;
            _ = &mut *abort => return Ok(false),
            response = tokio::time::timeout(RESOURCE_IDLE_TIMEOUT, request.send()) => {
                response
                    .map_err(|_| ManifestRunError::Retryable(
                        "Timed out while requesting an HLS fragment".to_string(),
                    ))?
                    .map_err(|error| ManifestRunError::Retryable(format!(
                        "Could not retrieve an HLS fragment: {}",
                        error.without_url()
                    )))?
            }
        };
        let status = response.status();
        if let Some(range) = resource.range {
            validate_byte_range_response(
                status,
                response
                    .headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|value| value.to_str().ok()),
                range,
                "an HLS byte range",
            )?;
        } else if status == reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(ManifestRunError::Failed(
                "The stream server returned partial content for a complete HLS fragment"
                    .to_string(),
            ));
        } else if !status.is_success() {
            return Err(match resource_status_error(status) {
                ManifestRunError::Unsupported(message) => ManifestRunError::Failed(message),
                other => other,
            });
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESOURCE_BYTES)
        {
            return Err(ManifestRunError::Failed(
                "An HLS fragment exceeds the 1 GiB limit".to_string(),
            ));
        }
        if resource.range.is_some_and(|range| {
            byte_range_content_length_exceeds(range, response.content_length())
        }) {
            return Err(ManifestRunError::Unsupported(
                "The stream server returned more bytes than the requested HLS byte range"
                    .to_string(),
            ));
        }

        let mut stream = response.bytes_stream();
        let mut size = 0_u64;
        let mut response_prefix = Vec::with_capacity(hls_live::LIVE_HLS_RESPONSE_PREFIX_BYTES);
        let mut response_prefix_checked = false;
        let pace_start = Instant::now();
        let mut paced = 0_u64;
        let mut last_progress = Instant::now();
        loop {
            let next = tokio::select! {
                biased;
                _ = &mut *abort => return Ok(false),
                next = tokio::time::timeout(RESOURCE_IDLE_TIMEOUT, stream.next()) => {
                    next.map_err(|_| ManifestRunError::Retryable(
                        "An HLS fragment stopped making progress".to_string(),
                    ))?
                }
            };
            let Some(chunk) = next else {
                break;
            };
            let chunk = chunk.map_err(|error| {
                ManifestRunError::Retryable(format!(
                    "An HLS fragment transfer failed: {}",
                    error.without_url()
                ))
            })?;
            let chunk_len = u64::try_from(chunk.len()).map_err(|_| {
                ManifestRunError::Failed("An HLS fragment exceeds the size limit".to_string())
            })?;
            if resource
                .range
                .is_some_and(|range| byte_range_chunk_exceeds(range, size, chunk.len()))
            {
                return Err(ManifestRunError::Unsupported(
                    "The stream server returned more bytes than the requested HLS byte range"
                        .to_string(),
                ));
            }
            size = size
                .checked_add(chunk_len)
                .filter(|size| *size <= MAX_RESOURCE_BYTES)
                .ok_or_else(|| {
                    ManifestRunError::Failed("An HLS fragment exceeds the 1 GiB limit".to_string())
                })?;
            if response_prefix_checked {
                writer
                    .write_streamed_chunk(&chunk)
                    .await
                    .map_err(ManifestRunError::Failed)?;
            } else {
                let prefix_bytes = (hls_live::LIVE_HLS_RESPONSE_PREFIX_BYTES
                    - response_prefix.len())
                .min(chunk.len());
                response_prefix.extend_from_slice(&chunk[..prefix_bytes]);
                if response_prefix.len() == hls_live::LIVE_HLS_RESPONSE_PREFIX_BYTES {
                    if hls_live::is_obvious_text_error_body(&response_prefix) {
                        return Err(ManifestRunError::Unsupported(
                            "An HLS fragment response contains a text error body".to_string(),
                        ));
                    }
                    writer
                        .write_streamed_chunk(&response_prefix)
                        .await
                        .map_err(ManifestRunError::Failed)?;
                    response_prefix.clear();
                    response_prefix_checked = true;
                    if chunk.len() > prefix_bytes {
                        writer
                            .write_streamed_chunk(&chunk[prefix_bytes..])
                            .await
                            .map_err(ManifestRunError::Failed)?;
                    }
                }
            }
            let total = reservation
                .reserve(chunk_len)
                .map_err(ManifestRunError::Failed)?;
            if last_progress.elapsed() >= Duration::from_millis(100) {
                tx.send(progress_message(total)).ok();
                last_progress = Instant::now();
            }
            crate::download_rate::pace_chunk(
                &mut paced,
                pace_start,
                crate::download_rate::live_rate_limit(),
                chunk.len(),
            )
            .await;
        }

        if !response_prefix_checked && !response_prefix.is_empty() {
            if hls_live::is_obvious_text_error_body(&response_prefix) {
                return Err(ManifestRunError::Unsupported(
                    "An HLS fragment response contains a text error body".to_string(),
                ));
            }
            writer
                .write_streamed_chunk(&response_prefix)
                .await
                .map_err(ManifestRunError::Failed)?;
        }
        if size == 0 {
            return Err(ManifestRunError::Failed(
                "The stream server returned an empty HLS fragment".to_string(),
            ));
        }
        if resource.range.is_some_and(|range| size != range.length) {
            return Err(ManifestRunError::Unsupported(
                "The stream server returned an incomplete HLS byte range".to_string(),
            ));
        }
        Ok(true)
    }
    .await;

    match result {
        Ok(true) => {
            if let Err(error) = writer.commit_streamed_resource().await {
                // Checkpoint publication may have completed before a directory-sync
                // error was reported. Leave the bytes intact so resume can reconcile
                // the journal and checkpoint instead of truncating a committed prefix.
                drop(reservation);
                tx.send(progress_message(downloaded.load(Ordering::Relaxed)))
                    .ok();
                return Err(ManifestRunError::Failed(error));
            }
            reservation.commit();
            tx.send(progress_message(downloaded.load(Ordering::Relaxed)))
                .ok();
            Ok(true)
        }
        outcome => {
            writer
                .rollback_streamed_resource()
                .await
                .map_err(ManifestRunError::Failed)?;
            drop(reservation);
            tx.send(progress_message(downloaded.load(Ordering::Relaxed)))
                .ok();
            outcome
        }
    }
}
struct ManifestOrderedTrackTransfer<'a> {
    client: &'a reqwest::Client,
    staging: &'a Path,
    resources: Vec<Resource>,
    directory: &'a Path,
    filename: &'a str,
    destination: &'a Path,
    item_id: u64,
    track: TrackKind,
    connections: usize,
    tx: &'a tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    downloaded: &'a Arc<AtomicU64>,
    abort: &'a mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
}

async fn fetch_and_write_ordered_track(
    context: ManifestOrderedTrackTransfer<'_>,
) -> Result<bool, ManifestRunError> {
    let ManifestOrderedTrackTransfer {
        client,
        staging,
        resources,
        directory,
        filename,
        destination,
        item_id,
        track,
        connections,
        tx,
        downloaded,
        abort,
    } = context;
    let resource_names = resources
        .iter()
        .map(|resource| resource.filename.clone())
        .collect::<Vec<_>>();
    let (writer, committed_bytes) =
        crate::manifest_writer::OrderedTrackWriter::open_destination_part(
            resource_names.clone(),
            directory,
            staging,
            filename,
            destination,
            item_id,
            track,
        )
        .await?;
    if committed_bytes > 0 {
        let total = reserve_download_bytes(downloaded, committed_bytes)?;
        tx.send(progress_message(total)).ok();
    }
    let Some(mut writer) = writer else {
        return Ok(true);
    };

    // Each bounded window fetches out of order. Completed indices wait in a
    // small reorder set until the single writer can commit the next index.
    let window_size = connections.clamp(1, MAX_RESOURCE_CONNECTIONS);
    let resource_count = resources.len();
    let mut next_resource_index = writer.next_resource_index();
    let mut remaining_resources = resources.into_iter().enumerate().skip(next_resource_index);
    loop {
        let window = remaining_resources
            .by_ref()
            .take(window_size)
            .collect::<Vec<_>>();
        if window.is_empty() {
            break;
        }
        let window_end = next_resource_index + window.len();
        let mut completed = std::collections::BTreeSet::new();
        let fetches = futures_util::stream::iter(window.into_iter().map(|(index, resource)| {
            let client = client.clone();
            let directory = directory.to_path_buf();
            let tx = tx.clone();
            let downloaded = Arc::clone(downloaded);
            async move {
                fetch_resource(&client, resource, &directory, &tx, &downloaded)
                    .await
                    .map(|_| index)
            }
        }))
        .buffer_unordered(window_size);

        let window_outcome = {
            tokio::pin!(fetches);
            'window: loop {
                tokio::select! {
                    biased;
                    _ = &mut *abort => break Ok(false),
                    result = fetches.next() => match result {
                        Some(Ok(index)) => {
                            completed.insert(index);
                            while completed.remove(&next_resource_index) {
                                match writer.append_next(abort).await {
                                    Ok(true) => next_resource_index += 1,
                                    Ok(false) => break 'window Ok(false),
                                    Err(error) => {
                                        break 'window Err(ManifestRunError::Failed(error));
                                    }
                                }
                            }
                        }
                        Some(Err(error)) => break Err(error),
                        None => break Ok(true),
                    }
                }
            }
        };
        match window_outcome {
            Ok(true) => {}
            Ok(false) => break,
            Err(error) => {
                tx.send(progress_message(downloaded.load(Ordering::Relaxed)))
                    .ok();
                if matches!(&error, ManifestRunError::Unsupported(_)) {
                    drop(writer);
                    crate::manifest_writer::cleanup_destination_part(
                        staging,
                        directory,
                        destination,
                        item_id,
                        track,
                        filename,
                    )
                    .await
                    .map_err(ManifestRunError::Failed)?;
                }
                return Err(error);
            }
        }
        if next_resource_index != window_end {
            tx.send(progress_message(downloaded.load(Ordering::Relaxed)))
                .ok();
            return Err(ManifestRunError::Failed(
                "The ordered DASH fetch window ended before its resources were committed"
                    .to_string(),
            ));
        }
    }

    // Dropping each bounded window cancels outstanding requests and refunds
    // partial byte reservations before publishing the corrected progress.
    tx.send(progress_message(downloaded.load(Ordering::Relaxed)))
        .ok();
    if next_resource_index != resource_count {
        return Ok(false);
    }
    writer.finish().await?;
    Ok(true)
}

#[cfg(test)]
async fn concat_dash_track(
    plan: &DashRepresentationPlan,
    directory: &Path,
    filename: &str,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
) -> Result<bool, String> {
    let resources = plan
        .resources
        .iter()
        .map(|resource| resource.filename.clone())
        .collect::<Vec<_>>();
    crate::manifest_writer::write_ordered_track(&resources, directory, filename, abort).await
}

#[cfg(test)]
async fn concat_dash_resource_names(
    resources: &[String],
    directory: &Path,
    filename: &str,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
) -> Result<bool, String> {
    let (writer, _) = crate::manifest_writer::OrderedTrackWriter::open_retaining_checkpoint(
        resources.to_vec(),
        directory,
        filename,
    )
    .await?;
    if let Some(mut writer) = writer {
        while writer.next_resource_index() < resources.len() {
            if !writer.append_next(abort).await? {
                return Ok(false);
            }
        }
        writer.finish().await?;
    } else {
        // A previously published track may have leftover sources from a
        // crash after the rename but before the idempotent cleanup completed.
        crate::manifest_writer::cleanup_committed_resources(resources, directory, filename).await?;
    }
    Ok(true)
}

async fn remove_regular_file_if_present(path: &Path) -> Result<(), String> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err("A manifest staging path is not a regular file".to_string())
        }
        Ok(_) => tokio::fs::remove_file(path)
            .await
            .map_err(|error| format!("Could not clear an incomplete staging file: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Could not inspect manifest staging: {error}")),
    }
}

fn is_hls_playlist_input(input: &Path) -> bool {
    input
        .extension()
        .is_some_and(|extension| extension == std::ffi::OsStr::new("m3u8"))
}

fn add_hls_staging_options(command: &mut tokio::process::Command) {
    // Every HLS child URI is rewritten to a generated, single-component
    // .bin staging name. Permit .m3u8 for FFmpeg to reopen the growing local
    // playlist, but keep actual segment/resource extensions restricted to .bin.
    command.args([
        "-protocol_whitelist",
        "file,crypto",
        "-allowed_extensions",
        "m3u8,bin",
        "-allowed_segment_extensions",
        "bin",
        "-extension_picky",
        "0",
    ]);
}

fn add_hls_incremental_reload_options(command: &mut tokio::process::Command) {
    // The local playlist intentionally remains open until Grab has fetched every
    // segment. Do not let FFmpeg's default reload counters mistake a slow fetch
    // for the end of the presentation.
    command
        .arg("-live_start_index")
        .arg("0")
        .arg("-max_reload")
        .arg(i32::MAX.to_string())
        .arg("-m3u8_hold_counters")
        .arg(i32::MAX.to_string());
}

#[derive(Clone, Copy, Default)]
struct SubtitleOutput<'a> {
    input: Option<&'a Path>,
    embed: bool,
    sidecar: Option<&'a Path>,
    format: SubtitleFormat,
}

#[derive(Clone, Copy)]
struct FfmpegInput<'a> {
    path: &'a Path,
    timestamp_offset: Option<&'a str>,
}

#[cfg(test)]
async fn run_ffmpeg(
    ffmpeg: &Path,
    video_input: &Path,
    audio_input: Option<&Path>,
    subtitles: SubtitleOutput<'_>,
    output: &Path,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
) -> Result<bool, ManifestRunError> {
    Ok(matches!(
        run_ffmpeg_with_offsets(
            ffmpeg,
            FfmpegInput {
                path: video_input,
                timestamp_offset: None,
            },
            audio_input.map(|path| FfmpegInput {
                path,
                timestamp_offset: None,
            }),
            InputTimestampPolicy::Normalize,
            subtitles,
            output,
            abort,
        )
        .await?,
        ManifestMuxOutcome::Finished
    ))
}

#[derive(Clone, Copy)]
struct ManifestMuxOptions {
    timestamp_policy: InputTimestampPolicy,
    timeout: Option<Duration>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InputTimestampPolicy {
    /// Preserve the shared source PTS clock across separate HLS renditions.
    Preserve,
    /// Let FFmpeg apply its normal output timestamp handling.
    Normalize,
}

fn mpegts_embedded_subtitle_error() -> ManifestRunError {
    ManifestRunError::Failed(
        "MPEG-TS output cannot embed the selected subtitle track. Disable subtitle embedding to write an SRT sidecar, or choose MP4/MKV output."
            .to_string(),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManifestMuxOutcome {
    Finished,
    Paused,
    Aborted,
}

async fn stop_ffmpeg_for_intent(
    child: &mut tokio::process::Child,
    stderr_task: tokio::task::JoinHandle<String>,
    outcome: ManifestMuxOutcome,
) -> ManifestMuxOutcome {
    let _ = child.kill().await;
    let _ = child.wait().await;
    let _ = stderr_task.await;
    outcome
}

async fn run_ffmpeg_with_offsets(
    ffmpeg: &Path,
    video_input: FfmpegInput<'_>,
    audio_input: Option<FfmpegInput<'_>>,
    timestamp_policy: InputTimestampPolicy,
    subtitles: SubtitleOutput<'_>,
    output: &Path,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
) -> Result<ManifestMuxOutcome, ManifestRunError> {
    run_ffmpeg_with_offsets_timeout(
        ffmpeg,
        video_input,
        audio_input,
        ManifestMuxOptions {
            timestamp_policy,
            timeout: Some(MANIFEST_MUX_TIMEOUT),
        },
        subtitles,
        FfmpegOutput::StagedPath(output.to_path_buf()),
        abort,
    )
    .await
}

async fn wait_for_manifest_mux(
    child: &mut tokio::process::Child,
    timeout: Option<Duration>,
) -> Result<std::process::ExitStatus, ManifestRunError> {
    let status = match timeout {
        Some(timeout) => match tokio::time::timeout(timeout, child.wait()).await {
            Ok(status) => status,
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(ManifestRunError::Failed(format!(
                    "FFmpeg finalization exceeded its {timeout:?} time limit"
                )));
            }
        },
        None => child.wait().await,
    };
    match status {
        Ok(status) => Ok(status),
        Err(error) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(ManifestRunError::Failed(format!(
                "FFmpeg could not finish: {error}"
            )))
        }
    }
}

enum FfmpegOutput {
    StagedPath(PathBuf),
    OwnedPart {
        file: std::fs::File,
        container: ManifestContainer,
        label: PathBuf,
    },
}

async fn run_ffmpeg_with_offsets_timeout(
    ffmpeg: &Path,
    video_input: FfmpegInput<'_>,
    audio_input: Option<FfmpegInput<'_>>,
    options: ManifestMuxOptions,
    subtitles: SubtitleOutput<'_>,
    output: FfmpegOutput,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
) -> Result<ManifestMuxOutcome, ManifestRunError> {
    let (output_argument, output_container, output_label, overwrite_output, stdout) = match output {
        FfmpegOutput::StagedPath(path) => {
            let container = manifest_output_container(&path)?;
            (
                path.clone(),
                container,
                path,
                false,
                std::process::Stdio::null(),
            )
        }
        FfmpegOutput::OwnedPart {
            file,
            container,
            label,
        } => (
            // stdout is an inherited seekable owned file descriptor. FFmpeg
            // reopens it through procfs so MP4 +faststart and Matroska can seek;
            // pipe:1 would be non-seekable. This relies on Linux procfs being
            // mounted inside the Flatpak/runtime.
            PathBuf::from("/proc/self/fd/1"),
            container,
            label,
            true,
            std::process::Stdio::from(file),
        ),
    };
    if output_container == ManifestContainer::Ts && subtitles.input.is_some() && subtitles.embed {
        return Err(mpegts_embedded_subtitle_error());
    }

    tracing::debug!(
        output = %output_label.display(),
        output_container = ?output_container,
        "starting FFmpeg manifest mux"
    );
    let mut command = tokio::process::Command::new(ffmpeg);
    if is_hls_playlist_input(video_input.path) {
        add_hls_staging_options(&mut command);
        if overwrite_output {
            add_hls_incremental_reload_options(&mut command);
        }
    }
    command
        .arg("-hide_banner")
        .arg("-nostdin")
        .arg(if overwrite_output { "-y" } else { "-n" });
    if options.timestamp_policy == InputTimestampPolicy::Preserve {
        command.arg("-copyts");
    }
    if let Some(offset) = video_input.timestamp_offset {
        command.arg("-itsoffset").arg(offset);
    }
    command.arg("-i").arg(video_input.path);
    // HLS CEA-608/708 captions are in-band video data. The video stream-copy
    // below preserves them; there is no separate subtitle playlist to fetch.
    if let Some(audio_input) = audio_input {
        if is_hls_playlist_input(audio_input.path) {
            add_hls_staging_options(&mut command);
            if overwrite_output {
                add_hls_incremental_reload_options(&mut command);
            }
        }
        if let Some(offset) = audio_input.timestamp_offset {
            command.arg("-itsoffset").arg(offset);
        }
        command.arg("-i").arg(audio_input.path);
        command.arg("-map").arg("0:v:0?").arg("-map").arg("1:a:0?");
    } else {
        command.arg("-map").arg("0:v:0?").arg("-map").arg("0:a:0?");
    }
    if let Some(subtitle_input) = subtitles.input {
        if is_hls_playlist_input(subtitle_input) {
            add_hls_staging_options(&mut command);
        }
        command.arg("-i").arg(subtitle_input);
    }
    command.args(["-c", "copy"]);
    if overwrite_output {
        // The destination part is consumed by the UI while FFmpeg is still
        // processing a growing playlist, so flush muxed packets to the file.
        command.args(["-flush_packets", "1"]);
    }
    if subtitles.input.is_some() && subtitles.embed {
        let subtitle_stream = if audio_input.is_some() {
            "2:s:0"
        } else {
            "1:s:0"
        };
        let subtitle_codec = match output_container {
            ManifestContainer::Mp4 if subtitles.format == SubtitleFormat::Fmp4 => "copy",
            ManifestContainer::Mp4 => "mov_text",
            // Matroska preserves text WebVTT codecs. fMP4 subtitle inputs
            // are routed to an MP4 sidecar by the embedding policy.
            ManifestContainer::Mkv if subtitles.format != SubtitleFormat::Fmp4 => "copy",
            ManifestContainer::Mkv | ManifestContainer::Ts => {
                return Err(mpegts_embedded_subtitle_error());
            }
        };
        command
            .arg("-map")
            .arg(subtitle_stream)
            .args(["-c:s", subtitle_codec]);
    }
    match output_container {
        ManifestContainer::Mp4 => {
            command.args(["-movflags", "+faststart", "-f", "mp4"]);
        }
        ManifestContainer::Mkv => {
            command.args(["-f", "matroska"]);
        }
        ManifestContainer::Ts => {
            command.args(["-f", "mpegts"]);
        }
    }
    command.arg(&output_argument);
    if subtitles.input.is_some() && !subtitles.embed {
        let sidecar = subtitles.sidecar.ok_or_else(|| {
            ManifestRunError::Failed("Subtitle sidecar destination is missing".to_string())
        })?;
        command.arg("-map").arg(if audio_input.is_some() {
            "2:s:0"
        } else {
            "1:s:0"
        });
        if subtitles.format == SubtitleFormat::Fmp4 {
            // Preserve the source subtitle codec through stream-copy.
            command
                .args(["-c:s", "copy", "-movflags", "+faststart", "-f", "mp4"])
                .arg(sidecar);
        } else {
            command.args(["-c:s", "srt", "-f", "srt"]).arg(sidecar);
        }
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(stdout)
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| ManifestRunError::Failed(format!("Could not start FFmpeg: {error}")))?;
    let stderr = child.stderr.take();
    let stderr_task = tokio::spawn(async move {
        let Some(stderr) = stderr else {
            return String::new();
        };
        let mut reader = tokio::io::BufReader::new(stderr).lines();
        let mut tail = std::collections::VecDeque::with_capacity(FFMPEG_LOG_LINES);
        while let Ok(Some(line)) = reader.next_line().await {
            if tail.len() == FFMPEG_LOG_LINES {
                tail.pop_front();
            }
            tail.push_back(line);
        }
        tail.into_iter().collect::<Vec<_>>().join("\n")
    });
    let status = tokio::select! {
        biased;
        stop = &mut *abort => match crate::video::stop_intent_or_pause(stop) {
            crate::video::StopIntent::Finish => {
                wait_for_manifest_mux(&mut child, options.timeout).await
            },
            crate::video::StopIntent::Pause => {
                return Ok(stop_ffmpeg_for_intent(
                    &mut child,
                    stderr_task,
                    ManifestMuxOutcome::Paused,
                )
                .await);
            }
            crate::video::StopIntent::Discard => {
                return Ok(stop_ffmpeg_for_intent(
                    &mut child,
                    stderr_task,
                    ManifestMuxOutcome::Aborted,
                )
                .await);
            }
        },
        status = wait_for_manifest_mux(&mut child, options.timeout) => status,
    };
    let status = match status {
        Ok(status) => status,
        Err(error) => {
            stderr_task.abort();
            let _ = stderr_task.await;
            return Err(error);
        }
    };
    let log = stderr_task.await.unwrap_or_default();
    if !status.success() {
        if subtitles.input.is_some() && subtitle_mux_unsupported(&log) {
            return Err(ManifestRunError::Unsupported(
                "The selected subtitle format is not supported by the native finalizer".to_string(),
            ));
        }
        return Err(mux_failure_error(&log));
    }
    Ok(ManifestMuxOutcome::Finished)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn fake_ffmpeg(directory: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;

        let path = directory.join("fake-ffmpeg");
        std::fs::write(&path, "#!/bin/sh\nexec sleep 0.1\n").expect("write fake FFmpeg executable");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("make fake FFmpeg executable");
        path
    }

    #[cfg(unix)]
    fn fake_ffmpeg_recording_arguments(directory: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;

        let path = directory.join("fake-ffmpeg-args");
        std::fs::write(
            &path,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.args\"\nexit 0\n",
        )
        .expect("write argument-recording fake FFmpeg executable");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("make argument-recording FFmpeg executable");
        path
    }

    #[cfg(unix)]
    async fn run_ffmpeg_recording_args(
        ffmpeg: &Path,
        output: &Path,
        subtitles: SubtitleOutput<'_>,
    ) -> Vec<String> {
        // Finish waits for the fake child to exit, making its captured args deterministic.
        let (sender, mut abort) = tokio::sync::oneshot::channel();
        sender
            .send(crate::video::StopIntent::Finish)
            .expect("queue FFmpeg Finish intent");
        run_ffmpeg_with_offsets(
            ffmpeg,
            FfmpegInput {
                path: Path::new("video.m3u8"),
                timestamp_offset: None,
            },
            None,
            InputTimestampPolicy::Normalize,
            subtitles,
            output,
            &mut abort,
        )
        .await
        .expect("recording FFmpeg invocation should complete");
        std::fs::read_to_string(ffmpeg.with_extension("args"))
            .expect("read recorded FFmpeg arguments")
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[cfg(unix)]
    fn fake_ffmpeg_that_hangs(directory: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;

        let path = directory.join("fake-ffmpeg-hang");
        std::fs::write(&path, "#!/bin/sh\nexec sleep 30\n")
            .expect("write hanging fake FFmpeg executable");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("make hanging fake FFmpeg executable");
        path
    }

    #[cfg(unix)]
    async fn mux_with_stop_intent(intent: Option<crate::video::StopIntent>) -> ManifestMuxOutcome {
        let directory = tempfile::tempdir().expect("create fake FFmpeg directory");
        let ffmpeg = fake_ffmpeg(directory.path());
        let output = directory.path().join("output.mp4");
        let (sender, mut abort) = tokio::sync::oneshot::channel();
        match intent {
            Some(intent) => sender.send(intent).expect("queue FFmpeg stop intent"),
            None => drop(sender),
        }

        tokio::time::timeout(
            Duration::from_secs(3),
            run_ffmpeg_with_offsets(
                &ffmpeg,
                FfmpegInput {
                    path: Path::new("unused-video-input"),
                    timestamp_offset: None,
                },
                None,
                InputTimestampPolicy::Normalize,
                SubtitleOutput::default(),
                &output,
                &mut abort,
            ),
        )
        .await
        .expect("fake FFmpeg must stop within the test deadline")
        .expect("fake FFmpeg process should be handled")
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ffmpeg_hls_staging_allows_local_playlist_reload_and_limits_child_segments() {
        let directory = tempfile::tempdir().expect("create FFmpeg argument test directory");
        let ffmpeg = fake_ffmpeg_recording_arguments(directory.path());
        let output = directory.path().join("output.mp4");
        let args = run_ffmpeg_recording_args(&ffmpeg, &output, SubtitleOutput::default()).await;

        assert!(
            args.windows(2)
                .any(|pair| pair == ["-allowed_extensions", "m3u8,bin"]),
            "FFmpeg must be allowed to reopen the growing local playlist: {args:?}"
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-allowed_segment_extensions", "bin"]),
            "HLS child resources must remain restricted to generated .bin files: {args:?}"
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-protocol_whitelist", "file,crypto"]),
            "the HLS local mux must not gain network protocol access: {args:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hls_subtitle_mux_preserves_fmp4_and_converts_webvtt() {
        let directory = tempfile::tempdir().expect("create FFmpeg argument test directory");
        let ffmpeg = fake_ffmpeg_recording_arguments(directory.path());
        let output = directory.path().join("output.mp4");
        let sidecar = directory.path().join(format!(
            "output.en.{}",
            crate::video_staging::FMP4_SUBTITLE_SIDECAR_EXTENSION
        ));

        let embedded_fmp4 = run_ffmpeg_recording_args(
            &ffmpeg,
            &output,
            SubtitleOutput {
                input: Some(Path::new("subtitles.m3u8")),
                embed: true,
                sidecar: None,
                format: SubtitleFormat::Fmp4,
            },
        )
        .await;
        assert!(
            embedded_fmp4
                .windows(2)
                .any(|pair| pair == ["-c:s", "copy"]),
            "fMP4 subtitles must be stream-copied into MP4: {embedded_fmp4:?}"
        );
        assert!(
            !embedded_fmp4
                .windows(2)
                .any(|pair| pair == ["-c:s", "mov_text"]),
            "fMP4 subtitles must not be converted to mov_text"
        );

        let preserved_sidecar = run_ffmpeg_recording_args(
            &ffmpeg,
            &output,
            SubtitleOutput {
                input: Some(Path::new("subtitles.m3u8")),
                embed: false,
                sidecar: Some(&sidecar),
                format: SubtitleFormat::Fmp4,
            },
        )
        .await;
        assert!(
            preserved_sidecar
                .windows(2)
                .any(|pair| pair == ["-c:s", "copy"]),
            "fMP4 sidecars must retain the source codec: {preserved_sidecar:?}"
        );
        assert!(
            preserved_sidecar
                .windows(4)
                .any(|args| args == ["-movflags", "+faststart", "-f", "mp4"]),
            "fMP4 subtitle sidecars must use an MP4 container: {preserved_sidecar:?}"
        );
        assert!(
            preserved_sidecar
                .iter()
                .any(|argument| argument == sidecar.to_string_lossy().as_ref()),
            "the fMP4 sidecar destination must be passed to FFmpeg"
        );

        let webvtt = run_ffmpeg_recording_args(
            &ffmpeg,
            &output,
            SubtitleOutput {
                input: Some(Path::new("subtitles.m3u8")),
                embed: true,
                sidecar: None,
                format: SubtitleFormat::Text,
            },
        )
        .await;
        assert!(
            webvtt.windows(2).any(|pair| pair == ["-c:s", "mov_text"]),
            "text WebVTT must retain the MP4 mov_text conversion path: {webvtt:?}"
        );

        let in_band_captions =
            run_ffmpeg_recording_args(&ffmpeg, &output, SubtitleOutput::default()).await;
        assert!(
            in_band_captions
                .windows(2)
                .any(|pair| pair == ["-map", "0:v:0?"]),
            "CEA-608/708 remain inside the copied HLS video stream: {in_band_captions:?}"
        );
        assert!(
            in_band_captions
                .windows(2)
                .any(|pair| pair == ["-c", "copy"]),
            "in-band caption user-data must not be transcoded away: {in_band_captions:?}"
        );
        assert!(
            !in_band_captions
                .iter()
                .any(|argument| argument.contains(":s:")),
            "in-band captions must not be treated as a separate subtitle playlist"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ffmpeg_finish_intent_allows_mux_to_complete() {
        assert_eq!(
            mux_with_stop_intent(Some(crate::video::StopIntent::Finish)).await,
            ManifestMuxOutcome::Finished
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn ffmpeg_owned_part_output_uses_the_inherited_file_descriptor() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().expect("create owned output directory");
        let ffmpeg = directory.path().join("fake-ffmpeg-owned-output");
        std::fs::write(
            &ffmpeg,
            "#!/bin/sh\nsaw_y=0\nlast=\nfor arg in \"$@\"; do\n  [ \"$arg\" = \"-y\" ] && saw_y=1\n  last=\"$arg\"\ndone\n[ \"$saw_y\" = \"1\" ] || exit 41\n[ \"$last\" = \"/proc/self/fd/1\" ] || exit 42\nprintf 'decrypted-output' > \"$last\"\n",
        )
        .expect("write fake FFmpeg executable");
        std::fs::set_permissions(&ffmpeg, std::fs::Permissions::from_mode(0o700))
            .expect("make fake FFmpeg executable");
        let output = directory.path().join("video.part");
        let file = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&output)
            .expect("create owned destination part");
        let (_sender, mut abort) = tokio::sync::oneshot::channel();

        let result = run_ffmpeg_with_offsets_timeout(
            &ffmpeg,
            FfmpegInput {
                path: Path::new("unused-video-input"),
                timestamp_offset: None,
            },
            None,
            ManifestMuxOptions {
                timestamp_policy: InputTimestampPolicy::Normalize,
                timeout: Some(Duration::from_secs(3)),
            },
            SubtitleOutput::default(),
            FfmpegOutput::OwnedPart {
                file,
                container: ManifestContainer::Mp4,
                label: output.clone(),
            },
            &mut abort,
        )
        .await
        .expect("FFmpeg should write through the owned descriptor");

        assert_eq!(result, ManifestMuxOutcome::Finished);
        assert_eq!(
            std::fs::read(&output).expect("read the owned output part"),
            b"decrypted-output"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ffmpeg_finish_intent_is_bounded_when_mux_stalls() {
        let directory = tempfile::tempdir().expect("create fake FFmpeg directory");
        let ffmpeg = fake_ffmpeg_that_hangs(directory.path());
        let output = directory.path().join("output.mp4");
        let (sender, mut abort) = tokio::sync::oneshot::channel();
        sender
            .send(crate::video::StopIntent::Finish)
            .expect("queue Finish intent");

        let result = tokio::time::timeout(
            Duration::from_secs(3),
            run_ffmpeg_with_offsets_timeout(
                &ffmpeg,
                FfmpegInput {
                    path: Path::new("unused-video-input"),
                    timestamp_offset: None,
                },
                None,
                ManifestMuxOptions {
                    timestamp_policy: InputTimestampPolicy::Normalize,
                    timeout: Some(Duration::from_millis(50)),
                },
                SubtitleOutput::default(),
                FfmpegOutput::StagedPath(output.clone()),
                &mut abort,
            ),
        )
        .await
        .expect("a stalled mux must not outlive its configured deadline");

        assert!(matches!(
            result,
            Err(ManifestRunError::Failed(message)) if message.contains("time limit")
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ffmpeg_pause_intent_stops_mux_and_preserves_staging() {
        assert_eq!(
            mux_with_stop_intent(Some(crate::video::StopIntent::Pause)).await,
            ManifestMuxOutcome::Paused
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ffmpeg_discard_intent_stops_mux() {
        assert_eq!(
            mux_with_stop_intent(Some(crate::video::StopIntent::Discard)).await,
            ManifestMuxOutcome::Aborted
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn closed_ffmpeg_stop_channel_defaults_to_pause() {
        assert_eq!(mux_with_stop_intent(None).await, ManifestMuxOutcome::Paused);
    }

    #[test]
    fn ffmpeg_input_rejection_is_eligible_for_extractor_fallback() {
        assert!(matches!(
            mux_failure_error("Error opening input: Invalid data found when processing input"),
            ManifestRunError::Unsupported(_)
        ));
        assert!(matches!(
            mux_failure_error("Could not open input: Invalid data found when processing input"),
            ManifestRunError::Unsupported(_)
        ));
    }

    #[test]
    fn ffmpeg_container_and_stream_header_rejections_are_eligible_for_fallback() {
        for log in [
            "[mov,mp4,m4a,3gp,3g2,mj2 @ 0x1] moov atom not found",
            "[matroska,webm @ 0x1] EBML header parsing failed",
            "[mov @ 0x1] error reading header: End of file",
            "[mov @ 0x1] Could not find codec parameters for stream 0",
        ] {
            assert!(
                matches!(mux_failure_error(log), ManifestRunError::Unsupported(_)),
                "input rejection should fall back: {log}"
            );
        }
    }

    #[test]
    fn ffmpeg_output_failures_remain_terminal() {
        assert!(matches!(
            mux_failure_error("Could not write header: Permission denied"),
            ManifestRunError::Failed(_)
        ));
        assert!(matches!(
            mux_failure_error("No space left on device"),
            ManifestRunError::Failed(_)
        ));
        assert!(matches!(
            mux_failure_error("Output file #0 does not contain any stream"),
            ManifestRunError::Failed(_)
        ));
    }

    #[test]
    fn vsd_adts_validation_rejects_zero_length_frames() {
        let malformed = [0xff, 0xf1, 0, 0, 0, 0, 0];
        assert!(validate_iori_adts_pes(&malformed).is_err());
    }

    #[test]
    fn vsd_adts_validation_accepts_complete_frames() {
        let frame = [0xff, 0xf1, 0, 0, 0, 0xe0, 0];
        assert!(validate_iori_adts_pes(&frame).is_ok());
        assert!(validate_iori_adts_pes(&[frame, frame].concat()).is_ok());
    }

    #[test]
    fn sample_aes_iv_uses_media_sequence_when_iv_is_omitted() {
        assert_eq!(
            hls_sample_aes_iv(None, 42).expect("derive media-sequence IV"),
            [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 42]
        );
    }

    #[test]
    fn sample_aes_iv_parses_explicit_128_bit_values() {
        assert_eq!(
            hls_sample_aes_iv(Some("0x0000000000000000000000000000002A"), 0)
                .expect("parse explicit IV"),
            [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 42]
        );
    }

    #[test]
    fn sample_aes_iv_rejects_non_hexadecimal_bytes() {
        assert!(hls_sample_aes_iv(Some("éééééééééééééééé"), 0).is_err());
    }

    fn valid_transport_stream(packet_count: usize, continuity_seed: u8) -> Vec<u8> {
        let mut bytes = vec![0xff; packet_count * 188];
        for (index, packet) in bytes.as_chunks_mut::<188>().0.iter_mut().enumerate() {
            packet[0] = 0x47;
            packet[1] = 0;
            packet[2] = 0;
            packet[3] = 0x10 | (continuity_seed.wrapping_add(index as u8) & 0x0f);
        }
        bytes
    }

    fn fmp4_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = u32::try_from(8 + payload.len()).expect("test box fits in a u32");
        let mut bytes = Vec::with_capacity(size as usize);
        bytes.extend_from_slice(&size.to_be_bytes());
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(payload);
        bytes
    }

    fn valid_fmp4_init_segment() -> Vec<u8> {
        let mut ftyp_payload = Vec::from(*b"isom");
        ftyp_payload.extend_from_slice(&0_u32.to_be_bytes());
        ftyp_payload.extend_from_slice(b"isom");
        let ftyp = fmp4_box(b"ftyp", &ftyp_payload);
        let moov = fmp4_box(b"moov", &[]);
        [ftyp, moov].concat()
    }

    fn valid_fmp4_media_segment(sequence_number: u32, media_data: &[u8]) -> Vec<u8> {
        let mut mfhd_payload = vec![0, 0, 0, 0];
        mfhd_payload.extend_from_slice(&sequence_number.to_be_bytes());
        let mfhd = fmp4_box(b"mfhd", &mfhd_payload);
        let moof = fmp4_box(b"moof", &mfhd);
        let mdat = fmp4_box(b"mdat", media_data);
        [moof, mdat].concat()
    }

    #[tokio::test]
    async fn static_hls_mpegts_fragments_grow_the_owned_destination_part_in_order() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind HLS fragment server");
        let address = listener.local_addr().expect("HLS server address");
        let mut first = valid_transport_stream(3, 0);
        let mut second = valid_transport_stream(3, 3);
        first[4] = 1;
        second[4] = 2;
        let first_response = first.clone();
        let second_response = second.clone();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.expect("accept HLS request");
                let mut request = [0_u8; 1024];
                let count = socket.read(&mut request).await.expect("read HLS request");
                let request = String::from_utf8_lossy(&request[..count]);
                let path = request.split_ascii_whitespace().nth(1).unwrap_or("/");
                let payload = if path.contains("segment-1.ts") {
                    &first_response
                } else {
                    &second_response
                };
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                );
                socket
                    .write_all(header.as_bytes())
                    .await
                    .expect("write HLS response header");
                socket
                    .write_all(payload)
                    .await
                    .expect("write HLS response body");
            }
        });

        let playlist = br#"#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:4
#EXTINF:4,
segment-1.ts
#EXTINF:4,
segment-2.ts
#EXT-X-ENDLIST
"#;
        let mut plan = crate::manifest::plan_hls_media_playlist(
            "https://cdn.example.test/playlist.m3u8",
            playlist,
        )
        .expect("plan static HLS media playlist");
        for (index, resource) in plan.resources.iter_mut().enumerate() {
            resource.url = url::Url::parse(&format!("http://{address}/segment-{}.ts", index + 1))
                .expect("set local HLS test resource URL");
        }
        assert!(plan.is_plain_mpegts_presentation());

        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build direct HLS test client");
        let staging = tempfile::tempdir().expect("create HLS staging root");
        let video_dir = staging.path().join("video");
        tokio::fs::create_dir(&video_dir)
            .await
            .expect("create HLS video track directory");
        let destination_dir = tempfile::tempdir().expect("create HLS destination");
        let destination = destination_dir.path().join("Example.mp4");
        crate::manifest_destination::ensure_plan(staging.path(), &destination, 84, false, false)
            .expect("persist HLS destination plan");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let downloaded = Arc::new(AtomicU64::new(0));
        let (_stop_tx, mut abort) = tokio::sync::oneshot::channel();

        assert!(
            fetch_and_write_hls_track(ManifestHlsTrackTransfer {
                client: &client,
                staging: staging.path(),
                plan: &plan,
                directory: &video_dir,
                filename: "video-track.ts",
                destination: &destination,
                item_id: 84,
                track: TrackKind::Video,
                tx: &tx,
                downloaded: &downloaded,
                abort: &mut abort,
            })
            .await
            .expect("fetch and assemble static HLS MPEG-TS fragments")
        );
        server.await.expect("HLS fragment server task");

        let part = crate::manifest_destination::part_path(
            staging.path(),
            &destination,
            84,
            TrackKind::Video,
        )
        .expect("resolve visible HLS destination part");
        let mut expected = first;
        expected.extend_from_slice(&second);
        assert_eq!(
            tokio::fs::read(part).await.expect("read visible HLS part"),
            expected
        );
        assert!(
            plan.resources
                .iter()
                .all(|resource| !video_dir.join(&resource.filename).exists()),
            "static HLS MPEG-TS resources must not be staged as individual files"
        );
        assert_eq!(downloaded.load(Ordering::Relaxed), 188 * 6);
    }

    #[tokio::test]
    async fn aes128_hls_vod_publishes_growing_playlist_prefixes() {
        // This test covers key/segment staging and ordered playlist publication;
        // FFmpeg consumes the resulting local playlist in the production runner.
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind encrypted HLS server");
        let address = listener.local_addr().expect("encrypted HLS server address");
        let key = vec![0x42; 16];
        let first = valid_transport_stream(3, 0);
        let second = valid_transport_stream(3, 3);
        let key_response = key.clone();
        let first_response = first.clone();
        let second_response = second.clone();
        let release_second = Arc::new(tokio::sync::Notify::new());
        let playlist = br#"#EXTM3U
#EXT-X-VERSION:3
#EXT-X-PLAYLIST-TYPE:VOD
#EXT-X-TARGETDURATION:4
#EXT-X-KEY:METHOD=AES-128,URI="key.bin",IV=0x00000000000000000000000000000001
#EXTINF:4,
segment-1.ts
#EXTINF:4,
segment-2.ts
#EXT-X-ENDLIST
"#;
        let mut plan = crate::manifest::plan_hls_media_playlist(
            "https://cdn.example.test/playlist.m3u8",
            playlist,
        )
        .expect("plan encrypted HLS VOD playlist");
        let mut segment_index = 0usize;
        for resource in &mut plan.resources {
            let path = match resource.kind {
                crate::manifest::HlsResourceKind::Key => "/key.bin".to_string(),
                crate::manifest::HlsResourceKind::Segment => {
                    segment_index += 1;
                    format!("/segment-{segment_index}.ts")
                }
                _ => panic!("unexpected resource in AES-128 MPEG-TS fixture"),
            };
            resource.url = url::Url::parse(&format!("http://{address}{path}"))
                .expect("set local encrypted HLS resource URL");
        }
        let request_count = plan.resources.len();
        assert!(plan.is_incremental_aes128_mpegts_vod_candidate());
        let server_release = Arc::clone(&release_second);
        let server = tokio::spawn(async move {
            let mut requests = Vec::with_capacity(request_count);
            for _ in 0..request_count {
                let (mut socket, _) = listener
                    .accept()
                    .await
                    .expect("accept encrypted HLS request");
                let release = Arc::clone(&server_release);
                let key_response = key_response.clone();
                let first_response = first_response.clone();
                let second_response = second_response.clone();
                requests.push(tokio::spawn(async move {
                    let mut request = [0_u8; 1024];
                    let count = socket
                        .read(&mut request)
                        .await
                        .expect("read encrypted HLS request");
                    let request = String::from_utf8_lossy(&request[..count]);
                    let path = request.split_ascii_whitespace().nth(1).unwrap_or("/");
                    let payload = match path {
                        "/key.bin" => key_response,
                        "/segment-1.ts" => first_response,
                        "/segment-2.ts" => {
                            release.notified().await;
                            second_response
                        }
                        _ => panic!("unexpected encrypted HLS resource path: {path}"),
                    };
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    );
                    socket
                        .write_all(header.as_bytes())
                        .await
                        .expect("write encrypted HLS header");
                    socket
                        .write_all(&payload)
                        .await
                        .expect("write encrypted HLS body");
                }));
            }
            for request in requests {
                request.await.expect("join encrypted HLS response task");
            }
        });

        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build direct encrypted HLS client");
        let directory = tempfile::tempdir().expect("create encrypted HLS staging directory");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let downloaded = Arc::new(AtomicU64::new(0));
        let (_stop_tx, mut abort) = tokio::sync::oneshot::channel();
        let (first_ready_tx, first_ready_rx) = tokio::sync::oneshot::channel();
        let (complete_tx, complete_rx) = tokio::sync::oneshot::channel();
        let fetch_client = client.clone();
        let fetch_plan = plan.clone();
        let fetch_directory = directory.path().to_path_buf();
        let fetch_output_part = directory.path().join("output.part");
        tokio::fs::write(&fetch_output_part, [])
            .await
            .expect("create growing output part");
        let fetch_tx = tx.clone();
        let fetch_downloaded = Arc::clone(&downloaded);
        let fetch_task = tokio::spawn(async move {
            fetch_hls_resources_incrementally(IncrementalHlsFetch {
                client: &fetch_client,
                plan: &fetch_plan,
                directory: &fetch_directory,
                destination_dir: &fetch_directory,
                output_part: &fetch_output_part,
                tx: &fetch_tx,
                downloaded: fetch_downloaded.as_ref(),
                abort: &mut abort,
                connections: 4,
                first_segment_ready: first_ready_tx,
                complete_ready: complete_tx,
            })
            .await
        });

        first_ready_rx
            .await
            .expect("first growing playlist snapshot");
        let first_snapshot = tokio::fs::read_to_string(directory.path().join("local.m3u8"))
            .await
            .expect("read first growing HLS playlist snapshot");
        assert_eq!(first_snapshot.matches("#EXTINF:").count(), 1);
        assert!(!first_snapshot.contains("#EXT-X-ENDLIST"));

        release_second.notify_one();
        complete_rx.await.expect("complete HLS playlist snapshot");
        assert!(
            fetch_task
                .await
                .expect("join incremental HLS fetcher")
                .expect("fetch encrypted HLS resources")
        );
        server.await.expect("encrypted HLS server task");

        let final_snapshot = tokio::fs::read_to_string(directory.path().join("local.m3u8"))
            .await
            .expect("read complete HLS playlist snapshot");
        assert_eq!(final_snapshot.matches("#EXTINF:").count(), 2);
        assert!(final_snapshot.contains("#EXT-X-ENDLIST"));
        assert_eq!(
            downloaded.load(Ordering::Relaxed),
            (key.len() + first.len() + second.len()) as u64
        );
    }

    /// Shared body for the AES-128 incremental VOD integration test. The output
    /// container follows the destination extension, so exercise TS (the native
    /// MPEG-TS case) plus MP4/MKV written through the same owned part.
    async fn assert_aes128_hls_vod_output(extension: &str, force_early_exit: bool) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let ffmpeg = match resolve_manifest_ffmpeg(&[PathBuf::from("/usr/bin")]) {
            Ok(path) => path,
            Err(error) if std::env::var_os("GRAB_REQUIRE_FFMPEG_MANIFEST").is_none() => {
                tracing::debug!(%error, "skipping AES-128 HLS integration test without FFmpeg");
                return;
            }
            Err(error) => {
                panic!("FFmpeg is required for the AES-128 HLS integration test: {error}")
            }
        };
        let fixture = tempfile::tempdir().expect("create AES-128 HLS fixture directory");
        let key = vec![0x42; 16];
        let key_path = fixture.path().join("key.bin");
        std::fs::write(&key_path, &key).expect("write AES-128 fixture key");
        let key_info = fixture.path().join("key-info.txt");
        std::fs::write(
            &key_info,
            format!(
                "key.bin\n{}\n0123456789abcdef0123456789abcdef\n",
                key_path.display()
            ),
        )
        .expect("write AES-128 HLS key info");
        let playlist_path = fixture.path().join("playlist.m3u8");
        let generated = tokio::process::Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x64:rate=5",
                "-t",
                "12",
                "-an",
                "-c:v",
                "mpeg2video",
                "-pix_fmt",
                "yuv420p",
                "-g",
                "5",
                "-sc_threshold",
                "0",
                "-f",
                "hls",
                "-hls_time",
                "1",
                "-hls_list_size",
                "0",
                "-hls_playlist_type",
                "vod",
                "-hls_key_info_file",
            ])
            .arg(&key_info)
            .arg("-hls_segment_filename")
            .arg(fixture.path().join("segment%03d.ts"))
            .arg(&playlist_path)
            .output()
            .await
            .expect("run FFmpeg AES-128 HLS fixture generator");
        assert!(
            generated.status.success(),
            "FFmpeg AES-128 fixture generation failed: {}",
            String::from_utf8_lossy(&generated.stderr)
        );
        let forced_exit_counter = if force_early_exit {
            use std::os::unix::fs::PermissionsExt as _;

            let counter = fixture.path().join("ffmpeg-invocations");
            let wrapper = fixture.path().join("ffmpeg-early-exit-wrapper");
            let real_ffmpeg = ffmpeg.display().to_string().replace('\'', "'\\''");
            let counter_path = counter.display().to_string().replace('\'', "'\\''");
            let script = format!(
                "#!/bin/sh\nif [ ! -f '{counter_path}' ]; then\n  printf '1' > '{counter_path}'\n  printf 'premature mux output'\n  exit 0\nfi\nprintf '2' > '{counter_path}'\nexec '{real_ffmpeg}' \"$@\"\n"
            );
            std::fs::write(&wrapper, script).expect("write early-exit FFmpeg wrapper");
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700))
                .expect("make early-exit FFmpeg wrapper executable");
            Some((wrapper, counter))
        } else {
            None
        };
        let playlist = std::fs::read(&playlist_path).expect("read generated encrypted playlist");

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind AES-128 fixture server");
        let address = listener
            .local_addr()
            .expect("AES-128 fixture server address");
        let mut plan = crate::manifest::plan_hls_media_playlist(
            "https://cdn.example.test/playlist.m3u8",
            &playlist,
        )
        .expect("plan generated AES-128 HLS VOD");
        assert!(plan.is_incremental_aes128_mpegts_vod_candidate());
        assert!(
            plan.segment_count() >= 8,
            "the progressive AES-128 fixture needs enough segments for FFmpeg to probe and mux; found {}",
            plan.segment_count()
        );

        let total_segments = plan.segment_count();
        let segment_paths = plan
            .resources
            .iter()
            .filter(|resource| resource.kind == crate::manifest::HlsResourceKind::Segment)
            .map(|resource| {
                format!(
                    "/{}",
                    resource
                        .url
                        .path()
                        .rsplit('/')
                        .next()
                        .filter(|name| !name.is_empty())
                        .expect("planned AES-128 segment must have a filename")
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            segment_paths.len(),
            total_segments,
            "the AES-128 resource plan must contain one resource for each playlist segment"
        );
        let final_segment_path = segment_paths
            .last()
            .cloned()
            .expect("encrypted HLS fixture must have a final segment");
        let first_segment_path = segment_paths
            .first()
            .expect("encrypted HLS fixture must have a first segment");
        assert_ne!(
            final_segment_path, *first_segment_path,
            "the final-segment gate must not hold back the first segment"
        );
        for resource in &mut plan.resources {
            let name = resource
                .url
                .path()
                .rsplit('/')
                .next()
                .filter(|name| !name.is_empty())
                .expect("planned AES-128 resource must have a filename")
                .to_string();
            resource.url = Url::parse(&format!("http://{address}/{name}"))
                .expect("set local AES-128 fixture resource URL");
        }
        let request_count = plan.resources.len();
        let fixture_root = fixture.path().to_path_buf();
        let release_final_segment = Arc::new(tokio::sync::Notify::new());
        let final_segment_requested = Arc::new(tokio::sync::Notify::new());
        let server_release = Arc::clone(&release_final_segment);
        let server_requested = Arc::clone(&final_segment_requested);
        let server_final_path = final_segment_path.clone();
        let server = tokio::spawn(async move {
            let mut responses = Vec::with_capacity(request_count);
            for _ in 0..request_count {
                let (mut socket, _) = listener
                    .accept()
                    .await
                    .expect("accept AES-128 resource request");
                let fixture_root = fixture_root.clone();
                let release = Arc::clone(&server_release);
                let requested = Arc::clone(&server_requested);
                let final_path = server_final_path.clone();
                responses.push(tokio::spawn(async move {
                    let mut request = [0_u8; 1024];
                    let count = socket
                        .read(&mut request)
                        .await
                        .expect("read AES-128 resource request");
                    let request = String::from_utf8_lossy(&request[..count]);
                    let path = request.split_ascii_whitespace().nth(1).unwrap_or("/");
                    if path == final_path.as_str() {
                        requested.notify_one();
                        release.notified().await;
                    }
                    let name = path.trim_start_matches('/');
                    let payload = tokio::fs::read(fixture_root.join(name))
                        .await
                        .expect("read requested AES-128 fixture resource");
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    );
                    socket
                        .write_all(header.as_bytes())
                        .await
                        .expect("write AES-128 response header");
                    socket
                        .write_all(&payload)
                        .await
                        .expect("write AES-128 response body");
                }));
            }
            for response in responses {
                response.await.expect("join AES-128 response task");
            }
        });

        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build AES-128 fixture client");
        let staging = tempfile::tempdir().expect("create AES-128 HLS staging directory");
        let destination_dir = tempfile::tempdir().expect("create AES-128 destination directory");
        let destination = destination_dir
            .path()
            .join(format!("encrypted-vod.{extension}"));
        let item_id = 73;
        crate::manifest_destination::ensure_plan(
            staging.path(),
            &destination,
            item_id,
            false,
            false,
        )
        .expect("persist destination ownership plan");
        let part_path = crate::manifest_destination::part_path_async(
            staging.path().to_path_buf(),
            destination.clone(),
            item_id,
            TrackKind::Video,
        )
        .await
        .expect("resolve owned destination part");
        let video_dir = staging.path().join("video");
        let audio_dir = staging.path().join("audio");
        tokio::fs::create_dir(&video_dir)
            .await
            .expect("create HLS video staging directory");
        tokio::fs::create_dir(&audio_dir)
            .await
            .expect("create HLS audio staging directory");

        let job = ManifestJob {
            item_id,
            source: crate::media_types::ManifestSource {
                url: "https://cdn.example.test/playlist.m3u8".to_string(),
                kind: ManifestKind::Hls,
                representation_key: "hls-media".into(),
                output_container: None,
                audio_key: None,
                subtitle_key: None,
                subtitle_language: None,
            },
            referer: None,
            dest: destination.clone(),
            proxy: None,
            cookies_browser: "none".to_string(),
            connections: 4,
            embed_subs: false,
        };
        let gate = crate::attempt_gate::AttemptGate::new();
        let downloaded = Arc::new(AtomicU64::new(0));
        let (stop_sender, abort) = tokio::sync::oneshot::channel();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let task_staging = staging.path().to_path_buf();
        let task_dest_dir = destination_dir.path().to_path_buf();
        let task_video_dir = video_dir;
        let task_audio_dir = audio_dir;
        let task_client = client;
        let task_ffmpeg = forced_exit_counter
            .as_ref()
            .map_or_else(|| ffmpeg.clone(), |(wrapper, _)| wrapper.clone());
        let task_downloaded = Arc::clone(&downloaded);
        let download_task = tokio::spawn(async move {
            run_incremental_hls_vod(ManifestIncrementalHlsVod {
                job: &job,
                gate,
                abort,
                tx: &tx,
                staging: &task_staging,
                dest_dir: &task_dest_dir,
                video_dir: &task_video_dir,
                audio_dir: &task_audio_dir,
                tracks: Tracks::Hls {
                    video: Box::new(plan),
                    audio: None,
                    subtitle: None,
                    subtitle_language: None,
                    subtitle_format: SubtitleFormat::Text,
                },
                ffmpeg: task_ffmpeg,
                downloaded: task_downloaded,
                connections: 1,
                client: &task_client,
            })
            .await
        });

        tokio::time::timeout(Duration::from_secs(10), final_segment_requested.notified())
            .await
            .expect("the final segment request should start while earlier segments are muxed");
        let growing_prefix = tokio::fs::read_to_string(staging.path().join("video/local.m3u8"))
            .await
            .expect("read the growing AES-128 playlist while the final segment is blocked");
        assert_eq!(
            growing_prefix.matches("#EXTINF:").count(),
            total_segments - 1,
            "every non-final AES-128 segment must be in the local playlist before final release; prefix:\n{growing_prefix}"
        );
        assert!(!growing_prefix.contains("#EXT-X-ENDLIST"));
        assert!(!growing_prefix.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
        assert!(
            !download_task.is_finished(),
            "the final segment must remain in flight while FFmpeg consumes the growing prefix"
        );
        let initial_growth = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let size = tokio::fs::metadata(&part_path)
                    .await
                    .expect("stat visible destination part")
                    .len();
                if size > 0 {
                    break size;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("FFmpeg should write decrypted bytes before the final segment is released");
        assert!(initial_growth > 0);
        assert!(
            !download_task.is_finished(),
            "the destination part must grow while the final segment download is still blocked"
        );

        release_final_segment.notify_one();
        let outcome = download_task
            .await
            .expect("join incremental AES-128 VOD runner")
            .expect("incremental AES-128 VOD runner should finish");
        assert!(
            matches!(outcome, ManifestOutcome::Finished(size) if size > 0),
            "expected publication of a non-empty decrypted stream, got {outcome:?}"
        );
        if let Some((_, counter)) = forced_exit_counter.as_ref() {
            assert_eq!(
                tokio::fs::read_to_string(counter)
                    .await
                    .expect("read FFmpeg invocation counter"),
                "2",
                "the completed-playlist recovery must run FFmpeg a second time"
            );
        }
        drop(stop_sender);
        server.await.expect("AES-128 fixture server task");

        let final_size = tokio::fs::metadata(&destination)
            .await
            .expect("stat published decrypted destination")
            .len();
        if extension == "ts" && !force_early_exit {
            assert!(
                final_size > initial_growth,
                "the destination output must grow after the withheld AES-128 segment is released (initial={initial_growth}, final={final_size})"
            );
        } else {
            assert!(final_size >= initial_growth);
        }
        assert!(
            tokio::fs::symlink_metadata(&part_path).await.is_err(),
            "successful publication must move the owned destination part into place"
        );
        let part_name = part_path
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .expect("owned destination part has a UTF-8 filename");
        assert!(
            tokio::fs::symlink_metadata(staging.path().join(format!("{part_name}.owner.json")))
                .await
                .is_err(),
            "successful publication must remove the part ownership record"
        );

        let verification = tokio::process::Command::new(&ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(&destination)
            .args(["-f", "null", "-"])
            .output()
            .await
            .expect("verify decrypted destination output");
        assert!(
            verification.status.success(),
            "decrypted AES-128 output is not readable: {}",
            String::from_utf8_lossy(&verification.stderr)
        );
    }

    #[tokio::test]
    async fn ffmpeg_decrypts_aes128_hls_vod_into_an_owned_ts_part() {
        assert_aes128_hls_vod_output("ts", false).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn aes128_vod_remuxes_again_when_ffmpeg_exits_before_playlist_completion() {
        assert_aes128_hls_vod_output("ts", true).await;
    }

    #[tokio::test]
    async fn ffmpeg_decrypts_aes128_hls_vod_into_an_owned_mp4_part() {
        assert_aes128_hls_vod_output("mp4", false).await;
    }

    #[tokio::test]
    async fn ffmpeg_decrypts_aes128_hls_vod_into_an_owned_mkv_part() {
        assert_aes128_hls_vod_output("mkv", false).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn discontinuous_clear_fmp4_vod_grows_owned_destination_part_while_fetching() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let ffmpeg = match resolve_manifest_ffmpeg(&[PathBuf::from("/usr/bin")]) {
            Ok(path) => path,
            Err(error) if std::env::var_os("GRAB_REQUIRE_FFMPEG_MANIFEST").is_none() => {
                tracing::debug!(%error, "skipping incremental fMP4 HLS integration test without FFmpeg");
                return;
            }
            Err(error) => {
                panic!("FFmpeg is required for the incremental fMP4 HLS integration test: {error}")
            }
        };

        let fixture = tempfile::tempdir().expect("create fMP4 HLS fixture directory");
        let playlist_path = fixture.path().join("playlist.m3u8");
        // Use FFmpeg's built-in encoder set; CI does not require an external x264 encoder.
        let generated = tokio::process::Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=96x64:rate=12",
                "-t",
                "4",
                "-an",
                "-c:v",
                "mpeg2video",
                "-pix_fmt",
                "yuv420p",
                "-g",
                "12",
                "-sc_threshold",
                "0",
                "-f",
                "hls",
                "-hls_time",
                "2",
                "-hls_list_size",
                "0",
                "-hls_playlist_type",
                "vod",
                "-hls_segment_type",
                "fmp4",
                "-hls_fmp4_init_filename",
                "init.mp4",
                "-hls_segment_filename",
            ])
            .arg(fixture.path().join("segment-%03d.m4s"))
            .arg(&playlist_path)
            .output()
            .await
            .expect("generate fMP4 HLS fixture");
        assert!(
            generated.status.success(),
            "FFmpeg fMP4 fixture generation failed: {}",
            String::from_utf8_lossy(&generated.stderr)
        );

        let playlist =
            std::fs::read_to_string(&playlist_path).expect("read generated fMP4 HLS playlist");
        let second_segment = playlist
            .rfind("#EXTINF:")
            .expect("fMP4 fixture has a second segment");
        let mut discontinuous_playlist = playlist;
        discontinuous_playlist.insert_str(second_segment, "#EXT-X-DISCONTINUITY\n");
        std::fs::write(&playlist_path, &discontinuous_playlist)
            .expect("add a discontinuity before the final fMP4 segment");

        let mut plan = crate::manifest::plan_hls_media_playlist(
            "https://cdn.example.test/playlist.m3u8",
            discontinuous_playlist.as_bytes(),
        )
        .expect("plan discontinuous fMP4 HLS VOD");
        assert!(
            !plan.is_destination_assemblable_candidate(),
            "the discontinuity must keep this out of direct byte concatenation"
        );
        assert!(plan.is_incremental_clear_vod_candidate());
        assert_eq!(
            plan.segment_count(),
            2,
            "fixture should produce two media segments"
        );

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind fMP4 fixture server");
        let address = listener.local_addr().expect("fMP4 fixture server address");
        let served_root = fixture.path().join("served");
        std::fs::create_dir(&served_root).expect("create served resource directory");
        let final_segment_filename = plan
            .resources
            .iter()
            .rev()
            .find(|resource| resource.kind == crate::manifest::HlsResourceKind::Segment)
            .map(|resource| resource.filename.clone())
            .expect("fMP4 fixture has a final media segment");
        let final_segment_path = format!("/{final_segment_filename}");
        for resource in &mut plan.resources {
            let source_name = resource
                .url
                .path()
                .rsplit('/')
                .next()
                .filter(|name| !name.is_empty())
                .expect("planned fMP4 resource has a filename")
                .to_string();
            std::fs::copy(
                fixture.path().join(&source_name),
                served_root.join(&resource.filename),
            )
            .expect("copy fixture resource under its localized filename");
            resource.url = Url::parse(&format!("http://{address}/{}", resource.filename))
                .expect("set local fMP4 resource URL");
        }
        let request_count = plan.resources.len();
        assert_eq!(
            request_count,
            plan.segment_count() + 1,
            "one init map and two media segments"
        );

        let release_final_segment = Arc::new(tokio::sync::Notify::new());
        let final_segment_requested = Arc::new(tokio::sync::Notify::new());
        let server_release = Arc::clone(&release_final_segment);
        let server_requested = Arc::clone(&final_segment_requested);
        let server_final_path = final_segment_path.clone();
        let server_root = served_root.clone();
        let server = tokio::spawn(async move {
            let mut responses = Vec::with_capacity(request_count);
            for _ in 0..request_count {
                let (mut socket, _) = listener
                    .accept()
                    .await
                    .expect("accept fMP4 resource request");
                let root = server_root.clone();
                let release = Arc::clone(&server_release);
                let requested = Arc::clone(&server_requested);
                let final_path = server_final_path.clone();
                responses.push(tokio::spawn(async move {
                    let mut request = [0_u8; 1024];
                    let count = socket
                        .read(&mut request)
                        .await
                        .expect("read fMP4 resource request");
                    let request = String::from_utf8_lossy(&request[..count]);
                    let path = request.split_ascii_whitespace().nth(1).unwrap_or("/");
                    if path == final_path.as_str() {
                        requested.notify_one();
                        release.notified().await;
                    }
                    let name = path.trim_start_matches('/');
                    let payload = tokio::fs::read(root.join(name))
                        .await
                        .expect("read served fMP4 fixture resource");
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    );
                    socket
                        .write_all(header.as_bytes())
                        .await
                        .expect("write fMP4 response header");
                    socket
                        .write_all(&payload)
                        .await
                        .expect("write fMP4 response body");
                }));
            }
            for response in responses {
                response.await.expect("join fMP4 response task");
            }
        });

        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build fMP4 fixture client");
        let staging = tempfile::tempdir().expect("create fMP4 HLS staging directory");
        let destination_dir = tempfile::tempdir().expect("create fMP4 HLS destination directory");
        let destination = destination_dir.path().join("discontinuous-vod.mp4");
        let item_id = 91;
        crate::manifest_destination::ensure_plan(
            staging.path(),
            &destination,
            item_id,
            false,
            false,
        )
        .expect("persist fMP4 destination ownership plan");
        let part_path = crate::manifest_destination::part_path_async(
            staging.path().to_path_buf(),
            destination.clone(),
            item_id,
            TrackKind::Video,
        )
        .await
        .expect("resolve owned fMP4 destination part");
        let video_dir = staging.path().join("video");
        let audio_dir = staging.path().join("audio");
        tokio::fs::create_dir(&video_dir)
            .await
            .expect("create fMP4 video staging directory");
        tokio::fs::create_dir(&audio_dir)
            .await
            .expect("create fMP4 audio staging directory");

        let job = ManifestJob {
            referer: None,
            item_id,
            source: crate::media_types::ManifestSource {
                url: "https://cdn.example.test/playlist.m3u8".to_string(),
                kind: ManifestKind::Hls,
                representation_key: "hls-media".into(),
                output_container: Some(ManifestContainer::Mp4),
                audio_key: None,
                subtitle_key: None,
                subtitle_language: None,
            },
            dest: destination.clone(),
            proxy: None,
            cookies_browser: "none".to_string(),
            connections: 4,
            embed_subs: false,
        };
        let gate = crate::attempt_gate::AttemptGate::new();
        let downloaded = Arc::new(AtomicU64::new(0));
        let (stop_sender, abort) = tokio::sync::oneshot::channel();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let task_staging = staging.path().to_path_buf();
        let task_dest_dir = destination_dir.path().to_path_buf();
        let task_video_dir = video_dir;
        let task_audio_dir = audio_dir;
        let task_client = client;
        let task_ffmpeg = ffmpeg.clone();
        let task_downloaded = Arc::clone(&downloaded);
        let download_task = tokio::spawn(async move {
            run_incremental_hls_vod(ManifestIncrementalHlsVod {
                job: &job,
                gate,
                abort,
                tx: &tx,
                staging: &task_staging,
                dest_dir: &task_dest_dir,
                video_dir: &task_video_dir,
                audio_dir: &task_audio_dir,
                tracks: Tracks::Hls {
                    video: Box::new(plan),
                    audio: None,
                    subtitle: None,
                    subtitle_language: None,
                    subtitle_format: SubtitleFormat::Text,
                },
                ffmpeg: task_ffmpeg,
                downloaded: task_downloaded,
                connections: 1,
                client: &task_client,
            })
            .await
        });

        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            final_segment_requested.notified(),
        )
        .await
        .expect("the final fMP4 segment request should start while the first segment is muxed");
        assert!(
            !download_task.is_finished(),
            "the final fMP4 segment must remain in flight while FFmpeg consumes the growing playlist"
        );
        let initial_growth = tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                let size = tokio::fs::metadata(&part_path)
                    .await
                    .expect("stat fMP4 destination part")
                    .len();
                if size > 0 {
                    break size;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("FFmpeg should write the destination part before the final segment is released");
        assert!(initial_growth > 0);

        release_final_segment.notify_one();
        let outcome = download_task
            .await
            .expect("join incremental fMP4 HLS VOD runner")
            .expect("incremental fMP4 HLS VOD runner should finish");
        assert!(
            matches!(outcome, ManifestOutcome::Finished(size) if size > 0),
            "expected publication of a non-empty fMP4 stream, got {outcome:?}"
        );
        drop(stop_sender);
        server.await.expect("fMP4 fixture server task");

        assert!(
            tokio::fs::symlink_metadata(&part_path).await.is_err(),
            "successful publication must move the owned fMP4 destination part into place"
        );
        let verification = tokio::process::Command::new(&ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(&destination)
            .args(["-f", "null", "-"])
            .output()
            .await
            .expect("verify published discontinuous fMP4 output");
        assert!(
            verification.status.success(),
            "discontinuous fMP4 output is not readable: {}",
            String::from_utf8_lossy(&verification.stderr)
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn separate_audio_fmp4_vod_grows_one_owned_destination_part_while_fetching() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let ffmpeg = match resolve_manifest_ffmpeg(&[PathBuf::from("/usr/bin")]) {
            Ok(path) => path,
            Err(error) if std::env::var_os("GRAB_REQUIRE_FFMPEG_MANIFEST").is_none() => {
                tracing::debug!(%error, "skipping multi-rendition HLS integration test without FFmpeg");
                return;
            }
            Err(error) => {
                panic!("FFmpeg is required for the multi-rendition HLS integration test: {error}")
            }
        };

        let fixture = tempfile::tempdir().expect("create multi-rendition fixture directory");
        let video_source = fixture.path().join("video-source");
        let audio_source = fixture.path().join("audio-source");
        let served_video = fixture.path().join("served-video");
        let served_audio = fixture.path().join("served-audio");
        for directory in [&video_source, &audio_source, &served_video, &served_audio] {
            std::fs::create_dir(directory).expect("create HLS fixture directory");
        }
        let video_playlist_path = video_source.join("playlist.m3u8");
        let video_generated = tokio::process::Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=96x64:rate=12",
                "-t",
                "4",
                "-an",
                "-c:v",
                "mpeg2video",
                "-pix_fmt",
                "yuv420p",
                "-g",
                "12",
                "-sc_threshold",
                "0",
                "-f",
                "hls",
                "-hls_time",
                "2",
                "-hls_list_size",
                "0",
                "-hls_playlist_type",
                "vod",
                "-hls_segment_type",
                "fmp4",
                "-hls_fmp4_init_filename",
                "init.mp4",
                "-hls_segment_filename",
            ])
            .arg(video_source.join("segment-%03d.m4s"))
            .arg(&video_playlist_path)
            .output()
            .await
            .expect("generate fMP4 video rendition");
        assert!(
            video_generated.status.success(),
            "FFmpeg video fixture generation failed: {}",
            String::from_utf8_lossy(&video_generated.stderr)
        );

        let audio_playlist_path = audio_source.join("playlist.m3u8");
        let audio_generated = tokio::process::Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=1000:sample_rate=48000",
                "-t",
                "4",
                "-vn",
                "-c:a",
                "aac",
                "-b:a",
                "96k",
                "-f",
                "hls",
                "-hls_time",
                "2",
                "-hls_list_size",
                "0",
                "-hls_playlist_type",
                "vod",
                "-hls_segment_type",
                "fmp4",
                "-hls_fmp4_init_filename",
                "init.mp4",
                "-hls_segment_filename",
            ])
            .arg(audio_source.join("segment-%03d.m4s"))
            .arg(&audio_playlist_path)
            .output()
            .await
            .expect("generate fMP4 audio rendition");
        assert!(
            audio_generated.status.success(),
            "FFmpeg audio fixture generation failed: {}",
            String::from_utf8_lossy(&audio_generated.stderr)
        );

        let video_playlist = std::fs::read_to_string(&video_playlist_path)
            .expect("read generated fMP4 video playlist");
        let second_video_segment = video_playlist
            .rfind("#EXTINF:")
            .expect("video fixture has a second segment");
        let mut discontinuous_video_playlist = video_playlist;
        discontinuous_video_playlist.insert_str(second_video_segment, "#EXT-X-DISCONTINUITY\n");
        std::fs::write(&video_playlist_path, &discontinuous_video_playlist)
            .expect("add a video discontinuity before the final segment");

        let mut video_plan = crate::manifest::plan_hls_media_playlist(
            "https://cdn.example.test/video/playlist.m3u8",
            discontinuous_video_playlist.as_bytes(),
        )
        .expect("plan discontinuous fMP4 video rendition");
        let mut audio_plan = crate::manifest::plan_hls_media_playlist(
            "https://cdn.example.test/audio/playlist.m3u8",
            &std::fs::read(&audio_playlist_path).expect("read generated fMP4 audio playlist"),
        )
        .expect("plan fMP4 audio rendition");
        assert!(
            video_plan.is_incremental_clear_vod_candidate(),
            "the video discontinuity must select playlist-aware muxing"
        );
        assert!(
            audio_plan.is_destination_assemblable_candidate(),
            "the simple audio rendition is still eligible for ordered resource fetching"
        );
        assert!(
            Tracks::Hls {
                video: Box::new(video_plan.clone()),
                audio: Some(Box::new(audio_plan.clone())),
                subtitle: None,
                subtitle_language: None,
                subtitle_format: SubtitleFormat::Text,
            }
            .uses_incremental_hls_vod(),
            "a complex video rendition and a simple separate audio rendition must use incremental muxing"
        );

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind multi-rendition fixture server");
        let address = listener
            .local_addr()
            .expect("multi-rendition fixture server address");
        let final_video_filename = video_plan
            .resources
            .iter()
            .rev()
            .find(|resource| resource.kind == crate::manifest::HlsResourceKind::Segment)
            .map(|resource| resource.filename.clone())
            .expect("video fixture has a final media segment");
        let final_video_path = format!("/video/{final_video_filename}");

        for resource in &mut video_plan.resources {
            let source_name = resource
                .url
                .path()
                .rsplit('/')
                .next()
                .filter(|name| !name.is_empty())
                .expect("video resource has a fixture filename")
                .to_string();
            std::fs::copy(
                video_source.join(source_name),
                served_video.join(&resource.filename),
            )
            .expect("copy video resource under its localized staging filename");
            resource.url = Url::parse(&format!("http://{address}/video/{}", resource.filename))
                .expect("set local video resource URL");
        }
        for resource in &mut audio_plan.resources {
            let source_name = resource
                .url
                .path()
                .rsplit('/')
                .next()
                .filter(|name| !name.is_empty())
                .expect("audio resource has a fixture filename")
                .to_string();
            std::fs::copy(
                audio_source.join(source_name),
                served_audio.join(&resource.filename),
            )
            .expect("copy audio resource under its localized staging filename");
            resource.url = Url::parse(&format!("http://{address}/audio/{}", resource.filename))
                .expect("set local audio resource URL");
        }
        let request_count = video_plan.resources.len() + audio_plan.resources.len();
        let release_final_video = Arc::new(tokio::sync::Notify::new());
        let final_video_requested = Arc::new(tokio::sync::Notify::new());
        let server_release = Arc::clone(&release_final_video);
        let server_requested = Arc::clone(&final_video_requested);
        let server_final_path = final_video_path.clone();
        let server_video_root = served_video.clone();
        let server_audio_root = served_audio.clone();
        let server = tokio::spawn(async move {
            let mut responses = Vec::with_capacity(request_count);
            for _ in 0..request_count {
                let (mut socket, _) = listener
                    .accept()
                    .await
                    .expect("accept multi-rendition HLS resource request");
                let video_root = server_video_root.clone();
                let audio_root = server_audio_root.clone();
                let release = Arc::clone(&server_release);
                let requested = Arc::clone(&server_requested);
                let final_path = server_final_path.clone();
                responses.push(tokio::spawn(async move {
                    let mut request = [0_u8; 1024];
                    let count = socket
                        .read(&mut request)
                        .await
                        .expect("read multi-rendition HLS request");
                    let request = String::from_utf8_lossy(&request[..count]);
                    let path = request.split_ascii_whitespace().nth(1).unwrap_or("/");
                    if path == final_path.as_str() {
                        requested.notify_one();
                        release.notified().await;
                    }
                    let (root, name) = if let Some(name) = path.strip_prefix("/video/") {
                        (video_root, name)
                    } else if let Some(name) = path.strip_prefix("/audio/") {
                        (audio_root, name)
                    } else {
                        panic!("unexpected multi-rendition resource path: {path}");
                    };
                    let payload = tokio::fs::read(root.join(name))
                        .await
                        .expect("read served multi-rendition fixture resource");
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    );
                    socket
                        .write_all(header.as_bytes())
                        .await
                        .expect("write multi-rendition response header");
                    socket
                        .write_all(&payload)
                        .await
                        .expect("write multi-rendition response body");
                }));
            }
            for response in responses {
                response.await.expect("join multi-rendition response task");
            }
        });

        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build multi-rendition fixture client");
        let staging = tempfile::tempdir().expect("create multi-rendition HLS staging directory");
        let destination_dir =
            tempfile::tempdir().expect("create multi-rendition destination directory");
        let destination = destination_dir.path().join("separate-audio-vod.mp4");
        let item_id = 92;
        crate::manifest_destination::ensure_plan(
            staging.path(),
            &destination,
            item_id,
            true,
            false,
        )
        .expect("persist multi-rendition destination ownership plan");
        let part_path = crate::manifest_destination::part_path_async(
            staging.path().to_path_buf(),
            destination.clone(),
            item_id,
            TrackKind::Video,
        )
        .await
        .expect("resolve combined destination partial");
        let video_dir = staging.path().join("video");
        let audio_dir = staging.path().join("audio");
        tokio::fs::create_dir(&video_dir)
            .await
            .expect("create video staging directory");
        tokio::fs::create_dir(&audio_dir)
            .await
            .expect("create audio staging directory");

        let job = ManifestJob {
            referer: None,
            item_id,
            source: crate::media_types::ManifestSource {
                url: "https://cdn.example.test/master.m3u8".to_string(),
                kind: ManifestKind::Hls,
                representation_key: "hls-variant".into(),
                output_container: Some(ManifestContainer::Mp4),
                audio_key: Some("hls-audio".into()),
                subtitle_key: None,
                subtitle_language: None,
            },
            dest: destination.clone(),
            proxy: None,
            cookies_browser: "none".to_string(),
            connections: 1,
            embed_subs: false,
        };
        let gate = crate::attempt_gate::AttemptGate::new();
        let downloaded = Arc::new(AtomicU64::new(0));
        let (stop_sender, abort) = tokio::sync::oneshot::channel();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let task_staging = staging.path().to_path_buf();
        let task_dest_dir = destination_dir.path().to_path_buf();
        let task_video_dir = video_dir;
        let task_audio_dir = audio_dir;
        let task_client = client;
        let task_ffmpeg = ffmpeg.clone();
        let task_downloaded = Arc::clone(&downloaded);
        let download_task = tokio::spawn(async move {
            run_incremental_hls_vod(ManifestIncrementalHlsVod {
                job: &job,
                gate,
                abort,
                tx: &tx,
                staging: &task_staging,
                dest_dir: &task_dest_dir,
                video_dir: &task_video_dir,
                audio_dir: &task_audio_dir,
                tracks: Tracks::Hls {
                    video: Box::new(video_plan),
                    audio: Some(Box::new(audio_plan)),
                    subtitle: None,
                    subtitle_language: None,
                    subtitle_format: SubtitleFormat::Text,
                },
                ffmpeg: task_ffmpeg,
                downloaded: task_downloaded,
                connections: 1,
                client: &task_client,
            })
            .await
        });

        tokio::time::timeout(Duration::from_secs(10), final_video_requested.notified())
            .await
            .expect("the final video segment request should start after the first video segment");
        assert!(
            !download_task.is_finished(),
            "the combined download must remain active while the final video segment is blocked"
        );
        let initial_growth = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let size = tokio::fs::metadata(&part_path)
                    .await
                    .expect("stat combined destination part")
                    .len();
                if size > 0 {
                    break size;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("FFmpeg should write the combined destination part before the final video segment is released");
        assert!(initial_growth > 0);

        release_final_video.notify_one();
        let outcome = download_task
            .await
            .expect("join separate-audio HLS VOD runner")
            .expect("separate-audio HLS VOD runner should finish");
        assert!(
            matches!(outcome, ManifestOutcome::Finished(size) if size > 0),
            "expected publication of a non-empty combined stream, got {outcome:?}"
        );
        drop(stop_sender);
        server.await.expect("multi-rendition fixture server task");

        assert!(
            tokio::fs::symlink_metadata(&part_path).await.is_err(),
            "successful publication must move the owned combined part into place"
        );
        let verification = tokio::process::Command::new(&ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(&destination)
            .args(["-map", "0:v:0", "-map", "0:a:0", "-f", "null", "-"])
            .output()
            .await
            .expect("verify published audio/video HLS output");
        assert!(
            verification.status.success(),
            "published output must contain readable video and audio: {}",
            String::from_utf8_lossy(&verification.stderr)
        );
    }

    #[tokio::test]
    async fn static_hls_fmp4_fragments_grow_the_owned_destination_part_in_order() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind fMP4 fragment server");
        let address = listener.local_addr().expect("fMP4 server address");
        let init = valid_fmp4_init_segment();
        let first = valid_fmp4_media_segment(1, &[0x12, 0x34]);
        let second = valid_fmp4_media_segment(2, &[0x56, 0x78]);
        let init_response = init.clone();
        let first_response = first.clone();
        let second_response = second.clone();
        let server = tokio::spawn(async move {
            for _ in 0..3 {
                let (mut socket, _) = listener.accept().await.expect("accept fMP4 request");
                let mut request = [0_u8; 1024];
                let count = socket.read(&mut request).await.expect("read fMP4 request");
                assert_ne!(count, 0, "fMP4 request ended before its headers");
                let request = String::from_utf8_lossy(&request[..count]);
                let path = request.split_ascii_whitespace().nth(1).unwrap_or("/");
                let payload = if path.contains("init.mp4") {
                    &init_response
                } else if path.contains("segment-1.m4s") {
                    &first_response
                } else {
                    &second_response
                };
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                );
                socket
                    .write_all(header.as_bytes())
                    .await
                    .expect("write fMP4 response header");
                socket
                    .write_all(payload)
                    .await
                    .expect("write fMP4 response body");
            }
        });

        let playlist = br#"#EXTM3U
#EXT-X-VERSION:6
#EXT-X-TARGETDURATION:4
#EXT-X-PLAYLIST-TYPE:VOD
#EXT-X-MAP:URI="init.mp4"
#EXTINF:4,
segment-1.m4s
#EXTINF:4,
segment-2.m4s
#EXT-X-ENDLIST
"#;
        let mut plan = crate::manifest::plan_hls_media_playlist(
            "https://cdn.example.test/playlist.m3u8",
            playlist,
        )
        .expect("plan static fMP4 HLS media playlist");
        for (index, resource) in plan.resources.iter_mut().enumerate() {
            let name = match index {
                0 => "init.mp4".to_string(),
                other => format!("segment-{other}.m4s"),
            };
            resource.url = url::Url::parse(&format!("http://{address}/{name}"))
                .expect("set local fMP4 test resource URL");
        }
        assert!(plan.is_destination_assemblable_candidate());
        assert!(plan.is_single_init_fmp4_presentation());

        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build direct fMP4 test client");
        let staging = tempfile::tempdir().expect("create fMP4 staging root");
        let video_dir = staging.path().join("video");
        tokio::fs::create_dir(&video_dir)
            .await
            .expect("create fMP4 video track directory");
        let destination_dir = tempfile::tempdir().expect("create fMP4 destination");
        let destination = destination_dir.path().join("Example.mp4");
        crate::manifest_destination::ensure_plan(staging.path(), &destination, 85, false, false)
            .expect("persist fMP4 destination plan");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let downloaded = Arc::new(AtomicU64::new(0));
        let (_stop_tx, mut abort) = tokio::sync::oneshot::channel();

        assert!(
            fetch_and_write_hls_track(ManifestHlsTrackTransfer {
                client: &client,
                staging: staging.path(),
                plan: &plan,
                directory: &video_dir,
                filename: "video-track.ts",
                destination: &destination,
                item_id: 85,
                track: TrackKind::Video,
                tx: &tx,
                downloaded: &downloaded,
                abort: &mut abort,
            })
            .await
            .expect("fetch and assemble static HLS fMP4 fragments")
        );
        server.await.expect("fMP4 fragment server task");

        let part = crate::manifest_destination::part_path(
            staging.path(),
            &destination,
            85,
            TrackKind::Video,
        )
        .expect("resolve visible fMP4 destination part");
        assert_eq!(
            downloaded.load(Ordering::Relaxed),
            (init.len() + first.len() + second.len()) as u64
        );
        let mut expected = init;
        expected.extend_from_slice(&first);
        expected.extend_from_slice(&second);
        assert_eq!(
            tokio::fs::read(part).await.expect("read visible fMP4 part"),
            expected
        );
        assert!(
            plan.resources
                .iter()
                .all(|resource| !video_dir.join(&resource.filename).exists()),
            "static HLS fMP4 resources must not be staged as individual files"
        );
    }

    #[tokio::test]
    async fn track_directory_rejects_existing_non_directory() {
        let directory = tempfile::tempdir().expect("create staging parent");
        let path = directory.path().join("track");
        tokio::fs::write(&path, b"not a directory")
            .await
            .expect("create conflicting track file");

        let error = ensure_track_dir(&path)
            .await
            .expect_err("a regular file cannot be used as a track directory");

        assert!(matches!(error, ManifestTrackDirError::NotDirectory));
    }

    #[tokio::test]
    async fn live_finish_marker_is_idempotent_and_preserves_committed_bytes() {
        let directory = tempfile::tempdir().expect("create live finish marker directory");
        assert_eq!(
            read_live_finish_marker(directory.path())
                .await
                .expect("read missing finish marker"),
            None
        );

        ensure_live_finish_marker(directory.path(), 123_456)
            .await
            .expect("persist finish marker");
        assert_eq!(
            read_live_finish_marker(directory.path())
                .await
                .expect("read finish marker"),
            Some(123_456)
        );
        ensure_live_finish_marker(directory.path(), 123_456)
            .await
            .expect("repeated finish marker write is idempotent");
        assert!(
            ensure_live_finish_marker(directory.path(), 123_457)
                .await
                .is_err(),
            "a retry must not silently change the committed byte count"
        );
    }

    #[test]
    fn hls_subtitle_playlist_format_distinguishes_text_from_fmp4() {
        let webvtt = crate::manifest::plan_hls_media_playlist(
            "https://cdn.example.test/subtitles/en.m3u8",
            b"#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2,\nen-0.vtt\n#EXT-X-ENDLIST\n",
        )
        .expect("plan a WebVTT subtitle playlist");
        assert_eq!(hls_subtitle_format(&webvtt), SubtitleFormat::Text);

        let fmp4 = crate::manifest::plan_hls_media_playlist(
            "https://cdn.example.test/subtitles/en.m3u8",
            b"#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:2\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:2,\nen-0.m4s\n#EXT-X-ENDLIST\n",
        )
        .expect("plan an fMP4 subtitle playlist");
        assert_eq!(hls_subtitle_format(&fmp4), SubtitleFormat::Fmp4);
    }

    #[test]
    fn subtitle_embedding_respects_output_container_and_source_format() {
        assert!(!embed_manifest_subtitles(
            true,
            ManifestContainer::Ts,
            SubtitleFormat::Text
        ));
        assert!(!embed_manifest_subtitles(
            false,
            ManifestContainer::Mp4,
            SubtitleFormat::Text
        ));
        assert!(embed_manifest_subtitles(
            true,
            ManifestContainer::Mp4,
            SubtitleFormat::Text
        ));
        assert!(embed_manifest_subtitles(
            true,
            ManifestContainer::Mkv,
            SubtitleFormat::Text
        ));
        assert!(!embed_manifest_subtitles(
            true,
            ManifestContainer::Mkv,
            SubtitleFormat::Fmp4
        ));
        assert!(!embed_manifest_subtitles(
            true,
            ManifestContainer::Ts,
            SubtitleFormat::Fmp4
        ));
        assert!(embed_manifest_subtitles(
            true,
            ManifestContainer::Mp4,
            SubtitleFormat::Fmp4
        ));
    }

    #[test]
    fn dash_output_container_follows_the_selected_video_mime_type() {
        assert_eq!(
            crate::manifest_plan::dash_output_container(Some("video/mp2t")),
            Some(crate::media_types::ManifestContainer::Ts)
        );
        assert_eq!(
            crate::manifest_plan::dash_output_container(Some("video/mp4")),
            Some(crate::media_types::ManifestContainer::Mp4)
        );
        assert_eq!(
            crate::manifest_plan::dash_output_container(Some("application/mp4; codecs=avc1")),
            Some(crate::media_types::ManifestContainer::Mp4)
        );
        assert_eq!(
            crate::manifest_plan::dash_output_container(Some("video/webm")),
            None
        );
        assert_eq!(crate::manifest_plan::dash_output_container(None), None);
    }

    #[test]
    fn manifest_container_from_path_accepts_mp4_mkv_and_ts_extensions() {
        assert_eq!(
            manifest_output_container(Path::new("stream.mp4")).expect("MP4 path"),
            ManifestContainer::Mp4
        );
        assert_eq!(
            manifest_output_container(Path::new("stream.MKV")).expect("MKV path"),
            ManifestContainer::Mkv
        );
        assert_eq!(
            manifest_output_container(Path::new("stream.TS")).expect("TS path"),
            ManifestContainer::Ts
        );
        assert!(manifest_output_container(Path::new("stream.webm")).is_err());
        assert!(manifest_output_container(Path::new("stream")).is_err());
    }

    #[tokio::test]
    async fn mpegts_output_rejects_embedded_subtitles_before_spawning_ffmpeg() {
        let (_abort_tx, mut abort) = tokio::sync::oneshot::channel();
        let result = run_ffmpeg_with_offsets(
            Path::new("ffmpeg-is-not-invoked"),
            FfmpegInput {
                path: Path::new("video.mp4"),
                timestamp_offset: None,
            },
            None,
            InputTimestampPolicy::Normalize,
            SubtitleOutput {
                input: Some(Path::new("subtitles.vtt")),
                embed: true,
                sidecar: None,
                format: SubtitleFormat::Text,
            },
            Path::new("output.ts"),
            &mut abort,
        )
        .await;

        assert!(matches!(
            result,
            Err(ManifestRunError::Failed(message))
                if message.contains("Disable subtitle embedding")
                    && message.contains("SRT sidecar")
        ));
    }

    #[test]
    fn mux_space_check_reserves_room_for_ffmpeg_output() {
        assert!(check_manifest_mux_space(100, 100).is_ok());
        let error = check_manifest_mux_space(99, 100).expect_err("one byte short must fail");
        assert!(matches!(
            error,
            ManifestRunError::Failed(message) if message.contains("Not enough free disk space")
        ));
    }

    #[test]
    fn incremental_hls_space_estimate_tracks_staged_bytes_and_growing_output() {
        assert_eq!(incremental_hls_remaining_output_bytes(1_000, 0), 1_100);
        assert_eq!(incremental_hls_remaining_output_bytes(1_000, 600), 500);
        assert_eq!(incremental_hls_remaining_output_bytes(1_000, 1_100), 0);
        assert_eq!(
            incremental_hls_remaining_output_bytes(u64::MAX, 0),
            u64::MAX
        );
    }

    #[test]
    fn byte_range_overshoot_is_rejected_before_writing() {
        let range = crate::manifest::ManifestByteRange {
            offset: 10,
            length: 8,
        };
        assert!(!byte_range_content_length_exceeds(range, None));
        assert!(!byte_range_content_length_exceeds(range, Some(8)));
        assert!(byte_range_content_length_exceeds(range, Some(9)));
        assert!(!byte_range_chunk_exceeds(range, 3, 5));
        assert!(byte_range_chunk_exceeds(range, 3, 6));
        assert!(byte_range_chunk_exceeds(range, 8, 1));
    }

    #[test]
    fn manifest_storage_errors_classify_enospc_and_edquot() {
        for errno in [rustix::io::Errno::NOSPC, rustix::io::Errno::DQUOT] {
            let error = std::io::Error::from_raw_os_error(errno.raw_os_error());
            assert!(matches!(
                manifest_storage_error("could not write media", error),
                ManifestRunError::Failed(message)
                    if message.contains("Not enough free disk space or storage quota")
            ));
        }
    }

    #[test]
    fn ffmpeg_storage_full_errors_are_not_classified_as_unsupported_media() {
        assert!(ffmpeg_log_indicates_storage_full(
            "av_interleaved_write_frame(): No space left on device"
        ));
        assert!(ffmpeg_log_indicates_storage_full("Disk quota exceeded"));
        assert!(!ffmpeg_log_indicates_storage_full(
            "Invalid data found when processing input"
        ));
    }

    #[test]
    fn mux_failure_preserves_the_staged_capture_for_retry() {
        let error = mux_failure_error("Could not write header: Permission denied");
        assert!(matches!(
            &error,
            ManifestRunError::Failed(message)
                if message.contains("FFmpeg could not mux the stream")
        ));
        assert!(
            matches!(error.into_engine_message(), EngineMsg::Failed(_)),
            "a mux error must not enter the fallback path that removes staged resources"
        );
    }

    #[test]
    fn mux_storage_full_failure_keeps_its_specific_error() {
        let error = mux_failure_error("av_interleaved_write_frame(): No space left on device");
        assert!(matches!(
            error,
            ManifestRunError::Failed(message)
                if message == "Not enough free disk space to finalize this stream"
        ));
    }

    #[tokio::test]
    async fn static_dash_fetches_concurrently_and_writes_in_manifest_order() {
        use std::sync::atomic::AtomicUsize;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10S">
  <Period duration="PT10S">
    <AdaptationSet id="video" contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1" duration="5" initialization="init.mp4" media="seg-$Number$.m4s"/>
      <Representation id="v1" bandwidth="1000000" codecs="avc1.64001f"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let parsed =
            crate::manifest::dash::parse_dash_manifest("https://example.test/manifest.mpd", body)
                .expect("parse static DASH fixture");
        let representation_key = parsed.video_representations[0].key.clone();
        let mut plan =
            crate::manifest::dash::plan_dash_representation(&parsed, &representation_key)
                .expect("plan video representation");

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind DASH resource server");
        let address = listener.local_addr().expect("resource server address");
        for resource in plan.resources.iter_mut() {
            let filename = match resource.kind {
                DashResourceKind::Initialization => "init.mp4".to_string(),
                DashResourceKind::Segment => {
                    format!("seg-{}.m4s", resource.segment_number.unwrap_or_default())
                }
            };
            resource.url =
                Url::parse(&format!("http://{address}/{filename}")).expect("local resource URL");
        }

        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let server_active = Arc::clone(&active);
        let server_maximum = Arc::clone(&maximum);
        let server = tokio::spawn(async move {
            let mut tasks = Vec::new();
            for _ in 0..3 {
                let (mut socket, _) = listener.accept().await.expect("accept DASH resource");
                let active = Arc::clone(&server_active);
                let maximum = Arc::clone(&server_maximum);
                tasks.push(tokio::spawn(async move {
                    let mut request = [0_u8; 1024];
                    let count = socket.read(&mut request).await.expect("read request");
                    let request = String::from_utf8_lossy(&request[..count]);
                    let path = request.split_ascii_whitespace().nth(1).unwrap_or("/");
                    let (payload, delay): (&[u8], u64) = match path {
                        "/init.mp4" => (b"init", 100),
                        "/seg-1.m4s" => (b"one", 5),
                        "/seg-2.m4s" => (b"two", 1),
                        _ => (b"unexpected", 0),
                    };
                    let current = active.fetch_add(1, Ordering::AcqRel) + 1;
                    maximum.fetch_max(current, Ordering::AcqRel);
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    );
                    socket
                        .write_all(header.as_bytes())
                        .await
                        .expect("write response header");
                    socket
                        .write_all(payload)
                        .await
                        .expect("write response body");
                    active.fetch_sub(1, Ordering::AcqRel);
                }));
            }
            for task in tasks {
                task.await.expect("resource request task");
            }
        });

        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build direct HTTP client");
        let staging = tempfile::tempdir().expect("create DASH staging");
        let video_dir = staging.path().join("video");
        tokio::fs::create_dir(&video_dir)
            .await
            .expect("create DASH video track directory");
        let destination_dir = tempfile::tempdir().expect("create DASH destination");
        let destination = destination_dir.path().join("Example.mp4");
        crate::manifest_destination::ensure_plan(staging.path(), &destination, 42, false, false)
            .expect("persist DASH destination plan");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let downloaded = Arc::new(AtomicU64::new(0));
        let (_stop_tx, mut abort) = tokio::sync::oneshot::channel();

        assert!(
            fetch_and_write_dash_track(ManifestDashTrackTransfer {
                client: &client,
                staging: staging.path(),
                plan: &plan,
                directory: &video_dir,
                filename: "video-track.mp4",
                destination: &destination,
                item_id: 42,
                track: TrackKind::Video,
                connections: 2,
                tx: &tx,
                downloaded: &downloaded,
                abort: &mut abort,
            })
            .await
            .expect("fetch and write DASH track")
        );
        server.await.expect("DASH resource server task");

        assert_eq!(maximum.load(Ordering::Relaxed), 2);
        assert_eq!(downloaded.load(Ordering::Relaxed), 10);
        let part = crate::manifest_destination::part_path(
            staging.path(),
            &destination,
            42,
            TrackKind::Video,
        )
        .expect("resolve assembled DASH part");
        assert_eq!(
            tokio::fs::read(part)
                .await
                .expect("read assembled DASH track"),
            b"initonetwo"
        );

        crate::manifest_writer::cleanup_destination_part(
            staging.path(),
            &video_dir,
            &destination,
            42,
            TrackKind::Video,
            "video-track.mp4",
        )
        .await
        .expect("clean destination part and nested track checkpoint");
        assert!(
            !video_dir.join("video-track.mp4.checkpoint.json").exists(),
            "track checkpoint should be removed from its track directory"
        );
        let part_path = crate::manifest_destination::part_path(
            staging.path(),
            &destination,
            42,
            TrackKind::Video,
        )
        .expect("resolve destination part after cleanup");
        let part_name = part_path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("destination part has a filename");
        assert!(
            !staging
                .path()
                .join(format!("{part_name}.owner.json"))
                .exists(),
            "destination owner record should be removed from the XDG staging root"
        );
    }

    #[tokio::test]
    async fn selected_dash_audio_representation_is_used_for_track_planning() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10S">
  <Period duration="PT10S">
    <AdaptationSet id="video" contentType="video" mimeType="video/mp4">
      <SegmentTemplate duration="5" media="video-$Number$.m4s"/>
      <Representation id="v1" bandwidth="1000000" width="1280" height="720" codecs="avc1.64001f"/>
    </AdaptationSet>
    <AdaptationSet id="audio-en" contentType="audio" mimeType="audio/mp4" lang="en">
      <Role schemeIdUri="urn:mpeg:dash:role:2011" value="main"/>
      <SegmentTemplate duration="5" media="audio-en-$Number$.m4s"/>
      <Representation id="a-en" bandwidth="128000" codecs="mp4a.40.2"/>
    </AdaptationSet>
    <AdaptationSet id="audio-fr" contentType="audio" mimeType="audio/mp4" lang="fr">
      <SegmentTemplate duration="5" media="audio-fr-$Number$.m4s"/>
      <Representation id="a-fr" bandwidth="192000" codecs="mp4a.40.2"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let manifest_url = "https://cdn.example.test/manifest.mpd";
        let dash = crate::manifest::dash::parse_dash_manifest(manifest_url, body)
            .expect("valid static DASH presentation");
        let video_key = dash.video_representations[0].key.clone();
        let main_audio_key = dash
            .default_audio()
            .expect("main DASH audio track")
            .key
            .clone();
        let alternate_audio_key = dash
            .audio_representations
            .iter()
            .find(|track| track.representation_id.as_deref() == Some("a-fr"))
            .expect("alternate language audio track")
            .key
            .clone();
        let client = reqwest::Client::new();

        let selected_source = ManifestSource {
            url: manifest_url.to_string(),
            kind: ManifestKind::Dash,
            representation_key: video_key.clone().into(),
            output_container: None,
            audio_key: Some(alternate_audio_key.into()),
            subtitle_key: None,
            subtitle_language: None,
        };
        let mut selected_hosts = crate::download_net::PublicResolutionSet::default();
        let selected = resolve_tracks(
            &client,
            ManifestLookup {
                source_url: manifest_url.to_string(),
                manifest: NativeManifest::Dash(dash.clone()),
            },
            &selected_source,
            &mut selected_hosts,
        )
        .await
        .expect("selected audio representation should be planned");
        let Tracks::Dash { audio, .. } = selected else {
            panic!("DASH manifest must resolve to DASH tracks");
        };
        assert_eq!(
            audio
                .as_deref()
                .and_then(|plan| plan.representation.representation_id.as_deref()),
            Some("a-fr"),
            "the queued audio key must select the matching representation"
        );

        let automatic_source = ManifestSource {
            audio_key: None,
            subtitle_key: None,
            ..selected_source
        };
        let mut automatic_hosts = crate::download_net::PublicResolutionSet::default();
        let automatic = resolve_tracks(
            &client,
            ManifestLookup {
                source_url: manifest_url.to_string(),
                manifest: NativeManifest::Dash(dash),
            },
            &automatic_source,
            &mut automatic_hosts,
        )
        .await
        .expect("legacy automatic audio choice should remain supported");
        let Tracks::Dash { audio, .. } = automatic else {
            panic!("DASH manifest must resolve to DASH tracks");
        };
        assert_eq!(
            audio
                .as_deref()
                .map(|plan| plan.representation.key.as_str()),
            Some(main_audio_key.as_str()),
            "queue entries without an audio key must preserve main/first fallback"
        );
        assert_eq!(
            audio
                .as_deref()
                .and_then(|plan| plan.representation.representation_id.as_deref()),
            Some("a-en")
        );
    }

    #[tokio::test]
    async fn dynamic_dash_refreshes_share_the_collection_host_limit() {
        let _policy = crate::download_net::isolate_untrusted_handoff_policy_for_test();
        assert!(crate::download_net::require_public_resolution(
            "page.example.com"
        ));

        let manifest_url = "https://page.example.com/live.mpd";
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     type="dynamic"
     availabilityStartTime="1970-01-01T00:00:00Z"
     minimumUpdatePeriod="PT1S"
     timeShiftBufferDepth="PT1S">
  <BaseURL>https://origin.example.com/</BaseURL>
  <Period>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1" duration="1"
                       media="https://cdn-$Number$.example.com/segment-$Number$.m4s"/>
      <Representation id="v1" bandwidth="1000000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

        let first = crate::manifest::dash::parse_dash_manifest_at(
            manifest_url,
            body,
            std::time::UNIX_EPOCH + Duration::from_secs(120),
        )
        .expect("initial dynamic MPD should parse");
        let representation_key = first.video_representations[0].live_key.clone();
        let source = ManifestSource {
            url: manifest_url.to_string(),
            kind: ManifestKind::Dash,
            representation_key: representation_key.clone().into(),
            output_container: None,
            audio_key: None,
            subtitle_key: None,
            subtitle_language: None,
        };
        let client = reqwest::Client::new();
        let mut guarded_child_hosts = crate::download_net::PublicResolutionSet::default();
        let limit = crate::download_net::MAX_GUARDED_CHILD_HOSTS_PER_COLLECTION;
        let mut saw_host_limit = false;

        // Every MPD uses the same representation template, but the expanded
        // segment URL rotates to a new public hostname as the live window advances.
        for refresh in 0..=limit {
            let dash = crate::manifest::dash::parse_dash_manifest_at(
                manifest_url,
                body,
                std::time::UNIX_EPOCH
                    + Duration::from_secs(
                        120 + u64::try_from(refresh).expect("refresh index fits in u64"),
                    ),
            )
            .expect("refreshed dynamic MPD should parse");
            assert_eq!(dash.video_representations[0].live_key, representation_key);
            let lookup = ManifestLookup {
                source_url: manifest_url.to_string(),
                manifest: NativeManifest::Dash(dash),
            };

            match resolve_tracks(&client, lookup, &source, &mut guarded_child_hosts).await {
                Ok(Tracks::Dash { video, .. }) => {
                    assert!(
                        video.resources.iter().any(|resource| {
                            resource.kind == crate::manifest::dash::DashResourceKind::Segment
                        }),
                        "each live refresh should expose a complete segment"
                    );
                    assert!(
                        !saw_host_limit,
                        "a collection that exceeded its host budget must remain blocked"
                    );
                }
                Err(ManifestRunError::Failed(message))
                    if message.contains("distinct resource hosts") =>
                {
                    saw_host_limit = true;
                }
                Err(error) => panic!("unexpected dynamic DASH refresh error: {error}"),
                Ok(_) => panic!("dynamic DASH refresh returned an unexpected track type"),
            }
        }

        assert!(
            saw_host_limit,
            "a rotating-host stream must hit the limit after repeated MPD refreshes"
        );
        assert_eq!(guarded_child_hosts.len(), limit);
    }

    #[test]
    fn native_manifest_ffmpeg_preflight_requires_both_executables() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = std::env::temp_dir().join(format!(
            "grab-manifest-ffmpeg-preflight-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("create FFmpeg tool directory");
        let ffmpeg = directory.join("ffmpeg");
        let ffprobe = directory.join("ffprobe");

        assert!(matches!(
            resolve_manifest_ffmpeg(std::slice::from_ref(&directory)),
            Err(ManifestRunError::Failed(_))
        ));
        std::fs::write(&ffmpeg, b"").expect("create FFmpeg executable");
        std::fs::set_permissions(&ffmpeg, std::fs::Permissions::from_mode(0o755))
            .expect("make FFmpeg executable");
        assert!(matches!(
            resolve_manifest_ffmpeg(std::slice::from_ref(&directory)),
            Err(ManifestRunError::Failed(_))
        ));
        std::fs::write(&ffprobe, b"").expect("create ffprobe executable");
        std::fs::set_permissions(&ffprobe, std::fs::Permissions::from_mode(0o755))
            .expect("make ffprobe executable");
        assert_eq!(
            resolve_manifest_ffmpeg(std::slice::from_ref(&directory)).expect("complete toolchain"),
            ffmpeg
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn run_manifest_download_rejects_a_destination_without_a_parent() {
        let job = ManifestJob {
            item_id: 7,
            source: ManifestSource {
                url: "https://cdn.example.test/master.m3u8".to_string(),
                kind: ManifestKind::Hls,
                representation_key: "hls-media".into(),
                output_container: None,
                audio_key: None,
                subtitle_key: None,
                subtitle_language: None,
            },
            referer: None,
            dest: PathBuf::new(),
            proxy: None,
            cookies_browser: "none".to_string(),
            connections: 1,
            embed_subs: false,
        };
        let gate = crate::attempt_gate::AttemptGate::new();
        let (_stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let result = run_manifest_download(job, gate, stop_rx, tx).await;

        assert!(matches!(result, Err(ManifestRunError::Failed(_))));
    }

    #[test]
    fn manifest_child_guard_rejects_private_hosts_for_untrusted_sources() {
        let _test_policy = crate::download_net::isolate_untrusted_handoff_policy_for_test();
        let parent_url = "https://manifest-private-guard.fixture.invalid/master.m3u8";
        assert!(crate::download_net::require_public_resolution(
            "manifest-private-guard.fixture.invalid"
        ));

        let child_url = Url::parse("https://127.0.0.1/segment.m4s")
            .expect("private manifest child URL is valid");
        let mut seen_hosts = crate::download_net::PublicResolutionSet::default();
        let error = guard_manifest_child_url(parent_url, &child_url, &mut seen_hosts)
            .expect_err("untrusted manifests must not reach private hosts");

        assert_eq!(error, ManifestChildUrlError::UnsafeDestination);
        assert_eq!(
            error.to_string(),
            "The stream manifest references a local or private network resource"
        );
        assert_eq!(
            seen_hosts.len(),
            0,
            "rejected hosts must not consume a slot"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn parsed_manifest_child_is_dns_filtered_before_resource_connection() {
        use tokio::time::{Duration, timeout};

        let _test_policy = crate::download_net::isolate_untrusted_handoff_policy_for_test();
        let parent_url = "https://manifest-origin.example.test/master.m3u8";
        assert!(crate::download_net::require_public_resolution(
            "manifest-origin.example.test"
        ));

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind hostile DNS fixture");
        let port = listener
            .local_addr()
            .expect("read hostile DNS fixture port")
            .port();
        let media_url = format!("https://cdn.example.test:{port}/media.m3u8?sig=media");
        let media_body = b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXTINF:1.0,\nsegment.ts?sig=segment\n#EXT-X-ENDLIST\n";
        let plan = crate::manifest::plan_hls_media_playlist(&media_url, media_body)
            .expect("parse HLS media playlist fixture");
        let child = &plan.resources[0].url;
        let mut seen_hosts = crate::download_net::PublicResolutionSet::default();
        guard_manifest_child_url(parent_url, child, &mut seen_hosts)
            .expect("register the manifest-derived segment host");
        assert_eq!(seen_hosts.len(), 1);

        let (client, dns_override_hits) =
            crate::download_net::http_client_for_with_dns_overrides_for_test(
                None,
                std::collections::HashMap::from([(
                    "cdn.example.test".to_string(),
                    vec![
                        format!("127.0.0.1:{port}")
                            .parse()
                            .expect("private loopback address"),
                    ],
                )]),
            )
            .expect("build the production guarded HTTP client with fixture DNS");

        let child_url = (*child).clone();
        let mut request = tokio::spawn(async move { client.get(child_url).send().await });
        match timeout(Duration::from_millis(300), listener.accept()).await {
            Ok(Ok((socket, _))) => {
                drop(socket);
                request.abort();
                panic!("a manifest-derived resource connected to a private DNS answer");
            }
            Ok(Err(error)) => {
                request.abort();
                panic!("the resolver fixture failed to accept: {error}");
            }
            Err(_) => {}
        }
        let response = match timeout(Duration::from_secs(2), &mut request).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => panic!("manifest resource request task failed: {error}"),
            Err(_) => {
                request.abort();
                panic!("manifest resource request did not finish after DNS rejection");
            }
        };
        assert!(dns_override_hits.load(std::sync::atomic::Ordering::Relaxed));
        assert!(
            response.is_err(),
            "a public manifest's child hostname must not connect when DNS resolves privately"
        );
    }

    #[test]
    fn manifest_child_guard_enforces_the_distinct_host_limit() {
        let _test_policy = crate::download_net::isolate_untrusted_handoff_policy_for_test();
        let parent_url = "https://manifest-limit-origin.fixture.invalid/master.m3u8";
        assert!(crate::download_net::require_public_resolution(
            "manifest-limit-origin.fixture.invalid"
        ));

        let mut seen_hosts = crate::download_net::PublicResolutionSet::default();
        for index in 0..crate::download_net::MAX_GUARDED_CHILD_HOSTS_PER_COLLECTION {
            let child_url = Url::parse(&format!("https://cdn-{index}.fixture.invalid/segment.m4s"))
                .expect("public manifest child URL is valid");
            guard_manifest_child_url(parent_url, &child_url, &mut seen_hosts)
                .expect("each child host below the limit should be accepted");
        }

        let overflow_url = Url::parse("https://overflow.fixture.invalid/segment.m4s")
            .expect("overflow manifest child URL is valid");
        let error = guard_manifest_child_url(parent_url, &overflow_url, &mut seen_hosts)
            .expect_err("the manifest host limit must fail closed");

        assert_eq!(
            error,
            ManifestChildUrlError::TooManyDistinctHosts(
                crate::download_net::MAX_GUARDED_CHILD_HOSTS_PER_COLLECTION
            )
        );
        assert_eq!(
            error.to_string(),
            format!(
                "The stream manifest exceeds the limit of {} distinct resource hosts",
                crate::download_net::MAX_GUARDED_CHILD_HOSTS_PER_COLLECTION
            )
        );
        assert_eq!(
            seen_hosts.len(),
            crate::download_net::MAX_GUARDED_CHILD_HOSTS_PER_COLLECTION
        );
        assert!(
            !crate::download_net::is_untrusted_handoff_host("overflow.fixture.invalid"),
            "a rejected child host must not enter the public-only policy"
        );
    }

    #[test]
    fn only_http_rate_limits_and_server_errors_are_retryable_for_resources() {
        assert!(matches!(
            resource_status_error(reqwest::StatusCode::REQUEST_TIMEOUT),
            ManifestRunError::Retryable(_)
        ));
        assert!(matches!(
            resource_status_error(reqwest::StatusCode::TOO_MANY_REQUESTS),
            ManifestRunError::Retryable(_)
        ));
        assert!(matches!(
            resource_status_error(reqwest::StatusCode::SERVICE_UNAVAILABLE),
            ManifestRunError::Retryable(_)
        ));
        assert!(matches!(
            resource_status_error(reqwest::StatusCode::NOT_FOUND),
            ManifestRunError::Unsupported(_)
        ));
        assert!(matches!(
            resource_status_error(reqwest::StatusCode::FORBIDDEN),
            ManifestRunError::Unsupported(_)
        ));
    }

    #[test]
    fn classified_errors_choose_fallback_retry_or_terminal_failure() {
        assert!(matches!(
            ManifestRunError::Unsupported("unsupported".to_string()).into_engine_message(),
            EngineMsg::ManifestFallback(_)
        ));
        assert!(matches!(
            ManifestRunError::Retryable("temporary".to_string()).into_engine_message(),
            EngineMsg::ManifestRetryable(_)
        ));
        assert!(matches!(
            ManifestRunError::Failed("local".to_string()).into_engine_message(),
            EngineMsg::Failed(_)
        ));
    }

    #[tokio::test]
    async fn transient_resource_response_fails_without_retrying() {
        use std::sync::atomic::AtomicUsize;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let directory =
            std::env::temp_dir().join(format!("grab-manifest-no-retry-{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&directory).await;
        tokio::fs::create_dir_all(&directory)
            .await
            .expect("create resource staging directory");

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind HTTP test listener");
        let address = listener.local_addr().expect("listener address");
        let requests = Arc::new(AtomicUsize::new(0));
        let server_requests = Arc::clone(&requests);
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept resource request");
            server_requests.fetch_add(1, Ordering::Relaxed);
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await.expect("read request");
            socket
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("write response");
            // A second request would indicate that the resource was retried.
            if let Ok(Ok((mut retry_socket, _))) =
                tokio::time::timeout(Duration::from_secs(1), listener.accept()).await
            {
                server_requests.fetch_add(1, Ordering::Relaxed);
                let mut retry_request = [0_u8; 1024];
                let _ = retry_socket.read(&mut retry_request).await;
            }
        });

        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build direct HTTP test client");
        let resource = Resource {
            url: Url::parse(&format!("http://{address}/segment.m4s")).expect("resource URL"),
            range: None,
            filename: "segment-000001-0123456789abcdef0123456789abcdef.bin".to_string(),
            key: false,
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let downloaded = AtomicU64::new(0);
        let result = fetch_resource(&client, resource.clone(), &directory, &tx, &downloaded).await;

        server.await.expect("HTTP test server task");
        assert!(matches!(result, Err(ManifestRunError::Retryable(_))));
        assert_eq!(requests.load(Ordering::Relaxed), 1);
        assert_eq!(downloaded.load(Ordering::Relaxed), 0);
        assert!(!directory.join(&resource.filename).exists());
        assert!(
            !directory
                .join("segment-000001-0123456789abcdef0123456789abcdef.bin.part")
                .exists()
        );
        let _ = tokio::fs::remove_dir_all(directory).await;
    }

    #[tokio::test]
    async fn partial_content_without_a_requested_range_is_rejected() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let directory = std::env::temp_dir().join(format!(
            "grab-manifest-unrequested-206-{}",
            std::process::id()
        ));
        let _ = tokio::fs::remove_dir_all(&directory).await;
        tokio::fs::create_dir_all(&directory)
            .await
            .expect("create resource staging directory");

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind HTTP test listener");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept resource request");
            let mut request = [0_u8; 1024];
            let _ = socket
                .read(&mut request)
                .await
                .expect("read resource request");
            let response = "HTTP/1.1 206 Partial Content\r\nContent-Length: 2\r\nContent-Range: bytes 0-1/4\r\nConnection: close\r\n\r\npa";
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write partial response");
        });

        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build direct HTTP test client");
        let resource = Resource {
            url: Url::parse(&format!("http://{address}/complete.m4s")).expect("resource URL"),
            range: None,
            filename: "complete-000001-0123456789abcdef0123456789abcdef.bin".to_string(),
            key: false,
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let downloaded = AtomicU64::new(0);
        let result = fetch_resource(&client, resource.clone(), &directory, &tx, &downloaded).await;

        server.await.expect("HTTP test server task");
        assert!(matches!(result, Err(ManifestRunError::Unsupported(_))));
        assert_eq!(downloaded.load(Ordering::Relaxed), 0);
        assert!(!directory.join(resource.filename).exists());
        assert!(
            !directory
                .join("complete-000001-0123456789abcdef0123456789abcdef.bin.part")
                .exists()
        );
        let _ = tokio::fs::remove_dir_all(directory).await;
    }

    #[tokio::test]
    async fn unrecoverable_capture_marker_is_synced_before_capture_returns() {
        let directory = dynamic_dash_test_directory("unrecoverable-marker");
        tokio::fs::create_dir_all(&directory)
            .await
            .expect("create live staging directory");
        let downloaded = AtomicU64::new(900);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        persist_unrecoverable_live_capture(
            &directory,
            ManifestKind::Hls,
            "HLS playlist continuity could not be verified",
            420,
            &downloaded,
            &tx,
        )
        .await
        .expect("persist unrecoverable marker before returning");

        let marker = crate::live_capture_state::read(&directory)
            .await
            .expect("read durable marker")
            .expect("unrecoverable marker exists");
        assert_eq!(marker.protocol, ManifestKind::Hls);
        assert_eq!(marker.staged_bytes, 420);
        assert_eq!(downloaded.load(Ordering::Relaxed), 420);
        assert!(matches!(
            rx.try_recv(),
            Ok(EngineMsg::Progress {
                downloaded: 420,
                ..
            })
        ));
        let _ = tokio::fs::remove_dir_all(directory).await;
    }

    #[test]
    fn dash_audio_continuity_loss_without_video_is_discarded() {
        assert_eq!(
            discard_unrecoverable_dash_capture_without_video(
                DashLiveCaptureOutcome::Unrecoverable,
                false,
            ),
            DashLiveCaptureOutcome::Discard
        );
        assert_eq!(
            discard_unrecoverable_dash_capture_without_video(
                DashLiveCaptureOutcome::Unrecoverable,
                true,
            ),
            DashLiveCaptureOutcome::Unrecoverable
        );
    }

    #[test]
    fn manifest_diagnostic_url_omits_signed_path_query_and_fragment() {
        let url = Url::parse(
            "https://cdn.example.test/private/path?signature=very-secret#session=also-secret",
        )
        .expect("test URL");
        let diagnostic = diagnostic_url(&url);
        assert_eq!(diagnostic, "https://cdn.example.test");
        assert!(!diagnostic.contains("private"));
        assert!(!diagnostic.contains("very-secret"));
        assert!(!diagnostic.contains("also-secret"));
    }

    #[tokio::test]
    async fn request_error_diagnostics_do_not_include_signed_url() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind then close a local endpoint");
        let address = listener.local_addr().expect("listener address");
        drop(listener);
        let url = format!("http://{address}/segment?signature=very-secret");
        let error = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build HTTP client")
            .get(&url)
            .send()
            .await
            .expect_err("closed endpoint should fail");
        let diagnostic = error.without_url().to_string();
        assert!(!diagnostic.contains("very-secret"));
    }

    #[tokio::test]
    async fn truncated_byte_range_fails_without_retrying_or_counting_partial_bytes() {
        use std::sync::atomic::AtomicUsize;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let directory = std::env::temp_dir().join(format!(
            "grab-manifest-range-no-retry-{}",
            std::process::id()
        ));
        let _ = tokio::fs::remove_dir_all(&directory).await;
        tokio::fs::create_dir_all(&directory)
            .await
            .expect("create range staging directory");

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind HTTP test listener");
        let address = listener.local_addr().expect("listener address");
        let requests = Arc::new(AtomicUsize::new(0));
        let server_requests = Arc::clone(&requests);
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept range request");
            server_requests.fetch_add(1, Ordering::Relaxed);
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await.expect("read range request");
            socket
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 0-3/4\r\nConnection: close\r\n\r\nda",
                )
                .await
                .expect("write truncated range response");
            if let Ok(Ok((mut retry_socket, _))) =
                tokio::time::timeout(Duration::from_secs(1), listener.accept()).await
            {
                server_requests.fetch_add(1, Ordering::Relaxed);
                let mut retry_request = [0_u8; 1024];
                let _ = retry_socket.read(&mut retry_request).await;
            }
        });

        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build direct HTTP test client");
        let resource = Resource {
            url: Url::parse(&format!("http://{address}/range.m4s")).expect("range resource URL"),
            range: Some(ManifestByteRange {
                offset: 0,
                length: 4,
            }),
            filename: "range-000001-0123456789abcdef0123456789abcdef.bin".to_string(),
            key: false,
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let downloaded = AtomicU64::new(0);
        let result = fetch_resource(&client, resource.clone(), &directory, &tx, &downloaded).await;

        server.await.expect("HTTP test server task");
        assert!(matches!(result, Err(ManifestRunError::Retryable(_))));
        assert_eq!(requests.load(Ordering::Relaxed), 1);
        assert_eq!(downloaded.load(Ordering::Relaxed), 0);
        assert!(!directory.join(&resource.filename).exists());
        let partial = directory.join(format!("{}.part", resource.filename));
        let partial_metadata = tokio::fs::metadata(&partial)
            .await
            .expect("incomplete transfer remains isolated in its staging file");
        assert_eq!(partial_metadata.len(), 2);
        let _ = tokio::fs::remove_dir_all(directory).await;
    }

    #[test]
    fn failed_resource_attempt_refunds_its_reserved_bytes() {
        let downloaded = AtomicU64::new(0);
        {
            let mut reservation = ResourceByteReservation::new(&downloaded);
            assert_eq!(reservation.reserve(16).expect("reserve bytes"), 16);
            assert_eq!(downloaded.load(Ordering::Relaxed), 16);
        }
        assert_eq!(downloaded.load(Ordering::Relaxed), 0);

        {
            let mut reservation = ResourceByteReservation::new(&downloaded);
            assert_eq!(reservation.reserve(8).expect("reserve bytes"), 8);
            reservation.commit();
        }
        assert_eq!(downloaded.load(Ordering::Relaxed), 8);
    }

    fn dynamic_dash_test_directory(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock must follow the Unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "grab-dash-live-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn dynamic_dash_finalization_resumes_from_ordered_writer_checkpoint() {
        let directory = dynamic_dash_test_directory("finalize-resume");
        tokio::fs::create_dir_all(&directory)
            .await
            .expect("create dynamic DASH finalization directory");
        let resources = vec![
            "dash-live-init-0123456789abcdef0123456789abcdef.bin".to_string(),
            "dash-live-segment-0123456789abcdef0123456789abcdef.bin".to_string(),
        ];
        tokio::fs::write(directory.join(&resources[0]), b"init")
            .await
            .expect("write initialization resource");
        tokio::fs::write(directory.join(&resources[1]), b"segment")
            .await
            .expect("write media resource");

        let (_stop_tx, mut abort) = tokio::sync::oneshot::channel();
        let (Some(mut writer), committed) =
            crate::manifest_writer::OrderedTrackWriter::open_retaining_checkpoint(
                resources.clone(),
                &directory,
                "video-track.mp4",
            )
            .await
            .expect("open live DASH ordered writer")
        else {
            panic!("a fresh live DASH track must return an active writer");
        };
        assert_eq!(committed, 0);
        assert!(
            writer
                .append_next(&mut abort)
                .await
                .expect("commit initialization")
        );
        drop(writer);

        assert!(
            concat_dash_resource_names(&resources, &directory, "video-track.mp4", &mut abort,)
                .await
                .expect("resume live DASH finalization")
        );
        assert_eq!(
            tokio::fs::read(directory.join("video-track.mp4"))
                .await
                .expect("read finalized live DASH track"),
            b"initsegment"
        );
        assert!(directory.join("video-track.mp4.checkpoint.json").exists());
        assert!(!directory.join(&resources[0]).exists());
        assert!(!directory.join(&resources[1]).exists());
        tokio::fs::remove_dir_all(directory)
            .await
            .expect("remove dynamic DASH finalization directory");
    }

    #[test]
    fn dynamic_dash_resume_state_rejects_path_traversal_and_sequence_gaps() {
        let key = "dash-live-0123456789abcdef0123456789abcdef";
        let valid = DashLiveState {
            version: DASH_LIVE_STATE_VERSION,
            representation_key: key.to_string().into(),
            initialization: Some("dash-live-init-0123456789abcdef0123456789abcdef.bin".to_string()),
            segments: vec![
                DashLiveSegment {
                    number: 100,
                    filename: "dash-live-segment-0123456789abcdef0123456789abcdef.bin".to_string(),
                },
                DashLiveSegment {
                    number: 101,
                    filename: "dash-live-segment-1123456789abcdef0123456789abcdef.bin".to_string(),
                },
            ],
        };
        assert!(validate_dash_live_state(&valid, key).is_ok());

        let mut traversal = valid.clone();
        traversal.segments[0].filename = "../outside.bin".to_string();
        assert!(validate_dash_live_state(&traversal, key).is_err());

        let mut gap = valid.clone();
        gap.segments[1].number = 102;
        assert!(validate_dash_live_state(&gap, key).is_err());

        assert!(
            validate_dash_live_state(&valid, "dash-live-ffffffffffffffffffffffffffffffff").is_err()
        );
    }

    #[tokio::test]
    async fn dynamic_dash_resume_state_persists_resource_order_and_restores_byte_total() {
        let directory = dynamic_dash_test_directory("restore");
        tokio::fs::create_dir_all(&directory)
            .await
            .expect("create dynamic DASH state directory");
        let representation_key = "dash-live-0123456789abcdef0123456789abcdef";
        let initialization = "dash-live-init-0123456789abcdef0123456789abcdef.bin";
        let first = "dash-live-segment-0123456789abcdef0123456789abcdef.bin";
        let second = "dash-live-segment-1123456789abcdef0123456789abcdef.bin";
        tokio::fs::write(directory.join(initialization), b"init")
            .await
            .expect("write staged initialization");
        tokio::fs::write(directory.join(first), b"segment-100")
            .await
            .expect("write first staged segment");
        tokio::fs::write(directory.join(second), b"segment-101")
            .await
            .expect("write second staged segment");

        let track = DashLiveTrack {
            directory: directory.clone(),
            committed_resource_count: 0,
            committed_bytes: 0,
            destination_part: None,
            state: DashLiveState {
                version: DASH_LIVE_STATE_VERSION,
                representation_key: ManifestRepresentationKey::from(representation_key),
                initialization: Some(initialization.to_string()),
                segments: vec![
                    DashLiveSegment {
                        number: 100,
                        filename: first.to_string(),
                    },
                    DashLiveSegment {
                        number: 101,
                        filename: second.to_string(),
                    },
                ],
            },
            track_filename: "video-track.mp4",
        };
        write_dash_live_state(&track)
            .await
            .expect("persist dynamic DASH resume state");

        let saved = tokio::fs::read(directory.join(DASH_LIVE_STATE_NAME))
            .await
            .expect("read dynamic DASH state file");
        let saved_text = String::from_utf8(saved).expect("DASH state is JSON text");
        assert!(!saved_text.contains("https://"));
        assert_eq!(
            read_dash_live_representation_key(&directory)
                .await
                .expect("read persisted representation key"),
            representation_key
        );

        let (restored, total_bytes) =
            load_dash_live_track(representation_key, &directory, None, "video-track.mp4")
                .await
                .expect("restore persisted dynamic DASH track");
        assert_eq!(restored.state, track.state);
        assert_eq!(
            restored.state.resource_names().collect::<Vec<_>>(),
            [initialization, first, second]
        );
        assert_eq!(
            total_bytes,
            (b"init".len() + b"segment-100".len() + b"segment-101".len()) as u64
        );
        assert_eq!(
            dash_live_track_bytes(
                &restored.state,
                &directory,
                restored.committed_resource_count,
                restored.committed_bytes
            )
            .await
            .expect("sum restored staged resources"),
            total_bytes
        );

        let resource_names = track
            .state
            .resource_names()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let (_stop_tx, mut abort) = tokio::sync::oneshot::channel();
        let (Some(mut writer), _) =
            crate::manifest_writer::OrderedTrackWriter::open_retaining_checkpoint(
                resource_names.clone(),
                &directory,
                "video-track.mp4",
            )
            .await
            .expect("open live DASH track writer")
        else {
            panic!("a fresh live DASH track must return an active writer");
        };
        assert!(
            writer
                .append_next(&mut abort)
                .await
                .expect("append first live DASH resource")
        );
        drop(writer);

        let (restored_partial, partial_bytes) =
            load_dash_live_track(representation_key, &directory, None, "video-track.mp4")
                .await
                .expect("restore a partial track after source cleanup");
        assert_eq!(restored_partial.state, track.state);
        assert_eq!(partial_bytes, total_bytes);
        assert_eq!(
            dash_live_track_bytes(
                &restored_partial.state,
                &directory,
                restored_partial.committed_resource_count,
                restored_partial.committed_bytes
            )
            .await
            .expect("sum partially committed track bytes"),
            total_bytes
        );

        let (Some(mut writer), _) =
            crate::manifest_writer::OrderedTrackWriter::open_retaining_checkpoint(
                resource_names.clone(),
                &directory,
                "video-track.mp4",
            )
            .await
            .expect("resume partially committed live DASH track")
        else {
            panic!("an interrupted live DASH track must return an active writer");
        };
        while writer.next_resource_index() < resource_names.len() {
            assert!(
                writer
                    .append_next(&mut abort)
                    .await
                    .expect("append remaining live DASH resource")
            );
        }
        writer.finish().await.expect("publish live DASH track");
        crate::manifest_writer::cleanup_committed_resources(
            &resource_names,
            &directory,
            "video-track.mp4",
        )
        .await
        .expect("remove redundant live DASH resources");

        let (restored_without_sources, restored_bytes) =
            load_dash_live_track(representation_key, &directory, None, "video-track.mp4")
                .await
                .expect("restore live DASH state from the verified track checkpoint");
        assert_eq!(restored_without_sources.state, track.state);
        assert_eq!(restored_bytes, total_bytes);
        assert_eq!(
            dash_live_track_bytes(
                &restored_without_sources.state,
                &directory,
                restored_without_sources.committed_resource_count,
                restored_without_sources.committed_bytes
            )
            .await
            .expect("sum verified track bytes after source cleanup"),
            total_bytes
        );

        tokio::fs::remove_dir_all(directory)
            .await
            .expect("remove dynamic DASH test directory");
    }

    #[test]
    fn content_range_must_match_exact_requested_range_and_total() {
        let range = ManifestByteRange {
            offset: 16,
            length: 8,
        };
        assert!(content_range_matches(Some("bytes 16-23/64"), range));
        assert!(!content_range_matches(Some("bytes 0-7/64"), range));
        assert!(!content_range_matches(Some("bytes 16-22/64"), range));
        assert!(!content_range_matches(Some("bytes 16-23/20"), range));
        assert!(!content_range_matches(Some("bytes 16-23/*"), range));
        assert!(!content_range_matches(Some("bytes */64"), range));
    }

    #[test]
    fn resource_names_must_be_single_safe_path_components() {
        assert!(safe_resource_filename("segment-000001.bin"));
        assert!(!safe_resource_filename("../segment.bin"));
        assert!(!safe_resource_filename("nested/segment.bin"));
        assert!(!safe_resource_filename(""));
    }
}

#[cfg(test)]
#[path = "manifest_runner_integration_tests.rs"]
mod integration_tests;

#[cfg(test)]
#[path = "manifest_runner_sample_aes_tests.rs"]
mod sample_aes_tests;

#[cfg(test)]
mod staged_file_inspection_tests {
    use super::*;

    #[tokio::test]
    async fn staged_file_inspection_distinguishes_missing_regular_and_nonregular_paths() {
        let directory = tempfile::tempdir().expect("create staging directory");
        let missing = directory.path().join("missing");
        assert!(
            inspect_staged_regular_file(&missing)
                .await
                .expect("inspect missing staged file")
                .is_none()
        );

        let regular = directory.path().join("regular");
        tokio::fs::write(&regular, b"staged")
            .await
            .expect("write regular staged file");
        assert!(
            inspect_staged_regular_file(&regular)
                .await
                .expect("inspect regular staged file")
                .is_some()
        );

        let nonregular = directory.path().join("directory");
        tokio::fs::create_dir(&nonregular)
            .await
            .expect("create non-regular staged path");
        assert!(matches!(
            inspect_staged_regular_file(&nonregular).await,
            Err(StagedFileError::NotRegularFile)
        ));
    }
}

#[cfg(test)]
mod staged_resource_size_tests {
    use super::*;

    #[test]
    fn staged_resource_size_validation_covers_bounds_ranges_and_keys() {
        assert!(staged_resource_size_ok(1, None, false));
        assert!(!staged_resource_size_ok(0, None, false));
        assert!(!staged_resource_size_ok(
            MAX_RESOURCE_BYTES + 1,
            None,
            false
        ));

        let range = ManifestByteRange {
            offset: 16,
            length: 32,
        };
        assert!(staged_resource_size_ok(32, Some(range), false));
        assert!(!staged_resource_size_ok(31, Some(range), false));

        assert!(staged_resource_size_ok(16, None, true));
        assert!(!staged_resource_size_ok(15, None, true));
    }
}

#[cfg(test)]
mod staged_file_atomic_tests {
    use super::*;

    #[tokio::test]
    async fn replace_publishes_new_contents_without_a_missing_target_window() {
        let directory = tempfile::tempdir().expect("create staging directory");
        let target = directory.path().join("local.m3u8");
        tokio::fs::write(&target, b"old playlist")
            .await
            .expect("write existing playlist");

        write_staged_file_atomic(
            directory.path(),
            "local.m3u8.part",
            "local.m3u8",
            b"new playlist",
            StagedPublish::Replace,
        )
        .await
        .expect("atomically replace playlist");

        assert_eq!(
            tokio::fs::read(&target)
                .await
                .expect("read published playlist"),
            b"new playlist"
        );
        assert!(
            tokio::fs::symlink_metadata(directory.path().join("local.m3u8.part"))
                .await
                .is_err(),
            "the temporary file must be renamed away"
        );
    }

    #[tokio::test]
    async fn no_replace_keeps_existing_target_and_removes_temporary_file() {
        let directory = tempfile::tempdir().expect("create staging directory");
        let target = directory.path().join("marker");
        tokio::fs::write(&target, b"existing marker")
            .await
            .expect("write existing marker");

        assert!(
            write_staged_file_atomic(
                directory.path(),
                "marker.part",
                "marker",
                b"new marker",
                StagedPublish::NoReplace,
            )
            .await
            .is_err(),
            "no-replace publication must reject an existing target"
        );
        assert_eq!(
            tokio::fs::read(&target)
                .await
                .expect("read existing marker"),
            b"existing marker"
        );
        assert!(
            tokio::fs::symlink_metadata(directory.path().join("marker.part"))
                .await
                .is_err(),
            "failed publication must clean its temporary file"
        );
    }
}

#[cfg(test)]
mod manifest_preparation_stop_tests {
    use super::*;

    #[tokio::test]
    async fn stop_intent_drops_pending_manifest_lookup() {
        struct DropNotice(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for DropNotice {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }

        let (stop_sender, stop_receiver) = tokio::sync::oneshot::channel();
        let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
        let (dropped_sender, dropped_receiver) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut abort = stop_receiver;
            let operation = async move {
                let _drop_notice = DropNotice(Some(dropped_sender));
                let _ = started_sender.send(());
                std::future::pending::<()>().await;
            };
            await_manifest_operation_or_stop(&mut abort, operation).await
        });

        started_receiver.await.expect("manifest lookup must start");
        stop_sender
            .send(crate::video::StopIntent::Pause)
            .expect("manifest preparation receiver must remain active");
        assert_eq!(
            task.await.expect("stop race task must complete"),
            Err(crate::video::StopIntent::Pause)
        );
        dropped_receiver
            .await
            .expect("stop intent must drop the pending manifest lookup");
    }
}

#[cfg(test)]
mod ffmpeg_rejected_input_tests {
    use super::ffmpeg_rejected_input;

    #[test]
    fn recognizes_input_demuxer_and_container_header_rejections() {
        for message in [
            "Invalid data found when processing input",
            "moov atom not found",
            "EBML header parsing failed",
            "Error reading header",
            "Could not find codec parameters for stream 0",
        ] {
            assert!(ffmpeg_rejected_input(message), "message: {message}");
        }
    }

    #[test]
    fn does_not_classify_output_or_storage_failures_as_input_rejections() {
        assert!(!ffmpeg_rejected_input("No space left on device"));
        assert!(!ffmpeg_rejected_input("Could not write header"));
        assert!(!ffmpeg_rejected_input("Unknown encoder"));
    }
}

#[cfg(test)]
mod byte_range_response_diagnostics_tests {
    use super::{ManifestRunError, validate_byte_range_response};
    use crate::manifest::ManifestByteRange;

    const REQUESTED: ManifestByteRange = ManifestByteRange {
        offset: 10,
        length: 5,
    };

    #[test]
    fn reports_http_status_for_rejected_range_requests() {
        let forbidden = validate_byte_range_response(
            reqwest::StatusCode::FORBIDDEN,
            None,
            REQUESTED,
            "a byte-range segment",
        )
        .expect_err("403 must reject the native range transfer");
        assert!(matches!(
            forbidden,
            ManifestRunError::Unsupported(message)
                if message.contains("HTTP 403") && message.contains("bytes 10-14")
        ));

        let ignored = validate_byte_range_response(
            reqwest::StatusCode::OK,
            None,
            REQUESTED,
            "a byte-range segment",
        )
        .expect_err("200 means the server ignored the Range request");
        assert!(matches!(
            ignored,
            ManifestRunError::Unsupported(message)
                if message.contains("HTTP 200") && message.contains("expected HTTP 206")
        ));
    }

    #[test]
    fn preserves_retry_classification_for_transient_range_statuses() {
        let error = validate_byte_range_response(
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            None,
            REQUESTED,
            "an HLS byte range",
        )
        .expect_err("503 must remain retryable");
        assert!(matches!(
            error,
            ManifestRunError::Retryable(message)
                if message.contains("HTTP 503") && message.contains("bytes 10-14")
        ));
    }

    #[test]
    fn distinguishes_missing_and_mismatched_content_range() {
        let missing = validate_byte_range_response(
            reqwest::StatusCode::PARTIAL_CONTENT,
            None,
            REQUESTED,
            "a byte-range segment",
        )
        .expect_err("206 without Content-Range is invalid");
        assert!(matches!(
            missing,
            ManifestRunError::Unsupported(message)
                if message.contains("HTTP 206") && message.contains("missing Content-Range")
        ));

        let mismatched = validate_byte_range_response(
            reqwest::StatusCode::PARTIAL_CONTENT,
            Some("bytes 0-4/100"),
            REQUESTED,
            "a byte-range segment",
        )
        .expect_err("206 for a different range is invalid");
        assert!(matches!(
            mismatched,
            ManifestRunError::Unsupported(message)
                if message.contains("HTTP 206") && message.contains("invalid or mismatched Content-Range")
        ));
    }

    #[test]
    fn accepts_matching_content_range() {
        assert!(
            validate_byte_range_response(
                reqwest::StatusCode::PARTIAL_CONTENT,
                Some("bytes 10-14/100"),
                REQUESTED,
                "a byte-range segment",
            )
            .is_ok()
        );
    }
}
