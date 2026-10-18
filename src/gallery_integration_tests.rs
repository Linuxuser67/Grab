//! Integration test for gallery-dl backend: runs real gallery-dl against a
//! local Python test server.
//!
//! Skipped when gallery-dl is not on PATH. Run in CI with a pinned
//! gallery-dl version.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use crate::gallery_argv::GalleryFilter;
use crate::gallery_probe::{GalleryItem, enumerate_gallery_files_streamed};

/// Find gallery-dl on PATH, or return None to skip the test.
/// If GRAB_REQUIRE_GALLERY_DL=1 and it's missing, panic (CI must have it).
fn find_gallery_dl() -> Option<PathBuf> {
    let found = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let p = dir.join("gallery-dl");
            if p.is_file() { Some(p) } else { None }
        })
    });
    if found.is_none() && std::env::var("GRAB_REQUIRE_GALLERY_DL").as_deref() == Ok("1") {
        panic!("GRAB_REQUIRE_GALLERY_DL=1 but gallery-dl not found on PATH");
    }
    found
}

/// Check if a gallery-dl error is an internal gallery-dl bug (not a Grab bug).
/// If so, return true to skip the test.
fn is_gallery_dl_internal_bug(err: &crate::gallery_tools::GalleryError) -> bool {
    let msg = format!("{err:?}");
    msg.contains("'NoneType' object has no attribute")
}

/// Collect the shipping streamed enumeration (and its document-mode
/// fallback) into a Vec for the assertions. An errored run still reports
/// its error after draining what was listed.
async fn enumerate_all(
    tool: &std::path::Path,
    url: &str,
    filter: GalleryFilter,
) -> Result<Vec<GalleryItem>, crate::gallery_tools::GalleryError> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let enumeration = enumerate_gallery_files_streamed(tool, url, None, None, filter, tx);
    let result = enumeration.await;
    let mut items = Vec::new();
    while let Ok(item) = rx.try_recv() {
        items.push(item);
    }
    result.map(|_| items)
}

