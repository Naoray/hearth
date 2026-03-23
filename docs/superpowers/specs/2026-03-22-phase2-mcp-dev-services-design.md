# Phase 2 Design: MCP Server + Dev Services

**Date:** 2026-03-22
**Status:** Approved
**Branch:** TBD (feature branch from main)
**Builds on:** Phase 1 (v0.1.1.0) — daemon, CLI, supervisor, site/PHP management, dump server

## Overview

Phase 2 adds four capabilities to Hearth:

1. **MCP server** — in-process Model Context Protocol server exposing Hearth tools to IDEs
2. **Mailpit managed service** — mail catcher with binary resolution chain and supervisor integration
3. **Dump server CLI polish** — timestamped output with auto-reconnect
4. **CLI distribution** — proper binary names, Homebrew formula, CI release workflow

SSL integration was planned for Phase 2 but is already complete from Phase 1 (CLI commands, daemon handlers, and site listing all functional).

## 1. MCP Server

### Architecture

A `HearthMcpServer` struct in `hearth-lib/src/mcp.rs` implements rmcp's `ServerHandler` trait using `#[tool_router]` for tool method definitions and `#[tool_handler]` on the `ServerHandler` impl block. Individual tools use the `#[tool(description = "...")]` attribute.

The MCP server holds individual `Arc<Mutex<T>>` references to the managers it needs (`ServiceSupervisor`, `SiteManager`, `PhpManager`, `HearthConfig`) rather than a reference to `DaemonState`. This is necessary because `DaemonState` is a private struct in the daemon binary crate, and `hearth-lib` cannot depend on the daemon. The daemon constructs `HearthMcpServer` by passing the same `Arc<Mutex<T>>` references it already holds. No serialization overhead; direct method calls.

### Transports

Two transports, addressing the original Herd orphan-process problem:

1. **Streamable HTTP** — The daemon listens on `127.0.0.1:{mcp_port}` (default `9900`) using rmcp's `StreamableHttpService` with an axum router at the `/mcp` endpoint. IDEs connect via URL (`http://127.0.0.1:9900/mcp`). Zero spawned processes — zero orphan risk by design. This is the primary transport. Note: MCP has deprecated the older SSE transport in favor of Streamable HTTP.

2. **Stdio bridge** — A `hearth mcp` CLI subcommand for clients that only support stdio transport. It acts as an HTTP-to-stdio proxy: reads JSON-RPC from stdin, POSTs to the daemon's Streamable HTTP endpoint (`http://127.0.0.1:{mcp_port}/mcp`), and writes responses to stdout. No new socket protocol or `DaemonRequest` variant needed — it's a simple HTTP client relay. If the IDE crashes, `hearth mcp` gets EOF on stdin and exits cleanly. The daemon's MCP handler stays alive.

**Port conflict handling:** If the MCP port is already in use, the daemon logs a clear error (`MCP server failed to bind port {mcp_port}: address already in use`) but continues running — the MCP server is optional, and the core Unix socket API + services should not be blocked by an MCP port conflict.

### MCP Tools (8)

| Tool | Parameters | Returns | Maps to |
|------|-----------|---------|---------|
| `hearth_status` | none | service name, state, pid for each service | `supervisor.status()` |
| `hearth_sites` | none | site name, path, ssl flag, php version for each site | `site_manager.list_sites()` |
| `hearth_php_list` | none | version, binary path, active flag for each version | `php_manager.installed_versions_with_paths()` |
| `hearth_php_switch` | `version: String` | confirmation message | stop/reconfigure/start php-fpm |
| `hearth_site_link` | `path: String, name: Option<String>` | confirmation message | `ValetCli::link()` |
| `hearth_site_unlink` | `name: String` | confirmation message | `ValetCli::unlink()` |
| `hearth_service_restart` | `service: Option<String>` | confirmation message | supervisor stop/start |
| `hearth_php_config` | `key: String, value: String` | confirmation message | `php_manager.set_ini_value()` + fpm restart |

All tools reuse existing business logic — no new domain code needed. The MCP module is purely a protocol adapter.

### Config Addition

```toml
# Added to HearthConfig
mcp_port = 9900
```

**Migration note:** The `mcp_port` field must use `#[serde(default = "default_mcp_port")]` so existing `config.toml` files from Phase 1 (which lack this key) don't fail to deserialize. Better yet, add `#[serde(default)]` to the entire `HearthConfig` struct since `Default::default()` already provides correct values for all fields. This future-proofs all subsequent field additions.

## 2. Mailpit Managed Service

### Binary Resolution Chain

New module `hearth-lib/src/mailpit.rs` with the same pattern as PHP resolution:

1. `~/.config/hearth/services/mailpit/mailpit` — downloaded binary (primary)
2. System PATH (`which mailpit`) — Homebrew or manual install
3. Download from GitHub Releases — `axllent/mailpit`, detect `darwin-arm64`/`darwin-amd64` asset

### Download Utility

New module `hearth-lib/src/download.rs` — reusable for future PHP binary downloads:

- `download_github_release(repo: &str, asset_pattern: &str, dest: &Path)` — hits GitHub Releases API, finds latest release, downloads matching asset, extracts to destination
- Uses `reqwest` (already a workspace dependency)
- Called explicitly by `hearth install` or `hearth mailpit install`, not implicitly on startup

### Supervisor Registration

Mailpit registered in `default_services()` with existing config ports:

