//! Typed GSettings access: one method per key, so renames fail at compile time.

use gtk4::gio;
use gtk4::prelude::SettingsExt as _;

pub mod key {
    pub const DOWNLOAD_DIR: &str = "download-dir";
    pub const RESTRICT_FILENAMES: &str = "restrict-filenames";
    pub const ENABLE_SCHEDULED_DOWNLOADS: &str = "enable-scheduled-downloads";
    pub const AUTO_ADD_DOWNLOADS: &str = "auto-add-downloads";
    pub const MAX_CONCURRENT: &str = "max-concurrent";
    pub const CONNECTIONS: &str = "connections";
    pub const SPEED_LIMIT: &str = "speed-limit";
    pub const PROXY_MODE: &str = "proxy-mode";
    pub const PROXY_TYPE: &str = "proxy-type";
    pub const PROXY_HOST: &str = "proxy-host";
    pub const PROXY_PORT: &str = "proxy-port";
    pub const KEEP_SERVER_DATE: &str = "keep-server-date";
    pub const SHOW_NOTIFICATIONS: &str = "show-notifications";
    pub const NOTIFY_BACKGROUND: &str = "notify-background";
    pub const INHIBIT_SUSPEND: &str = "inhibit-suspend";
    pub const TORRENT_SEED_FINISHED: &str = "torrent-seed-finished";
    pub const TORRENT_DHT: &str = "torrent-dht";
    pub const TORRENT_UPLOAD_LIMIT: &str = "torrent-upload-limit";
    pub const TORRENT_SEED_RATIO: &str = "torrent-seed-ratio";
    pub const TORRENT_SEED_TIME: &str = "torrent-seed-time";
    pub const TORRENT_BLOCKLIST_URL: &str = "torrent-blocklist-url";
    pub const TORRENT_LSD: &str = "torrent-lsd";
    pub const TORRENT_LISTEN_PORT: &str = "torrent-listen-port";
    pub const TORRENT_UPNP: &str = "torrent-upnp";
    pub const VIDEO_QUALITY: &str = "video-quality";
    pub const VIDEO_CODEC_PRIORITY: &str = "video-codec-priority";
    pub const SUBTITLE_LANGUAGE: &str = "subtitle-language";
    pub const EMBED_SUBS: &str = "embed-subs";
    pub const COOKIES_BROWSER: &str = "cookies-browser";
    pub const LAST_AUTO_PASTE: &str = "last-auto-paste";
    pub const SPONSORBLOCK_REMOVE: &str = "sponsorblock-remove";
    pub const SPONSORBLOCK_MARK: &str = "sponsorblock-mark";
    pub const REMUX_VIDEO: &str = "remux-video";
    pub const EMBED_CHAPTERS: &str = "embed-chapters";
    pub const LIVE_FROM_START: &str = "live-from-start";
    pub const WINDOW_WIDTH: &str = "window-width";
    pub const WINDOW_HEIGHT: &str = "window-height";
}

#[derive(Clone, Debug)]
pub struct AppSettings(gio::Settings);

impl AppSettings {
    pub fn new() -> Self {
        Self(gio::Settings::new(crate::APP_ID))
    }

