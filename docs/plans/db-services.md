# DB Services (MySQL · Postgres · Redis) — v0.3.0 Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship CLI-managed MySQL, Postgres, and Redis services supervised by the Hearth daemon so the maintainer can cancel Herd Pro on 2026-05-26 without losing the "Services panel for DBs" workflow.

**Architecture:** Each engine becomes a supervised process group registered conditionally in `default_services()`, mirroring the Mailpit pattern. Hearth never imports an engine client crate — it only spawns binaries, manages their data directories, and TCP-polls health. CLI surface `hearth db {start|stop|restart|status|config}` maps to existing `ServiceKind` variants via the supervisor.

**Tech Stack:** Rust 2024, Tokio, `command-group` (process groups), Homebrew binary resolution chain, TOML config with `#[serde(default)]`. No new crate dependencies — `mysql_async`, `tokio-postgres`, and `redis` are explicitly OUT.

---

## 1. Goals & Non-Goals

### Goals
- `hearth db start|stop|restart|status` works for `mysql`, `postgres`, `redis`, or `all`.
- Engines run as supervised process groups with the existing circuit breaker (3-in-60s).
- Binary resolution chain mirrors `mailpit.rs`: Hearth cache → Herd bin (where applicable) → Homebrew → `which`.
- Data directories live under `~/.config/hearth/data/{engine}/` and survive daemon restarts.
- New `HearthConfig` fields are forward-compatible (`#[serde(default)]`) — legacy configs load unchanged.
- MCP tools `hearth_db_start`, `hearth_db_stop`, `hearth_db_status` expose the same surface to agents.
- Health checks via TCP connect on the engine's port (no client lib needed).

### Non-Goals (explicit per NORTH_STAR)
- **No GUI.** No Tauri panel for DBs. Phase 5 problem.
- **No remote/cloud DBs.** Local only.
- **No migrations runner.** `php artisan migrate` stays the user's job.
- **No backup/restore UI.** Mysqldump/pg_dump remain manual.
- **No replication, no clustering, no read-replicas.**
- **No engine version manager.** One binary per engine, resolved at start. Multi-version support is Phase 4+.
- **No write access to client config (`~/.my.cnf`, `~/.pgpass`).** Hearth touches only its own data dirs.
- **No persistent root password rotation.** MySQL/Postgres start with empty/trust auth on `127.0.0.1` — same posture as Herd Pro.

---

## 2. CLI Surface

New subcommand `hearth db` added to `Commands` enum in `crates/hearth-cli/src/main.rs`:

```text
hearth db start    [mysql|postgres|redis|all]   # default: all
hearth db stop     [mysql|postgres|redis|all]   # default: all
hearth db restart  [mysql|postgres|redis|all]   # default: all
hearth db status   [--json]                     # status table; --json prints machine-parseable
hearth db config   <engine> <key> <value>       # set port/data_dir/version override
```

Stable flags:
- Engine arg accepts `mysql`, `postgres`/`postgresql`/`pg`, `redis` (already handled by `ServiceKind::FromStr`).
- `--json` on `status` emits `[{engine, state, pid, port, data_dir}]`.
- `config` valid keys: `port`, `data_dir`, `version`. Unknown key → exit 2 with `unknown config key`.

Exit codes: `0` ok, `1` engine not registered / binary missing, `2` arg parse error, `3` circuit breaker tripped.

New `DaemonRequest` variants in `crates/hearth-lib/src/socket.rs`:

```rust
DbStart   { engine: Option<String> }   // None == all
DbStop    { engine: Option<String> }
DbRestart { engine: Option<String> }
DbStatus
DbConfig  { engine: String, key: String, value: String }
```

`DbStart/Stop/Restart` reuse the existing supervisor methods — the new variants exist only to keep CLI ergonomics decoupled from generic `Restart { service }`. Daemon dispatcher in `crates/hearth-daemon/src/main.rs::process_request` adds matching arms.

---

## 3. Binary Resolution

One resolver per engine, each in its own file under `crates/hearth-lib/src/db/`. Pattern is copy-paste of `mailpit.rs::resolve_mailpit_binary` with engine-specific paths.

