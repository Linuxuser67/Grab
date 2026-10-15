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
}

impl GalleryCard {
    pub fn new(items: Vec<GalleryItem>, sticky: Option<StickyMode>) -> Self {
        Self {
            filter: Rc::new(RefCell::new(GalleryFilter::All)),
            items,
            sticky,
        }
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

    /// Set the filter from the UI picker index (0=All, 1=Images, 2=Videos).
    pub fn set_filter_index(&self, index: u32) {
        let filter = match index {
            1 => GalleryFilter::ImagesOnly,
            2 => GalleryFilter::VideosOnly,
            _ => GalleryFilter::All,
        };
        *self.filter.borrow_mut() = filter;
    }

    /// Get the sticky mode for this host (for the mode toggle).
    pub fn sticky_mode(&self) -> StickyMode {
        self.sticky.unwrap_or(StickyMode::Video)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        );
        assert_eq!(card.file_count(), 2);
    }

    #[test]
    fn filter_images_only() {
        let card = GalleryCard::new(
            vec![test_item("a.jpg", "jpg"), test_item("b.mp4", "mp4")],
            None,
        );
        card.set_filter_index(1);
        assert_eq!(card.file_count(), 1);
        assert_eq!(card.preview_filenames(), vec!["a.jpg".to_string()]);
    }

    #[test]
    fn filter_videos_only() {
        let card = GalleryCard::new(
            vec![test_item("a.jpg", "jpg"), test_item("b.mp4", "mp4")],
            None,
        );
        card.set_filter_index(2);
        assert_eq!(card.file_count(), 1);
    }

    #[test]
    fn sticky_defaults_to_video() {
        let card = GalleryCard::new(vec![], None);
        assert_eq!(card.sticky_mode(), StickyMode::Video);
    }

    #[test]
    fn sticky_gallery_honored() {
        let card = GalleryCard::new(vec![], Some(StickyMode::Gallery));
        assert_eq!(card.sticky_mode(), StickyMode::Gallery);
    }
}