```rust
ManagedService::new(
    ServiceKind::Mailpit,
    resolved_mailpit_path,
    vec![
        "--smtp", &format!("127.0.0.1:{}", config.mail_smtp_port),   // 1025
        "--listen", &format!("127.0.0.1:{}", config.mail_ui_port),   // 8025
        "--db-file", &config_dir.join("services/mailpit/mailpit.db"),
    ],
)
```

### Conditional Registration

Mailpit is only registered if the binary is found. If missing, `hearth status` shows `mailpit: not installed` rather than failing at startup. This avoids breaking the daemon for users who haven't opted into mail catching.

## 3. Dump Server CLI Streaming

### Current State

`stream_dumps()` in `dump.rs` does raw `print!("{}", line)` with no framing.

### Changes

Minimal enhancement:

1. **Dim timestamp prefix** — each line gets `\x1b[2m[HH:MM:SS]\x1b[0m ` using `chrono::Local::now()`
2. **Connection message** — `Listening for dumps on port {port}...` on connect, `Connection lost, reconnecting...` on disconnect
3. **Auto-reconnect** — if the daemon restarts, `stream_dumps()` retries connection with 1-second delay instead of exiting

VarDumper already sends ANSI-colored output when `VAR_DUMPER_FORMAT=server`, so Hearth just frames it with timestamps. No parsing or reformatting of dump payloads.

## 4. CLI Distribution & Installation

### Binary Names

- `crates/hearth-cli/Cargo.toml` — `[[bin]] name = "hearth"`
- `crates/hearth-daemon/Cargo.toml` — `[[bin]] name = "hearth-daemon"`

### Homebrew Formula

Repository: `github.com/Naoray/homebrew-tap`

Formula `hearth.rb`:
- Initially builds from source via `cargo install` (requires Rust toolchain)
- Installs both `hearth` and `hearth-daemon` binaries
- Install command: `brew install naoray/tap/hearth`

### GitHub Actions CI

Release workflow in the hearth repo:
- Trigger: tag push matching `v*`
- Build release binaries for `aarch64-apple-darwin` and `x86_64-apple-darwin`
- Upload as GitHub Release assets (tar.gz with both binaries)
- Once release artifacts exist, Homebrew formula switches from source build to pre-built binary download

### User Install Flow

```bash
brew tap naoray/tap
brew install hearth
hearth install          # first-time setup (DNS resolver, CA trust, Valet)
hearth-daemon &         # start daemon
hearth start            # bring up services
```

## File Changes

### New Files

| File | Purpose |
|------|---------|
| `crates/hearth-lib/src/mcp.rs` | `HearthMcpServer` with `#[tool_router]`, 8 MCP tools |
| `crates/hearth-lib/src/mailpit.rs` | Mailpit binary resolution chain |
| `crates/hearth-lib/src/download.rs` | Reusable GitHub Release downloader |
| `.github/workflows/release.yml` | CI release workflow for binary artifacts |

### Modified Files

| File | Changes |
|------|---------|
| `crates/hearth-lib/src/lib.rs` | Add `mcp`, `mailpit`, `download` module declarations |
| `crates/hearth-lib/src/dump.rs` | Timestamp prefix, auto-reconnect in `stream_dumps()` |
| `crates/hearth-lib/src/config.rs` | Add `mcp_port: u16` field (default 9900) |
| `crates/hearth-lib/src/service/manager.rs` | Conditional Mailpit registration in `default_services()` |
| `crates/hearth-daemon/src/main.rs` | Spawn MCP Streamable HTTP listener (axum + rmcp) |
| `crates/hearth-cli/src/main.rs` | Add `hearth mcp` subcommand (HTTP-to-stdio bridge) |
| `crates/hearth-cli/Cargo.toml` | Set `[[bin]] name = "hearth"` |
| `crates/hearth-daemon/Cargo.toml` | Set `[[bin]] name = "hearth-daemon"` |
| `Cargo.toml` | Add `rmcp`, `chrono`, `schemars`, `axum` workspace dependencies |

### New Dependencies

| Crate | Purpose | Feature flags |
|-------|---------|---------------|
| `rmcp` | MCP protocol server | `server`, `transport-streamable-http-server` |
| `schemars` | JSON Schema generation for MCP tool params | `1.0` (must match rmcp's schemars dep) |
| `chrono` | Timestamp formatting for dump CLI | — |
| `axum` | HTTP server for MCP Streamable HTTP transport | — |

## Testing Strategy

### Unit Tests

- **MCP tools:** Each tool gets a test that constructs a `HearthMcpServer` with mock `Arc<Mutex<T>>` instances (supervisor, site manager, PHP manager, config) and verifies the tool returns the expected `CallToolResult`. No network needed — test the tool handler functions directly.
- **Mailpit resolver:** Test resolution chain with temp dirs (same pattern as PHP resolver tests).
- **Download utility:** Test URL construction and asset pattern matching. Actual downloads tested in integration tests only.
- **Dump timestamps:** Test that `stream_dumps` output includes timestamp prefix format.

### Integration Tests (deferred)

- MCP Streamable HTTP end-to-end: spawn daemon, connect rmcp client, call tools
- MCP stdio bridge: spawn `hearth mcp`, pipe JSON-RPC, verify responses
- Mailpit download: hit real GitHub API (CI-only, skip in local test)

## Out of Scope

- MCP SSL/secure tools (can be added trivially later since handlers exist)
- MCP site logs tool (Phase 3 — needs log file aggregation)
- Mailpit `.env` auto-configuration for linked sites
- Pre-built PHP binary downloads (Phase 4)
- Tauri GUI (Phase 3)
- Database management (Phase 4)
- launchd plist for daemon auto-start (future)
