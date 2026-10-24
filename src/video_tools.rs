//! Tool resolution and managed provisioning for yt-dlp/Deno, plus system
//! FFmpeg discovery and version checks. Leaf module: the dialog, prefs and engines
//! consume it through the `video` facade.

use crate::download_net::tool_client_builder_proxied;
use gettextrs::gettext;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use thiserror::Error;
use yt_dlp::client::deps::Libraries;
use yt_dlp::model::DrmStatus;
use yt_dlp::model::format::{Format, FormatType, Protocol};

/// Errors surfaced by the video pipeline. User-facing strings are translated at
/// construction; match on the variant to branch the UI (install banner vs retry).
#[derive(Debug, Error)]
pub enum VideoError {
    /// A required toolchain is missing from the managed/system search paths.
    #[error("{0}")]
    MissingLibraries(String),
    /// The page could not be extracted (wrong URL, offline, …). Retryable.
    #[error("{0}")]
    Fetch(String),
    /// A tool was present but its version command exceeded the probe deadline.
    #[error("{0}")]
    ToolProbeTimeout(String),
    /// Background task machinery failed (panic/join) — internal.
    #[error("{0}")]
    Runtime(String),
    /// The probe was cancelled by its caller — internal, never surfaced
    /// to the user.
    #[error("probe cancelled")]
    Cancelled,
    /// Other translated failure.
    #[error("{0}")]
    Message(String),
}

impl VideoError {
    pub(crate) fn missing_tools() -> Self {
        Self::MissingLibraries(gettext("Media downloads need the yt-dlp support tools"))
    }
    pub(crate) fn tool_probe_failure(error: ToolProbeError) -> Self {
        match error {
            ToolProbeError::Timeout => Self::ToolProbeTimeout(gettext(
                "Checking the media support tools timed out. Please try again.",
            )),
            _ => Self::missing_tools(),
        }
    }
    pub(crate) fn fetch(e: impl std::fmt::Display) -> Self {
        Self::Fetch(
            gettext("Couldn't read the video page: {detail}").replace("{detail}", &e.to_string()),
        )
    }
    pub(crate) fn install(e: impl std::fmt::Display) -> Self {
        Self::Message(
            gettext("Couldn't install the media support tools: {detail}")
                .replace("{detail}", &e.to_string()),
        )
    }
    pub(crate) fn staging(e: impl std::fmt::Display) -> Self {
        Self::Message(
            gettext("Couldn't prepare video staging: {detail}").replace("{detail}", &e.to_string()),
        )
    }
    pub(crate) fn runtime(e: impl std::fmt::Display) -> Self {
        Self::Runtime(e.to_string())
    }
    pub(crate) fn cancelled() -> Self {
        Self::Cancelled
    }
    pub(crate) fn unavailable() -> Self {
        Self::Message(gettext("No suitable formats found for this media"))
    }
    /// Same failure with a rejection census, so a manifest-only page (live/HLS)
    /// reads differently from a DRM or link-less one instead of guessing.
    pub(crate) fn unavailable_detail(formats: &[Format]) -> Self {
        let total = formats.len();
        if total == 0 {
            return Self::Message(gettext(
                "No suitable formats found for this media (the page listed none)",
            ));
        }
        let (mut manifest, mut drm, mut no_link, mut video_only, mut unclassified) =
            (0, 0, 0, 0, 0);
        for f in formats {
            if f.protocol != Protocol::Https {
                manifest += 1;
            } else if matches!(f.has_drm, Some(DrmStatus::Yes)) {
                drm += 1;
            } else if f
                .download_info
                .url
                .as_deref()
                .filter(|u| !u.is_empty())
                .is_none()
            {
                no_link += 1;
            } else if f.format_type() == FormatType::Unknown {
                unclassified += 1;
            } else if f
                .codec_info
                .audio_codec
                .as_deref()
                .is_none_or(|c| c == "none")
            {
                video_only += 1;
            }
        }
        let mut reasons = Vec::new();
        for (n, label) in [
            (manifest, gettext("manifest")),
            (drm, gettext("DRM")),
            (no_link, gettext("no link")),
            (video_only, gettext("video-only")),
            (unclassified, gettext("unclassified")),
        ] {
            if n > 0 {
                reasons.push(format!("{label}: {n}"));
            }
        }
        Self::Message(
            gettext("No suitable formats found for this media ({total} listed: {reasons})")
                .replace("{total}", &total.to_string())
                .replace("{reasons}", &reasons.join(", ")),
        )
    }
    pub(crate) fn part_failed(e: impl std::fmt::Display) -> Self {
        Self::Message(
            gettext("Media download failed: {detail}").replace("{detail}", &e.to_string()),
        )
    }
    pub(crate) fn combine(e: impl std::fmt::Display) -> Self {
        Self::Message(
            gettext("Couldn't merge video and audio: {detail}").replace("{detail}", &e.to_string()),
        )
    }
    /// Append to the rendered message, preserving the variant: salvage exits
    /// keep the raw shell inside the hidden staging dir, so the failure has
    /// to name where the recording went.
    pub(crate) fn with_suffix(self, suffix: impl std::fmt::Display) -> Self {
        let suffix = suffix.to_string();
        match self {
            Self::MissingLibraries(s) => Self::MissingLibraries(format!("{s}{suffix}")),
            Self::Fetch(s) => Self::Fetch(format!("{s}{suffix}")),
            Self::ToolProbeTimeout(s) => Self::ToolProbeTimeout(format!("{s}{suffix}")),
            Self::Runtime(s) => Self::Runtime(format!("{s}{suffix}")),
            Self::Message(s) => Self::Message(format!("{s}{suffix}")),
            Self::Cancelled => Self::Cancelled,
        }
    }
    pub(crate) fn interrupted() -> Self {
        Self::Message(gettext("Download interrupted"))
    }
    pub(crate) fn outdated() -> Self {
        Self::Message(gettext("Video tools are too old — update them to continue"))
    }
    /// The exact [`crate::engine_msg::DEST_EXISTS`] sentence, so a foreign
    /// file at the destination fails the row Parabolic-style.
    pub(crate) fn exists() -> Self {
        Self::Message(crate::engine_msg::DEST_EXISTS.to_string())
    }
}

/// Whether we run inside the Flatpak sandbox. Only there is the Install button
/// the viable path; tarball/dev builds get guided self-install instead.
pub(crate) fn in_flatpak() -> bool {
    std::path::Path::new("/.flatpak-info").exists()
}

/// Package manager commands for the detected distro; `None` = unknown, so show
/// manual install links instead of a wrong command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DistroPackages {
    /// Pretty distro name for the dialog title, e.g. "Fedora".
    pub distro: String,
    /// One-line distro command for yt-dlp, ffmpeg, and Deno.
    pub install_all: String,
}

fn package_manager(id: &str) -> Option<&'static str> {
    match id {
        "fedora" | "rhel" | "centos" | "almalinux" | "rocky" => Some("sudo dnf install"),
        "ubuntu" | "debian" | "pop" | "linuxmint" | "elementary" | "zorin" => {
            Some("sudo apt install")
        }
        "arch" | "manjaro" | "endeavouros" | "cachyos" => Some("sudo pacman -S"),
        "opensuse-tumbleweed" | "opensuse-leap" | "sles" | "opensuse" => {
            Some("sudo zypper install")
        }
        "alpine" => Some("sudo apk add"),
        "gentoo" => Some("sudo emerge --ask"),
        "void" => Some("sudo xbps-install -S"),
        "solus" => Some("sudo eopkg install"),
        _ => None,
    }
}

/// Parse `/etc/os-release` content into install commands. Takes the content (not
/// the path) so tests feed fixtures directly; falls back to `ID_LIKE` tokens.
pub(crate) fn distro_packages(os_release: &str) -> Option<DistroPackages> {
    let mut id: Option<&str> = None;
    let mut id_like = "";
    let mut name: Option<&str> = None;
    for line in os_release.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim_matches('"');
        match key {
            "ID" => id = Some(value),
            "ID_LIKE" => id_like = value,
            "NAME" => name = Some(value),
            _ => {}
        }
    }
    let id = id?;
    let pm =
        package_manager(id).or_else(|| id_like.split_whitespace().find_map(package_manager))?;
    Some(DistroPackages {
        distro: name.unwrap_or(id).to_string(),
        install_all: format!("{pm} yt-dlp ffmpeg deno"),
    })
}

#[cfg(test)]
mod distro_package_tests {
    use super::distro_packages;

    #[test]
    fn install_command_includes_deno_for_supported_package_managers() {
        let cases = [
            (
                "ID=fedora\nNAME=Fedora\n",
                "sudo dnf install yt-dlp ffmpeg deno",
            ),
            (
                "ID=debian\nNAME=Debian GNU/Linux\n",
                "sudo apt install yt-dlp ffmpeg deno",
            ),
            (
                "ID=arch\nNAME=Arch Linux\n",
                "sudo pacman -S yt-dlp ffmpeg deno",
            ),
        ];

        for (os_release, expected) in cases {
            let packages = distro_packages(os_release).expect("supported distro");
            assert_eq!(packages.install_all, expected);
        }
    }

    #[test]
    fn install_command_uses_id_like_fallback_with_deno() {
        let packages =
            distro_packages("ID=custom\nID_LIKE=debian\n").expect("Debian-compatible distro");
        assert_eq!(packages.install_all, "sudo apt install yt-dlp ffmpeg deno");
    }
}

/// Where on-demand tool installs keep the yt-dlp/ffmpeg binaries:
/// `$XDG_DATA_HOME/grab/libs`. Both Flatpak and tarball/dev builds fetch here;
/// `/app/bin` and PATH remain fallbacks for system-provided copies.
///
/// Security: when both `XDG_DATA_HOME` and `HOME` are unset (broken container),
/// the temp-dir fallback is world-writable. We validate it like
/// `cookie_staging_dir()`: must be owned by us, not a symlink. On failure we
/// return a nonexistent path so no planted binary is ever discovered.
pub fn user_lib_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    match base {
        Some(b) => b.join("grab").join("libs"),
        None => {
            // Temp fallback: validate before use, but don't create here —
            // this is a getter, and the per-PID fallback never persists anyway.
            // Creation happens at install time.
            let fallback =
                std::env::temp_dir().join(format!("grab-fallback-data-{}", std::process::id()));
            if crate::download_net::validate_secure_dir(&fallback) || !fallback.exists() {
                fallback.join("grab").join("libs")
            } else {
                // Validation failed: return nonexistent path.
                PathBuf::from("/nonexistent-grab-lib-dir")
            }
        }
    }
}

/// Create the install library dir (0700). The `/tmp` fallback root (only when
/// `XDG_DATA_HOME` and `HOME` are both unset) is validated after creation —
/// owned by us, not a symlink, not group/world-writable — so a pre-planted
/// directory is refused. Synchronous: must work from the GTK main loop, where
/// `spawn_blocking` would panic with "no reactor running".
pub(crate) fn ensure_lib_dir(dir: &Path) -> Result<(), VideoError> {
    ensure_lib_dir_blocking(dir).map_err(VideoError::install)
}

fn ensure_lib_dir_blocking(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        let tmp = std::env::temp_dir();
        if let Ok(rel) = dir.strip_prefix(&tmp)
            && let Some(first) = rel.components().next()
            && first
                .as_os_str()
                .to_string_lossy()
                .starts_with("grab-fallback-data-")
        {
            let root = tmp.join(first);
            match std::fs::DirBuilder::new().mode(0o700).create(&root) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
            if !crate::download_net::validate_secure_dir(&root) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "temporary library dir is not private to this user",
                ));
            }
        }
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// Bundled-tool dir inside the Flatpak sandbox — Flatpak mounts the app tree at
/// `/app`; this is not an XDG rule. Absent outside Flatpak.
const FLATPAK_APP_BIN: &str = "/app/bin";

/// Flatpak runtime tools live in `/usr/bin`; an app-bundled copy would be in
/// `/app/bin`.
const FLATPAK_RUNTIME_BIN: &str = "/usr/bin";

