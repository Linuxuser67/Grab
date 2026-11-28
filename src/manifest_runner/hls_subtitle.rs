//! Bounded native HLS subtitle staging for incremental VOD muxing.

use std::path::{Path, PathBuf};
use std::sync::{atomic::AtomicU64, Arc};

use tokio::sync::{mpsc::UnboundedSender, oneshot};

use crate::engine_msg::EngineMsg;
use crate::manifest::HlsPlan;

use super::{
    HLS_STATIC_RESOURCE_CONNECTIONS, ManifestRunError, ensure_track_dir, fetch_resources,
    hls_resources, write_playlist,
};

/// A subtitle playlist can join incremental VOD only when it is finite and
/// does not rely on live or low-latency reload semantics.
pub(super) fn supports_incremental_vod(plan: &HlsPlan) -> bool {
    !plan.is_live
        && plan.end_list
        && plan.low_latency.is_none()
        && !plan.resources.is_empty()
}

/// The local subtitle playlist consumed by FFmpeg after its resources are staged.
pub(super) fn playlist_path(staging: &Path) -> PathBuf {
    staging.join("subtitles/local.m3u8")
}

/// Fetch and publish a complete local subtitle playlist while media resources
/// continue to arrive through the incremental video/audio workers.
pub(super) async fn fetch(
    client: reqwest::Client,
    plan: Arc<HlsPlan>,
    staging: PathBuf,
    tx: UnboundedSender<EngineMsg>,
    downloaded: Arc<AtomicU64>,
    mut abort: oneshot::Receiver<crate::video::StopIntent>,
) -> Result<bool, ManifestRunError> {
    let directory = staging.join("subtitles");
    ensure_track_dir(&directory).await?;
    if !fetch_resources(
        &client,
        hls_resources(plan.as_ref()),
        &directory,
        HLS_STATIC_RESOURCE_CONNECTIONS,
        &tx,
        downloaded.as_ref(),
        &mut abort,
    )
    .await?
    {
        return Ok(false);
    }

    write_playlist(&directory, plan.as_ref()).await?;
    Ok(true)
}
