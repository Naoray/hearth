# Phase 2: MCP Server + Dev Services — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an in-process MCP server, Mailpit mail catcher, polished dump CLI streaming, and Homebrew distribution to Hearth.

**Architecture:** The MCP server lives in `hearth-lib` as a protocol adapter using rmcp's `#[tool_router]` macro, holding `Arc<Mutex<T>>` references to existing managers. The daemon spawns it as a Streamable HTTP listener via axum. A `hearth mcp` stdio bridge proxies stdin/stdout to the HTTP endpoint for legacy IDE clients.

**Tech Stack:** Rust, rmcp (MCP SDK), axum (HTTP), schemars (JSON Schema), chrono (timestamps), reqwest (downloads)

**Spec:** `docs/superpowers/specs/2026-03-22-phase2-mcp-dev-services-design.md`

---

## File Map

### New Files

| File | Responsibility |
|------|---------------|
| `crates/hearth-lib/src/mcp.rs` | `HearthMcpServer` struct, 8 MCP tool definitions, `ServerHandler` impl |
| `crates/hearth-lib/src/mailpit.rs` | Mailpit binary resolution chain (`resolve_mailpit_binary`) |
| `crates/hearth-lib/src/download.rs` | Reusable GitHub Release downloader (`download_github_release`) |
| `.github/workflows/release.yml` | CI: build release binaries on tag push, upload to GitHub Releases |

### Modified Files

| File | What changes |
|------|-------------|
| `Cargo.toml` | Add `rmcp`, `schemars`, `chrono`, `axum`, `flate2`, `tar`, `zip` to `[workspace.dependencies]` |
| `crates/hearth-lib/Cargo.toml` | Add `rmcp`, `schemars`, `chrono`, `flate2`, `tar`, `zip` deps |
| `crates/hearth-daemon/Cargo.toml` | Add `rmcp`, `axum` deps |
| `crates/hearth-cli/Cargo.toml` | Add `chrono`, `reqwest` deps (note: `reqwest` already in workspace, just needs crate-level entry) |
| `crates/hearth-lib/src/lib.rs` | Add `pub mod mcp;`, `pub mod mailpit;`, `pub mod download;` |
| `crates/hearth-lib/src/config.rs` | Add `#[serde(default)]` to struct, add `mcp_port: u16` field |
| `crates/hearth-lib/src/dump.rs` | Rewrite `stream_dumps()` with timestamps + auto-reconnect |
| `crates/hearth-lib/src/service/manager.rs` | Add conditional Mailpit registration in `default_services()` |
| `crates/hearth-daemon/src/main.rs` | Refactor DaemonState to `Arc<Mutex<T>>` per field, rewrite all 13 `process_request` arms, spawn MCP HTTP server |
| `crates/hearth-cli/src/main.rs` | Add `Mcp` subcommand (HTTP-to-stdio bridge) |

**Already done (no changes needed):**
- `crates/hearth-cli/Cargo.toml` already has `[[bin]] name = "hearth"`
- `crates/hearth-daemon/Cargo.toml` already has `[[bin]] name = "hearth-daemon"`

**Lock ordering convention:** When acquiring multiple `Arc<Mutex<T>>` locks, always acquire in this order to prevent deadlocks: `config` → `php_manager` → `site_manager` → `supervisor`. Drop locks as soon as they are no longer needed.

---

## Task 1: Config Migration — Add `#[serde(default)]` and `mcp_port`

**Files:**
- Modify: `crates/hearth-lib/src/config.rs`

This must come first because every subsequent task depends on the config compiling with the new field.

- [ ] **Step 1: Write test for new `mcp_port` field**

Add to the existing `tests` module in `crates/hearth-lib/src/config.rs`:

```rust
#[test]
fn default_config_has_mcp_port() {
    let config = HearthConfig::default();
    assert_eq!(config.mcp_port, 9900);
}

#[test]
fn load_legacy_config_without_mcp_port() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_path = tmp.path().join("config.toml");
    // Write a Phase 1 config file that lacks mcp_port
    std::fs::write(&config_path, r#"
tld = "test"
default_php = "8.4"
dns_port = 5354
dump_port = 9912
mail_smtp_port = 1025
mail_ui_port = 8025
parked_paths = []
"#).unwrap();

    let config = HearthConfig::load_from(&config_path).unwrap();
    assert_eq!(config.mcp_port, 9900); // should get default
    assert_eq!(config.tld, "test");    // existing fields preserved
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p hearth-lib config::tests::default_config_has_mcp_port config::tests::load_legacy_config_without_mcp_port`

Expected: compile error — `mcp_port` field doesn't exist yet.

- [ ] **Step 3: Add `#[serde(default)]` to `HearthConfig` and add `mcp_port` field**

In `crates/hearth-lib/src/config.rs`, add `#[serde(default)]` to the struct and the new field:

```rust
#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]  // <-- ADD THIS: future-proofs all field additions
pub struct HearthConfig {
    pub tld: String,
    pub default_php: String,
    pub dns_port: u16,
    pub dump_port: u16,
    pub mail_smtp_port: u16,
    pub mail_ui_port: u16,
    pub mcp_port: u16,              // <-- NEW
    pub ploi_api_token: Option<String>,
    pub parked_paths: Vec<PathBuf>,
}
```

Update the `Default` impl to include `mcp_port`:

```rust
impl Default for HearthConfig {
    fn default() -> Self {
        Self {
            tld: "test".to_string(),
            default_php: "8.4".to_string(),
            dns_port: 5354,
            dump_port: 9912,
            mail_smtp_port: 1025,
            mail_ui_port: 8025,
            mcp_port: 9900,         // <-- NEW
            ploi_api_token: None,
            parked_paths: Vec::new(),
        }
    }
}
```

- [ ] **Step 4: Run all config tests**

Run: `cargo test -p hearth-lib config::tests`

Expected: all pass, including the new ones and existing `default_config_has_sane_values`.

- [ ] **Step 5: Commit**

```bash
git add crates/hearth-lib/src/config.rs
git commit -m "feat(config): add mcp_port field with serde(default) for migration safety"
```

---

## Task 2: Add Workspace Dependencies

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Modify: `crates/hearth-lib/Cargo.toml`
- Modify: `crates/hearth-daemon/Cargo.toml`
- Modify: `crates/hearth-cli/Cargo.toml`

- [ ] **Step 1: Add new dependencies to workspace root `Cargo.toml`**

