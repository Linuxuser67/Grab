//! PlayReady initialization parsing and DASH license-key acquisition.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use playready::Pssh;
use url::Url;
use zeroize::Zeroizing;

use crate::dash_drm::{PLAYREADY_SCHEME, ProtectionSystem};
use crate::drm::DrmKeyMap;

const PLAYREADY_PSSH_FIXED_BYTES: usize = 32;
const PLAYREADY_SYSTEM_ID: [u8; 16] = [
    0x9a, 0x04, 0xf0, 0x79, 0x98, 0x40, 0x42, 0x86, 0xab, 0x92, 0xe6, 0x5b, 0xe0, 0x88, 0x5f, 0x95,
];

fn decode_bounded_protection_data(encoded: &str, label: &str) -> Result<Vec<u8>, String> {
    if encoded.len() > crate::drm::MAX_PSSH_BYTES.saturating_mul(4) {
        return Err(format!("DASH {label} exceeds its encoded size limit"));
    }
    let compact: Vec<u8> = encoded
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    if compact.len() > crate::drm::MAX_PSSH_BYTES.saturating_mul(2) {
        return Err(format!("DASH {label} exceeds its encoded size limit"));
    }
    let decoded = STANDARD
        .decode(compact)
        .map_err(|_| format!("DASH {label} is not valid base64"))?;
    if decoded.is_empty() || decoded.len() > crate::drm::MAX_PSSH_BYTES {
        return Err(format!("DASH {label} exceeds its decoded size limit"));
    }
    Ok(decoded)
}

fn playready_object_from_pssh(pssh: &[u8]) -> Result<&[u8], String> {
    crate::drm::validate_pssh(pssh, ProtectionSystem::PlayReady)?;
    let invalid = || "DASH PlayReady PSSH data is malformed".to_string();
    let version = *pssh.get(8).ok_or_else(invalid)?;
    let mut offset = 28_usize;
    if version == 1 {
        let count_end = offset.checked_add(4).ok_or_else(invalid)?;
        let kid_count = u32::from_be_bytes(
            pssh.get(offset..count_end)
                .ok_or_else(invalid)?
                .try_into()
                .map_err(|_| invalid())?,
        );
        let kid_bytes = usize::try_from(kid_count)
            .ok()
            .and_then(|count| count.checked_mul(16))
            .ok_or_else(invalid)?;
        offset = count_end.checked_add(kid_bytes).ok_or_else(invalid)?;
    }
    let size_end = offset.checked_add(4).ok_or_else(invalid)?;
    let data_size = u32::from_be_bytes(
        pssh.get(offset..size_end)
            .ok_or_else(invalid)?
            .try_into()
            .map_err(|_| invalid())?,
    );
    let data_len = usize::try_from(data_size).map_err(|_| invalid())?;
    let data_end = size_end.checked_add(data_len).ok_or_else(invalid)?;
    if data_end != pssh.len() {
        return Err(invalid());
    }
    Ok(&pssh[size_end..data_end])
}

fn validate_playready_pro(pro: &[u8]) -> Result<(), String> {
    playready_wrm_header(pro).map(|_| ())
}

fn playready_wrm_header(pro: &[u8]) -> Result<String, String> {
    let invalid = || "PlayReady Object must contain exactly one valid WRMHEADER record".to_string();
    if pro.len() < 10 || pro.len() > crate::drm::MAX_HLS_DRM_INIT_DATA_BYTES {
        return Err(invalid());
    }
    let declared_size = u32::from_le_bytes(
        pro.get(..4)
            .ok_or_else(invalid)?
            .try_into()
            .map_err(|_| invalid())?,
    );
    if usize::try_from(declared_size).ok() != Some(pro.len()) {
        return Err(invalid());
    }
    let record_count = u16::from_le_bytes(
        pro.get(4..6)
            .ok_or_else(invalid)?
            .try_into()
            .map_err(|_| invalid())?,
    );
    if record_count == 0 {
        return Err(invalid());
    }

    let mut offset = 6_usize;
    let mut header_count = 0_usize;
    let mut wrm_header = None;
    for _ in 0..record_count {
        let header_end = offset.checked_add(4).ok_or_else(invalid)?;
        let record_header = pro.get(offset..header_end).ok_or_else(invalid)?;
        let record_type = u16::from_le_bytes(
            record_header
                .get(..2)
                .ok_or_else(invalid)?
                .try_into()
                .map_err(|_| invalid())?,
        );
        let record_len = usize::from(u16::from_le_bytes(
            record_header
                .get(2..4)
                .ok_or_else(invalid)?
                .try_into()
                .map_err(|_| invalid())?,
        ));
        let record_end = header_end.checked_add(record_len).ok_or_else(invalid)?;
        let record = pro.get(header_end..record_end).ok_or_else(invalid)?;
        match record_type {
            1 => {
                if record.is_empty() || record.len() % 2 != 0 {
                    return Err(invalid());
                }
                let units: Vec<u16> = record
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| u16::from_le_bytes(*pair))
                    .collect();
                wrm_header = Some(String::from_utf16(&units).map_err(|_| invalid())?);
                header_count += 1;
            }
            // PlayReady PRO record types 2 and 3 are reserved metadata records.
            2 | 3 => {}
            _ => return Err(invalid()),
        }
        offset = record_end;
    }
    if offset != pro.len() || header_count != 1 {
        return Err(invalid());
    }

    let parsed = Pssh::from_bytes(pro).map_err(|_| "PlayReady Object is malformed".to_string())?;
    if parsed.wrm_headers().len() != 1 {
        return Err(invalid());
    }
    wrm_header.ok_or_else(invalid)
}

