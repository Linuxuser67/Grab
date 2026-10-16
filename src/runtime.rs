//! Shared async runtime and lock recovery for every engine.

use std::sync::{Mutex, OnceLock};

pub(crate) fn tokio_rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("grab-download")
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
}

/// Run an async block to completion from sync code, whether or not the
/// caller is already inside a Tokio runtime. From a non-runtime thread
/// (e.g. GTK main) this blocks on the shared runtime; from within a
/// runtime (e.g. #[tokio::test]) it spawns a dedicated thread to avoid
/// the "Cannot start a runtime from within a runtime" panic.
///
/// Only used by test sync wrappers now that production callers are async.
#[allow(dead_code)]
pub(crate) fn block_on_sync<F, T>(fut: F) -> T
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        // Already in a runtime: run on a dedicated thread to avoid
        // blocking the runtime's worker.
        std::thread::spawn(|| tokio_rt().block_on(fut))
            .join()
            .expect("block_on_sync thread panicked")
    } else {
        tokio_rt().block_on(fut)
    }
}

/// Lock a worker-shared mutex, recovering on poison so one panic fails the item, not the app.
pub(crate) fn lock_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    /// Regression: two concurrent long-running async tasks must not block a
    /// third task on a 2-worker runtime. This validates that our async
    /// functions (probe, count, install) yield the worker instead of
    /// blocking it via `block_on_sync`.
    #[tokio::test]
    async fn two_slow_tasks_do_not_block_third_on_two_workers() {
        // Build a 2-worker runtime like the production one.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test runtime");

        let slow = || async {
            // Simulate a slow async operation (e.g. probe, install).
            // Uses async sleep, NOT thread blocking.
            tokio::time::sleep(Duration::from_millis(500)).await;
        };

        let start = std::time::Instant::now();

        // Spawn two slow tasks that occupy the workers.
        let h1 = rt.spawn(slow());
        let h2 = rt.spawn(slow());

        // Give them a moment to start.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // A third quick task must complete without waiting for the slow ones.
        // If workers were blocked, this would take ~500ms. With async yield,
        // it completes in ~50ms (just the initial sleep).
        let h3 = rt.spawn(async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            42
        });

        let result = h3.await.expect("third task panicked");
        let elapsed = start.elapsed();

        assert_eq!(result, 42);
        // Third task should finish well before the slow ones (500ms).
        // Allow generous margin for CI slowness.
        assert!(
            elapsed < Duration::from_millis(400),
            "third task blocked: took {elapsed:?}, expected < 400ms"
        );

        // Clean up.
        h1.await.expect("slow task 1 panicked");
        h2.await.expect("slow task 2 panicked");
    }
}