/// Tool search dirs for the self-updating tools, in priority order: the user's
/// own installs first (so Update takes effect over a bundled copy), then
/// `/app/bin` (Flatpak), then PATH. A stale user copy cannot pin old tools — the
/// version floor refuses it. FFmpeg uses [`ffmpeg_search_dirs`] instead: under
/// Flatpak a user copy must not shadow the runtime.
pub(crate) fn tool_search_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![user_lib_dir(), PathBuf::from(FLATPAK_APP_BIN)];
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path).filter(|d| !d.as_os_str().is_empty()));
    }
    dirs
}

fn ffmpeg_search_dirs_in(
    flatpak: bool,
    app_bin: &Path,
    runtime_bin: &Path,
    fallback: impl FnOnce() -> Vec<PathBuf>,
) -> Vec<PathBuf> {
    if flatpak {
        vec![app_bin.to_path_buf(), runtime_bin.to_path_buf()]
    } else {
        fallback()
    }
}

/// System FFmpeg/ffprobe search paths outside Flatpak. Do not use Grab's private
/// tool directory: older copies may remain there from versions that downloaded
/// FFmpeg, but Grab no longer manages or prefers them.
fn system_ffmpeg_search_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|d| !d.as_os_str().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// FFmpeg/ffprobe search order. Flatpak prefers an app-bundled `/app/bin` copy
/// and otherwise uses the runtime's `/usr/bin`. Other builds use system `PATH`.
/// Grab's private tool directory is never searched.
pub(crate) fn ffmpeg_search_dirs() -> Vec<PathBuf> {
    ffmpeg_search_dirs_in(
        in_flatpak(),
        Path::new(FLATPAK_APP_BIN),
        Path::new(FLATPAK_RUNTIME_BIN),
        system_ffmpeg_search_dirs,
    )
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    p.is_file()
        && p.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

pub(crate) fn find_in_dirs(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter().map(|d| d.join(name)).find(|p| is_executable(p))
}

/// True only when yt-dlp is available from its managed/search paths and a
/// complete ffmpeg/ffprobe toolchain is available from the FFmpeg resolver.
fn core_tools_available_in(yt_dlp_dirs: &[PathBuf], ffmpeg_dirs: &[PathBuf]) -> bool {
    find_in_dirs("yt-dlp", yt_dlp_dirs).is_some() && toolchain_dir_in(ffmpeg_dirs).is_some()
}

/// Required tools available anywhere Grab can resolve them, including distro tools on PATH.
pub(crate) fn core_tools_available() -> bool {
    core_tools_available_in(&tool_search_dirs(), &ffmpeg_search_dirs())
}

/// Flatpak readiness: yt-dlp is a Grab-managed user-dir install, while
/// ffmpeg/ffprobe come from `/app/bin` or the runtime. A user-dir FFmpeg is
/// deliberately ignored so a stale copy cannot shadow the runtime.
fn flatpak_core_tools_installed_in(user_dir: &Path, app_bin: &Path, runtime_bin: &Path) -> bool {
    find_in_dirs("yt-dlp", &[user_dir.to_path_buf()]).is_some()
        && toolchain_dir_in(&[app_bin.to_path_buf(), runtime_bin.to_path_buf()]).is_some()
}

/// True when the core video tools are ready. Under Flatpak yt-dlp must be in
/// Grab's private directory and FFmpeg comes from `/app/bin` or the runtime;
/// other builds resolve yt-dlp from the tool search paths and FFmpeg from system PATH.
pub(crate) fn managed_core_tools_installed() -> bool {
    let user_dir = user_lib_dir();
    if in_flatpak() {
        flatpak_core_tools_installed_in(
            &user_dir,
            Path::new(FLATPAK_APP_BIN),
            Path::new(FLATPAK_RUNTIME_BIN),
        )
    } else {
        core_tools_available()
    }
}

/// Resolve only the tool needed to inspect a media page. Metadata extraction
/// does not invoke FFmpeg, so lookup must remain available on yt-dlp-only
/// installations. The full download resolver below adds the FFmpeg pair.
pub(crate) fn resolve_ytdlp() -> Result<PathBuf, VideoError> {
    find_in_dirs("yt-dlp", &tool_search_dirs()).ok_or_else(VideoError::missing_tools)
}

/// Locate yt-dlp and a complete FFmpeg/ffprobe pair for actual media downloads.
pub fn resolve_libraries() -> Result<Libraries, VideoError> {
    let youtube = resolve_ytdlp()?;
    // Resolve FFmpeg as a pair from one directory. A lone app-bundled ffmpeg
    // must not shadow the runtime's complete ffmpeg/ffprobe toolchain.
    let ffmpeg_dir =
        toolchain_dir_in(&ffmpeg_search_dirs()).ok_or_else(VideoError::missing_tools)?;
    Ok(Libraries::new(youtube, ffmpeg_dir.join("ffmpeg")))
}

/// Modification time and size used to invalidate executable-probe caches.
type FileStamp = (Option<std::time::SystemTime>, u64);

const FAILED_PROBE_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(2);

struct ProbeCacheEntry<V, E> {
    stamp: FileStamp,
    cached_at: std::time::Instant,
    result: Result<V, E>,
}

struct ProbeCache<K, V, E> {
    values: Mutex<HashMap<K, ProbeCacheEntry<V, E>>>,
    locks: Mutex<HashMap<K, Arc<tokio::sync::Mutex<()>>>>,
}

impl<K, V, E> ProbeCache<K, V, E>
where
    K: Eq + std::hash::Hash + Clone,
    V: Clone,
    E: Clone,
{
    fn new() -> Self {
        Self {
            values: Mutex::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
        }
    }

    async fn get_or_probe<F, Fut>(
        &'static self,
        key: K,
        binary: &Path,
        failure_ttl: std::time::Duration,
        probe: F,
    ) -> Result<V, E>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<V, E>>,
    {
        let probe_lock = {
            let mut locks = self.locks.lock().unwrap_or_else(|p| p.into_inner());
            if locks.len() > 128 {
                // Preserve locks owned by a probe or its waiters; removing one
                // while in use could permit two probes for the key.
                locks.retain(|_, lock| Arc::strong_count(lock) > 1);
            }
            locks
                .entry(key.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _guard = probe_lock.lock().await;
        let stamp = file_stamp(binary);
        {
            let mut values = self.values.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(entry) = values.get(&key)
                && entry.stamp == stamp
                && (entry.result.is_ok() || entry.cached_at.elapsed() < failure_ttl)
            {
                return entry.result.clone();
            }
            values.remove(&key);
        }

        let result = probe().await;
        let after = file_stamp(binary);
        if stamp != (None, 0) && stamp == after && (result.is_ok() || !failure_ttl.is_zero()) {
            let mut values = self.values.lock().unwrap_or_else(|p| p.into_inner());
            if values.len() >= 128 {
                let evict = values.keys().find(|existing| *existing != &key).cloned();
                if let Some(evict) = evict {
                    values.remove(&evict);
                }
            }
            values.insert(
                key,
                ProbeCacheEntry {
                    stamp,
                    cached_at: std::time::Instant::now(),
                    result: result.clone(),
                },
            );
        }
        result
    }
}

fn file_stamp(binary: &Path) -> FileStamp {
    std::fs::metadata(binary)
        .map(|m| (m.modified().ok(), m.len()))
        .unwrap_or((None, 0))
}

static IMPERSONATE_CACHE: OnceLock<ProbeCache<PathBuf, bool, ()>> = OnceLock::new();

/// Whether yt-dlp's --impersonate flag is supported by this binary.
/// The probe is asynchronous and forwarded to Tokio when called outside a
/// runtime context. Failed probes are shared briefly, then retried; the 15-second
/// process deadline prevents a wedged executable from blocking indefinitely.
pub(crate) async fn ytdlp_supports_impersonation(youtube_bin: &Path) -> bool {
    let key = youtube_bin.to_path_buf();
    if tokio::runtime::Handle::try_current().is_err() {
        return AbortOnDrop(
            crate::runtime::tokio_rt()
                .spawn(async move { ytdlp_supports_impersonation_cached(key).await }),
        )
        .await
        .unwrap_or(false);
    }
    ytdlp_supports_impersonation_cached(key).await
}

async fn ytdlp_supports_impersonation_cached(key: PathBuf) -> bool {
    let binary = key.clone();
    let probe_binary = binary.clone();
    IMPERSONATE_CACHE
        .get_or_init(ProbeCache::new)
        .get_or_probe(key, &binary, FAILED_PROBE_CACHE_TTL, || async move {
            probe_impersonate_support(&probe_binary).await
        })
        .await
        .unwrap_or(false)
}

/// One-shot `--list-impersonate-targets` probe. The 15-second limit is retained
/// as a guard against a genuinely wedged executable; process waiting and output
/// draining are asynchronous. Timeout cleanup signals the whole process group.
async fn probe_impersonate_support(youtube_bin: &Path) -> Result<bool, ()> {
    use std::process::Stdio;
    let mut command = tokio::process::Command::new(youtube_bin);
    command
        .arg("--list-impersonate-targets")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.as_std_mut().process_group(0);
    }
    let child = command.spawn().map_err(|_| ())?;
    let pid = child.id().unwrap_or_default();
    let mut group = ProbeProcessGroup::new(pid);
    match tokio::time::timeout(std::time::Duration::from_secs(15), child.wait_with_output()).await {
        Ok(Ok(output)) => {
            // The child has exited, regardless of its exit code. Do not let
            // the drop guard signal a process group whose PID may be reused.
            group.disarm();
            if !output.status.success() {
                return Err(());
            }
            let out = String::from_utf8_lossy(&output.stdout);
            let supported = out.lines().any(|line| {
                let line = line.to_ascii_lowercase();
                line.contains("chrome") && !line.contains("unavailable")
            });
            Ok(supported)
        }
        Err(_) | Ok(Err(_)) => Err(()),
    }
}

/// Best-effort process-group cleanup when a probe is cancelled or times out.
/// The command independently configures kill-on-drop for the direct child.
struct ProbeProcessGroup {
    pid: u32,
    armed: bool,
}

impl ProbeProcessGroup {
    fn new(pid: u32) -> Self {
        Self { pid, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProbeProcessGroup {
    fn drop(&mut self) {
        if self.armed {
            kill_probe_group(self.pid);
        }
    }
}

fn kill_probe_group(pid: u32) {
    #[cfg(unix)]
    if pid != 0 {
        // SAFETY: constant signal; ESRCH is harmless if the child raced exit.
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// `--downloader-args` value for yt-dlp's ffmpeg downloader on live captures.
/// LL-HLS timestamp hygiene: split A/V HLS manifests drift when the
/// broadcaster drops frames; `-copyts -avoid_negative_ts make_zero` keeps the
/// tracks aligned and `-fps_mode passthrough` avoids frame-rate munging on
/// copy — all without re-encoding. `-fps_mode` needs ffmpeg 7+, which the
/// current 7.0 toolchain floor satisfies, so no version gating.
pub(crate) const LIVE_DOWNLOADER_ARGS: &str =
    "ffmpeg:-fps_mode passthrough -copyts -avoid_negative_ts make_zero";

/// Input-side `--downloader-args` value for yt-dlp's ffmpeg downloader on live
/// captures. `-seg_max_retry` is an HLS demuxer option (ffmpeg defaults it to
/// 0, so a segment that fails to open is skipped immediately). Retries give a
/// transient failure a chance to recover; this does not repair a corrupt body
/// or renew an invalidated session. Keep the `ffmpeg_i:` scope: yt-dlp's bare
/// `ffmpeg:` applies after `-i`.
/// fMP4 HLS URLs often end in .fmp4 while FFmpeg detects MOV/MP4.
pub(crate) const LIVE_INPUT_DOWNLOADER_ARGS: &str = "ffmpeg_i:-allowed_extensions 3gp,aac,avi,ac3,eac3,flac,mkv,m3u8,m4a,m4s,m4v,mpg,mov,mp2,mp3,mp4,mpeg,mpegts,ogg,ogv,oga,ts,vob,vtt,wav,webvtt,cmfv,cmfa,ec3,fmp4 -seg_max_retry 3";

static FFMPEG_HLS_CACHE: OnceLock<ProbeCache<PathBuf, bool, ()>> = OnceLock::new();

/// Whether this FFmpeg supports the HLS demuxer's extension_picky option.
pub(crate) async fn ffmpeg_supports_hls_extension_picky(ffmpeg_bin: &Path) -> bool {
    let key = ffmpeg_bin.to_path_buf();
    if tokio::runtime::Handle::try_current().is_err() {
        return AbortOnDrop(
            crate::runtime::tokio_rt()
                .spawn(async move { ffmpeg_supports_hls_extension_picky_cached(key).await }),
        )
        .await
        .unwrap_or(false);
    }
    ffmpeg_supports_hls_extension_picky_cached(key).await
}

async fn ffmpeg_supports_hls_extension_picky_cached(key: PathBuf) -> bool {
    let binary = key.clone();
    let probe_binary = binary.clone();
    FFMPEG_HLS_CACHE
        .get_or_init(ProbeCache::new)
        .get_or_probe(key, &binary, FAILED_PROBE_CACHE_TTL, || async move {
            probe_ffmpeg_hls_extension_picky(&probe_binary).await
        })
        .await
        .unwrap_or(false)
}

async fn probe_ffmpeg_hls_extension_picky(ffmpeg_bin: &Path) -> Result<bool, ()> {
    use std::process::Stdio;
    let mut command = tokio::process::Command::new(ffmpeg_bin);
    command
        .args(["-hide_banner", "-h", "demuxer=hls"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.as_std_mut().process_group(0);
    }
    let child = command.spawn().map_err(|_| ())?;
    let pid = child.id().unwrap_or_default();
    let mut group = ProbeProcessGroup::new(pid);
    let output =
        match tokio::time::timeout(std::time::Duration::from_secs(5), child.wait_with_output())
            .await
        {
            Ok(Ok(output)) => {
                group.disarm();
                output
            }
            Err(_) | Ok(Err(_)) => return Err(()),
        };
    if !output.status.success() {
        return Err(());
    }
    let help = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(help.contains("extension_picky"))
}

/// `--postprocessor-args` value moving the moov atom to the front of merged
/// mp4s so playback starts without a full scan. Scoped `Merger+ffmpeg` (per
/// yt-dlp's `PP+EXE:ARGS` syntax) so metadata/subtitle fixups are untouched.
/// mov-only: only passed when the merge target is mp4-family.
pub(crate) const MERGER_FASTSTART_ARGS: &str = "Merger+ffmpeg:-movflags +faststart";

/// Install yt-dlp into the user library directory. The crate installer exposes
/// no progress, so await this from a spawned task — never block the GTK thread.
/// yt-dlp binary size cap: the linux standalone is ~30MB; anything larger is
/// not the released binary.
const YTDLP_MAX_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;

pub async fn install_ytdlp() -> Result<PathBuf, VideoError> {
    let (url, digest) = latest_ytdlp_asset()
        .await
        .ok_or_else(|| VideoError::install("couldn't determine the latest yt-dlp release"))?;
    // Fail closed: no digest means no verified install. The digest comes from
    // GitHub's API alongside the binary; absent means we can't verify.
    let digest = digest.ok_or_else(|| {
        VideoError::install(
            "yt-dlp release has no SHA-256 digest; refusing unverified install. \
             Try the stable channel instead.",
        )
    })?;
    let dir = user_lib_dir();
    ensure_lib_dir(&dir)?;
    // Download straight to `yt-dlp.part`: a failed download must never leave a
    // half-written `yt-dlp` behind.
    let dest = dir.join("yt-dlp");
    let part = dir.join("yt-dlp.part");
    let dest_clone = dest.clone();
    let handle = crate::runtime::tokio_rt().spawn(async move {
        let result = fetch_ytdlp_inner(&url, &part, &dest_clone, &digest).await;
        if result.is_err() {
            std::fs::remove_file(&part).ok();
        }
        result
    });
    match handle.await {
        Ok(Ok(())) => Ok(dest),
        Ok(Err(e)) => Err(e),
        Err(e) => Err(VideoError::runtime(&e)),
    }
}

async fn fetch_ytdlp_inner(
    url: &str,
    part: &Path,
    dest: &Path,
    digest: &str,
) -> Result<(), VideoError> {
    download_to_file(url, part, digest, YTDLP_MAX_DOWNLOAD_BYTES)
        .await
        .map_err(VideoError::install)?;
    use std::os::unix::fs::PermissionsExt as _;
    tokio::fs::set_permissions(part, std::fs::Permissions::from_mode(0o755))
        .await
        .map_err(|e| {
            VideoError::install(format!("couldn't mark {} executable: {e}", part.display()))
        })?;
    tokio::fs::rename(part, dest)
        .await
        .map_err(|e| VideoError::install(format!("couldn't install {}: {e}", dest.display())))
}

/// Stable Deno Linux archives are published by denoland/deno with per-asset SHA-256 digests.
const DENO_MAX_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;
const DENO_MAX_BINARY_BYTES: u64 = 256 * 1024 * 1024;
const DENO_LATEST_RELEASE_API: &str = "https://api.github.com/repos/denoland/deno/releases/latest";
const DENO_RELEASE_METADATA_MAX_BYTES: usize = 1024 * 1024;
const DENO_MIN_VERSION: [u32; 3] = [2, 3, 0];

/// Candidate validated by ensure_deno, kept in sync with yt-dlp's runtime path.
static SELECTED_DENO_PATH: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

fn selected_deno_path_slot() -> &'static Mutex<Option<PathBuf>> {
    SELECTED_DENO_PATH.get_or_init(|| Mutex::new(None))
}

fn remember_deno_path(path: &Path) {
    let mut selected = selected_deno_path_slot()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    *selected = Some(path.to_path_buf());
}

fn selected_deno_path() -> Option<PathBuf> {
    let path = selected_deno_path_slot()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()?;
    is_executable(&path).then_some(path)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DenoRelease {
    tag: String,
    download_url: String,
    sha256: Option<String>,
}

fn deno_asset_name_for_arch(arch: &str) -> Option<&'static str> {
    match arch {
        "x86_64" => Some("deno-x86_64-unknown-linux-gnu.zip"),
        "aarch64" => Some("deno-aarch64-unknown-linux-gnu.zip"),
        _ => None,
    }
}

pub(crate) fn deno_arch_supported() -> bool {
    deno_asset_name_for_arch(std::env::consts::ARCH).is_some()
}

/// Whether a page URL is YouTube. The only site for which Grab explicitly
/// selects Deno; other sites retain yt-dlp's normal runtime discovery.
pub(crate) fn is_youtube_url(page_url: &str) -> bool {
    let host = url::Url::parse(page_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .unwrap_or_default();
    host == "youtube.com"
        || host.ends_with(".youtube.com")
        || host == "youtu.be"
        || host.ends_with(".youtu.be")
        || host == "youtube-nocookie.com"
        || host.ends_with(".youtube-nocookie.com")
}

/// Reject unexpected release tags before they can shape an asset URL.
pub(crate) fn valid_release_tag(tag: &str) -> bool {
    tag.as_bytes()
        .first()
        .is_some_and(|b| b.is_ascii_alphanumeric())
        && tag.bytes().all(|b| {
            matches!(
                b,
                b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'.' | b'-' | b'+' | b'_'
            )
        })
}

pub(crate) fn deno_download_url_for_arch(tag: &str, arch: &str) -> Option<String> {
    if !valid_release_tag(tag) {
        return None;
    }
    let asset = deno_asset_name_for_arch(arch)?;
    Some(format!(
        "https://github.com/denoland/deno/releases/download/{tag}/{asset}"
    ))
}

pub(crate) fn valid_sha256_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Read a text response without allowing untrusted metadata to grow without bound.
pub(crate) async fn bounded_response_text(
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

fn parse_deno_release_metadata(body: &str, arch: &str) -> Option<DenoRelease> {
    let release: serde_json::Value = serde_json::from_str(body).ok()?;
    let tag = release.get("tag_name")?.as_str()?.to_owned();
    if !valid_release_tag(&tag) {
        return None;
    }
    let expected_url = deno_download_url_for_arch(&tag, arch)?;
    let asset_name = deno_asset_name_for_arch(arch)?;
    let asset =
        release.get("assets")?.as_array()?.iter().find(|asset| {
            asset.get("name").and_then(serde_json::Value::as_str) == Some(asset_name)
        })?;
    let download_url = asset.get("browser_download_url")?.as_str()?.to_owned();
    // Only accept the expected upstream asset URL; never fetch a URL supplied
    // by arbitrary release metadata, even when the digest field is present.
    if download_url != expected_url {
        return None;
    }
    let sha256 = asset
        .get("digest")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| value.strip_prefix("sha256:"))
        .filter(|value| valid_sha256_digest(value))
        .map(str::to_owned);
    Some(DenoRelease {
        tag,
        download_url,
        sha256,
    })
}

async fn latest_deno_release() -> Option<DenoRelease> {
    let handle = crate::runtime::tokio_rt().spawn(async move {
        let client = tool_client_builder_proxied()
            .ok()?
            .user_agent(concat!("Grab/", env!("CARGO_PKG_VERSION")))
            .build()
            .ok()?;
        let response = client
            .get(DENO_LATEST_RELEASE_API)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?;
        let body = bounded_response_text(response, DENO_RELEASE_METADATA_MAX_BYTES).await?;
        parse_deno_release_metadata(&body, std::env::consts::ARCH)
    });
    handle.await.ok().flatten()
}

/// Latest stable Deno tag. GitHub's /releases/latest excludes prereleases.
pub async fn latest_deno_tag() -> Option<String> {
    latest_deno_release().await.map(|release| release.tag)
}

/// Locate the Deno selected by ensure_deno, or the first candidate before probing.
pub(crate) fn find_deno() -> Option<PathBuf> {
    selected_deno_path().or_else(|| find_in_dirs("deno", &tool_search_dirs()))
}

fn find_deno_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for dir in tool_search_dirs() {
        if let Some(path) = find_in_dirs("deno", std::slice::from_ref(&dir))
            && !candidates.contains(&path)
        {
            candidates.push(path);
        }
    }
    candidates
}

async fn first_supported_deno(
    candidates: &[PathBuf],
) -> Result<Option<PathBuf>, ToolProbeError> {
    let mut first_error = None;
    for path in candidates {
        match probe_first_line(path, "--version").await {
            Ok(version)
                if parse_dotted_version(&version).is_some_and(|v| v >= DENO_MIN_VERSION) =>
            {
                return Ok(Some(path.clone()));
            }
            Ok(_) => {}
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }

    match first_error {
        Some(error) => Err(error),
        None => Ok(None),
    }
}

fn deno_probe_failure(error: ToolProbeError) -> VideoError {
    VideoError::runtime(
        gettext(
            "Couldn't check the installed Deno version: {detail}. The existing binary was not replaced; retry the operation.",
        )
        .replace("{detail}", &error.to_string()),
    )
}

async fn remove_legacy_qjs_from(dir: &Path) {
    let qjs = dir.join("qjs");
    match tokio::fs::symlink_metadata(&qjs).await {
        Ok(metadata) if metadata.file_type().is_file() => {
            if let Err(error) = tokio::fs::remove_file(&qjs).await {
                tracing::debug!(path = %qjs.display(), %error, "could not remove obsolete Grab-managed qjs binary");
            }
        }
        // Never follow or remove symlinks/directories; only clean the private
        // regular-file path used by older Grab-managed QuickJS installations.
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::debug!(path = %qjs.display(), %error, "could not inspect obsolete Grab-managed qjs binary");
        }
    }
}

/// Install or update the latest stable Deno release. Await off the GTK thread.
pub async fn install_deno() -> Result<PathBuf, VideoError> {
    let _guard = deno_install_lock().lock().await;
    let path = install_deno_locked().await?;
    remember_deno_path(&path);
    remove_legacy_qjs_from(&user_lib_dir()).await;
    Ok(path)
}

fn deno_install_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

async fn install_deno_locked() -> Result<PathBuf, VideoError> {
    let dir = user_lib_dir();
    let handle = crate::runtime::tokio_rt().spawn(async move { install_deno_binary(dir).await });
    match handle.await {
        Ok(result) => result,
        Err(error) => Err(VideoError::runtime(&error)),
    }
}

async fn install_deno_binary(dir: PathBuf) -> Result<PathBuf, VideoError> {
    let release = latest_deno_release()
        .await
        .ok_or_else(|| VideoError::install("couldn't determine the latest stable Deno release"))?;
    let digest = release.sha256.ok_or_else(|| {
        VideoError::install("Deno release has no SHA-256 digest; refusing unverified install")
    })?;
    ensure_lib_dir(&dir)?;
    let archive = dir.join("deno.zip.part");
    let part = dir.join("deno.part");
    let dest = dir.join("deno");
    if let Err(error) = download_to_file(
        &release.download_url,
        &archive,
        &digest,
        DENO_MAX_ARCHIVE_BYTES,
    )
    .await
    {
        std::fs::remove_file(&archive).ok();
        return Err(VideoError::install(error));
    }

    let archive_for_worker = archive.clone();
    let part_for_worker = part.clone();
    let extracted = tokio::task::spawn_blocking(move || {
        extract_deno_archive(&archive_for_worker, &part_for_worker)
    })
    .await
    .map_err(|error| format!("Deno archive extraction task failed: {error}"))
    .and_then(|result| result);
    std::fs::remove_file(&archive).ok();
    if let Err(error) = extracted {
        std::fs::remove_file(&part).ok();
        return Err(VideoError::install(error));
    }

    use std::os::unix::fs::PermissionsExt as _;
    if let Err(error) =
        tokio::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o755)).await
    {
        std::fs::remove_file(&part).ok();
        return Err(VideoError::install(format!(
            "couldn't mark {} executable: {error}",
            part.display()
        )));
    }
    match std::fs::symlink_metadata(&dest) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            std::fs::remove_file(&part).ok();
            return Err(VideoError::install(format!(
                "refusing to replace Deno through symlink {}",
                dest.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            std::fs::remove_file(&part).ok();
            return Err(VideoError::install(format!(
                "couldn't inspect {}: {error}",
                dest.display()
            )));
        }
    }
    if let Err(error) = tokio::fs::rename(&part, &dest).await {
        std::fs::remove_file(&part).ok();
        return Err(VideoError::install(format!(
            "couldn't install {}: {error}",
            dest.display()
        )));
    }
    Ok(dest)
}

/// Extract only the expected executable from the verified archive; never unpack
/// arbitrary archive paths to disk.
fn extract_deno_archive(archive_path: &Path, dest: &Path) -> Result<(), String> {
    use std::io::{Read as _, Write as _};

    clear_dest_refusing_symlink(dest)?;
    let file = std::fs::File::open(archive_path)
        .map_err(|error| format!("couldn't open Deno archive: {error}"))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|error| format!("invalid Deno archive: {error}"))?;
    let mut binary = archive
        .by_name("deno")
        .map_err(|error| format!("Deno archive has no deno executable: {error}"))?;
    if binary.name() != "deno" || !binary.is_file() {
        return Err("Deno archive entry is not the expected executable".to_string());
    }
    if binary
        .unix_mode()
        .is_some_and(|mode| mode & 0o170000 == 0o120000)
    {
        return Err("Deno archive executable must not be a symbolic link".to_string());
    }
    let expected_size = binary.size();
    if expected_size == 0 || expected_size > DENO_MAX_BINARY_BYTES {
        return Err(format!(
            "Deno executable size is outside the accepted limit ({DENO_MAX_BINARY_BYTES} bytes)"
        ));
    }
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .map_err(|error| format!("couldn't create {}: {error}", dest.display()))?;
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = binary
            .read(&mut buffer)
            .map_err(|error| format!("couldn't extract Deno executable: {error}"))?;
        if count == 0 {
            break;
        }
        total = total.saturating_add(count as u64);
        if total > DENO_MAX_BINARY_BYTES {
            return Err(format!(
                "Deno executable exceeds the accepted size limit ({DENO_MAX_BINARY_BYTES} bytes)"
            ));
        }
        output
            .write_all(&buffer[..count])
            .map_err(|error| format!("couldn't write {}: {error}", dest.display()))?;
    }
    if total != expected_size {
        return Err("Deno executable size does not match the archive metadata".to_string());
    }
    output
        .sync_all()
        .map_err(|error| format!("couldn't flush {}: {error}", dest.display()))?;
    Ok(())
}

/// Pre-create guard for tool downloads and extracts: a planted symlink at
/// `dest` would divert the bytes — and the later chmod — onto an arbitrary
/// file, so refuse instead of following it. A stale regular file (crashed
/// run) is removed so the caller can create atomically with `create_new`,
/// which never follows a freshly planted link either (it just fails).
fn clear_dest_refusing_symlink(dest: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(dest) {
        Ok(m) if m.file_type().is_symlink() => Err(format!(
            "refusing to write through symlink {}",
            dest.display()
        )),
        Ok(_) => std::fs::remove_file(dest)
            .map_err(|e| format!("couldn't clear {}: {e}", dest.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("couldn't inspect {}: {e}", dest.display())),
    }
}

pub(crate) async fn download_to_file(
    url: &str,
    dest: &Path,
    expected_sha256: &str,
    max_bytes: u64,
) -> Result<(), String> {
    // Refuse a planted symlink before any I/O: it would divert the download
    // (and the later chmod) onto an arbitrary file.
    clear_dest_refusing_symlink(dest)?;
    // Tool installs use the app-proxy-honoring client: a proxy-configured
    // user's manual install must not go out direct and reveal the machine's
    // address to GitHub. An invalid manual proxy fails here, loudly.
    let client = tool_client_builder_proxied()?
        .build()
        .map_err(|e| format!("couldn't build HTTP client: {e}"))?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("couldn't fetch {url}: {e}"))?;
    let response = response
        .error_for_status()
        .map_err(|e| format!("couldn't fetch {url}: {e}"))?;
    write_capped_stream(
        Box::pin(response.bytes_stream()),
        dest,
        url,
        expected_sha256,
        max_bytes,
    )
    .await
}

/// Stream a download body into `dest`, enforcing the size cap while
/// streaming instead of buffering the whole body up front: a compromised
/// endpoint must not be able to fill memory or disk before we notice.
/// Split from `download_to_file` so the cap is pinnable with a synthetic
/// stream — pushing 8 MiB through a loopback test server proved flaky.
async fn write_capped_stream<S, B, E>(
    mut stream: S,
    dest: &Path,
    url: &str,
    expected_sha256: &str,
    max_bytes: u64,
) -> Result<(), String>
where
    S: futures_util::Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    let mut downloaded: u64 = 0;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .await
        .map_err(|e| format!("couldn't write {}: {e}", dest.display()))?;
    use futures_util::StreamExt as _;
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("couldn't read {url}: {e}"))?;
        let bytes = chunk.as_ref();
        downloaded += bytes.len() as u64;
        if downloaded > max_bytes {
            return Err(format!(
                "{url} exceeds the download size limit ({max_bytes} bytes)"
            ));
        }
        hasher.update(bytes);
        tokio::io::AsyncWriteExt::write_all(&mut file, bytes)
            .await
            .map_err(|e| format!("couldn't write {}: {e}", dest.display()))?;
    }
    // tokio::fs::File completes writes on the blocking pool: write_all
    // returning Ok only means the bytes were accepted, not that they hit
    // the fd. Flush so Ok really means "everything is in the file" —
    // without this a synchronous read right after can see a prefix.
    tokio::io::AsyncWriteExt::flush(&mut file)
        .await
        .map_err(|e| format!("couldn't write {}: {e}", dest.display()))?;
    // Verify SHA-256 before returning Ok: the .part must never be chmod'd or
    // renamed on a hash mismatch. Compare case-insensitively (GitHub hex is lowercase).
    let got: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if !got.eq_ignore_ascii_case(expected_sha256) {
        return Err(format!(
            "{url}: sha256 mismatch (expected {expected_sha256}, got {got})"
        ));
    }
    Ok(())
}

/// Ensure an up-to-date Deno runtime is available for YouTube JS challenges.
pub(crate) async fn ensure_deno(page_url: &str) -> Result<(), VideoError> {
    if !is_youtube_url(page_url) {
        return Ok(());
    }

    // A stale managed copy must not hide a compatible bundled/system Deno.
    let candidates = find_deno_candidates();
    match first_supported_deno(&candidates).await {
        Ok(Some(path)) => {
            remember_deno_path(&path);
            remove_legacy_qjs_from(&user_lib_dir()).await;
            return Ok(());
        }
        Ok(None) => {}
        Err(error) => return Err(deno_probe_failure(error)),
    }
    if let Some(path) = candidates.first() {
        if !in_flatpak() {
            return Err(VideoError::runtime(
                "Deno 2.3.0 or newer is required for YouTube extraction; update Deno, then retry",
            ));
        }
        tracing::info!(
            path = %path.display(),
            "no usable Deno candidate found; updating managed Deno to latest stable"
        );
    } else if !deno_arch_supported() {
        return Err(VideoError::runtime(
            "Deno is required for YouTube extraction on this architecture; install a supported Deno build and retry",
        ));
    } else if !in_flatpak() {
        return Err(VideoError::runtime(
            "Deno is required for YouTube extraction. Install Deno 2.3.0 or newer, then retry",
        ));
    }

    // The installer honors Grab's configured app proxy and accepts only the
    // official architecture-specific asset with its release API SHA-256.
    let _guard = deno_install_lock().lock().await;
    let candidates = find_deno_candidates();
    match first_supported_deno(&candidates).await {
        Ok(Some(path)) => {
            remember_deno_path(&path);
            remove_legacy_qjs_from(&user_lib_dir()).await;
            return Ok(());
        }
        Ok(None) => {}
        Err(error) => return Err(deno_probe_failure(error)),
    }
    let path = install_deno_locked().await?;
    remember_deno_path(&path);
    remove_legacy_qjs_from(&user_lib_dir()).await;
    Ok(())
}

/// Minimum accepted yt-dlp version by release date. Older binaries predate the
/// JS-challenge era and fail in ways that look like broken pages.
pub const MIN_YTDLP_VERSION: [u32; 3] = [2026, 1, 1];
/// Minimum ffmpeg: 7.0 (2024). Older releases lack codec/API coverage Grab relies on.
pub const MIN_FFMPEG_VERSION: [u32; 3] = [7, 0, 0];

/// Parse a dotted version with tolerant affixes into comparable parts: a
/// leading `v`/`n` (boul2gom tags its builds `v9.0.2`, and its binaries report
/// `n9.0.2`) and trailing non-numeric suffixes (`9.0.2-static`) are ignored;
/// missing parts pad with zero (`8.0` → `[8, 0, 0]`). `None` when no numeric
/// version is present (git builds like `N-…`, garbage).
pub(crate) fn parse_dotted_version(s: &str) -> Option<[u32; 3]> {
    let s = s.trim().trim_start_matches(['v', 'V', 'n', 'N']);
    let mut parts = s.split('.');
    let mut out = [0u32; 3];
    for slot in out.iter_mut() {
        let Some(part) = parts.next() else { break };
        let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return None;
        }
        *slot = digits.parse().ok()?;
    }
    Some(out)
}

