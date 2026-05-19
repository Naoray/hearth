//! Postgres engine: resolver chain + `ManagedService` builder.
//!
//! Resolver order:
//!   1. Hearth cache: `<config_dir>/services/postgresql/bin/{postgres,initdb,pg_ctl}`
//!   2. Homebrew: `/opt/homebrew/opt/postgresql@N/bin/...`, highest N wins
//!   3. Homebrew (Intel): `/usr/local/opt/postgresql@N/bin/...`, highest N wins
//!
//! All three binaries must come from the same install. The resolver also
//! captures `pg_ctl` for the `ShutdownStrategy::Postgres { pg_ctl_binary, .. }`
//! path used by the supervisor.
//!
//! Init flow (per Day-1 spike + plan §4):
//!   `initdb -D <data> --auth-host=trust --auth-local=trust -U <user> --encoding=UTF8 --locale=C`
//! The `--locale=C` is mandatory to dodge "encoding UTF8 does not match
//! locale's encoding (US-ASCII)" on stock macOS.

use std::path::{Path, PathBuf};

use tracing::info;

use crate::config::HearthConfig;
use crate::db::init::{shell_quote, shell_quote_str, write_wrapper_script, WrapperSpec};
use crate::service::supervisor::{ManagedService, ShutdownStrategy};
use crate::service::ServiceKind;

/// Resolved Postgres binaries from a single install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostgresBinaries {
    pub postgres: PathBuf,
    pub initdb: PathBuf,
    pub pg_ctl: PathBuf,
    /// Numeric major version (e.g. "17") or "cache" / "unknown".
    pub version: String,
}

/// Production resolver — probes Hearth cache, then Homebrew on both prefixes.
pub fn resolve_postgres_binaries(config_dir: &Path) -> Option<PostgresBinaries> {
    resolve_postgres_binaries_in(
        config_dir,
        &[
            PathBuf::from("/opt/homebrew/opt"),
            PathBuf::from("/usr/local/opt"),
        ],
    )
}

/// Injectable resolver used in tests; production wrapping fn supplies the real
/// Homebrew prefix list.
pub(crate) fn resolve_postgres_binaries_in(
    config_dir: &Path,
    homebrew_prefixes: &[PathBuf],
) -> Option<PostgresBinaries> {
    // 1. Hearth cache (single-version layout — plan §3).
    let cache_bin = config_dir.join("services/postgresql/bin");
    if let Some(p) = bins_in(&cache_bin, "cache") {
        info!(path = %p.postgres.display(), "resolved Postgres from Hearth cache");
        return Some(p);
    }

    // 2. Homebrew: glob postgresql@N, pick highest N.
    for prefix in homebrew_prefixes {
        for (version_num, bin_dir) in glob_postgres_bins(prefix) {
            if let Some(p) = bins_in(&bin_dir, &version_num.to_string()) {
                info!(
                    path = %p.postgres.display(),
                    version = %p.version,
                    "resolved Postgres from Homebrew"
                );
                return Some(p);
            }
        }
    }

    None
}

fn bins_in(bin_dir: &Path, version: &str) -> Option<PostgresBinaries> {
    let postgres = bin_dir.join("postgres");
    let initdb = bin_dir.join("initdb");
    let pg_ctl = bin_dir.join("pg_ctl");
    if postgres.exists() && initdb.exists() && pg_ctl.exists() {
        Some(PostgresBinaries {
            postgres,
            initdb,
            pg_ctl,
            version: version.to_string(),
        })
    } else {
        None
    }
}

/// Read `prefix` for `postgresql@N` entries; return `(version, bin/)` sorted
/// highest-version-first.
fn glob_postgres_bins(prefix: &Path) -> Vec<(u32, PathBuf)> {
    let mut out: Vec<(u32, PathBuf)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(prefix) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if let Some(rest) = name_str.strip_prefix("postgresql@") {
            if let Ok(v) = rest.parse::<u32>() {
                let bin = entry.path().join("bin");
                if bin.exists() {
                    out.push((v, bin));
                }
            }
        }
    }
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out
}

