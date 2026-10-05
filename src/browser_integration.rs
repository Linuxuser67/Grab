//! Browser native-messaging host installation.
//!
//! Registers the `grab-native-host` binary with Chromium- and Firefox-based
//! browsers so the Grab browser extension can hand downloads to the desktop
//! app. Ports `native-host/install.py` from the extension repo.
//!
//! In a Flatpak sandbox the browser config dirs are not writable; in that
//! case [`install`] refuses and prints the host-side commands instead.

use std::fs;
use std::path::{Path, PathBuf};

pub const HOST_NAME: &str = "io.github.linuxuser67.grab";
pub const FIREFOX_ADDON_ID: &str = "grab@linuxuser67.github.io";

/// Chromium config dirs probed for `NativeMessagingHosts`.
const CHROMIUM_CONFIG_DIRS: &[&str] = &[
    ".config/BraveSoftware/Brave-Browser",
    ".config/BraveSoftware/Brave-Browser-Beta",
    ".config/BraveSoftware/Brave-Browser-Dev",
    ".config/BraveSoftware/Brave-Browser-Nightly",
    ".config/BraveSoftware/Brave-Origin-Beta",
    ".config/google-chrome",
    ".config/google-chrome-beta",
    ".config/google-chrome-unstable",
    ".config/chromium",
    ".config/microsoft-edge",
    ".config/microsoft-edge-beta",
    ".config/vivaldi",
    ".config/opera",
];

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn in_flatpak() -> bool {
    Path::new("/.flatpak-info").exists()
}

fn host_install_dir() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".local/share/grab-native-host"))
}

/// Locate the `grab-native-host` binary next to the running executable.
fn find_host_binary() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let candidate = dir.join("grab-native-host");
    if candidate.is_file() {
        return Some(candidate);
    }
    // Development: target/debug/ or target/release/
    for profile in ["debug", "release"] {
        let c = dir.join(profile).join("grab-native-host");
        if c.is_file() {
            return Some(c);
        }
    }
    None
}

fn chromium_manifest(host_path: &str, extension_ids: &[String]) -> serde_json::Value {
    serde_json::json!({
        "name": HOST_NAME,
        "description": "Grab download manager native host",
        "path": host_path,
        "type": "stdio",
        "allowed_origins": extension_ids.iter().map(|id| format!("chrome-extension://{id}/")).collect::<Vec<_>>(),
    })
}

fn firefox_manifest(host_path: &str) -> serde_json::Value {
    serde_json::json!({
        "name": HOST_NAME,
        "description": "Grab download manager native host",
        "path": host_path,
        "type": "stdio",
        "allowed_extensions": [FIREFOX_ADDON_ID],
    })
}

fn write_manifest(path: &Path, manifest: &serde_json::Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut text = serde_json::to_string_pretty(manifest).unwrap_or_default();
    text.push('\n');
    fs::write(path, text)
}

/// Install the native host binary and browser manifests.
///
/// `chromium_ids` are the extension IDs from `chrome://extensions` (unpacked
/// installs get a generated ID). The Firefox add-on ID is fixed, so its
/// manifest is always written. Returns the manifest paths written.
pub fn install(chromium_ids: &[String]) -> Result<Vec<PathBuf>, String> {
    if in_flatpak() {
        return Err(flatpak_instructions().to_string());
    }
    let home = home_dir().ok_or_else(|| "HOME is not set".to_string())?;
    let install_dir = host_install_dir().ok_or_else(|| "HOME is not set".to_string())?;
    fs::create_dir_all(&install_dir).map_err(|e| format!("create {install_dir:?}: {e}"))?;

    let src = find_host_binary()
        .ok_or_else(|| "grab-native-host binary not found next to the Grab executable".to_string())?;
    let dst = install_dir.join("grab-native-host");
    fs::copy(&src, &dst).map_err(|e| format!("copy host binary: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = fs::metadata(&dst).map_err(|e| e.to_string())?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&dst, perms).map_err(|e| e.to_string())?;
    }
    let host_path = dst.to_string_lossy().into_owned();

    let mut written = Vec::new();

    for rel in CHROMIUM_CONFIG_DIRS {
        let cfg = home.join(rel);
        if !cfg.is_dir() {
            continue;
        }
        if chromium_ids.is_empty() {
            continue;
        }
        let target = cfg.join("NativeMessagingHosts").join(format!("{HOST_NAME}.json"));
        write_manifest(&target, &chromium_manifest(&host_path, chromium_ids))
            .map_err(|e| format!("write {target:?}: {e}"))?;
        written.push(target);
    }

    let ff_target = home.join(".mozilla/native-messaging-hosts").join(format!("{HOST_NAME}.json"));
    write_manifest(&ff_target, &firefox_manifest(&host_path))
        .map_err(|e| format!("write {ff_target:?}: {e}"))?;
    written.push(ff_target);

    Ok(written)
}

fn flatpak_instructions() -> &'static str {
    "Running in Flatpak: the sandbox cannot write browser config dirs.\n\
     On the host, run:\n  \
     flatpak run --command=grab-native-host io.github.linuxuser67.Grab --install-browser-host --chromium-id <id>\n\
     (get the extension ID from chrome://extensions with Developer mode on)"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chromium_manifest_has_allowed_origins() {
        let m = chromium_manifest("/home/u/.local/share/grab-native-host/grab-native-host", &["abc123".to_string()]);
        assert_eq!(m["name"], HOST_NAME);
        assert_eq!(m["type"], "stdio");
        assert_eq!(m["allowed_origins"], serde_json::json!(["chrome-extension://abc123/"]));
    }

    #[test]
    fn firefox_manifest_has_fixed_addon_id() {
        let m = firefox_manifest("/home/u/.local/share/grab-native-host/grab-native-host");
        assert_eq!(m["name"], HOST_NAME);
        assert_eq!(m["allowed_extensions"], serde_json::json!([FIREFOX_ADDON_ID]));
    }
}
