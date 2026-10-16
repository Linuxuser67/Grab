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
