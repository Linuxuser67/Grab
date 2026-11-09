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
async fn appendable_destination_writer_resumes_after_the_resource_plan_grows() {
    let root = test_directory();
    let staging = root.join("staging");
    let track_directory = staging.join("video");
    let destination = root.join("destination");
    tokio::fs::create_dir_all(&track_directory)
        .await
        .expect("create track staging");
    tokio::fs::create_dir_all(&destination)
        .await
        .expect("create destination");
    let output = destination.join("recording.mp4");
    crate::manifest_destination::ensure_plan(&staging, &output, 77, false, false)
        .expect("create destination plan");
    tokio::fs::write(track_directory.join("first.bin"), b"first")
        .await
        .expect("write first resource");
    let first = vec!["first.bin".to_string()];
    let (_stop_tx, mut abort) = stop_channel();

    let (Some(mut writer), _) = OrderedTrackWriter::open_appendable_destination_part(
        first,
        &track_directory,
        &staging,
        "video-track.mp4",
        &output,
        77,
        crate::manifest_destination::TrackKind::Video,
    )
    .await
    .expect("open initial appendable destination part") else {
        panic!("a fresh destination part must return an active writer");
    };
    assert!(writer.append_next(&mut abort).await.expect("append first"));
    writer.finish().await.expect("flush first prefix");
    let part = crate::manifest_destination::part_path(
        &staging,
        &output,
        77,
        crate::manifest_destination::TrackKind::Video,
    )
    .expect("resolve initial destination part");
    crate::manifest_destination::verify_owned_part(
        &staging,
        &output,
        77,
        crate::manifest_destination::TrackKind::Video,
    )
    .expect("verify destination part ownership");
    assert_eq!(
        crate::manifest_writer::committed_resource_prefix_at(
            &["first.bin".to_string()],
            &track_directory,
            "video-track.mp4",
            &part,
        )
        .await
        .expect("verify append-only committed prefix"),
        vec![resource_checkpoint(b"first")]
    );

    tokio::fs::write(track_directory.join("second.bin"), b"second")
        .await
        .expect("write second resource");
    let expanded = vec!["first.bin".to_string(), "second.bin".to_string()];
    let (Some(mut writer), committed) = OrderedTrackWriter::open_appendable_destination_part(
        expanded.clone(),
        &track_directory,
        &staging,
        "video-track.mp4",
        &output,
        77,
        crate::manifest_destination::TrackKind::Video,
    )
    .await
    .expect("resume with expanded resource plan") else {
        panic!("an appendable destination part must remain open");
    };
    assert_eq!(committed, 5);
    assert_eq!(writer.next_resource_index(), 1);
    assert!(writer.append_next(&mut abort).await.expect("append second"));
    writer.finish().await.expect("flush expanded prefix");

    let part = crate::manifest_destination::part_path(
        &staging,
        &output,
        77,
        crate::manifest_destination::TrackKind::Video,
    )
    .expect("resolve destination part");
    assert_eq!(
        tokio::fs::read(&part).await.expect("read destination part"),
        b"firstsecond"
    );
    let checkpoint: WriterCheckpoint = serde_json::from_slice(
        &tokio::fs::read(track_directory.join("video-track.mp4.checkpoint.json"))
            .await
            .expect("read append-only checkpoint"),
    )
    .expect("decode append-only checkpoint");
    assert!(checkpoint.append_only);
    assert_eq!(
        checkpoint.plan_fingerprint,
        plan_fingerprint(&expanded).expect("fingerprint expanded plan")
    );
    tokio::fs::remove_dir_all(root)
        .await
        .expect("remove test directory");
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
        read_resource_journal(&directory.join("track.mp4.resources"), &directory, 1, false,)
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
        append_only: false,
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
        &[
            resource_checkpoint(b"first"),
            resource_checkpoint(b"second"),
        ],
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
        read_resource_journal(&directory.join("track.mp4.resources"), &directory, 1, false,)
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
        plan_fingerprint: plan_fingerprint(&["second.bin".to_string(), "first.bin".to_string()])
            .expect("fingerprint"),
        next_resource_index: 1,
        committed_offset: 5,
        append_only: false,
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
        append_only: false,
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
        append_only: false,
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
    let checkpoint = WriterCheckpoint {
        version: LEGACY_CHECKPOINT_VERSION,
        plan_fingerprint: plan_fingerprint(&resources).expect("fingerprint"),
        next_resource_index: 1,
        committed_offset: 5,
        append_only: false,
    };
    tokio::fs::write(directory.join("track.mp4.part"), b"first")
        .await
        .expect("write part");
    tokio::fs::write(
        directory.join("track.mp4.checkpoint.json"),
        serde_json::to_vec(&checkpoint).expect("serialize checkpoint"),
    )
    .await
    .expect("write legacy checkpoint");
    let (_stop_tx, mut abort) = stop_channel();

    let (Some(mut writer), committed) =
        OrderedTrackWriter::open(resources.clone(), &directory, "track.mp4")
            .await
            .expect("migrate and resume legacy checkpoint")
    else {
        panic!("an interrupted track must return an active writer");
    };
    assert_eq!(committed, 5);
    let migrated: WriterCheckpoint = serde_json::from_slice(
        &tokio::fs::read(directory.join("track.mp4.checkpoint.json"))
            .await
            .expect("read migrated checkpoint"),
    )
    .expect("decode migrated checkpoint");
    assert_eq!(migrated.version, CHECKPOINT_VERSION);
    assert_eq!(
        read_resource_journal(&directory.join("track.mp4.resources"), &directory, 1, false,)
            .await
            .expect("read migrated digest journal"),
        vec![resource_checkpoint(b"first")]
    );
    assert!(writer.append_next(&mut abort).await.expect("append second"));
    writer.finish().await.expect("publish migrated track");
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
        append_only: false,
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
    let (Some(mut writer), _) =
        OrderedTrackWriter::open_retaining_checkpoint(resources.clone(), &directory, "track.mp4")
            .await
            .expect("open retained writer")
    else {
        panic!("a fresh track must return an active writer");
    };
    while writer.next_resource_index() < resources.len() {
        assert!(
            writer
                .append_next(&mut abort)
                .await
                .expect("append resource")
        );
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

    cleanup_committed_resources(&resources, &directory, "track.mp4")
        .await
        .expect("clean committed sources");
    assert!(!directory.join("first.bin").exists());
    assert!(!directory.join("second.bin").exists());
    let committed = committed_resources(&resources, &directory, "track.mp4")
        .await
        .expect("verify retained checkpoint")
        .expect("complete checkpoint exists");
    assert_eq!(
        committed
            .iter()
            .map(|resource| resource.length)
            .collect::<Vec<_>>(),
        vec![5, 6]
    );

    let (writer, committed_bytes) =
        OrderedTrackWriter::open_retaining_checkpoint(resources.clone(), &directory, "track.mp4")
            .await
            .expect("recover published track");
    assert!(writer.is_none());
    assert_eq!(committed_bytes, 11);
    cleanup_committed_resources(&resources, &directory, "track.mp4")
        .await
        .expect("repeat cleanup");
    tokio::fs::remove_dir_all(directory)
        .await
        .expect("remove staging");
}

#[tokio::test]
async fn rejects_corruption_in_a_published_track_before_source_cleanup() {
    let directory = test_directory();
    tokio::fs::create_dir_all(&directory)
        .await
        .expect("create staging");
    tokio::fs::write(directory.join("first.bin"), b"first")
        .await
        .expect("write first");
    let resources = vec!["first.bin".to_string()];
    let (_stop_tx, mut abort) = stop_channel();
    let (Some(mut writer), _) =
        OrderedTrackWriter::open_retaining_checkpoint(resources.clone(), &directory, "track.mp4")
            .await
            .expect("open retained writer")
    else {
        panic!("a fresh track must return an active writer");
    };
    assert!(
        writer
            .append_next(&mut abort)
            .await
            .expect("append resource")
    );
    writer.finish().await.expect("publish retained track");
    tokio::fs::write(directory.join("first.bin"), b"first")
        .await
        .expect("simulate a leftover staged source");
    tokio::fs::write(directory.join("track.mp4"), b"wrong")
        .await
        .expect("corrupt track");
    assert!(
        cleanup_committed_resources(&resources, &directory, "track.mp4")
            .await
            .is_err()
    );
    assert!(directory.join("first.bin").exists());
    tokio::fs::remove_dir_all(directory)
        .await
        .expect("remove staging");
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

#[tokio::test]
async fn destination_part_stays_visible_until_final_container_cleanup() {
    let directory = test_directory();
    let destination_dir = tempfile::tempdir().expect("create destination");
    tokio::fs::create_dir_all(&directory)
        .await
        .expect("create staging");
    let destination = destination_dir.path().join("Example.mp4");
    crate::manifest_destination::ensure_plan(&directory, &destination, 42, false, false)
        .expect("persist destination plan");
    tokio::fs::write(directory.join("first.bin"), b"first")
        .await
        .expect("write source");
    let resources = vec!["first.bin".to_string()];
    let (_stop_tx, mut abort) = stop_channel();

    let (Some(mut writer), committed) = OrderedTrackWriter::open_destination_part(
        resources,
        &directory,
        &directory,
        "video-track.mp4",
        &destination,
        42,
        crate::manifest_destination::TrackKind::Video,
    )
    .await
    .expect("open destination writer") else {
        panic!("a fresh destination track must return an active writer");
    };
    assert_eq!(committed, 0);
    assert!(
        writer
            .append_next(&mut abort)
            .await
            .expect("append resource")
    );
    writer.finish().await.expect("finish destination track");

    let part = crate::manifest_destination::part_path(
        &directory,
        &destination,
        42,
        crate::manifest_destination::TrackKind::Video,
    )
    .expect("resolve destination part");
    assert_eq!(
        tokio::fs::read(&part).await.expect("read visible part"),
        b"first"
    );
    assert!(directory.join("video-track.mp4.checkpoint.json").is_file());
    assert!(!directory.join("video-track.mp4").exists());

    cleanup_destination_part(
        &directory,
        &directory,
        &destination,
        42,
        crate::manifest_destination::TrackKind::Video,
        "video-track.mp4",
    )
    .await
    .expect("clean destination part after final publication");
    assert!(!part.exists());
    assert!(!directory.join("video-track.mp4.checkpoint.json").exists());
    tokio::fs::remove_dir_all(directory)
        .await
        .expect("remove staging");
}

#[tokio::test]
async fn migrates_a_completed_staging_track_to_the_owned_destination_part() {
    let directory = test_directory();
    let destination_dir = tempfile::tempdir().expect("create destination");
    tokio::fs::create_dir_all(&directory)
        .await
        .expect("create staging");
    let destination = destination_dir.path().join("Example.mp4");
    crate::manifest_destination::ensure_plan(&directory, &destination, 43, false, false)
        .expect("persist destination plan");
    let resources = vec!["first.bin".to_string()];
    tokio::fs::write(directory.join("first.bin"), b"complete")
        .await
        .expect("write source");
    tokio::fs::write(directory.join("video-track.mp4"), b"complete")
        .await
        .expect("write previously completed track");
    let checkpoint = WriterCheckpoint {
        version: CHECKPOINT_VERSION,
        plan_fingerprint: plan_fingerprint(&resources).expect("fingerprint"),
        next_resource_index: 1,
        committed_offset: 8,
        append_only: false,
    };
    tokio::fs::write(
        directory.join("video-track.mp4.checkpoint.json"),
        serde_json::to_vec(&checkpoint).expect("serialize checkpoint"),
    )
    .await
    .expect("write checkpoint");
    write_resource_journal(
        &directory.join("video-track.mp4.resources"),
        &directory,
        &[resource_checkpoint(b"complete")],
    )
    .await
    .expect("write resource journal");

    let (writer, committed) = OrderedTrackWriter::open_appendable_destination_part(
        resources,
        &directory,
        &directory,
        "video-track.mp4",
        &destination,
        43,
        crate::manifest_destination::TrackKind::Video,
    )
    .await
    .expect("migrate completed track");
    let writer = writer.expect("migrated track remains available for finalization");
    assert_eq!(writer.next_resource_index(), 1);
    assert_eq!(committed, 8);
    writer.finish().await.expect("finish migrated track");
    let part = crate::manifest_destination::part_path(
        &directory,
        &destination,
        43,
        crate::manifest_destination::TrackKind::Video,
    )
    .expect("resolve destination part");
    assert_eq!(
        tokio::fs::read(&part).await.expect("read migrated part"),
        b"complete"
    );
    assert!(!directory.join("video-track.mp4").exists());

    cleanup_destination_part(
        &directory,
        &directory,
        &destination,
        43,
        crate::manifest_destination::TrackKind::Video,
        "video-track.mp4",
    )
    .await
    .expect("clean migrated destination part");
    tokio::fs::remove_dir_all(directory)
        .await
        .expect("remove staging");
}

#[tokio::test]
async fn resumes_a_crash_during_staging_to_destination_part_migration() {
    let directory = test_directory();
    let destination_dir = tempfile::tempdir().expect("create destination");
    tokio::fs::create_dir_all(&directory)
        .await
        .expect("create staging");
    let destination = destination_dir.path().join("Example.mp4");
    crate::manifest_destination::ensure_plan(&directory, &destination, 44, false, false)
        .expect("persist destination plan");
    let resources = vec!["first.bin".to_string()];
    tokio::fs::write(directory.join("video-track.mp4"), b"complete")
        .await
        .expect("write completed staging track");
    let checkpoint = WriterCheckpoint {
        version: CHECKPOINT_VERSION,
        plan_fingerprint: plan_fingerprint(&resources).expect("fingerprint"),
        next_resource_index: 1,
        committed_offset: 8,
        append_only: false,
    };
    tokio::fs::write(
        directory.join("video-track.mp4.checkpoint.json"),
        serde_json::to_vec(&checkpoint).expect("serialize checkpoint"),
    )
    .await
    .expect("write checkpoint");
    write_resource_journal(
        &directory.join("video-track.mp4.resources"),
        &directory,
        &[resource_checkpoint(b"complete")],
    )
    .await
    .expect("write resource journal");
    let mut interrupted = crate::manifest_destination::open_owned_part(
        &directory,
        &destination,
        44,
        crate::manifest_destination::TrackKind::Video,
    )
    .expect("claim destination part");
    std::io::Write::write_all(&mut interrupted, b"par")
        .expect("write interrupted migration prefix");
    interrupted.sync_all().expect("sync interrupted prefix");
    drop(interrupted);

    let part = crate::manifest_destination::part_path(
        &directory,
        &destination,
        44,
        crate::manifest_destination::TrackKind::Video,
    )
    .expect("resolve interrupted destination part");
    assert_eq!(
        crate::manifest_writer::committed_resource_prefix_at(
            &resources,
            &directory,
            "video-track.mp4",
            &part,
        )
        .await
        .expect("verify completed staging track during migration"),
        vec![resource_checkpoint(b"complete")]
    );

    let (writer, committed) = OrderedTrackWriter::open_appendable_destination_part(
        resources,
        &directory,
        &directory,
        "video-track.mp4",
        &destination,
        44,
        crate::manifest_destination::TrackKind::Video,
    )
    .await
    .expect("recover interrupted migration");
    let writer = writer.expect("recovered track must be finalized");
    assert_eq!(writer.next_resource_index(), 1);
    assert_eq!(committed, 8);
    writer.finish().await.expect("finish recovered migration");

    let part = crate::manifest_destination::part_path(
        &directory,
        &destination,
        44,
        crate::manifest_destination::TrackKind::Video,
    )
    .expect("resolve destination part");
    assert_eq!(
        tokio::fs::read(&part).await.expect("read recovered part"),
        b"complete"
    );
    assert!(!directory.join("video-track.mp4").exists());
    cleanup_destination_part(
        &directory,
        &directory,
        &destination,
        44,
        crate::manifest_destination::TrackKind::Video,
        "video-track.mp4",
    )
    .await
    .expect("clean recovered destination part");
    tokio::fs::remove_dir_all(directory)
        .await
        .expect("remove staging");
}

#[tokio::test]
async fn streamed_destination_writer_resumes_without_staged_source_files() {
    let root = test_directory();
    let staging = root.join("staging");
    let track_directory = staging.join("video");
    let destination_dir = root.join("destination");
    tokio::fs::create_dir_all(&track_directory)
        .await
        .expect("create HLS track recovery directory");
    tokio::fs::create_dir_all(&destination_dir)
        .await
        .expect("create HLS destination directory");
    let destination = destination_dir.join("recording.mp4");
    crate::manifest_destination::ensure_plan(&staging, &destination, 78, false, false)
        .expect("create HLS destination plan");
    let resources = vec!["segment-1.bin".to_string(), "segment-2.bin".to_string()];
    let track = crate::manifest_destination::TrackKind::Video;

    let (Some(mut writer), committed) = OrderedTrackWriter::open_destination_part(
        resources.clone(),
        &track_directory,
        &staging,
        "video-track.ts",
        &destination,
        78,
        track,
    )
    .await
    .expect("open destination writer") else {
        panic!("a fresh destination part must return an active writer");
    };
    assert_eq!(committed, 0);
    writer
        .write_streamed_chunk(b"first")
        .await
        .expect("write first network chunk");
    writer
        .commit_streamed_resource()
        .await
        .expect("checkpoint first network resource");
    writer
        .write_streamed_chunk(b"discarded partial fragment")
        .await
        .expect("write partial fragment before rollback");
    writer
        .rollback_streamed_resource()
        .await
        .expect("discard uncommitted fragment bytes");
    assert_eq!(writer.committed_offset(), 5);
    drop(writer);

    let part = crate::manifest_destination::part_path(&staging, &destination, 78, track)
        .expect("resolve owned destination part");
    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .open(&part)
        .await
        .expect("open part to simulate an interrupted write");
    tokio::io::AsyncWriteExt::write_all(&mut file, b"uncommitted")
        .await
        .expect("write uncommitted crash tail");
    file.sync_all().await.expect("sync crash tail");
    drop(file);

    let (Some(mut writer), committed) = OrderedTrackWriter::open_destination_part(
        resources,
        &track_directory,
        &staging,
        "video-track.ts",
        &destination,
        78,
        track,
    )
    .await
    .expect("resume destination writer without staged source files") else {
        panic!("a committed destination prefix must remain resumable");
    };
    assert_eq!(committed, 5);
    assert_eq!(writer.next_resource_index(), 1);
    writer
        .write_streamed_chunk(b"sec")
        .await
        .expect("write second resource prefix");
    writer
        .write_streamed_chunk(b"ond")
        .await
        .expect("write second resource suffix");
    writer
        .commit_streamed_resource()
        .await
        .expect("checkpoint second network resource");
    writer
        .finish()
        .await
        .expect("flush streamed destination part");

    assert_eq!(
        tokio::fs::read(&part)
            .await
            .expect("read streamed destination part"),
        b"firstsecond"
    );
    assert!(!track_directory.join("segment-1.bin").exists());
    assert!(!track_directory.join("segment-2.bin").exists());
    tokio::fs::remove_dir_all(root)
        .await
        .expect("remove HLS writer test directory");
}

#[tokio::test]
async fn appendable_destination_writer_persists_empty_checkpoint_before_pause() {
    let root = test_directory();
    let staging = root.join("staging");
    let track_directory = staging.join("video");
    let destination_dir = root.join("destination");
    tokio::fs::create_dir_all(&track_directory)
        .await
        .expect("create track staging");
    tokio::fs::create_dir_all(&destination_dir)
        .await
        .expect("create destination");
    let output = destination_dir.join("recording.mp4");
    crate::manifest_destination::ensure_plan(&staging, &output, 79, false, false)
        .expect("create destination plan");
    tokio::fs::write(track_directory.join("first.bin"), b"first")
        .await
        .expect("write first resource");
    let resources = vec!["first.bin".to_string()];
    let track = crate::manifest_destination::TrackKind::Video;
    let (stop_tx, mut abort) = stop_channel();

    let (Some(mut writer), committed) = OrderedTrackWriter::open_appendable_destination_part(
        resources.clone(),
        &track_directory,
        &staging,
        "video-track.mp4",
        &output,
        79,
        track,
    )
    .await
    .expect("open fresh appendable destination writer")
    else {
        panic!("a fresh destination part must return an active writer");
    };
    assert_eq!(committed, 0);
    let checkpoint_path = track_directory.join("video-track.mp4.checkpoint.json");
    let checkpoint: WriterCheckpoint = serde_json::from_slice(
        &tokio::fs::read(&checkpoint_path)
            .await
            .expect("read the initial empty checkpoint"),
    )
    .expect("decode initial empty checkpoint");
    assert_eq!(checkpoint.next_resource_index, 0);
    assert_eq!(checkpoint.committed_offset, 0);
    assert!(checkpoint.append_only);

    let part = crate::manifest_destination::part_path(&staging, &output, 79, track)
        .expect("resolve empty destination part");
    assert_eq!(
        tokio::fs::metadata(&part)
            .await
            .expect("inspect empty part")
            .len(),
        0
    );

    stop_tx
        .send(crate::video::StopIntent::Pause)
        .expect("queue pause before the first resource commit");
    assert!(matches!(
        writer
            .append_next_outcome(&mut abort)
            .await
            .expect("pause first append"),
        AppendNextOutcome::Stopped(crate::video::StopIntent::Pause)
    ));
    drop(writer);

    let (_finish_tx, mut finish_abort) = stop_channel();
    let (Some(mut resumed), committed) = OrderedTrackWriter::open_appendable_destination_part(
        resources,
        &track_directory,
        &staging,
        "video-track.mp4",
        &output,
        79,
        track,
    )
    .await
    .expect("resume after pause before the first resource commit")
    else {
        panic!("an empty but checkpointed destination part must remain resumable");
    };
    assert_eq!(committed, 0);
    assert_eq!(resumed.next_resource_index(), 0);
    assert!(
        resumed
            .append_next(&mut finish_abort)
            .await
            .expect("append first resource after resume")
    );
    resumed
        .finish()
        .await
        .expect("flush resumed destination part");
    assert_eq!(
        tokio::fs::read(&part).await.expect("read resumed part"),
        b"first"
    );

    tokio::fs::remove_dir_all(root)
        .await
        .expect("remove test directory");
}

#[tokio::test]
async fn missing_appendable_checkpoint_preserves_existing_destination_part() {
    let root = test_directory();
    let staging = root.join("staging");
    let track_directory = staging.join("video");
    let destination = root.join("destination");
    tokio::fs::create_dir_all(&track_directory)
        .await
        .expect("create track staging");
    tokio::fs::create_dir_all(&destination)
        .await
        .expect("create destination");
    let output = destination.join("recording.mp4");
    crate::manifest_destination::ensure_plan(&staging, &output, 91, false, false)
        .expect("create destination plan");
    tokio::fs::write(track_directory.join("first.bin"), b"committed")
        .await
        .expect("write source resource");
    let resources = vec!["first.bin".to_string()];
    let (_stop_tx, mut abort) = stop_channel();

    let (Some(mut writer), _) = OrderedTrackWriter::open_appendable_destination_part(
        resources.clone(),
        &track_directory,
        &staging,
        "video-track.mp4",
        &output,
        91,
        crate::manifest_destination::TrackKind::Video,
    )
    .await
    .expect("open appendable destination writer") else {
        panic!("fresh destination writer must be active");
    };
    assert!(
        writer
            .append_next(&mut abort)
            .await
            .expect("append resource")
    );
    writer
        .finish()
        .await
        .expect("flush committed destination prefix");

    let part = crate::manifest_destination::part_path(
        &staging,
        &output,
        91,
        crate::manifest_destination::TrackKind::Video,
    )
    .expect("resolve destination part");
    let before = tokio::fs::read(&part).await.expect("read committed part");
    tokio::fs::remove_file(track_directory.join("video-track.mp4.checkpoint.json"))
        .await
        .expect("simulate missing checkpoint");
    tokio::fs::remove_file(track_directory.join("video-track.mp4.resources"))
        .await
        .expect("simulate missing resource journal");

    let reopened = OrderedTrackWriter::open_appendable_destination_part(
        resources,
        &track_directory,
        &staging,
        "video-track.mp4",
        &output,
        91,
        crate::manifest_destination::TrackKind::Video,
    )
    .await;
    assert!(
        reopened.is_err(),
        "a missing checkpoint must not reopen an existing appendable destination part as fresh"
    );
    assert_eq!(
        tokio::fs::read(&part)
            .await
            .expect("re-read preserved part"),
        before,
        "committed destination bytes must remain unchanged"
    );

    tokio::fs::remove_dir_all(root)
        .await
        .expect("remove writer test directory");
}
