//! Identity SAMPLE-AES decryption and bounded MPEG-TS/PES preflight.

use std::path::Path;

use tokio::io::AsyncWriteExt as _;

use super::{
    local_hls_resource_path, remove_regular_file_if_present, sync_staging_directory,
    ManifestRunError,
};
use crate::manifest::HlsPlan;

/// Maximum encrypted SAMPLE-AES segment size accepted by the in-memory decoder.
/// The decoder and MPEG-TS preflight both need whole-segment buffers, so this
/// limit is intentionally much smaller than the general staged-resource cap.
pub(super) const MAX_SAMPLE_AES_SEGMENT_BYTES: usize = 32 * 1024 * 1024;

/// Read a staged SAMPLE-AES resource off the async executor with a dedicated
/// bound for data that must be held in memory by the decryptor and preflight.
pub(super) async fn read_sample_aes_resource(
    path: &Path,
    description: &'static str,
    max_bytes: usize,
) -> Result<Vec<u8>, ManifestRunError> {
    let path = path.to_path_buf();
    let result = tokio::task::spawn_blocking(move || {
        crate::bounded_file::read_regular_file_bounded(&path, max_bytes)
    })
    .await
    .map_err(|error| {
        ManifestRunError::Failed(format!("The {description} read worker failed: {error}"))
    })?;

    result.map_err(|error| match error {
        crate::bounded_file::BoundedFileError::NotRegularFile => {
            ManifestRunError::Failed(format!("The {description} is not a regular file"))
        }
        crate::bounded_file::BoundedFileError::TooLarge => ManifestRunError::Unsupported(format!(
            "The {description} exceeds its {max_bytes}-byte SAMPLE-AES memory limit"
        )),
        crate::bounded_file::BoundedFileError::Io(source) => {
            ManifestRunError::Failed(format!("Could not read {description}: {source}"))
        }
    })
}

