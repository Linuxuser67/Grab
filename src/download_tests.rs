use super::*;
use crate::download_fetch::{
    AttemptFail, FetchCtx, StartMode, accepts_content_range, backoff_delay, fetch_piece,
    filename_from_content_disposition, has_holes, parse_content_range, rejects_unexpected_restart,
    response_total, run_download, stamp_request, truncate_to_prefix,
};
use crate::download_intake::{MAX_URL_LEN, normalize_url};
use crate::download_net::{
    DownloadOptions, PROXY_MODE_DIRECT, PROXY_MODE_SYSTEM, http_client, normalize_no_proxy,
    proxied_pool_len, proxy_mode_index, proxy_mode_labels, proxy_mode_value, proxy_type_index,
    proxy_type_labels, proxy_type_value,
};
use crate::download_pieces::{BLOCK_CELLS, SegmentState, aggregate, plan_pieces, split_count};
use crate::download_rate::{fmt_eta, format_amounts, live_rate_limit, parse_rate};
use crate::download_row::DownloadItem;
use crate::download_store::{QUEUE_VERSION, StoredItem, StoredQueue};
use crate::file_names::{
    PIECE_MAX, PIECE_MIN, dedupe_filename, filename_from_url, path_size, piece_len,
    rename_noreplace, restrict_filename_ascii, shorten_filename,
};

use crate::video::test_support::NoVideoTools;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

static QUEUE_FILE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// Serializes MainContext iteration: glib futures abort if polled from the
/// wrong thread. Take AFTER QUEUE_FILE_LOCK, always that order.
static MAIN_LOOP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Both locks in the safe order (queue, then loop); reversed order deadlocks the suite.
fn test_locks() -> (
    std::sync::MutexGuard<'static, ()>,
    std::sync::MutexGuard<'static, ()>,
) {
    // Recover from poison: a panicking test shouldn't cascade into 28 identical failures.
    let q = QUEUE_FILE_LOCK.lock().unwrap();
    let l = MAIN_LOOP_LOCK.lock().unwrap();
    (q, l)
}

/// Minimal persisted row for `restore_existing` fixtures (no id => pre-v3 allocate path).
fn stored_row(url: &str, dest_dir: &str, filename: &str, status: DownloadStatus) -> StoredItem {
    StoredItem {
        id: None,
        url: url.to_string(),
        dest_dir: dest_dir.to_string(),
        filename: filename.to_string(),
        status,
        progress: 0.0,
        segments: None,
        selected_files: None,
        output_dir: None,
        video_source: None,
        started: None,
        scheduled_at: None,
        etag: None,
        last_modified: None,
    }
}

fn test_queue_file(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "grab-q-{:?}-{tag}.json",
        std::thread::current().id()
    ));
    let _ = std::fs::remove_file(&p);
    // SAFETY: test setup runs before any test thread spawns.
    unsafe { std::env::set_var("GRAB_QUEUE_FILE", &p) };
    p
}

