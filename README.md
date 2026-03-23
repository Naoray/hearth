# Hearth

A single CLI that IS your Laravel development environment. Services, sites, SSL, PHP versions, mail catching, dump server, and AI tool integration — all supervised by one Rust daemon that guarantees no orphan processes.

Built because Herd leaks 1,000+ MCP processes, freezes with many sites, and gates databases behind a paywall.

## Install

```bash
brew tap naoray/tap
brew install hearth
```

Or build from source:

```bash
cargo install --path crates/hearth-cli
cargo install --path crates/hearth-daemon
```

## Quick start

```bash
hearth install          # one-time setup (DNS resolver, CA trust, Valet)
hearth-daemon &         # start the supervisor daemon
hearth start            # bring up nginx, php-fpm, dnsmasq

cd ~/Code/my-app
hearth link             # myapp.test is live
hearth secure my-app    # now with SSL
hearth php use 8.3      # switch PHP, FPM restarts automatically
```

## What it does

```
hearth start / stop / restart / status    — manage all services
hearth link / unlink / park / sites       — site management (via Valet)
hearth secure / unsecure                  — SSL certificates
hearth php use 8.3                        — switch PHP version
hearth php list                           — show installed versions
hearth php config memory_limit 512M       — edit php.ini + restart FPM
hearth dump                               — stream VarDumper output with timestamps
hearth mail                               — open Mailpit UI in browser
hearth mcp                                — MCP stdio bridge for IDE integration
hearth laravel new myapp                  — create a new Laravel project
hearth install                            — first-time setup
```

## Architecture

Hearth is a thin CLI talking to an always-on daemon over a Unix socket. The daemon owns all child processes via process groups — when it dies, everything dies with it. No orphans.

```
hearth-daemon
  ├── nginx          (process group)
  ├── php-fpm        (process group)
  ├── dnsmasq        (process group, port 5354)
  ├── mailpit        (process group, if installed)
  ├── dump server    (tokio task)
  └── MCP server     (tokio task, port 9900)
```

Circuit breaker stops restart loops: 3 crashes in 60 seconds = service marked failed, manual `hearth restart <service>` to retry.

## MCP server

The daemon runs an in-process MCP server on `http://127.0.0.1:9900/mcp` — IDEs connect directly, no spawned processes. This is the whole point: Herd's PHP-based MCP server leaks processes because stdio transport has no process group containment. Hearth's MCP server is a Tokio task inside the daemon. Nothing to orphan.

For IDEs that only support stdio, `hearth mcp` bridges stdin/stdout to the HTTP endpoint.

**Tools:** `hearth_status`, `hearth_sites`, `hearth_php_list`, `hearth_php_switch`, `hearth_site_link`, `hearth_site_unlink`, `hearth_service_restart`, `hearth_php_config`

## PHP resolution

Hearth finds PHP binaries in this order:

1. `~/.config/hearth/php/{version}/php` — own cached binaries
2. `~/Library/Application Support/Herd/bin/php{version}` — reuse Herd's binaries during migration
3. `/opt/homebrew/opt/php@{version}/bin/php` — Homebrew fallback

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

## What's free that Herd charges for

- Databases (MySQL, PostgreSQL, Redis) — Phase 4
- Mail catching (Mailpit)
- Dump server
- MCP without orphan processes
- Everything, really. MIT licensed.

## Roadmap

- [x] Phase 1: Core CLI + daemon, process supervision, site/PHP management
- [x] Phase 2: MCP server, Mailpit, dump polish, Homebrew distribution
- [ ] Phase 3: Tauri GUI with system tray
- [ ] Phase 4: Database management (MySQL, PostgreSQL, Redis)
- [ ] Phase 5: Anvil worktree integration, Ploi deployment

## License

MIT
