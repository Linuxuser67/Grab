//! Integration test for gallery-dl backend: runs real gallery-dl against a
//! local Python test server.
//!
//! Skipped when gallery-dl is not on PATH. Run in CI with a pinned
//! gallery-dl version.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

/// Find gallery-dl on PATH, or return None to skip the test.
fn find_gallery_dl() -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let p = dir.join("gallery-dl");
            if p.is_file() { Some(p) } else { None }
        })
    })
}

/// Start the Python gallery test server on an ephemeral port.
/// Returns (child, port).
fn start_server() -> (std::process::Child, u16) {
    // Find a free port.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
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
    (child, port)
}

#[tokio::test]
async fn gallery_integration_probe_count_download() {
    let Some(tool) = find_gallery_dl() else {
        eprintln!("SKIP: gallery-dl not on PATH");
        return;
    };
    let (_server, port) = start_server();
    let url = format!("generic:http://127.0.0.1:{port}/");

    // 1. Probe returns items.
    let items = crate::gallery_probe::probe_gallery(&tool, &url, None)
        .await
        .expect("probe failed")
        .expect("probe returned None");
    assert_eq!(items.len(), 3, "expected 3 images from test server");
    assert!(
        items.iter().all(|i| !i.filename.is_empty()),
        "all items need filenames"
    );

    // 2. Count matches.
    let count = crate::gallery_probe::count_gallery_files(&tool, &url, None).await;
    assert_eq!(count, Some(3), "count should be 3");

    // 3. Download produces files.
    let dest = std::env::temp_dir().join(format!("grab-gallery-test-{port}"));
    std::fs::create_dir_all(&dest).unwrap();
    let job = crate::gallery_argv::GalleryJob {
        item_id: 1,
        page_url: url.clone(),
        title: "test-gallery".to_string(),
        file_count: Some(3),
        dest_dir: dest.clone(),
        filter: crate::gallery_argv::GalleryFilter::All,
        cookies_file: None,
        speed_limit: None,
    };
    let mut completed = 0u32;
    let result = crate::gallery_runner::run_gallery_download(&job, &tool, |update| {
        completed = update.files_completed;
    })
    .await
    .expect("download failed");
    assert_eq!(result.files.len(), 3, "expected 3 downloaded files");
    assert_eq!(completed, 3, "progress should report 3 completed");
    for f in &result.files {
        assert!(f.exists(), "downloaded file should exist: {f:?}");
    }

    // 4. Second run reports all as skipped.
    let result2 = crate::gallery_runner::run_gallery_download(&job, &tool, |_| {})
        .await
        .expect("second download failed");
    assert_eq!(result2.files.len(), 0, "no new files on second run");
    assert_eq!(result2.skipped, 3, "all 3 files skipped on second run");

    // Cleanup.
    std::fs::remove_dir_all(&dest).ok();
}

#[tokio::test]
async fn gallery_integration_filtered_download() {
    let Some(tool) = find_gallery_dl() else {
        eprintln!("SKIP: gallery-dl not on PATH");
        return;
    };
    let (_server, port) = start_server();
    let url = format!("generic:http://127.0.0.1:{port}/");

    // Filtered run: images only (server has 2 jpg + 1 png, all images).
    let dest = std::env::temp_dir().join(format!("grab-gallery-filter-{port}"));
    std::fs::create_dir_all(&dest).unwrap();
    let job = crate::gallery_argv::GalleryJob {
        item_id: 2,
        page_url: url.clone(),
        title: "test-gallery-filter".to_string(),
        file_count: Some(3),
        dest_dir: dest.clone(),
        filter: crate::gallery_argv::GalleryFilter::ImagesOnly,
        cookies_file: None,
        speed_limit: None,
    };
    let result = crate::gallery_runner::run_gallery_download(&job, &tool, |_| {})
        .await
        .expect("filtered download failed");
    assert_eq!(
        result.files.len(),
        3,
        "all 3 test images pass the ImagesOnly filter"
    );

    std::fs::remove_dir_all(&dest).ok();
}
