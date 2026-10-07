//! Engine-to-UI messages: the channel every engine reports progress, completion and failure on.

use std::time::SystemTime;

/// Resume validator info (boxed in EngineMsg to keep the enum small).
#[derive(Clone, Debug)]
pub(crate) struct ValidatorInfo {
    pub(crate) etag: Option<String>,
    pub(crate) last_modified: Option<String>,
}

pub(crate) enum EngineMsg {
    Progress {
        downloaded: u64,
        total: Option<u64>,
        /// Torrent upload counters (HTTP sends zeros).
        uploaded: u64,
        upload_bps: u64,
    },
    Finished {
        size: u64,
    },
    /// Playlist-shaped video row with no pick: pump queues one row per item and retires the carrier.
    /// Boxed: this variant is rare but would otherwise inflate every message
    /// on this hot channel (progress ticks) to the largest variant's size.
    ExpandPlaylist(Box<crate::media_types::PlaylistInfo>),
    Failed(String),
    /// A multi worker finished one piece; the UI thread records it for resume.
    PieceDone(u64),
    /// Multi failed terminally: shrink to completed prefix (bitmap stays valid for retry).
    TruncatePrefix,
    /// Fresh multi probe succeeded; UI thread creates the resume bitmap and
    /// queues a persist, then acks. The engine waits (bounded) for the ack
    /// before preallocating the full-size file, so a crash can never leave a
    /// full-size file on disk with no bitmap in the queue file.
    SegmentsInit {
        total: u64,
        ack: tokio::sync::mpsc::Sender<()>,
    },
    /// Server throttling parallel connections: shrink to prefix, drop bitmap, ack to continue single-stream; handshake so pause/resume never sees bitmap without file.
    FallbackSingle {
        ack: tokio::sync::mpsc::Sender<()>,
    },
    /// Server-advertised filename; adopted at Finished when current name qualifies.
    SuggestName(String),
    /// Server Last-Modified; applied at Finished when keep-server-date is on (best-effort).
    LastModified(SystemTime),
    /// Resume validator from response headers (ETag preferred, Last-Modified
    /// fallback): stored on the item for If-Range on resume. Sent once per
    /// download generation (fresh 200, or 200 after If-Range mismatch); a 206
    /// in response to If-Range does not re-send, so a mid-download change
    /// cannot poison the stored validator.
    /// Boxed: this variant is rare but would otherwise inflate every message
    /// on this hot channel (progress ticks) to the largest variant's size.
    Validator(Box<ValidatorInfo>),
    /// Dialog-less live row: track so Stop finalizes capture instead of killing it as stalled VOD.
    LiveDetected,
    /// Server object changed mid-download: drop bitmap so retry starts fresh.
    FailedVersion(String),
    /// Torrent per-piece haves (500ms tick); replaces bitfield, redraws off progress ticks.
    TorrentPieces(Vec<bool>),
    /// Free-form phase label (resolve/merge stages byte counters miss); next Progress tick renders over it.
    Phase(String),
}

/// Fresh run found someone else's file at our path: the row fails instead of renaming (Parabolic-style).
pub(crate) const DEST_EXISTS: &str = "Destination already exists";
