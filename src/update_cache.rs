//! Cache for upstream "latest release" lookups, shared by the manual check
//! and the startup check.
//!
//! GitHub's unauthenticated API allows 60 requests an hour per IP, so a click
//! must not cost four requests each time. Every network lookup here runs on
//! Grab's Tokio runtime: this module is awaited from GLib tasks, and reqwest
//! and tokio::process panic when no Tokio runtime is entered.

use gettextrs::gettext;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Freshness for a manual "Check for Updates" click.
pub const MANUAL_MAX_AGE_SECS: u64 = 15 * 60;
/// Freshness for the startup check: at most one upstream fetch a day.
pub const STARTUP_MAX_AGE_SECS: u64 = 24 * 60 * 60;

/// Latest upstream release tags, plus the SHA-256 of the gallery-dl asset the
/// installer would fetch.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Latest {
    pub ytdlp: Option<String>,
    pub ffmpeg: Option<String>,
    pub quickjs: Option<String>,
    pub gallerydl: Option<String>,
    pub gallerydl_digest: Option<String>,
}

impl Latest {
    /// Only a complete answer is cached. A partial one (rate limit, outage) is
    /// retried on the next check instead of being pinned for an hour.
    fn is_complete(&self) -> bool {
        self.ytdlp.is_some()
            && self.ffmpeg.is_some()
            && self.quickjs.is_some()
            && self.gallerydl.is_some()
    }
}

/// Result of [`latest`]: the answer, and whether it came from a fresh fetch.
pub struct Snapshot {
    pub latest: Latest,
    pub fetched: bool,
}

#[derive(Serialize, Deserialize)]
struct Stored {
    checked_at: u64,
    latest: Latest,
}

fn cache_path() -> PathBuf {
    crate::video_tools::user_lib_dir().join("update-check.json")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A stored check is usable while it is younger than `max_age`. A stamp from
/// the future (the clock moved back) is never trusted.
pub(crate) fn is_fresh(checked_at: u64, now: u64, max_age: u64) -> bool {
    checked_at <= now && now - checked_at < max_age
}

fn read_cache(path: &Path) -> Option<Stored> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_cache(path: &Path, stored: &Stored) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string(stored) {
        // Best effort: a failed write only means the next check fetches again.
        let _ = std::fs::write(path, text);
    }
}

/// The four upstream lookups, awaited together. Each one already runs on the
/// Tokio runtime, and [`latest`] calls this from inside that runtime.
async fn fetch_all() -> Latest {
    let (ytdlp, ffmpeg, quickjs, gallery) = tokio::join!(
        crate::video::latest_ytdlp_tag(),
        crate::video::latest_ffmpeg_tag(),
        crate::video::latest_quickjs_tag(),
        crate::gallery_tools::latest_gallerydl_release(),
    );
    let (gallerydl, gallerydl_digest) = match gallery {
        Some((tag, digest)) => (Some(tag), digest),
        None => (None, None),
    };
    Latest {
        ytdlp,
        ffmpeg,
        quickjs,
        gallerydl,
        gallerydl_digest,
    }
}

/// Latest upstream releases, from the cache while it is younger than
/// `max_age` seconds. The work always runs on Grab's Tokio runtime, whatever
/// context the caller is in.
pub async fn latest(max_age: u64) -> Snapshot {
    let handle = crate::runtime::tokio_rt().spawn(async move {
        let path = cache_path();
        if let Some(stored) = read_cache(&path)
            && is_fresh(stored.checked_at, now_secs(), max_age)
        {
            return Snapshot {
                latest: stored.latest,
                fetched: false,
            };
        }
        let latest = fetch_all().await;
        if latest.is_complete() {
            let stored = Stored {
                checked_at: now_secs(),
                latest: latest.clone(),
            };
            write_cache(&path, &stored);
        }
        Snapshot {
            latest,
            fetched: true,
        }
    });
    match handle.await {
        Ok(snapshot) => snapshot,
        // A panicked lookup reads as "no answer" rather than taking the caller down.
        Err(_) => Snapshot {
            latest: Latest::default(),
            fetched: false,
        },
    }
}

/// Startup check: refreshes the cache at most once a day. Returns a notice only
/// when that fetch found a newer yt-dlp than the installed one, so a launch
/// that reuses the cache never repeats the notice.
pub async fn startup_notice() -> Option<String> {
    let snapshot = latest(STARTUP_MAX_AGE_SECS).await;
    if !snapshot.fetched {
        return None;
    }
    let tag = snapshot.latest.ytdlp?;
    let installed = gtk4::gio::spawn_blocking(crate::video_tools::installed_ytdlp_version)
        .await
        .ok()
        .flatten()?;
    if !crate::video_tools::ytdlp_update_available(&installed, &tag) {
        return None;
    }
    let template = gettext("yt-dlp {new} available, you have {old}. Update in Preferences.");
    let text = template.replace("{new}", tag.trim());
    Some(text.replace("{old}", installed.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freshness_respects_age_and_clock_skew() {
        assert!(is_fresh(1_000, 1_500, 600));
        assert!(!is_fresh(1_000, 1_700, 600));
        // Stamped in the future: a moved-back clock must not pin an answer.
        assert!(!is_fresh(2_000, 1_500, 600));
    }

    #[test]
    fn only_a_complete_answer_is_cached() {
        let mut latest = Latest {
            ytdlp: Some("2026.10.15".into()),
            ffmpeg: Some("n8.0".into()),
            quickjs: Some("v0.1".into()),
            gallerydl: Some("2026.10.15".into()),
            gallerydl_digest: None,
        };
        assert!(latest.is_complete());
        latest.quickjs = None;
        assert!(!latest.is_complete());
    }
}
