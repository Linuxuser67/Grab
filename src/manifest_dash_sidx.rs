//! Bounded parsing and expansion of hierarchical ISO BMFF Segment Index boxes.

use std::io::Cursor;

use url::Url;

use super::{
    DashError, DashResource, DashResourceKind, DashSegmentBaseIndex, MAX_SEGMENTS,
    ManifestByteRange, ranges_overlap,
};

const MAX_SEGMENT_INDEX_BYTES: u64 = crate::manifest_types::MAX_DASH_SIDX_BYTES;
pub(crate) const MAX_DASH_SIDX_SEGMENTS: usize = MAX_SEGMENTS;

/// One ordered entry from a parsed Segment Index box.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DashSidxEntry {
    Media(ManifestByteRange),
    Index(DashSidxChildIndex),
}

/// A child index reference with the exact parent timing interval it must describe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DashSidxChildIndex {
    pub(crate) range: ManifestByteRange,
    expected_reference_id: u32,
    parent_timescale: u32,
    expected_earliest_presentation_time: u64,
    expected_duration: u32,
}

/// Parsed entries from one Segment Index box.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DashSidxNode {
    pub(crate) entries: Vec<DashSidxEntry>,
}

#[derive(Clone, Copy)]
struct SidxExpectation {
    reference_id: u32,
    parent_timescale: u32,
    earliest_presentation_time: u64,
    duration: u32,
}

/// Parse the root SIDX referenced by a static SegmentBase plan.
pub(crate) fn parse_segment_base_index(
    index: &DashSegmentBaseIndex,
    bytes: &[u8],
    index_resource_length: u64,
    media_resource_length: u64,
) -> Result<DashSidxNode, DashError> {
    let index_end = checked_range_end(index.index_range)?;
    if index.index_range.length == 0
        || index.index_range.length > MAX_SEGMENT_INDEX_BYTES
        || u64::try_from(bytes.len()).ok() != Some(index.index_range.length)
        || index_end > index_resource_length
    {
        return Err(DashError::InvalidSegment);
    }
    if let Some(initialization_range) = index.initialization_range {
        let initialization_end = checked_range_end(initialization_range)?;
        if initialization_range.length == 0
            || initialization_range.length > crate::manifest_writer::MAX_RESOURCE_BYTES
            || (!index.initialization_is_external && initialization_end > media_resource_length)
            || (!index.initialization_is_external
                && !index.index_is_external
                && ranges_overlap(index.index_range, initialization_range)?)
        {
            return Err(DashError::InvalidSegment);
        }
    }

    parse_sidx_node(SidxNodeContext {
        index_range: index.index_range,
        bytes,
        index_resource_length,
        media_resource_length,
        initialization_range: index.initialization_range,
        initialization_is_external: index.initialization_is_external,
        manifest_timescale: index.timescale,
        index_is_external: index.index_is_external,
        expectation: None,
        index_range_exact: index.index_range_exact,
        // indexRangeExact describes the representation's byte prefix, which
        // may contain boxes before the SIDX. It does not require SIDX alone.
        require_exact_box: false,
    })
}

/// Parse a child index and verify that it describes exactly its parent's time interval.
pub(crate) fn parse_child_segment_index(
    index: &DashSegmentBaseIndex,
    child: &DashSidxChildIndex,
    bytes: &[u8],
    total_length: u64,
) -> Result<DashSidxNode, DashError> {
    let child_end = checked_range_end(child.range)?;
    if child.range.length == 0
        || child.range.length > MAX_SEGMENT_INDEX_BYTES
        || u64::try_from(bytes.len()).ok() != Some(child.range.length)
        || child_end > total_length
    {
        return Err(DashError::InvalidSegment);
    }
    if !index.initialization_is_external
        && let Some(initialization_range) = index.initialization_range
        && ranges_overlap(child.range, initialization_range)?
    {
        return Err(DashError::InvalidSegment);
    }

    let expectation = SidxExpectation {
        reference_id: child.expected_reference_id,
        parent_timescale: child.parent_timescale,
        earliest_presentation_time: child.expected_earliest_presentation_time,
        duration: child.expected_duration,
    };
    parse_sidx_node(SidxNodeContext {
        index_range: child.range,
        bytes,
        index_resource_length: total_length,
        media_resource_length: total_length,
        initialization_range: index.initialization_range,
        initialization_is_external: index.initialization_is_external,
        manifest_timescale: None,
        index_is_external: false,
        expectation: Some(expectation),
        index_range_exact: false,
        require_exact_box: true,
    })
}

