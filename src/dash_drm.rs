//! Bounded DASH Common Encryption support using HTTPS ClearKey and PlayReady licenses.
//!
//! Grab keeps downloaded encrypted resources as its resumable source of truth.
//! License keys are held in zeroizing memory, and decryption produces a separate
//! private track for FFmpeg without modifying encrypted source resources.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use futures_util::StreamExt as _;
use playready::Pssh;
use serde::{Deserialize, Serialize};
use url::Url;
use zeroize::{Zeroize as _, Zeroizing};

pub(crate) use crate::drm::{DrmKeyMap as DashKeyMap, EncryptionScheme, ProtectionSystem};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DashEncryption {
    pub(crate) scheme: EncryptionScheme,
    pub(crate) default_kid: [u8; 16],
    pub(crate) system: ProtectionSystem,
}

const MP4_PROTECTION_SCHEME: &str = "urn:mpeg:dash:mp4protection:2011";
const WIDEVINE_SCHEME: &str = "urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed";
const PLAYREADY_SCHEME: &str = "urn:uuid:9a04f079-9840-4286-ab92-e65be0885f95";
const CLEARKEY_SCHEME: &str = "urn:uuid:e2719d58-a985-b3c9-781a-ab030af78d30";
const MAX_LICENSE_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_LICENSE_KEYS: usize = 64;
const LICENSE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Resolve inherited DASH ContentProtection descriptors into one supported
/// Common Encryption scheme and one default KID.
///
/// ClearKey, PlayReady, and Widevine are distinguished during planning.
/// The runner executes ClearKey and configured PlayReady acquisitions; Widevine
/// remains on extractor fallback because no audited Rust CDM backend is available.
/// Fragment-local seig KID rotation is resolved from media fragments; conflicting
/// default KIDs across descriptor scopes remain unsupported.
pub(crate) fn encryption_from_scopes(
    scopes: &[&[dash_mpd_core::ContentProtection]],
) -> Result<Option<DashEncryption>, String> {
    let mut selected = None;
    let mut system = None;

    for descriptor in scopes.iter().flat_map(|scope| scope.iter()) {
        if descriptor.schemeIdUri == MP4_PROTECTION_SCHEME {
            let scheme = match descriptor
                .value
                .as_deref()
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                Some("cenc") => EncryptionScheme::Cenc,
                Some("cbcs") => EncryptionScheme::Cbcs,
                _ => {
                    return Err("DASH Common Encryption must declare cenc or cbcs".to_string());
                }
            };
            let kid = descriptor
                .default_KID
                .as_deref()
                .and_then(parse_hex_16)
                .ok_or_else(|| {
                    "DASH Common Encryption must declare exactly one valid default_KID".to_string()
                })?;
            let encryption = DashEncryption {
                scheme,
                default_kid: kid,
                system: system.unwrap_or(ProtectionSystem::ClearKey),
            };
            if selected.is_some_and(|previous: DashEncryption| {
                previous.scheme != encryption.scheme
                    || previous.default_kid != encryption.default_kid
            }) {
                return Err(
                    "DASH uses conflicting encryption schemes or default KIDs across scopes"
                        .to_string(),
                );
            }
            selected = Some(encryption);
        } else if descriptor.schemeIdUri == CLEARKEY_SCHEME
            || descriptor.schemeIdUri == WIDEVINE_SCHEME
            || descriptor.schemeIdUri == PLAYREADY_SCHEME
        {
            let next = match descriptor.schemeIdUri.as_str() {
                WIDEVINE_SCHEME => ProtectionSystem::Widevine,
                PLAYREADY_SCHEME => ProtectionSystem::PlayReady,
                _ => ProtectionSystem::ClearKey,
            };
            if system
                .replace(next)
                .is_some_and(|previous| previous != next)
            {
                return Err("DASH declares conflicting DRM systems".to_string());
            }
        } else if descriptor.schemeIdUri.starts_with("urn:uuid:") {
            return Err("DASH declares an unsupported protection system".to_string());
        } else {
            return Err("DASH declares an unsupported ContentProtection scheme".to_string());
        }
    }

    if system.is_some() && selected.is_none() {
        return Err(
            "DASH advertises a DRM system but has no supported mp4protection cenc/cbcs default_KID"
                .to_string(),
        );
    }

    Ok(selected.map(|mut encryption: DashEncryption| {
        encryption.system = system.unwrap_or(ProtectionSystem::ClearKey);
        encryption
    }))
}