/// Start the Python gallery test server on port 80.
/// Returns (guard, port). The guard kills the server on drop.
/// Returns None if port 80 can't be bound (need root).
/// Port 80 is required because gallery-dl's `generic:` URL prefix drops the
/// port number (upstream bug in GenericExtractor regex).
fn start_server() -> Option<(ServerGuard, u16)> {
    // gallery-dl's `generic:` URL prefix drops the port (upstream bug in the
    // GenericExtractor regex: domain group `[-\w\.]+` doesn't include `:port`).
    // Use port 80 so the URL works. If port 80 is unavailable (non-root or
    // busy), return None to skip the test.
    let port = 80;
    // Check if we can bind to port 80.
    let listener = match std::net::TcpListener::bind(format!("127.0.0.1:{port}")) {
        Ok(l) => l,
        Err(_) => return None,
    };
    drop(listener);

    let server_py = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("gallery_server.py");
    let child = std::process::Command::new("python3")
        .arg(server_py)
        .arg(port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to start gallery_server.py");
    // Give it a moment to bind.
    std::thread::sleep(Duration::from_millis(500));
    Some((ServerGuard(child), port))
}

/// Kills the test server on drop (also on panic).
struct ServerGuard(std::process::Child);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn gallery_integration_probe_count_download() {
    let Some(tool) = find_gallery_dl() else {
        eprintln!("SKIP: gallery-dl not on PATH");
        return;
    };
    let Some((_server, port)) = start_server() else {
        if std::env::var("GRAB_REQUIRE_GALLERY_DL").as_deref() == Ok("1") {
            panic!("GRAB_REQUIRE_GALLERY_DL=1 but cannot bind to port 80 for integration test");
        }
        eprintln!("SKIP: cannot bind to port 80 (need root for integration test)");
        return;
    };
    let url = format!("generic:http://127.0.0.1:{port}/");

    // 1. Probe returns items.
    let probe_result = crate::gallery_probe::probe_gallery(&tool, &url, None, None).await;
    let items = match probe_result {
        Ok(Some(items)) => items,
        Ok(None) => panic!("probe returned None"),
        Err(e) if is_gallery_dl_internal_bug(&e) => {
            // Upstream gallery-dl bug: the binary crashes internally with
            // AttributeError. Skip unless GRAB_REQUIRE_GALLERY_DL=1, in which
            // case fail with the full error.
            if std::env::var("GRAB_REQUIRE_GALLERY_DL").as_deref() == Ok("1") {
                panic!("gallery-dl internal bug (GRAB_REQUIRE_GALLERY_DL=1): {e:?}");
            }
            eprintln!("SKIP: gallery-dl has an internal bug in this environment: {e:?}");
            return;
        }
        Err(e) => panic!("probe failed: {e:?}"),
    };
    assert_eq!(
        items.len(),
        4,
        "expected 3 images + 1 video from test server"
    );
    assert!(
        items.iter().all(|i| !i.filename.is_empty()),
        "all items need filenames"
    );

    // 2. Count matches.
    let count = crate::gallery_probe::count_gallery_files(
        &tool,
        &url,
        None,
        None,
        crate::gallery_argv::GalleryFilter::All,
    )
    .await;
    assert_eq!(count, Some(4), "count should be 4");

    // 3. Download produces files.
    let dest = std::env::temp_dir().join(format!("grab-gallery-test-{port}"));
    std::fs::create_dir_all(&dest).unwrap();
    let job = crate::gallery_argv::GalleryJob {
        item_id: 1,
        page_url: url.clone(),
        title: "test-gallery".to_string(),
        file_count: Some(4),
        dest_dir: dest.clone(),
        filter: crate::gallery_argv::GalleryFilter::All,
        cookies_browser: "none".to_string(),
        speed_limit: None,
        range: None,
    };
    let completed = std::cell::Cell::new(0u32);
    let result =
        match crate::gallery_runner::run_gallery_download(&job, &tool, None, None, |update| {
            completed.set(update.files_completed);
        })
        .await
        {
            Ok(r) => r,
            Err(e) if is_gallery_dl_internal_bug(&e) => {
                if std::env::var("GRAB_REQUIRE_GALLERY_DL").as_deref() == Ok("1") {
                    panic!("gallery-dl internal bug (GRAB_REQUIRE_GALLERY_DL=1): {e:?}");
                }
                eprintln!("SKIP: gallery-dl has an internal bug in this environment: {e:?}");
                return;
            }
            Err(e) => panic!("download failed: {e:?}"),
        };
    assert_eq!(result.files.len(), 4, "expected 4 downloaded files");
    assert_eq!(completed.get(), 4, "progress should report 4 completed");
    for f in &result.files {
        assert!(f.exists(), "downloaded file should exist: {f:?}");
    }

    // 4. Second run reports all as skipped.
    let result2 =
        match crate::gallery_runner::run_gallery_download(&job, &tool, None, None, |_| {}).await {
            Ok(r) => r,
            Err(e) if is_gallery_dl_internal_bug(&e) => {
                if std::env::var("GRAB_REQUIRE_GALLERY_DL").as_deref() == Ok("1") {
                    panic!("gallery-dl internal bug (GRAB_REQUIRE_GALLERY_DL=1): {e:?}");
                }
                eprintln!("SKIP: gallery-dl has an internal bug in this environment: {e:?}");
                return;
            }
            Err(e) => panic!("second download failed: {e:?}"),
        };
    assert_eq!(result2.files.len(), 0, "no new files on second run");
    assert_eq!(result2.skipped, 4, "all 4 files skipped on second run");

    // Cleanup.
    std::fs::remove_dir_all(&dest).ok();
}

#[tokio::test]
async fn gallery_integration_filtered_download() {
    let Some(tool) = find_gallery_dl() else {
        eprintln!("SKIP: gallery-dl not on PATH");
        return;
    };
    let Some((_server, port)) = start_server() else {
        if std::env::var("GRAB_REQUIRE_GALLERY_DL").as_deref() == Ok("1") {
            panic!("GRAB_REQUIRE_GALLERY_DL=1 but cannot bind to port 80 for integration test");
        }
        eprintln!("SKIP: cannot bind to port 80 (need root for integration test)");
        return;
    };
    let url = format!("generic:http://127.0.0.1:{port}/");

    // Filtered run: images only. The server also serves a video ahead of
    // the images; the filter must exclude it (3 of 4 items).
    let dest = std::env::temp_dir().join(format!("grab-gallery-filter-{port}"));
    std::fs::create_dir_all(&dest).unwrap();
    let job = crate::gallery_argv::GalleryJob {
        item_id: 2,
        page_url: url.clone(),
        title: "test-gallery-filter".to_string(),
        file_count: Some(3),
        dest_dir: dest.clone(),
        filter: crate::gallery_argv::GalleryFilter::ImagesOnly,
        cookies_browser: "none".to_string(),
        speed_limit: None,
        range: None,
    };
    let result =
        match crate::gallery_runner::run_gallery_download(&job, &tool, None, None, |_| {}).await {
            Ok(r) => r,
            Err(e) if is_gallery_dl_internal_bug(&e) => {
                if std::env::var("GRAB_REQUIRE_GALLERY_DL").as_deref() == Ok("1") {
                    panic!("gallery-dl internal bug (GRAB_REQUIRE_GALLERY_DL=1): {e:?}");
                }
                eprintln!("SKIP: gallery-dl has an internal bug in this environment: {e:?}");
                return;
            }
            Err(e) => panic!("filtered download failed: {e:?}"),
        };
    assert_eq!(
        result.files.len(),
        3,
        "all 3 test images pass the ImagesOnly filter"
    );

    std::fs::remove_dir_all(&dest).ok();
}

#[tokio::test]
async fn gallery_integration_range_indexes_filtered_files() {
    let Some(tool) = find_gallery_dl() else {
        eprintln!("SKIP: gallery-dl not on PATH");
        return;
    };
    let Some((_server, port)) = start_server() else {
        if std::env::var("GRAB_REQUIRE_GALLERY_DL").as_deref() == Ok("1") {
            panic!("GRAB_REQUIRE_GALLERY_DL=1 but cannot bind to port 80 for integration test");
        }
        eprintln!("SKIP: cannot bind to port 80 (need root for integration test)");
        return;
    };
    let url = format!("generic:http://127.0.0.1:{port}/");

    // The picker enumerates with the download's filter and maps picks to
    // `--range` positions; gallery-dl applies `--range` to the filtered
    // list, so position 1 is the first IMAGE even though the server serves
    // a video first. Guard the fixture: if generic stops yielding the
    // video, the assertions below would pass vacuously.
    let all = match enumerate_all(&tool, &url, GalleryFilter::All).await {
        Ok(items) => items,
        Err(e) if is_gallery_dl_internal_bug(&e) => {
            if std::env::var("GRAB_REQUIRE_GALLERY_DL").as_deref() == Ok("1") {
                panic!("gallery-dl internal bug (GRAB_REQUIRE_GALLERY_DL=1): {e:?}");
            }
            eprintln!("SKIP: gallery-dl has an internal bug in this environment: {e:?}");
            return;
        }
        Err(e) => panic!("unfiltered enumerate failed: {e:?}"),
    };
    assert_eq!(all.len(), 4, "fixture: 3 images + 1 video");
    assert_eq!(all[0].extension, "mp4", "fixture: the video comes first");

    let filter = GalleryFilter::ImagesOnly;
    let filtered = match enumerate_all(&tool, &url, filter).await {
        Ok(items) => items,
        Err(e) if is_gallery_dl_internal_bug(&e) => {
            if std::env::var("GRAB_REQUIRE_GALLERY_DL").as_deref() == Ok("1") {
                panic!("gallery-dl internal bug (GRAB_REQUIRE_GALLERY_DL=1): {e:?}");
            }
            eprintln!("SKIP: gallery-dl has an internal bug in this environment: {e:?}");
            return;
        }
        Err(e) => panic!("filtered enumerate failed: {e:?}"),
    };
    assert_eq!(filtered.len(), 3, "the video must not survive the filter");
    assert_eq!(
        filtered[0].extension, "jpg",
        "first filtered file is an image"
    );

    // Pick the first listed file, as the picker would.
    let range = crate::gallery_argv::range_spec(&[0]).expect("range for one pick");
    assert_eq!(range, "1");

    let dest = std::env::temp_dir().join(format!("grab-gallery-range-{port}"));
    // A dest left over from a failed run would make gallery-dl skip files
    // and fail the count below for the wrong reason.
    std::fs::remove_dir_all(&dest).ok();
    std::fs::create_dir_all(&dest).unwrap();
    let job = crate::gallery_argv::GalleryJob {
        item_id: 3,
        page_url: url.clone(),
        title: "test-gallery-range".to_string(),
        file_count: Some(1),
        dest_dir: dest.clone(),
        filter,
        cookies_browser: "none".to_string(),
        speed_limit: None,
        range: Some(range),
    };
    let result =
        match crate::gallery_runner::run_gallery_download(&job, &tool, None, None, |_| {}).await {
            Ok(r) => r,
            Err(e) if is_gallery_dl_internal_bug(&e) => {
                if std::env::var("GRAB_REQUIRE_GALLERY_DL").as_deref() == Ok("1") {
                    panic!("gallery-dl internal bug (GRAB_REQUIRE_GALLERY_DL=1): {e:?}");
                }
                eprintln!("SKIP: gallery-dl has an internal bug in this environment: {e:?}");
                return;
            }
            Err(e) => panic!("range download failed: {e:?}"),
        };
    assert_eq!(result.files.len(), 1, "range 1 selects exactly one file");
    let name = result.files[0]
        .file_name()
        .map(|n| n.to_string_lossy().into_owned());
    assert_eq!(
        name.as_deref(),
        Some("img1.jpg"),
        "--range must index the filtered list, not the raw one"
    );

    std::fs::remove_dir_all(&dest).ok();
}

/// Recursive byte total of `path` (flat small test trees only).
fn dir_bytes(dir: &std::path::Path) -> u64 {
    fn walk(entry_path: &std::path::Path, total: &mut u64) {
        if let Ok(meta) = std::fs::metadata(entry_path)
            && meta.is_file()
        {
            *total += meta.len();
        }
        if entry_path.is_dir()
            && let Ok(entries) = std::fs::read_dir(entry_path)
        {
            for entry in entries.flatten() {
                let child = entry.path();
                if child.is_dir() {
                    walk(&child, total);
                }
            }
        }
    }
    let mut total = 0u64;
    walk(dir, &mut total);
    total
}

#[tokio::test]
async fn gallery_integration_cancel_kills_the_process() {
    // Cancelling must kill gallery-dl, not just the runner future: the spawn
    // used to build a ProcessGroupGuard whose killpg was a no-op because the
    // child never became a group leader — cancel/abort/shutdown left the
    // download running detached. The behavior is observable on the wire:
    // bytes stop flowing once the future is dropped.
    let Some(tool) = find_gallery_dl() else {
        eprintln!("SKIP: gallery-dl not on PATH");
        return;
    };
    let Some((_server, port)) = start_server() else {
        if std::env::var("GRAB_REQUIRE_GALLERY_DL").as_deref() == Ok("1") {
            panic!("GRAB_REQUIRE_GALLERY_DL=1 but cannot bind to port 80 for integration test");
        }
        eprintln!("SKIP: cannot bind to port 80 (need root for integration test)");
        return;
    };
    let url = format!("generic:http://127.0.0.1:{port}/slow/");
    let dest = std::env::temp_dir().join(format!("grab-gallery-cancel-{port}"));
    std::fs::create_dir_all(&dest).unwrap();
    let job = crate::gallery_argv::GalleryJob {
        item_id: 3,
        page_url: url.clone(),
        title: "test-gallery-cancel".to_string(),
        file_count: Some(1),
        dest_dir: dest.clone(),
        filter: GalleryFilter::All,
        cookies_browser: "none".to_string(),
        speed_limit: None,
        range: None,
    };
    // The runner borrows `job`; move clones into the spawned future.
    let tool = tool.clone();
    let task = tokio::spawn(async move {
        crate::gallery_runner::run_gallery_download(&job, &tool, None, None, |_| {}).await
    });

    // Wait until bytes are on disk, i.e. the download is truly in flight.
    let mut in_flight = 0u64;
    for _ in 0..250 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        in_flight = dir_bytes(&dest);
        if in_flight > 512 * 1024 {
            break;
        }
    }
    assert!(
        in_flight > 512 * 1024,
        "expected a streaming download; saw {in_flight} bytes"
    );

    // Cancel like the DownloadManager does: abort the runner future.
    task.abort();

    // Give the guard's group kill room to land (the child can't exit
    // abruptly without a final pipe write).
    tokio::time::sleep(Duration::from_millis(400)).await;
    let after_abort = dir_bytes(&dest);
    tokio::time::sleep(Duration::from_millis(900)).await;
    let settled = dir_bytes(&dest);
    assert_eq!(
        after_abort, settled,
        "bytes must stop flowing once cancelled (slow server sends continuously; \
         a surviving gallery-dl would keep growing the file)"
    );
    assert!(
        settled > 0 && settled < 100 * 1024 * 1024,
        "the file was mid-download when killed; saw {settled} bytes"
    );

    std::fs::remove_dir_all(&dest).ok();
}
