use crate::download::DownloadStatus;
use crate::media_types::PlaylistKind;
use crate::window_dialogs::{fmt_item_duration, playlist_count_label};
use crate::window_rows::{PulseTick, StopCopy, pulse_tick, should_pulse, stop_copy};

#[test]
fn stop_copy_distinguishes_a_live_capture_from_a_discard() {
    // Stop inverts by row: normal downloads discard, live captures keep the recording.
    // Shipped both as "Cancel" with no feedback, so live users couldn't tell it was safe.
    assert_eq!(
        stop_copy(false),
        StopCopy::Cancel,
        "a normal download is discarded, so the discard verb is right"
    );
    assert_eq!(
        stop_copy(true),
        StopCopy::StopRecording,
        "a live capture is kept, so the copy must not read as a discard"
    );
}

#[test]
fn should_pulse_covers_row_states() {
    use DownloadStatus::*;
    // Resolving: active with no fraction — the reported hang.
    assert!(should_pulse(Downloading, false, 0.0));
    // Determinate transfer renders fractions, never pulses.
    assert!(!should_pulse(Downloading, false, 0.5));
    assert!(!should_pulse(Downloading, false, 1.0));
    // Live captures pulse for the whole capture regardless of fraction.
    assert!(should_pulse(Downloading, true, 0.0));
    assert!(should_pulse(Downloading, true, 0.9));
    // Non-downloading rows never animate: queued, paused, and terminal.
    assert!(!should_pulse(Queued, false, 0.0));
    assert!(!should_pulse(Paused, false, 0.0));
    assert!(!should_pulse(Paused, true, 0.0));
    assert!(!should_pulse(Done, true, 0.0));
    assert!(!should_pulse(Failed, false, 0.0));
    assert!(!should_pulse(Cancelled, false, 0.0));
}

#[test]
fn should_pulse_locks_non_finite_and_negative_edges() {
    use DownloadStatus::*;
    // Negative fractions still read as "no fraction" — pulses.
    assert!(should_pulse(Downloading, false, -0.5));
    // NaN never satisfies `<= 0.0`, so it renders determinate (no pulse).
    assert!(!should_pulse(Downloading, false, f64::NAN));
    // Infinite progress is not "no fraction" — no pulse.
    assert!(!should_pulse(Downloading, false, f64::INFINITY));
    // Non-active rows never pulse regardless of fraction.
    assert!(!should_pulse(Queued, true, 0.0));
    assert!(!should_pulse(Done, false, f64::NAN));
}

/// The row pulse timer used to run until the widget died, so every finished
/// row kept waking the main loop 8x a second forever. It may only stop on a
/// state the row can never leave.
#[test]
fn the_pulse_timer_stops_only_where_the_row_cannot_come_back() {
    use DownloadStatus::*;
    use PulseTick::*;
    // Done is one-way for a given widget: no command moves a finished row
    // back to Queued, so the tick has nothing left to drive.
    assert_eq!(pulse_tick(Done, false, 1.0), Stop);
    assert_eq!(pulse_tick(Done, true, 0.0), Stop);
    // Failed and Cancelled are NOT safe to stop on: `retry` revives both by
    // mutating this very item back to Queued, so a stopped timer would
    // freeze the retried row's bar — the hang the tick exists to prevent.
    assert_eq!(pulse_tick(Failed, false, 0.5), Idle);
    assert_eq!(pulse_tick(Cancelled, false, 0.5), Idle);
    assert_eq!(pulse_tick(Paused, false, 0.0), Idle);
    assert_eq!(pulse_tick(Queued, false, 0.0), Idle);
    // Rows that can still animate keep animating.
    assert_eq!(pulse_tick(Downloading, false, 0.0), Pulse);
    assert_eq!(pulse_tick(Downloading, true, 0.9), Pulse);
    // An active row with a real fraction renders determinate: Pulse or Idle,
    // never Stop — the only input separating the two on an active row.
    assert_eq!(pulse_tick(Downloading, false, 0.5), Idle);
}

#[test]
fn fmt_item_duration_covers_minute_hour_boundaries() {
    assert_eq!(fmt_item_duration(0), "0:00");
    assert_eq!(fmt_item_duration(59), "0:59");
    assert_eq!(fmt_item_duration(60), "1:00");
    assert_eq!(fmt_item_duration(61), "1:01");
    assert_eq!(fmt_item_duration(3599), "59:59");
    assert_eq!(fmt_item_duration(3600), "1:00:00");
    assert_eq!(fmt_item_duration(3661), "1:01:01");
}

#[test]
fn fmt_item_duration_clamps_negative_to_zero() {
    assert_eq!(fmt_item_duration(-1), "0:00");
    assert_eq!(fmt_item_duration(i64::MIN), "0:00");
}

#[test]
fn playlist_count_label_covers_kinds_singular_plural() {
    assert_eq!(playlist_count_label(PlaylistKind::Stories, 1), "1 story");
    assert_eq!(playlist_count_label(PlaylistKind::Stories, 3), "3 stories");
    assert_eq!(
        playlist_count_label(PlaylistKind::Highlights, 1),
        "1 highlight"
    );
    assert_eq!(
        playlist_count_label(PlaylistKind::Highlights, 2),
        "2 highlights"
    );
    assert_eq!(playlist_count_label(PlaylistKind::Playlist, 1), "1 item");
    assert_eq!(playlist_count_label(PlaylistKind::Playlist, 5), "5 items");
}

#[test]
fn playlist_count_label_zero_uses_plural() {
    assert_eq!(playlist_count_label(PlaylistKind::Stories, 0), "0 stories");
    assert_eq!(
        playlist_count_label(PlaylistKind::Highlights, 0),
        "0 highlights"
    );
    assert_eq!(playlist_count_label(PlaylistKind::Playlist, 0), "0 items");
}
