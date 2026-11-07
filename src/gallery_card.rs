//! Gallery card UI: mode toggle, filter picker, file list.
//!
//! Shown when the probe finds an image gallery. The user picks a filter
//! (All/Images/Videos), previews the available items, then adds the download.

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
    /// Source URL, used when the user opens the explicit file picker.
    url: String,
}

impl GalleryCard {
    pub fn new(items: Vec<GalleryItem>, sticky: Option<StickyMode>, url: String) -> Self {
        Self {
            filter: Rc::new(RefCell::new(GalleryFilter::All)),
            items,
            sticky,
            url,
        }
    }

    /// The gallery URL used to enumerate files for the explicit picker.
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

