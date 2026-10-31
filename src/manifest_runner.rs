//! Native HLS/DASH segment transfer, resume and container finalization.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use futures_util::{StreamExt as _, TryStreamExt as _};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
use url::Url;

use crate::engine_msg::EngineMsg;
use crate::manifest::{
    HlsLiveAccumulator, HlsPlan, HlsResource, HlsResourceKind, ManifestByteRange,
    dash::{DashPresentation, DashRepresentationPlan, DashResource, DashResourceKind},
};
use crate::manifest_intake::{ManifestLookup, NativeManifest};
use crate::media_types::{ManifestKind, ManifestSource};

/// Upper bound for one media resource, independent of a server-supplied length.
const MAX_RESOURCE_BYTES: u64 = 1024 * 1024 * 1024;
/// Aggregate resource ceiling, matching the regular segmented downloader's 1 TiB trust limit.
const MAX_MANIFEST_DOWNLOAD_BYTES: u64 = 1 << 40;
/// Idle budget for receiving a response body; progressing transfers are not wall-clock limited.
const RESOURCE_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound parallel manifest requests to the same range as the regular HTTP engine.
const MAX_RESOURCE_CONNECTIONS: usize = 16;
/// Keep only a small diagnostic tail from FFmpeg.
const FFMPEG_LOG_LINES: usize = 32;
/// Maximum requests for one transiently failing resource before preserving staging for retry.
const MAX_RESOURCE_ATTEMPTS: u32 = 3;
/// Short linear backoff between transient resource requests.
const RESOURCE_RETRY_BASE_DELAY: Duration = Duration::from_millis(250);
/// Upper bound on the persisted dynamic DASH segment index.
const MAX_DASH_LIVE_SEGMENTS: usize = 100_000;
/// Bound for a serialized dynamic DASH resume index.
const MAX_DASH_LIVE_STATE_BYTES: usize = 16 * 1024 * 1024;

/// Inputs retained by the queue; URLs for individual segments are resolved again on every attempt.
#[derive(Clone, Debug)]
pub(crate) struct ManifestJob {
    pub(crate) item_id: u64,
    pub(crate) source: ManifestSource,
    pub(crate) dest: PathBuf,
    pub(crate) proxy: Option<crate::net_types::ResolvedProxy>,
    pub(crate) cookies_browser: String,
    pub(crate) connections: usize,
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

/// Result of one native manifest attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ManifestOutcome {
    Finished(u64),
    Aborted,
}

#[derive(Clone, Debug)]
struct Resource {
    url: Url,
    range: Option<ManifestByteRange>,
    filename: String,
    key: bool,
}

enum Tracks {
    Hls {
        video: HlsPlan,
        audio: Option<HlsPlan>,
    },
    Dash {
        video: DashRepresentationPlan,
        audio: Option<Box<DashRepresentationPlan>>,
    },
    DashLiveOffline {
        has_audio: bool,
    },
}

