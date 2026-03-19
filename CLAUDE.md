# Hearth — Unified Laravel Development Command Center

## Architecture

CLI-first Rust application with Tauri GUI wrapper (Phase 3).

```
hearth-daemon (always-on, owns all processes via process groups)
  ├── hearth-cli (thin client, talks to daemon via Unix socket)
  └── hearth-gui (Tauri, Phase 3, also thin client)
```

- **hearth-lib**: Core library shared by daemon, CLI, and GUI
- **hearth-daemon**: Process supervisor + Unix socket API server
- **hearth-cli**: `hearth` binary, thin client using clap

## Key Design Decisions

1. Valet is a managed dependency (Composer global). Hearth wraps Valet CLI for site management.
2. Process groups via `command-group` crate — guarantees no orphan processes.
3. Circuit breaker: 3 failures in 60s = service stopped, manual restart required.
4. Health poll every 5 seconds.
5. PHP resolution chain: Hearth cache → Herd binaries → Homebrew → download.
6. `sudo` only during `hearth install` (DNS resolver + CA trust). No persistent privileged helper.
7. dnsmasq on unprivileged port 5354, resolver file specifies custom port.

## Build & Run

```bash
cargo build                    # Build all crates
cargo run -p hearth-daemon     # Start the daemon
cargo run -p hearth-cli -- status  # CLI commands
```

## Testing

```bash
cargo test                     # Unit tests (all crates)
cargo test -p hearth-lib       # Library tests only
```

## Design Doc

Full design document with phased implementation plan:
`~/.gstack/projects/home/krishankonig-unknown-design-20260319-130500.md`
