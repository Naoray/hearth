use tracing::{info, warn};

use super::ServiceKind;
use super::supervisor::{FpmOwnership, ManagedService};
use crate::config::{AddedPackage, HearthConfig};

/// Current Herd ownership of the web/PHP services, as TYPED evidence
/// (C4-2, review 5650 r4): a failed probe is `Unknown`, never a silent
/// "not owned". Every activation policy treats `Unknown` fail-CLOSED.
///
/// Production detection is the Herd.app process check:
/// - exit 0 → `Owned` (a Herd.app process matched)
/// - exit 1 → `Unowned` (pgrep's documented "no processes matched")
/// - spawn error, death by signal, exit > 1 → `Unknown` with a
///   non-sensitive diagnostic (status/IO error class only).
///
/// A deterministic override seam exists for the isolated smoke gate ONLY:
/// honored exclusively while `HEARTH_ISOLATED_ROOT` (the typed isolated
/// runtime) is set, so no ambient environment variable can flip
/// Herd-coexistence behavior of a production daemon.
pub fn current_fpm_ownership() -> FpmOwnership {
    if let Some(state) = isolated_herd_ownership_override() {
        return state;
    }
    classify_pgrep_result(
        std::process::Command::new("pgrep")
            .args(["-q", "-f", "Herd\\.app"])
            .status(),
    )
}

/// Pure classification of the pgrep evidence (C4-2, unit-testable):
/// success → Owned; documented no-match exit 1 → Unowned; anything else —
/// spawn error, death by signal (no exit code), exit codes > 1 — is
/// `Unknown` with a non-sensitive diagnostic.
fn classify_pgrep_result(result: std::io::Result<std::process::ExitStatus>) -> FpmOwnership {
    match result {
        Ok(status) if status.success() => FpmOwnership::Owned,
        Ok(status) if status.code() == Some(1) => FpmOwnership::Unowned,
        Ok(status) => FpmOwnership::Unknown(format!(
            "ownership probe (pgrep) failed with status {status}"
        )),
        Err(e) => FpmOwnership::Unknown(format!(
            "ownership probe (pgrep) could not be spawned: {}",
            e.kind()
        )),
    }
}

/// Boolean convenience for paths that only need "is Herd PROVEN live":
/// `Owned` → true; `Unowned`/`Unknown` → false. Activation decisions must
/// NOT use this — they consume [`current_fpm_ownership`] and fail closed on
/// `Unknown`.
pub fn is_herd_running() -> bool {
    matches!(current_fpm_ownership(), FpmOwnership::Owned)
}

/// `HEARTH_HERD_OWNERSHIP=1|0|file:<path>` — deterministic Herd-ownership
/// answer for the isolated smoke daemon. `None` (ignored) unless the process
/// runs in the typed isolated runtime; unparseable env values are ignored,
/// never guessed. The `file:` form reads the flag file's CURRENT content on
/// every probe: exact `1` → Owned, exact `0` → Unowned, and anything else —
/// missing, unreadable, empty, invalid content — is `Unknown` (fail-closed,
/// C4-2), so the smoke can exercise the unknown-ownership policy too.
fn isolated_herd_ownership_override() -> Option<FpmOwnership> {
    std::env::var_os("HEARTH_ISOLATED_ROOT")?;
    let value = std::env::var("HEARTH_HERD_OWNERSHIP").ok()?;
    match value.as_str() {
        "1" => Some(FpmOwnership::Owned),
        "0" => Some(FpmOwnership::Unowned),
        other => {
            let path = other.strip_prefix("file:")?;
            Some(classify_flag_read(std::fs::read(path)))
        }
    }
}

/// Pure classification of the isolated flag-file evidence (C5-1A): the
/// EXACT single byte `1` → Owned, the exact single byte `0` → Unowned —
/// no trimming, ever. Any newline, surrounding or internal whitespace,
/// BOM, extra bytes, empty file, invalid UTF-8, or read failure
/// (missing/unreadable/racing) is `Unknown` (fail-closed) with a
/// non-sensitive diagnostic. Malformed evidence must never become
/// authoritative.
fn classify_flag_read(result: std::io::Result<Vec<u8>>) -> FpmOwnership {
    match result {
        Ok(bytes) => match bytes.as_slice() {
            b"1" => FpmOwnership::Owned,
            b"0" => FpmOwnership::Unowned,
            _ => FpmOwnership::Unknown(
                "isolated ownership flag content is not exactly `1` or `0`".to_string(),
            ),
        },
        Err(e) => FpmOwnership::Unknown(format!(
            "isolated ownership flag unreadable: {}",
            e.kind()
        )),
    }
}