/// Decode and validate Widevine PSSH boxes from inherited DASH protection scopes.
pub(crate) fn widevine_pssh_from_scopes(
    scopes: &[&[dash_mpd_core::ContentProtection]],
) -> Result<Vec<Vec<u8>>, String> {
    let mut pssh_boxes = Vec::new();
    for descriptor in scopes.iter().flat_map(|scope| scope.iter()) {
        if descriptor.schemeIdUri != WIDEVINE_SCHEME {
            continue;
        }
        for pssh in &descriptor.cenc_pssh {
            let Some(encoded) = pssh
                .content
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            else {
                continue;
            };
            let compact: Vec<u8> = encoded
                .bytes()
                .filter(|byte| !byte.is_ascii_whitespace())
                .collect();
            let decoded = STANDARD
                .decode(compact)
                .map_err(|_| "DASH Widevine PSSH is not valid base64".to_string())?;
            crate::drm::validate_pssh(&decoded, ProtectionSystem::Widevine)?;
            if !pssh_boxes.contains(&decoded) {
                pssh_boxes.push(decoded);
            }
        }
    }
    Ok(pssh_boxes)
}

const PLAYREADY_PSSH_FIXED_BYTES: usize = 32;
const PLAYREADY_SYSTEM_ID: [u8; 16] = [
    0x9a, 0x04, 0xf0, 0x79, 0x98, 0x40, 0x42, 0x86, 0xab, 0x92, 0xe6, 0x5b, 0xe0, 0x88, 0x5f,
    0x95,
];

fn decode_bounded_protection_data(encoded: &str, label: &str) -> Result<Vec<u8>, String> {
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
    let invalid = || "DASH PlayReady PRO must contain exactly one WRMHEADER record".to_string();
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
    let record_type = u16::from_le_bytes(
        pro.get(6..8)
            .ok_or_else(invalid)?
            .try_into()
            .map_err(|_| invalid())?,
    );
    let record_len = u16::from_le_bytes(
        pro.get(8..10)
            .ok_or_else(invalid)?
            .try_into()
            .map_err(|_| invalid())?,
    );
    if record_count != 1
        || record_type != 1
        || usize::from(record_len) != pro.len().saturating_sub(10)
    {
        return Err(invalid());
    }

    let parsed = Pssh::from_bytes(pro)
        .map_err(|_| "DASH PlayReady PRO is malformed".to_string())?;
    if parsed.wrm_headers().len() != 1 {
        return Err(invalid());
    }
    Ok(())
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
                return Err(
                    "DASH PlayReady descriptor contains conflicting PSSH boxes".to_string(),
                );
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

    let pssh_len = PLAYREADY_PSSH_FIXED_BYTES
        .checked_add(pro.len())
        .filter(|length| *length <= crate::drm::MAX_PSSH_BYTES)
        .ok_or_else(|| "DASH PlayReady PRO exceeds the PSSH size limit".to_string())?;
    let pssh_data_len = u32::try_from(pro.len())
        .map_err(|_| "DASH PlayReady PRO exceeds the PSSH size limit".to_string())?;
    let pssh_box_len = u32::try_from(pssh_len)
        .map_err(|_| "DASH PlayReady PSSH exceeds its size limit".to_string())?;

    let mut pssh = Vec::with_capacity(pssh_len);
    pssh.extend_from_slice(&pssh_box_len.to_be_bytes());
    pssh.extend_from_slice(b"pssh");
    pssh.extend_from_slice(&[0, 0, 0, 0]);
    pssh.extend_from_slice(&PLAYREADY_SYSTEM_ID);
    pssh.extend_from_slice(&pssh_data_len.to_be_bytes());
    pssh.extend_from_slice(&pro);
    validate_playready_pssh(&pssh)?;
    Ok(Some(pssh))
}

