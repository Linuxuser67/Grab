//! Read DASH metadata omitted by the typed MPD model.
//!
//! The main MPD parser owns validation and resource planning. This bounded,
//! streaming pass retains only representation-level main-role flags.

use quick_xml::XmlVersion;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;

/// DASH MPD elements are identified by this namespace, regardless of prefix.
const DASH_MPD_NAMESPACE: &str = "urn:mpeg:dash:schema:mpd:2011";
/// Maximum XML nesting accepted by this small metadata pass.
const MAX_ELEMENT_DEPTH: usize = 32;
/// Bound period metadata retained from an untrusted MPD.
pub(super) const MAX_PERIODS: usize = 32;
/// Role scheme used to identify the main audio or subtitle representation.
pub(super) const DASH_ROLE_SCHEME: &str = "urn:mpeg:dash:role:2011";

/// Representation identity and its direct-child `Role=main` flag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RepresentationRoleMetadata {
    pub(super) id: Option<String>,
    pub(super) is_main: bool,
    /// Effective inherited SegmentList presentation-time offset.
    pub(super) segment_list_presentation_time_offset: Option<u64>,
}

/// Representation metadata for one Period, grouped by AdaptationSet.
pub(super) type PeriodRepresentationMetadata = Vec<Vec<RepresentationRoleMetadata>>;

/// Representation metadata grouped in MPD Period source order.
pub(super) type RepresentationMetadata = Vec<PeriodRepresentationMetadata>;

#[derive(Clone, Copy)]
enum Element {
    Mpd,
    Period,
    AdaptationSet,
    Representation,
    Other,
}

