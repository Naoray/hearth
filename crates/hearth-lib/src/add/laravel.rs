//! Laravel-app detection helpers used by recipes + site_context.
//!
//! Operates on a site root directory; reads `composer.json` and optionally
//! `vendor/composer/installed.json` for the exact installed Laravel version.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

/// Read the `require.laravel/framework` constraint from `composer.json`.
pub fn framework_constraint(site_path: &Path) -> Result<String> {
    let composer_path = site_path.join("composer.json");
    let content = std::fs::read_to_string(&composer_path)
        .with_context(|| format!("composer.json not found at {}", composer_path.display()))?;
    let value: Value = serde_json::from_str(&content).context("composer.json is not valid JSON")?;
    value
        .get("require")
        .and_then(|r| r.get("laravel/framework"))
        .and_then(|c| c.as_str())
        .map(|s| s.to_string())
        .with_context(|| {
            format!(
                "{}: require.laravel/framework missing — not a Laravel app",
                composer_path.display()
            )
        })
}

/// True when `composer.json` lists `package` in `require` or `require-dev`.
pub fn package_required(site_path: &Path, package: &str) -> Result<bool> {
    let composer_path = site_path.join("composer.json");
    if !composer_path.exists() {
        return Ok(false);
    }
    let content = std::fs::read_to_string(&composer_path)?;
    let value: Value = serde_json::from_str(&content).context("composer.json is not valid JSON")?;
    let in_require = value
        .get("require")
        .and_then(|r| r.get(package))
        .is_some();
    let in_require_dev = value
        .get("require-dev")
        .and_then(|r| r.get(package))
        .is_some();
    Ok(in_require || in_require_dev)
}

/// True when `<site_path>/vendor/<package>` exists on disk.
pub fn package_installed(site_path: &Path, package: &str) -> bool {
    site_path.join("vendor").join(package).exists()
}

/// Look up the exact installed `laravel/framework` version from
/// `vendor/composer/installed.json`. Returns `None` if vendor is missing or the file
/// has no entry for laravel/framework.
pub fn installed_framework_version(site_path: &Path) -> Option<String> {
    let installed_path = site_path.join("vendor/composer/installed.json");
    let content = std::fs::read_to_string(&installed_path).ok()?;
    let value: Value = serde_json::from_str(&content).ok()?;
    // Composer 2.x: { "packages": [...] }; Composer 1.x: top-level array.
    let pkgs = value
        .get("packages")
        .and_then(|p| p.as_array())
        .or_else(|| value.as_array())?;
    for pkg in pkgs {
        if pkg.get("name").and_then(|n| n.as_str()) == Some("laravel/framework") {
            return pkg
                .get("version_normalized")
                .or_else(|| pkg.get("version"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
        }
    }
    None
}

/// Extract the leading major version digits from a composer constraint like `"^11.0"`,
/// `"~10.43.0"`, `">=11.0"`. Returns `None` on unparseable constraints (e.g. `dev-main`).
pub fn constraint_major_floor(constraint: &str) -> Option<u32> {
    let stripped = constraint.trim().trim_start_matches(|c: char| !c.is_ascii_digit());
    let digits: String = stripped.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_composer(dir: &Path, body: &str) {
        std::fs::write(dir.join("composer.json"), body).unwrap();
    }

    #[test]
    fn framework_constraint_reads_require_key() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_composer(
            tmp.path(),
            r#"{"require":{"laravel/framework":"^11.0"}}"#,
        );
        let c = framework_constraint(tmp.path()).unwrap();
        assert_eq!(c, "^11.0");
    }

    #[test]
    fn framework_constraint_errors_when_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_composer(tmp.path(), r#"{"require":{"php":"^8.2"}}"#);
        assert!(framework_constraint(tmp.path()).is_err());
    }

    #[test]
    fn package_required_checks_require_and_dev() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_composer(
            tmp.path(),
            r#"{"require":{"laravel/framework":"^11.0"},"require-dev":{"laravel/telescope":"^5.0"}}"#,
        );
        assert!(package_required(tmp.path(), "laravel/framework").unwrap());
        assert!(package_required(tmp.path(), "laravel/telescope").unwrap());
        assert!(!package_required(tmp.path(), "laravel/horizon").unwrap());
    }

    #[test]
    fn package_installed_checks_vendor_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("vendor/laravel/horizon")).unwrap();
        assert!(package_installed(tmp.path(), "laravel/horizon"));
        assert!(!package_installed(tmp.path(), "laravel/telescope"));
    }

    #[test]
    fn installed_framework_version_parses_composer2_layout() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("vendor/composer")).unwrap();
        std::fs::write(
            tmp.path().join("vendor/composer/installed.json"),
            r#"{"packages":[{"name":"laravel/framework","version":"v11.10.0","version_normalized":"11.10.0.0"}]}"#,
        )
        .unwrap();
        assert_eq!(
            installed_framework_version(tmp.path()),
            Some("11.10.0.0".to_string())
        );
    }

    #[test]
    fn installed_framework_version_returns_none_when_vendor_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(installed_framework_version(tmp.path()).is_none());
    }

    #[test]
    fn constraint_major_floor_parses_caret() {
        assert_eq!(constraint_major_floor("^11.0"), Some(11));
        assert_eq!(constraint_major_floor("~10.43.0"), Some(10));
        assert_eq!(constraint_major_floor(">=12.0"), Some(12));
    }

    #[test]
    fn constraint_major_floor_returns_none_on_dev_branches() {
        assert_eq!(constraint_major_floor("dev-main"), None);
        assert_eq!(constraint_major_floor("dev-master"), None);
    }
}