Add under `[workspace.dependencies]`:

```toml
# MCP (Phase 2)
rmcp = { version = "1", features = ["server", "transport-streamable-http-server"] }
schemars = "1"
axum = "0.8"
chrono = { version = "0.4", default-features = false, features = ["clock"] }

# Archive extraction (for GitHub Release downloads)
flate2 = "1"
tar = "0.4"
zip = "2"
```

**Note:** rmcp was previously commented out at version 0.16 in root `Cargo.toml`. We are using 1.x which has a significantly different API (`#[tool_router]`, `#[tool_handler]` macros, `ServerHandler` trait). If rmcp 1.x is not yet published or the feature flags differ, check crates.io and adapt the code snippets in Task 7 accordingly. The patterns shown are based on rmcp 1.x documentation.

- [ ] **Step 2: Add deps to `crates/hearth-lib/Cargo.toml`**

Add under `[dependencies]`:

```toml
rmcp = { workspace = true }
schemars = { workspace = true }
chrono = { workspace = true }
flate2 = { workspace = true }
tar = { workspace = true }
zip = { workspace = true }
```

- [ ] **Step 3: Add deps to `crates/hearth-daemon/Cargo.toml`**

Add under `[dependencies]`:

```toml
rmcp = { workspace = true }
axum = { workspace = true }
```

- [ ] **Step 4: Add dep to `crates/hearth-cli/Cargo.toml`**

Add under `[dependencies]`:

```toml
chrono = { workspace = true }
reqwest = { workspace = true }
```

- [ ] **Step 5: Verify workspace compiles**

Run: `cargo check --workspace`

Expected: compiles with no errors. This will download all new crates.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/hearth-lib/Cargo.toml crates/hearth-daemon/Cargo.toml crates/hearth-cli/Cargo.toml
git commit -m "deps: add rmcp, schemars, axum, chrono for Phase 2"
```

---

## Task 3: Dump Server CLI Polish — Timestamps + Auto-reconnect

**Files:**
- Modify: `crates/hearth-lib/src/dump.rs`

- [ ] **Step 1: Write test for timestamp formatting**

Add to the existing `tests` module in `crates/hearth-lib/src/dump.rs`:

```rust
#[test]
fn format_dump_line_prepends_timestamp() {
    let line = "some dump output";
    let formatted = format_dump_line(line);
    // Should match pattern: \x1b[2m[HH:MM:SS]\x1b[0m some dump output
    assert!(formatted.contains(line));
    assert!(formatted.starts_with("\x1b[2m["));
    assert!(formatted.contains("]\x1b[0m "));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p hearth-lib dump::tests::format_dump_line_prepends_timestamp`

Expected: compile error — `format_dump_line` not defined.

- [ ] **Step 3: Implement `format_dump_line` helper**

Add to `crates/hearth-lib/src/dump.rs` (above the `stream_dumps` function):

```rust
/// Format a dump line with a dim timestamp prefix.
pub fn format_dump_line(line: &str) -> String {
    let now = chrono::Local::now();
    format!("\x1b[2m[{}]\x1b[0m {}", now.format("%H:%M:%S"), line)
}
```

Add `use chrono` is not needed — we use the full path.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p hearth-lib dump::tests::format_dump_line_prepends_timestamp`

Expected: PASS.

- [ ] **Step 5: Rewrite `stream_dumps` with timestamps and auto-reconnect**

Replace the existing `stream_dumps` function in `crates/hearth-lib/src/dump.rs`. **Note:** The function now runs indefinitely (auto-reconnect loop). The CLI exits only on Ctrl+C or irrecoverable error. This is the desired behavior — the user runs `hearth dump` in a terminal and it streams until interrupted.

```rust
/// Connect to the dump relay and stream output to stdout with timestamps.
///
/// Auto-reconnects if the daemon restarts. Used by `hearth dump` CLI command.
pub async fn stream_dumps(port: u16) -> anyhow::Result<()> {
    let addr = dump_addr(relay_port(port));

    loop {
        match TcpStream::connect(addr).await {
            Ok(stream) => {
                eprintln!("Listening for dumps on port {}...", port);
                let (reader, _) = stream.into_split();
                let mut reader = BufReader::new(reader);
                let mut line = String::new();

                loop {
                    match reader.read_line(&mut line).await {
                        Ok(0) => break, // EOF — connection closed
                        Ok(_) => {
                            print!("{}", format_dump_line(line.trim_end()));
                            println!();
                            line.clear();
                        }
                        Err(e) => {
                            warn!(error = %e, "dump stream read error");
                            break;
                        }
                    }
                }

                eprintln!("Connection lost, reconnecting...");
            }
            Err(e) => {
                warn!(error = %e, "failed to connect to dump relay");
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}
```

- [ ] **Step 6: Run all dump tests**

Run: `cargo test -p hearth-lib dump::tests`

Expected: all pass (existing broadcast test + new timestamp test).

- [ ] **Step 7: Commit**

```bash
git add crates/hearth-lib/src/dump.rs
git commit -m "feat(dump): add timestamp prefix and auto-reconnect to hearth dump"
```

---

## Task 4: Mailpit Binary Resolution

**Files:**
- Create: `crates/hearth-lib/src/mailpit.rs`
- Modify: `crates/hearth-lib/src/lib.rs`

- [ ] **Step 1: Write tests for Mailpit resolution chain**

Create `crates/hearth-lib/src/mailpit.rs` with tests first:

```rust
use std::path::{Path, PathBuf};

use tracing::info;

/// Resolve the Mailpit binary using a resolution chain:
/// 1. Hearth cache: ~/.config/hearth/services/mailpit/mailpit
/// 2. System PATH: `which mailpit`
/// 3. Not found (caller can trigger download)
pub fn resolve_mailpit_binary(config_dir: &Path) -> Option<PathBuf> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_from_hearth_cache() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bin_path = tmp.path().join("services/mailpit/mailpit");
        std::fs::create_dir_all(bin_path.parent().unwrap()).unwrap();
        std::fs::write(&bin_path, "fake-binary").unwrap();

        let result = resolve_mailpit_binary(tmp.path());
        assert_eq!(result, Some(bin_path));
    }

    #[test]
    fn returns_none_when_not_installed() {
        let tmp = tempfile::TempDir::new().unwrap();
        // No mailpit binary anywhere in the temp dir
        let result = resolve_mailpit_binary(tmp.path());
        // May find system mailpit, so we check it doesn't point to our temp dir
        if let Some(path) = &result {
            assert!(!path.starts_with(tmp.path()));
        }
    }

    #[test]
    fn hearth_cache_takes_priority() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bin_path = tmp.path().join("services/mailpit/mailpit");
        std::fs::create_dir_all(bin_path.parent().unwrap()).unwrap();
        std::fs::write(&bin_path, "hearth-version").unwrap();

        let result = resolve_mailpit_binary(tmp.path());
        assert!(result.is_some());
        assert!(result.unwrap().starts_with(tmp.path()));
    }
}
```

- [ ] **Step 2: Add module declaration to `lib.rs`**

Add to `crates/hearth-lib/src/lib.rs`:

```rust
pub mod mailpit;
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p hearth-lib mailpit::tests`

Expected: FAIL — `todo!()` panics.

- [ ] **Step 4: Implement `resolve_mailpit_binary`**

Replace the `todo!()` in `crates/hearth-lib/src/mailpit.rs`:

```rust
pub fn resolve_mailpit_binary(config_dir: &Path) -> Option<PathBuf> {
    // 1. Hearth cache
    let cached = config_dir.join("services/mailpit/mailpit");
    if cached.exists() {
        info!(path = %cached.display(), "resolved Mailpit from Hearth cache");
        return Some(cached);
    }

    // 2. System PATH
    if let Ok(output) = std::process::Command::new("which")
        .arg("mailpit")
        .output()
    {
        if output.status.success() {
            let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
            if path.exists() {
                info!(path = %path.display(), "resolved Mailpit from system PATH");
                return Some(path);
            }
        }
    }

    // 3. Not found
    None
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p hearth-lib mailpit::tests`

Expected: all PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/hearth-lib/src/mailpit.rs crates/hearth-lib/src/lib.rs
git commit -m "feat(mailpit): add binary resolution chain"
```

---

## Task 5: GitHub Release Downloader

**Files:**
- Create: `crates/hearth-lib/src/download.rs`
- Modify: `crates/hearth-lib/src/lib.rs`

- [ ] **Step 1: Write test for asset URL construction**

Create `crates/hearth-lib/src/download.rs`:

```rust
use std::path::Path;

use anyhow::Context;
use tracing::info;

/// Asset pattern for the current platform.
pub fn platform_asset_pattern(base: &str) -> String {
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "amd64"
    };
    format!("{base}-darwin-{arch}")
}

