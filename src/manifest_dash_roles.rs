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
/// Role scheme used to identify the main audio or subtitle representation.
pub(super) const DASH_ROLE_SCHEME: &str = "urn:mpeg:dash:role:2011";

/// Representation identity and its direct-child `Role=main` flag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RepresentationRoleMetadata {
    pub(super) id: Option<String>,
    pub(super) is_main: bool,
}

#[derive(Clone, Copy)]
enum Element {
    Mpd,
    Period,
    AdaptationSet,
    Representation,
    Other,
}

/// Return representation IDs and Role=main flags in MPD source order.
///
/// IDs allow the caller to verify that this pass and the typed MPD parser
/// identify the same representation at each source-order position.
pub(super) fn representation_metadata(
    body: &[u8],
    max_representations: usize,
) -> Option<Vec<Vec<RepresentationRoleMetadata>>> {
    let mut reader = NsReader::from_reader(body);
    let mut buffer = Vec::new();
    let mut stack = Vec::new();
    let mut periods: Vec<Vec<Vec<RepresentationRoleMetadata>>> = Vec::new();
    let mut representation_count = 0_usize;
    let mut adaptation_count = 0_usize;

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

    if !stack.is_empty() || periods.len() != 1 {
        return None;
    }

    periods.pop()
}

fn push_element(
    start: &BytesStart<'_>,
    namespace: &ResolveResult<'_>,
    stack: &mut Vec<Element>,
    periods: &mut Vec<Vec<Vec<RepresentationRoleMetadata>>>,
    max_representations: usize,
    representation_count: &mut usize,
    adaptation_count: &mut usize,
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
            if !periods.is_empty() {
                return None;
            }
            periods.push(Vec::new());
            Element::Period
        }
        (Some(Element::Period), "AdaptationSet", true) => {
            *adaptation_count = (*adaptation_count).checked_add(1)?;
            if *adaptation_count > max_representations {
                return None;
            }
            periods.last_mut()?.push(Vec::new());
            Element::AdaptationSet
        }
        (Some(Element::AdaptationSet), "Representation", true) => {
            *representation_count = (*representation_count).checked_add(1)?;
            if *representation_count > max_representations {
                return None;
            }
            periods.last_mut()?.last_mut()?.push(RepresentationRoleMetadata {
                id: representation_id(start)?,
                is_main: false,
            });
            Element::Representation
        }
        (Some(Element::Representation), "Role", true) => {
            if is_main_role(start)? {
                periods.last_mut()?.last_mut()?.last_mut()?.is_main = true;
            }
            Element::Other
        }
        _ => Element::Other,
    };
    stack.push(element);
    Some(())
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

        let metadata =
            representation_metadata(body, 3).expect("valid MPD representation metadata");
        assert_eq!(metadata.len(), 2);
        assert_eq!(metadata[0][0].id.as_deref(), Some("first"));
        assert!(!metadata[0][0].is_main);
        assert_eq!(metadata[0][1].id.as_deref(), Some("main"));
        assert!(metadata[0][1].is_main);
        assert_eq!(metadata[1][0].id.as_deref(), Some("other"));
        assert!(!metadata[1][0].is_main);
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

        let metadata =
            representation_metadata(body, 1).expect("valid MPD representation metadata");
        assert_eq!(metadata[0][0].id.as_deref(), Some("main"));
        assert!(metadata[0][0].is_main);
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

        let metadata =
            representation_metadata(body, 1).expect("valid MPD representation metadata");
        assert_eq!(metadata[0][0].id.as_deref(), Some("main"));
        assert!(metadata[0][0].is_main);
    }

    #[test]
    fn ignores_namespaced_role_attributes() {
        let body = br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" xmlns:foreign="urn:example:foreign">
  <Period>
    <AdaptationSet>
      <Representation id="main">
        <Role foreign:schemeIdUri="urn:mpeg:dash:role:2011" foreign:value="main"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

        let metadata = representation_metadata(body, 1)
            .expect("foreign attributes do not invalidate the MPD");
        assert_eq!(metadata[0][0].id.as_deref(), Some("main"));
        assert!(!metadata[0][0].is_main);
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
    fn rejects_excessive_xml_nesting() {
        let nested_tags = format!("{}{}", "<Other>".repeat(32), "</Other>".repeat(32));
        let nested = format!(
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"><Period><AdaptationSet><Representation>{}</Representation></AdaptationSet></Period></MPD>"#,
            nested_tags
        );
        assert_eq!(representation_metadata(nested.as_bytes(), 1), None);
    }
}
