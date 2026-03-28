# Phase 3: Tauri GUI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a Tauri v2 desktop app (`hearth-gui`) with system tray status, dashboard panels (services/sites/PHP/dump), macOS notifications, and login item registration.

**Architecture:** The GUI is a thin client — identical to the CLI — that talks to the daemon via Unix socket using the existing `DaemonRequest`/`DaemonResponse` protocol. A new `DaemonClient` in `hearth-lib` is shared by both CLI and GUI. Frontend is vanilla HTML/CSS/JS. The Tauri backend polls the daemon for status, forwards events to the webview, and connects to the dump relay for live streaming.

**Tech Stack:** Tauri v2, Rust (tokio), vanilla HTML/CSS/JS, macOS notifications via `tauri-plugin-notification`, `tauri-plugin-autostart` for login item.

---

## File Structure

### New files

| File | Responsibility |
|------|---------------|
| `crates/hearth-lib/src/client.rs` | Reusable async `DaemonClient` (connect, send, is_running) |
| `crates/hearth-gui/Cargo.toml` | Tauri crate manifest |
| `crates/hearth-gui/tauri.conf.json` | Tauri config (window, tray, permissions, bundle) |
| `crates/hearth-gui/capabilities/default.json` | Tauri v2 capability permissions |
| `crates/hearth-gui/src/main.rs` | Tauri app setup, tray creation, window management |
| `crates/hearth-gui/src/commands.rs` | `#[tauri::command]` handlers wrapping `DaemonClient` |
| `crates/hearth-gui/src/tray.rs` | System tray icon + menu + polling loop |
| `crates/hearth-gui/src/notifications.rs` | macOS notification sending with debounce |
| `crates/hearth-gui/src/dump.rs` | TCP subscriber to dump relay, Tauri event emitter |
| `crates/hearth-gui/frontend/index.html` | Dashboard HTML (sidebar + panels) |
| `crates/hearth-gui/frontend/app.js` | Dashboard JS (Tauri invoke, event listeners, rendering) |
| `crates/hearth-gui/frontend/styles.css` | Dashboard CSS |
| `crates/hearth-gui/icons/tray-green.png` | Tray icon: all services running |
| `crates/hearth-gui/icons/tray-yellow.png` | Tray icon: partial/starting |
| `crates/hearth-gui/icons/tray-red.png` | Tray icon: failed or unreachable |
| `crates/hearth-gui/icons/tray-grey.png` | Tray icon: daemon not running |

### Modified files

| File | Change |
|------|--------|
| `Cargo.toml` (root) | Add `crates/hearth-gui` to workspace members, add tauri deps |
| `crates/hearth-lib/src/lib.rs` | Add `pub mod client;` |
| `crates/hearth-cli/src/main.rs` | Replace inline socket code with `DaemonClient` |
| `crates/hearth-cli/Cargo.toml` | (no change needed — already depends on hearth-lib) |
| `.github/workflows/release.yml` | Add `build-gui` job for Tauri DMG |

---

## Task 1: Extract `DaemonClient` into `hearth-lib`

**Files:**
- Create: `crates/hearth-lib/src/client.rs`
- Modify: `crates/hearth-lib/src/lib.rs`
- Test: `crates/hearth-lib/src/client.rs` (inline `#[cfg(test)]`)

- [ ] **Step 1: Write the failing test**

Add to `crates/hearth-lib/src/client.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::socket::{DaemonRequest, DaemonResponse};

    #[test]
    fn client_uses_default_socket_path() {
        let client = DaemonClient::new();
        assert_eq!(client.socket_path(), crate::socket::socket_path());
    }

    #[test]
    fn client_custom_socket_path() {
        let path = std::path::PathBuf::from("/tmp/test.sock");
        let client = DaemonClient::with_socket_path(path.clone());
        assert_eq!(client.socket_path(), &path);
    }
}
```

- [ ] **Step 2: Register the module**

In `crates/hearth-lib/src/lib.rs`, add after line 1:

```rust
pub mod client;
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p hearth-lib client::tests -- --nocapture`
Expected: FAIL — `DaemonClient` not found

- [ ] **Step 4: Implement `DaemonClient`**

Write `crates/hearth-lib/src/client.rs`:

```rust
use std::path::{Path, PathBuf};

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::socket::{DaemonRequest, DaemonResponse};

/// Async client for communicating with the hearth daemon over Unix socket.
///
/// Used by both CLI and GUI to send requests to the daemon.
pub struct DaemonClient {
    socket_path: PathBuf,
}

impl DaemonClient {
    /// Create a client using the default socket path.
    pub fn new() -> Self {
        Self {
            socket_path: crate::socket::socket_path(),
        }
    }

    /// Create a client with a custom socket path.
    pub fn with_socket_path(path: PathBuf) -> Self {
        Self { socket_path: path }
    }

    /// Get the socket path this client connects to.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Send a request to the daemon and return the response.
    pub async fn send(&self, request: DaemonRequest) -> anyhow::Result<DaemonResponse> {
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .context("daemon not running — start with: hearth daemon start")?;

        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        let request_json = serde_json::to_string(&request)?;
        writer.write_all(request_json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;

        let mut line = String::new();
        reader.read_line(&mut line).await?;

        let response: DaemonResponse = serde_json::from_str(line.trim())?;
        Ok(response)
    }

    /// Check if the daemon is reachable by sending a Ping.
    pub async fn is_daemon_running(&self) -> bool {
        matches!(
            self.send(DaemonRequest::Ping).await,
            Ok(DaemonResponse::Pong)
        )
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p hearth-lib client::tests`
Expected: 2 tests PASS

