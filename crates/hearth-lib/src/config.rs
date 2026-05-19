use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A Laravel package installed via `hearth add` and (for Horizon/Reverb) supervised
/// as a long-running worker. Persisted in `HearthConfig.added_packages` so the daemon
/// re-registers it on every restart.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AddedPackage {
    /// Package key — `"horizon" | "telescope" | "pulse" | "reverb"`.
    pub package: String,
    /// Absolute path to the Laravel site root.
    pub site_path: PathBuf,
    /// Site name (mirrors `Site.name`) — used to disambiguate per-site supervised
    /// rows in `hearth status` (e.g. `horizon[shopfront]`).
    pub site_name: String,
    /// Resolved PHP binary used to run the supervised worker.
    /// Empty string for install-only packages (Telescope, Pulse).
    #[serde(default)]
    pub command: String,
    /// Args passed to the PHP binary (e.g. `["artisan", "horizon"]`).
    /// Empty for install-only packages.
    #[serde(default)]
    pub args: Vec<String>,
    /// When the package was installed.
    pub installed_at: chrono::DateTime<chrono::Utc>,
}

/// Global Hearth configuration, stored at ~/.config/hearth/config.toml
#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
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

    /// Port for the MCP (Model Context Protocol) server
    pub mcp_port: u16,

    /// Port for MySQL/MariaDB (default: 3306)
    pub mysql_port: u16,

    /// Port for Postgres (default: 5432)
    pub postgres_port: u16,

    /// Port for Redis (default: 6379)
    pub redis_port: u16,

    /// Paths to parked directories
    pub parked_paths: Vec<PathBuf>,

    /// Absolute path to a `composer.phar` Hearth invokes via the site's resolved PHP
    /// binary. Resolved at `hearth install` time; avoids Composer's bash wrapper
    /// (which uses the system PHP and breaks Valet-isolated sites).
    pub composer_phar: Option<PathBuf>,

    /// Laravel packages installed via `hearth add`. Horizon/Reverb entries also become
    /// supervised services on daemon boot. Boot-time prune drops entries whose
    /// `<site_path>/vendor/<package>` no longer exists.
    pub added_packages: Vec<AddedPackage>,
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
            mcp_port: 9900,
            mysql_port: 3306,
            postgres_port: 5432,
            redis_port: 6379,
            parked_paths: Vec::new(),
            composer_phar: None,
            added_packages: Vec::new(),
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

    #[test]
    fn default_config_has_mcp_port() {
        let config = HearthConfig::default();
        assert_eq!(config.mcp_port, 9900);
    }

    #[test]
    fn default_config_has_db_ports() {
        let config = HearthConfig::default();
        assert_eq!(config.mysql_port, 3306);
        assert_eq!(config.postgres_port, 5432);
        assert_eq!(config.redis_port, 6379);
    }

    #[test]
    fn load_legacy_config_without_db_fields() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        // Pre-DB v0.2.x config — mcp_port present, no db_* keys.
        std::fs::write(&config_path, r#"
tld = "test"
default_php = "8.4"
dns_port = 5354
dump_port = 9912
mail_smtp_port = 1025
mail_ui_port = 8025
mcp_port = 9900
parked_paths = []
"#).unwrap();

        let config = HearthConfig::load_from(&config_path).unwrap();
        assert_eq!(config.mysql_port, 3306);
        assert_eq!(config.postgres_port, 5432);
        assert_eq!(config.redis_port, 6379);
        assert_eq!(config.tld, "test");
        assert_eq!(config.default_php, "8.4");
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

    #[test]
    fn default_config_has_no_added_packages() {
        let config = HearthConfig::default();
        assert!(config.added_packages.is_empty());
        assert!(config.composer_phar.is_none());
    }

    #[test]
    fn load_legacy_config_without_added_packages() {
        // Forward-compat: a config written before v0.3.0 must still load.
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, r#"
tld = "test"
default_php = "8.4"
dns_port = 5354
dump_port = 9912
mail_smtp_port = 1025
mail_ui_port = 8025
mcp_port = 9900
parked_paths = []
"#).unwrap();

        let config = HearthConfig::load_from(&config_path).unwrap();
        assert!(config.added_packages.is_empty());
        assert!(config.composer_phar.is_none());
    }

    #[test]
    fn added_packages_round_trip_serde() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");

        let mut config = HearthConfig::default();
        config.composer_phar = Some(PathBuf::from("/opt/homebrew/bin/composer.phar"));
        config.added_packages.push(AddedPackage {
            package: "horizon".to_string(),
            site_path: PathBuf::from("/Users/me/Sites/shopfront"),
            site_name: "shopfront".to_string(),
            command: "/opt/homebrew/opt/php@8.4/bin/php".to_string(),
            args: vec!["artisan".to_string(), "horizon".to_string()],
            installed_at: chrono::Utc::now(),
        });

        config.save_to(&config_path).unwrap();
        let loaded = HearthConfig::load_from(&config_path).unwrap();

        assert_eq!(loaded.added_packages.len(), 1);
        assert_eq!(loaded.added_packages[0].package, "horizon");
        assert_eq!(loaded.added_packages[0].site_name, "shopfront");
        assert_eq!(loaded.added_packages[0].args, vec!["artisan", "horizon"]);
        assert_eq!(
            loaded.composer_phar,
            Some(PathBuf::from("/opt/homebrew/bin/composer.phar"))
        );
    }
}
