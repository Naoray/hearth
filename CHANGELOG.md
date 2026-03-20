# Changelog

All notable changes to Hearth will be documented in this file.

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