fn playready_object_to_pssh(pro: &[u8]) -> Result<Vec<u8>, String> {
    validate_playready_pro(pro)?;
    let pssh_len = PLAYREADY_PSSH_FIXED_BYTES
        .checked_add(pro.len())
        .filter(|length| *length <= crate::drm::MAX_PSSH_BYTES)
        .ok_or_else(|| "PlayReady Object exceeds the PSSH size limit".to_string())?;
    let data_len = u32::try_from(pro.len())
        .map_err(|_| "PlayReady Object exceeds the PSSH size limit".to_string())?;
    let box_len =
        u32::try_from(pssh_len).map_err(|_| "PlayReady PSSH exceeds its size limit".to_string())?;

    let mut pssh = Vec::with_capacity(pssh_len);
    pssh.extend_from_slice(&box_len.to_be_bytes());
    pssh.extend_from_slice(b"pssh");
    pssh.extend_from_slice(&[0, 0, 0, 0]);
    pssh.extend_from_slice(&PLAYREADY_SYSTEM_ID);
    pssh.extend_from_slice(&data_len.to_be_bytes());
    pssh.extend_from_slice(pro);
    validate_playready_pssh(&pssh)?;
    Ok(pssh)
}

/// Convert an HLS PlayReady Object into the PSSH and explicit HTTPS endpoint
/// required by the isolated PlayReady worker.
///
/// Only one WRMHEADER and its explicit DATA/LA_URL are supported. LUI_URL and
/// endpoint inference are deliberately not used.
pub(crate) fn hls_license_request(pro: &[u8]) -> Result<(Vec<u8>, Url), String> {
    let header = playready_wrm_header(pro)?;
    let raw_url = playready_la_url(&header)?;
    let url = Url::parse(&raw_url)
        .map_err(|_| "HLS PlayReady LA_URL must be an absolute HTTPS URL".to_string())?;
    let authority_has_userinfo = url
        .as_str()
        .strip_prefix("https://")
        .and_then(|remainder| remainder.split(['/', '?', '#']).next())
        .is_some_and(|authority| authority.contains('@'));
    if url.scheme() != "https"
        || url.host_str().is_none()
        || authority_has_userinfo
        || url.fragment().is_some()
    {
        return Err(
            "HLS PlayReady LA_URL must be HTTPS without user-info or a fragment".to_string(),
        );
    }
    Ok((playready_object_to_pssh(pro)?, url))
}

