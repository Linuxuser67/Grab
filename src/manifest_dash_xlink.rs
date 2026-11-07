//! Bounded DASH XLink discovery and XML element replacement.
//!
//! Network access and host policy remain in manifest intake. This module only
//! validates XML structure and identifies exact source spans to replace.

use std::io::BufRead;
use std::ops::Range;

use quick_xml::XmlVersion;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;
use thiserror::Error;

const DASH_MPD_NAMESPACE: &str = "urn:mpeg:dash:schema:mpd:2011";
const XLINK_NAMESPACE: &str = "http://www.w3.org/1999/xlink";
const MAX_XML_DEPTH: usize = 32;
const MAX_XML_ELEMENTS: usize = 20_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DashXlinkElement {
    Period,
    AdaptationSet,
    Representation,
}

impl DashXlinkElement {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "Period" => Some(Self::Period),
            "AdaptationSet" => Some(Self::AdaptationSet),
            "Representation" => Some(Self::Representation),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DashXlinkAction {
    Remove,
    Fetch(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DashXlinkReference {
    pub(crate) range: Range<usize>,
    pub(crate) element: DashXlinkElement,
    pub(crate) action: DashXlinkAction,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum DashXlinkError {
    #[error("DASH XLink XML is malformed or exceeds its structural limits")]
    InvalidDocument,
    #[error("DASH XLink uses an unsupported element or actuation mode")]
    UnsupportedLink,
    #[error("DASH XLink replacements overlap")]
    OverlappingLinks,
    #[error("DASH XLink expansion exceeds the manifest size limit")]
    SizeLimit,
}

struct ElementFrame {
    start: usize,
    link: Option<(DashXlinkElement, DashXlinkAction)>,
}

struct DocumentAnalysis {
    root_name: String,
    root_is_dash: bool,
    root_range: Range<usize>,
    links: Vec<DashXlinkReference>,
}

struct LinkAttributes {
    href: Option<String>,
    actuate: Option<String>,
}

pub(crate) fn find_onload_links(body: &[u8]) -> Result<Vec<DashXlinkReference>, DashXlinkError> {
    let analysis = analyze_document(body, true)?;
    if analysis.root_name.as_str() != "MPD" || !analysis.root_is_dash {
        return Err(DashXlinkError::InvalidDocument);
    }
    Ok(analysis.links)
}

/// Validate that a fetched XLink entity is exactly one matching DASH element
/// and contains no further XLink links. Recursive resolution is deliberately
/// outside the supported profile.
pub(crate) fn linked_element(
    body: &[u8],
    expected: DashXlinkElement,
) -> Result<&[u8], DashXlinkError> {
    let analysis = analyze_document(body, false)?;
    let expected_name = match expected {
        DashXlinkElement::Period => "Period",
        DashXlinkElement::AdaptationSet => "AdaptationSet",
        DashXlinkElement::Representation => "Representation",
    };
    if !analysis.root_is_dash || analysis.root_name != expected_name || !analysis.links.is_empty() {
        return Err(DashXlinkError::UnsupportedLink);
    }
    body.get(analysis.root_range)
        .ok_or(DashXlinkError::InvalidDocument)
}

pub(crate) fn replace_linked_elements(
    body: &[u8],
    links: &[DashXlinkReference],
    replacements: &[Vec<u8>],
    max_output_bytes: usize,
) -> Result<Vec<u8>, DashXlinkError> {
    if links.len() != replacements.len() {
        return Err(DashXlinkError::InvalidDocument);
    }

    let mut output_length = body.len();
    let mut previous_end = 0_usize;
    for (link, replacement) in links.iter().zip(replacements) {
        if link.range.start < previous_end
            || link.range.end < link.range.start
            || link.range.end > body.len()
        {
            return Err(DashXlinkError::OverlappingLinks);
        }
        output_length = output_length
            .checked_sub(link.range.end - link.range.start)
            .and_then(|length| length.checked_add(replacement.len()))
            .filter(|length| *length <= max_output_bytes)
            .ok_or(DashXlinkError::SizeLimit)?;
        previous_end = link.range.end;
    }

    let mut output = Vec::with_capacity(output_length);
    let mut cursor = 0_usize;
    for (link, replacement) in links.iter().zip(replacements) {
        output.extend_from_slice(&body[cursor..link.range.start]);
        output.extend_from_slice(replacement);
        cursor = link.range.end;
    }
    output.extend_from_slice(&body[cursor..]);
    Ok(output)
}

fn analyze_document(body: &[u8], require_onload: bool) -> Result<DocumentAnalysis, DashXlinkError> {
    if body.is_empty() {
        return Err(DashXlinkError::InvalidDocument);
    }

    let mut reader = NsReader::from_reader(body);
    let mut buffer = Vec::new();
    let mut stack: Vec<ElementFrame> = Vec::new();
    let mut links = Vec::new();
    let mut root_name = None;
    let mut root_is_dash = false;
    let mut root_range = None;
    let mut root_count = 0_usize;
    let mut element_count = 0_usize;
    let mut previous_event_end = 0_usize;

    loop {
        let (namespace, event) = reader
            .read_resolved_event_into(&mut buffer)
            .map_err(|_| DashXlinkError::InvalidDocument)?;
        let is_dash_namespace = matches!(
            &namespace,
            ResolveResult::Bound(namespace)
                if namespace.into_inner() == DASH_MPD_NAMESPACE
        );
        drop(namespace);
        let event_end = usize::try_from(reader.buffer_position())
            .map_err(|_| DashXlinkError::InvalidDocument)?;
        match event {
            Event::Start(start) => {
                element_count = element_count
                    .checked_add(1)
                    .filter(|count| *count <= MAX_XML_ELEMENTS)
                    .ok_or(DashXlinkError::InvalidDocument)?;
                if stack.len() >= MAX_XML_DEPTH {
                    return Err(DashXlinkError::InvalidDocument);
                }
                let end = event_end;
                let start_offset = body
                    .get(previous_event_end..end)
                    .and_then(|event| event.iter().position(|byte| *byte == b'<'))
                    .and_then(|offset| previous_event_end.checked_add(offset))
                    .ok_or(DashXlinkError::InvalidDocument)?;
                let local_name = start.name().local_name();
                let is_dash = is_dash_namespace;

                if stack.is_empty() {
                    root_count = root_count
                        .checked_add(1)
                        .ok_or(DashXlinkError::InvalidDocument)?;
                    if root_count != 1 {
                        return Err(DashXlinkError::InvalidDocument);
                    }
                    root_name = Some(local_name.as_ref().to_string());
                    root_is_dash = is_dash;
                }

                let attributes = xlink_attributes(&reader, &start)?;
                let link = link_action(is_dash, local_name.as_ref(), attributes, require_onload)?;
                stack.push(ElementFrame {
                    start: start_offset,
                    link,
                });
            }
            Event::Empty(start) => {
                element_count = element_count
                    .checked_add(1)
                    .filter(|count| *count <= MAX_XML_ELEMENTS)
                    .ok_or(DashXlinkError::InvalidDocument)?;
                let end = event_end;
                let start_offset = body
                    .get(previous_event_end..end)
                    .and_then(|event| event.iter().position(|byte| *byte == b'<'))
                    .and_then(|offset| previous_event_end.checked_add(offset))
                    .ok_or(DashXlinkError::InvalidDocument)?;
                let local_name = start.name().local_name();
                let is_dash = is_dash_namespace;

                if stack.is_empty() {
                    root_count = root_count
                        .checked_add(1)
                        .ok_or(DashXlinkError::InvalidDocument)?;
                    if root_count != 1 {
                        return Err(DashXlinkError::InvalidDocument);
                    }
                    root_name = Some(local_name.as_ref().to_string());
                    root_is_dash = is_dash;
                    root_range = Some(start_offset..end);
                }

                let attributes = xlink_attributes(&reader, &start)?;
                if let Some((element, action)) =
                    link_action(is_dash, local_name.as_ref(), attributes, require_onload)?
                {
                    links.push(DashXlinkReference {
                        range: start_offset..end,
                        element,
                        action,
                    });
                }
            }
            Event::End(_) => {
                let end = event_end;
                let frame = stack.pop().ok_or(DashXlinkError::InvalidDocument)?;
                if stack.is_empty() {
                    root_range = Some(frame.start..end);
                }
                if let Some((element, action)) = frame.link {
                    links.push(DashXlinkReference {
                        range: frame.start..end,
                        element,
                        action,
                    });
                }
            }
            Event::Text(text) if stack.is_empty() => {
                if !text.as_ref().chars().all(char::is_whitespace) {
                    return Err(DashXlinkError::InvalidDocument);
                }
            }
            Event::DocType(_) => return Err(DashXlinkError::InvalidDocument),
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
        previous_event_end = event_end;
    }

    if !stack.is_empty() || root_count != 1 {
        return Err(DashXlinkError::InvalidDocument);
    }

    links.sort_unstable_by_key(|link| link.range.start);
    let mut previous_end = 0_usize;
    for link in &links {
        if link.range.start < previous_end || link.range.end > body.len() {
            return Err(DashXlinkError::OverlappingLinks);
        }
        previous_end = link.range.end;
    }

    Ok(DocumentAnalysis {
        root_name: root_name.ok_or(DashXlinkError::InvalidDocument)?,
        root_is_dash,
        root_range: root_range.ok_or(DashXlinkError::InvalidDocument)?,
        links,
    })
}

fn xlink_attributes<R: BufRead>(
    reader: &NsReader<R>,
    start: &BytesStart<'_>,
) -> Result<LinkAttributes, DashXlinkError> {
    let mut href = None;
    let mut actuate = None;

    for attribute in start.attributes() {
        let attribute = attribute.map_err(|_| DashXlinkError::InvalidDocument)?;
        let (namespace, local_name) = reader.resolver().resolve_attribute(attribute.key);
        if matches!(
            namespace,
            ResolveResult::Bound(namespace) if namespace.into_inner() == XLINK_NAMESPACE
        ) {
            let value = attribute
                .normalized_value(XmlVersion::Implicit1_0)
                .map_err(|_| DashXlinkError::InvalidDocument)?
                .into_owned();
            match local_name.as_ref() {
                "href" => {
                    if href.is_some() {
                        return Err(DashXlinkError::UnsupportedLink);
                    }
                    href = Some(value);
                }
                "actuate" => {
                    if actuate.is_some() {
                        return Err(DashXlinkError::UnsupportedLink);
                    }
                    actuate = Some(value);
                }
                _ => {}
            }
        }
    }

    Ok(LinkAttributes { href, actuate })
}

fn link_action(
    is_dash: bool,
    local_name: &str,
    attributes: LinkAttributes,
    require_onload: bool,
) -> Result<Option<(DashXlinkElement, DashXlinkAction)>, DashXlinkError> {
    let Some(href) = attributes.href else {
        return Ok(None);
    };
    let element = is_dash
        .then(|| DashXlinkElement::from_name(local_name))
        .flatten()
        .ok_or(DashXlinkError::UnsupportedLink)?;
    let href = href.trim();
    if href.is_empty() {
        return Err(DashXlinkError::UnsupportedLink);
    }
    if require_onload && attributes.actuate.as_deref() != Some("onLoad") {
        // XLink's default actuation is onRequest; Grab resolves only onLoad.
        return Err(DashXlinkError::UnsupportedLink);
    }
    let action = if href == "urn:mpeg:dash:resolve-to-zero:2013" {
        DashXlinkAction::Remove
    } else {
        DashXlinkAction::Fetch(href.to_string())
    };
    Ok(Some((element, action)))
}

#[cfg(test)]
mod tests {
    use super::{
        DashXlinkAction, DashXlinkElement, find_onload_links, linked_element,
        replace_linked_elements,
    };

    const MPD: &str = "urn:mpeg:dash:schema:mpd:2011";
    const XLINK: &str = "http://www.w3.org/1999/xlink";

    #[test]
    fn discovers_and_replaces_an_onload_period_link() {
        let body = format!(
            r#"<MPD xmlns="{MPD}" xmlns:xlink="{XLINK}"><Period xlink:href="period.xml" xlink:actuate="onLoad"/></MPD>"#
        );
        let links = find_onload_links(body.as_bytes()).expect("valid onLoad XLink");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].element, DashXlinkElement::Period);
        assert_eq!(
            links[0].action,
            DashXlinkAction::Fetch("period.xml".to_string())
        );
        let replacement = format!(r#"<Period xmlns="{MPD}" duration="PT2S"/>"#).into_bytes();
        let expanded = replace_linked_elements(body.as_bytes(), &links, &[replacement], 4096)
            .expect("replacement should fit");
        assert!(
            std::str::from_utf8(&expanded)
                .expect("expanded MPD is UTF-8")
                .contains(r#"duration="PT2S""#)
        );
    }

    #[test]
    fn resolve_to_zero_removes_the_linked_element() {
        let body = format!(
            r#"<MPD xmlns="{MPD}" xmlns:xlink="{XLINK}"><Period id="empty" xlink:href="urn:mpeg:dash:resolve-to-zero:2013" xlink:actuate="onLoad"/><Period id="keep"/></MPD>"#
        );
        let links = find_onload_links(body.as_bytes()).expect("resolve-to-zero XLink");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].action, DashXlinkAction::Remove);
        let expanded = replace_linked_elements(body.as_bytes(), &links, &[Vec::new()], 4096)
            .expect("empty replacement should succeed");
        let expanded = std::str::from_utf8(&expanded).expect("UTF-8 output");
        assert!(!expanded.contains("empty"));
        assert!(expanded.contains("keep"));
    }

    #[test]
    fn discovers_supported_xlink_element_kinds() {
        let bodies = [
            (
                format!(
                    r#"<MPD xmlns="{MPD}" xmlns:xlink="{XLINK}"><Period xlink:href="linked.xml" xlink:actuate="onLoad"/></MPD>"#
                ),
                DashXlinkElement::Period,
            ),
            (
                format!(
                    r#"<MPD xmlns="{MPD}" xmlns:xlink="{XLINK}"><Period><AdaptationSet xlink:href="linked.xml" xlink:actuate="onLoad"/></Period></MPD>"#
                ),
                DashXlinkElement::AdaptationSet,
            ),
            (
                format!(
                    r#"<MPD xmlns="{MPD}" xmlns:xlink="{XLINK}"><Period><AdaptationSet><Representation xlink:href="linked.xml" xlink:actuate="onLoad"/></AdaptationSet></Period></MPD>"#
                ),
                DashXlinkElement::Representation,
            ),
        ];
        for (body, expected) in bodies {
            let links = find_onload_links(body.as_bytes()).expect("supported XLink element");
            assert_eq!(links.len(), 1);
            assert_eq!(links[0].element, expected);
        }
    }

    #[test]
    fn rejects_onrequest_and_missing_actuation() {
        for actuate in [r#"xlink:actuate="onRequest""#, ""] {
            let body = format!(
                r#"<MPD xmlns="{MPD}" xmlns:xlink="{XLINK}"><Period xlink:href="period.xml" {actuate}/></MPD>"#
            );
            assert!(find_onload_links(body.as_bytes()).is_err());
        }
    }

    #[test]
    fn rejects_xlinks_on_unsupported_mpd_elements() {
        let body = format!(
            r#"<MPD xmlns="{MPD}" xmlns:xlink="{XLINK}"><BaseURL xlink:href="base.xml" xlink:actuate="onLoad"/></MPD>"#
        );
        assert!(find_onload_links(body.as_bytes()).is_err());
    }

    #[test]
    fn rejects_nested_or_recursive_linked_elements() {
        let body = format!(
            r#"<MPD xmlns="{MPD}" xmlns:xlink="{XLINK}"><Period xlink:href="period.xml" xlink:actuate="onLoad"><AdaptationSet xlink:href="set.xml" xlink:actuate="onLoad"/></Period></MPD>"#
        );
        assert!(find_onload_links(body.as_bytes()).is_err());

        let nested = format!(
            r#"<Period xmlns="{MPD}" xmlns:xlink="{XLINK}"><AdaptationSet xlink:href="set.xml" xlink:actuate="onLoad"/></Period>"#
        );
        assert!(linked_element(nested.as_bytes(), DashXlinkElement::Period).is_err());
    }

    #[test]
    fn accepts_only_one_matching_remote_dash_element() {
        let body = format!(r#"<Period xmlns="{MPD}"/>"#);
        assert!(linked_element(body.as_bytes(), DashXlinkElement::Period).is_ok());
        assert!(linked_element(body.as_bytes(), DashXlinkElement::AdaptationSet).is_err());
        let wrapped = format!(r#"<MPD xmlns="{MPD}"><Period/></MPD>"#);
        assert!(linked_element(wrapped.as_bytes(), DashXlinkElement::Period).is_err());
    }

    #[test]
    fn rejects_malformed_and_excessively_deep_xml() {
        assert!(find_onload_links(b"<MPD>").is_err());
        let body = format!(
            r#"<MPD xmlns="{MPD}"><Period>{}{} </Period></MPD>"#,
            "<Other>".repeat(32),
            "</Other>".repeat(32)
        );
        assert!(find_onload_links(body.as_bytes()).is_err());
    }

    #[test]
    fn rejects_overlapping_replacement_ranges() {
        let body = format!(
            r#"<MPD xmlns="{MPD}" xmlns:xlink="{XLINK}"><Period xlink:href="period.xml" xlink:actuate="onLoad"><AdaptationSet xlink:href="set.xml" xlink:actuate="onLoad"/></Period></MPD>"#
        );
        assert!(find_onload_links(body.as_bytes()).is_err());
    }
}
