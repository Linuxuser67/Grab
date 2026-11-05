//! Bounded extraction of fragmented DASH STPP subtitle tracks.
//!
//! This deliberately accepts only simple, media-time TTML text. Each sample is
//! a complete XML document whose cue times are on the track presentation
//! timeline; the output is clipped to that sample's MP4 presentation interval.
//! Styling, layout, external resources, and unsupported TTML timing stay on
//! Grab's existing yt-dlp fallback path.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use quick_xml::XmlVersion;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;

use crate::dash_subtitle::DashSubtitleError;

const TTML_NAMESPACE: &str = "http://www.w3.org/ns/ttml";
const TTML1_NAMESPACE: &str = "http://www.w3.org/2006/10/ttaf1";
const TTML_PARAMETER_NAMESPACE: &str = "http://www.w3.org/ns/ttml#parameter";

const MAX_TRACK_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SAMPLE_COUNT: usize = 50_000;
const MAX_SAMPLE_BYTES: usize = 256 * 1024;
const MAX_SAMPLE_TOTAL_BYTES: u64 = 48 * 1024 * 1024;
const MAX_SAMPLE_ENTRY_TEXT_BYTES: usize = 16 * 1024;
const MAX_XML_DEPTH: usize = 32;
const MAX_XML_ELEMENTS: usize = 20_000;
const MAX_CUE_COUNT: usize = 50_000;
const MAX_CUE_TEXT_BYTES: usize = 12 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
const LINE_BREAK_MARKER: char = '\u{e000}';

#[derive(Clone, Debug, PartialEq, Eq)]
struct Cue {
    start_ms: u64,
    end_ms: u64,
    identifier: Option<String>,
    text: String,
}

#[derive(Default)]
struct Paragraph {
    begin_ms: Option<u64>,
    end_ms: Option<u64>,
    duration_ms: Option<u64>,
    identifier: Option<String>,
    text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Element {
    Tt,
    Head,
    HeadContent,
    Body,
    Div,
    Paragraph,
    Span,
    Break,
}

/// Whether every codec identifier in a DASH declaration selects STPP.
pub(crate) fn is_stpp_codec(codecs: &str) -> bool {
    let codecs = codecs.trim();
    !codecs.is_empty()
        && codecs.split(',').all(|codec| {
            let codec = codec.trim().to_ascii_lowercase();
            codec == "stpp" || codec.starts_with("stpp.")
        })
}

/// Extract a selected fragmented STPP track into private WebVTT staging.
///
/// Returns false for a valid track with no visible text cues. Ambiguous,
/// unsupported, or malformed track data is rejected so the caller can retain
/// the existing yt-dlp fallback behavior.
///
/// # Errors
///
/// Returns an unsupported error for unsupported sample entries, timing, XML,
/// or extraction limits. Returns a failed error when local staging I/O fails.
pub(crate) fn extract_to_vtt(
    source: &Path,
    destination: &Path,
    codecs: &str,
) -> Result<bool, DashSubtitleError> {
    clear_destination(destination)?;
    if !is_stpp_codec(codecs) {
        return Err(DashSubtitleError::Unsupported(
            "Only STPP subtitle samples are supported by the native TTML extractor",
        ));
    }

    let mut file = open_source(source)?;
    let track_bytes = file
        .metadata()
        .map_err(|error| {
            DashSubtitleError::Failed(format!(
                "Could not inspect the staged DASH subtitle track: {error}"
            ))
        })?
        .len();
    if track_bytes == 0 || track_bytes > MAX_TRACK_BYTES {
        return Err(DashSubtitleError::Unsupported(
            "The staged DASH subtitle track is empty or exceeds the 64 MiB extraction limit",
        ));
    }

    let boxes = mp4box::get_boxes(&mut file, track_bytes, false).map_err(|error| {
        DashSubtitleError::UnsupportedMp4(format!(
            "The staged DASH subtitle track has malformed or unsupported MP4 boxes: {error}"
        ))
    })?;
    if contains_box(&boxes, "edts")
        || contains_box(&boxes, "elst")
        || contains_box(&boxes, "subs")
    {
        return Err(DashSubtitleError::Unsupported(
            "STPP tracks with edit lists or external sample resources are not supported",
        ));
    }
    if !has_fragmented_structure(&boxes) {
        return Err(DashSubtitleError::Unsupported(
            "The staged DASH subtitle track is not fragmented MP4 with media data",
        ));
    }
    let sample_entry = single_stpp_sample_entry(&boxes)?;
    validate_stpp_sample_entry(&mut file, sample_entry, track_bytes)?;

    let mut declared_samples = 0_usize;
    count_fragment_samples(&boxes, &mut file, &mut declared_samples)?;
    if declared_samples > MAX_SAMPLE_COUNT {
        return Err(DashSubtitleError::Unsupported(
            "The DASH subtitle fragments exceed the 50,000 sample limit",
        ));
    }

    file.seek(SeekFrom::Start(0)).map_err(|error| {
        DashSubtitleError::Failed(format!(
            "Could not rewind the staged DASH subtitle track: {error}"
        ))
    })?;
    let tracks = mp4box::track_samples_from_reader(&mut file).map_err(|error| {
        DashSubtitleError::UnsupportedMp4(format!(
            "The staged DASH subtitle track has malformed or unsupported sample tables: {error}"
        ))
    })?;
    if tracks.len() != 1 || tracks[0].handler_type != "subt" {
        return Err(DashSubtitleError::Unsupported(
            "The staged resource does not contain exactly one ISO BMFF subtitle track",
        ));
    }

    let track = &tracks[0];
    let sample_count = usize::try_from(track.sample_count).map_err(|_| {
        DashSubtitleError::Unsupported("The DASH subtitle sample count is out of range")
    })?;
    if track.timescale == 0
        || sample_count == 0
        || sample_count > MAX_SAMPLE_COUNT
        || sample_count != track.samples.len()
        || sample_count != declared_samples
    {
        return Err(DashSubtitleError::Unsupported(
            "The DASH subtitle track has an invalid timescale or inconsistent sample count",
        ));
    }

    let total_sample_bytes = track.samples.iter().try_fold(0_u64, |total, sample| {
        let next = total.checked_add(u64::from(sample.size))?;
        (next <= MAX_SAMPLE_TOTAL_BYTES).then_some(next)
    });
    if total_sample_bytes.is_none() {
        return Err(DashSubtitleError::Unsupported(
            "The DASH subtitle samples exceed the 48 MiB extraction limit",
        ));
    }

    let mut cues = Vec::new();
    let mut cue_text_bytes = 0_usize;
    let mut previous_sample_end = None;
    for sample in &track.samples {
        let sample_size = usize::try_from(sample.size).map_err(|_| {
            DashSubtitleError::Unsupported("A DASH subtitle sample size is out of range")
        })?;
        if sample_size == 0 || sample_size > MAX_SAMPLE_BYTES || sample.duration == 0 {
            return Err(DashSubtitleError::Unsupported(
                "A DASH subtitle sample is empty, has zero duration, or exceeds 256 KiB",
            ));
        }
        let sample_end = sample
            .file_offset
            .checked_add(u64::from(sample.size))
            .filter(|end| *end <= track_bytes)
            .ok_or(DashSubtitleError::Unsupported(
                "A DASH subtitle sample points outside its assembled track",
            ))?;
        if !sample_is_within_media_data(&boxes, sample.file_offset, u64::from(sample.size)) {
            return Err(DashSubtitleError::Unsupported(
                "A DASH subtitle sample points outside an MP4 media-data box",
            ));
        }

        let end_units = sample
            .pts
            .checked_add(u64::from(sample.duration))
            .ok_or(DashSubtitleError::Unsupported(
                "A DASH subtitle sample presentation interval overflows",
            ))?;
        if previous_sample_end.is_some_and(|previous_end| sample.pts < previous_end) {
            return Err(DashSubtitleError::Unsupported(
                "DASH subtitle sample presentation intervals overlap or are unordered",
            ));
        }
        previous_sample_end = Some(end_units);
        let start_ms = units_to_millis(sample.pts, track.timescale)?;
        let end_ms = units_to_millis(end_units, track.timescale)?;
        if end_ms <= start_ms {
            return Err(DashSubtitleError::Unsupported(
                "A DASH subtitle sample is shorter than the supported millisecond precision",
            ));
        }

        let mut bytes = vec![0_u8; sample_size];
        file.seek(SeekFrom::Start(sample.file_offset))
            .and_then(|_| file.read_exact(&mut bytes))
            .map_err(|error| {
                DashSubtitleError::Failed(format!(
                    "Could not read a staged DASH subtitle sample ending at byte {sample_end}: {error}"
                ))
            })?;

        for cue in parse_ttml_document(&bytes, start_ms, end_ms)? {
            cue_text_bytes = cue_text_bytes
                .checked_add(cue.text.len())
                .filter(|size| *size <= MAX_CUE_TEXT_BYTES)
                .ok_or(DashSubtitleError::Unsupported(
                    "The DASH TTML cues exceed the 12 MiB text limit",
                ))?;
            cues.push(cue);
            if cues.len() > MAX_CUE_COUNT {
                return Err(DashSubtitleError::Unsupported(
                    "The DASH TTML track exceeds the 50,000 cue limit",
                ));
            }
        }
    }

    if cues.is_empty() {
        return Ok(false);
    }

    cues.sort_unstable_by(|left, right| {
        left.start_ms
            .cmp(&right.start_ms)
            .then_with(|| left.end_ms.cmp(&right.end_ms))
            .then_with(|| left.identifier.cmp(&right.identifier))
            .then_with(|| left.text.cmp(&right.text))
    });
    let cues = merge_adjacent_cues(cues);
    let output = serialize_vtt(&cues);
    write_private_output(destination, output.as_bytes())?;
    Ok(true)
}

fn open_source(source: &Path) -> Result<File, DashSubtitleError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(source).map_err(|error| {
        DashSubtitleError::Failed(format!(
            "Could not safely open the staged DASH subtitle track: {error}"
        ))
    })?;
    if !file
        .metadata()
        .map_err(|error| {
            DashSubtitleError::Failed(format!(
                "Could not inspect the staged DASH subtitle track: {error}"
            ))
        })?
        .is_file()
    {
        return Err(DashSubtitleError::Failed(
            "The staged DASH subtitle track is not a regular file".to_string(),
        ));
    }
    Ok(file)
}

