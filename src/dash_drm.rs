//! Bounded DASH Common Encryption support for explicitly supplied content keys.
//!
//! Grab keeps downloaded encrypted resources as its resumable source of truth.
//! Decryption produces a separate private track for FFmpeg; the destination part
//! and its digest journal therefore remain bound to the original ciphertext.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use url::Url;

const MP4_PROTECTION_SCHEME: &str = "urn:mpeg:dash:mp4protection:2011";
const WIDEVINE_SCHEME: &str = "urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed";
const PLAYREADY_SCHEME: &str = "urn:uuid:9a04f079-9840-4286-ab92-e65be0885f95";
const CLEARKEY_SCHEME: &str = "urn:uuid:e2719d58-a985-b3c9-781a-ab030af78d30";
const MAX_KEY_FILE_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EncryptionScheme {
    Cenc,
    Cbcs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DashEncryption {
    pub(crate) scheme: EncryptionScheme,
    pub(crate) default_kid: [u8; 16],
}

pub(crate) struct DashKeyMap {
    keys: BTreeMap<[u8; 16], [u8; 16]>,
}

impl DashKeyMap {
    pub(crate) fn key_for(&self, encryption: DashEncryption) -> Result<[u8; 16], String> {
        self.keys
            .get(&encryption.default_kid)
            .copied()
            .ok_or_else(|| {
                format!(
                    "No content key is configured for DASH KID {}. Add a KID:KEY entry to {}.",
                    hex_string(&encryption.default_kid),
                    key_file_path().display()
                )
            })
    }
}

/// Resolve inherited DASH ContentProtection descriptors into one supported
/// Common Encryption scheme and one default KID.
///
/// This first implementation intentionally rejects key rotation and multiple
/// default KIDs. Recognized Widevine, PlayReady and ClearKey UUID descriptors
/// may accompany the MP4 protection descriptor; no license requests are made.
pub(crate) fn encryption_from_scopes(
    scopes: &[&[dash_mpd_core::ContentProtection]],
) -> Result<Option<DashEncryption>, String> {
    let mut selected = None;
    let mut has_system_descriptor = false;

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
            };
            if selected.is_some_and(|previous| previous != encryption) {
                return Err(
                    "DASH uses conflicting encryption schemes or default KIDs across scopes"
                        .to_string(),
                );
            }
            selected = Some(encryption);
        } else if descriptor.schemeIdUri.starts_with("urn:uuid:") {
            if descriptor.schemeIdUri != WIDEVINE_SCHEME
                && descriptor.schemeIdUri != PLAYREADY_SCHEME
                && descriptor.schemeIdUri != CLEARKEY_SCHEME
            {
                return Err("DASH declares an unsupported protection system".to_string());
            }
            has_system_descriptor = true;
        } else {
            return Err("DASH declares an unsupported ContentProtection scheme".to_string());
        }
    }

    if has_system_descriptor && selected.is_none() {
        return Err(
            "DASH advertises a DRM system but has no supported mp4protection cenc/cbcs default_KID"
                .to_string(),
        );
    }

    Ok(selected)
}

