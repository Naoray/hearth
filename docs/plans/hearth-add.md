# `hearth add` Implementation Plan — v0.3.0 (Phase 3)

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans`. This plan favors a structured spec format over per-step checkboxes because each recipe is internally coherent; the sequencing section provides the bite-sized TDD task list.

**Goal:** Ship `hearth add horizon|telescope|pulse|reverb` — a guided, interactive Laravel package installer that runs `composer require`, writes opinionated config, and registers long-running workers as Hearth-supervised services.

**Architecture:** New `hearth-lib::add` module with a `Recipe` trait per package. CLI invokes daemon via new `DaemonRequest::Add`. Daemon resolves site context, runs Composer + Artisan against the target Laravel app, patches `.env`, and (for Horizon/Reverb) persists the worker in `HearthConfig.added_packages` so the supervisor re-registers it on every daemon restart.

**Tech stack:** Rust 2024, `dialoguer 0.11` (interactive prompts), existing `command-group` + `tracing` + `serde` infrastructure.

**Deadline:** 2026-05-26 (5 working days from kickoff).

---

## 1. Goal & non-goals

### Goal (mirrors NORTH_STAR)
Let a maintainer in a Laravel app directory type `hearth add horizon` once and walk away with: package installed, sensible config written, queue worker running under Hearth's supervisor, surviving daemon restarts. Same for Telescope, Pulse, Reverb.

### Non-goals (explicit)
- **Not a generic Composer wrapper.** Only the 4 named packages. Arbitrary package install stays the user's job.
- **Not a Laravel installer / scaffolder.** That's `hearth laravel new`. `hearth add` only operates on existing Laravel apps.
- **No telemetry.** No phone-home, no usage counting. NORTH_STAR §"No paid tiers, no telemetry".
- **No remove command in v0.3.0.** `hearth remove horizon` is a fast-follow; ship `add` first.
- **No version pinning UI.** Always installs latest compatible version per the user's `composer.json` constraints. Power users can edit `composer.json` themselves.
- **No multi-site batch install.** One site per invocation. Resolved via cwd or `--site`.
- **No Linux/Windows.** macOS-only (NORTH_STAR §"Hard constraints").

---

## 2. CLI surface

### Subcommand
```
hearth add <PACKAGE> [--site=PATH] [--yes] [--no-supervise] [--dry-run]
```

### Supported packages (closed enum, validated at parse time)
- `horizon`   — `laravel/horizon`
- `telescope` — `laravel/telescope`
- `pulse`     — `laravel/pulse`
- `reverb`    — `laravel/reverb`

### Flags
| Flag | Default | Effect |
|------|---------|--------|
| `--site=PATH` | resolved from cwd | Absolute path to a linked Laravel site; must match an entry in `SiteManager.list_sites()`. |
| `--yes` / `-y` | false | Skip prompts; accept all defaults. Equivalent to non-interactive CI usage. |
| `--no-supervise` | false | Install + configure but do NOT register Horizon/Reverb workers in the supervisor. (Pulse/Telescope ignore this flag.) |
| `--dry-run` | false | Print the actions that would be taken; touch no files, run no Composer. For test harness + smoke test. |

### Clap surface (`crates/hearth-cli/src/main.rs`)
```rust
#[derive(Subcommand)]
enum Commands {
    // ... existing variants ...
    /// Guided install of a known Laravel package (horizon, telescope, pulse, reverb)
    Add {
        #[arg(value_enum)]
        package: AddPackage,
        #[arg(long)]
        site: Option<PathBuf>,
        #[arg(long, short = 'y')]
        yes: bool,
        #[arg(long)]
        no_supervise: bool,
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum AddPackage { Horizon, Telescope, Pulse, Reverb }
```

### Wire protocol (`crates/hearth-lib/src/socket.rs`)
Add to `DaemonRequest`:
```rust
Add {
    package: String,           // "horizon" | "telescope" | "pulse" | "reverb"
    site_path: String,         // absolute, already-resolved by CLI
    answers: AddAnswers,       // pre-collected prompt answers (CLI owns interactivity)
    no_supervise: bool,
    dry_run: bool,
},
```

`AddAnswers` is a serde struct with all recipe-specific fields (Option<String>); each recipe reads only the keys it needs. Interactivity stays in the **CLI** — daemon never opens a TTY. Daemon receives a fully-specified, non-interactive job.

---

## 3. Per-package recipes

Each recipe implements:
```rust
pub trait Recipe {
    fn package(&self) -> &'static str;
    fn composer_constraint(&self) -> &'static str; // e.g. "laravel/horizon"
    fn composer_dev(&self) -> bool;                // --dev for Telescope only
    fn prompts(&self) -> Vec<Prompt>;              // declarative; CLI renders
    fn apply(&self, ctx: &RecipeContext, answers: &AddAnswers) -> anyhow::Result<RecipeOutcome>;
    fn supervised(&self) -> Option<SupervisedSpec>; // None for Telescope/Pulse
}
```

`RecipeContext` carries: site root path, resolved PHP binary, dry-run flag, tracing span.

`RecipeOutcome` reports what was changed (composer install? env keys written? artisan commands run? service registered?) so the daemon can build the user-facing summary.

### 3.1 Horizon (`laravel/horizon`)

**Composer:** `composer require laravel/horizon --no-interaction`

**Artisan (post-install):**
1. `php artisan horizon:install` (publishes `config/horizon.php` + asset publish + service provider register)

**Prompts:**
| Key | Type | Default | Validation |
|-----|------|---------|------------|
| `horizon_connection` | select | `redis` | one of `redis`, `database`, `sqs`, `beanstalkd` |
| `horizon_environment` | text | `local` | non-empty; key in `config/horizon.php` `environments` array |
| `horizon_max_processes` | integer | `3` | 1..=64 |

**Files written/edited:**
- `.env` keys: `QUEUE_CONNECTION` ← user choice; if `redis`, also set `REDIS_CLIENT=phpredis` only if unset.
- `config/horizon.php` is auto-generated by `horizon:install`; we do NOT mutate it (avoid stomping on user customization). The maintainer can tune via the published config.

**Supervised process:**
```text
command: <site_php_binary>
args:    ["artisan", "horizon"]
cwd:     <site_path>
kind:    ServiceKind::Horizon
restart: circuit breaker default (3 in 60s)
stop:    SIGTERM (Horizon handles graceful shutdown; supervisor sends to process group)
```

### 3.2 Telescope (`laravel/telescope`)

**Composer:** `composer require laravel/telescope --dev --no-interaction`

**Artisan (post-install):**
1. `php artisan telescope:install`
2. `php artisan migrate --force` (Telescope ships migrations)

**Prompts:**
| Key | Type | Default | Validation |
|-----|------|---------|------------|
| `telescope_environments` | multi-select | `[local]` | subset of `[local, staging, production]` |
| `telescope_enable_in_prod` | confirm | `false` | n/a |

**Files written/edited:**
- `.env`: `TELESCOPE_ENABLED=true` (or `false` if user opts out of all envs)
- `app/Providers/TelescopeServiceProvider.php` — auto-created by `telescope:install`. We patch the `gate()` closure ONLY if `telescope_enable_in_prod=false`, replacing the default with `return $this->app->environment('local');` to add a guard. Backup the file before edit (see §5).

**Supervised:** none (Telescope is request-driven middleware).

### 3.3 Pulse (`laravel/pulse`)

**Composer:** `composer require laravel/pulse --no-interaction`

**Pre-check:** parse `composer.json` `require.laravel/framework`. If constraint resolves to `<10.x`, bail with `"Pulse requires Laravel 10+, found {version}"`. (Laravel version detection in §11.)

**Artisan (post-install):**
1. `php artisan vendor:publish --tag=pulse-config --no-interaction`
2. `php artisan vendor:publish --tag=pulse-migrations --no-interaction`
3. `php artisan migrate --force`

**Prompts:**
| Key | Type | Default | Validation |
|-----|------|---------|------------|
| `pulse_storage_driver` | select | `database` | one of `database`, `redis` |
| `pulse_ingest_trim_lottery` | integer | `100` | 1..=10000 (advanced; only if `--yes` not set, hide behind "advanced" disclosure prompt) |

**Files written/edited:**
- `.env`: `PULSE_INGEST_DRIVER=<choice>`, `PULSE_STORAGE_DRIVER=<choice>`
- `config/pulse.php` — published by step 1; we do not mutate.

**Supervised:** none in v0.3.0. (Pulse has an optional `pulse:check` recorder that benefits from a long-running worker; defer to a follow-up because it requires a Redis-or-DB queue already configured. Print a one-line hint after install.)

### 3.4 Reverb (`laravel/reverb`)

**Composer:** `composer require laravel/reverb --no-interaction`

**Artisan (post-install):**
1. `php artisan reverb:install --no-interaction` (publishes `config/reverb.php` and seeds `.env` with REVERB_APP_ID/KEY/SECRET if absent)

**Prompts:**
| Key | Type | Default | Validation |
|-----|------|---------|------------|
| `reverb_host` | text | `0.0.0.0` | valid IP or hostname |
| `reverb_port` | integer | `8080` | 1024..=65535; check port is free via `TcpListener::bind`; if busy, suggest +1 |
| `reverb_hostname` | text | `<site_name>.test` | non-empty (used for `REVERB_HOST` from client) |
| `reverb_scheme` | select | `http` | one of `http`, `https` |

**Files written/edited:**
- `.env`: `REVERB_HOST=<reverb_hostname>`, `REVERB_PORT=<reverb_port>`, `REVERB_SCHEME=<reverb_scheme>`, plus broadcast config: `BROADCAST_CONNECTION=reverb`. Preserve any `REVERB_APP_*` written by `reverb:install`.

**Supervised process:**
```text
command: <site_php_binary>
args:    ["artisan", "reverb:start", "--host=<reverb_host>", "--port=<reverb_port>"]
cwd:     <site_path>
kind:    ServiceKind::Reverb
restart: circuit breaker default
```

---

## 4. Interactive UX

### Library
`dialoguer 0.11` — workspace dep. Added to **CLI** crate only (daemon never prompts).

`Prompt` enum in `add/prompt.rs`:
```rust
pub enum Prompt {
    Text   { key: String, message: String, default: Option<String> },
    Confirm{ key: String, message: String, default: bool },
    Select { key: String, message: String, options: Vec<String>, default_index: usize },
    Integer{ key: String, message: String, default: i64, min: i64, max: i64 },
}
```

CLI loops over `recipe.prompts()`, renders via dialoguer, collects into `AddAnswers`, then ships the whole thing to the daemon over the socket. With `--yes`, the CLI skips rendering and uses defaults directly.

### Mock transcript 1 — Horizon, interactive
```
$ cd ~/Sites/shopfront
$ hearth add horizon
► Detected Laravel app at /Users/me/Sites/shopfront (Laravel 11.x, PHP 8.4)

Queue connection? (redis/database/sqs/beanstalkd) [redis]: ▮
Horizon environment name [local]: ▮
Max processes per supervisor (1-64) [3]: ▮

► composer require laravel/horizon
   - Installing laravel/horizon (v5.31.1): Extracting archive...
► php artisan horizon:install
   ✓ Horizon scaffolding installed
► Patching .env (QUEUE_CONNECTION=redis)
   ✓ Backup written to .env.hearth.20260520-094312.bak
► Registering supervised service: horizon
   ✓ horizon started (PID 84211)

Done. Open Horizon dashboard at https://shopfront.test/horizon
```

### Mock transcript 2 — Telescope, `--yes`
```
$ hearth add telescope --site=/Users/me/Sites/blog --yes
► Site: /Users/me/Sites/blog (Laravel 10.43.0, PHP 8.3)
► composer require laravel/telescope --dev
► php artisan telescope:install
► php artisan migrate --force
► Patching .env (TELESCOPE_ENABLED=true)
► Patching app/Providers/TelescopeServiceProvider.php (gate: local only)
Done.
```

### Mock transcript 3 — Reverb, port collision
```
$ hearth add reverb
► Detected Laravel app at /Users/me/Sites/chatapp

Reverb host [0.0.0.0]: ▮
Reverb port (1024-65535) [8080]: 8080
  ✗ Port 8080 is already in use. Try 8081? [Y/n]: ▮
Reverb hostname [chatapp.test]: ▮
Reverb scheme (http/https) [http]: https

► composer require laravel/reverb
► php artisan reverb:install --no-interaction
► Patching .env (REVERB_HOST, REVERB_PORT, REVERB_SCHEME, BROADCAST_CONNECTION)
► Registering supervised service: reverb
   ✓ reverb started on https://chatapp.test:8081
```

---

## 5. Idempotency & safety

### Composer
- Before `composer require`: parse `composer.json` (read-only). If the package is already in `require` (or `require-dev` for Telescope), **skip composer install**, log `"laravel/horizon already in composer.json — skipping install"`, but still run downstream artisan + env patches (idempotent by design).

### Artisan
- `horizon:install`, `telescope:install`, `reverb:install` are idempotent in modern Laravel (they prompt to overwrite; we pass `--no-interaction` which defaults to "no" for overwrites). For `vendor:publish` use `--existing` is too aggressive; rely on default no-overwrite behavior.
- `migrate --force` is idempotent.

### `.env` editing (`add/env.rs`)
- Read whole file, parse line-by-line preserving comments, blank lines, and order.
- For each `KEY=VALUE` to write: if `KEY` exists, replace value; else append at end of file (after a `# Added by hearth add <package>` comment).
- **Backup before any write:** `.env` → `.env.hearth.YYYYMMDD-HHMMSS.bak` (one backup per `hearth add` invocation, not per key). Skip if backup file with same timestamp exists (shouldn't happen, paranoid guard).
- Never write secrets to logs (mask values for keys matching `*KEY*|*SECRET*|*PASSWORD*|*TOKEN*` in trace output).

### `TelescopeServiceProvider.php` patch
- Backup file → `<file>.hearth.YYYYMMDD-HHMMSS.bak`.
- Find `protected function gate()` body. If it already contains `'local'` or `environment(`, skip patch (user-customized).
- Replace ONLY the array literal inside the closure. Do not regenerate the whole class.

### Re-run safety matrix
| State | `hearth add horizon` second time |
|-------|----------------------------------|
| Already installed via Composer + supervised | No-op composer, refresh config (with confirm), restart Horizon, log "already added" |
| Installed via Composer but not supervised (manual install pre-Hearth) | Skip composer, run idempotent patches, register supervisor |
| Supervised but Composer dep missing (someone ran `composer remove`) | Bail: `"horizon supervised in HearthConfig but composer.json no longer requires it; run hearth remove horizon first"` |

---

## 6. Supervisor integration

### New `ServiceKind` variants (`crates/hearth-lib/src/service.rs`)
```rust
pub enum ServiceKind {
    // existing ...
    Horizon,
    Reverb,
}
```
Update `name()`, `Display`, `FromStr` (accept `horizon`, `reverb`).

### Config persistence (`crates/hearth-lib/src/config.rs`)
Add:
```rust
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AddedPackage {
    pub package: String,         // "horizon" | "reverb"
    pub site_path: PathBuf,
    pub site_name: String,       // for logging
    pub command: String,         // resolved php binary
    pub args: Vec<String>,       // ["artisan", "horizon"] etc.
    pub installed_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct HearthConfig {
    // existing fields ...
    pub added_packages: Vec<AddedPackage>,
}
```
`#[serde(default)]` on `HearthConfig` already provides forward compat — old config files without `added_packages` will load with empty Vec (verified by existing `load_legacy_config_without_mcp_port` test pattern).

### Registration (`crates/hearth-lib/src/service/manager.rs`)
Extend `default_services()`:
```rust
pub fn default_services(config: &HearthConfig, config_dir: &Path) -> Vec<ManagedService> {
    let mut services = /* existing */;
    for pkg in &config.added_packages {
        let kind = match pkg.package.as_str() {
            "horizon" => ServiceKind::Horizon,
            "reverb"  => ServiceKind::Reverb,
            _ => continue, // unknown / non-supervised
        };
        services.push(ManagedService::with_cwd(
            kind,
            pkg.command.clone(),
            pkg.args.clone(),
            pkg.site_path.clone(),
        ));
    }
    services
}
```

### `ManagedService` cwd support
Current `ManagedService::new(kind, command, args)` always spawns in the daemon's cwd. Horizon/Reverb MUST run in the Laravel site dir. Add:
```rust
impl ManagedService {
    pub fn with_cwd(kind: ServiceKind, command: String, args: Vec<String>, cwd: PathBuf) -> Self { ... }
}
```
Internally store `cwd: Option<PathBuf>` and pass to `Command::current_dir(...)` in `start()`. Existing tests that use `ManagedService::new` remain valid (cwd = None).

### Visibility in `hearth status`
Existing status flow already calls `supervisor.status()` which iterates registered services. New entries appear automatically. Example:
```
SERVICE         STATE           PID
----------------------------------------
nginx           Running         84001
php-fpm         Running         84002
horizon         Running         84211
reverb          Running         84257
```

### Daemon restart survival
On `hearth daemon start` → daemon main → `HearthConfig::load()` reads `added_packages` → `default_services()` registers them → `supervisor.start_all()` spawns them. No extra wiring needed beyond §6's edits. Validates NORTH_STAR §"0 orphan processes" because everything is still a process group child of the daemon.

---

## 7. Site context resolution

### Module: `crates/hearth-lib/src/add/site_context.rs`

```rust
pub struct SiteContext {
    pub site: Site,             // from site::Site
    pub php_binary: PathBuf,    // resolved
    pub laravel_version: String,// e.g. "11.0.0" — parsed from composer.json
}

pub fn resolve(
    explicit: Option<&Path>,
    cwd: &Path,
    site_manager: &SiteManager,
    config: &HearthConfig,
) -> Result<SiteContext, SiteResolutionError>;
```

### Resolution algorithm
1. If `--site=PATH` given: canonicalize PATH. Find a `Site` whose canonicalized `path` matches. Error `SiteNotLinked` if no match.
2. Else: walk from `cwd` upward via `parent()`. At each level, check if the dir matches any linked `Site.path`. First hit wins.
3. Error `NotInLinkedSite { cwd, hint }` if no ancestor matches — hint suggests `hearth link` or `--site=PATH`.
4. Validate the resolved path contains `composer.json` AND that the JSON has `require.laravel/framework`. Else error `NotALaravelApp`.
5. Resolve PHP binary: prefer `site.php_version` (Valet isolation), else `config.default_php`. Use existing `php::resolver::resolve_php_binary(version, config_dir)`.
6. Parse Laravel version from the installed `vendor/composer/installed.json` if vendor exists (precise), else from the `composer.json` constraint as a coarse minimum.

### Error type
```rust
#[derive(Debug, thiserror::Error)]
pub enum SiteResolutionError {
    #[error("not in a linked Hearth site (cwd: {cwd}); run `hearth link` or pass --site=PATH")]
    NotInLinkedSite { cwd: PathBuf, hint: String },
    #[error("site path {0} is not in the linked sites list")]
    SiteNotLinked(PathBuf),
    #[error("{0} does not look like a Laravel app (missing composer.json or laravel/framework)")]
    NotALaravelApp(PathBuf),
    #[error("could not resolve PHP {version} binary")]
    PhpUnavailable { version: String },
}
```

CLI prints errors plainly; daemon returns `DaemonResponse::Error { message }`.

---

## 8. Files to create / modify

### Create
| Path | Purpose | Est. LOC |
|------|---------|----------|
| `crates/hearth-lib/src/add/mod.rs` | `pub mod ...`; `Recipe` trait; `AddAnswers`; dispatch fn `apply_recipe(package, ctx, answers)` | 90 |
| `crates/hearth-lib/src/add/recipe.rs` | `Recipe` trait + `RecipeContext` + `RecipeOutcome` + `SupervisedSpec` | 60 |
| `crates/hearth-lib/src/add/prompt.rs` | `Prompt` enum (declarative; serde-serializable so CLI ↔ daemon can share definitions if we ever need server-side prompts) | 50 |
| `crates/hearth-lib/src/add/composer.rs` | `run_composer_require(site_path, pkg, dev, dry_run)` — wraps `Command`, streams output via `tracing`, sets `COMPOSER_NO_INTERACTION=1`, returns parsed result | 80 |
| `crates/hearth-lib/src/add/artisan.rs` | `run_artisan(site_path, php_binary, args, dry_run)` — wraps Command similarly | 60 |
| `crates/hearth-lib/src/add/env_file.rs` | `.env` read/parse/patch/write with backup; preserves comments + key order | 140 |
| `crates/hearth-lib/src/add/laravel.rs` | Parse `composer.json`, detect framework version, find vendor dir | 70 |
| `crates/hearth-lib/src/add/site_context.rs` | `SiteContext` + `resolve()` as in §7 | 110 |
| `crates/hearth-lib/src/add/horizon.rs` | `HorizonRecipe` | 90 |
| `crates/hearth-lib/src/add/telescope.rs` | `TelescopeRecipe` + provider patch | 110 |
| `crates/hearth-lib/src/add/pulse.rs` | `PulseRecipe` | 80 |
| `crates/hearth-lib/src/add/reverb.rs` | `ReverbRecipe` + port-free check | 100 |
| **subtotal new code** | | **~1040** |

### Modify
| Path | Change | Est. LOC delta |
|------|--------|----------------|
| `crates/hearth-lib/src/lib.rs` | `pub mod add;` | +1 |
| `crates/hearth-lib/src/service.rs` | Add `Horizon`, `Reverb` to `ServiceKind` + `name()` + `FromStr` + tests | +40 |
| `crates/hearth-lib/src/service/supervisor.rs` | `ManagedService::with_cwd` + `cwd: Option<PathBuf>` field + Command::current_dir + tests | +50 |
| `crates/hearth-lib/src/service/manager.rs` | Register `added_packages` from config; tests | +35 |
| `crates/hearth-lib/src/config.rs` | `AddedPackage` struct + `added_packages: Vec<AddedPackage>` + legacy-load test | +60 |
| `crates/hearth-lib/src/socket.rs` | `DaemonRequest::Add { ... }` + `AddAnswers` struct + serde round-trip test | +50 |
| `crates/hearth-daemon/src/main.rs` | Handle `DaemonRequest::Add` — lock order config → site_manager → supervisor; persist `AddedPackage`, register service, start it | +120 |
| `crates/hearth-cli/src/main.rs` | `Add` subcommand; dialoguer-driven prompt loop; pre-resolve site path via daemon (`Sites` request) then send `Add` | +180 |
| `crates/hearth-lib/Cargo.toml` | `dialoguer = "0.11"` (only if shared; otherwise CLI-only) — actually keep in CLI to avoid coupling lib | 0 |
| `crates/hearth-cli/Cargo.toml` | `dialoguer = "0.11"` | +1 |
| `Cargo.toml` (workspace) | `dialoguer = "0.11"` in `[workspace.dependencies]` | +1 |
| `scripts/smoke-test.sh` | Add §10: `hearth add telescope --yes --site=/tmp/hearth-smoke-test/laravel` against a stub skeleton | +40 |
| `CHANGELOG.md` | New `## [0.3.0] — 2026-05-26` section | +20 |
| `README.md` | No change — `hearth add` is already advertised | 0 |
| **subtotal modified code** | | **~600** |

**Total: ~1640 LOC. Realistic for 5 working days with TDD and parallel recipe work.**

---

## 9. Test plan

### Unit tests (per recipe, in `#[cfg(test)] mod tests`)
Each recipe file ships with mock-filesystem tests using `tempfile::TempDir`:

- `horizon_recipe_skips_composer_when_already_required`
- `horizon_recipe_writes_queue_connection_to_env`
- `horizon_recipe_emits_supervised_spec_for_redis`
- `telescope_recipe_patches_service_provider_gate`
- `telescope_recipe_skips_patch_when_already_customized`
- `pulse_recipe_bails_on_laravel_9` (verify pre-check)
- `reverb_recipe_emits_supervised_spec_with_user_host_and_port`

### `env_file.rs`
- `parses_kv_lines`
- `preserves_comments_and_blank_lines`
- `replaces_existing_key_in_place`
- `appends_new_key_with_hearth_comment`
- `creates_backup_before_first_write`
- `does_not_backup_twice_per_invocation`
- `handles_quoted_values_and_spaces`
- `handles_missing_env_file` (creates new one)
- `does_not_log_secret_values`

### `composer.rs` / `artisan.rs`
- `dry_run_does_not_invoke_command`
- `bubbles_up_nonzero_exit_with_stderr`
- `sets_composer_no_interaction_env`
- `propagates_cwd`

### `site_context.rs`
- `resolves_via_cwd_when_inside_site`
- `resolves_via_explicit_path`
- `walks_up_to_find_ancestor_match`
- `errors_when_not_in_any_site`
- `errors_when_path_not_in_linked_sites`
- `errors_when_no_composer_json`
- `errors_when_not_laravel_framework`
- `picks_php_binary_from_valet_isolation_first`

### `config.rs`
- `added_packages_round_trip_serde`
- `load_legacy_config_without_added_packages` (extends existing pattern)

### `service.rs`
- `service_kind_parses_horizon_and_reverb`
- `service_kind_display_round_trips_horizon_reverb`

### `supervisor.rs`
- `managed_service_with_cwd_passes_current_dir_to_command`

### Integration test — `crates/hearth-lib/tests/add_integration.rs`
Build a stub Laravel skeleton in a tmpdir:
```
laravel/
├── composer.json   # { "require": { "laravel/framework": "^11.0" } }
├── artisan         # #!/usr/bin/env bash\necho "called: $@"
├── .env            # APP_ENV=local\nQUEUE_CONNECTION=sync
├── app/Providers/  # empty
```
Use a faked composer binary (a bash script on PATH that just writes a vendor/ marker) so we don't hit the network. Each test:
1. Construct `SiteContext` pointing at the skeleton.
2. Call `apply_recipe("telescope", ctx, default_answers)`.
3. Assert `.env` was patched, `artisan` was called with the expected args (capture via the stub artisan's output file).

### Smoke test (`scripts/smoke-test.sh`)
Add section 10:
```bash
info "Testing hearth add against a stub Laravel skeleton..."
mkdir -p /tmp/hearth-smoke-test/laravel
cat > /tmp/hearth-smoke-test/laravel/composer.json <<EOF
{ "require": { "laravel/framework": "^11.0" } }
EOF
echo "APP_ENV=local" > /tmp/hearth-smoke-test/laravel/.env
$CLI link /tmp/hearth-smoke-test/laravel >/dev/null 2>&1 || warn "link skipped"

if $CLI add telescope --site=/tmp/hearth-smoke-test/laravel --yes --dry-run 2>&1 | grep -q "composer require laravel/telescope"; then
    pass "hearth add telescope --dry-run prints planned actions"
else
    fail "hearth add telescope --dry-run output unexpected"
fi
```
(`--dry-run` keeps the smoke test fast and network-free.)

### Manual dogfooding checklist (before merging)
- [ ] `hearth add telescope` in a real Laravel 11 app, navigate to `/telescope`, see dashboard
- [ ] `hearth add horizon` in same app with Redis running, run `php artisan tinker` → `dispatch(new TestJob())` → see job in Horizon UI
- [ ] Restart daemon (`hearth daemon stop && hearth daemon start`) → `hearth status` shows horizon running
- [ ] Kill horizon process manually → supervisor restarts it within 5s
- [ ] Kill horizon 3x within 60s → supervisor stops trying (circuit breaker)
- [ ] Run `hearth add horizon` twice — second run is a no-op with friendly message

---

## 10. Sequencing

Ship in tight TDD slices. Each task ends with a green `cargo test` + a commit. Order chosen to minimize integration risk:

### Task 1 — Foundation (no recipes, no CLI)
**Files:** `add/mod.rs`, `add/recipe.rs`, `add/prompt.rs`, `add/composer.rs`, `add/artisan.rs`, `add/env_file.rs`, `add/laravel.rs`, `add/site_context.rs`.
**TDD steps:**
- [ ] Write `.env` editor tests → implement `env_file.rs`
- [ ] Write composer/artisan wrapper tests with dry-run → implement
- [ ] Write `site_context::resolve` tests → implement
- [ ] Write `Recipe` trait + dispatch (`apply_recipe`) tests using a `DummyRecipe` → implement
- [ ] Commit: `feat(add): scaffold recipe trait + composer/env/site primitives`

### Task 2 — Telescope (simplest, validates the foundation end-to-end)
**Why first:** no supervision, single artisan call chain, exercises composer + env + provider-patch paths together. If Telescope works, the harness is sound.
- [ ] Write `TelescopeRecipe` tests
- [ ] Implement `telescope.rs`
- [ ] Integration test with stub Laravel skeleton
- [ ] Commit: `feat(add): telescope recipe`

### Task 3 — Wire CLI + Daemon
**Files:** `socket.rs` (`Add` variant + `AddAnswers`), `cli/main.rs` (`Add` subcommand + dialoguer loop), `daemon/main.rs` (handler + lock order).
- [ ] Add `DaemonRequest::Add` + serde round-trip test
- [ ] Add `Add` subcommand with `--yes`, `--dry-run`, `--site` to clap parser
- [ ] Daemon handler: resolve `SiteContext`, dispatch recipe, return `Ok { message }` summary
- [ ] CLI runs Telescope end-to-end against the stub skeleton
- [ ] Commit: `feat(add): wire telescope through CLI ↔ daemon`

### Task 4 — Horizon (introduces supervision)
**Files:** add `ServiceKind::Horizon`, `ManagedService::with_cwd`, `AddedPackage`, persist in `HearthConfig`, register from `default_services`.
- [ ] Tests: `ServiceKind::Horizon` parse/display, `with_cwd` Command setup, `AddedPackage` serde, `default_services` includes added packages
- [ ] Implement supervisor cwd + config persistence
- [ ] Implement `horizon.rs` recipe
- [ ] Daemon: after recipe apply, persist `AddedPackage` (lock order config → supervisor), register service, start it
- [ ] Manual: `hearth add horizon` in dogfood app, restart daemon, verify Horizon comes back
- [ ] Commit: `feat(add): horizon recipe + supervised worker persistence`

### Task 5 — Reverb (second supervised, port-check edge case)
- [ ] Add `ServiceKind::Reverb`
- [ ] Reverb recipe with port-free check + `reverb:install --no-interaction`
- [ ] Integration test
- [ ] Manual smoke
- [ ] Commit: `feat(add): reverb recipe`

### Task 6 — Pulse (Laravel version pre-check)
- [ ] Laravel version detection in `laravel.rs` if not already implemented
- [ ] Pulse recipe with `<L10 bail` path
- [ ] Tests
- [ ] Commit: `feat(add): pulse recipe`

### Task 7 — Smoke test + CHANGELOG + tag
- [ ] Add `scripts/smoke-test.sh` block from §9
- [ ] Update `CHANGELOG.md` with `[0.3.0]` entries
- [ ] Bump `Cargo.toml` workspace version to `0.3.0`
- [ ] Final `cargo test --workspace && ./scripts/smoke-test.sh`
- [ ] Commit: `chore: v0.3.0 release`
- [ ] `git tag v0.3.0`

**Total schedule:** Tasks 1–3 = day 1–2 (foundation + first end-to-end). Tasks 4–6 = day 3–4 (3 recipes in parallel possible). Task 7 + dogfood = day 5. 2 day buffer remains before 2026-05-26 deadline.

---

## 11. Risks

### R1 — Composer hijacks stdin
**Risk:** `composer require` opens an interactive prompt (e.g. "Allow plugin foo to run?") and blocks forever because daemon has no TTY.
**Mitigation:** Always pass `--no-interaction` AND set env `COMPOSER_NO_INTERACTION=1` AND set `COMPOSER_ALLOW_SUPERUSER=1` (harmless on macOS but suppresses some prompts). Set `Stdio::null()` for stdin so any stray prompt errors out fast instead of hanging.

### R2 — Composer is slow (multi-minute installs)
**Risk:** Our current daemon process model doesn't stream long-running command output to the CLI; user sees a hung terminal.
**Mitigation:** In Task 1's `composer.rs`, run with piped stdout+stderr and stream lines back to the daemon's tracing log AND to the CLI via a new `DaemonResponse::Progress { line: String }` (extend the socket protocol to support a stream-then-final-result pattern, or use a temporary log file the CLI tails). MVP fallback: write to `~/Library/Application Support/hearth/log/add-<package>.log` and tell the user `"see log: <path>"` if it takes more than 30s. Decision: ship the log-file fallback for v0.3.0; streaming protocol is Phase 4.

### R3 — Laravel version detection is approximate
**Risk:** Pulse needs L10+, Reverb needs L11+. Parsing `composer.json` `require.laravel/framework` (e.g. `"^10.0"`) tells us the floor, not the installed version.
**Mitigation:** Prefer `vendor/composer/installed.json` (exact installed version) when present; fall back to constraint parse via the `semver` crate. If `vendor/` is missing, ask the user to run `composer install` first rather than guessing.

### R4 — Artisan needs the right PHP binary
**Risk:** If the user's global PHP is 8.4 but the Laravel app is on Valet-isolated 8.2, running `php artisan` finds the wrong PHP.
**Mitigation:** `SiteContext.php_binary` always uses the resolved version (Valet isolation → `config.default_php`). All `artisan` invocations use that absolute path, never bare `php`.

### R5 — `.env` edits clobber user formatting
**Risk:** Naive parse-then-rewrite loses comments, blank lines, key order.
**Mitigation:** Line-preserving editor in `env_file.rs` (see §5); test the round-trip explicitly. Always back up before write.

### R6 — Supervised Horizon/Reverb processes need the site dir + correct PHP — but daemon runs from `/`
**Risk:** Forgetting `cwd` makes Horizon error with "No application encoded in payload".
**Mitigation:** Mandatory `ManagedService::with_cwd` for added packages. Cover with a supervisor unit test that asserts `Command::current_dir` was called.

### R7 — Maintainer dogfood gap
**Risk:** Sprint ends without the maintainer actually using all 4 recipes against a real app.
**Mitigation:** §9 manual dogfooding checklist is a release blocker. Telescope + Horizon (the two the maintainer uses daily) must be greenlit by the maintainer's own use before tagging v0.3.0. Pulse + Reverb can ship marked "beta in CHANGELOG" if dogfood time is short.

### R8 — Lock-order violation when persisting added packages
**Risk:** Daemon's `Add` handler must `config → supervisor` per the project's lock-ordering rule, but the natural code flow ("save then register") matches that ordering only if we drop the config lock before acquiring supervisor.
**Mitigation:** Pattern is already established in `PhpSwitch` handler (`daemon/main.rs:413`). Mirror it: scope-block the config lock, drop, then acquire supervisor. Add a comment referencing the convention.

### R9 — `dialoguer` on a non-TTY (CI, scripted)
**Risk:** Piping into `hearth add` without `--yes` panics dialoguer.
**Mitigation:** CLI checks `atty::is(Stream::Stdin)` at startup of `Add`; if non-TTY and `--yes` not set, bail with `"refusing to prompt on non-TTY; pass --yes to accept defaults"`. (Add `is-terminal` crate, ~10 LOC dep.)

---

## Appendix A — Open architectural questions for orchestrator

1. **Progress streaming protocol** (R2). Acceptable to ship the log-file fallback for v0.3.0?
2. **Pulse worker** — ship without the `pulse:check` supervised worker? (We do, per §3.3.)
3. **`hearth remove <package>`** — confirmed out of scope for v0.3.0?
4. **Maintainer's daily 4** — confirmed all 4 packages must ship, or is Horizon + Telescope the minimum bar?
5. **Network-touching tests** — confirmed integration tests use a fake `composer` shim on PATH, not real network?
