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
    /// Effective inherited SegmentList availability-time offset.
    pub(super) segment_list_availability_time_offset: Option<String>,
}

/// Representation metadata for one Period, grouped by AdaptationSet.
pub(super) type PeriodRepresentationMetadata = Vec<Vec<RepresentationRoleMetadata>>;

/// Representation metadata and SegmentList compatibility flags collected in one bounded XML pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RepresentationMetadata {
    /// Representation metadata grouped in MPD Period source order.
    pub(super) periods: Vec<PeriodRepresentationMetadata>,
    pub(super) has_unsupported_segment_list_timing_attributes: bool,
    pub(super) has_nonzero_segment_list_presentation_time_offset: bool,
    pub(super) has_invalid_segment_list_presentation_time_offset: bool,
    pub(super) has_nondefault_segment_list_start_number: bool,
    pub(super) has_invalid_segment_list_start_number: bool,
}

#[derive(Default)]
struct MetadataState {
    representation_count: usize,
    adaptation_count: usize,
    period_list_pto: Option<u64>,
    adaptation_list_pto: Option<u64>,
    period_list_ato: Option<String>,
    adaptation_list_ato: Option<String>,
    has_unsupported_segment_list_timing_attributes: bool,
    has_nonzero_segment_list_presentation_time_offset: bool,
    has_invalid_segment_list_presentation_time_offset: bool,
    has_nondefault_segment_list_start_number: bool,
    has_invalid_segment_list_start_number: bool,
}

#[derive(Clone, Copy)]
enum Element {
    Mpd,
    Period,
    AdaptationSet,
    Representation,
    Other,
}