/// Decode PlayReady initialization data from inherited DASH protection scopes.
///
/// The most-specific scope containing PlayReady initialization data overrides
/// its ancestors.
pub(crate) fn playready_pssh_from_scopes(
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
            if scope_pssh.as_ref().is_some_and(|previous| previous != &pssh) {
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

/// Resolve ClearKey or PlayReady license URLs across inherited DASH protection scopes.
///
/// Relative URLs are resolved against the MPD. URL user-info, fragments, and
/// non-HTTPS schemes are rejected. PlayReady uses an explicit DASHIF Laurl;
/// embedded WRMHEADER LA_URL values are not treated as a fallback.
pub(crate) fn license_url_from_scopes(
    scopes: &[&[dash_mpd_core::ContentProtection]],
    manifest_url: &Url,
) -> Result<Option<Url>, String> {
    let mut has_clear_key = false;
    let mut has_playready = false;
    let mut selected: Option<Url> = None;

    for descriptor in scopes.iter().flat_map(|scope| scope.iter()) {
        let is_clear_key = descriptor.schemeIdUri == CLEARKEY_SCHEME;
        let is_playready = descriptor.schemeIdUri == PLAYREADY_SCHEME;
        if !is_clear_key && !is_playready {
            continue;
        }
        has_clear_key |= is_clear_key;
        has_playready |= is_playready;

        let raw = if is_playready {
            descriptor.laurl.as_ref()
        } else {
            descriptor
                .laurl
                .as_ref()
                .or(descriptor.clearkey_laurl.as_ref())
        }
        .and_then(|laurl| laurl.content.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty());
        let Some(raw) = raw else {
            continue;
        };
        let url = manifest_url.join(raw).map_err(|_| {
            if is_playready {
                "DASH PlayReady license URL is invalid".to_string()
            } else {
                "DASH ClearKey license URL is invalid".to_string()
            }
        })?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(
                "DASH license URL must be HTTPS and contain no user-info or fragment".to_string(),
            );
        }
        selected = Some(url);
    }

    if has_clear_key && selected.is_none() {
        return Err("DASH ClearKey protection has no license URL".to_string());
    }
    if has_playready && selected.is_none() {
        return Err("DASH PlayReady protection has no DASHIF Laurl".to_string());
    }
    Ok(selected)
}

#[derive(Serialize)]
struct ClearKeyLicenseRequest<'a> {
    kids: Vec<String>,
    #[serde(rename = "type")]
    license_type: &'a str,
}

#[derive(Deserialize)]
struct ClearKeyLicenseResponse {
    keys: Vec<ClearKeyJwk>,
    #[serde(rename = "type")]
    license_type: Option<String>,
}

#[derive(Deserialize)]
struct ClearKeyJwk {
    kty: Zeroizing<String>,
    kid: Zeroizing<String>,
    k: Zeroizing<String>,
}

/// Acquire all selected track keys from their HTTPS ClearKey license endpoints.
///
/// Keys are retained only in zeroizing memory and are never written to disk or logs.
pub(crate) async fn acquire_required_keys(
    client: &reqwest::Client,
    requests: &[(Url, BTreeSet<[u8; 16]>)],
) -> Result<Option<DashKeyMap>, String> {
    if requests.is_empty() {
        return Ok(None);
    }
    if requests.iter().any(|(_, kids)| kids.is_empty()) {
        return Err("DASH ClearKey acquisition requires at least one sample KID".to_string());
    }
    if requests
        .iter()
        .any(|(_, kids)| kids.len() > MAX_LICENSE_KEYS)
    {
        return Err(format!(
            "DASH ClearKey requests cannot exceed {MAX_LICENSE_KEYS} distinct KIDs per license URL"
        ));
    }

    let mut groups: Vec<(Url, BTreeSet<[u8; 16]>)> = Vec::new();
    for (license_url, required_kids) in requests {
        if let Some((_, kids)) = groups.iter_mut().find(|(url, _)| url == license_url) {
            kids.extend(required_kids.iter().copied());
        } else {
            groups.push((license_url.clone(), required_kids.clone()));
        }
    }

    if groups.iter().any(|(_, kids)| kids.len() > MAX_LICENSE_KEYS) {
        return Err(format!(
            "DASH ClearKey requests cannot exceed {MAX_LICENSE_KEYS} distinct KIDs per license URL"
        ));
    }

    let mut keys: BTreeMap<[u8; 16], Zeroizing<[u8; 16]>> = BTreeMap::new();
    for (license_url, required) in groups {
        let kids = required
            .iter()
            .map(|kid| URL_SAFE_NO_PAD.encode(kid))
            .collect();
        let request = ClearKeyLicenseRequest {
            kids,
            license_type: "temporary",
        };
        let request_body = serde_json::to_vec(&request)
            .map_err(|_| "Could not encode the DASH ClearKey license request".to_string())?;
        let response = tokio::time::timeout(
            LICENSE_REQUEST_TIMEOUT,
            client
                .post(license_url)
                .timeout(LICENSE_REQUEST_TIMEOUT)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header(reqwest::header::ACCEPT, "application/json")
                .body(request_body)
                .send(),
        )
        .await
        .map_err(|_| "The DASH ClearKey license request timed out".to_string())?
        .map_err(|error| {
            format!(
                "Could not contact the DASH ClearKey license server: {}",
                error.without_url()
            )
        })?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!(
                "The DASH ClearKey license server returned HTTP {}",
                status.as_u16()
            ));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_LICENSE_RESPONSE_BYTES as u64)
        {
            return Err("The DASH ClearKey license response exceeds its size limit".to_string());
        }

        let mut body = Zeroizing::new(Vec::with_capacity(MAX_LICENSE_RESPONSE_BYTES));
        let mut stream = response.bytes_stream();
        while let Some(chunk) = tokio::time::timeout(LICENSE_REQUEST_TIMEOUT, stream.next())
            .await
            .map_err(|_| "The DASH ClearKey license response timed out".to_string())?
        {
            let chunk = chunk.map_err(|error| {
                format!(
                    "Could not read the DASH ClearKey license response: {}",
                    error.without_url()
                )
            })?;
            let next_len = body
                .len()
                .checked_add(chunk.len())
                .filter(|length| *length <= MAX_LICENSE_RESPONSE_BYTES)
                .ok_or_else(|| {
                    "The DASH ClearKey license response exceeds its size limit".to_string()
                })?;
            body.extend_from_slice(&chunk);
            debug_assert_eq!(body.len(), next_len);
        }

        let received = parse_clear_key_license(&mut body, &required)?;
        for (kid, key) in received {
            if let Some(previous) = keys.get(&kid) {
                if **previous != *key {
                    return Err(
                        "DASH license servers returned conflicting keys for one KID".to_string()
                    );
                }
            } else {
                keys.insert(kid, key);
            }
        }
    }

    for (_, required_kids) in requests {
        for kid in required_kids {
            if !keys.contains_key(kid) {
                return Err(format!(
                    "The DASH ClearKey license response did not include selected KID {}",
                    hex_string(kid)
                ));
            }
        }
    }
    Ok(Some(DashKeyMap::from_keys(keys)?))
}