- [ ] **Step 6: Commit**

```bash
git add crates/hearth-lib/src/client.rs crates/hearth-lib/src/lib.rs
git commit -m "feat(lib): extract DaemonClient for reusable socket communication"
```

---

## Task 2: Refactor CLI to use `DaemonClient`

**Files:**
- Modify: `crates/hearth-cli/src/main.rs:200-222`

- [ ] **Step 1: Replace `send_to_daemon` with `DaemonClient`**

In `crates/hearth-cli/src/main.rs`, remove the `send_to_daemon` function (lines 200-222) and the unused imports. Replace the function body with:

```rust
async fn send_to_daemon(request: DaemonRequest) -> anyhow::Result<DaemonResponse> {
    let client = hearth_lib::client::DaemonClient::new();
    client.send(request).await
}
```

- [ ] **Step 2: Remove unused imports**

In `crates/hearth-cli/src/main.rs`, remove these now-unused imports from the top:

```rust
// Remove these lines:
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
```

Keep `use tokio::io::{AsyncBufReadExt, BufReader};` because `run_mcp_bridge` still uses `BufReader` and `AsyncBufReadExt`.

- [ ] **Step 3: Run existing tests + verify CLI builds**

Run: `cargo build -p hearth-cli && cargo test`
Expected: Build succeeds, all 55 tests pass

- [ ] **Step 4: Commit**

```bash
git add crates/hearth-cli/src/main.rs
git commit -m "refactor(cli): use DaemonClient instead of inline socket code"
```

---

## Task 3: Scaffold Tauri crate

**Files:**
- Create: `crates/hearth-gui/Cargo.toml`
- Create: `crates/hearth-gui/tauri.conf.json`
- Create: `crates/hearth-gui/capabilities/default.json`
- Create: `crates/hearth-gui/src/main.rs` (minimal Tauri app)
- Create: `crates/hearth-gui/frontend/index.html`
- Modify: `Cargo.toml` (root workspace)

- [ ] **Step 1: Add crate to workspace**

In root `Cargo.toml`, add to the `members` array:

```toml
[workspace]
resolver = "2"
members = [
    "crates/hearth-lib",
    "crates/hearth-cli",
    "crates/hearth-daemon",
    "crates/hearth-gui",
]
```

Add Tauri dependencies to `[workspace.dependencies]`:

```toml
# GUI (Phase 3)
tauri = { version = "2", features = ["tray-icon"] }
tauri-plugin-notification = "2"
tauri-plugin-autostart = "2"
tauri-plugin-dialog = "2"
tauri-plugin-shell = "2"
opener = "0.7"
```

- [ ] **Step 2: Create `crates/hearth-gui/Cargo.toml`**

```toml
[package]
name = "hearth-gui"
version.workspace = true
edition.workspace = true
license.workspace = true
description = "Tauri GUI for Hearth — system tray + dashboard"

[dependencies]
hearth-lib = { workspace = true }
tauri = { workspace = true }
tauri-plugin-notification = { workspace = true }
tauri-plugin-autostart = { workspace = true }
tauri-plugin-dialog = { workspace = true }
tauri-plugin-shell = { workspace = true }
tokio = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }

[build-dependencies]
tauri-build = { version = "2", features = [] }
```

- [ ] **Step 3: Create `crates/hearth-gui/build.rs`**

```rust
fn main() {
    tauri_build::build();
}
```

- [ ] **Step 4: Create `crates/hearth-gui/tauri.conf.json`**

```json
{
  "$schema": "https://raw.githubusercontent.com/nicedoc/tauri/next/crates/tauri-utils/schema.json",
  "productName": "Hearth",
  "version": "0.2.3",
  "identifier": "com.hearth.dev",
  "build": {
    "frontendDist": "frontend"
  },
  "app": {
    "withGlobalTauri": true,
    "windows": [],
    "trayIcon": {
      "iconPath": "icons/tray-grey.png",
      "iconAsTemplate": true
    }
  },
  "bundle": {
    "active": true,
    "targets": "dmg",
    "icon": [
      "icons/icon.icns"
    ],
    "macOS": {
      "minimumSystemVersion": "13.0"
    }
  }
}
```

- [ ] **Step 5: Create capabilities file**

Create `crates/hearth-gui/capabilities/default.json`:

```json
{
  "$schema": "https://raw.githubusercontent.com/nicedoc/tauri/next/crates/tauri-utils/schema.json",
  "identifier": "default",
  "description": "Default permissions for Hearth GUI",
  "windows": ["main"],
  "permissions": [
    "core:default",
    "notification:default",
    "notification:allow-notify",
    "notification:allow-request-permission",
    "autostart:default",
    "autostart:allow-enable",
    "autostart:allow-disable",
    "autostart:allow-is-enabled",
    "dialog:default",
    "dialog:allow-open",
    "shell:default",
    "shell:allow-open"
  ]
}
```

- [ ] **Step 6: Create minimal frontend**

Create `crates/hearth-gui/frontend/index.html`:

