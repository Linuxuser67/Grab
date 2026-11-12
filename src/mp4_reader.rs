//! Bounded readers shared by fragmented-MP4 subtitle sample parsing.

use std::io::{self, Read, Seek, SeekFrom};

/// Bound parser reads so a resource containing many tiny boxes cannot allocate
/// an unbounded box tree while being inspected.
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
    fn bounded_reader_stops_at_the_resource_end() {
        let mut source = Cursor::new(b"prefixresourcesuffix".to_vec());
        let start = 6;
        let end = start + u64::try_from(b"resource".len()).expect("literal length fits u64");
        source
            .seek(SeekFrom::Start(start))
            .expect("seek to resource start");
        let mut reader = BoundedReader::new(&mut source, start, end);
        let mut buffer = [0_u8; 32];

        let read = reader.read(&mut buffer).expect("read bounded resource");
        assert_eq!(&buffer[..read], b"resource");
        assert_eq!(reader.read(&mut buffer).expect("read resource end"), 0);
        assert_eq!(source.position(), end);
    }

    #[test]
    fn bounded_reader_rejects_seeks_outside_the_resource() {
        let mut source = Cursor::new(b"prefixresourcesuffix".to_vec());
        let start = 6;
        let end = start + 8;
        let mut reader = BoundedReader::new(&mut source, start, end);

        assert!(reader.seek(SeekFrom::Start(start - 1)).is_err());
        assert!(reader.seek(SeekFrom::End(1)).is_err());
        assert_eq!(reader.seek(SeekFrom::End(0)).expect("seek to resource end"), end);
    }

    #[test]
    fn read_budget_rejects_reads_after_its_limit() {
        let mut reader = ReadBudget::new(Cursor::new(b"abcde".to_vec()), 3);
        let mut buffer = [0_u8; 5];

        assert_eq!(reader.read(&mut buffer).expect("read budgeted bytes"), 3);
        let error = reader.read(&mut buffer).expect_err("budget must be exhausted");
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn mp4box_parser_stops_at_the_shared_read_budget() {
        let box_count =
            usize::try_from(MAX_PARSER_READ_BYTES / 8 + 1).expect("test box count fits usize");
        let mut bytes = Vec::with_capacity(box_count * 8);
        for _ in 0..box_count {
            bytes.extend_from_slice(&8_u32.to_be_bytes());
            bytes.extend_from_slice(b"free");
        }

        let track_bytes = u64::try_from(bytes.len()).expect("test track length fits u64");
        let mut file = Cursor::new(bytes);
        let bounded_reader = BoundedReader::new(&mut file, 0, track_bytes);
        let mut parser_reader = ReadBudget::new(bounded_reader, MAX_PARSER_READ_BYTES);

        assert!(
            mp4box::get_boxes(&mut parser_reader, track_bytes, false).is_err(),
            "subtitle box parsing must stay within the shared read budget"
        );
    }
}
