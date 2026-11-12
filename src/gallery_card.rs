//! Gallery card UI: mode toggle, filter picker, file list.
//!
//! Shown when the probe finds an image gallery. The user picks a filter
//! (All/Images/Videos), previews the available items, then adds the download.

use std::cell::RefCell;
use std::rc::Rc;

use crate::gallery_argv::GalleryFilter;
use crate::gallery_probe::GalleryItem;

/// Gallery card state: filter selection and file list.
pub struct GalleryCard {
    /// Currently selected filter.
    pub filter: Rc<RefCell<GalleryFilter>>,
    /// Probe items (for the file list).
    pub items: Vec<GalleryItem>,
    /// Per-filter file totals used as progress denominators when available.
    /// `None` = not yet counted or the count request failed.
    total: RefCell<std::collections::HashMap<GalleryFilter, Option<u32>>>,
    /// Source URL, used to retrieve progress totals when the filter changes.
    url: String,
}

impl GalleryCard {
    pub fn new(items: Vec<GalleryItem>, url: String) -> Self {
        Self {
            filter: Rc::new(RefCell::new(GalleryFilter::All)),
            items,
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
        self.total.borrow_mut().insert(filter, Some(n));
    }

    /// Mark a filter's count as attempted but failed (don't retry on every toggle).
    pub fn set_total_failed(&self, filter: GalleryFilter) {
        self.total.borrow_mut().insert(filter, None);
    }

    /// Get the cached total for a filter, if counted successfully.
    pub fn total(&self, filter: GalleryFilter) -> Option<u32> {
        self.total.borrow().get(&filter).copied().flatten()
    }

    /// Whether a count has been attempted for a filter (success or failure).
    pub fn is_count_cached(&self, filter: GalleryFilter) -> bool {
        self.total.borrow().contains_key(&filter)
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
            "https://example.com".to_string(),
        );
        assert_eq!(card.file_count(), 2);
    }

    #[test]
    fn filter_images_only() {
        let card = GalleryCard::new(
            vec![test_item("a.jpg", "jpg"), test_item("b.mp4", "mp4")],
            "https://example.com".to_string(),
        );
        set_filter_index(&card, 1);
        assert_eq!(card.file_count(), 1);
    }

    #[test]
    fn filter_videos_only() {
        let card = GalleryCard::new(
            vec![test_item("a.jpg", "jpg"), test_item("b.mp4", "mp4")],
            "https://example.com".to_string(),
        );
        set_filter_index(&card, 2);
        assert_eq!(card.file_count(), 1);
    }
}
