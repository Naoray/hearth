# Hearth — Unified Laravel Development Command Center

## Architecture

CLI-first Rust application with Tauri GUI wrapper (Phase 5).

**Current phase: Phase 3 (`hearth add`). Deadline 2026-05-26.**

```
hearth-daemon (always-on, owns all processes via process groups)
  ├── hearth-cli (thin client, talks to daemon via Unix socket)
  ├── hearth-gui (Tauri, Phase 5, also thin client)
  └── MCP server (in-process, Streamable HTTP on port 9900)
```

- **hearth-lib**: Core library shared by daemon, CLI, and GUI
- **hearth-daemon**: Process supervisor + Unix socket API server + MCP HTTP server
- **hearth-cli**: `hearth` binary, thin client using clap

### Key modules in hearth-lib

| Module | Purpose |
|--------|---------|
| `config.rs` | `HearthConfig` with TOML persistence, `#[serde(default)]` for migration safety |
| `mcp.rs` | `HearthMcpServer` with 12 MCP tools via rmcp `#[tool_router]` |
| `service/supervisor.rs` | Process group supervisor with circuit breaker + per-engine `ShutdownStrategy` |
| `service/manager.rs` | Default service registration (nginx, php-fpm, dnsmasq, mailpit, db engines) |
| `mailpit.rs` | Binary resolution chain (Hearth cache → system PATH) |
| `db.rs` + `db/{health,init,postgres,redis,mysql}.rs` | DB engines (Phase 4): TCP probe, init wrapper scripts with mkdir-lock + sentinel, per-engine resolvers (Postgres/Redis/MySQL+MariaDB) |
| `download.rs` | GitHub Release downloader (tar.gz/zip) |
| `dump.rs` | VarDumper TCP relay with broadcast + timestamped CLI streaming |
| `php.rs` + `php/resolver.rs` | PHP version management + binary resolution chain + Belt-E `scan_dir_env` |
| `php/targets.rs` | Provider-explicit target discovery, scan-dir/env-honor probing, channel verification/classification, launch-probed effective-value probes (`-r` CLI / `-i` FPM) |
| `php/engine.rs` | `PhpConfigEngine`: op-locked Set/Unset/Show/Status/Sync/Unmanage, row building with truthful coverage labels, launch-probe fill, conditional FPM restart |
| `php/reconcile.rs` | Manifest+journal channel-file reconciliation: atomic writes, exact-hash ownership, crash recovery, legacy INI migration, `applied_at_unix_ms` materialization stamps |
| `php/fpm.rs` | Generated private Unix-listener FPM conf + probe, typed ownership state, strict separate manifest, syntax matrix, recovery/unmanage transaction |
| `php/ini_guard.rs` | INI key/value validation (denylist, length, injection safety) — the single choke point before persistence and probes |
| `fastcgi.rs` | Dependency-free strict FastCGI RESPONDER v1 client with bounded framing, CGI parsing, clean-EOF enforcement, and fail-closed protocol validation |
| `site.rs` | Multi-home site enumeration (reads from both Valet and Herd config dirs) |
| `valet.rs` | Herd-aware site CLI wrapper (Valet fallback; link, unlink, park, secure, unsecure) |
| `socket.rs` | `DaemonRequest`/`DaemonResponse` protocol types |

## Key Design Decisions

1. Valet is a managed dependency (Composer global). Hearth uses Valet for site management unless Herd is running, then routes Herd-owned commands through Herd's CLI.
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

### MCP Tools (12)

| Tool | Description |
|------|-------------|
| `hearth_status` | List all services and their state |
| `hearth_sites` | List linked sites with paths, SSL, PHP version |
| `hearth_php_list` | List installed PHP versions |
| `hearth_php_switch` | Switch global PHP version + restart FPM |
| `hearth_site_link` | Link a directory through the active Valet/Herd backend |
| `hearth_site_unlink` | Unlink a site |
| `hearth_service_restart` | Restart one or all services |
| `hearth_php_config` | Set a PHP INI value (active version, `global`, or explicit `version` scope) via the shared engine; conditional FPM restart |
| `hearth_php_config_status` | Per-target PHP config coverage table (managed/UNMANAGED/LAUNCH-BLOCKED); optional `key` adds configured + launch-probed values, with `live-observed` reserved for fully evidence-gated running FPM workers |
| `hearth_db_start` | Start a DB engine (postgres/redis/mysql) |
| `hearth_db_stop` | Stop a DB engine |
| `hearth_db_status` | Per-engine status with port/data_dir/conflict info |

## Configuration

Config file: `~/Library/Application Support/hearth/config.toml`

Key ports (all configurable):
- `dns_port`: 5354 (dnsmasq)
- `dump_port`: 9912 (VarDumper relay)
- `mail_smtp_port`: 1025 (Mailpit SMTP)
- `mail_ui_port`: 8025 (Mailpit web UI)
- `mcp_port`: 9900 (MCP Streamable HTTP)

PHP INI store (canonical source for `hearth php config`):
- `[php_ini.global]` — directives applied to every version
- `[php_ini.overrides."X.Y"]` — sparse per-version overrides (override > global)
- Dotted directive keys must be TOML-quoted (`"date.timezone" = "..."`); an
  unquoted dotted key is rejected with an actionable error
- Materialized into per-version `php/{v}/conf.d/zz-hearth.ini` channel files,
  tracked exact-hash in `php/manifest.toml`; older binaries read this config
  but their next `config.save()` drops `[php_ini]` (downgrade is
  write-destructive — back up config.toml first)
- Hearth FPM launch artifacts: `fpm/php-fpm.conf`, `fpm/hearth-probe.php`, and
  the separate exact-hash `fpm/manifest.toml`; listener `run/php-fpm.sock`.
  A foreign conf without that manifest remains user-managed and byte-stable.

The exact observation vocabulary is `configured`, `materialized`,
`launch-probed`, and `live-observed`. For FPM, `live-observed` is available
only for keyed Show/Status when external ownership is proven `Unowned`, the
registered service is Running with Hearth-owned launch provenance, current
conf/probe hashes match that launch, the conf was applied no later than the
launch, the same current-user-owned Unix socket survives the request, the
strict FastCGI/CGI/JSON response has the exact key and fresh nonce with clean
EOF, the launch generation remains unchanged, and the responder PID belongs
to the supervised process group. A file parse can never mint `live-observed`;
user-managed or ambient FPM, stale config (`pending restart`), and any
ownership/generation/hash/socket/protocol/PID miss preserve the lesser label
and keep the command successful. After a daemon version mismatch, run
`hearth daemon stop && hearth daemon start`.