/// Collect representation metadata and SegmentList compatibility flags in one bounded pass.
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
    let mut periods: Vec<PeriodRepresentationMetadata> = Vec::new();
    let mut state = MetadataState::default();

    loop {
        let (namespace, event) = reader.read_resolved_event_into(&mut buffer).ok()?;
        match event {
            Event::Start(start) => push_element(
                &start,
                &namespace,
                &mut stack,
                &mut periods,
                max_representations,
                &mut state,
            )?,
            Event::Empty(start) => {
                push_element(
                    &start,
                    &namespace,
                    &mut stack,
                    &mut periods,
                    max_representations,
                    &mut state,
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

    Some(RepresentationMetadata {
        periods,
        has_unsupported_segment_list_timing_attributes: state
            .has_unsupported_segment_list_timing_attributes,
        has_nonzero_segment_list_presentation_time_offset: state
            .has_nonzero_segment_list_presentation_time_offset,
        has_invalid_segment_list_presentation_time_offset: state
            .has_invalid_segment_list_presentation_time_offset,
        has_nondefault_segment_list_start_number: state.has_nondefault_segment_list_start_number,
        has_invalid_segment_list_start_number: state.has_invalid_segment_list_start_number,
    })
}

fn push_element(
    start: &BytesStart<'_>,
    namespace: &ResolveResult<'_>,
    stack: &mut Vec<Element>,
    periods: &mut Vec<PeriodRepresentationMetadata>,
    max_representations: usize,
    state: &mut MetadataState,
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

    // Collect compatibility flags while the representation metadata pass is
    // already visiting each XML node. These checks deliberately cover every
    // DASH SegmentList, not only the locations modeled for inheritance below.
    if in_mpd_namespace && name == "SegmentList" {
        for attribute in start.attributes() {
            let attribute = attribute.ok()?;
            let key = attribute.key.as_ref();
            if key == "duration"
                || key == "availabilityTimeComplete"
                || key == "eptDelta"
                || key == "pbDelta"
            {
                state.has_unsupported_segment_list_timing_attributes = true;
            } else if key == "presentationTimeOffset" {
                let value = attribute.normalized_value(XmlVersion::Implicit1_0).ok()?;
                match value.parse::<u64>() {
                    Ok(offset) => {
                        state.has_nonzero_segment_list_presentation_time_offset |= offset != 0;
                    }
                    Err(_) => state.has_invalid_segment_list_presentation_time_offset = true,
                }
            } else if key == "startNumber" {
                let value = attribute.normalized_value(XmlVersion::Implicit1_0).ok()?;
                match value.parse::<u64>() {
                    Ok(number) => {
                        state.has_nondefault_segment_list_start_number |= number != 1;
                    }
                    Err(_) => state.has_invalid_segment_list_start_number = true,
                }
            }
        }
    }

    let element = match (stack.last().copied(), name, in_mpd_namespace) {
        (None, "MPD", true) => Element::Mpd,
        (Some(Element::Mpd), "Period", true) => {
            if periods.len() >= MAX_PERIODS {
                return None;
            }
            periods.push(Vec::new());
            state.period_list_pto = None;
            state.adaptation_list_pto = None;
            state.period_list_ato = None;
            state.adaptation_list_ato = None;
            Element::Period
        }
        (Some(Element::Period), "AdaptationSet", true) => {
            state.adaptation_count = state.adaptation_count.checked_add(1)?;
            if state.adaptation_count > max_representations {
                return None;
            }
            periods.last_mut()?.push(Vec::new());
            state.adaptation_list_pto = state.period_list_pto;
            state.adaptation_list_ato = state.period_list_ato.clone();
            Element::AdaptationSet
        }
        (Some(Element::AdaptationSet), "Representation", true) => {
            state.representation_count = state.representation_count.checked_add(1)?;
            if state.representation_count > max_representations {
                return None;
            }
            periods
                .last_mut()?
                .last_mut()?
                .push(RepresentationRoleMetadata {
                    id: representation_id(start)?,
                    is_main: false,
                    segment_list_presentation_time_offset: state.adaptation_list_pto,
                    segment_list_availability_time_offset: state.adaptation_list_ato.clone(),
                });
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
                state.period_list_pto = Some(offset);
            }
            if let Some(offset) = segment_list_availability_time_offset(start)? {
                state.period_list_ato = Some(offset);
            }
            Element::Other
        }
        (Some(Element::AdaptationSet), "SegmentList", true) => {
            if let Some(offset) = segment_list_presentation_time_offset(start)? {
                state.adaptation_list_pto = Some(offset);
            }
            if let Some(offset) = segment_list_availability_time_offset(start)? {
                state.adaptation_list_ato = Some(offset);
            }
            Element::Other
        }
        (Some(Element::Representation), "SegmentList", true) => {
            let metadata = periods.last_mut()?.last_mut()?.last_mut()?;
            if let Some(offset) = segment_list_presentation_time_offset(start)? {
                metadata.segment_list_presentation_time_offset = Some(offset);
            }
            if let Some(offset) = segment_list_availability_time_offset(start)? {
                metadata.segment_list_availability_time_offset = Some(offset);
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

fn segment_list_availability_time_offset(start: &BytesStart<'_>) -> Option<Option<String>> {
    let mut offset = None;
    for attribute in start.attributes() {
        let attribute = attribute.ok()?;
        if attribute.key.as_ref() == "availabilityTimeOffset" {
            let value = attribute.normalized_value(XmlVersion::Implicit1_0).ok()?;
            offset = Some(value.into_owned());
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
        assert_eq!(metadata.periods.len(), 1);
        assert_eq!(metadata.periods[0].len(), 2);
        assert_eq!(metadata.periods[0][0][0].id.as_deref(), Some("first"));
        assert!(!metadata.periods[0][0][0].is_main);
        assert_eq!(metadata.periods[0][0][1].id.as_deref(), Some("main"));
        assert!(metadata.periods[0][0][1].is_main);
        assert_eq!(metadata.periods[0][1][0].id.as_deref(), Some("other"));
        assert!(!metadata.periods[0][1][0].is_main);
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
        assert_eq!(metadata.periods[0][0][0].id.as_deref(), Some("main"));
        assert!(metadata.periods[0][0][0].is_main);
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
        assert_eq!(metadata.periods[0][0][0].id.as_deref(), Some("main"));
        assert!(metadata.periods[0][0][0].is_main);
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
        assert_eq!(metadata.periods[0][0][0].id.as_deref(), Some("main"));
        assert!(!metadata.periods[0][0][0].is_main);
    }

    #[test]
    fn detects_unsupported_segment_list_timing_attributes() {
        let supported = br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"><Period><AdaptationSet><SegmentList timescale="1000"/></AdaptationSet></Period></MPD>"#;
        for attribute in [
            r#"duration="2""#,
            r#"availabilityTimeComplete="false""#,
            r#"eptDelta="1""#,
            r#"pbDelta="1""#,
        ] {
            let body = format!(
                r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"><Period><AdaptationSet><SegmentList {attribute}/></AdaptationSet></Period></MPD>"#
            );
            assert_eq!(
                super::representation_metadata(body.as_bytes(), 1)
                    .map(|metadata| metadata.has_unsupported_segment_list_timing_attributes),
                Some(true),
                "{attribute}"
            );
        }
        assert_eq!(
            super::representation_metadata(supported, 1)
                .map(|metadata| metadata.has_unsupported_segment_list_timing_attributes),
            Some(false)
        );
        let finite_availability_offset = br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"><Period><AdaptationSet><SegmentList availabilityTimeOffset="1"/></AdaptationSet></Period></MPD>"#;
        assert_eq!(
            super::representation_metadata(finite_availability_offset, 1)
                .map(|metadata| metadata.has_unsupported_segment_list_timing_attributes),
            Some(false)
        );
    }

    #[test]
    fn detects_nondefault_segment_list_start_numbers() {
        let default_number = br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"><Period><AdaptationSet><SegmentList startNumber="1"/></AdaptationSet></Period></MPD>"#;
        let nondefault_number = br#"<mpd:MPD xmlns:mpd="urn:mpeg:dash:schema:mpd:2011"><mpd:Period><mpd:AdaptationSet><mpd:SegmentList startNumber="42"/></mpd:AdaptationSet></mpd:Period></mpd:MPD>"#;
        assert_eq!(
            super::representation_metadata(default_number, 1)
                .map(|metadata| metadata.has_nondefault_segment_list_start_number),
            Some(false)
        );
        assert_eq!(
            super::representation_metadata(nondefault_number, 1)
                .map(|metadata| metadata.has_nondefault_segment_list_start_number),
            Some(true)
        );
    }

    #[test]
    fn maps_inherited_segment_list_timing_offsets_to_representations() {
        let body = br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011">
  <Period>
    <SegmentList presentationTimeOffset="10" availabilityTimeOffset="1"/>
    <AdaptationSet>
      <SegmentList presentationTimeOffset="20" availabilityTimeOffset="2"/>
      <Representation id="inherited"/>
      <Representation id="overridden">
        <SegmentList presentationTimeOffset="30" availabilityTimeOffset="3"/>
      </Representation>
    </AdaptationSet>
    <AdaptationSet>
      <Representation id="period-inherited"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let metadata = representation_metadata(body, 3).expect("valid representation metadata");
        assert_eq!(
            metadata.periods[0][0][0].segment_list_presentation_time_offset,
            Some(20)
        );
        assert_eq!(
            metadata.periods[0][0][1].segment_list_presentation_time_offset,
            Some(30)
        );
        assert_eq!(
            metadata.periods[0][1][0].segment_list_presentation_time_offset,
            Some(10)
        );
        assert_eq!(
            metadata.periods[0][0][0]
                .segment_list_availability_time_offset
                .as_deref(),
            Some("2")
        );
        assert_eq!(
            metadata.periods[0][0][1]
                .segment_list_availability_time_offset
                .as_deref(),
            Some("3")
        );
        assert_eq!(
            metadata.periods[0][1][0]
                .segment_list_availability_time_offset
                .as_deref(),
            Some("1")
        );
    }

    #[test]
    fn detects_nonzero_segment_list_presentation_time_offsets() {
        let nonzero = br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"><Period><AdaptationSet><SegmentList presentationTimeOffset="9000"/></AdaptationSet></Period></MPD>"#;
        assert_eq!(
            super::representation_metadata(nonzero, 1)
                .map(|metadata| metadata.has_nonzero_segment_list_presentation_time_offset),
            Some(true)
        );

        let zero = br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"><Period><AdaptationSet><SegmentList presentationTimeOffset="0"/></AdaptationSet></Period></MPD>"#;
        assert_eq!(
            super::representation_metadata(zero, 1)
                .map(|metadata| metadata.has_nonzero_segment_list_presentation_time_offset),
            Some(false)
        );

        let foreign = br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" xmlns:foreign="urn:example:foreign"><Period><AdaptationSet><foreign:SegmentList presentationTimeOffset="9000"/></AdaptationSet></Period></MPD>"#;
        assert_eq!(
            super::representation_metadata(foreign, 1)
                .map(|metadata| metadata.has_nonzero_segment_list_presentation_time_offset),
            Some(false)
        );
    }

    #[test]
    fn records_invalid_dynamic_segment_list_numbers_without_rejecting_static_metadata() {
        let invalid_offsets = br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"><Period><AdaptationSet><Other><SegmentList presentationTimeOffset="invalid"/></Other></AdaptationSet></Period></MPD>"#;
        let invalid_start_number = br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"><Period><AdaptationSet><Other><SegmentList startNumber="invalid"/></Other></AdaptationSet></Period></MPD>"#;

        let offset_metadata =
            representation_metadata(invalid_offsets, 1).expect("bounded metadata scan");
        assert!(offset_metadata.has_invalid_segment_list_presentation_time_offset);

        let start_metadata =
            representation_metadata(invalid_start_number, 1).expect("bounded metadata scan");
        assert!(start_metadata.has_invalid_segment_list_start_number);
    }

    #[test]
    fn metadata_scan_rejects_excessive_xml_depth() {
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
            super::representation_metadata(nested.as_bytes(), 1)
                .map(|metadata| metadata.has_nonzero_segment_list_presentation_time_offset),
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
        assert_eq!(metadata.periods.len(), 2);
        assert_eq!(metadata.periods[0][0][0].id.as_deref(), Some("first"));
        assert_eq!(metadata.periods[1][0][0].id.as_deref(), Some("second"));
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
