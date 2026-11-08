//! Focused LL-DASH availability-planning regression tests.

use std::time::{Duration, SystemTime};

use super::{DashError, parse_dash_manifest_at, plan_dash_representation};

const BASE_URL: &str = "https://cdn.example.test/path/manifest.mpd";

#[test]
fn availability_time_offsets_must_be_finite_and_non_negative() {
    assert_eq!(
        super::low_latency::parse_availability_time_offset(30.0),
        Ok(Duration::from_secs(30))
    );
    assert_eq!(
        super::low_latency::parse_availability_time_offset(300.001),
        Ok(Duration::from_secs_f64(300.001))
    );
    assert!(super::low_latency::parse_availability_time_offset(-1.0).is_err());
    assert!(super::low_latency::parse_availability_time_offset(f64::INFINITY).is_err());
    assert!(super::low_latency::parse_availability_time_offset(f64::MAX).is_err());
}

#[test]
fn low_latency_representation_identity_survives_availability_policy_changes() {
    let first_body = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     type="dynamic"
     availabilityStartTime="1970-01-01T00:00:00Z"
     minimumUpdatePeriod="PT4S"
     timeShiftBufferDepth="PT20S">
  <Period start="PT0S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1" startNumber="1" duration="4"
                       initialization="video-init.mp4"
                       media="video-$Number$.m4s"
                       availabilityTimeOffset="1"
                       availabilityTimeComplete="false"/>
      <Representation id="v1" bandwidth="1000000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
    let second_body = first_body.replace(
        "availabilityTimeOffset=\"1\"",
        "availabilityTimeOffset=\"4\"",
    );
    let first = parse_dash_manifest_at(
        BASE_URL,
        first_body,
        SystemTime::UNIX_EPOCH + Duration::from_secs(120),
    )
    .expect("initial low-latency MPD should parse");
    let refreshed = parse_dash_manifest_at(
        BASE_URL,
        &second_body,
        SystemTime::UNIX_EPOCH + Duration::from_secs(120),
    )
    .expect("refreshed availability policy should parse");

    let first_representation = &first.video_representations[0];
    let refreshed_representation = &refreshed.video_representations[0];
    assert_eq!(
        first_representation.live_key, refreshed_representation.live_key,
        "availability policy is mutable MPD metadata, not representation identity"
    );
    assert_ne!(
        first_representation.legacy_live_key, refreshed_representation.legacy_live_key,
        "the compatibility key must retain the previous policy-sensitive identity"
    );

    let legacy_plan = plan_dash_representation(
        &first,
        first_representation
            .legacy_live_key
            .as_deref()
            .expect("non-default LL policy has a legacy alias"),
    )
    .expect("an existing queued selection may use the previous live key");
    let refreshed_plan = plan_dash_representation(&refreshed, &refreshed_representation.live_key)
        .expect("the refreshed stable key should plan with the new ATO");
    assert!(legacy_plan.low_latency);
    assert!(refreshed_plan.low_latency);
    assert_ne!(
        legacy_plan.resources, refreshed_plan.resources,
        "a changed ATO changes the current plan without changing the track identity"
    );
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
fn shorter_segment_timeline_on_refresh_does_not_invalidate_availability_offset() {
    let initial_body = dynamic_mpd(
        r#"<SegmentTemplate timescale="1" startNumber="1"
                           initialization="init.mp4" media="seg-$Number$"
                           availabilityTimeOffset="3"
                           availabilityTimeComplete="false">
  <SegmentTimeline><S t="0" d="4" r="-1"/></SegmentTimeline>
</SegmentTemplate>"#,
        r#"timeShiftBufferDepth="PT20S""#,
    );
    let refreshed_body = initial_body.replace("d=\"4\"", "d=\"2\"");
    let initial = parse_dash_manifest_at(BASE_URL, &initial_body, dynamic_test_clock(120))
        .expect("initial segment durations should parse");
    let refreshed = parse_dash_manifest_at(BASE_URL, &refreshed_body, dynamic_test_clock(124))
        .expect("shorter refreshed segment durations should remain supported");

    assert_eq!(
        initial.video_representations[0].live_key, refreshed.video_representations[0].live_key,
        "timeline refreshes and availability-window changes must not alter identity"
    );
    assert!(
        plan_dash_representation(&refreshed, &refreshed.video_representations[0].live_key,).is_ok(),
        "the previously accepted finite offset remains valid with shorter refreshed segments"
    );
}

#[test]
fn dynamic_segment_template_uses_availability_offset_for_chunked_live_segment() {
    let body = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     type="dynamic"
     availabilityStartTime="1970-01-01T00:00:00Z"
     minimumUpdatePeriod="PT1S">
  <Period start="PT0S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate duration="4" startNumber="1"
                       initialization="video-init.mp4"
                       media="video-$Number$.m4s"
                       availabilityTimeOffset="3"
                       availabilityTimeComplete="false"/>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
    let manifest = parse_dash_manifest_at(
        BASE_URL,
        body,
        SystemTime::UNIX_EPOCH + Duration::from_secs(6),
    )
    .expect("supported LL-DASH SegmentTemplate should parse");
    let representation = &manifest.video_representations[0];
    let plan = plan_dash_representation(&manifest, &representation.live_key)
        .expect("LL-DASH representation should expand");

    assert!(plan.low_latency);
    assert_eq!(
        plan.resources
            .iter()
            .filter_map(|resource| resource.segment_number)
            .collect::<Vec<_>>(),
        vec![1, 2],
        "the current incomplete segment is available at now + ATO"
    );
}

#[test]
fn dynamic_segment_template_without_availability_offset_keeps_complete_segment_window() {
    let body = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     type="dynamic"
     availabilityStartTime="1970-01-01T00:00:00Z"
     minimumUpdatePeriod="PT1S">
  <Period start="PT0S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate duration="4" startNumber="1" media="video-$Number$.m4s"/>
      <Representation id="v1"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
    let manifest = parse_dash_manifest_at(
        BASE_URL,
        body,
        SystemTime::UNIX_EPOCH + Duration::from_secs(6),
    )
    .expect("ordinary dynamic DASH should continue to parse");
    let plan = plan_dash_representation(&manifest, &manifest.video_representations[0].live_key)
        .expect("ordinary dynamic representation should expand");

    assert!(!plan.low_latency);
    assert_eq!(
        plan.resources
            .iter()
            .filter_map(|resource| resource.segment_number)
            .collect::<Vec<_>>(),
        vec![1],
        "ordinary dynamic DASH still waits for the nominal segment end"
    );
}

#[test]
fn dynamic_segment_template_inherits_and_bounds_availability_attributes() {
    let inherited = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     type="dynamic"
     availabilityStartTime="1970-01-01T00:00:00Z"
     minimumUpdatePeriod="PT1S">
  <Period start="PT0S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate duration="4" startNumber="1"
                       initialization="video-init.mp4"
                       media="video-$Number$.m4s"
                       availabilityTimeOffset="3"/>
      <Representation id="v1">
        <SegmentTemplate availabilityTimeOffset="2"
                         availabilityTimeComplete="false"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
    let manifest = parse_dash_manifest_at(
        BASE_URL,
        inherited,
        SystemTime::UNIX_EPOCH + Duration::from_secs(5),
    )
    .expect("representation-level values should override inherited template values");
    let plan = plan_dash_representation(&manifest, &manifest.video_representations[0].live_key)
        .expect("inherited LL-DASH representation should expand");
    assert!(plan.low_latency);
    assert_eq!(
        plan.resources
            .iter()
            .filter_map(|resource| resource.segment_number)
            .collect::<Vec<_>>(),
        vec![1],
        "the child ATO of two seconds excludes the next segment at t=5; the parent ATO of three seconds would include it"
    );

    let no_offset = inherited.replace(
        "availabilityTimeOffset=\"2\"",
        "availabilityTimeOffset=\"0\"",
    );
    assert_eq!(
        parse_dash_manifest_at(
            BASE_URL,
            &no_offset,
            SystemTime::UNIX_EPOCH + Duration::from_secs(6),
        )
        .err(),
        Some(DashError::UnsupportedAddressing),
        "an incomplete segment must advertise a positive availability offset"
    );

    let over_segment_offset = inherited.replace(
        "availabilityTimeOffset=\"2\"",
        "availabilityTimeOffset=\"4.1\"",
    );
    assert!(
        parse_dash_manifest_at(
            BASE_URL,
            &over_segment_offset,
            SystemTime::UNIX_EPOCH + Duration::from_secs(6),
        )
        .is_ok(),
        "a segment-duration change must not invalidate a finite, bounded offset"
    );

    let large_offset = inherited.replace(
        "availabilityTimeOffset=\"2\"",
        "availabilityTimeOffset=\"300.001\"",
    );
    assert!(
        parse_dash_manifest_at(
            BASE_URL,
            &large_offset,
            SystemTime::UNIX_EPOCH + Duration::from_secs(6),
        )
        .is_ok(),
        "a large finite offset is accepted when the planner can bound the resource window"
    );

    let non_finite = inherited.replace(
        "availabilityTimeOffset=\"2\"",
        "availabilityTimeOffset=\"INF\"",
    );
    assert!(
        parse_dash_manifest_at(
            BASE_URL,
            &non_finite,
            SystemTime::UNIX_EPOCH + Duration::from_secs(6),
        )
        .is_err(),
        "unbounded availability offsets remain unsupported"
    );

    let missing_init = inherited.replace("initialization=\"video-init.mp4\"\n", "");
    assert_eq!(
        parse_dash_manifest_at(
            BASE_URL,
            &missing_init,
            SystemTime::UNIX_EPOCH + Duration::from_secs(6),
        )
        .err(),
        Some(DashError::UnsupportedAddressing),
        "chunked CMAF segments require an initialization resource"
    );
}