const MAX_LICENSE_CONFIG_BYTES: usize = 64 * 1024;
const MAX_WIDEVINE_DEVICE_BYTES: usize = 4 * 1024 * 1024;
const MAX_WIDEVINE_PSSH_BYTES: usize = 1024 * 1024;
const MAX_LICENSE_CHALLENGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_LICENSE_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const LICENSE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DrmLicenseFile {
    version: u32,
    #[serde(default)]
    widevine: Option<WidevineLicenseConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WidevineLicenseConfig {
    device: PathBuf,
    license_url: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    service_certificate: Option<String>,
}

#[derive(Clone, Copy)]
enum WidevineServiceCertificate {
    None,
    GoogleCommon,
    GoogleStaging,
}

/// Extract Widevine PSSH data from the protection descriptors inherited by a representation.
pub(crate) fn widevine_pssh_from_scopes(
    scopes: &[&[dash_mpd_core::ContentProtection]],
) -> Box<[String]> {
    let mut seen = BTreeSet::new();
    scopes
        .iter()
        .flat_map(|scope| scope.iter())
        .filter(|descriptor| descriptor.schemeIdUri == WIDEVINE_SCHEME)
        .flat_map(|descriptor| descriptor.cenc_pssh.iter())
        .filter_map(|pssh| pssh.content.as_deref())
        .map(str::trim)
        .filter(|content| !content.is_empty() && seen.insert((*content).to_string()))
        .map(str::to_string)
        .collect::<Vec<_>>()
        .into_boxed_slice()
}

fn key_file_path() -> PathBuf {
    glib::user_config_dir().join("grab/drm-keys.txt")
}

fn license_file_path() -> PathBuf {
    glib::user_config_dir().join("grab/drm-license.json")
}

/// Read a bounded regular file owned by this user, without following a final symlink.
fn read_private_file(
    path: &Path,
    max_bytes: usize,
    description: &str,
    optional: bool,
) -> Result<Option<Vec<u8>>, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if optional && error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("{description} file {} does not exist", path.display()));
        }
        Err(error) => {
            return Err(format!("Could not safely open {description} file {}: {error}", path.display()));
        }
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("Could not inspect {description} file {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{description} path {} is not a regular file", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let effective_uid = unsafe { libc::geteuid() };
        if metadata.uid() != effective_uid || metadata.mode() & 0o077 != 0 {
            return Err(format!(
                "{description} file {} must be owned by the current user and have mode 0600",
                path.display()
            ));
        }
    }
    let max_bytes_u64 = u64::try_from(max_bytes).unwrap_or(u64::MAX);
    if metadata.len() > max_bytes_u64 {
        return Err(format!(
            "{description} file {} exceeds its {max_bytes}-byte limit",
            path.display()
        ));
    }
    let capacity = usize::try_from(metadata.len()).unwrap_or(max_bytes);
    let mut bytes = Vec::with_capacity(capacity);
    let read_limit = max_bytes_u64.saturating_add(1);
    file.take(read_limit)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Could not read {description} file {}: {error}", path.display()))?;
    if bytes.len() > max_bytes {
        return Err(format!(
            "{description} file {} exceeds its {max_bytes}-byte limit",
            path.display()
        ));
    }
    Ok(Some(bytes))
}

fn load_configured_keys() -> Result<BTreeMap<[u8; 16], [u8; 16]>, String> {
    let path = key_file_path();
    let Some(bytes) = read_private_file(&path, MAX_KEY_FILE_BYTES, "DRM key", true)? else {
        return Ok(BTreeMap::new());
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| format!("{} must contain UTF-8 KID:KEY entries", path.display()))?;
    let mut keys = BTreeMap::new();

    for (line_index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (kid_text, key_text) = line.split_once(':').ok_or_else(|| {
            format!("{}:{} must use KID:KEY format", path.display(), line_index + 1)
        })?;
        let kid = parse_hex_16(kid_text).ok_or_else(|| {
            format!("{}:{} contains an invalid 128-bit KID", path.display(), line_index + 1)
        })?;
        let key = parse_hex_16(key_text).ok_or_else(|| {
            format!(
                "{}:{} contains an invalid 128-bit content key",
                path.display(),
                line_index + 1
            )
        })?;
        if keys.insert(kid, key).is_some() {
            return Err(format!(
                "{}:{} repeats a KID; each KID must have exactly one key",
                path.display(),
                line_index + 1
            ));
        }
    }
    Ok(keys)
}

fn load_license_config() -> Result<Option<WidevineLicenseConfig>, String> {
    let path = license_file_path();
    let Some(bytes) = read_private_file(&path, MAX_LICENSE_CONFIG_BYTES, "DRM license configuration", true)? else {
        return Ok(None);
    };
    let config: DrmLicenseFile = serde_json::from_slice(&bytes).map_err(|_| {
        format!(
            "{} is not valid DRM license configuration JSON",
            path.display()
        )
    })?;
    if config.version != 1 {
        return Err(format!(
            "{} uses unsupported configuration version {}",
            path.display(),
            config.version
        ));
    }
    Ok(config.widevine)
}

