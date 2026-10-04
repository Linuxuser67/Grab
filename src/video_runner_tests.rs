//! Tests for the live-capture runner's private cleanup policy: the sweep helper
//! and exit/staging enums are private to `video_runner`, so their retention
//! contract is only observable from in here.

use super::*;

use crate::video_argv::VideoJob;

/// `Exit::RenameFailed` keeps the user's only copy: nothing re-records, nothing is
/// swept except `.ytdl` state. Also pins the non-recursive staging sweep, which is
/// what keeps an earlier attempt's unplaceable remux durable.
#[test]
fn rename_failed_exit_keeps_media_and_staging_but_sweeps_state() {
    let dir = std::env::temp_dir().join(format!("grab-sweep-rename-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    // Both dest-side shapes: clean exit renames `.part` away, kill leaves the shell.
    let out = dir.join("v.live.mp4");
    let part = dir.join("v.live.mp4.part");
    let state = dir.join("v.live.mp4.ytdl");
    let final_tmp = staging.join("final.1.mp4");
    let sibling = staging.join("final.9.mp4");
    std::fs::write(&out, b"finalized").unwrap();
    std::fs::write(&part, b"shell").unwrap();
    std::fs::write(&state, b"fragment-3").unwrap();
    std::fs::write(&final_tmp, b"recorded").unwrap();
    std::fs::write(&sibling, b"earlier-attempt").unwrap();

    crate::runtime::tokio_rt().block_on(async {
        sweep_live_capture(
            &out,
            &part,
            &state,
            &staging,
            None,
            Staging::Keep,
            Exit::RenameFailed,
        )
        .await;
    });

    assert_eq!(
        std::fs::read(&out).unwrap(),
        b"finalized",
        "the finalized capture shell is the user's only copy and must survive"
    );
    assert_eq!(
        std::fs::read(&part).unwrap(),
        b"shell",
        "the raw .part shell must survive for salvage"
    );
    assert!(
        !state.exists(),
        "the .ytdl state file is scratch on every exit and must be swept"
    );
    assert_eq!(
        std::fs::read(&final_tmp).unwrap(),
        b"recorded",
        "the completed remux must survive: Staging::Keep means no temp is removed"
    );
    assert_eq!(
        std::fs::read(&sibling).unwrap(),
        b"earlier-attempt",
        "an earlier attempt's completed remux must survive: the sweep is not recursive"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The sweeping exits remove exactly the temp they were handed, and leave
/// every sibling alone.
#[test]
fn a_sweep_removes_only_its_own_temp() {
    let dir = std::env::temp_dir().join(format!("grab-sweep-own-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let out = dir.join("v.live.mp4");
    let part = dir.join("v.live.mp4.part");
    let state = dir.join("v.live.mp4.ytdl");
    let mine = staging.join("final.1.mp4");
    let sibling = staging.join("final.2.mp4");
    std::fs::write(&mine, b"mine").unwrap();
    std::fs::write(&sibling, b"sibling").unwrap();

    crate::runtime::tokio_rt().block_on(async {
        sweep_live_capture(
            &out,
            &part,
            &state,
            &staging,
            Some(&mine),
            Staging::Sweep,
            Exit::Delivered,
        )
        .await;
    });

    assert!(!mine.exists(), "this attempt's own temp must be swept");
    assert_eq!(
        std::fs::read(&sibling).unwrap(),
        b"sibling",
        "a sibling attempt's completed remux must survive the sweep"
    );
    assert!(
        staging.exists(),
        "the directory must stay while a sibling temp is in it"
    );

    // With the last temp gone, the now-empty directory is reclaimed.
    std::fs::remove_file(&sibling).unwrap();
    crate::runtime::tokio_rt().block_on(async {
        sweep_live_capture(
            &out,
            &part,
            &state,
            &staging,
            None,
            Staging::Sweep,
            Exit::Delivered,
        )
        .await;
    });
    // Staging is the dest dir itself: it is never removed, only the item's
    // `grab-<id>-*` files are swept.
    assert!(
        staging.exists(),
        "the dest dir is never removed by the sweep"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Sweep must follow reap: sweeping first could delete a file the recorder still writes.
/// A real `Child` can't show this (both orders look the same when reap is quick), so
/// the sequence is driven by controlled futures instead.
#[test]
fn the_sweep_follows_the_reap() {
    let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let (reap_log, sweep_log) = (log.clone(), log.clone());
    crate::runtime::tokio_rt().block_on(reap_then_sweep(
        async move {
            reap_log.borrow_mut().push("reap");
        },
        move || async move {
            sweep_log.borrow_mut().push("sweep");
        },
    ));
    assert_eq!(
        *log.borrow(),
        ["reap", "sweep"],
        "the recorder must be reaped before its scratch is reclaimed"
    );
}

/// Non-vacuity guard: a stalled reap must stall the sweep, or scratch is reclaimed
/// under a still-running recorder.
#[test]
fn a_stalled_reap_blocks_the_sweep() {
    let swept = std::rc::Rc::new(std::cell::Cell::new(false));
    let sweep_flag = swept.clone();
    crate::runtime::tokio_rt().block_on(async {
        tokio::select! {
            _ = reap_then_sweep(
                std::future::pending::<()>(),
                move || async move { sweep_flag.set(true); },
            ) => {}
            // Nothing can satisfy the reap arm: give the sequence a real window to misbehave.
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }
    });
    assert!(
        !swept.get(),
        "the sweep ran while the recorder had not been reaped: scratch would be \
         deleted under a live writer"
    );
}

/// Item 3: the post-wait drain must be bounded — a child that exits while a
/// grandchild keeps its stdout pipe open must not stall the subtitle probe.
/// The fake binary prints valid subtitle JSON, exits at once, and leaves
/// `sleep 30` holding the pipe: without the drain timeout the probe blocks
/// ~30 s and returns the parsed language; with it, it gives up after 5 s and
/// yields `None`.
#[cfg(unix)]
#[test]
fn subtitle_probe_drain_times_out_when_pipe_stays_open() {
    let base = std::env::temp_dir().join(format!("grab-drain-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let bin = base.join("fake-ytdlp-hanging-pipe");
    std::fs::write(
        &bin,
        "#!/bin/sh\nprintf '{\"subtitles\": {\"en\": [{\"url\": \"http://x/y.vtt\"}]}}'\n( sleep 30 >&1 & )\nexit 0\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let job = VideoJob {
        item_id: 1,
        page_url: "https://example.com/v".into(),
        playlist_item_id: None,
        quality: "1080p".into(),
        audio_only: false,
        dest: base.join("v.mp4"),
        speed_limit: None,
        keep_server_date: false,
        video_format_id: None,
        is_live: false,
        live_from_start: false,
        newest_codecs: true,
        cookies_browser: "none".into(),
        subtitles: Some("en".into()),
        embed_subs: false,
        sponsorblock_remove: false,
        sponsorblock_mark: false,
        remux_video: None,
        embed_chapters: false,
        proxy: None,
    };
    let (_abort_tx, mut abort_rx) = tokio::sync::oneshot::channel::<StopIntent>();
    let start = std::time::Instant::now();
    let result =
        crate::runtime::tokio_rt().block_on(resolve_subtitle_lang(&bin, &job, &mut abort_rx, None));
    let elapsed = start.elapsed();
    assert_eq!(
        result,
        Ok(None),
        "the drain must give up on a pipe that outlives the child, not parse the late output"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "the probe stalled on the drain: {elapsed:?}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// Item 2 (handoff-4): on drain timeout the drain task must be aborted, not
/// detached — dropping the `JoinHandle` leaves the task parked on the pipe
/// until the grandchild exits (~30 s). The guard sender fires only when the
/// task's future is actually dropped, so a detached task fails this test.
#[test]
fn drain_with_timeout_aborts_lingering_task() {
    crate::runtime::tokio_rt().block_on(async {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        struct AbortGuard(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for AbortGuard {
            fn drop(&mut self) {
                if let Some(tx) = self.0.take() {
                    let _ = tx.send(());
                }
            }
        }
        let lingering: tokio::task::JoinHandle<Vec<u8>> = tokio::spawn(async move {
            let _guard = AbortGuard(Some(tx));
            std::future::pending::<()>().await;
            unreachable!("a lingering drain never resolves on its own")
        });
        let result = drain_with_timeout(lingering, std::time::Duration::from_millis(50)).await;
        assert!(
            result.is_none(),
            "a drain that outlives its bound must yield None"
        );
        let aborted = tokio::time::timeout(std::time::Duration::from_secs(5), rx).await;
        assert!(
            aborted.is_ok(),
            "drain task lingered after the timeout instead of being aborted"
        );
    });
}

/// Item 7: the guard's handle must stay private — construction goes through the
/// constructor so the abort-on-drop invariant can't be bypassed — and dropping
/// the guard must abort the watcher task.
/// RED was compile-time: the test calls `RecordingWatcherGuard::new()`, which
/// doesn't exist pre-GREEN (E0599) — an API-constraint RED, failing for the
/// right reason (no way to build the guard except through the constructor).
#[test]
fn recording_watcher_guard_aborts_on_drop() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let flag = std::sync::Arc::new(AtomicBool::new(false));
    let flag2 = flag.clone();
    crate::runtime::tokio_rt().block_on(async {
        let handle = tokio::spawn(async move {
            loop {
                flag2.store(true, Ordering::SeqCst);
                tokio::task::yield_now().await;
            }
        });
        {
            let _guard = RecordingWatcherGuard::new(handle);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert!(
                flag.load(Ordering::SeqCst),
                "test setup: the task must be running inside the guard"
            );
        }
        // The guard dropped: an aborted task can never set the flag again.
        flag.store(false, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !flag.load(Ordering::SeqCst),
            "dropping the guard must abort the watcher task"
        );
    });
}

/// Staging claim skips existing base files: if the user has Title-1.mp4,
/// the claim must choose Title-2.mp4 and leave Title-1.mp4 byte-identical.
/// Regression test for the live-staging collision data loss.
#[test]
fn staging_claim_skips_existing_base_file() {
    let dir = std::env::temp_dir().join(format!("grab-staging-claim-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // User's existing file: Title-1.mp4 with known content.
    let user_file = dir.join("Title-1.mp4");
    let user_content = b"user's precious video content";
    std::fs::write(&user_file, user_content).unwrap();

    // Simulate a VideoJob with dest Title.mp4 in the same dir.
    let job = VideoJob {
        item_id: 1,
        page_url: "https://example.com/v".into(),
        playlist_item_id: None,
        quality: "1080p".into(),
        audio_only: false,
        dest: dir.join("Title.mp4"),
        speed_limit: None,
        keep_server_date: false,
        video_format_id: None,
        is_live: false,
        live_from_start: false,
        newest_codecs: true,
        cookies_browser: "none".into(),
        subtitles: None,
        embed_subs: false,
        sponsorblock_remove: false,
        sponsorblock_mark: false,
        remux_video: None,
        embed_chapters: false,
        proxy: None,
    };

    // Claim a staging name: should skip Title.mp4 (== dest) and Title-1.mp4 (exists),
    // landing on Title-2.mp4.
    let (out, staging_name) = claim_staging_name(&job, "mp4").unwrap();

    assert_eq!(staging_name, "Title-2.mp4");
    assert_eq!(out, dir.join("Title-2.mp4"));

    // User's file must be byte-identical.
    assert_eq!(std::fs::read(&user_file).unwrap(), user_content);

    // The .part claim file should exist (we claimed it).
    let part = dir.join("Title-2.mp4.part");
    assert!(part.exists());

    // Cleanup
    let _ = std::fs::remove_dir_all(&dir);
}
