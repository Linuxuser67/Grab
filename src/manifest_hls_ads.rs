//! Removes only complete, explicitly marked HLS ad breaks before resource planning.

use std::collections::HashMap;

use m3u8_rs::{ExtTag, MediaPlaylist};

use super::ManifestError;

const CUE_ELAPSED_TOLERANCE_SECONDS: f64 = 0.25;
// Publishers can round CUE-OUT-CONT duration to whole seconds while CUE-OUT
// carries fractional seconds; the published MediaTailor example differs by 0.98s.
const CUE_DURATION_TOLERANCE_SECONDS: f64 = 1.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AdSegmentDisposition {
    Keep,
    KeepAndStripMarkers,
    Remove,
}

pub(super) struct AdRemovalPlan {
    pub(super) segments: Vec<AdSegmentDisposition>,
}

/// Identify ad segments from paired CUE markers without changing source identities.
///
/// Live windows are rejected because dropping a variable number of segments
/// would change the sequence mapping used by the resumable live accumulator.
pub(super) fn plan_ad_removal(
    playlist: &MediaPlaylist,
    is_live: bool,
) -> Result<AdRemovalPlan, ManifestError> {
    if playlist
        .segments
        .iter()
        .any(|segment| segment.daterange.is_some())
    {
        return Err(unsupported_ad_marker(
            "EXT-X-DATERANGE boundaries are not mapped to exact media segments",
        ));
    }

    let has_ad_marker = playlist
        .segments
        .iter()
        .flat_map(|segment| segment.unknown_tags.iter())
        .any(is_ad_marker_tag);
    if !has_ad_marker {
        return Ok(AdRemovalPlan {
            segments: vec![AdSegmentDisposition::Keep; playlist.segments.len()],
        });
    }
    if is_live {
        return Err(unsupported_ad_marker(
            "live ad breaks cross manifest snapshots and cannot be resumed safely yet",
        ));
    }
    if playlist.start.is_some() {
        return Err(unsupported_ad_marker(
            "EXT-X-START offsets cannot be preserved after removing ad segments",
        ));
    }

    let segment_count = playlist.segments.len();
    let mut segments = vec![AdSegmentDisposition::Keep; segment_count];
    let mut in_ad_break = false;
    let mut partial_break = false;
    let mut expected_elapsed = 0.0_f64;
    let mut actual_duration = 0.0_f64;
    let mut declared_duration = None;

    for (index, segment) in playlist.segments.iter().enumerate() {
        let cue_out = one_marker(&segment.unknown_tags, "X-CUE-OUT")?;
        let cue_out_cont = one_marker(&segment.unknown_tags, "X-CUE-OUT-CONT")?;
        let cue_in = one_marker(&segment.unknown_tags, "X-CUE-IN")?;

        if cue_out.is_some() && (cue_out_cont.is_some() || cue_in.is_some()) {
            return Err(unsupported_ad_marker(
                "multiple CUE boundaries share one segment",
            ));
        }
        if cue_in.is_some() && cue_out_cont.is_some() {
            return Err(unsupported_ad_marker(
                "CUE-IN and CUE-OUT-CONT share one segment",
            ));
        }
        if cue_in
            .and_then(|tag| tag.rest.as_deref())
            .is_some_and(|value| !value.trim().is_empty())
        {
            return Err(unsupported_ad_marker("CUE-IN unexpectedly has attributes"));
        }

        let oatcls = one_marker(&segment.unknown_tags, "OATCLS-SCTE35")?;
        if oatcls.is_some() {
            let associated_boundary =
                cue_out.is_some() || cue_out_cont.is_some() || cue_in.is_some() || in_ad_break;
            if !associated_boundary
                || !oatcls
                    .and_then(|tag| tag.rest.as_deref())
                    .is_some_and(is_base64_marker)
            {
                return Err(unsupported_ad_marker(
                    "standalone or malformed OATCLS-SCTE35 marker",
                ));
            }
        }
        let asset = one_marker(&segment.unknown_tags, "X-ASSET")?;
        if asset.is_some() {
            let associated_with_ad = cue_out.is_some() || cue_out_cont.is_some() || in_ad_break;
            let valid_attributes = asset
                .and_then(|tag| tag.rest.as_deref())
                .is_some_and(|value| parse_attributes(value, &["CAID", "UPID"]).is_ok());
            if !associated_with_ad || !valid_attributes {
                return Err(unsupported_ad_marker(
                    "standalone or malformed X-ASSET marker",
                ));
            }
        }

        if let Some(tag) = cue_out {
            if in_ad_break {
                return Err(unsupported_ad_marker("nested CUE-OUT marker"));
            }
            declared_duration = parse_cue_out_duration(tag)?;
            actual_duration = 0.0;
            expected_elapsed = 0.0;
            partial_break = false;
            in_ad_break = true;
            segments[index] = AdSegmentDisposition::Remove;
        } else if let Some(tag) = cue_out_cont {
            let (elapsed, duration) = parse_cue_out_cont(tag)?;
            if !in_ad_break {
                if index != 0 {
                    return Err(unsupported_ad_marker(
                        "CUE-OUT-CONT starts after program segments without CUE-OUT",
                    ));
                }
                in_ad_break = true;
                partial_break = true;
                declared_duration = Some(duration);
                expected_elapsed = elapsed;
                actual_duration = 0.0;
            } else {
                if declared_duration.is_some_and(|current| {
                    (current - duration).abs() > CUE_DURATION_TOLERANCE_SECONDS
                }) {
                    return Err(unsupported_ad_marker(
                        "CUE-OUT-CONT duration changes within one ad break",
                    ));
                }
            }
            if elapsed >= duration {
                return Err(unsupported_ad_marker(
                    "CUE-OUT-CONT elapsed time is outside its duration",
                ));
            }
            if declared_duration.is_none() {
                declared_duration = Some(duration);
            }
            segments[index] = AdSegmentDisposition::Remove;
        } else if cue_in.is_some() {
            if !in_ad_break {
                return Err(unsupported_ad_marker("CUE-IN has no preceding ad break"));
            }
            if !partial_break
                && declared_duration.is_some_and(|duration| {
                    (actual_duration - duration).abs() > CUE_DURATION_TOLERANCE_SECONDS
                })
            {
                return Err(unsupported_ad_marker(
                    "CUE-OUT duration disagrees with the marked ad segment interval",
                ));
            }
            in_ad_break = false;
            partial_break = false;
            declared_duration = None;
            expected_elapsed = 0.0;
            actual_duration = 0.0;
            segments[index] = AdSegmentDisposition::KeepAndStripMarkers;
        } else if in_ad_break {
            segments[index] = AdSegmentDisposition::Remove;
        }

        if segments[index] == AdSegmentDisposition::Remove {
            if !segment.duration.is_finite() || segment.duration <= 0.0 {
                return Err(ManifestError::InvalidDuration);
            }
            if segment.discontinuity {
                return Err(unsupported_ad_marker(
                    "ad interval contains a discontinuity boundary",
                ));
            }
            actual_duration += f64::from(segment.duration);
            if let Some(tag) = cue_out_cont {
                let (elapsed, duration) = parse_cue_out_cont(tag)?;
                if (elapsed - expected_elapsed).abs() > CUE_ELAPSED_TOLERANCE_SECONDS {
                    return Err(unsupported_ad_marker(
                        "CUE-OUT-CONT elapsed time does not match preceding ad segments",
                    ));
                }
                if elapsed + f64::from(segment.duration) > duration + CUE_DURATION_TOLERANCE_SECONDS
                {
                    return Err(unsupported_ad_marker(
                        "ad segment extends beyond the declared CUE-OUT-CONT duration",
                    ));
                }
                expected_elapsed = elapsed + f64::from(segment.duration);
            } else {
                expected_elapsed += f64::from(segment.duration);
            }
            if declared_duration.is_some_and(|duration| {
                expected_elapsed > duration + CUE_DURATION_TOLERANCE_SECONDS
            }) {
                return Err(unsupported_ad_marker(
                    "ad segments exceed the declared CUE-OUT duration",
                ));
            }
        }
    }

    if in_ad_break {
        return Err(unsupported_ad_marker(
            "ad break has no matching CUE-IN before ENDLIST",
        ));
    }

    Ok(AdRemovalPlan { segments })
}

