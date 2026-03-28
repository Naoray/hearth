# Phase 3: Tauri GUI with System Tray

## Summary

Add a `hearth-gui` crate — a Tauri v2 desktop app that wraps the existing daemon via Unix socket, providing a system tray icon with status and a webview dashboard. The GUI is a thin client identical in architecture to the CLI: it sends `DaemonRequest` JSON over `~/.config/hearth/hearth.sock` and renders `DaemonResponse` in a web UI.

## Goals

1. System tray icon showing daemon health (green/yellow/red)
2. Tray menu with quick actions (start/stop, open sites, switch PHP)
3. Dashboard window with service status, site list, PHP management
4. Zero new state — daemon remains the single source of truth
5. Auto-start daemon if not running when GUI launches

## Non-Goals

- No electron. Tauri v2 only.
- No new daemon protocol — reuse existing `DaemonRequest`/`DaemonResponse`
- No database management (Phase 4)
- No deployment features (Phase 5)
- No custom window chrome — use native title bar

## Architecture

```
hearth-gui (Tauri v2 app)
  ├── src-tauri/          (Rust backend — Tauri commands wrapping socket client)
  │   ├── src/main.rs     (Tauri setup, tray, window management)
  │   ├── src/commands.rs  (Tauri #[command] handlers → socket calls)
  │   └── src/tray.rs     (System tray menu + status polling)
  └── src/                (Frontend — vanilla HTML/CSS/JS or lightweight framework)
      ├── index.html
      ├── dashboard.js    (Service status, site list, PHP panel)
      └── styles.css
```

### Communication Flow

```
Frontend (JS) → Tauri invoke("get_status") → commands.rs → Unix socket → Daemon
                                              ← DaemonResponse ←
```

### Tauri Commands (mirror CLI operations)

Each Tauri command maps 1:1 to a `DaemonRequest`:

| Tauri Command | DaemonRequest | UI Location |
|---------------|---------------|-------------|
| `get_status` | `Status` | Dashboard header, tray icon |
| `start_services` | `Start` | Dashboard button, tray menu |
| `stop_services` | `Stop` | Dashboard button, tray menu |
| `restart_service` | `Restart { service }` | Per-service action |
| `get_sites` | `Sites` | Sites tab |
| `link_site` | `Link { path, name }` | Sites tab + drag-drop |
| `unlink_site` | `Unlink { name }` | Per-site action |
| `secure_site` | `Secure { name }` | Per-site toggle |
| `unsecure_site` | `Unsecure { name }` | Per-site toggle |
| `get_php_versions` | `PhpList` | PHP tab |
| `switch_php` | `PhpSwitch { version }` | PHP tab selector |
| `set_php_config` | `PhpConfig { version, key, value }` | PHP tab form |
| `ensure_daemon` | (spawn hearth-daemon if not running) | Startup |

### Socket Client

Extract the CLI's socket communication into `hearth-lib` as a reusable `DaemonClient`:

```rust
// hearth-lib/src/client.rs (new)
pub struct DaemonClient {
    socket_path: PathBuf,
}

impl DaemonClient {
    pub async fn connect() -> Result<Self>;
    pub async fn send(&self, request: DaemonRequest) -> Result<DaemonResponse>;
    pub async fn is_daemon_running(&self) -> bool;
}
```

This eliminates duplication — both CLI and GUI use `DaemonClient`. The CLI currently has inline socket code that should move here.

## System Tray

### Icon States
- **Green dot** — All services running
- **Yellow dot** — Some services stopped or starting
- **Red dot** — Daemon unreachable or services failed
- **Grey dot** — Daemon not running

### Tray Menu
```
Hearth v0.2.3
──────────────
● nginx         Running
● php-fpm 8.4   Running
● dnsmasq       Running
● mailpit       Running
──────────────
Start All
Stop All
──────────────
PHP: 8.4  ▸  [8.1, 8.2, 8.3, 8.4]
──────────────
Open Dashboard
Open Mailpit
──────────────
Quit Hearth
```

### Status Polling
Poll daemon every 5 seconds (matches daemon's own health check interval). Update tray icon color and menu items. If daemon becomes unreachable, switch to grey icon and show "Start Daemon" option.

## Dashboard Window

Single window with three panels, switchable via sidebar or tabs:

### Services Panel (default view)
- Service cards showing name, state (Running/Stopped/Failed), PID, uptime
- Start/Stop/Restart buttons per service
- Global Start All / Stop All
- Circuit breaker indicator (shows if a service hit the failure limit)

### Sites Panel
- Table: name, path, TLD, SSL status, PHP version
- Link new site (folder picker or drag-drop)
- Unlink, Secure/Unsecure per site
- Click site name → open in browser

### PHP Panel
- Currently active version (prominent)
- List of installed versions with "Switch" buttons
- php.ini quick-edit form (key/value pairs like memory_limit, upload_max_filesize)

## Frontend Stack

**Recommended: Vanilla HTML/CSS/JS** with Tauri's invoke API.

Rationale:
- The dashboard has ~3 views with simple data tables and buttons
- No complex state management needed (daemon is the state)
- Keeps the binary small (no bundled React/Vue runtime)
- Matches the project's philosophy of minimal dependencies
- Tauri's `invoke()` + `listen()` APIs are sufficient

If interactivity demands grow in Phase 4/5, upgrade to a lightweight framework (Preact or Solid) later.

## Daemon Lifecycle

The GUI needs to handle daemon lifecycle since users may launch the GUI without a running daemon:

1. **On launch**: Check if daemon is running (`DaemonClient::is_daemon_running()`)
2. **If not running**: Spawn `hearth-daemon` as a background process
3. **On quit**: Ask user preference — "Stop daemon?" or "Keep running in background"
4. **Connection lost**: Show reconnection banner, retry every 2 seconds

## Build & Distribution

### Crate Structure
```toml
# crates/hearth-gui/Cargo.toml
[package]
name = "hearth-gui"
version = "0.2.4"

[dependencies]
hearth-lib = { path = "../hearth-lib" }
tauri = { version = "2", features = ["tray-icon"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["full"] }
```

### Homebrew
The GUI ships as a separate cask (not the CLI formula):
```ruby
cask "hearth" do
  version "0.2.4"
  url "https://github.com/Naoray/hearth/releases/download/v#{version}/Hearth.dmg"
  name "Hearth"
  homepage "https://github.com/Naoray/hearth"
  app "Hearth.app"
end
```

### CI Addition
Add a `build-gui` job to the release workflow that:
1. Builds the Tauri app (`cargo tauri build`)
2. Signs and notarizes for macOS
3. Uploads `.dmg` to the GitHub Release

## Migration Path

1. Extract `DaemonClient` into `hearth-lib` (refactor CLI to use it)
2. Scaffold `hearth-gui` Tauri crate
3. Implement Tauri commands wrapping `DaemonClient`
4. Build system tray with status polling
5. Build dashboard panels (Services → Sites → PHP)
6. Add daemon auto-start logic
7. CI: add Tauri build + DMG packaging
8. Homebrew: add cask formula

## Open Questions

1. **Login item**: Should the GUI register as a macOS login item (auto-start on boot)?
2. **Dock icon**: Show in Dock when dashboard is open, or always hide (tray-only)?
3. **Dump streaming**: Should the dashboard show live dump output, or keep that CLI-only?
4. **Notifications**: macOS notifications for service crashes / circuit breaker trips?
