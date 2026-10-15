//! gallery-dl integration: image galleries and fallback video downloads.
//!
//! Facade over the gallery cluster: tool provisioning, probe/JSON parsing,
//! progress parsing, argv builders, and download orchestration. Phase 1 ships
//! the tool plumbing; phase 2 adds probe + routing.

/// Facade: probe + routing lives in [`gallery_probe`](crate::gallery_probe) now.
pub use crate::gallery_probe::{
    GalleryContent, GalleryItem, ProbeOutcome, StickyMode, count_gallery_files, probe_gallery,
    probe_title, route,
};
/// Facade: tool provisioning lives in [`gallery_tools`](crate::gallery_tools) now.
pub use crate::gallery_tools::{
    gallerydl_display_version, gallerydl_update_available, install_gallerydl, latest_gallerydl_tag,
    resolve_gallerydl,
};