/// Construct the GitHub Releases API URL for the latest release.
pub fn latest_release_url(repo: &str) -> String {
    format!("https://api.github.com/repos/{repo}/releases/latest")
}

/// Download a binary from a GitHub Release.
///
/// Hits the releases API, finds the asset matching `asset_pattern`,
/// downloads it, and extracts to `dest`. Supports `.tar.gz` and raw binaries.
pub async fn download_github_release(
    repo: &str,
    asset_pattern: &str,
    dest: &Path,
) -> anyhow::Result<()> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_asset_pattern_contains_darwin() {
        let pattern = platform_asset_pattern("mailpit");
        assert!(pattern.contains("darwin"));
        assert!(pattern.starts_with("mailpit-darwin-"));
    }

    #[test]
    fn latest_release_url_format() {
        let url = latest_release_url("axllent/mailpit");
        assert_eq!(url, "https://api.github.com/repos/axllent/mailpit/releases/latest");
    }
}
```

- [ ] **Step 2: Add module declaration to `lib.rs`**

Add to `crates/hearth-lib/src/lib.rs`:

```rust
pub mod download;
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p hearth-lib download::tests`

Expected: PASS (the `todo!()` is in the async function which isn't called in these tests).

- [ ] **Step 4: Implement `download_github_release`**

Replace the `todo!()` in `crates/hearth-lib/src/download.rs`:

```rust
pub async fn download_github_release(
    repo: &str,
    asset_pattern: &str,
    dest: &Path,
) -> anyhow::Result<()> {
    let url = latest_release_url(repo);

    let client = reqwest::Client::builder()
        .user_agent("hearth")
        .build()?;

    // Fetch latest release metadata
    let release: serde_json::Value = client
        .get(&url)
        .send()
        .await?
        .error_for_status()
        .context("failed to fetch latest release")?
        .json()
        .await?;

    // Find matching asset
    let assets = release["assets"]
        .as_array()
        .context("no assets in release")?;

    let asset = assets
        .iter()
        .find(|a| {
            a["name"]
                .as_str()
                .is_some_and(|n| n.contains(asset_pattern))
        })
        .context(format!("no asset matching '{asset_pattern}' in release"))?;

    let download_url = asset["browser_download_url"]
        .as_str()
        .context("asset missing download URL")?;

    let tag = release["tag_name"].as_str().unwrap_or("unknown");
    info!(repo, tag, asset = asset_pattern, "downloading release asset");

    // Download the asset
    let bytes = client
        .get(download_url)
        .send()
        .await?
        .error_for_status()
        .context("failed to download asset")?
        .bytes()
        .await?;

    // Ensure destination directory exists
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // If it looks like a tar.gz, extract; otherwise write raw binary
    let asset_name = asset["name"].as_str().unwrap_or("");
    if asset_name.ends_with(".tar.gz") || asset_name.ends_with(".tgz") {
        let decoder = flate2::read::GzDecoder::new(&bytes[..]);
        let mut archive = tar::Archive::new(decoder);
        let parent = dest.parent().context("dest has no parent")?;
        archive.unpack(parent)?;
        info!(path = %dest.display(), "extracted release archive");
    } else if asset_name.ends_with(".zip") {
        // Write to temp file, then unzip
        let tmp = dest.with_extension("zip");
        std::fs::write(&tmp, &bytes)?;
        let file = std::fs::File::open(&tmp)?;
        let mut archive = zip::ZipArchive::new(file)?;
        let parent = dest.parent().context("dest has no parent")?;
        archive.extract(parent)?;
        std::fs::remove_file(&tmp)?;
        info!(path = %dest.display(), "extracted zip archive");
    } else {
        std::fs::write(dest, &bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))?;
        }
        info!(path = %dest.display(), "wrote binary");
    }

    Ok(())
}
```

Note: `flate2`, `tar`, and `zip` were already added in Task 2.

- [ ] **Step 5: Verify compile**

Run: `cargo check -p hearth-lib`

Expected: compiles.

- [ ] **Step 6: Commit**

```bash
git add crates/hearth-lib/src/download.rs crates/hearth-lib/src/lib.rs Cargo.toml Cargo.lock crates/hearth-lib/Cargo.toml
git commit -m "feat(download): add reusable GitHub Release downloader"
```

---

## Task 6: Conditional Mailpit Registration in Supervisor

**Files:**
- Modify: `crates/hearth-lib/src/service/manager.rs`

- [ ] **Step 1: Write test for conditional Mailpit registration**

Add to the existing `tests` module in `crates/hearth-lib/src/service/manager.rs`:

```rust
#[test]
fn default_services_includes_mailpit_when_binary_found() {
    let tmp = tempfile::TempDir::new().unwrap();
    let bin_path = tmp.path().join("services/mailpit/mailpit");
    std::fs::create_dir_all(bin_path.parent().unwrap()).unwrap();
    std::fs::write(&bin_path, "fake").unwrap();

    let config = HearthConfig::default();
    let services = default_services(&config, tmp.path());

    let kinds: Vec<ServiceKind> = services.iter().map(|s| s.kind).collect();
    assert!(kinds.contains(&ServiceKind::Mailpit));
}