/// Ensure all expanded media and child-index ranges are disjoint from each other and init data.
pub(crate) fn validate_segment_base_sidx_ranges(
    index: &DashSegmentBaseIndex,
    child_indexes: &[ManifestByteRange],
    media: &[ManifestByteRange],
    total_length: u64,
) -> Result<(), DashError> {
    let mut ranges = Vec::with_capacity(child_indexes.len() + media.len() + 2);
    if !index.initialization_is_external
        && let Some(initialization_range) = index.initialization_range
    {
        ranges.push(initialization_range);
    }
    if !index.index_is_external {
        ranges.push(index.index_range);
    }
    ranges.extend_from_slice(child_indexes);
    ranges.extend_from_slice(media);

    for range in &ranges {
        if range.length == 0 || checked_range_end(*range)? > total_length {
            return Err(DashError::InvalidSegment);
        }
    }
    ranges.sort_unstable_by_key(|range| range.offset);

    let mut previous_end = 0;
    let mut has_previous = false;
    for range in ranges {
        if has_previous && range.offset < previous_end {
            return Err(DashError::InvalidSegment);
        }
        previous_end = checked_range_end(range)?;
        has_previous = true;
    }
    Ok(())
}

/// Create the ordinary journaled media resource for one resolved SIDX leaf.
pub(crate) fn dash_sidx_media_resource(
    url: &Url,
    range: ManifestByteRange,
    number: usize,
) -> Result<DashResource, DashError> {
    let segment_number = u64::try_from(number).map_err(|_| DashError::LimitExceeded)?;
    let filename = super::super::resource_filename("dash-segment", Some(number), url, Some(range));
    Ok(DashResource {
        url: url.clone(),
        range: Some(range),
        filename,
        kind: DashResourceKind::Segment,
        segment_number: Some(segment_number),
    })
}

struct SidxNodeContext<'a> {
    index_range: ManifestByteRange,
    bytes: &'a [u8],
    index_resource_length: u64,
    media_resource_length: u64,
    initialization_range: Option<ManifestByteRange>,
    initialization_is_external: bool,
    manifest_timescale: Option<u64>,
    index_is_external: bool,
    expectation: Option<SidxExpectation>,
    index_range_exact: bool,
    require_exact_box: bool,
}

