# Hearth 🔥

**Your complete Laravel development environment — one Rust daemon, zero friction.**

Services, sites, SSL, PHP version switching, mail catching, dump server, and AI tool integration. Install once, works immediately.

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
hearth mcp                                — MCP server for IDE integration
hearth laravel new myapp                  — scaffold a new Laravel project
```

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

## MCP integration

The daemon runs an in-process MCP server on `http://127.0.0.1:9900/mcp`. Connect your IDE directly — no spawned processes, no cleanup required.

For IDEs that only support stdio, `hearth mcp` bridges stdin/stdout to the HTTP endpoint.

**Available tools:** `hearth_status`, `hearth_sites`, `hearth_php_list`, `hearth_php_switch`, `hearth_site_link`, `hearth_site_unlink`, `hearth_service_restart`, `hearth_php_config`

## Architecture

Hearth is a thin CLI talking to an always-on daemon over a Unix socket. The daemon owns all child processes via process groups — when it exits, everything exits. No orphaned processes.

```
hearth-daemon
  ├── nginx          (process group)
  ├── php-fpm        (process group)
  ├── dnsmasq        (process group)
  ├── mailpit        (process group, if installed)
  ├── dump server    (tokio task)
  └── MCP server     (tokio task, port 9900)
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
```

## PHP resolution

Hearth finds PHP binaries in this order:

1. `~/.config/hearth/php/{version}/php` — own cached binaries
2. `~/Library/Application Support/Herd/bin/php{version}` — reuse existing binaries
3. `/opt/homebrew/opt/php@{version}/bin/php` — Homebrew fallback

## Roadmap

- [x] Phase 1: Core CLI + daemon, process supervision, site/PHP management
- [x] Phase 2: MCP server, Mailpit, dump server, Homebrew distribution
- [ ] Phase 3: Tauri GUI with system tray
- [ ] Phase 4: Database management (MySQL, PostgreSQL, Redis)
- [ ] Phase 5: Anvil worktree integration, Ploi deployment

## License

MIT — free forever, including databases (Phase 4).
