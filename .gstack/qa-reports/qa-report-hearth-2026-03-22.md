# QA Report: Hearth

**Date:** 2026-03-22
**Branch:** feat/phase2-mcp-dev-services
**Base:** main
**Runtime:** Rust (cargo test)
**Tier:** Standard
**Mode:** Diff-aware (Rust CLI/daemon, no web UI)

---

## Summary

| Metric | Value |
|--------|-------|
| Issues found | 4 |
| Fixes applied | 0 |
| Tests before | 51 |
| Tests after | 51 |
| Deferred | 4 |
| Reverted | 0 |

**PR Summary:** QA found 4 issues (1 medium, 3 low). All deferred — they are pre-existing patterns or non-critical design notes, not regressions from Phase 2. Test suite healthy at 51 tests, zero new clippy warnings.

---

## Health Score

| Category | Score | Weight | Weighted |
|----------|-------|--------|----------|
| Functional | 100 | 20% | 20.0 |
| Console (clippy) | 70 | 15% | 10.5 |
| Code Quality | 92 | 15% | 13.8 |
| Testing | 95 | 20% | 19.0 |
| Architecture | 100 | 15% | 15.0 |
| Documentation | 90 | 15% | 13.5 |
| **Total** | | | **91.8** |

---

## Issues

### QA-001 — `hearth_site_link` ignores `path` parameter [MEDIUM]
**Category:** Design Gap
**Status:** deferred
**Files:** `crates/hearth-lib/src/mcp.rs:238-257`
**Description:** The `hearth_site_link` MCP tool accepts a `path: String` parameter in `SiteLinkParams`, but `ValetCli::link()` uses the current working directory. The MCP tool calls `std::env::set_current_dir(&path)` inside `spawn_blocking`, which is a global side effect that could race with other async tasks. In practice this is unlikely to cause issues since the daemon processes requests sequentially per-connection, but it's architecturally incorrect.
**Recommendation:** Modify `ValetCli::link()` to accept an optional `working_dir` parameter and use `.current_dir(path)` on the `Command`, rather than mutating global state.

### QA-002 — Pre-existing clippy warnings in Phase 1 code [LOW]
**Category:** Code Quality
**Status:** deferred
**Files:** `crates/hearth-lib/src/php/resolver.rs:20,36,58,72`, `crates/hearth-lib/src/service/supervisor.rs:191`, `crates/hearth-cli/src/main.rs:217,229`
**Description:** 7 clippy warnings exist, all in Phase 1 code: `&PathBuf` instead of `&Path` (2), collapsible `if` (3), `print_literal` (2). Phase 2 introduced zero new warnings.
**Recommendation:** Fix in a separate cleanup PR to keep Phase 2 PR focused.

### QA-003 — `download_github_release` uses blocking I/O in async function [LOW]
**Category:** Code Quality
**Status:** deferred
**Files:** `crates/hearth-lib/src/download.rs:78-107`
**Description:** The `download_github_release` async function uses `std::fs::create_dir_all`, `std::fs::write`, and `std::fs::remove_file` which are blocking I/O operations. Per the project's Rust rules, these should use `tokio::fs` or `spawn_blocking`. However, this function is called explicitly by `hearth install` (not in a hot path), and the files are small, so the practical impact is negligible.
**Recommendation:** Replace with `tokio::fs` equivalents in a future cleanup pass.

### QA-004 — `stream_dumps` never returns Ok(()) [LOW]
**Category:** Design Note
**Status:** deferred
**Files:** `crates/hearth-lib/src/dump.rs:129-164`
**Description:** The rewritten `stream_dumps` runs an infinite reconnect loop and never returns `Ok(())`. The calling code in CLI does `hearth_lib::dump::stream_dumps(config.dump_port).await?;` followed by `return Ok(());`. This is intentional (user Ctrl+C to exit), but the function signature `-> anyhow::Result<()>` is misleading since the `Ok` case is unreachable. The return type could be `-> !` (never type) for clarity, but that's unstable for async functions.
**Recommendation:** Add a doc comment clarifying this is intentional. Already partially done.

---

## Remaining Coverage Gaps (Deferred — Integration Tests)

These were identified during Phase 2 planning and are appropriate for later phases:

- **MCP Streamable HTTP end-to-end** — spawn daemon, connect rmcp client, call all 8 tools
- **MCP stdio bridge** — spawn `hearth mcp`, pipe JSON-RPC, verify responses
- **Mailpit download** — hit real GitHub API (CI-only, skip in local test)
- **Daemon socket communication** — CLI ↔ daemon round-trip integration test
- **Process group supervision** — real process spawn/kill/health-check

---

## Verification Summary

| Check | Result |
|-------|--------|
| `cargo test --workspace` | 51 passed, 0 failed |
| `cargo clippy --workspace` | 0 new warnings (7 pre-existing from Phase 1) |
| `cargo check --workspace` | Clean compile |
| Lock ordering consistency | Verified: `config → php_manager → site_manager → supervisor` in both `mcp.rs` and `daemon/main.rs` |
| DaemonState refactoring | All 13 `process_request` arms correctly use per-field locks |
| MCP server integration | Compiles, tests pass, HTTP listener spawns non-fatally |
| Config migration safety | `#[serde(default)]` tested with legacy config files |