### MySQL/MariaDB → `mysqld`
1. Hearth cache: `~/.config/hearth/services/mysql/bin/mysqld`
2. Herd bin: `~/Library/Application Support/Herd/bin/mysqld` (Herd ships MariaDB-as-mysqld — confirmed on dev machine)
3. Homebrew (priority list):
   - `/opt/homebrew/opt/mysql/bin/mysqld`
   - `/opt/homebrew/opt/mariadb/bin/mysqld`
4. `which mysqld`

### Postgres → `postgres` + `initdb` (both required)
1. Hearth cache: `~/.config/hearth/services/postgresql/bin/{postgres,initdb}`
2. Herd bin: skipped — Herd does **not** ship postgres (confirmed via `ls ~/Library/.../Herd/bin/`).
3. Homebrew: glob `/opt/homebrew/opt/postgresql@*/bin/` — pick highest version number; resolver returns `(postgres_path, initdb_path, version_string)`.
4. `which postgres` + `which initdb`

If only one of the two binaries is found, resolver returns `None` (both must come from the same install).

### Redis → `redis-server`
1. Hearth cache: `~/.config/hearth/services/redis/bin/redis-server`
2. Herd bin: skipped (Herd does not ship redis).
3. Homebrew: `/opt/homebrew/opt/redis/bin/redis-server`
4. `which redis-server`

### Cache layout under `~/.config/hearth/`

```
~/.config/hearth/
├── services/
│   ├── mailpit/mailpit                    # existing
│   ├── mysql/bin/mysqld                   # new — populated by `hearth install` or manual
│   ├── postgresql/bin/{postgres,initdb}   # new
│   └── redis/bin/redis-server             # new
└── data/
    ├── mysql/                             # mysqld datadir
    ├── postgresql/                        # initdb -D target
    └── redis/                             # dump.rdb location only
```

`download.rs` is **not** extended for v0.3.0. Users install engines via Homebrew (`brew install mysql postgresql@17 redis`) and Hearth picks them up. Cache-based downloads are a Phase 4 follow-up — out of renewal-sprint scope.

---

## 4. Data Directories & Init Flow

All under `~/.config/hearth/data/{engine}/`. Created lazily on first start. Idempotent: if dir is non-empty and engine-specific sentinel exists, init is skipped.

### MySQL (`data/mysql/`)
Sentinel: `data/mysql/mysql/user.MYD` (or `user.ibd` on MariaDB 10.4+). Init when missing:
```bash
mysqld --initialize-insecure --datadir=~/.config/hearth/data/mysql
```
Runtime args:
```bash
mysqld \
  --datadir=~/.config/hearth/data/mysql \
  --socket=~/.config/hearth/run/mysql.sock \
  --port={config.mysql_port} \
  --bind-address=127.0.0.1 \
  --pid-file=~/.config/hearth/run/mysql.pid \
  --log-error=~/.config/hearth/log/mysql.err
```

### Postgres (`data/postgresql/`)
Sentinel: `data/postgresql/PG_VERSION`. Init when missing:
```bash
initdb -D ~/.config/hearth/data/postgresql --auth-host=trust --auth-local=trust -U $USER --encoding=UTF8
```
Runtime args:
```bash
postgres \
  -D ~/.config/hearth/data/postgresql \
  -p {config.postgres_port} \
  -h 127.0.0.1 \
  -k ~/.config/hearth/run     # socket dir
```

### Redis (`data/redis/`)
No init step — dataless engine. Args:
```bash
redis-server \
  --port {config.redis_port} \
  --bind 127.0.0.1 \
  --dir ~/.config/hearth/data/redis \
  --dbfilename dump.rdb \
  --daemonize no \
  --pidfile ~/.config/hearth/run/redis.pid
```

Init runs synchronously in `ManagedService::start` for now. If init takes >5s the supervisor's health check will see Starting → Running gap; acceptable for v0.3.0. Phase 4 may introduce a `pre_start` hook on `ManagedService`.

---

## 5. Config Changes

Add fields to `HearthConfig` in `crates/hearth-lib/src/config.rs`. `#[serde(default)]` is already on the struct, so missing fields fall back to `Default`.