#[test]
fn default_services_excludes_mailpit_when_binary_missing() {
    let tmp = tempfile::TempDir::new().unwrap();
    // No mailpit binary

    let config = HearthConfig::default();
    let services = default_services(&config, tmp.path());

    let kinds: Vec<ServiceKind> = services.iter().map(|s| s.kind).collect();
    assert!(!kinds.contains(&ServiceKind::Mailpit));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p hearth-lib service::manager::tests`

Expected: compile error — `default_services` doesn't accept a config_dir parameter.

- [ ] **Step 3: Update `default_services` to accept `config_dir` and conditionally add Mailpit**

In `crates/hearth-lib/src/service/manager.rs`, update the function signature and add Mailpit:

```rust
use super::supervisor::ManagedService;
use super::ServiceKind;
use crate::config::HearthConfig;
use crate::mailpit;

/// Build the default set of managed services based on configuration.
///
/// Services are configured here but NOT started — the daemon calls
/// `supervisor.start_all()` when ready.
pub fn default_services(config: &HearthConfig, config_dir: &std::path::Path) -> Vec<ManagedService> {
    let mut services = vec![
        // Nginx — delegates to Valet's installed nginx
        ManagedService::new(
            ServiceKind::Nginx,
            "nginx".to_string(),
            vec![
                "-c".to_string(),
                config_dir
                    .join("nginx/nginx.conf")
                    .to_string_lossy()
                    .to_string(),
                "-g".to_string(),
                "daemon off;".to_string(),
            ],
        ),
        // dnsmasq on unprivileged port
        ManagedService::new(
            ServiceKind::Dnsmasq,
            "dnsmasq".to_string(),
            vec![
                "--keep-in-foreground".to_string(),
                format!("--port={}", config.dns_port),
                format!(
                    "--conf-file={}",
                    config_dir.join("dnsmasq/dnsmasq.conf").display()
                ),
            ],
        ),
        // PHP-FPM using the active PHP version
        ManagedService::new(
            ServiceKind::PhpFpm,
            "php-fpm".to_string(),
            vec![
                "--nodaemonize".to_string(),
                format!(
                    "--fpm-config={}",
                    config_dir.join("fpm/php-fpm.conf").display()
                ),
            ],
        ),
    ];

    // Mailpit — only if binary is found
    if let Some(mailpit_bin) = mailpit::resolve_mailpit_binary(config_dir) {
        services.push(ManagedService::new(
            ServiceKind::Mailpit,
            mailpit_bin.to_string_lossy().to_string(),
            vec![
                "--smtp".to_string(),
                format!("127.0.0.1:{}", config.mail_smtp_port),
                "--listen".to_string(),
                format!("127.0.0.1:{}", config.mail_ui_port),
                "--db-file".to_string(),
                config_dir
                    .join("services/mailpit/mailpit.db")
                    .to_string_lossy()
                    .to_string(),
            ],
        ));
    }

    services
}
```

- [ ] **Step 4: Update daemon to pass `config_dir` to `default_services`**

In `crates/hearth-daemon/src/main.rs`, find the call to `default_services(&config)` and change it to:

```rust
let config_dir = hearth_lib::config_dir();
for svc in default_services(&config, &config_dir) {
    supervisor.register(svc);
}
```

(The `config_dir` variable already exists on the line above — rename the existing `_config_dir` to `config_dir`.)

- [ ] **Step 5: Update existing tests for new signature**

In `crates/hearth-lib/src/service/manager.rs`, update the existing test:

```rust
#[test]
fn default_services_contains_expected_kinds() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = HearthConfig::default();
    let services = default_services(&config, tmp.path());

    let kinds: Vec<ServiceKind> = services.iter().map(|s| s.kind).collect();
    assert!(kinds.contains(&ServiceKind::Nginx));
    assert!(kinds.contains(&ServiceKind::Dnsmasq));
    assert!(kinds.contains(&ServiceKind::PhpFpm));
    // Mailpit excluded — no binary in temp dir
    assert!(!kinds.contains(&ServiceKind::Mailpit));
}
```

- [ ] **Step 6: Run all manager tests**

Run: `cargo test -p hearth-lib service::manager::tests`

Expected: all PASS.

- [ ] **Step 7: Run full workspace check**

Run: `cargo check --workspace`

Expected: compiles (daemon updated to pass config_dir).

- [ ] **Step 8: Commit**

```bash
git add crates/hearth-lib/src/service/manager.rs crates/hearth-daemon/src/main.rs
git commit -m "feat(mailpit): conditional supervisor registration with binary resolution"
```

---

## Task 7: MCP Server — Tool Definitions

**Files:**
- Create: `crates/hearth-lib/src/mcp.rs`
- Modify: `crates/hearth-lib/src/lib.rs`

This is the largest task. The MCP server struct and all 8 tool definitions.

- [ ] **Step 1: Create `mcp.rs` with the `HearthMcpServer` struct and read-only tools**

Create `crates/hearth-lib/src/mcp.rs`. Start with the 4 read-only tools first:

```rust
use std::sync::Arc;

