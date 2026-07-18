pub mod engine;
pub mod ini_guard;
pub mod reconcile;
pub mod resolver;
pub mod targets;

use std::path::PathBuf;

use configparser::ini::Ini;
use tracing::info;

/// Supported PHP versions
pub const SUPPORTED_VERSIONS: &[&str] = &["7.4", "8.1", "8.2", "8.3", "8.4", "8.5"];

/// Manages PHP versions, binaries, and configuration.
pub struct PhpManager {
    config_dir: PathBuf,
}

impl PhpManager {
    pub fn new(config_dir: PathBuf) -> Self {
        Self { config_dir }
    }

    /// Get the php.ini path for a specific version.
    pub fn ini_path(&self, version: &str) -> PathBuf {
        self.config_dir
            .join("php")
            .join(version)
            .join("php.ini")
    }

    /// Read a value from php.ini for the given version.
    pub fn get_ini_value(&self, version: &str, key: &str) -> anyhow::Result<Option<String>> {
        let path = self.ini_path(version);
        if !path.exists() {
            return Ok(None);
        }
        let mut ini = Ini::new();
        ini.load(path.to_string_lossy().as_ref())
            .map_err(|e| anyhow::anyhow!("failed to parse php.ini: {}", e))?;

        // PHP INI values are typically in the unnamed section or "PHP"
        Ok(ini.get("PHP", key).or_else(|| ini.get("default", key)))
    }

    /// Set a value in php.ini for the given version.
    /// Uses configparser to preserve comments and formatting.
    pub fn set_ini_value(&self, version: &str, key: &str, value: &str) -> anyhow::Result<()> {
        let path = self.ini_path(version);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut ini = Ini::new();
        if path.exists() {
            ini.load(path.to_string_lossy().as_ref())
                .map_err(|e| anyhow::anyhow!("failed to parse php.ini: {}", e))?;
        }

        ini.set("PHP", key, Some(value.to_string()));
        ini.write(path.to_string_lossy().as_ref())
            .map_err(|e| anyhow::anyhow!("failed to write php.ini: {}", e))?;

        info!(version, key, value, "updated php.ini");
        Ok(())
    }

    /// Discover all installed PHP versions across all sources.
    ///
    /// Returns `(version, path)` pairs, scanning:
    /// 1. Hearth cache (`~/.config/hearth/php/*/php`)
    /// 2. Herd binaries (`~/Library/Application Support/Herd/bin/php*`)
    /// 3. Homebrew (`/opt/homebrew/opt/php@*/bin/php`)
    pub fn installed_versions_with_paths(&self) -> Vec<(String, std::path::PathBuf)> {
        use std::collections::BTreeMap;
        let mut found: BTreeMap<String, std::path::PathBuf> = BTreeMap::new();

        // 1. Hearth cache (highest priority)
        let php_dir = self.config_dir.join("php");
        if let Ok(entries) = std::fs::read_dir(&php_dir) {
            for entry in entries.flatten() {
                let bin = entry.path().join("php");
                if bin.exists()
                    && let Some(name) = entry.file_name().to_str()
                {
                    found.insert(name.to_string(), bin);
                }
            }
        }

        // 2. Herd binaries
        if let Some(herd_bin) = dirs::home_dir()
            .map(|h| h.join("Library/Application Support/Herd/bin"))
            && let Ok(entries) = std::fs::read_dir(&herd_bin)
        {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                // Match "php84", "php83", etc. (not "php84-fpm")
                if let Some(digits) = name.strip_prefix("php")
                    && digits.len() == 2
                    && digits.chars().all(|c| c.is_ascii_digit())
                    && entry.path().is_file()
                {
                    let version = format!("{}.{}", &digits[..1], &digits[1..]);
                    found.entry(version).or_insert_with(|| entry.path());
                }
            }
        }

        // 3. Homebrew
        let brew_opt = std::path::Path::new("/opt/homebrew/opt");
        if let Ok(entries) = std::fs::read_dir(brew_opt) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if let Some(version) = name.strip_prefix("php@") {
                    let bin = entry.path().join("bin/php");
                    if bin.exists() {
                        found.entry(version.to_string()).or_insert(bin);
                    }
                }
            }
        }

        found.into_iter().collect()
    }

    /// List installed PHP versions (version strings only).
    pub fn installed_versions(&self) -> Vec<String> {
        self.installed_versions_with_paths()
            .into_iter()
            .map(|(v, _)| v)
            .collect()
    }
}

/// Belt-E env pair for Hearth-launched PHP processes: the leading colon means
/// "compiled-in default scan dir first, then ours", so Hearth's `zz-hearth.ini`
/// values win on conflict. Only credited to binaries whose env-honor canary
/// passed — Herd-patched binaries ignore this variable whenever HOME is set.
pub fn scan_dir_env(config_dir: &std::path::Path, version: &str) -> (String, String) {
    (
        "PHP_INI_SCAN_DIR".to_string(),
        format!(
            ":{}",
            config_dir
                .join("php")
                .join(version)
                .join("conf.d")
                .display()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_dir_env_value_is_colon_prefixed() {
        let (key, value) = scan_dir_env(std::path::Path::new("/tmp/hearth config"), "8.4");
        assert_eq!(key, "PHP_INI_SCAN_DIR");
        assert_eq!(value, ":/tmp/hearth config/php/8.4/conf.d");
        assert!(
            value.starts_with(':'),
            "leading colon appends after default scan dir"
        );
    }

    #[test]
    fn installed_versions_includes_hearth_cache() {
        let tmp = tempfile::TempDir::new().unwrap();
        let php_dir = tmp.path().join("php");

        // Create version directories with php binary
        std::fs::create_dir_all(php_dir.join("8.3")).unwrap();
        std::fs::write(php_dir.join("8.3/php"), "").unwrap();
        std::fs::create_dir_all(php_dir.join("8.4")).unwrap();
        std::fs::write(php_dir.join("8.4/php"), "").unwrap();

        let manager = PhpManager::new(tmp.path().to_path_buf());
        let versions = manager.installed_versions();

        // Should include our mock versions (may also include system Herd/Homebrew)
        assert!(versions.contains(&"8.3".to_string()));
        assert!(versions.contains(&"8.4".to_string()));
    }

    #[test]
    fn hearth_cache_dir_without_binary_excluded() {
        let tmp = tempfile::TempDir::new().unwrap();
        let php_dir = tmp.path().join("php");

        // Dir without php binary should not appear from Hearth cache
        std::fs::create_dir_all(php_dir.join("99.9")).unwrap();
        // No php binary inside

        let manager = PhpManager::new(tmp.path().to_path_buf());
        let versions = manager.installed_versions();

        assert!(!versions.contains(&"99.9".to_string()));
    }

    #[test]
    fn hearth_cache_takes_priority_over_later_sources() {
        let tmp = tempfile::TempDir::new().unwrap();
        let php_dir = tmp.path().join("php");

        // Hearth cache entry
        std::fs::create_dir_all(php_dir.join("8.4")).unwrap();
        std::fs::write(php_dir.join("8.4/php"), "hearth-version").unwrap();

        let manager = PhpManager::new(tmp.path().to_path_buf());
        let versions_with_paths = manager.installed_versions_with_paths();

        // The 8.4 entry should point to our Hearth cache, not Herd/Homebrew
        if let Some((_, path)) = versions_with_paths.iter().find(|(v, _)| v == "8.4") {
            assert!(
                path.starts_with(tmp.path()),
                "8.4 should resolve from Hearth cache, got: {path:?}"
            );
        }
    }
}
