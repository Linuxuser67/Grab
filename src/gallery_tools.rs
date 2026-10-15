//! gallery-dl tool provisioning: download, verify, update, resolve.
//!
//! Mirrors the yt-dlp installer pattern in `video_tools.rs`: nightly channel,
//! SHA-256 verified against release metadata, fail-closed without a digest.
//! The nightly builds live in the separate `gdl-org/builds` repo
//! (the Codeberg repo is the source; GitHub hosts the built binaries).

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use crate::video_tools::{download_to_file, ensure_lib_dir, user_lib_dir};

/// Minimum gallery-dl version Grab supports. `gallery-dl --version` prints
/// semver-ish `1.32.14` (unlike yt-dlp's date-based `2026.08.19`), so the
/// parser below handles that shape.
pub const MIN_GALLERYDL_VERSION: [u32; 3] = [1, 27, 0];

/// Max bytes for the gallery-dl binary download (nightly is ~47MB; cap at 128MB).
const GALLERYDL_MAX_DOWNLOAD_BYTES: u64 = 128 * 1024 * 1024;

/// Nightly builds repo (owner, repo) for the standalone binaries.
const BUILDS_OWNER: &str = "gdl-org";
const BUILDS_REPO: &str = "builds";
/// Asset name of the Linux standalone binary in the nightly releases.
const LINUX_ASSET: &str = "gallery-dl_linux";

/// Errors from gallery-dl tool provisioning.
#[derive(Debug, thiserror::Error)]
pub enum GalleryError {
    /// Install/update failed (network, verification, filesystem).
    #[error("{0}")]
    Install(String),
    /// Background task machinery failed (panic/join) — internal.
    #[error("{0}")]
    Runtime(String),
}

impl GalleryError {
    pub(crate) fn install(e: impl std::fmt::Display) -> Self {
        Self::Install(e.to_string())
    }
    pub(crate) fn runtime(e: impl std::fmt::Display) -> Self {
        Self::Runtime(e.to_string())
    }
}

/// Parse `gallery-dl --version` output (`1.32.14`) into `[major, minor, patch]`.
/// Returns `None` on unparseable output — the caller treats it as "too old".
pub fn parse_gallerydl_version(output: &str) -> Option<[u32; 3]> {
    let first_line = output.lines().next()?.trim();
    // Some builds print "1.32.14" bare; others prefix ("gallery-dl 1.32.14").
    let version_str = first_line
        .strip_prefix("gallery-dl")
        .map(str::trim)
        .unwrap_or(first_line);
    let mut parts = version_str.split('.');
    let major = parts.next()?.parse::<u32>().ok()?;
    let minor = parts.next()?.parse::<u32>().ok()?;
    // Patch may carry a suffix ("14-dev"); take leading digits.
    let patch_str: String = parts
        .next()
        .unwrap_or("0")
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let patch = if patch_str.is_empty() {
        0
    } else {
        patch_str.parse::<u32>().ok()?
    };
    Some([major, minor, patch])
}

/// Whether `version` meets the minimum floor.
pub fn version_supported(version: [u32; 3]) -> bool {
    version >= MIN_GALLERYDL_VERSION
}

/// Latest nightly asset: `(download_url, sha256_digest)`.
/// `None` on any network/API failure; digest is `None` when the release
/// metadata lacks one (the installer then fails closed).
pub async fn latest_gallerydl_asset() -> Option<(String, Option<String>)> {
    let handle = crate::runtime::tokio_rt().spawn(async move {
        let fetcher = yt_dlp::client::deps::github::GitHubFetcher::new(BUILDS_OWNER, BUILDS_REPO);
        let release = fetcher.fetch_latest_release(None).await.ok()?;
        let asset = release.assets.iter().find(|a| a.name == LINUX_ASSET)?;
        let digest = asset
            .digest
            .as_deref()
            .and_then(|d| d.strip_prefix("sha256:"))
            .map(str::to_owned);
        Some((asset.download_url.clone(), digest))
    });
    handle.await.ok().flatten()
}

/// Latest nightly tag without downloading anything: one user-initiated
/// GitHub API call. `None` on any network/API failure.
pub async fn latest_gallerydl_tag() -> Option<String> {
    let handle = crate::runtime::tokio_rt().spawn(async move {
        let fetcher = yt_dlp::client::deps::github::GitHubFetcher::new(BUILDS_OWNER, BUILDS_REPO);
        fetcher
            .fetch_latest_release(None)
            .await
            .ok()
            .map(|release| release.tag_name)
    });
    handle.await.ok().flatten()
}

/// Install (or update) gallery-dl from the nightly channel into the user lib dir.
/// Fails closed when the release metadata carries no SHA-256 digest.
pub async fn install_gallerydl() -> Result<PathBuf, GalleryError> {
    let (url, digest) = latest_gallerydl_asset()
        .await
        .ok_or_else(|| GalleryError::install("couldn't determine the latest gallery-dl release"))?;
    // Fail closed: no digest means no verified install. The digest comes from
    // GitHub's API alongside the binary; absent means we can't verify.
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
    let dest_clone = dest.clone();
    let handle = crate::runtime::tokio_rt().spawn(async move {
        let result = fetch_gallerydl_inner(&url, &part, &dest_clone, &digest).await;
        if result.is_err() {
            std::fs::remove_file(&part).ok();
        }
        result
    });
    match handle.await {
        Ok(Ok(())) => Ok(dest),
        Ok(Err(e)) => Err(e),
        Err(e) => Err(GalleryError::runtime(&e)),
    }
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
    let out = tokio::process::Command::new(path)
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
    fn parse_version_bare() {
        assert_eq!(parse_gallerydl_version("1.32.14"), Some([1, 32, 14]));
    }

    #[test]
    fn parse_version_prefixed() {
        assert_eq!(
            parse_gallerydl_version("gallery-dl 1.32.14"),
            Some([1, 32, 14])
        );
    }

    #[test]
    fn parse_version_dev_suffix() {
        // Nightly builds may print "1.33.0-dev".
        assert_eq!(parse_gallerydl_version("1.33.0-dev"), Some([1, 33, 0]));
    }

    #[test]
    fn parse_version_garbage() {
        assert_eq!(parse_gallerydl_version(""), None);
        assert_eq!(parse_gallerydl_version("not a version"), None);
        assert_eq!(parse_gallerydl_version("1.x"), None);
    }

    #[test]
    fn version_floor() {
        assert!(version_supported([1, 27, 0]));
        assert!(version_supported([1, 32, 14]));
        assert!(version_supported([2, 0, 0]));
        assert!(!version_supported([1, 26, 9]));
        assert!(!version_supported([0, 9, 0]));
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
}