/// Whether upstream `tag` is newer than the installed version token.
/// Unparseable sides never prompt an update.
pub(crate) fn tool_update_available(installed: &str, tag: &str) -> bool {
    match (parse_dotted_version(installed), parse_dotted_version(tag)) {
        (Some(current), Some(latest)) => latest > current,
        _ => false,
    }
}

/// Version token from an `ffmpeg -version` first line: the word after "version".
/// `ffmpeg version 7.1.2 …` → `7.1.2`. Returns None if there's no "version"
/// word or nothing follows it. A positional nth(2) would match "2000-2025" in
/// git builds ("ffmpeg version N-... Copyright (c) 2000-2025") as 2000.
pub(crate) fn ffmpeg_version_token(first_line: &str) -> Option<&str> {
    let mut words = first_line.split_whitespace();
    for w in words.by_ref() {
        if w == "version" {
            return words.next();
        }
    }
    None
}

/// Real home dir from the passwd database, bypassing sandbox `$HOME` remapping
/// (inside Flatpak `$HOME` is the app sandbox dir). `None` on lookup failure.
///
/// Do NOT "simplify" this to `glib::home_dir()`: it prefers `$HOME`, which is
/// exactly the remapped sandbox dir this function exists to bypass.
#[cfg(unix)]
pub(crate) fn real_home_dir() -> Option<PathBuf> {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStrExt as _;

    // NSS may need more than the POSIX minimum. Retry ERANGE but bound growth
    // so a broken NSS module cannot make this allocate indefinitely.
    const MAX_PASSWD_BUFFER: usize = 1024 * 1024;
    let suggested = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let initial = if suggested > 0 {
        (suggested as usize).clamp(1024, MAX_PASSWD_BUFFER)
    } else {
        16 * 1024
    };
    let mut buffer = vec![0u8; initial];
    loop {
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: entry and buffer are writable for the call; result is an out pointer.
        let status = unsafe {
            libc::getpwuid_r(
                crate::download_net::current_uid(),
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && buffer.len() < MAX_PASSWD_BUFFER {
            buffer.resize((buffer.len() * 2).min(MAX_PASSWD_BUFFER), 0);
            continue;
        }
        if status != 0 || result.is_null() {
            return None;
        }
        // SAFETY: getpwuid_r returned the initialized `entry` record.
        let entry = unsafe { entry.assume_init() };
        if entry.pw_dir.is_null() {
            return None;
        }
        let bytes = unsafe { CStr::from_ptr(entry.pw_dir) }.to_bytes();
        return (!bytes.is_empty()).then(|| PathBuf::from(std::ffi::OsStr::from_bytes(bytes)));
    }
}

#[cfg(not(unix))]
pub(crate) fn real_home_dir() -> Option<PathBuf> {
    None
}

/// Real host config dir, even inside a Flatpak sandbox whose `$HOME` is the app's
/// own: `HOST_XDG_CONFIG_HOME`, then passwd home + `.config`, then XDG fallback.
pub(crate) fn real_config_home() -> PathBuf {
    if let Some(host) = std::env::var_os("HOST_XDG_CONFIG_HOME") {
        let p = PathBuf::from(&host);
        if p.is_absolute() {
            return p;
        }
    }
    if let Some(home) = real_home_dir() {
        return home.join(".config");
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("/etc/xdg"))
}

/// Profile dirs holding a Chromium `Cookies` database, best first: `Default`, a
/// top-level `Cookies` file, then `Profile *`.
fn chromium_profile_dirs(config: &Path, subdir: &str) -> Vec<PathBuf> {
    let base = config.join(subdir);
    let mut out = vec![base.join("Default"), base.clone()];
    if let Ok(entries) = std::fs::read_dir(&base) {
        let mut rest: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_dir()
                    && p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("Profile "))
            })
            .collect();
        rest.sort();
        out.extend(rest);
    }
    out.into_iter()
        .filter(|p| p.join("Cookies").is_file())
        .collect()
}

