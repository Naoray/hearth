//! Panic-safe, serialized environment mutation for tests (review 5594 B2-6).
//!
//! Every test that touches process-global environment variables MUST go
//! through [`EnvGuard`]: a shared static mutex serializes all env-mutating
//! tests (parallel test threads cannot race), and the guard captures the
//! exact prior `Option<OsString>` of every key it touches, restoring it in
//! `Drop` — including on panic unwind.

use std::ffi::{OsStr, OsString};
use std::sync::{Mutex, MutexGuard, PoisonError};

static ENV_LOCK: Mutex<()> = Mutex::new(());

pub(crate) struct EnvGuard {
    saved: Vec<(OsString, Option<OsString>)>,
    _lock: MutexGuard<'static, ()>,
}

impl EnvGuard {
    /// Acquire the serial env lock and snapshot `keys` for restoration.
    /// A poisoned lock (a previous env test panicked) is safe to reuse
    /// because Drop already restored that test's values.
    pub(crate) fn capture<I, K>(keys: I) -> Self
    where
        I: IntoIterator<Item = K>,
        K: AsRef<OsStr>,
    {
        let lock = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let saved = keys
            .into_iter()
            .map(|k| {
                let key = k.as_ref().to_os_string();
                let prior = std::env::var_os(&key);
                (key, prior)
            })
            .collect();
        Self { saved, _lock: lock }
    }

    pub(crate) fn set(&self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) {
        assert!(
            self.saved.iter().any(|(k, _)| k == key.as_ref()),
            "EnvGuard::set on uncaptured key {:?}",
            key.as_ref()
        );
        // SAFETY: all env mutation in this crate's tests is serialized by
        // ENV_LOCK held for this guard's lifetime.
        unsafe { std::env::set_var(key, value) };
    }

    pub(crate) fn remove(&self, key: impl AsRef<OsStr>) {
        assert!(
            self.saved.iter().any(|(k, _)| k == key.as_ref()),
            "EnvGuard::remove on uncaptured key {:?}",
            key.as_ref()
        );
        // SAFETY: as above.
        unsafe { std::env::remove_var(key) };
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, prior) in self.saved.drain(..) {
            // SAFETY: still holding ENV_LOCK via _lock (dropped after us).
            unsafe {
                match prior {
                    Some(value) => std::env::set_var(&key, value),
                    None => std::env::remove_var(&key),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_preexisting_non_unicode_value() {
        use std::os::unix::ffi::OsStrExt;
        let key = "HEARTH_TEST_ENV_GUARD_NON_UNICODE";
        let weird = OsStr::from_bytes(b"pre\xFFexisting");

        // Phase 1 (under the same serial lock): plant a pre-existing
        // non-Unicode value as ambient state.
        {
            let lock = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
            unsafe { std::env::set_var(key, weird) };
            drop(lock);
        }

        // Phase 2: a guarded "test" overwrites and even removes the var —
        // Drop must restore the exact prior non-Unicode OsString.
        {
            let guard = EnvGuard::capture([key]);
            guard.set(key, "overwritten");
            assert_eq!(
                std::env::var_os(key).as_deref(),
                Some(OsStr::new("overwritten"))
            );
            guard.remove(key);
            assert_eq!(std::env::var_os(key), None);
        }
        assert_eq!(
            std::env::var_os(key).as_deref(),
            Some(weird),
            "exact prior non-Unicode value restored by Drop"
        );

        // Cleanup under the lock.
        let lock = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        unsafe { std::env::remove_var(key) };
        drop(lock);
    }

    #[test]
    fn restores_prior_state_on_panic_unwind() {
        let key = "HEARTH_TEST_ENV_GUARD_PANIC";
        assert_eq!(std::env::var_os(key), None);
        let result = std::panic::catch_unwind(|| {
            let guard = EnvGuard::capture([key]);
            guard.set(key, "panicking-value");
            panic!("boom");
        });
        assert!(result.is_err());
        // Unwind ran Drop: the captured pre-state (absent) is restored and
        // the (poisoned) lock is released for the next capture.
        assert_eq!(std::env::var_os(key), None);
        drop(EnvGuard::capture([key])); // lock reusable after poison
    }

    #[test]
    fn serializes_concurrent_env_tests() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let key = "HEARTH_TEST_ENV_GUARD_SERIAL";
        let inside = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for i in 0..4 {
            let inside = Arc::clone(&inside);
            handles.push(std::thread::spawn(move || {
                let guard = EnvGuard::capture([key]);
                let now = inside.fetch_add(1, Ordering::SeqCst);
                assert_eq!(now, 0, "another env-mutating test ran concurrently");
                guard.set(key, format!("t{i}"));
                std::thread::sleep(std::time::Duration::from_millis(10));
                inside.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(std::env::var_os(key), None);
    }
}
