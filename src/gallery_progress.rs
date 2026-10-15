//! gallery-dl progress parsing: pipe-mode stdout line parser.
//!
//! gallery-dl's pipe output mode (customized via `output.mode` in gallery_argv)
//! emits:
//! - Success: `<full-path>\n` (bare path, no prefix)
//! - Skip: `# <full-path>\n` (`# ` prefix)
//!
//! Byte-level progress is disabled (`downloader.http.progress=null`), so
//! progress is tracked by file counts only. This is a pure line parser;
//! the caller feeds it stdout lines.

/// A parsed progress line from gallery-dl's pipe output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressLine {
    /// File completed: full path to the downloaded file.
    Completed(String),
    /// File skipped (already exists): full path.
    Skipped(String),
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

    // Success lines: bare path (non-empty, looks like a path)
    if !line.trim().is_empty() && line.contains('/') {
        return ProgressLine::Completed(line.to_string());
    }

    ProgressLine::Other(line.to_string())
}

/// Aggregate progress across multiple files: file counts only.
#[derive(Debug, Default)]
pub struct GalleryProgress {
    pub files_completed: u32,
    pub files_skipped: u32,
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
                Some(path)
            }
            ProgressLine::Skipped(_) => {
                self.files_skipped += 1;
                None
            }
            ProgressLine::Other(_) => None,
        }
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
    fn parse_other() {
        assert_eq!(
            parse_progress_line("some log noise\n"),
            ProgressLine::Other("some log noise".to_string())
        );
    }

    #[test]
    fn feed_counts() {
        let mut p = GalleryProgress::new();
        assert_eq!(
            p.feed(parse_progress_line("/a.jpg\n")),
            Some("/a.jpg".to_string())
        );
        assert_eq!(p.feed(parse_progress_line("# /b.jpg\n")), None);
        assert_eq!(p.feed(parse_progress_line("noise\n")), None);
        assert_eq!(p.files_completed, 1);
        assert_eq!(p.files_skipped, 1);
    }
}
