//! File-name primitives: sanitize, split, dedupe, derive, atomic rename, piece sizing, byte formatting.

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
    let keep = stem.floor_char_boundary(MAX_FILENAME_BYTES.saturating_sub(ext_len));
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
    match ext {
        Some(e) => format!("{stem}{}", fold_ascii_part(e)),
        None => stem,
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
        } else {
            out.push('_');
        }
    }
    let mut collapsed = String::with_capacity(out.len());
    let mut prev_underscore = false;
    for c in out.chars() {
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
    collapsed.trim_matches('_').to_string()
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
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{200E}' | '\u{200F}' | '\u{61C}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
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
    let mut collapsed = String::with_capacity(out.len());
    let mut prev_underscore = false;
    for c in out.chars() {
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
    let trimmed = collapsed.trim_matches(|c| c == '_' || c == ' ' || c == '.');
    if trimmed.is_empty() {
        "collection".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Titled subfolder for a multi-item collection (playlist, stories,
/// highlights), torrent-style: sanitized, created eagerly, and reused when the
/// same collection is added again (duplicate files then fail Parabolic-style
/// at start instead of scattering ` (1)` copies). Never follows a
/// pre-existing symlink into place: a planted link with the collection name
/// would redirect downloads outside the download dir (`create_dir_all`
/// follows it), so a symlink — or any non-dir squatter — dedupes to a fresh
/// name instead.
pub(crate) fn collection_subdir(dir: &str, title: &str) -> String {
    let folder = shorten_filename(&sanitize_folder_name(title));
    let base = std::path::Path::new(dir);
    let folder = dedupe_filename(&folder, |n| {
        let p = base.join(n);
        match std::fs::symlink_metadata(&p) {
            // `symlink_metadata` doesn't follow the link: a symlink reads as
            // a symlink even when its target is a dir.
            Ok(md) => {
                let ft = md.file_type();
                ft.is_symlink() || !ft.is_dir()
            }
            // Nothing there: the name is free.
            Err(_) => false,
        }
    });
    let path = base.join(&folder);
    let _ = std::fs::create_dir_all(&path);
    path.to_string_lossy().into_owned()
}

pub(crate) fn sane_filename(s: &str) -> bool {
    !s.is_empty()
        && !s.contains('/')
        && !s.contains('\0')
        && s != "."
        && s != ".."
        // Reject controls/bidi overrides (deceive listings and notifications; servers send them).
        && !s.chars().any(|c| c.is_control() || is_bidi_control(c))
}

/// Best-effort filename from URL path (decodes `%XX`, leaves `+`); falls back to `index.html`.
pub(crate) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let mut decoded = None;
        if bytes[i] == b'%'
            && let (Some(&h), Some(&l)) = (bytes.get(i + 1), bytes.get(i + 2))
            && let (Some(h), Some(l)) = ((h as char).to_digit(16), (l as char).to_digit(16))
        {
            decoded = Some((h << 4 | l) as u8);
        }
        if let Some(b) = decoded {
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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

pub(crate) fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < 4 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// On-disk size: files report length, folders sum contents (no symlink descent); `None` when unreadable.
pub(crate) fn path_size(path: &std::path::Path) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.is_file() {
        return Some(meta.len());
    }
    if !meta.is_dir() {
        return None;
    }
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                total = total.saturating_add(entry.metadata().map(|m| m.len()).unwrap_or(0));
            }
        }
    }
    Some(total)
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

/// Rename without clobbering: `renameat2(RENAME_NOREPLACE)`, else hard-link claim, else rename, else `create_new` copy cross-device.
pub(crate) fn rename_noreplace(
    old: &std::path::Path,
    new: &std::path::Path,
) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    match rename_noreplace_sys(old, new) {
        // Ancient kernels (< 3.15) lack renameat2 (ENOSYS 38): use the portable path.
        Err(e) if e.raw_os_error() == Some(38) => {}
        // Cross-device (EXDEV 18): copy through a `create_new` claim.
        Err(e) if e.raw_os_error() == Some(18) => return copy_noreplace(old, new),
        r => return r,
    }
    // Claim `new` via link: `exists()` + rename is a TOCTOU; the errno arms below pick the fallback.
    loop {
        match std::fs::hard_link(old, new) {
            Ok(()) => return std::fs::remove_file(old),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err(e),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if e.raw_os_error() == Some(18) => {
                return copy_noreplace(old, new);
            }
            Err(e) if matches!(e.raw_os_error(), Some(1 | 95 | 38)) => {
                return std::fs::rename(old, new);
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

/// `renameat2` without libc: one syscall, three stable constants.
#[cfg(target_os = "linux")]
fn rename_noreplace_sys(old: &std::path::Path, new: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    unsafe extern "C" {
        fn renameat2(
            olddirfd: std::os::raw::c_int,
            oldpath: *const std::os::raw::c_char,
            newdirfd: std::os::raw::c_int,
            newpath: *const std::os::raw::c_char,
            flags: std::os::raw::c_uint,
        ) -> std::os::raw::c_int;
    }
    const AT_FDCWD: std::os::raw::c_int = -100;
    const RENAME_NOREPLACE: std::os::raw::c_uint = 1; // renameat2(2)
    // Names never contain NUL; fail visibly instead of truncating if one slips through.
    let cvt = |p: &std::path::Path| {
        std::ffi::CString::new(p.as_os_str().as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
    };
    let (old, new) = (cvt(old)?, cvt(new)?);
    // SAFETY: NUL-terminated buffers outlive the call; the rest are integers.
    let r = unsafe {
        renameat2(
            AT_FDCWD,
            old.as_ptr(),
            AT_FDCWD,
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