/// Decrypt identity SAMPLE-AES MPEG-TS segments with iori-ssa, the engine used by VSD,
/// then remove the corresponding key tags from the local playlist.
pub(super) async fn decrypt_hls_sample_aes_segments(
    directory: &Path,
    plan: &HlsPlan,
) -> Result<(), ManifestRunError> {
    let playlist_bytes = plan
        .local_playlist_bytes()
        .map_err(|error| ManifestRunError::Failed(error.to_string()))?;
    let mut playlist = m3u8_rs::parse_media_playlist_res(&playlist_bytes).map_err(|error| {
        ManifestRunError::Failed(format!(
            "Could not parse the localized HLS playlist: {error:?}"
        ))
    })?;
    let mut changed = false;

    for (index, segment) in playlist.segments.iter_mut().enumerate() {
        let Some(key_tag) = segment
            .key
            .as_ref()
            .filter(|key| key.method == m3u8_rs::KeyMethod::SampleAES)
        else {
            continue;
        };

        let key_uri = key_tag.uri.as_deref().ok_or_else(|| {
            ManifestRunError::Unsupported("The HLS SAMPLE-AES key URI is missing".to_string())
        })?;
        let key_path = local_hls_resource_path(directory, key_uri)?;
        let key_bytes =
            read_sample_aes_resource(&key_path, "staged HLS SAMPLE-AES key", 16).await?;
        let key: [u8; 16] = key_bytes.as_slice().try_into().map_err(|_| {
            ManifestRunError::Unsupported("The HLS identity key is not 16 bytes".to_string())
        })?;

        let sequence_offset = u64::try_from(index).map_err(|_| {
            ManifestRunError::Failed("The HLS media sequence overflowed".to_string())
        })?;
        let media_sequence = playlist
            .media_sequence
            .checked_add(sequence_offset)
            .ok_or_else(|| {
                ManifestRunError::Failed("The HLS media sequence overflowed".to_string())
            })?;
        let iv = hls_sample_aes_iv(key_tag.iv.as_deref(), media_sequence)?;

        let segment_uri = segment.uri.clone();
        let segment_path = local_hls_resource_path(directory, &segment_uri)?;
        let encrypted = read_sample_aes_resource(
            &segment_path,
            "staged HLS SAMPLE-AES segment",
            MAX_SAMPLE_AES_SEGMENT_BYTES,
        )
        .await?;
        let encrypted_size = encrypted.len();

        let decrypted = tokio::task::spawn_blocking(move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                validate_iori_sample_aes_segment(&encrypted)?;
                let mut decrypted = Vec::with_capacity(encrypted.len());
                iori_ssa::decrypt(
                    std::io::Cursor::new(encrypted.as_slice()),
                    &mut decrypted,
                    key,
                    iv,
                )
                .map_err(|error| error.to_string())?;
                Ok::<Vec<u8>, String>(decrypted)
            }))
        })
        .await
        .map_err(|error| {
            ManifestRunError::Failed(format!(
                "The HLS SAMPLE-AES worker failed for {segment_uri}: {error}"
            ))
        })?;
        let decrypted = match decrypted {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(error)) => {
                return Err(ManifestRunError::Unsupported(format!(
                    "iori-ssa cannot safely process HLS SAMPLE-AES segment {segment_uri}: {error}"
                )));
            }
            Err(_) => {
                return Err(ManifestRunError::Unsupported(format!(
                    "iori-ssa panicked while processing HLS SAMPLE-AES segment {segment_uri}"
                )));
            }
        };

        if decrypted.len() != encrypted_size {
            return Err(ManifestRunError::Unsupported(
                "iori-ssa returned an incomplete HLS SAMPLE-AES segment".to_string(),
            ));
        }
        // The encrypted input has already passed the bounded SAMPLE-AES/PES
        // preflight. Let FFmpeg validate the complete decrypted playlist after
        // all segments are localized; individual segments may need demux context.

        // Write each decrypted occurrence to a distinct staged file. Keep
        // encrypted source resources untouched until the whole playlist succeeds.
        let decrypted_uri = format!("sample-aes-decrypted-{index}.bin");
        let decrypted_path = local_hls_resource_path(directory, &decrypted_uri)?;
        remove_regular_file_if_present(&decrypted_path)
            .await
            .map_err(ManifestRunError::Failed)?;
        let temporary = directory.join(format!("{decrypted_uri}.part"));
        remove_regular_file_if_present(&temporary)
            .await
            .map_err(ManifestRunError::Failed)?;
        let write_result = async {
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .await
                .map_err(|error| {
                    format!("Could not create decrypted HLS segment staging: {error}")
                })?;
            file.write_all(&decrypted).await.map_err(|error| {
                format!("Could not write decrypted HLS segment staging: {error}")
            })?;
            file.sync_all().await.map_err(|error| {
                format!("Could not flush decrypted HLS segment staging: {error}")
            })?;
            drop(file);
            tokio::fs::rename(&temporary, &decrypted_path)
                .await
                .map_err(|error| format!("Could not publish the decrypted HLS segment: {error}"))
        }
        .await;
        if let Err(error) = write_result {
            let _ = remove_regular_file_if_present(&temporary).await;
            return Err(ManifestRunError::Failed(error));
        }

        segment.uri = decrypted_uri;
        segment.key = None;
        changed = true;
    }

    if changed {
        let mut rewritten = Vec::new();
        playlist.write_to(&mut rewritten).map_err(|error| {
            ManifestRunError::Failed(format!(
                "Could not serialize the decrypted HLS playlist: {error}"
            ))
        })?;
        let target = directory.join("local.m3u8");
        let temporary = directory.join("local.m3u8.sample-aes.part");
        remove_regular_file_if_present(&temporary)
            .await
            .map_err(ManifestRunError::Failed)?;
        let write_result = async {
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .await
                .map_err(|error| {
                    format!("Could not create the decrypted HLS playlist staging: {error}")
                })?;
            file.write_all(&rewritten).await.map_err(|error| {
                format!("Could not write the decrypted HLS playlist staging: {error}")
            })?;
            file.sync_all().await.map_err(|error| {
                format!("Could not flush the decrypted HLS playlist staging: {error}")
            })?;
            drop(file);
            tokio::fs::rename(&temporary, &target)
                .await
                .map_err(|error| format!("Could not publish the decrypted HLS playlist: {error}"))
        }
        .await;
        if let Err(error) = write_result {
            let _ = remove_regular_file_if_present(&temporary).await;
            return Err(ManifestRunError::Failed(error));
        }
        sync_staging_directory(directory)
            .await
            .map_err(ManifestRunError::Failed)?;
    }

    Ok(())
}