use rmcp::{
    ErrorData as McpError,
    ServerHandler,
    model::*,
    tool, tool_router,
    handler::server::tool::ToolRouter,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::config::HearthConfig;
use crate::php::PhpManager;
use crate::service::supervisor::ServiceSupervisor;
use crate::service::ServiceState;
use crate::site::SiteManager;

/// MCP server exposing Hearth tools to IDEs.
///
/// Holds shared references to the daemon's managers. The daemon constructs
/// this with the same `Arc<Mutex<T>>` instances it uses for socket handlers.
#[derive(Clone)]
pub struct HearthMcpServer {
    pub supervisor: Arc<Mutex<ServiceSupervisor>>,
    pub site_manager: Arc<Mutex<SiteManager>>,
    pub php_manager: Arc<Mutex<PhpManager>>,
    pub config: Arc<Mutex<HearthConfig>>,
    tool_router: ToolRouter<Self>,
}

impl HearthMcpServer {
    pub fn new(
        supervisor: Arc<Mutex<ServiceSupervisor>>,
        site_manager: Arc<Mutex<SiteManager>>,
        php_manager: Arc<Mutex<PhpManager>>,
        config: Arc<Mutex<HearthConfig>>,
    ) -> Self {
        Self {
            supervisor,
            site_manager,
            php_manager,
            config,
            tool_router: Self::tool_router(),
        }
    }
}

// -- Tool parameter/result types --

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct PhpSwitchParams {
    /// PHP version to switch to (e.g., "8.4")
    pub version: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SiteLinkParams {
    /// Filesystem path to the project directory
    pub path: String,
    /// Custom site name (defaults to directory name)
    pub name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SiteUnlinkParams {
    /// Site name to unlink
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ServiceRestartParams {
    /// Service name to restart (e.g., "nginx", "php-fpm"). Omit to restart all.
    pub service: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct PhpConfigParams {
    /// php.ini key (e.g., "memory_limit")
    pub key: String,
    /// Value to set (e.g., "512M")
    pub value: String,
}

// -- Tool implementations --

#[tool_router]
impl HearthMcpServer {
    #[tool(description = "List all Hearth services and their current state (running, stopped, failed)")]
    async fn hearth_status(&self) -> Result<CallToolResult, McpError> {
        let sup = self.supervisor.lock().await;
        let statuses = sup.status();

        let mut lines = Vec::new();
        for (kind, state) in &statuses {
            let (state_str, pid_str) = match state {
                ServiceState::Running { pid } => ("running".to_string(), format!(" (PID {})", pid)),
                ServiceState::Stopped => ("stopped".to_string(), String::new()),
                ServiceState::Starting => ("starting".to_string(), String::new()),
                ServiceState::Failed { reason } => (format!("failed: {}", reason), String::new()),
            };
            lines.push(format!("{}: {}{}", kind.name(), state_str, pid_str));
        }

        Ok(CallToolResult::success(vec![Content::text(lines.join("\n"))]))
    }

    #[tool(description = "List all linked sites with their paths, SSL status, and PHP version")]
    async fn hearth_sites(&self) -> Result<CallToolResult, McpError> {
        let sm = self.site_manager.lock().await;
        match sm.list_sites() {
            Ok(sites) => {
                if sites.is_empty() {
                    return Ok(CallToolResult::success(vec![Content::text("No sites linked")]));
                }
                let mut lines = Vec::new();
                for site in &sites {
                    let ssl = if site.secured { "SSL" } else { "HTTP" };
                    let php = site.php_version.as_deref().unwrap_or("default");
                    lines.push(format!(
                        "{} ({}) — {} — PHP {}",
                        site.name,
                        site.path.display(),
                        ssl,
                        php
                    ));
                }
                Ok(CallToolResult::success(vec![Content::text(lines.join("\n"))]))
            }
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    #[tool(description = "List all installed PHP versions and which is currently active")]
    async fn hearth_php_list(&self) -> Result<CallToolResult, McpError> {
        let pm = self.php_manager.lock().await;
        let cfg = self.config.lock().await;
        let versions = pm.installed_versions_with_paths();

        if versions.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text("No PHP versions found")]));
        }

        let lines: Vec<String> = versions
            .iter()
            .map(|(ver, path)| {
                let active = if *ver == cfg.default_php { " (active)" } else { "" };
                format!("PHP {}{} — {}", ver, active, path.display())
            })
            .collect();

        Ok(CallToolResult::success(vec![Content::text(lines.join("\n"))]))
    }

    #[tool(description = "Switch the global PHP version and restart PHP-FPM")]
    async fn hearth_php_switch(
        &self,
        #[tool(param)] params: PhpSwitchParams,
    ) -> Result<CallToolResult, McpError> {
        let mut sup = self.supervisor.lock().await;
        let mut cfg = self.config.lock().await;
        let config_dir = crate::config_dir();

        let fpm_binary = crate::php::resolver::resolve_phpfpm_binary(&params.version, &config_dir)
            .ok_or_else(|| {
                McpError::invalid_params(
                    format!("PHP {} php-fpm binary not found", params.version),
                    None,
                )
            })?;

        let _ = sup.stop_service(crate::service::ServiceKind::PhpFpm);

        sup.reconfigure_service(
            crate::service::ServiceKind::PhpFpm,
            fpm_binary.to_string_lossy().to_string(),
            vec![
                "--nodaemonize".to_string(),
                format!("--fpm-config={}", config_dir.join("fpm/php-fpm.conf").display()),
            ],
        );

        sup.start_service(crate::service::ServiceKind::PhpFpm)
            .map_err(|e| McpError::internal_error(format!("failed to start php-fpm: {e}"), None))?;

        cfg.default_php = params.version.clone();
        cfg.save()
            .map_err(|e| McpError::internal_error(format!("config save failed: {e}"), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Switched to PHP {}",
            params.version
        ))]))
    }

    /// Known limitation: `path` param is not used yet — Valet links from the
    /// current working directory. A future improvement would pass `path` to
    /// `ValetCli::link` via `current_dir()`.
    #[tool(description = "Link a directory as a Hearth/Valet site")]
    async fn hearth_site_link(
        &self,
        #[tool(param)] params: SiteLinkParams,
    ) -> Result<CallToolResult, McpError> {
        crate::valet::ValetCli::link(params.name.as_deref())
            .map(|msg| CallToolResult::success(vec![Content::text(msg)]))
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(description = "Unlink a site")]
    async fn hearth_site_unlink(
        &self,
        #[tool(param)] params: SiteUnlinkParams,
    ) -> Result<CallToolResult, McpError> {
        crate::valet::ValetCli::unlink(&params.name)
            .map(|()| CallToolResult::success(vec![Content::text(format!("Unlinked {}", params.name))]))
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(description = "Restart a service (or all services if no name given)")]
    async fn hearth_service_restart(
        &self,
        #[tool(param)] params: ServiceRestartParams,
    ) -> Result<CallToolResult, McpError> {
        let mut sup = self.supervisor.lock().await;

        match params.service {
            None => {
                sup.stop_all()
                    .map_err(|e| McpError::internal_error(format!("stop failed: {e}"), None))?;
                sup.start_all()
                    .map_err(|e| McpError::internal_error(format!("start failed: {e}"), None))?;
                Ok(CallToolResult::success(vec![Content::text("All services restarted")]))
            }
            Some(name) => {
                let kind: crate::service::ServiceKind = name
                    .parse()
                    .map_err(|e: String| McpError::invalid_params(e, None))?;
                let _ = sup.stop_service(kind);
                sup.start_service(kind)
                    .map_err(|e| McpError::internal_error(format!("restart failed: {e}"), None))?;
                Ok(CallToolResult::success(vec![Content::text(format!("Restarted {name}"))]))
            }
        }
    }

    /// Lock ordering: config → php_manager → supervisor (per convention in File Map)
    #[tool(description = "Set a php.ini config value and restart PHP-FPM")]
    async fn hearth_php_config(
        &self,
        #[tool(param)] params: PhpConfigParams,
    ) -> Result<CallToolResult, McpError> {
        // Acquire locks in canonical order: config → php_manager → supervisor
        let cfg = self.config.lock().await;
        let pm = self.php_manager.lock().await;

        pm.set_ini_value(&cfg.default_php, &params.key, &params.value)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // Drop before acquiring supervisor lock
        drop(pm);
        drop(cfg);

        let mut sup = self.supervisor.lock().await;
        let _ = sup.stop_service(crate::service::ServiceKind::PhpFpm);
        sup.start_service(crate::service::ServiceKind::PhpFpm)
            .map_err(|e| McpError::internal_error(format!("fpm restart failed: {e}"), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Set {}={} and restarted php-fpm",
            params.key, params.value
        ))]))
    }
}

