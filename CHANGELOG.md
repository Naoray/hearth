# Changelog

All notable changes to Hearth will be documented in this file.

## [Unreleased]

### Added
- **Global PHP configuration** (`hearth php config`): one canonical INI store
  in `config.toml` (`[php_ini.global]` + `[php_ini.overrides."X.Y"]`,
  override > global) materialized into per-version `zz-hearth.ini` scan-dir
  channel files with exact-hash manifest ownership, crash-safe journaling,
  and guarded migration of the legacy per-version Hearth `php.ini` files.
  New actions: `--global`, `--php <V>`, `--show`, `--status`, `--unset`,
  `--sync`, `--unmanage`, plus the `hearth php exec` launch shim.
- **Launch-probed observation**: `--show [key]` and `--status [key]` report
  per-target observed values by executing each CLI binary under its exact
  launch environments (`-r 'echo ini_get(...)'`) and Hearth's supervised FPM
  via `php-fpm -i` under the exact service env; a keyless `--status` stays
  the pure coverage table. FPM binaries are never executed for
  classification — ambient (Herd/Homebrew) FPM is structurally unprobed. Four-state vocabulary
  (`configured` / `materialized` / `launch-probed` / `live-observed`), a
  truthful `pending restart` marker (manifest materialization timestamp vs
  the supervisor's own FPM spawn time), and `[not running]` for a
  registered-but-stopped FPM. Probe failures render `n/a` and never fail
  the command. Live FPM-worker observation is deferred to todo #2343;
  ambient (non-Hearth-launched) FPM remains an unverified row that Hearth
  never writes for. No universal coverage is claimed anywhere.

### Changed
- `hearth php use` never touches PHP-FPM while Herd owns it: the switch
  persists the version and reconciles channels, then truthfully skips all
  FPM supervisor mutation (`php-fpm untouched (Herd manages PHP-FPM)`).
- Every config mutation (CLI/daemon set/unset and the MCP config write)
  shares one centralized FPM-restart policy: under live Herd the restart is
  skipped with zero supervisor mutation — even for an already-registered
  FPM — and reported truthfully (`SkippedHerdOwned` on the wire; a legacy
  CLI that cannot parse the new outcome fails into the actionable
  stop/start version-mismatch remediation instead of rendering a false
  state).
- One coherent current-Herd-ownership policy for PHP-FPM now lives inside
  the supervisor: generic `hearth start`/`hearth restart` skip (or, for an
  explicit `restart php-fpm`, refuse with an actionable error) the FPM slot
  while Herd owns it, and health supervision never respawns it. If Herd
  appears while Hearth's own FPM is still running, the next health tick
  relinquishes Hearth's own child — Hearth never signals or stops Herd's
  process.
- Stopping a supervised service is transactional: the child handle and
  spawn record are kept until termination is positively confirmed, stop
  failures propagate (and `hearth stop`/restart aggregate them instead of
  reporting false success), a failed FPM handoff keeps the child
  supervised and retries on later health ticks, and "stopped" is only ever
  recorded after proof.
- Herd-ownership detection is typed evidence (`Owned`/`Unowned`/`Unknown`)
  end-to-end: a failed probe (pgrep spawn error, signal, exit codes above
  the documented no-match 1) is `Unknown`, the isolated flag file must
  contain EXACTLY the single byte `1` or `0` (any newline, whitespace,
  BOM, extra bytes, empty, invalid UTF-8, or unreadable content is
  `Unknown`), and every FPM path fails closed on it: registration and
  reconfiguration are refused at the supervisor boundary itself (TOCTOU-
  safe re-check immediately before mutation), start/restart/health skip,
  `--status` renders a loud `OWNERSHIP-UNKNOWN` FPM row with the
  diagnostic instead of a "supervised" row, no FPM binary is executed,
  and config restarts report the `SkippedOwnershipUnknown` outcome.
  Unknown is never silently treated as "Herd absent" anywhere in the
  PHP/FPM paths.
- The supervisor's FPM ownership evidence is fail-closed BY CONSTRUCTION:
  a supervisor whose ownership probe has not been configured reports
  `Unknown` (`FPM ownership probe is not configured`) and refuses every
  FPM mutation until explicit evidence is installed — missing evidence is
  never treated as "Unowned". The `hearth php use` FPM replacement is one
  atomic supervisor transaction under a single authoritative ownership
  snapshot taken immediately before mutation: the replacement service is
  built before the old child is touched, a failed stop aborts with the old
  child still supervised, a failed build never costs a running FPM, and a
  failed start reports the truthful partial state — no interleaving can
  report "untouched" after mutating.
- EVERY restart that can touch PHP-FPM is that same kind of atomic
  supervisor transaction: named `hearth restart php-fpm`, the all-services
  `hearth restart`, and the config/MCP conditional FPM restart all take one
  authoritative typed ownership snapshot immediately before any stop.
  Owned/Unknown refuse or skip truthfully BEFORE the old child is touched
  (the all-services aggregate reports the skip instead of claiming every
  service restarted), Unowned performs one coherent stop/start with no
  post-stop recheck, a failed stop keeps the old child fully supervised,
  and a failed start leaves a truthful `Failed` registration with no
  orphan. No restart path composes a public stop with the guarded start
  anymore.
- Provider roots are validated as a SET: equal, nested, or symlink-aliased
  Hearth/Herd/Homebrew roots are rejected at construction, and provider
  identity additionally requires membership in exactly one canonical root —
  ambiguous identities render `identity unverified`, are never executed,
  and hold no channel or pending-restart authority.
- A keyed `--status` is certified by the daemon echoing the applied key
  (`status_key`) AND a per-request correlation token (`status_token`) —
  the certification is bound to the exact request/response pair, so an
  unrelated success response or a stale report can never certify. Against
  an older daemon that silently ignores the key, the keyed request fails
  with the stop/start remediation instead of printing a valueless table;
  keyless `--status` stays fully compatible in both directions. Invalid
  `--status`/`--show` keys are rejected client-side and daemon-side. The
  MCP `hearth_php_config_status` tool uses the same key + binding.
- The truthful `pending restart` marker now requires the full chain of
  evidence: the supervised command must equal the exact canonical
  provider/version FPM layout binary (root-bounded, unambiguous), the
  channel file must exist right now as a regular file whose bytes hash to
  the recorded applied hash, and the manifest entry must be an
  Applied/Written record for the exact path/version/channel. Targets whose
  expected layout path resolves outside their provider root are rendered
  `identity unverified` and are never executed.
- Protocol-mismatch remediation now names only supported commands
  (`hearth daemon stop && hearth daemon start`); there is no
  `hearth daemon restart` command.

### Removed
- The legacy `PhpConfig { version, key, value }` socket request survives one
  compatibility window (old CLI ↔ new daemon) and will be removed after it.
  **Downgrade warning**: a pre-`[php_ini]` binary reads the new config but
  its next `config.save()` silently drops the `[php_ini]` tables —
  downgrading is write-destructive; back up `config.toml` first.

## [0.3.1] - 2026-07-18

### Changed
- Site management is now Herd-aware: when Herd.app is running, the `link`,
  `unlink`, `park`, `secure`, and `unsecure` commands are delegated to
  Herd's CLI (resolved from `~/Library/Application Support/Herd/bin/herd`)
  instead of Valet, because a Herd-managed Valet installation hides those
  commands. `isolate` deliberately remains Valet-only — Herd's CLI does not
  expose a compatible isolate command. If Herd is running but its CLI
  cannot be resolved, the affected commands fail with an explicit error
  instead of silently falling back to Valet. (#22)

### Fixed
- CHANGELOG date drift: the `[0.3.0]` entry now records the actual ship
  date of 2026-05-20. The correction landed on main after the v0.3.0 tag
  was cut, so v0.3.1 is the first release whose tarball includes it.

## [0.3.0] - 2026-05-20

### Added
- **`hearth add <package>`** — guided Laravel package installer for `horizon`,
  `telescope`, `pulse`, and `reverb`. Each recipe runs `composer require`
  through the site's resolved PHP binary, walks the per-package `artisan`
  scaffolding steps, and patches `.env` with a backup written to
  `.env.hearth.YYYYMMDD-HHMMSS-NNNNNNNNN-PID.bak`.
- Horizon and Reverb workers are registered as supervised process-group
  children via new `ServiceKind::Horizon` and `ServiceKind::Reverb`. Their
  spec is persisted in `HearthConfig.added_packages` so `hearth daemon`
  restarts re-register them. Boot-time prune drops entries whose
  `<site_path>/vendor/<vendor>/<pkg>` no longer exists on disk (closes the
  gap left by `hearth remove` arriving later).
- `ManagedService::with_cwd(...)` / `with_cwd_and_site(...)` + a new
  `site_name: Option<String>` field. Display name on multi-site workers
  reads `horizon[shopfront]` / `reverb[chatapp]`.
- `HearthConfig.composer_phar`: absolute path to `composer.phar`, resolved
  during `hearth install` (Homebrew Cellar layout, with a runtime probe
  fallback in the daemon). Composer always runs as
  `<site_php> -d memory_limit=-1 <phar> require ...` — bypasses the brew
  `composer` bash wrapper, which uses the system PHP and breaks
  Valet-isolated sites.
- `DaemonState.add_lock` serializes concurrent `hearth add` invocations so
  `.env` edits + supervisor registration don't race.
- Dialoguer-driven interactive prompts in the CLI (telescope environments,
  horizon connection / environment / max processes, reverb host / port /
  hostname / scheme, pulse storage driver). `--yes` accepts all defaults.
  Reverb auto-bumps the default port across `8080..=8099` if `--yes` lands
  on a busy port (caps at 20 tries).
- stdlib `std::io::IsTerminal` non-TTY guard — refuses to prompt without
  `--yes` when stdin is piped.
- `DaemonRequest::Add { package, site_path, answers, no_supervise, dry_run }`
  + `AddAnswers` over the socket protocol.

- **`hearth db` subcommand** with `start`, `stop`, `restart`, `status [--json]`
  for MySQL/MariaDB, PostgreSQL, and Redis. Engines are supervised process
  groups with the existing 3-in-60s circuit breaker.
- Postgres engine: resolver chain (Hearth cache → Homebrew `postgresql@N`,
  highest version wins, Apple Silicon + Intel prefixes). `initdb` runs once
  via an idempotent wrapper script (atomic sentinel + portable mkdir-lock,
  `--locale=C --encoding=UTF8` to dodge macOS US-ASCII clash). Shutdown via
  `pg_ctl stop -m fast` for a clean checkpoint, with SIGTERM + 20s grace
  fallback.
- MySQL/MariaDB engine: resolver detects flavor (filename or `--version`
  probe). MariaDB init invokes `<basedir>/scripts/mariadb-install-db` with
  `--auth-root-authentication-method=normal`; real MySQL uses
  `--initialize-insecure`. Runtime args include `--skip-name-resolve`.
  Shutdown grace 20s to protect InnoDB flush.
- Redis engine: dataless; runtime args `--appendonly no`, pidfile under
  `~/Library/Application Support/hearth/run/redis.pid`.
- Port-in-use guard at registration AND inside daemon dispatch — surfaces a
  typed `DaemonResponse::Conflict { engine, port, owner_hint }` so port
  collisions don't burn the circuit breaker.
- 3 new MCP tools (`hearth_db_start`, `hearth_db_stop`, `hearth_db_status`)
  bring the tool count to 11.
- `ShutdownStrategy` enum on `ManagedService` (`Default`, `LongGrace`,
  `Postgres { datadir, pg_ctl_binary }`); supervisor `stop()` now sends real
  SIGTERM via `nix::killpg`, honors the per-engine grace window, then
  SIGKILLs the whole process group.
- `HearthConfig` gains `mysql_port`, `postgres_port`, `redis_port`. Legacy
  configs without these keys backfill the standard defaults.
- `data_dir()` helper alongside `run_dir()` / `log_dir()`; pre-created at
  daemon boot.
- `scripts/spike-mysql-init.sh` + `scripts/spike-results/SPIKE_NOTES.md`
  documenting the MariaDB-vs-MySQL init branch discovery from Day-1 spike.

### Changed
- `ManagedService::stop()` now blocks for the strategy's grace window
  (previously SIGKILLed immediately after a single non-blocking `try_wait`).

### Notes
- v0.3.0 ships single-site Horizon/Reverb. A second `hearth add horizon`
  against a different linked site errors with a clear "multi-site arrives
  in v0.3.1" message rather than silently colliding on the supervisor's
  state.
- Telescope intentionally does not regex-patch
  `TelescopeServiceProvider::gate()`. A post-install hint reminds users
  to gate it manually in production.
- Pulse `pulse:check` recorder is install-only in v0.3.0; not registered
  as a supervised worker.

## [0.2.3] - 2026-03-23

### Added
- `hearth daemon start` — launch the daemon as a background process with log redirection
- `hearth daemon stop` — gracefully stop supervised services then terminate the daemon
- `hearth daemon status` — check if daemon is running and socket is responsive

### Fixed
- `hearth install` now detects Herd and writes `/etc/resolver/test` without custom port (defaults to port 53 for Herd's dnsmasq)
- `hearth daemon stop` sends a Stop request to gracefully shut down services before SIGTERM, preventing orphaned child processes

## [0.2.2] - 2026-03-23

### Fixed
- `hearth sites` now shows all sites from both Valet and Herd (was only reading Valet's config dir)

## [0.2.1] - 2026-03-23

### Added
- Herd coexistence — detects running Herd and skips nginx/php-fpm/dnsmasq, only runs Hearth-owned services
- Site listing reads from `Sites/` symlinks (primary) with `Nginx/` fallback, deduplicates across both
- `ValetCli::link_in()` for concurrent-safe site linking from MCP

### Fixed
- `hearth sites` now shows linked sites (was reading from empty Nginx dir instead of Sites dir)
- `hearth start` no longer aborts on first missing binary — skips failed services gracefully
- PHP deprecation warnings from Valet CLI are filtered from output

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
