//! Composer wrapper invoked through the site's resolved PHP binary.
//!
//! Always shells out as `<site_php_binary> -d memory_limit=-1 <composer.phar> require <pkg>
//! [--dev] --no-interaction`. Avoids the system `composer` bash wrapper which uses the
//! system PHP and breaks Valet-isolated sites (see scratchpad 796, B3).
//!
//! Composer can hijack stdin on plugin-allow prompts. We pass `--no-interaction`,
//! `COMPOSER_NO_INTERACTION=1`, and `Stdio::null()` on stdin so any stray prompt fails
//! fast rather than hanging the daemon.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use tracing::{info, warn};

pub struct ComposerInvocation<'a> {
    pub site_path: &'a Path,
    pub php_binary: &'a Path,
    /// Absolute path to `composer.phar`. Resolved at `hearth install` time.
    pub composer_phar: &'a Path,
    pub package: &'a str,
    pub dev: bool,
    /// If set, write combined stdout/stderr to this file so the CLI can `tail -f` it.
    pub log_path: Option<&'a Path>,
}

#[derive(Debug)]
pub struct ComposerResult {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
}

/// Format the planned invocation string. Used for dry-run output and log headers.
pub fn planned_command(inv: &ComposerInvocation) -> String {
    let dev = if inv.dev { " --dev" } else { "" };
    format!(
        "{} -d memory_limit=-1 {} require {}{} --no-interaction",
        inv.php_binary.display(),
        inv.composer_phar.display(),
        inv.package,
        dev
    )
}

pub fn run_require(inv: ComposerInvocation<'_>, dry_run: bool) -> Result<ComposerResult> {
    if dry_run {
        let planned = planned_command(&inv);
        info!(planned = %planned, "dry-run composer require");
        return Ok(ComposerResult {
            success: true,
            stdout: format!("[dry-run] {planned}"),
            stderr: String::new(),
            exit_code: Some(0),
        });
    }

    if !inv.composer_phar.exists() {
        anyhow::bail!(
            "composer.phar not found at {} — re-run `hearth install` to refresh the path",
            inv.composer_phar.display()
        );
    }
    if !inv.php_binary.exists() {
        anyhow::bail!(
            "PHP binary not found at {} — check Valet isolation / config.default_php",
            inv.php_binary.display()
        );
    }

    let mut cmd = std::process::Command::new(inv.php_binary);
    cmd.arg("-d")
        .arg("memory_limit=-1")
        .arg(inv.composer_phar)
        .arg("require")
        .arg(inv.package);
    if inv.dev {
        cmd.arg("--dev");
    }
    cmd.arg("--no-interaction")
        .env("COMPOSER_NO_INTERACTION", "1")
        .env("COMPOSER_ALLOW_SUPERUSER", "1")
        .current_dir(inv.site_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = cmd.output().context("failed to spawn composer")?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if let Some(log_path) = inv.log_path {
        write_log(log_path, &planned_command(&inv), &stdout, &stderr).ok();
    }

    if !output.status.success() {
        warn!(
            package = inv.package,
            exit = ?output.status.code(),
            "composer require failed"
        );
    }

    Ok(ComposerResult {
        success: output.status.success(),
        stdout,
        stderr,
        exit_code: output.status.code(),
    })
}

fn write_log(path: &Path, planned: &str, stdout: &str, stderr: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = format!(
        "$ {planned}\n=== stdout ===\n{stdout}\n=== stderr ===\n{stderr}\n"
    );
    std::fs::write(path, body)?;
    Ok(())
}

/// Build a composer-phar path option from `HearthConfig.composer_phar`. Used by the
/// daemon when constructing a `ComposerInvocation`.
pub fn composer_phar_from_config(path: Option<&PathBuf>) -> Option<PathBuf> {
    path.cloned()
}

/// Probe common locations for `composer.phar` (Homebrew Cellar layout). Returns the
/// first existing match. Called by `hearth install` to populate
/// `HearthConfig.composer_phar`, and by the daemon as a fallback when config is unset
/// — so dogfood works even if the user installed Hearth before this feature shipped.
pub fn resolve_composer_phar() -> Option<PathBuf> {
    // 1. Homebrew Cellar — `/opt/homebrew/Cellar/composer/X.Y.Z/libexec/composer.phar`.
    let homebrew_cellar = PathBuf::from("/opt/homebrew/Cellar/composer");
    if let Ok(entries) = std::fs::read_dir(&homebrew_cellar) {
        for entry in entries.flatten() {
            let phar = entry.path().join("libexec/composer.phar");
            if phar.exists() {
                return Some(phar);
            }
        }
    }

    // 2. Apple-silicon canonical symlinked path (some configs only expose this).
    let opt_path = PathBuf::from("/opt/homebrew/opt/composer/libexec/composer.phar");
    if opt_path.exists() {
        return Some(opt_path);
    }

    // 3. Intel-mac Homebrew layout.
    let intel_path = PathBuf::from("/usr/local/Cellar/composer");
    if let Ok(entries) = std::fs::read_dir(&intel_path) {
        for entry in entries.flatten() {
            let phar = entry.path().join("libexec/composer.phar");
            if phar.exists() {
                return Some(phar);
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inv<'a>(site: &'a Path, php: &'a Path, phar: &'a Path) -> ComposerInvocation<'a> {
        ComposerInvocation {
            site_path: site,
            php_binary: php,
            composer_phar: phar,
            package: "laravel/horizon",
            dev: false,
            log_path: None,
        }
    }

    #[test]
    fn planned_command_uses_php_binary_and_phar() {
        let i = inv(
            Path::new("/site"),
            Path::new("/usr/local/php82/bin/php"),
            Path::new("/opt/composer.phar"),
        );
        let s = planned_command(&i);
        assert!(s.contains("/usr/local/php82/bin/php"));
        assert!(s.contains("-d memory_limit=-1"));
        assert!(s.contains("/opt/composer.phar"));
        assert!(s.contains("require laravel/horizon"));
        assert!(s.contains("--no-interaction"));
        assert!(!s.contains("--dev"));
    }

    #[test]
    fn planned_command_includes_dev_flag() {
        let mut i = inv(
            Path::new("/site"),
            Path::new("/usr/bin/php"),
            Path::new("/c.phar"),
        );
        i.dev = true;
        let s = planned_command(&i);
        assert!(s.contains("require laravel/horizon --dev"));
    }

    #[test]
    fn dry_run_does_not_invoke_command() {
        // Use nonexistent paths — dry-run must not stat or spawn.
        let i = inv(
            Path::new("/nonexistent-site"),
            Path::new("/nonexistent-php"),
            Path::new("/nonexistent-phar"),
        );
        let r = run_require(i, true).unwrap();
        assert!(r.success);
        assert!(r.stdout.starts_with("[dry-run]"));
    }

    #[test]
    fn live_run_errors_when_phar_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let php_path = tmp.path().join("php-fake");
        let phar_path = tmp.path().join("composer.phar");
        let i = inv(tmp.path(), &php_path, &phar_path);
        let err = run_require(i, false).unwrap_err();
        assert!(err.to_string().contains("composer.phar not found"));
    }
}
