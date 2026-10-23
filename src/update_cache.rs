//! Cache for upstream "latest release" lookups, used by the Check for Updates
//! button.
//!
//! GitHub's unauthenticated API allows 60 requests an hour per IP, so a click
//! must not cost three requests each time. Every network lookup here runs on
//! Grab's Tokio runtime: this module is awaited from GLib tasks, and reqwest
//! and tokio::process panic when no Tokio runtime is entered.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Freshness for a "Check for Updates" click.
pub const MANUAL_MAX_AGE_SECS: u64 = 15 * 60;

/// Latest upstream release tags, plus the SHA-256 of the gallery-dl asset the
/// installer would fetch.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Latest {
    pub ytdlp: Option<String>,
    pub deno: Option<String>,
    pub gallerydl: Option<String>,
    pub gallerydl_digest: Option<String>,
}

impl Latest {
    /// Only a complete answer is cached. A partial one (rate limit, outage) is
    /// retried on the next check instead of being pinned for 15 minutes.
    fn is_complete(&self) -> bool {
        self.ytdlp.is_some() && self.deno.is_some() && self.gallerydl.is_some()
    }
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

/// The three upstream lookups, awaited together. Each one already runs on the
/// Tokio runtime, and [`latest`] calls this from inside that runtime.
async fn fetch_all() -> Latest {
    let (ytdlp, deno, gallery) = tokio::join!(
        crate::video::latest_ytdlp_tag(),
        crate::video::latest_deno_tag(),
        crate::gallery_tools::latest_gallerydl_release(),
    );
    let (gallerydl, gallerydl_digest) = match gallery {
        Some((tag, digest)) => (Some(tag), digest),
        None => (None, None),
    };
    Latest {
        ytdlp,
        deno,
        gallerydl,
        gallerydl_digest,
    }
}

/// Latest upstream releases, from the cache while it is younger than
/// `max_age` seconds. The work always runs on Grab's Tokio runtime, whatever
/// context the caller is in.
pub async fn latest(max_age: u64) -> Latest {
    let handle = crate::runtime::tokio_rt().spawn(async move {
        let path = cache_path();
        if let Some(stored) = read_cache(&path)
            && stored.latest.is_complete()
            && is_fresh(stored.checked_at, now_secs(), max_age)
        {
            return stored.latest;
        }
        let latest = fetch_all().await;
        if latest.is_complete() {
            let stored = Stored {
                checked_at: now_secs(),
                latest: latest.clone(),
            };
            write_cache(&path, &stored);
        }
        latest
    });
    // A panicked lookup reads as "no answer" rather than taking the caller down.
    handle.await.unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_cache_with_ffmpeg_field_is_accepted() {
        let old = r#"{
            "ytdlp":"2026.10.15",
            "ffmpeg":"n8.0",
            "quickjs":"0.17.0",
            "gallerydl":"2026.10.15",
            "gallerydl_digest":null
        }"#;
        let parsed: Latest = serde_json::from_str(old).unwrap();
        assert_eq!(parsed.ytdlp.as_deref(), Some("2026.10.15"));
        assert_eq!(
            parsed.deno, None,
            "legacy QuickJS cache cannot stand in for Deno"
        );
        assert!(!parsed.is_complete());
        assert_eq!(parsed.gallerydl.as_deref(), Some("2026.10.15"));
    }

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
            deno: Some("v2.9.8".into()),
            gallerydl: Some("2026.10.15".into()),
            gallerydl_digest: None,
        };
        assert!(latest.is_complete());
        latest.deno = None;
        assert!(!latest.is_complete());
    }
}