pub(super) fn is_ad_marker_tag(tag: &ExtTag) -> bool {
    matches!(
        tag.tag.as_str(),
        "X-CUE-OUT" | "X-CUE-OUT-CONT" | "X-CUE-IN" | "OATCLS-SCTE35" | "X-ASSET"
    )
}

fn one_marker<'a>(tags: &'a [ExtTag], name: &str) -> Result<Option<&'a ExtTag>, ManifestError> {
    let mut matches = tags.iter().filter(|tag| tag.tag == name);
    let first = matches.next();
    if matches.next().is_some() {
        return Err(unsupported_ad_marker("duplicate marker on one segment"));
    }
    Ok(first)
}

fn parse_cue_out_duration(tag: &ExtTag) -> Result<Option<f64>, ManifestError> {
    let Some(value) = tag.rest.as_deref() else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if let Some(duration) = parse_positive_decimal(value) {
        return Ok(Some(duration));
    }
    let attributes = parse_attributes(value, &["DURATION"])?;
    let duration = attributes
        .get("DURATION")
        .and_then(|value| parse_positive_decimal(value))
        .ok_or_else(|| unsupported_ad_marker("CUE-OUT duration is malformed"))?;
    Ok(Some(duration))
}

fn parse_cue_out_cont(tag: &ExtTag) -> Result<(f64, f64), ManifestError> {
    let value = tag
        .rest
        .as_deref()
        .ok_or_else(|| unsupported_ad_marker("CUE-OUT-CONT has no attributes"))?;
    let attributes = parse_attributes(value, &["ELAPSEDTIME", "DURATION", "SCTE35"])?;
    let elapsed = attributes
        .get("ELAPSEDTIME")
        .and_then(|value| parse_non_negative_decimal(value))
        .ok_or_else(|| unsupported_ad_marker("CUE-OUT-CONT elapsed time is malformed"))?;
    let duration = attributes
        .get("DURATION")
        .and_then(|value| parse_positive_decimal(value))
        .ok_or_else(|| unsupported_ad_marker("CUE-OUT-CONT duration is malformed"))?;
    if attributes
        .get("SCTE35")
        .is_some_and(|value| !is_base64_marker(value))
    {
        return Err(unsupported_ad_marker(
            "CUE-OUT-CONT SCTE35 data is malformed",
        ));
    }
    Ok((elapsed, duration))
}

