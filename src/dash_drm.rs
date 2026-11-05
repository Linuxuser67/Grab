//! Bounded DASH Common Encryption support for explicitly supplied content keys.
//!
//! Grab keeps downloaded encrypted resources as its resumable source of truth.
//! Decryption produces a separate private track for FFmpeg; the destination part
//! and its digest journal therefore remain bound to the original ciphertext.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::Read;
use std::path::{Path, PathBuf};

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

/// Load the user's private KID-to-key file only when a selected representation
/// is encrypted. Keys are not persisted by Grab.
pub(crate) fn load_required_keys(
    encryptions: &[DashEncryption],
) -> Result<Option<DashKeyMap>, String> {
    let required: BTreeSet<_> = encryptions
        .iter()
        .map(|encryption| encryption.default_kid)
        .collect();
    if required.is_empty() {
        return Ok(None);
    }

    let path = key_file_path();
    let bytes = read_private_key_file(&path)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| format!("{} must contain UTF-8 KID:KEY entries", path.display()))?;
    let mut keys = BTreeMap::new();

    for (line_index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (kid_text, key_text) = line.split_once(':').ok_or_else(|| {
            format!(
                "{}:{} must use KID:KEY format",
                path.display(),
                line_index + 1
            )
        })?;
        let kid = parse_hex_16(kid_text).ok_or_else(|| {
            format!(
                "{}:{} contains an invalid 128-bit KID",
                path.display(),
                line_index + 1
            )
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

    for kid in required {
        if !keys.contains_key(&kid) {
            return Err(format!(
                "No content key is configured for selected DASH KID {}. Add a KID:KEY entry to {}.",
                hex_string(&kid),
                path.display()
            ));
        }
    }

    Ok(Some(DashKeyMap { keys }))
}

fn key_file_path() -> PathBuf {
    glib::user_config_dir().join("grab/drm-keys.txt")
}

fn read_private_key_file(path: &Path) -> Result<Vec<u8>, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!(
                "DASH decryption needs explicit content keys. Create {} with one KID:KEY pair per line and restrict it to mode 0600.",
                path.display()
            )
        } else {
            format!("Could not safely open {}: {error}", path.display())
        }
    })?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("Could not inspect {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        // SAFETY: geteuid has no preconditions and only reads the effective user ID.
        let effective_uid = unsafe { libc::geteuid() };
        if metadata.uid() != effective_uid || metadata.mode() & 0o077 != 0 {
            return Err(format!(
                "{} must be owned by the current user and have no group/other permissions (chmod 600)",
                path.display()
            ));
        }
    }
    if metadata.len() > u64::try_from(MAX_KEY_FILE_BYTES).unwrap_or(u64::MAX) {
        return Err(format!(
            "{} exceeds the {}-byte limit",
            path.display(),
            MAX_KEY_FILE_BYTES
        ));
    }
    let mut bytes =
        Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(MAX_KEY_FILE_BYTES));
    file.take(u64::try_from(MAX_KEY_FILE_BYTES + 1).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Could not read {}: {error}", path.display()))?;
    if bytes.len() > MAX_KEY_FILE_BYTES {
        return Err(format!(
            "{} exceeds the {}-byte limit",
            path.display(),
            MAX_KEY_FILE_BYTES
        ));
    }
    Ok(bytes)
}

fn parse_hex_16(value: &str) -> Option<[u8; 16]> {
    let compact: String = value
        .chars()
        .filter(|character| *character != '-')
        .collect();
    if compact.len() != 32 || !compact.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut output = [0_u8; 16];
    for (index, pair) in compact.as_bytes().chunks_exact(2).enumerate() {
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
