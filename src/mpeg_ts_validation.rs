//! Bounded validation of MPEG-TS packet prefixes used while an LL-HLS
//! recording is being staged. Completed tracks are demuxed by FFmpeg.

use mpeg_ts::ts::{AdaptationFieldControl, TS_PACKET_SIZE, TsPacket};

/// Incrementally validates complete, aligned 188-byte packets without retaining
/// the resource. This is only used as an early live-capture prefix guard; FFmpeg
/// owns validation of the completed recording.
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
    /// Create a validator for one packet prefix.
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

    /// Whether all complete packets observed so far have valid MPEG-TS headers.
    #[must_use]
    pub(crate) fn is_valid_prefix(&self) -> bool {
        self.valid && self.saw_packet && self.packet_len == 0
    }

    /// Whether one downloaded segment is a complete aligned MPEG-TS resource.
    /// This remains for SAMPLE-AES integration checks; FFmpeg handles ordinary
    /// HLS track demuxing and recovery.
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

        if adaptation_length > 0 && !packet.adaptation_field().is_some_and(|field| field.is_ok()) {
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

    #[test]
    fn recognizes_a_complete_packet_after_split_chunks() {
        let bytes = packet();
        let mut validator = TransportStreamValidator::new();
        validator.observe(&bytes[..73]);
        assert!(!validator.is_valid_prefix());
        validator.observe(&bytes[73..]);
        assert!(validator.is_valid_prefix());
    }

    #[test]
    fn rejects_an_invalid_packet_header() {
        let mut bytes = packet();
        bytes[3] = 0;
        let mut validator = TransportStreamValidator::new();
        validator.observe(&bytes);
        assert!(!validator.is_valid_prefix());
    }
}
