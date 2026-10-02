//! Tool provisioning: finding, vetting, installing and feeding yt-dlp/ffmpeg to
//! every spawn. Leaf module: the dialog, prefs and engines consume it through the
//! `video` facade.

use gettextrs::gettext;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use thiserror::Error;
use yt_dlp::client::deps::{Libraries, LibraryInstaller};
use yt_dlp::model::DrmStatus;
use yt_dlp::model::format::{Format, FormatType, Protocol};

/// Errors surfaced by the video pipeline. User-facing strings are translated at
/// construction; match on the variant to branch the UI (install banner vs retry).
#[derive(Debug, Error)]
pub enum VideoError {
    /// Neither the Flatpak bundle, the user library dir nor PATH has the tools.
    #[error("{0}")]
    MissingLibraries(String),
    /// The page could not be extracted (wrong URL, offline, …). Retryable.
    #[error("{0}")]
    Fetch(String),
    /// Background task machinery failed (panic/join) — internal.
    #[error("{0}")]
    Runtime(String),
    /// Other translated failure.
    #[error("{0}")]
    Message(String),
}

impl VideoError {
    pub(crate) fn missing_tools() -> Self {
        Self::MissingLibraries(gettext("Media downloads need the yt-dlp support tools"))
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
            Self::Runtime(s) => Self::Runtime(format!("{s}{suffix}")),
            Self::Message(s) => Self::Message(format!("{s}{suffix}")),
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
    /// One-line install command for all three tools, e.g.
    /// "sudo dnf install yt-dlp ffmpeg quickjs-ng". quickjs-ng is appended
    /// only where a distro package is known — not on openSUSE, Void or
    /// Solus, where the command would fail.
    pub install_all: String,
    /// Whether the distro has a known quickjs package (i.e. `install_all`
    /// covers quickjs). Where false, the install-help dialog must also show
    /// the upstream quickjs release link, or those distros dead-end.
    pub has_quickjs_package: bool,
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

/// Distro package name for the quickjs-ng JS runtime. Verified against the
/// distro package trackers: Fedora, Debian/Ubuntu, Arch, Alpine and Gentoo
/// all ship `quickjs-ng`; openSUSE, Void and Solus do not, so those resolve
/// to `None` and the install-help row is hidden there.
fn quickjs_package(id: &str) -> Option<&'static str> {
    match id {
        "fedora" | "rhel" | "centos" | "almalinux" | "rocky" => Some("quickjs-ng"),
        "ubuntu" | "debian" | "pop" | "linuxmint" | "elementary" | "zorin" => Some("quickjs-ng"),
        "arch" | "manjaro" | "endeavouros" | "cachyos" => Some("quickjs-ng"),
        "alpine" => Some("quickjs-ng"),
        "gentoo" => Some("quickjs-ng"),
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
    let quickjs_pkg =
        quickjs_package(id).or_else(|| id_like.split_whitespace().find_map(quickjs_package));
    let quickjs = quickjs_pkg.map(|pkg| format!(" {pkg}")).unwrap_or_default();
    Some(DistroPackages {
        distro: name.unwrap_or(id).to_string(),
        install_all: format!("{pm} yt-dlp ffmpeg{quickjs}"),
        has_quickjs_package: quickjs_pkg.is_some(),
    })
}

/// Where on-demand tool installs keep the yt-dlp/ffmpeg binaries:
/// `$XDG_DATA_HOME/grab/libs`. Both Flatpak and tarball/dev builds fetch here;
/// `/app/bin` and PATH remain fallbacks for system-provided copies.
pub fn user_lib_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| std::env::temp_dir().join("grab-fallback-data"));
    base.join("grab").join("libs")
}

/// Bundled-tool dir inside the Flatpak sandbox — Flatpak mounts the app tree at
/// `/app`; this is not an XDG rule. Absent outside Flatpak.
const FLATPAK_APP_BIN: &str = "/app/bin";

/// Tool search dirs, in priority order: the user's own installs first (so Update
/// takes effect over a bundled copy), then `/app/bin` (Flatpak), then PATH. A
/// stale user copy cannot pin old tools — the version floor refuses it.
fn tool_search_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![user_lib_dir(), PathBuf::from(FLATPAK_APP_BIN)];
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path).filter(|d| !d.as_os_str().is_empty()));
    }
    dirs
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

