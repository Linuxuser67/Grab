//! Row object: `DownloadItem` GObject subclass, wrapper and accessors.

use crate::download_store::DownloadGroup;
use gtk4::glib;
use gtk4::glib::subclass::types::ObjectSubclassIsExt;

/// Mutually exclusive structural role of a queue row.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, glib::Enum)]
#[enum_type(name = "GrabDownloadRowRole")]
pub(crate) enum DownloadRowRole {
    #[default]
    Plain,
    GalleryOwner,
    PlaylistOwner,
    Child,
}

mod imp {
    use std::cell::{Cell, RefCell};

    use crate::download_store::{DownloadGroup, DownloadStatus};
    use super::DownloadRowRole;
    use gtk4::glib;
    use gtk4::glib::object::ObjectExt as _;
    use gtk4::glib::subclass::prelude::*;

    #[derive(Debug, Default, glib::Properties)]
    #[properties(wrapper_type = super::DownloadItem)]
    pub struct DownloadItem {
        #[property(get, set)]
        pub id: Cell<u64>,
        #[property(get, set)]
        pub url: RefCell<String>,
        #[property(get, set)]
        pub filename: RefCell<String>,
        #[property(get, set)]
        pub dest_dir: RefCell<String>,
        #[property(get, set, builder(DownloadStatus::Queued))]
        pub status: Cell<DownloadStatus>,
        #[property(get, set)]
        pub progress: Cell<f64>,
        #[property(get, set)]
        pub detail: RefCell<String>,
        /// Engine's real output folder for torrents; empty means unknown, use `file_path()`.
        #[property(get, set)]
        pub output_dir: RefCell<String>,
        /// Unix timestamp when a scheduled download should start; 0 = not scheduled.
        #[property(get, set)]
        pub scheduled_at: Cell<i64>,
        /// Resume validator: ETag from the server (empty = none). Sent as
        /// If-Range on resume so a changed file restarts instead of splicing.
        #[property(get, set)]
        pub etag: RefCell<String>,
        /// Resume validator fallback: Last-Modified header value (empty = none).
        #[property(get, set)]
        pub last_modified: RefCell<String>,
        /// Public-only network policy inherited from an untrusted browser handoff.
        #[property(get, set)]
        pub public_only: Cell<bool>,
        /// Mutually exclusive row role: ordinary item, gallery owner,
        /// playlist owner, or read-only collection child.
        #[property(get, set, builder(DownloadRowRole::Plain))]
        pub role: Cell<DownloadRowRole>,
        /// Stable extractor/file identity for yt-dlp playlist child rows.
        #[property(get, set)]
        pub collection_file_id: RefCell<String>,
        /// Exact output path announced by the owning downloader.
        #[property(get, set)]
        pub gallery_file_path: RefCell<String>,
        /// Progress denominator for a gallery download (0 if unknown or not a gallery).
        #[property(get, set)]
        pub gallery_file_count: Cell<u32>,
        /// Files downloaded so far (for progress).
        #[property(get, set)]
        pub gallery_files_done: Cell<u32>,
        /// Source page URL for the gallery (for retry).
        #[property(get, set)]
        pub gallery_url: RefCell<String>,
        /// Full error log for the failure dialog; `detail` holds the summary.
        #[property(get, set)]
        pub error_log: RefCell<String>,
        /// Optional multi-item source collection shared by this child row.
        pub(crate) group: RefCell<Option<DownloadGroup>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for DownloadItem {
        const NAME: &'static str = "GrabDownloadItem";
        type Type = super::DownloadItem;
    }

    #[glib::derived_properties]
    impl ObjectImpl for DownloadItem {}
}

glib::wrapper! {
    pub struct DownloadItem(ObjectSubclass<imp::DownloadItem>);
}

impl DownloadItem {
    /// Whether this row owns the queue entry for a gallery or video collection.
    pub(crate) fn is_collection_parent(&self) -> bool {
        matches!(self.role(), DownloadRowRole::GalleryOwner | DownloadRowRole::PlaylistOwner)
    }

    pub fn is_gallery(&self) -> bool {
        self.role() == DownloadRowRole::GalleryOwner
    }

    pub fn set_is_gallery(&self, value: bool) {
        self.set_role(if value {
            DownloadRowRole::GalleryOwner
        } else if self.is_gallery() {
            DownloadRowRole::Plain
        } else {
            self.role()
        });
    }

    pub fn is_collection_owner(&self) -> bool {
        self.role() == DownloadRowRole::PlaylistOwner
    }

    pub fn set_is_collection_owner(&self, value: bool) {
        self.set_role(if value {
            DownloadRowRole::PlaylistOwner
        } else if self.is_collection_owner() {
            DownloadRowRole::Plain
        } else {
            self.role()
        });
    }

    pub fn is_collection_child(&self) -> bool {
        self.role() == DownloadRowRole::Child
    }

    pub fn set_is_collection_child(&self, value: bool) {
        self.set_role(if value {
            DownloadRowRole::Child
        } else if self.is_collection_child() {
            DownloadRowRole::Plain
        } else {
            self.role()
        });
    }

    /// The collection this child belongs to, if it was queued from a multi-item source.
    pub(crate) fn group(&self) -> Option<DownloadGroup> {
        self.imp().group.borrow().clone()
    }

    /// Set or clear this row's collection membership.
    pub(crate) fn set_group(&self, group: Option<DownloadGroup>) {
        *self.imp().group.borrow_mut() = group;
    }

    /// Create a list item; prefer [`DownloadManager::enqueue`] which dedupes.
    pub fn new(id: u64, url: &str, filename: &str, dest_dir: &str) -> Self {
        glib::Object::builder()
            .property("id", id)
            .property("url", url)
            .property("filename", filename)
            .property("dest-dir", dest_dir)
            .build()
    }

    /// Full destination path (`dest_dir` joined with `filename`).
    pub fn file_path(&self) -> std::path::PathBuf {
        std::path::Path::new(&self.dest_dir()).join(self.filename())
    }

    /// Best on-disk guess for reveal: engine folder when known, else destination path.
    pub fn display_path(&self) -> std::path::PathBuf {
        let output_dir = self.output_dir();
        if output_dir.is_empty() {
            self.file_path()
        } else {
            std::path::Path::new(&output_dir).join(self.filename())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DownloadItem;

    #[test]
    fn collection_role_is_mutually_exclusive() {
        let item = DownloadItem::new(1, "https://example.com/collection", "Collection", "/tmp");
        item.set_is_gallery(true);
        item.set_is_collection_owner(true);
        assert!(!item.is_gallery());
        assert!(item.is_collection_owner());
        assert!(!item.is_collection_child());

        item.set_is_collection_child(true);
        assert!(!item.is_collection_owner());
        assert!(item.is_collection_child());
    }

    #[test]
    fn collection_parent_includes_gallery_and_video_owners_only() {
        let gallery = DownloadItem::new(1, "https://example.com/gallery", "Gallery", "/tmp");
        gallery.set_is_gallery(true);
        assert!(gallery.is_collection_parent());

        let playlist = DownloadItem::new(2, "https://example.com/playlist", "Playlist", "/tmp");
        playlist.set_is_collection_owner(true);
        assert!(playlist.is_collection_parent());

        let child = DownloadItem::new(3, "https://example.com/item", "Item", "/tmp");
        child.set_is_collection_child(true);
        assert!(!child.is_collection_parent());

        let ordinary = DownloadItem::new(4, "https://example.com/file", "File", "/tmp");
        assert!(!ordinary.is_collection_parent());
    }
}