fn parse_clear_key_license(
    body: &mut Vec<u8>,
    required: &BTreeSet<[u8; 16]>,
) -> Result<BTreeMap<[u8; 16], Zeroizing<[u8; 16]>>, String> {
    let parsed = serde_json::from_slice::<ClearKeyLicenseResponse>(body);
    body.zeroize();
    let mut response = parsed
        .map_err(|_| "The DASH ClearKey license response is not a valid JWK Set".to_string())?;
    if response
        .license_type
        .as_deref()
        .is_some_and(|license_type| license_type != "temporary")
        || response.keys.is_empty()
        || response.keys.len() > MAX_LICENSE_KEYS
    {
        return Err(
            "The DASH ClearKey license response has an unsupported type or key count".to_string(),
        );
    }

    let mut result: BTreeMap<[u8; 16], Zeroizing<[u8; 16]>> = BTreeMap::new();
    for jwk in response.keys.drain(..) {
        if jwk.kty.as_str() != "oct" {
            return Err("The DASH ClearKey response contains a non-octet key".to_string());
        }
        let kid = decode_clear_key_value(&jwk.kid)?;
        let key = Zeroizing::new(decode_clear_key_value(&jwk.k)?);
        if !required.contains(&kid) {
            continue;
        }
        if let Some(previous) = result.get(&kid) {
            if **previous != *key {
                return Err(
                    "The DASH ClearKey response repeats a KID with different keys".to_string(),
                );
            }
        } else {
            result.insert(kid, key);
        }
    }

    for kid in required {
        if !result.contains_key(kid) {
            return Err(format!(
                "The DASH ClearKey license response did not include selected KID {}",
                hex_string(kid)
            ));
        }
    }
    Ok(result)
}

fn decode_clear_key_value(value: &str) -> Result<[u8; 16], String> {
    let decoded =
        Zeroizing::new(URL_SAFE_NO_PAD.decode(value.as_bytes()).map_err(|_| {
            "The DASH ClearKey license contains invalid base64url data".to_string()
        })?);
    decoded
        .as_slice()
        .try_into()
        .map_err(|_| "The DASH ClearKey license contains a value that is not 128 bits".to_string())
}