/// Locate the yt-dlp and ffmpeg binaries, or say why they are missing.
pub fn resolve_libraries() -> Result<Libraries, VideoError> {
    let dirs = tool_search_dirs();
    let youtube = find_in_dirs("yt-dlp", &dirs).ok_or_else(VideoError::missing_tools)?;
    let ffmpeg = find_in_dirs("ffmpeg", &dirs).ok_or_else(VideoError::missing_tools)?;
    Ok(Libraries::new(youtube, ffmpeg))
}

/// Cached `--impersonate` support per yt-dlp binary path.
static IMPERSONATE_SUPPORT: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();

/// Whether `--impersonate chrome` is known-supported for this binary.
///
/// Sync and never blocks an async worker: returns the cached probe result,
/// or `false` (the safe default) while the probe is still running. The first
/// call for a binary kicks the probe off on the blocking pool — the probe can
/// take up to ~15 s on a wedged binary, so the download proceeds without the
/// flag rather than parking a worker; the next download gets the cached
/// answer. Outside a Tokio runtime (unit tests) the probe runs inline; the
/// test fakes answer immediately.
pub(crate) fn ytdlp_supports_impersonation(youtube_bin: &Path) -> bool {
    let cache = IMPERSONATE_SUPPORT.get_or_init(|| Mutex::new(HashMap::new()));
    // A poisoned cache must never panic the caller: skip the read and fall
    // through to the safe default; the next call retries the lock.
    if let Ok(mut guard) = cache.lock() {
        if let Some(&hit) = guard.get(youtube_bin) {
            return hit;
        }
        // Claim the key with the safe default so concurrent first calls don't
        // spawn duplicate probes; the background fill overwrites it.
        guard.insert(youtube_bin.to_path_buf(), false);
    }
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::spawn(warm_impersonation_cache(youtube_bin.to_path_buf()));
        false
    } else {
        let supported = probe_impersonate_support(youtube_bin);
        if let Ok(mut guard) = cache.lock() {
            guard.insert(youtube_bin.to_path_buf(), supported);
        }
        supported
    }
}

/// Probe once on the blocking pool and cache the result. Spawned in the
/// background by `ytdlp_supports_impersonation`; the `unwrap_or(false)` keeps
/// a panicking probe from taking the answer above the safe default.
async fn warm_impersonation_cache(youtube_bin: std::path::PathBuf) {
    let key = youtube_bin.clone();
    let supported = tokio::task::spawn_blocking(move || probe_impersonate_support(&youtube_bin))
        .await
        .unwrap_or(false);
    // A poisoned cache drops the fill silently instead of panicking the
    // background task: the next call re-probes and re-caches.
    if let Ok(mut guard) = IMPERSONATE_SUPPORT
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    {
        guard.insert(key, supported);
    }
}

/// One-shot `--list-impersonate-targets` probe: a `Chrome` row without an
/// "(unavailable)" marker means curl_cffi can impersonate. Any spawn failure,
/// non-zero exit, or timeout resolves to `false` — a wedged binary must not
/// hang the spawn that is being prepared, and must not leave children behind.
fn probe_impersonate_support(youtube_bin: &Path) -> bool {
    use std::io::Read as _;
    #[cfg(unix)]
    use std::os::unix::process::CommandExt as _;
    let mut cmd = std::process::Command::new(youtube_bin);
    cmd.arg("--list-impersonate-targets")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(_) => return false,
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let exited_cleanly = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    kill_probe(&mut child);
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(_) => {
                kill_probe(&mut child);
                return false;
            }
        }
    };
    if !exited_cleanly {
        return false;
    }
    let mut out = String::new();
    let read_ok = child
        .stdout
        .take()
        .is_some_and(|mut pipe| pipe.read_to_string(&mut out).is_ok());
    read_ok
        && out.lines().any(|line| {
            let line = line.to_ascii_lowercase();
            line.contains("chrome") && !line.contains("unavailable")
        })
}

/// Best-effort kill of a wedged probe: the whole process group, so a binary
/// that ignores its flags takes its children down with it. Always reaps.
fn kill_probe(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id();
        // try_wait just reported it still running, so the group is ours.
        // SAFETY: constant signal number; ESRCH (raced exit) is harmless.
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = child.kill();
    let _ = child.wait();
}

/// `--downloader-args` value for yt-dlp's ffmpeg downloader on live captures.
/// LL-HLS timestamp hygiene: split A/V HLS manifests drift when the
/// broadcaster drops frames; `-copyts -avoid_negative_ts make_zero` keeps the
/// tracks aligned and `-fps_mode passthrough` avoids frame-rate munging on
/// copy — all without re-encoding. `-fps_mode` needs ffmpeg 7+, which the
/// current toolchain floor (9+) satisfies, so no version gating.
pub(crate) const LIVE_DOWNLOADER_ARGS: &str =
    "ffmpeg:-fps_mode passthrough -copyts -avoid_negative_ts make_zero";

