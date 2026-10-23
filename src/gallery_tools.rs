//! gallery-dl tool provisioning: download, verify, update, resolve.
//!
//! Uses the latest stable release published by the official Codeberg repository.
//! The Linux executable is verified against the release's SHA256SUMS attachment;
//! installation fails closed if the release or checksum cannot be verified.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use crate::video_tools::{download_to_file, ensure_lib_dir, user_lib_dir};

/// Maximum download size for the stable gallery-dl Linux executable (128 MiB).
const GALLERYDL_MAX_DOWNLOAD_BYTES: u64 = 128 * 1024 * 1024;
const CODEBERG_RELEASE_API: &str =
    "https://codeberg.org/api/v1/repos/mikf/gallery-dl/releases/latest";
const CODEBERG_RELEASES_BASE: &str = "https://codeberg.org/mikf/gallery-dl/releases/download";
/// Asset names used by stable Codeberg releases.
const LINUX_ASSET: &str = "gallery-dl.bin";
const CHECKSUMS_ASSET: &str = "SHA256SUMS";
const RELEASE_METADATA_MAX_BYTES: usize = 1024 * 1024;
const CHECKSUMS_MAX_BYTES: usize = 256 * 1024;

/// Errors from gallery-dl tool provisioning.
#[derive(Debug, Clone, thiserror::Error)]
pub enum GalleryError {
    /// Install/update failed (network, verification, filesystem).
    #[error("{0}")]
    Install(String),
    /// Background task machinery failed (panic/join) — internal.
    #[error("{0}")]
    Runtime(String),
    /// Probe failed (timeout, spawn failure, parse error, gallery-dl error).
    #[error("{0}")]
    Probe(String),
}

impl GalleryError {
    pub(crate) fn install(e: impl std::fmt::Display) -> Self {
        Self::Install(e.to_string())
    }
    pub(crate) fn runtime(e: impl std::fmt::Display) -> Self {
        Self::Runtime(e.to_string())
    }
    pub(crate) fn probe(e: impl std::fmt::Display) -> Self {
        Self::Probe(e.to_string())
    }
}

/// Latest stable gallery-dl release as `(tag, SHA-256 digest of its asset)`.
/// Codeberg release metadata and its SHA256SUMS attachment are checked on Tokio.
pub async fn latest_gallerydl_release() -> Option<(String, Option<String>)> {
    latest_gallerydl_asset()
        .await
        .map(|(_, digest, tag)| (tag, digest))
}

/// SHA-256 of a file as lowercase hex, read in chunks.
pub(crate) fn sha256_file(path: &Path) -> std::io::Result<String> {
    use sha2::Digest as _;
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// The installed gallery-dl as the update check reads it: `None` when the
/// resolved binary is not Grab's own copy (a system or PATH install is not
/// Grab's to update), `Some(None)` when it is Grab's but unreadable, and
/// `Some(Some(sha))` otherwise. Blocking: call off the GTK thread.
pub fn managed_gallerydl_sha() -> Option<Option<String>> {
    let path = resolve_gallerydl().ok()?;
    if !path.starts_with(user_lib_dir()) {
        return None;
    }
    Some(sha256_file(&path).ok())
}

/// Whether Grab's own gallery-dl differs from the latest upstream build. It
/// compares the installed file's SHA-256 with the upstream asset digest, the
/// same value the installer verifies, so the answer follows the binary on
/// disk. A binary Grab does not manage, or a missing upstream digest, never
/// prompts.
pub fn gallerydl_needs_update(managed: Option<Option<&str>>, latest_digest: Option<&str>) -> bool {
    match (managed, latest_digest) {
        (Some(Some(installed)), Some(latest)) => !installed.eq_ignore_ascii_case(latest),
        // Grab's but unreadable: a reinstall is the fix.
        (Some(None), Some(_)) => true,
        _ => false,
    }
}

/// Latest stable asset: `(download_url, sha256_digest, tag)`.
/// Any API failure, draft/prerelease, unexpected asset URL, or missing checksum
/// makes the release unavailable rather than weakening verification.
fn valid_release_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.bytes().all(|b| {
            matches!(
                b,
                b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'.' | b'-' | b'+' | b'_'
            )
        })
}