/// Drain the engine slot then quiesce so no woken tail trips a later test's
/// thread guard. Caller must hold MAIN_LOOP_LOCK (via test_locks).
fn drain_engine(manager: &DownloadManager, id: u64) {
    let ctx = glib::MainContext::default();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    // Wait for both the engine AND the pump: the pump is a `spawn_future_local` task, and if it's still pending when the test ends, the next test (on another thread) will trip glib's thread guard when reaping it.
    // Also wait for finished-row sizings: `set_finished_detail` spawns one
    // per completion, and it outlives the engine that triggered it.
    while (manager.running.borrow().contains_key(&id)
        || manager.pump_handles.borrow().contains_key(&id)
        || manager.sizing_inflight.get() > 0)
        && std::time::Instant::now() < deadline
    {
        ctx.iteration(false);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    quiesce(&ctx);
}

/// Pump until quiet (50 idle rounds) so no woken tail trips another test's loop.
fn quiesce(ctx: &glib::MainContext) {
    let mut idle_rounds = 0;
    while idle_rounds < 50 {
        if ctx.iteration(false) {
            idle_rounds = 0;
        } else {
            idle_rounds += 1;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// Drain finished-row sizing futures on this thread: each restored or
/// completed Done row spawns one via `set_finished_detail`, and a later test
/// on another thread trips glib's thread guard reaping them. The counter —
/// not a fixed delay — waits exactly for the in-flight GIO blocking tasks.
/// Caller must hold MAIN_LOOP_LOCK (via test_locks).
fn drain_sizing(manager: &DownloadManager) {
    let ctx = glib::MainContext::default();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while manager.sizing_inflight.get() > 0 {
        if std::time::Instant::now() > deadline {
            panic!("timed out draining finished-row sizing tasks");
        }
        ctx.iteration(false);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    quiesce(&ctx);
}

fn test_settings() -> crate::settings::AppSettings {
    // SAFETY: single-threaded setup phase (cargo runs with --test-threads=1).
    unsafe {
        std::env::set_var("GSETTINGS_SCHEMA_DIR", env!("GRAB_SCHEMA_DIR"));
        std::env::set_var("GSETTINGS_BACKEND", "memory");
    }
    crate::settings::AppSettings::new()
}

/// Fresh loopback port per call: a counter avoids pid collisions, a salt
/// avoids stale listeners leaked by crashed runs.
fn test_port(offset: u16) -> u16 {
    static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    static SALT: OnceLock<u64> = OnceLock::new();
    let salt = *SALT.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0);
        std::process::id() as u64 ^ nanos ^ ((nanos >> 17) | 1)
    });
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    21000
        + ((n as u64)
            .wrapping_mul(173)
            .wrapping_add(offset as u64)
            .wrapping_add(salt)
            % 40000) as u16
}

#[test]
fn backoff_delay_stays_within_cap() {
    // Bound-only: deterministic, never flakes. Do NOT assert distinctness (jitter is random).
    for _ in 0..100 {
        assert!(
            backoff_delay(1) <= std::time::Duration::from_millis(500),
            "failures=1 must be within 500ms cap"
        );
        assert!(
            backoff_delay(2) <= std::time::Duration::from_millis(1000),
            "failures=2 must be within 1000ms cap"
        );
        assert!(
            backoff_delay(3) <= std::time::Duration::from_millis(2000),
            "failures=3 must be within 2000ms cap"
        );
    }
}

/// Throwaway HTTP fixture. On failure everything stays behind for ranges.log replay.
struct Fixture {
    dir: std::path::PathBuf,
    dl: std::path::PathBuf,
    payload: Vec<u8>,
    port: u16,
    server: Rc<RefCell<std::process::Child>>,
}

fn spawn_fixture(
    tag: &str,
    served_name: &str,
    payload_len: u32,
    sleep_secs: &str,
    extra_args: &[&str],
    port_offset: u16,
) -> Fixture {
    let dir = std::env::temp_dir().join(format!("grab-{tag}-{}", std::process::id()));
    let srv = dir.join("srv");
    let dl = dir.join("dl");
    std::fs::create_dir_all(&srv).unwrap();
    std::fs::create_dir_all(&dl).unwrap();
    let payload: Vec<u8> = (0..payload_len).map(|i| (i % 251) as u8).collect();
    std::fs::write(srv.join(served_name), &payload).unwrap();
    // Retry fresh ports: CI fixture bring-up flakes, and a panic here poisons
    // the shared locks and cascades into later tests.
    let mut last_port = 0;
    for _ in 0..3 {
        let port = test_port(port_offset);
        last_port = port;
        let mut cmd = std::process::Command::new("python3");
        cmd.arg(format!(
            "{}/tests/throttled_server.py",
            env!("CARGO_MANIFEST_DIR")
        ))
        .arg(port.to_string())
        .arg(srv.join(served_name))
        .arg(dir.join("ranges.log"))
        .arg(sleep_secs);
        for a in extra_args {
            cmd.arg(a);
        }
        let server = Rc::new(RefCell::new(
            cmd.stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("python3 range server"),
        ));
        let mut ready = false;
        for _ in 0..100 {
            if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
                ready = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if ready {
            return Fixture {
                dir,
                dl,
                payload,
                port,
                server,
            };
        }
        let _ = server.borrow_mut().kill();
    }
    panic!("test HTTP server did not listen on port {last_port}");
}

/// Fail the current test, killing its server first (dirs stay for logs).
fn abort(server: &Rc<RefCell<std::process::Child>>, msg: &str) -> ! {
    let _ = server.borrow_mut().kill();
    panic!("TEST FAILURE: {msg}");
}

/// Success-path teardown shared by the live-server tests.
fn cleanup(server: &Rc<RefCell<std::process::Child>>, dir: &std::path::Path) {
    let _ = server.borrow_mut().kill();
    let _ = std::fs::remove_dir_all(dir);
}

/// Watchdog + run + quiescence drain shared by main-loop tests. Caller must
/// hold MAIN_LOOP_LOCK: only one test may iterate at a time.
fn run_loop(main_loop: &glib::MainLoop, watchdog_secs: u64) {
    let watchdog = main_loop.clone();
    let timed_out = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let (flag, done) = (Arc::clone(&timed_out), Arc::clone(&finished));
    std::thread::spawn(move || {
        // Poll so a finished test doesn't leave us sleeping for minutes.
        for _ in 0..watchdog_secs.max(1) {
            std::thread::sleep(std::time::Duration::from_secs(1));
            if done.load(Ordering::SeqCst) || !watchdog.is_running() {
                return;
            }
        }
        if !done.load(Ordering::SeqCst) && watchdog.is_running() {
            flag.store(true, Ordering::SeqCst);
            watchdog.quit();
        }
    });
    main_loop.run();
    finished.store(true, Ordering::SeqCst);
    assert!(!timed_out.load(Ordering::SeqCst), "TEST TIMEOUT");
    let ctx = glib::MainContext::default();
    let mut idle_rounds = 0;
    while idle_rounds < 50 {
        if ctx.iteration(false) {
            idle_rounds = 0;
        } else {
            idle_rounds += 1;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}
#[test]
fn splits_pieces() {
    // Too small: single-stream fallback.
    assert!(plan_pieces(0, 4).is_empty());
    assert!(plan_pieces(1024, 4).is_empty());
    assert!(plan_pieces(8 * 1024 * 1024 - 1, 4).is_empty());
    // Just under one segment per connection still splits if >= 2 fit.
    assert!(!plan_pieces(16 * 1024 * 1024 - 1, 4).is_empty());
    // One connection disables splitting entirely.
    assert!(plan_pieces(100 * 1024 * 1024, 1).is_empty());
    assert!(plan_pieces(100 * 1024 * 1024, 0).is_empty());
    // 100 MB at 4 connections: full 1 MB coverage, inclusive ends.
    let pieces = plan_pieces(100 * 1024 * 1024, 4);
    assert_eq!(pieces.len(), 100);
    assert_eq!(pieces[0], (0, 1024 * 1024 - 1));
    assert_eq!(pieces[99].1, 100 * 1024 * 1024 - 1);
    for w in pieces.windows(2) {
        assert_eq!(w[0].1 + 1, w[1].0);
    }
    let pieces = plan_pieces(9_000_000, 4);
    assert_eq!(pieces.len(), 9);
    assert_eq!(pieces.last().unwrap().1, 8_999_999);
    // Absurd server-claimed sizes never split (FIND-01): avoids giant bitmap/sparse file.
    assert!(plan_pieces(u64::MAX, 16).is_empty());
    assert!(plan_pieces(2 * (1 << 40), 16).is_empty());
    // Worker split count follows connections x 4 MB.
    assert_eq!(split_count(100 * 1024 * 1024, 4), 4);
    assert_eq!(split_count(100 * 1024 * 1024, 99), 16);
    assert_eq!(split_count(1024, 4), 0);
}

#[test]
fn piece_size_scales_with_total() {
    // Floor holds through ~4 GB: everything ordinary splits as before.
    assert_eq!(piece_len(1024), PIECE_MIN);
    assert_eq!(piece_len(4 * 1024 * 1024 * 1024), PIECE_MIN);
    // Beyond that pieces grow toward ~4k per download, capped at 16 MB.
    assert_eq!(piece_len(20 * 1024 * 1024 * 1024), 5 * 1024 * 1024);
    assert_eq!(piece_len(1 << 40), PIECE_MAX);
    // Bitmap stays aligned: the piece count covers the total exactly.
    let total = 20 * 1024 * 1024 * 1024;
    let st = SegmentState::new(total);
    assert_eq!(st.done.len() as u64, total.div_ceil(piece_len(total)));
    let pieces = plan_pieces(total, 4);
    assert_eq!(pieces.len() as u64, total.div_ceil(piece_len(total)));
    assert_eq!(pieces.last().unwrap().1, total - 1);
}

#[test]
fn parses_range_totals() {
    assert_eq!(parse_content_range("bytes 0-0/12345"), Some((0, 0, 12345)));
    assert_eq!(
        parse_content_range("bytes 1048576-2097151/8388608"),
        Some((1048576, 2097151, 8388608))
    );
    // Inverted range or garbage rejected.
    assert_eq!(parse_content_range("bytes 99-0/12345"), None);
    assert_eq!(parse_content_range("bytes */12345"), None);
    assert_eq!(parse_content_range("nonsense"), None);
    assert_eq!(parse_content_range("bytes 0-0/0"), None);
}

#[test]
fn resolves_response_totals() {
    assert_eq!(response_total(Some(100), None, false, 0), Some(100));
    assert_eq!(response_total(Some(60), None, true, 40), Some(100));
    // Chunked 206 without Content-Length: trust Content-Range.
    assert_eq!(
        response_total(None, Some("bytes 40-99/100"), true, 40),
        Some(100)
    );
    // Range starts elsewhere, or no headers at all: unknown.
    assert_eq!(response_total(None, Some("bytes 0-0/100"), true, 40), None);
    assert_eq!(response_total(None, None, true, 40), None);
    assert_eq!(response_total(None, None, false, 0), None);
}

#[test]
fn rejects_size_mismatched_restarts() {
    // Resumed range answered 200 with a different length: reject.
    assert!(rejects_unexpected_restart(false, 40, Some(100), Some(12)));
    // Same length, fresh start, or unknown lengths: proceed.
    assert!(!rejects_unexpected_restart(false, 40, Some(100), Some(100)));
    assert!(!rejects_unexpected_restart(false, 0, Some(100), Some(12)));
    assert!(!rejects_unexpected_restart(true, 40, Some(100), Some(60)));
    assert!(!rejects_unexpected_restart(false, 40, None, Some(12)));
    assert!(!rejects_unexpected_restart(false, 40, Some(100), None));
}

#[test]
fn accepts_content_range_validates_206_start() {
    // Correct start and total: accept.
    assert!(accepts_content_range(
        Some("bytes 40-99/100"),
        40,
        Some(100)
    ));
    // Correct start, no expected total: accept.
    assert!(accepts_content_range(Some("bytes 40-99/100"), 40, None));
    // Wrong start (the corruption case): reject, even with Content-Length present.
    assert!(!accepts_content_range(
        Some("bytes 0-59/100"),
        40,
        Some(100)
    ));
    // Wrong total: reject.
    assert!(!accepts_content_range(
        Some("bytes 40-99/200"),
        40,
        Some(100)
    ));
    // Missing or unparseable: reject (206 without valid Content-Range is non-compliant).
    assert!(!accepts_content_range(None, 40, Some(100)));
    assert!(!accepts_content_range(Some("garbage"), 40, Some(100)));
}

#[test]
fn prefix_len_caps_at_total() {
    // Tail piece is short: cap prefix at total so it can't extend the file.
    let mut st = SegmentState::new(1_500_000);
    st.mark(0);
    st.mark(1);
    assert_eq!(st.prefix_len(), 1_500_000);
}

#[test]
fn content_disposition_star_case_insensitive() {
    // Servers emit FILENAME*= too; parameter names are case-insensitive.
    assert_eq!(
        filename_from_content_disposition("attachment; FILENAME*=UTF-8''%E2%82%ACrates.mp4"),
        Some("€rates.mp4".to_string())
    );
}

#[test]
fn dedupe_caps_iterations() {
    // Everything taken: must still return (not stat the disk forever).
    let name = dedupe_filename("f.iso", |_| true);
    assert!(name.starts_with("f ("));
}

#[test]
fn corrupt_queue_is_quarantined() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let qf = test_queue_file("quarantine");
    std::fs::write(&qf, b"{not json").unwrap();
    let settings = test_settings();
    let m = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    m.restore_queue();
    assert_eq!(m.store().n_items(), 0);
    let bak = qf.with_extension("json.bak");
    assert!(bak.exists());
    let _ = std::fs::remove_file(&bak);
    let _ = std::fs::remove_file(&qf);
}

#[test]
fn invalid_utf8_queue_is_quarantined() {
    // read_to_string fails on invalid UTF-8: must quarantine like the other
    // corrupt-queue branches instead of silently starting empty (a later
    // persist would then overwrite the unreadable file).
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let qf = test_queue_file("quarantine-utf8");
    std::fs::write(&qf, b"{\xff\xfe not utf-8").unwrap();
    let settings = test_settings();
    let m = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    m.restore_queue();
    assert_eq!(m.store().n_items(), 0);
    let bak = qf.with_extension("json.bak");
    assert!(bak.exists());
    let _ = std::fs::remove_file(&bak);
    let _ = std::fs::remove_file(&qf);
}

#[test]
fn shutdown_with_timeout_returns_promptly_with_stuck_finalizer() {
    // A wedged finalizer must not hang the GTK thread on quit: the join is
    // bounded, partial files are left for the next launch to reconcile.
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("shutdown-timeout");
    let settings = test_settings();
    let m = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let worker = tokio_rt().spawn(async {});
    let finalizer = tokio_rt().spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    });
    m.discards.borrow_mut().insert(
        1,
        PendingDiscard {
            worker_abort: worker.abort_handle(),
            finalizer,
        },
    );
    let start = std::time::Instant::now();
    m.shutdown_with_timeout(std::time::Duration::from_millis(200));
    assert!(
        start.elapsed() < std::time::Duration::from_secs(2),
        "shutdown waited for a stuck finalizer"
    );
    let _ = std::fs::remove_file(test_queue_file("shutdown-timeout"));
}

#[test]
fn rename_noreplace_never_clobbers() {
    let dir = std::env::temp_dir().join(format!("grab-rename-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (a, b, c) = (dir.join("a.bin"), dir.join("b.bin"), dir.join("c.bin"));
    std::fs::write(&a, b"aaa").unwrap();
    std::fs::write(&b, b"bbb").unwrap();
    // Occupied destination: error, victim untouched.
    assert!(rename_noreplace(&a, &b).is_err());
    assert_eq!(std::fs::read(&b).unwrap(), b"bbb");
    assert_eq!(std::fs::read(&a).unwrap(), b"aaa");
    // Free destination: moved, source gone.
    assert!(rename_noreplace(&a, &c).is_ok());
    assert!(!a.exists());
    assert_eq!(std::fs::read(&c).unwrap(), b"aaa");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rename_noreplace_spans_filesystems() {
    // Staging may sit on tmpfs while downloads sit on disk: must survive EXDEV.
    let shm = std::path::Path::new("/dev/shm");
    if !shm.is_dir() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("grab-xdev-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = shm.join(format!("grab-xdev-src-{}", std::process::id()));
    let busy = dir.join("busy.bin");
    let free = dir.join("free.bin");
    std::fs::write(&src, b"cross-device bytes").unwrap();
    std::fs::write(&busy, b"victim").unwrap();
    // Occupied destination: error, victim and source untouched.
    assert!(rename_noreplace(&src, &busy).is_err());
    assert_eq!(std::fs::read(&busy).unwrap(), b"victim");
    // Free destination: content moved, source gone.
    assert!(rename_noreplace(&src, &free).is_ok());
    assert!(!src.exists());
    assert_eq!(std::fs::read(&free).unwrap(), b"cross-device bytes");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn overcap_queue_keeps_active_first() {
    let (_lock, _loop) = test_locks();
    let qf = test_queue_file("overcap");
    let settings = test_settings();
    let mut items = vec![
        StoredItem {
            id: None,
            url: "https://example.com/active.iso".to_string(),
            dest_dir: "/tmp/dl".to_string(),
            filename: "active.iso".to_string(),
            status: DownloadStatus::Queued,
            progress: 0.0,
            segments: None,
            selected_files: None,
            output_dir: None,
            video_source: None,
            started: None,
            scheduled_at: None,
            etag: None,
            last_modified: None,
        },
        StoredItem {
            id: None,
            url: "https://example.com/paused.iso".to_string(),
            dest_dir: "/tmp/dl".to_string(),
            filename: "paused.iso".to_string(),
            status: DownloadStatus::Paused,
            progress: 0.5,
            segments: None,
            selected_files: None,
            output_dir: None,
            video_source: None,
            started: None,
            scheduled_at: None,
            etag: None,
            last_modified: None,
        },
    ];
    for i in 0..1000 {
        items.push(StoredItem {
            id: None,
            url: format!("https://example.com/f{i}.iso"),
            dest_dir: "/tmp/dl".to_string(),
            filename: format!("f{i}.iso"),
            status: DownloadStatus::Done,
            progress: 1.0,
            segments: None,
            selected_files: None,
            output_dir: None,
            video_source: None,
            started: None,
            scheduled_at: None,
            etag: None,
            last_modified: None,
        });
    }
    let queue = StoredQueue {
        version: QUEUE_VERSION,
        items,
    };
    std::fs::write(&qf, serde_json::to_string(&queue).unwrap()).unwrap();
    settings.set_int("max-concurrent", 1).unwrap();
    let m = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    // Occupy the only slot so Queued restores can't spawn downloads.
    let holder = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    m.running.borrow_mut().insert(99, holder);
    m.restore_queue();
    assert_eq!(m.store().n_items(), 1000);
    let names: Vec<String> = (0..m.store().n_items())
        .filter_map(|i| m.store().item(i).and_downcast::<DownloadItem>())
        .map(|it| it.filename().to_string())
        .collect();
    // Resumable intent survives even though it was oldest in the file.
    assert!(names.contains(&"active.iso".to_string()));
    assert!(names.contains(&"paused.iso".to_string()));
    // Newest history kept, oldest history dropped.
    assert!(names.contains(&"f999.iso".to_string()));
    assert!(!names.contains(&"f0.iso".to_string()));
    assert!(!names.contains(&"f1.iso".to_string()));
    let _ = std::fs::remove_file(&qf);
    // Drain the restored Done rows' sizing futures on this thread (see MAIN_LOOP_LOCK).
    drain_sizing(&m);
}

#[test]
fn forgets_bitmap_beyond_prefix() {
    let mut st = SegmentState::new(4 * PIECE_MIN);
    st.mark(0);
    st.mark(1);
    st.mark(3);
    st.forget_beyond_prefix();
    assert_eq!(
        st.missing(),
        vec![
            (2, 2 * PIECE_MIN, 3 * PIECE_MIN - 1),
            (3, 3 * PIECE_MIN, 4 * PIECE_MIN - 1),
        ]
    );
    assert_eq!(st.prefix_len(), 2 * PIECE_MIN);
}

#[test]
fn truncates_to_prefix() {
    let dir = std::env::temp_dir().join(format!("grab-trunc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("p.bin");
    std::fs::write(&file, vec![7u8; 3 * PIECE_MIN as usize]).unwrap();
    // Pieces 0,1 done, 2 missing: shrink to 2 MB.
    let mut st = SegmentState::new(3 * PIECE_MIN);
    st.mark(0);
    st.mark(1);
    truncate_to_prefix(&file, &st);
    assert_eq!(std::fs::metadata(&file).unwrap().len(), 2 * PIECE_MIN);
    truncate_to_prefix(&file, &st);
    assert_eq!(std::fs::metadata(&file).unwrap().len(), 2 * PIECE_MIN);
    let st = SegmentState::new(3 * PIECE_MIN);
    truncate_to_prefix(&file, &st);
    assert_eq!(std::fs::metadata(&file).unwrap().len(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn parses_content_disposition() {
    assert_eq!(
        filename_from_content_disposition("attachment; filename=\"movie.mp4\""),
        Some("movie.mp4".to_string())
    );
    assert_eq!(
        filename_from_content_disposition("attachment; filename=movie.mp4"),
        Some("movie.mp4".to_string())
    );
    // RFC 5987 encoding wins over the plain fallback.
    assert_eq!(
        filename_from_content_disposition(
            "attachment; filename=\"fallback.bin\"; filename*=UTF-8''%E2%82%ACrates.mp4"
        ),
        Some("€rates.mp4".to_string())
    );
    // Directory components stripped (some servers send Windows paths).
    assert_eq!(
        filename_from_content_disposition("attachment; filename=\"C:\\\\dl\\\\movie.mp4\""),
        Some("movie.mp4".to_string())
    );
    assert_eq!(
        filename_from_content_disposition("attachment; FILENAME=\"upper.mp4\""),
        Some("upper.mp4".to_string())
    );
    // Garbage rejected: traversal, empty, missing.
    assert_eq!(filename_from_content_disposition("attachment"), None);
    assert_eq!(
        filename_from_content_disposition("attachment; filename=\"../evil.sh\""),
        Some("evil.sh".to_string())
    );
    assert_eq!(
        filename_from_content_disposition("attachment; filename=\"\""),
        None
    );
    assert_eq!(filename_from_content_disposition(""), None);
}

#[test]
fn detects_sparse_holes() {
    let dir = std::env::temp_dir().join(format!("grab-holes-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sparse = dir.join("sparse.bin");
    let f = std::fs::File::create(&sparse).unwrap();
    f.set_len(20_000_000).unwrap();
    assert!(has_holes(&sparse));
    std::fs::write(&sparse, vec![9u8; 100]).unwrap();
    assert!(!has_holes(&sparse));
    assert!(!has_holes(&dir.join("missing.bin")));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn parses_rates() {
    assert_eq!(parse_rate(""), None);
    assert_eq!(parse_rate("0"), None);
    assert_eq!(parse_rate("500K"), Some(500 * 1024));
    assert_eq!(parse_rate("2M"), Some(2 * 1024 * 1024));
    assert_eq!(parse_rate("1.5M"), Some(1572864));
    assert_eq!(parse_rate("3G"), Some(3 * 1024 * 1024 * 1024));
    assert_eq!(parse_rate("1024"), Some(1024));
    assert_eq!(parse_rate("junk"), None);
    assert_eq!(parse_rate("-5K"), None);
}

#[test]
fn rate_limit_follows_settings_live() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("rate-live");
    let settings = test_settings();
    // Watch publishes; cap follows with no re-queue, junk reads as unlimited.
    let _manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    settings.set_string("speed-limit", "1M").unwrap();
    assert_eq!(live_rate_limit(), Some(1024 * 1024));
    settings.set_string("speed-limit", "").unwrap();
    assert_eq!(live_rate_limit(), None);
    settings.set_string("speed-limit", "junk").unwrap();
    assert_eq!(live_rate_limit(), None);
    // Restore shared memory backend or a leaked value throttles later tests.
    settings.set_string("speed-limit", "").unwrap();
}

#[test]
fn notification_toggles() {
    // No pristine-defaults assert: the memory backend is process-shared, so this
    // checks live key -> method wiring instead.
    let settings = test_settings();
    settings.set_boolean("show-notifications", true).unwrap();
    settings.set_boolean("notify-background", true).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    assert!(manager.notifications_enabled());
    assert!(manager.background_notifications_enabled());
    settings.set_boolean("notify-background", false).unwrap();
    assert!(!manager.background_notifications_enabled());
    assert!(manager.notifications_enabled());
}

#[test]
fn transferring_ignores_paused() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("transferring");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    assert!(!manager.has_transferring());
    let paused = DownloadItem::new(1, "https://example.com/a.bin", "a.bin", "/tmp/dl");
    paused.set_status(DownloadStatus::Paused);
    manager.store().append(&paused);
    assert!(manager.has_active());
    assert!(!manager.has_transferring());
    let queued = DownloadItem::new(2, "https://example.com/b.bin", "b.bin", "/tmp/dl");
    queued.set_status(DownloadStatus::Queued);
    manager.store().append(&queued);
    assert!(manager.has_transferring());
}

/// A bulk insert must refresh the UI once, not once per row. `changed()`
/// walks the whole store several times over (its own queued recount,
/// `finished_count`, then the window hook's `sync()` plus a status predicate
/// or two), and every step is a GObject ref, downcast and property read per
/// element — so a caller that enqueues N rows inside a batch used to pay
/// that N times. Pins the contract any batch opener relies on: nothing
/// refreshes until the batch closes, and the close refreshes exactly once.
#[test]
fn a_bulk_insert_refreshes_the_ui_once() {
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("batch-ui-coalesce");
    let settings = test_settings();
    // Occupy the only slot so the queued rows below spawn no real engine.
    settings.set_int("max-concurrent", 1).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let holder = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    manager.running.borrow_mut().insert(99, holder);

    let refreshes = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let seen = std::rc::Rc::clone(&refreshes);
    manager.set_on_change(move || seen.set(seen.get() + 1));

    {
        let _batch = manager.batch_guard();
        for i in 0..5 {
            manager
                .enqueue(
                    &format!("https://example.com/batched-{i}.bin"),
                    Some("/tmp/dl"),
                    Some(&format!("batched-{i}.bin")),
                )
                .unwrap();
        }
        assert_eq!(
            manager.store().n_items(),
            5,
            "the rows themselves still land"
        );
        assert_eq!(
            refreshes.get(),
            0,
            "no refresh while the batch is open: every one is a wasted store walk"
        );
    } // Guard drops: the batch closes with exactly one refresh.

    assert_eq!(refreshes.get(), 1, "the close refreshes exactly once");
    // Deferring the recount must not lose it: the Queue button reads this.
    assert_eq!(manager.queued_count(), 5);
}

/// The same coalescing must not swallow the refresh a restore depends on:
/// `restore_queue` opens the batch itself, so the guard's close has to carry
/// the refresh — otherwise the restored queue list stays empty.
#[test]
fn a_restore_refreshes_the_ui_once_rows_landed() {
    let (_q, _l) = test_locks();
    let qf = test_queue_file("restore-ui-coalesce");
    let settings = test_settings();
    settings.set_int("max-concurrent", 1).unwrap();
    let items: Vec<StoredItem> = (0..5)
        .map(|i| {
            stored_row(
                &format!("https://example.com/r{i}.bin"),
                "/tmp/dl",
                &format!("r{i}.bin"),
                DownloadStatus::Queued,
            )
        })
        .collect();
    std::fs::write(
        &qf,
        serde_json::to_string(&StoredQueue {
            version: QUEUE_VERSION,
            items,
        })
        .unwrap(),
    )
    .unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let holder = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    manager.running.borrow_mut().insert(99, holder);

    let refreshes = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let seen = std::rc::Rc::clone(&refreshes);
    manager.set_on_change(move || seen.set(seen.get() + 1));

    manager.restore_queue();
    assert_eq!(manager.store().n_items(), 5);
    assert_eq!(
        refreshes.get(),
        1,
        "restore must still reach the window hook, or the queue list stays empty"
    );
    assert_eq!(manager.queued_count(), 5);
}

/// The row pulse timer stops on `Done` and nowhere else, which is only sound
/// because no command revives a finished row in place. This pins that
/// property directly: `retry`/`resume` revive Failed/Cancelled and Paused by
/// mutating the same item, so if any of them ever reached `Done` the stopped
/// timer would freeze a restarted row's indeterminate bar.
#[test]
fn no_command_revives_a_finished_row_in_place() {
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("done-is-terminal");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let done = DownloadItem::new(
        7,
        "https://example.com/finished.bin",
        "finished.bin",
        "/tmp/dl",
    );
    done.set_status(DownloadStatus::Done);
    manager.store().append(&done);

    manager.retry(7);
    assert_eq!(done.status(), DownloadStatus::Done, "retry must not revive");
    manager.resume(7);
    assert_eq!(
        done.status(),
        DownloadStatus::Done,
        "resume must not revive"
    );
    manager.pause(7);
    assert_eq!(done.status(), DownloadStatus::Done, "pause must not revive");
    manager.park(7);
    assert_eq!(done.status(), DownloadStatus::Done, "park must not revive");
    assert_eq!(
        manager.retry_failed(),
        0,
        "Retry Failed must skip done rows"
    );
    // Still the very same single item: reviving by re-insert would build a new
    // widget, which the timer could safely outlive, so assert it did not.
    assert_eq!(manager.store().n_items(), 1);
    assert_eq!(done.status(), DownloadStatus::Done);
}

/// `cancel` with Preserve used to stamp `Cancelled` on any non-live row,
/// including finished ones — the invariant rested on the stop button being
/// hidden for Done rows. A finished row is terminal, so the stamp now skips it
/// structurally: `retry` revives Failed/Cancelled rows in place, and reviving
/// a Done row with a stopped pulse timer would freeze its indeterminate bar.
#[test]
fn cancel_preserve_leaves_a_finished_row_done() {
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("cancel-keeps-done");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let done = DownloadItem::new(
        7,
        "https://example.com/finished.bin",
        "finished.bin",
        "/tmp/dl",
    );
    done.set_status(DownloadStatus::Done);
    manager.store().append(&done);

    manager.cancel(7);
    assert_eq!(
        done.status(),
        DownloadStatus::Done,
        "cancel must not clobber a finished row"
    );
}

#[test]
fn enqueue_video_spawns_and_fails_without_tools() {
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("enqueue-video");
    let _notools = NoVideoTools::apply();
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::env::temp_dir()
        .join("grab-video-enqueue")
        .to_string_lossy()
        .into_owned();
    let item = manager
        .enqueue_video(
            "https://www.youtube.com/watch?v=gXtp6C-3JKo",
            Some(&dest),
            Some("My Video.mp4"),
            crate::media_types::VideoChoices {
                quality: "1080p".to_string(),
                audio_only: false,
                video_format_id: None,
                is_live: false,
                playlist_item_id: None,
            },
        )
        .expect("video enqueue");
    let id = item.id();
    // The worker runs and fails fast on the missing tools (no network).
    drain_engine(&manager, id);
    assert_eq!(item.status(), DownloadStatus::Failed);
    assert_eq!(
        item.detail(),
        "Media downloads need the yt-dlp support tools"
    );
    // The Page marker survives the failure, so Retry replays the pipeline.
    assert!(matches!(
        manager.video_source(id),
        Some(crate::media_types::VideoSource::Page { .. })
    ));
    // Engine slot and abort sender are both released.
    assert!(!manager.running.borrow().contains_key(&id));
    assert!(!manager.video_abort.borrow().contains_key(&id));
    // The worker stages dest-side before failing on the missing tools.
    let dest_dir = std::path::Path::new(&dest);
    crate::video::clean_staging_files(dest_dir, id);
}

#[test]
fn enqueue_video_restrict_filenames_folds_name() {
    // Opt-in applies where Grab names the file (enqueue): yt-dlp only sanitizes its own template fields.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("enqueue-restrict");
    let _notools = NoVideoTools::apply();
    let settings = test_settings();
    settings.set_boolean("restrict-filenames", true).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::env::temp_dir().join("grab-video-restrict");
    std::fs::create_dir_all(&dest).unwrap();
    let dest = dest.to_string_lossy().into_owned();
    let item = manager
        .enqueue_video(
            "https://www.youtube.com/watch?v=gXtp6C-3JKo",
            Some(&dest),
            Some("Café & Croissants.mp4"),
            crate::media_types::VideoChoices {
                quality: "1080p".to_string(),
                audio_only: false,
                video_format_id: None,
                is_live: false,
                playlist_item_id: None,
            },
        )
        .expect("video enqueue");
    assert_eq!(item.filename(), "Cafe_Croissants.mp4");
    let id = item.id();
    drain_engine(&manager, id);
    // The worker stages dest-side before failing on the missing tools.
    let dest_dir = std::path::Path::new(&dest);
    crate::video::clean_staging_files(dest_dir, id);
}

#[test]
fn a_restored_row_keeps_its_id_so_its_staging_stays_reachable() {
    // Id IS the staging key: restore must keep it or retry scans the wrong dir.
    // The gap makes it observable (contiguous restore passes on broken code).
    let (_q, _l) = test_locks();
    let qf = test_queue_file("restore-id");
    let dest = std::env::temp_dir().join("grab-restore-id");
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    let dest = dest.to_string_lossy().into_owned();

    let add_paused = |manager: &Rc<DownloadManager>, url: &str, name: &str| {
        manager
            .restore_existing(&stored_row(url, &dest, name, DownloadStatus::Paused))
            .expect("row created")
            .id()
    };

    let (first, middle, last) = {
        let settings = test_settings();
        let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
        let first = add_paused(&manager, "https://example.com/a.bin", "a.bin");
        let middle = add_paused(&manager, "https://example.com/b.bin", "b.bin");
        let last = add_paused(&manager, "https://example.com/c.bin", "c.bin");
        // Remove the middle row so the survivors are no longer contiguous.
        manager.remove(middle);
        (first, middle, last)
    };
    assert!(
        last > middle && middle > first,
        "the fixture needs three increasing ids ({first}, {middle}, {last})"
    );

    // A fresh manager over the same queue file.
    let settings = test_settings();
    let manager2 = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    manager2.restore_queue();
    for (id, url) in [
        (first, "https://example.com/a.bin"),
        (last, "https://example.com/c.bin"),
    ] {
        let row = (0..manager2.store().n_items())
            .filter_map(|i| manager2.store().item(i).and_downcast::<DownloadItem>())
            .find(|it| it.url() == url)
            .unwrap_or_else(|| panic!("row for {url} vanished across the restart"));
        assert_eq!(
            row.id(),
            id,
            "the row came back as {} so its staging dir \
             (<temp>/grab-video/{id}) is no longer the one its attempt wrote to",
            row.id()
        );
    }

    // A row created after the restore must not be handed a number a
    // restored row still owns, or the two would share a staging directory.
    let fresh = add_paused(&manager2, "https://example.com/d.bin", "d.bin");
    assert!(
        fresh != first && fresh != last,
        "a new row was handed id {fresh}, which a restored row still owns, so \
         both would share one staging directory"
    );

    let _ = std::fs::remove_dir_all(&dest);
    let _ = std::fs::remove_file(&qf);
}

#[test]
fn a_fresh_row_never_lands_on_a_dest_side_leftover_staging_dir() {
    // A leftover `grab-<id>-*` in the dest dir is invisible to the allocator,
    // so it must skip IDs with leftovers per-row or a new download would
    // resume (or sweep) a stranger's scratch. Leftovers stay on disk:
    // unreachable beats deleted.
    let (_q, _l) = test_locks();
    let qf = test_queue_file("dest-leftover-guard");
    let dest = std::env::temp_dir().join(format!("grab-dest-leftover-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    let dest_s = dest.to_string_lossy().into_owned();

    // A crashed attempt's leftover: scratch plus a completed recording (the user's only copy).
    // New scheme: `grab-<id>-*` files directly in dest dir.
    let leftover = dest.clone();
    std::fs::create_dir_all(&leftover).unwrap();
    std::fs::write(leftover.join("grab-9001-video.mp4.part"), b"scratch").unwrap();
    std::fs::write(
        leftover.join("grab-9001-final.1.mp4"),
        b"someone's recording",
    )
    .unwrap();

    // Row A pins the allocator at 9001; row B has no persisted id, so restore allocates.
    let items = vec![
        StoredItem {
            id: Some(9_000),
            url: "https://example.com/a.bin".to_string(),
            dest_dir: dest_s.clone(),
            filename: "a.bin".to_string(),
            status: DownloadStatus::Paused,
            progress: 0.0,
            segments: None,
            selected_files: None,
            output_dir: None,
            video_source: None,
            started: None,
            scheduled_at: None,
            etag: None,
            last_modified: None,
        },
        StoredItem {
            id: None,
            url: "https://example.com/b.bin".to_string(),
            dest_dir: dest_s.clone(),
            filename: "b.bin".to_string(),
            status: DownloadStatus::Paused,
            progress: 0.0,
            segments: None,
            selected_files: None,
            output_dir: None,
            video_source: None,
            started: None,
            scheduled_at: None,
            etag: None,
            last_modified: None,
        },
    ];
    std::fs::write(
        &qf,
        serde_json::to_string_pretty(&StoredQueue { version: 2, items }).unwrap(),
    )
    .unwrap();

    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    manager.restore_queue();
    let b = (0..manager.store().n_items())
        .filter_map(|i| manager.store().item(i).and_downcast::<DownloadItem>())
        .find(|it| it.url() == "https://example.com/b.bin")
        .expect("row B restored");
    assert_ne!(
        b.id(),
        9_001,
        "the restored row took id 9001 which a dest-side leftover staging dir still owns"
    );
    assert_eq!(
        std::fs::read(leftover.join("grab-9001-final.1.mp4")).unwrap(),
        b"someone's recording",
        "the leftover recording was destroyed instead of merely left unreachable"
    );
    assert!(
        leftover.join("grab-9001-video.mp4.part").exists(),
        "manifest-less orphan scratch is preserved (no manifest = no deletion)"
    );
    let _ = std::fs::remove_dir_all(&dest);
    let _ = std::fs::remove_file(&qf);
}

#[test]
fn video_source_survives_restore_and_retry() {
    let (_q, _l) = test_locks();
    let qf = test_queue_file("video-restore");
    let _notools = NoVideoTools::apply();
    let dest = std::env::temp_dir().join("grab-video-restore");
    std::fs::create_dir_all(&dest).unwrap();
    let dest = dest.to_string_lossy().into_owned();
    let id = {
        let settings = test_settings();
        let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
        let item = manager
            .enqueue_video(
                "https://vimeo.com/123456",
                Some(&dest),
                Some("Clip.mp4"),
                crate::media_types::VideoChoices {
                    quality: "720p".to_string(),
                    audio_only: false,
                    video_format_id: None,
                    is_live: false,
                    playlist_item_id: None,
                },
            )
            .expect("video enqueue");
        // Persisted with the Page marker (not silently dropped).
        let text = std::fs::read_to_string(&qf).unwrap();
        assert!(text.contains("\"video_source\""));
        assert!(text.contains("vimeo.com/123456"));
        let id = item.id();
        drain_engine(&manager, id);
        id
    };
    // Fresh manager over the same queue file re-stages the source and
    // replays the worker (which fails fast here, proving the dispatch).
    let settings = test_settings();
    let manager2 = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    manager2.restore_queue();
    let restored = manager2.find(id).expect("restored video row");
    drain_engine(&manager2, id);
    assert_eq!(restored.status(), DownloadStatus::Failed);
    let stored = manager2.video_source(id).expect("re-staged source");
    assert!(
        matches!(stored, crate::media_types::VideoSource::Page { ref quality, .. } if quality == "720p")
    );
    crate::video::clean_staging_files(std::path::Path::new(&dest), id);
    let _ = std::fs::remove_file(&qf);
}

#[test]
fn mismatched_video_source_dropped_on_restore() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let qf = test_queue_file("video-mismatch");
    // Hand-edited queue: the Page marker names another video.
    let queue = StoredQueue {
        version: QUEUE_VERSION,
        items: vec![StoredItem {
            id: None,
            url: "https://example.com/f.iso".into(),
            dest_dir: "/tmp/dl".into(),
            filename: "f.iso".into(),
            status: DownloadStatus::Paused,
            progress: 0.0,
            segments: None,
            selected_files: None,
            output_dir: None,
            video_source: Some(crate::media_types::VideoSource::Page {
                page_url: "https://vimeo.com/OTHER".into(),
                media_url: None,
                expires_at: None,
                quality: "1080p".into(),
                audio_only: false,
                is_live: false,
                video_format_id: None,
                playlist_item_id: None,
            }),
            started: None,
            scheduled_at: None,
            etag: None,
            last_modified: None,
        }],
    };
    std::fs::write(&qf, serde_json::to_string(&queue).unwrap()).unwrap();
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    // Paused stays parked: no spawn, no network.
    manager.restore_queue();
    assert_eq!(manager.store().n_items(), 1);
    let item = manager
        .store()
        .item(0)
        .and_downcast::<DownloadItem>()
        .unwrap();
    assert_eq!(manager.video_source(item.id()), None);
    let _ = std::fs::remove_file(&qf);
}

/// A finished video restored from the queue file used to lose its Page
/// source: `insert_history` rebuilt the Done row without staging the
/// video source, so the row fell back to the generic file icon instead of
/// the video icon.
#[test]
fn done_video_keeps_page_source_on_restore() {
    let (_lock, _loop) = test_locks();
    let qf = test_queue_file("video-done-restore");
    let queue = StoredQueue {
        version: QUEUE_VERSION,
        items: vec![StoredItem {
            id: None,
            url: "https://vimeo.com/123456".into(),
            dest_dir: "/tmp/dl".into(),
            filename: "Clip.mp4".into(),
            status: DownloadStatus::Done,
            progress: 1.0,
            segments: None,
            selected_files: None,
            output_dir: None,
            video_source: Some(crate::media_types::VideoSource::Page {
                page_url: "https://vimeo.com/123456".into(),
                media_url: None,
                expires_at: None,
                quality: "720p".into(),
                audio_only: false,
                is_live: false,
                video_format_id: None,
                playlist_item_id: None,
            }),
            started: None,
            scheduled_at: None,
            etag: None,
            last_modified: None,
        }],
    };
    std::fs::write(&qf, serde_json::to_string(&queue).unwrap()).unwrap();
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    // Done rows rebuild through insert_history: no spawn, no network.
    manager.restore_queue();
    assert_eq!(manager.store().n_items(), 1);
    let item = manager
        .store()
        .item(0)
        .and_downcast::<DownloadItem>()
        .unwrap();
    assert_eq!(item.status(), DownloadStatus::Done);
    let stored = manager
        .video_source(item.id())
        .expect("restored Done row keeps its Page source");
    assert!(
        matches!(stored, crate::media_types::VideoSource::Page { ref quality, .. } if quality == "720p")
    );
    let _ = std::fs::remove_file(&qf);
    // Drain the restored Done row's sizing future on this thread (see MAIN_LOOP_LOCK).
    drain_sizing(&manager);
}

#[test]
fn unremove_restores_video_source() {
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("video-unremove");
    let _notools = NoVideoTools::apply();
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::env::temp_dir()
        .join("grab-video-unremove")
        .to_string_lossy()
        .into_owned();
    let item = manager
        .enqueue_video(
            "https://vimeo.com/123456",
            Some(&dest),
            Some("Clip.mp4"),
            crate::media_types::VideoChoices {
                quality: "720p".to_string(),
                audio_only: false,
                video_format_id: None,
                is_live: false,
                playlist_item_id: None,
            },
        )
        .expect("video enqueue");
    let id = item.id();
    drain_engine(&manager, id);
    assert_eq!(item.status(), DownloadStatus::Failed);
    let snap = RemovedSnapshot {
        url: item.url().to_string(),
        dest_dir: item.dest_dir().to_string(),
        filename: item.filename().to_string(),
        status: item.status(),
        progress: item.progress(),
        detail: item.detail().to_string(),
        output_dir: item.output_dir().to_string(),
        segments: None,
        video_source: manager.video_source(id),
        started: false,
    };
    manager.remove(id);
    assert_eq!(manager.video_source(id), None);
    let revived = manager.unremove(snap);
    let new_id = revived.id();
    // Failed requeues and replays the worker (fast tools failure here).
    drain_engine(&manager, new_id);
    assert_eq!(revived.status(), DownloadStatus::Failed);
    assert!(matches!(
        manager.video_source(new_id),
        Some(crate::media_types::VideoSource::Page { .. })
    ));
    // The workers staged dest-side before failing; remove() already reclaimed
    // the first id's dir, this drops the revived row's.
    let dest_dir = std::path::Path::new(&dest);
    for stale in [id, new_id] {
        crate::video::clean_staging_files(dest_dir, stale);
    }
}

#[test]
fn queue_file_never_carries_cookies() {
    // Secrets must not reach the persisted queue, whatever the settings.
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _notools = NoVideoTools::apply();
    let qf = test_queue_file("video-cookies-persist");
    let settings = test_settings();
    settings.set_string("cookies-browser", "firefox").unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let item = manager
        .enqueue_video(
            "https://vimeo.com/123456",
            Some("/tmp/dl"),
            Some("Clip.mp4"),
            crate::media_types::VideoChoices {
                quality: "1080p".to_string(),
                audio_only: false,
                video_format_id: None,
                is_live: false,
                playlist_item_id: None,
            },
        )
        .expect("video enqueue");
    // Drain the worker here so no woken pump tail trips another test's thread guard.
    drain_engine(&manager, item.id());
    // The worker stages dest-side before failing on the missing tools.
    let dest_dir = std::path::Path::new("/tmp/dl");
    crate::video::clean_staging_files(dest_dir, item.id());
    let text = std::fs::read_to_string(&qf).unwrap();
    assert!(
        !text.contains("cookies"),
        "persisted queue must not mention cookies: {text}"
    );
    let _ = std::fs::remove_file(&qf);
}

#[test]
fn active_count_covers_cancel_all_scope() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("active-count");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    assert_eq!(manager.active_count(), 0);
    for (id, status) in [
        (1, DownloadStatus::Queued),
        (2, DownloadStatus::Downloading),
        (3, DownloadStatus::Paused),
        (4, DownloadStatus::Done),
        (5, DownloadStatus::Failed),
        (6, DownloadStatus::Cancelled),
    ] {
        let it = DownloadItem::new(id, "https://example.com/f.bin", "f.bin", "/tmp/dl");
        it.set_status(status);
        manager.store().append(&it);
    }
    assert_eq!(manager.active_count(), 3);
    assert!(manager.has_active());
}

#[test]
fn error_banner_ignores_cancelled() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("error-banner");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    assert!(!manager.has_failed());
    assert!(!manager.has_errored());
    for id in 1..=2 {
        let it = DownloadItem::new(id, "https://example.com/f.bin", "f.bin", "/tmp/dl");
        manager.store().append(&it);
    }
    manager.cancel_all();
    assert!(manager.has_failed());
    assert!(!manager.has_errored());
    let failed = DownloadItem::new(3, "https://example.com/g.bin", "g.bin", "/tmp/dl");
    failed.set_status(DownloadStatus::Failed);
    manager.store().append(&failed);
    assert!(manager.has_errored());
}

#[test]
fn formats_amounts() {
    assert_eq!(format_amounts(0, 1024), "0 bytes of 1.0\u{a0}kB");
    assert_eq!(
        format_amounts(5 * 1024 * 1024, 2 * 1024 * 1024 * 1024),
        "5.2\u{a0}MB of 2.1\u{a0}GB"
    );
}

#[test]
fn formats_bytes() {
    // glib::format_size: SI base-1000, "bytes" for sub-kB, NBSP before unit.
    assert_eq!(fmt_bytes(0), "0 bytes");
    assert_eq!(fmt_bytes(999), "999 bytes");
    assert_eq!(fmt_bytes(1024), "1.0\u{a0}kB");
    assert_eq!(fmt_bytes(1536), "1.5\u{a0}kB");
    assert_eq!(fmt_bytes(5 * 1024 * 1024), "5.2\u{a0}MB");
    assert_eq!(fmt_bytes(3 * 1024 * 1024 * 1024), "3.2\u{a0}GB");
}

#[test]
fn path_size_sums_folders() {
    // Multi-file torrents finish into a folder: report the content total, not the dir entry size.
    let dir = std::env::temp_dir().join(format!("grab-path-size-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sub = dir.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(dir.join("a.bin"), vec![0u8; 100]).unwrap();
    std::fs::write(sub.join("b.bin"), vec![0u8; 200]).unwrap();
    std::fs::write(dir.join("empty.bin"), []).unwrap();
    assert_eq!(path_size(&dir), Some(300));
    // Files still report their own length; missing paths read as absent.
    assert_eq!(path_size(&dir.join("a.bin")), Some(100));
    assert_eq!(path_size(&dir.join("nope")), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn path_size_async_measures_off_thread() {
    // The async wrapper must actually measure, not just resolve: a stub that
    // returns None without scanning would leave finished rows unsized.
    // MAIN_LOOP_LOCK: glib futures abort if polled from the wrong thread.
    let _loop = MAIN_LOOP_LOCK.lock().unwrap();
    let dir = std::env::temp_dir().join(format!("grab-path-size-async-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sub = dir.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(dir.join("a.bin"), vec![0u8; 100]).unwrap();
    std::fs::write(sub.join("b.bin"), vec![0u8; 200]).unwrap();
    let size = glib::MainContext::new().block_on(crate::file_names::path_size_async(dir.clone()));
    assert_eq!(size, Some(300));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn formats_eta() {
    assert_eq!(fmt_eta(0), "0 seconds");
    assert_eq!(fmt_eta(1), "1 second");
    assert_eq!(fmt_eta(45), "45 seconds");
    assert_eq!(fmt_eta(125), "2 minutes");
    assert_eq!(fmt_eta(3723), "1 hour");
}

#[test]
fn filenames() {
    assert_eq!(filename_from_url("https://example.com/a/b.iso"), "b.iso");
    assert_eq!(filename_from_url("https://example.com/"), "index.html");
    assert_eq!(filename_from_url("not a url"), "index.html");
    assert_eq!(
        filename_from_url("https://example.com/a/my%20game.zip"),
        "my game.zip"
    );
}

#[test]
fn urls() {
    assert!(normalize_url("https://example.com/f.iso").is_ok());
    assert!(normalize_url("ftp://example.com/f").is_err());
    assert!(normalize_url("file:///etc/passwd").is_err());
    assert!(normalize_url("--post-file=x").is_err());
    assert!(normalize_url("localhost:8080/f.iso").is_ok());
    // Userinfo would persist plaintext creds in queue.json: reject.
    assert!(normalize_url("https://user:pass@example.com/f.iso").is_err());
    assert!(normalize_url("https://user@example.com/f.iso").is_err());
    assert!(normalize_url("user:pass@example.com/f.iso").is_err());
}

#[test]
fn magnet_links() {
    let good = "magnet:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3&dn=test";
    assert_eq!(normalize_url(good).as_deref(), Ok(good));
    assert_eq!(
        normalize_url("  MAGNET:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3  ")
            .as_deref(),
        Ok("MAGNET:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3")
    );
    // Tracker-heavy magnets (Ubuntu's run ~2-4 KB) bypass the 2048
    // HTTP cap: parsed locally, never sent as a request line.
    let big = format!(
        "magnet:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3{}",
        "&tr=udp://tracker.example.com:1337/announce".repeat(100)
    );
    assert!(big.len() > MAX_URL_LEN);
    assert_eq!(normalize_url(&big).as_deref(), Ok(big.as_str()));
    // Absurd magnets still rejected.
    let huge = format!(
        "magnet:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3&x={}",
        "a".repeat(16384)
    );
    assert!(normalize_url(&huge).is_err());
    // Tracker params contain `://`: must not reach the http scheme branch.
    let tracked = "magnet:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3&tr=http://tracker.example.com:80/announce&tr=udp://tracker.example.com:1337/announce";
    assert_eq!(normalize_url(tracked).as_deref(), Ok(tracked));
    // Pasted BOM must not defeat the magnet classifier.
    let bom = format!("\u{feff}{good}");
    assert_eq!(normalize_url(&bom).as_deref(), Ok(good));
    // Magnet-shaped but unparseable: rejected by the parser, never the scheme branch.
    let bad_scheme = normalize_url("magnet://xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3");
    assert!(bad_scheme.is_err());
    assert!(!bad_scheme.unwrap_err().contains("Unsupported scheme"));
    // No BTv1 info-hash: unresolvable, reject at intake.
    assert!(normalize_url("magnet:?dn=nameless").is_err());
    assert!(normalize_url("magnet:?xt=urn:btih:xyz").is_err());
    assert!(normalize_url("magnet:").is_err());
}

/// Minimal single-file .torrent: info{length: 1, name: "foo"}.
fn single_torrent_bytes() -> Vec<u8> {
    format!(
        "d8:announce12:http://t.co/4:infod6:lengthi1e4:name3:foo12:piece lengthi16384e6:pieces20:{}ee",
        "A".repeat(20)
    )
    .into_bytes()
}

/// Minimal multi-file .torrent. Info-dict keys must be sorted: the parser enforces canonical order.
fn multi_torrent_bytes() -> Vec<u8> {
    format!(
        "d8:announce12:http://t.co/4:infod5:filesld6:lengthi2e4:pathl5:a.txteed6:lengthi3e4:pathl3:sub5:b.txteee4:name3:bar12:piece lengthi16384e6:pieces20:{}ee",
        "A".repeat(20)
    )
    .into_bytes()
}

#[test]
fn torrent_urls() {
    // Nothing archived under that path: rejected at intake.
    assert!(normalize_url("torrent:/nope/missing.torrent").is_err());
    assert!(!crate::torrent::is_torrent_url(
        "magnet:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3"
    ));
    // A real archived file round-trips through normalize.
    let stem = format!("grab-test-{}", std::process::id());
    let pseudo =
        crate::torrent::archive_torrent_file(&format!("{stem}.torrent"), &single_torrent_bytes())
            .unwrap();
    assert!(crate::torrent::is_torrent_url(&pseudo));
    assert!(crate::torrent::is_torrent(&pseudo));
    assert_eq!(normalize_url(&pseudo).as_deref(), Ok(pseudo.as_str()));
    crate::torrent::delete_archive_for_url(&pseudo);
    assert!(crate::torrent::archive_path_for_url(&pseudo).is_none());
}

#[test]
fn torrent_file_list_parses() {
    let (name, entries) = crate::torrent::torrent_file_list(&single_torrent_bytes()).unwrap();
    assert_eq!(name, "foo");
    assert!(entries.is_empty());
    let (name, entries) = crate::torrent::torrent_file_list(&multi_torrent_bytes()).unwrap();
    assert_eq!(name, "bar");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].raw_path, "a.txt");
    assert_eq!(entries[0].length, 2);
    assert_eq!(entries[1].raw_path, "sub/b.txt");
    assert_eq!(entries[1].length, 3);
    assert!(crate::torrent::torrent_file_list(b"not a torrent").is_err());
}

#[test]
fn sweep_keeps_only_referenced_archives() {
    let dir = std::env::temp_dir().join(format!("grab-sweep-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let keep = dir.join("keep.torrent");
    let drop = dir.join("drop.torrent");
    let skip = dir.join("notes.txt");
    std::fs::write(&keep, b"x").unwrap();
    std::fs::write(&drop, b"x").unwrap();
    std::fs::write(&skip, b"x").unwrap();
    let mut referenced = std::collections::HashSet::new();
    referenced.insert(format!("torrent:{}", keep.to_string_lossy()));
    crate::torrent::sweep_archives_in(&dir, &referenced);
    assert!(keep.exists());
    assert!(!drop.exists());
    assert!(skip.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn staged_selection_survives_respawn() {
    // Regression: take-once lost the file filter on re-spawn; selections now peek until pruned.
    let url = "torrent:/tmp/grab-test-sel.torrent";
    crate::torrent::stage_selection(url, vec![2]);
    assert_eq!(crate::torrent::get_selection(url), Some(vec![2]));
    assert_eq!(crate::torrent::get_selection(url), Some(vec![2]));
    let mut referenced = std::collections::HashSet::new();
    referenced.insert(url.to_string());
    crate::torrent::prune_selections(&referenced);
    assert_eq!(crate::torrent::get_selection(url), Some(vec![2]));
    crate::torrent::prune_selections(&std::collections::HashSet::new());
    assert_eq!(crate::torrent::get_selection(url), None);
}

#[test]
fn torrent_file_enqueue_uses_stem_stub() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("torrent-enqueue");
    let settings = test_settings();
    // Occupy the only slot so nothing spawns a real engine below.
    settings.set_int("max-concurrent", 1).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    let holder = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    manager.running.borrow_mut().insert(99, holder);
    let stem = format!("grab-enqueue-{}", std::process::id());
    let item = manager
        .enqueue_torrent_file(
            single_torrent_bytes(),
            &format!("{stem}.torrent"),
            None,
            None,
        )
        .unwrap();
    assert_eq!(item.status(), DownloadStatus::Queued);
    assert!(crate::torrent::is_torrent_url(&item.url()));
    assert!(sane_filename(&item.filename()));
    assert!(!item.filename().is_empty());
    // Leave no Queued row/archive behind; restore the shared memory-backend key.
    manager.cancel_all();
    crate::torrent::delete_archive_for_url(&item.url());
    assert!(crate::torrent::archive_path_for_url(&item.url()).is_none());
    settings.set_int("max-concurrent", 3).unwrap();
}

#[test]
fn staged_selection_reaches_spawn_take() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("torrent-take");
    let settings = test_settings();
    // Occupy the only slot so nothing spawns a real engine below.
    settings.set_int("max-concurrent", 1).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    let holder = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    manager.running.borrow_mut().insert(99, holder);
    let stem = format!("grab-take-{}", std::process::id());
    let item = manager
        .enqueue_torrent_file(
            single_torrent_bytes(),
            &format!("{stem}.torrent"),
            None,
            Some(vec![0]),
        )
        .unwrap();
    // Must survive the archive → normalize → store round-trip identically.
    assert_eq!(
        crate::torrent::get_selection(&item.url().to_string()),
        Some(vec![0])
    );
    // Leave no Queued row/archive behind; restore the shared memory-backend key.
    manager.cancel_all();
    crate::torrent::delete_archive_for_url(&item.url());
    assert!(crate::torrent::archive_path_for_url(&item.url()).is_none());
    settings.set_int("max-concurrent", 3).unwrap();
}

#[test]
fn delete_download_trashes_torrent_files() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("torrent-del");
    let settings = test_settings();
    // Home-backed dir: GIO refuses to trash across filesystems like /tmp.
    let dir = glib::user_data_dir().join(format!("grab-torrent-del-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("gone.bin");
    std::fs::write(&file, b"bye").unwrap();

    let store = gio::ListStore::new::<DownloadItem>();
    let manager = DownloadManager::new(store.clone(), settings);
    let item = DownloadItem::new(
        7,
        "magnet:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3&dn=gone",
        "gone.bin",
        &dir.to_string_lossy(),
    );
    item.set_status(DownloadStatus::Done);
    store.append(&item);

    // Explicit delete trashes real files and drops the row; no session entry offline.
    let dbg = manager.delete_download(7);
    eprintln!(
        "DEBUG delete={:?} user_data={:?} exists={}",
        dbg.as_ref().err(),
        glib::user_data_dir(),
        dir.display()
    );
    assert!(dbg.is_ok());
    assert!(!file.exists());
    assert_eq!(store.n_items(), 0);
    // Undo the test's own Trash litter.
    let trash = glib::user_data_dir().join("Trash");
    let _ = std::fs::remove_file(trash.join("files/gone.bin"));
    let _ = std::fs::remove_file(trash.join("info/gone.bin.trashinfo"));

    // Multi-file torrents trash the whole subfolder.
    let sub = dir.join("Some Torrent");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("a.mp4"), b"data").unwrap();
    let item2 = DownloadItem::new(
        8,
        "magnet:?xt=urn:btih:b94a8fe5ccb19ba61c4c0873d391e987982fbbd4&dn=Some+Torrent",
        "Some Torrent",
        &dir.to_string_lossy(),
    );
    item2.set_status(DownloadStatus::Done);
    store.append(&item2);
    assert!(manager.delete_download(8).is_ok());
    assert!(!sub.exists());
    assert_eq!(store.n_items(), 0);
    let _ = std::fs::remove_dir_all(trash.join("files/Some Torrent"));
    let _ = std::fs::remove_file(trash.join("info/Some Torrent.trashinfo"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn remove_keeps_torrent_archive_for_undo() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("torrent-remove");
    let settings = test_settings();
    // Occupy the only slot so nothing spawns a real engine below.
    settings.set_int("max-concurrent", 1).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    let holder = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    manager.running.borrow_mut().insert(99, holder);
    let stem = format!("grab-undo-{}", std::process::id());
    let item = manager
        .enqueue_torrent_file(
            single_torrent_bytes(),
            &format!("{stem}.torrent"),
            None,
            None,
        )
        .unwrap();
    let id = item.id();
    let url = item.url().to_string();
    // Remove drops the row but keeps the archive so Undo can re-add the same URL.
    manager.remove(id);
    assert_eq!(manager.store().n_items(), 0);
    assert!(
        crate::torrent::archive_path_for_url(&url).is_some_and(|p| p.exists()),
        "remove must keep the archive for Undo"
    );
    // Teardown BEFORE restoring keys (see rejects_relative_download_dir).
    manager.cancel_all();
    crate::torrent::delete_archive_for_url(&url);
    assert!(crate::torrent::archive_path_for_url(&url).is_none());
    settings.set_int("max-concurrent", 3).unwrap();
}

#[test]
fn rejects_relative_download_dir() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("relative-dir");
    let settings = test_settings();
    // Occupy the only slot so nothing spawns a real engine below.
    settings.set_int("max-concurrent", 1).unwrap();
    settings.set_string("download-dir", "relative/dir").unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    let holder = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    manager.running.borrow_mut().insert(99, holder);
    // Configured relative dir falls back to an absolute folder.
    let dir = manager.effective_download_dir();
    assert!(std::path::Path::new(&dir).is_absolute());
    // Explicit relative destinations fall back the same way.
    let item = manager
        .enqueue("https://example.com/f.iso", Some("relative/dir"), None)
        .unwrap();
    assert!(item.dest_dir() == dir);
    // Absolute destinations still pass through untouched.
    let item2 = manager
        .enqueue("https://example.com/g.iso", Some("/tmp"), None)
        .unwrap();
    assert!(item2.dest_dir() == "/tmp");
    // Teardown BEFORE restoring keys or the watch spawns real engines for queued rows.
    manager.cancel_all();
    // Restore: the memory backend is shared across tests.
    settings.set_string("download-dir", "").unwrap();
    settings.set_int("max-concurrent", 3).unwrap();
    let _ = std::fs::remove_file(&_qf);
}

#[test]
fn normalizes_bare_hosts() {
    assert_eq!(
        normalize_url("example.com/f.iso").as_deref(),
        Ok("https://example.com/f.iso")
    );
    assert_eq!(
        normalize_url("  example.com  ").as_deref(),
        Ok("https://example.com/")
    );
    assert_eq!(
        normalize_url("http://example.com/f.iso").as_deref(),
        Ok("http://example.com/f.iso")
    );
    assert_eq!(
        normalize_url("localhost:8080/f.iso").as_deref(),
        Ok("https://localhost:8080/f.iso")
    );
    assert!(normalize_url("not a url").is_err());
    assert!(normalize_url("").is_err());
    assert_eq!(
        normalize_url("example .com").unwrap_err(),
        "Invalid URL: example .com"
    );
    let long = format!("https://example.com/{}", "a".repeat(MAX_URL_LEN));
    assert_eq!(
        normalize_url(&long).unwrap_err(),
        format!("URL is too long (max {MAX_URL_LEN} characters)")
    );
    let maxed = format!("https://example.com/{}", "a".repeat(MAX_URL_LEN - 20));
    assert!(normalize_url(&maxed).is_ok());
}

#[test]
fn delete_download_removes_file_and_row() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("del");
    let settings = test_settings();
    // Home-backed dir: GIO refuses to trash across filesystems like /tmp.
    let dir = glib::user_data_dir().join(format!("grab-del-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("gone.bin");
    std::fs::write(&file, b"bye").unwrap();

    let store = gio::ListStore::new::<DownloadItem>();
    let manager = DownloadManager::new(store.clone(), settings);
    let item = DownloadItem::new(
        7,
        "https://example.com/gone.bin",
        "gone.bin",
        &dir.to_string_lossy(),
    );
    item.set_status(DownloadStatus::Done);
    store.append(&item);

    assert!(manager.delete_download(7).is_ok());
    assert!(!file.exists());
    assert_eq!(store.n_items(), 0);
    // Undo the test's own Trash litter.
    let trash = glib::user_data_dir().join("Trash");
    let _ = std::fs::remove_file(trash.join("files/gone.bin"));
    let _ = std::fs::remove_file(trash.join("info/gone.bin.trashinfo"));
    let item2 = DownloadItem::new(
        8,
        "https://example.com/missing.bin",
        "missing.bin",
        &dir.to_string_lossy(),
    );
    item2.set_status(DownloadStatus::Done);
    store.append(&item2);
    assert!(manager.delete_download(8).is_ok());
    assert_eq!(store.n_items(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn magnet_delete_trashes_recorded_subfolder() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("magnet-subfolder");
    let settings = test_settings();
    // Occupy the only slot so nothing spawns a real engine below.
    settings.set_int("max-concurrent", 1).unwrap();
    // Home-backed dir: GIO refuses to trash across filesystems like /tmp.
    let dir = glib::user_data_dir().join(format!("grab-magnet-sub-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    settings
        .set_string("download-dir", &dir.to_string_lossy())
        .unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    let holder = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    manager.running.borrow_mut().insert(99, holder);
    let item = manager
        .enqueue(
            "magnet:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3&dn=gone",
            None,
            None,
        )
        .unwrap();
    // Enqueue records the engine subfolder: no archive exists to recompute it at delete time.
    let folder = dir.join("gone");
    assert_eq!(
        std::path::PathBuf::from(item.output_dir().to_string()),
        folder
    );
    // Simulate engine output: finished row renamed, payload inside the recorded subfolder.
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("real-name.bin"), b"data").unwrap();
    item.set_filename("real-name.bin");
    item.set_status(DownloadStatus::Done);
    assert!(manager.delete_download(item.id()).is_ok());
    assert!(!folder.exists());
    assert_eq!(manager.store().n_items(), 0);
    // Undo the test's own Trash litter.
    let trash = glib::user_data_dir().join("Trash");
    let _ = std::fs::remove_dir_all(trash.join("files/gone"));
    let _ = std::fs::remove_file(trash.join("info/gone.trashinfo"));
    // Teardown BEFORE restoring keys (see rejects_relative_download_dir).
    manager.cancel_all();
    let _ = std::fs::remove_dir_all(&dir);
    settings.set_int("max-concurrent", 3).unwrap();
}

#[test]
fn finish_cleans_untoggled_placeholders() {
    // Two-file archive ("a.txt", "sub/b.txt"), only the first kept.
    let stem = format!("grab-cleanup-{}", std::process::id());
    let pseudo =
        crate::torrent::archive_torrent_file(&format!("{stem}.torrent"), &multi_torrent_bytes())
            .unwrap();
    crate::torrent::stage_selection(&pseudo, vec![0]);
    // Fake engine output: kept file plus untoggled leftovers.
    let folder = std::env::temp_dir().join(format!("grab-cleanup-{stem}"));
    std::fs::create_dir_all(folder.join("sub")).unwrap();
    std::fs::write(folder.join("a.txt"), b"kept").unwrap();
    std::fs::write(folder.join("sub").join("b.txt"), b"").unwrap();
    crate::torrent::cleanup_unselected(&folder, &pseudo);
    assert!(folder.join("a.txt").exists());
    assert!(!folder.join("sub").exists());
    assert!(folder.exists());
    crate::torrent::delete_archive_for_url(&pseudo);
    let _ = std::fs::remove_dir_all(&folder);
}

#[test]
fn delete_download_trashes_torrent_subfolder() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("del-subfolder");
    let settings = test_settings();
    // Home-backed dir: GIO refuses to trash across filesystems like /tmp.
    let dir = glib::user_data_dir().join(format!("grab-delsub-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dest = dir.to_string_lossy().into_owned();
    // Meta name ("bar") differs from archive stem: folder is dest/<meta-name>/.
    let pseudo =
        crate::torrent::archive_torrent_file("mymeta.torrent", &multi_torrent_bytes()).unwrap();
    let folder = dir.join("bar");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("a.txt"), b"hi").unwrap();

    let store = gio::ListStore::new::<DownloadItem>();
    let manager = DownloadManager::new(store.clone(), settings);
    let item = DownloadItem::new(9, &pseudo, "mymeta", &dest);
    item.set_status(DownloadStatus::Done);
    store.append(&item);

    assert!(manager.delete_download(9).is_ok());
    assert!(!folder.exists());
    assert!(crate::torrent::archive_path_for_url(&pseudo).is_none());
    assert_eq!(store.n_items(), 0);
    // Undo the test's own Trash litter.
    let trash = glib::user_data_dir().join("Trash");
    let _ = std::fs::remove_dir_all(trash.join("files/bar"));
    let _ = std::fs::remove_file(trash.join("info/bar.trashinfo"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn torrent_folder_delete_path_never_follows_a_planted_link() {
    // The delete/trash path recomputes the engine's folder from the archive:
    // a symlink planted at that leaf must dedupe to a fresh folder, never
    // resolve through the link onto its target.
    let stem = format!("grab-torrentguard-{}", std::process::id());
    let pseudo =
        crate::torrent::archive_torrent_file(&format!("{stem}.torrent"), &multi_torrent_bytes())
            .unwrap();
    let base = std::env::temp_dir().join(format!("grab-torrentguard-{stem}"));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let dest = base.to_string_lossy().into_owned();

    let outside = std::env::temp_dir().join(format!("grab-torrentguard-out-{stem}"));
    let _ = std::fs::remove_dir_all(&outside);
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("precious.txt"), b"do not touch").unwrap();
    // Meta name is "bar": the recomputed leaf is <dest>/bar.
    let link = base.join("bar");
    std::os::unix::fs::symlink(&outside, &link).unwrap();

    // output_dir left empty: forces the recompute path, like rows whose
    // folder was never recorded.
    let item = DownloadItem::new(10, &pseudo, "mymeta", &dest);
    assert!(item.output_dir().is_empty());
    let folder = DownloadManager::torrent_folder(&item);
    assert_ne!(
        folder, link,
        "the delete path must not resolve through the planted link"
    );
    assert!(
        folder.is_dir()
            && !std::fs::symlink_metadata(&folder)
                .unwrap()
                .file_type()
                .is_symlink(),
        "the delete path lands in a fresh real dir, not the link: {folder:?}"
    );
    assert!(outside.join("precious.txt").exists());
    assert!(outside.read_dir().unwrap().count() == 1);

    crate::torrent::delete_archive_for_url(&pseudo);
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&outside);
}

#[test]
fn dedupes() {
    let taken = |n: &str| matches!(n, "f.iso" | "f (1).iso");
    assert_eq!(dedupe_filename("g.iso", taken), "g.iso");
    assert_eq!(dedupe_filename("f.iso", taken), "f (2).iso");
    assert_eq!(dedupe_filename("README", taken), "README");
    let taken = |n: &str| n == "README";
    assert_eq!(dedupe_filename("README", taken), "README (1)");
    let taken = |_: &str| false;
    assert_eq!(dedupe_filename(".profile", taken), ".profile");
    assert_eq!(dedupe_filename("a.tar.gz", taken), "a.tar.gz");
}

#[test]
fn sanitize_folder_name_strips_unsafe_characters() {
    assert_eq!(
        crate::file_names::sanitize_folder_name("My Playlist: Best/Of"),
        "My Playlist: Best_Of"
    );
    assert_eq!(
        crate::file_names::sanitize_folder_name("a\\b\u{0}c\u{202e}d"),
        "a_b_c_d"
    );
    assert_eq!(crate::file_names::sanitize_folder_name("a__b"), "a_b");
    assert_eq!(crate::file_names::sanitize_folder_name("..."), "collection");
    assert_eq!(crate::file_names::sanitize_folder_name("///"), "collection");
    assert_eq!(crate::file_names::sanitize_folder_name("Mix"), "Mix");
}

#[test]
fn collection_subdir_reuses_titled_folder() {
    let base = std::env::temp_dir().join(format!("grab-collsub-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let base_s = base.to_string_lossy().into_owned();
    let first = crate::file_names::collection_subdir(&base_s, "My Mix: Vol. 1/Deluxe");
    let second = crate::file_names::collection_subdir(&base_s, "My Mix: Vol. 1/Deluxe");
    assert_eq!(first, second);
    assert!(std::path::Path::new(&first).is_dir());
    assert!(first.ends_with("My Mix: Vol. 1_Deluxe"));
    let _ = std::fs::remove_dir_all(&base);
}

#[cfg(unix)]
#[test]
fn collection_subdir_dodges_planted_symlink() {
    // A pre-existing symlink with the collection name must not be followed:
    // `create_dir_all` would happily create through it, redirecting downloads
    // outside the download dir.
    let base = std::env::temp_dir().join(format!("grab-collsym-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let target = base.join("elsewhere");
    std::fs::create_dir_all(&target).unwrap();
    std::os::unix::fs::symlink(&target, base.join("My Mix")).unwrap();
    let got = crate::file_names::collection_subdir(&base.to_string_lossy(), "My Mix");
    assert!(got.ends_with("My Mix (1)"), "unexpected: {got}");
    assert!(std::path::Path::new(&got).is_dir());
    // Nothing was created through the link.
    assert!(!target.join("My Mix").exists());
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn collection_subdir_dodges_stray_file() {
    // A non-dir squatter with the collection name used to make `create_dir_all`
    // fail silently and hand back an unusable path; now it dedupes instead.
    let base = std::env::temp_dir().join(format!("grab-collfile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(base.join("My Mix"), b"squatter").unwrap();
    let got = crate::file_names::collection_subdir(&base.to_string_lossy(), "My Mix");
    assert!(got.ends_with("My Mix (1)"), "unexpected: {got}");
    assert!(std::path::Path::new(&got).is_dir());
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn enqueue_keeps_name_when_file_exists() {
    // Parabolic-style: an on-disk file never renames the intake. The row keeps
    // its natural name and the engine refuses it at spawn; nothing is removed.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("keep-name-dup");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::env::temp_dir().join(format!("grab-keepdup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("dup.bin"), b"someone else").unwrap();
    let dest_s = dest.to_string_lossy().into_owned();
    let item = manager
        .enqueue(
            "https://example.com/dup.bin",
            Some(&dest_s),
            Some("dup.bin"),
        )
        .expect("enqueue");
    assert_eq!(item.filename(), "dup.bin");
    // insert -> start_next -> spawn run synchronously: the fresh row refuses the foreign file.
    assert_eq!(item.status(), DownloadStatus::Failed);
    assert_eq!(item.detail(), crate::engine_msg::DEST_EXISTS);
    assert_eq!(
        std::fs::read(dest.join("dup.bin")).unwrap(),
        b"someone else"
    );
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn enqueue_video_keeps_name_when_file_exists() {
    // Video intake likewise never renames for an on-disk file; the runner's
    // pre-flight refuses the existing destination at start.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("keep-name-video-dup");
    let _notools = NoVideoTools::apply();
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::env::temp_dir().join(format!("grab-keepviddup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("clip.mp4"), b"someone else").unwrap();
    let dest_s = dest.to_string_lossy().into_owned();
    let item = manager
        .enqueue_video(
            "https://www.youtube.com/watch?v=gXtp6C-3JKo",
            Some(&dest_s),
            Some("clip.mp4"),
            crate::media_types::VideoChoices {
                quality: "1080p".to_string(),
                audio_only: false,
                video_format_id: None,
                is_live: false,
                playlist_item_id: None,
            },
        )
        .expect("video enqueue");
    assert_eq!(item.filename(), "clip.mp4");
    assert_eq!(
        std::fs::read(dest.join("clip.mp4")).unwrap(),
        b"someone else"
    );
    // Let the tools-missing failure land so no worker outlives the test.
    drain_engine(&manager, item.id());
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn restored_unstarted_row_refuses_existing_dest() {
    // A queue-file row the engine never started is foreign to any bytes at
    // dest, exactly like a fresh intake: it refuses instead of resuming.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("refuse-restored");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::env::temp_dir().join(format!("grab-refuserest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("foreign.bin"), b"not ours").unwrap();
    let dest_s = dest.to_string_lossy().into_owned();
    let mut stored = stored_row(
        "https://example.com/foreign.bin",
        &dest_s,
        "foreign.bin",
        DownloadStatus::Downloading,
    );
    // Explicit never-started (new queue file): the bytes are foreign.
    stored.started = Some(false);
    let item = manager.restore_existing(&stored).expect("restore");
    assert_eq!(item.status(), DownloadStatus::Failed);
    assert_eq!(item.detail(), crate::engine_msg::DEST_EXISTS);
    assert_eq!(
        std::fs::read(dest.join("foreign.bin")).unwrap(),
        b"not ours"
    );
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn legacy_queue_row_without_started_resumes_partial() {
    // Queue files from before `started` existed carry None: the legacy
    // resume-anything-with-bytes behavior applies, so a pre-upgrade partial
    // is not refused as foreign.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("resume-legacy");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::env::temp_dir().join(format!("grab-resumeleg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("partial.bin"), b"ours").unwrap();
    let dest_s = dest.to_string_lossy().into_owned();
    // `stored_row` leaves `started` as None: a legacy queue file.
    let stored = stored_row(
        "https://example.com/partial.bin",
        &dest_s,
        "partial.bin",
        DownloadStatus::Downloading,
    );
    assert!(stored.started.is_none());
    let item = manager.restore_existing(&stored).expect("restore");
    assert_ne!(
        item.detail(),
        crate::engine_msg::DEST_EXISTS,
        "legacy partial must resume, not refuse"
    );
    manager.cancel(item.id());
    quiesce(&glib::MainContext::default());
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn unremove_restores_started_for_single_connection_partial() {
    // Undo of a removed single-connection partial must keep the row's
    // `started` mark under its new id: the bytes at dest are its own
    // partial, so the spawn gate resumes them instead of failing the row
    // at DEST_EXISTS as a foreign file (which no retry could escape).
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("unremove-started");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::env::temp_dir().join(format!("grab-unremstart-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("partial.bin"), b"ours").unwrap();
    let dest_s = dest.to_string_lossy().into_owned();
    // A row the engine started (single-connection: no segment bitmap).
    // Inserted Paused so nothing spawns before the snapshot; flipped to
    // Downloading to mirror a live row at remove time.
    let id = manager.alloc_id_for(std::path::Path::new(&dest_s));
    let item = DownloadItem::new(
        id,
        "https://example.com/partial.bin",
        "partial.bin",
        &dest_s,
    );
    item.set_status(DownloadStatus::Paused);
    let item = manager.insert(item);
    item.set_status(DownloadStatus::Downloading);
    manager.started.borrow_mut().insert(id);
    // Snapshot exactly like the remove-button handler, then remove.
    let snap = RemovedSnapshot {
        url: item.url().to_string(),
        dest_dir: item.dest_dir().to_string(),
        filename: item.filename().to_string(),
        status: item.status(),
        progress: item.progress(),
        detail: item.detail().to_string(),
        output_dir: item.output_dir().to_string(),
        segments: manager.segments_of(id),
        video_source: manager.video_source(id),
        started: manager.has_started(id),
    };
    manager.remove(id);
    assert!(
        !manager.has_started(id),
        "remove must drop the old id's mark"
    );
    // Undo re-inserts as Queued and the scheduler spawns synchronously.
    let restored = manager.unremove(snap);
    assert!(
        manager.has_started(restored.id()),
        "unremove dropped `started`: the restored row's own partial fails at DEST_EXISTS"
    );
    assert_ne!(
        restored.detail(),
        crate::engine_msg::DEST_EXISTS,
        "restored single-connection partial was refused as a foreign file"
    );
    // Let the resumed attempt land so no worker outlives the test.
    manager.cancel(restored.id());
    quiesce(&glib::MainContext::default());
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn expand_playlist_rows_use_titled_subfolder() {
    // Multi-item collections land in a titled subfolder, torrent-style; the
    // folder is reused, never suffixed, so repeats hit the new dedup errors.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("pl-subfolder");
    let _notools = NoVideoTools::apply();
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::env::temp_dir().join(format!("grab-plsub-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    let dest_s = dest.to_string_lossy().into_owned();
    let parent = manager
        .enqueue_video(
            "https://www.youtube.com/playlist?list=PLx",
            Some(&dest_s),
            Some("parent.mp4"),
            crate::media_types::VideoChoices {
                quality: "1080p".to_string(),
                audio_only: false,
                video_format_id: None,
                is_live: false,
                playlist_item_id: None,
            },
        )
        .expect("parent enqueue");
    // Stage a Page source so the expansion reads quality/audio_only.
    manager.video_sources.borrow_mut().insert(
        parent.id(),
        crate::media_types::VideoSource::Page {
            page_url: "https://www.youtube.com/playlist?list=PLx".to_string(),
            media_url: None,
            expires_at: None,
            quality: "1080p".to_string(),
            audio_only: false,
            is_live: false,
            video_format_id: None,
            playlist_item_id: None,
        },
    );
    let pl = crate::media_types::PlaylistInfo {
        id: "PLx".into(),
        title: "My Mix: Vol. 1".into(),
        page_url: "https://www.youtube.com/playlist?list=PLx".into(),
        kind: crate::media_types::PlaylistKind::Playlist,
        total: 2,
        items: vec![
            crate::media_types::PlaylistItem {
                index: 1,
                id: "a".into(),
                title: "First".into(),
                page_url: "https://www.youtube.com/watch?v=aaa".to_string(),
                duration: None,
            },
            crate::media_types::PlaylistItem {
                index: 2,
                id: "b".into(),
                title: "Second".into(),
                page_url: "https://www.youtube.com/watch?v=bbb".to_string(),
                duration: None,
            },
        ]
        .into_boxed_slice(),
    };
    let (added, total) = manager.expand_playlist_rows(parent.id(), &parent, &pl);
    assert_eq!((added, total), (2, 2));
    let sub = dest.join("My Mix: Vol. 1");
    assert!(sub.is_dir());
    let ids: Vec<u64> = manager.items().map(|it| it.id()).collect();
    for it in manager.items() {
        if it.id() == parent.id() {
            continue;
        }
        assert_eq!(it.dest_dir(), sub.to_string_lossy());
    }
    // Let the tools-missing failures land so no worker outlives the test.
    for id in ids {
        drain_engine(&manager, id);
    }
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn shortens_long_filenames() {
    assert_eq!(shorten_filename("short.mp4"), "short.mp4");
    let long = format!("{}.mp4", "a".repeat(300));
    let short = shorten_filename(&long);
    assert!(short.len() <= 240);
    assert!(short.ends_with(".mp4"));
    let wide = format!("{}.mp4", "é".repeat(200));
    let short = shorten_filename(&wide);
    assert!(short.len() <= 240);
    assert!(short.ends_with(".mp4"));
    let no_ext = "b".repeat(300);
    assert!(shorten_filename(&no_ext).len() <= 240);
    // Oversize extension (>= 240 bytes): dropped, stem truncated. Without
    // this, the result would be the extension alone (> 255 bytes → ENAMETOOLONG).
    let huge_ext = format!("file.{}", "x".repeat(300));
    let short = shorten_filename(&huge_ext);
    assert!(
        short.len() <= 240,
        "oversize extension must not produce ENAMETOOLONG"
    );
    assert!(
        !short.contains('x'),
        "pathological extension must be dropped"
    );
}

#[test]
fn restrict_filename_ascii_folds_to_ascii() {
    // Accented Latin folds to base; spaces/punctuation become "_"; extension survives.
    assert_eq!(
        restrict_filename_ascii("Café & Croissants.mp4"),
        "Cafe_Croissants.mp4"
    );
    assert_eq!(
        restrict_filename_ascii("naïve façade — 50%.mkv"),
        "naive_facade_50.mkv"
    );
    assert_eq!(
        restrict_filename_ascii("Straße æther œil.mp4"),
        "Strasse_aether_oeil.mp4"
    );
    // Plain ASCII names pass through untouched.
    assert_eq!(
        restrict_filename_ascii("my-video_01.final.mp4"),
        "my-video_01.final.mp4"
    );
    // Runs of "_" collapse; leading/trailing "_" are stripped.
    assert_eq!(restrict_filename_ascii("a  b.mp4"), "a_b.mp4");
    assert_eq!(restrict_filename_ascii(" _x_ .mp4"), "x.mp4");
    // Quotes and control characters are dropped, not underscored.
    assert_eq!(restrict_filename_ascii("a\"b.mp4"), "ab.mp4");
    // A stem that folds to nothing falls back instead of going empty.
    assert_eq!(restrict_filename_ascii("日本語"), "file");
    assert_eq!(restrict_filename_ascii("日本語.mp4"), "file.mp4");
}

#[test]
fn restore_keeps_exact_filename() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("restore-exact");
    test_settings();
    let dir = std::env::temp_dir().join(format!("grab-restore-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dir_s = dir.to_string_lossy().into_owned();
    std::fs::write(dir.join("ubuntu.iso"), b"partial").unwrap();
    let settings = test_settings();
    settings.set_int("max-concurrent", 1).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let holder = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    manager.running.borrow_mut().insert(99, holder);
    let item = manager
        .restore_existing(&stored_row(
            "https://example.com/ubuntu.iso",
            &dir_s,
            "ubuntu.iso",
            DownloadStatus::Downloading,
        ))
        .unwrap();
    assert_eq!(item.filename(), "ubuntu.iso");
    assert_eq!(item.status(), DownloadStatus::Queued);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn queue_roundtrip_and_mapping() {
    // Old queue files use the same lowercase status words.
    let legacy: StoredQueue = serde_json::from_str(
        r#"{"version":2,"items":[{"url":"https://example.com/c.iso","dest_dir":"/tmp","filename":"c.iso","status":"done"}]}"#,
    )
    .unwrap();
    assert_eq!(legacy.items[0].status, DownloadStatus::Done);
    let q = StoredQueue {
        version: QUEUE_VERSION,
        items: vec![
            StoredItem {
                id: None,
                url: "https://example.com/a.iso".to_string(),
                dest_dir: "/tmp/dl".to_string(),
                filename: "a.iso".to_string(),
                status: DownloadStatus::Queued,
                progress: 0.0,
                segments: None,
                selected_files: None,
                output_dir: None,
                video_source: None,
                started: None,
                scheduled_at: None,
                etag: None,
                last_modified: None,
            },
            StoredItem {
                id: None,
                url: "https://example.com/b.iso".to_string(),
                dest_dir: "/tmp/dl".to_string(),
                filename: "b.iso".to_string(),
                status: DownloadStatus::Done,
                progress: 1.0,
                segments: None,
                selected_files: None,
                output_dir: None,
                video_source: None,
                started: None,
                scheduled_at: None,
                etag: None,
                last_modified: None,
            },
        ],
    };
    let text = serde_json::to_string(&q).unwrap();
    let back: StoredQueue = serde_json::from_str(&text).unwrap();
    assert_eq!(back.version, QUEUE_VERSION);
    assert_eq!(back.items.len(), 2);
    assert_eq!(back.items[1].filename, "b.iso");
    let legacy = r#"{"version":1,"items":[{"url":"https://example.com/c.iso","dest_dir":"/tmp","filename":"c.iso","status":"done"}]}"#;
    let back: StoredQueue = serde_json::from_str(legacy).unwrap();
    assert!((back.items[0].progress - 0.0).abs() < f64::EPSILON);
}

#[test]
fn history_restore_roundtrip() {
    // Holds MAIN_LOOP_LOCK (via test_locks): restoring the Done row spawns a
    // thread-bound sizing future that must be drained before this test ends.
    let (_lock, _loop) = test_locks();
    let qf = test_queue_file("history");
    let settings = test_settings();
    let m1 = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    let done = DownloadItem::new(1, "https://example.com/old.iso", "old.iso", "/tmp/dl");
    done.set_progress(1.0);
    done.set_status(DownloadStatus::Done);
    done.set_detail("Finished".to_string());
    m1.store().append(&done);
    m1.persist_queue();
    assert!(qf.exists());

    let m2 = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    m2.restore_queue();
    assert_eq!(m2.store().n_items(), 1);
    let it = m2.store().item(0).and_downcast::<DownloadItem>().unwrap();
    assert_eq!(it.filename(), "old.iso");
    assert_eq!(it.status(), DownloadStatus::Done);
    assert!((it.progress() - 1.0).abs() < f64::EPSILON);
    // Atomic persist leaves no tmp debris behind: temp files are now
    // `queue.json.tmp.<pid>.<n>`, so scan the directory for the prefix.
    let qf_dir = qf.parent().unwrap();
    let qf_name = qf.file_name().unwrap().to_str().unwrap();
    let tmp_prefix = format!("{qf_name}.tmp.");
    assert!(
        !std::fs::read_dir(qf_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e
                .file_name()
                .to_str()
                .is_some_and(|n| n.starts_with(&tmp_prefix))),
        "tmp debris left behind"
    );
    // Drain the restored Done row's async sizing so no thread-bound future
    // outlives this test (glib thread guard, serial suite shares one context).
    drain_sizing(&m2);
    let _ = std::fs::remove_file(&qf);
}

#[test]
fn selection_survives_persist_restore() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let qf = test_queue_file("selection-roundtrip");
    let settings = test_settings();
    settings.set_int("max-concurrent", 1).unwrap();
    let m1 = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    // Occupy the only slot so nothing spawns a real engine below.
    let holder = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    m1.running.borrow_mut().insert(99, holder);
    // Archive + stage a selection like the intake dialog does.
    let pseudo =
        crate::torrent::archive_torrent_file("keep.torrent", &single_torrent_bytes()).unwrap();
    crate::torrent::stage_selection(&pseudo, vec![0]);
    let item = m1.enqueue(&pseudo, Some("/tmp/dl"), Some("keep")).unwrap();
    assert_eq!(item.status(), DownloadStatus::Queued);
    m1.persist_queue();
    // The queue file carries the selection...
    let text = std::fs::read_to_string(&qf).unwrap();
    let queue: StoredQueue = serde_json::from_str(&text).unwrap();
    assert_eq!(queue.items.len(), 1);
    assert_eq!(queue.items[0].selected_files, Some(vec![0]));
    // ...and restore re-stages it. Prune first to simulate restart wiping the map.
    crate::torrent::prune_selections(&std::collections::HashSet::new());
    let m2 = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    let holder2 = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    m2.running.borrow_mut().insert(99, holder2);
    m2.restore_queue();
    assert_eq!(m2.store().n_items(), 1);
    assert_eq!(crate::torrent::get_selection(&pseudo), Some(vec![0]));
    // Teardown: leave no Queued row behind and restore shared keys.
    m1.cancel_all();
    m2.cancel_all();
    crate::torrent::delete_archive_for_url(&pseudo);
    settings.set_int("max-concurrent", 3).unwrap();
    let _ = std::fs::remove_file(&qf);
}

#[test]
fn sane_filenames() {
    assert!(sane_filename("a.iso"));
    assert!(sane_filename("my file (1).tar.gz"));
    // Backslash is rejected for consistency with `basename()` in
    // download_fetch.rs, which treats it as a directory separator.
    assert!(!sane_filename("a\\b"));
    assert!(!sane_filename(""));
    assert!(!sane_filename("."));
    assert!(!sane_filename(".."));
    assert!(!sane_filename("/etc/passwd"));
    assert!(!sane_filename("a/b"));
    assert!(!sane_filename("a\0b"));
    // Control/bidi chars deceive in listings; arrive via Content-Disposition decoding.
    assert!(!sane_filename("a\nb.mp4"));
    assert!(!sane_filename("a\tb.mp4"));
    assert!(!sane_filename("evil\u{202e}mp4.txt"));
}

#[test]
fn restore_rejects_bad_filenames() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("restore-bad");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    for bad in ["/etc/passwd", "..", ".", "a/b", ""] {
        assert!(
            manager
                .restore_existing(&stored_row(
                    "https://example.com/f.iso",
                    "/tmp/dl",
                    bad,
                    DownloadStatus::Queued
                ))
                .is_err()
        );
    }
    assert!(
        manager
            .restore_existing(&stored_row(
                "https://example.com/f.iso",
                "relative/dir",
                "f.iso",
                DownloadStatus::Queued
            ))
            .is_err()
    );
    assert_eq!(manager.store().n_items(), 0);
}

#[test]
fn batch_restore_hundred_done() {
    let (_lock, _loop) = test_locks();
    let qf = test_queue_file("batch");
    let settings = test_settings();
    let items: Vec<StoredItem> = (0..100)
        .map(|i| StoredItem {
            id: None,
            url: format!("https://example.com/f{i}.iso"),
            dest_dir: "/tmp/dl".to_string(),
            filename: format!("f{i}.iso"),
            status: DownloadStatus::Done,
            progress: 1.0,
            segments: None,
            selected_files: None,
            output_dir: None,
            video_source: None,
            started: None,
            scheduled_at: None,
            etag: None,
            last_modified: None,
        })
        .collect();
    let queue = StoredQueue {
        version: QUEUE_VERSION,
        items,
    };
    std::fs::write(&qf, serde_json::to_string(&queue).unwrap()).unwrap();
    let m = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    m.restore_queue();
    assert_eq!(m.store().n_items(), 100);
    let it = m.store().item(99).and_downcast::<DownloadItem>().unwrap();
    assert_eq!(it.filename(), "f99.iso");
    assert_eq!(it.status(), DownloadStatus::Done);
    let _ = std::fs::remove_file(&qf);
    // Drain the 100 sizing futures on this thread: a later test on another
    // thread would trip glib's thread guard reaping them (see MAIN_LOOP_LOCK).
    drain_sizing(&m);
}

#[test]
fn cancel_frees_slot_immediately() {
    let (_lock, _loop) = test_locks();
    let _qf = test_queue_file("cancel-slot");
    let settings = test_settings();
    settings.set_int("max-concurrent", 1).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let a = DownloadItem::new(41, "https://example.com/a.bin", "a.bin", "/tmp/dl");
    a.set_status(DownloadStatus::Downloading);
    manager.store().append(&a);
    let holder = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    manager.running.borrow_mut().insert(41, holder);
    let b = DownloadItem::new(42, "http://127.0.0.1:9/b.bin", "b.bin", "/tmp/dl");
    manager.store().append(&b);
    manager.cancel(41);
    assert_eq!(a.status(), DownloadStatus::Cancelled);
    assert!(!manager.running.borrow().contains_key(&41));
    assert_eq!(b.status(), DownloadStatus::Downloading);
    assert!(manager.running.borrow().contains_key(&42));
    // Drain B's UI future here: already queued, or another test's loop aborts on thread-affinity.
    let ctx = glib::MainContext::default();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while manager.running.borrow().contains_key(&42) && std::time::Instant::now() < deadline {
        ctx.iteration(false);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(!manager.running.borrow().contains_key(&42));
    assert_eq!(b.status(), DownloadStatus::Failed);
}

#[test]
fn pause_starts_next_queued() {
    let (_lock, _loop) = test_locks();
    let _qf = test_queue_file("pause-next");
    let settings = test_settings();
    settings.set_int("max-concurrent", 1).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let a = DownloadItem::new(61, "https://example.com/a.bin", "a.bin", "/tmp/dl");
    a.set_status(DownloadStatus::Downloading);
    manager.store().append(&a);
    let holder = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    manager.running.borrow_mut().insert(61, holder);
    let b = DownloadItem::new(62, "http://127.0.0.1:9/b.bin", "b.bin", "/tmp/dl");
    manager.store().append(&b);
    manager.pause(61);
    assert_eq!(a.status(), DownloadStatus::Paused);
    assert!(!manager.running.borrow().contains_key(&61));
    assert_eq!(b.status(), DownloadStatus::Downloading);
    assert!(manager.running.borrow().contains_key(&62));
    let ctx = glib::MainContext::default();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while manager.running.borrow().contains_key(&62) && std::time::Instant::now() < deadline {
        ctx.iteration(false);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(!manager.running.borrow().contains_key(&62));
}

#[test]
fn raising_max_concurrent_starts_queued() {
    let (_lock, _loop) = test_locks();
    let _qf = test_queue_file("concurrent-bump");
    let settings = test_settings();
    settings.set_int("max-concurrent", 1).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    let a = DownloadItem::new(63, "https://example.com/a.bin", "a.bin", "/tmp/dl");
    a.set_status(DownloadStatus::Downloading);
    manager.store().append(&a);
    let holder = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    manager.running.borrow_mut().insert(63, holder);
    let b = DownloadItem::new(64, "http://127.0.0.1:9/b.bin", "b.bin", "/tmp/dl");
    manager.store().append(&b);
    assert_eq!(b.status(), DownloadStatus::Queued);
    // The preferences SpinRow writes this key; the queued row must start with no other event.
    settings.set_int("max-concurrent", 2).unwrap();
    assert_eq!(b.status(), DownloadStatus::Downloading);
    assert!(manager.running.borrow().contains_key(&64));
    let ctx = glib::MainContext::default();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while manager.running.borrow().contains_key(&64) && std::time::Instant::now() < deadline {
        ctx.iteration(false);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(!manager.running.borrow().contains_key(&64));
    // Restore: the memory backend is shared across tests.
    settings.set_int("max-concurrent", 3).unwrap();
}

#[test]
fn defer_yields_slot_and_goes_last() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("defer");
    let settings = test_settings();
    settings.set_int("max-concurrent", 1).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    // Both slots busy with stand-ins: defer spawns no real engine, stays synchronous.
    for (id, progress) in [(71, 0.5), (72, 0.0)] {
        let it = DownloadItem::new(id, "https://example.com/f.bin", "f.bin", "/tmp/dl");
        it.set_status(DownloadStatus::Downloading);
        it.set_progress(progress);
        manager.store().append(&it);
        let holder = tokio_rt().spawn(async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        manager.running.borrow_mut().insert(id, holder);
    }
    manager.defer(71);
    // Progress kept, row parked behind the other one, slot count kept.
    let a = manager.find(71).unwrap();
    assert_eq!(a.status(), DownloadStatus::Queued);
    assert_eq!(a.progress(), 0.5);
    let order: Vec<u64> = (0..manager.store.n_items())
        .filter_map(|i| manager.store().item(i).and_downcast::<DownloadItem>())
        .map(|it| it.id())
        .collect();
    assert_eq!(order, vec![72, 71]);
    assert!(manager.running.borrow().contains_key(&72));
    assert!(!manager.running.borrow().contains_key(&71));
    // Cancel first so no queued row remains for the backend restore to spawn.
    manager.cancel_all();
    assert!(!manager.running.borrow().contains_key(&72));
    settings.set_int("max-concurrent", 3).unwrap();
}

#[test]
fn lowering_max_concurrent_parks_newest() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("preempt");
    let settings = test_settings();
    settings.set_int("max-concurrent", 2).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    for id in [81, 82] {
        let it = DownloadItem::new(id, "https://example.com/f.bin", "f.bin", "/tmp/dl");
        it.set_status(DownloadStatus::Downloading);
        manager.store().append(&it);
        let holder = tokio_rt().spawn(async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        manager.running.borrow_mut().insert(id, holder);
    }
    let c = DownloadItem::new(83, "https://example.com/g.bin", "g.bin", "/tmp/dl");
    manager.store().append(&c);
    // Newest running row parks itself with no other queue event.
    settings.set_int("max-concurrent", 1).unwrap();
    assert_eq!(
        manager.find(81).unwrap().status(),
        DownloadStatus::Downloading
    );
    assert_eq!(manager.find(82).unwrap().status(), DownloadStatus::Queued);
    assert!(manager.running.borrow().contains_key(&81));
    assert!(!manager.running.borrow().contains_key(&82));
    // Leave no queued row behind or the backend restore spawns a real engine for it.
    manager.cancel_all();
    assert!(manager.running.borrow().is_empty());
    settings.set_int("max-concurrent", 3).unwrap();
}

#[test]
fn restore_preserves_intent() {
    // Holds MAIN_LOOP_LOCK (via test_locks): restoring the Done row spawns a
    // thread-bound sizing future that must be drained before this test ends.
    let (_lock, _loop) = test_locks();
    let qf = test_queue_file("intent");
    let settings = test_settings();
    let items: Vec<StoredItem> = [
        ("p.iso", DownloadStatus::Paused),
        ("f.iso", DownloadStatus::Failed),
        ("d.iso", DownloadStatus::Done),
    ]
    .into_iter()
    .map(|(f, status)| StoredItem {
        id: None,
        url: format!("https://example.com/{f}"),
        dest_dir: "/tmp/dl".to_string(),
        filename: f.to_string(),
        status,
        progress: 0.5,
        segments: None,
        selected_files: None,
        output_dir: None,
        video_source: None,
        // None: this round-trips a legacy queue file through restore.
        started: None,
        scheduled_at: None,
        etag: None,
        last_modified: None,
    })
    .collect();
    let queue = StoredQueue {
        version: QUEUE_VERSION,
        items,
    };
    std::fs::write(&qf, serde_json::to_string(&queue).unwrap()).unwrap();
    let m = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    m.restore_queue();
    assert_eq!(m.store().n_items(), 3);
    assert!(m.running.borrow().is_empty());
    let status_of = |name: &str| {
        (0..m.store().n_items())
            .filter_map(|i| m.store().item(i).and_downcast::<DownloadItem>())
            .find(|it| it.filename() == name)
            .map(|it| it.status())
    };
    assert_eq!(status_of("p.iso"), Some(DownloadStatus::Paused));
    assert_eq!(status_of("f.iso"), Some(DownloadStatus::Failed));
    assert_eq!(status_of("d.iso"), Some(DownloadStatus::Done));
    // Drain the restored Done row's async sizing so no thread-bound future
    // outlives this test (glib thread guard, serial suite shares one context).
    drain_sizing(&m);
    let _ = std::fs::remove_file(&qf);
}

#[test]
fn restored_status_mapping() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("restored-mapping");
    let settings = test_settings();
    // Occupy the only slot so restored rows never spawn an engine.
    settings.set_int("max-concurrent", 1).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let holder = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    manager.running.borrow_mut().insert(99, holder);
    // Settled rows keep their status; anything resumable requeues.
    for (stored, expected) in [
        (DownloadStatus::Paused, DownloadStatus::Paused),
        (DownloadStatus::Failed, DownloadStatus::Failed),
        (DownloadStatus::Done, DownloadStatus::Done),
        (DownloadStatus::Queued, DownloadStatus::Queued),
        (DownloadStatus::Downloading, DownloadStatus::Queued),
    ] {
        let item = manager
            .restore_existing(&stored_row(
                "https://example.com/m.iso",
                "/tmp/dl",
                "m.iso",
                stored,
            ))
            .unwrap();
        assert_eq!(item.status(), expected);
        manager.remove(item.id());
    }
    manager.cancel_all();
}

#[test]
fn retry_persists_immediately() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let qf = test_queue_file("retry-persist");
    let settings = test_settings();
    settings.set_int("max-concurrent", 1).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let item = DownloadItem::new(51, "https://example.com/r.bin", "r.bin", "/tmp/dl");
    item.set_status(DownloadStatus::Cancelled);
    manager.store().append(&item);
    let holder = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    manager.running.borrow_mut().insert(52, holder);
    manager.retry(51);
    assert_eq!(item.status(), DownloadStatus::Queued);
    assert_eq!(manager.running.borrow().len(), 1);
    let text = std::fs::read_to_string(&qf).unwrap();
    let queue: StoredQueue = serde_json::from_str(&text).unwrap();
    assert_eq!(queue.items.len(), 1);
    assert_eq!(queue.items[0].filename, "r.bin");
    assert_eq!(queue.items[0].status, DownloadStatus::Queued);
    let _ = std::fs::remove_file(&qf);
}

#[test]
fn manager_pause_resume_cancel() {
    let (_lock, _loop) = test_locks();
    let _qf = test_queue_file("lifecycle");
    let settings = test_settings();

    let dir = std::env::temp_dir().join(format!("grab-test-{}", std::process::id()));
    let srv = dir.join("srv");
    let dl = dir.join("dl");
    std::fs::create_dir_all(&srv).unwrap();
    std::fs::create_dir_all(&dl).unwrap();
    let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(srv.join("t.bin"), &payload).unwrap();

    let port = test_port(0);
    let server_log = dir.join("server.log");
    let server_log_file = std::fs::File::create(&server_log).unwrap();
    let ranges_log = dir.join("ranges.log");
    let server_py = format!("{}/tests/throttled_server.py", env!("CARGO_MANIFEST_DIR"));
    let server = std::process::Command::new("python3")
        .arg(&server_py)
        .arg(port.to_string())
        .arg(srv.join("t.bin"))
        .arg(&ranges_log)
        .stdout(std::process::Stdio::null())
        .stderr(server_log_file)
        .spawn()
        .expect("python3 throttled server");
    let mut ready = false;
    for _ in 0..100 {
        if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(ready, "test HTTP server did not listen on port {port}");

    settings.set_boolean("show-notifications", false).unwrap();

    let store = gio::ListStore::new::<DownloadItem>();
    let manager = DownloadManager::new(store, settings.clone());
    let url = format!("http://127.0.0.1:{port}/t.bin");
    let dest = dl.to_string_lossy().into_owned();

    let main_loop = glib::MainLoop::new(None, false);
    let quit = main_loop.clone();
    let server = Rc::new(RefCell::new(server));
    glib::MainContext::default().spawn_local(async move {
        let server_kill = Rc::clone(&server);
        let path = dl.join("t.bin");
        let path_dbg = path.clone();
        let fail = move |msg: &str| -> ! {
            eprintln!("TEST FAILURE: {msg}");
            eprintln!(
                "TEST server log:\n{}",
                std::fs::read_to_string(&server_log).unwrap_or_default()
            );
            eprintln!(
                "TEST partial: {:?}",
                std::fs::metadata(&path_dbg).map(|m| m.len())
            );
            let _ = server_kill.borrow_mut().kill();
            std::process::exit(1);
        };
        let item = manager
            .enqueue(&url, Some(&dest), Some("t.bin"))
            .unwrap_or_else(|e| fail(&e));
        let id = item.id();

        glib::timeout_future(std::time::Duration::from_secs(1)).await;
        if item.status() != DownloadStatus::Downloading {
            fail("expected Downloading after 1s");
        }
        manager.pause(id);
        assert_eq!(item.status(), DownloadStatus::Paused);
        let size_a = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        glib::timeout_future(std::time::Duration::from_millis(600)).await;
        let size_b = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        assert_eq!(size_a, size_b, "bytes moved while paused");

        manager.resume(id);
        let mut waited = 0;
        while item.status() != DownloadStatus::Done && waited < 300 {
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
            waited += 1;
        }
        if item.status() != DownloadStatus::Done {
            fail(&format!("expected Done, got {:?}", item.status()));
        }
        if std::fs::read(&path).unwrap() != payload {
            fail("bytes differ");
        }
        let ranges = std::fs::read_to_string(dir.join("ranges.log")).unwrap_or_default();
        if !ranges.lines().any(|l| l.contains("range=bytes=")) {
            fail("resume never sent a Range request (206 path untested)");
        }

        let item2 = manager.enqueue(&url, Some(&dest), Some("t2.bin")).unwrap();
        manager.cancel(item2.id());
        assert_eq!(item2.status(), DownloadStatus::Cancelled);

        cleanup(&server, &dir);
        quit.quit();
    });
    run_loop(&main_loop, 90);
}

#[test]
fn segmented_multi_connection_download() {
    let (_lock, _loop) = test_locks();
    let _qf = test_queue_file("segmented");
    let settings = test_settings();
    settings.set_int("connections", 4).unwrap();

    // 20 MB clears the 4 x 4 MB split threshold; unthrottled loopback
    // keeps the test to a few seconds.
    let Fixture {
        dir,
        dl,
        payload,
        port,
        server,
    } = spawn_fixture("seg", "big.bin", 20_000_000, "0", &[], 13);

    settings.set_boolean("show-notifications", false).unwrap();
    let store = gio::ListStore::new::<DownloadItem>();
    let manager = DownloadManager::new(store, settings.clone());
    let url = format!("http://127.0.0.1:{port}/big.bin");
    let dest = dl.to_string_lossy().into_owned();

    let main_loop = glib::MainLoop::new(None, false);
    let quit = main_loop.clone();
    glib::MainContext::default().spawn_local(async move {
        let path = dl.join("big.bin");
        let item = manager
            .enqueue(&url, Some(&dest), Some("big.bin"))
            .unwrap_or_else(|e| abort(&server, &e));
        let mut waited = 0;
        while item.status() != DownloadStatus::Done && waited < 600 {
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
            waited += 1;
        }
        if item.status() != DownloadStatus::Done {
            abort(&server, &format!("expected Done, got {:?}", item.status()));
        }
        if std::fs::read(&path).unwrap() != payload {
            abort(&server, "bytes differ");
        }
        if !item.detail().starts_with("Finished \u{2022} ") {
            abort(
                &server,
                &format!("expected size in detail, got {:?}", item.detail()),
            );
        }
        // Distinct bounded ranges prove parallel fetching (one stream would log one open-ended line).
        let ranges = std::fs::read_to_string(dir.join("ranges.log")).unwrap_or_default();
        let mut distinct = std::collections::HashSet::new();
        for line in ranges.lines().filter_map(|l| l.strip_prefix("range=")) {
            // Log format is "range=<Range>|if-range=<value>"; isolate the range part.
            let range = line.split('|').next().unwrap_or("");
            if range.starts_with("bytes=")
                && range.contains('-')
                && !range.ends_with('-')
                && range.split('-').count() == 2
            {
                distinct.insert(range.to_string());
            }
        }
        if distinct.len() < 2 {
            abort(
                &server,
                &format!("expected 2+ distinct ranges, log: {ranges:?}"),
            );
        }
        cleanup(&server, &dir);
        quit.quit();
    });
    run_loop(&main_loop, 120);
}

#[test]
fn adopts_content_disposition_filename() {
    let (_lock, _loop) = test_locks();
    let _qf = test_queue_file("disposition");
    let settings = test_settings();

    let Fixture {
        dir,
        dl,
        payload,
        port,
        server,
    } = spawn_fixture(
        "cd",
        "clip.mp4",
        300_000,
        "0.05",
        &["attachment; filename=\"movie.mp4\""],
        17,
    );

    settings.set_boolean("show-notifications", false).unwrap();
    let store = gio::ListStore::new::<DownloadItem>();
    let manager = DownloadManager::new(store, settings.clone());
    // Extensionless path, like CDN /videoplayback links.
    let url = format!("http://127.0.0.1:{port}/getfile");
    let dest = dl.to_string_lossy().into_owned();

    let main_loop = glib::MainLoop::new(None, false);
    let quit = main_loop.clone();
    glib::MainContext::default().spawn_local(async move {
        let item = manager
            .enqueue(&url, Some(&dest), None)
            .unwrap_or_else(|e| abort(&server, &e));
        assert_eq!(item.filename(), "getfile");
        let mut waited = 0;
        while (item.filename() != "movie.mp4" || item.status() != DownloadStatus::Done)
            && waited < 600
        {
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
            waited += 1;
        }
        if item.filename() != "movie.mp4" {
            abort(
                &server,
                &format!("expected rename, got {:?}", item.filename()),
            );
        }
        if item.status() != DownloadStatus::Done {
            abort(&server, &format!("expected Done, got {:?}", item.status()));
        }
        let new_path = dl.join("movie.mp4");
        if std::fs::read(&new_path).unwrap() != payload {
            abort(&server, "bytes differ");
        }
        if dl.join("getfile").exists() {
            abort(&server, "stale file left behind");
        }
        cleanup(&server, &dir);
        quit.quit();
    });
    run_loop(&main_loop, 120);
}

#[test]
fn prefers_shorter_content_disposition_filename() {
    let (_lock, _loop) = test_locks();
    let _qf = test_queue_file("disposition-short");
    let settings = test_settings();

    let Fixture {
        dir,
        dl,
        payload,
        port,
        server,
    } = spawn_fixture(
        "cds",
        "short.zip",
        300_000,
        "0.05",
        &["attachment; filename=\"short.zip\""],
        41,
    );

    settings.set_boolean("show-notifications", false).unwrap();
    let store = gio::ListStore::new::<DownloadItem>();
    let manager = DownloadManager::new(store, settings.clone());
    // Long tracked/tokenized URL name with an extension, like file hosts emit.
    let long_name = format!("{}.zip", "a".repeat(120));
    let url = format!("http://127.0.0.1:{port}/{long_name}");
    let dest = dl.to_string_lossy().into_owned();

    let main_loop = glib::MainLoop::new(None, false);
    let quit = main_loop.clone();
    glib::MainContext::default().spawn_local(async move {
        let item = manager
            .enqueue(&url, Some(&dest), None)
            .unwrap_or_else(|e| abort(&server, &e));
        assert_eq!(item.filename(), long_name);
        let mut waited = 0;
        while (item.filename() != "short.zip" || item.status() != DownloadStatus::Done)
            && waited < 600
        {
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
            waited += 1;
        }
        if item.filename() != "short.zip" {
            abort(
                &server,
                &format!("expected rename, got {:?}", item.filename()),
            );
        }
        if item.status() != DownloadStatus::Done {
            abort(&server, &format!("expected Done, got {:?}", item.status()));
        }
        let new_path = dl.join("short.zip");
        if std::fs::read(&new_path).unwrap() != payload {
            abort(&server, "bytes differ");
        }
        cleanup(&server, &dir);
        quit.quit();
    });
    run_loop(&main_loop, 120);
}

#[test]
fn stale_pump_future_ignores_respawned_row() {
    // Old pump must not drag progress back or steal the new engine's handle.
    let (_lock, _loop) = test_locks();
    let _qf = test_queue_file("stale-pump");
    let settings = test_settings();
    settings.set_int("max-concurrent", 1).unwrap();

    let Fixture {
        dir,
        dl,
        payload: _,
        port,
        server,
    } = spawn_fixture("hang", "h.bin", 20_000, "30", &[], 41);

    settings.set_boolean("show-notifications", false).unwrap();
    let store = gio::ListStore::new::<DownloadItem>();
    let manager = DownloadManager::new(store, settings.clone());
    let url = format!("http://127.0.0.1:{port}/h.bin");
    let dest = dl.to_string_lossy().into_owned();
    let item = manager
        .enqueue(&url, Some(&dest), Some("h.bin"))
        .unwrap_or_else(|e| abort(&server, &e));
    let id = item.id();
    assert_eq!(item.status(), DownloadStatus::Downloading);
    // Let the first engine get going, then defer: park aborts it and a
    // second engine takes over the same row.
    std::thread::sleep(std::time::Duration::from_millis(500));
    manager.defer(id);
    assert_eq!(item.status(), DownloadStatus::Downloading);
    // Let the aborted engine die and the hanging one settle, then pump:
    // the stale future's tail runs here and must stay silent.
    std::thread::sleep(std::time::Duration::from_millis(500));
    let ctx = glib::MainContext::default();
    for _ in 0..20 {
        ctx.iteration(false);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if item.status() != DownloadStatus::Downloading {
        abort(
            &server,
            &format!(
                "stale pump future clobbered the respawned row: {:?}",
                item.detail(),
            ),
        );
    }
    if !manager.running.borrow().contains_key(&id) {
        abort(&server, "respawned engine lost its handle");
    }
    manager.cancel(id);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while manager.running.borrow().contains_key(&id) && std::time::Instant::now() < deadline {
        ctx.iteration(false);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    if manager.running.borrow().contains_key(&id) {
        abort(&server, "cancelled engine never exited");
    }
    // Quiesce here so the cancelled pump's woken tail finishes on this thread.
    quiesce(&ctx);
    cleanup(&server, &dir);
    settings.set_int("max-concurrent", 3).unwrap();
}

#[test]
fn piece_rejects_changed_file_version() {
    // No locks: pure tokio + unique fixture, nothing shared.
    let Fixture {
        dir,
        dl,
        payload,
        port,
        server,
    } = spawn_fixture("verc", "v.bin", 300_000, "0", &[], 43);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let ctx = FetchCtx {
        client: http_client().clone(),
        url: format!("http://127.0.0.1:{port}/v.bin"),
        dest: dl.join("v.bin"),
        opts: DownloadOptions {
            ..Default::default()
        },
        cookies: None,
        timeout: Duration::from_secs(30),
        tx,
        if_range: std::sync::Arc::new(std::sync::OnceLock::new()),
    };
    let total = payload.len() as u64;
    // Bogus total: server Content-Range disagrees, must fail Changed at once.
    match tokio_rt().block_on(fetch_piece(&ctx, 0, 1023, 1)) {
        Err(AttemptFail::Changed(_)) => {}
        _ => abort(&server, "wrong-total piece must fail Changed"),
    }
    match tokio_rt().block_on(fetch_piece(&ctx, 0, 1023, total)) {
        Ok(body) => assert_eq!(body.len(), 1024),
        _ => abort(&server, "correct-total piece must succeed"),
    }
    cleanup(&server, &dir);
}

#[test]
fn falls_back_to_single_stream_when_throttled() {
    let (_lock, _loop) = test_locks();
    let _qf = test_queue_file("throttle");
    let settings = test_settings();
    settings.set_int("connections", 4).unwrap();

    // Big enough to split; the server 403s every range except the probe,
    // so the engine must downgrade to one stream and still finish intact.
    let Fixture {
        dir,
        dl,
        payload,
        port,
        server,
    } = spawn_fixture(
        "thr",
        "big.bin",
        20_000_000,
        "0",
        &["", "throttle-ranges"],
        19,
    );

    settings.set_boolean("show-notifications", false).unwrap();
    let store = gio::ListStore::new::<DownloadItem>();
    let manager = DownloadManager::new(store, settings.clone());
    let url = format!("http://127.0.0.1:{port}/big.bin");
    let dest = dl.to_string_lossy().into_owned();

    let main_loop = glib::MainLoop::new(None, false);
    let quit = main_loop.clone();
    glib::MainContext::default().spawn_local(async move {
        let path = dl.join("big.bin");
        let item = manager
            .enqueue(&url, Some(&dest), Some("big.bin"))
            .unwrap_or_else(|e| abort(&server, &e));
        let mut waited = 0;
        while item.status() != DownloadStatus::Done && waited < 600 {
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
            waited += 1;
        }
        if item.status() != DownloadStatus::Done {
            abort(&server, &format!("expected Done, got {:?}", item.status()));
        }
        if std::fs::read(&path).unwrap() != payload {
            abort(&server, "bytes differ");
        }
        // A full (unranged) request proves the single-stream fallback ran.
        let ranges = std::fs::read_to_string(dir.join("ranges.log")).unwrap_or_default();
        if !ranges.lines().any(|l| l.starts_with("range=full")) {
            abort(
                &server,
                &format!("expected single-stream fallback, log: {ranges:?}"),
            );
        }
        cleanup(&server, &dir);
        quit.quit();
    });
    run_loop(&main_loop, 120);
}

#[test]
fn segmented_pause_resume_keeps_bytes() {
    let (_lock, _loop) = test_locks();
    let _qf = test_queue_file("seg-pause");
    let settings = test_settings();
    settings.set_int("connections", 4).unwrap();

    // 20 MB clears the split threshold; throttled so a poll can land mid-transfer.
    let Fixture {
        dir,
        dl,
        payload,
        port,
        server,
    } = spawn_fixture("segp", "big.bin", 20_000_000, "0.002", &[], 23);

    settings.set_boolean("show-notifications", false).unwrap();
    let store = gio::ListStore::new::<DownloadItem>();
    let manager = DownloadManager::new(store, settings.clone());
    let url = format!("http://127.0.0.1:{port}/big.bin");
    let dest = dl.to_string_lossy().into_owned();

    let main_loop = glib::MainLoop::new(None, false);
    let quit = main_loop.clone();
    glib::MainContext::default().spawn_local(async move {
        let path = dl.join("big.bin");
        let item = manager
            .enqueue(&url, Some(&dest), Some("big.bin"))
            .unwrap_or_else(|e| abort(&server, &e));
        let id = item.id();
        // Wait for one 1 MB piece (5%), then pause. Status check is in the loop condition
        // so it cannot slip to Done before pause() with no await in between.
        let mut waited = 0;
        while item.status() == DownloadStatus::Downloading
            && item.progress() < 0.05
            && waited < 2000
        {
            glib::timeout_future(std::time::Duration::from_millis(5)).await;
            waited += 1;
        }
        if item.status() != DownloadStatus::Downloading {
            abort(&server, "finished before pause could land mid-transfer");
        }
        manager.pause(id);
        if item.status() != DownloadStatus::Paused {
            abort(
                &server,
                &format!("expected Paused, got {:?}", item.status()),
            );
        }
        // Resume must reuse the bitmap (partially done, not all).
        let mid: bool = manager
            .segment_state
            .borrow()
            .get(&id)
            .map(|st| st.done.iter().any(|b| *b) && st.done.iter().any(|b| !b))
            .unwrap_or(false);
        if !mid {
            abort(&server, "resume bitmap is not mid-transfer");
        }
        manager.resume(id);
        let mut waited = 0;
        while item.status() != DownloadStatus::Done && waited < 600 {
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
            waited += 1;
        }
        if item.status() != DownloadStatus::Done {
            abort(&server, &format!("expected Done, got {:?}", item.status()));
        }
        // Regression: resume truncated completed pieces while the bitmap claimed them done.
        if std::fs::read(&path).unwrap() != payload {
            abort(&server, "bytes differ after pause/resume");
        }
        cleanup(&server, &dir);
        quit.quit();
    });
    run_loop(&main_loop, 120);
}

#[test]
fn bitmap_persists_across_managers() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let qf = test_queue_file("segpersist");
    let settings = test_settings();
    let m1 = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    let item = DownloadItem::new(1, "https://example.com/big.bin", "big.bin", "/tmp/dl");
    item.set_status(DownloadStatus::Paused);
    m1.store().append(&item);
    let mut st = SegmentState::new(4 * PIECE_MIN);
    st.mark(0);
    st.mark(2);
    m1.segment_state.borrow_mut().insert(1, st);
    m1.persist_queue();

    // v1 readers ignore the unknown field; v2 keeps it.
    let text = std::fs::read_to_string(&qf).unwrap();
    assert!(text.contains("\"segments\""));
    let legacy = r#"{"version":1,"items":[{"url":"https://example.com/a.iso","dest_dir":"/tmp/dl","filename":"a.iso","status":"queued","progress":0.0}]}"#;
    let back: StoredQueue = serde_json::from_str(legacy).unwrap();
    assert!(back.items[0].segments.is_none());

    let m2 = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    m2.restore_queue();
    let restored = m2.segment_state.borrow();
    let st = restored.get(&1).expect("bitmap restored");
    assert_eq!(st.total, 4 * PIECE_MIN);
    assert_eq!(
        st.missing(),
        vec![
            (1, PIECE_MIN, 2 * PIECE_MIN - 1),
            (3, 3 * PIECE_MIN, 4 * PIECE_MIN - 1),
        ]
    );
    let _ = std::fs::remove_file(&qf);
}

#[test]
fn killed_segmented_resume_starts_over() {
    // SIGKILL mid-download: sparse full-size file, stale Queued entry, no bitmap.
    // Restart must re-fetch, never mark the holey file Done via the 416 shortcut.
    let (_lock, _loop) = test_locks();
    let qf = test_queue_file("kill");
    let settings = test_settings();

    let Fixture {
        dir,
        dl,
        payload,
        port,
        server,
    } = spawn_fixture("kill", "big.bin", 20_000_000, "0", &[], 29);

    // Sparse corpse: full size, only the first megabyte real.
    let dest = dl.join("big.bin");
    {
        let f = std::fs::File::create(&dest).unwrap();
        f.set_len(20_000_000).unwrap();
        use std::io::Write;
        let mut f = f;
        f.write_all(&payload[..1_000_000]).unwrap();
    }
    assert!(has_holes(&dest));
    std::fs::write(
        &qf,
        serde_json::to_string(&StoredQueue {
            version: QUEUE_VERSION,
            items: vec![StoredItem {
                id: None,
                url: format!("http://127.0.0.1:{port}/big.bin"),
                dest_dir: dl.to_string_lossy().into_owned(),
                filename: "big.bin".to_string(),
                status: DownloadStatus::Queued,
                progress: 0.0,
                segments: None,
                selected_files: None,
                output_dir: None,
                video_source: None,
                started: None,
                scheduled_at: None,
                etag: None,
                last_modified: None,
            }],
        })
        .unwrap(),
    )
    .unwrap();

    settings.set_boolean("show-notifications", false).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let main_loop = glib::MainLoop::new(None, false);
    let quit = main_loop.clone();
    glib::MainContext::default().spawn_local(async move {
        manager.restore_queue();
        if manager.store().n_items() != 1 {
            abort(&server, "queue did not restore");
        }
        let item = manager
            .store()
            .item(0)
            .and_downcast::<DownloadItem>()
            .unwrap();
        let mut waited = 0;
        while item.status() != DownloadStatus::Done && waited < 600 {
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
            waited += 1;
        }
        if item.status() != DownloadStatus::Done {
            abort(&server, &format!("expected Done, got {:?}", item.status()));
        }
        if std::fs::read(&dest).unwrap() != payload {
            abort(&server, "holey file was marked Done instead of re-fetched");
        }
        // Drain the finished row's async disk sizing before quitting: the
        // sizing future is thread-bound, and the serial suite shares one main
        // context across test threads, so it must not outlive this test.
        let mut swaited = 0;
        while manager.sizing_inflight.get() > 0 && swaited < 150 {
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
            swaited += 1;
        }
        if manager.sizing_inflight.get() > 0 {
            abort(&server, "finished-row sizing did not drain");
        }
        cleanup(&server, &dir);
        let _ = std::fs::remove_file(&qf);
        quit.quit();
    });
    run_loop(&main_loop, 120);
}

#[test]
fn shutdown_restart_resumes_segmented() {
    // Full kill-restart in-process: shutdown persists the bitmap, manager2 resumes segmented.
    let (_lock, _loop) = test_locks();
    let qf = test_queue_file("segrestart");
    let settings = test_settings();
    settings.set_int("connections", 4).unwrap();

    let Fixture {
        dir,
        dl,
        payload,
        port,
        server,
    } = spawn_fixture("segr", "big.bin", 20_000_000, "0.002", &[], 31);

    settings.set_boolean("show-notifications", false).unwrap();
    let manager1 = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    let main_loop = glib::MainLoop::new(None, false);
    let quit = main_loop.clone();
    glib::MainContext::default().spawn_local(async move {
        let url = format!("http://127.0.0.1:{port}/big.bin");
        let dest = dl.to_string_lossy().into_owned();
        let item = manager1
            .enqueue(&url, Some(&dest), Some("big.bin"))
            .unwrap_or_else(|e| abort(&server, &e));
        let id = item.id();
        let mut waited = 0;
        while item.status() == DownloadStatus::Downloading
            && item.progress() < 0.05
            && waited < 2000
        {
            glib::timeout_future(std::time::Duration::from_millis(5)).await;
            waited += 1;
        }
        if item.status() != DownloadStatus::Downloading {
            abort(&server, "finished before shutdown could land mid-transfer");
        }
        manager1.shutdown();
        // Bitmap survived shutdown, mid-transfer, and was persisted.
        let mid1 = manager1
            .segment_state
            .borrow()
            .get(&id)
            .map(|st| st.done.iter().any(|b| *b) && st.done.iter().any(|b| !b))
            .unwrap_or(false);
        if !mid1 {
            abort(&server, "no mid-transfer bitmap at shutdown");
        }
        let text = std::fs::read_to_string(&qf).unwrap();
        if !text.contains("\"segments\"") {
            abort(&server, "bitmap was not persisted");
        }
        // Fresh manager on the same queue file must resume segmented, not restart.
        let manager2 =
            DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
        manager2.restore_queue();
        if manager2.store().n_items() != 1 {
            abort(&server, "queue did not restore");
        }
        let mid2 = manager2
            .segment_state
            .borrow()
            .values()
            .any(|st| st.done.iter().any(|b| *b) && st.done.iter().any(|b| !b));
        if !mid2 {
            abort(&server, "restored manager has no segmented resume state");
        }
        let item2 = manager2
            .store()
            .item(0)
            .and_downcast::<DownloadItem>()
            .unwrap();
        let mut waited = 0;
        while item2.status() != DownloadStatus::Done && waited < 600 {
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
            waited += 1;
        }
        if item2.status() != DownloadStatus::Done {
            abort(&server, &format!("expected Done, got {:?}", item2.status()));
        }
        if std::fs::read(dl.join("big.bin")).unwrap() != payload {
            abort(&server, "bytes differ after kill-restart-resume");
        }
        // Drain the finished row's async disk sizing before quitting: the
        // sizing future is thread-bound, and the serial suite shares one main
        // context across test threads, so it must not outlive this test.
        let mut swaited = 0;
        while manager2.sizing_inflight.get() > 0 && swaited < 150 {
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
            swaited += 1;
        }
        if manager2.sizing_inflight.get() > 0 {
            abort(&server, "finished-row sizing did not drain");
        }
        cleanup(&server, &dir);
        let _ = std::fs::remove_file(&qf);
        quit.quit();
    });
    run_loop(&main_loop, 120);
}

#[test]
fn aborted_engine_marks_failed() {
    // Engine died without reporting (panic): abort must fail the row, not strand it.
    let (_lock, _loop) = test_locks();
    let _qf = test_queue_file("abortwatch");
    let settings = test_settings();

    let dir = std::env::temp_dir().join(format!("grab-abw-{}", std::process::id()));
    let srv = dir.join("srv");
    let dl = dir.join("dl");
    std::fs::create_dir_all(&srv).unwrap();
    std::fs::create_dir_all(&dl).unwrap();
    let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(srv.join("t.bin"), &payload).unwrap();

    let port = test_port(37);
    let ranges_log = dir.join("ranges.log");
    let server = std::process::Command::new("python3")
        .arg(format!(
            "{}/tests/throttled_server.py",
            env!("CARGO_MANIFEST_DIR")
        ))
        .arg(port.to_string())
        .arg(srv.join("t.bin"))
        .arg(&ranges_log)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("python3 range server");
    let mut ready = false;
    for _ in 0..100 {
        if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(ready, "test HTTP server did not listen on port {port}");

    let store = gio::ListStore::new::<DownloadItem>();
    let manager = DownloadManager::new(store, settings);
    let url = format!("http://127.0.0.1:{port}/t.bin");
    let dest = dl.to_string_lossy().into_owned();

    let main_loop = glib::MainLoop::new(None, false);
    let quit = main_loop.clone();
    let server = Rc::new(RefCell::new(server));
    glib::MainContext::default().spawn_local(async move {
        let item = manager
            .enqueue(&url, Some(&dest), Some("t.bin"))
            .unwrap_or_else(|e| abort(&server, &e));
        let id = item.id();
        let mut waited = 0;
        while item.status() != DownloadStatus::Downloading && waited < 200 {
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
            waited += 1;
        }
        if item.status() != DownloadStatus::Downloading {
            abort(&server, "never started downloading");
        }
        // Kill the engine task without touching row state, as a panic would.
        if let Some(handle) = manager.running.borrow().get(&id) {
            handle.abort();
        }
        let mut waited = 0;
        while item.status() != DownloadStatus::Failed && waited < 100 {
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
            waited += 1;
        }
        if item.status() != DownloadStatus::Failed {
            abort(
                &server,
                &format!("dead engine left row {:?}", item.status()),
            );
        }
        assert_eq!(item.detail(), "Download interrupted");
        cleanup(&server, &dir);
        quit.quit();
    });
    run_loop(&main_loop, 60);
}

#[test]
fn failed_download_reports_cause() {
    let (_lock, _loop) = test_locks();
    let _qf = test_queue_file("http-error");
    let settings = test_settings();
    let port = test_port(7);
    let server = std::process::Command::new("python3")
        .args(["-m", "http.server", &port.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("python3 http.server");
    for _ in 0..100 {
        if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let server = Rc::new(RefCell::new(server));
    let main_loop = glib::MainLoop::new(None, false);
    let quit = main_loop.clone();
    glib::MainContext::default().spawn_local(async move {
        let store = gio::ListStore::new::<DownloadItem>();
        let manager = DownloadManager::new(store, settings);
        let item = manager
            .enqueue(
                &format!("http://127.0.0.1:{port}/nope.bin"),
                None,
                Some("nope.bin"),
            )
            .unwrap_or_else(|e| abort(&server, &e));
        let mut waited = 0;
        while item.status() != DownloadStatus::Failed && waited < 200 {
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
            waited += 1;
        }
        if item.status() != DownloadStatus::Failed {
            abort(
                &server,
                &format!("expected Failed, got {:?}", item.status()),
            );
        }
        if !item.detail().contains("404") {
            abort(
                &server,
                &format!("expected 404 hint, got {:?}", item.detail()),
            );
        }
        let _ = server.borrow_mut().kill();
        quit.quit();
    });
    run_loop(&main_loop, 60);
}

#[test]
fn restart_with_smaller_file_keeps_partial() {
    // Resume answered from zero with a SMALLER object is a different file: fail
    // loudly and keep partial bytes. Plain http.server ignores Range, matching this shape.
    let (_lock, _loop) = test_locks();
    let _qf = test_queue_file("shrink-guard");
    let settings = test_settings();
    let dir = std::env::temp_dir().join(format!("grab-shrink-{}", std::process::id()));
    let srv = dir.join("srv");
    let dl = dir.join("dl");
    std::fs::create_dir_all(&srv).unwrap();
    std::fs::create_dir_all(&dl).unwrap();
    std::fs::write(srv.join("t.bin"), vec![7u8; 10 * 1024]).unwrap();
    std::fs::write(dl.join("t.bin"), vec![0u8; 1024 * 1024]).unwrap();

    let port = test_port(53);
    let server = std::process::Command::new("python3")
        .args([
            "-m",
            "http.server",
            &port.to_string(),
            "--directory",
            &srv.to_string_lossy(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("python3 http.server");
    let mut ready = false;
    for _ in 0..100 {
        if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(ready, "test HTTP server did not listen on port {port}");

    let server = Rc::new(RefCell::new(server));
    let store = gio::ListStore::new::<DownloadItem>();
    let manager = DownloadManager::new(store, settings.clone());
    let url = format!("http://127.0.0.1:{port}/t.bin");
    let dest = dl.to_string_lossy().into_owned();
    // restore_existing, not enqueue: the restore path is synchronous (no engine race).
    let mut stored = stored_row(&url, &dest, "t.bin", DownloadStatus::Downloading);
    // The row started in a past session: its bytes are its own partial, so the
    // engine resumes instead of refusing the destination as foreign.
    stored.started = Some(true);
    let item = manager
        .restore_existing(&stored)
        .unwrap_or_else(|e| abort(&server, &e));
    let id = item.id();
    // Drain the engine's pump future on this thread (see MAIN_LOOP_LOCK).
    let ctx = glib::MainContext::default();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while manager.running.borrow().contains_key(&id) && std::time::Instant::now() < deadline {
        ctx.iteration(false);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    if item.status() != DownloadStatus::Failed {
        abort(
            &server,
            &format!("expected loud failure, got {:?}", item.status()),
        );
    }
    if !item.detail().contains("smaller file") {
        abort(&server, &format!("wrong cause: {:?}", item.detail()));
    }
    if std::fs::metadata(dl.join("t.bin"))
        .map(|m| m.len())
        .unwrap_or(0)
        != 1024 * 1024
    {
        abort(&server, "partial file was truncated");
    }
    cleanup(&server, &dir);
}

#[test]
fn aggregate_downsamples_by_half() {
    assert!(aggregate(&[], 8).is_empty());
    assert!(aggregate(&[true, false], 0).is_empty());
    assert_eq!(aggregate(&[true, true, true, false], 2), vec![true, true]);
    assert_eq!(
        aggregate(&[true, false, false, false], 2),
        vec![true, false]
    );
    // Fewer pieces than cells: one true piece lights its own cells.
    let cells = aggregate(&[false, false, true, false], 8);
    assert_eq!(cells.len(), 8);
    assert!(cells[4] && cells[5]);
    assert_eq!(cells.iter().filter(|b| **b).count(), 2);
    assert_eq!(
        aggregate(&[true; 4096], BLOCK_CELLS),
        vec![true; BLOCK_CELLS]
    );
}

#[test]
fn piece_bitmap_prefers_segment_then_torrent_then_bytes() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("piece-bitmap");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    assert!(manager.piece_bitmap(7).is_empty());

    let item = DownloadItem::new(7, "https://example.com/big.bin", "big.bin", "/tmp/dl");
    manager.store().append(&item);
    // No bytes yet: nothing known.
    assert!(manager.piece_bitmap(7).is_empty());
    // Single-stream fallback fills a byte-progress prefix at BLOCK_CELLS.
    item.set_progress(0.5);
    let prefix = manager.piece_bitmap(7);
    assert_eq!(prefix.len(), BLOCK_CELLS);
    assert!(prefix[..128].iter().all(|b| *b));
    assert!(prefix[128..].iter().all(|b| !b));
    // A live segmented bitmap wins over the byte fill.
    let mut st = SegmentState::new(4 * PIECE_MIN);
    st.mark(1);
    manager.segment_state.borrow_mut().insert(7, st);
    assert_eq!(
        manager.piece_bitmap(7),
        manager.segment_state.borrow().get(&7).unwrap().done
    );

    // Torrent haves win over bytes but lose nothing: no segment entry here.
    let torrent = DownloadItem::new(
        8,
        "magnet:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3&dn=t",
        "t",
        "/tmp/dl",
    );
    manager.store().append(&torrent);
    // Magnets never byte-fill: no session haves yet, so empty.
    torrent.set_progress(0.5);
    assert!(manager.piece_bitmap(8).is_empty());
    let haves = vec![true, false, true];
    manager.torrent_pieces.borrow_mut().insert(8, haves.clone());
    assert_eq!(manager.piece_bitmap(8), haves);
}

#[test]
fn remove_cleans_video_staging() {
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("remove-staging");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let id = 910_000 + std::process::id() as u64;
    // Dest-dir parts go with the row too; finished file and foreign neighbors stay.
    // Under manifest-only cleanup, only manifest-owned files are deleted.
    let destdir = std::env::temp_dir().join(format!("grab-remove-parts-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&destdir);
    std::fs::create_dir_all(&destdir).unwrap();
    // Create a valid manifest owning the staging files.
    // Note: manifest writing is done via JSON directly in tests.
    let manifest_json = format!(
        r#"{{"page_url":"https://x.com/u/status/1","quality":"1080p","video_format_id":null,"video_ext":"mp4","audio_format_id":"","audio_ext":"","final_bytes":null,"staging_name":"v.{id}.mp4"}}"#
    );
    std::fs::write(destdir.join(format!(".{id}.manifest.json")), manifest_json).unwrap();
    for n in ["v.mp4", "v.srt"] {
        std::fs::write(destdir.join(n), b"x").unwrap();
    }
    // Manifest-owned files (will be deleted)
    std::fs::write(destdir.join(format!("v.{id}.mp4.part")), b"x").unwrap();
    let item = DownloadItem::new(
        id,
        "https://x.com/u/status/1",
        "v.mp4",
        destdir.to_str().unwrap(),
    );
    manager.store().append(&item);
    manager.video_sources.borrow_mut().insert(
        id,
        crate::media_types::VideoSource::Page {
            page_url: "https://x.com/u/status/1".to_string(),
            media_url: None,
            expires_at: None,
            quality: "1080p".to_string(),
            audio_only: false,
            is_live: false,
            video_format_id: None,
            playlist_item_id: None,
        },
    );
    manager.remove(id);
    assert!(
        !destdir.join(format!(".{id}.manifest.json")).exists(),
        "staged sidecar must go with the row"
    );
    assert!(
        !destdir.join(format!("v.{id}.mp4.part")).exists(),
        "manifest-owned part goes too"
    );
    assert!(destdir.join("v.mp4").exists(), "finished file stays");
    assert!(destdir.join("v.srt").exists(), "foreign files stay");
    let _ = std::fs::remove_dir_all(&destdir);
}

#[test]
fn remove_cleans_dest_side_video_staging() {
    // The worker's scratch lives as `grab-<id>-*` files in the dest dir
    // and goes with the row; the finished file and foreign neighbors stay.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("remove-dest-staging");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let id = 911_000 + std::process::id() as u64;
    let destdir = std::env::temp_dir().join(format!("grab-remove-dest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&destdir);
    std::fs::create_dir_all(&destdir).unwrap();
    // Visible staging: grab-<id>-* files live in the dest dir itself.
    std::fs::write(destdir.join(format!(".grab-{id}-manifest.json")), b"{}").unwrap();
    std::fs::write(destdir.join(format!("grab-{id}-video.f137.mp4")), b"x").unwrap();
    for n in ["v.mp4", "v.srt"] {
        std::fs::write(destdir.join(n), b"x").unwrap();
    }
    let item = DownloadItem::new(
        id,
        "https://x.com/u/status/1",
        "v.mp4",
        destdir.to_str().unwrap(),
    );
    manager.store().append(&item);
    manager.video_sources.borrow_mut().insert(
        id,
        crate::media_types::VideoSource::Page {
            page_url: "https://x.com/u/status/1".to_string(),
            media_url: None,
            expires_at: None,
            quality: "1080p".to_string(),
            audio_only: false,
            is_live: false,
            video_format_id: None,
            playlist_item_id: None,
        },
    );
    manager.remove(id);
    // The manifest goes; the finished file and foreign files stay.
    // Under manifest-only cleanup, the staging file without a valid manifest
    // staging_name is preserved (no proof of ownership).
    assert!(
        !destdir.join(format!(".grab-{id}-manifest.json")).exists(),
        "dest-side manifest must go with the row"
    );
    assert!(
        destdir.join(format!("grab-{id}-video.f137.mp4")).exists(),
        "manifest-less staging file is preserved (no ownership proof)"
    );
    assert!(destdir.exists(), "dest dir is never removed");
    assert!(destdir.join("v.mp4").exists(), "finished file stays");
    assert!(destdir.join("v.srt").exists(), "foreign files stay");
    let _ = std::fs::remove_dir_all(&destdir);
}

#[test]
fn removing_a_plain_row_still_aborts_its_task_and_frees_the_slot() {
    // Regression: removal was cooperative for every row, so plain rows kept writing
    // and leaked a slot when the pump tail early-returned on the dropped epoch.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("remove-plain-abort");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let id = 923_000 + std::process::id() as u64;
    let item = DownloadItem::new(id, "https://example.com/x.bin", "x.bin", "/tmp/dl");
    manager.store().append(&item);
    manager.epoch.borrow_mut().insert(id, 1);

    // A Drop flag inside the task, not an absence checked after a sleep: a flag
    // that was *not* set passes the same when the task was detached, not aborted.
    struct DropFlag(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let dropped_task = std::sync::Arc::clone(&dropped);
    let started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let started_task = std::sync::Arc::clone(&started);
    let handle = crate::runtime::tokio_rt().spawn(async move {
        let _flag = DropFlag(dropped_task);
        started_task.store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    });
    manager.running.borrow_mut().insert(id, handle);
    // The task must be genuinely up before the removal: aborting a
    // never-polled task would pass without proving anything ran.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !started.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        started.load(Ordering::SeqCst),
        "the stand-in engine never started, so the removal proved nothing"
    );

    manager.remove(id);

    assert!(
        manager.running.borrow().get(&id).is_none(),
        "the concurrency slot was not freed immediately, so a long session \
         would run out of slots"
    );
    // The abort must have destroyed the task, not detached it: dropping
    // the handle alone leaves the future (and its flag) alive.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !dropped.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        dropped.load(Ordering::SeqCst),
        "a plain row's engine was detached rather than aborted: it kept running \
         after its row was removed"
    );
}

#[test]
fn remove_tells_a_live_worker_to_discard_and_waits_for_it_to_stop() {
    // Old `remove` could neither say *how* to stop nor wait. The stand-in recreates
    // dirs before its late write, so only a sweep that waits for teardown passes.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("remove-discard-live");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let id = 922_000 + std::process::id() as u64;
    // A destination unique to this test: `clean_dest_parts` scans the
    // directory, so a shared one would let it delete files it does not own.
    let dest_dir = std::env::temp_dir().join(format!("grab-rmdiscard-{id}"));
    let _ = std::fs::remove_dir_all(&dest_dir);
    std::fs::create_dir_all(&dest_dir).unwrap();
    // Visible staging: grab-<id>-* files in the dest dir (no legacy subfolder).
    std::fs::write(dest_dir.join(format!(".grab-{id}-manifest.json")), b"{}").unwrap();
    let part = dest_dir.join(format!("v.live-{id}.mp4.part"));
    std::fs::write(&part, b"recorded").unwrap();

    let item = DownloadItem::new(
        id,
        "https://x.com/u/status/1",
        "v.mp4",
        &dest_dir.to_string_lossy(),
    );
    manager.store().append(&item);
    manager.video_sources.borrow_mut().insert(
        id,
        crate::media_types::VideoSource::Page {
            page_url: "https://x.com/u/status/1".to_string(),
            media_url: None,
            expires_at: None,
            quality: "1080p".to_string(),
            audio_only: false,
            is_live: true,
            video_format_id: None,
            playlist_item_id: None,
        },
    );
    manager.live_rows.borrow_mut().insert(id);
    manager.epoch.borrow_mut().insert(id, 1);

    let (intent_tx, intent_rx) = tokio::sync::oneshot::channel();
    manager.video_abort.borrow_mut().insert(id, intent_tx);
    let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
    let seen_task = std::sync::Arc::clone(&seen);
    let done = std::sync::Arc::new(std::sync::Mutex::new(false));
    let done_task = std::sync::Arc::clone(&done);
    let wrote = std::sync::Arc::new(std::sync::Mutex::new((false, false)));
    let wrote_task = std::sync::Arc::clone(&wrote);
    let dest_task = dest_dir.clone();
    let id_task = id;
    let handle = crate::runtime::tokio_rt().spawn(async move {
        let intent = intent_rx.await.ok();
        *seen_task.lock().unwrap() = intent;
        // A dying recorder is SIGKILLed, not politely shut down: give teardown
        // a beat, then recreate scratch the way a late write would.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let staging_ok = std::fs::write(
            dest_task.join(format!("grab-{id_task}-video.f137.mp4.part")),
            b"late",
        )
        .is_ok();
        let dest_ok = std::fs::write(dest_task.join("v.live.mp4"), b"late delivery").is_ok();
        *wrote_task.lock().unwrap() = (staging_ok, dest_ok);
        *done_task.lock().unwrap() = true;
    });
    manager.running.borrow_mut().insert(id, handle);

    manager.remove(id);

    // Cleanup is deferred to a finalizer; poll with a deadline instead of a fixed sleep.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !*done.lock().unwrap() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        *done.lock().unwrap(),
        "the stand-in worker never ran to completion"
    );
    // Both late writes must land: a silent fixture failure would pass against this bug.
    let (staging_ok, dest_ok) = *wrote.lock().unwrap();
    assert!(
        staging_ok && dest_ok,
        "the stand-in worker failed to write its late files (staging: {staging_ok}, \
         dest: {dest_ok}), so the sweep below proved nothing"
    );
    // Awaiting the finalizer handle proves the sweep ran after the worker stopped.
    let finalizer = manager
        .discards
        .borrow_mut()
        .remove(&id)
        .expect("remove tracks a finalizer for a live video row")
        .finalizer;
    let _ = crate::runtime::tokio_rt().block_on(finalizer);
    let listing = || {
        std::fs::read_dir(&dest_dir)
            .map(|r| {
                r.filter_map(|e| e.ok())
                    .map(|e| format!("{:?}", e.file_name()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|e| vec![format!("READDIR-ERR {e:?}")])
    };
    assert!(
        !dest_dir.join(format!(".grab-{id}-manifest.json")).exists(),
        "manifest is removed with the row"
    );
    // Under manifest-only cleanup, the late-written scratch (no manifest)
    // is preserved. This is the safe behavior: without ownership proof,
    // we don't delete.
    assert!(
        dest_dir
            .join(format!("grab-{id}-video.f137.mp4.part"))
            .exists(),
        "late-written scratch without manifest is preserved (safe)"
    );
    if part.exists() || dest_dir.join(format!("live-{id}.mp4")).exists() {
        panic!(
            "the recorder's scratch outlived the row, including anything it \
             recreated after the sweep; dest_dir holds {:?}",
            listing()
        );
    }
    assert_eq!(
        *seen.lock().unwrap(),
        Some(crate::video::StopIntent::Discard),
        "remove must say *how* to stop: a removed row told to Preserve runs \
         the finalize path and delivers a file that has no row to belong to"
    );
    assert!(
        !manager.live_rows.borrow().contains(&id),
        "a removed row stayed marked live, so a later row reusing the id would \
         take the live signal-only paths and skip the abort"
    );
    let _ = std::fs::remove_dir_all(&dest_dir);
}

#[test]
fn remove_leaves_plain_rows_staging_alone() {
    // Non-video rows never stage: no video source, no cleanup attempt.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("remove-plain");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let id = 930_000 + std::process::id() as u64;
    let dir = crate::video::staging_root().join(id.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    let item = DownloadItem::new(id, "https://example.com/a.bin", "a.bin", "/tmp/dl");
    manager.store().append(&item);
    manager.remove(id);
    assert!(dir.exists(), "plain rows must not trigger staging cleanup");
    let _ = std::fs::remove_dir_all(&dir);
}

fn proxy_opts(mode: &str, ptype: &str, host: &str, port: i32) -> DownloadOptions {
    DownloadOptions {
        limit_rate: String::new(),
        connections: 4,
        proxy_mode: mode.into(),
        proxy_type: ptype.into(),
        proxy_host: host.into(),
        proxy_port: port,
        cookies_browser: String::new(),
    }
}

#[test]
fn proxy_direct_and_unknown_modes_go_direct() {
    assert!(
        proxy_opts("direct", "socks5", "127.0.0.1", 9050)
            .proxy_config()
            .expect("valid")
            .is_none()
    );
    // Unknown values fail open like system; only well-formedness asserted here.
    assert!(
        proxy_opts("mystery", "socks5", "127.0.0.1", 9050)
            .proxy_config()
            .is_ok()
    );
}

#[test]
fn proxy_manual_builds_remote_dns_socks() {
    let proxy = proxy_opts("manual", "socks5", "127.0.0.1", 9050)
        .proxy_config()
        .expect("valid")
        .expect("proxied");
    // socks5h: names resolve remotely, never beside the tunnel.
    assert_eq!(proxy.cli_url, "socks5h://127.0.0.1:9050");
    assert!(proxy.no_proxy_env.contains("localhost"));
    // Pooled clients key on the full config: identical configs share, differing ones do not.
    let again = proxy_opts("manual", "socks5", "127.0.0.1", 9050)
        .proxy_config()
        .expect("valid")
        .expect("proxied");
    assert_eq!(proxy.cache_key, again.cache_key);
    let other = proxy_opts("manual", "socks5", "127.0.0.1", 9051)
        .proxy_config()
        .expect("valid")
        .expect("proxied");
    assert_ne!(proxy.cache_key, other.cache_key);
    let _ = http_client_for(Some(&proxy));
}

#[test]
fn proxy_manual_rejects_garbage_loudly() {
    // A typo must fail the row, never leak direct.
    assert!(
        proxy_opts("manual", "socks5", "   ", 9050)
            .proxy_config()
            .is_err()
    );
    assert!(
        proxy_opts("manual", "socks5", "127.0.0.1", 0)
            .proxy_config()
            .is_err()
    );
    assert!(
        proxy_opts("manual", "socks5", "127.0.0.1", 65536)
            .proxy_config()
            .is_err()
    );
    assert!(
        proxy_opts("manual", "socks5", "127.0.0.1", -1)
            .proxy_config()
            .is_err()
    );
    assert!(
        proxy_opts("manual", "gopher", "127.0.0.1", 9050)
            .proxy_config()
            .is_err()
    );
    // URL-smuggling hosts must fail, never build `http://a@b:port`.
    for hostile in [
        "proxy.lan@evil.com",
        "proxy.lan/path",
        "proxy.lan?x=1",
        "proxy.lan#frag",
        "proxy lan",
        "proxy.lan\nX-Injected: 1",
    ] {
        assert!(
            proxy_opts("manual", "http", hostile, 8080)
                .proxy_config()
                .is_err(),
            "hostile host accepted: {hostile:?}"
        );
    }
    // IPv6 literals and underscores stay valid (brackets required).
    assert!(
        proxy_opts("manual", "socks5", "[::1]", 9050)
            .proxy_config()
            .expect("valid")
            .is_some()
    );
    // HTTP covers both schemes on one URL.
    let proxy = proxy_opts("manual", "http", "proxy.lan", 8080)
        .proxy_config()
        .expect("valid")
        .expect("proxied");
    assert_eq!(proxy.cli_url, "http://proxy.lan:8080");
}

#[test]
fn proxy_clients_are_pooled_per_config() {
    // Same config reuses the client; different config adds a pool entry.
    let proxy = proxy_opts("manual", "socks5", "127.0.0.1", 19050)
        .proxy_config()
        .expect("valid")
        .expect("proxied");
    let before = proxied_pool_len();
    let _ = http_client_for(Some(&proxy));
    assert_eq!(proxied_pool_len(), before + 1);
    let _ = http_client_for(Some(&proxy));
    assert_eq!(proxied_pool_len(), before + 1);
}

#[test]
fn proxy_mode_and_type_mappings() {
    assert_eq!(proxy_mode_index("manual"), 1);
    assert_eq!(proxy_mode_index("mystery"), 0);
    assert_eq!(proxy_mode_value(2), PROXY_MODE_DIRECT);
    assert_eq!(proxy_mode_value(9), PROXY_MODE_SYSTEM);
    assert_eq!(proxy_type_index("http"), 0);
    assert_eq!(proxy_type_index("mystery"), 2);
    assert_eq!(proxy_type_value(0), "http");
    assert_eq!(proxy_type_value(9), "socks5");
    assert_eq!(proxy_mode_labels().len(), 3);
    assert_eq!(proxy_type_labels(), vec!["HTTP", "HTTPS", "SOCKS5"]);
}

#[test]
fn normalize_no_proxy_entries() {
    assert_eq!(
        normalize_no_proxy(&["*.local".into(), ".example.com ".into(), "  ".into()]),
        "local,example.com"
    );
    assert_eq!(normalize_no_proxy(&[]), "");
}

#[test]
fn cookies_parse_netscape_matrix() {
    let text = "# Netscape HTTP Cookie File\n.example.com\tTRUE\t/\tFALSE\t9999999999\tsid\tabc123\n#HttpOnly_.example.com\tTRUE\t/\tTRUE\t9999999999\ttok\tse cret\n#HttpOnly_.secure.example\tTRUE\t/\tTRUE\t9999999999\ts\t1\nbadline\nshort\ta\tb\nsemi.example\tTRUE\t/\tFALSE\t1\tn\tv;w\n.empty\tTRUE\t/\tFALSE\t1\t\tv\n";
    let (jar, count) = crate::cookies::jar_from_export(text);
    // sid + tok survive; short lines, empty names, semicolon values are dropped.
    assert_eq!(count, 3);
    let header =
        crate::cookies::cookie_header_for(&jar, "https://example.com/v").expect("in-scope cookies");
    let header = header.to_str().unwrap();
    assert!(header.contains("sid=abc123"), "{header}");
    assert!(header.contains("tok=se cret"), "{header}");
    // Wrong domain and empty jar send nothing.
    assert!(crate::cookies::cookie_header_for(&jar, "https://other.org/").is_none());
    let (empty, _) = crate::cookies::jar_from_export("# nothing here\n");
    assert!(crate::cookies::cookie_header_for(&empty, "https://example.com/").is_none());
}

#[test]
fn cookies_secure_flag_is_scheme_aware() {
    // Secure cookies ride https only; plain cookies ride both schemes.
    let (jar, _) = crate::cookies::jar_from_export(
        ".secure.example\tTRUE\t/\tTRUE\t9999999999\ts\t1\n.plain.example\tTRUE\t/\tFALSE\t9999999999\tp\t2\n",
    );
    let https = crate::cookies::cookie_header_for(&jar, "https://secure.example/v")
        .expect("secure over https");
    assert_eq!(https, "s=1");
    assert!(crate::cookies::cookie_header_for(&jar, "http://secure.example/v").is_none());
    let plain =
        crate::cookies::cookie_header_for(&jar, "http://plain.example/v").expect("plain over http");
    assert_eq!(plain, "p=2");
}

#[test]
fn cookies_stamp_request_headers() {
    let (jar, _) =
        crate::cookies::jar_from_export(".example.com\tTRUE\t/\tFALSE\t9999999999\tsid\tabc123\n");
    let built = stamp_request(
        http_client().get("https://example.com/v"),
        "Grab-test/1.0",
        Some(&jar),
        "https://example.com/v",
    )
    .build()
    .unwrap();
    assert_eq!(built.headers()["user-agent"], "Grab-test/1.0");
    assert_eq!(built.headers()["cookie"], "sid=abc123");
    // No jar, no Cookie header (plain rows unchanged).
    let built = stamp_request(
        http_client().get("https://example.com/v"),
        "",
        None,
        "https://example.com/v",
    )
    .build()
    .unwrap();
    assert!(!built.headers().contains_key("cookie"));
    assert!(!built.headers().contains_key("user-agent"));
}

#[test]
fn cookies_export_roundtrip() {
    // Fake yt-dlp honoring `--cookies PATH`: proves export glue without a browser.
    let dir = std::env::temp_dir().join(format!("grab-fakecookies-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("fake-ytdlp-cookies");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
out=""
prev=""
for a in "$@"; do
    if [ "$prev" = "--cookies" ]; then out="$a"; fi
    prev="$a"
done
printf '.example.com\tTRUE\t/\tFALSE\t9999999999\tsid\tabc123\n' > "$out"
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let jar = crate::runtime::tokio_rt()
        .block_on(crate::cookies::jar_for_browser(
            "firefox",
            &bin,
            "https://example.com/v",
        ))
        .expect("exported jar");
    let header = crate::cookies::cookie_header_for(&jar, "https://sub.example.com/v")
        .expect("subdomain in scope");
    assert_eq!(header, "sid=abc123");
    // The off switch never spawns.
    let none = crate::runtime::tokio_rt().block_on(crate::cookies::jar_for_browser(
        "none",
        &bin,
        "https://example.com/v",
    ));
    assert!(none.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cookie_export_ignores_ambient_configs() {
    // --ignore-config must reach the export spawn or an ambient config silently degrades export.
    let dir = std::env::temp_dir().join(format!("grab-ignorecfg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("fake-ytdlp-ignorecfg");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
has=0
for a in "$@"; do
    if [ "$a" = "--ignore-config" ]; then has=1; fi
done
[ "$has" = 1 ] || exit 1
out=""
prev=""
for a in "$@"; do
    if [ "$prev" = "--cookies" ]; then out="$a"; fi
    prev="$a"
done
printf '.example.com\tTRUE\t/\tFALSE\t9999999999\tsid\tabc123\n' > "$out"
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let jar = crate::runtime::tokio_rt()
        .block_on(crate::cookies::jar_for_browser(
            "firefox",
            &bin,
            "https://example.com/v",
        ))
        .expect("exported jar with --ignore-config");
    assert!(crate::cookies::cookie_header_for(&jar, "https://example.com/v").is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn direct_mode_ignores_proxy_env() {
    // Ambient proxy vars must never steer Direct rows.
    let (_lock, _loop) = test_locks();
    let Fixture {
        dir,
        dl,
        payload,
        port,
        server,
    } = spawn_fixture("noenv", "v.bin", 20_000, "0", &[], 44);
    // SAFETY: serial suite; the guard below restores on all paths
    // including panic unwinds.
    struct EnvGuard;
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: same serial-suite context as the setters.
            unsafe {
                std::env::remove_var("HTTP_PROXY");
                std::env::remove_var("HTTPS_PROXY");
                std::env::remove_var("ALL_PROXY");
            }
        }
    }
    unsafe {
        std::env::set_var("HTTP_PROXY", "http://127.0.0.1:9");
        std::env::set_var("HTTPS_PROXY", "http://127.0.0.1:9");
        std::env::set_var("ALL_PROXY", "http://127.0.0.1:9");
    }
    let _env = EnvGuard;
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let ctx = FetchCtx {
        client: http_client().clone(),
        url: format!("http://127.0.0.1:{port}/v.bin"),
        dest: dl.join("v.bin"),
        opts: DownloadOptions {
            ..Default::default()
        },
        cookies: None,
        timeout: Duration::from_secs(30),
        tx,
        if_range: std::sync::Arc::new(std::sync::OnceLock::new()),
    };
    tokio_rt().block_on(run_download(ctx, 1, StartMode::Single));
    let got = std::fs::read(dl.join("v.bin")).unwrap_or_default();
    if got != payload {
        abort(&server, "direct download must ignore proxy env");
    }
    cleanup(&server, &dir);
    drop(_env);
}

#[test]
fn proxy_argv_precedes_end_of_options() {
    // optparse treats everything after `--` positionally: proxied argv must flag before it.
    let proxy = proxy_opts("manual", "socks5", "127.0.0.1", 9050)
        .proxy_config()
        .expect("valid")
        .expect("proxied");
    let job = crate::video::VideoJob {
        item_id: 1,
        page_url: "https://x.com/u/status/1".into(),
        playlist_item_id: None,
        quality: "best".into(),
        audio_only: false,
        dest: std::path::PathBuf::from("/tmp/dl/v.mp4"),
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
        proxy: Some(proxy),
    };
    for argv in [
        crate::video_argv::unified_download_argv(
            &job,
            "v+a/bv*+ba/b",
            true,
            "mp4",
            std::path::Path::new("/usr/bin/ffmpeg"),
            std::path::Path::new("/tmp/staging/media.%(ext)s"),
            None,
        ),
        crate::video_argv::live_capture_argv(&job, "h", std::path::Path::new("/tmp/x.mp4"), None),
    ] {
        let flag = argv
            .iter()
            .position(|a| a == "--proxy")
            .expect("proxy flag");
        let sep = argv.iter().position(|a| a == "--").expect("separator");
        assert!(flag < sep, "{argv:?}");
        assert_eq!(argv[flag + 1], "socks5h://127.0.0.1:9050");
    }
}

#[test]
fn single_connection_skips_probe() {
    // Single-use token URLs die on any pre-request: no probe, single GET only.
    let (_lock, _loop) = test_locks();
    let Fixture {
        dir,
        dl,
        payload,
        port,
        server,
    } = spawn_fixture("noprobe", "v.bin", 20_000, "0", &[], 45);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let ctx = FetchCtx {
        client: http_client().clone(),
        url: format!("http://127.0.0.1:{port}/v.bin"),
        dest: dl.join("v.bin"),
        opts: DownloadOptions {
            ..Default::default()
        },
        cookies: None,
        timeout: Duration::from_secs(30),
        tx,
        if_range: std::sync::Arc::new(std::sync::OnceLock::new()),
    };
    tokio_rt().block_on(run_download(ctx, 1, StartMode::Fresh));
    let got = std::fs::read(dl.join("v.bin")).unwrap_or_default();
    if got != payload {
        abort(&server, "single-stream download must complete");
    }
    let ranges = std::fs::read_to_string(dir.join("ranges.log")).unwrap_or_default();
    assert!(
        ranges.lines().all(|l| l.starts_with("range=full")),
        "no Range request may precede the download: {ranges:?}"
    );
    cleanup(&server, &dir);
}

#[test]
fn stamp_request_sends_no_referer() {
    // No Referer is sent: a manually-stamped Referer would persist across
    // redirects (reqwest doesn't strip custom headers), leaking the origin
    // to cross-origin targets.
    let built = stamp_request(
        http_client().get("http://127.0.0.1:8080/a/b?token=secret"),
        "",
        None,
        "http://127.0.0.1:8080/a/b?token=secret",
    )
    .build()
    .unwrap();
    assert!(!built.headers().contains_key("referer"));
    let built = stamp_request(
        http_client().get("https://example.com/v"),
        "",
        None,
        "https://example.com/v",
    )
    .build()
    .unwrap();
    assert!(!built.headers().contains_key("referer"));
}

#[test]
fn cookies_export_failure_means_plain_requests() {
    // Failing export degrades to plain requests instead of bricking authed downloads.
    let dir = std::env::temp_dir().join(format!("grab-fakecookies-fail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("fake-ytdlp-cookies");
    std::fs::write(&bin, "#!/bin/sh\necho 'locked profile' >&2\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let jar = crate::runtime::tokio_rt().block_on(crate::cookies::jar_for_browser(
        "chrome",
        &bin,
        "https://example.com/v",
    ));
    assert!(jar.is_none(), "failed export degrades to plain");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cookies_empty_export_still_returns_jar() {
    // Logged-out profile (valid but empty) is usable: no Cookie header, move on.
    let dir = std::env::temp_dir().join(format!("grab-fakecookies-empty-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("fake-ytdlp-cookies");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
out=""
prev=""
for a in "$@"; do
    if [ "$prev" = "--cookies" ]; then out="$a"; fi
    prev="$a"
done
printf '# Netscape HTTP Cookie File\n' > "$out"
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let jar = crate::runtime::tokio_rt()
        .block_on(crate::cookies::jar_for_browser(
            "opera",
            &bin,
            "https://example.com/v",
        ))
        .expect("empty export is still an answer");
    assert!(
        crate::cookies::cookie_header_for(&jar, "https://example.com/v").is_none(),
        "nothing in scope, no header"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clear_finished_drops_only_done() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let qf = test_queue_file("clear-finished");
    let settings = test_settings();
    let m = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let add = |id: u64, url: &str, name: &str, status: DownloadStatus| {
        let it = DownloadItem::new(id, url, name, "/tmp/dl");
        it.set_status(status);
        m.store().append(&it);
    };
    add(
        1,
        "https://example.com/a.iso",
        "a.iso",
        DownloadStatus::Done,
    );
    add(
        2,
        "https://example.com/b.iso",
        "b.iso",
        DownloadStatus::Done,
    );
    add(
        3,
        "https://example.com/c.iso",
        "c.iso",
        DownloadStatus::Queued,
    );
    add(
        4,
        "https://example.com/d.iso",
        "d.iso",
        DownloadStatus::Failed,
    );
    assert_eq!(m.finished_count(), 2);
    assert_eq!(m.clear_finished(), 2);
    assert_eq!(m.finished_count(), 0);
    assert_eq!(m.store().n_items(), 2);
    // The persisted queue carries only the survivors.
    let text = std::fs::read_to_string(&qf).unwrap();
    let queue: StoredQueue = serde_json::from_str(&text).unwrap();
    assert_eq!(queue.items.len(), 2);
    assert!(
        queue
            .items
            .iter()
            .all(|it| it.status != DownloadStatus::Done),
        "no Done rows may persist"
    );
    // Empty clear is a quiet no-op (no file churn, no UI sync).
    let mtime = std::fs::metadata(&qf).and_then(|m| m.modified()).ok();
    let syncs = std::rc::Rc::new(std::cell::Cell::new(0u32));
    m.set_on_change({
        let syncs = syncs.clone();
        move || {
            syncs.set(syncs.get() + 1);
        }
    });
    assert_eq!(m.clear_finished(), 0);
    assert_eq!(syncs.get(), 0, "empty clear must not sync the UI");
    assert_eq!(
        std::fs::metadata(&qf).and_then(|m| m.modified()).ok(),
        mtime,
        "empty clear must not rewrite the queue file"
    );
    m.cancel_all();
    let _ = std::fs::remove_file(&qf);
}

#[test]
fn restore_dedups_finished_urls() {
    // Queue files before dedup may hold several Done rows per URL; newest (last) wins.
    // Holds MAIN_LOOP_LOCK (via test_locks): restoring the Done row spawns a
    // thread-bound sizing future that must be drained before this test ends.
    let (_lock, _loop) = test_locks();
    let qf = test_queue_file("history-dedup");
    let settings = test_settings();
    let m1 = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    for (id, name) in [(1u64, "old.iso"), (2, "new.iso")] {
        let it = DownloadItem::new(id, "https://example.com/x.iso", name, "/tmp/dl");
        it.set_progress(1.0);
        it.set_status(DownloadStatus::Done);
        it.set_detail("Finished".to_string());
        m1.store().append(&it);
    }
    m1.persist_queue();

    let m2 = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    m2.restore_queue();
    assert_eq!(m2.store().n_items(), 1);
    let it = m2.store().item(0).and_downcast::<DownloadItem>().unwrap();
    assert_eq!(it.filename(), "new.iso");
    assert_eq!(it.status(), DownloadStatus::Done);
    // Drain the restored Done row's async sizing so no thread-bound future
    // outlives this test (glib thread guard, serial suite shares one context).
    drain_sizing(&m2);
    let _ = std::fs::remove_file(&qf);
}

#[test]
fn drop_finished_duplicates_keeps_active_and_newest() {
    // The finish hook's policy, directly: the just-finished row stays,
    // older Done rows for the URL go, everything else is user intent.
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("history-dedup-live");
    let settings = test_settings();
    let m = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let add = |id: u64, url: &str, status: DownloadStatus| {
        let it = DownloadItem::new(id, url, &format!("f{id}.iso"), "/tmp/dl");
        it.set_status(status);
        m.store().append(&it);
    };
    add(1, "https://example.com/a.iso", DownloadStatus::Done);
    add(2, "https://example.com/a.iso", DownloadStatus::Done);
    add(3, "https://example.com/a.iso", DownloadStatus::Queued);
    add(4, "https://example.com/b.iso", DownloadStatus::Done);
    // Row 2 just finished: older Done for the URL drops, queued rows and other URLs stay.
    m.drop_finished_duplicates("https://example.com/a.iso", 2);
    let remaining: Vec<(u64, DownloadStatus)> = (0..m.store().n_items())
        .filter_map(|i| m.store().item(i).and_downcast::<DownloadItem>())
        .map(|it| (it.id(), it.status()))
        .collect();
    assert_eq!(
        remaining,
        vec![
            (2, DownloadStatus::Done),
            (3, DownloadStatus::Queued),
            (4, DownloadStatus::Done)
        ]
    );
    // Unnormalizable URLs skip quietly instead of dropping anything.
    m.drop_finished_duplicates("", u64::MAX);
    assert_eq!(m.store().n_items(), 3);
}

#[test]
fn enqueue_video_reserves_part_namespaced_stems() {
    // Foreign file under Grab's part namespace reserves the stem: intake dedupes onward.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("enqueue-video-reserve");
    let _notools = NoVideoTools::apply();
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::env::temp_dir().join(format!("grab-video-reserve-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("Clip.video.mp4"), b"foreign").unwrap();
    let dest_s = dest.to_string_lossy().into_owned();
    let item = manager
        .enqueue_video(
            "https://www.youtube.com/watch?v=gXtp6C-3JKo",
            Some(&dest_s),
            Some("Clip.mp4"),
            crate::media_types::VideoChoices {
                quality: "1080p".to_string(),
                audio_only: false,
                video_format_id: None,
                is_live: false,
                playlist_item_id: None,
            },
        )
        .expect("video enqueue");
    assert_eq!(item.filename(), "Clip (1).mp4");
    // The foreign file is untouched and the fresh stem is unreserved.
    assert_eq!(
        std::fs::read(dest.join("Clip.video.mp4")).unwrap(),
        b"foreign"
    );
    assert!(!crate::video_staging::stem_reserved_in(
        &crate::video_staging::dir_file_names(&dest),
        "Clip (1)"
    ));
    drain_engine(&manager, item.id());
    crate::video::clean_staging(&crate::video::staging_root().join(item.id().to_string()));
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn delete_download_trashes_video_sidecars() {
    // Trashing a video row takes its collected subtitle sidecars along;
    // plain rows keep a same-named srt (never Grab's).
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("video-del-sidecar");
    let settings = test_settings();
    // Home-backed dir: GIO refuses to trash across filesystems like /tmp.
    let dir = glib::user_data_dir().join(format!("grab-video-del-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let store = gio::ListStore::new::<DownloadItem>();
    let manager = DownloadManager::new(store.clone(), settings);
    let video = DownloadItem::new(
        11,
        "https://x.com/u/status/1",
        "Clip.mp4",
        &dir.to_string_lossy(),
    );
    video.set_status(DownloadStatus::Done);
    store.append(&video);
    manager.video_sources.borrow_mut().insert(
        11,
        crate::media_types::VideoSource::Page {
            page_url: "https://x.com/u/status/1".to_string(),
            media_url: None,
            expires_at: None,
            quality: "1080p".to_string(),
            audio_only: false,
            is_live: false,
            video_format_id: None,
            playlist_item_id: None,
        },
    );
    let plain = DownloadItem::new(
        12,
        "https://example.com/other.mp4",
        "Other.mp4",
        &dir.to_string_lossy(),
    );
    plain.set_status(DownloadStatus::Done);
    store.append(&plain);
    for n in [
        "Clip.mp4",
        "Clip.en.srt",
        "Clip.fr.srt",
        "Other.mp4",
        "Other.en.srt",
    ] {
        std::fs::write(dir.join(n), b"x").unwrap();
    }
    assert!(manager.delete_download(11).is_ok());
    assert!(manager.delete_download(12).is_ok());
    // Video + its sidecars trashed; plain row's same-named srt stays.
    for n in ["Clip.mp4", "Clip.en.srt", "Clip.fr.srt", "Other.mp4"] {
        assert!(!dir.join(n).exists(), "{n} must be trashed");
    }
    assert!(dir.join("Other.en.srt").exists());
    assert_eq!(store.n_items(), 0);
    // Undo the test's own Trash litter.
    let trash = glib::user_data_dir().join("Trash");
    for n in ["Clip.mp4", "Clip.en.srt", "Clip.fr.srt", "Other.mp4"] {
        let _ = std::fs::remove_file(trash.join(format!("files/{n}")));
        let _ = std::fs::remove_file(trash.join(format!("info/{n}.trashinfo")));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn enqueue_video_reserves_subtitle_sidecar_stems() {
    // Pre-existing foreign sidecar reserves the stem like a part file.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("enqueue-video-reserve-srt");
    let _notools = NoVideoTools::apply();
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::env::temp_dir().join(format!("grab-video-reserve-srt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("Clip.en.srt"), b"foreign").unwrap();
    let dest_s = dest.to_string_lossy().into_owned();
    let item = manager
        .enqueue_video(
            "https://www.youtube.com/watch?v=gXtp6C-3JKo",
            Some(&dest_s),
            Some("Clip.mp4"),
            crate::media_types::VideoChoices {
                quality: "1080p".to_string(),
                audio_only: false,
                video_format_id: None,
                is_live: false,
                playlist_item_id: None,
            },
        )
        .expect("video enqueue");
    assert_eq!(item.filename(), "Clip (1).mp4");
    assert_eq!(std::fs::read(dest.join("Clip.en.srt")).unwrap(), b"foreign");
    drain_engine(&manager, item.id());
    crate::video::clean_staging(&crate::video::staging_root().join(item.id().to_string()));
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn enqueue_video_accepts_unlisted_url() {
    // No domain gate: any normalizable URL queues as video (misuse fails at resolve).
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("enqueue-video-probed");
    let _notools = NoVideoTools::apply();
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let choices = crate::media_types::VideoChoices {
        quality: "1080p".to_string(),
        audio_only: false,
        video_format_id: None,
        is_live: false,
        playlist_item_id: None,
    };
    let item = manager
        .enqueue_video(
            "https://example.com/f.iso",
            Some("/tmp/dl"),
            None,
            choices.clone(),
        )
        .expect("probed intake");
    assert_eq!(item.filename(), "f.iso");
    assert!(
        manager
            .enqueue_video("not a url", Some("/tmp/dl"), None, choices)
            .is_err()
    );
    // Drain the spawned worker (fails fast without tools) so no woken
    // pump tail trips the next test's thread guard.
    drain_engine(&manager, item.id());
    manager.cancel_all();
    crate::video::clean_staging(&crate::video::staging_root().join(item.id().to_string()));
}

#[test]
fn a_video_attempt_gets_a_gate_and_a_plain_row_does_not() {
    // Video attempts arbitrate delivery via a gate; plain rows abort outright and need none.
    let (_q, _l) = test_locks();
    let qf = test_queue_file("gate-spawn");
    let settings = test_settings();
    // Fail validation after gate creation but before worker/pump starts: stays synchronous.
    settings
        .set_string(
            crate::settings::key::PROXY_MODE,
            crate::download_net::PROXY_MODE_MANUAL,
        )
        .unwrap();
    settings
        .set_string(crate::settings::key::PROXY_HOST, "")
        .unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);

    let plain = DownloadItem::new(700_001, "https://example.com/a.bin", "a.bin", "/tmp/dl");
    manager.store().append(&plain);
    manager.epoch.borrow_mut().insert(plain.id(), 1);
    assert!(
        manager.gate_for(plain.id()).is_none(),
        "a plain row must not be given a gate to arbitrate"
    );

    let mut row = stored_row(
        "https://example.com/v.mp4",
        "/tmp/dl",
        "v.mp4",
        DownloadStatus::Queued,
    );
    row.video_source = Some(crate::media_types::VideoSource::Page {
        page_url: "https://example.com/v.mp4".to_string(),
        media_url: None,
        expires_at: None,
        quality: "1080p".to_string(),
        audio_only: false,
        is_live: false,
        video_format_id: None,
        playlist_item_id: None,
    });
    let item = manager.restore_existing(&row).expect("video row");
    assert!(
        manager.gate_for(item.id()).is_some(),
        "a video row with no gate cannot arbitrate delivery, so a removal \
         could not stop it placing a file"
    );

    manager
        .settings()
        .set_string(crate::settings::key::PROXY_MODE, PROXY_MODE_DIRECT)
        .unwrap();
    let _ = std::fs::remove_file(qf);
}

#[test]
fn a_removal_claims_the_gate_before_the_worker_can_commit() {
    // Gate must be claimed at `remove` time: a pre-rename worker must find the row gone.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("remove-claims-gate");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let id = 924_000 + std::process::id() as u64;
    let dest_dir = std::env::temp_dir().join(format!("grab-gateclaim-{id}"));
    let _ = std::fs::remove_dir_all(&dest_dir);
    std::fs::create_dir_all(&dest_dir).unwrap();
    let item = DownloadItem::new(
        id,
        "https://x.com/u/status/1",
        "v.mp4",
        &dest_dir.to_string_lossy(),
    );
    manager.store().append(&item);
    manager.video_sources.borrow_mut().insert(
        id,
        crate::media_types::VideoSource::Page {
            page_url: "https://x.com/u/status/1".to_string(),
            media_url: None,
            expires_at: None,
            quality: "1080p".to_string(),
            audio_only: false,
            is_live: false,
            video_format_id: None,
            playlist_item_id: None,
        },
    );
    manager.epoch.borrow_mut().insert(id, 1);
    let gate = crate::video::AttemptGate::new();
    manager
        .gates
        .borrow_mut()
        .insert(id, std::sync::Arc::clone(&gate));
    let part = dest_dir.join(format!("v.live-{id}.mp4.part"));
    std::fs::write(&part, b"recorded").unwrap();

    manager.remove(id);

    assert!(
        !gate.try_commit(),
        "remove returned without claiming the gate, so a worker that had not \
         reached its rename could still deliver"
    );
    assert!(!gate.was_delivered());
    assert!(
        !part.exists(),
        "remove claimed the gate but left the row's part shell behind: the \
         inline reclaim must sweep it"
    );
    let _ = std::fs::remove_dir_all(&dest_dir);
}

#[test]
fn a_finalizer_removes_the_orphan_a_lost_commit_left_behind() {
    // Commit won and row is gone, so the finalizer must remove the placed orphan.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("remove-orphan");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let id = 925_000 + std::process::id() as u64;
    let dest_dir = std::env::temp_dir().join(format!("grab-orphan-{id}"));
    let _ = std::fs::remove_dir_all(&dest_dir);
    std::fs::create_dir_all(&dest_dir).unwrap();
    let dest = dest_dir.join("v.mp4");
    let item = DownloadItem::new(
        id,
        "https://x.com/u/status/1",
        "v.mp4",
        &dest_dir.to_string_lossy(),
    );
    manager.store().append(&item);
    manager.video_sources.borrow_mut().insert(
        id,
        crate::media_types::VideoSource::Page {
            page_url: "https://x.com/u/status/1".to_string(),
            media_url: None,
            expires_at: None,
            quality: "1080p".to_string(),
            audio_only: false,
            is_live: false,
            video_format_id: None,
            playlist_item_id: None,
        },
    );
    manager.epoch.borrow_mut().insert(id, 1);
    let gate = crate::video::AttemptGate::new();
    manager
        .gates
        .borrow_mut()
        .insert(id, std::sync::Arc::clone(&gate));
    // The commit already won, so the attempt will place the file.
    assert!(gate.try_commit());
    let worker_dest = dest.clone();
    let handle = crate::runtime::tokio_rt().spawn(async move {
        // Stand in for a worker that was already inside its rename.
        std::fs::write(&worker_dest, b"orphan").expect("stand-in worker must place the orphan");
        gate.mark_delivered();
    });
    manager.running.borrow_mut().insert(id, handle);

    manager.remove(id);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while dest.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        !dest.exists(),
        "the commit won, so the attempt placed a file, and the row is gone: \
         the orphan outlived it"
    );
    let _ = std::fs::remove_dir_all(&dest_dir);
}

#[test]
fn removing_a_settled_video_row_keeps_the_users_finished_file() {
    // Settled row (COMMITTING + delivered, no worker in flight): removal keeps the
    // user's file; only an awaited worker can leave an orphan behind.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("remove-settled-keeps");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let id = 926_000 + std::process::id() as u64;
    let dest_dir = std::env::temp_dir().join(format!("grab-settled-{id}"));
    let _ = std::fs::remove_dir_all(&dest_dir);
    std::fs::create_dir_all(&dest_dir).unwrap();
    let dest = dest_dir.join("v.mp4");
    std::fs::write(&dest, b"owned").unwrap();
    // Visible staging: grab-<id>-* files in the dest dir (no legacy subfolder).
    std::fs::write(dest_dir.join(format!(".grab-{id}-manifest.json")), b"{}").unwrap();
    let item = DownloadItem::new(
        id,
        "https://x.com/u/status/1",
        "v.mp4",
        &dest_dir.to_string_lossy(),
    );
    manager.store().append(&item);
    manager.video_sources.borrow_mut().insert(
        id,
        crate::media_types::VideoSource::Page {
            page_url: "https://x.com/u/status/1".to_string(),
            media_url: None,
            expires_at: None,
            quality: "1080p".to_string(),
            audio_only: false,
            is_live: false,
            video_format_id: None,
            playlist_item_id: None,
        },
    );
    manager.epoch.borrow_mut().insert(id, 1);
    let gate = crate::video::AttemptGate::new();
    assert!(gate.try_commit());
    gate.mark_delivered();
    manager
        .gates
        .borrow_mut()
        .insert(id, std::sync::Arc::clone(&gate));
    // No running handle: the attempt settled long ago.

    manager.remove(id);

    assert!(
        dest.exists(),
        "no worker was in flight, so nothing could have orphaned this file: \
         the finished file belongs to the user"
    );
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        b"owned",
        "the settled row's file must survive removal byte-for-byte"
    );
    assert!(
        !dest_dir.join(format!(".grab-{id}-manifest.json")).exists(),
        "the settled row's scratch must still go with the row"
    );
    let _ = std::fs::remove_dir_all(&dest_dir);
}

#[test]
fn a_pending_discard_reserves_the_destination_against_intake() {
    // Between remove and its finalizer the stem must not be claimable.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("reserve-intake");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::path::PathBuf::from("/tmp/dl/v.mp4");
    assert!(!manager.dest_reserved(&dest));
    manager.reserve_dest(&dest);
    assert!(
        manager.dest_reserved(&dest),
        "intake could claim a destination whose row is still tearing down"
    );
    manager.release_dest(&dest);
    assert!(
        !manager.dest_reserved(&dest),
        "the reservation outlived the cleanup"
    );
}

#[test]
fn an_undo_does_not_reclaim_a_destination_with_a_pending_discard() {
    // Undo bypasses intake dedupe, so it must consult the reservation too.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("reserve-undo");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::path::PathBuf::from("/tmp/dl/v.mp4");
    manager.reserve_dest(&dest);
    assert!(
        manager.dest_reserved(&dest),
        "Undo was allowed to reclaim a destination with a pending discard"
    );
}

#[test]
fn a_reserved_destination_forces_video_intake_to_dedupe() {
    // Destination counts as taken at intake or the old finalizer sweeps the new row's parts.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("reserve-video-intake");
    let _notools = NoVideoTools::apply();
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::env::temp_dir().join(format!("grab-reserve-intake-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    let dest_s = dest.to_string_lossy().into_owned();
    manager.reserve_dest(&dest.join("Clip.mp4"));
    let item = manager
        .enqueue_video(
            "https://www.youtube.com/watch?v=gXtp6C-3JKo",
            Some(&dest_s),
            Some("Clip.mp4"),
            crate::media_types::VideoChoices {
                quality: "1080p".to_string(),
                audio_only: false,
                video_format_id: None,
                is_live: false,
                playlist_item_id: None,
            },
        )
        .expect("video enqueue");
    assert_eq!(
        item.filename(),
        "Clip (1).mp4",
        "intake claimed a destination whose row is still tearing down"
    );
    drain_engine(&manager, item.id());
    crate::video::clean_staging(&crate::video::staging_root().join(item.id().to_string()));
    manager.release_dest(&dest.join("Clip.mp4"));
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn a_pending_discard_reserves_the_stem_against_a_different_extension() {
    // Sweep is stem-wide, so the reservation is too: different extension, same stem still counts.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("reserve-stem");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dir = std::path::PathBuf::from("/tmp/dl");
    manager.reserve_dest(&dir.join("v.mp4"));
    assert!(
        manager.dest_reserved(&dir.join("v.m4a")),
        "same stem, different extension was not reserved"
    );
    assert!(
        !manager.dest_reserved(&dir.join("w.mp4")),
        "a different stem in the same dir read as reserved"
    );
    assert!(
        !manager.dest_reserved(&std::path::PathBuf::from("/tmp/other/v.m4a")),
        "the same stem in a different dir read as reserved"
    );
    manager.release_dest(&dir.join("v.mp4"));
    assert!(
        !manager.dest_reserved(&dir.join("v.m4a")),
        "the stem reservation outlived the cleanup"
    );
}

#[test]
fn a_reserved_stem_forces_video_intake_to_dedupe() {
    // Same stem, different extension: intake must dedupe or the old sweep takes the new parts.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("reserve-stem-intake");
    let _notools = NoVideoTools::apply();
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::env::temp_dir().join(format!("grab-reserve-stem-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    let dest_s = dest.to_string_lossy().into_owned();
    manager.reserve_dest(&dest.join("Clip.mp4"));
    let item = manager
        .enqueue_video(
            "https://www.youtube.com/watch?v=gXtp6C-3JKo",
            Some(&dest_s),
            Some("Clip.m4a"),
            crate::media_types::VideoChoices {
                quality: "1080p".to_string(),
                audio_only: false,
                video_format_id: None,
                is_live: false,
                playlist_item_id: None,
            },
        )
        .expect("video enqueue");
    assert_eq!(
        item.filename(),
        "Clip (1).m4a",
        "intake claimed a stem whose row is still tearing down"
    );
    drain_engine(&manager, item.id());
    crate::video::clean_staging(&crate::video::staging_root().join(item.id().to_string()));
    manager.release_dest(&dest.join("Clip.mp4"));
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn a_second_discard_keeps_the_reservation_after_the_first_releases() {
    // Remove, Undo, remove again: the first release must not reopen the window.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("reserve-refcount");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::path::PathBuf::from("/tmp/dl/v.mp4");
    manager.reserve_dest(&dest);
    manager.reserve_dest(&dest);
    manager.release_dest(&dest);
    assert!(
        manager.dest_reserved(&dest),
        "the first release dropped a reservation a second discard still holds"
    );
    assert!(
        manager.dest_reserved(&std::path::PathBuf::from("/tmp/dl/v.m4a")),
        "the stem half of the reservation was dropped early too"
    );
    manager.release_dest(&dest);
    assert!(
        !manager.dest_reserved(&dest),
        "the reservation outlived both cleanups"
    );
}

#[test]
fn a_reserved_destination_forces_plain_intake_to_dedupe() {
    // Exact-path reservation applies to plain intake too.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("reserve-plain-intake");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::env::temp_dir().join(format!("grab-reserve-plain-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    let dest_s = dest.to_string_lossy().into_owned();
    manager.reserve_dest(&dest.join("a.bin"));
    let item = manager
        .enqueue("https://example.com/a.bin", Some(&dest_s), Some("a.bin"))
        .expect("plain enqueue");
    assert_eq!(
        item.filename(),
        "a (1).bin",
        "plain intake claimed a destination whose row is still tearing down"
    );
    manager.cancel(item.id());
    // Let the aborted engine's pump tail settle here, so no woken future
    // outlives this test's queue file.
    quiesce(&glib::MainContext::default());
    manager.release_dest(&dest.join("a.bin"));
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn a_finished_name_claim_honours_a_pending_discard() {
    // Finished-name claims consult `is_name_taken`: reserved must read as taken.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("reserve-claim");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::env::temp_dir().join(format!("grab-reserve-claim-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    let dest_s = dest.to_string_lossy().into_owned();
    let existing = crate::video_staging::dir_file_names(&dest);
    assert!(!manager.is_name_taken(&dest_s, &existing, "Clip.mp4"));
    manager.reserve_dest(&dest.join("Clip.mp4"));
    assert!(
        manager.is_name_taken(&dest_s, &existing, "Clip.mp4"),
        "a finished-name claim could take a destination still tearing down"
    );
    manager.release_dest(&dest.join("Clip.mp4"));
    assert!(!manager.is_name_taken(&dest_s, &existing, "Clip.mp4"));
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn released_reservation_wakes_parked_unremoved_row() {
    // Reserved destinations requeue without starting; release wakes via `wake_tx`.
    // Drives the async finalizer branch (worker in flight) across the tokio/main hop.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("reserve-unremove-wake");
    let _notools = NoVideoTools::apply();
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::env::temp_dir().join(format!("grab-reserve-wake-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    let dest_s = dest.to_string_lossy().into_owned();
    let reserved = dest.join("Clip.mp4");
    manager.reserve_dest(&reserved);
    let snap = RemovedSnapshot {
        url: "https://vimeo.com/123456".to_string(),
        dest_dir: dest_s,
        filename: "Clip.mp4".to_string(),
        status: DownloadStatus::Downloading,
        progress: 0.3,
        detail: String::new(),
        output_dir: String::new(),
        segments: None,
        video_source: Some(crate::media_types::VideoSource::Page {
            page_url: "https://vimeo.com/123456".to_string(),
            media_url: None,
            expires_at: None,
            quality: "720p".to_string(),
            audio_only: false,
            is_live: false,
            video_format_id: None,
            playlist_item_id: None,
        }),
        started: false,
    };
    let revived = manager.unremove(snap);
    let id = revived.id();
    assert_eq!(revived.status(), DownloadStatus::Queued);
    assert!(
        !manager.running.borrow().contains_key(&id),
        "unremove started a row whose destination is still tearing down"
    );
    manager.start_next();
    assert_eq!(
        revived.status(),
        DownloadStatus::Queued,
        "a later trigger started a row whose destination is still reserved"
    );
    assert!(!manager.running.borrow().contains_key(&id));
    // Stand-in worker for the async finalizer branch: waits, sweeps, releases, wakes.
    // Awaiting the tracked handle proves all of that finished.
    manager
        .running
        .borrow_mut()
        .insert(4242, crate::runtime::tokio_rt().spawn(async {}));
    manager.finish_discard(4242, reserved.clone(), AttemptGate::new());
    let finalizer = manager
        .discards
        .borrow_mut()
        .remove(&4242)
        .expect("finish_discard tracks a finalizer for a live worker")
        .finalizer;
    let _ = crate::runtime::tokio_rt().block_on(finalizer);
    assert!(
        !manager.dest_reserved(&reserved),
        "the finalizer must release the destination reservation"
    );
    quiesce(&glib::MainContext::default());
    // Under NoVideoTools the woken engine fails fast to Failed; either way the wakeup started it.
    assert_ne!(
        revived.status(),
        DownloadStatus::Queued,
        "releasing the reservation must wake the parked row"
    );
    drain_engine(&manager, id);
    crate::video::clean_staging(&crate::video::staging_root().join(id.to_string()));
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn an_unremove_of_a_settled_row_stays_settled_while_reserved() {
    // Only rows that would start divert to requeue: Done restores as Done, never starts.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("reserve-unremove-done");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::env::temp_dir().join(format!("grab-reserve-done-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    let dest_s = dest.to_string_lossy().into_owned();
    let reserved = dest.join("Clip.mp4");
    manager.reserve_dest(&reserved);
    let snap = RemovedSnapshot {
        url: "https://vimeo.com/123456".to_string(),
        dest_dir: dest_s,
        filename: "Clip.mp4".to_string(),
        status: DownloadStatus::Done,
        progress: 1.0,
        detail: String::new(),
        output_dir: String::new(),
        segments: None,
        video_source: Some(crate::media_types::VideoSource::Page {
            page_url: "https://vimeo.com/123456".to_string(),
            media_url: None,
            expires_at: None,
            quality: "720p".to_string(),
            audio_only: false,
            is_live: false,
            video_format_id: None,
            playlist_item_id: None,
        }),
        started: false,
    };
    let revived = manager.unremove(snap);
    assert_eq!(
        revived.status(),
        DownloadStatus::Done,
        "a settled restore must not be diverted into a re-download"
    );
    assert!(!manager.running.borrow().contains_key(&revived.id()));
    manager.release_dest(&reserved);
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn a_finalizer_releases_the_reservation_after_the_worker() {
    // The awaited-worker arm holds the destination until the sweep is done,
    // then releases it.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("reserve-release-async");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let id = 927_000 + std::process::id() as u64;
    let dest_dir = std::env::temp_dir().join(format!("grab-reserve-async-{id}"));
    let _ = std::fs::remove_dir_all(&dest_dir);
    std::fs::create_dir_all(&dest_dir).unwrap();
    let dest = dest_dir.join("v.mp4");
    let item = DownloadItem::new(
        id,
        "https://x.com/u/status/1",
        "v.mp4",
        &dest_dir.to_string_lossy(),
    );
    manager.store().append(&item);
    manager.video_sources.borrow_mut().insert(
        id,
        crate::media_types::VideoSource::Page {
            page_url: "https://x.com/u/status/1".to_string(),
            media_url: None,
            expires_at: None,
            quality: "1080p".to_string(),
            audio_only: false,
            is_live: false,
            video_format_id: None,
            playlist_item_id: None,
        },
    );
    manager.epoch.borrow_mut().insert(id, 1);
    let gate = crate::video::AttemptGate::new();
    manager
        .gates
        .borrow_mut()
        .insert(id, std::sync::Arc::clone(&gate));
    let handle = crate::runtime::tokio_rt().spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    });
    manager.running.borrow_mut().insert(id, handle);

    manager.remove(id);

    assert!(
        manager.dest_reserved(&dest),
        "remove must hold the destination until the finalizer finishes"
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while manager.dest_reserved(&dest) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        !manager.dest_reserved(&dest),
        "the finalizer never released the destination"
    );
    let _ = std::fs::remove_dir_all(&dest_dir);
}

#[test]
fn removing_a_never_spawned_video_row_releases_synchronously() {
    // No worker in flight: reclaim runs inline, reservation already gone at return.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("reserve-release-sync");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let id = 928_000 + std::process::id() as u64;
    let dest_dir = std::env::temp_dir().join(format!("grab-reserve-sync-{id}"));
    let _ = std::fs::remove_dir_all(&dest_dir);
    std::fs::create_dir_all(&dest_dir).unwrap();
    let dest = dest_dir.join("w.mp4");
    let item = DownloadItem::new(
        id,
        "https://x.com/u/status/2",
        "w.mp4",
        &dest_dir.to_string_lossy(),
    );
    manager.store().append(&item);
    manager.video_sources.borrow_mut().insert(
        id,
        crate::media_types::VideoSource::Page {
            page_url: "https://x.com/u/status/2".to_string(),
            media_url: None,
            expires_at: None,
            quality: "1080p".to_string(),
            audio_only: false,
            is_live: false,
            video_format_id: None,
            playlist_item_id: None,
        },
    );
    manager.remove(id);
    assert!(
        !manager.dest_reserved(&dest),
        "the inline reclaim must release the destination before remove returns"
    );
    let _ = std::fs::remove_dir_all(&dest_dir);
}

#[cfg(target_os = "linux")]
#[test]
fn shutdown_during_a_pending_discard_stops_the_worker_rather_than_detaching_it() {
    // Finalizer owns the worker handle: aborting it must abort the worker, not detach it (#180).
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("shutdown-discard");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let id = 926_000 + std::process::id() as u64;
    let dest_dir = std::env::temp_dir().join(format!("grab-shutdisc-{id}"));
    let _ = std::fs::remove_dir_all(&dest_dir);
    std::fs::create_dir_all(&dest_dir).unwrap();
    let item = DownloadItem::new(
        id,
        "https://x.com/u/status/1",
        "v.mp4",
        &dest_dir.to_string_lossy(),
    );
    manager.store().append(&item);
    manager.video_sources.borrow_mut().insert(
        id,
        crate::media_types::VideoSource::Page {
            page_url: "https://x.com/u/status/1".to_string(),
            media_url: None,
            expires_at: None,
            quality: "1080p".to_string(),
            audio_only: false,
            is_live: false,
            video_format_id: None,
            playlist_item_id: None,
        },
    );
    manager.epoch.borrow_mut().insert(id, 1);
    let gate = crate::video::AttemptGate::new();
    manager
        .gates
        .borrow_mut()
        .insert(id, std::sync::Arc::clone(&gate));

    // Scratch the real finalizer must reclaim: staging sidecar + dest-dir part.
    // Visible staging: grab-<id>-* files in the dest dir (no legacy subfolder).
    std::fs::write(dest_dir.join(format!(".grab-{id}-manifest.json")), b"{}").unwrap();
    let part = dest_dir.join("v.video.mp4");
    std::fs::write(&part, b"recorded").unwrap();

    // A worker that runs long and records that it was dropped.
    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    struct DropFlag(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let flag = std::sync::Arc::clone(&dropped);
    // Owned by the worker from construction: abort drops it whether or not the first poll ran.
    let guard = DropFlag(flag);
    let handle = crate::runtime::tokio_rt().spawn(async move {
        let _guard = guard;
        std::future::pending::<()>().await;
    });
    manager.running.borrow_mut().insert(id, handle);

    // Real path: `remove` claims the gate and hands the worker to `finish_discard`.
    manager.remove(id);

    manager.shutdown();

    assert!(
        dropped.load(std::sync::atomic::Ordering::SeqCst),
        "shutdown left the worker running: the finalizer's handle was dropped, \
         which detaches the task instead of aborting it"
    );
    assert!(
        !dest_dir.join(format!(".grab-{id}-manifest.json")).exists(),
        "shutdown never ran the finalizer's staging sweep: aborting finalizers \
         instead of awaiting them would leave this behind"
    );
    // Under manifest-only cleanup, the part without a valid manifest
    // staging_name is preserved (no ownership proof).
    assert!(
        part.exists(),
        "part without valid manifest is preserved (safe)"
    );
    let _ = std::fs::remove_dir_all(&dest_dir);
}

#[test]
fn finalize_filename_applies_ascii_fold_for_late_names() {
    // ASCII fold must cover late-adopted names too: finalize_filename is the single policy point.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("finalize-restrict");
    let _notools = NoVideoTools::apply();
    let settings = test_settings();
    settings.set_boolean("restrict-filenames", true).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let folded = manager.finalize_filename("Café & Croissants.mp4");
    assert!(folded.is_ascii(), "late name must fold to ASCII: {folded}");
    assert_eq!(
        folded,
        restrict_filename_ascii(&shorten_filename("Café & Croissants.mp4"))
    );
    // Pref off: only shortening applies. Set explicitly: shared backend may still hold true.
    let settings = test_settings();
    settings.set_boolean("restrict-filenames", false).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    assert_eq!(
        manager.finalize_filename("Café & Croissants.mp4"),
        shorten_filename("Café & Croissants.mp4")
    );
}

#[cfg(unix)]
#[test]
fn parent_contained_in_dest_cases() {
    let base = std::env::temp_dir().join(format!("grab-parent-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let base_str = base.to_string_lossy().into_owned();

    // The destination itself and a real subdir: contained.
    assert!(parent_contained_in_dest(&base, &base_str));
    let sub = base.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    assert!(parent_contained_in_dest(&sub, &base_str));

    // A planted symlink escaping the destination: refused.
    let target = std::env::temp_dir().join(format!("grab-parent-target-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&target);
    std::fs::create_dir_all(&target).unwrap();
    let link = base.join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(!parent_contained_in_dest(&link, &base_str));

    // Unresolvable paths fail closed.
    assert!(!parent_contained_in_dest(&base.join("nope"), &base_str));

    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&target);
}

#[cfg(unix)]
#[test]
fn ensure_contained_parent_checks_before_creating() {
    // The ordering fix: an existing escaping parent is refused *without*
    // creating the link's target through the link first.
    let base = std::env::temp_dir().join(format!("grab-ensure-parent-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let base_str = base.to_string_lossy().into_owned();

    // Existing real dir: allowed, still a dir.
    let real = base.join("real");
    std::fs::create_dir_all(&real).unwrap();
    assert!(ensure_contained_parent(&real, &base_str).unwrap());
    assert!(real.is_dir());

    // Missing parent: created, then verified.
    let fresh = base.join("fresh");
    assert!(!fresh.exists());
    assert!(ensure_contained_parent(&fresh, &base_str).unwrap());
    assert!(fresh.is_dir());

    // Planted link to an existing outside dir: refused, target untouched.
    let outside = std::env::temp_dir().join(format!("grab-ensure-outside-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&outside);
    std::fs::create_dir_all(&outside).unwrap();
    let link = base.join("link");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    assert!(!ensure_contained_parent(&link, &base_str).unwrap());
    assert!(outside.read_dir().unwrap().next().is_none());

    // Dangling link: refused, and the target is NOT created through it.
    // (create_dir_all on a leaf link hits EEXIST on the link itself, so even
    // the old order created nothing here — this pins the refusal contract.)
    let dangling_target = std::env::temp_dir()
        .join(format!("grab-ensure-dangle-{}", std::process::id()))
        .join("never-created");
    let dangling = base.join("dangling");
    std::os::unix::fs::symlink(&dangling_target, &dangling).unwrap();
    assert!(!ensure_contained_parent(&dangling, &base_str).unwrap());
    assert!(
        !dangling_target.exists(),
        "the link target must not be created through the link"
    );

    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&outside);
}

#[cfg(unix)]
#[test]
fn torrent_folder_refuses_recorded_path_swapped_for_symlink() {
    // Recorded-branch guard: a symlink swapped in for the recorded folder
    // after the finish must not be trusted — `cleanup_unselected`'s `is_dir`
    // check follows the link, and `remove_file` would then delete through it
    // onto the target.
    let id = 928_000 + std::process::id() as u64;
    let base = std::env::temp_dir().join(format!("grab-tfolder-{id}"));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();

    let recorded = base.join("recorded");
    std::fs::create_dir_all(&recorded).unwrap();
    let victim = base.join("victim");
    std::fs::create_dir_all(&victim).unwrap();
    std::fs::write(victim.join("keep.txt"), b"victim").unwrap();

    let item = DownloadItem::new(
        id,
        "magnet:?xt=urn:btih:tfolder9e9aff",
        "t.bin",
        base.to_str().unwrap(),
    );
    // A real recorded dir is still honored (no over-refusal).
    item.set_output_dir(recorded.to_str().unwrap());
    assert_eq!(DownloadManager::torrent_folder(&item), recorded);

    // Post-finish swap: the recorded folder becomes a link onto the victim dir.
    std::fs::remove_dir(&recorded).unwrap();
    std::os::unix::fs::symlink(&victim, &recorded).unwrap();
    assert!(
        recorded.is_dir(),
        "test setup: the link must resolve for is_dir"
    );

    let folder = DownloadManager::torrent_folder(&item);
    assert_ne!(
        folder,
        recorded,
        "torrent_folder trusted the recorded path after it was swapped for a symlink: \
         delete/cleanup would then run through the link onto {}",
        victim.display()
    );

    // Dangling swap: the link target doesn't exist — still refused, since
    // `symlink_metadata` sees the link itself without following it.
    std::fs::remove_file(&recorded).unwrap();
    std::os::unix::fs::symlink(base.join("no-such-target"), &recorded).unwrap();
    let folder = DownloadManager::torrent_folder(&item);
    assert_ne!(
        folder, recorded,
        "torrent_folder trusted a dangling symlink for the recorded path"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn ensure_contained_parent_reports_mkdir_failure_distinctly() {
    // A `create_dir_all` failure is `Err`, not a containment refusal: the
    // caller must not blame the download folder. A file where the dir should
    // be fails the create even as root (no permission-bit trick needed).
    let base = std::env::temp_dir().join(format!("grab-ensure-mkdir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let base_str = base.to_string_lossy().into_owned();

    let blocker = base.join("blocker");
    std::fs::write(&blocker, b"x").unwrap();
    let err = ensure_contained_parent(&blocker, &base_str)
        .expect_err("a file blocking the parent dir must surface the mkdir failure, not Ok");
    assert!(
        !err.to_string().is_empty(),
        "the io error should describe the failing create"
    );

    // Containment behavior is unchanged: existing inside -> Ok(true),
    // existing outside -> Ok(false).
    let inside = base.join("inside");
    std::fs::create_dir_all(&inside).unwrap();
    assert_eq!(ensure_contained_parent(&inside, &base_str).unwrap(), true);
    let outside =
        std::env::temp_dir().join(format!("grab-ensure-mkdir-out-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&outside);
    std::fs::create_dir_all(&outside).unwrap();
    assert_eq!(ensure_contained_parent(&outside, &base_str).unwrap(), false);

    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&outside);
}

/// Build a scheduled item the way the New Download card does: enqueue under a
/// batch guard (so `insert` doesn't spawn the still-Queued item), then stamp
/// the time and flip the status to Scheduled.
fn schedule_test_item(m: &Rc<DownloadManager>, at: i64) -> DownloadItem {
    let _guard = m.batch_guard();
    let item = m
        .enqueue("https://example.com/file.zip", None, None)
        .expect("enqueue should succeed");
    item.set_scheduled_at(at);
    item.set_status(DownloadStatus::Scheduled);
    item
}

#[test]
fn scheduled_item_starts_with_timestamp_and_status() {
    let settings = test_settings();
    let m = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let future = glib::DateTime::now_local()
        .unwrap()
        .add_hours(1)
        .unwrap()
        .to_unix();
    let item = schedule_test_item(&m, future);
    assert_eq!(item.status(), DownloadStatus::Scheduled);
    assert_eq!(item.scheduled_at(), future);
}

#[test]
fn check_scheduled_queues_due_items() {
    let settings = test_settings();
    let m = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    // Schedule 1 hour out, then manually backdate to simulate time passing.
    let future = glib::DateTime::now_local()
        .unwrap()
        .add_hours(1)
        .unwrap()
        .to_unix();
    let item = schedule_test_item(&m, future);
    assert_eq!(item.status(), DownloadStatus::Scheduled);
    // Simulate the scheduled time arriving.
    item.set_scheduled_at(
        glib::DateTime::now_local()
            .unwrap()
            .add_seconds(-10.0)
            .unwrap()
            .to_unix(),
    );
    m.check_scheduled();
    // Due items leave Scheduled; with a free slot start_next picks them up immediately.
    assert!(matches!(
        item.status(),
        DownloadStatus::Queued | DownloadStatus::Downloading
    ));
    assert_eq!(item.scheduled_at(), 0);
}

#[test]
fn check_scheduled_leaves_future_items_alone() {
    let settings = test_settings();
    let m = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let future = glib::DateTime::now_local()
        .unwrap()
        .add_hours(2)
        .unwrap()
        .to_unix();
    let item = schedule_test_item(&m, future);
    m.check_scheduled();
    assert_eq!(item.status(), DownloadStatus::Scheduled);
    assert_eq!(item.scheduled_at(), future);
}

#[test]
fn scheduler_does_not_start_when_preference_off() {
    let settings = test_settings();
    settings
        .set_boolean(crate::settings::key::ENABLE_SCHEDULED_DOWNLOADS, false)
        .unwrap();
    let m = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    m.start_scheduler();
    assert!(
        m.scheduler_source.borrow().is_none(),
        "no 30s timer while the scheduling preference is off"
    );
}

#[test]
fn scheduler_starts_and_stops() {
    let settings = test_settings();
    // The memory backend is process-global: pin the value instead of relying on the schema default.
    settings
        .set_boolean(crate::settings::key::ENABLE_SCHEDULED_DOWNLOADS, true)
        .unwrap();
    let m = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    m.start_scheduler();
    assert!(m.scheduler_source.borrow().is_some());
    // Idempotent: a second start must not install a second timer.
    m.start_scheduler();
    m.stop_scheduler();
    assert!(m.scheduler_source.borrow().is_none());
}

#[test]
fn toggling_preference_stops_and_restarts_scheduler() {
    let settings = test_settings();
    // The memory backend is process-global: pin the starting value.
    settings
        .set_boolean(crate::settings::key::ENABLE_SCHEDULED_DOWNLOADS, true)
        .unwrap();
    let m = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    m.start_scheduler();
    assert!(m.scheduler_source.borrow().is_some());
    settings
        .set_boolean(crate::settings::key::ENABLE_SCHEDULED_DOWNLOADS, false)
        .unwrap();
    assert!(
        m.scheduler_source.borrow().is_none(),
        "disabling the preference removes the 30s timer"
    );
    settings
        .set_boolean(crate::settings::key::ENABLE_SCHEDULED_DOWNLOADS, true)
        .unwrap();
    assert!(m.scheduler_source.borrow().is_some());
    m.stop_scheduler();
}

#[test]
fn dropping_manager_removes_scheduler_source() {
    let settings = test_settings();
    settings
        .set_boolean(crate::settings::key::ENABLE_SCHEDULED_DOWNLOADS, true)
        .unwrap();
    let m = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    m.start_scheduler();
    // Grab the live Source object before dropping the manager.
    let source = {
        let borrow = m.scheduler_source.borrow();
        let id = borrow.as_ref().expect("scheduler source installed");
        glib::MainContext::default()
            .find_source_by_id(id)
            .expect("source present in context")
    };
    drop(m);
    // Drop must remove the 30s tick; a leaked source would survive in the
    // context and trip a later test's thread guard when dispatched.
    assert!(
        source.is_destroyed(),
        "dropping the manager removes the scheduler source"
    );
}

/// If-Range header is sent when the session validator is set.
/// The fixture server logs the If-Range header; we assert it matches.
#[test]
fn piece_sends_if_range_when_validator_set() {
    let Fixture {
        dir,
        dl,
        payload,
        port,
        server,
    } = spawn_fixture("ifrange", "v.bin", 300_000, "0", &[], 44);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let if_range = std::sync::Arc::new(std::sync::OnceLock::new());
    let _ = if_range.set("\"test-etag-123\"".to_string());
    let ctx = FetchCtx {
        client: http_client().clone(),
        url: format!("http://127.0.0.1:{port}/v.bin"),
        dest: dl.join("v.bin"),
        opts: DownloadOptions {
            ..Default::default()
        },
        cookies: None,
        timeout: Duration::from_secs(30),
        tx,
        if_range,
    };
    let total = payload.len() as u64;
    // Fetch a piece; the server logs the If-Range header.
    match tokio_rt().block_on(fetch_piece(&ctx, 0, 1023, total)) {
        Ok(_) => {}
        Err(_) => abort(&server, "piece fetch failed"),
    }
    // Verify the If-Range header was sent with our validator.
    let log = std::fs::read_to_string(dir.join("ranges.log")).unwrap_or_default();
    assert!(
        log.contains("if-range=\"test-etag-123\""),
        "If-Range header not sent or wrong value. Log:\n{log}"
    );
    cleanup(&server, &dir);
}

/// 200 after If-Range yields Changed: when the server's ETag doesn't match
/// our If-Range, it returns 200 (full body) instead of 206. fetch_piece must
/// fail with Changed, not silently accept the wrong bytes.
#[test]
fn piece_rejects_200_after_if_range_mismatch() {
    // Use if-range-strict mode: server returns 200 if If-Range != ETAG.
    let Fixture {
        dir,
        dl,
        payload,
        port,
        server,
    } = spawn_fixture(
        "ifrange200",
        "v.bin",
        300_000,
        "0",
        &["", "if-range-strict", "\"server-etag\""],
        45,
    );
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let if_range = std::sync::Arc::new(std::sync::OnceLock::new());
    // Our validator differs from the server's ETag.
    let _ = if_range.set("\"client-etag\"".to_string());
    let ctx = FetchCtx {
        client: http_client().clone(),
        url: format!("http://127.0.0.1:{port}/v.bin"),
        dest: dl.join("v.bin"),
        opts: DownloadOptions {
            ..Default::default()
        },
        cookies: None,
        timeout: Duration::from_secs(30),
        tx,
        if_range,
    };
    let total = payload.len() as u64;
    match tokio_rt().block_on(fetch_piece(&ctx, 0, 1023, total)) {
        Err(AttemptFail::Changed(_)) => {}
        Ok(_) => abort(
            &server,
            "200 after If-Range mismatch must fail Changed, not succeed",
        ),
        Err(_) => abort(&server, "wrong error kind"),
    }
    // Prove the 200 path was taken: the server logged our If-Range header,
    // and in strict mode a mismatched If-Range yields 200 (not 206). The
    // 206-validator-mismatch path cannot trigger here.
    let log = std::fs::read_to_string(dir.join("ranges.log")).unwrap_or_default();
    assert!(
        log.contains("if-range=\"client-etag\""),
        "server never saw our If-Range; strict mode not engaged. Log:\n{log}"
    );
    cleanup(&server, &dir);
}

/// 206 with mismatched validator yields Changed: when the server ignores
/// If-Range (returns 206 anyway) but the response ETag differs from our
/// session validator, fetch_piece must fail with Changed, not splice versions.
#[test]
fn piece_rejects_206_with_mismatched_validator() {
    // Normal mode (not strict): server returns 206 with its ETag even when
    // If-Range doesn't match. Our validator differs from the server's ETag.
    let Fixture {
        dir: _dir,
        dl,
        payload,
        port,
        server,
    } = spawn_fixture(
        "ifrange206",
        "v.bin",
        300_000,
        "0",
        &["", "", "\"server-etag-v2\""],
        46,
    );
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let if_range = std::sync::Arc::new(std::sync::OnceLock::new());
    // Our validator differs from the server's ETag ("server-etag-v2").
    let _ = if_range.set("\"client-etag-v1\"".to_string());
    let ctx = FetchCtx {
        client: http_client().clone(),
        url: format!("http://127.0.0.1:{port}/v.bin"),
        dest: dl.join("v.bin"),
        opts: DownloadOptions {
            ..Default::default()
        },
        cookies: None,
        timeout: Duration::from_secs(30),
        tx,
        if_range,
    };
    let total = payload.len() as u64;
    match tokio_rt().block_on(fetch_piece(&ctx, 0, 1023, total)) {
        Err(AttemptFail::Changed(_)) => {}
        Ok(_) => abort(
            &server,
            "206 with mismatched validator must fail Changed, not succeed",
        ),
        Err(_) => abort(&server, "wrong error kind"),
    }
    cleanup(&server, &_dir);
}

/// Probe validator is persisted via EngineMsg::Validator: when a fresh
/// segmented download starts, the probe's ETag must be sent through the
/// channel before workers proceed, so pause/restart can resume with If-Range.
#[test]
fn probe_validator_sent_before_workers() {
    let Fixture {
        dir,
        dl,
        payload: _payload,
        port,
        server,
    } = spawn_fixture(
        "probeval",
        "v.bin",
        5_000_000,
        "0",
        &["", "", "\"probe-etag-456\""],
        47,
    );
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let ctx = FetchCtx {
        client: http_client().clone(),
        url: format!("http://127.0.0.1:{port}/v.bin"),
        dest: dl.join("v.bin"),
        opts: DownloadOptions {
            ..Default::default()
        },
        cookies: None,
        timeout: Duration::from_secs(30),
        tx,
        if_range: std::sync::Arc::new(std::sync::OnceLock::new()),
    };
    // Run the download in the background; we only care about the Validator message.
    let dl_clone = dl.clone();
    let handle = std::thread::spawn(move || {
        tokio_rt().block_on(run_download(ctx, 4, StartMode::Fresh));
        let _ = std::fs::remove_dir_all(dl_clone);
    });
    // Wait for the Validator message (probe must send it before workers start).
    let validator = tokio_rt().block_on(async {
        let timeout = tokio::time::timeout(Duration::from_secs(30), async {
            while let Some(msg) = rx.recv().await {
                if let EngineMsg::Validator(info) = msg {
                    return Some(info);
                }
                // Stop waiting once the download finishes or fails.
                if matches!(msg, EngineMsg::Finished { .. } | EngineMsg::Failed(_)) {
                    return None;
                }
            }
            None
        });
        timeout.await.unwrap_or(None)
    });
    let _ = handle.join();
    let info = validator.unwrap_or_else(|| abort(&server, "probe never sent EngineMsg::Validator"));
    assert_eq!(
        info.etag.as_deref(),
        Some("\"probe-etag-456\""),
        "validator ETag must match the probe response"
    );
    cleanup(&server, &dir);
}

/// StoredItem validator persistence: etag/last_modified round-trip through
/// JSON serialization, so cross-session resume has the validator.
#[test]
fn stored_item_validator_roundtrip() {
    let mut item = stored_row(
        "http://example.com/f.bin",
        "/tmp",
        "f.bin",
        DownloadStatus::Paused,
    );
    item.etag = Some("\"test-etag-789\"".to_string());
    item.last_modified = Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string());

    let json = serde_json::to_string(&item).expect("serialize");
    assert!(json.contains("test-etag-789"), "etag not persisted: {json}");
    assert!(
        json.contains("Wed, 21 Oct 2015"),
        "last_modified not persisted: {json}"
    );

    let restored: StoredItem = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(
        restored.etag.as_deref(),
        Some("\"test-etag-789\""),
        "etag not restored"
    );
    assert_eq!(
        restored.last_modified.as_deref(),
        Some("Wed, 21 Oct 2015 07:28:00 GMT"),
        "last_modified not restored"
    );
}

/// Old queue compatibility: a queue file predating the validator fields
/// (no etag/last_modified keys) must deserialize with None, not fail.
#[test]
fn old_queue_without_validators_deserializes() {
    // Simulate an old queue file: StoredItem JSON without etag/last_modified.
    let old_json = r#"{
        "id": null,
        "url": "http://example.com/old.bin",
        "dest_dir": "/tmp",
        "filename": "old.bin",
        "status": "paused",
        "progress": 0.5,
        "segments": null,
        "selected_files": null,
        "output_dir": null,
        "video_source": null,
        "started": true,
        "scheduled_at": null
    }"#;
    let item: StoredItem =
        serde_json::from_str(old_json).expect("old queue format must deserialize");
    assert_eq!(item.etag, None, "missing etag must default to None");
    assert_eq!(
        item.last_modified, None,
        "missing last_modified must default to None"
    );
    assert_eq!(item.url, "http://example.com/old.bin");
}

#[test]
fn persist_task_loop_delivers_final_value_before_close() {
    // The watch channel delivers the final send_replace value before the
    // closed-channel Err, so the last write lands before the task exits.
    use std::sync::{Arc, Mutex};
    let writes = Arc::new(Mutex::new(Vec::new()));
    let writes_clone = writes.clone();
    let (tx, rx) = tokio::sync::watch::channel(None::<String>);
    let task = crate::runtime::tokio_rt().spawn(async move {
        crate::download::DownloadManager::persist_task_loop(rx, move |text| {
            writes_clone.lock().unwrap().push(text);
        })
        .await;
    });
    tx.send_replace(Some("A".to_string()));
    tx.send_replace(Some("B".to_string()));
    drop(tx);
    crate::runtime::tokio_rt().block_on(task).unwrap();
    let writes = writes.lock().unwrap();
    assert_eq!(
        writes.last().unwrap(),
        "B",
        "final value must be last write"
    );
}