fn parse_attributes(
    value: &str,
    allowed: &[&str],
) -> Result<HashMap<String, String>, ManifestError> {
    let mut attributes = HashMap::new();
    for attribute in value.split(',') {
        let (name, value) = attribute
            .split_once('=')
            .ok_or_else(|| unsupported_ad_marker("malformed ad-marker attribute"))?;
        let name = name.trim().to_ascii_uppercase();
        let value = value.trim();
        let value = if let Some(quoted) = value.strip_prefix('"') {
            let Some(unquoted) = quoted.strip_suffix('"') else {
                return Err(unsupported_ad_marker(
                    "malformed quoted ad-marker attribute",
                ));
            };
            if unquoted.contains('"') {
                return Err(unsupported_ad_marker(
                    "malformed quoted ad-marker attribute",
                ));
            }
            unquoted
        } else {
            if value.contains('"') {
                return Err(unsupported_ad_marker(
                    "malformed quoted ad-marker attribute",
                ));
            }
            value
        };
        if value.is_empty()
            || !allowed.iter().any(|allowed_name| *allowed_name == name)
            || attributes.insert(name, value.to_string()).is_some()
        {
            return Err(unsupported_ad_marker("unsupported ad-marker attribute"));
        }
    }
    Ok(attributes)
}

fn parse_positive_decimal(value: &str) -> Option<f64> {
    let value = value.parse::<f64>().ok()?;
    (value.is_finite() && value > 0.0).then_some(value)
}

fn parse_non_negative_decimal(value: &str) -> Option<f64> {
    let value = value.parse::<f64>().ok()?;
    (value.is_finite() && value >= 0.0).then_some(value)
}

fn is_base64_marker(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return false;
    }
    let padding = if bytes.ends_with(b"==") {
        2
    } else if bytes.ends_with(b"=") {
        1
    } else {
        0
    };
    let payload = &bytes[..bytes.len() - padding];
    !payload.is_empty()
        && payload
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'))
}

fn unsupported_ad_marker(reason: &str) -> ManifestError {
    ManifestError::UnsupportedAdInsertion(reason.to_string())
}

#[cfg(test)]
mod tests {
    use crate::manifest::plan_hls_media_playlist;

    const BASE_URL: &str = "https://cdn.example.test/path/master.m3u8";

