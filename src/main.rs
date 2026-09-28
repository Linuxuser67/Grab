mod application;
mod attempt_gate;
mod cookies;
mod download;
mod download_fetch;
mod download_intake;
mod download_net;
mod download_pieces;
mod download_rate;
mod download_row;
mod download_store;
mod engine_msg;
mod file_names;
mod install_help;
mod install_progress;
mod media_types;
mod net_types;
mod preferences;
mod runtime;
mod settings;
mod torrent;
mod ui_util;
// Video-page extraction + resolver worker: dialog, prefs, queue and engine entry.
mod video;
mod video_argv;
mod video_plan;
mod video_prefs;
mod video_probe;
mod video_progress;
mod video_quality;
mod video_runner;
mod video_spawn;
mod video_staging;
mod video_tools;
mod video_types;
mod window;
mod window_dialogs;
mod window_rows;

use gtk4::gio::prelude::*;
use gtk4::{gio, glib};
use libadwaita as adw;

pub const APP_ID: &str = "io.github.houssemko.Grab";

/// Metainfo catalog for About, embedded: `from_appdata` reads GResource paths, and OUT_DIR is absent at runtime.
static GRESOURCE_DATA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/grab.gresource"));

/// Locale for translated UI: compiled .mo under prefix or GRAB_LOCALEDIR override; missing catalogs fall back to English.
fn init_locale() {
    // First existing dir wins (override, Flatpak /app, build prefix); missing dir falls back to English.
    let compiled = env!("GRAB_LOCALEDIR").to_string();
    let dir = std::env::var("GRAB_LOCALEDIR")
        .ok()
        .into_iter()
        .chain(["/app/share/locale".to_string(), compiled])
        .find(|d| std::path::Path::new(d).exists())
        .unwrap_or_default();
    gettextrs::bindtextdomain("grab", dir).ok();
    gettextrs::textdomain("grab").ok();
}

/// Whether `dir` provides our schema via explicit source (never the cached process-wide default).
fn schema_in_dir(dir: &str) -> bool {
    gio::SettingsSchemaSource::from_directory(dir, None::<&gio::SettingsSchemaSource>, true)
        .ok()
        .and_then(|source| source.lookup(APP_ID, false))
        .is_some()
}

fn ensure_schema_dir() {
    // Preset dir honored only if it provides our schema, else fall through to our install tree.
    if let Some(preset) = std::env::var_os("GSETTINGS_SCHEMA_DIR")
        && preset.to_string_lossy().split(':').any(schema_in_dir)
    {
        return;
    }
    // Compile-time dir exists only for `cargo run`; fallback is the install tree of this binary (tarball works anywhere).
    let mut candidates = vec![env!("GRAB_SCHEMA_DIR").to_string()];
    if let Ok(exe) = std::env::current_exe()
        && let Some(prefix) = exe.parent().and_then(|b| b.parent())
    {
        candidates.push(
            prefix
                .join("share/glib-2.0/schemas")
                .to_string_lossy()
                .into_owned(),
        );
    }
    if let Some(dir) = candidates
        .into_iter()
        .find(|d| std::path::Path::new(&format!("{d}/gschemas.compiled")).exists())
    {
        // SAFETY: single-threaded startup, before any GSettings use.
        unsafe { std::env::set_var("GSETTINGS_SCHEMA_DIR", dir) };
    }
}

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt::init();
    ensure_schema_dir();
    // Register before dialogs open; corrupt bundle only loses release notes (About guards the missing case).
    if let Ok(res) = gio::Resource::from_data(&glib::Bytes::from_static(GRESOURCE_DATA)) {
        gio::resources_register(&res);
    }
    init_locale();
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();

    application::setup(&app);
    app.run()
}
