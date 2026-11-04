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

#[derive(Clone, Copy)]
enum Element {
    Mpd,
    Period,
    AdaptationSet,
    Representation,
    Other,
}

/// Return representation-level Role=main flags in MPD source order.
///
/// The caller accepts only single-period presentations, so the result is
/// aligned with that Period's AdaptationSets and Representations.
pub(super) fn representation_main_roles(
    body: &[u8],
    max_representations: usize,
) -> Option<Vec<Vec<bool>>> {
    let mut reader = NsReader::from_reader(body);
    let mut buffer = Vec::new();
    let mut stack = Vec::new();
    let mut periods: Vec<Vec<Vec<bool>>> = Vec::new();
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
    periods: &mut Vec<Vec<Vec<bool>>>,
    max_representations: usize,
    representation_count: &mut usize,
    adaptation_count: &mut usize,
) -> Option<()> {
    if stack.len() >= MAX_ELEMENT_DEPTH {
        return None;
    }

    let name = start.name().local_name().as_ref();
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
            periods.last_mut()?.last_mut()?.push(false);
            Element::Representation
        }
        (Some(Element::Representation), "Role", true) => {
            if is_main_role(start)? {
                *periods.last_mut()?.last_mut()?.last_mut()? = true;
            }
            Element::Other
        }
        _ => Element::Other,
    };
    stack.push(element);
    Some(())
}

fn is_main_role(start: &BytesStart<'_>) -> Option<bool> {
    let mut scheme = None;
    let mut value = None;

    for attribute in start.attributes() {
        let attribute = attribute.ok()?;
        match attribute.key.local_name().as_ref() {
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
    use super::representation_main_roles;

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

        assert_eq!(
            representation_main_roles(body, 3),
            Some(vec![vec![false, true], vec![false]])
        );
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

        assert_eq!(representation_main_roles(body, 1), Some(vec![vec![true]]));
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

        assert_eq!(representation_main_roles(body, 1), Some(vec![vec![true]]));
    }

    #[test]
    fn rejects_malformed_or_unbounded_role_documents() {
        assert_eq!(representation_main_roles(b"<MPD>", 1), None);

        let too_many = br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"><Period><AdaptationSet>
  <Representation id="one"/>
  <Representation id="two"/>
</AdaptationSet></Period></MPD>"#;
        assert_eq!(representation_main_roles(too_many, 1), None);
    }

    #[test]
    fn rejects_excessive_xml_nesting() {
        let nested_tags = format!("{}{}", "<Other>".repeat(32), "</Other>".repeat(32));
        let nested = format!(
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"><Period><AdaptationSet><Representation>{}</Representation></AdaptationSet></Period></MPD>"#,
            nested_tags
        );
        assert_eq!(representation_main_roles(nested.as_bytes(), 1), None);
    }
}