fn contains_box(boxes: &[mp4box::Box], kind: &str) -> bool {
    boxes.iter().any(|box_| {
        box_.typ == kind
            || box_
                .children
                .as_deref()
                .is_some_and(|children| contains_box(children, kind))
    })
}

fn count_boxes(boxes: &[mp4box::Box], kind: &str) -> usize {
    boxes
        .iter()
        .map(|box_| {
            usize::from(box_.typ == kind)
                + box_
                    .children
                    .as_deref()
                    .map_or(0, |children| count_boxes(children, kind))
        })
        .sum()
}

fn find_box<'boxes>(boxes: &'boxes [mp4box::Box], kind: &str) -> Option<&'boxes mp4box::Box> {
    for box_ in boxes {
        if box_.typ == kind {
            return Some(box_);
        }
        if let Some(children) = box_.children.as_deref()
            && let Some(found) = find_box(children, kind)
        {
            return Some(found);
        }
    }
    None
}

fn has_fragmented_structure(boxes: &[mp4box::Box]) -> bool {
    contains_box(boxes, "moof") && contains_box(boxes, "trun") && contains_box(boxes, "mdat")
}

fn single_stpp_sample_entry(
    boxes: &[mp4box::Box],
) -> Result<&mp4box::Box, DashSubtitleError> {
    if count_boxes(boxes, "stsd") != 1 {
        return Err(DashSubtitleError::Unsupported(
            "The DASH subtitle track must contain exactly one sample description",
        ));
    }
    let entries = find_box(boxes, "stsd")
        .and_then(|stsd| stsd.children.as_deref())
        .ok_or(DashSubtitleError::Unsupported(
            "The DASH subtitle sample description is missing",
        ))?;
    if entries.len() != 1 || entries[0].typ != "stpp" {
        return Err(DashSubtitleError::Unsupported(
            "The DASH subtitle track must have exactly one STPP sample entry",
        ));
    }
    Ok(&entries[0])
}