fn parse_widevine_service_certificate(
    value: Option<&str>,
) -> Result<WidevineServiceCertificate, String> {
    match value {
        None | Some("none") => Ok(WidevineServiceCertificate::None),
        Some("google-common") => Ok(WidevineServiceCertificate::GoogleCommon),
        Some("google-staging") => Ok(WidevineServiceCertificate::GoogleStaging),
        Some(_) => Err(
            "Widevine service_certificate must be none, google-common, or google-staging"
                .to_string(),
        ),
    }
}

fn validate_license_url(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw).map_err(|_| "The configured Widevine license_url is invalid".to_string())?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "Widevine license_url must be HTTPS, have no user information, and have no fragment"
                .to_string(),
        );
    }
    Ok(url)
}

fn parse_license_headers(
    headers: &BTreeMap<String, String>,
) -> Result<reqwest::header::HeaderMap, String> {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    let mut output = HeaderMap::new();
    let mut names = BTreeSet::new();
    if headers.len() > 64 {
        return Err("The Widevine license configuration contains too many headers".to_string());
    }
    for (raw_name, raw_value) in headers {
        let name = HeaderName::from_bytes(raw_name.as_bytes())
            .map_err(|_| "The Widevine license configuration contains an invalid header name".to_string())?;
        let normalized = name.as_str().to_ascii_lowercase();
        if matches!(
            normalized.as_str(),
            "host" | "content-length" | "content-type" | "connection" | "transfer-encoding"
                | "proxy-authorization"
        ) || !names.insert(normalized)
        {
            return Err(
                "The Widevine license configuration contains a forbidden or duplicate header"
                    .to_string(),
            );
        }
        if raw_value.len() > 16 * 1024 {
            return Err("A Widevine license header exceeds the size limit".to_string());
        }
        let mut value = HeaderValue::from_str(raw_value)
            .map_err(|_| "The Widevine license configuration contains an invalid header value".to_string())?;
        value.set_sensitive(true);
        output.insert(name, value);
    }
    Ok(output)
}

fn parse_license_content_type(value: Option<&str>) -> Result<reqwest::header::HeaderValue, String> {
    let value = value.unwrap_or("application/octet-stream");
    let header = reqwest::header::HeaderValue::from_str(value)
        .map_err(|_| "Widevine content_type is not a valid HTTP header value".to_string())?;
    if value.len() > 256 {
        return Err("Widevine content_type exceeds the size limit".to_string());
    }
    Ok(header)
}

