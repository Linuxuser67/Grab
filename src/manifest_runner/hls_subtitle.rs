//! Bounded native HLS subtitle staging for incremental VOD muxing.

use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::AtomicU64};

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
    !plan.is_live && plan.end_list && plan.low_latency.is_none() && !plan.resources.is_empty()
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
        &downloaded,
        &mut abort,
    )
    .await?
    {
        return Ok(false);
    }

    write_playlist(&directory, plan.as_ref()).await?;
    Ok(true)
}


#[cfg(test)]
mod tests {
    use super::supports_incremental_vod;
    use super::super::{SubtitleFormat, Tracks};
    use crate::manifest::{HlsPlan, plan_hls_media_playlist};

    const SUBTITLE_URL: &str = "https://cdn.example.test/subtitles/index.m3u8";
    const FINITE_VOD: &[u8] = br#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXTINF:4.0,
caption.vtt
#EXT-X-ENDLIST
"#;

    fn plan(body: &[u8]) -> HlsPlan {
        plan_hls_media_playlist(SUBTITLE_URL, body).expect("parse HLS test playlist")
    }

    #[test]
    fn accepts_finite_end_list_vod_with_resources() {
        assert!(supports_incremental_vod(&plan(FINITE_VOD)));
    }

    #[test]
    fn rejects_event_playlist_without_end_list() {
        let plan = plan(
            br#"#EXTM3U
#EXT-X-PLAYLIST-TYPE:EVENT
#EXT-X-TARGETDURATION:4
#EXTINF:4.0,
caption.vtt
"#,
        );
        assert!(plan.is_live);
        assert!(!plan.end_list);
        assert!(!plan.resources.is_empty());
        assert!(!supports_incremental_vod(&plan));
    }

    #[test]
    fn rejects_playlist_without_end_list_or_vod_type() {
        let plan = plan(
            br#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXTINF:4.0,
caption.vtt
"#,
        );
        assert!(plan.is_live);
        assert!(!plan.end_list);
        assert!(!supports_incremental_vod(&plan));
    }

    #[test]
    fn rejects_low_latency_hls() {
        let plan = plan(
            br#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:100
#EXT-X-MAP:URI="init.mp4"
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK=1.5
#EXT-X-PART-INF:PART-TARGET=0.5
#EXT-X-PART:DURATION=0.5,URI="100.0.m4s",INDEPENDENT=YES
#EXTINF:0.5,
100.m4s
#EXT-X-PRELOAD-HINT:TYPE=PART,URI="100.1.m4s"
#EXT-X-RENDITION-REPORT:URI="audio.m3u8",LAST-MSN=100,LAST-PART=0
"#,
        );
        assert!(plan.low_latency.is_some());
        assert!(!supports_incremental_vod(&plan));
    }

    #[test]
    fn rejects_empty_resource_plan() {
        let mut plan = plan(FINITE_VOD);
        plan.resources = Vec::new().into_boxed_slice();
        assert!(!plan.is_live);
        assert!(plan.end_list);
        assert!(!supports_incremental_vod(&plan));
    }

    #[test]
    fn unsupported_subtitle_selects_staged_hls_fallback() {
        let video = plan(
            br#"#EXTM3U
#EXT-X-VERSION:7
#EXT-X-TARGETDURATION:4
#EXT-X-MAP:URI="init.mp4"
#EXTINF:4.0,
segment-000.m4s
#EXT-X-DISCONTINUITY
#EXTINF:4.0,
segment-001.m4s
#EXT-X-ENDLIST
"#,
        );
        assert!(
            video.is_incremental_clear_vod_candidate(),
            "the video alone must qualify for incremental VOD"
        );
        let without_subtitle = Tracks::Hls {
            video: Box::new(video.clone()),
            audio: None,
            subtitle: None,
            subtitle_language: None,
            subtitle_format: SubtitleFormat::Text,
        };
        assert!(without_subtitle.uses_incremental_hls_vod());

        let live_subtitle = plan(
            br#"#EXTM3U
#EXT-X-PLAYLIST-TYPE:EVENT
#EXT-X-TARGETDURATION:4
#EXTINF:4.0,
caption.vtt
"#,
        );
        let with_unsupported_subtitle = Tracks::Hls {
            video: Box::new(video),
            audio: None,
            subtitle: Some(Box::new(live_subtitle)),
            subtitle_language: Some("en".to_string()),
            subtitle_format: SubtitleFormat::Text,
        };
        assert!(
            !with_unsupported_subtitle.uses_incremental_hls_vod(),
            "unsupported subtitle plans must use the staged HLS fallback"
        );
    }
}