#[tool_handler]
impl ServerHandler for HearthMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V2025_03_26,
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .build(),
            server_info: Implementation::from_build_env(),
            instructions: Some("Hearth — Unified Laravel development command center. Use these tools to manage services, sites, and PHP versions.".to_string()),
        }
    }
}
```

- [ ] **Step 2: Add module declaration to `lib.rs`**

Add to `crates/hearth-lib/src/lib.rs`:

```rust
pub mod mcp;
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check -p hearth-lib`

Expected: compiles. (The exact rmcp API may need minor adjustments — adapt `#[tool(param)]` syntax to match the rmcp version's actual API. Check rmcp docs if compiler errors on parameter passing.)

- [ ] **Step 4: Write unit tests for read-only tools**

Add to `crates/hearth-lib/src/mcp.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::supervisor::{ManagedService, ServiceSupervisor};
    use std::path::PathBuf;

    fn make_test_server() -> HearthMcpServer {
        let mut sup = ServiceSupervisor::new();
        sup.register(ManagedService::new(
            crate::service::ServiceKind::Nginx,
            "true".to_string(),
            vec![],
        ));

        let tmp = tempfile::TempDir::new().unwrap();
        let valet_dir = tmp.path().join("valet");
        std::fs::create_dir_all(valet_dir.join("Nginx")).unwrap();

        HearthMcpServer::new(
            Arc::new(Mutex::new(sup)),
            Arc::new(Mutex::new(SiteManager::new(valet_dir, "test".to_string()))),
            Arc::new(Mutex::new(PhpManager::new(tmp.path().to_path_buf()))),
            Arc::new(Mutex::new(HearthConfig::default())),
        )
    }

    #[tokio::test]
    async fn status_tool_returns_services() {
        let server = make_test_server();
        let result = server.hearth_status().await.unwrap();
        let text = result.content.first().unwrap();
        match text {
            Content::Text(t) => assert!(t.text.contains("nginx")),
            _ => panic!("expected text content"),
        }
    }

    #[tokio::test]
    async fn sites_tool_returns_empty_list() {
        let server = make_test_server();
        let result = server.hearth_sites().await.unwrap();
        let text = result.content.first().unwrap();
        match text {
            Content::Text(t) => assert!(t.text.contains("No sites linked")),
            _ => panic!("expected text content"),
        }
    }

    #[tokio::test]
    async fn php_list_tool_works() {
        let server = make_test_server();
        let result = server.hearth_php_list().await.unwrap();
        // May find system PHP or return "No PHP versions found" — both are valid
        assert!(!result.content.is_empty());
    }

    #[tokio::test]
    async fn php_switch_errors_on_nonexistent_version() {
        let server = make_test_server();
        let result = server
            .hearth_php_switch(PhpSwitchParams {
                version: "99.99".to_string(),
            })
            .await;
        assert!(result.is_err());
    }
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p hearth-lib mcp::tests`

Expected: all PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/hearth-lib/src/mcp.rs crates/hearth-lib/src/lib.rs
git commit -m "feat(mcp): add HearthMcpServer with 8 MCP tools"
```

---

## Task 8: Daemon — Spawn MCP Streamable HTTP Server

**Files:**
- Modify: `crates/hearth-daemon/src/main.rs`

- [ ] **Step 1: Refactor DaemonState to use `Arc<Mutex<T>>` for individual managers**

This is a large refactor affecting all 13 match arms in `process_request`. The key change: replace `Arc<Mutex<DaemonState>>` (one big lock) with `Arc<DaemonState>` where each field is individually `Arc<Mutex<T>>`.

**Lock ordering convention:** Always acquire in order: `config` → `php_manager` → `site_manager` → `supervisor`. Drop locks before acquiring the next when possible.

Change `DaemonState` struct:

```rust
struct DaemonState {
    supervisor: Arc<Mutex<ServiceSupervisor>>,
    config: Arc<Mutex<HearthConfig>>,
    site_manager: Arc<Mutex<SiteManager>>,
    php_manager: Arc<Mutex<PhpManager>>,
}
```

Change construction in `main()`:

```rust
let supervisor = Arc::new(Mutex::new(supervisor));
let config_mutex = Arc::new(Mutex::new(config));
let site_manager = Arc::new(Mutex::new(SiteManager::new(valet_home, tld)));
let php_manager = Arc::new(Mutex::new(PhpManager::new(hearth_lib::config_dir())));