/// Return representation IDs and Role=main flags grouped by Period, AdaptationSet, and source order.
///
/// IDs allow the caller to verify that this pass and the typed MPD parser
/// identify the same representation at each source-order position.
pub(super) fn representation_metadata(
    body: &[u8],
    max_representations: usize,
) -> Option<RepresentationMetadata> {
    let mut reader = NsReader::from_reader(body);
    let mut buffer = Vec::new();
    let mut stack = Vec::new();
    let mut periods: RepresentationMetadata = Vec::new();
    let mut representation_count = 0_usize;
    let mut adaptation_count = 0_usize;
    let mut period_list_pto = None;
    let mut adaptation_list_pto = None;
    let mut representation_list_pto = None;

    loop {
        let (namespace, event) = reader.read_resolved_event_into(&mut buffer).ok()?;
        match event {
            Event::Start(start) => push_element(
                &start,
                &namespace,
                &mut stack,
                &mut periods,
                max_representations,
                &mut representation_count,
                &mut adaptation_count,
                &mut period_list_pto,
                &mut adaptation_list_pto,
                &mut representation_list_pto,
            )?,
            Event::Empty(start) => {
                push_element(
                    &start,
                    &namespace,
                    &mut stack,
                    &mut periods,
                    max_representations,
                    &mut representation_count,
                    &mut adaptation_count,
                    &mut period_list_pto,
                    &mut adaptation_list_pto,
                    &mut representation_list_pto,
                )?;
                stack.pop()?;
            }
            Event::End(_) => {
                stack.pop()?;
            }
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }

    if !stack.is_empty() || periods.is_empty() || periods.len() > MAX_PERIODS {
        return None;
    }

    Some(periods)
}

fn push_element(
    start: &BytesStart<'_>,
    namespace: &ResolveResult<'_>,
    stack: &mut Vec<Element>,
    periods: &mut RepresentationMetadata,
    max_representations: usize,
    representation_count: &mut usize,
    adaptation_count: &mut usize,
    period_list_pto: &mut Option<u64>,
    adaptation_list_pto: &mut Option<u64>,
    representation_list_pto: &mut Option<u64>,
) -> Option<()> {
    if stack.len() >= MAX_ELEMENT_DEPTH {
        return None;
    }

    let local_name = start.name().local_name();
    let name = local_name.as_ref();
    let in_mpd_namespace = matches!(
        namespace,
        ResolveResult::Bound(namespace) if namespace.into_inner() == DASH_MPD_NAMESPACE
    );
    let element = match (stack.last().copied(), name, in_mpd_namespace) {
        (None, "MPD", true) => Element::Mpd,
        (Some(Element::Mpd), "Period", true) => {
            if periods.len() >= MAX_PERIODS {
                return None;
            }
            periods.push(Vec::new());
            *period_list_pto = None;
            *adaptation_list_pto = None;
            *representation_list_pto = None;
            Element::Period
        }
        (Some(Element::Period), "AdaptationSet", true) => {
            *adaptation_count = (*adaptation_count).checked_add(1)?;
            if *adaptation_count > max_representations {
                return None;
            }
            periods.last_mut()?.push(Vec::new());
            *adaptation_list_pto = *period_list_pto;
            *representation_list_pto = None;
            Element::AdaptationSet
        }
        (Some(Element::AdaptationSet), "Representation", true) => {
            *representation_count = (*representation_count).checked_add(1)?;
            if *representation_count > max_representations {
                return None;
            }
            periods
                .last_mut()?
                .last_mut()?
                .push(RepresentationRoleMetadata {
                    id: representation_id(start)?,
                    is_main: false,
                    segment_list_presentation_time_offset: *adaptation_list_pto,
                });
            *representation_list_pto = *adaptation_list_pto;
            Element::Representation
        }
        (Some(Element::Representation), "Role", true) => {
            if is_main_role(start)? {
                periods.last_mut()?.last_mut()?.last_mut()?.is_main = true;
            }
            Element::Other
        }
        (Some(Element::Period), "SegmentList", true) => {
            if let Some(offset) = segment_list_presentation_time_offset(start)? {
                *period_list_pto = Some(offset);
            }
            Element::Other
        }
        (Some(Element::AdaptationSet), "SegmentList", true) => {
            if let Some(offset) = segment_list_presentation_time_offset(start)? {
                *adaptation_list_pto = Some(offset);
            }
            Element::Other
        }
        (Some(Element::Representation), "SegmentList", true) => {
            if let Some(offset) = segment_list_presentation_time_offset(start)? {
                *representation_list_pto = Some(offset);
                periods.last_mut()?.last_mut()?.last_mut()?
                    .segment_list_presentation_time_offset = Some(offset);
            }
            Element::Other
        }
        _ => Element::Other,
    };
    stack.push(element);
    Some(())
}

fn segment_list_presentation_time_offset(start: &BytesStart<'_>) -> Option<Option<u64>> {
    let mut offset = None;
    for attribute in start.attributes() {
        let attribute = attribute.ok()?;
        if attribute.key.as_ref() == "presentationTimeOffset" {
            let value = attribute.normalized_value(XmlVersion::Implicit1_0).ok()?;
            offset = Some(value.parse::<u64>().ok()?);
        }
    }
    Some(offset)
}

fn representation_id(start: &BytesStart<'_>) -> Option<Option<String>> {
    let mut id = None;
    for attribute in start.attributes() {
        let attribute = attribute.ok()?;
        if attribute.key.as_ref() == "id" {
            id = Some(
                attribute
                    .normalized_value(XmlVersion::Implicit1_0)
                    .ok()?
                    .into_owned(),
            );
        }
    }
    Some(id)
}

fn is_main_role(start: &BytesStart<'_>) -> Option<bool> {
    let mut scheme = None;
    let mut value = None;

    for attribute in start.attributes() {
        let attribute = attribute.ok()?;
        match attribute.key.as_ref() {
            "schemeIdUri" => {
                scheme = Some(
                    attribute
                        .normalized_value(XmlVersion::Implicit1_0)
                        .ok()?
                        .into_owned(),
                );
            }
            "value" => {
                value = Some(
                    attribute
                        .normalized_value(XmlVersion::Implicit1_0)
                        .ok()?
                        .into_owned(),
                );
            }
            _ => {}
        }
    }

    Some(scheme.as_deref() == Some(DASH_ROLE_SCHEME) && value.as_deref() == Some("main"))
}

/// Detect non-zero SegmentList PTO that the pinned typed MPD model omits.
pub(super) fn has_nonzero_segment_list_presentation_time_offset(body: &[u8]) -> Option<bool> {
    let mut reader = NsReader::from_reader(body);
    let mut buffer = Vec::new();
    let mut depth = 0_usize;
    let mut has_nonzero_offset = false;

    loop {
        let (namespace, event) = reader.read_resolved_event_into(&mut buffer).ok()?;
        let start = match &event {
            Event::Start(start) => {
                depth = depth.checked_add(1)?;
                if depth > MAX_ELEMENT_DEPTH {
                    return None;
                }
                Some(start)
            }
            Event::Empty(start) => Some(start),
            Event::End(_) => {
                depth = depth.checked_sub(1)?;
                None
            }
            Event::Eof => return (depth == 0).then_some(has_nonzero_offset),
            _ => None,
        };

        if let Some(start) = start
            && matches!(
                &namespace,
                ResolveResult::Bound(namespace)
                    if namespace.into_inner() == DASH_MPD_NAMESPACE
            )
            && start.name().local_name().as_ref() == "SegmentList"
        {
            for attribute in start.attributes() {
                let attribute = attribute.ok()?;
                if attribute.key.as_ref() == "presentationTimeOffset" {
                    let value = attribute.normalized_value(XmlVersion::Implicit1_0).ok()?;
                    let offset = value.parse::<u64>().ok()?;
                    has_nonzero_offset |= offset != 0;
                }
            }
        }

        buffer.clear();
    }
}

/// Reject SegmentList timing attributes not interpreted by the bounded live planner.
pub(super) fn has_unsupported_segment_list_timing_attributes(body: &[u8]) -> Option<bool> {
    let mut reader = NsReader::from_reader(body);
    let mut buffer = Vec::new();
    let mut depth = 0_usize;
    let mut unsupported = false;

    loop {
        let (namespace, event) = reader.read_resolved_event_into(&mut buffer).ok()?;
        let start = match &event {
            Event::Start(start) => {
                depth = depth.checked_add(1)?;
                if depth > MAX_ELEMENT_DEPTH {
                    return None;
                }
                Some(start)
            }
            Event::Empty(start) => Some(start),
            Event::End(_) => {
                depth = depth.checked_sub(1)?;
                None
            }
            Event::Eof => return (depth == 0).then_some(unsupported),
            _ => None,
        };

        if let Some(start) = start
            && matches!(
                &namespace,
                ResolveResult::Bound(namespace)
                    if namespace.into_inner() == DASH_MPD_NAMESPACE
            )
            && start.name().local_name().as_ref() == "SegmentList"
        {
            for attribute in start.attributes() {
                let attribute = attribute.ok()?;
                let key = attribute.key.as_ref();
                if key == "duration"
                    || key == "availabilityTimeOffset"
                    || key == "availabilityTimeComplete"
                    || key == "eptDelta"
                    || key == "pbDelta"
                {
                    unsupported = true;
                }
            }
        }

        buffer.clear();
    }
}

/// Detect SegmentList startNumber values that cannot be represented by the typed MPD model.
pub(super) fn has_nondefault_segment_list_start_number(body: &[u8]) -> Option<bool> {
    let mut reader = NsReader::from_reader(body);
    let mut buffer = Vec::new();
    let mut depth = 0_usize;
    let mut nondefault = false;

    loop {
        let (namespace, event) = reader.read_resolved_event_into(&mut buffer).ok()?;
        let start = match &event {
            Event::Start(start) => {
                depth = depth.checked_add(1)?;
                if depth > MAX_ELEMENT_DEPTH {
                    return None;
                }
                Some(start)
            }
            Event::Empty(start) => Some(start),
            Event::End(_) => {
                depth = depth.checked_sub(1)?;
                None
            }
            Event::Eof => return (depth == 0).then_some(nondefault),
            _ => None,
        };

        if let Some(start) = start
            && matches!(
                &namespace,
                ResolveResult::Bound(namespace)
                    if namespace.into_inner() == DASH_MPD_NAMESPACE
            )
            && start.name().local_name().as_ref() == "SegmentList"
        {
            for attribute in start.attributes() {
                let attribute = attribute.ok()?;
                if attribute.key.as_ref() == "startNumber" {
                    let value = attribute.normalized_value(XmlVersion::Implicit1_0).ok()?;
                    let number = value.parse::<u64>().ok()?;
                    nondefault |= number != 1;
                }
            }
        }

        buffer.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::representation_metadata;

    #[test]
    fn maps_representation_roles_in_source_order() {
        let body = br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011">
  <Period>
    <AdaptationSet>
      <Representation id="first"/>
      <Representation id="main">
        <Role schemeIdUri="urn:mpeg:dash:role:2011" value="main"/>
      </Representation>
    </AdaptationSet>
    <AdaptationSet>
      <Representation id="other"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

        let metadata = representation_metadata(body, 3).expect("valid MPD representation metadata");
        assert_eq!(metadata.len(), 1);
        assert_eq!(metadata[0].len(), 2);
        assert_eq!(metadata[0][0][0].id.as_deref(), Some("first"));
        assert!(!metadata[0][0][0].is_main);
        assert_eq!(metadata[0][0][1].id.as_deref(), Some("main"));
        assert!(metadata[0][0][1].is_main);
        assert_eq!(metadata[0][1][0].id.as_deref(), Some("other"));
        assert!(!metadata[0][1][0].is_main);
    }

    #[test]
    fn supports_prefixed_elements_and_empty_role_elements() {
        let body = br#"<mpd:MPD xmlns:mpd="urn:mpeg:dash:schema:mpd:2011">
  <mpd:Period>
    <mpd:AdaptationSet>
      <mpd:Representation id="main">
        <mpd:Role schemeIdUri="urn:mpeg:dash:role:2011" value="main"/>
      </mpd:Representation>
    </mpd:AdaptationSet>
  </mpd:Period>
</mpd:MPD>"#;

        let metadata = representation_metadata(body, 1).expect("valid MPD representation metadata");
        assert_eq!(metadata[0][0][0].id.as_deref(), Some("main"));
        assert!(metadata[0][0][0].is_main);
    }

    #[test]
    fn ignores_foreign_namespaces_without_shifting_representation_indexes() {
        let body =
            br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" xmlns:foreign="urn:example:foreign">
  <Period>
    <AdaptationSet>
      <foreign:Representation id="not-a-dash-representation">
        <foreign:Role schemeIdUri="urn:mpeg:dash:role:2011" value="main"/>
      </foreign:Representation>
      <Representation id="main">
        <Role schemeIdUri="urn:mpeg:dash:role:2011" value="main"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

        let metadata = representation_metadata(body, 1).expect("valid MPD representation metadata");
        assert_eq!(metadata[0][0][0].id.as_deref(), Some("main"));
        assert!(metadata[0][0][0].is_main);
    }

    #[test]
    fn ignores_namespaced_role_attributes() {
        let body =
            br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" xmlns:foreign="urn:example:foreign">
  <Period>
    <AdaptationSet>
      <Representation id="main">
        <Role foreign:schemeIdUri="urn:mpeg:dash:role:2011" foreign:value="main"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

        let metadata =
            representation_metadata(body, 1).expect("foreign attributes do not invalidate the MPD");
        assert_eq!(metadata[0][0][0].id.as_deref(), Some("main"));
        assert!(!metadata[0][0][0].is_main);
    }

    #[test]
    fn detects_unsupported_segment_list_timing_attributes() {
        let supported = br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"><Period><AdaptationSet><SegmentList timescale="1000"/></AdaptationSet></Period></MPD>"#;
        for attribute in [
            r#"duration="2""#,
            r#"availabilityTimeOffset="1""#,
            r#"availabilityTimeComplete="false""#,
            r#"eptDelta="1""#,
            r#"pbDelta="1""#,
        ] {
            let body = format!(
                r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"><Period><AdaptationSet><SegmentList {attribute}/></AdaptationSet></Period></MPD>"#
            );
            assert_eq!(
                super::has_unsupported_segment_list_timing_attributes(body.as_bytes()),
                Some(true),
                "{attribute}"
            );
        }
        assert_eq!(
            super::has_unsupported_segment_list_timing_attributes(supported),
            Some(false)
        );
    }

    #[test]
    fn detects_nondefault_segment_list_start_numbers() {
        let default_number = br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"><Period><AdaptationSet><SegmentList startNumber="1"/></AdaptationSet></Period></MPD>"#;
        let nondefault_number = br#"<mpd:MPD xmlns:mpd="urn:mpeg:dash:schema:mpd:2011"><mpd:Period><mpd:AdaptationSet><mpd:SegmentList startNumber="42"/></mpd:AdaptationSet></mpd:Period></mpd:MPD>"#;
        assert_eq!(
            super::has_nondefault_segment_list_start_number(default_number),
            Some(false)
        );
        assert_eq!(
            super::has_nondefault_segment_list_start_number(nondefault_number),
            Some(true)
        );
    }

    #[test]
    fn maps_inherited_segment_list_presentation_time_offsets_to_representations() {
        let body = br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011">
  <Period>
    <SegmentList presentationTimeOffset="10"/>
    <AdaptationSet>
      <SegmentList presentationTimeOffset="20"/>
      <Representation id="inherited"/>
      <Representation id="overridden">
        <SegmentList presentationTimeOffset="30"/>
      </Representation>
    </AdaptationSet>
    <AdaptationSet>
      <Representation id="period-inherited"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let metadata = representation_metadata(body, 3).expect("valid representation metadata");
        assert_eq!(
            metadata[0][0][0].segment_list_presentation_time_offset,
            Some(20)
        );
        assert_eq!(
            metadata[0][0][1].segment_list_presentation_time_offset,
            Some(30)
        );
        assert_eq!(
            metadata[0][1][0].segment_list_presentation_time_offset,
            Some(10)
        );
    }

    #[test]
    fn detects_nonzero_segment_list_presentation_time_offsets() {
        let nonzero = br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"><Period><AdaptationSet><SegmentList presentationTimeOffset="9000"/></AdaptationSet></Period></MPD>"#;
        assert_eq!(
            super::has_nonzero_segment_list_presentation_time_offset(nonzero),
            Some(true)
        );

        let zero = br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"><Period><AdaptationSet><SegmentList presentationTimeOffset="0"/></AdaptationSet></Period></MPD>"#;
        assert_eq!(
            super::has_nonzero_segment_list_presentation_time_offset(zero),
            Some(false)
        );

        let foreign = br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" xmlns:foreign="urn:example:foreign"><Period><AdaptationSet><foreign:SegmentList presentationTimeOffset="9000"/></AdaptationSet></Period></MPD>"#;
        assert_eq!(
            super::has_nonzero_segment_list_presentation_time_offset(foreign),
            Some(false)
        );
    }

    #[test]
    fn segment_list_timestamp_scan_rejects_excessive_xml_depth() {
        let nested_tags = format!(
            "{}{}",
            "<Other>".repeat(super::MAX_ELEMENT_DEPTH),
            "</Other>".repeat(super::MAX_ELEMENT_DEPTH)
        );
        let nested = format!(
            r#"<MPD xmlns="{}"><Period><AdaptationSet><Representation>{nested_tags}</Representation></AdaptationSet></Period></MPD>"#,
            super::DASH_MPD_NAMESPACE
        );

        assert_eq!(
            super::has_nonzero_segment_list_presentation_time_offset(nested.as_bytes()),
            None
        );
    }

    #[test]
    fn rejects_malformed_or_unbounded_role_documents() {
        assert_eq!(representation_metadata(b"<MPD>", 1), None);

        let too_many = br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"><Period><AdaptationSet>
  <Representation id="one"/>
  <Representation id="two"/>
</AdaptationSet></Period></MPD>"#;
        assert_eq!(representation_metadata(too_many, 1), None);
    }

    #[test]
    fn preserves_period_boundaries_in_metadata() {
        let body = br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011">
  <Period><AdaptationSet><Representation id="first"/></AdaptationSet></Period>
  <Period><AdaptationSet><Representation id="second"/></AdaptationSet></Period>
</MPD>"#;
        let metadata = representation_metadata(body, 2).expect("two-period metadata");
        assert_eq!(metadata.len(), 2);
        assert_eq!(metadata[0][0][0].id.as_deref(), Some("first"));
        assert_eq!(metadata[1][0][0].id.as_deref(), Some("second"));
    }

    #[test]
    fn rejects_excessive_xml_nesting() {
        let nested_tags = format!("{}{}", "<Other>".repeat(32), "</Other>".repeat(32));
        let nested = format!(
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"><Period><AdaptationSet><Representation>{}</Representation></AdaptationSet></Period></MPD>"#,
            nested_tags
        );
        assert_eq!(representation_metadata(nested.as_bytes(), 1), None);
    }
}