/// `--postprocessor-args` value moving the moov atom to the front of merged
/// mp4s so playback starts without a full scan. Scoped `Merger+ffmpeg` (per
/// yt-dlp's `PP+EXE:ARGS` syntax) so metadata/subtitle fixups are untouched.
/// mov-only: only passed when the merge target is mp4-family.
pub(crate) const MERGER_FASTSTART_ARGS: &str = "Merger+ffmpeg:-movflags +faststart";

/// Install just yt-dlp into the user library dir. Split from ffmpeg so the UI
/// can report honest per-tool stages; the crate installer exposes no progress.
/// Await from a spawned task — never block the GTK thread.
pub async fn install_ytdlp() -> Result<PathBuf, VideoError> {
    let dir = user_lib_dir();
    let handle = crate::runtime::tokio_rt()
        .spawn(async move { LibraryInstaller::new(dir).install_youtube(None).await });
    match handle.await {
        Ok(Ok(path)) => Ok(path),
        Ok(Err(e)) => Err(VideoError::install(&e)),
        Err(e) => Err(VideoError::runtime(&e)),
    }
}

/// Install the ffmpeg toolchain (ffmpeg *and* ffprobe) into the user library dir.
/// The crate's installer only extracts `ffmpeg`, leaving `--ffmpeg-location`
/// pointing at a dir without ffprobe — so Grab fetches the static build itself.
pub async fn install_ffmpeg() -> Result<PathBuf, VideoError> {
    let dir = user_lib_dir();
    let handle =
        crate::runtime::tokio_rt().spawn(async move { install_ffmpeg_toolchain(dir).await });
    match handle.await {
        Ok(res) => res,
        Err(e) => Err(VideoError::runtime(&e)),
    }
}

/// quickjs-ng is Grab's JS runtime for YouTube. It is pinned on YouTube spawns
/// (see `ytdlp_identity_args`) so a system runtime (e.g. deno, which yt-dlp
/// prefers and enables by default) can never shadow it there. quickjs-ng ships
/// tiny (~2.5MB) official linux x86_64 and aarch64 binaries; the deno
/// alternative is ~40x larger. The installed release follows the quickjs-ng
/// latest tag so the update check and the installer agree on the source of
/// truth. quickjs-ng publishes no checksums, so unpinned releases carry no
/// hash verification (yt-dlp and ffmpeg never had any).
/// Hard cap on the quickjs download: the asset is ~2.5MB, so anything larger
/// is not the released binary. Enforced while streaming, before the bytes are
/// trusted.
const QUICKJS_MAX_DOWNLOAD_BYTES: u64 = 8 * 1024 * 1024;

/// Whether this build's arch has a quickjs-ng release to fetch. Gate for the
/// update check and the installer; where false, yt-dlp keeps its own runtime
/// discovery.
pub(crate) fn quickjs_arch_supported() -> bool {
    matches!(std::env::consts::ARCH, "x86_64" | "aarch64")
}

/// Whether a page URL is YouTube: the only site where Grab pins quickjs-ng as
/// yt-dlp's JS runtime (its authenticated player clients need a JS runtime to
/// yield any formats). Everywhere else yt-dlp keeps its own runtime discovery,
/// so a system runtime there is never shadowed or disabled.
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

/// Download URL for a quickjs-ng release tag, mapped from the build arch
/// to its asset names. The asset is the `qjs` binary itself, not an archive.
/// `None` on architectures quickjs-ng doesn't ship.
pub(crate) fn quickjs_download_url(version: &str) -> Option<String> {
    if !quickjs_arch_supported() {
        return None;
    }
    let arch = std::env::consts::ARCH;
    Some(format!(
        "https://github.com/quickjs-ng/quickjs/releases/download/{version}/qjs-linux-{arch}"
    ))
}

/// Release tags are interpolated into the quickjs download URL, so reject
/// anything outside the tag character set (`A-Za-z0-9._+-`) before it can
/// shape a request.
pub(crate) fn valid_release_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.bytes().all(|b| {
            matches!(
                b,
                b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'.' | b'-' | b'+' | b'_'
            )
        })
}

/// Locate a `qjs` binary: user lib dir first (Grab-installed), then `/app/bin`,
/// then PATH for a system copy. `None` when no JS runtime is on hand.
pub(crate) fn find_quickjs() -> Option<PathBuf> {
    find_in_dirs("qjs", &tool_search_dirs())
}

