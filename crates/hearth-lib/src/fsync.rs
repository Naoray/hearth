//! Directory-durability barrier for atomic renames and removals.
//!
//! A file `sync_all` before rename does NOT durably commit the rename itself:
//! the new directory entry lives in the parent directory, which must be
//! fsynced separately. Every temp→destination rename and every managed-file
//! removal in Hearth calls [`sync_dir`] on the containing directory before
//! the operation is reported successful.
//!
//! Under `cfg(test)` this module also provides deterministic failpoints
//! (exact-directory match) and an ordered event log, so crash-ordering
//! invariants are provable without timing or power-loss theater.

use std::path::Path;

/// Open and fsync a directory, committing rename/unlink entries within it.
/// Failures propagate — callers must not report success past a failed barrier.
pub(crate) fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if test_hooks::should_fail_sync(dir) {
        return Err(std::io::Error::other(format!(
            "injected directory-sync failure for {}",
            dir.display()
        )));
    }
    let handle = std::fs::File::open(dir)?;
    handle.sync_all()?;
    #[cfg(test)]
    test_hooks::record(format!("sync:{}", dir.display()));
    Ok(())
}

/// Record a completed rename (test-observable ordering event).
pub(crate) fn note_rename(_dst: &Path) {
    #[cfg(test)]
    test_hooks::record(format!("rename:{}", _dst.display()));
}

/// Record a completed removal (test-observable ordering event).
pub(crate) fn note_remove(_path: &Path) {
    #[cfg(test)]
    test_hooks::record(format!("remove:{}", _path.display()));
}

#[cfg(test)]
pub(crate) mod test_hooks {
    use std::cell::RefCell;
    use std::path::Path;

    struct SyncFailpoint {
        dir: String,
        /// 1-based index of the matching sync attempt to fail.
        nth: usize,
        /// Matching sync attempts observed so far.
        seen: usize,
    }

    thread_local! {
        static EVENTS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
        static FAILPOINTS: RefCell<Vec<SyncFailpoint>> = const { RefCell::new(Vec::new()) };
    }

    /// RAII registration of one sync failpoint. Deregisters on drop — normal
    /// return AND panic unwind — so failpoints never leak across tests.
    /// Thread-local storage keeps parallel tests isolated.
    #[must_use = "the failpoint is active only while this guard lives"]
    pub(crate) struct FailpointGuard {
        dir: String,
        nth: usize,
    }

    impl Drop for FailpointGuard {
        fn drop(&mut self) {
            FAILPOINTS.with(|f| {
                let mut points = f.borrow_mut();
                if let Some(idx) = points
                    .iter()
                    .position(|p| p.dir == self.dir && p.nth == self.nth)
                {
                    points.remove(idx);
                }
            });
        }
    }

    /// Clear the event log (failpoints are lifetime-managed by their guards).
    pub(crate) fn reset() {
        EVENTS.with(|e| e.borrow_mut().clear());
    }

    /// Fail the FIRST sync attempt for one exact directory path.
    /// Exact match — a parent dir failpoint never triggers for a child dir.
    pub(crate) fn fail_sync_of(dir: &Path) -> FailpointGuard {
        fail_nth_sync_of(dir, 1)
    }

    /// Fail exactly the `nth` (1-based) matching sync attempt for one exact
    /// directory path; earlier and later matching attempts succeed. This makes
    /// same-directory phases independently selectable (e.g. the pending
    /// manifest save is attempt 1, its finalization is attempt 2).
    pub(crate) fn fail_nth_sync_of(dir: &Path, nth: usize) -> FailpointGuard {
        assert!(nth >= 1, "nth is 1-based");
        let dir = dir.display().to_string();
        FAILPOINTS.with(|f| {
            f.borrow_mut().push(SyncFailpoint {
                dir: dir.clone(),
                nth,
                seen: 0,
            })
        });
        FailpointGuard { dir, nth }
    }

    pub(crate) fn should_fail_sync(dir: &Path) -> bool {
        let dir = dir.display().to_string();
        FAILPOINTS.with(|f| {
            let mut fail = false;
            for point in f.borrow_mut().iter_mut() {
                if point.dir == dir {
                    point.seen += 1;
                    if point.seen == point.nth {
                        fail = true;
                    }
                }
            }
            fail
        })
    }

    pub(crate) fn record(event: String) {
        EVENTS.with(|e| e.borrow_mut().push(event));
    }

    /// Ordered barrier events observed on this test thread.
    pub(crate) fn events() -> Vec<String> {
        EVENTS.with(|e| e.borrow().clone())
    }

    /// Index of the first occurrence of `event`, or panic with the log.
    pub(crate) fn index_of(events: &[String], event: &str) -> usize {
        events
            .iter()
            .position(|e| e == event)
            .unwrap_or_else(|| panic!("event '{event}' not found in {events:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failpoint_selects_nth_sync_independently() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().canonicalize().unwrap();

        {
            let _fp = test_hooks::fail_nth_sync_of(&dir, 2);
            assert!(sync_dir(&dir).is_ok(), "attempt 1 must pass");
            assert!(sync_dir(&dir).is_err(), "attempt 2 alone must fail");
            assert!(sync_dir(&dir).is_ok(), "attempt 3 must pass again");
        }
        // Guard dropped — no leakage into subsequent syncs.
        assert!(sync_dir(&dir).is_ok());
    }

    #[test]
    fn failpoint_guard_cleans_up_on_panic() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let dir2 = dir.clone();

        let result = std::panic::catch_unwind(move || {
            let _fp = test_hooks::fail_sync_of(&dir2);
            panic!("boom");
        });
        assert!(result.is_err());
        assert!(
            sync_dir(&dir).is_ok(),
            "failpoint must be deregistered by the unwind"
        );
    }
}
