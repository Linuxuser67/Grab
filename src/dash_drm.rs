//! Bounded DASH Common Encryption support using HTTPS ClearKey licenses.
//!
//! Grab keeps downloaded encrypted resources as its resumable source of truth.
//! License keys are held in zeroizing memory, and decryption produces a separate
//! private track for FFmpeg without modifying encrypted source resources.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};
use url::Url;
use zeroize::{Zeroize as _, Zeroizing};

const MP4_PROTECTION_SCHEME: &str = "urn:mpeg:dash:mp4protection:2011";
const WIDEVINE_SCHEME: &str = "urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed";
const WIDEVINE_SYSTEM_ID: [u8; 16] = [0xed, 0xef, 0x8b, 0xa9, 0x79, 0xd6, 0x4a, 0xce, 0xa3, 0xc8, 0x27, 0xdc, 0xd5, 0x1d, 0x21, 0xed];
const PLAYREADY_SCHEME: &str = "urn:uuid:9a04f079-9840-4286-ab92-e65be0885f95";
const CLEARKEY_SCHEME: &str = "urn:uuid:e2719d58-a985-b3c9-781a-ab030af78d30";
const MAX_LICENSE_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_LICENSE_KEYS: usize = 64;
const LICENSE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EncryptionScheme {
    Cenc,
    Cbcs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DashEncryption {
    pub(crate) scheme: EncryptionScheme,
    pub(crate) default_kid: [u8; 16],
    pub(crate) system: ProtectionSystem,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProtectionSystem {
    ClearKey,
    Widevine,
}

pub(crate) struct DashKeyMap {
    keys: BTreeMap<[u8; 16], Zeroizing<[u8; 16]>>,
}

impl DashKeyMap {
    pub(crate) fn key_for_kid(&self, kid: &[u8; 16]) -> Result<Zeroizing<[u8; 16]>, String> {
        self.keys
            .get(kid)
            .map(|key| Zeroizing::new(**key))
            .ok_or_else(|| {
                format!(
                    "The DASH license response did not include selected KID {}",
                    hex_string(kid)
                )
            })
    }
}

/// Resolve inherited DASH ContentProtection descriptors into one supported
/// Common Encryption scheme and one default KID.
///
/// Native license acquisition currently supports ClearKey only. Widevine and
/// PlayReady descriptors are rejected until their CDM exchanges have a safe,
/// reusable integration. Fragment-local seig KID rotation is resolved from
/// media fragments; conflicting default KIDs across descriptor scopes remain unsupported.
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
            if selected.is_some_and(|previous| previous != encryption) {
                return Err(
                    "DASH uses conflicting encryption schemes or default KIDs across scopes"
                        .to_string(),
                );
            }
            selected = Some(encryption);
        } else if descriptor.schemeIdUri == CLEARKEY_SCHEME
            || descriptor.schemeIdUri == WIDEVINE_SCHEME
        {
            let next = if descriptor.schemeIdUri == WIDEVINE_SCHEME {
                ProtectionSystem::Widevine
            } else {
                ProtectionSystem::ClearKey
            };
            if system.replace(next).is_some_and(|previous| previous != next) {
                return Err("DASH declares conflicting DRM systems".to_string());
            }
        } else if descriptor.schemeIdUri == PLAYREADY_SCHEME {
            return Err("Native DASH PlayReady support is not available yet".to_string());
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

/// Decode and validate the Widevine PSSH declared by inherited DASH protection scopes.
pub(crate) fn widevine_pssh_from_scopes(
    scopes: &[&[dash_mpd_core::ContentProtection]],
) -> Result<Option<Vec<u8>>, String> {
    let mut selected: Option<Vec<u8>> = None;
    for descriptor in scopes.iter().flat_map(|scope| scope.iter()) {
        if descriptor.schemeIdUri != WIDEVINE_SCHEME {
            continue;
        }
        for pssh in &descriptor.cenc_pssh {
            let Some(encoded) = pssh.content.as_deref().map(str::trim).filter(|value| !value.is_empty()) else {
                continue;
            };
            let decoded = STANDARD
                .decode(encoded.as_bytes())
                .map_err(|_| "DASH Widevine PSSH is not valid base64".to_string())?;
            validate_widevine_pssh(&decoded)?;
            if selected.as_ref().is_some_and(|previous| previous != &decoded) {
                return Err("DASH declares conflicting Widevine PSSH boxes".to_string());
            }
            selected = Some(decoded);
        }
    }
    Ok(selected)
}

fn validate_widevine_pssh(bytes: &[u8]) -> Result<(), String> {
    let invalid = || "DASH Widevine PSSH box is malformed or has the wrong system ID".to_string();
    if bytes.len() < 32
        || bytes.get(4..8) != Some(b"pssh")
        || bytes.get(8).is_none_or(|version| *version > 1)
        || bytes.get(12..28) != Some(WIDEVINE_SYSTEM_ID.as_slice())
    {
        return Err(invalid());
    }
    let declared_size = u32::from_be_bytes(bytes[0..4].try_into().map_err(|_| invalid())?);
    if usize::try_from(declared_size).ok() != Some(bytes.len()) {
        return Err(invalid());
    }
    let version = bytes[8];
    let mut offset = 28_usize;
    if version == 1 {
        let count_bytes: [u8; 4] = bytes.get(offset..offset + 4).ok_or_else(invalid)?
            .try_into().map_err(|_| invalid())?;
        let count = usize::try_from(u32::from_be_bytes(count_bytes)).map_err(|_| invalid())?;
        offset = offset.checked_add(4)
            .and_then(|value| count.checked_mul(16).and_then(|kids| value.checked_add(kids)))
            .ok_or_else(invalid)?;
    }
    let data_size_bytes: [u8; 4] = bytes.get(offset..offset + 4).ok_or_else(invalid)?
        .try_into().map_err(|_| invalid())?;
    let data_size = usize::try_from(u32::from_be_bytes(data_size_bytes)).map_err(|_| invalid())?;
    let end = offset.checked_add(4).and_then(|value| value.checked_add(data_size))
        .ok_or_else(invalid)?;
    if end != bytes.len() {
        return Err(invalid());
    }
    Ok(())
}

/// Resolve the ClearKey license URL across inherited protection scopes.
///
/// Relative license URLs are resolved against the MPD. URL user-info,
/// fragments, and non-HTTPS schemes are rejected.
pub(crate) fn license_url_from_scopes(
    scopes: &[&[dash_mpd_core::ContentProtection]],
    manifest_url: &Url,
) -> Result<Option<Url>, String> {
    let mut is_clear_key = false;
    let mut selected: Option<Url> = None;

    for descriptor in scopes.iter().flat_map(|scope| scope.iter()) {
        if descriptor.schemeIdUri != CLEARKEY_SCHEME {
            continue;
        }
        is_clear_key = true;
        let raw = descriptor
            .laurl
            .as_ref()
            .or(descriptor.clearkey_laurl.as_ref())
            .and_then(|laurl| laurl.content.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let Some(raw) = raw else {
            continue;
        };
        let url = manifest_url
            .join(raw)
            .map_err(|_| "DASH ClearKey license URL is invalid".to_string())?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(
                "DASH ClearKey license URL must be HTTPS and contain no user-info or fragment"
                    .to_string(),
            );
        }
        // More specific inherited scopes may override the parent license URL.
        selected = Some(url);
    }

    if is_clear_key && selected.is_none() {
        return Err("DASH ClearKey protection has no license URL".to_string());
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
    Ok(Some(DashKeyMap { keys }))
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

    #[test]
    fn resolves_cenc_and_cbcs_kids() {
        let cenc = protection("cenc", "00112233-4455-6677-8899-aabbccddeeff");
        let cbcs = protection("cbcs", "ffeeddcc-bbaa-9988-7766-554433221100");

        let cenc_result = encryption_from_scopes(&[std::slice::from_ref(&cenc)])
            .expect("CENC descriptor should be valid")
            .expect("CENC descriptor should be recognized");
        assert_eq!(cenc_result.scheme, EncryptionScheme::Cenc);
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
    fn rejects_widevine_and_playready_without_cdms() {
        for scheme in [WIDEVINE_SCHEME, PLAYREADY_SCHEME] {
            let descriptor = dash_mpd_core::ContentProtection {
                schemeIdUri: scheme.to_string(),
                ..Default::default()
            };
            assert!(encryption_from_scopes(&[std::slice::from_ref(&descriptor)]).is_err());
        }
    }

    #[test]
    fn clear_content_without_protection_remains_unencrypted() {
        assert_eq!(encryption_from_scopes(&[&[]]).expect("no protection"), None);
    }
}