let state = Arc::new(DaemonState {
    supervisor: supervisor.clone(),
    config: config_mutex.clone(),
    site_manager: site_manager.clone(),
    php_manager: php_manager.clone(),
});
```

Change `handle_client` and `process_request` signatures from `Arc<Mutex<DaemonState>>` to `Arc<DaemonState>`:

```rust
async fn handle_client(
    stream: tokio::net::UnixStream,
    state: Arc<DaemonState>,
) -> anyhow::Result<()> { /* unchanged body */ }

async fn process_request(
    request: DaemonRequest,
    state: &Arc<DaemonState>,
) -> DaemonResponse { /* rewrite each arm */ }
```

Change the health check spawn to lock only supervisor:

```rust
let health_state = state.clone();
tokio::spawn(async move {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let mut s = health_state.supervisor.lock().await;
        s.health_check();
    }
});
```

Change the dump server spawn to read config once:

```rust
let dump_port = state.config.lock().await.dump_port;
```

**Rewrite all `process_request` match arms.** The pattern change for each arm:

- **Before:** `let mut s = state.lock().await; s.supervisor.start_all()`
- **After:** `let mut sup = state.supervisor.lock().await; sup.start_all()`

Arms that access multiple fields (PhpSwitch, PhpConfig, PhpList, Park, Sites, Status) need to lock each field individually. Follow the lock ordering convention. Example for `PhpConfig`:

```rust
DaemonRequest::PhpConfig { version, key, value } => {
    // Lock order: config → php_manager → supervisor
    let cfg = state.config.lock().await;
    let resolved_version = if version == "active" {
        cfg.default_php.clone()
    } else {
        version
    };
    drop(cfg);

    let pm = state.php_manager.lock().await;
    if let Err(e) = pm.set_ini_value(&resolved_version, &key, &value) {
        return DaemonResponse::Error { message: e.to_string() };
    }
    drop(pm);

    let mut sup = state.supervisor.lock().await;
    if let Err(e) = sup.stop_service(hearth_lib::service::ServiceKind::PhpFpm) {
        return DaemonResponse::Error {
            message: format!("INI updated but php-fpm stop failed: {e}"),
        };
    }
    if let Err(e) = sup.start_service(hearth_lib::service::ServiceKind::PhpFpm) {
        return DaemonResponse::Error {
            message: format!("INI updated but php-fpm restart failed: {e}"),
        };
    }

    DaemonResponse::Ok {
        message: Some(format!("Set {key}={value} for PHP {resolved_version} and restarted php-fpm")),
    }
}
```

Simple arms (Start, Stop, Ping, Link, Unlink, Secure, Unsecure) just change from `state.lock().await.X` to `state.X.lock().await`.

- [ ] **Step 2: Add MCP server spawn after dump server spawn**

Add after the dump server spawn in `main()`:

```rust
// Spawn MCP Streamable HTTP server
let mcp_port = state.config.lock().await.mcp_port;
let mcp_server = hearth_lib::mcp::HearthMcpServer::new(
    state.supervisor.clone(),
    state.site_manager.clone(),
    state.php_manager.clone(),
    state.config.clone(),
);

tokio::spawn(async move {
    use rmcp::transport::StreamableHttpServerConfig;

    let config = StreamableHttpServerConfig {
        bind: std::net::SocketAddr::from(([127, 0, 0, 1], mcp_port)),
        ..Default::default()
    };

    info!(port = mcp_port, "starting MCP Streamable HTTP server");

    match rmcp::transport::streamable_http_server(config, mcp_server).await {
        Ok(()) => info!("MCP server stopped"),
        Err(e) => error!(
            port = mcp_port,
            error = %e,
            "MCP server failed to bind (port may be in use) — continuing without MCP"
        ),
    }
});
```

Note: The exact rmcp API for starting a Streamable HTTP server may differ from the above. Consult the rmcp docs/examples for the correct function signature. The key points are: (1) bind to `127.0.0.1:9900`, (2) pass the `HearthMcpServer` instance, (3) non-fatal on failure.

- [ ] **Step 3: Verify daemon compiles**

Run: `cargo check -p hearth-daemon`

Expected: compiles. You may need to adjust the rmcp server startup API calls to match the actual rmcp version.

- [ ] **Step 4: Commit**

```bash
git add crates/hearth-daemon/src/main.rs
git commit -m "feat(daemon): spawn MCP Streamable HTTP server on startup"
```

---

## Task 9: CLI — `hearth mcp` Stdio Bridge

**Files:**
- Modify: `crates/hearth-cli/src/main.rs`

- [ ] **Step 1: Add `Mcp` subcommand to the CLI**

In `crates/hearth-cli/src/main.rs`, add to the `Commands` enum:

```rust
/// Start MCP stdio bridge (for IDE integration)
Mcp,
```

- [ ] **Step 2: Handle the Mcp command in main()**

Add the handler before the `send_to_daemon` call, alongside the existing `Install`, `Laravel`, `Mail`, and `Dump` early returns:

```rust
Commands::Mcp => {
    let config = hearth_lib::config::HearthConfig::load()?;
    let mcp_url = format!("http://127.0.0.1:{}/mcp", config.mcp_port);
    eprintln!("Hearth MCP stdio bridge → {}", mcp_url);
    return run_mcp_bridge(&mcp_url).await;
}
```

- [ ] **Step 3: Implement the bridge function**

Add at the bottom of `crates/hearth-cli/src/main.rs`:

```rust
/// Stdio-to-HTTP bridge for MCP clients that only support stdio transport.
///
/// Reads JSON-RPC from stdin, POSTs to the daemon's Streamable HTTP endpoint,
/// writes responses to stdout. Exits on stdin EOF.
async fn run_mcp_bridge(mcp_url: &str) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let client = reqwest::Client::new();
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();

    while reader.read_line(&mut line).await? > 0 {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            line.clear();
            continue;
        }

        match client
            .post(mcp_url)
            .header("Content-Type", "application/json")
            .body(trimmed.to_string())
            .send()
            .await
        {
            Ok(resp) => {
                let body = resp.text().await.unwrap_or_default();
                println!("{}", body);
            }
            Err(e) => {
                eprintln!("MCP bridge error: {}", e);
            }
        }

        line.clear();
    }

    Ok(())
}
```

Note: The actual MCP Streamable HTTP protocol may use SSE streaming for responses, which would require reading the response as a stream rather than a single body. The implementation above handles the simple request/response case. If rmcp's Streamable HTTP uses SSE for streaming, the bridge will need to read the event stream and forward each event line. Adjust based on testing.

- [ ] **Step 4: Verify it compiles**

Run: `cargo check -p hearth-cli`

Expected: compiles.

- [ ] **Step 5: Commit**

```bash
git add crates/hearth-cli/src/main.rs
git commit -m "feat(cli): add hearth mcp stdio bridge subcommand"
```

---

## Task 10: GitHub Actions Release Workflow

**Files:**
- Create: `.github/workflows/release.yml`

- [ ] **Step 1: Create the release workflow**

Create `.github/workflows/release.yml`:

```yaml
name: Release

