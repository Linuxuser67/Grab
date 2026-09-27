//! Queue persistence: row status, versioned queue file and stored items.

use crate::download_pieces::SegmentState;
use gettextrs::gettext;
use gtk4::glib;

#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, glib::Enum, serde::Serialize, serde::Deserialize,
)]
#[enum_type(name = "GrabDownloadStatus")]
#[serde(rename_all = "lowercase")]
pub enum DownloadStatus {
    #[default]
    Queued,
    Downloading,
    Paused,
    Done,
    Failed,
    Cancelled,
}

impl DownloadStatus {
    /// Short human-readable label for the status, for list rows and toasts.
    pub fn label(self) -> String {
        // gettext() here (not call sites) so xgettext extracts every msgid.
        match self {
            DownloadStatus::Queued => gettext("Queued"),
            DownloadStatus::Downloading => gettext("Downloading"),
            DownloadStatus::Paused => gettext("Paused"),
            DownloadStatus::Done => gettext("Done"),
            DownloadStatus::Failed => gettext("Failed"),
            DownloadStatus::Cancelled => gettext("Cancelled"),
        }
    }
}

pub(crate) const QUEUE_VERSION: u32 = 3;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct StoredItem {
    /// Manager id, also staging key; absent before v3 (fresh id on restore). Persisted so retained recordings stay reachable after restart.
    #[serde(default)]
    pub(crate) id: Option<u64>,
    pub(crate) url: String,
    pub(crate) dest_dir: String,
    pub(crate) filename: String,
    pub(crate) status: DownloadStatus,
    #[serde(default)]
    pub(crate) progress: f64,
    /// Completed pieces for segmented resume (v2+); absent on v1 or when unneeded.
    #[serde(default)]
    pub(crate) segments: Option<SegmentState>,
    /// Multi-file torrent selection (v2+); re-staged on restore or resume would download everything.
    #[serde(default)]
    pub(crate) selected_files: Option<Vec<usize>>,
    /// Recorded engine output folder for torrents (v2+).
    #[serde(default)]
    pub(crate) output_dir: Option<String>,
    /// Video-page source; only `Some(Page)` written so old files/versions stay compatible.
    #[serde(default)]
    pub(crate) video_source: Option<crate::media_types::VideoSource>,
    /// Whether the engine ever started this row (additive, no version bump):
    /// a restored row with bytes at dest resumes instead of refusing as foreign.
    /// `None` = queue file predates the field: keep the legacy resume behavior
    /// rather than refusing a partial the old version was downloading.
    #[serde(default)]
    pub(crate) started: Option<bool>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct StoredQueue {
    pub(crate) version: u32,
    pub(crate) items: Vec<StoredItem>,
}
