//! Rate parsing/pacing + progress text: limits, throttle cell, chunk pacing, status formatting.

use crate::engine_msg::EngineMsg;
use crate::file_names::fmt_bytes;
use gettextrs::{gettext, ngettext};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Parse a speed limit like `500K`/`2M`/`1.5G` into bytes/sec; `None` means unlimited or invalid.
pub(crate) fn parse_rate(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() || s == "0" {
        return None;
    }
    let (num, mult) = match s.as_bytes().last()? {
        b'K' | b'k' => (&s[..s.len() - 1], 1024u64),
        b'M' | b'm' => (&s[..s.len() - 1], 1024 * 1024),
        b'G' | b'g' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        b'0'..=b'9' => (s, 1),
        _ => return None,
    };
    num.trim()
        .parse::<f64>()
        .ok()
        .filter(|v| *v > 0.0)
        .map(|v| (v * mult as f64) as u64)
}

/// Live cap in bytes/sec (0 = unlimited), one atomic for all engines; `gio::Settings` is main-thread-only, hence the hop.
static LIVE_RATE_LIMIT: AtomicU64 = AtomicU64::new(0);

pub(crate) fn publish_rate_limit(settings: &crate::settings::AppSettings) {
    LIVE_RATE_LIMIT.store(
        parse_rate(settings.speed_limit().as_str()).unwrap_or(0),
        Ordering::Relaxed,
    );
}

pub(crate) fn live_rate_limit() -> Option<u64> {
    match LIVE_RATE_LIMIT.load(Ordering::Relaxed) {
        0 => None,
        r => Some(r),
    }
}

/// Progress message for the direct engine (HTTP rows carry no upload counters).
pub(crate) fn progress_msg(downloaded: u64, total: Option<u64>) -> EngineMsg {
    EngineMsg::Progress {
        downloaded,
        total,
        uploaded: 0,
        upload_bps: 0,
    }
}

/// Throttle one chunk; limit passed per call so preference edits apply mid-download.
pub(crate) async fn pace_chunk(paced: &mut u64, pace_start: Instant, rate: Option<u64>, n: usize) {
    if let Some(r) = rate {
        *paced += n as u64;
        let wait = *paced as f64 / r as f64 - pace_start.elapsed().as_secs_f64();
        if wait > 0.0 {
            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
        }
    }
}

/// "{downloaded} of {total}" for progress rows (Files copy-dialog convention).
/// The connector word is translatable; the byte sizes come localized from GLib.
pub(crate) fn format_amounts(downloaded: u64, total: u64) -> String {
    gettext("{downloaded} of {total}")
        .replace("{downloaded}", &fmt_bytes(downloaded))
        .replace("{total}", &fmt_bytes(total))
}

/// Human ETA (longest whole unit) for the HIG's "About {eta} left" phrasing. Pure for tests.
pub(crate) fn fmt_eta(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    if h > 0 {
        ngettext("1 hour", "{n} hours", h as u32).replace("{n}", &h.to_string())
    } else if m > 0 {
        ngettext("1 minute", "{n} minutes", m as u32).replace("{n}", &m.to_string())
    } else {
        ngettext("1 second", "{n} seconds", s as u32).replace("{n}", &s.to_string())
    }
}
