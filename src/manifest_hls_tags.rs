//! Conservative HLS extension-tag policy for native manifest planning.

/// Whether a parser-unknown tag is documented metadata that cannot select
/// or redirect a media resource.
///
/// The allowlist is tied to specific ad-marker formats. Their attributes are
/// validated so URI-bearing or unfamiliar variants remain extractor fallbacks.
pub(super) fn is_benign_hls_tag(tag: &m3u8_rs::ExtTag) -> bool {
    let Some(rest) = tag.rest.as_deref() else {
        return tag.tag == "X-CUE-IN";
    };

    match tag.tag.as_str() {
        "X-ALLOW-CACHE" => matches!(rest, "YES" | "NO"),
        "X-BITRATE" => rest.parse::<u64>().is_ok_and(|value| value > 0),
        "OATCLS-SCTE35" => {
            !rest.is_empty()
                && rest
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
        }
        "X-ASSET" => has_only_attributes(rest, &["CAID", "UPID"]),
        "X-CUE" => has_only_attributes(rest, &["DURATION", "ID", "TYPE", "TIME", "ELAPSEDTIME"]),
        "X-CUE-OUT" => {
            is_positive_decimal(rest) || has_only_attributes(rest, &["DURATION"])
        }
        "X-CUE-OUT-CONT" => {
            has_only_attributes(rest, &["ELAPSEDTIME", "DURATION", "SCTE35"])
        }
        _ => false,
    }
}

fn has_only_attributes(value: &str, allowed: &[&str]) -> bool {
    let mut found_attribute = false;
    for attribute in value.split(',') {
        let Some((name, attribute_value)) = attribute.split_once('=') else {
            return false;
        };
        if attribute_value.trim().is_empty()
            || !allowed
                .iter()
                .any(|allowed_name| name.trim().eq_ignore_ascii_case(allowed_name))
        {
            return false;
        }
        found_attribute = true;
    }
    found_attribute
}

fn is_positive_decimal(value: &str) -> bool {
    value
        .parse::<f64>()
        .is_ok_and(|duration| duration.is_finite() && duration > 0.0)
}

#[cfg(test)]
mod tests {
    use crate::manifest::plan_hls_media_playlist;

    const BASE_URL: &str = "https://cdn.example.test/path/master.m3u8";

    #[test]
    fn documented_ad_marker_fixture_keeps_every_segment_and_drops_only_metadata() {
        let body = include_bytes!("../tests/fixtures/hls-ad-marker-metadata.m3u8");
        let plan = plan_hls_media_playlist(BASE_URL, body)
            .expect("documented ad-marker metadata should remain on the native path");

        let expected_urls = [
            "https://cdn.example.test/path/segment-8779957.ts?m=1566416212",
            "https://cdn.example.test/path/segment-8779958.ts?m=1566416212",
            "https://cdn.example.test/path/segment-8779959.ts?m=1566416212",
            "https://cdn.example.test/path/segment-8779960.ts?m=1566416212",
            "https://cdn.example.test/path/segment-8779961.ts?m=1566416212",
            "https://cdn.example.test/path/segment-8779962.ts?m=1566416212",
            "https://cdn.example.test/path/segment-8779963.ts?m=1566416212",
            "https://cdn.example.test/path/segment-8779964.ts?m=1566416212",
            "https://cdn.example.test/path/segment-8779965.ts?m=1566416212",
            "https://cdn.example.test/path/segment-8779966.ts?m=1566416212",
            "https://cdn.example.test/path/segment-8779967.ts?m=1566416212",
            "https://cdn.example.test/path/segment-8779968.ts?m=1566416212",
        ];
        let actual_urls = plan
            .resources
            .iter()
            .map(|resource| resource.url.as_str())
            .collect::<Vec<_>>();
        assert_eq!(actual_urls, expected_urls);

        let local = plan.local_playlist_bytes().expect("serialize local playlist");
        let local = String::from_utf8(local).expect("local playlist should be UTF-8");
        for tag in [
            "EXT-OATCLS-SCTE35",
            "EXT-X-ASSET",
            "EXT-X-CUE:",
            "EXT-X-CUE-OUT",
            "EXT-X-CUE-IN",
        ] {
            assert!(
                !local.contains(tag),
                "local FFmpeg playlist must omit metadata tag {tag}"
            );
        }

        let parsed = m3u8_rs::parse_media_playlist_res(local.as_bytes())
            .expect("serialized local playlist should parse");
        assert_eq!(parsed.segments.len(), expected_urls.len());
        assert!(
            parsed
                .segments
                .iter()
                .all(|segment| segment.unknown_tags.is_empty()),
            "the local playlist must not retain discarded marker metadata"
        );
    }

    #[test]
    fn resource_bearing_or_unrecognized_ad_marker_variants_still_fall_back() {
        for tag in [
            "X-ASSET:URI=\"https://cdn.example.test/other.m3u8\"",
            "X-CUE-OUT-CONT:ElapsedTime=1,Duration=10,URI=\"https://cdn.example.test/other.m3u8\"",
            "X-CUE-SPAN:DURATION=10",
        ] {
            let body = format!(
                "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-{tag}\n#EXTINF:2,\none.ts\n#EXT-X-ENDLIST\n"
            );
            let error = plan_hls_media_playlist(BASE_URL, body.as_bytes())
                .expect_err("resource-bearing and unrecognized tags must be rejected");
            assert!(
                matches!(
                    error,
                    crate::manifest::ManifestError::UnsupportedPlaylistFeature(_)
                ),
                "unexpected error for {tag}: {error}"
            );
        }
    }

    #[test]
    fn ad_marker_attribute_names_are_case_insensitive_but_not_open_ended() {
        let tag = m3u8_rs::ExtTag {
            tag: "X-CUE-OUT-CONT".to_string(),
            rest: Some("elapsedtime=0.5,DURATION=2,SCTE35=AAAA".to_string()),
        };
        assert!(super::is_benign_hls_tag(&tag));

        let tag = m3u8_rs::ExtTag {
            tag: "X-CUE-OUT-CONT".to_string(),
            rest: Some("ElapsedTime=0.5,Duration=2,URI=https://cdn.example.test/next".to_string()),
        };
        assert!(!super::is_benign_hls_tag(&tag));
    }
}
