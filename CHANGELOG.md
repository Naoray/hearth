# Changelog

All notable changes to Hearth will be documented in this file.

## [0.2.0] - 2026-03-23

### Added
- In-process MCP server with 8 tools for IDE integration (status, sites, php list/switch/config, site link/unlink, service restart)
- Streamable HTTP transport on port 9900 — IDEs connect via URL, zero spawned processes, zero orphan risk
- `hearth mcp` stdio bridge for IDEs that only support stdio transport
- Mailpit mail catcher as a managed service with binary resolution chain (Hearth cache → system PATH)
- Conditional Mailpit registration — only starts if binary is found
- GitHub Release downloader utility (`download_github_release`) with tar.gz/zip support
- Timestamped output for `hearth dump` with auto-reconnect on daemon restart
- `mcp_port` config field (default 9900)
- `#[serde(default)]` on `HearthConfig` for forward-compatible config migration
- `ValetCli::link_in()` for concurrent-safe site linking (uses `Command::current_dir()`)
- GitHub Actions release workflow for macOS binary artifacts (aarch64 + x86_64)
- Homebrew formula in `naoray/tap` (source build via `brew install naoray/tap/hearth`)
- 16 new unit tests (total: 51)

### Changed
- `DaemonState` refactored from single `Arc<Mutex<DaemonState>>` to per-field `Arc<Mutex<T>>` with documented lock ordering convention (`config → php_manager → site_manager → supervisor`)
- `default_services()` now accepts `config_dir` parameter for testability and Mailpit resolution

### Fixed
- Daemon `PhpSwitch` handler lock ordering corrected to `config → supervisor` (was `supervisor → config`, potential deadlock with concurrent MCP calls)
- MCP `hearth_php_switch` now uses `resolve_phpfpm_binary()` and includes `--fpm-config` arg (was silently using wrong php-fpm config)
- MCP `hearth_php_switch` now persists config to disk via `config.save()`
- MCP `hearth_site_link` uses `Command::current_dir()` instead of `std::env::set_current_dir()` (was a process-global race condition)

## [0.1.1.0] - 2026-03-20

### Added
- All 6 remaining daemon request handlers: Sites, Park, PhpList, Restart, PhpConfig, PhpSwitch
- `DaemonState` struct to share config, site manager, and PHP manager across handlers
- `FromStr` for `ServiceKind` with human-friendly aliases (e.g., `php` → PhpFpm, `dns` → Dnsmasq)
- Multi-source PHP version discovery: Hearth cache → Herd binaries → Homebrew
- `SiteManager::resolve_isolated_php()` to read Valet per-site PHP isolation config
- `ServiceSupervisor::reconfigure_service()` for PHP version switching
- Dump server with broadcast relay (VarDumper input on `dump_port`, CLI subscribers on `dump_port + 1`)
- `hearth dump` CLI command connects to relay port to stream dump output
- Config `save_to`/`load_from` methods for testability
- 35 unit tests covering config, socket serde, site enumeration, ServiceKind parsing, supervisor operations, dump server broadcast, PHP version scanning, and TLD-aware isolation lookup

### Fixed
- Dump server deadlock: both sides tried to read, nobody wrote — replaced with broadcast channel architecture
- `start_service`/`stop_service` now return errors for unregistered services instead of silent no-ops
- PhpConfig handler checks PHP-FPM restart errors instead of swallowing them
- Site isolation lookup uses configurable TLD instead of hardcoded `.test`

## [0.1.0.0] - 2026-03-19

### Added
- Initial scaffold: Rust workspace with hearth-daemon, hearth-cli, and hearth-lib crates
- Unix socket daemon with request/response protocol
- Service supervisor with process groups via `command-group` crate
- Circuit breaker (3 failures in 60s = service stopped)
- PHP binary resolution chain
- Valet CLI wrapper for site management
- HearthConfig with TOML persistence
