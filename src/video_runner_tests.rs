//! Tests for the live-capture runner's private cleanup policy: the sweep helper
//! and exit/staging enums are private to `video_runner`, so their retention
//! contract is only observable from in here.

use super::*;

use crate::video_argv::VideoJob;

/// A removal that wins before the final claim must keep the partial in staging
/// and must not create the final destination.
#[test]
fn dash_preserve_does_not_deliver_after_discard_wins() {
    let base =
        std::env::temp_dir().join(format!("grab-dash-preserve-discard-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let staging = base.join("staging");
    std::fs::create_dir_all(&staging).unwrap();

    let part = staging.join("video.f137.mp4");
    let part_content = vec![b'p'; 2048];
    std::fs::write(&part, &part_content).unwrap();
    let dest = base.join("video.mp4");
    let gate = AttemptGate::new();
    assert!(gate.discard());

    let result = crate::runtime::tokio_rt().block_on(try_merge_dash_partials(
        std::path::Path::new("unused-ffmpeg"),
        &staging,
        &dest,
        "https://example.com/video",
        &gate,
    ));

    assert_eq!(result, None);
    assert!(!dest.exists());
    assert!(part.exists());
    assert_eq!(std::fs::read(&part).unwrap(), part_content);
    assert!(!gate.was_delivered());
    let _ = std::fs::remove_dir_all(&base);
}

/// Successful preservation marks delivery so a concurrent row removal can
/// reclaim the delivered orphan after the worker exits.
#[test]
fn dash_preserve_success_marks_delivery() {
    let base =
        std::env::temp_dir().join(format!("grab-dash-preserve-success-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let staging = base.join("staging");
    std::fs::create_dir_all(&staging).unwrap();

    let part = staging.join("video.f137.mp4");
    let part_content = vec![b'p'; 2048];
    std::fs::write(&part, &part_content).unwrap();
    let dest = base.join("video.mp4");
    let gate = AttemptGate::new();

    let result = crate::runtime::tokio_rt().block_on(async {
        let result = try_merge_dash_partials(
            std::path::Path::new("unused-ffmpeg"),
            &staging,
            &dest,
            "https://example.com/video",
            &gate,
        )
        .await;
        gate.wait_for_rollbacks().await;
        result
    });

    assert_eq!(result, Some(part_content.len() as u64));
    assert_eq!(std::fs::read(&dest).unwrap(), part_content);
    assert!(!part.exists());
    assert!(gate.was_delivered());
    let _ = std::fs::remove_dir_all(&base);
}

/// A destination created after download preflight must never be replaced when
/// preserving a single completed DASH part on Stop.
#[test]
fn dash_preserve_single_part_does_not_clobber_existing_destination() {
    let base =
        std::env::temp_dir().join(format!("grab-dash-preserve-single-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let staging = base.join("staging");
    std::fs::create_dir_all(&staging).unwrap();

    let part = staging.join("video.f137.mp4");
    let part_content = vec![b'p'; 2048];
    std::fs::write(&part, &part_content).unwrap();
    let dest = base.join("video.mp4");
    let user_content = b"user-created file".to_vec();
    std::fs::write(&dest, &user_content).unwrap();
    let gate = AttemptGate::new();

    let result = crate::runtime::tokio_rt().block_on(try_merge_dash_partials(
        std::path::Path::new("unused-ffmpeg"),
        &staging,
        &dest,
        "https://example.com/video",
        &gate,
    ));

    assert_eq!(result, None);
    assert_eq!(std::fs::read(&dest).unwrap(), user_content);
    assert_eq!(std::fs::read(&part).unwrap(), part_content);
    let _ = std::fs::remove_dir_all(&base);
}

/// A failed final claim after FFmpeg has produced the merged temporary must
/// leave the pre-existing destination and both source fragments intact.
#[cfg(unix)]
#[test]
fn dash_preserve_merge_does_not_clobber_existing_destination() {
    use std::os::unix::fs::PermissionsExt as _;

    let base =
        std::env::temp_dir().join(format!("grab-dash-preserve-merge-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let staging = base.join("staging");
    std::fs::create_dir_all(&staging).unwrap();

    let video_part = staging.join("video.f137.mp4");
    let audio_part = staging.join("audio.f140.m4a");
    std::fs::write(&video_part, vec![b'v'; 2048]).unwrap();
    std::fs::write(&audio_part, vec![b'a'; 2048]).unwrap();

    let fake_ffmpeg = base.join("fake-ffmpeg");
    std::fs::write(
        &fake_ffmpeg,
        "#!/bin/sh\nfor output do :; done\nprintf 'merged output' > \"$output\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake_ffmpeg, std::fs::Permissions::from_mode(0o755)).unwrap();

    let dest = base.join("video.mp4");
    let user_content = b"user-created file".to_vec();
    std::fs::write(&dest, &user_content).unwrap();
    let user_temp = base.join("video.merged.tmp");
    let user_temp_content = b"unrelated user file".to_vec();
    std::fs::write(&user_temp, &user_temp_content).unwrap();
    let gate = AttemptGate::new();

    let result = crate::runtime::tokio_rt().block_on(try_merge_dash_partials(
        &fake_ffmpeg,
        &staging,
        &dest,
        "https://example.com/video",
        &gate,
    ));

    assert_eq!(result, None);
    assert_eq!(std::fs::read(&dest).unwrap(), user_content);
    assert_eq!(
        std::fs::read(&user_temp).unwrap(),
        user_temp_content,
        "the user's similarly named temp file must be untouched"
    );
    assert!(video_part.exists(), "video fragment must remain resumable");
    assert!(audio_part.exists(), "audio fragment must remain resumable");
    assert!(
        std::fs::read_dir(&staging).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".grab-dash-merge-")),
        "private merge workspace must be cleaned up"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn live_remux_retry_policy_repairs_timestamps_before_dropping_aac_filter() {
    assert_eq!(
        next_live_remux_attempt(true, false, "non-monotonous DTS in output"),
        Some((true, true))
    );
    assert_eq!(
        next_live_remux_attempt(true, true, "non-monotonous DTS in output"),
        Some((false, true))
    );
    assert_eq!(
        next_live_remux_attempt(false, true, "generic failure"),
        None
    );
}

#[test]
fn live_remux_retry_policy_drops_aac_filter_once_for_other_errors() {
    assert_eq!(
        next_live_remux_attempt(true, false, "disk I/O error"),
        Some((false, false))
    );
    assert_eq!(
        next_live_remux_attempt(false, false, "disk I/O error"),
        None
    );
}

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
    crate::runtime::tokio_rt()
        .block_on(reap_then_sweep(
            async move {
                reap_log.borrow_mut().push("reap");
            },
            None,
            move || async move {
                sweep_log.borrow_mut().push("sweep");
            },
        ))
        .expect("quiescent group should permit scratch sweep");
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
                None,
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

#[cfg(target_os = "linux")]
#[test]
fn hls_group_quiescence_waits_for_extractor_descendants() {
    let base = std::env::temp_dir().join(format!("grab-hls-quiescence-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let bin = base.join("fake-ytdlp-background-child");
    let marker = base.join("descendant-finished");
    std::fs::write(
        &bin,
        "#!/bin/sh\n(sleep 1; printf done > \"$MARKER_PATH\") &\nexit 0\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    crate::runtime::tokio_rt().block_on(async {
        let mut command = tokio::process::Command::new(&bin);
        command.process_group(0);
        command
            .env("MARKER_PATH", &marker)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = command.spawn().unwrap();
        let mut group = ProcessGroupGuard::new(&child);
        let pgid = group.pgid();
        assert!(pgid.is_some());

        let status = child.wait().await.unwrap();
        assert!(status.success());
        group.disarm();
        assert!(
            !marker.exists(),
            "the descendant must still be running after its leader exits"
        );

        await_ytdlp_group_quiescence(pgid.as_ref())
            .await
            .expect("extractor descendants must become quiescent");
        assert_eq!(
            std::fs::read(&marker).unwrap(),
            b"done",
            "quiescence must wait until extractor descendants finish"
        );
    });
    let _ = std::fs::remove_dir_all(&base);
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
        info_json: None,
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

/// Oversized extractor metadata is discarded at the same hard ceiling as the main JSON probe.
#[cfg(unix)]
#[test]
fn subtitle_probe_rejects_oversized_json_output() {
    let base = std::env::temp_dir().join(format!("grab-subtitle-oversize-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let bin = base.join("fake-ytdlp-oversized-output");
    let output_bytes = crate::video_spawn::MAX_STDOUT_BYTES + 1;
    std::fs::write(
        &bin,
        format!("#!/bin/sh\nhead -c {output_bytes} /dev/zero\n"),
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
        info_json: None,
    };
    let (_abort_tx, mut abort_rx) = tokio::sync::oneshot::channel::<StopIntent>();
    let result =
        crate::runtime::tokio_rt().block_on(resolve_subtitle_lang(&bin, &job, &mut abort_rx, None));

    assert_eq!(
        result,
        Ok(None),
        "oversized extractor metadata must be rejected without failing the media download"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[cfg(unix)]
#[test]
fn subtitle_probe_disarms_group_after_leader_is_reaped() {
    let base = std::env::temp_dir().join(format!("grab-subtitle-reaped-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let bin = base.join("fake-ytdlp-exit");
    std::fs::write(&bin, "#!/bin/sh\nexit 1\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    crate::runtime::tokio_rt().block_on(async {
        let mut command = tokio::process::Command::new(&bin);
        command.process_group(0);
        let mut child = command.spawn().unwrap();
        let mut group = ProcessGroupGuard::new(&child);
        assert!(group.is_armed());

        let status = child.wait().await.unwrap();
        assert!(!status.success());
        assert!(
            group.is_armed(),
            "the test must exercise a reaped, armed guard"
        );

        finish_subtitle_probe_child(&mut child, &mut group).await;
        assert!(
            !group.is_armed(),
            "a reaped child must disarm its group guard before it is dropped"
        );
    });
    let _ = std::fs::remove_dir_all(&base);
}

#[cfg(unix)]
#[test]
fn subtitle_probe_cleanup_reaps_a_live_process_group() {
    let base = std::env::temp_dir().join(format!("grab-subtitle-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let bin = base.join("fake-ytdlp-sleep");
    std::fs::write(&bin, "#!/bin/sh\nsleep 30\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    crate::runtime::tokio_rt().block_on(async {
        let mut command = tokio::process::Command::new(&bin);
        command.process_group(0);
        let mut child = command.spawn().unwrap();
        let mut group = ProcessGroupGuard::new(&child);
        assert!(group.is_armed());

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            finish_subtitle_probe_child(&mut child, &mut group),
        )
        .await
        .expect("subtitle probe cleanup must reap a live child promptly");

        assert!(child.id().is_none(), "the live probe leader must be reaped");
        assert!(!group.is_armed(), "cleanup must disarm the group guard");
    });
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
        let lingering: tokio::task::JoinHandle<std::io::Result<Vec<u8>>> =
            tokio::spawn(async move {
                let _guard = AbortGuard(Some(tx));
                std::future::pending::<std::io::Result<Vec<u8>>>().await
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
        // The guard dropped: abort() is asynchronous, so give the worker a
        // chance to deliver it before clearing the flag — otherwise a task
        // still mid-iteration could set the flag after we clear it and the
        // assert below would flake. Once the abort has landed, an aborted
        // task can never set the flag again.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        flag.store(false, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !flag.load(Ordering::SeqCst),
            "dropping the guard must abort the watcher task"
        );
    });
}

/// Staging claim skips existing base files: if the user has Title.1.mp4,
/// the claim must choose Title.1-1.mp4 and leave Title.1.mp4 byte-identical.
/// Regression test for the live-staging collision data loss.
#[test]
fn staging_claim_skips_existing_base_file() {
    let dir = std::env::temp_dir().join(format!("grab-staging-claim-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // User's existing file: Title.1.mp4 with known content.
    let user_file = dir.join("Title.1.mp4");
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
        info_json: None,
    };

    // Claim a staging name: should skip Title.mp4 (== dest) and Title.1.mp4 (exists),
    // landing on Title.1-1.mp4.
    let (out, staging_name) = claim_staging_name(&job.dest, job.item_id, "mp4").unwrap();

    assert_eq!(staging_name, "Title.1-1.mp4");
    assert_eq!(out, dir.join("Title.1-1.mp4"));

    // User's file must be byte-identical.
    assert_eq!(std::fs::read(&user_file).unwrap(), user_content);

    // The .part claim file should exist (we claimed it).
    let part = dir.join("Title.1-1.mp4.part");
    assert!(part.exists());

    // Cleanup
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn live_info_json_args_none_is_empty() {
    assert!(live_info_json_args(None).is_empty());
}

#[test]
fn live_info_json_args_some_emits_flag_pair() {
    let args = live_info_json_args(Some(std::path::Path::new("/tmp/.grab-live-1.json")));
    assert_eq!(
        args,
        vec![
            "--load-info-json".to_string(),
            "/tmp/.grab-live-1.json".to_string()
        ]
    );
}

#[test]
fn splice_before_terminator_inserts_before_dashes() {
    let mut argv = vec![
        "-f".to_string(),
        "best".to_string(),
        "--".to_string(),
        "https://example.com".to_string(),
    ];
    splice_before_terminator(
        &mut argv,
        vec!["--load-info-json".to_string(), "/tmp/info.json".to_string()],
    );
    let flag_pos = argv.iter().position(|a| a == "--load-info-json").unwrap();
    let term_pos = argv.iter().position(|a| a == "--").unwrap();
    assert!(flag_pos < term_pos);
    assert_eq!(
        argv,
        vec![
            "-f",
            "best",
            "--load-info-json",
            "/tmp/info.json",
            "--",
            "https://example.com"
        ]
    );
}

#[test]
fn splice_before_terminator_empty_is_noop() {
    let mut argv = vec!["-f".to_string(), "--".to_string(), "url".to_string()];
    splice_before_terminator(&mut argv, vec![]);
    assert_eq!(argv, vec!["-f", "--", "url"]);
}

#[test]
fn splice_before_terminator_missing_terminator_is_noop() {
    let mut argv = vec!["-f".to_string(), "best".to_string()];
    splice_before_terminator(&mut argv, vec!["--flag".to_string()]);
    assert_eq!(argv, vec!["-f", "best"]);
}

#[test]
fn live_remux_retries_known_mp4_timestamp_mux_failures_only() {
    assert!(needs_live_timestamp_repair(
        "[out#0/mp4] Error writing trailer: Not yet implemented in FFmpeg, patches welcome"
    ));
    assert!(needs_live_timestamp_repair(
        "[mp4] pts/dts pair unsupported"
    ));
    assert!(needs_live_timestamp_repair(
        "Non-monotonous DTS in output stream 0:1"
    ));
    assert!(
        !needs_live_timestamp_repair("Error writing trailer: No space left on device"),
        "unrelated output failures must not trigger another full remux"
    );
}

#[tokio::test]
async fn ytdlp_stdout_reader_lossily_decodes_and_continues_after_invalid_utf8() {
    let bytes = b"before\ninvalid:\xff\nafter\n".to_vec();
    let mut reader = super::YtdlpStdoutReader::new(std::io::Cursor::new(bytes));

    assert_eq!(
        reader.next_line().await.unwrap(),
        super::YtdlpStdoutLine::Line("before".to_string())
    );
    assert_eq!(
        reader.next_line().await.unwrap(),
        super::YtdlpStdoutLine::Line("invalid:\u{fffd}".to_string())
    );
    assert_eq!(
        reader.next_line().await.unwrap(),
        super::YtdlpStdoutLine::Line("after".to_string())
    );
    assert_eq!(
        reader.next_line().await.unwrap(),
        super::YtdlpStdoutLine::Eof
    );
}

#[tokio::test]
async fn ytdlp_stdout_reader_discards_oversized_records_and_keeps_draining() {
    let mut bytes = vec![b'x'; super::MAX_YTDLP_STDOUT_LINE_BYTES + 1];
    bytes.extend_from_slice(b"\nvalid\n");
    let mut reader = super::YtdlpStdoutReader::new(std::io::Cursor::new(bytes));

    assert_eq!(
        reader.next_line().await.unwrap(),
        super::YtdlpStdoutLine::SkippedOversized
    );
    assert_eq!(
        reader.next_line().await.unwrap(),
        super::YtdlpStdoutLine::Line("valid".to_string())
    );
    assert_eq!(
        reader.next_line().await.unwrap(),
        super::YtdlpStdoutLine::Eof
    );
}

#[tokio::test]
async fn ytdlp_stdout_reader_emits_a_final_record_without_a_newline() {
    let mut reader = super::YtdlpStdoutReader::new(std::io::Cursor::new(b"final".to_vec()));

    assert_eq!(
        reader.next_line().await.unwrap(),
        super::YtdlpStdoutLine::Line("final".to_string())
    );
    assert_eq!(
        reader.next_line().await.unwrap(),
        super::YtdlpStdoutLine::Eof
    );
}

#[test]
fn split_format_total_is_unknown_on_overflow() {
    assert_eq!(combined_format_size(Some(12), Some(30)), Some(42));
    assert_eq!(combined_format_size(Some(u64::MAX), Some(1)), None);
    assert_eq!(combined_format_size(Some(12), None), None);
}

#[test]
fn cumulative_progress_saturates_instead_of_wrapping() {
    assert_eq!(add_progress_bytes(12, 30), 42);
    assert_eq!(add_progress_bytes(u64::MAX, 1), u64::MAX);
}

#[tokio::test]
async fn video_resolution_stop_drops_the_pending_probe_future() {
    struct DropNotice(Option<tokio::sync::oneshot::Sender<()>>);
    impl Drop for DropNotice {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    let (stop_sender, stop_receiver) = tokio::sync::oneshot::channel();
    let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
    let (dropped_sender, dropped_receiver) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut abort = stop_receiver;
        let operation = async move {
            let _drop_notice = DropNotice(Some(dropped_sender));
            let _ = started_sender.send(());
            std::future::pending::<()>().await;
        };
        await_video_operation_or_stop(&mut abort, operation).await
    });

    started_receiver.await.expect("probe future must start");
    stop_sender
        .send(StopIntent::Pause)
        .expect("probe receiver must remain active");
    assert_eq!(
        task.await.expect("stop race task must complete"),
        Err(StopIntent::Pause)
    );
    dropped_receiver
        .await
        .expect("stop intent must drop the pending probe future");
}

#[cfg(unix)]
#[tokio::test]
async fn live_remux_does_not_overwrite_a_foreign_final_slot() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = tempfile::tempdir().expect("create remux test directory");
    let ffmpeg = directory.path().join("fake-ffmpeg");
    std::fs::write(&ffmpeg, "#!/bin/sh\nfor arg in \"$@\"; do output=\"$arg\"; done\nprintf partial > \"$output\"\ndest=\"${output%.part}\"\nprintf foreign > \"$dest\"\n")
        .expect("write fake FFmpeg executable");
    std::fs::set_permissions(&ffmpeg, std::fs::Permissions::from_mode(0o700))
        .expect("make fake FFmpeg executable");
    let source = directory.path().join("capture.ts");
    std::fs::write(&source, b"source").expect("write fake capture");
    let final_slot = directory.path().join("final.mp4");
    let gate = AttemptGate::new();

    let result = remux_live_capture(
        &ffmpeg,
        &source,
        &final_slot,
        false,
        Duration::from_secs(5),
        "https://example.com/live",
        &gate,
    )
    .await;

    assert!(result.is_err());
    assert_eq!(std::fs::read(&final_slot).unwrap(), b"foreign");
    assert!(!directory.path().join("final.mp4.part").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn discarding_during_live_remux_reaps_ffmpeg_and_removes_its_partial() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = tempfile::tempdir().expect("create remux test directory");
    let ffmpeg = directory.path().join("fake-ffmpeg");
    let script = "#!/bin/sh\nfor arg in \"$@\"; do output=\"$arg\"; done\nprintf partial > \"$output\"\nexec sleep 30\n";
    std::fs::write(&ffmpeg, script).expect("write fake FFmpeg executable");
    std::fs::set_permissions(&ffmpeg, std::fs::Permissions::from_mode(0o700))
        .expect("make fake FFmpeg executable");
    let source = directory.path().join("capture.ts");
    std::fs::write(&source, b"source").expect("write fake capture");
    let final_slot = directory.path().join("final.mp4");
    let partial = directory.path().join("final.mp4.part");
    let gate = AttemptGate::new();
    let worker_gate = gate.clone();
    let worker_ffmpeg = ffmpeg.clone();
    let worker_source = source.clone();
    let worker_final_slot = final_slot.clone();
    let worker = tokio::spawn(async move {
        remux_live_capture(
            &worker_ffmpeg,
            &worker_source,
            &worker_final_slot,
            false,
            Duration::from_secs(30),
            "https://example.com/live",
            &worker_gate,
        )
        .await
    });

    let partial_ready = tokio::time::timeout(Duration::from_secs(3), async {
        while !partial.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    if partial_ready.is_err() {
        worker.abort();
        let _ = worker.await;
        panic!("fake FFmpeg must create its partial output");
    }
    if !gate.discard() {
        worker.abort();
        let _ = worker.await;
        panic!("discard must win before remux publication");
    }
    let mut worker = worker;
    let result = match tokio::time::timeout(Duration::from_secs(8), &mut worker).await {
        Ok(joined) => joined.expect("remux task must not panic"),
        Err(_) => {
            worker.abort();
            let _ = worker.await;
            panic!("discard must stop the remux promptly");
        }
    };

    assert!(result.is_err());
    assert!(
        !partial.exists(),
        "quiescent discarded partial must be removed"
    );
    assert!(
        !final_slot.exists(),
        "discarded remux must not publish a final slot"
    );
}

#[test]
fn closed_live_capture_stop_channel_preserves_recorded_media() {
    assert_eq!(
        live_capture_abort_result::<()>(Err(())),
        (true, false),
        "a dropped stop sender must preserve an in-progress recording"
    );
}

#[test]
fn explicit_live_capture_discard_does_not_adopt_recorded_media() {
    assert_eq!(
        live_capture_abort_result::<()>(Ok(crate::video::StopIntent::Discard)),
        (true, true)
    );
}

#[test]
fn live_capture_pause_and_finish_preserve_recorded_media() {
    assert_eq!(
        live_capture_abort_result::<()>(Ok(crate::video::StopIntent::Pause)),
        (true, false)
    );
    assert_eq!(
        live_capture_abort_result::<()>(Ok(crate::video::StopIntent::Finish)),
        (true, false)
    );
}

#[cfg(unix)]
#[test]
fn destination_preflight_rejects_a_dangling_symlink() {
    use std::os::unix::fs::symlink;

    let dir = std::env::temp_dir().join(format!("grab-destination-symlink-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let destination = dir.join("dangling.mp4");
    symlink(dir.join("missing-target"), &destination).unwrap();

    let result = crate::runtime::tokio_rt().block_on(ensure_destination_unclaimed(&destination));
    assert!(
        result.is_err(),
        "a dangling symlink still occupies the destination path"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn destination_preflight_allows_missing_and_rejects_regular_files() {
    let dir =
        std::env::temp_dir().join(format!("grab-destination-preflight-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let destination = dir.join("video.mp4");

    assert!(
        crate::runtime::tokio_rt()
            .block_on(ensure_destination_unclaimed(&destination))
            .is_ok()
    );
    std::fs::write(&destination, b"foreign").unwrap();
    assert!(
        crate::runtime::tokio_rt()
            .block_on(ensure_destination_unclaimed(&destination))
            .is_err()
    );
    let _ = std::fs::remove_dir_all(&dir);
}
