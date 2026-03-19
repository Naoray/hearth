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

    /// List installed PHP versions (those with binaries in the config dir).
    pub fn installed_versions(&self) -> Vec<String> {
        let php_dir = self.config_dir.join("php");
        if !php_dir.exists() {
            return Vec::new();
        }

        let mut versions = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&php_dir) {
            for entry in entries.flatten() {
                if entry.path().join("php").exists() {
                    if let Some(name) = entry.file_name().to_str() {
                        versions.push(name.to_string());
                    }
                }
            }
        }
        versions.sort();
        versions
    }
}
