//! Domain types shared by native manifest intake, planning, and execution.

use crate::manifest::{HlsMasterPlan, HlsPlan, dash::DashManifestPlan};
use crate::media_types::{
    ManifestAudioKey, ManifestKind, ManifestRepresentationKey, ManifestSubtitleKey,
};

/// Shared ceilings for manifest parsing and persisted live state. Keeping the
/// coupled parser/runner limits here prevents a manifest being accepted by one
/// layer and rejected by another solely because their caps drifted.
pub(crate) const MAX_MANIFEST_SEGMENTS: usize = 100_000;
pub(crate) const MAX_MANIFEST_TOTAL_BYTES: u64 = 1 << 40;

/// One selectable video choice from a parsed native manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManifestChoice {
    pub(crate) key: ManifestRepresentationKey,
    pub(crate) label: String,
    pub(crate) height: Option<u64>,
    pub(crate) bandwidth: Option<u64>,
}

/// One selectable audio rendition associated with a selected video representation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManifestAudioChoice {
    pub(crate) key: ManifestAudioKey,
    pub(crate) label: String,
    pub(crate) is_default: bool,
    pub(crate) is_autoselect: bool,
}

/// One selectable subtitle rendition associated with a selected video representation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManifestSubtitleChoice {
    pub(crate) key: ManifestSubtitleKey,
    pub(crate) language: Option<String>,
    pub(crate) is_default: bool,
    pub(crate) is_autoselect: bool,
    pub(crate) is_forced: bool,
}

/// Parsed native presentation, before the user's representation selection is applied.
#[derive(Clone, Debug)]
pub(crate) enum NativeManifest {
    HlsMaster(Box<HlsMasterPlan>),
    HlsMedia(Box<HlsPlan>),
    Dash(DashManifestPlan),
}

/// Parsed manifest associated with the exact URL entered by the user.
///
/// The source URL is the freshness key used by the inline card. Child resources
/// have already been resolved against the post-redirect URL.
#[derive(Clone, Debug)]
pub(crate) struct ManifestLookup {
    pub(crate) source_url: String,
    pub(crate) manifest: NativeManifest,
}

impl ManifestLookup {
    /// Protocol represented by the parsed presentation.
    #[must_use]
    pub(crate) fn kind(&self) -> ManifestKind {
        match &self.manifest {
            NativeManifest::HlsMaster(_) | NativeManifest::HlsMedia(_) => ManifestKind::Hls,
            NativeManifest::Dash(_) => ManifestKind::Dash,
        }
    }
}
