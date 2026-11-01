//! Shared async runtime and fail-fast locking for worker-shared state.

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

/// Lock worker-shared state, failing fast if a panic may have left it inconsistent.
pub(crate) fn lock_or_panic<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().expect("worker-shared mutex poisoned")
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poisoned_worker_mutex_fails_fast() {
        let mutex = Mutex::new(());
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = mutex.lock().expect("initial mutex lock");
            panic!("poison the test mutex");
        }));

        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                drop(lock_or_panic(&mutex));
            }))
            .is_err(),
            "poisoned worker state must not be silently reused"
        );
    }
}