fn playready_la_url(header: &str) -> Result<String, String> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let invalid = || "HLS PlayReady WRMHEADER must contain one non-empty DATA/LA_URL".to_string();
    let mut reader = Reader::from_str(header);
    reader.config_mut().trim_text(true);
    let mut depth = 0_usize;
    let mut data_depth = None;
    let mut data_count = 0_usize;
    let mut la_url_depth = None;
    let mut la_url: Option<String> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                depth = depth.checked_add(1).ok_or_else(invalid)?;
                match element.local_name().as_ref() {
                    "DATA" => {
                        data_count += 1;
                        if data_count != 1 || data_depth.is_some() {
                            return Err(invalid());
                        }
                        data_depth = Some(depth);
                    }
                    "LA_URL" if data_depth == depth.checked_sub(1) => {
                        if la_url.is_some() {
                            return Err(invalid());
                        }
                        la_url = Some(String::new());
                        la_url_depth = Some(depth);
                    }
                    _ if la_url_depth.is_some() => return Err(invalid()),
                    _ => {}
                }
            }
            Ok(Event::Empty(element)) => {
                let name = element.local_name();
                if name.as_ref() == "DATA" {
                    data_count += 1;
                    if data_count != 1 || data_depth.is_some() {
                        return Err(invalid());
                    }
                } else if name.as_ref() == "LA_URL" && data_depth == Some(depth) {
                    if la_url.is_some() {
                        return Err(invalid());
                    }
                    la_url = Some(String::new());
                } else if la_url_depth.is_some() {
                    return Err(invalid());
                }
            }
            Ok(Event::Text(text)) if la_url_depth == Some(depth) => {
                let decoded = quick_xml::escape::unescape(text.as_ref()).map_err(|_| invalid())?;
                let value = la_url.as_mut().ok_or_else(invalid)?;
                value.push_str(&decoded);
                if value.len() > 2048 {
                    return Err(invalid());
                }
            }
            Ok(Event::CData(text)) if la_url_depth == Some(depth) => {
                let value = la_url.as_mut().ok_or_else(invalid)?;
                value.push_str(text.as_ref());
                if value.len() > 2048 {
                    return Err(invalid());
                }
            }
            Ok(Event::GeneralRef(reference)) if la_url_depth == Some(depth) => {
                let resolved = match reference.resolve_char_ref().map_err(|_| invalid())? {
                    Some(character) => character.to_string(),
                    None => quick_xml::escape::resolve_xml_entity(reference.as_ref())
                        .ok_or_else(invalid)?
                        .to_string(),
                };
                let value = la_url.as_mut().ok_or_else(invalid)?;
                value.push_str(&resolved);
                if value.len() > 2048 {
                    return Err(invalid());
                }
            }
            Ok(Event::End(_)) => {
                if la_url_depth == Some(depth) {
                    la_url_depth = None;
                }
                if data_depth == Some(depth) {
                    data_depth = None;
                }
                depth = depth.checked_sub(1).ok_or_else(invalid)?;
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => return Err(invalid()),
        }
    }

    if data_count != 1 || data_depth.is_some() || la_url_depth.is_some() {
        return Err(invalid());
    }
    let value = la_url.ok_or_else(invalid)?;
    let value = value.trim();
    if value.is_empty() || value.len() > 2048 {
        return Err(invalid());
    }
    Ok(value.to_string())
}

fn validate_playready_pssh(pssh: &[u8]) -> Result<(), String> {
    validate_playready_pro(playready_object_from_pssh(pssh)?)
}

