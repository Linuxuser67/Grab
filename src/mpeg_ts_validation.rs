//! Validation of MPEG-2 transport-stream packets used by native HLS downloads.

use mpeg_ts::ts::{AdaptationFieldControl, TS_PACKET_SIZE, TsPacket};

/// Incrementally validates aligned MPEG-TS packets without retaining the segment.
pub(crate) struct TransportStreamValidator {
    packet: [u8; TS_PACKET_SIZE],
    packet_len: usize,
    total_bytes: u64,
    saw_packet: bool,
    valid: bool,
}

impl Default for TransportStreamValidator {
    fn default() -> Self {
        Self::new()
    }
}

impl TransportStreamValidator {
    /// Create a validator for one complete transport-stream resource.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            packet: [0; TS_PACKET_SIZE],
            packet_len: 0,
            total_bytes: 0,
            saw_packet: false,
            valid: true,
        }
    }

    /// Add an arbitrary body chunk, including chunks that split packet boundaries.
    pub(crate) fn observe(&mut self, mut chunk: &[u8]) {
        let Ok(chunk_len) = u64::try_from(chunk.len()) else {
            self.valid = false;
            return;
        };
        let Some(total_bytes) = self.total_bytes.checked_add(chunk_len) else {
            self.valid = false;
            return;
        };
        self.total_bytes = total_bytes;

        while !chunk.is_empty() {
            let remaining = TS_PACKET_SIZE - self.packet_len;
            let copied = remaining.min(chunk.len());
            let end = self.packet_len + copied;
            self.packet[self.packet_len..end].copy_from_slice(&chunk[..copied]);
            self.packet_len = end;
            chunk = &chunk[copied..];

            if self.packet_len == TS_PACKET_SIZE {
                self.saw_packet = true;
                if self.valid && !is_valid_packet(&self.packet) {
                    self.valid = false;
                }
                self.packet_len = 0;
            }
        }
    }

    /// Whether all observed bytes consist of one or more complete valid packets.
    #[must_use]
    pub(crate) fn is_valid_prefix(&self) -> bool {
        self.valid && self.saw_packet && self.packet_len == 0
    }

    /// Whether the observed body is a complete, aligned transport-stream resource.
    #[must_use]
    pub(crate) fn is_valid_complete(&self, expected_size: u64) -> bool {
        self.is_valid_prefix()
            && self.total_bytes == expected_size
            && expected_size >= TS_PACKET_SIZE as u64
            && expected_size.is_multiple_of(TS_PACKET_SIZE as u64)
    }
}

fn is_valid_packet(bytes: &[u8; TS_PACKET_SIZE]) -> bool {
    let Ok(packet) = TsPacket::parse(bytes) else {
        return false;
    };

    if packet.header.tei
        || packet.header.adaptation_field_control() == AdaptationFieldControl::Reserved
    {
        return false;
    }

    if packet.header.has_adaptation {
        let adaptation_length = usize::from(packet.raw[4]);
        if adaptation_length > TS_PACKET_SIZE - 5 {
            return false;
        }

        // An adaptation-only packet consumes the rest of the packet; when a
        // payload is also present, at least one payload byte must remain.
        let invalid_adaptation_length = if packet.header.has_payload {
            adaptation_length >= TS_PACKET_SIZE - 5
        } else {
            adaptation_length != TS_PACKET_SIZE - 5
        };
        if invalid_adaptation_length {
            return false;
        }

        if adaptation_length > 0
            && !packet
                .adaptation_field()
                .is_some_and(|field| field.is_ok())
        {
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::TransportStreamValidator;
    use mpeg_ts::ts::TS_PACKET_SIZE;

    fn packet() -> [u8; TS_PACKET_SIZE] {
        let mut packet = [0xff; TS_PACKET_SIZE];
        packet[0] = 0x47;
        packet[1] = 0;
        packet[2] = 0;
        packet[3] = 0x10;
        packet
    }

    fn validate_chunks(bytes: &[u8], chunk_size: usize) -> bool {
        let mut validator = TransportStreamValidator::new();
        for chunk in bytes.chunks(chunk_size) {
            validator.observe(chunk);
        }
        validator.is_valid_complete(bytes.len() as u64)
    }

    #[test]
    fn validates_packets_across_arbitrary_chunk_boundaries() {
        let mut bytes = Vec::with_capacity(TS_PACKET_SIZE * 2);
        bytes.extend_from_slice(&packet());
        bytes.extend_from_slice(&packet());
        assert!(validate_chunks(&bytes, 1));
        assert!(validate_chunks(&bytes, 73));
        assert!(validate_chunks(&bytes, TS_PACKET_SIZE + 17));

        bytes[TS_PACKET_SIZE + 3] = 0;
        assert!(!validate_chunks(&bytes, 73));
    }

    #[test]
    fn rejects_transport_error_and_reserved_adaptation_control() {
        let mut bytes = packet();
        bytes[1] |= 0x80;
        assert!(!validate_chunks(&bytes, TS_PACKET_SIZE));

        let mut bytes = packet();
        bytes[3] = 0;
        assert!(!validate_chunks(&bytes, TS_PACKET_SIZE));
    }

    #[test]
    fn validates_adaptation_only_packet_length() {
        let mut bytes = packet();
        bytes[3] = 0x20;
        bytes[4] = 183;
        bytes[5] = 0;
        assert!(validate_chunks(&bytes, TS_PACKET_SIZE));

        bytes[4] = 182;
        assert!(!validate_chunks(&bytes, TS_PACKET_SIZE));
    }

    #[test]
    fn validates_present_adaptation_fields() {
        let mut bytes = packet();
        bytes[3] = 0x30; // adaptation field followed by payload
        bytes[4] = 1;
        bytes[5] = 0; // flags with no optional fields
        assert!(validate_chunks(&bytes, TS_PACKET_SIZE));

        bytes[5] = 0x10; // PCR flag requires six more bytes
        assert!(!validate_chunks(&bytes, TS_PACKET_SIZE));

        bytes[5] = 0;
        bytes[4] = 184; // adaptation length cannot exceed the packet body
        assert!(!validate_chunks(&bytes, TS_PACKET_SIZE));

        bytes[3] = 0x30;
        bytes[4] = 183; // adaptation plus payload must leave at least one byte
        assert!(!validate_chunks(&bytes, TS_PACKET_SIZE));
    }

    #[test]
    fn rejects_incomplete_and_misaligned_resources() {
        let mut validator = TransportStreamValidator::new();
        let bytes = packet();
        validator.observe(&bytes[..TS_PACKET_SIZE - 1]);
        assert!(!validator.is_valid_prefix());
        assert!(!validator.is_valid_complete(TS_PACKET_SIZE as u64));

        let mut validator = TransportStreamValidator::new();
        validator.observe(&bytes);
        assert!(!validator.is_valid_complete((TS_PACKET_SIZE + 1) as u64));
        assert!(validator.is_valid_complete(TS_PACKET_SIZE as u64));
    }
}
