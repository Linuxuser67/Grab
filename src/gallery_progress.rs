//! gallery-dl progress parsing: pipe-mode stdout line parser.
//!
//! gallery-dl's pipe output mode (customized via `output.mode` in gallery_argv)
//! emits:
//! - Success: `<full-path>\n` (bare path, no prefix)
//! - Skip: `# <full-path>\n` (`# ` prefix)
//! - Progress: `\r<bytes>/<total>` or `\r<pct>% <bytes>/<total>` (carriage-return prefixed)
//!
//! This is a pure line parser; the caller feeds it stdout lines.

/// A parsed progress line from gallery-dl's pipe output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressLine {
    /// File completed: full path to the downloaded file.
    Completed(String),
    /// File skipped (already exists): full path.
    Skipped(String),
    /// In-progress: bytes downloaded, total bytes (if known).
    Progress { bytes: u64, total: Option<u64> },
    /// Unrecognized line (log noise, warnings).
    Other(String),
}

/// Parse a single stdout line from gallery-dl's pipe output.
pub fn parse_progress_line(line: &str) -> ProgressLine {
    let line = line.strip_suffix('\n').unwrap_or(line);

    // Skip lines: `# <path>`
    if let Some(path) = line.strip_prefix("# ") {
        return ProgressLine::Skipped(path.to_string());
    }

    // Progress lines start with `\r` (carriage return to overwrite).
    // `parse_progress` trims it; we detect by the `/` and numeric shape.
    if let Some(progress) = parse_progress(line) {
        return progress;
    }

    // Success lines: bare path (non-empty, looks like a path)
    if !line.trim().is_empty() && line.contains('/') {
        return ProgressLine::Completed(line.to_string());
    }

    ProgressLine::Other(line.to_string())
}

/// Parse `\r`-style progress: `<bytes>/<total>` or `<pct>% <bytes>/<total>`.
fn parse_progress(line: &str) -> Option<ProgressLine> {
    // Format: "12345/67890" or "18% 12345/67890"
    let line = line.trim();
    let (bytes_part, total_part) = if let Some(pct_pos) = line.find("% ") {
        // "18% 12345/67890" → take the part after "% "
        let rest = &line[pct_pos + 2..];
        rest.split_once('/')?
    } else {
        // "12345/67890"
        line.split_once('/')?
    };

    let bytes: u64 = bytes_part.trim().parse().ok()?;
    let total: Option<u64> = total_part.trim().parse().ok();

    Some(ProgressLine::Progress { bytes, total })
}

/// Aggregate progress across multiple files: total bytes downloaded vs
/// total expected (sum of per-file totals where known).
#[derive(Debug, Default)]
pub struct GalleryProgress {
    pub files_completed: u32,
    pub files_skipped: u32,
    pub bytes_downloaded: u64,
    pub bytes_total: Option<u64>,
    // Per-file progress: gallery-dl emits `\r` lines per file, not cumulative.
    // We track the current file's bytes and add its total on completion.
    current_file_bytes: u64,
    current_file_total: Option<u64>,
}

impl GalleryProgress {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a parsed line; returns the completed file path if a file finished.
    pub fn feed(&mut self, line: ProgressLine) -> Option<String> {
        match line {
            ProgressLine::Completed(path) => {
                self.files_completed += 1;
                // Add the completed file's size to the cumulative total.
                if let Some(t) = self.current_file_total {
                    self.bytes_downloaded += t;
                    self.bytes_total = Some(self.bytes_total.unwrap_or(0) + t);
                }
                self.current_file_bytes = 0;
                self.current_file_total = None;
                Some(path)
            }
            ProgressLine::Skipped(_) => {
                self.files_skipped += 1;
                self.current_file_bytes = 0;
                self.current_file_total = None;
                None
            }
            ProgressLine::Progress { bytes, total } => {
                // Per-file progress: track max for the current file.
                self.current_file_bytes = self.current_file_bytes.max(bytes);
                if let Some(t) = total {
                    self.current_file_total = Some(self.current_file_total.unwrap_or(0).max(t));
                }
                None
            }
            ProgressLine::Other(_) => None,
        }
    }