/// Install quickjs into the user library dir. Split from yt-dlp/ffmpeg so the
/// UI can stage it separately; await off the GTK thread like the other
/// installers.
pub async fn install_quickjs() -> Result<PathBuf, VideoError> {
    let dir = user_lib_dir();
    let handle = crate::runtime::tokio_rt().spawn(async move { install_quickjs_binary(dir).await });
    match handle.await {
        Ok(res) => res,
        Err(e) => Err(VideoError::runtime(&e)),
    }
}

async fn install_quickjs_binary(dir: PathBuf) -> Result<PathBuf, VideoError> {
    let tag = latest_quickjs_tag()
        .await
        .ok_or_else(|| VideoError::install("couldn't determine the latest quickjs-ng release"))?;
    if !valid_release_tag(&tag) {
        return Err(VideoError::install(format!(
            "quickjs-ng published an unexpected release tag: {tag}"
        )));
    }
    let url = quickjs_download_url(&tag)
        .ok_or_else(|| VideoError::install("quickjs has no release for this architecture"))?;
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(VideoError::install)?;
    // Download straight to `qjs.part`: a failed download must never leave a
    // half-written `qjs` behind for `find_quickjs` to mistake as installed.
    let dest = dir.join("qjs");
    let part = dir.join("qjs.part");
    fetch_quickjs(&url, &part, &dest).await.map(|_| dest)
}

/// Fetch the `qjs` binary, mark it executable, and move it into place.
/// Size-capped while streaming; the asset is the released binary itself.
/// Cleans up the `.part` file on any failure, so a half-written binary is
/// never left behind for a later run to mistake as usable.
async fn fetch_quickjs(url: &str, part: &Path, dest: &Path) -> Result<(), VideoError> {
    let result = fetch_quickjs_inner(url, part, dest).await;
    if result.is_err() {
        std::fs::remove_file(part).ok();
    }
    result
}

async fn fetch_quickjs_inner(url: &str, part: &Path, dest: &Path) -> Result<(), VideoError> {
    download_to_file(url, part)
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

/// Pre-create guard for tool downloads and extracts: a planted symlink at
/// `dest` would divert the bytes — and the later chmod — onto an arbitrary
/// file, so refuse instead of following it. A stale regular file (crashed
/// run) is removed so the caller can create atomically with `create_new`,
/// which never follows a freshly planted link either (it just fails).
fn refuse_symlink_target(dest: &Path) -> Result<(), String> {
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

async fn download_to_file(url: &str, dest: &Path) -> Result<(), String> {
    // Refuse a planted symlink before any I/O: it would divert the download
    // (and the later chmod) onto an arbitrary file.
    refuse_symlink_target(dest)?;
    let response = reqwest::get(url)
        .await
        .map_err(|e| format!("couldn't fetch {url}: {e}"))?;
    let response = response
        .error_for_status()
        .map_err(|e| format!("couldn't fetch {url}: {e}"))?;
    write_capped_stream(Box::pin(response.bytes_stream()), dest, url).await
}

/// Stream a download body into `dest`, enforcing the size cap while
/// streaming instead of buffering the whole body up front: a compromised
/// endpoint must not be able to fill memory or disk before we notice.
/// (Currently only the quickjs download uses this helper.)
/// Split from `download_to_file` so the cap is pinnable with a synthetic
/// stream — pushing 8 MiB through a loopback test server proved flaky.
async fn write_capped_stream<S, B, E>(mut stream: S, dest: &Path, url: &str) -> Result<(), String>
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
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("couldn't read {url}: {e}"))?;
        let bytes = chunk.as_ref();
        downloaded += bytes.len() as u64;
        if downloaded > QUICKJS_MAX_DOWNLOAD_BYTES {
            return Err(format!(
                "{url} exceeds the download size limit ({QUICKJS_MAX_DOWNLOAD_BYTES} bytes)"
            ));
        }
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
    Ok(())
}

/// Ensure Grab's quickjs is on hand for a YouTube page URL, installing it on
/// first use. Concurrent callers serialize on a single install and re-check
/// after waiting. No-op off YouTube (nothing else needs it) and on
/// architectures quickjs-ng doesn't ship: yt-dlp then falls back to its own
/// runtime discovery.
pub(crate) async fn ensure_quickjs(page_url: &str) -> Result<(), VideoError> {
    if !is_youtube_url(page_url) || find_quickjs().is_some() || !quickjs_arch_supported() {
        return Ok(());
    }
    static INSTALL_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    let lock = INSTALL_LOCK.get_or_init(|| tokio::sync::Mutex::new(()));
    let _guard = lock.lock().await;
    if find_quickjs().is_some() {
        return Ok(());
    }
    install_quickjs().await.map(|_| ())
}