/// Firefox profile dirs under one base dir, default first. Parses every
/// `[Profile*]` section of `profiles.ini`; falls back to a dir scan without it.
fn firefox_profile_dirs(base: &Path) -> Vec<PathBuf> {
    if let Ok(text) = std::fs::read_to_string(base.join("profiles.ini")) {
        let mut ranked: Vec<(PathBuf, bool)> = Vec::new();
        let mut path: Option<String> = None;
        let mut relative = true;
        let mut is_default = false;
        let mut flush = |path: &mut Option<String>, relative: &mut bool, is_default: &mut bool| {
            if let Some(p) = path.take() {
                let dir = if *relative && !std::path::Path::new(&p).is_absolute() {
                    base.join(&p)
                } else {
                    PathBuf::from(&p)
                };
                ranked.push((dir, *is_default));
            }
            *relative = true;
            *is_default = false;
        };
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                flush(&mut path, &mut relative, &mut is_default);
            } else if let Some((key, value)) = line.split_once('=') {
                match key.trim() {
                    "Path" => path = Some(value.trim().to_string()),
                    "IsRelative" => relative = value.trim() != "0",
                    "Default" => is_default = value.trim() == "1",
                    _ => {}
                }
            }
        }
        flush(&mut path, &mut relative, &mut is_default);
        ranked.sort_by_key(|(_, d)| !d);
        return ranked
            .into_iter()
            .map(|(p, _)| p)
            .filter(|p| p.join("cookies.sqlite").is_file())
            .collect();
    }
    if let Ok(entries) = std::fs::read_dir(base) {
        let mut names: Vec<String> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".default-release") || n.ends_with(".default"))
            .collect();
        names.sort();
        names
            .into_iter()
            .map(|n| base.join(n))
            .filter(|p| p.join("cookies.sqlite").is_file())
            .collect()
    } else {
        Vec::new()
    }
}

