//! Integration test for gallery-dl backend: runs real gallery-dl against a
//! local Python test server.
//!
//! Skipped when gallery-dl is not on PATH. Run in CI with a pinned
//! gallery-dl version.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

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
    msg.contains("AttributeError: 'NoneType' object has no attribute")
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
    let probe_result = crate::gallery_probe::probe_gallery(&tool, &url, None).await;
    let items = match probe_result {
        Ok(Some(items)) => items,
        Ok(None) => panic!("probe returned None"),
        Err(e) if is_gallery_dl_internal_bug(&e) => {
            // Upstream gallery-dl bug: the binary is installed and runs,
            // but crashes internally. When required (CI), this is a hard
            // failure; otherwise skip.
            if std::env::var("GRAB_REQUIRE_GALLERY_DL").as_deref() == Ok("1") {
                panic!("GRAB_REQUIRE_GALLERY_DL=1 but gallery-dl has an internal bug: {e:?}");
            }
            eprintln!("SKIP: gallery-dl has an internal bug in this environment: {e:?}");
            return;
        }
        Err(e) => panic!("probe failed: {e:?}"),
    };
    assert_eq!(items.len(), 3, "expected 3 images from test server");
    assert!(
        items.iter().all(|i| !i.filename.is_empty()),
        "all items need filenames"
    );

    // 2. Count matches.
    let count = crate::gallery_probe::count_gallery_files(
        &tool,
        &url,
        None,
        crate::gallery_argv::GalleryFilter::All,
    )
    .await;
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
        cookies_browser: "none".to_string(),
        speed_limit: None,
    };
    let completed = std::cell::Cell::new(0u32);
    let result = match crate::gallery_runner::run_gallery_download(&job, &tool, None, |update| {
        completed.set(update.files_completed);
    })
    .await
    {
        Ok(r) => r,
        Err(e) if is_gallery_dl_internal_bug(&e) => {
            // Upstream gallery-dl bug: the binary is installed and runs,
            // but crashes internally. When required (CI), this is a hard
            // failure; otherwise skip.
            if std::env::var("GRAB_REQUIRE_GALLERY_DL").as_deref() == Ok("1") {
                panic!("GRAB_REQUIRE_GALLERY_DL=1 but gallery-dl has an internal bug: {e:?}");
            }
            eprintln!("SKIP: gallery-dl has an internal bug in this environment: {e:?}");
            return;
        }
        Err(e) => panic!("download failed: {e:?}"),
    };
    assert_eq!(result.files.len(), 3, "expected 3 downloaded files");
    assert_eq!(completed.get(), 3, "progress should report 3 completed");
    for f in &result.files {
        assert!(f.exists(), "downloaded file should exist: {f:?}");
    }

    // 4. Second run reports all as skipped.
    let result2 = match crate::gallery_runner::run_gallery_download(&job, &tool, None, |_| {}).await
    {
        Ok(r) => r,
        Err(e) if is_gallery_dl_internal_bug(&e) => {
            // Upstream gallery-dl bug: the binary is installed and runs,
            // but crashes internally. When required (CI), this is a hard
            // failure; otherwise skip.
            if std::env::var("GRAB_REQUIRE_GALLERY_DL").as_deref() == Ok("1") {
                panic!("GRAB_REQUIRE_GALLERY_DL=1 but gallery-dl has an internal bug: {e:?}");
            }
            eprintln!("SKIP: gallery-dl has an internal bug in this environment: {e:?}");
            return;
        }
        Err(e) => panic!("second download failed: {e:?}"),
    };
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
    let Some((_server, port)) = start_server() else {
        if std::env::var("GRAB_REQUIRE_GALLERY_DL").as_deref() == Ok("1") {
            panic!("GRAB_REQUIRE_GALLERY_DL=1 but cannot bind to port 80 for integration test");
        }
        eprintln!("SKIP: cannot bind to port 80 (need root for integration test)");
        return;
    };
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
        cookies_browser: "none".to_string(),
        speed_limit: None,
    };
    let result = match crate::gallery_runner::run_gallery_download(&job, &tool, None, |_| {}).await
    {
        Ok(r) => r,
        Err(e) if is_gallery_dl_internal_bug(&e) => {
            // Upstream gallery-dl bug: the binary is installed and runs,
            // but crashes internally. When required (CI), this is a hard
            // failure; otherwise skip.
            if std::env::var("GRAB_REQUIRE_GALLERY_DL").as_deref() == Ok("1") {
                panic!("GRAB_REQUIRE_GALLERY_DL=1 but gallery-dl has an internal bug: {e:?}");
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