fn valid_sha256_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn codeberg_asset_url(tag: &str, asset_name: &str) -> Option<String> {
    valid_release_tag(tag).then(|| format!("{CODEBERG_RELEASES_BASE}/{tag}/{asset_name}"))
}

/// Accept only the expected assets in a stable release from the official project.
/// Do not download arbitrary URLs returned by release metadata.
fn parse_codeberg_release_metadata(body: &str) -> Option<(String, String, String)> {
    let release: serde_json::Value = serde_json::from_str(body).ok()?;
    if release.get("draft")?.as_bool()? || release.get("prerelease")?.as_bool()? {
        return None;
    }
    let tag = release.get("tag_name")?.as_str()?.to_owned();
    let expected_binary_url = codeberg_asset_url(&tag, LINUX_ASSET)?;
    let expected_checksums_url = codeberg_asset_url(&tag, CHECKSUMS_ASSET)?;
    let assets = release.get("assets")?.as_array()?;

    let asset_url = |name: &str, expected: &str| -> Option<String> {
        let asset = assets
            .iter()
            .find(|asset| asset.get("name").and_then(serde_json::Value::as_str) == Some(name))?;
        let url = asset.get("browser_download_url")?.as_str()?;
        (url == expected).then(|| url.to_owned())
    };

    let binary_url = asset_url(LINUX_ASSET, &expected_binary_url)?;
    let checksums_url = asset_url(CHECKSUMS_ASSET, &expected_checksums_url)?;
    Some((tag, binary_url, checksums_url))
}

/// Parse standard sha256sum output, requiring one unambiguous entry for the
/// named asset. The file is served alongside the binary in the same release.
fn parse_sha256sums(body: &str, filename: &str) -> Option<String> {
    let mut found: Option<String> = None;
    for line in body.lines() {
        let mut fields = line.split_whitespace();
        let Some(digest) = fields.next() else {
            continue;
        };
        let Some(name) = fields.next() else {
            continue;
        };
        if name.strip_prefix('*').unwrap_or(name) != filename {
            continue;
        }
        if fields.next().is_some() || !valid_sha256_digest(digest) || found.is_some() {
            return None;
        }
        found = Some(digest.to_ascii_lowercase());
    }
    found
}

async fn bounded_response_text(
    response: reqwest::Response,
    max_bytes: usize,
) -> Option<String> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return None;
    }

    use futures_util::StreamExt as _;
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        if body.len().saturating_add(chunk.len()) > max_bytes {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).ok()
}

async fn latest_gallerydl_asset() -> Option<(String, Option<String>, String)> {
    let handle = crate::runtime::tokio_rt().spawn(async move {
        let client = crate::download_net::tool_client_builder_proxied()
            .ok()?
            .user_agent(concat!("Grab/", env!("CARGO_PKG_VERSION")))
            .build()
            .ok()?;
        let response = client
            .get(CODEBERG_RELEASE_API)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?;
        let release_body = bounded_response_text(response, RELEASE_METADATA_MAX_BYTES).await?;
        let (tag, download_url, checksums_url) = parse_codeberg_release_metadata(&release_body)?;

        let response = client
            .get(&checksums_url)
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?;
        let checksums = bounded_response_text(response, CHECKSUMS_MAX_BYTES).await?;
        let digest = parse_sha256sums(&checksums, LINUX_ASSET)?;
        Some((download_url, Some(digest), tag))
    });
    handle.await.ok().flatten()
}

