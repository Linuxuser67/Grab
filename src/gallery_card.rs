//! Gallery card UI: mode toggle, filter picker, file list.
//!
//! Shown when the probe finds an image gallery. The user picks a filter
//! (All/Images/Videos), sees the file count and list, then adds the download.

use std::cell::RefCell;
use std::rc::Rc;

use crate::gallery_argv::GalleryFilter;
use crate::gallery_probe::{GalleryItem, StickyMode};

/// Gallery card state: filter selection and file list.
pub struct GalleryCard {
    /// Currently selected filter.
    pub filter: Rc<RefCell<GalleryFilter>>,
    /// Probe items (for the file list).
    pub items: Vec<GalleryItem>,
    /// Per-host sticky mode preference.
    pub sticky: Option<StickyMode>,
    /// Total file counts per filter (from `count_gallery_files`).
    /// `None` = not yet counted.
    total: RefCell<std::collections::HashMap<GalleryFilter, u32>>,
    /// Source URL (for filter-toggle recounts).
    url: String,
}

impl GalleryCard {
    pub fn new(items: Vec<GalleryItem>, sticky: Option<StickyMode>, url: String) -> Self {
        Self {
            filter: Rc::new(RefCell::new(GalleryFilter::All)),
            items,
            sticky,
            total: RefCell::new(std::collections::HashMap::new()),
            url,
        }
    }

    /// The gallery URL (for recounts).
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Store the total file count for a filter.
    pub fn set_total(&self, filter: GalleryFilter, n: u32) {
        self.total.borrow_mut().insert(filter, n);
    }

    /// Get the cached total for a filter, if counted.
    pub fn total(&self, filter: GalleryFilter) -> Option<u32> {
        self.total.borrow().get(&filter).copied()
    }

    /// Filtered items based on the current filter selection.
    pub fn filtered_items(&self) -> Vec<&GalleryItem> {
        let filter = *self.filter.borrow();
        self.items
            .iter()
            .filter(|item| match filter {
                GalleryFilter::All => true,
                GalleryFilter::ImagesOnly => {
                    crate::gallery_probe::GalleryContent::from_extension(&item.extension)
                        == crate::gallery_probe::GalleryContent::Image
                }
                GalleryFilter::VideosOnly => {
                    crate::gallery_probe::GalleryContent::from_extension(&item.extension)
                        == crate::gallery_probe::GalleryContent::Video
                }
            })
            .collect()
    }

    /// File count for the current filter.
    pub fn file_count(&self) -> usize {
        self.filtered_items().len()
    }

    /// Filenames for the file list expander (first 5).
    pub fn preview_filenames(&self) -> Vec<String> {
        self.filtered_items()
            .iter()
            .take(5)
            .map(|item| item.filename.clone())
            .collect()
    }

    /// Get the sticky mode for this host (for the mode toggle).
    pub fn sticky_mode(&self) -> StickyMode {
        self.sticky.unwrap_or(StickyMode::Video)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test helper: set filter by picker index (UI will call this in phase 7).
    fn set_filter_index(card: &GalleryCard, index: u32) {
        let filter = match index {
            1 => GalleryFilter::ImagesOnly,
            2 => GalleryFilter::VideosOnly,
            _ => GalleryFilter::All,
        };
        *card.filter.borrow_mut() = filter;
    }

    fn test_item(filename: &str, extension: &str) -> GalleryItem {
        GalleryItem {
            filename: filename.to_string(),
            extension: extension.to_string(),
            category: "test".to_string(),
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn filter_all_shows_everything() {
        let card = GalleryCard::new(
            vec![test_item("a.jpg", "jpg"), test_item("b.mp4", "mp4")],
            None,
            "https://example.com".to_string(),
        );
        assert_eq!(card.file_count(), 2);
    }

    #[test]
    fn filter_images_only() {
        let card = GalleryCard::new(
            vec![test_item("a.jpg", "jpg"), test_item("b.mp4", "mp4")],
            None,
            "https://example.com".to_string(),
        );
        set_filter_index(&card, 1);
        assert_eq!(card.file_count(), 1);
        assert_eq!(card.preview_filenames(), vec!["a.jpg".to_string()]);
    }

    #[test]
    fn sticky_defaults_to_video() {
        let card = GalleryCard::new(vec![], None, "https://example.com".to_string());
        assert_eq!(card.sticky_mode(), StickyMode::Video);
    }

    #[test]
    fn sticky_gallery_honored() {
        let card = GalleryCard::new(
            vec![],
            Some(StickyMode::Gallery),
            "https://example.com".to_string(),
        );
        assert_eq!(card.sticky_mode(), StickyMode::Gallery);
    }

    #[test]
    fn filter_videos_only() {
        let card = GalleryCard::new(
            vec![test_item("a.jpg", "jpg"), test_item("b.mp4", "mp4")],
            None,
            "https://example.com".to_string(),
        );
        set_filter_index(&card, 2);
        assert_eq!(card.file_count(), 1);
    }
}

/// Display rule for the gallery file count.
/// - `total`: exact count from `count_gallery_files`, if known.
/// - `items_len`: number of probe items (capped at 5 by `--range 1-5`).
/// - `cap`: the probe cap (5).
///
/// When total is unknown and we've hit the cap, show "5+ files".
pub fn display_count(items_len: usize, cap: usize, total: Option<u32>) -> String {
    match total {
        Some(n) => {
            if n == 1 {
                "1 file".to_string()
            } else {
                format!("{n} files")
            }
        }
        None if items_len >= cap => format!("{cap}+ files"),
        None => {
            if items_len == 1 {
                "1 file".to_string()
            } else {
                format!("{items_len} files")
            }
        }
    }
}

#[cfg(test)]
mod display_count_tests {
    use super::*;

    #[test]
    fn exact_count() {
        assert_eq!(display_count(5, 5, Some(42)), "42 files");
        assert_eq!(display_count(5, 5, Some(1)), "1 file");
    }

    #[test]
    fn capped_unknown() {
        assert_eq!(display_count(5, 5, None), "5+ files");
    }

    #[test]
    fn uncapped_unknown() {
        assert_eq!(display_count(3, 5, None), "3 files");
        assert_eq!(display_count(1, 5, None), "1 file");
    }
}
