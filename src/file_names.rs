//! File-name primitives: sanitize, split, dedupe, derive, atomic rename, piece sizing, byte formatting.

use gio::prelude::FileExt as _;

/// Split stem and extension (last dot only; leading dot is stem). Pure.
fn split_stem_ext(name: &str) -> (&str, Option<&str>) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], Some(&name[i..])),
        _ => (name, None),
    }
}

/// Cap filename to filesystem limits, keeping extension; reserves room for the ` (n)` suffix.
pub(crate) fn shorten_filename(name: &str) -> String {
    const MAX_FILENAME_BYTES: usize = 240;
    if name.len() <= MAX_FILENAME_BYTES {
        return name.to_string();
    }
    let (stem, ext) = split_stem_ext(name);
    let ext_len = ext.map_or(0, str::len);
    // If the extension alone exceeds the budget, drop it: a 240-byte
    // "extension" is pathological, and keeping it would produce a name
    // over 255 bytes (ENAMETOOLONG).
    if ext_len >= MAX_FILENAME_BYTES {
        let keep = stem.floor_char_boundary(MAX_FILENAME_BYTES);
        return stem[..keep].to_string();
    }
    let keep = stem.floor_char_boundary(MAX_FILENAME_BYTES - ext_len);
    match ext {
        Some(e) => format!("{}{e}", &stem[..keep]),
        None => stem[..keep].to_string(),
    }
}

/// Fold to ASCII like yt-dlp `--restrict-filenames` (accents to base, other non-ASCII to `_`, quotes/controls dropped, `_` collapsed).
/// Extension kept; empty stem falls back to `"file"`.
pub(crate) fn restrict_filename_ascii(name: &str) -> String {
    let (stem, ext) = split_stem_ext(name);
    let stem = fold_ascii_part(stem);
    let stem = if stem.is_empty() {
        "file".to_string()
    } else {
        stem
    };
    let out = match ext {
        Some(e) => format!("{stem}{}", fold_ascii_part(e)),
        None => stem,
    };
    // The fold keeps dots: "..ф" -> "..", "._" -> ".", "a.фф" -> "a.".
    // Re-validate so a server-supplied name can't escape to parent dir.
    let out = out.trim_end_matches('.').to_string();
    if out.is_empty() || !sane_filename(&out) {
        "file".to_string()
    } else {
        out
    }
}

/// Fold one filename part (stem or extension) to ASCII.
fn fold_ascii_part(part: &str) -> String {
    /// Base-letter fold for accented Latin; unlisted becomes `_` in caller.
    fn fold_accent(c: char) -> Option<&'static str> {
        match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' | 'ǎ' => Some("a"),
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => Some("e"),
            'ì' | 'í' | 'î' | 'ï' | 'ī' | 'ĭ' | 'į' => Some("i"),
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' | 'ǒ' => Some("o"),
            'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' | 'ǔ' => Some("u"),
            'ý' | 'ÿ' => Some("y"),
            'ñ' | 'ń' | 'ň' => Some("n"),
            'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => Some("c"),
            'ß' => Some("ss"),
            'æ' => Some("ae"),
            'œ' => Some("oe"),
            'ð' | 'ď' | 'đ' => Some("d"),
            'þ' => Some("th"),
            'ł' => Some("l"),
            'š' | 'ś' | 'ŝ' | 'ş' => Some("s"),
            'ž' | 'ź' | 'ż' => Some("z"),
            'ğ' => Some("g"),
            'ř' => Some("r"),
            'ť' | 'ţ' => Some("t"),
            'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' | 'Ā' | 'Ă' | 'Ą' | 'Ǎ' => Some("A"),
            'È' | 'É' | 'Ê' | 'Ë' | 'Ē' | 'Ĕ' | 'Ė' | 'Ę' | 'Ě' => Some("E"),
            'Ì' | 'Í' | 'Î' | 'Ï' | 'Ī' | 'Ĭ' | 'Į' => Some("I"),
            'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'Ø' | 'Ō' | 'Ŏ' | 'Ő' | 'Ǒ' => Some("O"),
            'Ù' | 'Ú' | 'Û' | 'Ü' | 'Ū' | 'Ŭ' | 'Ů' | 'Ű' | 'Ų' | 'Ǔ' => Some("U"),
            'Ý' | 'Ÿ' => Some("Y"),
            'Ñ' | 'Ń' | 'Ň' => Some("N"),
            'Ç' | 'Ć' | 'Ĉ' | 'Ċ' | 'Č' => Some("C"),
            'Æ' => Some("AE"),
            'Œ' => Some("OE"),
            'Ð' | 'Ď' | 'Đ' => Some("D"),
            'Þ' => Some("TH"),
            'Ł' => Some("L"),
            'Š' | 'Ś' | 'Ŝ' | 'Ş' => Some("S"),
            'Ž' | 'Ź' | 'Ż' => Some("Z"),
            'Ğ' => Some("G"),
            'Ř' => Some("R"),
            'Ť' | 'Ţ' => Some("T"),
            _ => None,
        }
    }

    let mut out = String::with_capacity(part.len());
    for c in part.chars() {
        if let Some(base) = fold_accent(c) {
            out.push_str(base);
        } else if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
            out.push(c);
        } else if c == '"' || c.is_control() {
            continue;
        } else {
            out.push('_');
        }
    }
    collapse_underscores(&out).trim_matches('_').to_string()
}

