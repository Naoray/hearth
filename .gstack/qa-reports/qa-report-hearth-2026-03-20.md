# QA Report: Hearth

**Date:** 2026-03-20
**Branch:** feat/phase1-handlers-and-dump-server
**Base:** main
**Runtime:** Rust (cargo test)
**Tier:** Standard
**Mode:** Diff-aware (Rust CLI/daemon, no web UI)

---

## Summary

| Metric | Value |
|--------|-------|
| Issues found | 6 |
| Fixes applied | 6 (verified: 6) |
| Tests before | 19 |
| Tests after | 35 (+16 new) |
| Deferred | 0 |
| Reverted | 0 |

**PR Summary:** QA found 6 issues (test gaps + 1 clippy warning), fixed all 6, test count 19 → 35.

---

## Issues

### QA-001 — Supervisor start/stop error on unregistered service [HIGH]
**Category:** Test Gap
**Status:** verified
**Commit:** 021ec1b
**Files Changed:** `crates/hearth-lib/src/service/supervisor.rs`
**Description:** The review added error returns for unregistered services in `start_service`/`stop_service`, but no tests verified this behavior.
**Fix:** Added 2 tests covering error paths for unregistered services.

### QA-002 — Supervisor reconfigure and status tests [HIGH]
**Category:** Test Gap
**Status:** verified
**Commit:** 021ec1b (same commit as QA-001)
**Files Changed:** `crates/hearth-lib/src/service/supervisor.rs`
**Description:** `reconfigure_service`, `status`, `register` had no test coverage.
**Fix:** Added 6 tests: reconfigure updates command, reconfigure unregistered is noop, status returns services, register replaces existing, start/stop succeed for registered.

### QA-003 — Dump server helper and broadcast tests [MEDIUM]
**Category:** Test Gap
**Status:** verified
**Commit:** e05620f
**Files Changed:** `crates/hearth-lib/src/dump.rs`
**Description:** Entire dump.rs module (136 LOC) had zero tests, including the new broadcast architecture.
**Fix:** Added 3 tests: `dump_addr` returns loopback, `relay_port` arithmetic, integration test proving VarDumper→broadcast→subscriber data flow works.

### QA-004 — PHP version scanning tests [MEDIUM]
**Category:** Test Gap
**Status:** verified
**Commit:** 19489c0
**Files Changed:** `crates/hearth-lib/src/php.rs`
**Description:** `installed_versions_with_paths` was expanded to scan 3 sources but had no tests for the Hearth cache scanning logic.
**Fix:** Added 3 tests: cache entries with binary found, entries without binary excluded, Hearth cache takes priority over system sources.

### QA-005 — TLD-aware isolation lookup tests [MEDIUM]
**Category:** Test Gap
**Status:** verified
**Commit:** f3a23be
**Files Changed:** `crates/hearth-lib/src/site.rs`
**Description:** The review fix added configurable TLD to `SiteManager`, but no test verified that a non-default TLD works or that the `.test` fallback was removed.
**Fix:** Added 2 tests: custom TLD matches isolation file while non-matching TLD doesn't, bare filename (no TLD suffix) works as fallback.

### QA-006 — Clippy field_reassign_with_default [LOW]
**Category:** Code Quality
**Status:** verified
**Commit:** af67680
**Files Changed:** `crates/hearth-lib/src/config.rs`
**Description:** Config test used `Default::default()` then assigned fields individually instead of struct update syntax.
**Fix:** Changed to `HearthConfig { default_php: "8.3".to_string(), ..HearthConfig::default() }`.

---

## Remaining Coverage Gaps (Deferred — Integration Tests)

These require external dependencies or process spawning and are appropriate for a later phase:

- **Daemon request handlers** (14 handlers in `hearth-daemon/src/main.rs`) — need mock supervisor/managers
- **Valet CLI wrappers** (`valet.rs`) — shell out to real `valet` binary
- **CLI ↔ daemon socket communication** — end-to-end integration test
- **Process group supervision** — real process spawn/kill/health-check