```html
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>Hearth</title>
  <link rel="stylesheet" href="styles.css">
</head>
<body>
  <div id="app">
    <nav id="sidebar">
      <div class="nav-header">Hearth</div>
      <button class="nav-item active" data-panel="services">Services</button>
      <button class="nav-item" data-panel="sites">Sites</button>
      <button class="nav-item" data-panel="php">PHP</button>
      <button class="nav-item" data-panel="dump">Dump</button>
    </nav>
    <main id="content">
      <div id="connection-banner" class="banner hidden">
        Daemon not connected. <button id="start-daemon-btn">Start Daemon</button>
      </div>
      <section id="panel-services" class="panel active">
        <h2>Services</h2>
        <div id="services-list"></div>
        <div class="actions">
          <button id="start-all-btn">Start All</button>
          <button id="stop-all-btn">Stop All</button>
        </div>
      </section>
      <section id="panel-sites" class="panel">
        <h2>Sites</h2>
        <div id="sites-list"></div>
        <div class="actions">
          <button id="link-site-btn">Link Site...</button>
        </div>
      </section>
      <section id="panel-php" class="panel">
        <h2>PHP Versions</h2>
        <div id="php-list"></div>
        <div id="php-config-form"></div>
      </section>
      <section id="panel-dump" class="panel">
        <h2>Dump Server</h2>
        <div class="dump-controls">
          <span id="dump-status" class="status-dot grey"></span>
          <button id="dump-pause-btn">Pause</button>
          <button id="dump-clear-btn">Clear</button>
        </div>
        <pre id="dump-output"></pre>
      </section>
    </main>
  </div>
  <script src="app.js"></script>
</body>
</html>
```

- [ ] **Step 7: Create minimal CSS**

Create `crates/hearth-gui/frontend/styles.css`:

```css
* { margin: 0; padding: 0; box-sizing: border-box; }

:root {
  --bg: #1a1a2e;
  --bg-surface: #16213e;
  --bg-card: #0f3460;
  --text: #e0e0e0;
  --text-muted: #8892b0;
  --accent: #4ade80;
  --warning: #fbbf24;
  --danger: #f87171;
  --border: #233554;
}

body {
  font-family: -apple-system, BlinkMacSystemFont, "SF Pro Text", system-ui, sans-serif;
  background: var(--bg);
  color: var(--text);
  height: 100vh;
  overflow: hidden;
}

#app {
  display: flex;
  height: 100%;
}

#sidebar {
  width: 180px;
  background: var(--bg-surface);
  border-right: 1px solid var(--border);
  display: flex;
  flex-direction: column;
  padding: 16px 0;
}

.nav-header {
  padding: 0 16px 16px;
  font-weight: 600;
  font-size: 18px;
  color: var(--accent);
}

.nav-item {
  background: none;
  border: none;
  color: var(--text-muted);
  padding: 10px 16px;
  text-align: left;
  cursor: pointer;
  font-size: 14px;
  transition: all 0.15s;
}

.nav-item:hover { color: var(--text); background: var(--bg-card); }
.nav-item.active { color: var(--accent); background: var(--bg-card); border-left: 3px solid var(--accent); }

#content {
  flex: 1;
  padding: 24px;
  overflow-y: auto;
}

.panel { display: none; }
.panel.active { display: block; }

.banner {
  background: var(--danger);
  color: white;
  padding: 10px 16px;
  border-radius: 6px;
  margin-bottom: 16px;
  display: flex;
  align-items: center;
  gap: 12px;
}
.banner.hidden { display: none; }
.banner button {
  background: white;
  color: var(--danger);
  border: none;
  padding: 4px 12px;
  border-radius: 4px;
  cursor: pointer;
  font-weight: 500;
}

h2 { margin-bottom: 16px; font-size: 20px; }

.service-card {
  display: flex;
  align-items: center;
  justify-content: space-between;
  background: var(--bg-card);
  padding: 12px 16px;
  border-radius: 8px;
  margin-bottom: 8px;
}

.service-name { font-weight: 500; }
.service-state { font-size: 13px; }

.status-dot {
  display: inline-block;
  width: 8px;
  height: 8px;
  border-radius: 50%;
  margin-right: 8px;
}
.status-dot.green { background: var(--accent); }
.status-dot.yellow { background: var(--warning); }
.status-dot.red { background: var(--danger); }
.status-dot.grey { background: var(--text-muted); }

.actions { margin-top: 16px; display: flex; gap: 8px; }

button {
  background: var(--bg-card);
  color: var(--text);
  border: 1px solid var(--border);
  padding: 8px 16px;
  border-radius: 6px;
  cursor: pointer;
  font-size: 13px;
  transition: all 0.15s;
}
button:hover { background: var(--border); }

.site-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 10px 16px;
  background: var(--bg-card);
  border-radius: 8px;
  margin-bottom: 6px;
}

.site-name { font-weight: 500; cursor: pointer; }
.site-name:hover { color: var(--accent); text-decoration: underline; }
.site-path { font-size: 12px; color: var(--text-muted); }
.site-badge {
  font-size: 11px;
  padding: 2px 8px;
  border-radius: 4px;
  background: var(--border);
}
.site-badge.ssl { background: var(--accent); color: var(--bg); }

.php-version-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 10px 16px;
  background: var(--bg-card);
  border-radius: 8px;
  margin-bottom: 6px;
}
.php-active { color: var(--accent); font-weight: 600; }

#dump-output {
  background: var(--bg-surface);
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 12px;
  font-size: 12px;
  font-family: "SF Mono", "Fira Code", monospace;
  max-height: calc(100vh - 200px);
  overflow-y: auto;
  white-space: pre-wrap;
  word-break: break-all;
  margin-top: 12px;
}

.dump-controls {
  display: flex;
  align-items: center;
  gap: 12px;
}
```

- [ ] **Step 8: Create minimal JS**

Create `crates/hearth-gui/frontend/app.js`:

