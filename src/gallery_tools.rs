//! gallery-dl tool provisioning: download, verify, update, resolve.
//!
//! Mirrors the yt-dlp installer pattern in `video_tools.rs`: nightly channel,
//! SHA-256 verified against release metadata, fail-closed without a digest.
//! The nightly builds live in the separate `gdl-org/builds` repo
//! (the Codeberg repo is the source; GitHub hosts the built binaries).

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use crate::video_tools::{download_to_file, ensure_lib_dir, user_lib_dir};

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

/// Whether a gallery-dl update is available. The nightly tag is date-based
/// (`2026.10.15`) while `gallery-dl --version` prints semver (`1.32.14`),
/// Whether a gallery-dl update is available.
/// Compares the latest nightly tag against the sidecar written at install time.
/// Missing sidecar means "update available" (once).
pub fn gallerydl_update_available(tag: &str) -> bool {
    let tag = tag.trim();
    if tag.is_empty() {
        return false;
    }
    let sidecar = user_lib_dir().join("gallery-dl.tag");
    let stored = std::fs::read_to_string(&sidecar).ok();
    update_available_for_tags(stored.as_deref(), tag)
}

/// Pure helper: is an update available given the stored vs latest tags?
fn update_available_for_tags(stored: Option<&str>, latest: &str) -> bool {
    let latest = latest.trim();
    if latest.is_empty() {
        return false;
    }
    match stored {
        Some(s) => s.trim() != latest,
        None => true,
    }
}

/// Latest nightly asset: `(download_url, sha256_digest, tag)`.
/// `None` on any network/API failure; digest is `None` when the release
/// metadata lacks one (the installer then fails closed).
async fn latest_gallerydl_asset() -> Option<(String, Option<String>, String)> {
    let handle = crate::runtime::tokio_rt().spawn(async move {
        let fetcher = yt_dlp::client::deps::github::GitHubFetcher::new(BUILDS_OWNER, BUILDS_REPO);
        let release = fetcher.fetch_latest_release(None).await.ok()?;
        let asset = release.assets.iter().find(|a| a.name == LINUX_ASSET)?;
        let digest = asset
            .digest
            .as_deref()
            .and_then(|d| d.strip_prefix("sha256:"))
            .map(str::to_owned);
        Some((asset.download_url.clone(), digest, release.tag_name.clone()))
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
    let (url, digest, tag) = latest_gallerydl_asset()
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
        Ok(Ok(())) => {
            // Write the installed tag to the sidecar for the update check.
            let tag_file = dir.join("gallery-dl.tag");
            std::fs::write(&tag_file, &tag).ok();
            Ok(dest)
        }
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
/// Runs on the Tokio runtime: callers may be on the GTK main loop.
pub async fn gallerydl_display_version(path: &Path) -> Result<String, GalleryError> {
    let path = path.to_path_buf();
    crate::runtime::tokio_rt()
        .spawn(async move {
            let out = tokio::process::Command::new(&path)
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
        })
        .await
        .unwrap_or_else(|e| Err(GalleryError::runtime(&e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_available_on_nightly() {
        // Same tag: no update.
        assert!(!update_available_for_tags(Some("2026.10.15"), "2026.10.15"));
        // Different tag: update available.
        assert!(update_available_for_tags(Some("2026.10.14"), "2026.10.15"));
        // Missing sidecar: update available (once).
        assert!(update_available_for_tags(None, "2026.10.15"));
        // Empty latest tag: no update.
        assert!(!update_available_for_tags(Some("2026.10.15"), ""));
        assert!(!update_available_for_tags(None, ""));
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
}