fn validate_stpp_sample_entry(
    reader: &mut File,
    entry: &mp4box::Box,
    track_bytes: u64,
) -> Result<(), DashSubtitleError> {
    if entry.header_size < 8 {
        return Err(DashSubtitleError::Unsupported(
            "The STPP sample entry has an invalid box header",
        ));
    }
    let entry_end = entry
        .offset
        .checked_add(entry.size)
        .filter(|end| *end <= track_bytes)
        .ok_or(DashSubtitleError::Unsupported(
            "The STPP sample entry points outside the assembled track",
        ))?;
    let strings_start = entry
        .offset
        .checked_add(entry.header_size)
        .and_then(|offset| offset.checked_add(8))
        .ok_or(DashSubtitleError::Unsupported(
            "The STPP sample-entry metadata offset overflows",
        ))?;
    let remaining = entry_end
        .checked_sub(strings_start)
        .ok_or(DashSubtitleError::Unsupported(
            "The STPP sample entry is truncated",
        ))?;
    let read_len = usize::try_from(remaining.min(MAX_SAMPLE_ENTRY_TEXT_BYTES as u64))
        .map_err(|_| {
            DashSubtitleError::Unsupported("The STPP sample-entry metadata size is out of range")
        })?;
    if read_len == 0 {
        return Err(DashSubtitleError::Unsupported(
            "The STPP sample entry is missing its namespace metadata",
        ));
    }
    let mut bytes = vec![0_u8; read_len];
    reader
        .seek(SeekFrom::Start(strings_start))
        .and_then(|_| reader.read_exact(&mut bytes))
        .map_err(|error| {
            DashSubtitleError::Failed(format!(
                "Could not read STPP sample-entry metadata: {error}"
            ))
        })?;

    let mut fields = Vec::with_capacity(3);
    let mut start = 0;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == 0 {
            fields.push(&bytes[start..index]);
            start = index + 1;
            if fields.len() == 3 {
                break;
            }
        }
    }
    if fields.len() != 3 {
        return Err(DashSubtitleError::Unsupported(
            "The STPP sample entry does not contain three bounded null-terminated fields",
        ));
    }
    let namespace = std::str::from_utf8(fields[0]).map_err(|_| {
        DashSubtitleError::Unsupported("The STPP namespace field is not valid UTF-8")
    })?;
    let schema_location = std::str::from_utf8(fields[1]).map_err(|_| {
        DashSubtitleError::Unsupported("The STPP schema-location field is not valid UTF-8")
    })?;
    let auxiliary_mime_types = std::str::from_utf8(fields[2]).map_err(|_| {
        DashSubtitleError::Unsupported("The STPP auxiliary MIME field is not valid UTF-8")
    })?;
    if (namespace != TTML_NAMESPACE && namespace != TTML1_NAMESPACE)
        || !auxiliary_mime_types.trim().is_empty()
    {
        return Err(DashSubtitleError::Unsupported(
            "Only plain TTML STPP samples without external resources are supported",
        ));
    }
    let _schema_location = schema_location;
    if entry
        .children
        .as_deref()
        .is_some_and(|children| children.iter().any(|child| child.typ != "btrt"))
    {
        return Err(DashSubtitleError::Unsupported(
            "The STPP sample entry contains an unsupported extension box",
        ));
    }
    Ok(())
}

fn sample_is_within_media_data(
    boxes: &[mp4box::Box],
    sample_offset: u64,
    sample_size: u64,
) -> bool {
    let Some(sample_end) = sample_offset.checked_add(sample_size) else {
        return false;
    };
    boxes.iter().any(|box_| {
        if box_.typ != "mdat" {
            return false;
        }
        let (Some(data_start), Some(data_size)) = (box_.payload_offset, box_.payload_size) else {
            return false;
        };
        data_start
            .checked_add(data_size)
            .is_some_and(|data_end| sample_offset >= data_start && sample_end <= data_end)
    })
}

fn count_fragment_samples(
    boxes: &[mp4box::Box],
    reader: &mut File,
    total: &mut usize,
) -> Result<(), DashSubtitleError> {
    for box_ in boxes {
        if box_.typ == "trun" {
            let payload_offset = box_.payload_offset.ok_or(DashSubtitleError::Unsupported(
                "A DASH fragment run has no payload offset",
            ))?;
            let payload_size = box_.payload_size.ok_or(DashSubtitleError::Unsupported(
                "A DASH fragment run has no payload size",
            ))?;
            if payload_size < 8 {
                return Err(DashSubtitleError::Unsupported(
                    "A DASH fragment run has a truncated sample count",
                ));
            }
            let count_offset =
                payload_offset
                    .checked_add(4)
                    .ok_or(DashSubtitleError::Unsupported(
                        "A DASH fragment run sample-count offset overflows",
                    ))?;
            reader
                .seek(SeekFrom::Start(count_offset))
                .map_err(|error| {
                    DashSubtitleError::Failed(format!(
                        "Could not inspect a DASH fragment run: {error}"
                    ))
                })?;
            let mut bytes = [0_u8; 4];
            reader.read_exact(&mut bytes).map_err(|error| {
                DashSubtitleError::Failed(format!(
                    "Could not read a DASH fragment run sample count: {error}"
                ))
            })?;
            let count = usize::try_from(u32::from_be_bytes(bytes)).map_err(|_| {
                DashSubtitleError::Unsupported("A DASH fragment run sample count is out of range")
            })?;
            *total = total
                .checked_add(count)
                .filter(|count| *count <= MAX_SAMPLE_COUNT)
                .ok_or(DashSubtitleError::Unsupported(
                    "The DASH subtitle fragments exceed the 50,000 sample limit",
                ))?;
        }
        if let Some(children) = box_.children.as_deref() {
            count_fragment_samples(children, reader, total)?;
        }
    }
    Ok(())
}