```javascript
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

// Panel navigation
document.querySelectorAll('.nav-item').forEach(btn => {
  btn.addEventListener('click', () => {
    document.querySelectorAll('.nav-item').forEach(b => b.classList.remove('active'));
    document.querySelectorAll('.panel').forEach(p => p.classList.remove('active'));
    btn.classList.add('active');
    document.getElementById(`panel-${btn.dataset.panel}`).classList.add('active');
  });
});

// Services
async function refreshServices() {
  try {
    const services = await invoke('get_status');
    const list = document.getElementById('services-list');
    list.innerHTML = services.map(s => {
      const color = s.state === 'Running' ? 'green'
        : s.state === 'Stopped' ? 'grey'
        : s.state === 'Starting' ? 'yellow' : 'red';
      const pid = s.pid ? ` (PID ${s.pid})` : '';
      return `<div class="service-card">
        <div>
          <span class="status-dot ${color}"></span>
          <span class="service-name">${s.name}</span>
        </div>
        <div>
          <span class="service-state">${s.state}${pid}</span>
          <button onclick="restartService('${s.name}')">Restart</button>
        </div>
      </div>`;
    }).join('');
    document.getElementById('connection-banner').classList.add('hidden');
  } catch (e) {
    document.getElementById('connection-banner').classList.remove('hidden');
  }
}

async function restartService(name) {
  await invoke('restart_service', { service: name });
  await refreshServices();
}

document.getElementById('start-all-btn').addEventListener('click', async () => {
  await invoke('start_services');
  await refreshServices();
});

document.getElementById('stop-all-btn').addEventListener('click', async () => {
  await invoke('stop_services');
  await refreshServices();
});

document.getElementById('start-daemon-btn').addEventListener('click', async () => {
  await invoke('ensure_daemon');
  setTimeout(refreshServices, 1500);
});

// Sites
async function refreshSites() {
  try {
    const sites = await invoke('get_sites');
    const list = document.getElementById('sites-list');
    list.innerHTML = sites.map(s => {
      const sslBadge = s.secured ? '<span class="site-badge ssl">SSL</span>' : '';
      const phpBadge = s.php_version ? `<span class="site-badge">PHP ${s.php_version}</span>` : '';
      return `<div class="site-row">
        <div>
          <span class="site-name" onclick="window.__TAURI__.shell.open('https://${s.name}.test')">${s.name}</span>
          <div class="site-path">${s.path}</div>
        </div>
        <div style="display:flex;gap:6px;align-items:center">
          ${sslBadge}${phpBadge}
          <button onclick="toggleSsl('${s.name}', ${s.secured})">${s.secured ? 'Unsecure' : 'Secure'}</button>
          <button onclick="unlinkSite('${s.name}')">Unlink</button>
        </div>
      </div>`;
    }).join('');
  } catch (e) { console.error('Failed to load sites:', e); }
}

async function toggleSsl(name, isSecured) {
  await invoke(isSecured ? 'unsecure_site' : 'secure_site', { name });
  await refreshSites();
}

async function unlinkSite(name) {
  await invoke('unlink_site', { name });
  await refreshSites();
}

document.getElementById('link-site-btn').addEventListener('click', async () => {
  const { open } = window.__TAURI__.dialog;
  const selected = await open({ directory: true, title: 'Select site directory' });
  if (selected) {
    await invoke('link_site', { path: selected, name: null });
    await refreshSites();
  }
});

// PHP
async function refreshPhp() {
  try {
    const versions = await invoke('get_php_versions');
    const list = document.getElementById('php-list');
    list.innerHTML = versions.map(v => {
      const cls = v.active ? 'php-active' : '';
      return `<div class="php-version-row">
        <span class="${cls}">PHP ${v.version}${v.active ? ' (active)' : ''}</span>
        <div>
          <span class="site-path">${v.path}</span>
          ${v.active ? '' : `<button onclick="switchPhp('${v.version}')">Switch</button>`}
        </div>
      </div>`;
    }).join('');
  } catch (e) { console.error('Failed to load PHP versions:', e); }
}

async function switchPhp(version) {
  await invoke('switch_php', { version });
  await refreshPhp();
  await refreshServices();
}

// Dump streaming
let dumpPaused = false;
let dumpBuffer = [];

listen('dump-line', (event) => {
  if (dumpPaused) {
    dumpBuffer.push(event.payload);
    return;
  }
  appendDumpLine(event.payload);
});

listen('dump-connected', () => {
  document.getElementById('dump-status').className = 'status-dot green';
});

listen('dump-disconnected', () => {
  document.getElementById('dump-status').className = 'status-dot grey';
});

function appendDumpLine(line) {
  const output = document.getElementById('dump-output');
  output.textContent += line + '\n';
  output.scrollTop = output.scrollHeight;
}

document.getElementById('dump-pause-btn').addEventListener('click', () => {
  dumpPaused = !dumpPaused;
  document.getElementById('dump-pause-btn').textContent = dumpPaused ? 'Resume' : 'Pause';
  if (!dumpPaused) {
    dumpBuffer.forEach(appendDumpLine);
    dumpBuffer = [];
  }
});

document.getElementById('dump-clear-btn').addEventListener('click', () => {
  document.getElementById('dump-output').textContent = '';
});

// Status polling — triggers on panel switch
const panelRefreshMap = {
  services: refreshServices,
  sites: refreshSites,
  php: refreshPhp,
};

document.querySelectorAll('.nav-item').forEach(btn => {
  btn.addEventListener('click', () => {
    const fn = panelRefreshMap[btn.dataset.panel];
    if (fn) fn();
  });
});

// Initial load
refreshServices();
```

- [ ] **Step 9: Create minimal Tauri main.rs**

