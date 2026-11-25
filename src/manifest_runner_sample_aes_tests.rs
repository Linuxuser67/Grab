//! Focused tests for the preflight protecting iori-ssa's SAMPLE-AES parser.

use super::{
    MAX_SAMPLE_AES_SEGMENT_BYTES, ManifestRunError, read_sample_aes_resource,
    validate_iori_adts_pes, validate_iori_sample_aes_segment,
};
use mpeg2ts::{
    es::{StreamId, StreamType},
    pes::PesHeader,
    ts::{
        ContinuityCounter, EsInfo, Pid, ProgramAssociation, TransportScramblingControl, TsHeader,
        TsPacket, TsPacketWriter, TsPayload, VersionNumber, WriteTsPacket,
        payload::{Bytes, Pat, Pes, Pmt},
    },
};

const PMT_PID: u16 = 480;
const AAC_PID: u16 = 257;
// Smaller than the TS payload capacity so the fixture uses multiple Raw packets.
const RAW_CONTINUATION_CHUNK_SIZE: usize = 130;

fn adts_header(frame_length: usize) -> [u8; 7] {
    assert!(frame_length <= 0x1fff, "ADTS length is a 13-bit field");
    [
        0xff,
        0xf1,
        0x50,
        u8::try_from((frame_length >> 11) & 0x03).expect("two-bit field fits"),
        u8::try_from((frame_length >> 3) & 0xff).expect("eight-bit field fits"),
        u8::try_from(((frame_length & 0x07) << 5) | 0x1f).expect("ADTS length bits fit"),
        0xfc,
    ]
}

fn adts_frame(frame_length: usize) -> Vec<u8> {
    let mut frame = adts_header(frame_length).to_vec();
    frame.resize(frame_length.max(7), 0);
    frame
}

fn ts_packet(pid: u16, continuity_counter: u8, payload: TsPayload) -> TsPacket {
    TsPacket {
        header: TsHeader {
            transport_error_indicator: false,
            transport_priority: false,
            pid: Pid::new(pid).expect("fixture PID is valid"),
            transport_scrambling_control: TransportScramblingControl::NotScrambled,
            continuity_counter: ContinuityCounter::from_u8(continuity_counter)
                .expect("fixture continuity counter is valid"),
        },
        adaptation_field: None,
        payload: Some(payload),
    }
}

fn sample_aes_aac_transport_stream(split_at: usize, elementary_stream: &[u8]) -> Vec<u8> {
    let (pes_data, continuation) = elementary_stream.split_at(split_at);
    let mut writer = TsPacketWriter::new(Vec::new());

    let pat = Pat {
        transport_stream_id: 1,
        version_number: VersionNumber::new(),
        table: vec![ProgramAssociation {
            program_num: 1,
            program_map_pid: Pid::new(PMT_PID).expect("fixture PMT PID is valid"),
        }],
    };
    writer
        .write_ts_packet(&ts_packet(0, 0, TsPayload::Pat(pat)))
        .expect("write PAT");

    let pmt = Pmt {
        program_num: 1,
        pcr_pid: Some(Pid::new(AAC_PID).expect("fixture AAC PID is valid")),
        version_number: VersionNumber::new(),
        program_info: Vec::new(),
        es_info: vec![EsInfo {
            stream_type: StreamType::AdtsAac,
            elementary_pid: Pid::new(AAC_PID).expect("fixture AAC PID is valid"),
            descriptors: Vec::new(),
        }],
    };
    writer
        .write_ts_packet(&ts_packet(PMT_PID, 0, TsPayload::Pmt(pmt)))
        .expect("write PMT");

    let pes_header = PesHeader {
        stream_id: StreamId::new_audio(StreamId::AUDIO_MIN).expect("valid audio stream id"),
        priority: false,
        data_alignment_indicator: false,
        copyright: false,
        original_or_copy: false,
        pts: None,
        dts: None,
        escr: None,
    };
    let pes_packet_len = u16::try_from(3 + pes_data.len()).expect("fixture PES length fits");
    let pes = Pes {
        header: pes_header,
        pes_packet_len,
        data: Bytes::new(pes_data).expect("fixture PES payload fits in one TS packet"),
    };
    writer
        .write_ts_packet(&ts_packet(AAC_PID, 0, TsPayload::Pes(pes)))
        .expect("write PES start");

    for (index, chunk) in continuation.chunks(RAW_CONTINUATION_CHUNK_SIZE).enumerate() {
        let continuity_counter = u8::try_from(index + 1).expect("fixture continuity counter fits");
        writer
            .write_ts_packet(&ts_packet(
                AAC_PID,
                continuity_counter,
                TsPayload::Raw(
                    Bytes::new(chunk).expect("fixture continuation fits in one TS packet"),
                ),
            ))
            .expect("write PES continuation");
    }

    writer.into_stream()
}

