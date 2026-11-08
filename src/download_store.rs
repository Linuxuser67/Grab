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
    Scheduled,
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
            DownloadStatus::Scheduled => gettext("Scheduled"),
            DownloadStatus::Downloading => gettext("Downloading"),
            DownloadStatus::Paused => gettext("Paused"),
            DownloadStatus::Done => gettext("Done"),
            DownloadStatus::Failed => gettext("Failed"),
            DownloadStatus::Cancelled => gettext("Cancelled"),
        }
    }
}

pub(crate) const QUEUE_VERSION: u32 = 4;

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
    /// Persisted video source (page or native manifest); older entries default to `None`.
    /// Supported older queue versions load; unsupported versions are quarantined.
    #[serde(default)]
    pub(crate) video_source: Option<crate::media_types::VideoSource>,
    /// Whether the engine ever started this row (additive, no version bump):
    /// a restored row with bytes at dest resumes instead of refusing as foreign.
    /// `None` = queue file predates the field: keep the legacy resume behavior
    /// rather than refusing a partial the old version was downloading.
    #[serde(default)]
    pub(crate) started: Option<bool>,
    /// Unix timestamp for scheduled downloads (additive, no version bump).
    /// `None` or 0 = not scheduled.
    #[serde(default)]
    pub(crate) scheduled_at: Option<i64>,
    /// Resume validator: ETag (strong) or Last-Modified for If-Range.
    /// Additive, no version bump; `None` = predates the field or server
    /// sent no validator.
    #[serde(default)]
    pub(crate) etag: Option<String>,
    #[serde(default)]
    pub(crate) last_modified: Option<String>,
    /// Whether this row originated from an untrusted handoff and must retain public-only network checks.
    /// Additive for old queue files.
    #[serde(default)]
    pub(crate) public_only: Option<bool>,
    /// Gallery job payload (raw Value: a bad shape in one entry must not
    /// quarantine the whole queue). Additive, no version bump.
    /// `None` = not a gallery row or predates the field.
    #[serde(default)]
    pub(crate) gallery: Option<serde_json::Value>,
    /// Shared identity for multi-row collection downloads; additive queue metadata.
    #[serde(default)]
    pub(crate) group_id: Option<u64>,
    /// Collection title displayed above grouped child rows; additive queue metadata.
    #[serde(default)]
    pub(crate) group_title: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct StoredQueue {
    pub(crate) version: u32,
    pub(crate) items: Vec<StoredItem>,
}

#[cfg(test)]
mod public_only_metadata_tests {
    use super::StoredItem;

    #[test]
    fn legacy_queue_item_without_public_only_field_is_compatible() {
        let item: StoredItem = serde_json::from_value(serde_json::json!({
            "url": "https://example.com/file.mp4",
            "dest_dir": "/tmp/downloads",
            "filename": "file.mp4",
            "status": "queued",
            "progress": 0.0
        }))
        .expect("legacy queue item should deserialize");
        assert_eq!(item.public_only, None);
        assert_eq!(item.group_id, None);
        assert_eq!(item.group_title, None);
    }

    #[test]
    fn public_only_metadata_round_trips() {
        let item: StoredItem = serde_json::from_value(serde_json::json!({
            "url": "https://example.com/file.mp4",
            "dest_dir": "/tmp/downloads",
            "filename": "file.mp4",
            "status": "queued",
            "public_only": true
        }))
        .expect("new queue item should deserialize");
        assert_eq!(item.public_only, Some(true));
        let serialized = serde_json::to_value(item).expect("queue item should serialize");
        assert_eq!(serialized["public_only"], true);
    }
}


#[cfg(test)]
mod group_metadata_tests {
    use super::StoredItem;

    #[test]
    fn collection_group_metadata_round_trips() {
        let item: StoredItem = serde_json::from_value(serde_json::json!({
            "url": "https://example.com/video/one",
            "dest_dir": "/tmp/downloads",
            "filename": "one.mp4",
            "status": "queued",
            "group_id": 41,
            "group_title": "Course playlist"
        }))
        .expect("grouped queue item should deserialize");

        assert_eq!(item.group_id, Some(41));
        assert_eq!(item.group_title.as_deref(), Some("Course playlist"));

        let serialized = serde_json::to_value(item).expect("grouped queue item should serialize");
        assert_eq!(serialized["group_id"], 41);
        assert_eq!(serialized["group_title"], "Course playlist");
    }
}