/// Install (or update) gallery-dl from the latest stable Codeberg release.
/// Fails closed when its published SHA256SUMS file lacks a valid binary digest.
pub async fn install_gallerydl() -> Result<PathBuf, GalleryError> {
    let (url, digest, _) = latest_gallerydl_asset()
        .await
        .ok_or_else(|| GalleryError::install("couldn't determine the latest gallery-dl release"))?;
    // Fail closed: no digest means no verified install. The digest comes from
    // the SHA256SUMS attachment published with the same Codeberg release.
    let digest = digest.ok_or_else(|| {
        GalleryError::install(
            "gallery-dl release has no SHA-256 digest; refusing unverified install",
        )
    })?;
    let dir = user_lib_dir();
    ensure_lib_dir(&dir).map_err(GalleryError::install)?;
    // Download straight to `gallery-dl.part`: a failed download must never leave a
    // half-written `gallery-dl` behind.
    let dest = dir.join("gallery-dl");
    let part = dir.join("gallery-dl.part");
    let result = fetch_gallerydl_inner(&url, &part, &dest, &digest).await;
    if result.is_err() {
        std::fs::remove_file(&part).ok();
    }
    result.map(|()| dest)
}

async fn fetch_gallerydl_inner(
    url: &str,
    part: &Path,
    dest: &Path,
    digest: &str,
) -> Result<(), GalleryError> {
    download_to_file(url, part, digest, GALLERYDL_MAX_DOWNLOAD_BYTES)
        .await
        .map_err(GalleryError::install)?;
    tokio::fs::set_permissions(part, std::fs::Permissions::from_mode(0o755))
        .await
        .map_err(|e| {
            GalleryError::install(format!("couldn't mark {} executable: {e}", part.display()))
        })?;
    tokio::fs::rename(part, dest)
        .await
        .map_err(|e| GalleryError::install(format!("couldn't install {}: {e}", dest.display())))
}

/// Resolve the gallery-dl binary via the standard tool search dirs
/// (user lib dir → /app/bin → PATH), requiring the executable bit.
pub fn resolve_gallerydl() -> Result<PathBuf, GalleryError> {
    crate::video_tools::find_in_dirs("gallery-dl", &crate::video_tools::tool_search_dirs())
        .ok_or_else(|| {
            GalleryError::install(
                "gallery-dl not found: install it via Preferences → Check for Updates",
            )
        })
}