fn playready_pssh_from_descriptor(
    descriptor: &dash_mpd_core::ContentProtection,
) -> Result<Option<Vec<u8>>, String> {
    let encoded_pssh: Vec<&str> = descriptor
        .cenc_pssh
        .iter()
        .filter_map(|pssh| pssh.content.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect();

    if !encoded_pssh.is_empty() {
        let mut selected: Option<Vec<u8>> = None;
        for encoded in encoded_pssh {
            let pssh = decode_bounded_protection_data(encoded, "PlayReady PSSH")?;
            validate_playready_pssh(&pssh)?;
            if selected.as_ref().is_some_and(|previous| previous != &pssh) {
                return Err("DASH PlayReady descriptor contains conflicting PSSH boxes".to_string());
            }
            selected = Some(pssh);
        }
        // cenc:pssh takes precedence over mspr:pro within one descriptor.
        return Ok(selected);
    }

    let Some(encoded_pro) = descriptor
        .msprpro
        .as_ref()
        .and_then(|pro| pro.content.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    let pro = decode_bounded_protection_data(encoded_pro, "PlayReady PRO")?;
    validate_playready_pro(&pro)?;

    Ok(Some(playready_object_to_pssh(&pro)?))
}

/// Decode PlayReady initialization data from inherited DASH protection scopes.
///
/// The most-specific scope containing PlayReady initialization data overrides
/// its ancestors.
pub(crate) fn pssh_from_scopes(
    scopes: &[&[dash_mpd_core::ContentProtection]],
) -> Result<Option<Vec<u8>>, String> {
    let mut selected = None;
    for scope in scopes {
        let mut scope_pssh: Option<Vec<u8>> = None;
        for descriptor in *scope {
            if descriptor.schemeIdUri != PLAYREADY_SCHEME {
                continue;
            }
            let Some(pssh) = playready_pssh_from_descriptor(descriptor)? else {
                continue;
            };
            if scope_pssh
                .as_ref()
                .is_some_and(|previous| previous != &pssh)
            {
                return Err(
                    "DASH PlayReady descriptors conflict within one protection scope".to_string(),
                );
            }
            scope_pssh = Some(pssh);
        }
        if scope_pssh.is_some() {
            selected = scope_pssh;
        }
    }
    Ok(selected)
}

/// Acquire only the PlayReady content keys referenced by the selected DASH track.
///
/// The CDM's `KeyId` type already converts the PlayReady GUID's little-endian
/// fields into ISO Common Encryption byte order. Keep those bytes unchanged.
///
/// # Errors
/// Returns an error for an empty sample-KID set, an invalid worker exchange, duplicate
/// selected KIDs, or a license that does not contain every selected content key.
pub(crate) async fn acquire_keys(
    client: &reqwest::Client,
    device_path: &Path,
    pssh: &[u8],
    license_url: &Url,
    required_kids: &BTreeSet<[u8; 16]>,
) -> Result<DrmKeyMap, String> {
    if required_kids.is_empty() {
        return Err("DASH PlayReady acquisition requires at least one sample KID".to_string());
    }

    let keys =
        crate::playready_worker::acquire_keys(client, device_path, pssh, license_url).await?;
    let mut accepted: BTreeMap<[u8; 16], Zeroizing<[u8; 16]>> = BTreeMap::new();
    for key in keys {
        let kid = key.kid;
        if required_kids.contains(&kid) && accepted.insert(kid, key.key).is_some() {
            return Err("PlayReady returned duplicate keys for a selected KID".to_string());
        }
    }
    if accepted.len() != required_kids.len() {
        return Err("The PlayReady license did not return every selected content key".to_string());
    }
    DrmKeyMap::from_keys(accepted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dash_drm::{DashEncryption, EncryptionScheme, encryption_from_scopes};

    fn playready_pro() -> Vec<u8> {
        let wrm_header = concat!(
            "<WRMHEADER xmlns=\"http://schemas.microsoft.com/DRM/2007/03/PlayReadyHeader\" ",
            "version=\"4.3.0.0\"><DATA><PROTECTINFO><KIDS>",
            "<KID ALGID=\"AESCTR\" VALUE=\"MyIRAFVEd2aImaq7zN3u/w==\"/>",
            "</KIDS></PROTECTINFO><LA_URL>https://license.example.test/Acquire</LA_URL>",
            "</DATA></WRMHEADER>"
        );
        let xml: Vec<u8> = wrm_header
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let record_len = u16::try_from(xml.len()).expect("test WRMHEADER length");
        let total_len = 10_u32 + u32::from(record_len);
        let mut pro = Vec::with_capacity(total_len as usize);
        pro.extend_from_slice(&total_len.to_le_bytes());
        pro.extend_from_slice(&1_u16.to_le_bytes());
        pro.extend_from_slice(&1_u16.to_le_bytes());
        pro.extend_from_slice(&record_len.to_le_bytes());
        pro.extend_from_slice(&xml);
        pro
    }

    fn protection(scheme: &str, kid: &str) -> dash_mpd_core::ContentProtection {
        dash_mpd_core::ContentProtection {
            schemeIdUri: "urn:mpeg:dash:mp4protection:2011".to_string(),
            value: Some(scheme.to_string()),
            default_KID: Some(kid.to_string()),
            ..Default::default()
        }
    }

    fn playready_descriptor(pro: Vec<u8>) -> dash_mpd_core::ContentProtection {
        dash_mpd_core::ContentProtection {
            schemeIdUri: PLAYREADY_SCHEME.to_string(),
            msprpro: Some(dash_mpd_core::MsprPro {
                content: Some(STANDARD.encode(pro)),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn pssh_from_pro(pro: &[u8]) -> Vec<u8> {
        let pssh_len = 32 + pro.len();
        let mut pssh = Vec::with_capacity(pssh_len);
        pssh.extend_from_slice(&u32::try_from(pssh_len).expect("PSSH length").to_be_bytes());
        pssh.extend_from_slice(b"pssh");
        pssh.extend_from_slice(&[0, 0, 0, 0]);
        pssh.extend_from_slice(&[
            0x9a, 0x04, 0xf0, 0x79, 0x98, 0x40, 0x42, 0x86, 0xab, 0x92, 0xe6, 0x5b, 0xe0, 0x88,
            0x5f, 0x95,
        ]);
        pssh.extend_from_slice(&u32::try_from(pro.len()).expect("PRO length").to_be_bytes());
        pssh.extend_from_slice(pro);
        pssh
    }

    #[test]
    fn parses_hls_playready_pro_and_requires_one_absolute_https_la_url() {
        let pro = playready_pro();
        let (pssh, url) = hls_license_request(&pro).expect("HLS PlayReady request data");
        crate::drm::validate_pssh(&pssh, ProtectionSystem::PlayReady)
            .expect("HLS PRO should be wrapped as a PlayReady PSSH");
        assert_eq!(url.as_str(), "https://license.example.test/Acquire");

        let multiple = test_pro_from_xml(concat!(
            "<DATA>",
            "<LA_URL>https://one.example.test/license</LA_URL>",
            "<LA_URL>https://two.example.test/license</LA_URL>",
            "</DATA>"
        ));
        assert!(hls_license_request(&multiple).is_err());

        for content in [
            "<DATA></DATA>",
            "<DATA><LA_URL>http://license.example.test</LA_URL></DATA>",
            "<DATA><LA_URL>https://user:pass@license.example.test</LA_URL></DATA>",
            "<DATA><LA_URL>https://@license.example.test</LA_URL></DATA>",
            "<DATA><LUI_URL>https://license.example.test/ui</LUI_URL></DATA>",
        ] {
            assert!(hls_license_request(&test_pro_from_xml(content)).is_err());
        }
    }

    fn test_pro_from_xml(xml: &str) -> Vec<u8> {
        let xml = format!(
            "<WRMHEADER xmlns=\"http://schemas.microsoft.com/DRM/2007/03/PlayReadyHeader\" version=\"4.3.0.0\">{xml}</WRMHEADER>"
        );
        let utf16: Vec<u8> = xml.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut pro = Vec::with_capacity(10 + utf16.len());
        pro.extend_from_slice(
            &u32::try_from(10 + utf16.len())
                .expect("test PRO size")
                .to_le_bytes(),
        );
        pro.extend_from_slice(&1_u16.to_le_bytes());
        pro.extend_from_slice(&1_u16.to_le_bytes());
        pro.extend_from_slice(
            &u16::try_from(utf16.len())
                .expect("test WRMHEADER size")
                .to_le_bytes(),
        );
        pro.extend_from_slice(&utf16);
        pro
    }

    #[test]
    fn recognizes_playready_common_encryption_metadata() {
        let mp4_protection = protection("cenc", "00112233445566778899aabbccddeeff");
        let playready = dash_mpd_core::ContentProtection {
            schemeIdUri: PLAYREADY_SCHEME.to_string(),
            ..Default::default()
        };
        let encryption = encryption_from_scopes(&[&[mp4_protection, playready]])
            .expect("PlayReady encryption metadata should be accepted")
            .expect("mp4protection declares encryption");
        assert_eq!(
            encryption,
            DashEncryption {
                scheme: EncryptionScheme::Cenc,
                default_kid: [
                    0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc,
                    0xdd, 0xee, 0xff,
                ],
                system: ProtectionSystem::PlayReady,
            }
        );
    }

    #[test]
    fn extracts_playready_pssh_from_mspr_pro() {
        let descriptor = playready_descriptor(playready_pro());
        let pssh = pssh_from_scopes(&[std::slice::from_ref(&descriptor)])
            .expect("valid PlayReady PRO should parse")
            .expect("PlayReady initialization data should be retained");
        crate::drm::validate_pssh(&pssh, ProtectionSystem::PlayReady)
            .expect("wrapped PSSH should have the PlayReady system ID");
        assert_eq!(
            Pssh::from_bytes(&pssh)
                .expect("wrapped PSSH")
                .wrm_headers()
                .len(),
            1
        );
    }

    #[test]
    fn playready_cenc_pssh_takes_precedence_over_mspr_pro() {
        let pro = playready_pro();
        let pssh = pssh_from_pro(&pro);
        let descriptor = dash_mpd_core::ContentProtection {
            schemeIdUri: PLAYREADY_SCHEME.to_string(),
            cenc_pssh: vec![dash_mpd_core::CencPssh {
                content: Some(STANDARD.encode(&pssh)),
            }],
            msprpro: Some(dash_mpd_core::MsprPro {
                content: Some("not-base64".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let selected = pssh_from_scopes(&[std::slice::from_ref(&descriptor)])
            .expect("valid cenc:pssh should take precedence");
        assert_eq!(selected.as_deref(), Some(pssh.as_slice()));
    }

    #[test]
    fn rejects_playready_pro_with_extra_records_or_trailing_bytes() {
        let valid = playready_pro();
        let mut extra_record = valid.clone();
        extra_record[4..6].copy_from_slice(&2_u16.to_le_bytes());
        assert!(validate_playready_pro(&extra_record).is_err());

        let mut trailing_bytes = valid;
        trailing_bytes.push(0);
        assert!(validate_playready_pro(&trailing_bytes).is_err());
    }

    #[test]
    fn rejects_conflicting_playready_psshs_in_one_scope() {
        let first = playready_pro();
        let mut second = playready_pro();
        let encoded_url: Vec<u8> = "Acquire"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let url_offset = second
            .windows(encoded_url.len())
            .position(|window| window == encoded_url)
            .expect("test PRO contains its license URL");
        second[url_offset] = b'B';

        let descriptor = |pro: Vec<u8>| playready_descriptor(pro);
        assert!(pssh_from_scopes(&[&[descriptor(first), descriptor(second)]]).is_err());
    }

    #[test]
    fn playready_pssh_inherits_until_a_more_specific_scope_overrides_it() {
        let parent = playready_descriptor(playready_pro());
        let child = dash_mpd_core::ContentProtection {
            schemeIdUri: PLAYREADY_SCHEME.to_string(),
            ..Default::default()
        };
        let parent_pssh = pssh_from_scopes(&[std::slice::from_ref(&parent)])
            .expect("parent PlayReady data")
            .expect("parent PSSH");
        assert_eq!(
            pssh_from_scopes(&[std::slice::from_ref(&parent), std::slice::from_ref(&child),])
                .expect("inherited PlayReady data")
                .as_deref(),
            Some(parent_pssh.as_slice())
        );

        let mut overriding_pro = playready_pro();
        let encoded_url: Vec<u8> = "Acquire"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let offset = overriding_pro
            .windows(encoded_url.len())
            .position(|window| window == encoded_url)
            .expect("test PRO URL");
        overriding_pro[offset] = b'B';
        let overriding = playready_descriptor(overriding_pro);
        let overriding_pssh = pssh_from_scopes(&[
            std::slice::from_ref(&parent),
            std::slice::from_ref(&overriding),
        ])
        .expect("representation PlayReady data")
        .expect("representation PSSH");
        assert_ne!(overriding_pssh, parent_pssh);
    }

    #[test]
    fn playready_cenc_pssh_version_one_payload_is_read_correctly() {
        let pro = playready_pro();
        let mut pssh = Vec::with_capacity(32 + 4 + 16 + pro.len());
        let pssh_len = 32 + 4 + 16 + pro.len();
        pssh.extend_from_slice(&u32::try_from(pssh_len).expect("PSSH length").to_be_bytes());
        pssh.extend_from_slice(b"pssh");
        pssh.extend_from_slice(&[1, 0, 0, 0]);
        pssh.extend_from_slice(&[
            0x9a, 0x04, 0xf0, 0x79, 0x98, 0x40, 0x42, 0x86, 0xab, 0x92, 0xe6, 0x5b, 0xe0, 0x88,
            0x5f, 0x95,
        ]);
        pssh.extend_from_slice(&1_u32.to_be_bytes());
        pssh.extend_from_slice(&[0x11; 16]);
        pssh.extend_from_slice(&u32::try_from(pro.len()).expect("PRO length").to_be_bytes());
        pssh.extend_from_slice(&pro);
        let descriptor = dash_mpd_core::ContentProtection {
            schemeIdUri: PLAYREADY_SCHEME.to_string(),
            cenc_pssh: vec![dash_mpd_core::CencPssh {
                content: Some(STANDARD.encode(&pssh)),
            }],
            ..Default::default()
        };
        assert_eq!(
            pssh_from_scopes(&[std::slice::from_ref(&descriptor)])
                .expect("version-one PSSH")
                .as_deref(),
            Some(pssh.as_slice())
        );
    }
}
