//! Tests for crash-safe ordered DASH track assembly.

use super::*;
use std::path::PathBuf;

fn test_directory() -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock must follow the Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "grab-ordered-writer-{}-{nonce}",
        std::process::id()
    ))
}

fn stop_channel() -> (
    tokio::sync::oneshot::Sender<crate::video::StopIntent>,
    tokio::sync::oneshot::Receiver<crate::video::StopIntent>,
) {
    tokio::sync::oneshot::channel()
}

fn resource_checkpoint(bytes: &[u8]) -> CommittedResource {
    let digest = Sha256::digest(bytes);
    CommittedResource {
        length: u64::try_from(bytes.len()).expect("test resource length fits u64"),
        sha256: digest_hex(&digest),
    }
}

#[tokio::test]
async fn incremental_writer_checkpoints_and_resumes_between_resources() {
    let directory = test_directory();
    tokio::fs::create_dir_all(&directory)
        .await
        .expect("create staging");
    tokio::fs::write(directory.join("first.bin"), b"first")
        .await
        .expect("write first");
    tokio::fs::write(directory.join("second.bin"), b"second")
        .await
        .expect("write second");
    let resources = vec!["first.bin".to_string(), "second.bin".to_string()];
    let (_stop_tx, mut abort) = stop_channel();

    let (Some(mut writer), committed) =
        OrderedTrackWriter::open(resources.clone(), &directory, "track.mp4")
            .await
            .expect("open ordered writer")
    else {
        panic!("a fresh track must return an active writer");
    };
    assert_eq!(committed, 0);
    assert_eq!(writer.next_resource_index(), 0);
    assert!(writer.append_next(&mut abort).await.expect("append first"));

    let checkpoint: WriterCheckpoint = serde_json::from_slice(
        &tokio::fs::read(directory.join("track.mp4.checkpoint.json"))
            .await
            .expect("read checkpoint"),
    )
    .expect("decode checkpoint");
    assert_eq!(checkpoint.next_resource_index, 1);
    assert_eq!(checkpoint.committed_offset, 5);
    assert_eq!(
        read_resource_journal(
            &directory.join("track.mp4.resources"),
            &directory,
            1,
            false,
        )
        .await
        .expect("read committed resource journal"),
        vec![resource_checkpoint(b"first")]
    );
    drop(writer);

    let (Some(mut writer), committed) =
        OrderedTrackWriter::open(resources, &directory, "track.mp4")
            .await
            .expect("resume ordered writer")
    else {
        panic!("an interrupted track must return an active writer");
    };
    assert_eq!(committed, 5);
    assert_eq!(writer.next_resource_index(), 1);
    assert!(writer.append_next(&mut abort).await.expect("append second"));
    writer.finish().await.expect("publish ordered track");

    assert_eq!(
        tokio::fs::read(directory.join("track.mp4"))
            .await
            .expect("read track"),
        b"firstsecond"
    );
    tokio::fs::remove_dir_all(directory)
        .await
        .expect("remove staging");
}

#[tokio::test]
async fn aborting_an_append_keeps_the_uncommitted_source() {
    let directory = test_directory();
    tokio::fs::create_dir_all(&directory)
        .await
        .expect("create staging");
    let resources = vec!["first.bin".to_string()];
    tokio::fs::write(directory.join("first.bin"), b"first")
        .await
        .expect("write source");
    let (Some(mut writer), _) =
        OrderedTrackWriter::open_retaining_checkpoint(resources, &directory, "track.mp4")
            .await
            .expect("open retained writer")
    else {
        panic!("a fresh track must return an active writer");
    };
    let (stop_tx, mut abort) = stop_channel();
    assert!(stop_tx.send(crate::video::StopIntent::Pause).is_ok());

    assert!(!writer.append_next(&mut abort).await.expect("abort append"));
    assert!(directory.join("first.bin").is_file());
    assert_eq!(
        tokio::fs::metadata(directory.join("track.mp4.part"))
            .await
            .expect("inspect truncated part")
            .len(),
        0
    );
    assert!(!directory.join("track.mp4.checkpoint.json").exists());
    assert!(!directory.join("track.mp4.resources").exists());

    drop(writer);
    tokio::fs::remove_dir_all(directory)
        .await
        .expect("remove staging");
}

