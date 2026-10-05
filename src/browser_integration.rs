//! Browser native-messaging host installation.
//!
//! Registers the `grab-native-host` binary with Chromium- and Firefox-based
//! browsers so the Grab browser extension can hand downloads to the desktop
//! app. Uses the `native_messaging` crate's installer for the browsers it
//! knows; a thin supplement covers the Brave channels it misses (Beta, Dev,
//! Nightly, Origin Beta) plus Opera.
//!
//! In a Flatpak sandbox the browser config dirs are not writable; in that
//! case [`install`] refuses and prints the host-side commands instead.

use std::fs;
use std::path::{Path, PathBuf};

pub const HOST_NAME: &str = "io.github.linuxuser67.grab";
pub const FIREFOX_ADDON_ID: &str = "grab@linuxuser67.github.io";
const DESCRIPTION: &str = "Grab download manager native host";

/// Extra Chromium config dirs the `native_messaging` crate doesn't cover.
/// The crate's `brave` key handles standard Brave; these are the other
/// channels (Beta, Dev, Nightly, Origin Beta) plus Opera.
const EXTRA_CHROMIUM_DIRS: &[&str] = &[
    ".config/BraveSoftware/Brave-Browser-Beta",
    ".config/BraveSoftware/Brave-Browser-Dev",
    ".config/BraveSoftware/Brave-Browser-Nightly",
    ".config/BraveSoftware/Brave-Origin-Beta",
    ".config/opera",
];

/// Browser keys for `native_messaging::install` on Linux.
const CRATE_BROWSERS: &[&str] = &["chrome", "chromium", "brave", "vivaldi", "edge", "firefox"];

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn in_flatpak() -> bool {
    Path::new("/.flatpak-info").exists()
}

/// Locate the `grab-native-host` binary next to the running executable.
fn find_host_binary() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let candidate = dir.join("grab-native-host");
    if candidate.is_file() {
        return Some(candidate);
    }
    None
}

/// Install the host binary to `~/.local/share/grab-native-host/`.
fn install_binary() -> Result<(PathBuf, bool), String> {
    let install_dir = home_dir()
        .map(|h| h.join(".local/share/grab-native-host"))
        .ok_or_else(|| "HOME is not set".to_string())?;
    fs::create_dir_all(&install_dir).map_err(|e| format!("create {install_dir:?}: {e}"))?;

    let src = find_host_binary().ok_or_else(|| {
        "grab-native-host binary not found next to the Grab executable".to_string()
    })?;
    let dst = install_dir.join("grab-native-host");
    // Skip when the installed copy is already identical and executable
    // (avoids churn; a byte mismatch detects a stale binary after upgrades).
    // Compare lengths first: only read when they match.
    let fresh = {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt as _;
        let src_len = fs::metadata(&src).ok().map(|m| m.len());
        let dst_meta = fs::metadata(&dst).ok();
        let len_match = src_len.is_some_and(|l| dst_meta.as_ref().is_some_and(|m| m.len() == l));
        #[cfg(unix)]
        let exec_ok = dst_meta
            .as_ref()
            .is_some_and(|m| m.permissions().mode() & 0o111 != 0);
        #[cfg(not(unix))]
        let exec_ok = true;
        len_match
            && exec_ok
            && fs::read(&src)
                .ok()
                .zip(fs::read(&dst).ok())
                .is_some_and(|(a, b)| a == b)
    };
    if !fresh {
        // Atomic replace: copy to a temp file, chmod, then rename over the
        // destination. rename(2) is safe while the old binary is executing
        // (the running image keeps its inode); a direct copy would hit
        // ETXTBSY.
        let tmp = install_dir.join(format!(".grab-native-host.{}", std::process::id()));
        let install = || -> Result<(), String> {
            fs::copy(&src, &tmp).map_err(|e| format!("copy host binary: {e}"))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let mut perms = fs::metadata(&tmp).map_err(|e| e.to_string())?.permissions();
                perms.set_mode(0o755);
                fs::set_permissions(&tmp, perms).map_err(|e| e.to_string())?;
            }
            fs::rename(&tmp, &dst).map_err(|e| format!("install host binary: {e}"))?;
            Ok(())
        };
        if let Err(e) = install() {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
    }
    Ok((dst, !fresh))
}

/// Manifest JSON for the extra Chromium channels (same shape the crate writes).
fn extra_chromium_manifest(host_path: &str, extension_ids: &[String]) -> serde_json::Value {
    serde_json::json!({
        "name": HOST_NAME,
        "description": DESCRIPTION,
        "path": host_path,
        "type": "stdio",
        "allowed_origins": extension_ids
            .iter()
            .map(|id| format!("chrome-extension://{id}/"))
            .collect::<Vec<_>>(),
    })
}