fn parse_ttml_document(
    bytes: &[u8],
    sample_start_ms: u64,
    sample_end_ms: u64,
) -> Result<Vec<Cue>, DashSubtitleError> {
    if bytes.is_empty() || bytes.len() > MAX_SAMPLE_BYTES {
        return Err(DashSubtitleError::Unsupported(
            "A DASH TTML sample is empty or exceeds the 256 KiB XML limit",
        ));
    }
    std::str::from_utf8(bytes).map_err(|_| {
        DashSubtitleError::Unsupported("A DASH TTML sample is not valid UTF-8")
    })?;

    let mut reader = NsReader::from_reader(bytes);
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut stack = Vec::new();
    let mut paragraph = None;
    let mut cues = Vec::new();
    let mut root_seen = false;
    let mut root_closed = false;
    let mut body_seen = false;
    let mut element_count = 0_usize;

    loop {
        let (namespace, event) = reader
            .read_resolved_event_into(&mut buffer)
            .map_err(|error| {
                DashSubtitleError::UnsupportedMp4(format!(
                    "The DASH TTML sample contains malformed XML: {error}"
                ))
            })?;
        match event {
            Event::Start(start) => {
                element_count = element_count.checked_add(1).ok_or(
                    DashSubtitleError::Unsupported("The DASH TTML element count overflows"),
                )?;
                if element_count > MAX_XML_ELEMENTS || stack.len() >= MAX_XML_DEPTH {
                    return Err(DashSubtitleError::Unsupported(
                        "The DASH TTML sample exceeds the XML depth or element limit",
                    ));
                }
                let (element, next_paragraph) =
                    classify_element(&start, &namespace, stack.last().copied(), &mut root_seen, &mut body_seen)?;
                if element == Element::Break {
                    return Err(DashSubtitleError::Unsupported(
                        "TTML line breaks must use empty br elements",
                    ));
                }
                if element == Element::Paragraph {
                    if paragraph.is_some() {
                        return Err(DashSubtitleError::Unsupported(
                            "Nested TTML paragraph elements are not supported",
                        ));
                    }
                    paragraph = next_paragraph;
                }
                stack.push(element);
            }
            Event::Empty(start) => {
                element_count = element_count.checked_add(1).ok_or(
                    DashSubtitleError::Unsupported("The DASH TTML element count overflows"),
                )?;
                if element_count > MAX_XML_ELEMENTS || stack.len() >= MAX_XML_DEPTH {
                    return Err(DashSubtitleError::Unsupported(
                        "The DASH TTML sample exceeds the XML depth or element limit",
                    ));
                }
                let (element, next_paragraph) =
                    classify_element(&start, &namespace, stack.last().copied(), &mut root_seen, &mut body_seen)?;
                match element {
                    Element::Paragraph => {
                        let paragraph = next_paragraph.ok_or(
                            DashSubtitleError::Unsupported("A TTML paragraph has invalid timing"),
                        )?;
                        finish_paragraph(paragraph, sample_start_ms, sample_end_ms, &mut cues)?;
                    }
                    Element::Break => {
                        append_paragraph_break(&mut paragraph)?;
                    }
                    Element::Tt => root_closed = true,
                    _ => {}
                }
            }
            Event::End(_) => {
                let element = stack.pop().ok_or(DashSubtitleError::Unsupported(
                    "The DASH TTML sample has an unexpected closing element",
                ))?;
                if element == Element::Paragraph {
                    let paragraph = paragraph.take().ok_or(
                        DashSubtitleError::Unsupported("A TTML paragraph lost its parser state"),
                    )?;
                    finish_paragraph(paragraph, sample_start_ms, sample_end_ms, &mut cues)?;
                } else if element == Element::Tt {
                    root_closed = true;
                }
            }
            Event::Text(text) => {
                let content = text
                    .xml_content(XmlVersion::Implicit1_0)
                    .map_err(|_| {
                        DashSubtitleError::Unsupported(
                            "A DASH TTML text node cannot be decoded as XML 1.0",
                        )
                    })?;
                match stack.last().copied() {
                    Some(Element::Paragraph | Element::Span) => {
                        append_paragraph_text(&mut paragraph, &content)?;
                    }
                    Some(Element::Head | Element::HeadContent) => {}
                    _ if content.trim().is_empty() => {}
                    _ => {
                        return Err(DashSubtitleError::Unsupported(
                            "Text outside a supported TTML paragraph is not supported",
                        ));
                    }
                }
            }
            Event::GeneralRef(reference) => {
                match stack.last().copied() {
                    Some(Element::Paragraph | Element::Span) => {
                        let character = if let Some(character) =
                            reference.resolve_char_ref().map_err(|_| {
                                DashSubtitleError::Unsupported(
                                    "A TTML character reference is invalid",
                                )
                            })?
                        {
                            character.to_string()
                        } else {
                            resolve_predefined_entity(reference.as_ref())
                                .ok_or(DashSubtitleError::Unsupported(
                                    "Custom TTML XML entities are not supported",
                                ))?
                                .to_string()
                        };
                        append_paragraph_text(&mut paragraph, &character)?;
                    }
                    Some(Element::Head | Element::HeadContent) => {}
                    _ => {
                        return Err(DashSubtitleError::Unsupported(
                            "An XML entity reference outside TTML text is not supported",
                        ));
                    }
                }
            }
            Event::DocType(_) => {
                return Err(DashSubtitleError::Unsupported(
                    "TTML documents with document types or custom entities are not supported",
                ));
            }
            Event::CData(_) | Event::PI(_) => {
                return Err(DashSubtitleError::Unsupported(
                    "TTML CDATA and processing instructions are not supported",
                ));
            }
            Event::Decl(_) | Event::Comment(_) => {}
            Event::Eof => break,
        }
        buffer.clear();
    }

    if !root_seen || !root_closed || !stack.is_empty() || paragraph.is_some() {
        return Err(DashSubtitleError::Unsupported(
            "The DASH TTML sample does not contain one complete TTML document",
        ));
    }
    Ok(cues)
}

