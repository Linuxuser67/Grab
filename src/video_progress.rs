//! Progress-line parsing: the `[Grab];` template model, merge/leg detectors
//! and pump helpers. Leaf module (no crate deps).

/// Progress reports are throttled to this many bytes between row updates, so
/// the bar stays live on slow links without churning the UI.
pub(crate) const PROGRESS_GRANULARITY: u64 = 16384;

/// One parsed template line: absolute byte counts (never percents), so callers
/// accumulate instead of re-deriving. `total` folds the estimate fallback;
/// `None` means unknown (live/unsized), not zero. `speed`/`eta` are parsed but
/// not consumed (the pump recomputes both from ticks) — they document the line
/// shape. `finished` marks a leg boundary: yt-dlp prints one per completed
/// format and `downloaded_bytes` resets for the next leg.
#[derive(Debug, PartialEq)]
pub(crate) struct YtProgress {
    pub downloaded: Option<u64>,
    pub total: Option<u64>,
    pub speed: Option<f64>,
    pub eta: Option<u64>,
    pub finished: bool,
}

pub(crate) fn parse_ytdlp_template(line: &str) -> Option<YtProgress> {
    let rest = line.strip_prefix("[Grab];")?;
    let mut f = rest.split(';');
    let status = f.next()?;
    // "error" lines carry no usable counts; finished lines do.
    if status == "error" {
        return None;
    }
    let num = |s: Option<&str>| {
        s.filter(|v| *v != "NA")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|n| n.is_finite() && *n >= 0.0)
    };
    let downloaded = num(f.next()).map(|v| v as u64);
    let total = num(f.next()).map(|v| v as u64);
    let estimate = num(f.next()).map(|v| v as u64);
    let speed = num(f.next());
    let eta = f
        .next()
        .filter(|v| *v != "NA" && *v != "Unknown")
        .and_then(|v| v.parse::<u64>().ok());
    Some(YtProgress {
        downloaded,
        total: total.or(estimate),
        speed,
        eta,
        finished: status == "finished",
    })
}

/// Whether a `--newline` line announces a merge/extract phase.
pub(crate) fn is_ytdlp_merge_line(line: &str) -> bool {
    line.starts_with("[Merger]") || line.starts_with("[ExtractAudio]")
}

/// Final path from `--print after_move:filepath`: a bare absolute
/// path line (every other stdout line carries a `[tag]` prefix).
pub(crate) fn parse_ytdlp_after_move(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    (!trimmed.is_empty() && !trimmed.starts_with('[') && trimmed.starts_with('/'))
        .then_some(trimmed)
}

/// Whether a fresh total starts a new format leg (video→audio) rather than
/// HLS/DASH estimate wobble. Wobble moves the total alone; a new leg moves it
/// substantially *and* resets downloaded back near zero (legs are sequential).
/// Unknown bytes count as reset. The first known total always (re)inits.
pub(crate) fn leg_changed(
    max_total: Option<u64>,
    max_dl: u64,
    total: u64,
    downloaded: Option<u64>,
) -> bool {
    if total == 0 {
        return false;
    }
    match max_total {
        None | Some(0) => true,
        Some(m) => {
            let total_moved = total < m / 2 || total > m.saturating_mul(2);
            total_moved && downloaded.is_none_or(|d| d <= max_dl / 2)
        }
    }
}

/// Whether a refined-up total must move the current leg's denominator. A stale
/// smaller denominator saturates early: once downloaded passes it every cell
/// reads done while the bar still shows partial. Any growth adopts the bigger
/// denominator; growth adoptions only ever raise it, so rebuilds are bounded
/// by new high-water marks and can never oscillate. Downward wobble never
/// adopts here: the bar keeps the bigger denominator, so bar and grid stay
/// consistent on a stale larger total (sharp drops go through
/// `estimate_collapsed` below).
pub(crate) fn grid_needs_rebuild(grid_total: Option<u64>, total: u64) -> bool {
    total > 0 && grid_total.is_none_or(|g| total > g)
}

/// Whether a collapsed HLS estimate invalidates the current denominator.
/// `total_bytes_estimate` can spike to ~2x the true total mid-download and
/// then revise sharply down; keeping the spike sizes the bar for a phantom
/// total it can never fill (the bar stalls half-lit while the download runs
/// to completion). A downward revision beyond wobble adopts the correction:
/// the old denominator was wrong, the new estimate is yt-dlp's best current
/// guess. Checked after `leg_changed`, so a genuine new leg (total moves
/// *and* downloaded resets) still wins.
pub(crate) fn estimate_collapsed(max_total: Option<u64>, total: u64) -> bool {
    match max_total {
        Some(m) if m > 0 => total > 0 && total.saturating_mul(4) < m.saturating_mul(3),
        _ => false,
    }
}

/// Canonical progress state for one HLS attempt across format legs.
///
/// yt-dlp reports each leg (video, audio, …) with leg-local byte counts, so a
/// running sticky max freezes the bar when a small leg starts — and a bitmap
/// grid rebuilt per estimate redefines every cell. Instead this banks each
/// finished leg's actual bytes once and reports cumulative
/// `(completed + leg)` numbers, from which the bar and the fraction-derived
/// grid both render. One value, two views; they cannot disagree.
///
/// Leg boundaries reuse [`leg_changed`] against the CURRENT leg's baseline
/// (not a global sticky max), so a boundary fires exactly once: after banking,
/// the baseline becomes the new leg and identical following lines no longer
/// match. Same-leg wobble reuses [`grid_needs_rebuild`] (growth) and
/// [`estimate_collapsed`] + containment (sharp drops), mirroring the old grid
/// policy byte-for-byte in fraction space.
#[derive(Default)]
pub(crate) struct HlsProgress {
    /// Actual bytes banked from finished legs (capped per leg, see `update`).
    completed: u64,
    /// Max bytes seen in the current leg, capped at its total.
    leg_have: u64,
    /// Current leg's denominator (`None` = unknown: indeterminate, as before).
    leg_total: Option<u64>,
    /// Last banked amount, to suppress duplicate `finished` lines.
    last_bank: u64,
}

