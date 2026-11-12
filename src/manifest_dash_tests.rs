use super::*;

    const BASE_URL: &str = "https://cdn.example.test/path/manifest.mpd";
    #[derive(Clone)]
    struct SharedLogWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for SharedLogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let mut output = self
                .0
                .lock()
                .map_err(|_| std::io::Error::other("DASH test log buffer is poisoned"))?;
            output.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedLogWriter {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn capture_dash_warnings(
        parse: impl FnOnce() -> Result<DashManifestPlan, DashError>,
    ) -> (Result<DashManifestPlan, DashError>, String) {
        let output = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(SharedLogWriter(std::sync::Arc::clone(&output)))
            .finish();

        let result = tracing::subscriber::with_default(subscriber, parse);
        let captured = output.lock().expect("DASH diagnostic output lock").clone();
        (
            result,
            String::from_utf8(captured).expect("DASH diagnostic logs are UTF-8"),
        )
    }

    #[test]
    fn accepts_an_unused_xlink_namespace_declaration() {
        let body = include_str!("../tests/fixtures/dash/static-segment-template.mpd")
            .replace(
                r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011""#,
                r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" xmlns:xlink="http://www.w3.org/1999/xlink""#,
            );
        assert!(
            parse_dash_manifest(BASE_URL, &body).is_ok(),
            "an unused XLink namespace declaration is not an XLink reference"
        );
    }

    #[test]
    fn rejects_xlink_mpd_without_following_external_references() {
        let body = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" xmlns:xlink="http://www.w3.org/1999/xlink" type="static" mediaPresentationDuration="PT2S"><Period xlink:href="period.xml" xlink:actuate="onLoad"/></MPD>"#;
        assert_eq!(
            parse_dash_manifest(BASE_URL, body).err(),
            Some(DashError::UnsupportedPresentation)
        );
    }

    #[test]
    fn supported_static_and_dynamic_fixtures_have_no_ignored_element_warnings() {
        let (static_result, static_logs) = capture_dash_warnings(|| {
            parse_dash_manifest(
                BASE_URL,
                include_str!("../tests/fixtures/dash/static-segment-template.mpd"),
            )
        });
        assert!(static_result.is_ok(), "static fixture should parse");
        assert!(
            !static_logs.contains("Unused XML element"),
            "supported static fixture unexpectedly logged ignored elements: {static_logs}"
        );

        let (dynamic_result, dynamic_logs) = capture_dash_warnings(|| {
            parse_dash_manifest_at(
                BASE_URL,
                include_str!("../tests/fixtures/dash/dynamic-segment-template.mpd"),
                SystemTime::UNIX_EPOCH + Duration::from_secs(120),
            )
        });
        assert!(dynamic_result.is_ok(), "dynamic fixture should parse");
        assert!(
            !dynamic_logs.contains("Unused XML element"),
            "supported dynamic fixture unexpectedly logged ignored elements: {dynamic_logs}"
        );
    }

    #[test]
    fn warns_when_the_typed_dash_model_ignores_output_protection() {
        let (result, logs) = capture_dash_warnings(|| {
            parse_dash_manifest(
                BASE_URL,
                include_str!("../tests/fixtures/dash/unmodeled-output-protection.mpd"),
            )
        });

        assert!(
            result.is_ok(),
            "the typed parser should remain tolerant of unknown elements"
        );
        assert!(
            logs.contains("Unused XML element") && logs.contains("OutputProtection"),
            "expected an ignored OutputProtection diagnostic, got: {logs}"
        );

        // OutputProtection is an HDCP/output-device constraint, not DRM. Since
        // Grab downloads the encoded media rather than rendering it, the normal
        // native planner should still expand the same complete media resources.
        let manifest = result.expect("unmodeled output protection must not block downloads");
        let video = manifest
            .video_representations
            .first()
            .expect("video representation");
        let plan = plan_dash_representation(&manifest, &video.key)
            .expect("output protection must not block native resource planning");
        let segment_urls = plan
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .map(|resource| resource.url.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            segment_urls,
            [
                "https://cdn.example.test/path/seg-1.m4s",
                "https://cdn.example.test/path/seg-2.m4s"
            ]
        );
    }

    #[test]
    fn warns_when_the_typed_dash_model_ignores_vendor_extensions() {
        let (result, logs) = capture_dash_warnings(|| {
            parse_dash_manifest(
                BASE_URL,
                include_str!("../tests/fixtures/dash/vendor-extension.mpd"),
            )
        });

        assert!(
            result.is_ok(),
            "the typed parser should remain tolerant of vendor extensions"
        );
        assert!(
            logs.contains("Unused XML element") && logs.contains("SegmentPresentation"),
            "expected an ignored vendor extension diagnostic, got: {logs}"
        );
    }

    fn mpd_with_trick_mode_property(
        on_adaptation: bool,
        element: &str,
        scheme_id_uri: &str,
    ) -> String {
        let property = format!(r#"<{element} schemeIdUri="{scheme_id_uri}" value="0"/>"#);
        let adaptation_property = if on_adaptation { &property } else { "" };
        let representation_property = if on_adaptation { "" } else { &property };

        format!(
            r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10S">
  <Period duration="PT10S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      {adaptation_property}
      <SegmentTemplate timescale="1" duration="5" media="seg-$Number$.m4s"/>
      <Representation id="trick" bandwidth="1000000">{representation_property}</Representation>
      <Representation id="normal" bandwidth="2000000"/>
    </AdaptationSet>
  </Period>
</MPD>"#
        )
    }

    const TEMPLATE_MPD: &str = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10S">
  <BaseURL>media/</BaseURL>
  <Period duration="PT10S">
    <AdaptationSet id="1" contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1" duration="4" startNumber="5"
                       initialization="init-$RepresentationID$.mp4"
                       media="$RepresentationID$/$Number%03d$-$Time$.m4s"/>
      <Representation id="v1" bandwidth="1000000" width="1280" height="720" codecs="avc1.64001f"/>
      <Representation id="v2" bandwidth="2500000" width="1920" height="1080" codecs="avc1.640028"/>
    </AdaptationSet>
    <AdaptationSet id="2" contentType="audio" mimeType="audio/mp4" lang="en">
      <Role schemeIdUri="urn:mpeg:dash:role:2011" value="main"/>
      <SegmentTemplate timescale="1" duration="5" initialization="audio-init.mp4" media="audio/$Number$.m4s"/>
      <Representation id="a1" bandwidth="128000" codecs="mp4a.40.2"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

    #[test]
    fn parses_supported_mp4_subtitle_representations_only() {
        let body = TEMPLATE_MPD.replace(
            "</Period>",
            r#"
    <AdaptationSet contentType="text" mimeType="application/mp4" lang="en">
      <Role schemeIdUri="urn:mpeg:dash:role:2011" value="main"/>
      <SegmentTemplate timescale="1" duration="5" initialization="sub-init.mp4" media="sub-$Number$.m4s"/>
      <Representation id="sub-en" bandwidth="1000" codecs="wvtt"/>
    </AdaptationSet>
    <AdaptationSet contentType="text" mimeType="text/vtt" lang="fr">
      <SegmentTemplate duration="5" media="unsupported-$Number$.vtt"/>
      <Representation id="sub-fr" bandwidth="1000"/>
    </AdaptationSet>
</Period>"#,
        );
        let plan = parse_dash_manifest(BASE_URL, &body)
            .expect("supported DASH subtitle representation should parse");

        assert_eq!(plan.subtitle_representations.len(), 1);
        let subtitle = &plan.subtitle_representations[0];
        assert_eq!(subtitle.representation_id.as_deref(), Some("sub-en"));
        assert_eq!(subtitle.language.as_deref(), Some("en"));
        assert!(subtitle.is_default_subtitle);
        assert_eq!(subtitle.kind, DashTrackKind::Subtitle);
        let download = plan_dash_representation(&plan, &subtitle.key)
            .expect("subtitle representation should be plannable");
        assert_eq!(download.representation.key, subtitle.key);
        assert_eq!(download.resources.len(), 3);
        assert!(download.has_zero_origin_fragmented_subtitle_timeline());
    }

    #[test]
    fn classifies_stpp_as_a_supported_zero_origin_mp4_subtitle() {
        let body = TEMPLATE_MPD.replace(
            "</Period>",
            r#"
    <AdaptationSet contentType="text" mimeType="application/mp4" lang="en">
      <Role schemeIdUri="urn:mpeg:dash:role:2011" value="main"/>
      <SegmentTemplate timescale="1" duration="5" initialization="ttml-init.mp4" media="ttml-$Number$.m4s"/>
      <Representation id="sub-ttml" bandwidth="1000" codecs="stpp.ttml.im1t"/>
    </AdaptationSet>
</Period>"#,
        );
        let manifest =
            parse_dash_manifest(BASE_URL, &body).expect("STPP representation should be recognized");
        assert_eq!(manifest.subtitle_representations.len(), 1);
        let representation = &manifest.subtitle_representations[0];
        assert_eq!(
            representation.representation_id.as_deref(),
            Some("sub-ttml")
        );
        assert_eq!(representation.codecs.as_deref(), Some("stpp.ttml.im1t"));

        let plan = plan_dash_representation(&manifest, &representation.key)
            .expect("STPP representation should be plannable");
        assert!(plan.has_zero_origin_fragmented_subtitle_timeline());
    }

    #[test]
    fn wvtt_sample_timing_rejects_nonzero_period_start_and_presentation_offset() {
        let body = TEMPLATE_MPD.replace(
            "</Period>",
            r#"
    <AdaptationSet contentType="text" mimeType="application/mp4" lang="en">
      <SegmentTemplate timescale="1" duration="5" initialization="sub-init.mp4" media="sub-$Number$.m4s"/>
      <Representation id="sub-en" bandwidth="1000" codecs="wvtt"/>
    </AdaptationSet>
</Period>"#,
        );
        let offset_body = body.replace(
            r#"timescale="1" duration="5" initialization="sub-init.mp4""#,
            r#"timescale="1" duration="5" presentationTimeOffset="3" initialization="sub-init.mp4""#,
        );
        let offset_manifest = parse_dash_manifest(BASE_URL, &offset_body)
            .expect("a supported nonzero-PTO DASH manifest should parse");
        let offset_choice = &offset_manifest.subtitle_representations[0];
        let offset_plan = plan_dash_representation(&offset_manifest, &offset_choice.key)
            .expect("nonzero-PTO subtitle should remain plannable");
        assert!(!offset_plan.has_zero_origin_fragmented_subtitle_timeline());

        let period_body = body.replace(
            r#"<Period duration="PT10S">"#,
            r#"<Period start="PT2S" duration="PT8S">"#,
        );
        let period_manifest = parse_dash_manifest(BASE_URL, &period_body)
            .expect("a supported single Period with an explicit start should parse");
        let period_choice = &period_manifest.subtitle_representations[0];
        let period_plan = plan_dash_representation(&period_manifest, &period_choice.key)
            .expect("nonzero-start subtitle should remain plannable");
        assert!(!period_plan.has_zero_origin_fragmented_subtitle_timeline());
    }

    #[test]
    fn representation_level_main_role_selects_the_declared_audio_track() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10S">
  <Period duration="PT10S">
    <AdaptationSet contentType="audio" mimeType="audio/mp4">
      <SegmentTemplate duration="5" media="first-$Number$.m4s"/>
      <Representation id="first" bandwidth="128000"/>
    </AdaptationSet>
    <AdaptationSet contentType="audio" mimeType="audio/mp4">
      <SegmentTemplate duration="5" media="main-$Number$.m4s"/>
      <Representation id="declared-main" bandwidth="192000">
        <Role schemeIdUri="urn:mpeg:dash:role:2011" value="main"/>
      </Representation>
    </AdaptationSet>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate duration="5" media="video-$Number$.m4s"/>
      <Representation id="video" bandwidth="1000000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let plan = parse_dash_manifest(BASE_URL, body)
            .expect("a representation-level main role should be preserved");

        assert_eq!(plan.audio_representations.len(), 2);
        assert!(!plan.audio_representations[0].is_default_audio);
        assert!(plan.audio_representations[1].is_default_audio);
        assert_eq!(
            plan.default_audio()
                .and_then(|track| track.representation_id.as_deref()),
            Some("declared-main"),
            "Representation-level Role=main must override the first-track fallback"
        );
    }

    #[test]
    fn parses_video_and_audio_representations_and_selects_main_audio() {
        let plan = parse_dash_manifest(BASE_URL, TEMPLATE_MPD)
            .expect("static single-period MPD should parse");

        assert_eq!(plan.duration, Duration::from_secs(10));
        assert_eq!(plan.video_representations.len(), 2);
        assert_eq!(
            plan.video_representations[0].representation_id.as_deref(),
            Some("v1")
        );
        assert_eq!(plan.video_representations[0].height, Some(720));
        assert_eq!(plan.video_representations[1].height, Some(1080));
        assert_eq!(plan.audio_representations.len(), 1);
        assert_eq!(
            plan.default_audio()
                .and_then(|audio| audio.representation_id.as_deref()),
            Some("a1")
        );
    }

    #[test]
    fn representation_key_survives_manifest_order_changes() {
        let original =
            parse_dash_manifest(BASE_URL, TEMPLATE_MPD).expect("original MPD should parse");
        let original_v1 = original
            .video_representations
            .iter()
            .find(|representation| representation.representation_id.as_deref() == Some("v1"))
            .expect("v1 representation");
        let selected_key = original_v1.key.clone();

        let reordered_body = TEMPLATE_MPD.replace(
            concat!(
                "      <Representation id=\"v1\" bandwidth=\"1000000\" width=\"1280\" height=\"720\" codecs=\"avc1.64001f\"/>\n",
                "      <Representation id=\"v2\" bandwidth=\"2500000\" width=\"1920\" height=\"1080\" codecs=\"avc1.640028\"/>"
            ),
            concat!(
                "      <Representation id=\"v2\" bandwidth=\"2500000\" width=\"1920\" height=\"1080\" codecs=\"avc1.640028\"/>\n",
                "      <Representation id=\"v1\" bandwidth=\"1000000\" width=\"1280\" height=\"720\" codecs=\"avc1.64001f\"/>"
            ),
        );
        assert_ne!(
            reordered_body, TEMPLATE_MPD,
            "fixture must reorder the variants"
        );

        let refreshed =
            parse_dash_manifest(BASE_URL, &reordered_body).expect("reordered MPD should parse");
        let refreshed_v1 = refreshed
            .video_representations
            .iter()
            .find(|representation| representation.representation_id.as_deref() == Some("v1"))
            .expect("v1 representation after refresh");
        assert_eq!(refreshed_v1.key, selected_key);
        assert!(
            plan_dash_representation(&refreshed, &selected_key).is_ok(),
            "the queued key must still resolve to v1 after a manifest reorder"
        );
    }

    #[test]
    fn excludes_adaptation_sets_marked_for_trick_mode() {
        for (element, scheme_id_uri) in [
            (
                "EssentialProperty",
                "https://dashif.org/guidelines/trickmode",
            ),
            (
                "EssentialProperty",
                "http://dashif.org/guidelines/trickmode",
            ),
            (
                "SupplementalProperty",
                "http://dashif.org/guidelines/trickmode",
            ),
        ] {
            let body = mpd_with_trick_mode_property(true, element, scheme_id_uri);
            assert_eq!(
                parse_dash_manifest(BASE_URL, &body).err(),
                Some(DashError::NoVideoRepresentations),
                "{element} with {scheme_id_uri} must exclude the entire AdaptationSet"
            );
        }
    }

    #[test]
    fn excludes_representations_marked_for_trick_mode() {
        for (element, scheme_id_uri) in [
            ("EssentialProperty", "urn:mpeg:dash:trickmode:2016"),
            (
                "SupplementalProperty",
                "https://dashif.org/guidelines/trickmode",
            ),
        ] {
            let body = mpd_with_trick_mode_property(false, element, scheme_id_uri);
            let plan = parse_dash_manifest(BASE_URL, &body)
                .expect("regular video representation should remain selectable");

            assert_eq!(plan.video_representations.len(), 1);
            assert_eq!(
                plan.video_representations[0].representation_id.as_deref(),
                Some("normal"),
                "{element} with {scheme_id_uri} must exclude only its Representation"
            );
        }
    }

    fn dynamic_mpd(addressing: &str, extra_attributes: &str) -> String {
        format!(
            r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     type="dynamic"
     availabilityStartTime="1970-01-01T00:00:00Z"
     minimumUpdatePeriod="PT4S"
     {extra_attributes}>
  <Period start="PT0S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      {addressing}
      <Representation id="v1" bandwidth="1000000" width="1280" height="720"/>
    </AdaptationSet>
  </Period>
</MPD>"#
        )
    }

    fn dynamic_test_clock(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn dynamic_duration_template_selects_only_complete_segments_inside_time_shift_window() {
        let body = dynamic_mpd(
            r#"<SegmentTemplate timescale="1" startNumber="1" duration="4" initialization="init.mp4" media="seg-$Number$.m4s"/>"#,
            r#"timeShiftBufferDepth="PT20S""#,
        );
        let manifest = parse_dash_manifest_at(BASE_URL, &body, dynamic_test_clock(120))
            .expect("bounded dynamic MPD should parse");
        let video = manifest.video_representations.first().expect("video track");
        assert!(matches!(
            manifest.presentation,
            DashPresentation::Dynamic(_)
        ));

        let plan = plan_dash_representation(&manifest, &video.key)
            .expect("expand current complete live segments");
        let numbers = plan
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .map(|resource| resource.segment_number.expect("media segment number"))
            .collect::<Vec<_>>();
        assert_eq!(numbers, [26, 27, 28, 29, 30]);
        assert_eq!(plan.resources[0].kind, DashResourceKind::Initialization);
        assert!(
            plan.resources[1..]
                .iter()
                .all(|resource| resource.filename.starts_with("dash-live-segment-"))
        );
    }

    #[test]
    fn dynamic_without_time_shift_depth_starts_at_the_newest_complete_segment() {
        let body = dynamic_mpd(
            r#"<SegmentTemplate timescale="1" startNumber="10" duration="4" media="seg-$Number$.m4s"/>"#,
            "",
        );
        let manifest = parse_dash_manifest_at(BASE_URL, &body, dynamic_test_clock(120))
            .expect("dynamic MPD without TSBD should parse");
        let representation = &manifest.video_representations[0];
        let plan = plan_dash_representation(&manifest, &representation.live_key)
            .expect("plan newest complete live segment");
        let numbers = plan
            .resources
            .iter()
            .filter_map(|resource| resource.segment_number)
            .collect::<Vec<_>>();
        assert_eq!(numbers, [38, 39]);
    }

    #[test]
    fn dynamic_timeline_negative_repeat_uses_only_complete_segments_in_window() {
        let body = dynamic_mpd(
            r#"<SegmentTemplate timescale="1" startNumber="1" media="seg-$Number$-$Time$.m4s">
  <SegmentTimeline><S t="0" d="2" r="-1"/></SegmentTimeline>
</SegmentTemplate>"#,
            r#"timeShiftBufferDepth="PT4S""#,
        );
        let manifest = parse_dash_manifest_at(BASE_URL, &body, dynamic_test_clock(11))
            .expect("dynamic timeline MPD should parse");
        let representation = &manifest.video_representations[0];
        let plan = plan_dash_representation(&manifest, &representation.key)
            .expect("expand complete timeline window");
        let segments = plan
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .collect::<Vec<_>>();
        assert_eq!(
            segments
                .iter()
                .map(|resource| resource.segment_number.expect("segment number"))
                .collect::<Vec<_>>(),
            [4, 5]
        );
        assert_eq!(
            segments
                .iter()
                .map(|resource| resource.url.path().to_string())
                .collect::<Vec<_>>(),
            ["/path/seg-4-6.m4s", "/path/seg-5-8.m4s"]
        );
    }

    #[test]
    fn dynamic_timeline_negative_repeat_uses_ceiling_before_next_explicit_start() {
        let body = dynamic_mpd(
            r#"<SegmentTemplate timescale="1" startNumber="10" media="seg-$Number$-$Time$.m4s">
  <SegmentTimeline>
    <S t="0" d="2" r="-1"/>
    <S t="5" d="2"/>
  </SegmentTimeline>
</SegmentTemplate>"#,
            r#"timeShiftBufferDepth="PT20S""#,
        );
        let manifest = parse_dash_manifest_at(BASE_URL, &body, dynamic_test_clock(9))
            .expect("dynamic timeline MPD should parse");
        let representation = &manifest.video_representations[0];
        let plan = plan_dash_representation(&manifest, &representation.live_key)
            .expect("expand complete dynamic timeline segments");
        let segments = plan
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .collect::<Vec<_>>();

        assert_eq!(
            segments
                .iter()
                .map(|resource| resource.segment_number.expect("segment number"))
                .collect::<Vec<_>>(),
            [10, 11, 12, 13],
            "the negative repeat includes the final segment that overlaps the next S@t"
        );
        assert_eq!(
            segments
                .iter()
                .map(|resource| resource.url.path().to_string())
                .collect::<Vec<_>>(),
            [
                "/path/seg-10-0.m4s",
                "/path/seg-11-2.m4s",
                "/path/seg-12-4.m4s",
                "/path/seg-13-5.m4s",
            ]
        );
    }

    #[test]
    fn dynamic_timeline_stops_at_inclusive_end_number() {
        let body = dynamic_mpd(
            r#"<SegmentTemplate timescale="1" startNumber="1" endNumber="2" media="seg-$Number$-$Time$.m4s">
  <SegmentTimeline>
    <S t="0" d="2"/>
    <S t="2" d="2"/>
    <S t="4" d="2"/>
  </SegmentTimeline>
</SegmentTemplate>"#,
            r#"timeShiftBufferDepth="PT20S""#,
        );
        let manifest = parse_dash_manifest_at(BASE_URL, &body, dynamic_test_clock(11))
            .expect("dynamic MPD with endNumber should parse");
        let representation = &manifest.video_representations[0];
        let plan = plan_dash_representation(&manifest, &representation.live_key)
            .expect("expand segments through endNumber");
        let numbers = plan
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .map(|resource| resource.segment_number.expect("media segment number"))
            .collect::<Vec<_>>();
        assert_eq!(numbers, [1, 2]);
    }

    #[test]
    fn dynamic_timeline_rejects_overlap_after_positive_repeat() {
        let body = dynamic_mpd(
            r#"<SegmentTemplate timescale="1" startNumber="1" media="seg-$Time$.m4s">
  <SegmentTimeline>
    <S t="0" d="4" r="2"/>
    <S t="4" d="4"/>
  </SegmentTimeline>
</SegmentTemplate>"#,
            r#"timeShiftBufferDepth="PT20S""#,
        );
        let manifest = parse_dash_manifest_at(BASE_URL, &body, dynamic_test_clock(20))
            .expect("dynamic timeline MPD syntax should parse");
        let representation = &manifest.video_representations[0];

        assert_eq!(
            plan_dash_representation(&manifest, &representation.live_key).err(),
            Some(DashError::InvalidSegment),
            "an explicit start inside a positive-repeat run must be rejected"
        );
    }

    #[test]
    fn accepts_dynamic_single_period_with_nonzero_period_start() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic"
     availabilityStartTime="1970-01-01T00:00:00Z" minimumUpdatePeriod="PT4S">
  <Period start="PT3600S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate duration="4" media="video-$Number$.m4s"/>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let manifest = parse_dash_manifest_at(BASE_URL, body, dynamic_test_clock(3604))
            .expect("single-period dynamic MPD may have a nonzero period start");
        let DashPresentation::Dynamic(dynamic) = &manifest.presentation else {
            panic!("fixture is dynamic");
        };
        assert_eq!(dynamic.period_start, Duration::from_secs(3600));

        let representation = &manifest.video_representations[0];
        let plan = plan_dash_representation(&manifest, &representation.key)
            .expect("existing dynamic Period-start planning remains supported");
        assert_eq!(
            plan.input_timestamp_offset.ffmpeg_argument(),
            None,
            "the static PTO change must not change dynamic finalization semantics"
        );
    }

    #[test]
    fn plans_period_continuous_static_dash_as_one_resource_sequence() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT4S">
  <Period id="p1" start="PT0S" duration="PT2S">
    <AssetIdentifier schemeIdUri="urn:example:asset" value="asset-1"/>
    <AdaptationSet id="video" contentType="video" mimeType="video/mp4" codecs="avc1.640028">
      <SegmentTemplate timescale="1" startNumber="1" duration="1"
                       initialization="init.mp4" media="video-$Number$.m4s"/>
      <Representation id="v1" bandwidth="1000000" width="1280" height="720"/>
    </AdaptationSet>
  </Period>
  <Period id="p2" start="PT2S" duration="PT2S">
    <AssetIdentifier schemeIdUri="urn:example:asset" value="asset-1"/>
    <AdaptationSet id="video" contentType="video" mimeType="video/mp4" codecs="avc1.640028">
      <SupplementalProperty schemeIdUri="urn:mpeg:dash:period-continuity:2015" value="p1"/>
      <SegmentTemplate timescale="1" startNumber="3" duration="1" presentationTimeOffset="2"
                       initialization="init.mp4" media="video-$Number$.m4s"/>
      <Representation id="v1" bandwidth="1000000" width="1280" height="720"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

        let manifest = parse_dash_manifest(BASE_URL, body)
            .expect("explicitly continuous static periods should parse");
        assert_eq!(manifest.duration, Duration::from_secs(4));
        assert_eq!(manifest.video_representations.len(), 1);

        let plan = plan_dash_representation(&manifest, &manifest.video_representations[0].key)
            .expect("period resources should be combined in presentation order");
        assert_eq!(plan.duration, Duration::from_secs(4));
        assert_eq!(
            plan.resources
                .iter()
                .filter(|resource| resource.kind == DashResourceKind::Initialization)
                .count(),
            1,
            "continuous periods share the first initialization section"
        );
        let segment_numbers = plan
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .map(|resource| resource.segment_number)
            .collect::<Vec<_>>();
        assert_eq!(segment_numbers, [Some(1), Some(2), Some(3), Some(4)]);

        let repeated_url_body = body.replace("video-$Number$.m4s", "video.m4s");
        let repeated_url_manifest = parse_dash_manifest(BASE_URL, &repeated_url_body)
            .expect("repeated media URLs are valid with continuous period timing");
        let repeated_url_plan = plan_dash_representation(
            &repeated_url_manifest,
            &repeated_url_manifest.video_representations[0].key,
        )
        .expect("period staging names should remain unique");
        let mut filenames = std::collections::HashSet::new();
        assert!(
            repeated_url_plan
                .resources
                .iter()
                .all(|resource| filenames.insert(resource.filename.as_str()))
        );

        let different_asset = body.replacen(r#"value="asset-1""#, r#"value="asset-2""#, 1);
        assert_eq!(
            parse_dash_manifest(BASE_URL, &different_asset).err(),
            Some(DashError::UnsupportedPresentation)
        );

        let discontinuous_timeline = body.replace(
            r#"presentationTimeOffset="2""#,
            r#"presentationTimeOffset="1""#,
        );
        assert_eq!(
            parse_dash_manifest(BASE_URL, &discontinuous_timeline).err(),
            Some(DashError::UnsupportedPresentation)
        );

        let changed_initialization = body
            .rsplit_once(r#"initialization="init.mp4" "#)
            .map(|(prefix, suffix)| format!("{prefix}{suffix}"))
            .expect("second period initialization exists");
        assert_eq!(
            parse_dash_manifest(BASE_URL, &changed_initialization).err(),
            Some(DashError::UnsupportedPresentation)
        );

        let changed_initialization_url = body
            .rsplit_once(r#"initialization="init.mp4""#)
            .map(|(prefix, suffix)| format!(r#"{prefix}initialization="init2.mp4"{suffix}"#))
            .expect("second period initialization exists");
        let changed_init_manifest = parse_dash_manifest(BASE_URL, &changed_initialization_url)
            .expect("period init addressing parses before continuity is planned");
        assert_eq!(
            plan_dash_representation(
                &changed_init_manifest,
                &changed_init_manifest.video_representations[0].key,
            )
            .err(),
            Some(DashError::UnsupportedPresentation)
        );

        let crossing_segment = body.replace(r#"duration="1""#, r#"duration="3""#);
        let crossing_manifest = parse_dash_manifest(BASE_URL, &crossing_segment)
            .expect("period metadata is valid before segment-boundary planning");
        assert_eq!(
            plan_dash_representation(
                &crossing_manifest,
                &crossing_manifest.video_representations[0].key,
            )
            .err(),
            Some(DashError::UnsupportedPresentation)
        );
    }

    #[test]
    fn plans_three_continuous_periods_using_each_adjacent_timeline() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT6S">
  <Period id="p1" start="PT0S" duration="PT2S">
    <AssetIdentifier schemeIdUri="urn:example:asset" value="asset-1"/>
    <AdaptationSet id="video" contentType="video" mimeType="video/mp4" codecs="avc1.640028">
      <SegmentTemplate timescale="1" startNumber="1" duration="1"
                       initialization="init.mp4" media="video-$Number$.m4s"/>
      <Representation id="v1" width="1280" height="720"/>
    </AdaptationSet>
  </Period>
  <Period id="p2" start="PT2S" duration="PT2S">
    <AssetIdentifier schemeIdUri="urn:example:asset" value="asset-1"/>
    <AdaptationSet id="video" contentType="video" mimeType="video/mp4" codecs="avc1.640028">
      <SupplementalProperty schemeIdUri="urn:mpeg:dash:period-continuity:2015" value="p1"/>
      <SegmentTemplate timescale="1" startNumber="3" duration="1" presentationTimeOffset="2"
                       initialization="init.mp4" media="video-$Number$.m4s"/>
      <Representation id="v1" width="1280" height="720"/>
    </AdaptationSet>
  </Period>
  <Period id="p3" start="PT4S" duration="PT2S">
    <AssetIdentifier schemeIdUri="urn:example:asset" value="asset-1"/>
    <AdaptationSet id="video" contentType="video" mimeType="video/mp4" codecs="avc1.640028">
      <SupplementalProperty schemeIdUri="urn:mpeg:dash:period-continuity:2015" value="p2"/>
      <SegmentTemplate timescale="1" startNumber="5" duration="1" presentationTimeOffset="4"
                       initialization="init.mp4" media="video-$Number$.m4s"/>
      <Representation id="v1" width="1280" height="720"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

        let manifest = parse_dash_manifest(BASE_URL, body)
            .expect("three adjacent period-continuous periods should parse");
        let plan = plan_dash_representation(&manifest, &manifest.video_representations[0].key)
            .expect("all three period resource sequences should be appended");
        let segment_numbers = plan
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .map(|resource| resource.segment_number)
            .collect::<Vec<_>>();
        assert_eq!(
            segment_numbers,
            [Some(1), Some(2), Some(3), Some(4), Some(5), Some(6)]
        );
        assert_eq!(
            plan.resources
                .iter()
                .filter(|resource| resource.kind == DashResourceKind::Initialization)
                .count(),
            1
        );
    }

    #[test]
    fn rejects_multi_period_dash_without_continuity_or_with_timing_gaps() {
        let missing_continuity = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT4S">
  <Period id="p1" start="PT0S" duration="PT2S">
    <AssetIdentifier schemeIdUri="urn:example:asset" value="asset-1"/>
    <AdaptationSet id="video" contentType="video" mimeType="video/mp4">
      <SegmentTemplate duration="1" media="video-$Number$.m4s"/>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
  <Period id="p2" start="PT2S" duration="PT2S">
    <AssetIdentifier schemeIdUri="urn:example:asset" value="asset-1"/>
    <AdaptationSet id="video" contentType="video" mimeType="video/mp4">
      <SegmentTemplate duration="1" startNumber="3" media="video-$Number$.m4s"/>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        assert_eq!(
            parse_dash_manifest(BASE_URL, missing_continuity).err(),
            Some(DashError::UnsupportedPresentation)
        );

        let gap = missing_continuity
            .replace(
                r#"<Period id="p2" start="PT2S" duration="PT2S">"#,
                r#"<Period id="p2" start="PT3S" duration="PT1S">"#,
            )
            .replace(
                r#"<AssetIdentifier schemeIdUri="urn:example:asset" value="asset-1"/>
    <AdaptationSet id="video" contentType="video" mimeType="video/mp4">
      <SegmentTemplate duration="1" startNumber="3""#,
                r#"<AssetIdentifier schemeIdUri="urn:example:asset" value="asset-1"/>
    <AdaptationSet id="video" contentType="video" mimeType="video/mp4">
      <SupplementalProperty schemeIdUri="urn:mpeg:dash:period-continuity:2015" value="p1"/>
      <SegmentTemplate duration="1" startNumber="3""#,
            );
        assert_eq!(
            parse_dash_manifest(BASE_URL, &gap).err(),
            Some(DashError::UnsupportedPresentation)
        );

        let nonzero_start = missing_continuity.replace(r#"start="PT0S""#, r#"start="PT1S""#);
        assert_eq!(
            parse_dash_manifest(BASE_URL, &nonzero_start).err(),
            Some(DashError::UnsupportedPresentation)
        );
    }

    #[test]
    fn dynamic_representation_key_survives_timeline_growth_and_static_finalization() {
        let dynamic = dynamic_mpd(
            r#"<SegmentTemplate timescale="1" startNumber="1" duration="4" media="seg-$Number$.m4s"/>"#,
            r#"timeShiftBufferDepth="PT20S""#,
        );
        let static_mpd = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT120S">
  <Period duration="PT120S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1" startNumber="1" duration="4" media="seg-$Number$.m4s"/>
      <Representation id="v1" bandwidth="1000000" width="1280" height="720"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let live = parse_dash_manifest_at(BASE_URL, &dynamic, dynamic_test_clock(120))
            .expect("parse dynamic representation");
        let finalized = parse_dash_manifest(BASE_URL, static_mpd)
            .expect("parse equivalent static presentation");
        let live_representation = &live.video_representations[0];
        let static_representation = &finalized.video_representations[0];
        assert_eq!(live_representation.live_key, static_representation.live_key);
        assert_ne!(live_representation.key, static_representation.key);
        let live_plan = plan_dash_representation(&live, &live_representation.live_key)
            .expect("plan current dynamic window");
        let finalized_plan = plan_dash_representation(&finalized, &live_representation.live_key)
            .expect("plan final static presentation with the queued live key");
        let dynamic_resource = live_plan
            .resources
            .iter()
            .find(|resource| resource.segment_number == Some(26))
            .expect("dynamic window starts with segment 26");
        let final_resource = finalized_plan
            .resources
            .iter()
            .find(|resource| resource.segment_number == Some(26))
            .expect("final MPD includes captured segment 26");
        assert_eq!(
            dynamic_resource.filename, final_resource.filename,
            "dynamic and static finalization must retain the same staged resource identity"
        );

        let later = dynamic_mpd(
            r#"<SegmentTemplate timescale="1" startNumber="1" duration="4" media="seg-$Number$.m4s">
  <SegmentTimeline><S t="0" d="4" r="-1"/></SegmentTimeline>
</SegmentTemplate>"#,
            r#"timeShiftBufferDepth="PT20S""#,
        );
        let refreshed = parse_dash_manifest_at(BASE_URL, &later, dynamic_test_clock(124))
            .expect("parse refreshed dynamic timeline");
        assert_eq!(
            refreshed.video_representations[0].live_key,
            live_representation.live_key
        );
    }

    #[test]
    fn dynamic_segment_list_uses_explicit_timeline_times_as_stable_sequence_ids() {
        let mut urls = String::new();
        let mut timeline = String::new();
        for index in 0..60_u64 {
            let time = index * 2;
            urls.push_str(&format!(r#"<SegmentURL media="seg-{time}.m4s"/>"#));
            timeline.push_str(&format!(r#"<S t="{time}" d="2"/>"#));
        }
        let body = format!(
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic" availabilityStartTime="1970-01-01T00:00:00Z" minimumUpdatePeriod="PT2S" timeShiftBufferDepth="PT10S"><Period><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentList timescale="1"><Initialization sourceURL="init.mp4"/><SegmentTimeline>{timeline}</SegmentTimeline>{urls}</SegmentList><Representation id="v1" bandwidth="1000000"/></AdaptationSet></Period></MPD>"#
        );
        let manifest = parse_dash_manifest_at(
            BASE_URL,
            &body,
            SystemTime::UNIX_EPOCH + Duration::from_secs(120),
        )
        .expect("dynamic SegmentList with explicit timeline should parse");
        let representation = &manifest.video_representations[0];
        let plan = plan_dash_representation(&manifest, &representation.live_key)
            .expect("dynamic SegmentList should expand");
        let numbers = plan
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .filter_map(|resource| resource.segment_number)
            .collect::<Vec<_>>();
        assert_eq!(numbers, vec![56, 57, 58, 59, 60]);
    }

    #[test]
    fn dynamic_segment_template_retains_cumulative_base_url_availability_offsets() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     type="dynamic"
     availabilityStartTime="1970-01-01T00:00:00Z"
     minimumUpdatePeriod="PT2S"
     timeShiftBufferDepth="PT10S">
  <BaseURL availabilityTimeOffset="1">https://cdn.example.test/</BaseURL>
  <Period>
    <BaseURL availabilityTimeOffset="2">live/</BaseURL>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <BaseURL availabilityTimeOffset="3">video/</BaseURL>
      <SegmentTemplate timescale="1" duration="2"
                       media="seg-$Number$.m4s"
                       initialization="init.mp4"/>
      <Representation id="v1" bandwidth="1000000">
        <BaseURL availabilityTimeOffset="4">high/</BaseURL>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let manifest = parse_dash_manifest_at(BASE_URL, body, dynamic_test_clock(120))
            .expect("finite BaseURL offsets should remain supported for SegmentTemplate");
        let representation = &manifest.video_representations[0];
        let plan = plan_dash_representation(&manifest, &representation.live_key)
            .expect("dynamic SegmentTemplate should expand");
        let numbers = plan
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .filter_map(|resource| resource.segment_number)
            .collect::<Vec<_>>();
        assert_eq!(numbers.last(), Some(&65));
        assert!(numbers.contains(&61));
    }

    #[test]
    fn dynamic_segment_list_applies_inherited_segment_list_availability_offset() {
        let mut urls = String::new();
        let mut timeline = String::new();
        for index in 0..70_u64 {
            let time = index * 2;
            urls.push_str(&format!(r#"<SegmentURL media="seg-{time}.m4s"/>"#));
            timeline.push_str(&format!(r#"<S t="{time}" d="2"/>"#));
        }
        let body = format!(
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
                type="dynamic"
                availabilityStartTime="1970-01-01T00:00:00Z"
                minimumUpdatePeriod="PT2S"
                timeShiftBufferDepth="PT10S">
              <Period>
                <AdaptationSet contentType="video" mimeType="video/mp4">
                  <SegmentList timescale="1" availabilityTimeOffset="4">
                    <Initialization sourceURL="init.mp4"/>
                    <SegmentTimeline>{timeline}</SegmentTimeline>
                    {urls}
                  </SegmentList>
                  <Representation id="v1" bandwidth="1000000"/>
                </AdaptationSet>
              </Period>
            </MPD>"#
        );
        let manifest = parse_dash_manifest_at(BASE_URL, &body, dynamic_test_clock(120))
            .expect("finite SegmentList availability offset should parse");
        let representation = &manifest.video_representations[0];
        let plan = plan_dash_representation(&manifest, &representation.live_key)
            .expect("dynamic SegmentList should expand");
        let numbers = plan
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .filter_map(|resource| resource.segment_number)
            .collect::<Vec<_>>();
        assert_eq!(numbers.first(), Some(&56));
        assert_eq!(numbers.last(), Some(&62));
        assert_eq!(numbers.len(), 7);
    }

    #[test]
    fn dynamic_segment_list_rejects_negative_segment_list_availability_offset() {
        let body = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
            type="dynamic"
            availabilityStartTime="1970-01-01T00:00:00Z"
            minimumUpdatePeriod="PT2S"
            timeShiftBufferDepth="PT10S">
          <Period>
            <AdaptationSet contentType="video" mimeType="video/mp4">
              <SegmentList timescale="1" availabilityTimeOffset="-1">
                <Initialization sourceURL="init.mp4"/>
                <SegmentTimeline><S t="0" d="2"/></SegmentTimeline>
                <SegmentURL media="seg-0.m4s"/>
              </SegmentList>
              <Representation id="v1" bandwidth="1000000"/>
            </AdaptationSet>
          </Period>
        </MPD>"#;
        assert_eq!(
            parse_dash_manifest_at(BASE_URL, body, dynamic_test_clock(120)).err(),
            Some(DashError::UnsupportedAddressing)
        );
    }

    #[test]
    fn static_segment_list_rejects_availability_time_complete() {
        let body = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
            type="static" mediaPresentationDuration="PT4S">
          <Period duration="PT4S">
            <AdaptationSet contentType="video" mimeType="video/mp4">
              <SegmentList availabilityTimeComplete="false">
                <SegmentURL media="seg.m4s"/>
              </SegmentList>
              <Representation id="v1"/>
            </AdaptationSet>
          </Period>
        </MPD>"#;
        assert_eq!(
            parse_dash_manifest(BASE_URL, body).err(),
            Some(DashError::UnsupportedAddressing)
        );
    }

    #[test]
    fn static_segment_list_rejects_nonzero_segment_list_availability_offset() {
        let body = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
            type="static" mediaPresentationDuration="PT4S">
          <Period duration="PT4S">
            <AdaptationSet contentType="video" mimeType="video/mp4">
              <SegmentList availabilityTimeOffset="1">
                <SegmentURL media="seg.m4s"/>
              </SegmentList>
              <Representation id="v1"/>
            </AdaptationSet>
          </Period>
        </MPD>"#;
        assert_eq!(
            parse_dash_manifest(BASE_URL, body).err(),
            Some(DashError::UnsupportedAddressing)
        );
    }

    #[test]
    fn dynamic_segment_list_applies_cumulative_base_url_availability_offsets() {
        let mut urls = String::new();
        let mut timeline = String::new();
        for index in 0..70_u64 {
            let time = index * 2;
            urls.push_str(&format!(r#"<SegmentURL media="seg-{time}.m4s"/>"#));
            timeline.push_str(&format!(r#"<S t="{time}" d="2"/>"#));
        }
        let body = format!(
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
                type="dynamic"
                availabilityStartTime="1970-01-01T00:00:00Z"
                minimumUpdatePeriod="PT2S"
                timeShiftBufferDepth="PT10S">
              <BaseURL availabilityTimeOffset="1">https://cdn.example.test/</BaseURL>
              <Period>
                <BaseURL availabilityTimeOffset="2">live/</BaseURL>
                <AdaptationSet contentType="video" mimeType="video/mp4">
                  <BaseURL availabilityTimeOffset="3">video/</BaseURL>
                  <SegmentList timescale="1">
                    <Initialization sourceURL="init.mp4"/>
                    <SegmentTimeline>{timeline}</SegmentTimeline>
                    {urls}
                  </SegmentList>
                  <Representation id="v1" bandwidth="1000000">
                    <BaseURL availabilityTimeOffset="4">high/</BaseURL>
                  </Representation>
                </AdaptationSet>
              </Period>
            </MPD>"#
        );
        let manifest = parse_dash_manifest_at(BASE_URL, &body, dynamic_test_clock(120))
            .expect("finite BaseURL offsets should be supported for dynamic SegmentList");
        let representation = &manifest.video_representations[0];
        let plan = plan_dash_representation(&manifest, &representation.live_key)
            .expect("dynamic SegmentList should expand");
        let numbers = plan
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .filter_map(|resource| resource.segment_number)
            .collect::<Vec<_>>();
        assert_eq!(numbers.first(), Some(&56));
        assert_eq!(numbers.last(), Some(&65));
        assert_eq!(numbers.len(), 10);
        assert!(plan.resources[1..].iter().all(|resource| {
            resource
                .url
                .as_str()
                .starts_with("https://cdn.example.test/live/video/high/")
        }));
    }

    #[test]
    fn dynamic_segment_list_rejects_negative_base_url_availability_offsets() {
        let body = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
            type="dynamic"
            availabilityStartTime="1970-01-01T00:00:00Z"
            minimumUpdatePeriod="PT2S"
            timeShiftBufferDepth="PT10S">
          <BaseURL availabilityTimeOffset="-1">https://cdn.example.test/</BaseURL>
          <Period>
            <AdaptationSet contentType="video" mimeType="video/mp4">
              <SegmentList timescale="1">
                <Initialization sourceURL="init.mp4"/>
                <SegmentTimeline><S t="0" d="2"/></SegmentTimeline>
                <SegmentURL media="seg-0.m4s"/>
              </SegmentList>
              <Representation id="v1" bandwidth="1000000"/>
            </AdaptationSet>
          </Period>
        </MPD>"#;
        assert_eq!(
            parse_dash_manifest_at(BASE_URL, body, dynamic_test_clock(120)).err(),
            Some(DashError::UnsupportedAddressing)
        );
    }

    #[test]
    fn static_segment_list_rejects_base_url_availability_offsets() {
        let body = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
            type="static" mediaPresentationDuration="PT4S">
          <BaseURL availabilityTimeOffset="1">https://cdn.example.test/</BaseURL>
          <Period duration="PT4S">
            <AdaptationSet contentType="video" mimeType="video/mp4">
              <SegmentList>
                <SegmentURL media="seg.m4s"/>
              </SegmentList>
              <Representation id="v1"/>
            </AdaptationSet>
          </Period>
        </MPD>"#;
        assert_eq!(
            parse_dash_manifest(BASE_URL, body).err(),
            Some(DashError::UnsupportedAddressing)
        );
    }

    #[test]
    fn dynamic_segment_list_rejects_nondefault_start_number() {
        let body = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic" availabilityStartTime="1970-01-01T00:00:00Z" minimumUpdatePeriod="PT2S" timeShiftBufferDepth="PT10S"><Period><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentList startNumber="2" timescale="1"><Initialization sourceURL="init.mp4"/><SegmentTimeline><S t="0" d="2"/></SegmentTimeline><SegmentURL media="seg-0.m4s"/></SegmentList><Representation id="v1" bandwidth="1000000"/></AdaptationSet></Period></MPD>"#;
        assert_eq!(
            parse_dash_manifest_at(
                BASE_URL,
                body,
                SystemTime::UNIX_EPOCH + Duration::from_secs(120)
            )
            .err(),
            Some(DashError::UnsupportedAddressing)
        );
    }

    #[test]
    fn dynamic_segment_list_requires_an_explicit_initialization_url() {
        for initialization in [
            "",
            r#"<Initialization/>"#,
            r#"<Initialization range="0-99"/>"#,
        ] {
            let body = format!(
                r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic" availabilityStartTime="1970-01-01T00:00:00Z" minimumUpdatePeriod="PT2S" timeShiftBufferDepth="PT10S"><Period><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentList timescale="1">{initialization}<SegmentTimeline><S t="0" d="2"/></SegmentTimeline><SegmentURL media="seg-0.m4s"/></SegmentList><Representation id="v1" bandwidth="1000000"/></AdaptationSet></Period></MPD>"#
            );
            assert_eq!(
                parse_dash_manifest_at(
                    BASE_URL,
                    &body,
                    SystemTime::UNIX_EPOCH + Duration::from_secs(120)
                )
                .err(),
                Some(DashError::UnsupportedAddressing),
                "{initialization}"
            );
        }
    }

    #[test]
    fn dynamic_segment_list_rejects_duplicate_or_overlapping_resources() {
        for (initialization, segments) in [
            (
                r#"<Initialization sourceURL="init.mp4"/>"#,
                r#"<SegmentURL media="same.m4s"/><SegmentURL media="same.m4s"/>"#,
            ),
            (
                r#"<Initialization sourceURL="init.mp4"/>"#,
                r#"<SegmentURL media="same.m4s" mediaRange="0-99"/><SegmentURL media="same.m4s" mediaRange="50-149"/>"#,
            ),
            (
                r#"<Initialization sourceURL="same.m4s" range="0-99"/>"#,
                r#"<SegmentURL media="same.m4s" mediaRange="50-149"/><SegmentURL media="other.m4s"/>"#,
            ),
        ] {
            let body = format!(
                r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic" availabilityStartTime="1970-01-01T00:00:00Z" minimumUpdatePeriod="PT2S" timeShiftBufferDepth="PT10S"><Period><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentList timescale="1">{initialization}<SegmentTimeline><S t="0" d="2"/><S t="2" d="2"/></SegmentTimeline>{segments}</SegmentList><Representation id="v1" bandwidth="1000000"/></AdaptationSet></Period></MPD>"#
            );
            let manifest = parse_dash_manifest_at(
                BASE_URL,
                &body,
                SystemTime::UNIX_EPOCH + Duration::from_secs(120),
            )
            .expect("valid addressing should parse before resource expansion");
            let representation = &manifest.video_representations[0];
            assert_eq!(
                plan_dash_representation(&manifest, &representation.live_key).err(),
                Some(DashError::InvalidSegment)
            );
        }
    }

    #[test]
    fn dynamic_segment_list_rejects_implicit_timeline_times_and_repeat_runs() {
        for timeline in [r#"<S d="2"/>"#, r#"<S t="100" d="2" r="1"/>"#] {
            let body = format!(
                r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic" availabilityStartTime="1970-01-01T00:00:00Z" minimumUpdatePeriod="PT2S" timeShiftBufferDepth="PT10S"><Period><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentList timescale="1"><Initialization sourceURL="init.mp4"/><SegmentTimeline>{timeline}</SegmentTimeline><SegmentURL media="seg-100.m4s"/><SegmentURL media="seg-102.m4s"/></SegmentList><Representation id="v1" bandwidth="1000000"/></AdaptationSet></Period></MPD>"#
            );
            assert_eq!(
                parse_dash_manifest_at(
                    BASE_URL,
                    &body,
                    SystemTime::UNIX_EPOCH + Duration::from_secs(120)
                )
                .err(),
                Some(DashError::UnsupportedAddressing),
                "{timeline}"
            );
        }
    }

    #[test]
    fn dynamic_dash_requires_bounded_update_metadata_and_rejects_segment_list() {
        let missing_ast = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic" minimumUpdatePeriod="PT4S"><Period><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentTemplate duration="4" media="s-$Number$.m4s"/><Representation id="v1"/></AdaptationSet></Period></MPD>"#;
        let missing_update = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic" availabilityStartTime="1970-01-01T00:00:00Z"><Period><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentTemplate duration="4" media="s-$Number$.m4s"/><Representation id="v1"/></AdaptationSet></Period></MPD>"#;
        let list = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic" availabilityStartTime="1970-01-01T00:00:00Z" minimumUpdatePeriod="PT4S"><Period><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentList><SegmentURL media="segment.m4s"/></SegmentList><Representation id="v1"/></AdaptationSet></Period></MPD>"#;
        assert_eq!(
            parse_dash_manifest_at(BASE_URL, missing_ast, dynamic_test_clock(20)).err(),
            Some(DashError::UnsupportedPresentation)
        );
        assert_eq!(
            parse_dash_manifest_at(BASE_URL, missing_update, dynamic_test_clock(20)).err(),
            Some(DashError::UnsupportedPresentation)
        );
        assert_eq!(
            parse_dash_manifest_at(BASE_URL, list, dynamic_test_clock(20)).err(),
            Some(DashError::UnsupportedAddressing)
        );
    }

    #[test]
    fn dynamic_dash_does_not_plan_a_segment_before_it_is_complete() {
        let body = dynamic_mpd(
            r#"<SegmentTemplate timescale="1" duration="4" media="seg-$Number$.m4s"/>"#,
            "",
        );
        let manifest = parse_dash_manifest_at(BASE_URL, &body, dynamic_test_clock(3))
            .expect("dynamic presentation can arrive before its first complete segment");
        let plan = plan_dash_representation(&manifest, &manifest.video_representations[0].live_key)
            .expect("an empty first live window remains refreshable");
        assert!(
            !plan
                .resources
                .iter()
                .any(|resource| resource.kind == DashResourceKind::Segment),
            "a segment is not exposed until its whole duration is available"
        );
    }

    #[test]
    fn expands_duration_template_with_initialization_and_padded_numbers() {
        let plan =
            parse_dash_manifest(BASE_URL, TEMPLATE_MPD).expect("valid template MPD should parse");
        let video = &plan.video_representations[0];
        let resources = plan_dash_representation(&plan, &video.key)
            .expect("selected representation should expand");

        assert_eq!(resources.resources.len(), 4);
        assert_eq!(
            resources.resources[0].kind,
            DashResourceKind::Initialization
        );
        assert_eq!(
            resources.resources[0].url.as_str(),
            "https://cdn.example.test/path/media/init-v1.mp4"
        );
        assert_eq!(
            resources.resources[1].url.as_str(),
            "https://cdn.example.test/path/media/v1/005-0.m4s"
        );
        assert_eq!(
            resources.resources[3].url.as_str(),
            "https://cdn.example.test/path/media/v1/007-8.m4s"
        );
    }

    #[test]
    fn expands_timeline_with_explicit_start_and_bounded_negative_repeat() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10S">
  <Period duration="PT10S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1" startNumber="10" media="seg-$Number$-$Time$.m4s">
        <SegmentTimeline>
          <S t="0" d="2" r="1"/>
          <S t="4" d="3" r="-1"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v1" bandwidth="500000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let plan = parse_dash_manifest(BASE_URL, body).expect("timeline MPD should parse");
        let video = &plan.video_representations[0];
        let resources =
            plan_dash_representation(&plan, &video.key).expect("timeline should expand");

        let urls = resources
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .map(|resource| resource.url.path().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            urls,
            [
                "/path/seg-10-0.m4s",
                "/path/seg-11-2.m4s",
                "/path/seg-12-4.m4s",
                "/path/seg-13-7.m4s"
            ]
        );
    }

    #[test]
    fn expands_static_timeline_using_presentation_time_offset() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT4S">
  <Period duration="PT4S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1000" startNumber="7"
                       presentationTimeOffset="900000"
                       media="seg-$Number$-$Time$.m4s">
        <SegmentTimeline>
          <S t="900000" d="2000" r="1"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let plan = parse_dash_manifest(BASE_URL, body).expect("non-zero PTO timeline should parse");
        let video = &plan.video_representations[0];
        let resources = plan_dash_representation(&plan, &video.key)
            .expect("PTO-aligned timeline should expand");

        let urls = resources
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .map(|resource| resource.url.path().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(urls, ["/path/seg-7-900000.m4s", "/path/seg-8-902000.m4s"]);
    }

    #[test]
    fn expands_static_duration_template_from_presentation_time_offset() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT4S">
  <Period duration="PT4S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1000" duration="2000"
                       presentationTimeOffset="900000"
                       media="seg-$Time$.m4s"/>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let plan = parse_dash_manifest(BASE_URL, body)
            .expect("duration template PTO may exceed the Period duration in raw units");
        let resources = plan_dash_representation(&plan, &plan.video_representations[0].key)
            .expect("duration template should expand from the sample-timeline offset");

        let urls = resources
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .map(|resource| resource.url.path().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(urls, ["/path/seg-900000.m4s", "/path/seg-902000.m4s"]);
    }

    #[test]
    fn negative_timeline_repeat_uses_presentation_time_offset_for_period_end() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT4S">
  <Period duration="PT4S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1000" presentationTimeOffset="900000"
                       media="seg-$Time$.m4s">
        <SegmentTimeline>
          <S t="900000" d="2000" r="-1"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let plan = parse_dash_manifest(BASE_URL, body).expect("non-zero PTO timeline should parse");
        let resources = plan_dash_representation(&plan, &plan.video_representations[0].key)
            .expect("negative repeat should reach the PTO-adjusted Period end");

        let urls = resources
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .map(|resource| resource.url.path().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(urls, ["/path/seg-900000.m4s", "/path/seg-902000.m4s"]);
    }

    #[test]
    fn skips_timeline_segments_before_the_period_and_preserves_numbers() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <Period duration="PT2S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1" startNumber="10"
                       presentationTimeOffset="4"
                       media="seg-$Number$-$Time$.m4s">
        <SegmentTimeline>
          <S t="0" d="2" r="2"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let plan = parse_dash_manifest(BASE_URL, body).expect("static timeline should parse");
        let resources = plan_dash_representation(&plan, &plan.video_representations[0].key)
            .expect("the segment at the Period start should remain available");

        let urls = resources
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .map(|resource| resource.url.path().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(urls, ["/path/seg-12-4.m4s"]);
    }

    #[test]
    fn falls_back_when_a_timeline_segment_crosses_the_period_start() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <Period duration="PT2S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1" presentationTimeOffset="4"
                       media="seg-$Time$.m4s">
        <SegmentTimeline>
          <S t="3" d="2"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let plan = parse_dash_manifest(BASE_URL, body).expect("MPD structure should parse");
        assert_eq!(
            plan_dash_representation(&plan, &plan.video_representations[0].key).err(),
            Some(DashError::UnsupportedAddressing)
        );
    }

    #[test]
    fn resolves_presentation_time_offset_with_representation_precedence() {
        let inherited = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <Period duration="PT2S">
    <SegmentTemplate timescale="1" duration="2" presentationTimeOffset="100"
                     media="seg-$Time$.m4s"/>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate presentationTimeOffset="200"/>
      <Representation id="v1">
        <SegmentTemplate presentationTimeOffset="300"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let plan =
            parse_dash_manifest(BASE_URL, inherited).expect("inherited templates should parse");
        let resources = plan_dash_representation(&plan, &plan.video_representations[0].key)
            .expect("the most specific PTO should be used");
        assert_eq!(resources.resources[0].url.path(), "/path/seg-300.m4s");

        let explicit_zero = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <Period duration="PT2S">
    <SegmentTemplate timescale="1" duration="2" presentationTimeOffset="100"
                     media="seg-$Time$.m4s"/>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <Representation id="v1">
        <SegmentTemplate presentationTimeOffset="0"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let plan = parse_dash_manifest(BASE_URL, explicit_zero)
            .expect("explicit zero should override PTO");
        let resources = plan_dash_representation(&plan, &plan.video_representations[0].key)
            .expect("explicit zero PTO should be honored");
        assert_eq!(resources.resources[0].url.path(), "/path/seg-0.m4s");
    }

    #[test]
    fn representation_specific_pto_offsets_include_period_start() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT6S">
  <Period start="PT2S" duration="PT4S">
    <AdaptationSet id="video" contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1000" duration="2000"
                       presentationTimeOffset="1000" media="video-$Time$.m4s"/>
      <Representation id="v1"/>
    </AdaptationSet>
    <AdaptationSet id="audio" contentType="audio" mimeType="audio/mp4">
      <SegmentTemplate timescale="1000" duration="2000"
                       presentationTimeOffset="8000" media="audio-$Time$.m4s"/>
      <Representation id="a1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let manifest = parse_dash_manifest(BASE_URL, body)
            .expect("static DASH with distinct track PTO values should parse");
        let video = plan_dash_representation(&manifest, &manifest.video_representations[0].key)
            .expect("plan video representation");
        let audio = plan_dash_representation(&manifest, &manifest.audio_representations[0].key)
            .expect("plan audio representation");

        assert_eq!(
            video.input_timestamp_offset.ffmpeg_argument().as_deref(),
            Some("1.000000000")
        );
        assert_eq!(
            audio.input_timestamp_offset.ffmpeg_argument().as_deref(),
            Some("-6.000000000")
        );
    }

    #[test]
    fn zero_origin_dash_omits_ffmpeg_timestamp_offset() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <Period duration="PT2S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1" duration="2" media="segment-$Number$.m4s"/>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let manifest =
            parse_dash_manifest(BASE_URL, body).expect("zero-origin static DASH should parse");
        let plan = plan_dash_representation(&manifest, &manifest.video_representations[0].key)
            .expect("plan zero-origin representation");
        assert_eq!(plan.input_timestamp_offset.ffmpeg_argument(), None);
    }

    #[test]
    fn pto_changes_preserve_the_existing_representation_key() {
        fn plan_for_pto(presentation_time_offset: u64) -> (String, String) {
            let body = format!(
                r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <Period duration="PT2S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1000" duration="2000"
                       presentationTimeOffset="{presentation_time_offset}"
                       media="segment-$Time$.m4s"/>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#
            );
            let manifest =
                parse_dash_manifest(BASE_URL, &body).expect("static PTO manifest should parse");
            let representation = &manifest.video_representations[0];
            let plan = plan_dash_representation(&manifest, &representation.key)
                .expect("static PTO representation should be planned");
            (
                representation.key.clone(),
                plan.input_timestamp_offset
                    .ffmpeg_argument()
                    .expect("non-zero PTO produces an offset"),
            )
        }

        let (first_key, first_offset) = plan_for_pto(8_000);
        let (second_key, second_offset) = plan_for_pto(9_000);
        assert_eq!(
            first_key, second_key,
            "persisted representation keys remain stable"
        );
        assert_ne!(
            first_offset, second_offset,
            "PTO remains separate plan metadata"
        );
    }

    #[test]
    fn dynamic_segment_list_with_nonzero_presentation_time_offset_uses_fallback() {
        let body = dynamic_mpd(
            r#"<SegmentList timescale="1000" presentationTimeOffset="9000">
                 <Initialization sourceURL="init.mp4"/>
                 <SegmentTimeline><S t="0" d="2000"/></SegmentTimeline>
                 <SegmentURL media="segment.m4s"/>
               </SegmentList>"#,
            r#"timeShiftBufferDepth="PT10S""#,
        );
        assert_eq!(
            parse_dash_manifest_at(BASE_URL, &body, dynamic_test_clock(120)).err(),
            Some(DashError::UnsupportedAddressing)
        );
    }

    #[test]
    fn dynamic_template_with_nonzero_presentation_time_offset_uses_fallback() {
        let body = dynamic_mpd(
            r#"<SegmentTemplate timescale="1" duration="4"
                               presentationTimeOffset="4" media="seg-$Number$.m4s"/>"#,
            "",
        );
        assert_eq!(
            parse_dash_manifest_at(BASE_URL, &body, dynamic_test_clock(20)).err(),
            Some(DashError::UnsupportedAddressing)
        );
    }

    #[test]
    fn rejects_negative_template_presentation_time_offset() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <Period duration="PT2S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1" duration="2"
                       presentationTimeOffset="-1" media="seg-$Time$.m4s"/>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        assert!(parse_dash_manifest(BASE_URL, body).is_err());
    }

    #[test]
    fn negative_timeline_repeat_uses_ceiling_to_next_explicit_start() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10S">
  <Period duration="PT10S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1" media="seg-$Time$.m4s">
        <SegmentTimeline>
          <S t="0" d="2" r="-1"/>
          <S t="5" d="3" r="0"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let plan = parse_dash_manifest(BASE_URL, body).expect("timeline MPD should parse");
        let video = &plan.video_representations[0];
        let resources = plan_dash_representation(&plan, &video.key)
            .expect("negative repeat should expand to the next explicit start");
        let times = resources
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .map(|resource| resource.url.path().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            times,
            [
                "/path/seg-0.m4s",
                "/path/seg-2.m4s",
                "/path/seg-4.m4s",
                "/path/seg-5.m4s"
            ]
        );
    }

    #[test]
    fn rejects_explicit_timeline_start_overlapping_previous_positive_repeat_run() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT12S">
  <Period duration="PT12S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1" media="seg-$Time$.m4s">
        <SegmentTimeline>
          <S t="0" d="4" r="2"/>
          <S t="4" d="4"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let plan = parse_dash_manifest(BASE_URL, body).expect("MPD syntax is valid");
        let video = &plan.video_representations[0];

        assert_eq!(
            plan_dash_representation(&plan, &video.key).err(),
            Some(DashError::InvalidSegment),
            "the explicit t=4 entry overlaps the preceding run ending at t=12"
        );
    }

    #[test]
    fn negative_timeline_repeat_uses_ceiling_to_period_end() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10S">
  <Period duration="PT10S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1" media="seg-$Time$.m4s">
        <SegmentTimeline>
          <S t="4" d="4" r="-1"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let plan = parse_dash_manifest(BASE_URL, body).expect("timeline MPD should parse");
        let video = &plan.video_representations[0];
        let resources = plan_dash_representation(&plan, &video.key)
            .expect("last negative repeat should reach the Period end");
        let times = resources
            .resources
            .iter()
            .filter(|resource| resource.kind == DashResourceKind::Segment)
            .map(|resource| resource.url.path().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(times, ["/path/seg-4.m4s", "/path/seg-8.m4s"]);
    }

    #[test]
    fn video_and_audio_resource_filenames_are_representation_scoped() {
        let plan = parse_dash_manifest(BASE_URL, TEMPLATE_MPD)
            .expect("static video/audio MPD should parse");
        let video = plan_dash_representation(&plan, &plan.video_representations[0].key)
            .expect("video representation should expand");
        let audio = plan_dash_representation(&plan, &plan.audio_representations[0].key)
            .expect("audio representation should expand");

        let video_names = video
            .resources
            .iter()
            .map(|resource| resource.filename.as_str())
            .collect::<std::collections::HashSet<_>>();
        let audio_names = audio
            .resources
            .iter()
            .map(|resource| resource.filename.as_str())
            .collect::<std::collections::HashSet<_>>();
        assert!(
            video_names.is_disjoint(&audio_names),
            "audio and video staging names must not collide"
        );
        assert!(
            video
                .resources
                .iter()
                .all(|resource| { resource.filename.starts_with("dash-") })
        );
        assert!(
            audio
                .resources
                .iter()
                .all(|resource| { resource.filename.starts_with("dash-") })
        );
    }

    #[test]
    fn expands_segment_list_with_initialization_and_byte_ranges() {
        let body = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT4S">
  <Period duration="PT4S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentList>
        <Initialization sourceURL="all.mp4" range="0-999"/>
        <SegmentURL media="all.mp4" mediaRange="1000-1999"/>
        <SegmentURL media="all.mp4" mediaRange="2000-2999"/>
      </SegmentList>
      <Representation id="v1" bandwidth="500000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let plan = parse_dash_manifest(BASE_URL, body).expect("SegmentList MPD should parse");
        let video = &plan.video_representations[0];
        let resources =
            plan_dash_representation(&plan, &video.key).expect("SegmentList should expand");

        assert_eq!(resources.resources.len(), 3);
        assert_eq!(
            resources.resources[0].range,
            Some(ManifestByteRange {
                offset: 0,
                length: 1_000
            })
        );
        assert_eq!(
            resources.resources[1].range,
            Some(ManifestByteRange {
                offset: 1_000,
                length: 1_000
            })
        );
        assert_eq!(
            resources.resources[2].range,
            Some(ManifestByteRange {
                offset: 2_000,
                length: 1_000
            })
        );
    }

    #[test]
    fn rejects_dynamic_multi_period_drm_xlink_and_webm() {
        let rejected = [
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic"><Period/></MPD>"#,
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT1S"/><Period duration="PT1S"/></MPD>"#,
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><ContentProtection schemeIdUri="urn:uuid:00000000-0000-0000-0000-000000000000"/><Period duration="PT2S"/></MPD>"#,
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" xmlns:xlink="http://www.w3.org/1999/xlink" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S" xlink:href="external.xml"/></MPD>"#,
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S"><AdaptationSet contentType="video" mimeType="video/webm"><SegmentList><SegmentURL media="v.webm"/></SegmentList><Representation id="v1"/></AdaptationSet></Period></MPD>"#,
        ];

        for body in rejected {
            assert!(
                parse_dash_manifest(BASE_URL, body).is_err(),
                "unsupported MPD should not enter native path: {body}"
            );
        }
    }

    #[test]
    fn segment_base_and_representation_index_are_unsupported() {
        let cases = [
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S"><AdaptationSet contentType="video" mimeType="video/mp4"><Representation id="v1"><BaseURL>video.mp4</BaseURL><SegmentBase/></Representation></AdaptationSet></Period></MPD>"#,
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S"><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentBase indexRange="0-99"/><Representation id="v1"/></AdaptationSet></Period></MPD>"#,
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S"><AdaptationSet contentType="video" mimeType="video/mp4"><Representation id="v1"><SegmentBase><RepresentationIndex sourceURL="index.sidx" range="0-43"/></SegmentBase></Representation></AdaptationSet></Period></MPD>"#,
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S"><AdaptationSet contentType="video" mimeType="video/mp4"><Representation id="v1"><BaseURL>folder/</BaseURL><SegmentBase indexRange="0-99"/></Representation></AdaptationSet></Period></MPD>"#,
        ];
        for body in cases {
            assert_eq!(
                parse_dash_manifest(BASE_URL, body).err(),
                Some(DashError::UnsupportedAddressing),
                "SegmentBase/SIDX must not enter the native path: {body}"
            );
        }
    }

    #[test]
    fn dynamic_segment_base_is_unsupported() {
        let dynamic = dynamic_mpd(
            r#"<BaseURL>video.mp4</BaseURL><SegmentBase indexRange="0-999"/>"#,
            "",
        );
        assert_eq!(
            parse_dash_manifest_at(BASE_URL, &dynamic, dynamic_test_clock(120)).err(),
            Some(DashError::UnsupportedAddressing)
        );
    }

    #[test]
    fn rejects_subset_and_preselection_combinations() {
        let subset = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <Period duration="PT2S">
    <Subset id="1" contains="1 2"/>
    <AdaptationSet id="1" contentType="video" mimeType="video/mp4">
      <SegmentTemplate duration="2" media="video-$Number$.m4s"/>
      <Representation id="v1"/>
    </AdaptationSet>
    <AdaptationSet id="2" contentType="audio" mimeType="audio/mp4">
      <SegmentTemplate duration="2" media="audio-$Number$.m4s"/>
      <Representation id="a1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let preselection = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">
  <Period duration="PT2S">
    <AdaptationSet id="1" contentType="audio" mimeType="audio/mp4">
      <SegmentTemplate duration="2" media="audio-$Number$.m4s"/>
      <Representation id="a1"/>
    </AdaptationSet>
    <Preselection id="main" tag="main" preselectionComponents="1"
                  codecs="ac-4.02.01.03"/>
  </Period>
</MPD>"#;

        let parsed_subset = dash_mpd_core::parse(subset).expect("Subset fixture MPD should parse");
        assert_eq!(parsed_subset.periods[0].subsets.len(), 1);
        assert_eq!(
            parse_dash_manifest(BASE_URL, subset).err(),
            Some(DashError::UnsupportedPresentation)
        );

        let parsed_preselection =
            dash_mpd_core::parse(preselection).expect("Preselection fixture MPD should parse");
        assert_eq!(parsed_preselection.periods[0].pre_selections.len(), 1);
        assert_eq!(
            parse_dash_manifest(BASE_URL, preselection).err(),
            Some(DashError::UnsupportedPresentation)
        );
    }

    #[test]
    fn rejects_unknown_templates_and_invalid_byte_ranges() {
        let unknown = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S"><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentTemplate media="$Unknown$.m4s" duration="2"/><Representation id="v1"/></AdaptationSet></Period></MPD>"#;
        let invalid_range = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S"><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentList><SegmentURL media="v.mp4" mediaRange="9-2"/></SegmentList><Representation id="v1"/></AdaptationSet></Period></MPD>"#;

        assert_eq!(
            parse_dash_manifest(BASE_URL, unknown)
                .and_then(|plan| plan_dash_representation(
                    &plan,
                    &plan.video_representations[0].key
                ))
                .err(),
            Some(DashError::UnsupportedAddressing)
        );
        assert_eq!(
            parse_dash_manifest(BASE_URL, invalid_range).err(),
            Some(DashError::InvalidSegment)
        );
    }

    #[test]
    fn rejects_non_https_and_oversized_manifests() {
        assert_eq!(
            parse_dash_manifest("http://cdn.example.test/manifest.mpd", TEMPLATE_MPD).err(),
            Some(DashError::InvalidManifestUrl)
        );

        let oversized = " ".repeat(MAX_MANIFEST_BYTES + 1);
        assert_eq!(
            parse_dash_manifest(BASE_URL, &oversized).err(),
            Some(DashError::Parse)
        );
    }

    #[test]
    fn accepts_scte35_event_metadata_without_filtering_media() {
        let period_event = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S"><EventStream schemeIdUri="urn:scte:scte35:2013:xml"><Event id="1" presentationTime="0" duration="2"/></EventStream><AdaptationSet contentType="video" mimeType="video/mp4"><SegmentTemplate media="v-$Number$.m4s" duration="2"/><Representation id="v1" bandwidth="500000"/></AdaptationSet></Period></MPD>"#;
        let inband_event = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S"><Period duration="PT2S"><AdaptationSet contentType="video" mimeType="video/mp4"><InbandEventStream schemeIdUri="urn:scte:scte35:2013:bin"/><SegmentTemplate media="v-$Number$.m4s" duration="2"/><Representation id="v1" bandwidth="500000"/></AdaptationSet></Period></MPD>"#;

        for body in [period_event, inband_event] {
            assert!(
                parse_dash_manifest(BASE_URL, body).is_ok(),
                "SCTE-35 metadata must not cause media segments to be filtered: {body}"
            );
        }
    }