on:
  push:
    tags:
      - 'v*'

permissions:
  contents: write

jobs:
  build:
    strategy:
      matrix:
        include:
          - target: aarch64-apple-darwin
            os: macos-latest
          - target: x86_64-apple-darwin
            os: macos-13

    runs-on: ${{ matrix.os }}

    steps:
      - uses: actions/checkout@v4

      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
        with:
          targets: ${{ matrix.target }}

      - name: Build release binaries
        run: cargo build --release --target ${{ matrix.target }}

      - name: Package binaries
        run: |
          mkdir -p dist
          cp target/${{ matrix.target }}/release/hearth dist/
          cp target/${{ matrix.target }}/release/hearth-daemon dist/
          tar -czf hearth-${{ github.ref_name }}-${{ matrix.target }}.tar.gz -C dist hearth hearth-daemon

      - name: Upload artifact
        uses: actions/upload-artifact@v4
        with:
          name: hearth-${{ matrix.target }}
          path: hearth-*.tar.gz

  release:
    needs: build
    runs-on: ubuntu-latest

    steps:
      - uses: actions/download-artifact@v4
        with:
          merge-multiple: true

      - name: Create GitHub Release
        uses: softprops/action-gh-release@v2
        with:
          files: hearth-*.tar.gz
          generate_release_notes: true
```

- [ ] **Step 2: Verify YAML syntax**

Run: `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/release.yml'))" 2>&1 || echo "install pyyaml or just check manually"`

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "ci: add release workflow for macOS binary artifacts"
```

---

## Task 11: Homebrew Formula

**Files:**
- External repo: `github.com/Naoray/homebrew-tap` — create `Formula/hearth.rb`

This task creates the Homebrew formula. Since it's in a separate repo, the implementer should clone the tap repo, add the formula, and push.

- [ ] **Step 1: Create the Homebrew formula**

In the `Naoray/homebrew-tap` repo, create `Formula/hearth.rb`:

```ruby
class Hearth < Formula
  desc "Unified Laravel development command center"
  homepage "https://github.com/Naoray/hearth"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Naoray/hearth/releases/latest/download/hearth-#{version}-aarch64-apple-darwin.tar.gz"
    else
      url "https://github.com/Naoray/hearth/releases/latest/download/hearth-#{version}-x86_64-apple-darwin.tar.gz"
    end
  end

  # Fallback: build from source if no pre-built binary
  head "https://github.com/Naoray/hearth.git", branch: "main"

  depends_on "rust" => :build if build.head?

  def install
    if build.head?
      system "cargo", "install", "--locked", "--root", prefix, "--path", "crates/hearth-cli"
      system "cargo", "install", "--locked", "--root", prefix, "--path", "crates/hearth-daemon"
    else
      bin.install "hearth"
      bin.install "hearth-daemon"
    end
  end

  test do
    assert_match "hearth", shell_output("#{bin}/hearth --help")
  end
end
```

Note: The `url` with `#{version}` won't resolve until the first tagged release exists. The `head` block provides a source build fallback. After the first release, update the formula with the actual version and SHA256 checksums.

- [ ] **Step 2: Commit and push to the tap repo**

```bash
cd /path/to/homebrew-tap
git add Formula/hearth.rb
git commit -m "Add hearth formula"
git push
```

- [ ] **Step 3: Verify install works (head build)**

```bash
brew install --HEAD naoray/tap/hearth
hearth --help
```

Expected: shows Hearth help text.

---

## Task 12: Smoke Test Update

**Files:**
- Modify: `scripts/smoke-test.sh`

- [ ] **Step 1: Add MCP server test to smoke test**

Add a new test section to `scripts/smoke-test.sh` after the dump server test:

```bash
# ── 9. MCP server reachable ──────────────────────────────────────
info "Testing MCP Streamable HTTP endpoint..."

sleep 1  # give MCP server time to bind

if curl -sf http://127.0.0.1:9900/mcp -X POST \
    -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"smoke-test","version":"0.1.0"}}}' \
    -o /tmp/hearth-smoke-test/mcp-response.json 2>/dev/null; then
    pass "MCP endpoint responded"
else
    fail "MCP endpoint not reachable at :9900"
fi
```

- [ ] **Step 2: Commit**

```bash
git add scripts/smoke-test.sh
git commit -m "test: add MCP endpoint check to smoke test"
```

---

## Task 13: Final Integration Verification

- [ ] **Step 1: Run full test suite**

Run: `cargo test --workspace`

Expected: all tests pass (35 existing + new tests from this plan).

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace --all-targets`

Expected: no warnings.

- [ ] **Step 3: Run smoke test**

Run: `./scripts/smoke-test.sh`

Expected: all checks pass (daemon starts, CLI connects, MCP responds, dump broadcasts).

- [ ] **Step 4: Final commit (if any fixups needed)**

```bash
git add -A
git commit -m "fix: address clippy warnings and test fixes from Phase 2"
```

---

## Execution Order & Dependencies

```
Task 1 (config)
  └─► Task 2 (workspace deps)
        ├─► Task 3 (dump polish) — independent
        ├─► Task 4 (mailpit resolver) → Task 6 (supervisor registration)
        ├─► Task 5 (download utility) — independent
        └─► Task 7 (MCP tools) → Task 8 (daemon integration) → Task 9 (CLI bridge)
Task 10 (CI release) — independent of all above
Task 11 (Homebrew) — depends on Task 10
Task 12 (smoke test) — depends on Task 8
Task 13 (final verification) — depends on all above
```

Tasks 3, 4, 5, and 10 can be parallelized after Task 2 completes.