fn classify_element(
    start: &BytesStart<'_>,
    namespace: &ResolveResult<'_>,
    parent: Option<Element>,
    root_seen: &mut bool,
    body_seen: &mut bool,
) -> Result<(Element, Option<Paragraph>), DashSubtitleError> {
    if !matches!(
        namespace,
        ResolveResult::Bound(namespace)
            if namespace.into_inner() == TTML_NAMESPACE.as_bytes()
                || namespace.into_inner() == TTML1_NAMESPACE.as_bytes()
    ) {
        return Err(DashSubtitleError::Unsupported(
            "The DASH TTML document uses an unsupported XML namespace",
        ));
    }
    let name = std::str::from_utf8(start.name().local_name().as_ref()).map_err(|_| {
        DashSubtitleError::Unsupported("A TTML element name is not valid UTF-8")
    })?;

    if matches!(parent, Some(Element::Head | Element::HeadContent)) {
        return Ok((Element::HeadContent, None));
    }

    match (parent, name) {
        (None, "tt") if !*root_seen => {
            validate_root_attributes(start)?;
            *root_seen = true;
            Ok((Element::Tt, None))
        }
        (Some(Element::Tt), "head") => {
            validate_metadata_attributes(start)?;
            Ok((Element::Head, None))
        }
        (Some(Element::Tt), "body") if !*body_seen => {
            validate_container_attributes(start)?;
            *body_seen = true;
            Ok((Element::Body, None))
        }
        (Some(Element::Body | Element::Div), "div") => {
            validate_container_attributes(start)?;
            Ok((Element::Div, None))
        }
        (Some(Element::Div), "p") => {
            let paragraph = parse_paragraph_attributes(start)?;
            Ok((Element::Paragraph, Some(paragraph)))
        }
        (Some(Element::Paragraph | Element::Span), "span") => {
            validate_span_attributes(start)?;
            Ok((Element::Span, None))
        }
        (Some(Element::Paragraph | Element::Span), "br") => {
            validate_span_attributes(start)?;
            Ok((Element::Break, None))
        }
        _ => Err(DashSubtitleError::Unsupported(
            "The TTML document contains an unsupported element or nesting pattern",
        )),
    }
}

fn read_attributes(
    start: &BytesStart<'_>,
) -> Result<BTreeMap<String, String>, DashSubtitleError> {
    let mut attributes = BTreeMap::new();
    for attribute in start.attributes() {
        let attribute = attribute.map_err(|_| {
            DashSubtitleError::Unsupported("A TTML element has malformed attributes")
        })?;
        let key = std::str::from_utf8(attribute.key.as_ref()).map_err(|_| {
            DashSubtitleError::Unsupported("A TTML attribute name is not valid UTF-8")
        })?;
        let value = attribute
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|_| {
                DashSubtitleError::Unsupported("A TTML attribute value is malformed")
            })?
            .into_owned();
        if attributes.insert(key.to_string(), value).is_some() {
            return Err(DashSubtitleError::Unsupported(
                "A TTML element contains duplicate attributes",
            ));
        }
    }
    Ok(attributes)
}

fn is_namespace_declaration(name: &str) -> bool {
    name == "xmlns" || name.starts_with("xmlns:")
}

fn validate_root_attributes(start: &BytesStart<'_>) -> Result<(), DashSubtitleError> {
    let attributes = read_attributes(start)?;
    for (name, value) in &attributes {
        if is_namespace_declaration(name) || name == "xml:lang" {
            continue;
        }
        if name == "xml:space" {
            if value != "default" {
                return Err(DashSubtitleError::Unsupported(
                    "TTML xml:space preservation is not supported",
                ));
            }
            continue;
        }
        if name == "ttp:timeBase" || name == "ttp:profile" {
            if attributes.get("xmlns:ttp").map(String::as_str)
                != Some(TTML_PARAMETER_NAMESPACE)
            {
                return Err(DashSubtitleError::Unsupported(
                    "The TTML parameter attributes use an unexpected namespace",
                ));
            }
            if name == "ttp:timeBase" && value != "media" {
                return Err(DashSubtitleError::Unsupported(
                    "Only TTML media timeBase is defined for native STPP extraction",
                ));
            }
            continue;
        }
        return Err(DashSubtitleError::Unsupported(
            "The TTML root uses an unsupported processing or timing attribute",
        ));
    }
    Ok(())
}

fn validate_metadata_attributes(start: &BytesStart<'_>) -> Result<(), DashSubtitleError> {
    for (name, value) in read_attributes(start)? {
        if is_namespace_declaration(&name) || name == "xml:lang" {
            continue;
        }
        if name == "xml:space" && value == "default" {
            continue;
        }
        return Err(DashSubtitleError::Unsupported(
            "The TTML head uses unsupported metadata attributes",
        ));
    }
    Ok(())
}

fn validate_container_attributes(start: &BytesStart<'_>) -> Result<(), DashSubtitleError> {
    for (name, value) in read_attributes(start)? {
        if is_namespace_declaration(&name) || name == "xml:lang" {
            continue;
        }
        if name == "xml:space" && value == "default" {
            continue;
        }
        if name == "timeContainer" && value == "par" {
            continue;
        }
        return Err(DashSubtitleError::Unsupported(
            "TTML body and div elements must use the default parallel, zero-origin timing",
        ));
    }
    Ok(())
}

fn validate_span_attributes(start: &BytesStart<'_>) -> Result<(), DashSubtitleError> {
    for (name, value) in read_attributes(start)? {
        if is_namespace_declaration(&name) || name == "xml:lang" {
            continue;
        }
        if name == "xml:space" && value == "default" {
            continue;
        }
        return Err(DashSubtitleError::Unsupported(
            "Styled, positioned, or timed TTML span and br elements are not supported",
        ));
    }
    Ok(())
}

fn parse_paragraph_attributes(
    start: &BytesStart<'_>,
) -> Result<Paragraph, DashSubtitleError> {
    let mut paragraph = Paragraph::default();
    for (name, value) in read_attributes(start)? {
        if is_namespace_declaration(&name) {
            continue;
        }
        match name.as_str() {
            "begin" => paragraph.begin_ms = Some(parse_ttml_time(&value)?),
            "end" => paragraph.end_ms = Some(parse_ttml_time(&value)?),
            "dur" => paragraph.duration_ms = Some(parse_ttml_time(&value)?),
            "xml:id" => {
                if value.contains('\n') || value.contains('\r') || value.is_empty() {
                    return Err(DashSubtitleError::Unsupported(
                        "A TTML paragraph identifier is empty or contains a line break",
                    ));
                }
                paragraph.identifier = Some(value);
            }
            "xml:lang" => {}
            "xml:space" if value == "default" => {}
            _ => {
                return Err(DashSubtitleError::Unsupported(
                    "TTML paragraphs must not use styling, layout, or unsupported timing attributes",
                ));
            }
        }
    }
    Ok(paragraph)
}