fn parse_sidx_node(context: SidxNodeContext<'_>) -> Result<DashSidxNode, DashError> {
    use mp4box::StructuredData;

    let SidxNodeContext {
        index_range,
        bytes,
        index_resource_length,
        media_resource_length,
        initialization_range,
        initialization_is_external,
        manifest_timescale,
        index_is_external,
        expectation,
        index_range_exact,
        require_exact_box,
    } = context;

    let index_end = checked_range_end(index_range)?;
    if index_range.length == 0
        || index_range.length > MAX_SEGMENT_INDEX_BYTES
        || u64::try_from(bytes.len()).ok() != Some(index_range.length)
        || index_end > index_resource_length
    {
        return Err(DashError::InvalidSegment);
    }

    let mut reader = Cursor::new(bytes);
    let boxes =
        mp4box::get_boxes(&mut reader, index_range.length, true).map_err(|_| DashError::Parse)?;
    let mut sidx = None;
    for box_ref in &boxes {
        if box_ref.typ == "sidx" && sidx.replace(box_ref).is_some() {
            return Err(DashError::UnsupportedAddressing);
        }
    }
    let sidx = sidx.ok_or(DashError::UnsupportedAddressing)?;
    if require_exact_box
        && (boxes.len() != 1 || sidx.offset != 0 || sidx.size != index_range.length)
    {
        // The supported child-reference profile is one complete SIDX box per reference.
        // References that bundle sibling indexes or other boxes remain on yt-dlp.
        return Err(DashError::UnsupportedAddressing);
    }

    let sidx_end = sidx
        .offset
        .checked_add(sidx.size)
        .filter(|end| *end <= index_range.length)
        .ok_or(DashError::InvalidSegment)?;
    let Some(StructuredData::SegmentIndex(data)) = sidx.structured_data.as_ref() else {
        return Err(DashError::Parse);
    };
    if data.version > 1
        || data.flags != 0
        || data.timescale == 0
        || manifest_timescale.is_some_and(|timescale| timescale != u64::from(data.timescale))
        || data.references.is_empty()
        || data.references.len() > MAX_SEGMENTS
    {
        return Err(DashError::UnsupportedAddressing);
    }

    if let Some(expected) = expectation
        && (data.reference_id != expected.reference_id
            || !presentation_time_matches(
                data.earliest_presentation_time,
                data.timescale,
                expected.earliest_presentation_time,
                expected.parent_timescale,
            )
            || !duration_matches(
                &data.references,
                data.timescale,
                expected.duration,
                expected.parent_timescale,
            ))
    {
        return Err(DashError::UnsupportedAddressing);
    }

    let mut reference_offset = if index_is_external {
        // An external RepresentationIndex describes byte ranges in the media
        // resource, so first_offset is anchored to that resource's start.
        data.first_offset
    } else {
        index_range
            .offset
            .checked_add(sidx_end)
            .and_then(|offset| offset.checked_add(data.first_offset))
            .ok_or(DashError::InvalidSegment)?
    };
    if index_range_exact && !index_is_external && reference_offset < index_end {
        return Err(DashError::UnsupportedAddressing);
    }

    let mut presentation_time = data.earliest_presentation_time;
    let mut entries = Vec::with_capacity(data.references.len());
    for reference in &data.references {
        if reference.referenced_size == 0 || reference.subsegment_duration == 0 {
            return Err(DashError::UnsupportedAddressing);
        }
        let length = u64::from(reference.referenced_size);
        let end = reference_offset
            .checked_add(length)
            .filter(|end| *end <= media_resource_length)
            .ok_or(DashError::InvalidSegment)?;
        let range = ManifestByteRange {
            offset: reference_offset,
            length,
        };
        if !initialization_is_external
            && let Some(initialization) = initialization_range
            && ranges_overlap(range, initialization)?
        {
            return Err(DashError::InvalidSegment);
        }

        match reference.reference_type {
            0 => {
                if length > crate::manifest_writer::MAX_RESOURCE_BYTES {
                    return Err(DashError::LimitExceeded);
                }
                entries.push(DashSidxEntry::Media(range));
            }
            1 => {
                if length > MAX_SEGMENT_INDEX_BYTES {
                    return Err(DashError::LimitExceeded);
                }
                entries.push(DashSidxEntry::Index(DashSidxChildIndex {
                    range,
                    expected_reference_id: data.reference_id,
                    parent_timescale: data.timescale,
                    expected_earliest_presentation_time: presentation_time,
                    expected_duration: reference.subsegment_duration,
                }));
            }
            _ => return Err(DashError::UnsupportedAddressing),
        }

        presentation_time = presentation_time
            .checked_add(u64::from(reference.subsegment_duration))
            .ok_or(DashError::InvalidSegment)?;
        reference_offset = end;
    }

    Ok(DashSidxNode { entries })
}

fn presentation_time_matches(
    actual_time: u64,
    actual_timescale: u32,
    expected_time: u64,
    expected_timescale: u32,
) -> bool {
    u128::from(actual_time) * u128::from(expected_timescale)
        == u128::from(expected_time) * u128::from(actual_timescale)
}

fn duration_matches(
    references: &[mp4box::registry::SidxReference],
    actual_timescale: u32,
    expected_duration: u32,
    expected_timescale: u32,
) -> bool {
    let actual_duration = references.iter().fold(0_u128, |sum, reference| {
        sum + u128::from(reference.subsegment_duration)
    });
    actual_duration * u128::from(expected_timescale)
        == u128::from(expected_duration) * u128::from(actual_timescale)
}

fn checked_range_end(range: ManifestByteRange) -> Result<u64, DashError> {
    range
        .offset
        .checked_add(range.length)
        .ok_or(DashError::InvalidSegment)
}