/// Chromium config subdirs per browser, most common first; one entry covers the
/// whole family (stable, beta, nightly, dev). Across channels the freshest
/// `Cookies` database wins — see [`freshest_chromium_profile`].
pub(crate) fn chromium_subdirs(browser: &str) -> &'static [&'static str] {
    match browser {
        "brave" => &[
            "BraveSoftware/Brave-Browser",
            "BraveSoftware/Brave-Browser-Beta",
            "BraveSoftware/Brave-Browser-Nightly",
            // Rebranded builds seen in the wild; add forks only with a reported real path.
            "BraveSoftware/Brave-Origin-Beta",
            "BraveSoftware/Brave-Origin-Nightly",
            "BraveSoftware/Brave-Browser-Origin-Nightly",
        ],
        "chrome" => &[
            "google-chrome",
            "google-chrome-beta",
            "google-chrome-unstable",
        ],
        "chromium" => &["chromium", "chromium-beta"],
        "edge" => &[
            "microsoft-edge",
            "microsoft-edge-beta",
            "microsoft-edge-dev",
            // Not shipped on Linux today; harmless if absent.
            "microsoft-edge-canary",
        ],
        "opera" => &["opera", "opera-beta", "opera-developer"],
        "vivaldi" => &["vivaldi", "vivaldi-snapshot"],
        "whale" => &["naver-whale"],
        _ => &[],
    }
}

/// Best profile dir across every channel subdir: each channel contributes its
/// preferred profile (`Default` first), then the freshest `Cookies` mtime wins —
/// a stale install that merely exists (e.g. Brave stable shadowing Origin Beta)
/// must not win. Unreadable mtimes sort last, so the outcome stays deterministic.
fn freshest_chromium_profile(config_home: &Path, browser: &str) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = chromium_subdirs(browser)
        .iter()
        .filter_map(|sub| chromium_profile_dirs(config_home, sub).into_iter().next())
        .collect();
    candidates.sort_by(|a, b| {
        let mtime = |profile: &PathBuf| {
            std::fs::metadata(profile.join("Cookies"))
                .and_then(|meta| meta.modified())
                .ok()
        };
        // Descending; `Option` orders `None` last.
        mtime(b).cmp(&mtime(a))
    });
    candidates.into_iter().next()
}

/// Absolute browser profile dir for `--cookies-from-browser`, resolved against the
/// real host dirs (not the sandbox `$HOME`). `config_home` stands in for
/// [`real_config_home`], `home` for the passwd home (snap Firefox lives there).
pub(crate) fn browser_profile_dir_in(
    config_home: &Path,
    home: &Path,
    browser: &str,
) -> Option<PathBuf> {
    if browser == "firefox" {
        return [
            home.join(".mozilla/firefox"),
            config_home.join("mozilla/firefox"),
            home.join("snap/firefox/common/.mozilla/firefox"),
        ]
        .into_iter()
        .find_map(|base| firefox_profile_dirs(&base).into_iter().next());
    }
    // Zen is Firefox-based (same profiles.ini layout) but keeps its profiles
    // under ~/.zen.
    if browser == "zen" {
        return [home.join(".zen"), config_home.join("zen")]
            .into_iter()
            .find_map(|base| firefox_profile_dirs(&base).into_iter().next());
    }
    freshest_chromium_profile(config_home, browser)
}

