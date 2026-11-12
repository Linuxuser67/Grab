//! Bounded extraction of selected static DASH WVTT subtitle tracks.
//!
//! DASH planning and resource transfer remain protocol concerns. This module
//! consumes the already-assembled subtitle track and uses mp4box sample tables
//! for sample offsets and presentation times, then normalizes WVTT cue boxes
//! into WebVTT for the existing FFmpeg finalization path.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::mp4_reader::{BoundedReader, MAX_PARSER_READ_BYTES, ReadBudget};

/// Caps the assembled track before parsing its MP4 box tree.
pub(super) const MAX_TRACK_BYTES: u64 = 64 * 1024 * 1024;
/// Bounds the sample-table allocation performed by mp4box.
pub(super) const MAX_SAMPLE_COUNT: usize = 50_000;
/// Bounds decoded MP4 sample-table metadata while retaining support for the 50,000-sample limit.
pub(super) const MAX_SAMPLE_TABLE_READ_BYTES: u64 = 2 * 1024 * 1024;
/// Prevents one subtitle sample from dominating parsing memory.
pub(super) const MAX_SAMPLE_BYTES: usize = 256 * 1024;
/// Bounds total bytes read from subtitle samples.
pub(super) const MAX_SAMPLE_TOTAL_BYTES: u64 = 48 * 1024 * 1024;
/// Bounds cue allocation and sorting work.
pub(super) const MAX_CUE_COUNT: usize = 50_000;
/// Bounds owned cue text, identifiers, and settings.
pub(super) const MAX_CUE_TEXT_BYTES: usize = 12 * 1024 * 1024;
/// Bounds the normalized WebVTT staging file.
pub(super) const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
/// Bounds the nested cue boxes allocated for each sample.
const MAX_WVTT_BOXES_PER_SAMPLE: usize = 256;
/// Bounds the WebVTT configuration header copied from the sample entry.
const MAX_VTT_HEADER_BYTES: usize = 16 * 1024;