fn parse_ttml_time(value: &str) -> Result<u64, DashSubtitleError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(DashSubtitleError::Unsupported(
            "A TTML time expression is empty",
        ));
    }

    let mut clock = value.split(':');
    if let (Some(hours), Some(minutes), Some(seconds), None) =
        (clock.next(), clock.next(), clock.next(), clock.next())
    {
        let hours = hours.parse::<u128>().map_err(|_| {
            DashSubtitleError::Unsupported("A TTML clock-time expression is invalid")
        })?;
        let minutes = minutes.parse::<u128>().map_err(|_| {
            DashSubtitleError::Unsupported("A TTML clock-time expression is invalid")
        })?;
        let seconds_ms = parse_decimal_scaled(seconds, 1_000)?;
        if minutes >= 60 || seconds_ms >= 60_000 {
            return Err(DashSubtitleError::Unsupported(
                "A TTML clock-time field is outside its valid range",
            ));
        }
        let total = hours
            .checked_mul(3_600_000)
            .and_then(|time| time.checked_add(minutes * 60_000))
            .and_then(|time| time.checked_add(seconds_ms))
            .ok_or(DashSubtitleError::Unsupported(
                "A TTML clock-time expression overflows",
            ))?;
        return u64::try_from(total)
            .map_err(|_| DashSubtitleError::Unsupported("A TTML time is out of range"));
    }

    for (unit, scale) in [("ms", 1_u128), ("h", 3_600_000), ("m", 60_000), ("s", 1_000)] {
        if let Some(number) = value.strip_suffix(unit) {
            let millis = parse_decimal_scaled(number, scale)?;
            return u64::try_from(millis)
                .map_err(|_| DashSubtitleError::Unsupported("A TTML time is out of range"));
        }
    }
    Err(DashSubtitleError::Unsupported(
        "Only TTML clock-time and h, m, s, or ms offset-time expressions are supported",
    ))
}

fn parse_decimal_scaled(value: &str, scale: u128) -> Result<u128, DashSubtitleError> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || (!fraction.is_empty() && !fraction.bytes().all(|byte| byte.is_ascii_digit()))
        || fraction.len() > 9
        || value.ends_with('.')
    {
        return Err(DashSubtitleError::Unsupported(
            "A TTML decimal time expression is invalid or too precise",
        ));
    }
    let whole = whole.parse::<u128>().map_err(|_| {
        DashSubtitleError::Unsupported("A TTML decimal time expression is out of range")
    })?;
    let scaled_whole = whole
        .checked_mul(scale)
        .ok_or(DashSubtitleError::Unsupported(
            "A TTML decimal time expression overflows",
        ))?;
    if fraction.is_empty() {
        return Ok(scaled_whole);
    }
    let denominator = 10_u128
        .checked_pow(u32::try_from(fraction.len()).map_err(|_| {
            DashSubtitleError::Unsupported("A TTML decimal time expression is too precise")
        })?)
        .ok_or(DashSubtitleError::Unsupported(
            "A TTML decimal time expression is too precise",
        ))?;
    let fraction = fraction.parse::<u128>().map_err(|_| {
        DashSubtitleError::Unsupported("A TTML decimal time expression is invalid")
    })?;
    let scaled_fraction = fraction
        .checked_mul(scale)
        .ok_or(DashSubtitleError::Unsupported(
            "A TTML decimal time expression overflows",
        ))?;
    if scaled_fraction % denominator != 0 {
        return Err(DashSubtitleError::Unsupported(
            "A TTML time expression is finer than one millisecond",
        ));
    }
    scaled_whole
        .checked_add(scaled_fraction / denominator)
        .ok_or(DashSubtitleError::Unsupported(
            "A TTML decimal time expression overflows",
        ))
}

fn append_paragraph_break(paragraph: &mut Option<Paragraph>) -> Result<(), DashSubtitleError> {
    let paragraph = paragraph.as_mut().ok_or(DashSubtitleError::Unsupported(
        "A TTML line break has no enclosing paragraph",
    ))?;
    let new_len = paragraph
        .text
        .len()
        .checked_add(LINE_BREAK_MARKER.len_utf8())
        .filter(|length| *length <= MAX_CUE_TEXT_BYTES)
        .ok_or(DashSubtitleError::Unsupported(
            "A TTML paragraph exceeds the 12 MiB text limit",
        ))?;
    paragraph.text.reserve(new_len.saturating_sub(paragraph.text.len()));
    paragraph.text.push(LINE_BREAK_MARKER);
    Ok(())
}

fn append_paragraph_text(
    paragraph: &mut Option<Paragraph>,
    value: &str,
) -> Result<(), DashSubtitleError> {
    let paragraph = paragraph.as_mut().ok_or(DashSubtitleError::Unsupported(
        "TTML text has no enclosing paragraph",
    ))?;
    if value.contains(LINE_BREAK_MARKER) {
        return Err(DashSubtitleError::Unsupported(
            "The TTML text contains a reserved line-break character",
        ));
    }
    let new_len = paragraph
        .text
        .len()
        .checked_add(value.len())
        .filter(|length| *length <= MAX_CUE_TEXT_BYTES)
        .ok_or(DashSubtitleError::Unsupported(
            "A TTML paragraph exceeds the 12 MiB text limit",
        ))?;
    paragraph.text.reserve(new_len.saturating_sub(paragraph.text.len()));
    paragraph.text.push_str(value);
    Ok(())
}