/// [`browser_profile_dir_in`] against the real host dirs: when
/// `HOST_XDG_CONFIG_HOME` is set, home is its parent (`<home>/.config`).
pub(crate) fn browser_profile_dir(browser: &str) -> Option<PathBuf> {
    let config = real_config_home();
    let home = std::env::var_os("HOST_XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .and_then(|p| p.parent().map(PathBuf::from))
        .or_else(real_home_dir)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/"));
    let found = browser_profile_dir_in(&config, &home, browser);
    tracing::debug!(browser, config = %config.display(), found = ?found, "browser profile lookup");
    found
}

/// Browsers offered for `--cookies-from-browser`, in combo order (yt-dlp names).
pub const COOKIES_BROWSERS: &[&str] = &[
    "none", "brave", "chrome", "chromium", "edge", "firefox", "opera", "vivaldi", "whale", "zen",
];

/// Profile *roots* (not the profile dir itself) a browser's cookie database can
/// live under, in `~`-style Flatpak `--filesystem` form. The manifest grants no
/// browser access, so the preferences UI offers these as a `flatpak override`
/// command when the profile is unreachable in the sandbox. One entry per
/// channel subdir the lookup probes — a stable-only command would strand beta
/// users with a silently failing lookup.
pub(crate) fn browser_override_dirs(browser: &str) -> Vec<String> {
    if browser == "firefox" {
        return [
            "~/.mozilla/firefox",
            "~/.config/mozilla/firefox",
            "~/snap/firefox/common/.mozilla/firefox",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
    }
    if browser == "zen" {
        return ["~/.zen", "~/.config/zen"]
            .into_iter()
            .map(str::to_owned)
            .collect();
    }
    chromium_subdirs(browser)
        .iter()
        .map(|sub| format!("~/.config/{sub}"))
        .collect()
}

/// `flatpak override` command granting the sandbox read access to every root
/// [`browser_override_dirs`] lists. Pure and testable; `None` for
/// unknown/off browsers.
pub(crate) fn browser_override_command_for(browser: &str, app_id: &str) -> Option<String> {
    let dirs = browser_override_dirs(browser);
    if dirs.is_empty() {
        return None;
    }
    let mut cmd = String::from("flatpak override --user");
    for dir in &dirs {
        cmd.push_str(&format!(" --filesystem={dir}:ro"));
    }
    cmd.push(' ');
    cmd.push_str(app_id);
    Some(cmd)
}

/// [`browser_override_command_for`] with our own Flatpak app id. `None`
/// outside the sandbox (`FLATPAK_ID` is always set inside it) — without the id
/// the command would be wrong, so show nothing instead.
pub(crate) fn browser_override_command(browser: &str) -> Option<String> {
    let app_id = std::env::var("FLATPAK_ID").ok()?;
    browser_override_command_for(browser, &app_id)
}

/// Spec for `--cookies-from-browser`: `browser:/absolute/profile/dir` when the
/// profile resolves, else the bare name so yt-dlp falls back to its own
/// `$HOME`-relative lookup (correct outside Flatpak). `None`/unknown = off. The
/// path must be the profile *directory* — yt-dlp opens and decrypts the cookie
/// database itself; a raw `Cookies` file is not a `--cookies` export.
pub(crate) fn cookies_browser_spec(value: &str) -> Option<String> {
    if value.is_empty() || value == "none" || !COOKIES_BROWSERS.contains(&value) {
        return None;
    }
    // yt-dlp has no "zen" browser; Zen is Firefox-based, so its resolved
    // profile goes to the Firefox extractor.
    let ytdlp_browser = if value == "zen" { "firefox" } else { value };
    if let Some(dir) = browser_profile_dir(value) {
        return Some(format!("{ytdlp_browser}:{}", dir.display()));
    }
    Some(ytdlp_browser.to_string())
}

/// Validate one extractor-specific KEY:ARGS entry. It is never shell-expanded,
/// but control characters, malformed keys and oversized values are rejected.
pub(crate) fn validated_ytdlp_extractor_args(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
        return None;
    }
    let (key, args) = value.split_once(':')?;
    if key.is_empty()
        || !key.as_bytes()[0].is_ascii_alphanumeric()
        || !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        || args.trim().is_empty()
    {
        return None;
    }
    Some(value.to_string())
}

/// Explicit Deno runtime flags used for YouTube extraction.
/// No other JavaScript runtime is enabled as a fallback.
fn ytdlp_runtime_args(page_url: &str, deno_path: Option<&Path>) -> Vec<String> {
    if !is_youtube_url(page_url) {
        return Vec::new();
    }
    let runtime = deno_path
        .map(|path| format!("deno:{}", path.display()))
        .unwrap_or_else(|| "deno".to_string());
    vec![
        "--no-js-runtimes".to_string(),
        "--js-runtimes".to_string(),
        runtime,
    ]
}

/// Shared trailing argv for every yt-dlp spawn: built-in/custom extractor args,
/// JS runtime pin for YouTube, cookies, UA, then the page URL behind `--`.
pub(crate) fn ytdlp_identity_args(
    cookies_browser: &str,
    user_agent: Option<&str>,
    page_url: &str,
) -> Vec<String> {
    let mut args = Vec::new();
    // YouTube force-enables SABR-only streaming for the `web` player client
    // (yt-dlp#12482): its URL-less formats fail the whole extraction. `web`
    // only enters yt-dlp's default rotation when a JS runtime is available
    // (e.g. Deno on PATH), so exclude it everywhere. Scoped to the
    // youtube extractor: a no-op for other sites.
    let extra_args =
        validated_ytdlp_extractor_args(&crate::settings::AppSettings::new().ytdlp_extractor_args());
    match extra_args {
        Some(extra_args) => {
            let (extractor, settings) = extra_args.split_once(':').expect("validated KEY:ARGS");
            if extractor == "youtube" {
                let overrides_player_client = settings
                    .split(';')
                    .filter_map(|entry| entry.split_once('='))
                    .any(|(key, _)| key.trim() == "player_client");
                let merged = if overrides_player_client {
                    extra_args
                } else {
                    format!("youtube:player_client=default,-web;{settings}")
                };
                args.push("--extractor-args".to_string());
                args.push(merged);
            } else {
                args.push("--extractor-args".to_string());
                args.push("youtube:player_client=default,-web".to_string());
                args.push("--extractor-args".to_string());
                args.push(extra_args);
            }
        }
        None => {
            args.push("--extractor-args".to_string());
            args.push("youtube:player_client=default,-web".to_string());
        }
    }
    args.extend(ytdlp_runtime_args(page_url, find_deno().as_deref()));
    if let Some(spec) = cookies_browser_spec(cookies_browser) {
        args.push(format!("--cookies-from-browser={spec}"));
    }
    if let Some(ua) = user_agent.map(str::trim).filter(|s| !s.is_empty()) {
        args.push("--user-agent".to_string());
        args.push(ua.to_string());
    }
    // `--` before the page URL: option parsing ends here, so a hostile URL
    // can never be read as a flag.
    args.push("--".to_string());
    args.push(page_url.to_string());
    args
}

#[cfg(test)]
mod extractor_args_compat_tests {
    use super::{validated_ytdlp_extractor_args, ytdlp_runtime_args};
    use std::path::Path;

    #[test]
    fn accepts_site_specific_values() {
        assert_eq!(
            validated_ytdlp_extractor_args(" youtube:player_client=android ").as_deref(),
            Some("youtube:player_client=android")
        );
        assert!(validated_ytdlp_extractor_args("instagram:api_version=sample").is_some());
    }

    #[test]
    fn rejects_malformed_control_chars_and_oversized_values() {
        assert!(validated_ytdlp_extractor_args("").is_none());
        assert!(validated_ytdlp_extractor_args("--bad:flag").is_none());
        assert!(validated_ytdlp_extractor_args("youtube:\nplayer_client=android").is_none());
        assert!(validated_ytdlp_extractor_args(&"x".repeat(4097)).is_none());
    }

    #[test]
    fn youtube_runtime_args_pin_deno_without_a_quickjs_fallback() {
        let with_path = ytdlp_runtime_args(
            "https://youtu.be/example",
            Some(Path::new("/grab/libs/deno")),
        );
        assert_eq!(
            with_path.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["--no-js-runtimes", "--js-runtimes", "deno:/grab/libs/deno"]
        );
        let on_path = ytdlp_runtime_args("https://youtube.com/watch?v=example", None);
        assert_eq!(
            on_path.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["--no-js-runtimes", "--js-runtimes", "deno"]
        );
        assert!(ytdlp_runtime_args("https://example.org/video", None).is_empty());
    }
}

/// First search dir holding a complete ffmpeg toolchain (`ffmpeg` plus `ffprobe`).
/// yt-dlp resolves both from `--ffmpeg-location` and never falls back to PATH for
/// a missing sibling, so a dir with only `ffmpeg` breaks post-processing with
/// "ffprobe not found".
pub(crate) fn toolchain_dir_in(dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter()
        .find(|d| is_executable(&d.join("ffmpeg")) && is_executable(&d.join("ffprobe")))
        .cloned()
}

/// Directory form of a resolved tool binary for `--ffmpeg-location` (yt-dlp wants
/// the directory). Prefers a dir with both tools, falling back to the binary's
/// own dir when no complete toolchain is on hand.
pub(crate) fn ffmpeg_location_dir(ffmpeg_bin: &Path) -> String {
    toolchain_dir_in(&ffmpeg_search_dirs())
        .or_else(|| ffmpeg_bin.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("/usr/bin"))
        .to_string_lossy()
        .into_owned()
}

/// Latest nightly yt-dlp download URL and its SHA-256 digest.
/// Fail closed: `None` digest means no verified install.
/// The nightly builds publish `yt-dlp_linux` with a `digest: "sha256:…"` field.
pub async fn latest_ytdlp_asset() -> Option<(String, Option<String>)> {
    let handle = crate::runtime::tokio_rt().spawn(async move {
        let fetcher =
            yt_dlp::client::deps::github::GitHubFetcher::new("yt-dlp", "yt-dlp-nightly-builds");
        let release = fetcher.fetch_latest_release(None).await.ok()?;
        let asset = release.assets.iter().find(|a| a.name == "yt-dlp_linux")?;
        let digest = asset
            .digest
            .as_deref()
            .and_then(|d| d.strip_prefix("sha256:"))
            .map(str::to_owned);
        Some((asset.download_url.clone(), digest))
    });
    handle.await.ok().flatten()
}

/// Latest nightly yt-dlp tag without downloading anything: one user-initiated
/// GitHub API call. `None` on any network/API failure — the row then reports the
/// check failed instead of prompting.
pub async fn latest_ytdlp_tag() -> Option<String> {
    let handle = crate::runtime::tokio_rt().spawn(async move {
        let fetcher =
            yt_dlp::client::deps::github::GitHubFetcher::new("yt-dlp", "yt-dlp-nightly-builds");
        fetcher
            .fetch_latest_release(None)
            .await
            .ok()
            .map(|release| release.tag_name)
    });
    handle.await.ok().flatten()
}

/// Whether a failed yt-dlp log shows the extractor breaking, which a newer
/// yt-dlp usually fixes. Removed or private videos and network errors don't match.
pub(crate) fn extractor_failure(log: &str) -> bool {
    ["Unable to extract", "Please report this issue"]
        .iter()
        .any(|marker| log.contains(marker))
}

/// Why an installed tool's version probe did not return a version line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub(crate) enum ToolProbeError {
    #[error("could not start the version-check process")]
    Spawn,
    #[error("version-check process exited unsuccessfully")]
    FailedExit,
    #[error("version-check process returned no output")]
    EmptyOutput,
    #[error("version-check process returned invalid output")]
    InvalidOutput,
    #[error("version check timed out")]
    Timeout,
    #[error("background version check failed")]
    Runtime,
}

/// Upper bound for version probes: long enough for cold Python startup, but
/// prevents a wedged executable from keeping previews/preferences pending forever.
pub(crate) const TOOL_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A child JoinHandle that is aborted if its awaiting future is dropped.
/// Tokio normally detaches a JoinHandle on drop; this wrapper propagates caller
/// cancellation to the task so its `kill_on_drop` subprocess is cleaned up too.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Unpin for AbortOnDrop<T> {}

impl<T> std::future::Future for AbortOnDrop<T> {
    type Output = Result<T, tokio::task::JoinError>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::future::Future::poll(std::pin::Pin::new(&mut self.get_mut().0), cx)
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

type ToolVersionKey = (PathBuf, String);

static TOOL_VERSION_CACHE: OnceLock<ProbeCache<ToolVersionKey, String, ToolProbeError>> =
    OnceLock::new();

pub(crate) async fn probe_first_line(
    binary: &Path,
    version_arg: &str,
) -> Result<String, ToolProbeError> {
    let binary = binary.to_path_buf();
    let version_arg = version_arg.to_owned();
    if tokio::runtime::Handle::try_current().is_ok() {
        cached_tool_first_line(binary, version_arg).await
    } else {
        AbortOnDrop(
            crate::runtime::tokio_rt()
                .spawn(async move { cached_tool_first_line(binary, version_arg).await }),
        )
        .await
        .map_err(|_| ToolProbeError::Runtime)?
    }
}

async fn cached_tool_first_line(
    binary: PathBuf,
    version_arg: String,
) -> Result<String, ToolProbeError> {
    let key = (binary.clone(), version_arg);
    let probe_key = key.clone();
    TOOL_VERSION_CACHE
        .get_or_init(ProbeCache::new)
        .get_or_probe(key, &binary, FAILED_PROBE_CACHE_TTL, || async move {
            run_version_probe_uncached(&probe_key.0, &probe_key.1, TOOL_PROBE_TIMEOUT).await
        })
        .await
}

/// Run a version command. Waiting is asynchronous, but a defensive deadline
/// still distinguishes a slow/hung installed binary from a genuinely missing tool.
async fn run_version_probe_uncached(
    binary: &Path,
    version_arg: &str,
    timeout: std::time::Duration,
) -> Result<String, ToolProbeError> {
    use std::process::Stdio;

    let mut command = tokio::process::Command::new(binary);
    command
        .arg(version_arg)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.as_std_mut().process_group(0);
    }
    let child = command.spawn().map_err(|_| ToolProbeError::Spawn)?;
    let pid = child.id().unwrap_or_default();
    let mut group = ProbeProcessGroup::new(pid);
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            group.disarm();
            output
        }
        Ok(Err(_)) => return Err(ToolProbeError::FailedExit),
        Err(_) => return Err(ToolProbeError::Timeout),
    };
    if !output.status.success() {
        return Err(ToolProbeError::FailedExit);
    }
    let text = String::from_utf8(output.stdout).map_err(|_| ToolProbeError::InvalidOutput)?;
    let first = text
        .lines()
        .next()
        .ok_or(ToolProbeError::EmptyOutput)?
        .trim();
    if first.is_empty() {
        return Err(ToolProbeError::EmptyOutput);
    }
    if binary
        .file_name()
        .is_some_and(|name| name.to_string_lossy() == "deno")
        && let Some(version) = first
            .strip_prefix("deno ")
            .and_then(|rest| rest.split_whitespace().next())
    {
        return Ok(version.to_owned());
    }
    Ok(first.to_owned())
}

/// Display-ready version line for an installed yt-dlp or Deno binary.
/// Deno and yt-dlp versions are normalized to their dotted version tokens.
pub(crate) async fn tool_display_version(
    binary: PathBuf,
    version_arg: &'static str,
) -> Option<String> {
    let line = probe_first_line(&binary, version_arg).await.ok()?;
    if binary
        .file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with("yt-dlp"))
    {
        return Some(format!("yt-dlp {line}"));
    }
    if binary
        .file_name()
        .is_some_and(|n| n.to_string_lossy() == "deno")
    {
        return Some(format!("Deno {line}"));
    }
    Some(line)
}

/// Validate only yt-dlp for metadata lookup. FFmpeg is not involved in the
/// lookup process, so don't spawn its version probe until an actual download
/// attempt needs the full toolchain.
pub(crate) async fn ensure_ytdlp_version(youtube_bin: &Path) -> Result<String, VideoError> {
    let line = probe_first_line(youtube_bin, "--version")
        .await
        .map_err(VideoError::tool_probe_failure)?;
    validate_ytdlp_version(line)
}

fn validate_ytdlp_version(line: String) -> Result<String, VideoError> {
    let fresh = parse_dotted_version(&line).is_some_and(|v| v >= MIN_YTDLP_VERSION);
    if !fresh {
        return Err(VideoError::outdated());
    }
    Ok(line)
}