Create `crates/hearth-gui/src/main.rs`:

```rust
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
```

- [ ] **Step 10: Verify the crate compiles**

Run: `cargo check -p hearth-gui`
Expected: Compiles (may warn about unused — that's fine for scaffold)

- [ ] **Step 11: Commit**

```bash
git add crates/hearth-gui/ Cargo.toml
git commit -m "feat(gui): scaffold Tauri v2 crate with frontend"
```

---

## Task 4: Implement Tauri commands

**Files:**
- Create: `crates/hearth-gui/src/commands.rs`
- Modify: `crates/hearth-gui/src/main.rs`

- [ ] **Step 1: Create `crates/hearth-gui/src/commands.rs`**

```rust
use hearth_lib::client::DaemonClient;
use hearth_lib::socket::{
    DaemonRequest, DaemonResponse, PhpVersionInfo, ServiceStatus, SiteInfo,
};
use serde::Serialize;

fn client() -> DaemonClient {
    DaemonClient::new()
}

#[tauri::command]
pub async fn get_status() -> Result<Vec<ServiceStatus>, String> {
    match client().send(DaemonRequest::Status).await {
        Ok(DaemonResponse::Status { services }) => Ok(services),
        Ok(DaemonResponse::Error { message }) => Err(message),
        Ok(_) => Err("unexpected response".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub async fn start_services() -> Result<String, String> {
    match client().send(DaemonRequest::Start).await {
        Ok(DaemonResponse::Ok { message }) => Ok(message.unwrap_or_default()),
        Ok(DaemonResponse::Error { message }) => Err(message),
        Ok(_) => Err("unexpected response".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub async fn stop_services() -> Result<String, String> {
    match client().send(DaemonRequest::Stop).await {
        Ok(DaemonResponse::Ok { message }) => Ok(message.unwrap_or_default()),
        Ok(DaemonResponse::Error { message }) => Err(message),
        Ok(_) => Err("unexpected response".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub async fn restart_service(service: String) -> Result<String, String> {
    match client().send(DaemonRequest::Restart { service: Some(service) }).await {
        Ok(DaemonResponse::Ok { message }) => Ok(message.unwrap_or_default()),
        Ok(DaemonResponse::Error { message }) => Err(message),
        Ok(_) => Err("unexpected response".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub async fn get_sites() -> Result<Vec<SiteInfo>, String> {
    match client().send(DaemonRequest::Sites).await {
        Ok(DaemonResponse::Sites { sites }) => Ok(sites),
        Ok(DaemonResponse::Error { message }) => Err(message),
        Ok(_) => Err("unexpected response".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub async fn link_site(path: String, name: Option<String>) -> Result<String, String> {
    match client().send(DaemonRequest::Link { path, name }).await {
        Ok(DaemonResponse::Ok { message }) => Ok(message.unwrap_or_default()),
        Ok(DaemonResponse::Error { message }) => Err(message),
        Ok(_) => Err("unexpected response".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub async fn unlink_site(name: String) -> Result<String, String> {
    match client().send(DaemonRequest::Unlink { name }).await {
        Ok(DaemonResponse::Ok { message }) => Ok(message.unwrap_or_default()),
        Ok(DaemonResponse::Error { message }) => Err(message),
        Ok(_) => Err("unexpected response".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub async fn secure_site(name: String) -> Result<String, String> {
    match client().send(DaemonRequest::Secure { name }).await {
        Ok(DaemonResponse::Ok { message }) => Ok(message.unwrap_or_default()),
        Ok(DaemonResponse::Error { message }) => Err(message),
        Ok(_) => Err("unexpected response".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub async fn unsecure_site(name: String) -> Result<String, String> {
    match client().send(DaemonRequest::Unsecure { name }).await {
        Ok(DaemonResponse::Ok { message }) => Ok(message.unwrap_or_default()),
        Ok(DaemonResponse::Error { message }) => Err(message),
        Ok(_) => Err("unexpected response".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub async fn get_php_versions() -> Result<Vec<PhpVersionInfo>, String> {
    match client().send(DaemonRequest::PhpList).await {
        Ok(DaemonResponse::PhpVersions { versions }) => Ok(versions),
        Ok(DaemonResponse::Error { message }) => Err(message),
        Ok(_) => Err("unexpected response".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub async fn switch_php(version: String) -> Result<String, String> {
    match client().send(DaemonRequest::PhpSwitch { version }).await {
        Ok(DaemonResponse::Ok { message }) => Ok(message.unwrap_or_default()),
        Ok(DaemonResponse::Error { message }) => Err(message),
        Ok(_) => Err("unexpected response".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub async fn set_php_config(key: String, value: String) -> Result<String, String> {
    match client().send(DaemonRequest::PhpConfig {
        version: "active".to_string(),
        key,
        value,
    }).await {
        Ok(DaemonResponse::Ok { message }) => Ok(message.unwrap_or_default()),
        Ok(DaemonResponse::Error { message }) => Err(message),
        Ok(_) => Err("unexpected response".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub async fn ensure_daemon() -> Result<String, String> {
    if client().is_daemon_running().await {
        return Ok("daemon already running".to_string());
    }

    std::process::Command::new("hearth-daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to start daemon: {e}"))?;

    // Wait for socket to appear
    let socket_path = hearth_lib::socket::socket_path();
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if socket_path.exists() {
            return Ok("daemon started".to_string());
        }
    }

    Ok("daemon starting (socket not yet available)".to_string())
}
```

- [ ] **Step 2: Update `main.rs` to register commands**

Replace `crates/hearth-gui/src/main.rs`:

```rust
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            commands::get_status,
            commands::start_services,
            commands::stop_services,
            commands::restart_service,
            commands::get_sites,
            commands::link_site,
            commands::unlink_site,
            commands::secure_site,
            commands::unsecure_site,
            commands::get_php_versions,
            commands::switch_php,
            commands::set_php_config,
            commands::ensure_daemon,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
```

- [ ] **Step 3: Verify compilation**

Run: `cargo check -p hearth-gui`
Expected: Compiles

- [ ] **Step 4: Commit**

```bash
git add crates/hearth-gui/src/
git commit -m "feat(gui): implement Tauri commands wrapping DaemonClient"
```

---

## Task 5: System tray with status polling

**Files:**
- Create: `crates/hearth-gui/src/tray.rs`
- Modify: `crates/hearth-gui/src/main.rs`
- Create: Tray icon PNGs (16x16 template images)

- [ ] **Step 1: Create tray icon placeholder PNGs**

Create 16x16 PNG files for each tray state. For development, generate solid-color circles:

```bash
mkdir -p crates/hearth-gui/icons
# Use sips or ImageMagick to create placeholder icons
# For now, we'll use the Tauri default icon and swap later
```

Note: For the initial build, we'll use Tauri's `icon_as_template: true` and dynamically change the icon. Create placeholder PNGs — actual icon design is a polish step.

- [ ] **Step 2: Create `crates/hearth-gui/src/tray.rs`**

```rust
use std::sync::Arc;
use std::time::Duration;

use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::menu::{MenuBuilder, MenuItemBuilder, PredefinedMenuItem};
use tauri::{AppHandle, Manager};
use tokio::sync::Mutex;

use hearth_lib::client::DaemonClient;
use hearth_lib::socket::{DaemonRequest, DaemonResponse, ServiceStatus};

#[derive(Clone, Copy, PartialEq)]
pub enum TrayState {
    Green,  // All running
    Yellow, // Partial
    Red,    // Failed
    Grey,   // Daemon down
}

pub fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let tray = TrayIconBuilder::new()
        .icon(app.default_window_icon().unwrap().clone())
        .icon_as_template(true)
        .tooltip("Hearth — Loading...")
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                toggle_dashboard(app);
            }
        })
        .build(app)?;

    // Spawn polling loop
    let app_handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let client = DaemonClient::new();
        let mut last_state = TrayState::Grey;

        loop {
            let (state, tooltip) = poll_status(&client).await;

            if state != last_state {
                // Update tooltip
                let _ = app_handle
                    .tray_by_id("main")
                    .map(|t| t.set_tooltip(Some(&tooltip)));
                last_state = state;
            }

            // Emit status to frontend for dashboard updates
            let _ = app_handle.emit("tray-status-changed", &tooltip);

            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });

    Ok(())
}

async fn poll_status(client: &DaemonClient) -> (TrayState, String) {
    match client.send(DaemonRequest::Status).await {
        Ok(DaemonResponse::Status { services }) => {
            let running = services.iter().filter(|s| s.state == "Running").count();
            let failed = services.iter().filter(|s| s.state.starts_with("Failed")).count();
            let total = services.len();

            let state = if failed > 0 {
                TrayState::Red
            } else if running == total {
                TrayState::Green
            } else if running > 0 {
                TrayState::Yellow
            } else {
                TrayState::Yellow
            };

            let summary: Vec<String> = services
                .iter()
                .map(|s| format!("{}: {}", s.name, s.state))
                .collect();

            let tooltip = format!("Hearth — {}/{} running\n{}", running, total, summary.join("\n"));
            (state, tooltip)
        }
        _ => (TrayState::Grey, "Hearth — Daemon not connected".to_string()),
    }
}

fn toggle_dashboard(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        if window.is_visible().unwrap_or(false) {
            let _ = window.hide();
        } else {
            let _ = window.show();
            let _ = window.set_focus();
        }
    } else {
        let _ = tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::App("index.html".into()))
            .title("Hearth")
            .inner_size(900.0, 600.0)
            .resizable(true)
            .build();
    }
}
```

- [ ] **Step 3: Update `main.rs` to set up tray**

Replace `crates/hearth-gui/src/main.rs`:

```rust
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod tray;

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            tray::setup_tray(app.handle())?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_status,
            commands::start_services,
            commands::stop_services,
            commands::restart_service,
            commands::get_sites,
            commands::link_site,
            commands::unlink_site,
            commands::secure_site,
            commands::unsecure_site,
            commands::get_php_versions,
            commands::switch_php,
            commands::set_php_config,
            commands::ensure_daemon,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
```

- [ ] **Step 4: Verify compilation**

Run: `cargo check -p hearth-gui`
Expected: Compiles

- [ ] **Step 5: Commit**

```bash
git add crates/hearth-gui/src/tray.rs crates/hearth-gui/src/main.rs crates/hearth-gui/icons/
git commit -m "feat(gui): add system tray with status polling"
```

---

## Task 6: Dump streaming via Tauri events

**Files:**
- Create: `crates/hearth-gui/src/dump.rs`
- Modify: `crates/hearth-gui/src/main.rs`

- [ ] **Step 1: Create `crates/hearth-gui/src/dump.rs`**

```rust
use std::time::Duration;

use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tracing::{error, info, warn};

/// Spawn a background task that connects to the dump relay and emits lines as Tauri events.
pub fn start_dump_listener(app: &AppHandle, dump_port: u16) {
    let app_handle = app.clone();
    let relay_port = dump_port + 1;

    tauri::async_runtime::spawn(async move {
        loop {
            let addr = format!("127.0.0.1:{relay_port}");
            match TcpStream::connect(&addr).await {
                Ok(stream) => {
                    info!(relay_port, "dump subscriber connected");
                    let _ = app_handle.emit("dump-connected", ());

                    let (reader, _) = stream.into_split();
                    let mut reader = BufReader::new(reader);
                    let mut line = String::new();

                    loop {
                        match reader.read_line(&mut line).await {
                            Ok(0) => break, // EOF
                            Ok(_) => {
                                let trimmed = line.trim_end().to_string();
                                if !trimmed.is_empty() {
                                    let now = chrono::Local::now();
                                    let formatted = format!(
                                        "[{}] {}",
                                        now.format("%H:%M:%S"),
                                        trimmed
                                    );
                                    let _ = app_handle.emit("dump-line", &formatted);
                                }
                                line.clear();
                            }
                            Err(e) => {
                                warn!(error = %e, "dump stream read error");
                                break;
                            }
                        }
                    }

                    let _ = app_handle.emit("dump-disconnected", ());
                    info!("dump subscriber disconnected, reconnecting...");
                }
                Err(_) => {
                    // Daemon not running or dump server not ready — silent retry
                }
            }

            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}
```

- [ ] **Step 2: Wire into `main.rs` setup**

In `crates/hearth-gui/src/main.rs`, add `mod dump;` and update the `setup` closure:

```rust
mod dump;
```

Update the setup closure to start the dump listener:

```rust
        .setup(|app| {
            tray::setup_tray(app.handle())?;

            // Start dump stream listener
            let config = hearth_lib::config::HearthConfig::load().unwrap_or_default();
            dump::start_dump_listener(app.handle(), config.dump_port);

            Ok(())
        })
```

- [ ] **Step 3: Verify compilation**

Run: `cargo check -p hearth-gui`
Expected: Compiles

- [ ] **Step 4: Commit**

```bash
git add crates/hearth-gui/src/dump.rs crates/hearth-gui/src/main.rs
git commit -m "feat(gui): add dump streaming via Tauri events"
```

---

## Task 7: macOS notifications with debounce

**Files:**
- Create: `crates/hearth-gui/src/notifications.rs`
- Modify: `crates/hearth-gui/src/tray.rs`

- [ ] **Step 1: Create `crates/hearth-gui/src/notifications.rs`**

```rust
use std::collections::HashMap;
use std::time::{Duration, Instant};

use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;

/// Tracks last notification time per service to prevent spam.
pub struct NotificationDebouncer {
    last_sent: HashMap<String, Instant>,
    cooldown: Duration,
}

impl NotificationDebouncer {
    pub fn new() -> Self {
        Self {
            last_sent: HashMap::new(),
            cooldown: Duration::from_secs(30),
        }
    }

    /// Send a notification if the cooldown has elapsed for this service.
    pub fn notify_if_allowed(&mut self, app: &AppHandle, service: &str, title: &str, body: &str) {
        let now = Instant::now();
        if let Some(last) = self.last_sent.get(service) {
            if now.duration_since(*last) < self.cooldown {
                return;
            }
        }

        let _ = app.notification()
            .builder()
            .title(title)
            .body(body)
            .show();

        self.last_sent.insert(service.to_string(), now);
    }
}
```

- [ ] **Step 2: Integrate notifications into tray polling**

In `crates/hearth-gui/src/tray.rs`, update the polling loop to detect state changes and send notifications. Update the `poll_status` function signature and the spawn loop:

Add to imports in `tray.rs`:

```rust
use crate::notifications::NotificationDebouncer;
```

Update the polling loop inside `setup_tray` to track previous states and notify:

```rust
    // Spawn polling loop
    let app_handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let client = DaemonClient::new();
        let mut last_state = TrayState::Grey;
        let mut debouncer = NotificationDebouncer::new();
        let mut prev_services: Vec<ServiceStatus> = Vec::new();

        loop {
            let (state, tooltip, services) = poll_status(&client).await;

            if state != last_state {
                let _ = app_handle
                    .tray_by_id("main")
                    .map(|t| t.set_tooltip(Some(&tooltip)));
                last_state = state;
            }

            // Check for state transitions → notifications
            for svc in &services {
                let prev = prev_services.iter().find(|s| s.name == svc.name);
                if let Some(prev) = prev {
                    if prev.state == "Running" && svc.state.starts_with("Failed") {
                        debouncer.notify_if_allowed(
                            &app_handle,
                            &svc.name,
                            "Hearth — Service Failed",
                            &format!("{} stopped after repeated failures. Manual restart required.", svc.name),
                        );
                    }
                }
            }
            prev_services = services;

            let _ = app_handle.emit("tray-status-changed", &tooltip);
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
```

Update `poll_status` to return services:

```rust
async fn poll_status(client: &DaemonClient) -> (TrayState, String, Vec<ServiceStatus>) {
    match client.send(DaemonRequest::Status).await {
        Ok(DaemonResponse::Status { services }) => {
            let running = services.iter().filter(|s| s.state == "Running").count();
            let failed = services.iter().filter(|s| s.state.starts_with("Failed")).count();
            let total = services.len();

            let state = if failed > 0 {
                TrayState::Red
            } else if running == total {
                TrayState::Green
            } else if running > 0 {
                TrayState::Yellow
            } else {
                TrayState::Yellow
            };

            let summary: Vec<String> = services
                .iter()
                .map(|s| format!("{}: {}", s.name, s.state))
                .collect();

            let tooltip = format!("Hearth — {}/{} running\n{}", running, total, summary.join("\n"));
            (state, tooltip, services)
        }
        _ => (TrayState::Grey, "Hearth — Daemon not connected".to_string(), Vec::new()),
    }
}
```

- [ ] **Step 3: Add `mod notifications;` to `main.rs`**

In `crates/hearth-gui/src/main.rs`, add:

```rust
mod notifications;
```

- [ ] **Step 4: Verify compilation**

Run: `cargo check -p hearth-gui`
Expected: Compiles

- [ ] **Step 5: Commit**

```bash
git add crates/hearth-gui/src/notifications.rs crates/hearth-gui/src/tray.rs crates/hearth-gui/src/main.rs
git commit -m "feat(gui): add macOS notifications with 30s debounce per service"
```

---

## Task 8: Login item (auto-start on boot)

**Files:**
- Modify: `crates/hearth-gui/src/main.rs`
- Create: `crates/hearth-gui/src/autostart.rs`

- [ ] **Step 1: Create `crates/hearth-gui/src/autostart.rs`**

```rust
use tauri::AppHandle;
use tauri_plugin_autostart::ManagerExt;

/// On first launch, enable auto-start. User can disable via dashboard settings.
pub fn setup_autostart(app: &AppHandle) {
    let manager = app.autolaunch();
    match manager.is_enabled() {
        Ok(false) => {
            // First launch or user hasn't configured — enable by default
            if let Err(e) = manager.enable() {
                tracing::warn!(error = %e, "failed to enable autostart");
            } else {
                tracing::info!("autostart enabled");
            }
        }
        Ok(true) => {
            tracing::info!("autostart already enabled");
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to check autostart status");
        }
    }
}
```

- [ ] **Step 2: Add Tauri commands for toggling autostart**

Add to `crates/hearth-gui/src/commands.rs`:

```rust
use tauri::AppHandle;
use tauri_plugin_autostart::ManagerExt;

#[tauri::command]
pub async fn get_autostart_enabled(app: AppHandle) -> Result<bool, String> {
    app.autolaunch()
        .is_enabled()
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn set_autostart(app: AppHandle, enabled: bool) -> Result<(), String> {
    let manager = app.autolaunch();
    if enabled {
        manager.enable().map_err(|e| e.to_string())
    } else {
        manager.disable().map_err(|e| e.to_string())
    }
}
```

- [ ] **Step 3: Wire into main.rs**

Add `mod autostart;` and call `autostart::setup_autostart` in the setup closure. Register the new commands:

```rust
mod autostart;
```

In setup:

```rust
        .setup(|app| {
            tray::setup_tray(app.handle())?;
            autostart::setup_autostart(app.handle());

            let config = hearth_lib::config::HearthConfig::load().unwrap_or_default();
            dump::start_dump_listener(app.handle(), config.dump_port);

            Ok(())
        })
```

Add to `invoke_handler`:

```rust
            commands::get_autostart_enabled,
            commands::set_autostart,
```

- [ ] **Step 4: Verify compilation**

Run: `cargo check -p hearth-gui`
Expected: Compiles

- [ ] **Step 5: Commit**

```bash
git add crates/hearth-gui/src/autostart.rs crates/hearth-gui/src/commands.rs crates/hearth-gui/src/main.rs
git commit -m "feat(gui): add login item auto-start (on by default)"
```

---

## Task 9: CI — Tauri build + DMG packaging

**Files:**
- Modify: `.github/workflows/release.yml`

- [ ] **Step 1: Add `build-gui` job to release workflow**

Add a new job after the existing `release` job in `.github/workflows/release.yml`:

```yaml
  build-gui:
    runs-on: macos-latest
    needs: release

    steps:
      - uses: actions/checkout@v4

      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable

      - name: Install Tauri CLI
        run: cargo install tauri-cli --version "^2"

      - name: Build Tauri app
        working-directory: crates/hearth-gui
        run: cargo tauri build

      - name: Upload DMG to release
        uses: softprops/action-gh-release@v2
        with:
          files: crates/hearth-gui/target/release/bundle/dmg/*.dmg
```

- [ ] **Step 2: Verify workflow syntax**

Run: `cat .github/workflows/release.yml | python3 -c "import sys,yaml; yaml.safe_load(sys.stdin)" && echo "YAML OK"`
Expected: YAML OK

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "ci: add Tauri GUI build + DMG upload to release workflow"
```

---

## Task 10: Smoke test + final verification

**Files:**
- Modify: `scripts/smoke-test.sh` (add GUI build check)

- [ ] **Step 1: Add GUI build check to smoke test**

Append to `scripts/smoke-test.sh`:

```bash
echo "=== GUI build check ==="
cargo check -p hearth-gui
echo "GUI build check: OK"
```

- [ ] **Step 2: Run full test suite**

Run: `cargo test && cargo check -p hearth-gui`
Expected: All 55+ tests pass, GUI compiles

- [ ] **Step 3: Run cargo clippy on the new crate**

Run: `cargo clippy -p hearth-gui -- -D warnings`
Expected: No warnings

- [ ] **Step 4: Commit**

```bash
git add scripts/smoke-test.sh
git commit -m "test: add GUI build check to smoke test"
```

- [ ] **Step 5: Update CLAUDE.md and README for Phase 3**

Update `CLAUDE.md` architecture diagram to include `hearth-gui`. Update `README.md` roadmap to check off Phase 3. Update version references.

```bash
git add CLAUDE.md README.md
git commit -m "docs: update CLAUDE.md and README for Phase 3"
```