#[test]
fn rejects_zero_length_adts_frame() {
    assert!(
        validate_iori_adts_pes(&adts_header(0)).is_err(),
        "a zero-length frame must not leave iori-ssa's parser loop stationary"
    );
}

#[test]
fn rejects_zero_length_adts_frame_in_transport_stream() {
    let pes_data = adts_header(0);
    let transport_stream = sample_aes_aac_transport_stream(pes_data.len(), &pes_data);

    assert!(
        validate_iori_sample_aes_segment(&transport_stream).is_err(),
        "the transport-stream preflight must reject the malformed AAC PES"
    );
}

#[test]
fn rejects_adts_frame_shorter_than_its_header() {
    assert!(
        validate_iori_adts_pes(&adts_header(6)).is_err(),
        "the declared frame length must include its seven-byte ADTS header"
    );
}

#[test]
fn rejects_adts_frame_longer_than_the_remaining_pes_data() {
    assert!(
        validate_iori_adts_pes(&adts_header(8)).is_err(),
        "the declared frame length must fit within the remaining PES data"
    );
}

#[test]
fn accepts_multiple_complete_adts_frames_in_one_pes() {
    let mut pes = adts_frame(17);
    pes.extend(adts_frame(31));
    assert!(
        validate_iori_adts_pes(&pes).is_ok(),
        "all complete frames in a PES should be accepted"
    );
}

#[test]
fn accepts_adts_frame_split_across_pes_and_raw_continuations() {
    // The PES data ends inside the first frame; the following TS Raw payload
    // complete it and contain the second frame.
    let mut elementary_stream = adts_frame(300);
    elementary_stream.extend(adts_frame(59));
    assert_eq!(elementary_stream.len(), 359);

    let transport_stream = sample_aes_aac_transport_stream(100, &elementary_stream);
    assert!(
        validate_iori_sample_aes_segment(&transport_stream).is_ok(),
        "the PES assembler must validate frames after joining Raw continuations"
    );
}

#[test]
fn rejects_id3_and_packed_audio_before_iori_ssa() {
    let id3_tag = b"ID3\x04\x00\x00\x00\x00\x00\x00";
    let packed_audio_frame = [0xff, 0xfb, 0x90, 0x64];

    assert!(
        validate_iori_sample_aes_segment(id3_tag).is_err(),
        "an ID3-tagged packed-audio resource is not a transport stream"
    );
    assert!(
        validate_iori_sample_aes_segment(&packed_audio_frame).is_err(),
        "a packed MPEG-audio frame is not a transport stream"
    );
}

#[tokio::test]
async fn rejects_sample_aes_segment_over_the_in_memory_limit() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let path = directory.path().join("oversized.ts");
    let file = std::fs::File::create(&path).expect("create oversized segment");
    file.set_len(MAX_SAMPLE_AES_SEGMENT_BYTES as u64 + 1)
        .expect("extend segment beyond the processing limit");

    let error = read_sample_aes_resource(
        &path,
        "staged HLS SAMPLE-AES segment",
        MAX_SAMPLE_AES_SEGMENT_BYTES,
    )
    .await
    .expect_err("oversized SAMPLE-AES segments must not enter the decoder");

    assert!(
        matches!(error, ManifestRunError::Unsupported(message) if message.contains("memory limit"))
    );
}

#[tokio::test]
async fn reads_sample_aes_key_with_a_sixteen_byte_bound() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let path = directory.path().join("key.bin");
    std::fs::write(&path, [0x2a; 16]).expect("write identity key");

    let key = read_sample_aes_resource(&path, "staged HLS SAMPLE-AES key", 16)
        .await
        .expect("read bounded key");

    assert_eq!(key, [0x2a; 16]);
}
