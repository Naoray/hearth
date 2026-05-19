//! MySQL/MariaDB engine: resolver + flavor branch + wrapper-script builder.
//!
//! Per Day-1 spike (`scripts/spike-results/SPIKE_NOTES.md`):
//!
//! - MariaDB 11.x does NOT accept `--initialize-insecure`. Init MUST invoke
//!   `<basedir>/scripts/mariadb-install-db --datadir=... --basedir=...
//!   --auth-root-authentication-method=normal`.
//! - Real MySQL 8/9 init: `mysqld --initialize-insecure --datadir=<data>`.
//! - Runtime args are MySQL-wire-compatible across both flavors.
//!
//! Resolver chain (binary name → flavor):
//!   1. Hearth cache `services/mysql/bin/{mysqld,mariadbd}`
//!   2. Herd `~/Library/Application Support/Herd/bin/{mysqld,mariadbd}`
//!   3. Homebrew `/opt/homebrew/opt/{mysql,mariadb}/bin/...`
//!   4. Homebrew (Intel) `/usr/local/opt/{mysql,mariadb}/bin/...`
//!
//! Resolver canonicalizes symlinks (Herd uses `bin/mariadbd → /Users/Shared/.../bin/mariadbd`)
//! so `basedir = canonical_bin.parent().parent()` lands on the actual install root.

use std::path::{Path, PathBuf};

use tracing::info;

use crate::config::HearthConfig;
use crate::db::init::{shell_quote, write_wrapper_script, WrapperSpec};
use crate::service::supervisor::{ManagedService, ShutdownStrategy};
use crate::service::ServiceKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MysqlFlavor {
    Mysql,
    MariaDB,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MysqlBinaries {
    /// Path to mysqld or mariadbd (preserves caller-visible path; may be a symlink).
    pub binary: PathBuf,
    /// Canonical install root (canonical_binary.parent().parent()). Needed to
    /// locate `scripts/mariadb-install-db`.
    pub basedir: PathBuf,
    pub flavor: MysqlFlavor,
}

/// Production resolver. Uses real Homebrew + Herd prefixes and the real
/// `<binary> --version` probe.
pub fn resolve_mysql_binaries(config_dir: &Path) -> Option<MysqlBinaries> {
    let herd = dirs::home_dir()
        .map(|h| h.join("Library/Application Support/Herd"))
        .unwrap_or_else(|| PathBuf::from("/nonexistent"));
    resolve_mysql_binaries_in(
        config_dir,
        &herd,
        &[
            PathBuf::from("/opt/homebrew/opt"),
            PathBuf::from("/usr/local/opt"),
        ],
        detect_flavor,
    )
}

pub(crate) fn resolve_mysql_binaries_in(
    config_dir: &Path,
    herd_root: &Path,
    homebrew_prefixes: &[PathBuf],
    flavor_probe: impl Fn(&Path) -> MysqlFlavor,
) -> Option<MysqlBinaries> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    // 1. Hearth cache (real MySQL preferred over MariaDB if both shipped).
    candidates.push(config_dir.join("services/mysql/bin/mysqld"));
    candidates.push(config_dir.join("services/mysql/bin/mariadbd"));
    // 2. Herd ships both names; mysqld may be a dangling symlink (spike).
    candidates.push(herd_root.join("bin/mysqld"));
    candidates.push(herd_root.join("bin/mariadbd"));
    // 3 & 4. Homebrew prefixes.
    for prefix in homebrew_prefixes {
        candidates.push(prefix.join("mysql/bin/mysqld"));
        candidates.push(prefix.join("mariadb/bin/mysqld"));
        candidates.push(prefix.join("mariadb/bin/mariadbd"));
    }

    for cand in candidates {
        // `.exists()` follows symlinks. Dangling symlink (Herd's broken
        // bin/mysqld) returns false here — exactly what we want.
        if !cand.exists() {
            continue;
        }
        let canonical = std::fs::canonicalize(&cand).unwrap_or_else(|_| cand.clone());
        let basedir = canonical
            .parent()
            .and_then(|p| p.parent())
            .map(PathBuf::from)
            .unwrap_or_else(|| canonical.clone());
        let flavor = flavor_probe(&cand);
        info!(
            path = %cand.display(),
            basedir = %basedir.display(),
            flavor = ?flavor,
            "resolved mysqld/mariadbd"
        );
        return Some(MysqlBinaries {
            binary: cand,
            basedir,
            flavor,
        });
    }
    None
}

