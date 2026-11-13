//! Bounded readers for ISO BMFF subtitle metadata parsing.

use std::io::{self, Read, Seek, SeekFrom};

/// Bound parser reads so untrusted subtitle box trees cannot allocate an
/// unbounded amount of metadata while being inspected.
pub(crate) const MAX_PARSER_READ_BYTES: u64 = 256 * 1024;

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
    use std::io::{Cursor, Read, Seek, SeekFrom};

    use super::{BoundedReader, MAX_PARSER_READ_BYTES, ReadBudget};

    #[test]
    fn bounded_reader_cannot_escape_its_resource_range() {
        let mut inner = Cursor::new(b"prefixpayloadsuffix".to_vec());
        let mut reader = BoundedReader::new(&mut inner, 6, 13);
        let mut buffer = [0_u8; 32];

        let read = reader.read(&mut buffer).expect("read bounded payload");
        assert_eq!(&buffer[..read], b"payload");
        assert_eq!(reader.read(&mut buffer).expect("read resource end"), 0);
        assert!(
            reader.seek(SeekFrom::Start(5)).is_err(),
            "a parser must not seek before the selected resource"
        );
        assert!(
            reader.seek(SeekFrom::Start(14)).is_err(),
            "a parser must not seek after the selected resource"
        );
    }

    #[test]
    fn read_budget_rejects_reads_after_its_limit() {
        let mut reader = ReadBudget::new(Cursor::new(b"payload".to_vec()), 3);
        let mut buffer = [0_u8; 8];

        assert_eq!(reader.read(&mut buffer).expect("read within budget"), 3);
        assert_eq!(&buffer[..3], b"pay");
        assert!(
            reader.read(&mut buffer).is_err(),
            "metadata parsing must stop when its total read budget is exhausted"
        );
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
