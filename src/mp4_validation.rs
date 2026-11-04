//! Bounded structural validation for ISO Base Media File Format resources.

use std::io::{self, Read, Seek, SeekFrom};

use mp4box::{BoxRef, NodeKind};

/// Bound parser reads so a resource containing many tiny boxes cannot allocate
/// an unbounded box tree while being validated.
const MAX_PARSER_READ_BYTES: u64 = 256 * 1024;

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

    let mut reader = ReadBudget::new(reader, MAX_PARSER_READ_BYTES);
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
        match box_ref.hdr.typ.0 {
            *b"ftyp" => has_file_type = true,
            *b"moov" => has_movie = true,
            *b"moof" => has_movie_fragment = true,
            *b"mdat" => has_media_data = true,
            _ => {}
        }
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
        if box_end <= header.start {
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
                children,
                ..
            } => match children.first() {
                Some(first_child) if first_child.hdr.start >= *data_offset => {
                    boxes_cover_range(children, first_child.hdr.start, box_end)
                }
                Some(_) => false,
                None => true,
            },
            NodeKind::FullBox { .. } | NodeKind::Leaf { .. } | NodeKind::Unknown { .. } => true,
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

struct ReadBudget<R> {
    inner: R,
    remaining: u64,
}

impl<R> ReadBudget<R> {
    fn new(inner: R, remaining: u64) -> Self {
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
    use std::io::Cursor;

    use super::is_fmp4_resource;

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

    fn media_segment() -> Vec<u8> {
        let mut mfhd_payload = vec![0, 0, 0, 0];
        mfhd_payload.extend_from_slice(&1_u32.to_be_bytes());
        let mfhd = box_bytes(b"mfhd", &mfhd_payload);
        let moof = box_bytes(b"moof", &mfhd);
        let mdat = box_bytes(b"mdat", &[0x12, 0x34]);
        [moof, mdat].concat()
    }

    #[test]
    fn accepts_complete_initialization_and_media_segments() {
        let init = init_segment();
        assert!(is_fmp4_resource(&mut Cursor::new(init.clone()), 0, init.len() as u64));

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
}