```rust
pub struct HearthConfig {
    // ...existing fields...

    /// Port for MySQL/MariaDB (default: 3306)
    pub mysql_port: u16,
    /// Port for Postgres (default: 5432)
    pub postgres_port: u16,
    /// Port for Redis (default: 6379)
    pub redis_port: u16,

    /// Optional data-dir override per engine (defaults to ~/.config/hearth/data/{engine})
    pub mysql_data_dir: Option<PathBuf>,
    pub postgres_data_dir: Option<PathBuf>,
    pub redis_data_dir: Option<PathBuf>,

    /// Optional binary version pin (e.g. "8.0" for mysql, "17" for postgres).
    /// Today only informational; honored once cache-download lands in Phase 4.
    pub mysql_version: Option<String>,
    pub postgres_version: Option<String>,
    pub redis_version: Option<String>,
}
```

`Default` impl extends with literals `3306`, `5432`, `6379`, all `Option::None` for overrides.

Add unit tests:
- `default_config_has_db_ports` — asserts the three port defaults.
- `load_legacy_config_without_db_fields` — writes a Phase 2 TOML, asserts defaults backfill, existing fields preserved (mirrors `load_legacy_config_without_mcp_port`).

---

## 6. Supervisor Registration

In `crates/hearth-lib/src/service/manager.rs::default_services`, append three conditional blocks **after** the Mailpit block:

```rust
if let Some(bin) = crate::db::mysql::resolve_mysqld_binary(config_dir) {
    if !port_in_use("127.0.0.1", config.mysql_port) {
        services.push(crate::db::mysql::managed_service(&bin, config, config_dir));
    } else {
        info!(port = config.mysql_port, "mysql port already in use — skipping registration");
    }
}
// repeat for postgres + redis, same port-in-use guard
```

Each `crate::db::{engine}::managed_service` returns a fully-formed `ManagedService` with command, args, and (for mysql/postgres) any pre-start init logic encapsulated in a wrapper script approach — see §9 for the init wrinkle.

Health-check signature: rely on `ManagedService::is_alive()` (PID alive check) plus a new per-engine TCP probe used only by `hearth db status` (not by the running health loop, which stays PID-based to avoid 3-second TCP timeouts on every 5s tick).

```rust
// crates/hearth-lib/src/db/health.rs
pub async fn tcp_probe(port: u16, timeout_ms: u64) -> bool {
    tokio::time::timeout(
        Duration::from_millis(timeout_ms),
        tokio::net::TcpStream::connect(("127.0.0.1", port)),
    ).await.map(|r| r.is_ok()).unwrap_or(false)
}
```

---

## 7. Herd Coexistence

`is_herd_running()` already exists. New helper `port_in_use(host, port) -> bool` (sync TCP probe with 200ms timeout) handles the finer-grained case where Herd Pro's Services panel owns the standard DB port.

Rules:

| Engine | Herd ships it? | Default behavior |
|--------|----------------|------------------|
| MySQL  | Yes (MariaDB)  | Register only if port 3306 is free. If Herd is running AND 3306 is bound, log & skip. |
| Postgres | No           | Register if 5432 is free (Postgres install is rare on Herd machines, but a Homebrew postgres can collide). |
| Redis  | No             | Register if 6379 is free. |

Detection sequence at daemon startup:
1. `is_herd_running()` — informational log only; we no longer hard-skip mysql.
2. `port_in_use(...)` — authoritative; this catches Herd Pro Services panel, Homebrew services, and ad-hoc processes equally.
3. If skipped, the service is **not** registered at all (mirrors the Mailpit-binary-missing case), so `hearth db start mysql` returns "service not registered" — pointing the user at the conflict.

`hearth db status --json` includes a `conflict_port: bool` flag derived from port-in-use so the CLI can render `mysql: skipped (port 3306 owned by another process)`.

---

## 8. MCP Tool Additions

Add to `crates/hearth-lib/src/mcp.rs` `#[tool_router] impl HearthMcpServer`:

```rust
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct DbActionParams {
    /// Engine name: mysql, postgres, redis. Omit to target all DB engines.
    #[serde(default)]
    pub engine: Option<String>,
}

#[tool(name = "hearth_db_start",
       description = "Start a database engine (mysql, postgres, redis) or all DB engines")]
async fn hearth_db_start(&self, Parameters(p): Parameters<DbActionParams>) -> Result<String, String> { ... }

#[tool(name = "hearth_db_stop",
       description = "Stop a database engine or all DB engines")]
async fn hearth_db_stop(&self, Parameters(p): Parameters<DbActionParams>) -> Result<String, String> { ... }

#[tool(name = "hearth_db_status",
       description = "Report state, pid, port, and data dir for each registered DB engine")]
async fn hearth_db_status(&self) -> Result<String, String> { ... }
```

Implementation pattern mirrors `hearth_service_restart`: lock `supervisor`, iterate matching `ServiceKind`s (`Mysql`, `Postgresql`, `Redis`), call `start_service`/`stop_service`. `tool_router_lists_eight_tools` test bumps to **eleven**.

No new MCP tool for `db config` — `hearth_php_config` is the precedent for "rare config writes belong to the CLI, not the agent surface" (avoids agents footgun-ing port changes mid-session).

---

## 9. Files to Create/Modify

| File | Action | Est. LOC | Notes |
|------|--------|---------:|-------|
| `crates/hearth-lib/src/db.rs` | create | 10 | `pub mod mysql; pub mod postgres; pub mod redis; pub mod health;` |
| `crates/hearth-lib/src/db/mysql.rs` | create | 130 | resolver + `managed_service` builder + `init_datadir` |
| `crates/hearth-lib/src/db/postgres.rs` | create | 140 | resolver returns `(postgres, initdb, version)`; `init_datadir` runs `initdb` |
| `crates/hearth-lib/src/db/redis.rs` | create | 80 | dataless; just resolver + args builder |
| `crates/hearth-lib/src/db/health.rs` | create | 40 | `tcp_probe`, `port_in_use` |
| `crates/hearth-lib/src/lib.rs` | modify | +1 | add `pub mod db;` |
| `crates/hearth-lib/src/config.rs` | modify | +60 | 9 new fields + Default + 2 tests |
| `crates/hearth-lib/src/service/manager.rs` | modify | +50 | 3 conditional registration blocks, 2 tests |
| `crates/hearth-lib/src/socket.rs` | modify | +30 | 5 new variants + serde round-trip cases |
| `crates/hearth-lib/src/mcp.rs` | modify | +120 | 3 tools + params struct + 3 tests; bump tool count test to 11 |
| `crates/hearth-daemon/src/main.rs` | modify | +90 | dispatcher arms for 5 new requests |
| `crates/hearth-cli/src/main.rs` | modify | +110 | `Db` subcommand + `DbCommands` enum + dispatcher |
| `scripts/smoke-test.sh` | modify | +50 | DB-engine probes (see §10) |
| `docs/plans/db-services.md` | this file | — | — |
| `README.md` | modify | +20 | document `hearth db` |
| `CHANGELOG.md` | modify | +12 | Unreleased section |
| `CLAUDE.md` | modify | +6 | bump MCP tool count to 11, add db module row |

**Total estimate: ~950 LOC across 16 files.** No new workspace dependencies.

**Crate deps**: explicitly DO NOT add `mysql_async`, `tokio-postgres`, `redis`, or any libpq binding. Health checks use `tokio::net::TcpStream` (already in tokio). Port-in-use uses `std::net::TcpStream::connect_timeout` (already in std).

---

## 10. Test Plan

### Unit tests (per engine)