/// Filter `added_packages` to only entries whose `<site_path>/vendor/<vendor>/<pkg>`
/// still exists on disk. Returns the kept entries and the dropped entries so the
/// caller can persist the cleaned config + log warnings.
///
/// Closes the gap left by `hearth remove` being out of v0.3.0 scope — without this,
/// a user who `composer remove laravel/horizon` would be stuck with the supervisor
/// hammering a broken worker until the circuit breaker trips. (Scratchpad 796 B1.)
pub fn prune_added_packages(packages: &[AddedPackage]) -> (Vec<AddedPackage>, Vec<AddedPackage>) {
    let mut kept = Vec::with_capacity(packages.len());
    let mut dropped = Vec::new();
    for pkg in packages {
        let vendor_path = pkg.site_path.join("vendor").join(vendor_path_for(&pkg.package));
        if vendor_path.exists() {
            kept.push(pkg.clone());
        } else {
            warn!(
                package = %pkg.package,
                site = %pkg.site_path.display(),
                "prune: {} at {} no longer installed; removed from supervised list",
                pkg.package,
                pkg.site_path.display(),
            );
            dropped.push(pkg.clone());
        }
    }
    (kept, dropped)
}

/// Map a package key to its vendor-relative path (e.g. `"horizon"` → `"laravel/horizon"`).
pub fn vendor_path_for(package: &str) -> String {
    match package {
        "horizon" => "laravel/horizon".to_string(),
        "telescope" => "laravel/telescope".to_string(),
        "pulse" => "laravel/pulse".to_string(),
        "reverb" => "laravel/reverb".to_string(),
        other => format!("laravel/{other}"),
    }
}

