//! Focused LL-DASH availability-planning regression tests.

use std::time::{Duration, SystemTime};

use super::{DashError, parse_dash_manifest_at, plan_dash_representation};

const BASE_URL: &str = "https://cdn.example.test/path/manifest.mpd";

#[test]
fn availability_time_offsets_are_finite_and_bounded() {
    assert_eq!(
        super::low_latency::parse_availability_time_offset(30.0),
        Ok(Duration::from_secs(30))
    );
    assert!(super::low_latency::parse_availability_time_offset(30.001).is_err());
    assert!(super::low_latency::parse_availability_time_offset(-1.0).is_err());
    assert!(super::low_latency::parse_availability_time_offset(f64::INFINITY).is_err());
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
    let plan = plan_dash_representation(
        &manifest,
        &manifest.video_representations[0].live_key,
    )
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
    let plan = plan_dash_representation(
        &manifest,
        &manifest.video_representations[0].live_key,
    )
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

    let excessive = inherited.replace(
        "availabilityTimeOffset=\"2\"",
        "availabilityTimeOffset=\"4.1\"",
    );
    assert_eq!(
        parse_dash_manifest_at(
            BASE_URL,
            &excessive,
            SystemTime::UNIX_EPOCH + Duration::from_secs(6),
        )
        .err(),
        Some(DashError::UnsupportedAddressing),
        "the availability offset must not exceed the shortest segment duration"
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