#[derive(Debug)]
struct LiveHlsTrack {
    playlist_url: Url,
    directory: PathBuf,
    accumulator: HlsLiveAccumulator,
    snapshot_at: Instant,
    ended: bool,
    next_poll_at: Option<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LiveHlsCaptureOutcome {
    Ended,
    Preserve,
    PreserveVideoOnly,
    Discard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResourceFetchOutcome {
    Complete,
    Stopped(crate::video::StopIntent),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LiveHlsSnapshotOutcome {
    Committed,
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
    representation_key: String,
    initialization: Option<String>,
    segments: Vec<DashLiveSegment>,
}

impl DashLiveState {
    fn new(representation_key: &str) -> Self {
        Self {
            version: 1,
            representation_key: representation_key.to_string(),
            initialization: None,
            segments: Vec::new(),
        }
    }

    fn resource_names(&self) -> impl Iterator<Item = &str> {
        self.initialization
            .iter()
            .map(String::as_str)
            .chain(self.segments.iter().map(|segment| segment.filename.as_str()))
    }

    fn next_number(&self) -> Option<u64> {
        self.segments
            .last()
            .and_then(|segment| segment.number.checked_add(1))
    }
}

#[derive(Debug)]
struct DashLiveTrack {
    directory: PathBuf,
    state: DashLiveState,
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
    Preserve,
    PreserveVideoOnly,
    Discard,
}

/// Run one HLS or static-DASH attempt, keeping completed resources for retry.
///
/// # Errors
/// Returns a classified error. Only unsupported presentations use yt-dlp fallback;
/// transient transfer errors preserve completed resources for a later native retry.
pub(crate) async fn run_manifest_download(
    job: ManifestJob,
    gate: Arc<crate::attempt_gate::AttemptGate>,
    mut abort: tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
    tx: tokio::sync::mpsc::UnboundedSender<EngineMsg>,
) -> Result<ManifestOutcome, ManifestRunError> {
    let Some(dest_dir) = job.dest.parent() else {
        return Err(ManifestRunError::Failed(
            "The download destination has no parent directory".to_string(),
        ));
    };
    let parent = dest_dir.to_path_buf();
    let item_id = job.item_id;
    let staging = tokio::task::spawn_blocking(move || {
        crate::video_staging::ensure_manifest_staging_dir(&parent, item_id)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("Could not prepare manifest staging: {error}"))??;

    let video_dir = staging.join("video");
    ensure_track_dir(&video_dir).await?;
    let audio_dir = staging.join("audio");
    let live_marker_exists = read_live_hls_marker(&staging)
        .await
        .map_err(ManifestRunError::Failed)?;
    let dash_live_requested = job.source.kind == ManifestKind::Dash
        && job.source.representation_key.starts_with("dash-live-");
    let mut offline_tracks = if live_marker_exists && job.source.kind == ManifestKind::Hls {
        restore_local_live_hls_tracks(&job.source.url, &video_dir, &audio_dir).await?
    } else if dash_live_requested && dash_live_state_exists(&video_dir).await? {
        let has_audio = dash_live_state_exists(&audio_dir).await?;
        Some(Tracks::DashLiveOffline { has_audio })
    } else {
        None
    };

    let (client, force_offline) = match crate::manifest_intake::client_for_manifest(
        &job.source.url,
        &job.cookies_browser,
        job.proxy.as_ref(),
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
    };

    let tracks = if force_offline {
        offline_tracks.take().ok_or_else(|| {
            ManifestRunError::Failed(
                "The saved live HLS recording is no longer available".to_string(),
            )
        })?
    } else {
        match crate::manifest_intake::lookup_with_client(&client, &job.source.url).await {
            Ok(Some(lookup)) if lookup.kind() == job.source.kind => {
                match resolve_tracks(&client, lookup, &job.source).await {
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
    let mut has_audio = match &tracks {
        Tracks::Hls { audio, .. } => audio.is_some(),
        Tracks::Dash { audio, .. } => audio.is_some(),
        Tracks::DashLiveOffline { has_audio } => *has_audio,
    };
    if has_audio {
        ensure_track_dir(&audio_dir).await?;
    }
    let live_source = matches!(
        &tracks,
        Tracks::Hls { video, audio }
            if video.is_live || audio.as_ref().is_some_and(|track| track.is_live)
    );
    let live_hls = matches!(&tracks, Tracks::Hls { .. }) && (live_source || live_marker_exists);
    let live_dash = dash_live_requested
        && matches!(&tracks, Tracks::Dash { .. } | Tracks::DashLiveOffline { .. });
    if live_source {
        ensure_live_hls_marker(&staging)
            .await
            .map_err(ManifestRunError::Failed)?;
    } else if live_marker_exists && !matches!(&tracks, Tracks::Hls { .. }) {
        return Err(ManifestRunError::Failed(
            "A live HLS staging marker belongs to a non-HLS queue item".to_string(),
        ));
    }
    tx.send(EngineMsg::Phase(if live_source || live_dash {
        "Recording live stream…".to_string()
    } else if live_hls {
        "Finalizing live stream…".to_string()
    } else {
        "Downloading stream segments…".to_string()
    }))
    .ok();
    let concurrency = job.connections.clamp(1, MAX_RESOURCE_CONNECTIONS);
    let downloaded = Arc::new(AtomicU64::new(0));
    let mut _preserve_stop_sender = None;
    if live_hls {
        let Tracks::Hls { video, audio } = &tracks else {
            return Err(ManifestRunError::Failed(
                "Live HLS capture requires HLS media tracks".to_string(),
            ));
        };
        let capture = run_live_hls_tracks(
            &client,
            &job.source.url,
            video,
            audio.as_ref(),
            &video_dir,
            &audio_dir,
            concurrency,
            &tx,
            &downloaded,
            &mut abort,
        )
        .await?;
        match capture {
            LiveHlsCaptureOutcome::Ended => {}
            LiveHlsCaptureOutcome::Preserve | LiveHlsCaptureOutcome::PreserveVideoOnly => {
                // A preserved stop ends recording but still needs a live
                // receiver during FFmpeg finalization. Removal remains guarded
                // by AttemptGate::try_commit at the final delivery boundary.
                if !staged_regular_file_exists(&video_dir.join("local.m3u8")).await {
                    return Ok(ManifestOutcome::Aborted);
                }
                if matches!(capture, LiveHlsCaptureOutcome::PreserveVideoOnly) {
                    // Keep a video-only recovery decision across process restarts:
                    // an old audio playlist must not be restored on the next retry.
                    remove_regular_file_if_present(&audio_dir.join("local.m3u8"))
                        .await
                        .map_err(ManifestRunError::Failed)?;
                    has_audio = false;
                } else if has_audio
                    && !staged_regular_file_exists(&audio_dir.join("local.m3u8")).await
                {
                    has_audio = false;
                }
                let (sender, receiver) = tokio::sync::oneshot::channel();
                _preserve_stop_sender = Some(sender);
                abort = receiver;
            }
            LiveHlsCaptureOutcome::Discard => return Ok(ManifestOutcome::Aborted),
        }
    } else if live_dash {
        let (video_plan, audio_plan) = match &tracks {
            Tracks::Dash { video, audio } => (Some(video), audio.as_deref()),
            Tracks::DashLiveOffline { .. } => (None, None),
            Tracks::Hls { .. } => {
                return Err(ManifestRunError::Failed(
                    "A live DASH queue item resolved to HLS tracks".to_string(),
                ));
            }
        };
        let capture = run_live_dash_tracks(
            &client,
            &job.source.url,
            &job.source.representation_key,
            job.source.audio_key.as_deref(),
            video_plan,
            audio_plan,
            &video_dir,
            &audio_dir,
            concurrency,
            &tx,
            &downloaded,
            &mut abort,
        )
        .await?;
        match capture.outcome {
            DashLiveCaptureOutcome::Discard => return Ok(ManifestOutcome::Aborted),
            DashLiveCaptureOutcome::Preserve
            | DashLiveCaptureOutcome::PreserveVideoOnly => {
                let (sender, receiver) = tokio::sync::oneshot::channel();
                _preserve_stop_sender = Some(sender);
                abort = receiver;
            }
            DashLiveCaptureOutcome::Ended => {}
        }
        if !capture
            .video_resources
            .iter()
            .any(|name| name.starts_with("dash-live-segment-"))
        {
            return Ok(ManifestOutcome::Aborted);
        }
        if matches!(capture.outcome, DashLiveCaptureOutcome::PreserveVideoOnly) {
            remove_regular_file_if_present(&audio_dir.join(DASH_LIVE_STATE_NAME))
                .await
                .map_err(ManifestRunError::Failed)?;
            has_audio = false;
        } else {
            has_audio = capture.audio_resources.as_ref().is_some_and(|resources| {
                resources
                    .iter()
                    .any(|name| name.starts_with("dash-live-segment-"))
            });
        }
        if !concat_dash_resource_names(
            &capture.video_resources,
            &video_dir,
            "video-track.mp4",
            &mut abort,
        )
        .await?
        {
            return Ok(ManifestOutcome::Aborted);
        }
        if has_audio
            && let Some(audio_resources) = capture.audio_resources.as_ref()
            && !concat_dash_resource_names(
                audio_resources,
                &audio_dir,
                "audio-track.mp4",
                &mut abort,
            )
            .await?
        {
            return Ok(ManifestOutcome::Aborted);
        }
    } else {
        match &tracks {
            Tracks::Hls { video, audio } => {
                if !fetch_resources(
                    &client,
                    hls_resources(video),
                    &video_dir,
                    concurrency,
                    &tx,
                    &downloaded,
                    &mut abort,
                )
                .await?
                {
                    return Ok(ManifestOutcome::Aborted);
                }
                if let Some(audio) = audio
                    && !fetch_resources(
                        &client,
                        hls_resources(audio),
                        &audio_dir,
                        concurrency,
                        &tx,
                        &downloaded,
                        &mut abort,
                    )
                    .await?
                {
                    return Ok(ManifestOutcome::Aborted);
                }
                write_playlist(&video_dir, video).await?;
                if let Some(audio) = audio {
                    write_playlist(&audio_dir, audio).await?;
                }
            }
            Tracks::Dash { video, audio } => {
                if !fetch_resources(
                    &client,
                    dash_resources(video),
                    &video_dir,
                    concurrency,
                    &tx,
                    &downloaded,
                    &mut abort,
                )
                .await?
                {
                    return Ok(ManifestOutcome::Aborted);
                }
                if !concat_dash_track(video, &video_dir, "video-track.mp4", &mut abort).await? {
                    return Ok(ManifestOutcome::Aborted);
                }
                if let Some(audio) = audio {
                    if !fetch_resources(
                        &client,
                        dash_resources(audio),
                        &audio_dir,
                        concurrency,
                        &tx,
                        &downloaded,
                        &mut abort,
                    )
                    .await?
                    {
                        return Ok(ManifestOutcome::Aborted);
                    }
                    if !concat_dash_track(audio, &audio_dir, "audio-track.mp4", &mut abort).await? {
                        return Ok(ManifestOutcome::Aborted);
                    }
                }
            }
            Tracks::DashLiveOffline { .. } => {
                return Err(ManifestRunError::Failed(
                    "A restored live DASH item bypassed the native live capture path".to_string(),
                ));
            }
        }
    }

    if abort.try_recv().is_ok() {
        return Ok(ManifestOutcome::Aborted);
    }
    tx.send(EngineMsg::Phase("Merging audio and video…".to_string()))
        .ok();

    let ffmpeg_dir =
        crate::video_tools::toolchain_dir_in(&crate::video_tools::ffmpeg_search_dirs())
            .ok_or_else(|| {
                ManifestRunError::Failed(
                    "FFmpeg and ffprobe are required for native stream downloads".to_string(),
                )
            })?;
    let ffmpeg = ffmpeg_dir.join("ffmpeg");
    if !ffmpeg.is_file() {
        return Err(ManifestRunError::Failed(
            "The FFmpeg executable could not be found".to_string(),
        ));
    }

    let video_input = match tracks {
        Tracks::Hls { .. } => video_dir.join("local.m3u8"),
        Tracks::Dash { .. } | Tracks::DashLiveOffline { .. } => {
            video_dir.join("video-track.mp4")
        }
    };
    let audio_input = has_audio.then(|| match job.source.kind {
        ManifestKind::Hls => audio_dir.join("local.m3u8"),
        ManifestKind::Dash => audio_dir.join("audio-track.mp4"),
    });
    let output_part = staging.join("output.part.mp4");
    remove_regular_file_if_present(&output_part).await?;
    if !run_ffmpeg(
        &ffmpeg,
        &video_input,
        audio_input.as_deref(),
        &output_part,
        &mut abort,
    )
    .await?
    {
        return Ok(ManifestOutcome::Aborted);
    }

    // Validate and size the muxed file before the delivery commit. After the
    // no-clobber rename succeeds, no later I/O failure may turn a delivered
    // download into a retry/fallback.
    let output_metadata = tokio::fs::symlink_metadata(&output_part)
        .await
        .map_err(|error| format!("Could not inspect the finalized stream: {error}"))?;
    if output_metadata.file_type().is_symlink()
        || !output_metadata.is_file()
        || output_metadata.len() == 0
    {
        return Err(ManifestRunError::Failed(
            "FFmpeg did not produce a regular, non-empty media file".to_string(),
        ));
    }
    let size = output_metadata.len();

    if !gate.try_commit() {
        return Ok(ManifestOutcome::Aborted);
    }
    match crate::file_names::rename_noreplace(&output_part, &job.dest) {
        Ok(()) => {
            gate.mark_delivered();
            crate::extension_server::discard_handoff_cookies(&job.source.url);
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(ManifestRunError::Failed(
                crate::engine_msg::DEST_EXISTS.to_string(),
            ));
        }
        Err(error) => {
            return Err(ManifestRunError::Failed(format!(
                "Could not move the finished stream into place: {error}"
            )));
        }
    }
    let parent = job.dest.parent().map(Path::to_path_buf);
    if let Some(parent) = parent {
        let id = job.item_id;
        let _ = tokio::task::spawn_blocking(move || {
            crate::video_staging::clean_manifest_staging_files(&parent, id);
        })
        .await;
    }
    Ok(ManifestOutcome::Finished(size))
}

async fn resolve_tracks(
    client: &reqwest::Client,
    lookup: ManifestLookup,
    source: &ManifestSource,
) -> Result<Tracks, ManifestRunError> {
    let mut guarded_child_hosts = std::collections::HashSet::new();
    match lookup.manifest {
        NativeManifest::HlsMedia(plan) if source.representation_key == "hls-media" => {
            guard_hls_resources(&source.url, &plan, &mut guarded_child_hosts)?;
            Ok(Tracks::Hls {
                video: *plan,
                audio: None,
            })
        }
        NativeManifest::HlsMaster(master) => {
            let variant = master
                .variants
                .iter()
                .zip(crate::manifest_intake::hls_variant_keys(&master.variants))
                .find_map(|(variant, key)| (key == source.representation_key).then_some(variant))
                .ok_or_else(|| {
                    ManifestRunError::Unsupported(
                        "The selected HLS variant is no longer available".to_string(),
                    )
                })?;
            let video_url = variant.url.clone();
            let selected_audio = match source.audio_key.as_deref() {
                Some(audio_key) => Some(
                    master
                        .audio_renditions
                        .iter()
                        .zip(crate::manifest_intake::hls_audio_rendition_keys(
                            &master.audio_renditions,
                        ))
                        .find_map(|(rendition, key)| (key == audio_key).then_some(rendition))
                        .filter(|rendition| {
                            variant
                                .audio_group
                                .as_deref()
                                .is_some_and(|group| rendition.group_id == group)
                        })
                        .ok_or_else(|| {
                            ManifestRunError::Unsupported(
                                "The selected HLS audio rendition is no longer available"
                                    .to_string(),
                            )
                        })?,
                ),
                None => variant.audio_group.as_deref().and_then(|group| {
                    master
                        .audio_renditions
                        .iter()
                        .filter(|rendition| rendition.group_id == group)
                        .find(|rendition| rendition.is_default)
                        .or_else(|| {
                            master
                                .audio_renditions
                                .iter()
                                .find(|rendition| rendition.group_id == group)
                        })
                }),
            };
            let audio_url = selected_audio.and_then(|rendition| rendition.url.clone());
            guard_manifest_child_url(&source.url, &video_url, &mut guarded_child_hosts)?;
            if let Some(audio_url) = audio_url.as_ref() {
                guard_manifest_child_url(&source.url, audio_url, &mut guarded_child_hosts)?;
            }
            let video =
                crate::manifest_intake::lookup_hls_media(client, video_url.as_str()).await?;
            guard_hls_resources(&source.url, &video, &mut guarded_child_hosts)?;
            let audio = match audio_url {
                Some(url) => {
                    let plan =
                        crate::manifest_intake::lookup_hls_media(client, url.as_str()).await?;
                    guard_hls_resources(&source.url, &plan, &mut guarded_child_hosts)?;
                    Some(plan)
                }
                None => None,
            };
            Ok(Tracks::Hls { video, audio })
        }
        NativeManifest::Dash(dash) => {
            let video =
                crate::manifest::dash::plan_dash_representation(&dash, &source.representation_key)
                    .map_err(|error| ManifestRunError::Unsupported(error.to_string()))?;
            let selected_audio = match source.audio_key.as_deref() {
                Some(audio_key) => Some(
                    dash.audio_representations
                        .iter()
                        .find(|track| track.key == audio_key || track.live_key == audio_key)
                        .ok_or_else(|| {
                            ManifestRunError::Unsupported(
                                "The selected DASH audio representation is no longer available"
                                    .to_string(),
                            )
                        })?,
                ),
                None => dash.default_audio(),
            };
            let audio = selected_audio
                .map(|track| {
                    let plan_key = if source.representation_key.starts_with("dash-live-")
                        || source.audio_key.as_deref() == Some(track.live_key.as_str())
                    {
                        track.live_key.as_str()
                    } else {
                        track.key.as_str()
                    };
                    crate::manifest::dash::plan_dash_representation(&dash, plan_key)
                        .map(Box::new)
                        .map_err(|error| ManifestRunError::Unsupported(error.to_string()))
                })
                .transpose()?;
            for resource in video.resources.iter() {
                guard_manifest_child_url(&source.url, &resource.url, &mut guarded_child_hosts)?;
            }
            if let Some(audio) = audio.as_ref() {
                for resource in audio.resources.iter() {
                    guard_manifest_child_url(&source.url, &resource.url, &mut guarded_child_hosts)?;
                }
            }
            Ok(Tracks::Dash { video, audio })
        }
        NativeManifest::HlsMedia(_) => Err(ManifestRunError::Unsupported(
            "The selected HLS media playlist is invalid".to_string(),
        )),
    }
}

fn guard_hls_resources(
    parent_url: &str,
    plan: &HlsPlan,
    seen_hosts: &mut std::collections::HashSet<String>,
) -> Result<(), String> {
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
    seen_hosts: &mut std::collections::HashSet<String>,
) -> Result<(), String> {
    match crate::download_net::inherit_public_resolution_for_collection(
        parent_url,
        child_url.as_str(),
        seen_hosts,
    ) {
        Ok(()) => Ok(()),
        Err(crate::download_net::CollectionHostError::TooManyDistinctHosts) => Err(format!(
            "The stream manifest exceeds the limit of {} distinct resource hosts",
            crate::download_net::MAX_GUARDED_CHILD_HOSTS_PER_COLLECTION
        )),
        Err(crate::download_net::CollectionHostError::UnsafeDestination) => {
            Err("The stream manifest references a local or private network resource".to_string())
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

async fn ensure_track_dir(path: &Path) -> Result<(), String> {
    match tokio::fs::create_dir(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = tokio::fs::symlink_metadata(path)
                .await
                .map_err(|error| format!("Could not inspect manifest staging: {error}"))?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err("Manifest staging path is not a real directory".to_string());
            }
            Ok(())
        }
        Err(error) => Err(format!("Could not create manifest staging: {error}")),
    }
}

async fn read_local_live_hls_plan(
    source_url: &str,
    directory: &Path,
) -> Result<Option<HlsPlan>, ManifestRunError> {
    let path = directory.join("local.m3u8");
    let metadata = match tokio::fs::symlink_metadata(&path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(ManifestRunError::Failed(format!(
                "Could not inspect the staged live HLS playlist: {error}"
            )));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ManifestRunError::Failed(
            "The staged live HLS playlist is not a regular file".to_string(),
        ));
    }
    if metadata.len() > crate::manifest::MAX_HLS_LOCAL_PLAYLIST_BYTES as u64 {
        return Err(ManifestRunError::Failed(
            "The staged live HLS playlist exceeds its size limit".to_string(),
        ));
    }
    let bytes = tokio::fs::read(&path).await.map_err(|error| {
        ManifestRunError::Failed(format!(
            "Could not read the staged live HLS recording: {error}"
        ))
    })?;
    HlsPlan::from_local_capture(source_url, &bytes)
        .map(Some)
        .map_err(|error| {
            ManifestRunError::Failed(format!(
                "Could not restore the staged live HLS recording: {error}"
            ))
        })
}

async fn restore_local_live_hls_tracks(
    source_url: &str,
    video_dir: &Path,
    audio_dir: &Path,
) -> Result<Option<Tracks>, ManifestRunError> {
    let Some(video) = read_local_live_hls_plan(source_url, video_dir).await? else {
        return Ok(None);
    };
    let audio = read_local_live_hls_plan(source_url, audio_dir).await?;
    Ok(Some(Tracks::Hls { video, audio }))
}

const LIVE_HLS_MARKER_NAME: &str = "live-hls.capture";
const LIVE_HLS_MARKER_CONTENT: &[u8] = b"grab-live-hls-v1\n";

async fn read_live_hls_marker(directory: &Path) -> Result<bool, String> {
    let marker = directory.join(LIVE_HLS_MARKER_NAME);
    let metadata = match tokio::fs::symlink_metadata(&marker).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!(
                "Could not inspect live HLS staging marker: {error}"
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("The live HLS staging marker is not a regular file".to_string());
    }
    if metadata.len() != LIVE_HLS_MARKER_CONTENT.len() as u64 {
        return Err("The live HLS staging marker has an unsupported version".to_string());
    }
    let contents = tokio::fs::read(&marker)
        .await
        .map_err(|error| format!("Could not read live HLS staging marker: {error}"))?;
    if contents != LIVE_HLS_MARKER_CONTENT {
        return Err("The live HLS staging marker has an unsupported version".to_string());
    }
    Ok(true)
}

async fn ensure_live_hls_marker(directory: &Path) -> Result<(), String> {
    if read_live_hls_marker(directory).await? {
        return Ok(());
    }
    let marker = directory.join(LIVE_HLS_MARKER_NAME);
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
        .await
        .map_err(|error| format!("Could not create live HLS staging marker: {error}"))?;
    if let Err(error) = file.write_all(LIVE_HLS_MARKER_CONTENT).await {
        drop(file);
        let _ = remove_regular_file_if_present(&marker).await;
        return Err(format!("Could not write live HLS staging marker: {error}"));
    }
    if let Err(error) = file.sync_all().await {
        drop(file);
        let _ = remove_regular_file_if_present(&marker).await;
        return Err(format!("Could not flush live HLS staging marker: {error}"));
    }
    Ok(())
}

async fn load_live_hls_track(
    plan: &HlsPlan,
    directory: &Path,
) -> Result<LiveHlsTrack, ManifestRunError> {
    let local_playlist = directory.join("local.m3u8");
    let mut accumulator = match tokio::fs::symlink_metadata(&local_playlist).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(ManifestRunError::Failed(
                "The staged live HLS playlist is not a regular file".to_string(),
            ));
        }
        Ok(metadata) if metadata.len() > crate::manifest::MAX_HLS_LOCAL_PLAYLIST_BYTES as u64 => {
            return Err(ManifestRunError::Failed(
                "The staged live HLS playlist exceeds its size limit".to_string(),
            ));
        }
        Ok(_) => {
            let bytes = tokio::fs::read(&local_playlist).await.map_err(|error| {
                ManifestRunError::Failed(format!(
                    "Could not read the staged live HLS recording: {error}"
                ))
            })?;
            HlsLiveAccumulator::from_local_playlist_bytes(&bytes).map_err(|error| {
                ManifestRunError::Failed(format!(
                    "Could not restore the staged live HLS recording: {error}"
                ))
            })?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => HlsLiveAccumulator::new(),
        Err(error) => {
            return Err(ManifestRunError::Failed(format!(
                "Could not inspect the staged live HLS recording: {error}"
            )));
        }
    };

    let current_resources: std::collections::HashMap<&str, &HlsResource> = plan
        .resources
        .iter()
        .map(|resource| (resource.filename.as_str(), resource))
        .collect();
    let restored_names = accumulator
        .resource_names()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let mut total_resource_bytes = 0_u64;
    for filename in restored_names {
        let path = directory.join(&filename);
        let metadata = match tokio::fs::symlink_metadata(&path).await {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(ManifestRunError::Failed(
                    "A staged live HLS resource is not a regular file".to_string(),
                ));
            }
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(ManifestRunError::Failed(format!(
                    "Could not inspect a staged live HLS resource: {error}"
                )));
            }
        };
        let expected = current_resources.get(filename.as_str()).copied();
        let is_key = accumulator.is_key_resource(&filename)
            || expected.is_some_and(|resource| resource.kind == HlsResourceKind::Key);
        let cached_is_valid = metadata.as_ref().is_some_and(|metadata| {
            metadata.len() > 0
                && metadata.len() <= MAX_RESOURCE_BYTES
                && (!is_key || metadata.len() == 16)
                && expected.is_none_or(|resource| {
                    resource
                        .range
                        .is_none_or(|range| metadata.len() == range.length)
                })
        });
        if cached_is_valid {
            if let Some(metadata) = metadata.as_ref() {
                total_resource_bytes = total_resource_bytes
                    .checked_add(metadata.len())
                    .ok_or_else(|| {
                        ManifestRunError::Failed(
                            "The staged live HLS recording byte total overflowed".to_string(),
                        )
                    })?;
            }
        } else {
            if expected.is_none() {
                return Err(ManifestRunError::Failed(
                    "A captured live HLS resource is missing or invalid and no longer appears in the current playlist"
                        .to_string(),
                ));
            }
            accumulator.forget_resource(&filename);
        }
    }
    accumulator
        .restore_resource_bytes(total_resource_bytes)
        .map_err(|error| ManifestRunError::Failed(error.to_string()))?;

    Ok(LiveHlsTrack {
        playlist_url: plan.playlist_url.clone(),
        directory: directory.to_path_buf(),
        accumulator,
        snapshot_at: Instant::now(),
        ended: false,
        next_poll_at: None,
    })
}

fn live_hls_poll_interval(target_duration: u64, changed: bool) -> Duration {
    let target = Duration::from_secs(target_duration);
    if changed { target } else { target.div_f32(2.0) }
}

async fn staged_regular_file_exists(path: &Path) -> bool {
    tokio::fs::symlink_metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
}

async fn write_live_playlist(
    directory: &Path,
    accumulator: &HlsLiveAccumulator,
) -> Result<(), String> {
    let bytes = accumulator
        .local_playlist_bytes()
        .map_err(|error| error.to_string())?;
    let target = directory.join("local.m3u8");
    let part = directory.join("local.m3u8.next.part");
    remove_regular_file_if_present(&part).await?;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part)
        .await
        .map_err(|error| format!("Could not create the live HLS playlist: {error}"))?;
    file.write_all(&bytes)
        .await
        .map_err(|error| format!("Could not write the live HLS playlist: {error}"))?;
    file.sync_all()
        .await
        .map_err(|error| format!("Could not flush the live HLS playlist: {error}"))?;
    drop(file);

    match tokio::fs::symlink_metadata(&target).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err("The staged live HLS playlist is not a regular file".to_string());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "Could not inspect the staged live HLS playlist: {error}"
            ));
        }
    }
    tokio::fs::rename(&part, &target)
        .await
        .map_err(|error| format!("Could not publish the live HLS playlist: {error}"))
}