    #[test]
    fn aws_cue_markers_remove_only_the_ad_segment_interval() {
        let body = include_bytes!("../tests/fixtures/hls-ad-marker-metadata.m3u8");
        let plan = plan_hls_media_playlist(BASE_URL, body)
            .expect("paired ad markers should be planned after dropping ad media");

        let expected_urls: &[&str] = &[
            "https://cdn.example.test/path/segment-8779957.ts?m=1566416212",
            "https://cdn.example.test/path/segment-8779958.ts?m=1566416212",
            "https://cdn.example.test/path/segment-8779959.ts?m=1566416212",
            "https://cdn.example.test/path/segment-8779965.ts?m=1566416212",
            "https://cdn.example.test/path/segment-8779966.ts?m=1566416212",
            "https://cdn.example.test/path/segment-8779967.ts?m=1566416212",
            "https://cdn.example.test/path/segment-8779968.ts?m=1566416212",
        ];
        let actual_urls = plan
            .resources
            .iter()
            .filter(|resource| resource.kind == crate::manifest::HlsResourceKind::Segment)
            .map(|resource| resource.url.as_str())
            .collect::<Vec<_>>();
        assert_eq!(actual_urls.as_slice(), expected_urls);

        let local = plan
            .local_playlist_bytes()
            .expect("serialize local playlist");
        let local = String::from_utf8(local).expect("local playlist is UTF-8");
        assert!(!local.contains("EXT-X-CUE"));
        assert!(!local.contains("EXT-OATCLS-SCTE35"));
        assert!(!local.contains("EXT-X-ASSET"));
        let parsed = m3u8_rs::parse_media_playlist_res(local.as_bytes())
            .expect("parse ad-free local playlist");
        assert_eq!(parsed.segments.len(), expected_urls.len());
    }

    #[test]
    fn incomplete_or_ambiguous_ad_markers_fall_back() {
        for body in [
            b"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-CUE-OUT:8
#EXTINF:4,
ad.ts
#EXT-X-ENDLIST
"
            .as_slice(),
            b"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-CUE-IN
#EXTINF:4,
program.ts
#EXT-X-ENDLIST
"
            .as_slice(),
            b"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXTINF:4,
program.ts
#EXT-X-CUE-OUT-CONT:ElapsedTime=2,Duration=8
#EXTINF:4,
ad.ts
#EXT-X-CUE-IN
#EXTINF:4,
program2.ts
#EXT-X-ENDLIST
"
            .as_slice(),
        ] {
            assert!(
                matches!(
                    plan_hls_media_playlist(BASE_URL, body).err(),
                    Some(crate::manifest::ManifestError::UnsupportedAdInsertion(_))
                ),
                "ambiguous markers must fall back"
            );
        }
    }

    #[test]
    fn live_ad_markers_and_daterange_scte_signals_fall_back() {
        let live = b"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-CUE-OUT:8
#EXTINF:4,
ad.ts
";
        assert!(matches!(
            plan_hls_media_playlist(BASE_URL, live).err(),
            Some(crate::manifest::ManifestError::UnsupportedAdInsertion(_))
        ));

        for daterange in [
            br#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-DATERANGE:ID="splice",CLASS="com.apple.hls.interstitial",START-DATE="2026-01-01T00:00:00Z",PLANNED-DURATION=8.0,SCTE35-OUT=0xFC30
#EXTINF:4,
ad.ts
#EXT-X-ENDLIST
"#.as_slice(),
            br#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-DATERANGE:ID="event",CLASS="com.example.unknown",START-DATE="2026-01-01T00:00:00Z",DURATION=4.0
#EXTINF:4,
program.ts
#EXT-X-ENDLIST
"#.as_slice(),
        ] {
            assert!(matches!(
                plan_hls_media_playlist(BASE_URL, daterange).err(),
                Some(crate::manifest::ManifestError::UnsupportedAdInsertion(_))
            ));
        }
    }

