use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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

/// Canonical global PHP INI store: `[php_ini.global]` plus sparse per-version
/// `[php_ini.overrides."X.Y"]` tables. Directive keys containing dots
/// (e.g. `opcache.enable`) are always TOML-quoted when serialized; manual
/// editors must quote them too or TOML expands them into nested tables.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PhpIniSettings {
    /// Directives applied to every PHP version.
    pub global: BTreeMap<String, String>,
    /// Sparse per-version overrides: `"8.4"` → directive map. Override wins
    /// over `global` per key.
    pub overrides: BTreeMap<String, BTreeMap<String, String>>,
}

impl PhpIniSettings {
    /// Effective directive map for one version: `global` ⊕ `overrides[version]`,
    /// override winning per key. BTreeMap keeps the result deterministic.
    pub fn effective_for(&self, version: &str) -> BTreeMap<String, String> {
        let mut effective = self.global.clone();
        if let Some(overrides) = self.overrides.get(version) {
            for (key, value) in overrides {
                effective.insert(key.clone(), value.clone());
            }
        }
        effective
    }
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

    /// Global + per-version PHP INI directives (see [`PhpIniSettings`]).
    pub php_ini: PhpIniSettings,
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
            php_ini: PhpIniSettings::default(),
        }
    }
}

impl HearthConfig {
    pub fn load() -> anyhow::Result<Self> {
        Self::load_from(&crate::config_dir().join("config.toml"))
    }

    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&crate::config_dir().join("config.toml"))
    }

    /// Save to a specific path. Atomic: same-dir exclusive temp file + rename,
    /// so a crash mid-save can never truncate an existing config.toml.
    ///
    /// The config can carry credentials (`ploi_api_token`), so the replacement
    /// preserves an existing target's Unix mode exactly and a brand-new config
    /// is created 0600 regardless of umask. No path — including error/retry
    /// paths — ever widens permissions.
    pub fn save_to(&self, path: &std::path::Path) -> anyhow::Result<()> {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => std::path::PathBuf::from("."),
        };
        std::fs::create_dir_all(&parent)?;
        let content = toml::to_string_pretty(self)?;

        // Existing target: keep its mode. New target: restrictive 0600.
        let target_mode = std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o7777)
            .unwrap_or(0o600);

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("config.toml");
        let mut attempt = 0u32;
        let (tmp_path, file) = loop {
            let candidate = parent.join(format!(
                ".{}.tmp.{}.{}",
                file_name,
                std::process::id(),
                attempt
            ));
            // Born 0600 (minus umask) — never wider than the final mode.
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&candidate)
            {
                Ok(file) => break (candidate, file),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt < 1000 => {
                    attempt += 1;
                }
                Err(e) => return Err(e.into()),
            }
        };

        let result =
            std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(target_mode))
                .map_err(Into::into)
                .and_then(|()| Self::write_and_rename(file, &content, &tmp_path, path));
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp_path);
        }
        result
    }

    fn write_and_rename(
        mut file: std::fs::File,
        content: &str,
        tmp_path: &std::path::Path,
        path: &std::path::Path,
    ) -> anyhow::Result<()> {
        use std::io::Write;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(tmp_path, path)?;
        Ok(())
    }

    /// Load from a specific path (useful for testing).
    pub fn load_from(path: &std::path::Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path)?;
        match toml::from_str(&content) {
            Ok(config) => Ok(config),
            Err(parse_err) => {
                if let Some(diagnosis) = Self::diagnose_unquoted_php_ini_key(&content) {
                    Err(anyhow::anyhow!("{diagnosis} (parse error: {parse_err})"))
                } else {
                    Err(parse_err.into())
                }
            }
        }
    }

    /// Detect the classic manual-edit mistake: an unquoted dotted directive key
    /// under `[php_ini.global]` or `[php_ini.overrides."X.Y"]`, which TOML
    /// expands into a nested table. Returns an actionable message naming the
    /// quoted form the user should write.
    fn diagnose_unquoted_php_ini_key(content: &str) -> Option<String> {
        fn find_nested(table: &toml::Value) -> Option<String> {
            for (key, val) in table.as_table()? {
                if val.is_table() {
                    let mut path = key.clone();
                    let mut inner = val;
                    while let Some(t) = inner.as_table() {
                        let (k, v) = t.iter().next()?;
                        path.push('.');
                        path.push_str(k);
                        inner = v;
                    }
                    return Some(path);
                }
            }
            None
        }

        let value: toml::Value = content.parse().ok()?;
        let php_ini = value.get("php_ini")?;
        let offending = php_ini.get("global").and_then(find_nested).or_else(|| {
            php_ini
                .get("overrides")?
                .as_table()?
                .values()
                .find_map(find_nested)
        })?;

        Some(format!(
            "config.toml: PHP INI directive keys containing dots must be TOML-quoted — \
             write \"{offending}\" = \"...\" instead of {offending} = \"...\" \
             (unquoted dotted keys become nested TOML tables)"
        ))
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
        std::fs::write(
            &config_path,
            r#"
tld = "test"
default_php = "8.4"
dns_port = 5354
dump_port = 9912
mail_smtp_port = 1025
mail_ui_port = 8025
mcp_port = 9900
parked_paths = []
"#,
        )
        .unwrap();

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
        std::fs::write(
            &config_path,
            r#"
tld = "test"
default_php = "8.4"
dns_port = 5354
dump_port = 9912
mail_smtp_port = 1025
mail_ui_port = 8025
parked_paths = []
"#,
        )
        .unwrap();

        let config = HearthConfig::load_from(&config_path).unwrap();
        assert_eq!(config.mcp_port, 9900); // should get default
        assert_eq!(config.tld, "test"); // existing fields preserved
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
        std::fs::write(
            &config_path,
            r#"
tld = "test"
default_php = "8.4"
dns_port = 5354
dump_port = 9912
mail_smtp_port = 1025
mail_ui_port = 8025
mcp_port = 9900
parked_paths = []
"#,
        )
        .unwrap();

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

    #[test]
    fn php_ini_defaults_empty() {
        let config = HearthConfig::default();
        assert!(config.php_ini.global.is_empty());
        assert!(config.php_ini.overrides.is_empty());
    }

    #[test]
    fn php_ini_round_trip_toml() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");

        let mut config = HearthConfig::default();
        config
            .php_ini
            .global
            .insert("memory_limit".to_string(), "1G".to_string());
        config
            .php_ini
            .global
            .insert("opcache.enable".to_string(), "1".to_string());
        config
            .php_ini
            .overrides
            .entry("8.4".to_string())
            .or_default()
            .insert("memory_limit".to_string(), "2G".to_string());

        config.save_to(&config_path).unwrap();
        let serialized = std::fs::read_to_string(&config_path).unwrap();
        // Dotted directive keys must serialize TOML-quoted, or they would
        // round-trip as nested tables instead of flat directive maps.
        assert!(
            serialized.lines().any(|l| l == r#""opcache.enable" = "1""#),
            "serialized config missing exact quoted dotted-key line:\n{serialized}"
        );

        let loaded = HearthConfig::load_from(&config_path).unwrap();
        assert_eq!(loaded.php_ini, config.php_ini);
    }

    #[test]
    fn quoted_dotted_key_fixture_deserializes() {
        // Hand-quoted fixture, as a manual editor following the README writes it.
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
tld = "test"
default_php = "8.4"

[php_ini.global]
memory_limit = "1G"
"opcache.enable" = "1"

[php_ini.overrides."8.4"]
memory_limit = "2G"
"#,
        )
        .unwrap();

        let config = HearthConfig::load_from(&config_path).unwrap();
        assert_eq!(
            config.php_ini.global.get("opcache.enable"),
            Some(&"1".to_string())
        );
        assert_eq!(
            config.php_ini.global.get("memory_limit"),
            Some(&"1G".to_string())
        );
        assert_eq!(
            config
                .php_ini
                .overrides
                .get("8.4")
                .and_then(|m| m.get("memory_limit")),
            Some(&"2G".to_string())
        );
    }

    #[test]
    fn unquoted_dotted_key_yields_actionable_error() {
        // Unquoted dotted key: TOML expands it into a nested table, which cannot
        // be a directive value. The error must tell the user how to fix it.
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
tld = "test"

[php_ini.global]
opcache.enable = "1"
"#,
        )
        .unwrap();

        let err = HearthConfig::load_from(&config_path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains(r#""opcache.enable""#),
            "error must name the quoted form of the offending key: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("quote"),
            "error must instruct the user to quote the key: {msg}"
        );
    }

    #[test]
    fn legacy_config_without_php_ini_loads() {
        // Verbatim v0.3.1-era config.toml — no [php_ini] table anywhere.
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
tld = "test"
default_php = "8.4"
dns_port = 5354
dump_port = 9912
mail_smtp_port = 1025
mail_ui_port = 8025
mcp_port = 9900
mysql_port = 3306
postgres_port = 5432
redis_port = 6379
parked_paths = []
composer_phar = "/opt/homebrew/bin/composer.phar"

[[added_packages]]
package = "horizon"
site_path = "/Users/me/Sites/shopfront"
site_name = "shopfront"
command = "/opt/homebrew/opt/php@8.4/bin/php"
args = ["artisan", "horizon"]
installed_at = "2026-05-20T12:00:00Z"
"#,
        )
        .unwrap();

        let config = HearthConfig::load_from(&config_path).unwrap();
        assert!(config.php_ini.global.is_empty());
        assert!(config.php_ini.overrides.is_empty());
        assert_eq!(config.added_packages.len(), 1);
        assert_eq!(config.default_php, "8.4");
    }

    #[test]
    fn effective_for_override_wins() {
        let mut ini = PhpIniSettings::default();
        ini.global
            .insert("memory_limit".to_string(), "1G".to_string());
        ini.global
            .insert("upload_max_filesize".to_string(), "8M".to_string());
        ini.overrides
            .entry("8.4".to_string())
            .or_default()
            .insert("memory_limit".to_string(), "2G".to_string());

        let effective = ini.effective_for("8.4");
        assert_eq!(effective.get("memory_limit"), Some(&"2G".to_string()));
        assert_eq!(
            effective.get("upload_max_filesize"),
            Some(&"8M".to_string())
        );
    }

    #[test]
    fn effective_for_falls_back_to_global() {
        let mut ini = PhpIniSettings::default();
        ini.global
            .insert("memory_limit".to_string(), "1G".to_string());
        ini.overrides
            .entry("8.4".to_string())
            .or_default()
            .insert("memory_limit".to_string(), "2G".to_string());

        let effective = ini.effective_for("8.1");
        assert_eq!(effective.get("memory_limit"), Some(&"1G".to_string()));
    }

    #[test]
    fn save_is_atomic_no_partial_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        // Pre-existing file must be replaced whole, never truncated in place.
        std::fs::write(&config_path, "tld = \"old\"").unwrap();

        let config = HearthConfig::default();
        config.save_to(&config_path).unwrap();

        // Temp file was renamed into place — no litter remains beside the config.
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("config.toml")]);

        let loaded = HearthConfig::load_from(&config_path).unwrap();
        assert_eq!(loaded.tld, "test");
    }

    #[test]
    fn save_preserves_restrictive_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        // Existing credential-bearing config locked down to 0600.
        std::fs::write(&config_path, "tld = \"old\"").unwrap();
        std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600)).unwrap();

        HearthConfig::default().save_to(&config_path).unwrap();

        let mode = std::fs::metadata(&config_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "atomic replacement must not widen 0600");
    }

    #[test]
    fn save_preserves_other_restrictive_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "tld = \"old\"").unwrap();
        std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o640)).unwrap();

        HearthConfig::default().save_to(&config_path).unwrap();

        let mode = std::fs::metadata(&config_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o640, "existing mode must be preserved exactly");
    }

    #[test]
    fn new_config_saved_group_world_unreadable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");

        HearthConfig::default().save_to(&config_path).unwrap();

        let mode = std::fs::metadata(&config_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "a new config may hold credentials — 0600 regardless of umask"
        );
    }
}