/// Refuse stale or unverifiable toolchains before any network happens; returns
/// the raw version lines for attempt logging.
pub(crate) async fn ensure_tool_versions(libs: &Libraries) -> Result<(String, String), VideoError> {
    // ffmpeg takes a single-dash -version; --version is an error there.
    let (yt, ff) = tokio::join!(
        probe_first_line(&libs.youtube, "--version"),
        probe_first_line(&libs.ffmpeg, "-version")
    );
    let yt = validate_ytdlp_version(yt.map_err(VideoError::tool_probe_failure)?)?;
    let ff = ff.map_err(VideoError::tool_probe_failure)?;
    // ffmpeg version line looks like "ffmpeg version 7.1.2 ...": take the token
    // right after "version". A find_map over all tokens would match "2000-2025"
    // in git builds ("ffmpeg version N-... Copyright (c) 2000-2025") as 2000.
    let ff_version = ffmpeg_version_token(&ff).and_then(parse_dotted_version);
    let ff_fresh = ff_version.is_some_and(|v| v >= MIN_FFMPEG_VERSION);
    if !ff_fresh {
        return Err(VideoError::outdated());
    }
    Ok((yt, ff))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Unique scratch dir per test (never a shared staging parent).
    fn unique_dir(tag: &str) -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("grab-tools-{tag}-{}-{n}", std::process::id()))
    }

    fn write_test_executable(path: &Path) {
        std::fs::write(path, b"test").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn write_test_script(path: &Path, contents: &str) {
        std::fs::write(path, contents).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[tokio::test]
    async fn async_version_probe_reads_first_line_from_a_successful_tool() {
        let base = unique_dir("version-probe-success");
        std::fs::create_dir_all(&base).unwrap();
        let binary = base.join("yt-dlp");
        write_test_script(
            &binary,
            "#!/bin/sh\nprintf '2026.10.21.233146\\nignored second line\\n'\n",
        );

        assert_eq!(
            probe_first_line(&binary, "--version").await.unwrap(),
            "2026.10.21.233146"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn successful_version_probe_is_cached_until_the_binary_changes() {
        let base = unique_dir("version-probe-cache");
        std::fs::create_dir_all(&base).unwrap();
        let binary = base.join("yt-dlp");
        let counter = base.join("counter");
        write_test_script(
            &binary,
            &format!(
                "#!/bin/sh\nprintf x >> '{}'\nprintf 'cached-one\\n'\n",
                counter.display()
            ),
        );

        assert_eq!(
            probe_first_line(&binary, "--version").await.unwrap(),
            "cached-one"
        );
        assert_eq!(
            probe_first_line(&binary, "--version").await.unwrap(),
            "cached-one"
        );
        assert_eq!(std::fs::read(&counter).unwrap(), b"x");

        write_test_script(
            &binary,
            &format!(
                "#!/bin/sh\nprintf x >> '{}'\nprintf 'cached-two-longer\\n'\n",
                counter.display()
            ),
        );
        assert_eq!(
            probe_first_line(&binary, "--version").await.unwrap(),
            "cached-two-longer"
        );
        assert_eq!(std::fs::read(&counter).unwrap(), b"xx");
        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn async_version_probe_rejects_a_nonzero_exit() {
        let base = unique_dir("version-probe-failure");
        std::fs::create_dir_all(&base).unwrap();
        let binary = base.join("yt-dlp");
        write_test_script(
            &binary,
            "#!/bin/sh\nprintf '2026.10.21.233146\\n'\nexit 1\n",
        );

        assert_eq!(
            probe_first_line(&binary, "--version").await,
            Err(ToolProbeError::FailedExit)
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn async_version_probe_times_out_for_a_hung_tool() {
        let base = unique_dir("version-probe-timeout");
        std::fs::create_dir_all(&base).unwrap();
        let binary = base.join("yt-dlp");
        write_test_script(&binary, "#!/bin/sh\nsleep 5\nprintf 'never-reached\\n'\n");

        let start = std::time::Instant::now();
        let result =
            run_version_probe_uncached(&binary, "--version", std::time::Duration::from_millis(50))
                .await;
        assert_eq!(result, Err(ToolProbeError::Timeout));
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "the probe did not enforce its deadline: {:?}",
            start.elapsed()
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn concurrent_version_probes_share_one_cold_process() {
        let base = unique_dir("version-probe-concurrent");
        std::fs::create_dir_all(&base).unwrap();
        let binary = base.join("yt-dlp");
        let counter = base.join("counter");
        write_test_script(
            &binary,
            &format!(
                "#!/bin/sh\nprintf x >> '{}'\nsleep 0.1\nprintf 'shared\\n'\n",
                counter.display()
            ),
        );

        let (first, second) = tokio::join!(
            probe_first_line(&binary, "--version"),
            probe_first_line(&binary, "--version")
        );
        assert_eq!(first.unwrap(), "shared");
        assert_eq!(second.unwrap(), "shared");
        assert_eq!(std::fs::read(&counter).unwrap(), b"x");
        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn concurrent_failed_version_probes_share_a_short_negative_cache() {
        let base = unique_dir("version-probe-failure-cache");
        std::fs::create_dir_all(&base).unwrap();
        let binary = base.join("yt-dlp");
        let counter = base.join("counter");
        write_test_script(
            &binary,
            &format!(
                "#!/bin/sh\nprintf x >> '{}'\nsleep 0.1\nexit 1\n",
                counter.display()
            ),
        );

        let (first, second) = tokio::join!(
            probe_first_line(&binary, "--version"),
            probe_first_line(&binary, "--version")
        );
        assert_eq!(first, Err(ToolProbeError::FailedExit));
        assert_eq!(second, Err(ToolProbeError::FailedExit));
        assert_eq!(
            probe_first_line(&binary, "--version").await,
            Err(ToolProbeError::FailedExit)
        );
        assert_eq!(std::fs::read(&counter).unwrap(), b"x");

        tokio::time::sleep(FAILED_PROBE_CACHE_TTL + std::time::Duration::from_millis(50)).await;
        assert_eq!(
            probe_first_line(&binary, "--version").await,
            Err(ToolProbeError::FailedExit)
        );
        assert_eq!(std::fs::read(&counter).unwrap(), b"xx");
        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn impersonation_probe_caches_unsupported_but_retries_failures() {
        let base = unique_dir("impersonation-failure-cache");
        std::fs::create_dir_all(&base).unwrap();

        let unsupported = base.join("yt-dlp-unsupported");
        let unsupported_count = base.join("unsupported-count");
        write_test_script(
            &unsupported,
            &format!(
                "#!/bin/sh\nprintf x >> '{}'\nprintf 'Chrome (unavailable)\\n'\n",
                unsupported_count.display()
            ),
        );
        assert!(!ytdlp_supports_impersonation(&unsupported).await);
        assert!(!ytdlp_supports_impersonation(&unsupported).await);
        assert_eq!(std::fs::read(&unsupported_count).unwrap(), b"x");

        let failed = base.join("yt-dlp-failed");
        let failed_count = base.join("failed-count");
        write_test_script(
            &failed,
            &format!(
                "#!/bin/sh\nprintf x >> '{}'\nsleep 0.1\nexit 1\n",
                failed_count.display()
            ),
        );
        assert!(!ytdlp_supports_impersonation(&failed).await);
        assert!(!ytdlp_supports_impersonation(&failed).await);
        assert_eq!(std::fs::read(&failed_count).unwrap(), b"x");

        tokio::time::sleep(FAILED_PROBE_CACHE_TTL + std::time::Duration::from_millis(50)).await;
        assert!(!ytdlp_supports_impersonation(&failed).await);
        assert_eq!(std::fs::read(&failed_count).unwrap(), b"xx");
        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn failed_hls_probe_is_shared_briefly_then_retried() {
        let base = unique_dir("hls-failure-cache");
        std::fs::create_dir_all(&base).unwrap();
        let binary = base.join("ffmpeg");
        let counter = base.join("counter");
        write_test_script(
            &binary,
            &format!(
                "#!/bin/sh\nprintf x >> '{}'\nsleep 0.1\nexit 1\n",
                counter.display()
            ),
        );

        assert!(!ffmpeg_supports_hls_extension_picky(&binary).await);
        assert!(!ffmpeg_supports_hls_extension_picky(&binary).await);
        assert_eq!(std::fs::read(&counter).unwrap(), b"x");

        tokio::time::sleep(FAILED_PROBE_CACHE_TTL + std::time::Duration::from_millis(50)).await;
        assert!(!ffmpeg_supports_hls_extension_picky(&binary).await);
        assert_eq!(std::fs::read(&counter).unwrap(), b"xx");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn core_tools_availability_ignores_legacy_user_ffmpeg() {
        let base = unique_dir("system-core-tools");
        let user = base.join("user");
        let system = base.join("system");
        for dir in [&user, &system] {
            std::fs::create_dir_all(dir).unwrap();
        }
        write_test_executable(&user.join("yt-dlp"));
        write_test_executable(&user.join("ffmpeg"));
        write_test_executable(&user.join("ffprobe"));

        // A legacy private FFmpeg copy is not enough: the resolver uses the
        // system search paths now that Grab no longer installs FFmpeg.
        assert!(!core_tools_available_in(
            std::slice::from_ref(&user),
            std::slice::from_ref(&system),
        ));
        write_test_executable(&system.join("ffmpeg"));
        write_test_executable(&system.join("ffprobe"));
        assert!(core_tools_available_in(
            std::slice::from_ref(&user),
            std::slice::from_ref(&system),
        ));

        std::fs::remove_file(system.join("ffprobe")).unwrap();
        assert!(!core_tools_available_in(
            std::slice::from_ref(&user),
            std::slice::from_ref(&system),
        ));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn flatpak_ffmpeg_search_ignores_user_copy() {
        let base = unique_dir("flatpak-ffmpeg-dirs");
        let app = base.join("app");
        let runtime = base.join("runtime");
        let user = base.join("user");
        let system = base.join("system");
        for dir in [&app, &runtime, &user, &system] {
            std::fs::create_dir_all(dir).unwrap();
            write_test_executable(&dir.join("ffmpeg"));
            write_test_executable(&dir.join("ffprobe"));
        }

        // Flatpak prefers /app/bin, then the runtime, and never the user dir.
        let dirs = ffmpeg_search_dirs_in(true, &app, &runtime, || vec![system.clone()]);
        assert_eq!(dirs, vec![app.clone(), runtime.clone()]);
        assert_eq!(find_in_dirs("ffmpeg", &dirs), Some(app.join("ffmpeg")));
        assert!(!dirs.contains(&user));

        // A partial /app/bin toolchain must not shadow the complete runtime pair.
        std::fs::remove_file(app.join("ffprobe")).unwrap();
        assert_eq!(find_in_dirs("ffmpeg", &dirs), Some(app.join("ffmpeg")));
        assert_eq!(toolchain_dir_in(&dirs), Some(runtime.clone()));

        // Non-Flatpak resolution falls back to system paths, not the private
        // tool directory where FFmpeg downloads from old releases may remain.
        let dirs = ffmpeg_search_dirs_in(false, &app, &runtime, || vec![system.clone()]);
        assert_eq!(dirs, vec![system.clone()]);
        assert_eq!(find_in_dirs("ffmpeg", &dirs), Some(system.join("ffmpeg")));
        assert!(!dirs.contains(&user));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn flatpak_core_tools_use_runtime_ffmpeg_not_user_copy() {
        let base = unique_dir("flatpak-core-tools");
        let user = base.join("user");
        let app = base.join("app");
        let runtime = base.join("runtime");
        for dir in [&user, &app, &runtime] {
            std::fs::create_dir_all(dir).unwrap();
        }
        write_test_executable(&user.join("yt-dlp"));
        write_test_executable(&user.join("ffmpeg"));
        write_test_executable(&user.join("ffprobe"));
        assert!(!flatpak_core_tools_installed_in(&user, &app, &runtime));

        write_test_executable(&runtime.join("ffmpeg"));
        write_test_executable(&runtime.join("ffprobe"));
        assert!(flatpak_core_tools_installed_in(&user, &app, &runtime));

        std::fs::remove_file(user.join("yt-dlp")).unwrap();
        assert!(!flatpak_core_tools_installed_in(&user, &app, &runtime));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn distro_packages_include_deno_for_supported_distros() {
        let fedora = distro_packages("ID=fedora\nNAME=\"Fedora Linux\"\n").unwrap();
        assert_eq!(fedora.install_all, "sudo dnf install yt-dlp ffmpeg deno");
        let void = distro_packages("ID=void\nNAME=\"Void\"\n").unwrap();
        assert_eq!(void.install_all, "sudo xbps-install -S yt-dlp ffmpeg deno");
        let derivative =
            distro_packages("ID=neon\nID_LIKE=\"ubuntu debian\"\nNAME=\"KDE neon\"\n").unwrap();
        assert_eq!(
            derivative.install_all,
            "sudo apt install yt-dlp ffmpeg deno"
        );
    }

    #[test]
    fn clear_dest_refusing_symlink_rejects_link() {
        let dir = unique_dir("symlink");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target");
        std::fs::write(&target, b"precious").unwrap();
        let link = dir.join("deno.part");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err = clear_dest_refusing_symlink(&link).unwrap_err();
        assert!(err.contains("symlink"), "unexpected error: {err}");
        // Untouched: the link still points at the intact target.
        assert!(link.is_symlink());
        assert_eq!(std::fs::read(&target).unwrap(), b"precious");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn clear_dest_refusing_symlink_clears_regular_file() {
        let dir = unique_dir("regular");
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("deno.part");
        std::fs::write(&part, b"stale").unwrap();

        clear_dest_refusing_symlink(&part).unwrap();
        assert!(!part.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn clear_dest_refusing_symlink_missing_is_fine() {
        let dir = unique_dir("missing");
        std::fs::create_dir_all(&dir).unwrap();

        clear_dest_refusing_symlink(&dir.join("deno.part")).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn download_to_file_refuses_symlink_before_network() {
        let dir = unique_dir("dl-symlink");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target");
        std::fs::write(&target, b"precious").unwrap();
        let link = dir.join("deno.part");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        // Unroutable URL: the symlink guard must fire before any I/O.
        let err = download_to_file("http://127.0.0.1:1/nope", &link, "dummy", 8 * 1024 * 1024)
            .await
            .unwrap_err();
        assert!(err.contains("symlink"), "unexpected error: {err}");
        assert_eq!(std::fs::read(&target).unwrap(), b"precious");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn valid_release_tag_rejects_evil_tags() {
        for good in ["v0.17.0", "n9.0.2", "2026.08.19", "v9.0.2-static", "a+b_c"] {
            assert!(valid_release_tag(good), "rejected good tag: {good}");
        }
        for evil in [
            "",
            ".",
            "..",
            ".hidden",
            "v0.17.0;touch pwned",
            "../../etc/passwd",
            "v0.17.0\n",
            "tag with spaces",
            "https://evil.example/x",
            "v0.17.0$HOME",
        ] {
            assert!(!valid_release_tag(evil), "accepted evil tag: {evil:?}");
        }
        // The tag is interpolated into the download URL, so whatever passes
        // here must survive URL interpolation unchanged.
        let tag = "v2.9.8";
        assert!(valid_release_tag(tag));
        let url = deno_download_url_for_arch(tag, "x86_64").unwrap();
        assert!(
            url.ends_with("/releases/download/v2.9.8/deno-x86_64-unknown-linux-gnu.zip")
                || url.ends_with("/releases/download/v2.9.8/deno-aarch64-unknown-linux-gnu.zip")
        );
    }

    #[tokio::test]
    async fn write_capped_stream_enforces_size_cap() {
        // More than the cap: the write must fail instead of buffering it.
        // The stream is synthetic — no network round-trip — so the cap is
        // pinned deterministically.
        let chunks =
            futures_util::stream::iter((0..129).map(|_| Ok::<Vec<u8>, String>(vec![0u8; 65536])));
        let dir = unique_dir("cap-stream");
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("deno.part");

        let err = write_capped_stream(
            Box::pin(chunks),
            &dest,
            "http://127.0.0.1/deno",
            "dummy",
            8 * 1024 * 1024,
        )
        .await
        .unwrap_err();
        assert!(
            err.contains("exceeds the download size limit"),
            "unexpected error: {err}"
        );
        // The over-limit chunk is rejected before it is written: the partial
        // never grows past the cap.
        let written = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
        assert!(written <= 8 * 1024 * 1024);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn write_capped_stream_writes_small_bodies() {
        // Under the cap everything lands on disk byte-for-byte.
        let chunks = futures_util::stream::iter([
            Ok::<Vec<u8>, String>(b"hello ".to_vec()),
            Ok::<Vec<u8>, String>(b"world".to_vec()),
        ]);
        let dir = unique_dir("cap-stream-ok");
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("deno.part");

        write_capped_stream(
            Box::pin(chunks),
            &dest,
            "http://127.0.0.1/deno",
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9",
            8 * 1024 * 1024,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello world");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn write_capped_stream_rejects_hash_mismatch() {
        // Wrong digest: the write must fail and the .part must not be usable.
        let chunks = futures_util::stream::iter([
            Ok::<Vec<u8>, String>(b"hello ".to_vec()),
            Ok::<Vec<u8>, String>(b"world".to_vec()),
        ]);
        let dir = unique_dir("cap-stream-badhash");
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("deno.part");

        let err = write_capped_stream(
            Box::pin(chunks),
            &dest,
            "http://127.0.0.1/deno",
            "0000000000000000000000000000000000000000000000000000000000000000",
            8 * 1024 * 1024,
        )
        .await
        .unwrap_err();
        assert!(err.contains("sha256 mismatch"), "unexpected error: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod deno_release_tests {
    use super::*;

    fn unique_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "grab-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn write_executable_test_script(path: &Path, contents: &str) {
        std::fs::write(path, contents).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[tokio::test]
    async fn deno_candidate_selection_skips_a_stale_preferred_copy() {
        let base = unique_dir("deno-candidates");
        let managed_dir = base.join("managed");
        let path_dir = base.join("path");
        std::fs::create_dir_all(&managed_dir).unwrap();
        std::fs::create_dir_all(&path_dir).unwrap();
        let stale = managed_dir.join("deno");
        let compatible = path_dir.join("deno");
        write_executable_test_script(&stale, "#!/bin/sh\nprintf '2.2.9\\n'\n");
        write_executable_test_script(&compatible, "#!/bin/sh\nprintf '2.3.0\\n'\n");

        assert_eq!(
            first_supported_deno(&[stale, compatible.clone()]).await,
            Ok(Some(compatible))
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn deno_candidate_selection_preserves_probe_errors() {
        let base = unique_dir("deno-probe-error");
        std::fs::create_dir_all(&base).unwrap();
        let failing = base.join("deno");
        write_executable_test_script(&failing, "#!/bin/sh\\nexit 1\\n");

        assert_eq!(
            first_supported_deno(std::slice::from_ref(&failing)).await,
            Err(ToolProbeError::FailedExit)
        );

        // An old candidate is not sufficient evidence to replace Deno when
        // another candidate could not be checked.
        let old = base.join("old-deno");
        write_executable_test_script(&old, "#!/bin/sh\\nprintf '2.2.9\\n'\\n");
        assert_eq!(
            first_supported_deno(&[old, failing]).await,
            Err(ToolProbeError::FailedExit)
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn legacy_qjs_cleanup_removes_only_regular_managed_file() {
        let base = unique_dir("legacy-qjs");
        std::fs::create_dir_all(&base).unwrap();
        let qjs = base.join("qjs");
        std::fs::write(&qjs, b"old managed QuickJS").unwrap();
        remove_legacy_qjs_from(&base).await;
        assert!(!qjs.exists());

        let target = base.join("user-target");
        std::fs::write(&target, b"keep").unwrap();
        std::os::unix::fs::symlink(&target, &qjs).unwrap();
        remove_legacy_qjs_from(&base).await;
        assert!(
            std::fs::symlink_metadata(&qjs)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn release_tag_and_asset_url_are_validated() {
        for good in ["v2.9.8", "v0.17.0", "2026.08.19", "v9.0.2-static", "a+b_c"] {
            assert!(valid_release_tag(good), "rejected good tag: {good}");
        }
        for evil in [
            "",
            ".",
            "..",
            ".hidden",
            "v2.9.8;touch pwned",
            "../../etc/passwd",
            "v2.9.8\n",
            "tag with spaces",
            "https://evil.example/x",
            "v2.9.8$HOME",
        ] {
            assert!(!valid_release_tag(evil), "accepted evil tag: {evil:?}");
        }
        assert_eq!(
            deno_download_url_for_arch("v2.9.8", "x86_64").as_deref(),
            Some(
                "https://github.com/denoland/deno/releases/download/v2.9.8/deno-x86_64-unknown-linux-gnu.zip"
            )
        );
        assert_eq!(
            deno_download_url_for_arch("v2.9.8", "aarch64").as_deref(),
            Some(
                "https://github.com/denoland/deno/releases/download/v2.9.8/deno-aarch64-unknown-linux-gnu.zip"
            )
        );
        assert!(deno_download_url_for_arch("v2.9.8", "riscv64").is_none());
        assert!(deno_download_url_for_arch("../main", "x86_64").is_none());
    }

    #[test]
    fn release_metadata_requires_expected_asset_and_sha256() {
        let digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let body = format!(
            r#"{{"tag_name":"v2.9.8","assets":[{{"name":"deno-x86_64-unknown-linux-gnu.zip","browser_download_url":"https://github.com/denoland/deno/releases/download/v2.9.8/deno-x86_64-unknown-linux-gnu.zip","digest":"sha256:{digest}"}}]}}"#
        );
        let release = parse_deno_release_metadata(&body, "x86_64").unwrap();
        assert_eq!(release.tag, "v2.9.8");
        assert_eq!(release.sha256.as_deref(), Some(digest));
        let bad_url = body.replace(
            "https://github.com/denoland/deno/releases/download/v2.9.8/deno-x86_64-unknown-linux-gnu.zip",
            "https://attacker.example/deno.zip",
        );
        assert!(parse_deno_release_metadata(&bad_url, "x86_64").is_none());
        let bad_digest = body.replace(digest, "abc123");
        assert!(
            parse_deno_release_metadata(&bad_digest, "x86_64")
                .unwrap()
                .sha256
                .is_none()
        );
        assert!(parse_deno_release_metadata(&body, "unsupported").is_none());
    }

    #[test]
    fn archive_extraction_only_accepts_the_expected_binary() {
        use std::io::Write as _;
        let dir = unique_dir("deno-archive");
        std::fs::create_dir_all(&dir).unwrap();
        let archive_path = dir.join("deno.zip");
        let extracted = dir.join("deno.part");
        {
            let file = std::fs::File::create(&archive_path).unwrap();
            let mut archive = zip::ZipWriter::new(file);
            archive
                .start_file(
                    "deno",
                    zip::write::SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Stored),
                )
                .unwrap();
            archive.write_all(b"expected executable bytes").unwrap();
            archive.finish().unwrap();
        }
        extract_deno_archive(&archive_path, &extracted).unwrap();
        assert_eq!(
            std::fs::read(&extracted).unwrap(),
            b"expected executable bytes"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(all(test, unix))]
mod lib_dir_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn ensure_lib_dir_refuses_planted_fallback_root() {
        let root =
            std::env::temp_dir().join(format!("grab-fallback-data-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        // Looks like an attacker pre-created the predictable per-PID root.
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
        let dir = root.join("grab").join("libs");
        let err = ensure_lib_dir_blocking(&dir).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(!dir.exists(), "nothing is created inside a refused root");
        // A private root is accepted and the tree is created 0700.
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        ensure_lib_dir_blocking(&dir).unwrap();
        assert!(dir.is_dir());
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o077,
            0
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod update_hint_tests {
    use super::*;

    #[test]
    fn extractor_failure_matches_breakage_only() {
        assert!(extractor_failure("ERROR: Unable to extract data"));
        assert!(extractor_failure("ERROR: Please report this issue"));
        assert!(!extractor_failure("ERROR: Video unavailable"));
        assert!(!extractor_failure("ERROR: Private video"));
        assert!(!extractor_failure("ERROR: unable to download webpage"));
    }
}
