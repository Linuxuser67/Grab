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
    /// Source URL, used to count the complete filtered listing before Queue All.
    url: String,
}

impl GalleryCard {
    pub fn new(items: Vec<GalleryItem>, url: String) -> Self {
        Self {
            filter: Rc::new(RefCell::new(GalleryFilter::All)),
            items,
            url,
        }
    }

    /// The gallery URL (for recounts).
    pub fn url(&self) -> &str {
        &self.url
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

    /// Exact count for the active filter when the fast preview is complete.
    /// Reaching the preview cap means the listing may contain more items.
    pub fn exact_file_count(&self) -> Option<u32> {
        if self.items.len() >= crate::gallery_probe::GALLERY_PREVIEW_ITEM_LIMIT {
            return None;
        }
        u32::try_from(self.file_count()).ok()
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

    #[test]
    fn exact_count_uses_the_complete_filtered_preview() {
        let card = GalleryCard::new(
            vec![
                test_item("a.jpg", "jpg"),
                test_item("b.png", "png"),
                test_item("c.mp4", "mp4"),
                test_item("d.xyz", "xyz"),
            ],
            "https://example.com".to_string(),
        );
        set_filter_index(&card, 1);
        assert_eq!(card.exact_file_count(), Some(2));
    }

    #[test]
    fn exact_count_is_unknown_when_preview_reaches_its_cap() {
        let card = GalleryCard::new(
            vec![
                test_item("a.jpg", "jpg"),
                test_item("b.png", "png"),
                test_item("c.mp4", "mp4"),
                test_item("d.xyz", "xyz"),
                test_item("e.webm", "webm"),
            ],
            "https://example.com".to_string(),
        );
        assert_eq!(card.exact_file_count(), None);
    }
}
