use crate::download::DownloadStatus;
use crate::download_details::{DownloadKind, download_kind};
use crate::media_types::{PlaylistKind, VideoSource};
use crate::video_types::VideoFormatOption;
use crate::window_dialogs::{
    MediaPick, SessionCtl, fmt_item_duration, initial_media_pick, media_pick_params,
    playlist_count_label,
};
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

#[test]
fn download_kind_classifies_details_dialog_types() {
    fn page(audio_only: bool) -> VideoSource {
        VideoSource::Page {
            page_url: "https://example.com/watch".to_string(),
            media_url: None,
            expires_at: None,
            quality: "best".to_string(),
            audio_only,
            is_live: false,
            video_format_id: None,
            playlist_item_id: None,
        }
    }
    // The URL decides first: a torrent stays a torrent whatever the source says.
    assert_eq!(download_kind(true, None, false), DownloadKind::Torrent);
    assert_eq!(
        download_kind(true, Some(&page(false)), false),
        DownloadKind::Torrent
    );
    assert_eq!(
        download_kind(false, Some(&page(false)), true),
        DownloadKind::LiveVideo,
        "a live page reads as a live video, not a plain one"
    );
    assert_eq!(
        download_kind(false, Some(&page(false)), false),
        DownloadKind::Video
    );
    assert_eq!(
        download_kind(false, Some(&page(true)), true),
        DownloadKind::Audio,
        "audio-only wins over the live flag: no video track exists"
    );
    assert_eq!(
        download_kind(false, Some(&VideoSource::Direct), false),
        DownloadKind::File
    );
    assert_eq!(download_kind(false, None, false), DownloadKind::File);
}

// ── media-format picker ──────────────────────────────────────────────

fn media_test_pins() -> Vec<VideoFormatOption> {
    [1080u32, 720, 480]
        .iter()
        .map(|h| VideoFormatOption {
            id: format!("v{h}"),
            label: format!("{h}p"),
            detail: String::new(),
            height: *h,
        })
        .collect()
}

#[test]
fn media_pick_pin_returns_its_id_and_not_audio() {
    let pins = media_test_pins();
    let (id, audio) = media_pick_params(MediaPick::Pin(1), &pins);
    assert_eq!(id.as_deref(), Some("v720"));
    assert!(!audio, "a pinned format is a video download");
}

#[test]
fn media_pick_automatic_returns_none_and_not_audio() {
    let pins = media_test_pins();
    let (id, audio) = media_pick_params(MediaPick::Automatic, &pins);
    assert_eq!(
        id, None,
        "Automatic carries no pin — the global preference applies"
    );
    assert!(!audio);
}

#[test]
fn media_pick_audio_only_returns_none_and_audio() {
    let pins = media_test_pins();
    let (id, audio) = media_pick_params(MediaPick::AudioOnly, &pins);
    assert_eq!(id, None, "audio-only drops the pin");
    assert!(audio);
}

#[test]
fn media_pick_stale_pin_index_degrades_to_no_pin() {
    // A pin index that outlives its resolve must not panic or pin garbage.
    let pins = media_test_pins();
    let (id, audio) = media_pick_params(MediaPick::Pin(99), &pins);
    assert_eq!(id, None);
    assert!(!audio);
}

#[test]
fn initial_media_pick_without_pins_is_automatic() {
    assert_eq!(initial_media_pick(&[], "720p"), MediaPick::Automatic);
}

#[test]
fn initial_media_pick_preselects_preference_closest_pin() {
    let pins = media_test_pins();
    assert_eq!(initial_media_pick(&pins, "best"), MediaPick::Pin(0));
    assert_eq!(initial_media_pick(&pins, "720p"), MediaPick::Pin(1));
    assert_eq!(initial_media_pick(&pins, "480p"), MediaPick::Pin(2));
}

#[test]
fn initial_media_pick_unknown_pref_falls_back_to_1080p() {
    // Unknown values fall back to 1080p by design (quality_height): with
    // 1080p in the middle the preselect is Pin(1) — a row-0 fallback would
    // fail this, so the 1080p fallback is genuinely pinned.
    let pins: Vec<VideoFormatOption> = [2160u32, 1080, 720]
        .iter()
        .map(|h| VideoFormatOption {
            id: format!("v{h}"),
            label: format!("{h}p"),
            detail: String::new(),
            height: *h,
        })
        .collect();
    assert_eq!(initial_media_pick(&pins, "mystery"), MediaPick::Pin(1));
}

// ── session reset ────────────────────────────────────────────────────

#[test]
fn session_ctl_clone_sees_reset_installed_after_clone() {
    // Regression: SessionCtl used to hold the reset callback in a plain
    // RefCell, so #[derive(Clone)] snapshotted the initial no-op and every
    // submit path — all working on pre-install clones — showed the toast
    // without resetting the form.
    let fired = std::rc::Rc::new(std::cell::Cell::new(false));
    let ctl = SessionCtl::test_ctl();
    let cloned = ctl.clone();
    let fired_b = fired.clone();
    ctl.set_reset(std::rc::Rc::new(move || fired_b.set(true)));
    cloned.succeed("Download added");
    assert!(
        fired.get(),
        "reset installed after cloning must reach the clones"
    );
}
