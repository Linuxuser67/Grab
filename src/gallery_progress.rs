//! Parse gallery-dl file events and per-file progress records.
//!
//! The custom output mode keeps completed/skipped paths on stdout and sends
//! percentage records to stderr. Both streams feed this pure line parser.

/// A parsed file event or progress record from gallery-dl.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressLine {
    /// File completed: full path to the downloaded file.
    Completed(String),
    /// File skipped (already exists): full path.
    Skipped(String),
    /// Current file's percentage from gallery-dl's stderr progress indicator.
    Progress(Option<u8>),
    /// Unrecognized line (log noise, warnings).
    Other(String),
}

/// Parse one stdout file event or stderr progress record from gallery-dl.
pub fn parse_progress_line(line: &str) -> ProgressLine {
    let line = line.strip_suffix('\n').unwrap_or(line);

    let trimmed = line.trim();
    if trimmed == "?" {
        return ProgressLine::Progress(None);
    }
    if let Some(percent) = trimmed
        .strip_suffix('%')
        .and_then(|value| value.trim().parse::<u8>().ok())
        .filter(|percent| *percent <= 100)
    {
        return ProgressLine::Progress(Some(percent));
    }

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

/// Calculate the aggregate fraction from completed files and the current file.
pub fn progress_fraction(
    files_done: u32,
    current_file_percent: Option<u8>,
    total_files: u32,
) -> Option<f64> {
    if total_files == 0 || files_done > total_files {
        return None;
    }
    let current = current_file_percent.unwrap_or(0) as f64 / 100.0;
    Some(((files_done as f64 + current) / total_files as f64).min(1.0))
}

/// Aggregate file completion and the current file's percentage.
#[derive(Debug, Default)]
pub struct GalleryProgress {
    pub files_completed: u32,
    pub files_skipped: u32,
    pub current_file_percent: Option<u8>,
}

impl GalleryProgress {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of completed or skipped files in the current gallery-dl run.
    pub fn files_done(&self) -> u32 {
        self.files_completed.saturating_add(self.files_skipped)
    }

    /// Feed a parsed line; returns the completed file path if a file finished.
    pub fn feed(&mut self, line: ProgressLine) -> Option<String> {
        match line {
            ProgressLine::Completed(path) => {
                self.files_completed = self.files_completed.saturating_add(1);
                self.current_file_percent = None;
                Some(path)
            }
            ProgressLine::Skipped(_) => {
                self.files_skipped = self.files_skipped.saturating_add(1);
                self.current_file_percent = None;
                None
            }
            ProgressLine::Progress(percent) => {
                self.current_file_percent = percent;
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
    fn parse_progress_percent_and_unknown_total() {
        assert_eq!(parse_progress_line("42%"), ProgressLine::Progress(Some(42)));
        assert_eq!(parse_progress_line("?"), ProgressLine::Progress(None));
        assert_eq!(parse_progress_line("101%"), ProgressLine::Other("101%".to_string()));
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
        assert_eq!(p.files_done(), 2);
    }

    #[test]
    fn progress_fraction_combines_completed_files_and_current_file() {
        assert_eq!(progress_fraction(3, Some(50), 10), Some(0.35));
        assert_eq!(progress_fraction(10, Some(50), 10), Some(1.0));
        assert_eq!(progress_fraction(0, None, 0), None);
        assert_eq!(progress_fraction(11, None, 10), None);

        let mut resumed = GalleryProgress::new();
        resumed.feed(ProgressLine::Skipped("/a.jpg".to_string()));
        resumed.feed(ProgressLine::Skipped("/b.jpg".to_string()));
        assert_eq!(progress_fraction(resumed.files_done(), Some(50), 4), Some(0.625));
    }

    #[test]
    fn progress_is_reset_when_a_file_finishes_or_is_skipped() {
        let mut progress = GalleryProgress::new();
        progress.feed(ProgressLine::Progress(Some(67)));
        assert_eq!(progress.current_file_percent, Some(67));

        progress.feed(ProgressLine::Completed("/a.jpg".to_string()));
        assert_eq!(progress.current_file_percent, None);

        progress.feed(ProgressLine::Progress(Some(25)));
        progress.feed(ProgressLine::Skipped("/b.jpg".to_string()));
        assert_eq!(progress.current_file_percent, None);
        assert_eq!(progress.files_done(), 2);
    }
}