async fn fetched_hls_resource_bytes(
    directory: &Path,
    resources: &[HlsResource],
) -> Result<u64, ManifestRunError> {
    let mut total = 0_u64;
    for resource in resources {
        let path = directory.join(&resource.filename);
        let metadata = tokio::fs::symlink_metadata(&path).await.map_err(|error| {
            ManifestRunError::Failed(format!(
                "Could not inspect a fetched live HLS resource: {error}"
            ))
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ManifestRunError::Failed(
                "A fetched live HLS resource is not a regular file".to_string(),
            ));
        }
        let size = metadata.len();
        if size == 0
            || size > MAX_RESOURCE_BYTES
            || resource.range.is_some_and(|range| size != range.length)
            || (resource.kind == HlsResourceKind::Key && size != 16)
        {
            return Err(ManifestRunError::Failed(
                "A fetched live HLS resource has an invalid size".to_string(),
            ));
        }
        total = total.checked_add(size).ok_or_else(|| {
            ManifestRunError::Failed("The live HLS resource byte total overflowed".to_string())
        })?;
    }
    Ok(total)
}

#[expect(
    clippy::too_many_arguments,
    reason = "one rendition update shares its client, cancellation, progress and aggregate byte-budget context"
)]
async fn apply_live_hls_snapshot(
    client: &reqwest::Client,
    plan: &HlsPlan,
    track: &mut LiveHlsTrack,
    connections: usize,
    tx: &tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    downloaded: &Arc<AtomicU64>,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
    other_rendition_bytes: u64,
) -> Result<LiveHlsSnapshotOutcome, ManifestRunError> {
    if plan.is_live == plan.end_list {
        if track.accumulator.segment_count() > 0 {
            return Ok(LiveHlsSnapshotOutcome::ContinuityLost);
        }
        return Err(ManifestRunError::Unsupported(
            "The live HLS playlist state is inconsistent".to_string(),
        ));
    }
    let playlist_url = plan.playlist_url.clone();
    let update = match track.accumulator.prepare_update(plan) {
        Ok(update) => update,
        Err(crate::manifest::ManifestError::LiveSequenceGap) => {
            tracing::warn!(
                playlist = %playlist_url,
                "live HLS playlist continuity was lost; preserving captured media"
            );
            return Ok(LiveHlsSnapshotOutcome::ContinuityLost);
        }
        Err(crate::manifest::ManifestError::LiveCaptureLimit) => {
            tracing::warn!(
                playlist = %playlist_url,
                "live HLS capture reached a configured resource limit; preserving captured media"
            );
            return Ok(LiveHlsSnapshotOutcome::ContinuityLost);
        }
        Err(error) if track.accumulator.segment_count() > 0 => {
            tracing::warn!(
                playlist = %playlist_url,
                error = %error,
                "live HLS update is no longer supported; preserving captured media"
            );
            return Ok(LiveHlsSnapshotOutcome::ContinuityLost);
        }
        Err(error) => return Err(ManifestRunError::Unsupported(error.to_string())),
    };
    let changed = update.changed;
    let end_list = update.end_list;
    let next_poll_at = if end_list {
        None
    } else {
        match track
            .snapshot_at
            .checked_add(live_hls_poll_interval(plan.source_target_duration, changed))
        {
            Some(deadline) => Some(deadline),
            None if track.accumulator.segment_count() > 0 => {
                tracing::warn!(
                    playlist = %playlist_url,
                    "live HLS reload deadline is out of range; preserving captured media"
                );
                return Ok(LiveHlsSnapshotOutcome::ContinuityLost);
            }
            None => {
                return Err(ManifestRunError::Unsupported(
                    "The live HLS target duration is too large".to_string(),
                ));
            }
        }
    };
    let resources = hls_resources_from(&update.resources);
    let fetch_outcome = fetch_resources_with_stop(
        client,
        resources,
        &track.directory,
        connections,
        tx,
        downloaded,
        abort,
    )
    .await;
    let fetch_outcome = match fetch_outcome {
        Err(ManifestRunError::Unsupported(error)) if track.accumulator.segment_count() > 0 => {
            tracing::warn!(
                playlist = %playlist_url,
                error = %error,
                "live HLS resource update is no longer supported; preserving captured media"
            );
            return Ok(LiveHlsSnapshotOutcome::ContinuityLost);
        }
        Err(error) => return Err(error),
        Ok(outcome) => outcome,
    };
    match fetch_outcome {
        ResourceFetchOutcome::Complete => {}
        ResourceFetchOutcome::Stopped(intent) => {
            return Ok(LiveHlsSnapshotOutcome::Stopped(intent));
        }
    }

    let added_resource_bytes =
        fetched_hls_resource_bytes(&track.directory, &update.resources).await?;
    match track.accumulator.commit_update_with_budget(
        update,
        added_resource_bytes,
        other_rendition_bytes,
    ) {
        Ok(()) => {}
        Err(crate::manifest::ManifestError::LiveCaptureLimit) => {
            tracing::warn!(
                playlist = %playlist_url,
                "live HLS capture reached a configured resource limit; preserving captured media"
            );
            return Ok(LiveHlsSnapshotOutcome::ContinuityLost);
        }
        Err(error) if track.accumulator.segment_count() > 0 => {
            tracing::warn!(
                playlist = %playlist_url,
                error = %error,
                "live HLS commit is no longer supported; preserving captured media"
            );
            return Ok(LiveHlsSnapshotOutcome::ContinuityLost);
        }
        Err(error) => return Err(ManifestRunError::Unsupported(error.to_string())),
    }
    write_live_playlist(&track.directory, &track.accumulator)
        .await
        .map_err(ManifestRunError::Failed)?;
    track.playlist_url = playlist_url;
    track.ended = end_list;
    track.next_poll_at = next_poll_at;
    Ok(LiveHlsSnapshotOutcome::Committed)
}

