# Hearth — Unified Laravel Development Command Center

## Architecture

CLI-first Rust application with Tauri GUI wrapper (Phase 3).

```
hearth-daemon (always-on, owns all processes via process groups)
  ├── hearth-cli (thin client, talks to daemon via Unix socket)
  ├── hearth-gui (Tauri, Phase 3, also thin client)
  └── MCP server (in-process, Streamable HTTP on port 9900)
```

- **hearth-lib**: Core library shared by daemon, CLI, and GUI
- **hearth-daemon**: Process supervisor + Unix socket API server + MCP HTTP server
- **hearth-cli**: `hearth` binary, thin client using clap

### Key modules in hearth-lib

| Module | Purpose |
|--------|---------|
| `config.rs` | `HearthConfig` with TOML persistence, `#[serde(default)]` for migration safety |
| `mcp.rs` | `HearthMcpServer` with 8 MCP tools via rmcp `#[tool_router]` |
| `service/supervisor.rs` | Process group supervisor with circuit breaker |
| `service/manager.rs` | Default service registration (nginx, php-fpm, dnsmasq, mailpit) |
| `mailpit.rs` | Binary resolution chain (Hearth cache → system PATH) |
| `download.rs` | GitHub Release downloader (tar.gz/zip) |
| `dump.rs` | VarDumper TCP relay with broadcast + timestamped CLI streaming |
| `php.rs` + `php/resolver.rs` | PHP version management + binary resolution chain |
| `site.rs` | Multi-home site enumeration (reads from both Valet and Herd config dirs) |
| `valet.rs` | Valet CLI wrapper (link, unlink, park, secure, unsecure) |
| `socket.rs` | `DaemonRequest`/`DaemonResponse` protocol types |

## Key Design Decisions

1. Valet is a managed dependency (Composer global). Hearth wraps Valet CLI for site management.
2. Process groups via `command-group` crate — guarantees no orphan processes.
3. Circuit breaker: 3 failures in 60s = service stopped, manual restart required.
4. Health poll every 5 seconds.
5. PHP resolution chain: Hearth cache → Herd binaries → Homebrew → download.
6. `sudo` only during `hearth install` (DNS resolver + CA trust). No persistent privileged helper.
7. dnsmasq on unprivileged port 5354, resolver file specifies custom port.
8. MCP server is in-process (Tokio task in daemon) — zero external processes, zero orphan risk.
9. DaemonState uses per-field `Arc<Mutex<T>>` (not a single outer mutex) so MCP tools can hold individual references.
10. Lock ordering convention: `config → php_manager → site_manager → supervisor`. Always acquire in this order to prevent deadlocks.
11. Mailpit registered conditionally — only if binary is found on disk.
12. Herd coexistence: when Herd.app is detected via `pgrep`, nginx/php-fpm/dnsmasq are skipped. Hearth only manages its own services.
13. Site enumeration reads from multiple valet home dirs (`~/.config/valet` + `~/Library/Application Support/Herd/config/valet`), deduplicates by name.

## Build & Run

```bash
cargo build                    # Build all crates
cargo run -p hearth-daemon     # Start the daemon (Unix socket + MCP HTTP)
cargo run -p hearth-cli -- status  # CLI commands
```

### Install via Homebrew

```bash
brew tap naoray/tap
brew install hearth
```

## Testing

```bash
cargo test                     # Unit tests (all crates, 55 tests)
cargo test -p hearth-lib       # Library tests only
./scripts/smoke-test.sh        # End-to-end: daemon, CLI, dump server, MCP endpoint
```

## MCP Server

The daemon serves an MCP (Model Context Protocol) server on `127.0.0.1:9900` via Streamable HTTP. IDEs connect directly to this URL — no spawned processes.

For IDEs that only support stdio transport, use the bridge: `hearth mcp` (reads JSON-RPC from stdin, POSTs to the HTTP endpoint).

### MCP Tools (8)

| Tool | Description |
|------|-------------|
| `hearth_status` | List all services and their state |
| `hearth_sites` | List linked sites with paths, SSL, PHP version |
| `hearth_php_list` | List installed PHP versions |
| `hearth_php_switch` | Switch global PHP version + restart FPM |
| `hearth_site_link` | Link a directory as a Valet site |
| `hearth_site_unlink` | Unlink a site |
| `hearth_service_restart` | Restart one or all services |
| `hearth_php_config` | Set php.ini value + restart FPM |

## Configuration

Config file: `~/.config/hearth/config.toml`

Key ports (all configurable):
- `dns_port`: 5354 (dnsmasq)
- `dump_port`: 9912 (VarDumper relay)
- `mail_smtp_port`: 1025 (Mailpit SMTP)
- `mail_ui_port`: 8025 (Mailpit web UI)
- `mcp_port`: 9900 (MCP Streamable HTTP)

