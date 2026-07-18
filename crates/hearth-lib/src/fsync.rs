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

    thread_local! {
        static EVENTS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
        static FAIL_SYNC_DIRS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    /// Clear the event log and all failpoints (call at test start and end).
    pub(crate) fn reset() {
        EVENTS.with(|e| e.borrow_mut().clear());
        FAIL_SYNC_DIRS.with(|f| f.borrow_mut().clear());
    }

    /// Inject a persistent sync failure for one exact directory path.
    /// Exact match — a parent dir failpoint never triggers for a child dir.
    pub(crate) fn fail_sync_of(dir: &Path) {
        FAIL_SYNC_DIRS.with(|f| f.borrow_mut().push(dir.display().to_string()));
    }

    pub(crate) fn should_fail_sync(dir: &Path) -> bool {
        let dir = dir.display().to_string();
        FAIL_SYNC_DIRS.with(|f| f.borrow().contains(&dir))
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