#[expect(
    clippy::too_many_arguments,
    reason = "one rendition poll carries the same guarded-network and cancellation context into its update"
)]
async fn poll_live_hls_track(
    client: &reqwest::Client,
    source_url: &str,
    track: &mut LiveHlsTrack,
    connections: usize,
    tx: &tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    downloaded: &Arc<AtomicU64>,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
    guarded_child_hosts: &mut std::collections::HashSet<String>,
    other_rendition_bytes: u64,
) -> Result<LiveHlsSnapshotOutcome, ManifestRunError> {
    let result = tokio::select! {
        biased;
        stop = &mut *abort => {
            return Ok(LiveHlsSnapshotOutcome::Stopped(
                stop.unwrap_or(crate::video::StopIntent::Discard),
            ));
        }
        result = crate::manifest_intake::lookup_hls_media(
            client,
            track.playlist_url.as_str(),
        ) => result,
    };
    let plan = match result {
        Ok(plan) => plan,
        Err(crate::manifest_intake::ManifestIntakeError::Unsupported(error))
            if track.accumulator.segment_count() > 0 =>
        {
            tracing::warn!(
                playlist = %track.playlist_url,
                error = %error,
                "live HLS playlist is no longer supported; preserving captured media"
            );
            return Ok(LiveHlsSnapshotOutcome::ContinuityLost);
        }
        Err(error) => return Err(error.into()),
    };
    track.snapshot_at = Instant::now();
    if let Err(error) =
        guard_manifest_child_url(source_url, &plan.playlist_url, guarded_child_hosts)
    {
        if track.accumulator.segment_count() > 0 {
            tracing::warn!(
                playlist = %plan.playlist_url,
                error = %error,
                "live HLS playlist host is no longer eligible; preserving captured media"
            );
            return Ok(LiveHlsSnapshotOutcome::ContinuityLost);
        }
        return Err(ManifestRunError::Unsupported(error));
    }
    if let Err(error) = guard_hls_resources(source_url, &plan, guarded_child_hosts) {
        if track.accumulator.segment_count() > 0 {
            tracing::warn!(
                playlist = %plan.playlist_url,
                error = %error,
                "live HLS resource host is no longer eligible; preserving captured media"
            );
            return Ok(LiveHlsSnapshotOutcome::ContinuityLost);
        }
        return Err(ManifestRunError::Unsupported(error));
    }
    apply_live_hls_snapshot(
        client,
        &plan,
        track,
        connections,
        tx,
        downloaded,
        abort,
        other_rendition_bytes,
    )
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "the paired live renditions share one bounded capture session and cancellation signal"
)]
async fn run_live_hls_tracks(
    client: &reqwest::Client,
    source_url: &str,
    video_plan: &HlsPlan,
    audio_plan: Option<&HlsPlan>,
    video_dir: &Path,
    audio_dir: &Path,
    connections: usize,
    tx: &tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    downloaded: &Arc<AtomicU64>,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
) -> Result<LiveHlsCaptureOutcome, ManifestRunError> {
    let mut video = load_live_hls_track(video_plan, video_dir).await?;
    let mut audio = match audio_plan {
        Some(plan) => Some(load_live_hls_track(plan, audio_dir).await?),
        None => None,
    };
    let retained_bytes = video
        .accumulator
        .resource_bytes()
        .checked_add(
            audio
                .as_ref()
                .map_or(0, |track| track.accumulator.resource_bytes()),
        )
        .filter(|total| *total <= crate::manifest::MAX_HLS_LIVE_CAPTURE_BYTES)
        .ok_or_else(|| {
            ManifestRunError::Failed(
                "The staged live HLS recording exceeds Grab's native byte limit".to_string(),
            )
        })?;
    downloaded.store(retained_bytes, Ordering::Relaxed);
    let mut guarded_child_hosts = std::collections::HashSet::new();

    let audio_resource_bytes = audio
        .as_ref()
        .map_or(0, |track| track.accumulator.resource_bytes());
    if video
        .accumulator
        .resource_bytes()
        .checked_add(audio_resource_bytes)
        .is_none_or(|total| total > crate::manifest::MAX_HLS_LIVE_CAPTURE_BYTES)
    {
        return Ok(if video.accumulator.segment_count() > 0 {
            LiveHlsCaptureOutcome::PreserveVideoOnly
        } else {
            LiveHlsCaptureOutcome::Discard
        });
    }

    let all_tracks_ended = video_plan.end_list && audio_plan.is_none_or(|plan| plan.end_list);
    if !all_tracks_ended {
        guard_manifest_child_url(
            source_url,
            &video_plan.playlist_url,
            &mut guarded_child_hosts,
        )
        .map_err(ManifestRunError::Unsupported)?;
        if let Some(plan) = audio_plan {
            guard_manifest_child_url(source_url, &plan.playlist_url, &mut guarded_child_hosts)
                .map_err(ManifestRunError::Unsupported)?;
        }
    }

    match apply_live_hls_snapshot(
        client,
        video_plan,
        &mut video,
        connections,
        tx,
        downloaded,
        abort,
        audio_resource_bytes,
    )
    .await?
    {
        LiveHlsSnapshotOutcome::Committed => {}
        LiveHlsSnapshotOutcome::Stopped(intent) => {
            return Ok(match intent {
                crate::video::StopIntent::Preserve => LiveHlsCaptureOutcome::Preserve,
                crate::video::StopIntent::Discard => LiveHlsCaptureOutcome::Discard,
            });
        }
        LiveHlsSnapshotOutcome::ContinuityLost => {
            return Ok(if video.accumulator.segment_count() > 0 {
                LiveHlsCaptureOutcome::PreserveVideoOnly
            } else {
                LiveHlsCaptureOutcome::Discard
            });
        }
    }
    if let (Some(plan), Some(track)) = (audio_plan, audio.as_mut()) {
        let audio_outcome = apply_live_hls_snapshot(
            client,
            plan,
            track,
            connections,
            tx,
            downloaded,
            abort,
            video.accumulator.resource_bytes(),
        )
        .await;
        let audio_outcome = match audio_outcome {
            Err(ManifestRunError::Unsupported(error)) => {
                tracing::warn!(
                    playlist = %plan.playlist_url,
                    error = %error,
                    "live HLS audio rendition is no longer supported; preserving video capture"
                );
                return Ok(LiveHlsCaptureOutcome::PreserveVideoOnly);
            }
            Err(error) => return Err(error),
            Ok(outcome) => outcome,
        };
        match audio_outcome {
            LiveHlsSnapshotOutcome::Committed => {}
            LiveHlsSnapshotOutcome::Stopped(intent) => {
                return Ok(match intent {
                    crate::video::StopIntent::Preserve => LiveHlsCaptureOutcome::PreserveVideoOnly,
                    crate::video::StopIntent::Discard => LiveHlsCaptureOutcome::Discard,
                });
            }
            LiveHlsSnapshotOutcome::ContinuityLost => {
                return Ok(if video.accumulator.segment_count() > 0 {
                    LiveHlsCaptureOutcome::PreserveVideoOnly
                } else {
                    LiveHlsCaptureOutcome::Discard
                });
            }
        }
    }

    while !video.ended || audio.as_ref().is_some_and(|track| !track.ended) {
        let next_poll = [
            (!video.ended).then_some(video.next_poll_at).flatten(),
            audio
                .as_ref()
                .filter(|track| !track.ended)
                .and_then(|track| track.next_poll_at),
        ]
        .into_iter()
        .flatten()
        .min();
        let Some(next_poll) = next_poll else {
            return Err(ManifestRunError::Failed(
                "A live HLS playlist has no next reload deadline".to_string(),
            ));
        };
        tokio::select! {
            biased;
            stop = &mut *abort => {
                return Ok(match stop.unwrap_or(crate::video::StopIntent::Discard) {
                    crate::video::StopIntent::Preserve => LiveHlsCaptureOutcome::Preserve,
                    crate::video::StopIntent::Discard => LiveHlsCaptureOutcome::Discard,
                });
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(next_poll)) => {}
        }

        let now = Instant::now();
        if !video.ended && video.next_poll_at.is_some_and(|deadline| deadline <= now) {
            match poll_live_hls_track(
                client,
                source_url,
                &mut video,
                connections,
                tx,
                downloaded,
                abort,
                &mut guarded_child_hosts,
                audio
                    .as_ref()
                    .map_or(0, |track| track.accumulator.resource_bytes()),
            )
            .await?
            {
                LiveHlsSnapshotOutcome::Committed => {}
                LiveHlsSnapshotOutcome::Stopped(intent) => {
                    return Ok(match intent {
                        crate::video::StopIntent::Preserve => LiveHlsCaptureOutcome::Preserve,
                        crate::video::StopIntent::Discard => LiveHlsCaptureOutcome::Discard,
                    });
                }
                LiveHlsSnapshotOutcome::ContinuityLost => {
                    return Ok(if video.accumulator.segment_count() > 0 {
                        LiveHlsCaptureOutcome::PreserveVideoOnly
                    } else {
                        LiveHlsCaptureOutcome::Discard
                    });
                }
            }
        }
        if let Some(audio) = audio.as_mut()
            && !audio.ended
            && audio
                .next_poll_at
                .is_some_and(|deadline| deadline <= Instant::now())
        {
            match poll_live_hls_track(
                client,
                source_url,
                audio,
                connections,
                tx,
                downloaded,
                abort,
                &mut guarded_child_hosts,
                video.accumulator.resource_bytes(),
            )
            .await?
            {
                LiveHlsSnapshotOutcome::Committed => {}
                LiveHlsSnapshotOutcome::Stopped(intent) => {
                    return Ok(match intent {
                        crate::video::StopIntent::Preserve => {
                            LiveHlsCaptureOutcome::PreserveVideoOnly
                        }
                        crate::video::StopIntent::Discard => LiveHlsCaptureOutcome::Discard,
                    });
                }
                LiveHlsSnapshotOutcome::ContinuityLost => {
                    return Ok(if video.accumulator.segment_count() > 0 {
                        LiveHlsCaptureOutcome::PreserveVideoOnly
                    } else {
                        LiveHlsCaptureOutcome::Discard
                    });
                }
            }
        }
    }

    Ok(LiveHlsCaptureOutcome::Ended)
}

