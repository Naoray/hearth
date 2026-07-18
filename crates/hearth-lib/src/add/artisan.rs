//! `php artisan ...` wrapper invoked through the site's resolved PHP binary.
//!
//! Always uses the absolute PHP path from `SiteContext.php_binary`, never bare `php`,
//! so Valet-isolated sites keep their isolated PHP version (see hearth-add plan §11 R4).

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result};
use tracing::{info, warn};

#[derive(Debug)]
pub struct ArtisanResult {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
}

pub fn planned_command(php_binary: &Path, args: &[&str]) -> String {
    format!("{} artisan {}", php_binary.display(), args.join(" "))
}

pub fn run(
    site_path: &Path,
    php_binary: &Path,
    args: &[&str],
    dry_run: bool,
    log_path: Option<&Path>,
    scan_env: Option<&(String, String)>,
) -> Result<ArtisanResult> {
    if dry_run {
        let planned = planned_command(php_binary, args);
        info!(planned = %planned, "dry-run artisan");
        return Ok(ArtisanResult {
            success: true,
            stdout: format!("[dry-run] {planned}"),
            stderr: String::new(),
            exit_code: Some(0),
        });
    }
    if !php_binary.exists() {
        anyhow::bail!(
            "PHP binary not found at {} — check Valet isolation / config.default_php",
            php_binary.display()
        );
    }

    let mut cmd = std::process::Command::new(php_binary);
    cmd.arg("artisan")
        .args(args)
        .current_dir(site_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some((key, value)) = scan_env {
        cmd.env(key, value);
    }

    let output = cmd.output().context("failed to spawn artisan")?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if let Some(log_path) = log_path
        && let Some(parent) = log_path.parent()
    {
        let _ = std::fs::create_dir_all(parent);
        let body = format!(
            "$ {}\n=== stdout ===\n{stdout}\n=== stderr ===\n{stderr}\n",
            planned_command(php_binary, args)
        );
        let _ = std::fs::write(log_path, body);
    }
    if !output.status.success() {
        warn!(args = ?args, exit = ?output.status.code(), "artisan failed");
    }
    Ok(ArtisanResult {
        success: output.status.success(),
        stdout,
        stderr,
        exit_code: output.status.code(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planned_command_formats_args() {
        let s = planned_command(Path::new("/usr/bin/php"), &["migrate", "--force"]);
        assert_eq!(s, "/usr/bin/php artisan migrate --force");
    }

    #[test]
    fn dry_run_does_not_invoke_command() {
        let r = run(
            Path::new("/nonexistent"),
            Path::new("/nonexistent-php"),
            &["telescope:install"],
            true,
            None,
            None,
        )
        .unwrap();
        assert!(r.success);
        assert!(r.stdout.contains("[dry-run]"));
        assert!(r.stdout.contains("artisan telescope:install"));
    }

    #[test]
    fn live_run_errors_when_php_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let php_path = tmp.path().join("php-fake");
        let err = run(tmp.path(), &php_path, &["migrate"], false, None, None).unwrap_err();
        assert!(err.to_string().contains("PHP binary not found"));
    }

    #[test]
    fn artisan_receives_scan_env() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let out = tmp.path().join("out");
        let php = tmp.path().join("php");
        std::fs::write(
            &php,
            format!("#!/bin/sh\nprintf %s \"$PHP_INI_SCAN_DIR\" > '{}'\n", out.display()),
        )
        .unwrap();
        std::fs::set_permissions(&php, std::fs::Permissions::from_mode(0o755)).unwrap();

        let pair = (
            "PHP_INI_SCAN_DIR".to_string(),
            ":/tmp/hearth/php/8.4/conf.d".to_string(),
        );
        let r = run(tmp.path(), &php, &["migrate"], false, None, Some(&pair)).unwrap();
        assert!(r.success, "fake php should exit 0: {r:?}");
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            ":/tmp/hearth/php/8.4/conf.d"
        );
    }
}