    #[test]
    fn playlists_without_ad_signals_keep_all_segments() {
        let body = b"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXTINF:4,
program.ts
#EXTINF:4,
program2.ts
#EXT-X-ENDLIST
";
        let plan =
            plan_hls_media_playlist(BASE_URL, body).expect("ordinary playlists remain unchanged");
        assert_eq!(
            plan.resources
                .iter()
                .filter(|resource| resource.kind == crate::manifest::HlsResourceKind::Segment)
                .count(),
            2
        );
    }
    #[test]
    fn skipped_ad_keys_are_not_fetched_and_content_key_rotation_is_preserved() {
        let body = br#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-KEY:METHOD=AES-128,URI="keys/content.key",IV=0x00000000000000000000000000000001
#EXTINF:4,
program-before.ts
#EXT-X-KEY:METHOD=AES-128,URI="keys/ad.key",IV=0x00000000000000000000000000000002
#EXT-X-CUE-OUT:8
#EXTINF:4,
ad-one.ts
#EXTINF:4,
ad-two.ts
#EXT-X-CUE-IN
#EXT-X-KEY:METHOD=AES-128,URI="keys/content.key",IV=0x00000000000000000000000000000001
#EXTINF:4,
program-after.ts
#EXT-X-ENDLIST
"#;
        let plan = plan_hls_media_playlist(BASE_URL, body)
            .expect("paired cue markers should remove the ad resources");
        assert_eq!(
            plan.resources
                .iter()
                .filter(|resource| resource.kind == crate::manifest::HlsResourceKind::Segment)
                .count(),
            2
        );
        assert_eq!(
            plan.resources
                .iter()
                .filter(|resource| resource.kind == crate::manifest::HlsResourceKind::Key)
                .count(),
            1
        );
        assert!(
            plan.resources
                .iter()
                .all(|resource| !resource.url.as_str().contains("ad.key"))
        );
    }

    #[test]
    fn ad_only_initialization_map_is_not_planned_when_program_map_is_restored() {
        let body = br#"#EXTM3U
#EXT-X-VERSION:6
#EXT-X-TARGETDURATION:4
#EXT-X-MAP:URI="init-content.mp4"
#EXTINF:4,
program-before.m4s
#EXT-X-MAP:URI="init-ad.mp4"
#EXT-X-CUE-OUT:8
#EXTINF:4,
ad-one.m4s
#EXTINF:4,
ad-two.m4s
#EXT-X-CUE-IN
#EXT-X-MAP:URI="init-content.mp4"
#EXTINF:4,
program-after.m4s
#EXT-X-ENDLIST
"#;
        let plan = plan_hls_media_playlist(BASE_URL, body)
            .expect("paired cue markers should omit the ad initialization resource");
        let init_urls = plan
            .resources
            .iter()
            .filter(|resource| resource.kind == crate::manifest::HlsResourceKind::InitMap)
            .map(|resource| resource.url.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            init_urls,
            vec!["https://cdn.example.test/path/init-content.mp4"]
        );
        assert!(
            plan.resources
                .iter()
                .all(|resource| !resource.url.as_str().contains("init-ad.mp4")),
            "ad-only initialization maps must not be fetched"
        );
    }

    #[test]
    fn method_none_reset_inside_removed_ad_is_reapplied_to_program_segment() {
        let body = br#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-KEY:METHOD=AES-128,URI="keys/content.key",IV=0x00000000000000000000000000000001
#EXTINF:4,
encrypted-program.ts
#EXT-X-KEY:METHOD=NONE
#EXT-X-CUE-OUT:8
#EXTINF:4,
clear-ad-one.ts
#EXTINF:4,
clear-ad-two.ts
#EXT-X-CUE-IN
#EXTINF:4,
clear-program.ts
#EXT-X-ENDLIST
"#;
        let plan = plan_hls_media_playlist(BASE_URL, body)
            .expect("a reset in an omitted ad must remain active for the program segment");
        let local_bytes = plan
            .local_playlist_bytes()
            .expect("serialize local playlist");
        let local_text =
            std::str::from_utf8(&local_bytes).expect("serialized local playlist is UTF-8");
        assert!(
            local_text.contains("#EXT-X-KEY:METHOD=NONE"),
            "removing the in-ad reset must emit a METHOD=NONE tag before clear program media"
        );
        let local = m3u8_rs::parse_media_playlist_res(&local_bytes)
            .expect("parse the ad-free local playlist");
        assert_eq!(local.segments.len(), 2);
        assert_eq!(
            local.segments[0].key.as_ref().map(|key| &key.method),
            Some(&m3u8_rs::KeyMethod::AES128)
        );
        // m3u8-rs represents METHOD=NONE as no active key in its parsed model.
        assert!(local.segments[1].key.is_none());
    }

    #[test]
    fn implicit_byte_ranges_continue_across_omitted_ad_segments() {
        let body = br#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-BYTERANGE:4@0
#EXTINF:4,
media.ts
#EXT-X-CUE-OUT:8
#EXT-X-BYTERANGE:4
#EXTINF:4,
media.ts
#EXT-X-BYTERANGE:4
#EXTINF:4,
media.ts
#EXT-X-CUE-IN
#EXT-X-BYTERANGE:4
#EXTINF:4,
media.ts
#EXT-X-ENDLIST
"#;
        let plan = plan_hls_media_playlist(BASE_URL, body)
            .expect("implicit byte ranges after the ad should retain source offsets");
        let ranges = plan
            .resources
            .iter()
            .filter(|resource| resource.kind == crate::manifest::HlsResourceKind::Segment)
            .map(|resource| resource.range.map(|range| (range.offset, range.length)))
            .collect::<Vec<_>>();
        assert_eq!(ranges, vec![Some((0, 4)), Some((12, 4))]);
    }