async fn write_playlist(directory: &Path, plan: &HlsPlan) -> Result<(), String> {
    let bytes = plan
        .local_playlist_bytes()
        .map_err(|error| error.to_string())?;
    let target = directory.join("local.m3u8");
    let part = directory.join("local.m3u8.part");
    remove_regular_file_if_present(&target).await?;
    remove_regular_file_if_present(&part).await?;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part)
        .await
        .map_err(|error| format!("Could not write the local HLS playlist: {error}"))?;
    file.write_all(&bytes)
        .await
        .map_err(|error| format!("Could not write the local HLS playlist: {error}"))?;
    file.sync_all()
        .await
        .map_err(|error| format!("Could not flush the local HLS playlist: {error}"))?;
    drop(file);
    crate::file_names::rename_noreplace(&part, &target)
        .map_err(|error| format!("Could not publish the local HLS playlist: {error}"))
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
                stop.unwrap_or(crate::video::StopIntent::Discard),
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
            .ok_or_else(|| "The stream resource byte count overflowed".to_string())?;
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
    let mut attempt = 0_u32;
    loop {
        let result = fetch_resource_once(client, &resource, directory, tx, downloaded).await;
        if result.is_err() {
            // The failed attempt's reservation has been dropped; publish the rolled-back total.
            tx.send(progress_message(downloaded.load(Ordering::Relaxed)))
                .ok();
        }
        match result {
            Err(ManifestRunError::Retryable(error)) if attempt + 1 < MAX_RESOURCE_ATTEMPTS => {
                attempt += 1;
                let delay = RESOURCE_RETRY_BASE_DELAY * attempt;
                tracing::debug!(
                    filename = %resource.filename,
                    attempt,
                    delay_ms = delay.as_millis() as u64,
                    error = %error,
                    "retrying transient native stream resource"
                );
                tokio::time::sleep(delay).await;
            }
            result => return result,
        }
    }
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
    if let Ok(metadata) = tokio::fs::symlink_metadata(&target).await {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ManifestRunError::Failed(
                "A manifest resource path is not a regular file".to_string(),
            ));
        }
        let cached_range_matches = resource
            .range
            .is_none_or(|range| metadata.len() == range.length);
        if metadata.len() > 0
            && metadata.len() <= MAX_RESOURCE_BYTES
            && cached_range_matches
            && (!resource.key || metadata.len() == 16)
        {
            let size = metadata.len();
            let total = reserve_download_bytes(downloaded, size)?;
            tx.send(progress_message(total)).ok();
            return Ok(size);
        }
        tokio::fs::remove_file(&target)
            .await
            .map_err(|error| format!("Could not reset an incomplete manifest resource: {error}"))?;
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
            ManifestRunError::Retryable(format!("Could not retrieve a stream segment: {error}"))
        })?;
    let status = response.status();
    if let Some(range) = resource.range {
        if is_retryable_resource_status(status) {
            return Err(ManifestRunError::Retryable(format!(
                "The stream server returned HTTP {} for a byte-range segment",
                status.as_u16()
            )));
        }
        if status != reqwest::StatusCode::PARTIAL_CONTENT
            || !content_range_matches(
                response
                    .headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|value| value.to_str().ok()),
                range,
            )
        {
            return Err(ManifestRunError::Unsupported(
                "The stream server did not honor the requested byte range".to_string(),
            ));
        }
    } else if !status.is_success() {
        return Err(resource_status_error(status));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESOURCE_BYTES)
    {
        return Err(ManifestRunError::Unsupported(
            "A stream segment exceeds the size limit".to_string(),
        ));
    }
    if resource.key && response.content_length().is_some_and(|length| length != 16) {
        return Err(ManifestRunError::Unsupported(
            "The HLS AES-128 key is not 16 bytes".to_string(),
        ));
    }

    let mut reservation = ResourceByteReservation::new(downloaded);
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part)
        .await
        .map_err(|error| format!("Could not create a segment file: {error}"))?;
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
            ManifestRunError::Retryable(format!("A stream segment transfer failed: {error}"))
        })?;
        size = size
            .checked_add(chunk.len() as u64)
            .filter(|size| *size <= MAX_RESOURCE_BYTES)
            .ok_or_else(|| {
                ManifestRunError::Unsupported("A stream segment exceeds the size limit".to_string())
            })?;
        if resource.key && size > 16 {
            return Err(ManifestRunError::Unsupported(
                "The HLS AES-128 key is not 16 bytes".to_string(),
            ));
        }
        file.write_all(&chunk)
            .await
            .map_err(|error| format!("Could not save a stream segment: {error}"))?;
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
    if let Some(range) = resource.range {
        if size < range.length {
            return Err(ManifestRunError::Retryable(
                "The stream server ended a byte-range segment before all bytes arrived".to_string(),
            ));
        }
        if size > range.length {
            return Err(ManifestRunError::Unsupported(
                "The stream server returned more bytes than the requested range".to_string(),
            ));
        }
    }
    if size == 0 || (resource.key && size != 16) {
        return Err(ManifestRunError::Unsupported(if resource.key {
            "The HLS AES-128 key is not 16 bytes".to_string()
        } else {
            "The stream server returned an empty segment".to_string()
        }));
    }
    file.sync_all()
        .await
        .map_err(|error| format!("Could not flush a stream segment: {error}"))?;
    drop(file);
    crate::file_names::rename_noreplace(&part, &target)
        .map_err(|error| format!("Could not publish a stream segment: {error}"))?;
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
    !filename.is_empty()
        && filename != "."
        && filename != ".."
        && !filename.contains('/')
        && !filename.contains('\\')
        && Path::new(filename)
            .file_name()
            .and_then(|name| name.to_str())
            == Some(filename)
}

fn content_range_matches(header: Option<&str>, range: ManifestByteRange) -> bool {
    let Some(value) = header.and_then(|header| header.strip_prefix("bytes ")) else {
        return false;
    };
    let Some((positions, total)) = value.split_once('/') else {
        return false;
    };
    let Some((start, end)) = positions.split_once('-') else {
        return false;
    };
    let (Ok(start), Ok(end), Ok(total)) = (
        start.parse::<u64>(),
        end.parse::<u64>(),
        total.parse::<u64>(),
    ) else {
        return false;
    };
    range
        .offset
        .checked_add(range.length)
        .is_some_and(|exclusive_end| {
            start == range.offset && end == exclusive_end - 1 && total >= exclusive_end
        })
}

const DASH_LIVE_STATE_NAME: &str = "live-dash-state.json";
const DASH_LIVE_STATE_VERSION: u32 = 1;

async fn dash_live_state_exists(directory: &Path) -> Result<bool, ManifestRunError> {
    let path = directory.join(DASH_LIVE_STATE_NAME);
    match tokio::fs::symlink_metadata(&path).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(ManifestRunError::Failed(
                "The staged dynamic DASH state is not a regular file".to_string(),
            ))
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(ManifestRunError::Failed(format!(
            "Could not inspect dynamic DASH staging state: {error}"
        ))),
    }
}

async fn read_dash_live_representation_key(
    directory: &Path,
) -> Result<String, ManifestRunError> {
    let path = directory.join(DASH_LIVE_STATE_NAME);
    let metadata = tokio::fs::symlink_metadata(&path).await.map_err(|error| {
        ManifestRunError::Failed(format!(
            "Could not inspect saved dynamic DASH audio state: {error}"
        ))
    })?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_DASH_LIVE_STATE_BYTES as u64
    {
        return Err(ManifestRunError::Failed(
            "The saved dynamic DASH audio state is not a valid regular file".to_string(),
        ));
    }
    let bytes = tokio::fs::read(&path).await.map_err(|error| {
        ManifestRunError::Failed(format!(
            "Could not read saved dynamic DASH audio state: {error}"
        ))
    })?;
    let state: DashLiveState = serde_json::from_slice(&bytes).map_err(|error| {
        ManifestRunError::Failed(format!(
            "Could not parse saved dynamic DASH audio state: {error}"
        ))
    })?;
    validate_dash_live_state(&state, &state.representation_key)?;
    Ok(state.representation_key)
}