/// Validate the MPEG-TS packet/PES structure before handing untrusted bytes to
/// iori-ssa's SAMPLE-AES parser. In particular, iori-ssa's ADTS frame loop does not
/// advance when a malformed frame advertises a zero length.
pub(super) fn validate_iori_sample_aes_segment(bytes: &[u8]) -> Result<(), String> {
    use mpeg2ts::{
        es::StreamType,
        ts::{ReadTsPacket, TsPacketReader, TsPayload},
    };

    let mut reader = TsPacketReader::new(std::io::Cursor::new(bytes));
    let mut stream_types = std::collections::HashMap::<u16, StreamType>::new();
    let mut aac_pes = std::collections::HashMap::<u16, Vec<u8>>::new();

    while let Some(packet) = reader
        .read_ts_packet()
        .map_err(|error| format!("invalid MPEG-TS packet: {error:?}"))?
    {
        let pid = packet.header.pid.as_u16();
        let flush = matches!(
            &packet.payload,
            Some(TsPayload::Pat(_) | TsPayload::Pmt(_) | TsPayload::Section(_))
        );

        match packet.payload {
            Some(TsPayload::Pmt(pmt)) => {
                for stream in pmt.es_info {
                    stream_types.insert(stream.elementary_pid.as_u16(), stream.stream_type);
                }
            }
            Some(TsPayload::Pes(pes)) => {
                if let Some(previous) = aac_pes.remove(&pid) {
                    validate_iori_adts_pes(&previous)?;
                }
                if stream_types.get(&pid).is_some_and(|stream_type| {
                    matches!(
                        *stream_type,
                        StreamType::AdtsAac | StreamType::AdtsAacWithAes128Cbc
                    )
                }) {
                    aac_pes.insert(pid, pes.data.to_vec());
                }
            }
            Some(TsPayload::Raw(payload)) => {
                if let Some(data) = aac_pes.get_mut(&pid) {
                    data.extend_from_slice(&payload);
                } else if stream_types.get(&pid).is_some_and(|stream_type| {
                    matches!(
                        *stream_type,
                        StreamType::AdtsAac | StreamType::AdtsAacWithAes128Cbc
                    )
                }) {
                    return Err("AAC PES continuation has no preceding PES header".to_string());
                }
            }
            _ => {}
        }

        if flush && let Some(data) = aac_pes.remove(&pid) {
            validate_iori_adts_pes(&data)?;
        }
    }

    for data in aac_pes.into_values() {
        validate_iori_adts_pes(&data)?;
    }

    Ok(())
}

/// Reject ADTS frame lengths that could make iori-ssa's audio loop fail to
/// advance or index beyond the current PES packet.
pub(super) fn validate_iori_adts_pes(mut data: &[u8]) -> Result<(), String> {
    while !data.is_empty() {
        if data.len() < 7 || data[0] != 0xff || data[1] & 0xf6 != 0xf0 {
            return Err("malformed ADTS frame header".to_string());
        }

        let frame_length = (usize::from(data[3] & 0x03) << 11)
            | (usize::from(data[4]) << 3)
            | usize::from(data[5] >> 5);
        let header_length = if data[1] & 0x01 == 0 { 9 } else { 7 };
        if frame_length < header_length || frame_length > data.len() {
            return Err("ADTS frame length is invalid or incomplete".to_string());
        }
        data = &data[frame_length..];
    }

    Ok(())
}

pub(super) fn hls_sample_aes_iv(
    iv: Option<&str>,
    media_sequence: u64,
) -> Result<[u8; 16], ManifestRunError> {
    let Some(iv) = iv else {
        return Ok(u128::from(media_sequence).to_be_bytes());
    };
    let hex = iv
        .strip_prefix("0x")
        .or_else(|| iv.strip_prefix("0X"))
        .unwrap_or(iv);
    if hex.len() != 32 {
        return Err(ManifestRunError::Unsupported(
            "The HLS SAMPLE-AES IV is not 128 bits".to_string(),
        ));
    }
    let mut parsed = [0_u8; 16];
    for (index, byte) in parsed.iter_mut().enumerate() {
        let high = hls_hex_nibble(hex.as_bytes()[index * 2]).ok_or_else(|| {
            ManifestRunError::Unsupported("The HLS SAMPLE-AES IV is invalid".to_string())
        })?;
        let low = hls_hex_nibble(hex.as_bytes()[index * 2 + 1]).ok_or_else(|| {
            ManifestRunError::Unsupported("The HLS SAMPLE-AES IV is invalid".to_string())
        })?;
        *byte = (high << 4) | low;
    }
    Ok(parsed)
}

fn hls_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
