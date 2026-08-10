# Hearth 🔥 — archived

> **This project is archived and no longer maintained.** It is preserved as a
> read-only historical record. Do not install it.
>
> **Use [Yerd](https://yerd.io) instead.** Yerd covers everything Hearth did —
> sites, SSL, PHP version switching and per-site PHP, INI settings, databases,
> mail capture, dump telemetry, and an MCP server for AI agents — plus a large
> surface Hearth never reached: reverse proxies, wildcard domains, per-site web
> roots, Cloudflare tunnels, LAN exposure, pcov coverage shims, `doctor fix`,
> self-update, and a shipping GUI.

## Why this was archived

Hearth set out to be the best free, MIT-licensed, daemon-driven Laravel
development environment for solo macOS developers and AI coding agents.
[`docs/NORTH_STAR.md`](docs/NORTH_STAR.md) defined the success signal that
mattered most:

> Maintainer uses Hearth as their primary local Laravel stack for the full
> sprint without falling back to other tooling.

That signal failed. The maintainer migrated to Yerd, which had shipped a
superset of Hearth's roadmap — including the Tauri GUI that was still parked in
Hearth's PR #4 as unbuilt Phase 5 work. Hearth's own first decision principle
states the conclusion plainly: *"if the maintainer doesn't use it, it isn't
ready to ship."*

Continuing would have meant building features to match another tool's
checklist rather than because they were needed — explicitly listed in the North
Star as an anti-signal. So it stops here, at v0.4.0, with the work intact and
the reasoning on the record.

Everything below documents Hearth as it stood at archival.

---

**Your complete Laravel development environment — one Rust daemon, zero friction.**

Services, sites, SSL, PHP version switching, mail catching, dump server, and package setup.

MIT licensed. No subscriptions. No paid tiers.

---

## What Hearth manages

```
hearth start / stop / restart / status    — all services at once
hearth link / unlink / park / sites       — site management
hearth secure / unsecure                  — SSL certificates (trusted locally)
hearth php use 8.3                        — switch PHP version instantly
hearth php list                           — show installed versions
hearth php config --global memory_limit 1G — set a PHP INI value across versions
hearth php config --status [key]          — per-target coverage (+values with a key)
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
`~/Library/Application Support/hearth/data/{engine}/`, and probes health via TCP.

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

**Not available.** Hearth is archived and its Homebrew formula has been removed
from `naoray/tap`. If you have it installed from a previous release, remove it:

```bash
hearth daemon stop
brew uninstall hearth
brew untap naoray/tap   # only if you use no other formulae from this tap
```

Building from source still works for archaeological purposes
(`cargo install --path crates/hearth-cli`), but nothing here is supported.

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

Hearth served an in-process MCP server and shipped a [Scribe](https://github.com/Naoray/scribe)
skill so agents could manage sites, switch PHP versions, and check service
status through plain CLI calls.

**Both are retired.** Use Yerd's MCP server instead:

```bash
claude mcp add --scope user yerd -- yerd mcp
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

`~/Library/Application Support/hearth/config.toml` — all ports are configurable:

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

1. `~/Library/Application Support/hearth/php/{version}/php` — own cached binaries
2. `~/Library/Application Support/Herd/bin/php{version}` — reuse existing binaries
3. `/opt/homebrew/opt/php@{version}/bin/php` — Homebrew fallback

## PHP configuration

Hearth keeps one canonical PHP INI store in `config.toml` and materializes it
into per-version `zz-hearth.ini` channel files that PHP's scan-dir mechanism
loads. You never edit `php.ini` files by hand.

```bash
hearth php config --global memory_limit 1G     # applies to every version
hearth php config --php 8.3 memory_limit 512M  # one version's override
hearth php config memory_limit 512M            # shorthand: the active version
hearth php config --show [key]                 # configured + observed values
hearth php config --status [key]               # coverage table; a key adds
                                               # per-target configured/observed
hearth php config --unset [--global|--php V] <key>
hearth php config --sync                       # force re-reconcile
hearth php config --unmanage                   # remove every Hearth-written file
```

**Precedence** is deterministic: a per-version override always beats the
global value for that version.

**Manual edits**: the store lives in `config.toml` under `[php_ini.global]`
and `[php_ini.overrides."X.Y"]`. Directive keys that contain dots MUST be
quoted in TOML (`"date.timezone" = "Europe/Berlin"`), otherwise TOML splits
the key into nested tables and Hearth rejects the file with an actionable
error.

**Restart semantics**: `set`/`unset` persist first, then reconcile channel
files, then restart the supervised php-fpm only when it is actually
registered. An unregistered or launch-blocked FPM is reported informationally
and never fails the command; a failed restart of a registered FPM does.
While Herd owns PHP-FPM, configuration changes still persist and reconcile,
but Hearth never starts or restarts its FPM — config output states that the
restart was skipped because Herd owns PHP-FPM, generic `hearth start`/
`hearth restart` skip or refuse the FPM slot, and health supervision never
respawns it. Two distinct rules: Hearth **never controls Herd's own FPM
process**, and Hearth **may stop its own supervised FPM child** during an
ownership handoff (when Herd appears while Hearth's FPM is still running).
A handoff stop that cannot positively confirm termination is reported as a
failure with the child kept fully supervised, and is retried on later
health ticks — Hearth never records a false "stopped". If the Herd
ownership probe itself fails, ownership is treated as **unknown** and every
FPM activation fails closed (skipped with an actionable "ownership
unknown" message) until the probe recovers.
When the running FPM predates the newest materialized config, status shows a
truthful `pending restart` marker.

### Hearth FPM

On `hearth install`, daemon boot, or explicit `hearth php config --sync`,
Hearth atomically generates `fpm/php-fpm.conf`, `fpm/hearth-probe.php`, and
the exact-hash ownership record `fpm/manifest.toml`. The supervised master
listens only on `run/php-fpm.sock` (mode `0600`); the generated pool accepts
`.php` scripts, runs in the foreground, and keeps canonical PHP INI values in
the existing scan-dir channel rather than duplicating them in FPM config.

A pre-existing `fpm/php-fpm.conf` without Hearth's manifest is intentionally
`user-managed`: Hearth can launch it but does not add a probe or ownership
manifest, overwrite it, or remove it. Remove or rename that file and run
`hearth php config --sync` to adopt the generated configuration.

`hearth php config --unmanage` removes the Hearth-owned conf, probe, and
manifest as one locked exact-hash set. It does not stop or unregister a running
master; that child keeps running, while future service construction is
launch-blocked until Sync regenerates the files. User-managed bytes survive
Unmanage unchanged.

Before launching a Hearth-owned config, stale-socket cleanup is deliberately
narrow: only the exact `run/php-fpm.sock` path is considered, `lstat` must show
an actual Unix socket, a synchronous connect must prove the endpoint dead, and
the socket identity must remain unchanged. Live sockets, symlinks, regular
files, and foreign paths are never unlinked.

The separate `fpm/manifest.toml` prevents an older binary's INI reconcile from
deleting FPM artifacts. Its persistent `.manifest.lock` is inert when no
transaction is running.

**Observation labels** are exact about what was verified:

- `configured` — the value in the canonical store
- `materialized` — the channel file on disk carries it
- `launch-probed` — the binary was executed under the exact launch
  environment (`php -r 'echo ini_get(...)'` for CLI; `php-fpm -i` under the
  service env for Hearth's supervised FPM) and reported it
- `live-observed` — a running Hearth-supervised FPM worker answered the
  evidence-gated FastCGI probe

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

A value read from a file is never presented as the effective value of a
running process.

**Coverage scope (exact)**: Hearth hard-guarantees only

1. Hearth-launched processes whose binaries honor `PHP_INI_SCAN_DIR`
   (verified per binary by a canary probe), and
2. verified user-owned, version-exclusive scan-dir channels
   (`~/Library/Application Support/Herd/config/php/{XY}`,
   `/opt/homebrew/etc/php/{v}/conf.d`, and Hearth's own `conf.d` dirs).

Everything else renders loudly as UNMANAGED/LAUNCH-BLOCKED/unverified.
There is no universal guarantee, and `--status` never claims one.

Known limits, stated plainly:

- **Sanitized Herd binaries** (env-clearing launchers running Herd's
  absolute PHP path): their only scan channel is the root-owned
  `/usr/local/etc/php/conf.d`, which Hearth never writes. The row renders
  `UNMANAGED: privileged-dir` with remediation: use
  `hearth php exec -- <cmd>` for env-clearing launchers, or a
  Hearth/Homebrew build.
- **`hearth php exec` shim limit**: the shim covers a launcher that invokes
  the shim as its final PHP launch boundary. A *descendant* process that
  clears the environment and re-execs the absolute `PHP_BINARY` bypasses it —
  for that case use a Homebrew/Hearth build (verified compiled-in channel).
- **Ambient Herd/Homebrew FPM** (not launched by Hearth): its launch context
  can never be authenticated. Hearth never writes on its behalf and never
  executes it; use Hearth's supervised FPM for managed and `live-observed`
  coverage.

**Downgrade warning**: older Hearth binaries read this config but their next
`config.save()` silently drops the `[php_ini]` tables — downgrading is
write-destructive. Back up `config.toml` before downgrading.

If the CLI reports a version mismatch with the daemon, run
`hearth daemon stop && hearth daemon start` and retry.

## Roadmap

- [x] Phase 1: Core CLI + daemon, process supervision, site/PHP management
- [x] Phase 2: Mailpit, dump server, Homebrew distribution
- [ ] Phase 3: `hearth add` — guided package installer (Horizon, Telescope, Pulse, Reverb)
- [ ] Phase 4: Database management (MySQL, PostgreSQL, Redis)
- [ ] Phase 5: Tauri GUI (parked PR #4 lands here)
- Future (not in v0.3.0): Log Viewer, XDebug detection

## License

MIT — free forever, including databases (Phase 4).