#[tokio::test]
async fn writes_resources_in_manifest_order_and_publishes_track() {
    let directory = test_directory();
    tokio::fs::create_dir_all(&directory)
        .await
        .expect("create staging");
    tokio::fs::write(directory.join("second.bin"), b"second")
        .await
        .expect("write second");
    tokio::fs::write(directory.join("first.bin"), b"first")
        .await
        .expect("write first");
    let resources = vec!["first.bin".to_string(), "second.bin".to_string()];
    let (_stop_tx, mut abort) = stop_channel();

    assert!(
        write_ordered_track(&resources, &directory, "track.mp4", &mut abort)
            .await
            .expect("write track")
    );
    assert_eq!(
        tokio::fs::read(directory.join("track.mp4"))
            .await
            .expect("read track"),
        b"firstsecond"
    );
    assert!(!directory.join("track.mp4.checkpoint.json").exists());
    tokio::fs::remove_dir_all(directory)
        .await
        .expect("remove staging");
}

#[tokio::test]
async fn resumes_from_checkpoint_and_truncates_uncommitted_tail() {
    let directory = test_directory();
    tokio::fs::create_dir_all(&directory)
        .await
        .expect("create staging");
    tokio::fs::write(directory.join("first.bin"), b"first")
        .await
        .expect("write first");
    tokio::fs::write(directory.join("second.bin"), b"second")
        .await
        .expect("write second");
    let resources = vec!["first.bin".to_string(), "second.bin".to_string()];
    let fingerprint = plan_fingerprint(&resources).expect("fingerprint");
    let checkpoint = WriterCheckpoint {
        version: CHECKPOINT_VERSION,
        plan_fingerprint: fingerprint,
        next_resource_index: 1,
        committed_offset: 5,
    };
    tokio::fs::write(directory.join("track.mp4.part"), b"first-uncommitted-tail")
        .await
        .expect("write interrupted output");
    tokio::fs::write(
        directory.join("track.mp4.checkpoint.json"),
        serde_json::to_vec(&checkpoint).expect("serialize checkpoint"),
    )
    .await
    .expect("write checkpoint");
    write_resource_journal(
        &directory.join("track.mp4.resources"),
        &directory,
        &[resource_checkpoint(b"first"), resource_checkpoint(b"second")],
    )
    .await
    .expect("write journal with an uncommitted trailing record");
    let (_stop_tx, mut abort) = stop_channel();

    let (Some(mut writer), committed) =
        OrderedTrackWriter::open(resources.clone(), &directory, "track.mp4")
            .await
            .expect("resume track and truncate the journal tail")
    else {
        panic!("an interrupted track must return an active writer");
    };
    assert_eq!(committed, 5);
    assert_eq!(
        read_resource_journal(
            &directory.join("track.mp4.resources"),
            &directory,
            1,
            false,
        )
        .await
        .expect("read truncated journal"),
        vec![resource_checkpoint(b"first")]
    );
    assert!(writer.append_next(&mut abort).await.expect("append second"));
    writer.finish().await.expect("publish resumed track");
    assert_eq!(
        tokio::fs::read(directory.join("track.mp4"))
            .await
            .expect("read track"),
        b"firstsecond"
    );
    tokio::fs::remove_dir_all(directory)
        .await
        .expect("remove staging");
}