#[cfg(test)]
pub(crate) fn resolve_segment_base_index(
    index: &DashSegmentBaseIndex,
    bytes: &[u8],
    total_length: u64,
) -> Result<Vec<DashResource>, DashError> {
    let node = parse_segment_base_index(index, bytes, total_length, total_length)?;
    let mut resources = Vec::with_capacity(node.entries.len());
    for entry in node.entries {
        let DashSidxEntry::Media(range) = entry else {
            return Err(DashError::UnsupportedAddressing);
        };
        resources.push(dash_sidx_media_resource(
            &index.url,
            range,
            resources.len(),
        )?);
    }
    Ok(resources)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_sidx_box(
        reference_id: u32,
        timescale: u32,
        earliest_presentation_time: u32,
        first_offset: u32,
        references: &[(u32, u32, u32)],
    ) -> Vec<u8> {
        let size = 32_u32 + u32::try_from(references.len()).expect("test reference count") * 12;
        let mut bytes = Vec::with_capacity(usize::try_from(size).expect("test index size"));
        bytes.extend_from_slice(&size.to_be_bytes());
        bytes.extend_from_slice(b"sidx");
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes.extend_from_slice(&reference_id.to_be_bytes());
        bytes.extend_from_slice(&timescale.to_be_bytes());
        bytes.extend_from_slice(&earliest_presentation_time.to_be_bytes());
        bytes.extend_from_slice(&first_offset.to_be_bytes());
        bytes.extend_from_slice(&0_u16.to_be_bytes());
        bytes.extend_from_slice(
            &u16::try_from(references.len())
                .expect("test reference count")
                .to_be_bytes(),
        );
        for (reference, duration, sap) in references {
            bytes.extend_from_slice(&reference.to_be_bytes());
            bytes.extend_from_slice(&duration.to_be_bytes());
            bytes.extend_from_slice(&sap.to_be_bytes());
        }
        bytes
    }

    fn test_index(index_bytes: &[u8]) -> DashSegmentBaseIndex {
        DashSegmentBaseIndex {
            url: Url::parse("https://cdn.example.test/path/media.mp4").expect("test media URL"),
            index_url: Url::parse("https://cdn.example.test/path/media.mp4")
                .expect("test index URL"),
            index_range: ManifestByteRange {
                offset: 100,
                length: u64::try_from(index_bytes.len()).expect("test index length"),
            },
            index_is_external: false,
            initialization_url: Url::parse("https://cdn.example.test/path/media.mp4")
                .expect("test initialization URL"),
            initialization_is_external: false,
            initialization_range: Some(ManifestByteRange {
                offset: 0,
                length: 100,
            }),
            index_range_exact: true,
            timescale: Some(1_000),
        }
    }

    #[test]
    fn expands_child_sidx_into_ordered_media_ranges() {
        let child = build_sidx_box(7, 1_000, 0, 0, &[(80, 2_000, 0x9000_0000)]);
        let root = build_sidx_box(
            7,
            1_000,
            0,
            0,
            &[(
                0x8000_0000 | u32::try_from(child.len()).expect("child size"),
                2_000,
                0x9000_0000,
            )],
        );
        let index = test_index(&root);
        let total_length = 100
            + u64::try_from(root.len()).expect("root length")
            + u64::try_from(child.len()).expect("child length")
            + 80;
        let root_node =
            parse_segment_base_index(&index, &root, total_length, total_length).expect("root SIDX");
        let DashSidxEntry::Index(child_reference) = &root_node.entries[0] else {
            panic!("root should point to a child index");
        };
        let child_node = parse_child_segment_index(&index, child_reference, &child, total_length)
            .expect("child SIDX");
        assert_eq!(
            child_node.entries,
            [DashSidxEntry::Media(ManifestByteRange {
                offset: 100
                    + u64::try_from(root.len()).expect("root length")
                    + u64::try_from(child.len()).expect("child length"),
                length: 80,
            })]
        );
        validate_segment_base_sidx_ranges(
            &index,
            &[child_reference.range],
            &[ManifestByteRange {
                offset: total_length - 80,
                length: 80,
            }],
            total_length,
        )
        .expect("all SIDX and media ranges are disjoint");
    }

    #[test]
    fn expands_multiple_hierarchy_levels_with_different_timescales() {
        let grandchild = build_sidx_box(7, 90_000, 0, 0, &[(96, 180_000, 0x9000_0000)]);
        let child = build_sidx_box(
            7,
            500,
            0,
            0,
            &[(
                0x8000_0000 | u32::try_from(grandchild.len()).expect("grandchild size"),
                1_000,
                0x9000_0000,
            )],
        );
        let root = build_sidx_box(
            7,
            1_000,
            0,
            0,
            &[(
                0x8000_0000 | u32::try_from(child.len()).expect("child size"),
                2_000,
                0x9000_0000,
            )],
        );
        let index = test_index(&root);
        let total_length = 100
            + u64::try_from(root.len()).expect("root length")
            + u64::try_from(child.len()).expect("child length")
            + u64::try_from(grandchild.len()).expect("grandchild length")
            + 96;
        let root_node =
            parse_segment_base_index(&index, &root, total_length, total_length).expect("root SIDX");
        let DashSidxEntry::Index(child_reference) = &root_node.entries[0] else {
            panic!("root should point to a child index");
        };
        let child_node = parse_child_segment_index(&index, child_reference, &child, total_length)
            .expect("child SIDX");
        let DashSidxEntry::Index(grandchild_reference) = &child_node.entries[0] else {
            panic!("child should point to a grandchild index");
        };
        let grandchild_node =
            parse_child_segment_index(&index, grandchild_reference, &grandchild, total_length)
                .expect("grandchild SIDX");
        assert_eq!(
            grandchild_node.entries,
            [DashSidxEntry::Media(ManifestByteRange {
                offset: total_length - 96,
                length: 96,
            })]
        );
        validate_segment_base_sidx_ranges(
            &index,
            &[child_reference.range, grandchild_reference.range],
            &[ManifestByteRange {
                offset: total_length - 96,
                length: 96,
            }],
            total_length,
        )
        .expect("nested SIDX ranges should be ordered and disjoint");
    }

    #[test]
    fn rejects_child_with_wrong_reference_id_or_timeline() {
        let child = build_sidx_box(8, 1_000, 0, 0, &[(80, 2_000, 0x9000_0000)]);
        let root = build_sidx_box(
            7,
            1_000,
            0,
            0,
            &[(
                0x8000_0000 | u32::try_from(child.len()).expect("child size"),
                2_000,
                0x9000_0000,
            )],
        );
        let index = test_index(&root);
        let total_length = 100
            + u64::try_from(root.len()).expect("root length")
            + u64::try_from(child.len()).expect("child length")
            + 80;
        let root_node =
            parse_segment_base_index(&index, &root, total_length, total_length).expect("root SIDX");
        let DashSidxEntry::Index(child_reference) = &root_node.entries[0] else {
            panic!("root should point to a child index");
        };
        assert_eq!(
            parse_child_segment_index(&index, child_reference, &child, total_length).err(),
            Some(DashError::UnsupportedAddressing)
        );

        let wrong_duration = build_sidx_box(7, 1_000, 0, 0, &[(80, 1_999, 0x9000_0000)]);
        assert_eq!(
            parse_child_segment_index(&index, child_reference, &wrong_duration, total_length,)
                .err(),
            Some(DashError::UnsupportedAddressing)
        );
    }

    #[test]
    fn accepts_equivalent_rational_presentation_time_and_duration() {
        let child = build_sidx_box(7, 90_000, 90_000, 0, &[(80, 90_000, 0x9000_0000)]);
        let root = build_sidx_box(
            7,
            1_000,
            1_000,
            0,
            &[(
                0x8000_0000 | u32::try_from(child.len()).expect("child size"),
                1_000,
                0x9000_0000,
            )],
        );
        let index = test_index(&root);
        let total_length = 100
            + u64::try_from(root.len()).expect("root length")
            + u64::try_from(child.len()).expect("child length")
            + 80;
        let root_node =
            parse_segment_base_index(&index, &root, total_length, total_length).expect("root SIDX");
        let DashSidxEntry::Index(child_reference) = &root_node.entries[0] else {
            panic!("root should point to a child index");
        };
        let child_node = parse_child_segment_index(&index, child_reference, &child, total_length)
            .expect("child SIDX has equivalent scaled timing");
        assert_eq!(child_node.entries.len(), 1);
    }

    #[test]
    fn rejects_child_reference_that_contains_extra_boxes() {
        let mut child = build_sidx_box(7, 1_000, 0, 0, &[(80, 2_000, 0x9000_0000)]);
        child.extend_from_slice(&[0, 0, 0, 8, b'f', b'r', b'e', b'e']);
        let root = build_sidx_box(
            7,
            1_000,
            0,
            0,
            &[(
                0x8000_0000 | u32::try_from(child.len()).expect("child size"),
                2_000,
                0x9000_0000,
            )],
        );
        let index = test_index(&root);
        let total_length = 100
            + u64::try_from(root.len()).expect("root length")
            + u64::try_from(child.len()).expect("child length")
            + 80;
        let root_node =
            parse_segment_base_index(&index, &root, total_length, total_length).expect("root SIDX");
        let DashSidxEntry::Index(child_reference) = &root_node.entries[0] else {
            panic!("root should point to a child index");
        };
        assert_eq!(
            parse_child_segment_index(&index, child_reference, &child, total_length).err(),
            Some(DashError::UnsupportedAddressing)
        );
    }

    #[test]
    fn resolves_external_representation_index_offsets_against_media_resource() {
        let bytes = build_sidx_box(7, 1_000, 0, 144, &[(80, 2_000, 0x9000_0000)]);
        let mut index = test_index(&bytes);
        index.index_url =
            Url::parse("https://cdn.example.test/path/index.sidx").expect("external index URL");
        index.index_range = ManifestByteRange {
            offset: 0,
            length: u64::try_from(bytes.len()).expect("external SIDX length"),
        };
        index.index_is_external = true;

        let node = parse_segment_base_index(&index, &bytes, 44, 224)
            .expect("external RepresentationIndex should parse");
        assert_eq!(
            node.entries,
            [DashSidxEntry::Media(ManifestByteRange {
                offset: 144,
                length: 80,
            })]
        );
        validate_segment_base_sidx_ranges(
            &index,
            &[],
            &[ManifestByteRange {
                offset: 144,
                length: 80,
            }],
            224,
        )
        .expect("external index and media ranges use their respective resources");
    }

    #[test]
    fn validates_separate_initialization_ranges_against_their_own_resource() {
        let bytes = build_sidx_box(7, 1_000, 0, 144, &[(80, 2_000, 0x9000_0000)]);
        let mut index = test_index(&bytes);
        index.index_url =
            Url::parse("https://cdn.example.test/path/index.sidx").expect("external index URL");
        index.index_range = ManifestByteRange {
            offset: 0,
            length: u64::try_from(bytes.len()).expect("external SIDX length"),
        };
        index.index_is_external = true;
        index.initialization_url =
            Url::parse("https://cdn.example.test/path/init.mp4").expect("separate init URL");
        index.initialization_is_external = true;
        index.initialization_range = Some(ManifestByteRange {
            offset: 500,
            length: 100,
        });

        let node = parse_segment_base_index(&index, &bytes, 44, 224)
            .expect("external init range is not relative to the media resource");
        assert_eq!(
            node.entries,
            [DashSidxEntry::Media(ManifestByteRange {
                offset: 144,
                length: 80,
            })]
        );
        validate_segment_base_sidx_ranges(
            &index,
            &[],
            &[ManifestByteRange {
                offset: 144,
                length: 80,
            }],
            224,
        )
        .expect("separate init ranges do not overlap media-resource ranges");
    }

    #[test]
    fn rejects_overlapping_expanded_index_and_media_ranges() {
        let index_bytes = build_sidx_box(7, 1_000, 0, 0, &[(80, 2_000, 0x9000_0000)]);
        let index = test_index(&index_bytes);
        assert_eq!(
            validate_segment_base_sidx_ranges(
                &index,
                &[ManifestByteRange {
                    offset: 200,
                    length: 40,
                }],
                &[ManifestByteRange {
                    offset: 220,
                    length: 80,
                }],
                1_000,
            )
            .err(),
            Some(DashError::InvalidSegment)
        );
        assert_eq!(
            validate_segment_base_sidx_ranges(
                &index,
                &[],
                &[ManifestByteRange {
                    offset: 120,
                    length: 10,
                }],
                1_000,
            )
            .err(),
            Some(DashError::InvalidSegment)
        );
    }
}