`crates/hearth-lib/src/db/mysql.rs`:
- `resolves_from_hearth_cache` — writes fake binary in `services/mysql/bin/mysqld`, asserts resolver returns it.
- `resolves_from_herd_when_cache_missing` — sets `$HOME` to a tmpdir with a fake Herd layout, asserts Herd path wins over Homebrew (which won't exist in the tmpdir).
- `returns_none_when_nothing_installed` — tmpdir with no binaries; resolver returns None.
- `managed_service_args_include_datadir` — built `ManagedService` has `--datadir` in args.

`crates/hearth-lib/src/db/postgres.rs`:
- `resolver_requires_both_postgres_and_initdb` — only `postgres` in fake homebrew dir → None.
- `resolver_picks_highest_version` — fake `postgresql@16` and `postgresql@17`; resolver picks 17.

`crates/hearth-lib/src/db/redis.rs`:
- `resolves_from_hearth_cache` (mirrors mailpit test).
- `args_include_dbfilename_and_dir`.

`crates/hearth-lib/src/db/health.rs`:
- `tcp_probe_false_on_unbound_port` — pick high port, assert false.
- `tcp_probe_true_on_listening_socket` — bind tokio listener, assert true.
- `port_in_use_returns_true_for_listener`.

### Manager tests
`crates/hearth-lib/src/service/manager.rs`:
- `default_services_skips_mysql_when_port_in_use` — bind 3306 in test (use OS-assigned port via injected config), assert no Mysql `ServiceKind` registered.
- `default_services_registers_all_three_when_binaries_present` — three fake binaries in tmpdir cache, assert kinds set contains `Mysql`, `Postgresql`, `Redis`.

### Config tests
Add to existing `mod tests` in `config.rs`:
- `default_config_has_db_ports` — 3306/5432/6379.
- `load_legacy_config_without_db_fields` — writes Phase 2 TOML (no db_* keys), asserts defaults backfill and tld/default_php preserved.

### MCP tests
In `mcp.rs::tests`:
- `db_start_unknown_engine_errors`.
- `db_status_reports_all_registered_db_kinds`.
- Update `tool_router_lists_eight_tools` → `tool_router_lists_eleven_tools`.

### Smoke test additions (`scripts/smoke-test.sh`)
After the MCP section, add a `── 10. DB engines ──` block:

```bash
info "Testing DB engine commands..."

# Status — must succeed even with zero engines installed
if OUTPUT=$($CLI db status 2>&1); then
    pass "hearth db status — command succeeded"
else
    fail "hearth db status — failed"
fi

# JSON output is parseable
if $CLI db status --json 2>/dev/null | python3 -c 'import sys, json; json.load(sys.stdin)' 2>/dev/null; then
    pass "hearth db status --json — valid JSON"
else
    warn "JSON output unparseable (skipped if python3 missing)"
fi

# Start mysql — soft pass: succeeds OR returns 'not registered' on machines without mysqld
OUTPUT=$($CLI db start mysql 2>&1) || true
if echo "$OUTPUT" | grep -qiE "started|not registered"; then
    pass "hearth db start mysql — expected start or not-registered response"
else
    fail "hearth db start mysql — unexpected: $OUTPUT"
fi

# Same for redis
OUTPUT=$($CLI db start redis 2>&1) || true
echo "$OUTPUT" | grep -qiE "started|not registered" \
    && pass "hearth db start redis — expected response" \
    || fail "hearth db start redis — unexpected: $OUTPUT"

# Stop all — must not fail even if nothing is running
$CLI db stop all >/dev/null 2>&1 && pass "hearth db stop all — succeeded" || fail "hearth db stop all"
```

Smoke test stays tolerant of missing engines so CI on a fresh CI box still passes; the maintainer-machine dogfood run is where real start/stop is exercised.

---

## 11. Sequencing

Implement in this order. Each engine ships in its own PR/commit chain so dogfooding can begin without waiting for all three.

1. **Foundation (Task block A — 1 day)**
   - `db/health.rs` (`tcp_probe`, `port_in_use`).
   - `HearthConfig` field additions + tests.
   - `db.rs` module skeleton.
   - `DaemonRequest::Db*` variants + serde tests.
   - CLI subcommand wiring (dispatch only, returns "not implemented" until daemon arms land).

2. **Postgres (Task block B — 1 day)**
   - Recommended first engine: **simplest init flow** (`initdb -D` is one shot; no `--initialize-insecure` flag quirks; no MariaDB-vs-MySQL ambiguity).
   - Implement `db/postgres.rs`, register in `default_services`, daemon arm.
   - Smoke-test step 10 passes for postgres.
   - **Dogfood checkpoint:** maintainer points Laravel `.env` at Hearth-managed postgres on port 5432.

3. **Redis (Task block C — 0.5 day)**
   - Dataless = trivial. `db/redis.rs`, register, arm.
   - **Dogfood checkpoint:** `php artisan queue:work redis` against Hearth-managed redis.

4. **MySQL (Task block D — 1.5 days)**
   - Last because: MariaDB-as-mysqld via Herd needs runtime branching (the binary self-identifies on `--version`), and `--initialize-insecure` write-out is fragile on macOS APFS.
   - Implement `db/mysql.rs`, register, arm.
   - **Dogfood checkpoint:** Laravel app reads/writes against Hearth mysql while Herd Pro is shut down.

5. **MCP tools (Task block E — 0.5 day)**
   - Three tools in `mcp.rs`. Touches all three engines so it lands last to avoid intermediate "8 → 9 → 10 → 11" tool-count test churn.

6. **Docs (Task block F — 0.5 day)**
   - README section, CHANGELOG entry, CLAUDE.md updates (MCP tool count, db module row).

**Total estimate: 5 working days.** Leaves margin in the 7-day window for the parallel `hearth add` track and a real dogfooding day.

---

## 12. Risks

1. **macOS firewall popup on first DB start.** `mysqld`/`postgres`/`redis-server` aren't code-signed by us; macOS will prompt. *Mitigation:* document the prompt in README; bind explicitly to `127.0.0.1` so the firewall message is at most a one-time annoyance, not a recurring loop.

2. **Port collisions with Herd Pro Services panel.** Renewal-week users still have Herd Pro running. *Mitigation:* §7 port-in-use guard. Skipped registration is loud (status shows `conflict_port: true`) so the user knows what to do.

3. **MariaDB vs MySQL `mysqld` divergence.** Herd ships MariaDB-as-mysqld; `--initialize-insecure` semantics differ between MySQL 8 and MariaDB 10+. *Mitigation:* probe `mysqld --version` at init; branch the init args. Acceptance: works for MariaDB 10.6+ (Herd) and MySQL 8.0+ (Homebrew). Older versions out of scope.

4. **Postgres init creates user matching `$USER`.** If `$USER` is unusual (CI: `runner`), Laravel `.env` needs `DB_USERNAME` matched. *Mitigation:* document in README; `hearth db status` prints the data-dir's `pg_role` so the user can copy it into `.env`. Do not try to auto-rewrite Laravel `.env`.

5. **Data-dir corruption on hard kill.** Circuit breaker still SIGKILLs after 5s. Postgres + MySQL recover via crash-recovery on next start; redis loses last <1s of writes if AOF disabled. *Mitigation:* document; default redis to `appendonly no` (matches Herd Pro default); do not try to flip SIGKILL → SIGTERM-only for DB engines (would violate the "no orphan processes" invariant from NORTH_STAR).

6. **Init step blocks supervisor start_all for >5s on first run.** `initdb` on macOS can take 3–10s. *Mitigation:* run init in `ManagedService::start` synchronously; accept the one-time delay. If users complain in dogfood, Phase 4 introduces an async `pre_start` hook.

7. **Health-check TCP probe creates per-tick connection churn.** *Mitigation:* probe is opt-in via `hearth db status` only; the 5-second health loop stays PID-based. No regression for non-DB services.

8. **Homebrew path drift.** Apple Silicon vs Intel Macs use `/opt/homebrew` vs `/usr/local`. *Mitigation:* probe both prefixes in resolver; document Apple Silicon as the primary target (matches NORTH_STAR's "macOS-first" disqualification of Intel-only paths).

9. **Test flakiness from real port binding.** `port_in_use` tests bind real sockets. *Mitigation:* use OS-assigned port (`:0`) plus retrieve via `local_addr()`; never hardcode 3306/5432/6379 in tests.

10. **Renewal-week scope creep.** Tempting to add `hearth db reset`, `hearth db shell`, `hearth db backup`. *Mitigation:* explicit non-goals in §1. These are Phase 4. If a stakeholder pushes, point at NORTH_STAR decision principle #1 ("Cancel-renewal beats everything else this week").