#[tokio::test]
async fn rejects_checkpoint_for_a_different_resource_order() {
    let directory = test_directory();
    tokio::fs::create_dir_all(&directory)
        .await
        .expect("create staging");
    let resources = vec!["first.bin".to_string(), "second.bin".to_string()];
    let checkpoint = WriterCheckpoint {
        version: CHECKPOINT_VERSION,
        plan_fingerprint: plan_fingerprint(&[
            "second.bin".to_string(),
            "first.bin".to_string(),
        ])
        .expect("fingerprint"),
        next_resource_index: 1,
        committed_offset: 5,
    };
    tokio::fs::write(
        directory.join("track.mp4.checkpoint.json"),
        serde_json::to_vec(&checkpoint).expect("serialize checkpoint"),
    )
    .await
    .expect("write checkpoint");
    let (_stop_tx, mut abort) = stop_channel();

    assert!(
        write_ordered_track(&resources, &directory, "track.mp4", &mut abort)
            .await
            .is_err()
    );
    tokio::fs::remove_dir_all(directory)
        .await
        .expect("remove staging");
}

#[tokio::test]
async fn rejects_checkpoint_when_committed_offset_disagrees_with_resources() {
    let directory = test_directory();
    tokio::fs::create_dir_all(&directory)
        .await
        .expect("create staging");
    tokio::fs::write(directory.join("first.bin"), b"first")
        .await
        .expect("write first");
    let resources = vec!["first.bin".to_string(), "second.bin".to_string()];
    let checkpoint = WriterCheckpoint {
        version: CHECKPOINT_VERSION,
        plan_fingerprint: plan_fingerprint(&resources).expect("fingerprint"),
        next_resource_index: 1,
        committed_offset: 4,
    };
    tokio::fs::write(directory.join("track.mp4.part"), b"first")
        .await
        .expect("write part");
    tokio::fs::write(
        directory.join("track.mp4.checkpoint.json"),
        serde_json::to_vec(&checkpoint).expect("serialize checkpoint"),
    )
    .await
    .expect("write checkpoint");
    write_resource_journal(
        &directory.join("track.mp4.resources"),
        &directory,
        &[resource_checkpoint(b"first")],
    )
    .await
    .expect("write mismatched digest journal");
    let (_stop_tx, mut abort) = stop_channel();

    assert!(
        write_ordered_track(&resources, &directory, "track.mp4", &mut abort)
            .await
            .is_err()
    );
    tokio::fs::remove_dir_all(directory)
        .await
        .expect("remove staging");
}

#[tokio::test]
async fn recovers_crash_after_track_rename_before_checkpoint_removal() {
    let directory = test_directory();
    tokio::fs::create_dir_all(&directory)
        .await
        .expect("create staging");
    let resources = vec!["first.bin".to_string()];
    tokio::fs::write(directory.join("first.bin"), b"complete")
        .await
        .expect("write resource");
    let checkpoint = WriterCheckpoint {
        version: CHECKPOINT_VERSION,
        plan_fingerprint: plan_fingerprint(&resources).expect("fingerprint"),
        next_resource_index: 1,
        committed_offset: 8,
    };
    tokio::fs::write(directory.join("track.mp4"), b"complete")
        .await
        .expect("write published track");
    tokio::fs::write(
        directory.join("track.mp4.checkpoint.json"),
        serde_json::to_vec(&checkpoint).expect("serialize checkpoint"),
    )
    .await
    .expect("write checkpoint");
    write_resource_journal(
        &directory.join("track.mp4.resources"),
        &directory,
        &[resource_checkpoint(b"complete")],
    )
    .await
    .expect("write committed digest journal");
    let (_stop_tx, mut abort) = stop_channel();

    assert!(
        write_ordered_track(&resources, &directory, "track.mp4", &mut abort)
            .await
            .expect("recover completed track")
    );
    assert!(!directory.join("track.mp4.checkpoint.json").exists());
    tokio::fs::remove_dir_all(directory)
        .await
        .expect("remove staging");
}