/// Collapse runs of `_` into one. Shared by `fold_ascii_part` and
/// `sanitize_folder_name`; each applies its own edge trim afterward.
fn collapse_underscores(s: &str) -> String {
    let mut collapsed = String::with_capacity(s.len());
    let mut prev_underscore = false;
    for c in s.chars() {
        if c == '_' {
            if prev_underscore {
                continue;
            }
            prev_underscore = true;
        } else {
            prev_underscore = false;
        }
        collapsed.push(c);
    }
    collapsed
}

/// Append ` (n)` before extension until `taken` is false, e.g. `f.iso` taken returns `f (1).iso`.
pub fn dedupe_filename(filename: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(filename) {
        return filename.to_string();
    }
    let (stem, ext) = match filename.rfind('.') {
        Some(i) if i > 0 => (&filename[..i], Some(&filename[i + 1..])),
        _ => (filename, None),
    };
    let mut n = 1;
    for _ in 1..=9999 {
        let cand = match ext {
            Some(e) => format!("{stem} ({n}).{e}"),
            None => format!("{filename} ({n})"),
        };
        if !taken(&cand) {
            return cand;
        }
        n += 1;
    }
    // Absurd collisions: return next candidate anyway rather than stat-ing forever.
    match ext {
        Some(e) => format!("{stem} ({n}).{e}"),
        None => format!("{filename} ({n})"),
    }
}

/// File stem of a finished-name candidate; `""` when none (never reserves).
pub(crate) fn name_stem(name: &str) -> &str {
    std::path::Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
}