    pub fn download_dir(&self) -> String {
        self.0.string(key::DOWNLOAD_DIR).to_string()
    }
    /// Fold download filenames to ASCII-only.
    pub fn restrict_filenames(&self) -> bool {
        self.0.boolean(key::RESTRICT_FILENAMES)
    }
    /// Master toggle for scheduled downloads; off stops the scheduler timer entirely.
    pub fn scheduled_downloads_enabled(&self) -> bool {
        self.0.boolean(key::ENABLE_SCHEDULED_DOWNLOADS)
    }
    /// Skip confirmation: single videos auto-start after lookup, and
    /// links opened from other apps skip the "Add this download?" dialog.
    pub fn auto_add_downloads(&self) -> bool {
        self.0.boolean(key::AUTO_ADD_DOWNLOADS)
    }
    pub fn max_concurrent(&self) -> i32 {
        self.0.int(key::MAX_CONCURRENT)
    }
    pub fn connections(&self) -> i32 {
        self.0.int(key::CONNECTIONS)
    }
    pub fn speed_limit(&self) -> String {
        self.0.string(key::SPEED_LIMIT).to_string()
    }
    pub fn proxy_mode(&self) -> String {
        self.0.string(key::PROXY_MODE).to_string()
    }
    pub fn proxy_type(&self) -> String {
        self.0.string(key::PROXY_TYPE).to_string()
    }
    pub fn proxy_host(&self) -> String {
        self.0.string(key::PROXY_HOST).to_string()
    }
    pub fn proxy_port(&self) -> i32 {
        self.0.int(key::PROXY_PORT)
    }
    pub fn keep_server_date(&self) -> bool {
        self.0.boolean(key::KEEP_SERVER_DATE)
    }
    pub fn show_notifications(&self) -> bool {
        self.0.boolean(key::SHOW_NOTIFICATIONS)
    }
    pub fn notify_background(&self) -> bool {
        self.0.boolean(key::NOTIFY_BACKGROUND)
    }
    pub fn inhibit_suspend(&self) -> bool {
        self.0.boolean(key::INHIBIT_SUSPEND)
    }
    pub fn torrent_seed_finished(&self) -> bool {
        self.0.boolean(key::TORRENT_SEED_FINISHED)
    }
    pub fn torrent_dht(&self) -> bool {
        self.0.boolean(key::TORRENT_DHT)
    }
    pub fn torrent_upload_limit(&self) -> String {
        self.0.string(key::TORRENT_UPLOAD_LIMIT).to_string()
    }
    pub fn torrent_seed_ratio(&self) -> f64 {
        self.0.double(key::TORRENT_SEED_RATIO)
    }
    pub fn torrent_seed_time(&self) -> i32 {
        self.0.int(key::TORRENT_SEED_TIME)
    }
    pub fn torrent_blocklist_url(&self) -> String {
        self.0.string(key::TORRENT_BLOCKLIST_URL).to_string()
    }
    /// Local Service Discovery: find peers on the local network.
    pub fn torrent_lsd(&self) -> bool {
        self.0.boolean(key::TORRENT_LSD)
    }
    /// Listen port for incoming torrent connections. 0 = disabled.
    pub fn torrent_listen_port(&self) -> i32 {
        self.0.int(key::TORRENT_LISTEN_PORT)
    }
    /// UPnP port forwarding for the torrent listen port.
    pub fn torrent_upnp(&self) -> bool {
        self.0.boolean(key::TORRENT_UPNP)
    }
    // Read by the New Download video step (per-download defaults).
    pub fn video_quality(&self) -> String {
        self.0.string(key::VIDEO_QUALITY).to_string()
    }
    pub fn video_codec_priority(&self) -> String {
        self.0.string(key::VIDEO_CODEC_PRIORITY).to_string()
    }
    pub fn video_codec_newest(&self) -> bool {
        self.video_codec_priority() == crate::video::CODEC_PRIORITY_NEWEST
    }
    /// Stored subtitle code (`"off"` disabled); resolved to `Option` at spawn time.
    pub fn subtitle_language(&self) -> String {
        self.0.string(key::SUBTITLE_LANGUAGE).to_string()
    }
    /// Mux subtitles into the file; opt-in, no-op when subs off.
    pub fn embed_subs(&self) -> bool {
        self.0.boolean(key::EMBED_SUBS)
    }
    pub fn cookies_browser(&self) -> String {
        self.0.string(key::COOKIES_BROWSER).to_string()
    }
    pub fn last_auto_paste(&self) -> String {
        self.0.string(key::LAST_AUTO_PASTE).to_string()
    }
    pub fn set_last_auto_paste(&self, url: &str) {
        let _ = self.0.set_string(key::LAST_AUTO_PASTE, url);
    }
    /// Cut SponsorBlock-flagged sponsor segments out of yt-dlp downloads.
    pub fn sponsorblock_remove(&self) -> bool {
        self.0.boolean(key::SPONSORBLOCK_REMOVE)
    }
    /// Mark SponsorBlock-flagged sponsor segments as chapters in yt-dlp downloads.
    pub fn sponsorblock_mark(&self) -> bool {
        self.0.boolean(key::SPONSORBLOCK_MARK)
    }
    /// Target container for opt-in remuxing (`--remux-video`); `"off"` disables it.
    pub fn remux_video(&self) -> String {
        self.0.string(key::REMUX_VIDEO).to_string()
    }
    /// Write chapters into the file; opt-in, live rows never take it.
    pub fn embed_chapters(&self) -> bool {
        self.0.boolean(key::EMBED_CHAPTERS)
    }
    /// Record live from start; opt-in, live rows only.
    pub fn live_from_start(&self) -> bool {
        self.0.boolean(key::LIVE_FROM_START)
    }
    pub fn window_width(&self) -> i32 {
        self.0.int(key::WINDOW_WIDTH)
    }
    pub fn window_height(&self) -> i32 {
        self.0.int(key::WINDOW_HEIGHT)
    }
}

impl From<gio::Settings> for AppSettings {
    fn from(s: gio::Settings) -> Self {
        Self(s)
    }
}

impl std::ops::Deref for AppSettings {
    type Target = gio::Settings;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