#[tokio::test]
async fn migrates_legacy_checkpoint_before_resuming() {
    let directory = test_directory();
    tokio::fs::create_dir_all(&directory).await.expect("create staging");
    tokio::fs::write(directory.join("first.bin"), b"first").await.expect("write first");
    tokio::fs::write(directory.join("second.bin"), b"second").await.expect("write second");
    let resources = vec!["first.bin".to_string(), "second.bin".to_string()];
    let checkpoint = WriterCheckpoint {
        version: LEGACY_CHECKPOINT_VERSION,
        plan_fingerprint: plan_fingerprint(&resources).expect("fingerprint"),
        next_resource_index: 1,
        committed_offset: 5,
    };
    tokio::fs::write(directory.join("track.mp4.part"), b"first").await.expect("write part");
    tokio::fs::write(directory.join("track.mp4.checkpoint.json"), serde_json::to_vec(&checkpoint).expect("serialize checkpoint"))
        .await.expect("write legacy checkpoint");
    let (_stop_tx, mut abort) = stop_channel();

    let (Some(mut writer), committed) = OrderedTrackWriter::open(resources.clone(), &directory, "track.mp4")
        .await.expect("migrate and resume legacy checkpoint") else {
        panic!("an interrupted track must return an active writer");
    };
    assert_eq!(committed, 5);
    let migrated: WriterCheckpoint = serde_json::from_slice(
        &tokio::fs::read(directory.join("track.mp4.checkpoint.json")).await.expect("read migrated checkpoint"),
    ).expect("decode migrated checkpoint");
    assert_eq!(migrated.version, CHECKPOINT_VERSION);
    assert_eq!(
        read_resource_journal(
            &directory.join("track.mp4.resources"),
            &directory,
            1,
            false,
        )
        .await
        .expect("read migrated digest journal"),
        vec![resource_checkpoint(b"first")]
    );
    assert!(writer.append_next(&mut abort).await.expect("append second"));
    writer.finish().await.expect("publish migrated track");
    assert_eq!(tokio::fs::read(directory.join("track.mp4")).await.expect("read track"), b"firstsecond");
    tokio::fs::remove_dir_all(directory).await.expect("remove staging");
}

#[tokio::test]
async fn empty_resource_plan_has_no_committed_track() {
    let directory = test_directory();
    tokio::fs::create_dir_all(&directory)
        .await
        .expect("create staging");
    assert!(
        committed_resources(&[], &directory, "track.mp4")
            .await
            .expect("inspect empty plan")
            .is_none()
    );
    tokio::fs::remove_dir_all(directory)
        .await
        .expect("remove staging");
}

#[tokio::test]
async fn rebuilds_a_missing_digest_journal_from_retained_sources() {
    let directory = test_directory();
    tokio::fs::create_dir_all(&directory)
        .await
        .expect("create staging");
    let resources = vec!["first.bin".to_string()];
    tokio::fs::write(directory.join("first.bin"), b"first")
        .await
        .expect("write staged source");
    tokio::fs::write(directory.join("track.mp4"), b"first")
        .await
        .expect("write published track");
    let checkpoint = WriterCheckpoint {
        version: CHECKPOINT_VERSION,
        plan_fingerprint: plan_fingerprint(&resources).expect("fingerprint"),
        next_resource_index: 1,
        committed_offset: 5,
    };
    tokio::fs::write(
        directory.join("track.mp4.checkpoint.json"),
        serde_json::to_vec(&checkpoint).expect("serialize checkpoint"),
    )
    .await
    .expect("write checkpoint");

    let committed = committed_resources(&resources, &directory, "track.mp4")
        .await
        .expect("rebuild missing digest journal")
        .expect("published track is complete");
    assert_eq!(committed, vec![resource_checkpoint(b"first")]);
    assert!(directory.join("track.mp4.resources").is_file());

    tokio::fs::remove_dir_all(directory)
        .await
        .expect("remove staging");
}