/// Resolve selected encrypted video/audio keys from a private key file and, when needed,
/// the explicitly configured Widevine CDM. Network requests are made only to the
/// configured HTTPS license endpoint and never follow redirects.
pub(crate) async fn resolve_selected_keys(
    proxy: Option<&crate::net_types::ResolvedProxy>,
    parent_url: &str,
    selected: &[(DashEncryption, &[String])],
    guarded_child_hosts: &mut std::collections::HashSet<String>,
) -> Result<Option<DashKeyMap>, String> {
    let required: BTreeSet<[u8; 16]> = selected
        .iter()
        .map(|(encryption, _)| encryption.default_kid)
        .collect();
    if required.is_empty() {
        return Ok(None);
    }

    let mut keys = load_configured_keys()?;
    let missing = || required.iter().filter(|kid| !keys.contains_key(*kid)).copied().collect::<BTreeSet<_>>();
    let mut missing_kids = missing();
    if missing_kids.is_empty() {
        return Ok(Some(DashKeyMap { keys }));
    }

    let Some(config) = load_license_config()? else {
        return Err(missing_key_error(&missing_kids));
    };
    let device_path = config.device.clone();
    if !device_path.is_absolute() {
        return Err("Widevine device must be configured with an absolute .wvd path".to_string());
    }
    let license_url = validate_license_url(&config.license_url)?;
    let host = license_url
        .host_str()
        .ok_or_else(|| "Widevine license_url has no host".to_string())?
        .to_ascii_lowercase();
    if !guarded_child_hosts.contains(&host)
        && guarded_child_hosts.len() >= crate::download_net::MAX_GUARDED_CHILD_HOSTS_PER_COLLECTION
    {
        return Err(format!(
            "The stream manifest exceeds the limit of {} distinct resource hosts",
            crate::download_net::MAX_GUARDED_CHILD_HOSTS_PER_COLLECTION
        ));
    }
    if !crate::download_net::require_public_resolution(&host) {
        return Err("The configured Widevine license host is local or private".to_string());
    }
    guarded_child_hosts.insert(host);

    let mut pssh_to_kids: BTreeMap<String, BTreeSet<[u8; 16]>> = BTreeMap::new();
    for (encryption, pssh_values) in selected {
        if !missing_kids.contains(&encryption.default_kid) {
            continue;
        }
        for pssh in *pssh_values {
            let pssh = pssh.trim();
            if pssh.is_empty() {
                continue;
            }
            if pssh.len() > MAX_WIDEVINE_PSSH_BYTES {
                return Err("The Widevine PSSH in the MPD exceeds the size limit".to_string());
            }
            pssh_to_kids
                .entry(pssh.to_string())
                .or_default()
                .insert(encryption.default_kid);
        }
    }
    if pssh_to_kids.is_empty() {
        return Err(format!(
            "{}; the selected MPD does not provide a Widevine cenc:pssh value",
            missing_key_error(&missing_kids)
        ));
    }

    let headers = parse_license_headers(&config.headers)?;
    let content_type = parse_license_content_type(config.content_type.as_deref())?;
    let service_certificate =
        parse_widevine_service_certificate(config.service_certificate.as_deref())?;
    let client = crate::download_net::license_http_client(proxy)?;
    let Some(device_bytes) = read_private_file(
        &device_path,
        MAX_WIDEVINE_DEVICE_BYTES,
        "Widevine device",
        false,
    )? else {
        return Err("The configured Widevine device file could not be read".to_string());
    };
    let cdm = tokio::task::spawn_blocking(move || {
        widevine::Device::read_wvd(std::io::Cursor::new(device_bytes))
            .map(widevine::Cdm::new)
            .map_err(|_| "The configured Widevine .wvd device file is invalid".to_string())
    })
    .await
    .map_err(|error| format!("Widevine device task failed: {error}"))??;

    for (pssh, kids) in pssh_to_kids {
        if kids.iter().all(|kid| keys.contains_key(kid)) {
            continue;
        }
        let (request, challenge) = create_widevine_challenge(
            cdm.clone(),
            pssh,
            service_certificate,
        )
        .await?;
        if challenge.len() > MAX_LICENSE_CHALLENGE_BYTES {
            return Err("The generated Widevine license challenge exceeds the size limit".to_string());
        }
        let response = send_widevine_license_request(
            &client,
            &license_url,
            &headers,
            &content_type,
            challenge,
        )
        .await?;
        let acquired = parse_widevine_license_response(request, response, kids).await?;
        keys.extend(acquired);
        missing_kids = missing();
        if missing_kids.is_empty() {
            return Ok(Some(DashKeyMap { keys }));
        }
    }

    if missing_kids.is_empty() {
        Ok(Some(DashKeyMap { keys }))
    } else {
        Err(missing_key_error(&missing_kids))
    }
}

fn missing_key_error(missing: &BTreeSet<[u8; 16]>) -> String {
    let kid = missing
        .iter()
        .next()
        .map(hex_string)
        .unwrap_or_else(|| "unknown".to_string());
    format!(
        "No content key is configured or could be acquired for DASH KID {kid}. Configure {} for Widevine license acquisition or add a KID:KEY entry to {}.",
        license_file_path().display(),
        key_file_path().display()
    )
}