/// Whether the Chromium manifest matches the expected structure exactly.
fn chromium_manifest_matches(
    manifest: &serde_json::Value,
    host_path: &str,
    ids: &[String],
) -> bool {
    let expected_origins: Vec<serde_json::Value> = ids
        .iter()
        .map(|id| serde_json::Value::String(format!("chrome-extension://{id}/")))
        .collect();
    manifest.get("name").and_then(|v| v.as_str()) == Some(HOST_NAME)
        && manifest.get("description").and_then(|v| v.as_str()) == Some(DESCRIPTION)
        && manifest.get("path").and_then(|v| v.as_str()) == Some(host_path)
        && manifest.get("type").and_then(|v| v.as_str()) == Some("stdio")
        && manifest.get("allowed_origins").and_then(|v| v.as_array()) == Some(&expected_origins)
}

/// Whether the Firefox manifest matches the expected structure exactly.
/// Validates name, description, path, type, and allowed_extensions — not just
/// the path, so a stale/corrupted manifest is detected and replaced.
fn firefox_manifest_matches(manifest: &serde_json::Value, host_path: &str) -> bool {
    manifest.get("name").and_then(|v| v.as_str()) == Some(HOST_NAME)
        && manifest.get("description").and_then(|v| v.as_str()) == Some(DESCRIPTION)
        && manifest.get("path").and_then(|v| v.as_str()) == Some(host_path)
        && manifest.get("type").and_then(|v| v.as_str()) == Some("stdio")
        && manifest
            .get("allowed_extensions")
            .and_then(|v| v.as_array())
            == Some(&vec![serde_json::Value::String(
                FIREFOX_ADDON_ID.to_string(),
            )])
}

pub fn ensure_firefox_host() -> bool {
    // Ensure the Firefox native host is installed, silently fixing it if
    // missing or stale. The Firefox add-on ID is fixed, so this needs no user
    // input and runs on every startup. Returns true if (re)installed.
    if in_flatpak() {
        return false;
    }
    let home = match home_dir() {
        Some(h) => h,
        None => return false,
    };
    let manifest_path = home
        .join(".mozilla/native-messaging-hosts")
        .join(format!("{HOST_NAME}.json"));

    // Install/refresh the binary first: the manifest check below can't
    // detect a stale binary after an upgrade.
    let (host_bin, replaced) = match install_binary() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("browser host install: {e}");
            return false;
        }
    };
    let host_path = host_bin.to_string_lossy().into_owned();

    // Manifest already correct? Nothing more to do.
    if let Ok(text) = fs::read_to_string(&manifest_path)
        && let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&text)
        && firefox_manifest_matches(&manifest, &host_path)
    {
        return replaced;
    }

    // (Re)install the manifest (atomic: write temp + rename).
    let manifest = serde_json::json!({
        "name": HOST_NAME,
        "description": DESCRIPTION,
        "path": host_path,
        "type": "stdio",
        "allowed_extensions": [FIREFOX_ADDON_ID],
    });
    let mut text = serde_json::to_string_pretty(&manifest).unwrap_or_default();
    text.push('\n');
    if let Some(parent) = manifest_path.parent()
        && let Err(e) = fs::create_dir_all(parent)
    {
        tracing::warn!("firefox host manifest: cannot create dir: {e}");
        return false;
    }
    // Atomic write via uniquely-named temp file (create_new prevents symlink attacks).
    match crate::file_names::atomic_replace_file(&manifest_path, text.as_bytes()) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("firefox host manifest: write failed: {e}");
            false
        }
    }
}