/// Build the default set of managed services based on configuration.
///
/// When Herd is detected, nginx/php-fpm/dnsmasq are skipped — Herd
/// manages those. Hearth only registers its own services (Mailpit, etc.).
/// The dump server and MCP server are Tokio tasks, not supervised processes.
///
/// `php_launches_enabled = false` (daemon boot after a HARD php-config
/// reconciliation failure) skips every PHP process registration — FPM and
/// Horizon/Reverb workers — so no PHP launches with unreconciled channel
/// state; non-PHP services and status access stay available.
///
/// B5-4: `roots` is the caller's already-validated `ProviderRoots` snapshot
/// (the same value the PHP engine holds). This function performs NO root
/// detection of its own — a root-construction failure must propagate at the
/// entrypoint before any PHP/FPM/worker registration, never degrade into a
/// warning plus a partial service list.
pub fn default_services(
    config: &HearthConfig,
    config_dir: &std::path::Path,
    php_launches_enabled: bool,
    roots: &crate::php::targets::ProviderRoots,
) -> Vec<ManagedService> {
    let mut services = Vec::new();

    // C4-2: boot registration consumes TYPED ownership and fails closed —
    // unknown evidence must never register the Herd-conflicting trio.
    let ownership = current_fpm_ownership();
    if ownership == FpmOwnership::Owned {
        info!("Herd detected — skipping nginx, php-fpm, dnsmasq (managed by Herd)");
    } else if let FpmOwnership::Unknown(diag) = &ownership {
        warn!(
            "Herd ownership unknown ({diag}) — skipping nginx, php-fpm, dnsmasq \
             fail-closed until the ownership probe recovers"
        );
    } else {
        services.push(ManagedService::new(
            ServiceKind::Nginx,
            "nginx".to_string(),
            vec![
                "-c".to_string(),
                config_dir
                    .join("nginx/nginx.conf")
                    .to_string_lossy()
                    .to_string(),
                "-g".to_string(),
                "daemon off;".to_string(),
            ],
        ));
        services.push(ManagedService::new(
            ServiceKind::Dnsmasq,
            "dnsmasq".to_string(),
            vec![
                "--keep-in-foreground".to_string(),
                format!("--port={}", config.dns_port),
                format!(
                    "--conf-file={}",
                    config_dir.join("dnsmasq/dnsmasq.conf").display()
                ),
            ],
        ));
        if php_launches_enabled {
            match hearth_fpm_service(&config.default_php, config_dir) {
                Ok(svc) => services.push(svc),
                Err(reason) => warn!(reason = %reason, "php-fpm not registered"),
            }
        } else {
            warn!("php launches disabled (hard php-config failure) — php-fpm not registered");
        }
    }

    // Mailpit — only if binary is found
    if let Some(mailpit_bin) = crate::mailpit::resolve_mailpit_binary(config_dir) {
        services.push(ManagedService::new(
            ServiceKind::Mailpit,
            mailpit_bin.to_string_lossy().to_string(),
            vec![
                "--smtp".to_string(),
                format!("127.0.0.1:{}", config.mail_smtp_port),
                "--listen".to_string(),
                format!("127.0.0.1:{}", config.mail_ui_port),
                "--db-file".to_string(),
                config_dir
                    .join("services/mailpit/mailpit.db")
                    .to_string_lossy()
                    .to_string(),
            ],
        ));
    }

    // Postgres — register only if binaries resolve AND nothing else owns
    // the configured port (other tools' services panels, Homebrew services, etc.).
    if let Some(pg) = crate::db::postgres::resolve_postgres_binaries(config_dir) {
        if crate::db::health::port_in_use("127.0.0.1", config.postgres_port) {
            info!(
                port = config.postgres_port,
                "postgres port already in use — skipping registration"
            );
        } else {
            match crate::db::postgres::managed_service(&pg, config, config_dir) {
                Ok(svc) => services.push(svc),
                Err(e) => {
                    info!(error = %e, "failed to build postgres ManagedService — skipping")
                }
            }
        }
    }

    // MySQL/MariaDB — flavor detection inside resolver (spike-confirmed).
    if let Some(mb) = crate::db::mysql::resolve_mysql_binaries(config_dir) {
        if crate::db::health::port_in_use("127.0.0.1", config.mysql_port) {
            info!(
                port = config.mysql_port,
                "mysql port already in use — skipping registration"
            );
        } else {
            match crate::db::mysql::managed_service(&mb, config, config_dir) {
                Ok(svc) => services.push(svc),
                Err(e) => {
                    info!(error = %e, "failed to build mysql ManagedService — skipping")
                }
            }
        }
    }

    // Redis — dataless engine. Same port_in_use guard as Postgres.
    if let Some(redis_bin) = crate::db::redis::resolve_redis_binary(config_dir) {
        if crate::db::health::port_in_use("127.0.0.1", config.redis_port) {
            info!(
                port = config.redis_port,
                "redis port already in use — skipping registration"
            );
        } else {
            match crate::db::redis::managed_service(&redis_bin, config, config_dir) {
                Ok(svc) => services.push(svc),
                Err(e) => {
                    info!(error = %e, "failed to build redis ManagedService — skipping")
                }
            }
        }
    }

    // Supervised AddedPackage entries (Horizon, Reverb). Boot-time prune already ran
    // before this is called (see daemon main.rs), so every entry here has a vendor dir.
    if php_launches_enabled {
        for pkg in &config.added_packages {
            let kind = match pkg.package.as_str() {
                "horizon" => ServiceKind::Horizon,
                "reverb" => ServiceKind::Reverb,
                _ => continue, // telescope/pulse are install-only
            };
            services.push(worker_service(kind, pkg, config_dir, roots));
        }
    } else if !config.added_packages.is_empty() {
        warn!("php launches disabled (hard php-config failure) — workers not registered");
    }

    services
}

/// Build Hearth's own supervised php-fpm service for one PHP version, or a
/// truthful skip reason. Registration requires BOTH a resolvable binary and a
/// generated fpm config; a missing config is launch-blocked (generation is
/// todo #2343) and must never surface as a supervisor start failure. Takes
/// the version directly so version-switch paths build the NEW version's
/// service — binary, args, AND scan-dir env — from scratch.
pub fn hearth_fpm_service(
    version: &str,
    config_dir: &std::path::Path,
) -> Result<ManagedService, String> {
    let Some(binary) =
        crate::php::resolver::resolve_phpfpm_binary(version, &config_dir.to_path_buf())
    else {
        return Err(format!(
            "php-fpm binary for {version} not found — skipping registration"
        ));
    };
    let fpm_conf = config_dir.join("fpm/php-fpm.conf");
    if !fpm_conf.is_file() {
        return Err("launch-blocked: missing fpm config — see todo #2343".to_string());
    }
    Ok(ManagedService::new(
        ServiceKind::PhpFpm,
        binary.to_string_lossy().to_string(),
        vec![
            "--nodaemonize".to_string(),
            format!("--fpm-config={}", fpm_conf.display()),
        ],
    )
    .with_env(vec![crate::php::scan_dir_env(config_dir, version)]))
}

