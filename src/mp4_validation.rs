//! Bounded structural validation for ISO Base Media File Format resources.

use std::io::{self, Read, Seek, SeekFrom};

use mp4box::{BoxRef, NodeKind};

/// Bound parser reads so a resource containing many tiny boxes cannot allocate
/// an unbounded box tree while being validated.
pub(crate) const MAX_PARSER_READ_BYTES: u64 = 256 * 1024;

/// Parse one ISO BMFF resource and require complete, contiguous box coverage.
///
/// The range may be a whole file or one resource inside a larger file. In
/// addition to the crate's parser diagnostics, Grab checks that every byte in
/// the range belongs to a parsed top-level box and that the resource contains
/// either an initialization segment (ftyp + moov) or a media fragment
/// (moof + mdat).
pub(crate) fn is_fmp4_resource<R>(reader: &mut R, start: u64, length: u64) -> bool
where
    R: Read + Seek,
{
    let Some(end) = start.checked_add(length) else {
        return false;
    };
    if length < 8 {
        return false;
    }

    let bounded_reader = BoundedReader::new(reader, start, end);
    let mut reader = ReadBudget::new(bounded_reader, MAX_PARSER_READ_BYTES);
    let Ok((boxes, issues)) = mp4box::parser::parse_boxes_tolerant(&mut reader, start, end) else {
        return false;
    };
    if !issues.is_empty() || boxes.is_empty() || !boxes_cover_range(&boxes, start, end) {
        return false;
    }

    let mut has_file_type = false;
    let mut has_movie = false;
    let mut has_movie_fragment = false;
    let mut has_media_data = false;

    for box_ref in &boxes {
        let box_type = box_ref.hdr.typ.0;
        has_file_type |= box_type == *b"ftyp";
        has_movie |= box_type == *b"moov";
        has_movie_fragment |= box_type == *b"moof";
        has_media_data |= box_type == *b"mdat";
    }

    (has_file_type && has_movie) || (has_movie_fragment && has_media_data)
}

fn boxes_cover_range(boxes: &[BoxRef], start: u64, end: u64) -> bool {
    let mut next_offset = start;

    for box_ref in boxes {
        let header = &box_ref.hdr;
        if header.start != next_offset {
            return false;
        }

        let Some(box_end) = box_end(header.start, header.size, end) else {
            return false;
        };
        let Some(header_end) = header.start.checked_add(header.header_size) else {
            return false;
        };
        if box_end <= header.start || header_end > box_end {
            return false;
        }

        let children_are_contiguous = match &box_ref.kind {
            NodeKind::Container(children) => {
                let Some(children_start) = header.start.checked_add(header.header_size) else {
                    return false;
                };
                boxes_cover_range(children, children_start, box_end)
            }
            NodeKind::FullContainer {
                data_offset,
                data_len,
                children,
                ..
            } => {
                if !payload_covers_box(*data_offset, *data_len, header_end, box_end) {
                    return false;
                }

                // stsd carries its entry_count after version/flags. Other
                // FullContainer types place child boxes immediately after flags.
                let prefix_len = if header.typ.0 == *b"stsd" { 4 } else { 0 };
                let Some(children_start) = data_offset.checked_add(prefix_len) else {
                    return false;
                };
                if children_start > box_end {
                    return false;
                }

                match children.first() {
                    Some(first_child) if first_child.hdr.start == children_start => {
                        boxes_cover_range(children, children_start, box_end)
                    }
                    Some(_) => false,
                    None => children_start == box_end,
                }
            }
            NodeKind::FullBox {
                data_offset,
                data_len,
                ..
            }
            | NodeKind::Leaf {
                data_offset,
                data_len,
            }
            | NodeKind::Unknown {
                data_offset,
                data_len,
            } => payload_covers_box(*data_offset, *data_len, header_end, box_end),
        };
        if !children_are_contiguous {
            return false;
        }

        next_offset = box_end;
    }

    next_offset == end
}

fn box_end(start: u64, size: u64, parent_end: u64) -> Option<u64> {
    if size == 0 {
        return Some(parent_end);
    }

    start.checked_add(size).filter(|end| *end <= parent_end)
}