async fn load_dash_live_track(
    representation_key: &str,
    directory: &Path,
    current_plan: Option<&DashRepresentationPlan>,
) -> Result<(DashLiveTrack, u64), ManifestRunError> {
    if current_plan.is_some_and(|plan| plan.representation.live_key != representation_key) {
        return Err(ManifestRunError::Failed(
            "The refreshed DASH representation no longer matches the queued selection".to_string(),
        ));
    }
    let path = directory.join(DASH_LIVE_STATE_NAME);
    let state = match tokio::fs::symlink_metadata(&path).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(ManifestRunError::Failed(
                "The staged dynamic DASH state is not a regular file".to_string(),
            ));
        }
        Ok(metadata) if metadata.len() > MAX_DASH_LIVE_STATE_BYTES as u64 => {
            return Err(ManifestRunError::Failed(
                "The staged dynamic DASH state exceeds its size limit".to_string(),
            ));
        }
        Ok(_) => {
            let bytes = tokio::fs::read(&path).await.map_err(|error| {
                ManifestRunError::Failed(format!(
                    "Could not read dynamic DASH staging state: {error}"
                ))
            })?;
            serde_json::from_slice::<DashLiveState>(&bytes).map_err(|error| {
                ManifestRunError::Failed(format!(
                    "Could not restore dynamic DASH staging state: {error}"
                ))
            })?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if current_plan.is_none() {
                return Err(ManifestRunError::Failed(
                    "No committed dynamic DASH recording is available".to_string(),
                ));
            }
            DashLiveState::new(representation_key)
        }
        Err(error) => {
            return Err(ManifestRunError::Failed(format!(
                "Could not inspect dynamic DASH staging state: {error}"
            )));
        }
    };
    validate_dash_live_state(&state, representation_key)?;

    let current_resources: std::collections::HashMap<&str, &DashResource> = current_plan
        .into_iter()
        .flat_map(|plan| plan.resources.iter())
        .map(|resource| (resource.filename.as_str(), resource))
        .collect();
    let mut retained_bytes = 0_u64;
    for filename in state.resource_names() {
        let expected = current_resources.get(filename).copied();
        let resource_path = directory.join(filename);
        let metadata = match tokio::fs::symlink_metadata(&resource_path).await {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(ManifestRunError::Failed(
                    "A staged dynamic DASH resource is not a regular file".to_string(),
                ));
            }
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(ManifestRunError::Failed(format!(
                    "Could not inspect a staged dynamic DASH resource: {error}"
                )));
            }
        };
        let cached_is_valid = metadata.as_ref().is_some_and(|metadata| {
            metadata.len() > 0
                && metadata.len() <= MAX_RESOURCE_BYTES
                && expected.is_none_or(|resource| {
                    resource.range.is_none_or(|range| metadata.len() == range.length)
                })
        });
        if cached_is_valid {
            if let Some(metadata) = metadata.as_ref() {
                retained_bytes = retained_bytes
                    .checked_add(metadata.len())
                    .ok_or_else(|| {
                        ManifestRunError::Failed(
                            "The staged dynamic DASH byte total overflowed".to_string(),
                        )
                    })?;
            }
        } else if expected.is_none() {
            return Err(ManifestRunError::Failed(
                "A captured dynamic DASH resource is missing or invalid and no longer appears in the current MPD"
                    .to_string(),
            ));
        }
    }
    if retained_bytes > MAX_MANIFEST_DOWNLOAD_BYTES {
        return Err(ManifestRunError::Failed(
            "The staged dynamic DASH recording exceeds Grab's 1 TiB limit".to_string(),
        ));
    }

    Ok((
        DashLiveTrack {
            directory: directory.to_path_buf(),
            state,
        },
        retained_bytes,
    ))
}

fn validate_dash_live_state(
    state: &DashLiveState,
    representation_key: &str,
) -> Result<(), ManifestRunError> {
    if state.version != DASH_LIVE_STATE_VERSION
        || state.representation_key != representation_key
        || !representation_key
            .strip_prefix("dash-live-")
            .is_some_and(|fingerprint| {
                fingerprint.len() == 32
                    && fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        || state.segments.len() > MAX_DASH_LIVE_SEGMENTS
    {
        return Err(ManifestRunError::Failed(
            "The saved dynamic DASH state is incompatible with this queue item".to_string(),
        ));
    }
    if state
        .initialization
        .as_deref()
        .is_some_and(|name| {
            !safe_resource_filename(name)
                || !name.starts_with("dash-live-init-")
                || !name.ends_with(".bin")
        })
    {
        return Err(ManifestRunError::Failed(
            "The saved dynamic DASH initialization name is invalid".to_string(),
        ));
    }
    let mut seen_names = std::collections::HashSet::new();
    if let Some(initialization) = state.initialization.as_deref() {
        seen_names.insert(initialization);
    }
    let mut previous_number = None;
    for segment in &state.segments {
        if !safe_resource_filename(&segment.filename)
            || !segment.filename.starts_with("dash-live-segment-")
            || !segment.filename.ends_with(".bin")
            || !seen_names.insert(segment.filename.as_str())
            || previous_number.is_some_and(|previous: u64| {
                previous.checked_add(1) != Some(segment.number)
            })
        {
            return Err(ManifestRunError::Failed(
                "The saved dynamic DASH segment index is invalid".to_string(),
            ));
        }
        previous_number = Some(segment.number);
    }
    Ok(())
}

async fn write_dash_live_state(track: &DashLiveTrack) -> Result<(), ManifestRunError> {
    validate_dash_live_state(&track.state, &track.state.representation_key)?;
    let bytes = serde_json::to_vec(&track.state)
        .map_err(|error| ManifestRunError::Failed(error.to_string()))?;
    if bytes.len() > MAX_DASH_LIVE_STATE_BYTES {
        return Err(ManifestRunError::Failed(
            "The dynamic DASH state exceeds its serialization limit".to_string(),
        ));
    }

    let target = track.directory.join(DASH_LIVE_STATE_NAME);
    match tokio::fs::symlink_metadata(&target).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(ManifestRunError::Failed(
                "The staged dynamic DASH state is not a regular file".to_string(),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(ManifestRunError::Failed(format!(
                "Could not inspect dynamic DASH staging state: {error}"
            )));
        }
    }

    let part = track.directory.join(format!("{DASH_LIVE_STATE_NAME}.next.part"));
    remove_regular_file_if_present(&part)
        .await
        .map_err(ManifestRunError::Failed)?;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part)
        .await
        .map_err(|error| {
            ManifestRunError::Failed(format!("Could not create dynamic DASH state: {error}"))
        })?;
    if let Err(error) = file.write_all(&bytes).await {
        drop(file);
        let _ = remove_regular_file_if_present(&part).await;
        return Err(ManifestRunError::Failed(format!(
            "Could not write dynamic DASH state: {error}"
        )));
    }
    if let Err(error) = file.sync_all().await {
        drop(file);
        let _ = remove_regular_file_if_present(&part).await;
        return Err(ManifestRunError::Failed(format!(
            "Could not flush dynamic DASH state: {error}"
        )));
    }
    drop(file);
    tokio::fs::rename(&part, &target).await.map_err(|error| {
        ManifestRunError::Failed(format!("Could not publish dynamic DASH state: {error}"))
    })
}

async fn dash_live_track_bytes(state: &DashLiveState, directory: &Path) -> Result<u64, ManifestRunError> {
    let mut total = 0_u64;
    for filename in state.resource_names() {
        let metadata = tokio::fs::symlink_metadata(directory.join(filename))
            .await
            .map_err(|error| {
                ManifestRunError::Failed(format!(
                    "Could not inspect a committed dynamic DASH resource: {error}"
                ))
            })?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() == 0
            || metadata.len() > MAX_RESOURCE_BYTES
        {
            return Err(ManifestRunError::Failed(
                "A committed dynamic DASH resource has an invalid file type or size".to_string(),
            ));
        }
        total = total.checked_add(metadata.len()).ok_or_else(|| {
            ManifestRunError::Failed("The dynamic DASH byte total overflowed".to_string())
        })?;
    }
    Ok(total)
}

fn dash_presentation_ended(presentation: &DashPresentation) -> bool {
    match presentation {
        DashPresentation::Static => true,
        DashPresentation::Dynamic(dynamic) => dynamic
            .availability_end_time
            .is_some_and(|end| dynamic.server_now >= end),
    }
}

async fn dash_live_resource_needs_fetch(
    directory: &Path,
    resource: &DashResource,
) -> Result<bool, ManifestRunError> {
    let path = directory.join(&resource.filename);
    match tokio::fs::symlink_metadata(&path).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(ManifestRunError::Failed(
                "A dynamic DASH resource path is not a regular file".to_string(),
            ))
        }
        Ok(metadata) => Ok(
            metadata.len() == 0
                || metadata.len() > MAX_RESOURCE_BYTES
                || resource
                    .range
                    .is_some_and(|range| metadata.len() != range.length),
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(ManifestRunError::Failed(format!(
            "Could not inspect a dynamic DASH resource: {error}"
        ))),
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "one rendition update shares its client, cancellation, progress and aggregate byte-budget context"
)]
async fn apply_dash_live_snapshot(
    client: &reqwest::Client,
    plan: &DashRepresentationPlan,
    track: &mut DashLiveTrack,
    connections: usize,
    tx: &tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    downloaded: &Arc<AtomicU64>,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
    other_rendition_bytes: u64,
) -> Result<DashLiveUpdateOutcome, ManifestRunError> {
    if plan.representation.live_key != track.state.representation_key {
        return Ok(DashLiveUpdateOutcome::ContinuityLost);
    }

    let mut candidate = track.state.clone();
    let mut resources_to_fetch = Vec::new();
    let mut filenames_to_fetch = std::collections::HashSet::new();
    let initialization = plan
        .resources
        .iter()
        .find(|resource| resource.kind == DashResourceKind::Initialization);
    if let Some(initialization) = initialization {
        match candidate.initialization.as_deref() {
            Some(previous) if previous != initialization.filename => {
                if !candidate.segments.is_empty() {
                    tracing::warn!(
                        representation = %plan.representation.live_key,
                        "dynamic DASH initialization changed after media was captured"
                    );
                    return Ok(DashLiveUpdateOutcome::ContinuityLost);
                }
                candidate.initialization = Some(initialization.filename.clone());
            }
            None if !candidate.segments.is_empty() => {
                tracing::warn!(
                    representation = %plan.representation.live_key,
                    "dynamic DASH introduced an initialization resource after media was captured"
                );
                return Ok(DashLiveUpdateOutcome::ContinuityLost);
            }
            None => {
                candidate.initialization = Some(initialization.filename.clone());
            }
            Some(_) => {}
        }
        if dash_live_resource_needs_fetch(&track.directory, initialization).await?
            && filenames_to_fetch.insert(initialization.filename.clone())
        {
            resources_to_fetch.push(initialization.clone());
        }
    }

    let mut previous_remote_number = None;
    for resource in plan
        .resources
        .iter()
        .filter(|resource| resource.kind == DashResourceKind::Segment)
    {
        let Some(number) = resource.segment_number else {
            return Err(ManifestRunError::Unsupported(
                "A dynamic DASH segment has no stable sequence number".to_string(),
            ));
        };
        if previous_remote_number.is_some_and(|previous| number <= previous) {
            return Err(ManifestRunError::Unsupported(
                "The dynamic DASH MPD contains unordered segment numbers".to_string(),
            ));
        }
        previous_remote_number = Some(number);

        let first_number = candidate.segments.first().map(|segment| segment.number);
        if first_number.is_some_and(|first| number < first) {
            continue;
        }
        if let Some(existing) = candidate
            .segments
            .iter()
            .find(|segment| segment.number == number)
        {
            if existing.filename != resource.filename {
                tracing::warn!(
                    representation = %plan.representation.live_key,
                    segment_number = number,
                    "dynamic DASH changed the URL of an already captured segment"
                );
                return Ok(DashLiveUpdateOutcome::ContinuityLost);
            }
        } else {
            match candidate.next_number() {
                None => {}
                Some(next) if number == next => {}
                Some(next) if number > next => {
                    tracing::warn!(
                        representation = %plan.representation.live_key,
                        expected_segment = next,
                        received_segment = number,
                        "dynamic DASH MPD skipped a required segment sequence"
                    );
                    return Ok(DashLiveUpdateOutcome::ContinuityLost);
                }
                Some(_) => {
                    return Ok(DashLiveUpdateOutcome::ContinuityLost);
                }
            }
            if candidate.segments.len() >= MAX_DASH_LIVE_SEGMENTS {
                tracing::warn!(
                    representation = %plan.representation.live_key,
                    "dynamic DASH recording reached its segment limit"
                );
                return Ok(DashLiveUpdateOutcome::ContinuityLost);
            }
            candidate.segments.push(DashLiveSegment {
                number,
                filename: resource.filename.clone(),
            });
        }

        if dash_live_resource_needs_fetch(&track.directory, resource).await?
            && filenames_to_fetch.insert(resource.filename.clone())
        {
            resources_to_fetch.push(resource.clone());
        }
    }

    if !resources_to_fetch.is_empty() {
        match fetch_resources_with_stop(
            client,
            dash_resources_from(&resources_to_fetch),
            &track.directory,
            connections,
            tx,
            downloaded,
            abort,
        )
        .await?
        {
            ResourceFetchOutcome::Complete => {}
            ResourceFetchOutcome::Stopped(intent) => {
                return Ok(DashLiveUpdateOutcome::Stopped(intent));
            }
        }
    }

    if candidate.segments.len() > MAX_DASH_LIVE_SEGMENTS {
        return Ok(DashLiveUpdateOutcome::ContinuityLost);
    }
    let retained_bytes = dash_live_track_bytes(&candidate, &track.directory).await?;
    if retained_bytes
        .checked_add(other_rendition_bytes)
        .is_none_or(|total| total > MAX_MANIFEST_DOWNLOAD_BYTES)
    {
        tracing::warn!(
            representation = %plan.representation.live_key,
            "dynamic DASH recording reached its aggregate byte limit"
        );
        return Ok(DashLiveUpdateOutcome::ContinuityLost);
    }

    if candidate.segments != track.state.segments
        || candidate.initialization != track.state.initialization
    {
        let next_track = DashLiveTrack {
            directory: track.directory.clone(),
            state: candidate,
        };
        write_dash_live_state(&next_track).await?;
        track.state = next_track.state;
    }
    downloaded.store(
        retained_bytes.saturating_add(other_rendition_bytes),
        Ordering::Relaxed,
    );
    Ok(DashLiveUpdateOutcome::Committed)
}