fn parse_hex_16(value: &str) -> Option<[u8; 16]> {
    let compact: String = value
        .chars()
        .filter(|character| *character != '-')
        .collect();
    if compact.len() != 32 || !compact.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let (pairs, remainder) = compact.as_bytes().as_chunks::<2>();
    if !remainder.is_empty() {
        return None;
    }
    let mut output = [0_u8; 16];
    for (index, pair) in pairs.iter().enumerate() {
        let high = hex_nibble(pair[0])?;
        let low = hex_nibble(pair[1])?;
        output[index] = (high << 4) | low;
    }
    Some(output)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub(super) fn hex_string(bytes: &[u8; 16]) -> String {
    let mut output = String::with_capacity(32);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

#[path = "dash_drm_mp4.rs"]
mod mp4;

pub(crate) use mp4::{decrypt_track, required_kids};

#[cfg(test)]
mod tests {
    use super::*;

    fn protection(scheme: &str, kid: &str) -> dash_mpd_core::ContentProtection {
        dash_mpd_core::ContentProtection {
            schemeIdUri: MP4_PROTECTION_SCHEME.to_string(),
            value: Some(scheme.to_string()),
            default_KID: Some(kid.to_string()),
            ..Default::default()
        }
    }

    fn widevine() -> dash_mpd_core::ContentProtection {
        dash_mpd_core::ContentProtection {
            schemeIdUri: WIDEVINE_SCHEME.to_string(),
            value: Some("widevine".to_string()),
            ..Default::default()
        }
    }

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

    #[test]
    fn parses_widevine_pssh_and_associates_its_protection_system() {
        let pssh = "AAAAIHBzc2gAAAAA7e+LqXnWSs6jyCfc1R0h7QAAAAA=";
        let mut descriptor = widevine();
        descriptor.cenc_pssh.push(dash_mpd_core::CencPssh {
            content: Some(format!("\n  {pssh}\n")),
        });
        let common_encryption = protection("cenc", "00112233445566778899aabbccddeeff");
        let scopes = [
            std::slice::from_ref(&common_encryption),
            std::slice::from_ref(&descriptor),
        ];

        let encryption = encryption_from_scopes(&scopes)
            .expect("Widevine CENC metadata should be accepted")
            .expect("Widevine CENC metadata should be present");
        assert_eq!(encryption.system, ProtectionSystem::Widevine);
        assert_eq!(
            widevine_pssh_from_scopes(&scopes).expect("Widevine PSSH should decode"),
            vec![STANDARD.decode(pssh).expect("test PSSH base64")]
        );
    }

    #[test]
    fn rejects_malformed_and_retains_distinct_widevine_pssh() {
        let mut malformed = widevine();
        malformed.cenc_pssh.push(dash_mpd_core::CencPssh {
            content: Some("bm90LWEtcHNzaA==".to_string()),
        });
        assert!(widevine_pssh_from_scopes(&[std::slice::from_ref(&malformed)]).is_err());

        let mut wrong_system = widevine();
        wrong_system.cenc_pssh.push(dash_mpd_core::CencPssh {
            content: Some("AAAAIHBzc2gAAAAAEREREREREREREREREREREQAAAAA=".to_string()),
        });
        assert!(widevine_pssh_from_scopes(&[std::slice::from_ref(&wrong_system)]).is_err());

        let mut multiple = widevine();
        multiple.cenc_pssh.extend([
            dash_mpd_core::CencPssh {
                content: Some("AAAAIHBzc2gAAAAA7e+LqXnWSs6jyCfc1R0h7QAAAAA=".to_string()),
            },
            dash_mpd_core::CencPssh {
                content: Some("AAAAIXBzc2gAAAAA7e+LqXnWSs6jyCfc1R0h7QAAAAEB".to_string()),
            },
            dash_mpd_core::CencPssh {
                content: Some(
                    "AAAANHBzc2gBAAAA7e+LqXnWSs6jyCfc1R0h7QAAAAEAESIzRFVmd4iZqrvM3e7/AAAAAA=="
                        .to_string(),
                ),
            },
        ]);
        let retained = widevine_pssh_from_scopes(&[std::slice::from_ref(&multiple)])
            .expect("multiple valid PSSH boxes should be retained");
        assert_eq!(retained.len(), 3);
    }

    #[test]
    fn resolves_cenc_and_cbcs_kids() {
        let cenc = protection("cenc", "00112233-4455-6677-8899-aabbccddeeff");
        let cbcs = protection("cbcs", "ffeeddcc-bbaa-9988-7766-554433221100");

        let cenc_result = encryption_from_scopes(&[std::slice::from_ref(&cenc)])
            .expect("CENC descriptor should be valid")
            .expect("CENC descriptor should be recognized");
        assert_eq!(cenc_result.scheme, EncryptionScheme::Cenc);
        assert_eq!(cenc_result.system, ProtectionSystem::ClearKey);
        assert_eq!(
            cenc_result.default_kid,
            [
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff,
            ]
        );

        let cbcs_result = encryption_from_scopes(&[std::slice::from_ref(&cbcs)])
            .expect("CBCS descriptor should be valid")
            .expect("CBCS descriptor should be recognized");
        assert_eq!(cbcs_result.scheme, EncryptionScheme::Cbcs);
        assert_eq!(
            cbcs_result.default_kid,
            [
                0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa, 0x99, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22,
                0x11, 0x00,
            ]
        );
    }

    #[test]
    fn rejects_drm_system_descriptor_without_mp4protection_default_kid() {
        let widevine = widevine();
        assert!(
            encryption_from_scopes(&[std::slice::from_ref(&widevine)]).is_err(),
            "a DRM UUID descriptor alone must not be mistaken for clear media"
        );
    }

    #[test]
    fn rejects_conflicting_inherited_kids_and_schemes() {
        let inherited = protection("cenc", "00112233445566778899aabbccddeeff");
        let conflicting_kid = protection("cenc", "ffeeddccbbaa99887766554433221100");
        let conflicting_scheme = protection("cbcs", "00112233445566778899aabbccddeeff");
        assert!(
            encryption_from_scopes(&[
                std::slice::from_ref(&inherited),
                std::slice::from_ref(&conflicting_kid),
            ])
            .is_err()
        );
        assert!(
            encryption_from_scopes(&[
                std::slice::from_ref(&inherited),
                std::slice::from_ref(&conflicting_scheme),
            ])
            .is_err()
        );
    }

    #[test]
    fn rejects_unsupported_schemes_and_malformed_kids() {
        let unsupported = protection("cens", "00112233445566778899aabbccddeeff");
        let malformed = protection("cenc", "00112233445566778899aabbccddeef");
        assert!(encryption_from_scopes(&[std::slice::from_ref(&unsupported)]).is_err());
        assert!(encryption_from_scopes(&[std::slice::from_ref(&malformed)]).is_err());
    }

    #[test]
    fn clear_key_license_url_resolves_relative_to_the_mpd() {
        let descriptor = dash_mpd_core::ContentProtection {
            schemeIdUri: CLEARKEY_SCHEME.to_string(),
            laurl: Some(dash_mpd_core::Laurl {
                content: Some("../license".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let url = license_url_from_scopes(
            &[std::slice::from_ref(&descriptor)],
            &Url::parse("https://media.example.test/path/manifest.mpd").expect("test MPD URL"),
        )
        .expect("ClearKey license URL should parse")
        .expect("ClearKey URL should be present");
        assert_eq!(url.as_str(), "https://media.example.test/license");
    }

    #[test]
    fn rejects_clear_key_license_urls_without_secure_origin() {
        for value in ["http://license.example.test/key", "javascript:alert(1)"] {
            let descriptor = dash_mpd_core::ContentProtection {
                schemeIdUri: CLEARKEY_SCHEME.to_string(),
                laurl: Some(dash_mpd_core::Laurl {
                    content: Some(value.to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            assert!(
                license_url_from_scopes(
                    &[std::slice::from_ref(&descriptor)],
                    &Url::parse("https://media.example.test/manifest.mpd").expect("test MPD URL"),
                )
                .is_err(),
                "insecure license URL should be rejected: {value}"
            );
        }
    }

    #[test]
    fn clear_key_request_uses_the_eme_temporary_request_shape() {
        let request = ClearKeyLicenseRequest {
            kids: vec![URL_SAFE_NO_PAD.encode([0x11; 16])],
            license_type: "temporary",
        };
        let value = serde_json::to_value(request).expect("serialize ClearKey request");
        assert_eq!(
            value,
            serde_json::json!({
                "kids": [URL_SAFE_NO_PAD.encode([0x11; 16])],
                "type": "temporary"
            })
        );
    }

    #[test]
    fn clear_key_license_response_returns_only_requested_128_bit_keys() {
        let kid = [0x11; 16];
        let key = [0x22; 16];
        let extra_kid = [0x33; 16];
        let extra_key = [0x44; 16];
        let body = serde_json::json!({
            "type": "temporary",
            "keys": [
                {
                    "kty": "oct",
                    "kid": URL_SAFE_NO_PAD.encode(kid),
                    "k": URL_SAFE_NO_PAD.encode(key)
                },
                {
                    "kty": "oct",
                    "kid": URL_SAFE_NO_PAD.encode(extra_kid),
                    "k": URL_SAFE_NO_PAD.encode(extra_key)
                }
            ]
        });
        let mut bytes = serde_json::to_vec(&body).expect("serialize test JWK Set");
        let parsed = parse_clear_key_license(&mut bytes, &BTreeSet::from([kid]))
            .expect("valid ClearKey license response");
        assert!(bytes.iter().all(|byte| *byte == 0));
        assert_eq!(parsed.len(), 1);
        assert_eq!(&**parsed.get(&kid).expect("requested key"), &key);
        assert!(!parsed.contains_key(&extra_kid));
    }

    #[test]
    fn rejects_clear_key_license_responses_with_missing_or_malformed_keys() {
        let kid = [0x11; 16];
        let required = BTreeSet::from([kid]);
        let missing = serde_json::json!({"keys": []});
        let mut missing_bytes = serde_json::to_vec(&missing).expect("serialize missing keyset");
        assert!(parse_clear_key_license(&mut missing_bytes, &required).is_err());

        let malformed = serde_json::json!({
            "keys": [{
                "kty": "oct",
                "kid": URL_SAFE_NO_PAD.encode(kid),
                "k": "not-a-128-bit-key"
            }]
        });
        let mut malformed_bytes =
            serde_json::to_vec(&malformed).expect("serialize malformed keyset");
        assert!(parse_clear_key_license(&mut malformed_bytes, &required).is_err());

        let wrong_type = serde_json::json!({
            "type": "persistent-license",
            "keys": [{
                "kty": "oct",
                "kid": URL_SAFE_NO_PAD.encode(kid),
                "k": URL_SAFE_NO_PAD.encode([0x22; 16])
            }]
        });
        let mut wrong_type_bytes =
            serde_json::to_vec(&wrong_type).expect("serialize persistent keyset");
        assert!(parse_clear_key_license(&mut wrong_type_bytes, &required).is_err());
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
        assert_eq!(encryption.system, ProtectionSystem::PlayReady);
        assert_eq!(encryption.scheme, EncryptionScheme::Cenc);
    }

    #[test]
    fn extracts_playready_pssh_from_mspr_pro() {
        let descriptor = dash_mpd_core::ContentProtection {
            schemeIdUri: PLAYREADY_SCHEME.to_string(),
            msprpro: Some(dash_mpd_core::MsprPro {
                content: Some(STANDARD.encode(playready_pro())),
                ..Default::default()
            }),
            ..Default::default()
        };
        let pssh = playready_pssh_from_scopes(&[std::slice::from_ref(&descriptor)])
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
        let mut pssh = Vec::with_capacity(32 + pro.len());
        pssh.extend_from_slice(
            &u32::try_from(32 + pro.len())
                .expect("PSSH length")
                .to_be_bytes(),
        );
        pssh.extend_from_slice(b"pssh");
        pssh.extend_from_slice(&[0, 0, 0, 0]);
        pssh.extend_from_slice(&PLAYREADY_SYSTEM_ID);
        pssh.extend_from_slice(&u32::try_from(pro.len()).expect("PRO length").to_be_bytes());
        pssh.extend_from_slice(&pro);

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
        let selected = playready_pssh_from_scopes(&[std::slice::from_ref(&descriptor)])
            .expect("valid cenc:pssh should take precedence");
        assert_eq!(selected.as_deref(), Some(pssh.as_slice()));
    }

    #[test]
    #[test]
    fn rejects_playready_pro_with_extra_records_or_trailing_bytes() {
        let valid = playready_pro();
        let descriptor = |pro: Vec<u8>| dash_mpd_core::ContentProtection {
            schemeIdUri: PLAYREADY_SCHEME.to_string(),
            msprpro: Some(dash_mpd_core::MsprPro {
                content: Some(STANDARD.encode(pro)),
                ..Default::default()
            }),
            ..Default::default()
        };

        let mut extra_record = valid.clone();
        extra_record[4..6].copy_from_slice(&2_u16.to_le_bytes());
        assert!(
            playready_pssh_from_scopes(&[&[descriptor(extra_record)]]).is_err(),
            "unexpected extra PRO record must fail closed"
        );

        let mut trailing_bytes = valid;
        trailing_bytes.push(0);
        assert!(
            playready_pssh_from_scopes(&[&[descriptor(trailing_bytes)]]).is_err(),
            "PRO trailing bytes must fail closed"
        );
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

        let parse_wr_header = |pro: &[u8]| {
            Pssh::from_bytes(pro)
                .expect("test PRO should parse")
                .wrm_headers()
                .len()
        };
        assert_eq!(parse_wr_header(&first), 1);
        assert_eq!(parse_wr_header(&second), 1);

        let descriptor = |pro: Vec<u8>| dash_mpd_core::ContentProtection {
            schemeIdUri: PLAYREADY_SCHEME.to_string(),
            msprpro: Some(dash_mpd_core::MsprPro {
                content: Some(STANDARD.encode(pro)),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(playready_pssh_from_scopes(&[&[descriptor(first), descriptor(second)]]).is_err());
    }

    #[test]
    fn playready_pssh_inherits_until_a_more_specific_scope_overrides_it() {
        let parent = dash_mpd_core::ContentProtection {
            schemeIdUri: PLAYREADY_SCHEME.to_string(),
            msprpro: Some(dash_mpd_core::MsprPro {
                content: Some(STANDARD.encode(playready_pro())),
                ..Default::default()
            }),
            ..Default::default()
        };
        let child = dash_mpd_core::ContentProtection {
            schemeIdUri: PLAYREADY_SCHEME.to_string(),
            ..Default::default()
        };
        let parent_pssh =
            playready_pssh_from_scopes(&[std::slice::from_ref(&parent)])
                .expect("parent PlayReady data")
                .expect("parent PSSH");
        assert_eq!(
            playready_pssh_from_scopes(&[
                std::slice::from_ref(&parent),
                std::slice::from_ref(&child),
            ])
            .expect("inherited PlayReady data")
            .as_deref(),
            Some(parent_pssh.as_slice())
        );

        let overriding = dash_mpd_core::ContentProtection {
            schemeIdUri: PLAYREADY_SCHEME.to_string(),
            msprpro: Some(dash_mpd_core::MsprPro {
                content: Some(STANDARD.encode({
                    let mut pro = playready_pro();
                    let encoded_url: Vec<u8> = "Acquire"
                        .encode_utf16()
                        .flat_map(u16::to_le_bytes)
                        .collect();
                    let offset = pro
                        .windows(encoded_url.len())
                        .position(|window| window == encoded_url)
                        .expect("test PRO URL");
                    pro[offset] = b'B';
                    pro
                })),
                ..Default::default()
            }),
            ..Default::default()
        };
        let overriding_pssh = playready_pssh_from_scopes(&[
            std::slice::from_ref(&parent),
            std::slice::from_ref(&overriding),
        ])
        .expect("representation PlayReady data")
        .expect("representation PSSH");
        assert_ne!(overriding_pssh, parent_pssh);
    }

    #[test]
    fn playready_license_url_uses_dashif_laurl_and_requires_https() {
        let descriptor = dash_mpd_core::ContentProtection {
            schemeIdUri: PLAYREADY_SCHEME.to_string(),
            laurl: Some(dash_mpd_core::Laurl {
                content: Some("../license".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let manifest =
            Url::parse("https://media.example.test/path/manifest.mpd").expect("manifest URL");
        let url = license_url_from_scopes(&[std::slice::from_ref(&descriptor)], &manifest)
            .expect("relative PlayReady license URL should parse")
            .expect("license URL should be present");
        assert_eq!(url.as_str(), "https://media.example.test/license");

        for value in ["http://license.example.test/key", "javascript:alert(1)"] {
            let insecure = dash_mpd_core::ContentProtection {
                schemeIdUri: PLAYREADY_SCHEME.to_string(),
                laurl: Some(dash_mpd_core::Laurl {
                    content: Some(value.to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            assert!(
                license_url_from_scopes(&[std::slice::from_ref(&insecure)], &manifest).is_err()
            );
        }
    }

    #[test]
    fn rejects_widevine_without_a_supported_cdm() {
        let descriptor = widevine();
        assert!(encryption_from_scopes(&[std::slice::from_ref(&descriptor)]).is_err());
    }

    #[test]
    fn clear_content_without_protection_remains_unencrypted() {
        assert_eq!(encryption_from_scopes(&[&[]]).expect("no protection"), None);
    }
}
