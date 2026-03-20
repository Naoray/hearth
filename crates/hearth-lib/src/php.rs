pub mod resolver;

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