async fn create_widevine_challenge(
    cdm: widevine::Cdm,
    pssh_b64: String,
    service_certificate: WidevineServiceCertificate,
) -> Result<(widevine::CdmLicenseRequest, Vec<u8>), String> {
    tokio::task::spawn_blocking(move || {
        let pssh = widevine::Pssh::from_b64(&pssh_b64)
            .map_err(|_| "The MPD contains an invalid Widevine PSSH value".to_string())?;
        let session = cdm.open();
        let session = match service_certificate {
            WidevineServiceCertificate::None => session,
            WidevineServiceCertificate::GoogleCommon => session.set_service_certificate_common(),
            WidevineServiceCertificate::GoogleStaging => session.set_service_certificate_staging(),
        };
        let request = session
            .get_license_request(pssh, widevine::LicenseType::STREAMING)
            .map_err(|_| "The Widevine CDM could not create a license request".to_string())?;
        let challenge = request
            .challenge()
            .map_err(|_| "The Widevine CDM could not create a license challenge".to_string())?;
        Ok::<_, String>((request, challenge))
    })
    .await
    .map_err(|error| format!("Widevine challenge task failed: {error}"))?
}

async fn send_widevine_license_request(
    client: &reqwest::Client,
    license_url: &Url,
    headers: &reqwest::header::HeaderMap,
    content_type: &reqwest::header::HeaderValue,
    challenge: Vec<u8>,
) -> Result<Vec<u8>, String> {
    let response = tokio::time::timeout(
        LICENSE_REQUEST_TIMEOUT,
        client
            .post(license_url.clone())
            .headers(headers.clone())
            .header(reqwest::header::CONTENT_TYPE, content_type.clone())
            .body(challenge)
            .send(),
    )
    .await
    .map_err(|_| "The Widevine license request timed out".to_string())?
    .map_err(|error| {
        format!(
            "Could not contact the configured Widevine license server: {}",
            error.without_url()
        )
    })?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "The configured Widevine license server returned HTTP {}",
            status.as_u16()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > u64::try_from(MAX_LICENSE_RESPONSE_BYTES).unwrap_or(u64::MAX))
    {
        return Err("The Widevine license response exceeds the size limit".to_string());
    }
    use futures_util::StreamExt as _;
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            format!(
                "Could not read the Widevine license response: {}",
                error.without_url()
            )
        })?;
        let next_len = body
            .len()
            .checked_add(chunk.len())
            .filter(|length| *length <= MAX_LICENSE_RESPONSE_BYTES)
            .ok_or_else(|| "The Widevine license response exceeds the size limit".to_string())?;
        body.reserve(next_len.saturating_sub(body.len()));
        body.extend_from_slice(&chunk);
    }
    if body.is_empty() {
        return Err("The configured Widevine license server returned an empty response".to_string());
    }
    Ok(body)
}

async fn parse_widevine_license_response(
    request: widevine::CdmLicenseRequest,
    response: Vec<u8>,
    required_kids: BTreeSet<[u8; 16]>,
) -> Result<BTreeMap<[u8; 16], [u8; 16]>, String> {
    tokio::task::spawn_blocking(move || {
        let key_set = request
            .get_keys(&response)
            .map_err(|_| "The Widevine CDM could not parse the license response".to_string())?;
        let mut acquired = BTreeMap::new();
        for key in key_set.of_type(widevine::KeyType::CONTENT) {
            if !required_kids.contains(&key.kid) {
                continue;
            }
            if key.key.len() != 16 {
                return Err("The Widevine CDM returned a content key with an invalid length".to_string());
            }
            let mut content_key = [0_u8; 16];
            content_key.copy_from_slice(&key.key);
            acquired.insert(key.kid, content_key);
        }
        Ok(acquired)
    })
    .await
    .map_err(|error| format!("Widevine license parsing task failed: {error}"))?
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

pub(crate) use mp4::decrypt_track;

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
    fn clear_content_without_protection_remains_unencrypted() {
        assert_eq!(encryption_from_scopes(&[&[]]).expect("no protection"), None);
    }
}
