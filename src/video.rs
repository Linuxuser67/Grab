//! Video-page downloads (YouTube, Vimeo, …) powered by yt-dlp.
//!
//! Facade over the video cluster: probe identity, quality ladder, page
//! probing, preference combos, stream planning, staging/manifest/resume,
//! attempt inputs + argv builders, progress parsing, spawn plumbing and
//! attempt orchestration. Grab only *extracts* with yt-dlp: format URLs are
//! resolved here and the bytes are pulled by the existing engine as ordinary
//! queue items.

/// Facade: the per-attempt delivery decision lives in
/// [`attempt_gate`](crate::attempt_gate) now.
pub use crate::attempt_gate::AttemptGate;
/// Facade: attempt inputs + argv builders live in
/// [`video_argv`](crate::video_argv) now.
pub use crate::video_argv::VideoJob;
/// Facade: preference combos + active-value resolution live in
/// [`video_prefs`](crate::video_prefs) now.
pub use crate::video_prefs::{
    CODEC_PRIORITY_NEWEST, codec_priority_index, codec_priority_labels, codec_priority_value,
    cookies_browser_index, cookies_browser_labels, cookies_browser_value, remux_video_index,
    remux_video_labels, remux_video_value, subtitle_language_index, subtitle_language_labels,
    subtitle_language_value,
};
/// Facade: page probing + playlist parsing lives in
/// [`video_probe`](crate::video_probe) now.
pub use crate::video_probe::{
    drive_direct_url, is_direct_file_url, is_http_url, is_stream_manifest_url,
};
/// Facade: quality ladder lives in [`video_quality`](crate::video_quality) now.
pub use crate::video_quality::{
    default_quality_index, default_video_filename, quality_for_height, quality_labels,
};
/// Facade: attempt orchestration lives in [`video_runner`](crate::video_runner) now.
pub use crate::video_runner::{StopIntent, run_video_download};
/// Facade: spawn plumbing + fetch resolve live in
/// [`video_spawn`](crate::video_spawn) now.
pub use crate::video_spawn::fetch_video_infos;
#[cfg(test)]
pub use crate::video_staging::clean_staging;
pub use crate::video_staging::clean_staging_files;
/// Facade: staging/parts/manifest/resume lives in
/// [`video_staging`](crate::video_staging) now.
pub use crate::video_staging::{
    clean_dest_parts, staging_occupied, staging_root, sweep_dest_staging,
};
/// Facade: tool provisioning lives in [`video_tools`](crate::video_tools) now.
pub use crate::video_tools::{
    install_ffmpeg, install_quickjs, install_ytdlp, latest_ffmpeg_tag, latest_quickjs_tag,
    latest_ytdlp_tag, resolve_libraries,
};
/// Facade: probe identity lives in [`video_types`](crate::video_types) now.
pub use crate::video_types::{ProbeResult, VideoInfo, VideoOutcome, is_video_page, preview_fresh};

#[cfg(test)]
pub(crate) mod test_support {
    /// Scrub tool lookup so video spawns deterministically fail with
    /// MissingLibraries instead of depending on the dev machine. Serial suite
    /// only: the environment is process-global. Restores on drop.
    pub(crate) struct NoVideoTools {
        path: Option<std::ffi::OsString>,
        xdg: Option<std::ffi::OsString>,
    }

    impl NoVideoTools {
        pub(crate) fn apply() -> Self {
            let path = std::env::var_os("PATH");
            let xdg = std::env::var_os("XDG_DATA_HOME");
            // SAFETY: serial suite (cargo runs with --test-threads=1); no
            // other test reads PATH/XDG while this guard lives.
            unsafe {
                std::env::set_var("PATH", "/nonexistent-grab-test");
                std::env::set_var("XDG_DATA_HOME", "/nonexistent-grab-test-dir");
            }
            Self { path, xdg }
        }
    }

    impl Drop for NoVideoTools {
        fn drop(&mut self) {
            // SAFETY: same serial-suite context as apply().
            unsafe {
                match &self.path {
                    Some(v) => std::env::set_var("PATH", v),
                    None => std::env::remove_var("PATH"),
                }
                match &self.xdg {
                    Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                    None => std::env::remove_var("XDG_DATA_HOME"),
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "video_tests.rs"]
mod tests;
