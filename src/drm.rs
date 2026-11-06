//! Protocol-neutral DRM metadata shared by native HLS and DASH planning.
//!
//! This module deliberately does not acquire licenses or decrypt media. Protocol
//! adapters retain validated initialization data here so a shared CDM/decryption
//! pipeline can consume it without duplicating provider handling.

use std::collections::HashMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use m3u8_rs::{Key, KeyMethod};

pub(crate) const MAX_PSSH_BYTES: usize = 64 * 1024;
pub(crate) const MAX_HLS_DRM_INIT_DATA_BYTES: usize = 64 * 1024;

const WIDEVINE_SYSTEM_ID: [u8; 16] = [
    0xed, 0xef, 0x8b, 0xa9, 0x79, 0xd6, 0x4a, 0xce, 0xa3, 0xc8, 0x27, 0xdc, 0xd5, 0x1d, 0x21, 0xed,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EncryptionScheme {
    Cenc,
    Cbcs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProtectionSystem {
    ClearKey,
    Widevine,
    PlayReady,
    FairPlay,
}

impl ProtectionSystem {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::ClearKey => "ClearKey",
            Self::Widevine => "Widevine",
            Self::PlayReady => "PlayReady",
            Self::FairPlay => "FairPlay",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HlsInitializationData {
    /// A complete ISO BMFF Protection System Specific Header box.
    Pssh(Box<[u8]>),
    /// PlayReady Object bytes from the HLS com.microsoft.playready data URI.
    PlayReadyObject(Box<[u8]>),
    /// FairPlay Streaming key identifier. It is not a PSSH box.
    FairPlayKeyUri(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HlsProtection {
    pub(crate) system: ProtectionSystem,
    pub(crate) scheme: EncryptionScheme,
    pub(crate) initialization_data: HlsInitializationData,
}

/// Validate the complete box framing and system identifier of a PSSH box.
///
/// Only ordinary 32-bit box sizes and versions 0/1 are accepted. Extended-size
/// boxes and nonzero flags are left to the extractor fallback rather than being
/// interpreted differently by the native pipeline.
pub(crate) fn validate_pssh(bytes: &[u8], system: ProtectionSystem) -> Result<(), String> {
    let invalid = || "DRM PSSH box is malformed or has the wrong system ID".to_string();
    if bytes.len() < 32 || bytes.len() > MAX_PSSH_BYTES || bytes.get(4..8) != Some(&b"pssh"[..]) {
        return Err(invalid());
    }

    let version = *bytes.get(8).ok_or_else(invalid)?;
    if version > 1 || bytes.get(9..12) != Some(&[0, 0, 0][..]) {
        return Err(invalid());
    }
    let expected_system_id = match system {
        ProtectionSystem::Widevine => WIDEVINE_SYSTEM_ID,
        ProtectionSystem::PlayReady => [
            0x9a, 0x04, 0xf0, 0x79, 0x98, 0x40, 0x42, 0x86, 0xab, 0x92, 0xe6, 0x5b, 0xe0, 0x88,
            0x5f, 0x95,
        ],
        ProtectionSystem::ClearKey | ProtectionSystem::FairPlay => return Err(invalid()),
    };
    if bytes.get(12..28) != Some(expected_system_id.as_slice()) {
        return Err(invalid());
    }

    let declared_size = u32::from_be_bytes(
        bytes
            .get(0..4)
            .ok_or_else(invalid)?
            .try_into()
            .map_err(|_| invalid())?,
    );
    if usize::try_from(declared_size).ok() != Some(bytes.len()) {
        return Err(invalid());
    }

    let mut offset = 28_usize;
    if version == 1 {
        let count_end = offset.checked_add(4).ok_or_else(invalid)?;
        let count = u32::from_be_bytes(
            bytes
                .get(offset..count_end)
                .ok_or_else(invalid)?
                .try_into()
                .map_err(|_| invalid())?,
        );
        let kid_bytes = usize::try_from(count)
            .ok()
            .and_then(|count| count.checked_mul(16))
            .ok_or_else(invalid)?;
        offset = count_end.checked_add(kid_bytes).ok_or_else(invalid)?;
    }

    let size_end = offset.checked_add(4).ok_or_else(invalid)?;
    let data_size = u32::from_be_bytes(
        bytes
            .get(offset..size_end)
            .ok_or_else(invalid)?
            .try_into()
            .map_err(|_| invalid())?,
    );
    let data_end = size_end
        .checked_add(usize::try_from(data_size).map_err(|_| invalid())?)
        .ok_or_else(invalid)?;
    if data_end != bytes.len() {
        return Err(invalid());
    }
    Ok(())
}

/// Convert one non-identity HLS key descriptor into shared provider metadata.
/// Identity AES-128 and identity MPEG-TS SAMPLE-AES continue through their
/// existing download/decryption paths and therefore return Ok(None).
pub(crate) fn hls_protection_from_key(key: &Key) -> Result<Option<HlsProtection>, String> {
    if key.keyformatversions.as_deref().is_some_and(|value| value != "1") {
        return Err("HLS DRM key uses an unsupported KEYFORMATVERSIONS value".to_string());
    }

    let key_format = key.keyformat.as_deref().unwrap_or("identity");
    if key_format == "identity" {
        return Ok(None);
    }

    let scheme = match &key.method {
        KeyMethod::SampleAES => EncryptionScheme::Cbcs,
        KeyMethod::Other(method) if method == "SAMPLE-AES-CENC" || method == "SAMPLE-AES-CTR" => {
            EncryptionScheme::Cenc
        }
        _ => {
            return Err("HLS DRM key uses an unsupported encryption method".to_string());
        }
    };

    let uri = key
        .uri
        .as_deref()
        .ok_or_else(|| "HLS DRM key URI is missing".to_string())?;
    let (system, initialization_data) = match key_format {
        "urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed" => {
            let bytes = decode_hls_data_uri(uri, &[
                "text/plain;base64",
                "text/plain;charset=utf-8;base64",
            ])?;
            validate_pssh(&bytes, ProtectionSystem::Widevine)?;
            (
                ProtectionSystem::Widevine,
                HlsInitializationData::Pssh(bytes.into_boxed_slice()),
            )
        }
        "com.microsoft.playready" => {
            let bytes = decode_hls_data_uri(uri, &["text/plain;charset=utf-16;base64"])?;
            validate_playready_object(&bytes)?;
            (
                ProtectionSystem::PlayReady,
                HlsInitializationData::PlayReadyObject(bytes.into_boxed_slice()),
            )
        }
        "com.apple.streamingkeydelivery" => {
            if scheme != EncryptionScheme::Cbcs || !uri.starts_with("skd://") {
                return Err("HLS FairPlay key descriptor is malformed or unsupported".to_string());
            }
            (
                ProtectionSystem::FairPlay,
                HlsInitializationData::FairPlayKeyUri(uri.to_string()),
            )
        }
        _ => return Err("HLS declares an unsupported KEYFORMAT".to_string()),
    };

    Ok(Some(HlsProtection {
        system,
        scheme,
        initialization_data,
    }))
}

fn decode_hls_data_uri(uri: &str, accepted_metadata: &[&str]) -> Result<Vec<u8>, String> {
    let (metadata, encoded) = uri
        .strip_prefix("data:")
        .and_then(|data| data.split_once(','))
        .ok_or_else(|| "HLS DRM initialization URI is not a valid data URI".to_string())?;
    if !accepted_metadata
        .iter()
        .any(|accepted| metadata.eq_ignore_ascii_case(accepted))
    {
        return Err("HLS DRM initialization data URI has an unsupported media type".to_string());
    }
    if encoded.is_empty() || encoded.len() > (MAX_HLS_DRM_INIT_DATA_BYTES * 4 / 3) + 8 {
        return Err("HLS DRM initialization data exceeds its size limit".to_string());
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| "HLS DRM initialization data is not valid base64".to_string())?;
    if bytes.is_empty() || bytes.len() > MAX_HLS_DRM_INIT_DATA_BYTES {
        return Err("HLS DRM initialization data exceeds its size limit".to_string());
    }
    Ok(bytes)
}

fn validate_playready_object(bytes: &[u8]) -> Result<(), String> {
    let invalid = || "HLS PlayReady initialization data is not a valid PlayReady Object".to_string();
    if bytes.len() < 10 || bytes.len() > MAX_HLS_DRM_INIT_DATA_BYTES {
        return Err(invalid());
    }
    let declared_size = u32::from_le_bytes(
        bytes
            .get(0..4)
            .ok_or_else(invalid)?
            .try_into()
            .map_err(|_| invalid())?,
    );
    if usize::try_from(declared_size).ok() != Some(bytes.len()) {
        return Err(invalid());
    }
    let record_count = u16::from_le_bytes(
        bytes
            .get(4..6)
            .ok_or_else(invalid)?
            .try_into()
            .map_err(|_| invalid())?,
    );
    if record_count == 0 {
        return Err(invalid());
    }

    let mut offset = 6_usize;
    let mut has_wrm_header = false;
    for _ in 0..record_count {
        let header_end = offset.checked_add(4).ok_or_else(invalid)?;
        let header = bytes.get(offset..header_end).ok_or_else(invalid)?;
        let record_type = u16::from_le_bytes(
            header
                .get(0..2)
                .ok_or_else(invalid)?
                .try_into()
                .map_err(|_| invalid())?,
        );
        let record_length = usize::from(u16::from_le_bytes(
            header
                .get(2..4)
                .ok_or_else(invalid)?
                .try_into()
                .map_err(|_| invalid())?,
        ));
        let record_end = header_end.checked_add(record_length).ok_or_else(invalid)?;
        let record = bytes.get(header_end..record_end).ok_or_else(invalid)?;
        match record_type {
            1 => {
                if record.is_empty() || record.len() % 2 != 0 {
                    return Err(invalid());
                }
                let units: Vec<u16> = record
                    .chunks_exact(2)
                    .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                    .collect();
                String::from_utf16(&units).map_err(|_| invalid())?;
                has_wrm_header = true;
            }
            2 | 3 => {}
            _ => return Err(invalid()),
        }
        offset = record_end;
    }
    if offset != bytes.len() || !has_wrm_header {
        return Err(invalid());
    }
    Ok(())
}

/// Reject multiple simultaneously active KEYFORMATs because m3u8-rs exposes
/// only one Key on each media segment. Silently retaining only the last tag
/// would lose a valid protection descriptor and could select the wrong path.
pub(crate) fn validate_hls_keyformat_multiplexing(body: &[u8]) -> Result<(), String> {
    use std::collections::HashSet;

    let text = std::str::from_utf8(body)
        .map_err(|_| "HLS playlist is not valid UTF-8".to_string())?;
    let mut active_formats = HashSet::new();
    for line in text.lines().map(str::trim) {
        if let Some(attributes) = line.strip_prefix("#EXT-X-KEY:") {
            let fields: HashMap<String, String> = parse_hls_attribute_list(attributes)?
                .into_iter()
                .collect();
            let method = fields
                .get("METHOD")
                .ok_or_else(|| "HLS key tag has no METHOD".to_string())?;
            if method == "NONE" {
                active_formats.clear();
                continue;
            }
            let format = fields
                .get("KEYFORMAT")
                .map(String::as_str)
                .unwrap_or("identity");
            active_formats.insert(format.to_string());
        } else if !line.is_empty() && !line.starts_with('#') && active_formats.len() > 1 {
            return Err(
                "HLS media segment uses multiple KEYFORMATs which the current parser cannot preserve"
                    .to_string(),
            );
        }
    }
    Ok(())
}

fn parse_hls_attribute_list(input: &str) -> Result<Vec<(String, String)>, String> {
    let mut fields = Vec::new();
    let mut start = 0_usize;
    let mut quoted = false;
    for (index, character) in input.char_indices() {
        match character {
            '"' => quoted = !quoted,
            ',' if !quoted => {
                fields.push(parse_hls_attribute(&input[start..index])?);
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    if quoted {
        return Err("HLS key tag has an unterminated quoted attribute".to_string());
    }
    fields.push(parse_hls_attribute(&input[start..])?);
    Ok(fields)
}

fn parse_hls_attribute(input: &str) -> Result<(String, String), String> {
    let (name, value) = input
        .split_once('=')
        .ok_or_else(|| "HLS key tag contains an invalid attribute".to_string())?;
    let name = name.trim();
    let value = value.trim();
    if name.is_empty() || value.is_empty() {
        return Err("HLS key tag contains an empty attribute".to_string());
    }
    let value = if value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
        &value[1..value.len() - 1]
    } else if value.starts_with('"') || value.ends_with('"') {
        return Err("HLS key tag contains an invalid quoted attribute".to_string());
    } else {
        value
    };
    Ok((name.to_ascii_uppercase(), value.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIDEVINE_PSSH: &str = "AAAAIHBzc2gAAAAA7e+LqXnWSs6jyCfc1R0h7QAAAAA=";
    const WIDEVINE_KEYFORMAT: &str = "urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed";

    fn key(method: KeyMethod, uri: &str, keyformat: &str) -> Key {
        Key {
            method,
            uri: Some(uri.to_string()),
            iv: None,
            keyformat: Some(keyformat.to_string()),
            keyformatversions: None,
        }
    }

    #[test]
    fn validates_widevine_pssh_v0_and_rejects_other_system_ids() {
        let bytes = STANDARD.decode(WIDEVINE_PSSH).expect("test PSSH base64");
        assert_eq!(validate_pssh(&bytes, ProtectionSystem::Widevine), Ok(()));
        assert!(validate_pssh(&bytes, ProtectionSystem::PlayReady).is_err());
        assert!(validate_pssh(&bytes[..bytes.len() - 1], ProtectionSystem::Widevine).is_err());
    }

    #[test]
    fn parses_widevine_hls_data_uri_without_treating_it_as_a_network_key() {
        let uri = format!("data:text/plain;base64,{WIDEVINE_PSSH}");
        let parsed = hls_protection_from_key(&key(
            KeyMethod::SampleAES,
            &uri,
            WIDEVINE_KEYFORMAT,
        ))
        .expect("Widevine HLS key descriptor should parse")
        .expect("Widevine descriptor should be retained");
        assert_eq!(parsed.system, ProtectionSystem::Widevine);
        assert_eq!(parsed.scheme, EncryptionScheme::Cbcs);
        assert!(matches!(parsed.initialization_data, HlsInitializationData::Pssh(_)));
    }

    #[test]
    fn parses_playready_hls_pro_and_fairplay_key_uris() {
        let xml = "<WRMHEADER><DATA></DATA></WRMHEADER>";
        let utf16: Vec<u8> = xml
            .encode_utf16()
            .flat_map(|unit| unit.to_le_bytes())
            .collect();
        let mut pro = Vec::new();
        pro.extend_from_slice(
            &u32::try_from(10 + utf16.len())
                .expect("test PRO length fits u32")
                .to_le_bytes(),
        );
        pro.extend_from_slice(&1_u16.to_le_bytes());
        pro.extend_from_slice(&1_u16.to_le_bytes());
        pro.extend_from_slice(
            &u16::try_from(utf16.len())
                .expect("test WRMHEADER length fits u16")
                .to_le_bytes(),
        );
        pro.extend_from_slice(&utf16);
        let pro_uri = format!(
            "data:text/plain;charset=UTF-16;base64,{}",
            STANDARD.encode(&pro)
        );
        let playready = hls_protection_from_key(&key(
            KeyMethod::SampleAES,
            &pro_uri,
            "com.microsoft.playready",
        ))
        .expect("PlayReady HLS key descriptor should parse")
        .expect("PlayReady descriptor should be retained");
        assert_eq!(playready.system, ProtectionSystem::PlayReady);
        assert!(matches!(
            playready.initialization_data,
            HlsInitializationData::PlayReadyObject(_)
        ));

        let fairplay = hls_protection_from_key(&key(
            KeyMethod::SampleAES,
            "skd://asset.example.test/content-id",
            "com.apple.streamingkeydelivery",
        ))
        .expect("FairPlay HLS key descriptor should parse")
        .expect("FairPlay descriptor should be retained");
        assert_eq!(fairplay.system, ProtectionSystem::FairPlay);
        assert!(matches!(
            fairplay.initialization_data,
            HlsInitializationData::FairPlayKeyUri(_)
        ));
    }

    #[test]
    fn leaves_identity_keys_to_the_existing_hls_paths() {
        let identity = key(KeyMethod::AES128, "https://cdn.example.test/key.bin", "identity");
        assert_eq!(hls_protection_from_key(&identity), Ok(None));
    }

    #[test]
    fn rejects_multiple_active_keyformats_but_allows_single_format_rotation() {
        let multiple = b"#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"one\"\n#EXTINF:2,\none.ts\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"two\",KEYFORMAT=\"com.microsoft.playready\"\n#EXTINF:2,\ntwo.ts\n";
        assert!(validate_hls_keyformat_multiplexing(multiple).is_err());

        let rotation = b"#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"one\"\n#EXTINF:2,\none.ts\n#EXT-X-KEY:METHOD=AES-128,URI=\"two\"\n#EXTINF:2,\ntwo.ts\n";
        assert_eq!(validate_hls_keyformat_multiplexing(rotation), Ok(()));
    }
}