/// `gallery-dl --version` output, trimmed. Used for the Preferences row.
pub async fn gallerydl_display_version(path: &Path) -> Result<String, GalleryError> {
    // kill_on_drop: the Preferences row that calls this can be closed
    // mid-probe; the child must not outlive the dropped future.
    let mut cmd = tokio::process::Command::new(path);
    cmd.kill_on_drop(true);
    let out = cmd
        .arg("--version")
        .output()
        .await
        .map_err(|e| GalleryError::install(format!("couldn't run gallery-dl: {e}")))?;
    let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if version.is_empty() {
        return Err(GalleryError::install(
            "gallery-dl --version produced no output",
        ));
    }
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codeberg_stable_release_metadata_accepts_only_expected_assets() {
        let body = r#"{
            "tag_name":"v1.32.17",
            "draft":false,
            "prerelease":false,
            "assets":[
                {
                    "name":"gallery-dl.bin",
                    "browser_download_url":"https://codeberg.org/mikf/gallery-dl/releases/download/v1.32.17/gallery-dl.bin"
                },
                {
                    "name":"SHA256SUMS",
                    "browser_download_url":"https://codeberg.org/mikf/gallery-dl/releases/download/v1.32.17/SHA256SUMS"
                }
            ]
        }"#;
        let release = parse_codeberg_release_metadata(body).unwrap();
        assert_eq!(release.0, "v1.32.17");
        assert_eq!(
            release.1,
            "https://codeberg.org/mikf/gallery-dl/releases/download/v1.32.17/gallery-dl.bin"
        );
        assert_eq!(
            release.2,
            "https://codeberg.org/mikf/gallery-dl/releases/download/v1.32.17/SHA256SUMS"
        );
    }

    #[test]
    fn codeberg_release_parser_rejects_prereleases_and_untrusted_asset_urls() {
        let valid = r#"{
            "tag_name":"v1.32.17",
            "draft":false,
            "prerelease":false,
            "assets":[
                {"name":"gallery-dl.bin","browser_download_url":"https://codeberg.org/mikf/gallery-dl/releases/download/v1.32.17/gallery-dl.bin"},
                {"name":"SHA256SUMS","browser_download_url":"https://codeberg.org/mikf/gallery-dl/releases/download/v1.32.17/SHA256SUMS"}
            ]
        }"#;
        assert!(
            parse_codeberg_release_metadata(
                &valid.replace("\"prerelease\":false", "\"prerelease\":true")
            )
            .is_none()
        );
        assert!(
            parse_codeberg_release_metadata(&valid.replace(
                "https://codeberg.org/mikf/gallery-dl/releases/download/v1.32.17/gallery-dl.bin",
                "https://attacker.invalid/gallery-dl.bin"
            ))
            .is_none()
        );
        assert!(codeberg_asset_url("v1.32.17/../../evil", LINUX_ASSET).is_none());
    }

    #[test]
    fn sha256sums_requires_one_valid_entry_for_the_linux_asset() {
        let digest = "a".repeat(64);
        let sums = format!("{digest}  other.bin\n{digest} *gallery-dl.bin\n");
        assert_eq!(parse_sha256sums(&sums, LINUX_ASSET), Some(digest.clone()));
        let invalid = format!(
            "{digest}  gallery-dl.bin\n{}  gallery-dl.bin\n",
            "b".repeat(64)
        );
        assert!(parse_sha256sums(&invalid, LINUX_ASSET).is_none());
        let trailing_fields = format!("{digest}  gallery-dl.bin trailing-data\n");
        assert!(parse_sha256sums(&trailing_fields, LINUX_ASSET).is_none());
        assert!(
            parse_sha256sums(
                &format!("{}  gallery-dl.bin\n", "not-a-digest"),
                LINUX_ASSET
            )
            .is_none()
        );
    }

    #[test]
    fn gallerydl_needs_update_follows_the_binary_not_a_sidecar() {
        // Grab's binary matches upstream: current.
        assert!(!gallerydl_needs_update(Some(Some("abc")), Some("abc")));
        // Different bytes: update available.
        assert!(gallerydl_needs_update(Some(Some("abc")), Some("def")));
        // Grab's but unreadable: reinstall.
        assert!(gallerydl_needs_update(Some(None), Some("def")));
        // Not Grab's (system or PATH install): never prompts.
        assert!(!gallerydl_needs_update(None, Some("def")));
        // No upstream digest to compare against: never prompts.
        assert!(!gallerydl_needs_update(Some(Some("abc")), None));
    }

    #[test]
    fn sha256_file_matches_the_known_digest() {
        let path = std::env::temp_dir().join("grab-sha-test");
        std::fs::write(&path, b"abc").unwrap();
        let got = sha256_file(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            got.unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn error_messages_follow_convention() {
        // gtk-rs skill: error strings lowercase, no trailing punctuation.
        // Assert on the real messages, not a placeholder.
        let messages = [
            GalleryError::install("couldn't determine the latest gallery-dl release").to_string(),
            GalleryError::install(
                "gallery-dl release has no SHA-256 digest; refusing unverified install",
            )
            .to_string(),
            GalleryError::install(
                "gallery-dl not found: install it via Preferences → Check for Updates",
            )
            .to_string(),
            GalleryError::install("gallery-dl --version produced no output").to_string(),
        ];
        for msg in messages {
            let first = msg.chars().next().unwrap();
            assert!(
                first.is_lowercase() || first == '"',
                "must start lowercase: {msg}"
            );
            assert!(
                !msg.ends_with(['.', '!', '?']),
                "no trailing punctuation: {msg}"
            );
        }
    }

    /// Regression: gallerydl_display_version must work from a thread
    /// without a Tokio runtime.
    #[test]
    fn gallerydl_display_version_works_without_tokio_runtime() {
        let result = std::thread::spawn(|| {
            assert!(tokio::runtime::Handle::try_current().is_err());
            crate::runtime::tokio_rt().block_on(gallerydl_display_version(std::path::Path::new(
                "/nonexistent/gallery-dl",
            )))
        })
        .join()
        .expect("thread panicked");
        assert!(result.is_err());
    }
}
