use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Global Hearth configuration, stored at ~/.config/hearth/config.toml
#[derive(Debug, Serialize, Deserialize)]
pub struct HearthConfig {
    /// Top-level domain for sites (default: "test")
    pub tld: String,

    /// Default PHP version (e.g., "8.4")
    pub default_php: String,

    /// Port for dnsmasq (unprivileged, default: 5354)
    pub dns_port: u16,

    /// Port for the dump server relay
    pub dump_port: u16,

    /// Port for Mailpit SMTP
    pub mail_smtp_port: u16,

    /// Port for Mailpit web UI
    pub mail_ui_port: u16,

    /// Ploi API token (optional)
    pub ploi_api_token: Option<String>,

    /// Paths to parked directories
    pub parked_paths: Vec<PathBuf>,
}

impl Default for HearthConfig {
    fn default() -> Self {
        Self {
            tld: "test".to_string(),
            default_php: "8.4".to_string(),
            dns_port: 5354,
            dump_port: 9912,
            mail_smtp_port: 1025,
            mail_ui_port: 8025,
            ploi_api_token: None,
            parked_paths: Vec::new(),
        }
    }
}

impl HearthConfig {
    pub fn load() -> anyhow::Result<Self> {
        let path = crate::config_dir().join("config.toml");
        if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            Ok(toml::from_str(&content)?)
        } else {
            Ok(Self::default())
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = crate::config_dir().join("config.toml");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        std::fs::write(&path, content)?;
        Ok(())
    }

    /// Save to a specific path (useful for testing).
    pub fn save_to(&self, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Load from a specific path (useful for testing).
    pub fn load_from(path: &std::path::Path) -> anyhow::Result<Self> {
        if path.exists() {
            let content = std::fs::read_to_string(path)?;
            Ok(toml::from_str(&content)?)
        } else {
            Ok(Self::default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_sane_values() {
        let config = HearthConfig::default();
        assert_eq!(config.tld, "test");
        assert_eq!(config.default_php, "8.4");
        assert_eq!(config.dns_port, 5354);
        assert_eq!(config.dump_port, 9912);
        assert!(config.parked_paths.is_empty());
    }

    #[test]
    fn round_trip_save_load() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");

        let mut config = HearthConfig {
            default_php: "8.3".to_string(),
            ..HearthConfig::default()
        };
        config.parked_paths.push(PathBuf::from("/home/sites"));

        config.save_to(&config_path).unwrap();
        let loaded = HearthConfig::load_from(&config_path).unwrap();

        assert_eq!(loaded.default_php, "8.3");
        assert_eq!(loaded.parked_paths, vec![PathBuf::from("/home/sites")]);
        assert_eq!(loaded.tld, "test");
    }

    #[test]
    fn load_missing_file_returns_defaults() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("nonexistent.toml");

        let config = HearthConfig::load_from(&config_path).unwrap();
        assert_eq!(config.tld, "test");
        assert_eq!(config.default_php, "8.4");
    }
}