#[tokio::test]
async fn retained_checkpoint_allows_idempotent_source_cleanup_and_recovery() {
    let directory = test_directory();
    tokio::fs::create_dir_all(&directory).await.expect("create staging");
    tokio::fs::write(directory.join("first.bin"), b"first").await.expect("write first");
    tokio::fs::write(directory.join("second.bin"), b"second").await.expect("write second");
    let resources = vec!["first.bin".to_string(), "second.bin".to_string()];
    let (_stop_tx, mut abort) = stop_channel();
    let (Some(mut writer), _) = OrderedTrackWriter::open_retaining_checkpoint(resources.clone(), &directory, "track.mp4")
        .await.expect("open retained writer") else {
        panic!("a fresh track must return an active writer");
    };
    while writer.next_resource_index() < resources.len() {
        assert!(writer.append_next(&mut abort).await.expect("append resource"));
        let committed_index = writer.next_resource_index() - 1;
        assert!(
            !directory.join(&resources[committed_index]).exists(),
            "a source is removed immediately after its checkpoint commits"
        );
    }
    writer.finish().await.expect("publish retained track");
    assert!(directory.join("track.mp4.checkpoint.json").exists());
    tokio::fs::write(directory.join("second.bin"), b"second")
        .await
        .expect("simulate a source left behind by interrupted cleanup");

    cleanup_committed_resources(&resources, &directory, "track.mp4").await.expect("clean committed sources");
    assert!(!directory.join("first.bin").exists());
    assert!(!directory.join("second.bin").exists());
    let committed = committed_resources(&resources, &directory, "track.mp4").await
        .expect("verify retained checkpoint").expect("complete checkpoint exists");
    assert_eq!(
        committed.iter().map(|resource| resource.length).collect::<Vec<_>>(),
        vec![5, 6]
    );

    let (writer, committed_bytes) = OrderedTrackWriter::open_retaining_checkpoint(resources.clone(), &directory, "track.mp4")
        .await.expect("recover published track");
    assert!(writer.is_none());
    assert_eq!(committed_bytes, 11);
    cleanup_committed_resources(&resources, &directory, "track.mp4").await.expect("repeat cleanup");
    tokio::fs::remove_dir_all(directory).await.expect("remove staging");
}

#[tokio::test]
async fn rejects_corruption_in_a_published_track_before_source_cleanup() {
    let directory = test_directory();
    tokio::fs::create_dir_all(&directory).await.expect("create staging");
    tokio::fs::write(directory.join("first.bin"), b"first").await.expect("write first");
    let resources = vec!["first.bin".to_string()];
    let (_stop_tx, mut abort) = stop_channel();
    let (Some(mut writer), _) = OrderedTrackWriter::open_retaining_checkpoint(resources.clone(), &directory, "track.mp4")
        .await.expect("open retained writer") else {
        panic!("a fresh track must return an active writer");
    };
    assert!(writer.append_next(&mut abort).await.expect("append resource"));
    writer.finish().await.expect("publish retained track");
    tokio::fs::write(directory.join("first.bin"), b"first")
        .await
        .expect("simulate a leftover staged source");
    tokio::fs::write(directory.join("track.mp4"), b"wrong").await.expect("corrupt track");
    assert!(cleanup_committed_resources(&resources, &directory, "track.mp4").await.is_err());
    assert!(directory.join("first.bin").exists());
    tokio::fs::remove_dir_all(directory).await.expect("remove staging");
}

#[test]
fn rejects_unsafe_and_duplicate_resource_names() {
    assert!(!safe_filename("../outside.bin"));
    assert!(!safe_filename("nested/file.bin"));
    assert!(safe_filename("segment.bin"));
    let names = ["segment.bin".to_string(), "segment.bin".to_string()];
    let mut unique = std::collections::HashSet::new();
    assert!(names.iter().any(|name| !unique.insert(name)));
}
