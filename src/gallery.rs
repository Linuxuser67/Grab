//! gallery-dl integration: image galleries and fallback video downloads.
//!
//! Facade over the gallery cluster: tool provisioning, probe/JSON parsing,
//! progress parsing, argv builders, and download orchestration. Phase 1 ships
//! the tool plumbing; phase 2 adds probe + routing.
//!
//! Note: `gallery_probe` items are `pub` for the upcoming card integration
//! (phase 4); they have no non-test callers yet.
/// Facade: tool provisioning lives in [`gallery_tools`](crate::gallery_tools) now.
pub use crate::gallery_tools::{
    gallerydl_display_version, gallerydl_display_version_sync, gallerydl_update_available,
    install_gallerydl, install_gallerydl_sync, latest_gallerydl_tag, latest_gallerydl_tag_sync,
    resolve_gallerydl,
};