/// Cumulative progress for one template line: the pump renders the bar from
/// exactly these two numbers, and the HLS grid derives its cells from the
/// same fraction. No `SegmentsInit`/`PieceDone` needed for HLS rows.
/// (`fraction` itself is intentionally NOT precomputed: the pump owns the
/// single downloaded/total→fraction formula, so there is only one place
/// that can drift.)
pub(crate) struct DisplayProgress {
    pub downloaded: u64,
    pub total: Option<u64>,
}

impl HlsProgress {
    /// Fold one parsed template line into canonical cumulative progress.
    /// Pure for tests.
    pub(crate) fn update(
        &mut self,
        downloaded: Option<u64>,
        total: Option<u64>,
        finished: bool,
    ) -> DisplayProgress {
        // Authoritative leg boundary: yt-dlp prints `finished` once per
        // completed format leg. Bank the leg's actual bytes (capped at its
        // estimate — estimates overshoot) and reset for the next leg.
        // This is edge-triggered, unlike the `leg_changed` heuristic below,
        // so it cannot re-fire on subsequent lines.
        if finished {
            // No progress seen since the last bank: a duplicate `finished`
            // line for the same leg, not a new one.
            let had_progress = self.leg_have > 0;
            if let Some(d) = downloaded {
                self.leg_have = self.leg_have.max(d);
            }
            if self.leg_total.is_none()
                && let Some(t) = total.filter(|&t| t > 0)
            {
                self.leg_total = Some(t);
            }
            let bank = self.leg_have.min(self.leg_total.unwrap_or(self.leg_have));
            // Suppress double-banking on duplicate `finished`: same amount,
            // no new progress, and we've banked before. (A same-sized next
            // leg without any progress lines would also match; yt-dlp always
            // emits `downloading` lines before `finished`, so this is safe.)
            if bank != self.last_bank || had_progress || self.completed == 0 {
                self.completed += bank;
                self.last_bank = bank;
            }
            let disp = DisplayProgress {
                downloaded: self.completed,
                // A leg that never reported a total stays indeterminate;
                // finished means done, not sized.
                total: self.leg_total.map(|_| self.completed),
            };
            self.leg_have = 0;
            self.leg_total = None;
            return disp;
        }

        if let Some(t) = total.filter(|&t| t > 0) {
            match self.leg_total {
                // First known total starts the first leg.
                None => {
                    self.leg_total = Some(t);
                }
                Some(cur) => {
                    // A new leg: bank the finished leg's ACTUAL bytes (capped
                    // at its estimate — estimates overshoot), exactly once per
                    // transition: after banking the baseline IS the new leg.
                    if leg_changed(Some(cur), self.leg_have, t, downloaded) {
                        self.completed += self.leg_have.min(cur);
                        self.leg_have = 0;
                        self.leg_total = Some(t);
                    } else if grid_needs_rebuild(self.leg_total, t) {
                        // Refined-up estimate: adopt the bigger denominator.
                        self.leg_total = Some(t);
                    } else if estimate_collapsed(self.leg_total, t)
                        && downloaded.is_some_and(|d| t >= d)
                    {
                        // Sharp downward revision that still contains what we
                        // have: adopt it rather than sizing for a phantom peak.
                        self.leg_total = Some(t);
                    }
                }
            }
        }
        if let Some(d) = downloaded {
            let cap = self.leg_total.unwrap_or(u64::MAX);
            self.leg_have = self.leg_have.max(d.min(cap));
        }
        DisplayProgress {
            downloaded: self.completed + self.leg_have,
            total: self.leg_total.map(|l| self.completed + l),
        }
    }
}

/// Trace yt-dlp's selected-format line as it streams past, to audit our pick
/// against yt-dlp's own sort and id aliasing. `pending` carries a line split
/// across 4 KiB reads.
pub(crate) fn trace_format_lines(pending: &mut String, chunk: &[u8]) {
    pending.push_str(&String::from_utf8_lossy(chunk));
    while let Some(pos) = pending.find('\n') {
        let line: String = pending.drain(..=pos).collect();
        let line = line.trim_end();
        if is_format_selection_line(line) {
            tracing::debug!("{line}");
        }
    }
}

/// Whether a yt-dlp stderr line announces the selected formats.
pub(crate) fn is_format_selection_line(line: &str) -> bool {
    line.contains("Downloading ") && line.contains("format(s)")
}

/// Last non-blank line of captured child output, for error detail.
/// `fallback` names the tool when the output carries nothing usable.
pub(crate) fn last_log_line(output: &str, fallback: &str) -> String {
    output
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(fallback)
        .trim()
        .to_string()
}

/// Last `ERROR:` line of captured child output, for failure detail: yt-dlp
/// prints a Python traceback after its error line, so the last non-blank line
/// is usually a useless `File "...", line N, in ...`.
pub(crate) fn last_error_line(output: &str, fallback: &str) -> String {
    output
        .lines()
        .rev()
        .find_map(|l| {
            let trimmed = l.trim();
            if trimmed.starts_with("ERROR:") {
                Some(trimmed.to_owned())
            } else {
                None
            }
        })
        .unwrap_or_else(|| last_log_line(output, fallback))
}