fn payload_covers_box(data_offset: u64, data_len: u64, header_end: u64, box_end: u64) -> bool {
    data_offset >= header_end && data_offset.checked_add(data_len) == Some(box_end)
}

/// Restrict parser reads and seeks to one resource, even when the underlying
/// file contains adjacent resources or unrelated data.
pub(crate) struct BoundedReader<'reader, R> {
    inner: &'reader mut R,
    start: u64,
    end: u64,
}

impl<'reader, R> BoundedReader<'reader, R> {
    pub(crate) fn new(inner: &'reader mut R, start: u64, end: u64) -> Self {
        Self { inner, start, end }
    }
}

impl<R: Read + Seek> Read for BoundedReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }

        let position = self.inner.stream_position()?;
        if position < self.start || position > self.end {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "reader position is outside the ISO BMFF resource range",
            ));
        }

        let available = self.end - position;
        let allowed = usize::try_from(available)
            .unwrap_or(usize::MAX)
            .min(buffer.len());
        self.inner.read(&mut buffer[..allowed])
    }
}

impl<R: Read + Seek> Seek for BoundedReader<'_, R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let target = match position {
            SeekFrom::Start(position) => Some(position),
            SeekFrom::Current(offset) => self.inner.stream_position()?.checked_add_signed(offset),
            SeekFrom::End(offset) => self.end.checked_add_signed(offset),
        }
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek position overflows the ISO BMFF resource range",
            )
        })?;

        if !(self.start..=self.end).contains(&target) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek position is outside the ISO BMFF resource range",
            ));
        }

        self.inner.seek(SeekFrom::Start(target))
    }
}

/// Bound the total amount of box-header and metadata data consumed by the
/// parser. Media payloads are skipped by seeking and do not count against it.
pub(crate) struct ReadBudget<R> {
    inner: R,
    remaining: u64,
}

impl<R> ReadBudget<R> {
    pub(crate) fn new(inner: R, remaining: u64) -> Self {
        Self { inner, remaining }
    }
}

impl<R: Read> Read for ReadBudget<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "ISO BMFF parser read budget exhausted",
            ));
        }

        let allowed = usize::try_from(self.remaining)
            .unwrap_or(buffer.len())
            .min(buffer.len());
        let read = self.inner.read(&mut buffer[..allowed])?;
        self.remaining -= read as u64;
        Ok(read)
    }
}