fn finish_paragraph(
    paragraph: Paragraph,
    sample_start_ms: u64,
    sample_end_ms: u64,
    cues: &mut Vec<Cue>,
) -> Result<(), DashSubtitleError> {
    let begin_ms = paragraph.begin_ms.unwrap_or(0);
    let mut end_ms = paragraph.end_ms.unwrap_or(u64::MAX);
    if let Some(duration_ms) = paragraph.duration_ms {
        let duration_end = begin_ms.checked_add(duration_ms).ok_or(
            DashSubtitleError::Unsupported("A TTML paragraph duration overflows"),
        )?;
        end_ms = end_ms.min(duration_end);
    }
    if end_ms <= begin_ms {
        return Err(DashSubtitleError::Unsupported(
            "A TTML paragraph has an empty or inverted time interval",
        ));
    }

    let text = normalize_ttml_text(&paragraph.text);
    if text.is_empty() {
        return Ok(());
    }
    let start_ms = begin_ms.max(sample_start_ms);
    let end_ms = end_ms.min(sample_end_ms);
    if end_ms <= start_ms {
        return Ok(());
    }
    cues.push(Cue {
        start_ms,
        end_ms,
        identifier: paragraph.identifier,
        text,
    });
    Ok(())
}

fn normalize_ttml_text(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut pending_space = false;
    for character in text.chars() {
        if character == LINE_BREAK_MARKER {
            while normalized.ends_with(' ') {
                normalized.pop();
            }
            if !normalized.is_empty() && !normalized.ends_with('\n') {
                normalized.push('\n');
            }
            pending_space = false;
        } else if character.is_whitespace() {
            pending_space = true;
        } else {
            if pending_space && !normalized.is_empty() && !normalized.ends_with('\n') {
                normalized.push(' ');
            }
            normalized.push(character);
            pending_space = false;
        }
    }
    normalized.trim().to_string()
}

fn merge_adjacent_cues(cues: Vec<Cue>) -> Vec<Cue> {
    let mut merged: Vec<Cue> = Vec::with_capacity(cues.len());
    for cue in cues {
        if let Some(previous) = merged.last_mut()
            && previous.identifier == cue.identifier
            && previous.text == cue.text
            && cue.start_ms <= previous.end_ms
        {
            previous.end_ms = previous.end_ms.max(cue.end_ms);
        } else {
            merged.push(cue);
        }
    }
    merged
}

