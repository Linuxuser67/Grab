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

/// Latest nightly gallery-dl release as `(tag, SHA-256 digest of its asset)`.
/// One GitHub lookup on the Tokio runtime; `None` on any network/API failure.
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

/// Install (or update) gallery-dl from the nightly channel into the user lib dir.
/// Fails closed when the release metadata carries no SHA-256 digest.
pub async fn install_gallerydl() -> Result<PathBuf, GalleryError> {
    let (url, digest, _) = latest_gallerydl_asset()
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
