# Hearth 🔥

**Your complete Laravel development environment — one Rust daemon, zero friction.**

Services, sites, SSL, PHP version switching, mail catching, dump server, and package setup. Install once, works immediately.

```bash
brew install naoray/tap/hearth
hearth install    # first-time setup
hearth start      # everything is running
```

MIT licensed. No subscriptions. No paid tiers.

---

## What Hearth manages

```
hearth start / stop / restart / status    — all services at once
hearth link / unlink / park / sites       — site management
hearth secure / unsecure                  — SSL certificates (trusted locally)
hearth php use 8.3                        — switch PHP version instantly
hearth php list                           — show installed versions
hearth php config memory_limit 512M       — edit php.ini + restart FPM
hearth dump                               — stream VarDumper output with timestamps
hearth mail                               — open Mailpit UI in browser
hearth db status [--json]                 — show DB engine state, ports, data dirs
hearth db start  [mysql|postgres|redis]   — start one DB engine, or all when omitted
hearth db stop   [mysql|postgres|redis]   — stop one or all DB engines
hearth add horizon                        — guided install with opinionated config
hearth laravel new myapp                  — scaffold a new Laravel project
```

### DB engines (MySQL/MariaDB, PostgreSQL, Redis)

Hearth supervises locally installed DB engines using the same circuit-breaker
process-group machinery as nginx/php-fpm. No client libraries are linked — Hearth
spawns the engine binaries, manages their data directories under
`~/.config/hearth/data/{engine}/`, and probes health via TCP.

Install the engines via Homebrew (cache-based install lands in a later phase):

```bash
brew install postgresql@17 redis mysql
```

Hearth picks them up automatically. Port collisions with another tool already
managing the same DB (Herd's services panel, `brew services`, etc.) are
auto-detected: when the configured port is already bound, that engine is
skipped at registration and `hearth db status` reports `conflict_port: true`
so you know which colliding service to stop.

## Install

```bash
brew tap naoray/tap
brew install hearth
hearth install
```

Or build from source:

```bash
cargo install --path crates/hearth-cli
cargo install --path crates/hearth-daemon
```

## Works alongside your existing setup

If you're already using Valet or Herd, Hearth detects them and works alongside — sharing sites and skipping services the other tool already manages. Migrate gradually, or run both indefinitely.

## `hearth add` — guided package setup

Laravel packages like Horizon, Telescope, Pulse, and Reverb all require additional configuration beyond `composer require`. `hearth add` walks you through it interactively, writes sensible defaults based on your answers, and registers long-running services (like Horizon) as supervised daemon processes.

```bash
hearth add horizon     # installs, configures, and supervises Horizon
hearth add telescope   # installs with safe production guards
hearth add pulse       # installs with recommended aggregation settings
hearth add reverb      # installs and starts the WebSocket server
```

No more copy-pasting config snippets from docs. No more manually running workers in a terminal tab.

## Agent integration

Hearth is designed to work with AI coding agents via [Scribe](https://github.com/Naoray/scribe) — a skill manager for agents. Install the Hearth skill and your agent can manage sites, switch PHP versions, check service status, and more through plain CLI calls. No embedded server required.

```bash
scribe install hearth
```

## Architecture

Hearth is a thin CLI talking to an always-on daemon over a Unix socket. The daemon owns all child processes via process groups — when it exits, everything exits. No orphaned processes.

```
hearth-daemon
  ├── nginx          (process group)
  ├── php-fpm        (process group)
  ├── dnsmasq        (process group)
  ├── mailpit        (process group, if installed)
  ├── horizon        (process group, if added)
  └── dump server    (tokio task)
```

A circuit breaker prevents restart loops: 3 crashes in 60 seconds marks a service as failed. Use `hearth restart <service>` to retry manually.

## Configuration

`~/.config/hearth/config.toml` — all ports are configurable:

```toml
tld = "test"
default_php = "8.4"
dns_port = 5354
dump_port = 9912
mail_smtp_port = 1025
mail_ui_port = 8025
mcp_port = 9900
mysql_port = 3306
postgres_port = 5432
redis_port = 6379
```

## PHP resolution

Hearth finds PHP binaries in this order:

1. `~/.config/hearth/php/{version}/php` — own cached binaries
2. `~/Library/Application Support/Herd/bin/php{version}` — reuse existing binaries
3. `/opt/homebrew/opt/php@{version}/bin/php` — Homebrew fallback

## Roadmap

- [x] Phase 1: Core CLI + daemon, process supervision, site/PHP management
- [x] Phase 2: Mailpit, dump server, Homebrew distribution
- [ ] Phase 3: `hearth add` — guided package installer (Horizon, Telescope, Pulse, Reverb)
- [ ] Phase 4: Database management (MySQL, PostgreSQL, Redis)
- [ ] Phase 5: Tauri GUI (parked PR #4 lands here)
- Future (not in v0.3.0): Log Viewer, XDebug detection

## License

MIT — free forever, including databases (Phase 4).