/// Download one boul2gom/ffmpeg-builds archive and extract `ffmpeg` + `ffprobe`
/// into `dir`; returns the ffmpeg path. Await off the GTK thread.
async fn install_ffmpeg_toolchain(dir: PathBuf) -> Result<PathBuf, VideoError> {
    use yt_dlp::client::deps::ffmpeg::BuildFetcher;

    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(VideoError::install)?;
    let release = BuildFetcher::new()
        .fetch_binary()
        .await
        .map_err(VideoError::install)?;
    let archive = dir.join(&release.name);
    release
        .download(&archive)
        .await
        .map_err(VideoError::install)?;
    tokio::task::spawn_blocking(move || extract_ffmpeg_toolchain(&archive, &dir))
        .await
        .map_err(VideoError::runtime)?
        .map_err(VideoError::install)
}

/// Extract `ffmpeg` and `ffprobe` from a static-build archive into `dir`, mark
/// them executable and delete the archive. Entries match by file name, so flat
/// zips and `bin/`-style layouts both work. A missing ffprobe is not an error.
pub(crate) fn extract_ffmpeg_toolchain(archive: &Path, dir: &Path) -> Result<PathBuf, String> {
    // Always clean up the (large) archive, even when extraction fails.
    let result = extract_ffmpeg_toolchain_inner(archive, dir, MAX_EXTRACT_BYTES);
    std::fs::remove_file(archive).ok();
    result
}

/// Cap extracted binaries: the ffmpeg static build is ~75MB, so 200MB is
/// generous. Enforced via take() — the zip header's size field can't be
/// trusted against a decompression bomb.
const MAX_EXTRACT_BYTES: u64 = 200 * 1024 * 1024;

pub(crate) fn extract_ffmpeg_toolchain_inner(
    archive: &Path,
    dir: &Path,
    max_bytes: u64,
) -> Result<PathBuf, String> {
    use std::io::Read as _;
    use std::os::unix::fs::PermissionsExt as _;

    let file = std::fs::File::open(archive)
        .map_err(|e| format!("couldn't open ffmpeg archive {}: {e}", archive.display()))?;
    let mut zip =
        zip::ZipArchive::new(file).map_err(|e| format!("couldn't read ffmpeg archive: {e}"))?;
    let mut ffmpeg_path = None;
    let mut extracted = Vec::new();
    for i in 0..zip.len() {
        let entry = zip
            .by_index(i)
            .map_err(|e| format!("couldn't read ffmpeg archive entry: {e}"))?;
        if entry.is_dir() {
            continue;
        }
        let tool = match Path::new(entry.name()).file_name().and_then(|n| n.to_str()) {
            Some("ffmpeg") => "ffmpeg",
            Some("ffprobe") => "ffprobe",
            _ => continue,
        };
        let dest = dir.join(tool);
        // A planted symlink would divert the extracted binary (and the
        // chmod) onto an arbitrary file: refuse instead of following it.
        refuse_symlink_target(&dest)?;
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&dest)
            .map_err(|e| format!("couldn't write {}: {e}", dest.display()))?;
        let n = std::io::copy(&mut entry.take(max_bytes + 1), &mut out)
            .map_err(|e| format!("couldn't extract {}: {e}", dest.display()))?;
        // take() truncates silently: if we hit the cap, the entry is not the
        // released binary (or it's a bomb). A truncated executable would fail
        // mysteriously later — refuse it here. Reading max_bytes + 1 lets a
        // legitimate entry of exactly max_bytes through. `copy`'s byte count
        // is the check: no separate metadata() call that could fail open.
        if n > max_bytes {
            // Don't leave a half-installed toolchain: drop what we extracted
            // so far along with the partial file.
            for path in extracted.drain(..) {
                let _ = std::fs::remove_file(path);
            }
            let _ = std::fs::remove_file(&dest);
            return Err(format!(
                "{} in the ffmpeg archive exceeds the {max_bytes}-byte limit",
                dest.display()
            ));
        }
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("couldn't mark {} executable: {e}", dest.display()))?;
        extracted.push(dest.clone());
        if tool == "ffmpeg" {
            ffmpeg_path = Some(dest);
        }
    }
    ffmpeg_path.ok_or_else(|| "ffmpeg binary not found in the downloaded archive".to_string())
}

