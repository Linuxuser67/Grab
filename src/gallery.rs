//! gallery-dl integration: image galleries and fallback video downloads.
//!
//! Facade over the gallery cluster: tool provisioning, probe/JSON parsing,
//! progress parsing, argv builders, and download orchestration. Phase 1 ships
//! only the tool plumbing; later phases add the rest.

/// Facade: tool provisioning lives in [`gallery_tools`](crate::gallery_tools) now.
pub use crate::gallery_tools::{
    GalleryError, MIN_GALLERYDL_VERSION, gallerydl_display_version, gallerydl_update_available,
    install_gallerydl, latest_gallerydl_asset, latest_gallerydl_tag, parse_gallerydl_version,
    resolve_gallerydl, version_supported,
};