/// Supervised worker with Belt-E scan-dir env when its PHP version is known.
/// Used by BOTH worker registration sites (boot rehydration here, and the
/// daemon's add-time registration). Legacy entries without a version get one
/// inferred ONLY when the stored command equals a discovered target's
/// canonical binary; otherwise the worker spawns env-unguaranteed (loud warn,
/// never a wrong env credit).
pub fn worker_service(
    kind: ServiceKind,
    pkg: &AddedPackage,
    config_dir: &std::path::Path,
    roots: &crate::php::targets::ProviderRoots,
) -> ManagedService {
    let svc = ManagedService::with_cwd_and_site(
        kind,
        pkg.command.clone(),
        pkg.args.clone(),
        pkg.site_path.clone(),
        pkg.site_name.clone(),
    );
    let version = pkg
        .php_version
        .clone()
        .or_else(|| infer_worker_version(&pkg.command, roots));
    match version {
        Some(version) => svc.with_env(vec![crate::php::scan_dir_env(config_dir, &version)]),
        None => {
            warn!(
                package = %pkg.package,
                command = %pkg.command,
                "worker PHP version unknown and command matches no discovered \
                 PHP binary — spawning without scan-dir env (unguaranteed)",
            );
            svc
        }
    }
}

/// Infer a legacy worker's PHP version from its stored command path — only an
/// exact canonical-binary match counts (plan 5560 §2.4 provenance rule).
fn infer_worker_version(
    command: &str,
    roots: &crate::php::targets::ProviderRoots,
) -> Option<String> {
    let canonical = std::path::Path::new(command).canonicalize().ok()?;
    crate::php::targets::discover_provider_targets(roots)
        .into_iter()
        .find(|id| id.binary == canonical)
        .map(|id| id.version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HearthConfig;

    #[test]
    fn herd_detection_returns_bool() {
        // Just verify it doesn't panic — result depends on whether Herd is running
        let _ = is_herd_running();
    }

    #[test]
    fn default_services_includes_mailpit_when_binary_found() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bin_path = tmp.path().join("services/mailpit/mailpit");
        std::fs::create_dir_all(bin_path.parent().unwrap()).unwrap();
        std::fs::write(&bin_path, "fake").unwrap();

        let config = HearthConfig::default();
        let services = default_services(&config, tmp.path(), true, &test_roots(tmp.path()));

        let kinds: Vec<ServiceKind> = services.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&ServiceKind::Mailpit));
    }

    #[test]
    fn default_services_excludes_mailpit_when_binary_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = HearthConfig::default();
        let services = default_services(&config, tmp.path(), true, &test_roots(tmp.path()));

        let kinds: Vec<ServiceKind> = services.iter().map(|s| s.kind).collect();
        assert!(!kinds.contains(&ServiceKind::Mailpit));
    }

    fn write_fake_pg_install(bin_dir: &std::path::Path) {
        std::fs::create_dir_all(bin_dir).unwrap();
        for name in ["postgres", "initdb", "pg_ctl"] {
            std::fs::write(bin_dir.join(name), "fake").unwrap();
        }
    }

    #[test]
    fn default_services_registers_postgres_when_binaries_present_and_port_free() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_fake_pg_install(&tmp.path().join("services/postgresql/bin"));

        // Pick an OS-assigned port we know is free (bind+drop releases it; brief
        // window for races but acceptable for unit scope).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let free_port = listener.local_addr().unwrap().port();
        drop(listener);

        let config = HearthConfig {
            postgres_port: free_port,
            ..HearthConfig::default()
        };
        let services = default_services(&config, tmp.path(), true, &test_roots(tmp.path()));
        let kinds: Vec<ServiceKind> = services.iter().map(|s| s.kind).collect();
        assert!(
            kinds.contains(&ServiceKind::Postgresql),
            "postgres should be registered, got: {kinds:?}"
        );
    }

    fn write_fake_redis(bin: &std::path::Path) {
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(bin, "fake").unwrap();
    }

    fn write_fake_mysqld(bin: &std::path::Path) {
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(bin, "fake").unwrap();
    }

    #[test]
    fn default_services_registers_mysql_when_binary_present_and_port_free() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_fake_mysqld(&tmp.path().join("services/mysql/bin/mysqld"));

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let free_port = listener.local_addr().unwrap().port();
        drop(listener);

        let config = HearthConfig {
            mysql_port: free_port,
            ..HearthConfig::default()
        };
        let services = default_services(&config, tmp.path(), true, &test_roots(tmp.path()));
        let kinds: Vec<ServiceKind> = services.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&ServiceKind::Mysql), "got: {kinds:?}");
    }

    #[test]
    fn default_services_skips_mysql_when_port_in_use() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_fake_mysqld(&tmp.path().join("services/mysql/bin/mysqld"));

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let busy_port = listener.local_addr().unwrap().port();

        let config = HearthConfig {
            mysql_port: busy_port,
            ..HearthConfig::default()
        };
        let services = default_services(&config, tmp.path(), true, &test_roots(tmp.path()));
        drop(listener);

        let kinds: Vec<ServiceKind> = services.iter().map(|s| s.kind).collect();
        assert!(!kinds.contains(&ServiceKind::Mysql), "got: {kinds:?}");
    }

    #[test]
    fn default_services_registers_redis_when_binary_present_and_port_free() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_fake_redis(&tmp.path().join("services/redis/bin/redis-server"));

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let free_port = listener.local_addr().unwrap().port();
        drop(listener);

        let config = HearthConfig {
            redis_port: free_port,
            ..HearthConfig::default()
        };
        let services = default_services(&config, tmp.path(), true, &test_roots(tmp.path()));
        let kinds: Vec<ServiceKind> = services.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&ServiceKind::Redis), "got: {kinds:?}");
    }

    #[test]
    fn default_services_skips_redis_when_port_in_use() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_fake_redis(&tmp.path().join("services/redis/bin/redis-server"));

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let busy_port = listener.local_addr().unwrap().port();
        let config = HearthConfig {
            redis_port: busy_port,
            ..HearthConfig::default()
        };
        let services = default_services(&config, tmp.path(), true, &test_roots(tmp.path()));
        drop(listener);

        let kinds: Vec<ServiceKind> = services.iter().map(|s| s.kind).collect();
        assert!(!kinds.contains(&ServiceKind::Redis), "got: {kinds:?}");
    }

    #[test]
    fn default_services_skips_postgres_when_port_in_use() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_fake_pg_install(&tmp.path().join("services/postgresql/bin"));

        // Hold the listener over the call so port_in_use sees it bound.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let busy_port = listener.local_addr().unwrap().port();

        let config = HearthConfig {
            postgres_port: busy_port,
            ..HearthConfig::default()
        };
        let services = default_services(&config, tmp.path(), true, &test_roots(tmp.path()));
        drop(listener);

        let kinds: Vec<ServiceKind> = services.iter().map(|s| s.kind).collect();
        assert!(
            !kinds.contains(&ServiceKind::Postgresql),
            "postgres should be skipped on port collision, got: {kinds:?}"
        );
    }

    #[test]
    fn vendor_path_for_maps_known_packages() {
        assert_eq!(vendor_path_for("horizon"), "laravel/horizon");
        assert_eq!(vendor_path_for("telescope"), "laravel/telescope");
        assert_eq!(vendor_path_for("pulse"), "laravel/pulse");
        assert_eq!(vendor_path_for("reverb"), "laravel/reverb");
    }

    #[test]
    fn prune_drops_packages_whose_vendor_dir_is_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let site_kept = tmp.path().join("site-kept");
        std::fs::create_dir_all(site_kept.join("vendor/laravel/horizon")).unwrap();
        let site_gone = tmp.path().join("site-gone");
        std::fs::create_dir_all(site_gone.join("vendor")).unwrap();
        // vendor/laravel/horizon NOT created — composer remove happened externally

        let now = chrono::Utc::now();
        let packages = vec![
            AddedPackage {
                package: "horizon".to_string(),
                site_path: site_kept.clone(),
                site_name: "kept".to_string(),
                command: "/php".to_string(),
                args: vec!["artisan".to_string(), "horizon".to_string()],
                php_version: None,
                installed_at: now,
            },
            AddedPackage {
                package: "horizon".to_string(),
                site_path: site_gone.clone(),
                site_name: "gone".to_string(),
                command: "/php".to_string(),
                args: vec!["artisan".to_string(), "horizon".to_string()],
                php_version: None,
                installed_at: now,
            },
        ];
        let (kept, dropped) = prune_added_packages(&packages);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].site_name, "kept");
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].site_name, "gone");
    }

    #[test]
    fn prune_empty_input_returns_empty() {
        let (kept, dropped) = prune_added_packages(&[]);
        assert!(kept.is_empty());
        assert!(dropped.is_empty());
    }

    #[test]
    fn default_services_registers_horizon_from_added_packages() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut config = HearthConfig::default();
        config.added_packages.push(AddedPackage {
            package: "horizon".to_string(),
            site_path: tmp.path().to_path_buf(),
            site_name: "shopfront".to_string(),
            command: "/php".to_string(),
            args: vec!["artisan".to_string(), "horizon".to_string()],
            php_version: None,
            installed_at: chrono::Utc::now(),
        });

        let services = default_services(&config, tmp.path(), true, &test_roots(tmp.path()));
        let horizon = services
            .iter()
            .find(|s| s.kind == ServiceKind::Horizon)
            .expect("Horizon should be registered");
        assert_eq!(horizon.site_name(), Some("shopfront"));
        assert_eq!(horizon.display_name(), "horizon[shopfront]");
    }

    #[test]
    fn default_services_ignores_telescope_in_added_packages() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut config = HearthConfig::default();
        config.added_packages.push(AddedPackage {
            package: "telescope".to_string(),
            site_path: tmp.path().to_path_buf(),
            site_name: "blog".to_string(),
            command: String::new(),
            args: vec![],
            php_version: None,
            installed_at: chrono::Utc::now(),
        });

        let services = default_services(&config, tmp.path(), true, &test_roots(tmp.path()));
        assert!(services.iter().all(|s| s.kind != ServiceKind::Horizon));
        assert!(services.iter().all(|s| s.kind != ServiceKind::Reverb));
    }

    #[test]
    fn php_disabled_skips_fpm_and_workers_keeps_others() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Mailpit present so a non-PHP service would register.
        let bin_path = tmp.path().join("services/mailpit/mailpit");
        std::fs::create_dir_all(bin_path.parent().unwrap()).unwrap();
        std::fs::write(&bin_path, "fake").unwrap();
        // Launchable FPM + a worker entry that WOULD register.
        write_fake_fpm(tmp.path(), "8.4");
        std::fs::create_dir_all(tmp.path().join("fpm")).unwrap();
        std::fs::write(tmp.path().join("fpm/php-fpm.conf"), "; fpm").unwrap();
        let mut config = HearthConfig::default();
        config.added_packages.push(AddedPackage {
            package: "horizon".to_string(),
            site_path: tmp.path().to_path_buf(),
            site_name: "s".to_string(),
            command: "/php".to_string(),
            args: vec!["artisan".to_string(), "horizon".to_string()],
            php_version: Some("8.4".to_string()),
            installed_at: chrono::Utc::now(),
        });

        let services = default_services(&config, tmp.path(), false, &test_roots(tmp.path()));
        let kinds: Vec<ServiceKind> = services.iter().map(|s| s.kind).collect();
        assert!(!kinds.contains(&ServiceKind::PhpFpm), "got: {kinds:?}");
        assert!(!kinds.contains(&ServiceKind::Horizon), "got: {kinds:?}");
        assert!(kinds.contains(&ServiceKind::Mailpit), "got: {kinds:?}");
    }

    fn write_fake_fpm(config_dir: &std::path::Path, version: &str) -> std::path::PathBuf {
        let dir = config_dir.join("php").join(version);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("php-fpm");
        std::fs::write(&bin, "fake").unwrap();
        bin
    }

    #[test]
    fn fpm_uses_resolved_binary_and_scan_env() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_dir = tmp.path();
        let bin = write_fake_fpm(config_dir, "8.4");
        std::fs::create_dir_all(config_dir.join("fpm")).unwrap();
        std::fs::write(config_dir.join("fpm/php-fpm.conf"), "; fpm").unwrap();

        let svc = hearth_fpm_service("8.4", config_dir).expect("fpm should register");
        assert_eq!(
            svc.env(),
            &[(
                "PHP_INI_SCAN_DIR".to_string(),
                format!(":{}", config_dir.join("php/8.4/conf.d").display()),
            )]
        );
        // Resolved absolute binary, never bare PATH "php-fpm".
        assert_eq!(svc.command(), bin.display().to_string());
        assert_eq!(svc.display_name(), "php-fpm");
    }

    #[test]
    fn fpm_skipped_and_launch_blocked_when_conf_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_fake_fpm(tmp.path(), "8.4");
        // No fpm/php-fpm.conf generated (that is todo #2343's job).
        let err = match hearth_fpm_service("8.4", tmp.path()) {
            Err(e) => e,
            Ok(_) => panic!("expected launch-blocked skip"),
        };
        assert!(err.contains("launch-blocked"), "got: {err}");
        assert!(err.contains("#2343"), "got: {err}");
    }

    #[test]
    fn fpm_skipped_when_unresolvable() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("fpm")).unwrap();
        std::fs::write(tmp.path().join("fpm/php-fpm.conf"), "; fpm").unwrap();
        let err = match hearth_fpm_service("7.4", tmp.path()) {
            Err(e) => e,
            Ok(_) => panic!("expected unresolvable skip"),
        };
        assert!(err.contains("not found"), "got: {err}");
    }

    fn test_roots(base: &std::path::Path) -> crate::php::targets::ProviderRoots {
        crate::php::targets::ProviderRoots::isolated(
            base,
            base.join("hearth"),
            base.join("herd"),
            base.join("homebrew"),
        )
        .unwrap()
    }

    #[test]
    fn worker_env_injected_at_boot_and_add_registration() {
        // worker_service is the single constructor used by BOTH registration
        // sites (manager boot loop + daemon add-arm).
        let tmp = tempfile::TempDir::new().unwrap();
        let pkg = AddedPackage {
            package: "horizon".to_string(),
            site_path: tmp.path().to_path_buf(),
            site_name: "shopfront".to_string(),
            command: "/php".to_string(),
            args: vec!["artisan".to_string(), "horizon".to_string()],
            php_version: Some("8.3".to_string()),
            installed_at: chrono::Utc::now(),
        };
        let roots = test_roots(tmp.path());
        let svc = worker_service(ServiceKind::Horizon, &pkg, tmp.path(), &roots);
        assert_eq!(
            svc.env(),
            &[(
                "PHP_INI_SCAN_DIR".to_string(),
                format!(":{}", tmp.path().join("php/8.3/conf.d").display()),
            )]
        );
        assert_eq!(svc.site_name(), Some("shopfront"));
    }

    #[test]
    fn legacy_added_package_inferred_only_on_canonical_match_else_unguaranteed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let roots = test_roots(&base);
        // A discoverable Herd 8.4 CLI binary.
        let herd_bin = roots.herd.join("bin");
        std::fs::create_dir_all(&herd_bin).unwrap();
        let php84 = herd_bin.join("php84");
        std::fs::write(&php84, "fake").unwrap();

        let mut pkg = AddedPackage {
            package: "horizon".to_string(),
            site_path: base.clone(),
            site_name: "legacy".to_string(),
            command: php84.display().to_string(),
            args: vec!["artisan".to_string(), "horizon".to_string()],
            php_version: None,
            installed_at: chrono::Utc::now(),
        };
        // Canonical match → version inferred → env granted.
        let svc = worker_service(ServiceKind::Horizon, &pkg, &base, &roots);
        assert_eq!(
            svc.env(),
            &[(
                "PHP_INI_SCAN_DIR".to_string(),
                format!(":{}", base.join("php/8.4/conf.d").display()),
            )]
        );

        // No canonical match → unguaranteed: NO env, never a guess.
        pkg.command = base.join("somewhere-else/php").display().to_string();
        let svc = worker_service(ServiceKind::Horizon, &pkg, &base, &roots);
        assert!(
            svc.env().is_empty(),
            "unmatched legacy command must not get env credit"
        );
    }

    #[test]
    fn root_failure_can_never_register_php_workers() {
        // B5-4 (red reproduced live at 224621f: a lone HEARTH_HERD_ROOT made
        // the internal detect() fail, and default_services still registered
        // the Horizon worker behind a warning). Post-fix: default_services
        // performs NO root detection of its own — the entrypoint's
        // `ProviderRoots::detect()?` propagates the typed error BEFORE any
        // registration, and service construction consumes only the passed
        // validated snapshot, immune to poisoned root env.
        let guard = crate::test_env::EnvGuard::capture([
            "HEARTH_ISOLATED_ROOT",
            "HEARTH_HERD_ROOT",
            "HEARTH_HOMEBREW_ROOT",
            "HEARTH_CONFIG_DIR",
        ]);
        guard.remove("HEARTH_ISOLATED_ROOT");
        guard.remove("HEARTH_CONFIG_DIR");
        guard.remove("HEARTH_HOMEBREW_ROOT");
        // A lone discovery override makes ProviderRoots::detect() fail closed.
        guard.set("HEARTH_HERD_ROOT", "/tmp/lone-override");
        assert!(
            crate::php::targets::ProviderRoots::detect().is_err(),
            "the entrypoint gate is a typed error — never a warning"
        );

        let tmp = tempfile::TempDir::new().unwrap();
        let mut config = HearthConfig::default();
        config.added_packages.push(AddedPackage {
            package: "horizon".to_string(),
            site_path: tmp.path().to_path_buf(),
            site_name: "shopfront".to_string(),
            command: "/php".to_string(),
            args: vec!["artisan".to_string(), "horizon".to_string()],
            php_version: Some("8.3".to_string()),
            installed_at: chrono::Utc::now(),
        });
        // With an explicit validated snapshot the worker registers with its
        // Belt-E env — proof no second (env-driven) detection remains.
        let roots = test_roots(tmp.path());
        let services = default_services(&config, tmp.path(), true, &roots);
        let worker = services
            .iter()
            .find(|s| s.kind == crate::service::ServiceKind::Horizon)
            .expect("explicit snapshot registers the worker regardless of ambient env");
        assert_eq!(
            worker.env(),
            &[(
                "PHP_INI_SCAN_DIR".to_string(),
                format!(":{}", tmp.path().join("php/8.3/conf.d").display()),
            )]
        );
        drop(guard);
    }

    // ---- C4-2 (review 5650 r4): typed ownership evidence ----

    /// pgrep evidence matrix: 0→Owned, documented 1→Unowned, everything
    /// else (exit >1, death by signal, spawn error) → Unknown.
    #[test]
    fn pgrep_classification_matrix() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::ExitStatus;

        let exit = |code: i32| Ok(ExitStatus::from_raw(code << 8));
        assert_eq!(classify_pgrep_result(exit(0)), FpmOwnership::Owned);
        assert_eq!(classify_pgrep_result(exit(1)), FpmOwnership::Unowned);
        assert!(matches!(
            classify_pgrep_result(exit(2)),
            FpmOwnership::Unknown(_)
        ));
        // Death by signal: no exit code.
        let signaled: std::io::Result<ExitStatus> = Ok(ExitStatus::from_raw(15));
        assert!(matches!(
            classify_pgrep_result(signaled),
            FpmOwnership::Unknown(_)
        ));
        let spawn_err: std::io::Result<ExitStatus> =
            Err(std::io::Error::from(std::io::ErrorKind::NotFound));
        match classify_pgrep_result(spawn_err) {
            FpmOwnership::Unknown(diag) => {
                assert!(diag.contains("could not be spawned"), "{diag}");
                // Non-sensitive: error-kind only, no environment contents.
                assert!(!diag.contains("HEARTH_"), "{diag}");
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    /// Isolated flag-file evidence matrix (C5-1A): the EXACT bytes `1`/`0`
    /// only — trailing newlines, whitespace, BOM, extra bytes, empty
    /// content, invalid UTF-8, and read failures are all Unknown.
    #[test]
    fn flag_file_classification_matrix() {
        assert_eq!(
            classify_flag_read(Ok(b"1".to_vec())),
            FpmOwnership::Owned,
            "exact byte 1"
        );
        assert_eq!(
            classify_flag_read(Ok(b"0".to_vec())),
            FpmOwnership::Unowned,
            "exact byte 0"
        );
        let invalid_cases: &[&[u8]] = &[
            b"",
            b"1\n",
            b"0\n",
            b" 0 ",
            b"\t1",
            b"1 ",
            b"0 0",
            b"\xef\xbb\xbf1", // BOM + 1
            b"10",
            b"01",
            b"yes",
            b"2",
            b"\xff\x31", // invalid UTF-8 followed by '1'
        ];
        for invalid in invalid_cases {
            assert!(
                matches!(
                    classify_flag_read(Ok(invalid.to_vec())),
                    FpmOwnership::Unknown(_)
                ),
                "bytes {invalid:?} must be Unknown"
            );
        }
        assert!(matches!(
            classify_flag_read(Err(std::io::Error::from(std::io::ErrorKind::NotFound))),
            FpmOwnership::Unknown(_)
        ));
        assert!(matches!(
            classify_flag_read(Err(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied
            ))),
            FpmOwnership::Unknown(_)
        ));
    }
}