/// Production flavor probe. Binary name carries strong signal; fall back to
/// running `--version` if name is ambiguous (and tolerate execution failure).
pub fn detect_flavor(binary: &Path) -> MysqlFlavor {
    if binary
        .file_name()
        .and_then(|n| n.to_str())
        .map_or(false, |n| n.eq_ignore_ascii_case("mariadbd"))
    {
        return MysqlFlavor::MariaDB;
    }
    match std::process::Command::new(binary).arg("--version").output() {
        Ok(out) => {
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            if combined.to_lowercase().contains("mariadb") {
                MysqlFlavor::MariaDB
            } else {
                MysqlFlavor::Mysql
            }
        }
        Err(_) => MysqlFlavor::Mysql,
    }
}

pub fn managed_service(
    bins: &MysqlBinaries,
    config: &HearthConfig,
    config_dir: &Path,
) -> anyhow::Result<ManagedService> {
    let data_dir = config_dir.join("data/mysql");
    let run_dir = config_dir.join("run");
    let log_dir = config_dir.join("log");
    std::fs::create_dir_all(&log_dir)?;

    let init_command = match bins.flavor {
        MysqlFlavor::MariaDB => {
            let install_db = bins.basedir.join("scripts/mariadb-install-db");
            format!(
                "{install} --datadir={data} --basedir={basedir} --auth-root-authentication-method=normal",
                install = shell_quote(&install_db),
                data = shell_quote(&data_dir),
                basedir = shell_quote(&bins.basedir),
            )
        }
        MysqlFlavor::Mysql => format!(
            "{bin} --initialize-insecure --datadir={data}",
            bin = shell_quote(&bins.binary),
            data = shell_quote(&data_dir),
        ),
    };

    let socket = run_dir.join("mysql.sock");
    let pidfile = run_dir.join("mysql.pid");
    let errlog = log_dir.join("mysql.err");

    let runtime_args: Vec<String> = vec![
        format!("--datadir={}", data_dir.display()),
        format!("--socket={}", socket.display()),
        format!("--port={}", config.mysql_port),
        "--bind-address=127.0.0.1".to_string(),
        format!("--pid-file={}", pidfile.display()),
        format!("--log-error={}", errlog.display()),
        // --skip-name-resolve avoids the 100ms-on-first-connect PTR lookup
        // and tightens attack surface against accidental misconfig.
        "--skip-name-resolve".to_string(),
    ];

    let wrapper_path = write_wrapper_script(&WrapperSpec {
        engine: "mysql",
        engine_binary: &bins.binary,
        engine_args: &runtime_args,
        data_dir: &data_dir,
        run_dir: &run_dir,
        init_command,
        // mysqld does not require 0700 like postgres does; leave default umask.
        dir_mode: None,
    })?;

    let svc = ManagedService::new(
        ServiceKind::Mysql,
        wrapper_path.display().to_string(),
        vec![],
    )
    // Per orchestrator brief P0 #4: 20s grace prevents SIGKILL during InnoDB
    // flush from shredding ib_logfile.
    .with_shutdown_strategy(ShutdownStrategy::LongGrace { grace_ms: 20_000 });
    Ok(svc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_fake(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "fake").unwrap();
    }

    #[test]
    fn detect_flavor_mariadbd_by_name() {
        // Real probe is skipped when filename matches mariadbd.
        let tmp = tempfile::TempDir::new().unwrap();
        let p = tmp.path().join("mariadbd");
        std::fs::write(&p, "").unwrap();
        assert_eq!(detect_flavor(&p), MysqlFlavor::MariaDB);
    }

    #[test]
    fn resolver_prefers_hearth_cache() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_fake(&tmp.path().join("services/mysql/bin/mysqld"));
        let r = resolve_mysql_binaries_in(
            tmp.path(),
            &tmp.path().join("nonexistent-herd"),
            &[],
            |_| MysqlFlavor::Mysql,
        );
        let r = r.expect("cache should resolve");
        assert_eq!(r.flavor, MysqlFlavor::Mysql);
        assert!(r.binary.ends_with("services/mysql/bin/mysqld"));
    }

    #[test]
    fn resolver_falls_back_to_herd_mariadbd() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = tmp.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        let herd = tmp.path().join("Herd");
        write_fake(&herd.join("bin/mariadbd"));

        let r = resolve_mysql_binaries_in(&config, &herd, &[], |_| MysqlFlavor::MariaDB)
            .expect("herd mariadbd should resolve");
        assert_eq!(r.flavor, MysqlFlavor::MariaDB);
        assert!(r.binary.ends_with("Herd/bin/mariadbd"));
    }

    #[test]
    fn resolver_skips_dangling_herd_mysqld_symlink() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = tmp.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        let herd = tmp.path().join("Herd");
        std::fs::create_dir_all(herd.join("bin")).unwrap();

        // Dangling symlink — points at a nonexistent target.
        #[cfg(unix)]
        std::os::unix::fs::symlink("/nonexistent/mysqld", herd.join("bin/mysqld")).unwrap();
        // mariadbd is real — should win.
        write_fake(&herd.join("bin/mariadbd"));

        let r = resolve_mysql_binaries_in(&config, &herd, &[], |_| MysqlFlavor::MariaDB)
            .expect("mariadbd should resolve when mysqld dangles");
        assert!(
            r.binary.ends_with("Herd/bin/mariadbd"),
            "dangling mysqld must be skipped, got: {}",
            r.binary.display()
        );
    }

    #[test]
    fn resolver_returns_none_when_nothing_installed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let r = resolve_mysql_binaries_in(
            tmp.path(),
            &tmp.path().join("none"),
            &[],
            |_| MysqlFlavor::Mysql,
        );
        assert!(r.is_none());
    }

    #[test]
    fn mariadb_wrapper_uses_mariadb_install_db() {
        let tmp = tempfile::TempDir::new().unwrap();
        let basedir = tmp.path().join("Herd/services/mariadb/11.8.6");
        write_fake(&basedir.join("bin/mariadbd"));
        std::fs::create_dir_all(basedir.join("scripts")).unwrap();
        // Doesn't need to be executable for the wrapper test — we only check
        // that the wrapper invokes it with the right args.
        std::fs::write(basedir.join("scripts/mariadb-install-db"), "#!/bin/sh\n").unwrap();

        let bins = MysqlBinaries {
            binary: basedir.join("bin/mariadbd"),
            basedir: basedir.clone(),
            flavor: MysqlFlavor::MariaDB,
        };
        let mut config = HearthConfig::default();
        config.mysql_port = 3399;

        let svc = managed_service(&bins, &config, tmp.path()).unwrap();
        assert_eq!(svc.kind, ServiceKind::Mysql);
        assert!(matches!(
            svc.shutdown_strategy,
            ShutdownStrategy::LongGrace { grace_ms: 20_000 }
        ));

        let wrapper = tmp.path().join("run/init-mysql.sh");
        let content = std::fs::read_to_string(&wrapper).unwrap();
        assert!(
            content.contains("scripts/mariadb-install-db"),
            "MariaDB init must use mariadb-install-db, got:\n{content}"
        );
        assert!(content.contains("--auth-root-authentication-method=normal"));
        assert!(content.contains("--basedir="));
        assert!(content.contains("3399"));
        assert!(content.contains("--skip-name-resolve"));
    }

    #[test]
    fn mysql_wrapper_uses_initialize_insecure() {
        let tmp = tempfile::TempDir::new().unwrap();
        let basedir = tmp.path().join("homebrew/mysql");
        write_fake(&basedir.join("bin/mysqld"));

        let bins = MysqlBinaries {
            binary: basedir.join("bin/mysqld"),
            basedir,
            flavor: MysqlFlavor::Mysql,
        };
        let config = HearthConfig::default();

        let _svc = managed_service(&bins, &config, tmp.path()).unwrap();
        let wrapper = tmp.path().join("run/init-mysql.sh");
        let content = std::fs::read_to_string(&wrapper).unwrap();
        assert!(
            content.contains("--initialize-insecure"),
            "real MySQL init must use --initialize-insecure"
        );
        assert!(
            !content.contains("mariadb-install-db"),
            "MySQL init must NOT invoke mariadb-install-db"
        );
    }
}