/// Ensure Chromium manifests for previously-registered extension IDs.
/// Called on startup; the IDs were stored by `--install-browser-host`.
pub fn ensure_chromium_hosts(ids: &[String]) -> bool {
    if in_flatpak() || ids.is_empty() {
        return false;
    }
    let home = match home_dir() {
        Some(h) => h,
        None => return false,
    };
    let (host_bin, _) = match install_binary() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("browser host install: {e}");
            return false;
        }
    };
    let host_path = host_bin.to_string_lossy().into_owned();
    let mut changed = false;

    // Crate-covered browsers. Note: this also rewrites the Firefox manifest
    // that ensure_firefox_host just handled; the crate offers no
    // chromium-only entry point, so we accept the redundant write.
    let chromium_origins: Vec<String> = ids
        .iter()
        .map(|id| format!("chrome-extension://{id}/"))
        .collect();
    let firefox_ids = vec![FIREFOX_ADDON_ID.to_string()];
    if let Err(e) = native_messaging::install(
        HOST_NAME,
        DESCRIPTION,
        Path::new(&host_path),
        &chromium_origins,
        &firefox_ids,
        CRATE_BROWSERS,
        native_messaging::Scope::User,
    ) {
        tracing::warn!("browser host install via crate failed: {e}");
    }

    // Extra channels the crate misses.
    for rel in EXTRA_CHROMIUM_DIRS {
        let cfg = home.join(rel);
        if !cfg.is_dir() {
            continue;
        }
        let target = cfg
            .join("NativeMessagingHosts")
            .join(format!("{HOST_NAME}.json"));
        // Skip if already correct (full structure, not just origins).
        if let Ok(text) = fs::read_to_string(&target)
            && let Ok(m) = serde_json::from_str::<serde_json::Value>(&text)
            && chromium_manifest_matches(&m, &host_path, ids)
        {
            continue;
        }
        if let Some(parent) = target.parent()
            && let Err(e) = fs::create_dir_all(parent)
        {
            tracing::warn!("chromium host manifest: cannot create dir: {e}");
            continue;
        }
        let mut text = serde_json::to_string_pretty(&extra_chromium_manifest(&host_path, ids))
            .unwrap_or_default();
        text.push('\n');
        // Atomic write via uniquely-named temp file.
        match crate::file_names::atomic_replace_file(&target, text.as_bytes()) {
            Ok(()) => changed = true,
            Err(e) => {
                tracing::warn!("chromium host manifest: write failed: {e}");
            }
        }
    }
    changed
}

/// Install the native host binary and browser manifests.
///
/// `chromium_ids` are the extension IDs from `chrome://extensions` (unpacked
/// installs get a generated ID). The Firefox add-on ID is fixed, so its
/// manifest is always written.
pub fn install(chromium_ids: &[String]) -> Result<Vec<PathBuf>, String> {
    if in_flatpak() {
        return Err(flatpak_instructions().to_string());
    }
    let home = home_dir().ok_or_else(|| "HOME is not set".to_string())?;
    let (host_bin, _) = install_binary()?;
    let host_path = host_bin.to_string_lossy().into_owned();

    let chromium_origins: Vec<String> = chromium_ids
        .iter()
        .map(|id| format!("chrome-extension://{id}/"))
        .collect();
    let firefox_ids = vec![FIREFOX_ADDON_ID.to_string()];

    // Standard browsers via the crate.
    native_messaging::install(
        HOST_NAME,
        DESCRIPTION,
        Path::new(&host_path),
        &chromium_origins,
        &firefox_ids,
        CRATE_BROWSERS,
        native_messaging::Scope::User,
    )
    .map_err(|e| format!("crate installer: {e}"))?;

    let mut written: Vec<PathBuf> = CRATE_BROWSERS
        .iter()
        .filter_map(|b| {
            native_messaging::manifest_paths(b, native_messaging::Scope::User, HOST_NAME)
                .ok()
                .map(|paths| paths.into_iter())
        })
        .flatten()
        .filter(|p| p.exists())
        .collect();

    // Extra Brave channels + Opera the crate doesn't know.
    if !chromium_ids.is_empty() {
        for rel in EXTRA_CHROMIUM_DIRS {
            let cfg = home.join(rel);
            if !cfg.is_dir() {
                continue;
            }
            let target = cfg
                .join("NativeMessagingHosts")
                .join(format!("{HOST_NAME}.json"));
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|e| format!("create {parent:?}: {e}"))?;
            }
            let mut text =
                serde_json::to_string_pretty(&extra_chromium_manifest(&host_path, chromium_ids))
                    .unwrap_or_default();
            text.push('\n');
            crate::file_names::atomic_replace_file(&target, text.as_bytes())
                .map_err(|e| format!("write {target:?}: {e}"))?;
            written.push(target);
        }
    }

    Ok(written)
}

fn flatpak_instructions() -> &'static str {
    "Running in Flatpak: the sandbox cannot write browser config dirs.\n\
     On the host, run:\n  \
     flatpak run --command=grab io.github.linuxuser67.Grab --install-browser-host --chromium-id <id>\n\
     (get the extension ID from chrome://extensions with Developer mode on)"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_manifest_has_allowed_origins() {
        let m = extra_chromium_manifest("/home/u/host", &["abc123".to_string()]);
        assert_eq!(m["name"], HOST_NAME);
        assert_eq!(m["type"], "stdio");
        assert_eq!(
            m["allowed_origins"],
            serde_json::json!(["chrome-extension://abc123/"])
        );
    }
}