fn serialize_vtt(cues: &[Cue]) -> String {
    let mut output = String::from("WEBVTT\n\n");
    for cue in cues {
        if let Some(identifier) = &cue.identifier {
            output.push_str(identifier);
            output.push('\n');
        }
        output.push_str(&format_timestamp(cue.start_ms));
        output.push_str(" --> ");
        output.push_str(&format_timestamp(cue.end_ms));
        output.push('\n');
        for (index, line) in cue.text.split('\n').enumerate() {
            if index > 0 {
                output.push('\n');
            }
            output.push_str(&line.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;"));
        }
        output.push_str("\n\n");
    }
    output
}

fn format_timestamp(milliseconds: u64) -> String {
    let hours = milliseconds / 3_600_000;
    let minutes = (milliseconds / 60_000) % 60;
    let seconds = (milliseconds / 1_000) % 60;
    let fraction = milliseconds % 1_000;
    format!("{hours:02}:{minutes:02}:{seconds:02}.{fraction:03}")
}

fn units_to_millis(units: u64, timescale: u32) -> Result<u64, DashSubtitleError> {
    if timescale == 0 {
        return Err(DashSubtitleError::Unsupported(
            "A DASH subtitle track has a zero timescale",
        ));
    }
    let millis = u128::from(units)
        .checked_mul(1_000)
        .ok_or(DashSubtitleError::Unsupported(
            "A DASH subtitle timestamp overflows the supported time range",
        ))?
        / u128::from(timescale);
    u64::try_from(millis)
        .map_err(|_| DashSubtitleError::Unsupported("A DASH subtitle timestamp is out of range"))
}

fn clear_destination(destination: &Path) -> Result<(), DashSubtitleError> {
    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(DashSubtitleError::Failed(
                "The normalized DASH subtitle staging path is not a regular file".to_string(),
            ))
        }
        Ok(_) => fs::remove_file(destination).map_err(|error| {
            DashSubtitleError::Failed(format!(
                "Could not clear an old normalized DASH subtitle file: {error}"
            ))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(DashSubtitleError::Failed(format!(
            "Could not inspect normalized DASH subtitle staging: {error}"
        ))),
    }
}

fn write_private_output(destination: &Path, bytes: &[u8]) -> Result<(), DashSubtitleError> {
    if bytes.len() > MAX_OUTPUT_BYTES {
        return Err(DashSubtitleError::Unsupported(
            "The normalized DASH subtitle output exceeds the 16 MiB limit",
        ));
    }
    let parent = destination.parent().ok_or_else(|| {
        DashSubtitleError::Failed("The normalized DASH subtitle path has no parent".to_string())
    })?;
    let temporary = tempfile::NamedTempFile::new_in(parent).map_err(|error| {
        DashSubtitleError::Failed(format!(
            "Could not create private subtitle staging: {error}"
        ))
    })?;
    temporary.as_file().write_all(bytes).map_err(|error| {
        DashSubtitleError::Failed(format!(
            "Could not write normalized DASH subtitles: {error}"
        ))
    })?;
    temporary.as_file().sync_all().map_err(|error| {
        DashSubtitleError::Failed(format!(
            "Could not flush normalized DASH subtitles: {error}"
        ))
    })?;
    temporary.persist_noclobber(destination).map_err(|error| {
        DashSubtitleError::Failed(format!(
            "Could not publish normalized DASH subtitle staging without clobbering: {}",
            error.error
        ))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ttml(body: &str) -> Vec<u8> {
        format!(
            r#"<tt xmlns="{TTML_NAMESPACE}" xmlns:ttp="{TTML_PARAMETER_NAMESPACE}" ttp:timeBase="media"><body><div>{body}</div></body></tt>"#
        )
        .into_bytes()
    }

    #[test]
    fn recognizes_stpp_codec_profiles_without_accepting_wvtt() {
        assert!(is_stpp_codec("stpp"));
        assert!(is_stpp_codec("stpp.ttml.im1t"));
        assert!(is_stpp_codec("stpp, stpp.ttml.etd1"));
        assert!(!is_stpp_codec("wvtt"));
        assert!(!is_stpp_codec("stppx"));
        assert!(!is_stpp_codec(""));
    }

    #[test]
    fn parses_absolute_track_times_and_clips_to_mp4_sample_interval() {
        let xml = ttml(
            r#"<p begin="00:00:00" end="00:00:20">long cue</p><p begin="15s" dur="10s">short cue</p>"#,
        );
        let cues = parse_ttml_document(&xml, 10_000, 20_000).expect("parse TTML document");
        assert_eq!(
            cues,
            [
                Cue {
                    start_ms: 10_000,
                    end_ms: 20_000,
                    identifier: None,
                    text: "long cue".to_string(),
                },
                Cue {
                    start_ms: 15_000,
                    end_ms: 20_000,
                    identifier: None,
                    text: "short cue".to_string(),
                },
            ]
        );
    }

    #[test]
    fn supports_default_media_timebase_and_empty_ttml_documents() {
        let xml = format!(r#"<tt xmlns="{TTML_NAMESPACE}"/>"#);
        assert!(
            parse_ttml_document(xml.as_bytes(), 0, 1_000)
                .expect("empty TTML document")
                .is_empty()
        );
        let xml = format!(r#"<tt xmlns="{TTML_NAMESPACE}"><body/></tt>"#);
        assert!(
            parse_ttml_document(xml.as_bytes(), 0, 1_000)
                .expect("empty TTML body")
                .is_empty()
        );
    }

    #[test]
    fn rejects_non_media_timebases_and_inherited_or_sequential_timing() {
        let clock = format!(
            r#"<tt xmlns="{TTML_NAMESPACE}" xmlns:ttp="{TTML_PARAMETER_NAMESPACE}" ttp:timeBase="clock"><body><div><p>text</p></div></body></tt>"#
        );
        assert!(parse_ttml_document(clock.as_bytes(), 0, 1_000).is_err());
        let inherited = format!(
            r#"<tt xmlns="{TTML_NAMESPACE}"><body begin="1s"><div><p>text</p></div></body></tt>"#
        );
        assert!(parse_ttml_document(inherited.as_bytes(), 0, 1_000).is_err());
        let sequential = format!(
            r#"<tt xmlns="{TTML_NAMESPACE}"><body timeContainer="seq"><div><p>text</p></div></body></tt>"#
        );
        assert!(parse_ttml_document(sequential.as_bytes(), 0, 1_000).is_err());
    }

    #[test]
    fn rejects_styling_layout_frame_timing_and_external_xml() {
        let styled = ttml(r#"<p style="caption" begin="0s" end="1s">text</p>"#);
        assert!(parse_ttml_document(&styled, 0, 1_000).is_err());
        let positioned = ttml(r#"<p region="bottom" begin="0s" end="1s">text</p>"#);
        assert!(parse_ttml_document(&positioned, 0, 1_000).is_err());
        let frame = ttml(r#"<p begin="00:00:01:12" end="00:00:02:00">text</p>"#);
        assert!(parse_ttml_document(&frame, 0, 3_000).is_err());
        let external = format!(
            r#"<!DOCTYPE tt [<!ENTITY x "bad">]><tt xmlns="{TTML_NAMESPACE}"><body><div><p>&x;</p></div></body></tt>"#
        );
        assert!(parse_ttml_document(external.as_bytes(), 0, 1_000).is_err());
    }

    #[test]
    fn normalizes_plain_text_spans_and_line_breaks_and_escapes_vtt_markup() {
        let xml = ttml(
            r#"<p begin="0s" end="2s"> hello <span>world &amp; all</span><br/>next &lt;line&gt;</p>"#,
        );
        let cues = parse_ttml_document(&xml, 0, 2_000).expect("parse TTML text");
        assert_eq!(cues[0].text, "hello world & all\nnext <line>");
        assert_eq!(
            serialize_vtt(&cues),
            "WEBVTT\n\n00:00:00.000 --> 00:00:02.000\nhello world &amp; all\nnext &lt;line&gt;\n\n"
        );
    }

    #[test]
    fn accepts_simple_clock_and_offset_times_at_millisecond_precision() {
        assert_eq!(parse_ttml_time("01:02:03.004").expect("clock time"), 3_723_004);
        assert_eq!(parse_ttml_time("1.5h").expect("hour offset"), 5_400_000);
        assert_eq!(parse_ttml_time("2m").expect("minute offset"), 120_000);
        assert_eq!(parse_ttml_time("1.25s").expect("second offset"), 1_250);
        assert_eq!(parse_ttml_time("250ms").expect("millisecond offset"), 250);
        assert!(parse_ttml_time("00:00:01:12").is_err());
        assert!(parse_ttml_time("12f").is_err());
        assert!(parse_ttml_time("3t").is_err());
        assert!(parse_ttml_time("1.0001s").is_err());
    }

    #[test]
    fn clips_repeated_sample_cues_and_merges_adjacent_intervals() {
        let first = parse_ttml_document(
            &ttml(r#"<p begin="1s" end="4s" xml:id="same">repeat</p>"#),
            1_000,
            2_000,
        )
        .expect("first sample");
        let second = parse_ttml_document(
            &ttml(r#"<p begin="1s" end="4s" xml:id="same">repeat</p>"#),
            2_000,
            3_000,
        )
        .expect("second sample");
        let merged = merge_adjacent_cues([first, second].concat());
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].start_ms, 1_000);
        assert_eq!(merged[0].end_ms, 3_000);
    }

    #[test]
    fn rejects_malformed_xml_foreign_namespaces_and_excessive_nesting() {
        assert!(parse_ttml_document(b"<tt>", 0, 1_000).is_err());
        let foreign = br#"<tt xmlns="urn:other"><body/></tt>"#;
        assert!(parse_ttml_document(foreign, 0, 1_000).is_err());
        let nested = format!(
            r#"<tt xmlns="{TTML_NAMESPACE}"><body><div>{}</div></body></tt>"#,
            "<div>".repeat(MAX_XML_DEPTH + 1)
        );
        assert!(parse_ttml_document(nested.as_bytes(), 0, 1_000).is_err());
    }

    #[test]
    fn vtt_timestamps_preserve_hours_and_milliseconds() {
        assert_eq!(format_timestamp(3_723_004), "01:02:03.004");
        assert_eq!(format_timestamp(100 * 3_600_000), "100:00:00.000");
    }
}