/// Explicit bidi controls (escapes, never literal glyphs: invisible in source).
pub(crate) fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{200E}' | '\u{200F}' | '\u{061C}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Folder-safe form of a collection title (playlist, stories, highlights):
/// path separators, NUL, controls and bidi overrides become `_`, runs
/// collapse and edges trim; falls back to `"collection"` when nothing
/// survives. Pure.
pub(crate) fn sanitize_folder_name(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    for c in title.chars() {
        if c == '/' || c == '\\' || c == '\0' || c.is_control() || is_bidi_control(c) {
            out.push('_');
        } else {
            out.push(c);
        }
    }
    // Collapse runs and trim edges, mirroring the ASCII fold's tidying.
    let collapsed = collapse_underscores(&out);
    let trimmed = collapsed.trim_matches(|c| c == '_' || c == ' ' || c == '.');
    if trimmed.is_empty() {
        "collection".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Whether `p` squats a directory name without being a real dir: a symlink
/// (checked with `symlink_metadata`, which doesn't follow the link — a
/// symlink reads as a symlink even when its target is a dir) or any non-dir.
fn is_squatter(p: &std::path::Path) -> bool {
    match std::fs::symlink_metadata(p) {
        Ok(md) => {
            let ft = md.file_type();
            ft.is_symlink() || !ft.is_dir()
        }
        // Nothing there: the name is free.
        Err(_) => false,
    }
}

/// Atomically create `base/name` as a real directory, reusing an existing
/// real dir but never following a planted symlink into place.
///
/// `create_dir` (not `_all`) is the atomic check-and-create: a symlink
/// planted between the dedupe scan and the create fails with
/// `AlreadyExists`, and the name dedupes to a fresh one. A post-create
/// `symlink_metadata` re-check closes the residual gap after our own
/// create. Retries are bounded: a persistent squatter can't spin us forever.
///
/// Returns the directory to use. On unrecoverable I/O errors (base
/// unwritable etc.) it keeps the old best-effort behavior and returns the
/// path; the write then fails loudly at its own site.
pub(crate) fn create_guarded_dir(base: &std::path::Path, name: &str) -> std::path::PathBuf {
    // Keep the old ensure-parents behavior; only the leaf is guarded.
    let _ = std::fs::create_dir_all(base);
    let mut tried: Vec<String> = Vec::new();
    for _ in 0..32 {
        let candidate = dedupe_filename(name, |n| {
            tried.iter().any(|t| t.as_str() == n) || is_squatter(&base.join(n))
        });
        tried.push(candidate.clone());
        let path = base.join(&candidate);
        match std::fs::create_dir(&path) {
            Ok(()) => {
                // We created it: only a swap in the gap after create could
                // leave a non-dir here — retry under a fresh name.
                if !is_squatter(&path) {
                    return path;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // A real dir (pre-existing, or ours from an earlier retry)
                // is reused, preserving share-the-folder semantics; a
                // symlink or non-dir squatter dedupes on the next round.
                if let Ok(md) = std::fs::symlink_metadata(&path)
                    && md.file_type().is_dir()
                {
                    return path;
                }
            }
            Err(_) => return path,
        }
    }
    // Absurd contention: hand back the last candidate; writes fail loudly.
    base.join(tried.pop().unwrap_or_else(|| name.to_string()))
}

/// Titled subfolder for a multi-item collection (playlist, stories,
/// highlights), torrent-style: sanitized, created eagerly, and reused when the
/// same collection is added again (duplicate files then fail Parabolic-style
/// at start instead of scattering ` (1)` copies). Never follows a
/// pre-existing symlink into place: a planted link with the collection name
/// would redirect downloads outside the download dir, so a symlink — or any
/// non-dir squatter — dedupes to a fresh name instead. The create itself is
/// atomic, closing the dedupe-then-create race.
pub(crate) fn collection_subdir(dir: &str, title: &str) -> String {
    let folder = shorten_filename(&sanitize_folder_name(title));
    create_guarded_dir(std::path::Path::new(dir), &folder)
        .to_string_lossy()
        .into_owned()
}

pub(crate) fn sane_filename(s: &str) -> bool {
    !s.is_empty()
        && !s.contains('/')
        // Backslash: `basename()` in download_fetch.rs already treats it as a
        // directory separator; reject here for consistency.
        && !s.contains('\\')
        && !s.contains('\0')
        && s != "."
        && s != ".."
        // Reject controls/bidi overrides (deceive listings and notifications; servers send them).
        && !s.chars().any(|c| c.is_control() || is_bidi_control(c))
}

/// Best-effort filename from URL path via `percent_encoding`
/// (single-pass `%XX` decode, leaves `+`); falls back to the literal input,
/// then `index.html` at the call sites.
pub(crate) fn percent_decode(s: &str) -> String {
    // Decoded NUL (from `%00` or a literal NUL) is rejected: filenames
    // can't contain it, so keep the literal like the old GLib path did.
    percent_encoding::percent_decode_str(s)
        .decode_utf8()
        .ok()
        .filter(|decoded| !decoded.contains('\0'))
        .map(|cow| cow.into_owned())
        .unwrap_or_else(|| s.to_owned())
}

pub fn filename_from_url(url_str: &str) -> String {
    url::Url::parse(url_str)
        .ok()
        .and_then(|u| {
            u.path_segments()
                .and_then(|mut segs| segs.rfind(|s| !s.is_empty()).map(|s| s.to_string()))
        })
        .map(|s| percent_decode(&s))
        .filter(|s| sane_filename(s))
        .unwrap_or_else(|| "index.html".to_string())
}

/// Human-readable byte size via GLib's `g_format_size` (SI base-1000, localized).
pub(crate) fn fmt_bytes(n: u64) -> String {
    glib::format_size(n).to_string()
}

/// On-disk size via GIO's `measure_disk_usage` (apparent size, no symlink
/// descent); `None` when unreadable.
pub(crate) fn path_size(path: &std::path::Path) -> Option<u64> {
    let file = gio::File::for_path(path);
    file.measure_disk_usage(
        gio::FileMeasureFlags::APPARENT_SIZE,
        None::<&gio::Cancellable>,
        None,
    )
    .map(|(size, _, _)| size)
    .ok()
}

/// Off-thread [`path_size`]: `measure_disk_usage` is a recursive `du`-style
/// scan that can stall the GTK main loop on large folders, so completions
/// measure it on GIO's blocking pool and update the row when it lands.
pub(crate) async fn path_size_async(path: std::path::PathBuf) -> Option<u64> {
    gio::spawn_blocking(move || path_size(&path))
        .await
        .ok()
        .flatten()
}

/// Smallest piece: <=~4GB splits into 1MB pieces so one slow connection delays only the tail.
pub(crate) const PIECE_MIN: u64 = 1024 * 1024;
/// Largest piece: bounds per-request overhead without starving work-stealing.
pub(crate) const PIECE_MAX: u64 = 16 * 1024 * 1024;
/// Pieces per download to aim for; beyond this the piece size grows.
const PIECE_TARGET_COUNT: u64 = 4096;

/// Piece byte range; pure function of total so bitmaps survive restarts. Do not change without a queue migration.
pub(crate) fn piece_len(total: u64) -> u64 {
    total
        .div_ceil(PIECE_TARGET_COUNT)
        .clamp(PIECE_MIN, PIECE_MAX)
}

/// Rename without clobbering: `renameat2(RENAME_NOREPLACE)`, else hard-link claim, else atomic-claim rename, else `create_new` copy cross-device.
pub(crate) fn rename_noreplace(
    old: &std::path::Path,
    new: &std::path::Path,
) -> std::io::Result<()> {
    fn noreplace_unsupported(e: &std::io::Error) -> bool {
        matches!(
            e.raw_os_error(),
            Some(c) if c == libc::ENOSYS || c == libc::EINVAL || c == libc::EOPNOTSUPP
        )
    }
    fn link_unsupported(e: &std::io::Error) -> bool {
        matches!(
            e.raw_os_error(),
            Some(c) if c == libc::EPERM || c == libc::EOPNOTSUPP || c == libc::ENOSYS
        )
    }

    #[cfg(target_os = "linux")]
    match rename_noreplace_sys(old, new) {
        // Old kernels (< 3.15) lack renameat2 (ENOSYS); some filesystems
        // reject the flag with EINVAL/EOPNOTSUPP: use the portable path.
        Err(e) if noreplace_unsupported(&e) => {}
        // Cross-device (EXDEV): copy through a `create_new` claim.
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => return copy_noreplace(old, new),
        r => return r,
    }
    // Claim `new` via link: `exists()` + rename is a TOCTOU; the errno arms below pick the fallback.
    loop {
        match std::fs::hard_link(old, new) {
            Ok(()) => {
                if let Err(e) = std::fs::remove_file(old) {
                    tracing::warn!("rename_noreplace: linked but could not unlink source: {e}");
                }
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err(e),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                return copy_noreplace(old, new);
            }
            Err(e) if link_unsupported(&e) => {
                // Filesystem doesn't support hard links: claim `new`
                // atomically with create_new, then rename over our own
                // placeholder. AlreadyExists propagates; cleanup on failure.
                tracing::warn!(
                    "rename_noreplace: hard link unsupported (errno {}), using atomic claim",
                    e.raw_os_error().unwrap_or(-1)
                );
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(new)?;
                return std::fs::rename(old, new).inspect_err(|_| {
                    let _ = std::fs::remove_file(new);
                });
            }
            Err(e) => return Err(e),
        }
    }
}

/// Copy without replacing (cross-device fallback): `create_new` claim, unlink source after landing; failed copy removes partial dest.
fn copy_noreplace(old: &std::path::Path, new: &std::path::Path) -> std::io::Result<()> {
    let mut src = std::fs::File::open(old)?;
    let permissions = src.metadata().map(|m| m.permissions()).ok();
    let mut dst = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(new)?;
    let copy = std::io::copy(&mut src, &mut dst).and_then(|_| dst.sync_all());
    if copy.is_err() {
        let _ = std::fs::remove_file(new);
        return copy.map(|_| ());
    }
    drop(dst);
    if let Some(permissions) = permissions {
        let _ = std::fs::set_permissions(new, permissions);
    }
    std::fs::remove_file(old)
}

/// `renameat2` via libc: no hand-declared FFI, no glibc version dependency.
#[cfg(target_os = "linux")]
fn rename_noreplace_sys(old: &std::path::Path, new: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    const RENAME_NOREPLACE: std::os::raw::c_ulong = 1; // renameat2(2)
    // Names never contain NUL; fail visibly instead of truncating if one slips through.
    let cvt = |p: &std::path::Path| {
        std::ffi::CString::new(p.as_os_str().as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
    };
    let (old, new) = (cvt(old)?, cvt(new)?);
    // SAFETY: NUL-terminated buffers outlive the call; the rest are integers.
    let r = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            old.as_ptr(),
            libc::AT_FDCWD,
            new.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Whether a row name is the URL-derived intake name (modulo ` (N)` suffixes). Pure for tests.
pub(crate) fn is_url_derived_name(current: &str, page_url: &str) -> bool {
    let derived = filename_from_url(page_url);
    current == derived || strip_dedupe_suffix(current) == derived
}

/// Strip intake-dedupe suffix (`watch (12)` → `watch`); ASCII-boundary ops only, never splits non-ASCII mid-codepoint. Pure.
pub(crate) fn strip_dedupe_suffix(name: &str) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], Some(&name[i..])),
        _ => (name, None),
    };
    if let Some(open) = stem.rfind(" (") {
        let inner = &stem[open + 2..];
        if !inner.is_empty()
            && inner
                .strip_suffix(')')
                .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()))
        {
            let base = &stem[..open];
            if base.is_empty() {
                return name.to_string();
            }
            return match ext {
                Some(e) => format!("{base}{e}"),
                None => base.to_string(),
            };
        }
    }
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Unique scratch dir per test (never a shared staging parent).
    fn unique_dir(tag: &str) -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("grab-guarded-{tag}-{}-{n}", std::process::id()))
    }

    fn plant_symlink(link: &std::path::Path, target: &std::path::Path) {
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    #[test]
    fn guarded_dir_creates_real_dir() {
        let base = unique_dir("create");
        let got = create_guarded_dir(&base, "Videos");
        assert_eq!(got, base.join("Videos"));
        assert!(got.is_dir() && !got.is_symlink());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn guarded_dir_dedupes_planted_symlink() {
        let base = unique_dir("symlink");
        std::fs::create_dir_all(&base).unwrap();
        let target = base.join("evil-target");
        std::fs::write(&target, b"do not touch").unwrap();
        plant_symlink(&base.join("Videos"), &target);

        let got = create_guarded_dir(&base, "Videos");
        // Deduped past the squatter, never through it.
        assert_ne!(got, base.join("Videos"));
        assert!(got.is_dir() && !got.is_symlink());
        // The planted link and its target are untouched.
        assert!(base.join("Videos").is_symlink());
        assert_eq!(std::fs::read(&target).unwrap(), b"do not touch");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn guarded_dir_dedupes_non_dir_squatter() {
        let base = unique_dir("file");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("Videos"), b"squatter").unwrap();

        let got = create_guarded_dir(&base, "Videos");
        assert_ne!(got, base.join("Videos"));
        assert!(got.is_dir());
        // The squatting file is untouched.
        assert_eq!(std::fs::read(base.join("Videos")).unwrap(), b"squatter");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn guarded_dir_reuses_real_dir() {
        let base = unique_dir("reuse");
        let existing = base.join("Videos");
        std::fs::create_dir_all(&existing).unwrap();

        let got = create_guarded_dir(&base, "Videos");
        assert_eq!(got, existing);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn collection_subdir_dodges_symlink() {
        let base = unique_dir("collection");
        std::fs::create_dir_all(&base).unwrap();
        let target = base.join("target");
        std::fs::create_dir_all(&target).unwrap();
        plant_symlink(&base.join("My Show"), &target);

        let got = PathBuf::from(collection_subdir(base.to_str().unwrap(), "My Show"));
        assert_ne!(got, base.join("My Show"));
        assert!(got.is_dir() && !got.is_symlink());
        // Nothing was written through the link.
        assert!(std::fs::read_dir(&target).unwrap().next().is_none());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn percent_decode_nul_escape_falls_back_to_literal() {
        // GLib rejects %00 (escaped NUL) as an error; we fall back to the
        // original string rather than producing a NUL byte. The literal
        // "%00" is harmless in a filename (no NUL, no traversal).
        let got = percent_decode("foo%00bar.mp4");
        assert_eq!(got, "foo%00bar.mp4");
        assert!(!got.contains('\0'));
        // Normal escapes still decode.
        assert_eq!(percent_decode("hello%20world"), "hello world");
    }

    #[test]
    fn percent_decode_invalid_utf8_escape_falls_back_to_literal() {
        // GLib decodes %FF%FE to raw bytes (invalid UTF-8) instead of
        // erroring, and gtk-rs wraps the result unchecked (debug_assert
        // only): without re-validation that launders non-UTF-8 into a Rust
        // String (soundness hole). We fall back to the literal text instead,
        // and the result is always valid UTF-8.
        let got = percent_decode("%FF%FE.bin");
        assert_eq!(got, "%FF%FE.bin");
        assert!(std::str::from_utf8(got.as_bytes()).is_ok());
    }

    #[test]
    fn path_size_tolerates_symlinks() {
        // GIO's measure_disk_usage uses AT_SYMLINK_NOFOLLOW: symlinks are
        // counted as themselves (apparent size = target path length), never
        // traversed. This matches the old std::fs walk which never descended
        // links. Pin the contract: symlinks to outside, dangling links, and
        // leaf symlinks must not fail, and outside targets are NOT included.
        let base = unique_dir("pathsize");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("file.txt"), vec![b'x'; 100]).unwrap();
        // Symlink to a 1000-byte file outside the measured tree.
        let outside = unique_dir("pathsize-outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("big.bin"), vec![b'y'; 1000]).unwrap();
        let link_target = outside.join("big.bin");
        std::os::unix::fs::symlink(&link_target, base.join("link-outside")).unwrap();
        // Dangling symlink.
        std::os::unix::fs::symlink(base.join("nonexistent"), base.join("dangling")).unwrap();

        let size = path_size(&base);
        assert!(size.is_some(), "path_size must tolerate symlinks");
        let total = size.unwrap();
        // The 1000-byte outside target is NOT traversed (NOFOLLOW).
        assert!(
            total < 100 + 1000,
            "outside target must not be included, got {total}"
        );
        // The symlink itself IS counted (apparent size = target path length).
        let expected = 100
            + link_target.to_str().unwrap().len() as u64
            + base.join("nonexistent").to_str().unwrap().len() as u64;
        assert_eq!(total, expected, "symlinks counted as themselves (NOFOLLOW)");

        std::fs::remove_dir_all(&base).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn restrict_ascii_never_returns_dot_or_dotdot() {
        // Regression: the fold kept dots, so server-supplied names like
        // "..ф" folded to ".." (parent dir escape).
        for (input, bad) in [("..ф", ".."), ("._", "."), ("a.фф", "a.")] {
            let out = restrict_filename_ascii(input);
            assert!(
                out != bad,
                "restrict_filename_ascii({input:?}) returned {out:?}, want not {bad:?}"
            );
            assert!(
                sane_filename(&out),
                "restrict_filename_ascii({input:?}) returned insane {out:?}"
            );
        }
    }

    #[test]
    fn restrict_ascii_dot_cases_fall_back_to_file() {
        assert_eq!(restrict_filename_ascii("..ф"), "file");
        assert_eq!(restrict_filename_ascii("._"), "file");
        assert_eq!(restrict_filename_ascii("a.фф"), "a");
    }
}