    /// Overall progress as a fraction (0.0-1.0), if totals are known.
    /// Used by the gallery row UI (phase 6) for the progress bar.
    #[allow(dead_code)]
    pub fn fraction(&self) -> Option<f64> {
        self.bytes_total
            .filter(|&t| t > 0)
            .map(|t| (self.bytes_downloaded as f64 / t as f64).min(1.0))
    }

    /// Current file's progress fraction (for per-file UI), if known.
    /// Used by the gallery row UI (phase 6).
    #[allow(dead_code)]
    pub fn current_file_fraction(&self) -> Option<f64> {
        self.current_file_total
            .filter(|&t| t > 0)
            .map(|t| (self.current_file_bytes as f64 / t as f64).min(1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_completed() {
        assert_eq!(
            parse_progress_line("/tmp/dest/image.jpg\n"),
            ProgressLine::Completed("/tmp/dest/image.jpg".to_string())
        );
    }

    #[test]
    fn parse_skipped() {
        assert_eq!(
            parse_progress_line("# /tmp/dest/image.jpg\n"),
            ProgressLine::Skipped("/tmp/dest/image.jpg".to_string())
        );
    }

    #[test]
    fn parse_progress_bytes() {
        assert_eq!(
            parse_progress_line("\r12345/67890"),
            ProgressLine::Progress {
                bytes: 12345,
                total: Some(67890)
            }
        );
    }

    #[test]
    fn parse_progress_percent() {
        assert_eq!(
            parse_progress_line("\r18% 12345/67890"),
            ProgressLine::Progress {
                bytes: 12345,
                total: Some(67890)
            }
        );
    }

    #[test]
    fn parse_other() {
        assert_eq!(
            parse_progress_line("some log noise\n"),
            ProgressLine::Other("some log noise".to_string())
        );
        assert_eq!(
            parse_progress_line("\n"),
            ProgressLine::Other(String::new())
        );
    }

    #[test]
    fn progress_aggregation() {
        let mut p = GalleryProgress::new();
        // File 1: 1000 bytes total
        p.feed(parse_progress_line("\r500/1000"));
        assert_eq!(p.current_file_fraction(), Some(0.5));
        p.feed(parse_progress_line("\r1000/1000"));
        assert_eq!(
            p.feed(parse_progress_line("/a/1.jpg\n")),
            Some("/a/1.jpg".to_string())
        );
        assert_eq!(p.files_completed, 1);
        assert_eq!(p.bytes_downloaded, 1000);
        assert_eq!(p.bytes_total, Some(1000));
        assert_eq!(p.fraction(), Some(1.0));

        // File 2: skipped
        assert_eq!(p.feed(parse_progress_line("# /a/2.jpg\n")), None);
        assert_eq!(p.files_skipped, 1);

        // File 3: 2000 bytes total
        p.feed(parse_progress_line("\r1000/2000"));
        p.feed(parse_progress_line("/a/3.jpg\n"));
        assert_eq!(p.files_completed, 2);
        assert_eq!(p.bytes_downloaded, 3000);
        assert_eq!(p.bytes_total, Some(3000));
        assert_eq!(p.fraction(), Some(1.0));
    }

    #[test]
    fn progress_per_file_not_cumulative() {
        // Two files, 1000 bytes each. Progress lines are per-file;
        // bytes_downloaded must sum, not max.
        let mut p = GalleryProgress::new();
        p.feed(parse_progress_line("\r1000/1000"));
        p.feed(parse_progress_line("/a/1.jpg\n"));
        p.feed(parse_progress_line("\r1000/1000"));
        p.feed(parse_progress_line("/a/2.jpg\n"));
        assert_eq!(p.bytes_downloaded, 2000);
        assert_eq!(p.bytes_total, Some(2000));
    }
}