/// Build a `ManagedService` for Postgres backed by an `init-postgresql.sh`
/// wrapper script.
pub fn managed_service(
    bins: &PostgresBinaries,
    config: &HearthConfig,
    config_dir: &Path,
) -> anyhow::Result<ManagedService> {
    let data_dir = config_dir.join("data/postgresql");
    let run_dir = config_dir.join("run");

    // `$USER` may be absent in detached daemon contexts; fall back to "postgres".
    let user = std::env::var("USER").unwrap_or_else(|_| "postgres".to_string());

    let init_command = format!(
        "{initdb} -D {data} --auth-host=trust --auth-local=trust -U {user} --encoding=UTF8 --locale=C",
        initdb = shell_quote(&bins.initdb),
        data = shell_quote(&data_dir),
        user = shell_quote_str(&user),
    );

    // Postgres runtime: `-k` is the unix-socket directory (must be 0700);
    // wrapper enforces 0700 on data_dir AND run_dir via `dir_mode`.
    let runtime_args: Vec<String> = vec![
        "-D".to_string(),
        data_dir.display().to_string(),
        "-p".to_string(),
        config.postgres_port.to_string(),
        "-h".to_string(),
        "127.0.0.1".to_string(),
        "-k".to_string(),
        run_dir.display().to_string(),
    ];

    let wrapper_path = write_wrapper_script(&WrapperSpec {
        engine: "postgresql",
        engine_binary: &bins.postgres,
        engine_args: &runtime_args,
        data_dir: &data_dir,
        run_dir: &run_dir,
        init_command,
        dir_mode: Some(0o700),
    })?;

    let svc = ManagedService::new(
        ServiceKind::Postgresql,
        wrapper_path.display().to_string(),
        vec![],
    )
    .with_shutdown_strategy(ShutdownStrategy::Postgres {
        datadir: data_dir,
        pg_ctl_binary: bins.pg_ctl.clone(),
    });
    Ok(svc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_fake_pg_install(bin_dir: &Path) {
        std::fs::create_dir_all(bin_dir).unwrap();
        for name in ["postgres", "initdb", "pg_ctl"] {
            std::fs::write(bin_dir.join(name), "fake").unwrap();
        }
    }

    #[test]
    fn resolves_from_hearth_cache() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_fake_pg_install(&tmp.path().join("services/postgresql/bin"));
        let r = resolve_postgres_binaries_in(tmp.path(), &[]);
        let r = r.expect("hearth cache should resolve");
        assert_eq!(r.version, "cache");
        assert!(r.postgres.ends_with("services/postgresql/bin/postgres"));
        assert!(r.initdb.ends_with("services/postgresql/bin/initdb"));
        assert!(r.pg_ctl.ends_with("services/postgresql/bin/pg_ctl"));
    }

    #[test]
    fn resolver_requires_all_three_binaries() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bin = tmp.path().join("services/postgresql/bin");
        std::fs::create_dir_all(&bin).unwrap();
        // Only postgres; missing initdb + pg_ctl.
        std::fs::write(bin.join("postgres"), "fake").unwrap();
        let r = resolve_postgres_binaries_in(tmp.path(), &[]);
        assert!(r.is_none(), "should require initdb + pg_ctl too");
    }

    #[test]
    fn resolver_picks_highest_homebrew_version() {
        let tmp = tempfile::TempDir::new().unwrap();
        let hb = tmp.path().join("homebrew/opt");
        write_fake_pg_install(&hb.join("postgresql@15/bin"));
        write_fake_pg_install(&hb.join("postgresql@17/bin"));
        write_fake_pg_install(&hb.join("postgresql@16/bin"));

        let config_dir = tmp.path().join("empty-config");
        std::fs::create_dir_all(&config_dir).unwrap();
        let r =
            resolve_postgres_binaries_in(&config_dir, &[hb.clone()]).expect("homebrew resolved");
        assert_eq!(r.version, "17", "highest version must win");
        assert!(r.postgres.starts_with(hb.join("postgresql@17")));
    }

    #[test]
    fn resolver_falls_back_to_lower_when_highest_incomplete() {
        let tmp = tempfile::TempDir::new().unwrap();
        let hb = tmp.path().join("homebrew/opt");
        // @17 has postgres only — incomplete.
        std::fs::create_dir_all(hb.join("postgresql@17/bin")).unwrap();
        std::fs::write(hb.join("postgresql@17/bin/postgres"), "fake").unwrap();
        // @16 is complete.
        write_fake_pg_install(&hb.join("postgresql@16/bin"));

        let config_dir = tmp.path().join("empty-config");
        std::fs::create_dir_all(&config_dir).unwrap();
        let r =
            resolve_postgres_binaries_in(&config_dir, &[hb.clone()]).expect("@16 should resolve");
        assert_eq!(r.version, "16");
    }

    #[test]
    fn hearth_cache_beats_homebrew() {
        let tmp = tempfile::TempDir::new().unwrap();
        let hb = tmp.path().join("homebrew/opt");
        write_fake_pg_install(&hb.join("postgresql@17/bin"));
        write_fake_pg_install(&tmp.path().join("services/postgresql/bin"));

        let r = resolve_postgres_binaries_in(tmp.path(), &[hb]).expect("cache wins");
        assert_eq!(r.version, "cache");
    }

    #[test]
    fn managed_service_wrapper_contains_initdb_and_runtime_args() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_fake_pg_install(&tmp.path().join("services/postgresql/bin"));
        let bins = resolve_postgres_binaries_in(tmp.path(), &[]).unwrap();

        let mut config = HearthConfig::default();
        config.postgres_port = 5499;

        let svc = managed_service(&bins, &config, tmp.path()).unwrap();
        assert_eq!(svc.kind, ServiceKind::Postgresql);
        assert!(matches!(
            svc.shutdown_strategy,
            ShutdownStrategy::Postgres { .. }
        ));

        let wrapper = tmp.path().join("run/init-postgresql.sh");
        assert!(wrapper.exists(), "wrapper script must be written");
        let content = std::fs::read_to_string(&wrapper).unwrap();
        assert!(content.contains("initdb"), "init command should call initdb");
        assert!(
            content.contains("--auth-host=trust"),
            "trust auth required for Laravel passwordless connect"
        );
        assert!(
            content.contains("--locale=C"),
            "must set --locale=C to dodge macOS US-ASCII clash"
        );
        assert!(content.contains("--encoding=UTF8"));
        assert!(content.contains("5499"), "port must reach runtime args");
        assert!(content.contains("-k"), "socket dir flag must be present");
        assert!(content.contains("chmod 700"), "0700 on socket+data dirs");
    }
}
