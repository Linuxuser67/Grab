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
fn install_binary() -> Result<PathBuf, String> {
    let install_dir = home_dir()
        .map(|h| h.join(".local/share/grab-native-host"))
        .ok_or_else(|| "HOME is not set".to_string())?;
    fs::create_dir_all(&install_dir).map_err(|e| format!("create {install_dir:?}: {e}"))?;

    let src = find_host_binary().ok_or_else(|| {
        "grab-native-host binary not found next to the Grab executable".to_string()
    })?;
    let dst = install_dir.join("grab-native-host");
    fs::copy(&src, &dst).map_err(|e| format!("copy host binary: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = fs::metadata(&dst).map_err(|e| e.to_string())?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&dst, perms).map_err(|e| e.to_string())?;
    }
    Ok(dst)
}

/// Manifest JSON for the extra Chromium channels (same shape the crate writes).
fn extra_chromium_manifest(host_path: &str, extension_ids: &[String]) -> serde_json::Value {
    serde_json::json!({
        "name": HOST_NAME,
        "description": DESCRIPTION,
        "path": host_path,
        "type": "stdio",
        "allowed_origins": extension_ids.iter().map(|id| format!("chrome-extension://{id}/")).collect::<Vec<_>>(),
    })
}

/// Ensure the Firefox native host is installed, silently fixing it if missing
/// or stale. The Firefox add-on ID is fixed, so this needs no user input and
/// runs on every startup. Returns true if (re)installed.
pub fn ensure_firefox_host() -> bool {
    if in_flatpak() {
        return false;
    }
    let home = match home_dir() {
        Some(h) => h,
        None => return false,
    };
    let manifest_path = home.join(".mozilla/native-messaging-hosts").join(format!("{HOST_NAME}.json"));

    // Already installed and the binary exists? Nothing to do.
    if let Ok(text) = fs::read_to_string(&manifest_path) {
        if let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(path) = manifest.get("path").and_then(|p| p.as_str()) {
                if Path::new(path).is_file() {
                    return false;
                }
            }
        }
    }

    // (Re)install the binary and manifest.
    let host_bin = match install_binary() {
        Ok(p) => p,
        Err(_) => return false,
    };
    let host_path = host_bin.to_string_lossy().into_owned();
    let manifest = serde_json::json!({
        "name": HOST_NAME,
        "description": DESCRIPTION,
        "path": host_path,
        "type": "stdio",
        "allowed_extensions": [FIREFOX_ADDON_ID],
    });
    let mut text = serde_json::to_string_pretty(&manifest).unwrap_or_default();
    text.push('\n');
    if let Some(parent) = manifest_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(&manifest_path, text).is_ok()
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
    let host_bin = match install_binary() {
        Ok(p) => p,
        Err(_) => return false,
    };
    let host_path = host_bin.to_string_lossy().into_owned();
    let mut changed = false;

    // Crate-covered browsers.
    let chromium_origins: Vec<String> = ids.iter().map(|id| format!("chrome-extension://{id}/")).collect();
    let firefox_ids = vec![FIREFOX_ADDON_ID.to_string()];
    if native_messaging::install(
        HOST_NAME,
        DESCRIPTION,
        Path::new(&host_path),
        &chromium_origins,
        &firefox_ids,
        CRATE_BROWSERS,
        native_messaging::Scope::User,
    )
    .is_ok()
    {
        changed = true;
    }

    // Extra channels the crate misses.
    for rel in EXTRA_CHROMIUM_DIRS {
        let cfg = home.join(rel);
        if !cfg.is_dir() {
            continue;
        }
        let target = cfg.join("NativeMessagingHosts").join(format!("{HOST_NAME}.json"));
        // Skip if already correct.
        if let Ok(text) = fs::read_to_string(&target) {
            if let Ok(m) = serde_json::from_str::<serde_json::Value>(&text) {
                let origins: Vec<String> = m
                    .get("allowed_origins")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
                    .unwrap_or_default();
                if origins == chromium_origins {
                    continue;
                }
            }
        }
        if let Some(parent) = target.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let mut text = serde_json::to_string_pretty(&extra_chromium_manifest(&host_path, ids)).unwrap_or_default();
        text.push('\n');
        if fs::write(&target, text).is_ok() {
            changed = true;
        }
    }
    changed
}
///
/// `chromium_ids` are the extension IDs from `chrome://extensions` (unpacked
/// installs get a generated ID). The Firefox add-on ID is fixed, so its
/// manifest is always written.
pub fn install(chromium_ids: &[String]) -> Result<Vec<PathBuf>, String> {
    if in_flatpak() {
        return Err(flatpak_instructions().to_string());
    }
    let home = home_dir().ok_or_else(|| "HOME is not set".to_string())?;
    let host_bin = install_binary()?;
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
            fs::write(&target, text).map_err(|e| format!("write {target:?}: {e}"))?;
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