    #[test]
    fn ad_intervals_with_discontinuities_or_start_offsets_fall_back() {
        let discontinuity = br#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-CUE-OUT:4
#EXT-X-DISCONTINUITY
#EXTINF:4,
ad.ts
#EXT-X-CUE-IN
#EXTINF:4,
program.ts
#EXT-X-ENDLIST
"#;
        assert!(matches!(
            plan_hls_media_playlist(BASE_URL, discontinuity).err(),
            Some(crate::manifest::ManifestError::UnsupportedAdInsertion(_))
        ));

        let start_offset = br#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-START:TIME-OFFSET=1.0
#EXT-X-CUE-OUT:4
#EXTINF:4,
ad.ts
#EXT-X-CUE-IN
#EXTINF:4,
program.ts
#EXT-X-ENDLIST
"#;
        assert!(matches!(
            plan_hls_media_playlist(BASE_URL, start_offset).err(),
            Some(crate::manifest::ManifestError::UnsupportedAdInsertion(_))
        ));
    }

    #[test]
    fn malformed_ad_marker_attributes_and_cue_timing_fall_back() {
        let malformed_asset = br#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-CUE-OUT:8
#EXT-X-ASSET:CAID="unterminated
#EXTINF:4,
ad-one.ts
#EXTINF:4,
ad-two.ts
#EXT-X-CUE-IN
#EXTINF:4,
program.ts
#EXT-X-ENDLIST
"#;
        let malformed_cue_out = br#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-CUE-OUT:DURATION="8
#EXTINF:4,
ad-one.ts
#EXTINF:4,
ad-two.ts
#EXT-X-CUE-IN
#EXTINF:4,
program.ts
#EXT-X-ENDLIST
"#;
        let resource_bearing_continuation = br#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-CUE-OUT:8
#EXTINF:4,
ad-one.ts
#EXT-X-CUE-OUT-CONT:ElapsedTime=4,Duration=8,URI="https://ads.example.test/ad.ts"
#EXTINF:4,
ad-two.ts
#EXT-X-CUE-IN
#EXTINF:4,
program.ts
#EXT-X-ENDLIST
"#;

        for body in [
            malformed_asset.as_slice(),
            malformed_cue_out.as_slice(),
            resource_bearing_continuation.as_slice(),
        ] {
            assert!(
                matches!(
                    plan_hls_media_playlist(BASE_URL, body).err(),
                    Some(crate::manifest::ManifestError::UnsupportedAdInsertion(_))
                ),
                "malformed or resource-bearing ad markers must remain on fallback"
            );
        }

        let mismatched_elapsed = br#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-CUE-OUT:8
#EXTINF:4,
ad-one.ts
#EXT-X-CUE-OUT-CONT:ElapsedTime=6,Duration=8
#EXTINF:4,
ad-two.ts
#EXT-X-CUE-IN
#EXTINF:4,
program.ts
#EXT-X-ENDLIST
"#;
        assert!(matches!(
            plan_hls_media_playlist(BASE_URL, mismatched_elapsed).err(),
            Some(crate::manifest::ManifestError::UnsupportedAdInsertion(_))
        ));
    }

    #[test]
    fn resource_affecting_and_unrecognized_tags_still_fall_back() {
        for tag in [
            "#EXT-X-CONTENT-STEERING:SERVER-URI=\"steering.json\"",
            "#EXT-X-AD-UNKNOWN:URI=\"other.m3u8\"",
        ] {
            let body = format!(
                "#EXTM3U\n#EXT-X-TARGETDURATION:4\n{tag}\n#EXTINF:4,\nprogram.ts\n#EXT-X-ENDLIST\n"
            );
            assert!(
                matches!(
                    plan_hls_media_playlist(BASE_URL, body.as_bytes()).err(),
                    Some(crate::manifest::ManifestError::UnsupportedPlaylistFeature(
                        _
                    ))
                ),
                "unsupported tag must remain a fallback: {tag}"
            );
        }
    }
}