/// Minimum accepted yt-dlp version by release date. Older binaries predate the
/// JS-challenge era and fail in ways that look like broken pages.
pub const MIN_YTDLP_VERSION: [u32; 3] = [2026, 1, 1];

/// Parse a `yt-dlp --version` first line into comparable parts; anything else
/// (nightlies, forks) is unverifiable.
pub(crate) fn parse_yt_dlp_version(first_line: &str) -> Option<[u32; 3]> {
    let mut parts = first_line.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some([major, minor, patch])
}

/// Whether a release `tag` is newer than the installed version line. Unparseable
/// tags never prompt: an unknown upstream shape must not nag. A `v` prefix is
/// tolerated.
pub(crate) fn ytdlp_update_available(installed: &str, tag: &str) -> bool {
    match (
        parse_yt_dlp_version(installed),
        parse_yt_dlp_version(tag.trim_start_matches('v')),
    ) {
        (Some(current), Some(latest)) => latest > current,
        _ => false,
    }
}

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

/// Version token from an `ffmpeg -version` first line:
/// `ffmpeg version n9.0.2-static …` → `n9.0.2-static`.
pub(crate) fn ffmpeg_version_token(first_line: &str) -> Option<&str> {
    first_line.split_whitespace().nth(2)
}

/// Real home dir from the passwd database, bypassing sandbox `$HOME` remapping
/// (inside Flatpak `$HOME` is the app sandbox dir). `None` on lookup failure.
///
/// Do NOT "simplify" this to `glib::home_dir()`: it prefers `$HOME`, which is
/// exactly the remapped sandbox dir this function exists to bypass.
#[cfg(unix)]
pub(crate) fn real_home_dir() -> Option<PathBuf> {
    // SAFETY: getpwuid returns static storage (or null); only pw_dir up to its NUL is read.
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() {
            return None;
        }
        let dir = (*pw).pw_dir;
        if dir.is_null() {
            return None;
        }
        let len = libc::strlen(dir);
        if len == 0 {
            return None;
        }
        let bytes = std::slice::from_raw_parts(dir as *const u8, len);
        std::str::from_utf8(bytes).ok().map(PathBuf::from)
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

/// Shared trailing argv for every yt-dlp spawn: player-client workaround, JS
/// runtime pin for YouTube, cookies, user agent, then the page URL
/// behind `--`. One helper so these flags cannot drift between spawns (or let
/// a hostile URL parse as a flag).
pub(crate) fn ytdlp_identity_args(
    cookies_browser: &str,
    user_agent: Option<&str>,
    page_url: &str,
) -> Vec<String> {
    let mut args = Vec::new();
    // YouTube force-enables SABR-only streaming for the `web` player client
    // (yt-dlp#12482): its URL-less formats fail the whole extraction. `web`
    // only enters yt-dlp's default rotation when a JS runtime is available
    // (e.g. quickjs on PATH), so exclude it everywhere. Scoped to the
    // youtube extractor: a no-op for other sites.
    args.push("--extractor-args".to_string());
    args.push("youtube:player_client=-web".to_string());
    // quickjs-ng is Grab's JS runtime for YouTube: pin it so a system runtime
    // (e.g. deno, which yt-dlp prefers and enables by default) can never
    // shadow it where the player-client challenges need it. Only deno is
    // enabled by default, so the pin must also disable the other runtimes.
    // Scoped to YouTube: other sites keep yt-dlp's own runtime discovery.
    // Skipped where quickjs-ng ships no release; yt-dlp then keeps its own
    // runtime discovery.
    if is_youtube_url(page_url) && quickjs_arch_supported() {
        args.push("--no-js-runtimes".to_string());
        args.push("--js-runtimes".to_string());
        args.push("quickjs".to_string());
    }
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
    toolchain_dir_in(&tool_search_dirs())
        .or_else(|| ffmpeg_bin.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("/usr/bin"))
        .to_string_lossy()
        .into_owned()
}

/// Latest released yt-dlp tag without downloading anything: one user-initiated
/// GitHub API call. `None` on any network/API failure — the row then reports the
/// check failed instead of prompting.
pub async fn latest_ytdlp_tag() -> Option<String> {
    let handle = crate::runtime::tokio_rt().spawn(async move {
        let fetcher = yt_dlp::client::deps::github::GitHubFetcher::new("yt-dlp", "yt-dlp");
        fetcher
            .fetch_latest_release(None)
            .await
            .ok()
            .map(|release| release.tag_name)
    });
    handle.await.ok().flatten()
}

/// Latest release tag of the upstream Grab downloads ffmpeg from
/// (boul2gom/ffmpeg-builds static builds); `None` when GitHub is unreachable.
pub async fn latest_ffmpeg_tag() -> Option<String> {
    let handle = crate::runtime::tokio_rt().spawn(async move {
        let fetcher = yt_dlp::client::deps::github::GitHubFetcher::new("boul2gom", "ffmpeg-builds");
        fetcher
            .fetch_latest_release(None)
            .await
            .ok()
            .map(|release| release.tag_name)
    });
    handle.await.ok().flatten()
}

/// Latest quickjs-ng release tag; `None` when GitHub is unreachable.
pub async fn latest_quickjs_tag() -> Option<String> {
    let handle = crate::runtime::tokio_rt().spawn(async move {
        let fetcher = yt_dlp::client::deps::github::GitHubFetcher::new("quickjs-ng", "quickjs");
        fetcher
            .fetch_latest_release(None)
            .await
            .ok()
            .map(|release| release.tag_name)
    });
    handle.await.ok().flatten()
}

/// Run `binary --version` off the caller's thread and return its first output
/// line. `None` covers missing binaries, spawn failures and empty output alike.
/// Uses the shared runtime's handle directly so the GTK thread (no tokio
/// context entered) can call it.
async fn tool_first_line(binary: PathBuf, version_arg: &'static str) -> Option<String> {
    crate::runtime::tokio_rt()
        .spawn_blocking(move || {
            std::process::Command::new(&binary)
                .arg(version_arg)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.lines().next().unwrap_or("").trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .await
        .ok()
        .flatten()
}

/// Display-ready version line for an installed tool binary: yt-dlp's
/// `--version` output labeled ("2026.08.19" → "yt-dlp 2026.08.19"), ffmpeg's
/// first line trimmed to its version token ("ffmpeg version n9.0.1 …" →
/// "ffmpeg n9.0.1"). `None` when the binary can't be probed.
pub(crate) async fn tool_display_version(
    binary: PathBuf,
    version_arg: &'static str,
) -> Option<String> {
    let line = tool_first_line(binary.clone(), version_arg).await?;
    if binary
        .file_name()
        .is_some_and(|n| n.to_string_lossy() == "ffmpeg")
    {
        let token = line.split_whitespace().nth(2)?;
        return Some(format!("ffmpeg {token}"));
    }
    if binary
        .file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with("yt-dlp"))
    {
        return Some(format!("yt-dlp {line}"));
    }
    if binary
        .file_name()
        .is_some_and(|n| n.to_string_lossy() == "qjs")
    {
        // `qjs --version` prints just "0.17.0".
        return Some(format!("quickjs {line}"));
    }
    Some(line)
}

/// Refuse stale or unverifiable toolchains before any network happens; returns
/// the raw version lines for attempt logging.
pub(crate) async fn ensure_tool_versions(libs: &Libraries) -> Result<(String, String), VideoError> {
    // ffmpeg takes a single-dash -version; --version is an error there.
    let (yt, ff) = tokio::join!(
        tool_first_line(libs.youtube.clone(), "--version"),
        tool_first_line(libs.ffmpeg.clone(), "-version")
    );
    let yt = yt.ok_or_else(VideoError::missing_tools)?;
    let fresh = parse_yt_dlp_version(&yt).is_some_and(|v| v >= MIN_YTDLP_VERSION);
    if !fresh {
        return Err(VideoError::outdated());
    }
    let ff = ff.ok_or_else(VideoError::missing_tools)?;
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

    #[test]
    fn distro_packages_reports_quickjs_coverage() {
        // Distros with a known quickjs package get it in the install
        // command; distros without one (Void, Solus, openSUSE) omit it, and
        // the dialog must fall back to the upstream release link there.
        let fedora = distro_packages("ID=fedora\nNAME=\"Fedora Linux\"\n").unwrap();
        assert!(fedora.has_quickjs_package);
        assert!(
            fedora.install_all.contains("quickjs-ng"),
            "{}",
            fedora.install_all
        );

        let void = distro_packages("ID=void\nNAME=\"Void Linux\"\n").unwrap();
        assert!(!void.has_quickjs_package);
        assert!(
            !void.install_all.contains("quickjs"),
            "{}",
            void.install_all
        );

        let solus = distro_packages("ID=solus\nNAME=\"Solus\"\n").unwrap();
        assert!(!solus.has_quickjs_package);

        // ID_LIKE fallback: a derivative inherits the parent's packages.
        let derivative =
            distro_packages("ID=neon\nID_LIKE=\"ubuntu debian\"\nNAME=\"KDE neon\"\n").unwrap();
        assert!(derivative.has_quickjs_package);
    }

    #[test]
    fn refuse_symlink_target_rejects_link() {
        let dir = unique_dir("symlink");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target");
        std::fs::write(&target, b"precious").unwrap();
        let link = dir.join("qjs.part");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err = refuse_symlink_target(&link).unwrap_err();
        assert!(err.contains("symlink"), "unexpected error: {err}");
        // Untouched: the link still points at the intact target.
        assert!(link.is_symlink());
        assert_eq!(std::fs::read(&target).unwrap(), b"precious");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refuse_symlink_target_clears_regular_file() {
        let dir = unique_dir("regular");
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("qjs.part");
        std::fs::write(&part, b"stale").unwrap();

        refuse_symlink_target(&part).unwrap();
        assert!(!part.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refuse_symlink_target_missing_is_fine() {
        let dir = unique_dir("missing");
        std::fs::create_dir_all(&dir).unwrap();

        refuse_symlink_target(&dir.join("qjs.part")).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn download_to_file_refuses_symlink_before_network() {
        let dir = unique_dir("dl-symlink");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target");
        std::fs::write(&target, b"precious").unwrap();
        let link = dir.join("qjs.part");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        // Unroutable URL: the symlink guard must fire before any I/O.
        let err = download_to_file("http://127.0.0.1:1/nope", &link)
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
        let tag = "v0.17.0";
        assert!(valid_release_tag(tag));
        let url = quickjs_download_url(tag).unwrap();
        assert!(
            url.ends_with("/releases/download/v0.17.0/qjs-linux-x86_64")
                || url.ends_with("/releases/download/v0.17.0/qjs-linux-aarch64")
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
        let dest = dir.join("qjs.part");

        let err = write_capped_stream(Box::pin(chunks), &dest, "http://127.0.0.1/qjs")
            .await
            .unwrap_err();
        assert!(
            err.contains("exceeds the download size limit"),
            "unexpected error: {err}"
        );
        // The over-limit chunk is rejected before it is written: the partial
        // never grows past the cap.
        let written = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
        assert!(written <= QUICKJS_MAX_DOWNLOAD_BYTES);
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
        let dest = dir.join("qjs.part");

        write_capped_stream(Box::pin(chunks), &dest, "http://127.0.0.1/qjs")
            .await
            .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello world");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn fetch_quickjs_removes_half_written_part() {
        // The server promises a megabyte but hangs up after one chunk: the
        // download fails mid-stream with a half-written `.part` on disk.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            // A failed accept must never silently drop the listener: the
            // client's queued connection would get RST and fail with a
            // confusing request error instead of the truncation below.
            let mut stream = None;
            for _ in 0..20 {
                match listener.accept() {
                    Ok((s, _)) => {
                        stream = Some(s);
                        break;
                    }
                    Err(e) => {
                        eprintln!("test server: accept failed ({e}), retrying");
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                }
            }
            let mut stream = stream.expect("test server: accept kept failing");
            // Read the request before responding: exiting with the request
            // still unread makes the kernel RST the connection, and if that
            // RST lands before the client's request write the fetch fails
            // with "error sending request" instead of the truncation below.
            // With the request consumed, close() sends a clean FIN.
            let mut req = Vec::new();
            let mut buf = [0u8; 1024];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => return, // client went away
                    Ok(n) => {
                        req.extend_from_slice(&buf[..n]);
                        if req.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(_) => return,
                }
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\nConnection: close\r\n\r\n"
            )
            .ok();
            // One chunk, then hang up: the body is truncated mid-download.
            stream.write_all(&[0u8; 4096]).ok();
        });

        let dir = unique_dir("qjs-truncated");
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("qjs.part");
        let dest = dir.join("qjs");
        let err = fetch_quickjs(&format!("http://{addr}/qjs"), &part, &dest)
            .await
            .unwrap_err();
        // The failure must be the mid-stream truncation — not a connect
        // failure, which would pass the assertions below vacuously.
        assert!(
            err.to_string().contains("couldn't read"),
            "unexpected error: {err}"
        );
        // The half-written `.part` must not survive a failed fetch, and the
        // destination must never appear without a complete download.
        assert!(!part.exists(), "half-written .part survived a failed fetch");
        assert!(!dest.exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