/// Failure while extracting one selected DASH WVTT subtitle track.
#[must_use]
#[derive(Debug, thiserror::Error)]
pub(crate) enum DashSubtitleError {
    #[error("{0}")]
    Unsupported(&'static str),
    #[error("{0}")]
    UnsupportedMp4(String),
    #[error("{0}")]
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Cue {
    start_ms: u64,
    end_ms: u64,
    identifier: Option<String>,
    settings: Option<String>,
    text: String,
}

/// Extract a selected DASH WVTT track to a private WebVTT staging file.
///
/// Returns false for a valid track with no non-empty cues. Unsupported or
/// ambiguous sample layouts are reported separately so the caller can use the
/// existing yt-dlp fallback without guessing at timestamps or sample offsets.
///
/// # Errors
///
/// Returns an unsupported error for codecs, timeline mappings, box layouts,
/// or extraction limits outside this initial native subset. Returns a failed
/// error when opening, reading, or staging files fails.
pub(crate) fn extract_to_vtt(
    source: &Path,
    destination: &Path,
    codecs: &str,
) -> Result<bool, DashSubtitleError> {
    clear_destination(destination)?;
    if !is_wvtt_codec(codecs) {
        return Err(DashSubtitleError::Unsupported(
            "Only WVTT subtitle samples are currently supported by the native DASH extractor",
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

    // Inspect fragment run counts before asking the sample-table API to
    // materialize per-sample metadata from untrusted input.
    let bounded_reader = BoundedReader::new(&mut file, 0, track_bytes);
    let mut parser_reader = ReadBudget::new(bounded_reader, MAX_PARSER_READ_BYTES);
    let boxes = mp4box::get_boxes(&mut parser_reader, track_bytes, false).map_err(|error| {
        DashSubtitleError::UnsupportedMp4(format!(
            "The staged DASH subtitle track has malformed or unsupported MP4 boxes: {error}"
        ))
    })?;
    if contains_box(&boxes, "edts") || contains_box(&boxes, "elst") {
        return Err(DashSubtitleError::Unsupported(
            "WVTT tracks with MP4 edit lists require presentation-time normalization",
        ));
    }
    if !has_fragmented_wvtt_structure(&boxes) {
        return Err(DashSubtitleError::Unsupported(
            "The staged DASH subtitle track is not a fragmented MP4 with media data",
        ));
    }
    if !has_single_wvtt_sample_entry(&boxes) {
        return Err(DashSubtitleError::Unsupported(
            "The staged DASH subtitle track does not have exactly one WVTT sample entry",
        ));
    }
    let header = read_vtt_header(&boxes, &mut file, track_bytes)?;
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
    let bounded_reader = BoundedReader::new(&mut file, 0, track_bytes);
    let parser_reader = ReadBudget::new(bounded_reader, MAX_SAMPLE_TABLE_READ_BYTES);
    let tracks = mp4box::track_samples_from_reader(parser_reader).map_err(|error| {
        DashSubtitleError::UnsupportedMp4(format!(
            "The staged DASH subtitle track has malformed or unsupported MP4 sample tables: {error}"
        ))
    })?;
    if tracks.len() != 1 || tracks[0].handler_type != "text" {
        return Err(DashSubtitleError::Unsupported(
            "The staged DASH resource does not contain exactly one ISO BMFF subtitle track",
        ));
    }

    let track = &tracks[0];
    let sample_count = usize::try_from(track.sample_count).map_err(|_| {
        DashSubtitleError::Unsupported("The DASH subtitle sample count is out of range")
    })?;
    if track.timescale == 0
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
    for sample in &track.samples {
        let sample_size = usize::try_from(sample.size).map_err(|_| {
            DashSubtitleError::Unsupported("A DASH subtitle sample size is out of range")
        })?;
        if sample_size > MAX_SAMPLE_BYTES {
            return Err(DashSubtitleError::Unsupported(
                "A DASH subtitle sample exceeds the 256 KiB sample limit",
            ));
        }
        let sample_end = sample
            .file_offset
            .checked_add(u64::from(sample.size))
            .filter(|end| *end <= track_bytes)
            .ok_or(DashSubtitleError::Unsupported(
                "A DASH subtitle sample points outside its assembled track",
            ))?;
        if sample_size == 0 {
            continue;
        }
        if !sample_is_within_media_data(&boxes, sample.file_offset, u64::from(sample.size)) {
            return Err(DashSubtitleError::Unsupported(
                "A WVTT subtitle sample points outside an MP4 media-data box",
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

        let end_units = sample
            .pts
            .checked_add(u64::from(sample.duration))
            .filter(|end| *end > sample.pts)
            .ok_or(DashSubtitleError::Unsupported(
                "A WVTT subtitle sample has an invalid presentation interval",
            ))?;
        let start_ms = units_to_millis(sample.pts, track.timescale)?;
        let end_ms = units_to_millis(end_units, track.timescale)?;
        for cue in parse_wvtt_sample(&bytes, start_ms, end_ms)? {
            add_cue(cue, &mut cues, &mut cue_text_bytes)?;
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
    let output = serialize_vtt(&header, &cues)?;
    write_private_output(destination, output.as_bytes())?;
    Ok(true)
}

/// Whether a DASH codec declaration contains only supported WVTT codec identifiers.
pub(crate) fn is_wvtt_codec(codecs: &str) -> bool {
    let codecs = codecs.trim();
    !codecs.is_empty()
        && codecs.split(',').all(|codec| {
            let codec = codec.trim().to_ascii_lowercase();
            codec == "wvtt" || codec.starts_with("wvtt.")
        })
}

pub(super) fn open_source(source: &Path) -> Result<File, DashSubtitleError> {
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

pub(super) fn contains_box(boxes: &[mp4box::Box], kind: &str) -> bool {
    boxes.iter().any(|box_| {
        box_.typ == kind
            || box_
                .children
                .as_deref()
                .is_some_and(|children| contains_box(children, kind))
    })
}

pub(super) fn count_boxes(boxes: &[mp4box::Box], kind: &str) -> usize {
    boxes
        .iter()
        .map(|box_| {
            (if box_.typ == kind { 1 } else { 0 })
                + box_
                    .children
                    .as_deref()
                    .map_or(0, |children| count_boxes(children, kind))
        })
        .sum()
}

pub(super) fn find_box<'boxes>(
    boxes: &'boxes [mp4box::Box],
    kind: &str,
) -> Option<&'boxes mp4box::Box> {
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

fn has_single_wvtt_sample_entry(boxes: &[mp4box::Box]) -> bool {
    if count_boxes(boxes, "stsd") != 1 {
        return false;
    }
    find_box(boxes, "stsd")
        .and_then(|stsd| stsd.children.as_deref())
        .is_some_and(|entries| entries.len() == 1 && entries[0].typ == "wvtt")
}

fn read_vtt_header(
    boxes: &[mp4box::Box],
    reader: &mut File,
    track_bytes: u64,
) -> Result<String, DashSubtitleError> {
    let stsd = find_box(boxes, "stsd").ok_or(DashSubtitleError::Unsupported(
        "The WVTT sample description is missing",
    ))?;
    let entries = stsd
        .children
        .as_deref()
        .ok_or(DashSubtitleError::Unsupported(
            "The WVTT sample description has no sample entry",
        ))?;
    let sample_entry = entries.first().filter(|entry| entry.typ == "wvtt").ok_or(
        DashSubtitleError::Unsupported("The WVTT sample entry is missing"),
    )?;
    let config_boxes = sample_entry
        .children
        .as_deref()
        .ok_or(DashSubtitleError::Unsupported(
            "The WVTT sample entry has no configuration box",
        ))?;
    let mut config = None;
    for box_ in config_boxes {
        match box_.typ.as_str() {
            "vttC" => {
                if config.replace(box_).is_some() {
                    return Err(DashSubtitleError::Unsupported(
                        "The WVTT sample entry contains multiple WebVTT configuration boxes",
                    ));
                }
            }
            // A source label only identifies the origin of this single track;
            // it does not change the standalone cue text.
            "vlab" => {}
            _ => {
                return Err(DashSubtitleError::Unsupported(
                    "The WVTT sample entry contains an unsupported extension box",
                ));
            }
        }
    }
    let config = config.ok_or(DashSubtitleError::Unsupported(
        "The WVTT sample entry has no WebVTT configuration box",
    ))?;
    let offset = config.payload_offset.ok_or(DashSubtitleError::Unsupported(
        "The WebVTT configuration box has no payload offset",
    ))?;
    let size = config.payload_size.ok_or(DashSubtitleError::Unsupported(
        "The WebVTT configuration box has no payload size",
    ))?;
    let end = offset
        .checked_add(size)
        .filter(|end| *end <= track_bytes)
        .ok_or(DashSubtitleError::Unsupported(
            "The WebVTT configuration box points outside its MP4 track",
        ))?;
    let size = usize::try_from(size).map_err(|_| {
        DashSubtitleError::Unsupported("The WebVTT configuration header size is out of range")
    })?;
    if size == 0 || size > MAX_VTT_HEADER_BYTES {
        return Err(DashSubtitleError::Unsupported(
            "The WebVTT configuration header is empty or exceeds the 16 KiB limit",
        ));
    }
    let mut bytes = vec![0_u8; size];
    reader
        .seek(SeekFrom::Start(offset))
        .and_then(|_| reader.read_exact(&mut bytes))
        .map_err(|error| {
            DashSubtitleError::Failed(format!(
                "Could not read the WebVTT configuration box ending at byte {end}: {error}"
            ))
        })?;
    normalize_vtt_header(&bytes)
}

fn normalize_vtt_header(bytes: &[u8]) -> Result<String, DashSubtitleError> {
    let header = std::str::from_utf8(bytes).map_err(|_| {
        DashSubtitleError::Unsupported("The WebVTT configuration header is not valid UTF-8")
    })?;
    let normalized = header.replace("\r\n", "\n").replace('\r', "\n");
    let normalized = normalized.strip_prefix('\u{feff}').unwrap_or(&normalized);
    let without_one_trailing_newline = normalized.strip_suffix('\n').unwrap_or(normalized);
    let first_line = without_one_trailing_newline
        .lines()
        .next()
        .unwrap_or_default();
    if without_one_trailing_newline.is_empty()
        || without_one_trailing_newline.ends_with('\n')
        || without_one_trailing_newline.contains("\n\n")
        || without_one_trailing_newline.contains('\0')
        || !(first_line == "WEBVTT"
            || first_line.starts_with("WEBVTT ")
            || first_line.starts_with("WEBVTT\t"))
    {
        return Err(DashSubtitleError::Unsupported(
            "The WebVTT configuration box does not contain a valid WebVTT file header",
        ));
    }
    Ok(without_one_trailing_newline.to_string())
}

fn has_fragmented_wvtt_structure(boxes: &[mp4box::Box]) -> bool {
    contains_box(boxes, "moof") && contains_box(boxes, "trun") && contains_box(boxes, "mdat")
}

pub(super) fn sample_is_within_media_data(
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

pub(super) fn count_fragment_samples(
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

pub(super) fn clear_destination(destination: &Path) -> Result<(), DashSubtitleError> {
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

pub(super) fn write_private_output(
    destination: &Path,
    bytes: &[u8],
) -> Result<(), DashSubtitleError> {
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
            "Could not publish the normalized DASH subtitle staging file without clobbering: {}",
            error.error
        ))
    })?;
    Ok(())
}

pub(super) fn units_to_millis(units: u64, timescale: u32) -> Result<u64, DashSubtitleError> {
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

fn add_cue(
    cue: Cue,
    cues: &mut Vec<Cue>,
    cue_text_bytes: &mut usize,
) -> Result<(), DashSubtitleError> {
    let text = cue
        .text
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim_end_matches('\n')
        .to_string();
    if text.trim().is_empty() {
        return Ok(());
    }
    if text.contains('\0') || cue.start_ms >= cue.end_ms {
        return Err(DashSubtitleError::Unsupported(
            "A DASH subtitle cue contains invalid text or timing",
        ));
    }
    if cues.len() >= MAX_CUE_COUNT {
        return Err(DashSubtitleError::Unsupported(
            "The DASH subtitle track exceeds the 50,000 cue limit",
        ));
    }
    if cue
        .identifier
        .as_ref()
        .is_some_and(|value| value.contains('\n') || value.contains('\r') || value.contains("-->"))
        || cue
            .settings
            .as_ref()
            .is_some_and(|value| value.contains('\n') || value.contains('\r'))
    {
        return Err(DashSubtitleError::Unsupported(
            "A WVTT cue contains an invalid identifier or settings line",
        ));
    }
    let cue_bytes = text
        .len()
        .checked_add(cue.identifier.as_ref().map_or(0, String::len))
        .and_then(|size| size.checked_add(cue.settings.as_ref().map_or(0, String::len)))
        .ok_or(DashSubtitleError::Unsupported(
            "The DASH subtitle cue text size overflows",
        ))?;
    *cue_text_bytes = cue_text_bytes
        .checked_add(cue_bytes)
        .filter(|size| *size <= MAX_CUE_TEXT_BYTES)
        .ok_or(DashSubtitleError::Unsupported(
            "The DASH subtitle cue text exceeds the 12 MiB limit",
        ))?;
    cues.push(Cue { text, ..cue });
    Ok(())
}

fn serialize_vtt(header: &str, cues: &[Cue]) -> Result<String, DashSubtitleError> {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(header.len().saturating_add(cues.len() * 64));
    output.push_str(header);
    output.push_str("\n\n");
    for cue in cues {
        if let Some(identifier) = cue.identifier.as_deref() {
            let _ = writeln!(output, "{identifier}");
        }
        let _ = write!(
            output,
            "{} --> {}",
            format_vtt_timestamp(cue.start_ms),
            format_vtt_timestamp(cue.end_ms)
        );
        if let Some(settings) = cue.settings.as_deref()
            && !settings.is_empty()
        {
            let _ = write!(output, " {settings}");
        }
        let _ = writeln!(output, "\n{}\n", cue.text);
        if output.len() > MAX_OUTPUT_BYTES {
            return Err(DashSubtitleError::Unsupported(
                "The normalized DASH subtitle output exceeds the 16 MiB limit",
            ));
        }
    }
    Ok(output)
}

fn format_vtt_timestamp(milliseconds: u64) -> String {
    let hours = milliseconds / 3_600_000;
    let minutes = (milliseconds / 60_000) % 60;
    let seconds = (milliseconds / 1_000) % 60;
    let millis = milliseconds % 1_000;
    format!("{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
}

#[derive(Clone, Copy)]
struct SampleBox<'a> {
    kind: [u8; 4],
    payload: &'a [u8],
}

fn parse_sample_boxes(bytes: &[u8]) -> Result<Vec<SampleBox<'_>>, DashSubtitleError> {
    let mut boxes = Vec::new();
    let mut offset = 0_usize;
    while offset < bytes.len() {
        if boxes.len() >= MAX_WVTT_BOXES_PER_SAMPLE {
            return Err(DashSubtitleError::Unsupported(
                "A WVTT sample contains too many cue boxes",
            ));
        }
        let header =
            bytes
                .get(offset..offset.saturating_add(8))
                .ok_or(DashSubtitleError::Unsupported(
                    "A WVTT sample contains a truncated box header",
                ))?;
        let size_32 = u32::from_be_bytes(header[..4].try_into().map_err(|_| {
            DashSubtitleError::Unsupported("A WVTT sample contains an invalid box size")
        })?);
        let kind: [u8; 4] = header[4..8].try_into().map_err(|_| {
            DashSubtitleError::Unsupported("A WVTT sample contains an invalid box type")
        })?;

        let (box_size, header_size) = match size_32 {
            0 => (bytes.len().saturating_sub(offset), 8_usize),
            1 => {
                let extended = bytes
                    .get(offset.saturating_add(8)..offset.saturating_add(16))
                    .ok_or(DashSubtitleError::Unsupported(
                        "A WVTT sample contains a truncated extended box header",
                    ))?;
                let size = u64::from_be_bytes(extended.try_into().map_err(|_| {
                    DashSubtitleError::Unsupported(
                        "A WVTT sample contains an invalid extended size",
                    )
                })?);
                (
                    usize::try_from(size).map_err(|_| {
                        DashSubtitleError::Unsupported("A WVTT sample box size is out of range")
                    })?,
                    16_usize,
                )
            }
            size => (
                usize::try_from(size).map_err(|_| {
                    DashSubtitleError::Unsupported("A WVTT sample box size is out of range")
                })?,
                8_usize,
            ),
        };
        if box_size < header_size {
            return Err(DashSubtitleError::Unsupported(
                "A WVTT sample contains a box smaller than its header",
            ));
        }
        let end = offset
            .checked_add(box_size)
            .filter(|end| *end <= bytes.len())
            .ok_or(DashSubtitleError::Unsupported(
                "A WVTT sample box extends beyond its sample",
            ))?;
        boxes.push(SampleBox {
            kind,
            payload: &bytes[offset + header_size..end],
        });
        offset = end;
    }
    Ok(boxes)
}

fn decode_wvtt_line(bytes: &[u8], label: &'static str) -> Result<String, DashSubtitleError> {
    let line = std::str::from_utf8(bytes).map_err(|_| {
        DashSubtitleError::Unsupported("A WVTT cue contains a non-UTF-8 text field")
    })?;
    if line.contains('\n') || line.contains('\r') {
        return Err(DashSubtitleError::Unsupported(label));
    }
    Ok(line.to_string())
}

fn parse_wvtt_sample(
    bytes: &[u8],
    start_ms: u64,
    end_ms: u64,
) -> Result<Vec<Cue>, DashSubtitleError> {
    let mut cues = Vec::new();
    for sample_box in parse_sample_boxes(bytes)? {
        match &sample_box.kind {
            b"vtte" if sample_box.payload.is_empty() => {}
            b"vttc" => {
                let children = parse_sample_boxes(sample_box.payload)?;
                let mut payload = None;
                let mut identifier = None;
                let mut settings = None;
                for child in children {
                    match &child.kind {
                        b"payl" if payload.is_none() => payload = Some(child.payload),
                        b"payl" => {
                            return Err(DashSubtitleError::Unsupported(
                                "A WVTT cue contains multiple payload boxes",
                            ));
                        }
                        b"iden" if identifier.is_none() => {
                            identifier = Some(decode_wvtt_line(
                                child.payload,
                                "A WVTT cue identifier contains a line break",
                            )?);
                        }
                        b"sttg" if settings.is_none() => {
                            settings = Some(decode_wvtt_line(
                                child.payload,
                                "A WVTT cue settings field contains a line break",
                            )?);
                        }
                        b"iden" | b"sttg" => {
                            return Err(DashSubtitleError::Unsupported(
                                "A WVTT cue contains duplicate identifier or settings boxes",
                            ));
                        }
                        // ctim changes the source cue interval; do not discard it
                        // until its exact time semantics can be represented.
                        b"ctim" => {
                            return Err(DashSubtitleError::Unsupported(
                                "WVTT cues with explicit cue-time boxes are not supported",
                            ));
                        }
                        _ => {
                            return Err(DashSubtitleError::Unsupported(
                                "A WVTT cue contains an unsupported child box",
                            ));
                        }
                    }
                }
                let payload = payload.ok_or(DashSubtitleError::Unsupported(
                    "A WVTT cue does not contain a cue-payload box",
                ))?;
                let text = std::str::from_utf8(payload).map_err(|_| {
                    DashSubtitleError::Unsupported("A WVTT cue payload is not valid UTF-8")
                })?;
                let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
                if normalized.contains("\n\n") {
                    return Err(DashSubtitleError::Unsupported(
                        "A WVTT cue payload contains more than one WebVTT cue",
                    ));
                }
                cues.push(Cue {
                    start_ms,
                    end_ms,
                    identifier,
                    settings,
                    text: text.to_string(),
                });
            }
            _ => {
                return Err(DashSubtitleError::Unsupported(
                    "A WVTT sample contains an unsupported sample box",
                ));
            }
        }
    }
    Ok(cues)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_box(typ: &str, children: Option<Vec<mp4box::Box>>) -> mp4box::Box {
        mp4box::Box {
            offset: 0,
            size: 8,
            header_size: 8,
            payload_offset: None,
            payload_size: None,
            typ: typ.to_string(),
            uuid: None,
            version: None,
            flags: None,
            kind: "container".to_string(),
            full_name: typ.to_string(),
            decoded: None,
            structured_data: None,
            field_spans: None,
            children,
        }
    }

    fn box_bytes(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = u32::try_from(8 + payload.len()).expect("test box fits in u32");
        let mut bytes = Vec::with_capacity(size as usize);
        bytes.extend_from_slice(&size.to_be_bytes());
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn wvtt_sample_uses_mp4_sample_timestamps_and_preserves_cue_fields() {
        let mut children = box_bytes(b"iden", b"cue-1");
        children.extend_from_slice(&box_bytes(b"sttg", b"align:start"));
        children.extend_from_slice(&box_bytes(b"payl", b"Hello\nworld"));
        let sample = box_bytes(b"vttc", &children);
        let cues = parse_wvtt_sample(&sample, 1_250, 2_500).expect("parse WVTT cue");
        assert_eq!(
            cues,
            [Cue {
                start_ms: 1_250,
                end_ms: 2_500,
                identifier: Some("cue-1".to_string()),
                settings: Some("align:start".to_string()),
                text: "Hello\nworld".to_string()
            }]
        );
    }

    #[test]
    fn wvtt_empty_cue_samples_produce_no_cues() {
        let sample = box_bytes(b"vtte", &[]);
        assert!(
            parse_wvtt_sample(&sample, 0, 1_000)
                .expect("parse empty WVTT cue")
                .is_empty()
        );
        assert!(
            parse_wvtt_sample(&[], 0, 1_000)
                .expect("parse empty sample")
                .is_empty()
        );
    }

    #[test]
    fn wvtt_rejects_truncated_and_unknown_sample_boxes() {
        assert!(parse_wvtt_sample(b"\0\0\0", 0, 1_000).is_err());
        let unknown = box_bytes(b"free", b"data");
        assert!(parse_wvtt_sample(&unknown, 0, 1_000).is_err());
    }

    #[test]
    fn wvtt_payload_cannot_smuggle_multiple_text_cues() {
        let sample = box_bytes(b"vttc", &box_bytes(b"payl", b"first\n\nsecond"));
        assert!(parse_wvtt_sample(&sample, 0, 1_000).is_err());
        let missing_payload = box_bytes(b"vttc", &box_bytes(b"iden", b"cue-1"));
        assert!(parse_wvtt_sample(&missing_payload, 0, 1_000).is_err());
    }

    #[test]
    fn wvtt_codecs_are_recognized_without_accepting_stpp() {
        assert!(is_wvtt_codec("wvtt"));
        assert!(is_wvtt_codec("wvtt.00.00.00"));
        assert!(is_wvtt_codec("wvtt, wvtt.00.00.00"));
        assert!(!is_wvtt_codec("stpp.ttml.im1t"));
        assert!(!is_wvtt_codec(""));
    }

    #[test]
    fn sample_description_must_be_one_wvtt_entry() {
        let boxes = [make_box(
            "moov",
            Some(vec![make_box(
                "trak",
                Some(vec![make_box(
                    "mdia",
                    Some(vec![make_box(
                        "minf",
                        Some(vec![make_box(
                            "stbl",
                            Some(vec![make_box(
                                "stsd",
                                Some(vec![make_box("wvtt", Some(Vec::new()))]),
                            )]),
                        )]),
                    )]),
                )]),
            )]),
        )];
        assert!(has_single_wvtt_sample_entry(&boxes));

        let mixed_entries = [make_box(
            "moov",
            Some(vec![make_box(
                "trak",
                Some(vec![make_box(
                    "mdia",
                    Some(vec![make_box(
                        "minf",
                        Some(vec![make_box(
                            "stbl",
                            Some(vec![make_box(
                                "stsd",
                                Some(vec![
                                    make_box("wvtt", Some(Vec::new())),
                                    make_box("stpp", Some(Vec::new())),
                                ]),
                            )]),
                        )]),
                    )]),
                )]),
            )]),
        )];
        assert!(!has_single_wvtt_sample_entry(&mixed_entries));
    }

    #[test]
    fn fragmented_track_requires_moof_trun_and_mdat() {
        let boxes = [
            make_box(
                "moof",
                Some(vec![make_box("traf", Some(vec![make_box("trun", None)]))]),
            ),
            make_box("mdat", None),
        ];
        assert!(has_fragmented_wvtt_structure(&boxes));
        assert!(!has_fragmented_wvtt_structure(&boxes[..1]));
    }

    #[test]
    fn subtitle_samples_must_fit_inside_one_mdat_payload() {
        let media_data = make_box("mdat", None);
        let boxes = [mp4box::Box {
            payload_offset: Some(100),
            payload_size: Some(20),
            ..media_data
        }];
        assert!(sample_is_within_media_data(&boxes, 100, 20));
        assert!(sample_is_within_media_data(&boxes, 110, 5));
        assert!(!sample_is_within_media_data(&boxes, 90, 11));
        assert!(!sample_is_within_media_data(&boxes, 119, 2));
        assert!(!sample_is_within_media_data(&boxes, u64::MAX, 1));
    }

    #[test]
    fn rejects_unknown_wvtt_sample_entry_extensions() {
        let directory = tempfile::tempdir().expect("create test directory");
        let path = directory.path().join("subtitle-init.mp4");
        let header = b"WEBVTT";
        std::fs::write(&path, header).expect("write configuration payload");
        let config = mp4box::Box {
            payload_offset: Some(0),
            payload_size: Some(u64::try_from(header.len()).expect("header fits")),
            ..make_box("vttC", None)
        };
        let boxes = [make_box(
            "moov",
            Some(vec![make_box(
                "trak",
                Some(vec![make_box(
                    "mdia",
                    Some(vec![make_box(
                        "minf",
                        Some(vec![make_box(
                            "stbl",
                            Some(vec![make_box(
                                "stsd",
                                Some(vec![make_box(
                                    "wvtt",
                                    Some(vec![config, make_box("free", None)]),
                                )]),
                            )]),
                        )]),
                    )]),
                )]),
            )]),
        )];
        let mut file = File::open(&path).expect("open subtitle init");
        assert!(
            read_vtt_header(
                &boxes,
                &mut file,
                u64::try_from(header.len()).expect("track length fits")
            )
            .is_err()
        );
    }

    #[test]
    fn reads_webvtt_header_from_the_sample_entry_configuration_box() {
        let directory = tempfile::tempdir().expect("create test directory");
        let path = directory.path().join("subtitle-init.mp4");
        let prefix = b"box-prefix";
        let header = b"WEBVTT\r\nKind: captions\r\n";
        let mut bytes = prefix.to_vec();
        bytes.extend_from_slice(header);
        std::fs::write(&path, &bytes).expect("write configuration payload");

        let config = mp4box::Box {
            payload_offset: Some(u64::try_from(prefix.len()).expect("prefix fits")),
            payload_size: Some(u64::try_from(header.len()).expect("header fits")),
            ..make_box("vttC", None)
        };
        let boxes = [make_box(
            "moov",
            Some(vec![make_box(
                "trak",
                Some(vec![make_box(
                    "mdia",
                    Some(vec![make_box(
                        "minf",
                        Some(vec![make_box(
                            "stbl",
                            Some(vec![make_box(
                                "stsd",
                                Some(vec![make_box("wvtt", Some(vec![config]))]),
                            )]),
                        )]),
                    )]),
                )]),
            )]),
        )];
        let mut file = File::open(&path).expect("open subtitle init");
        assert_eq!(
            read_vtt_header(
                &boxes,
                &mut file,
                u64::try_from(bytes.len()).expect("track length fits")
            )
            .expect("read WebVTT header"),
            "WEBVTT\nKind: captions"
        );
    }

    #[test]
    fn fragment_sample_count_is_read_and_bounded_before_sample_materialization() {
        let directory = tempfile::tempdir().expect("create test directory");
        let path = directory.path().join("trun.bin");
        let mut payload = vec![0_u8; 4];
        payload.extend_from_slice(&3_u32.to_be_bytes());
        std::fs::write(&path, &payload).expect("write sample count");
        let mut count = 0;
        {
            let mut reader = File::open(&path).expect("open sample count");
            let run = mp4box::Box {
                payload_offset: Some(0),
                payload_size: Some(8),
                ..make_box("trun", None)
            };
            count_fragment_samples(&[run], &mut reader, &mut count).expect("count bounded samples");
        }
        assert_eq!(count, 3);

        let oversized = mp4box::Box {
            payload_offset: Some(0),
            payload_size: Some(8),
            ..make_box("trun", None)
        };
        let mut oversized_payload = vec![0_u8; 4];
        oversized_payload.extend_from_slice(&u32::MAX.to_be_bytes());
        std::fs::write(&path, &oversized_payload).expect("write oversized sample count");
        let mut reader = File::open(&path).expect("reopen oversized sample count");
        let mut count = 0;
        assert!(count_fragment_samples(&[oversized], &mut reader, &mut count).is_err());
    }

    #[test]
    fn zero_sample_timescale_returns_unsupported_instead_of_dividing_by_zero() {
        assert!(units_to_millis(1, 0).is_err());
    }

    #[test]
    fn vtt_configuration_header_is_preserved_and_validated() {
        assert_eq!(
            normalize_vtt_header(b"WEBVTT\r\nKind: subtitles\r\n")
                .expect("normalize a valid configuration header"),
            "WEBVTT\nKind: subtitles"
        );
        assert_eq!(
            normalize_vtt_header(b"\xef\xbb\xbfWEBVTT").expect("accept a UTF-8 BOM"),
            "WEBVTT"
        );
        assert!(normalize_vtt_header(b"NOTE not a header").is_err());
        assert!(normalize_vtt_header(b"WEBVTT\n\nKind: subtitles").is_err());
        assert!(normalize_vtt_header(b"WEBVTT\n\n").is_err());
    }

    #[test]
    fn vtt_serialization_writes_timestamps_settings_and_ids() {
        let output = serialize_vtt(
            "WEBVTT\nKind: subtitles",
            &[Cue {
                start_ms: 3_723_004,
                end_ms: 3_724_005,
                identifier: Some("cue-1".to_string()),
                settings: Some("align:start".to_string()),
                text: "A cue".to_string(),
            }],
        )
        .expect("serialize WebVTT");
        assert_eq!(
            output,
            "WEBVTT\nKind: subtitles\n\ncue-1\n01:02:03.004 --> 01:02:04.005 align:start\nA cue\n\n"
        );
    }
}