#[expect(
    clippy::too_many_arguments,
    reason = "the live poll loop owns the shared DASH manifest selection, rendition states, progress and cancellation"
)]
async fn run_live_dash_tracks(
    client: &reqwest::Client,
    source_url: &str,
    representation_key: &str,
    audio_key: Option<&str>,
    video_plan: Option<&DashRepresentationPlan>,
    audio_plan: Option<&DashRepresentationPlan>,
    video_dir: &Path,
    audio_dir: &Path,
    connections: usize,
    tx: &tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    downloaded: &Arc<AtomicU64>,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
) -> Result<DashLiveCapture, ManifestRunError> {
    let (mut video, video_bytes) =
        load_dash_live_track(representation_key, video_dir, video_plan).await?;
    let mut audio_key = audio_key
        .map(str::to_string)
        .or_else(|| audio_plan.map(|plan| plan.representation.live_key.clone()));
    let mut audio = match audio_plan {
        Some(plan) => {
            let key = audio_key.as_deref().unwrap_or(&plan.representation.live_key);
            audio_key = Some(key.to_string());
            Some(load_dash_live_track(key, audio_dir, Some(plan)).await?)
        }
        None if dash_live_state_exists(audio_dir).await? => {
            let key = match audio_key.as_deref() {
                Some(key) => key.to_string(),
                None => read_dash_live_representation_key(audio_dir).await?,
            };
            audio_key = Some(key.clone());
            Some(load_dash_live_track(&key, audio_dir, None).await?)
        }
        None => None,
    };
    macro_rules! finish_capture {
        ($outcome:expr) => {{
            let outcome = $outcome;
            let video_resources = video
                .state
                .resource_names()
                .map(str::to_string)
                .collect::<Vec<_>>();
            let audio_resources = if outcome == DashLiveCaptureOutcome::PreserveVideoOnly {
                None
            } else {
                audio.as_ref().map(|(track, _)| {
                    track
                        .state
                        .resource_names()
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
            };
            return Ok(DashLiveCapture {
                outcome,
                video_resources,
                audio_resources,
            });
        }};
    }

    let audio_bytes = audio
        .as_ref()
        .map_or(0, |(track, bytes)| {
            let _ = track;
            *bytes
        });
    let total_bytes = video_bytes.checked_add(audio_bytes).ok_or_else(|| {
        ManifestRunError::Failed("The staged dynamic DASH byte total overflowed".to_string())
    })?;
    if total_bytes > MAX_MANIFEST_DOWNLOAD_BYTES {
        finish_capture!(if !video.state.segments.is_empty() {
            DashLiveCaptureOutcome::PreserveVideoOnly
        } else {
            DashLiveCaptureOutcome::Discard
        });
    }
    downloaded.store(total_bytes, Ordering::Relaxed);

    let (Some(mut current_video_plan), mut current_audio_plan) =
        (video_plan.cloned(), audio_plan.cloned())
    else {
        finish_capture!(if video.state.segments.is_empty() {
            DashLiveCaptureOutcome::Discard
        } else {
            DashLiveCaptureOutcome::Preserve
        });
    };

    match apply_dash_live_snapshot(
        client,
        &current_video_plan,
        &mut video,
        connections,
        tx,
        downloaded,
        abort,
        audio_bytes,
    )
    .await?
    {
        DashLiveUpdateOutcome::Committed => {}
        DashLiveUpdateOutcome::Stopped(intent) => {
            finish_capture!(match intent {
                crate::video::StopIntent::Preserve => DashLiveCaptureOutcome::Preserve,
                crate::video::StopIntent::Discard => DashLiveCaptureOutcome::Discard,
            });
        }
        DashLiveUpdateOutcome::ContinuityLost => {
            finish_capture!(if video.state.segments.is_empty() {
                DashLiveCaptureOutcome::Discard
            } else {
                DashLiveCaptureOutcome::PreserveVideoOnly
            });
        }
    }

    if let Some(audio) = audio.as_mut() {
        let Some(plan) = current_audio_plan.as_ref() else {
            finish_capture!(DashLiveCaptureOutcome::PreserveVideoOnly);
        };
        let updated_video_bytes = dash_live_track_bytes(&video.state, &video.directory).await?;
        match apply_dash_live_snapshot(
            client,
            plan,
            &mut audio.0,
            connections,
            tx,
            downloaded,
            abort,
            updated_video_bytes,
        )
        .await?
        {
            DashLiveUpdateOutcome::Committed => {}
            DashLiveUpdateOutcome::Stopped(intent) => {
                finish_capture!(match intent {
                    crate::video::StopIntent::Preserve => {
                        DashLiveCaptureOutcome::PreserveVideoOnly
                    }
                    crate::video::StopIntent::Discard => DashLiveCaptureOutcome::Discard,
                });
            }
            DashLiveUpdateOutcome::ContinuityLost => {
                finish_capture!(DashLiveCaptureOutcome::PreserveVideoOnly);
            }
        }
    }
    loop {
        if dash_presentation_ended(&current_video_plan.presentation) {
            finish_capture!(DashLiveCaptureOutcome::Ended);
        }
        let DashPresentation::Dynamic(dynamic) = &current_video_plan.presentation else {
            finish_capture!(DashLiveCaptureOutcome::Ended);
        };
        let deadline = Instant::now()
            .checked_add(dynamic.minimum_update_period)
            .ok_or_else(|| {
                ManifestRunError::Failed(
                    "The dynamic DASH update interval is outside the supported clock range"
                        .to_string(),
                )
            })?;
        tokio::select! {
            biased;
            stop = &mut *abort => {
                finish_capture!(match stop.unwrap_or(crate::video::StopIntent::Discard) {
                    crate::video::StopIntent::Preserve => DashLiveCaptureOutcome::Preserve,
                    crate::video::StopIntent::Discard => DashLiveCaptureOutcome::Discard,
                });
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {}
        }

        let lookup = tokio::select! {
            biased;
            stop = &mut *abort => {
                finish_capture!(match stop.unwrap_or(crate::video::StopIntent::Discard) {
                    crate::video::StopIntent::Preserve => DashLiveCaptureOutcome::Preserve,
                    crate::video::StopIntent::Discard => DashLiveCaptureOutcome::Discard,
                });
            }
            result = crate::manifest_intake::lookup_with_client(client, source_url) => result,
        };
        let lookup = match lookup {
            Ok(Some(lookup)) if lookup.kind() == ManifestKind::Dash => lookup,
            Ok(Some(_)) | Ok(None) => {
                tracing::warn!("dynamic DASH MPD is no longer supported; preserving committed media");
                finish_capture!(if video.state.segments.is_empty() {
                    DashLiveCaptureOutcome::Discard
                } else {
                    DashLiveCaptureOutcome::PreserveVideoOnly
                });
            }
            Err(crate::manifest_intake::ManifestIntakeError::Retryable(error)) => {
                return Err(ManifestRunError::Retryable(error));
            }
            Err(error) => {
                tracing::warn!(error = %error, "dynamic DASH MPD refresh failed; preserving committed media");
                finish_capture!(if video.state.segments.is_empty() {
                    DashLiveCaptureOutcome::Discard
                } else {
                    DashLiveCaptureOutcome::PreserveVideoOnly
                });
            }
        };
        let refreshed = resolve_tracks(client, lookup, &ManifestSource {
            url: source_url.to_string(),
            kind: ManifestKind::Dash,
            representation_key: representation_key.to_string(),
            audio_key: audio_key.clone(),
        });
        let refreshed = match refreshed {
            Ok(Tracks::Dash { video, audio }) => (video, audio.map(|audio| *audio)),
            Ok(_) => {
                finish_capture!(if video.state.segments.is_empty() {
                    DashLiveCaptureOutcome::Discard
                } else {
                    DashLiveCaptureOutcome::PreserveVideoOnly
                });
            }
            Err(ManifestRunError::Unsupported(error)) => {
                tracing::warn!(error = %error, "dynamic DASH track selection disappeared; preserving committed media");
                finish_capture!(if video.state.segments.is_empty() {
                    DashLiveCaptureOutcome::Discard
                } else {
                    DashLiveCaptureOutcome::PreserveVideoOnly
                });
            }
            Err(error) => return Err(error),
        };
        current_video_plan = refreshed.0;
        current_audio_plan = refreshed.1;
        let current_audio_bytes = audio
            .as_ref()
            .map(|(track, _)| dash_live_track_bytes(&track.state, &track.directory))
            .transpose()
            .await?
            .unwrap_or(0);

        match apply_dash_live_snapshot(
            client,
            &current_video_plan,
            &mut video,
            connections,
            tx,
            downloaded,
            abort,
            current_audio_bytes,
        )
        .await?
        {
            DashLiveUpdateOutcome::Committed => {}
            DashLiveUpdateOutcome::Stopped(intent) => {
                finish_capture!(match intent {
                    crate::video::StopIntent::Preserve => DashLiveCaptureOutcome::Preserve,
                    crate::video::StopIntent::Discard => DashLiveCaptureOutcome::Discard,
                });
            }
            DashLiveUpdateOutcome::ContinuityLost => {
                finish_capture!(if video.state.segments.is_empty() {
                    DashLiveCaptureOutcome::Discard
                } else {
                    DashLiveCaptureOutcome::PreserveVideoOnly
                });
            }
        }

        match (&mut audio, current_audio_plan.as_ref()) {
            (Some((track, _)), Some(plan)) => {
                let video_bytes = dash_live_track_bytes(&video.state, &video.directory).await?;
                match apply_dash_live_snapshot(
                    client,
                    plan,
                    track,
                    connections,
                    tx,
                    downloaded,
                    abort,
                    video_bytes,
                )
                .await?
                {
                    DashLiveUpdateOutcome::Committed => {}
                    DashLiveUpdateOutcome::Stopped(intent) => {
                        finish_capture!(match intent {
                            crate::video::StopIntent::Preserve => {
                                DashLiveCaptureOutcome::PreserveVideoOnly
                            }
                            crate::video::StopIntent::Discard => DashLiveCaptureOutcome::Discard,
                        });
                    }
                    DashLiveUpdateOutcome::ContinuityLost => {
                        finish_capture!(if video.state.segments.is_empty() {
                            DashLiveCaptureOutcome::Discard
                        } else {
                            DashLiveCaptureOutcome::PreserveVideoOnly
                        });
                    }
                }
            }
            (None, Some(plan)) => {
                let key = audio_key
                    .clone()
                    .unwrap_or_else(|| plan.representation.live_key.clone());
                let (mut track, _) = load_dash_live_track(&key, audio_dir, Some(plan)).await?;
                let video_bytes = dash_live_track_bytes(&video.state, &video.directory).await?;
                match apply_dash_live_snapshot(
                    client,
                    plan,
                    &mut track,
                    connections,
                    tx,
                    downloaded,
                    abort,
                    video_bytes,
                )
                .await?
                {
                    DashLiveUpdateOutcome::Committed => {
                        let bytes = dash_live_track_bytes(&track.state, &track.directory).await?;
                        audio_key = Some(key);
                        audio = Some((track, bytes));
                    }
                    DashLiveUpdateOutcome::Stopped(intent) => {
                        finish_capture!(match intent {
                            crate::video::StopIntent::Preserve => {
                                DashLiveCaptureOutcome::PreserveVideoOnly
                            }
                            crate::video::StopIntent::Discard => DashLiveCaptureOutcome::Discard,
                        });
                    }
                    DashLiveUpdateOutcome::ContinuityLost => {
                        finish_capture!(DashLiveCaptureOutcome::PreserveVideoOnly);
                    }
                }
            }
            (Some(_), None) => {
                finish_capture!(if video.state.segments.is_empty() {
                    DashLiveCaptureOutcome::Discard
                } else {
                    DashLiveCaptureOutcome::PreserveVideoOnly
                });
            }
            (None, None) => {}
        }

        if dash_presentation_ended(&current_video_plan.presentation) {
            finish_capture!(DashLiveCaptureOutcome::Ended);
        }
    }
}

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
    concat_dash_resource_names(&resources, directory, filename, abort).await
}

async fn concat_dash_resource_names(
    resources: &[String],
    directory: &Path,
    filename: &str,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
) -> Result<bool, String> {
    if resources.is_empty() {
        return Err("The DASH recording contains no media resources".to_string());
    }
    let target = directory.join(filename);
    let part = directory.join(format!("{filename}.part"));
    remove_regular_file_if_present(&target).await?;
    remove_regular_file_if_present(&part).await?;
    let mut output = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part)
        .await
        .map_err(|error| format!("Could not create a DASH track file: {error}"))?;
    for filename in resources {
        let input_path = directory.join(filename);
        let mut input = tokio::fs::File::open(&input_path)
            .await
            .map_err(|error| format!("Could not open a DASH segment: {error}"))?;
        tokio::select! {
            biased;
            _ = &mut *abort => return Ok(false),
            result = tokio::io::copy(&mut input, &mut output) => {
                result.map_err(|error| format!("Could not assemble a DASH track: {error}"))?;
            }
        }
    }
    tokio::select! {
        biased;
        _ = &mut *abort => return Ok(false),
        result = output.sync_all() => {
            result.map_err(|error| format!("Could not flush a DASH track: {error}"))?;
        }
    }
    drop(output);
    crate::file_names::rename_noreplace(&part, &target)
        .map(|()| true)
        .map_err(|error| format!("Could not publish a DASH track: {error}"))
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
    // .bin staging name. Permit only those names and probe their contents:
    // HLS resources may contain MPEG-TS, fragmented MP4, or encryption keys.
    command.args([
        "-allowed_extensions",
        "bin",
        "-allowed_segment_extensions",
        "bin",
        "-extension_picky",
        "0",
    ]);
}

async fn run_ffmpeg(
    ffmpeg: &Path,
    video_input: &Path,
    audio_input: Option<&Path>,
    output: &Path,
    abort: &mut tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
) -> Result<bool, ManifestRunError> {
    let mut command = tokio::process::Command::new(ffmpeg);
    if is_hls_playlist_input(video_input) {
        add_hls_staging_options(&mut command);
    }
    command
        .arg("-hide_banner")
        .arg("-nostdin")
        .arg("-n")
        .arg("-i")
        .arg(video_input);
    if let Some(audio_input) = audio_input {
        if is_hls_playlist_input(audio_input) {
            add_hls_staging_options(&mut command);
        }
        command.arg("-i").arg(audio_input);
        command.arg("-map").arg("0:v:0").arg("-map").arg("1:a:0");
    } else {
        command.arg("-map").arg("0:v:0?").arg("-map").arg("0:a:0?");
    }
    command
        .args(["-c", "copy", "-movflags", "+faststart", "-f", "mp4"])
        .arg(output)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
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
        _ = abort => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = stderr_task.await;
            return Ok(false);
        }
        status = child.wait() => status.map_err(|error| {
            ManifestRunError::Failed(format!("FFmpeg could not finish: {error}"))
        })?,
    };
    let log = stderr_task.await.unwrap_or_default();
    if !status.success() {
        let detail = log
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("FFmpeg could not mux the stream");
        return Err(ManifestRunError::Unsupported(format!(
            "FFmpeg could not mux the stream: {detail}"
        )));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            representation_key: video_key.clone(),
            audio_key: Some(alternate_audio_key),
        };
        let selected = resolve_tracks(
            &client,
            ManifestLookup {
                source_url: manifest_url.to_string(),
                manifest: NativeManifest::Dash(dash.clone()),
            },
            &selected_source,
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
            ..selected_source
        };
        let automatic = resolve_tracks(
            &client,
            ManifestLookup {
                source_url: manifest_url.to_string(),
                manifest: NativeManifest::Dash(dash),
            },
            &automatic_source,
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
    async fn run_manifest_download_rejects_a_destination_without_a_parent() {
        let job = ManifestJob {
            item_id: 7,
            source: ManifestSource {
                url: "https://cdn.example.test/master.m3u8".to_string(),
                kind: ManifestKind::Hls,
                representation_key: "hls-media".to_string(),
                audio_key: None,
            },
            dest: PathBuf::new(),
            proxy: None,
            cookies_browser: "none".to_string(),
            connections: 1,
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
        let mut seen_hosts = std::collections::HashSet::new();
        let error = guard_manifest_child_url(parent_url, &child_url, &mut seen_hosts)
            .expect_err("untrusted manifests must not reach private hosts");

        assert_eq!(
            error,
            "The stream manifest references a local or private network resource"
        );
        assert!(
            seen_hosts.is_empty(),
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
        let mut seen_hosts = std::collections::HashSet::new();
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

        let mut seen_hosts = std::collections::HashSet::new();
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
    async fn transient_resource_response_is_retried_and_completed_file_is_reused() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let directory =
            std::env::temp_dir().join(format!("grab-manifest-retry-{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&directory).await;
        tokio::fs::create_dir_all(&directory)
            .await
            .expect("create resource staging directory");

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind HTTP test listener");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            for (status, body) in [("503 Service Unavailable", ""), ("200 OK", "data")] {
                let (mut socket, _) = listener.accept().await.expect("accept resource request");
                let mut request = [0_u8; 1024];
                let _ = socket.read(&mut request).await.expect("read request");
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
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
        let size = fetch_resource(&client, resource.clone(), &directory, &tx, &downloaded)
            .await
            .expect("second resource attempt should succeed");

        server.await.expect("HTTP test server task");
        assert_eq!(size, 4);
        assert_eq!(downloaded.load(Ordering::Relaxed), 4);
        assert_eq!(
            tokio::fs::read(directory.join(resource.filename))
                .await
                .expect("completed resource"),
            b"data"
        );
        let _ = tokio::fs::remove_dir_all(directory).await;
    }

    #[tokio::test]
    async fn truncated_byte_range_is_retried_without_counting_partial_bytes() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let directory =
            std::env::temp_dir().join(format!("grab-manifest-range-retry-{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&directory).await;
        tokio::fs::create_dir_all(&directory)
            .await
            .expect("create range staging directory");

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind HTTP test listener");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            for body in ["da", "data"] {
                let (mut socket, _) = listener.accept().await.expect("accept range request");
                let mut request = [0_u8; 1024];
                let _ = socket.read(&mut request).await.expect("read range request");
                let response = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes 0-3/4\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write range response");
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
        let size = fetch_resource(&client, resource.clone(), &directory, &tx, &downloaded)
            .await
            .expect("truncated range should recover on retry");

        server.await.expect("HTTP test server task");
        assert_eq!(size, 4);
        assert_eq!(downloaded.load(Ordering::Relaxed), 4);
        assert_eq!(
            tokio::fs::read(directory.join(resource.filename))
                .await
                .expect("completed byte-range resource"),
            b"data"
        );
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

    #[test]
    fn dynamic_dash_resume_state_rejects_path_traversal_and_sequence_gaps() {
        let key = "dash-live-0123456789abcdef0123456789abcdef";
        let valid = DashLiveState {
            version: DASH_LIVE_STATE_VERSION,
            representation_key: key.to_string(),
            initialization: Some(
                "dash-live-init-0123456789abcdef0123456789abcdef.bin".to_string(),
            ),
            segments: vec![
                DashLiveSegment {
                    number: 100,
                    filename: "dash-live-segment-0123456789abcdef0123456789abcdef.bin"
                        .to_string(),
                },
                DashLiveSegment {
                    number: 101,
                    filename: "dash-live-segment-1123456789abcdef0123456789abcdef.bin"
                        .to_string(),
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

        assert!(validate_dash_live_state(&valid, "dash-live-ffffffffffffffffffffffffffffffff").is_err());
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
            state: DashLiveState {
                version: DASH_LIVE_STATE_VERSION,
                representation_key: representation_key.to_string(),
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
            load_dash_live_track(representation_key, &directory, None)
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
            dash_live_track_bytes(&restored.state, &directory)
                .await
                .expect("sum restored staged resources"),
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
#[path = "manifest_runner/integration_tests.rs"]
mod integration_tests;
