//! Conservative handling for unavailable HLS media segments.

use m3u8_rs::MediaPlaylist;

use super::ManifestError;

pub(super) struct GapRemovalPlan {
    /// Source segment positions that must not be requested or written locally.
    pub(super) segments: Vec<bool>,
}

/// Omit explicitly unavailable segments from complete media playlists.
///
/// A live gap changes the sequence-to-resource history across snapshots, so
/// live and LL-HLS captures remain on the yt-dlp fallback path. The local
/// playlist contains only resources Grab actually fetched; it does not invent
/// a discontinuity because EXT-X-GAP alone does not assert a timestamp reset.
pub(super) fn plan_gap_removal(
    playlist: &MediaPlaylist,
    is_live: bool,
) -> Result<GapRemovalPlan, ManifestError> {
    let mut segments = vec![false; playlist.segments.len()];
    for (index, segment) in playlist.segments.iter().enumerate() {
        let mut gap_tags = segment
            .unknown_tags
            .iter()
            .filter(|tag| tag.tag == "GAP");
        let Some(gap_tag) = gap_tags.next() else {
            continue;
        };
        if gap_tags.next().is_some()
            || gap_tag
                .rest
                .as_deref()
                .is_some_and(|attributes| !attributes.trim().is_empty())
        {
            return Err(unsupported_gap("duplicate or malformed EXT-X-GAP tag"));
        }
        segments[index] = true;
    }

    if segments.iter().any(|skip| *skip) && is_live {
        return Err(unsupported_gap(
            "live gaps cannot be reconciled safely across playlist snapshots",
        ));
    }

    Ok(GapRemovalPlan { segments })
}

fn unsupported_gap(reason: &str) -> ManifestError {
    ManifestError::UnsupportedPlaylistFeature(format!("#EXT-X-GAP: {reason}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{HlsResourceKind, plan_hls_media_playlist};

    const BASE_URL: &str = "https://cdn.example.test/path/media.m3u8";

    #[test]
    fn complete_playlist_skips_gap_resources_without_fabricating_discontinuities() {
        let body = b"#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXTINF:4,\nfirst.ts\n#EXT-X-GAP\n#EXTINF:4,\nunavailable.ts\n#EXTINF:4,\nlast.ts\n#EXT-X-ENDLIST\n";
        let plan = plan_hls_media_playlist(BASE_URL, body)
            .expect("complete playlist with a gap should be planned");

        let segment_urls = plan
            .resources
            .iter()
            .filter(|resource| resource.kind == HlsResourceKind::Segment)
            .map(|resource| resource.url.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            segment_urls,
            [
                "https://cdn.example.test/path/first.ts",
                "https://cdn.example.test/path/last.ts",
            ]
        );

        let local_bytes = plan.local_playlist_bytes().expect("local playlist");
        let local_text = String::from_utf8(local_bytes).expect("local playlist is UTF-8");
        assert!(!local_text.contains("#EXT-X-GAP"));
        let local = m3u8_rs::parse_media_playlist_res(local_text.as_bytes())
            .expect("parse local playlist");
        assert_eq!(local.segments.len(), 2);
        assert!(
            local.segments.iter().all(|segment| !segment.discontinuity),
            "a missing segment alone does not signal a timestamp discontinuity"
        );
    }

    #[test]
    fn implicit_byte_ranges_continue_across_a_skipped_gap() {
        let body = b"#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXTINF:4,\n#EXT-X-BYTERANGE:4@0\nmedia.mp4\n#EXT-X-GAP\n#EXTINF:4,\n#EXT-X-BYTERANGE:4\nmedia.mp4\n#EXTINF:4,\n#EXT-X-BYTERANGE:4\nmedia.mp4\n#EXT-X-ENDLIST\n";
        let plan = plan_hls_media_playlist(BASE_URL, body)
            .expect("implicit range after a gap should retain its source offset");

        let segments = plan
            .resources
            .iter()
            .filter(|resource| resource.kind == HlsResourceKind::Segment)
            .collect::<Vec<_>>();
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].range.expect("first range").offset, 0);
        assert_eq!(segments[1].range.expect("third source range").offset, 8);
    }

    #[test]
    fn live_playlists_with_gap_markers_fall_back() {
        let body =
            b"#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4,\nfirst.ts\n#EXT-X-GAP\n#EXTINF:4,\nunavailable.ts\n";
        assert!(matches!(
            plan_hls_media_playlist(BASE_URL, body),
            Err(ManifestError::UnsupportedPlaylistFeature(_))
        ));
    }

    #[test]
    fn malformed_gap_markers_fall_back() {
        for marker in ["#EXT-X-GAP:VALUE=YES", "#EXT-X-GAP\n#EXT-X-GAP"] {
            let body = format!(
                "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4,\nfirst.ts\n{marker}\n#EXTINF:4,\nunavailable.ts\n#EXT-X-ENDLIST\n"
            );
            assert!(
                plan_hls_media_playlist(BASE_URL, body.as_bytes()).is_err(),
                "malformed marker must fail closed: {marker}"
            );
        }
    }
}
