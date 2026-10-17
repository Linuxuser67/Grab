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

/// Lock a worker-shared mutex, recovering on poison so one panic fails the item, not the app.
pub(crate) fn lock_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    // NOTE: `two_slow_tasks_do_not_block_third_on_two_workers` was removed:
    // it tested tokio's scheduler (timing margins on a 2-worker runtime),
    // not Grab code, and its rationale referenced `block_on_sync`, which no
    // longer exists.
}