impl<R: Seek> Seek for ReadBudget<R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.inner.seek(position)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Cursor, Read, Seek, SeekFrom};

    use super::{BoundedReader, MAX_PARSER_READ_BYTES, ReadBudget, is_fmp4_resource};

    fn box_bytes(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = u32::try_from(8 + payload.len()).expect("test box fits in a u32");
        let mut bytes = Vec::with_capacity(size as usize);
        bytes.extend_from_slice(&size.to_be_bytes());
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(payload);
        bytes
    }

    fn init_segment() -> Vec<u8> {
        let mut ftyp_payload = Vec::from(*b"isom");
        ftyp_payload.extend_from_slice(&0_u32.to_be_bytes());
        ftyp_payload.extend_from_slice(b"isom");
        let ftyp = box_bytes(b"ftyp", &ftyp_payload);
        let moov = box_bytes(b"moov", &[]);
        [ftyp, moov].concat()
    }

    fn init_segment_with_meta(meta_payload: &[u8]) -> Vec<u8> {
        let mut ftyp_payload = Vec::from(*b"isom");
        ftyp_payload.extend_from_slice(&0_u32.to_be_bytes());
        ftyp_payload.extend_from_slice(b"isom");
        let ftyp = box_bytes(b"ftyp", &ftyp_payload);
        let meta = box_bytes(b"meta", meta_payload);
        let moov = box_bytes(b"moov", &meta);
        [ftyp, moov].concat()
    }

    fn init_segment_with_empty_stsd(stsd_suffix: &[u8]) -> Vec<u8> {
        let mut ftyp_payload = Vec::from(*b"isom");
        ftyp_payload.extend_from_slice(&0_u32.to_be_bytes());
        ftyp_payload.extend_from_slice(b"isom");
        let ftyp = box_bytes(b"ftyp", &ftyp_payload);

        let mut stsd_payload = vec![0, 0, 0, 0];
        stsd_payload.extend_from_slice(&0_u32.to_be_bytes());
        stsd_payload.extend_from_slice(stsd_suffix);
        let stsd = box_bytes(b"stsd", &stsd_payload);
        let stbl = box_bytes(b"stbl", &stsd);
        let minf = box_bytes(b"minf", &stbl);
        let mdia = box_bytes(b"mdia", &minf);
        let trak = box_bytes(b"trak", &mdia);
        let moov = box_bytes(b"moov", &trak);
        [ftyp, moov].concat()
    }

    fn media_segment() -> Vec<u8> {
        let mut mfhd_payload = vec![0, 0, 0, 0];
        mfhd_payload.extend_from_slice(&1_u32.to_be_bytes());
        let mfhd = box_bytes(b"mfhd", &mfhd_payload);
        let moof = box_bytes(b"moof", &mfhd);
        let mdat = box_bytes(b"mdat", &[0x12, 0x34]);
        [moof, mdat].concat()
    }

    struct TrackingReader {
        inner: Cursor<Vec<u8>>,
        furthest_read: u64,
    }

    impl Read for TrackingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let start = self.inner.position();
            let read = self.inner.read(buffer)?;
            self.furthest_read = self.furthest_read.max(start.saturating_add(read as u64));
            Ok(read)
        }
    }

    impl Seek for TrackingReader {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.inner.seek(position)
        }
    }

    #[test]
    fn accepts_complete_initialization_and_media_segments() {
        let init = init_segment();
        assert!(is_fmp4_resource(
            &mut Cursor::new(init.clone()),
            0,
            init.len() as u64
        ));

        let media = media_segment();
        assert!(is_fmp4_resource(
            &mut Cursor::new(media.clone()),
            0,
            media.len() as u64
        ));
    }

    #[test]
    fn validates_a_resource_range_inside_a_larger_file() {
        let prefix = b"not part of the resource";
        let media = media_segment();
        let mut bytes = prefix.to_vec();
        bytes.extend_from_slice(&media);
        bytes.extend_from_slice(b"tail");
        assert!(is_fmp4_resource(
            &mut Cursor::new(bytes),
            prefix.len() as u64,
            media.len() as u64
        ));
    }

    #[test]
    fn parser_does_not_read_past_the_resource_end() {
        let mut bytes = media_segment();
        bytes.extend_from_slice(&1_u32.to_be_bytes());
        bytes.extend_from_slice(b"free");
        let resource_length = bytes.len() as u64;
        bytes.extend_from_slice(&64_u64.to_be_bytes());

        let mut reader = TrackingReader {
            inner: Cursor::new(bytes),
            furthest_read: 0,
        };
        assert!(!is_fmp4_resource(&mut reader, 0, resource_length));
        assert!(
            reader.furthest_read <= resource_length,
            "parser read through byte {} past resource end {}",
            reader.furthest_read,
            resource_length
        );
    }

    #[test]
    fn rejects_child_headers_that_overrun_their_parent() {
        let mut malformed = Vec::new();
        malformed.extend_from_slice(&16_u32.to_be_bytes());
        malformed.extend_from_slice(b"moof");
        malformed.extend_from_slice(&0_u32.to_be_bytes());
        malformed.extend_from_slice(b"uuid");
        malformed.extend_from_slice(&box_bytes(b"mdat", &[0x12, 0x34]));
        malformed.extend_from_slice(&box_bytes(b"free", &[]));

        assert!(!is_fmp4_resource(
            &mut Cursor::new(malformed.clone()),
            0,
            malformed.len() as u64
        ));
    }

    #[test]
    fn rejects_full_box_fields_that_overrun_their_parent() {
        let mut malformed = Vec::new();
        malformed.extend_from_slice(&17_u32.to_be_bytes());
        malformed.extend_from_slice(b"moof");
        malformed.extend_from_slice(&9_u32.to_be_bytes());
        malformed.extend_from_slice(b"mfhd");
        malformed.push(0);
        malformed.extend_from_slice(&box_bytes(b"mdat", &[0x12, 0x34]));

        assert!(!is_fmp4_resource(
            &mut Cursor::new(malformed.clone()),
            0,
            malformed.len() as u64
        ));
    }

    #[test]
    fn accepts_empty_full_containers_and_stsd_entry_count_prefix() {
        let empty_meta = init_segment_with_meta(&[0, 0, 0, 0]);
        assert!(is_fmp4_resource(
            &mut Cursor::new(empty_meta.clone()),
            0,
            empty_meta.len() as u64
        ));

        let empty_stsd = init_segment_with_empty_stsd(&[]);
        assert!(is_fmp4_resource(
            &mut Cursor::new(empty_stsd.clone()),
            0,
            empty_stsd.len() as u64
        ));
    }

    #[test]
    fn rejects_unparsed_full_container_payload_bytes() {
        let invalid_meta = init_segment_with_meta(&[0, 0, 0, 0, 0xde, 0xad, 0xbe, 0xef]);
        assert!(!is_fmp4_resource(
            &mut Cursor::new(invalid_meta.clone()),
            0,
            invalid_meta.len() as u64
        ));

        let invalid_stsd = init_segment_with_empty_stsd(&[0xde, 0xad, 0xbe, 0xef]);
        assert!(!is_fmp4_resource(
            &mut Cursor::new(invalid_stsd.clone()),
            0,
            invalid_stsd.len() as u64
        ));
    }

    #[test]
    fn rejects_malformed_overrunning_and_unparsed_box_data() {
        let mut malformed_child = media_segment();
        // Make the mfhd child claim more bytes than its enclosing moof contains.
        malformed_child[8..12].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(!is_fmp4_resource(
            &mut Cursor::new(malformed_child.clone()),
            0,
            malformed_child.len() as u64
        ));

        let mut trailing = media_segment();
        trailing.extend_from_slice(&[0, 1, 2, 3]);
        assert!(!is_fmp4_resource(
            &mut Cursor::new(trailing.clone()),
            0,
            trailing.len() as u64
        ));

        let truncated = b"moof";
        assert!(!is_fmp4_resource(
            &mut Cursor::new(truncated),
            0,
            truncated.len() as u64
        ));
    }

    #[test]
    fn rejects_structurally_valid_but_non_fmp4_resources() {
        let free = box_bytes(b"free", &[0; 8]);
        assert!(!is_fmp4_resource(
            &mut Cursor::new(free.clone()),
            0,
            free.len() as u64
        ));
    }

    #[test]
    fn parser_read_budget_bounds_tiny_box_input() {
        let ftyp_payload = [*b"isom", 0_u32.to_be_bytes(), *b"isom"].concat();
        let ftyp = box_bytes(b"ftyp", &ftyp_payload);
        let moov = box_bytes(b"moov", &[]);
        let mut bytes = Vec::new();
        for _ in 0..20_000 {
            bytes.extend_from_slice(&ftyp);
            bytes.extend_from_slice(&moov);
        }
        assert!(!is_fmp4_resource(
            &mut Cursor::new(bytes.clone()),
            0,
            bytes.len() as u64
        ));
    }

    #[test]
    fn high_level_box_tree_parser_stops_at_read_budget() {
        let box_count =
            usize::try_from(MAX_PARSER_READ_BYTES / 8 + 1).expect("test box count fits in usize");
        let mut bytes = Vec::with_capacity(box_count * 8);
        for _ in 0..box_count {
            bytes.extend_from_slice(&8_u32.to_be_bytes());
            bytes.extend_from_slice(b"free");
        }

        let track_bytes = u64::try_from(bytes.len()).expect("test track length fits in u64");
        let mut file = Cursor::new(bytes);
        let bounded_reader = BoundedReader::new(&mut file, 0, track_bytes);
        let mut parser_reader = ReadBudget::new(bounded_reader, MAX_PARSER_READ_BYTES);

        assert!(
            mp4box::get_boxes(&mut parser_reader, track_bytes, false).is_err(),
            "a box tree larger than the parser read budget must be rejected"
        );
    }
}
