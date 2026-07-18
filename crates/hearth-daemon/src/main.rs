use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use hearth_lib::config::{AddedPackage, HearthConfig};
use hearth_lib::db::health::port_in_use;
use hearth_lib::mcp::HearthMcpServer;
use hearth_lib::php::PhpManager;
use hearth_lib::php::engine::PhpConfigEngine;
use hearth_lib::php::targets::ProviderRoots;
use hearth_lib::service::manager::{default_services, prune_added_packages};
use hearth_lib::service::supervisor::ServiceSupervisor;
use hearth_lib::service::{ServiceKind, ServiceState};
use hearth_lib::site::SiteManager;
use hearth_lib::socket::{DaemonRequest, DaemonResponse, DbEngineStatus};

use rmcp::transport::StreamableHttpServerConfig;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::StreamableHttpService;

mod php_config;

/// Shared state accessible by all daemon request handlers.
///
/// Each field is individually wrapped in `Arc<Mutex<T>>` so that the MCP
/// server can hold cloned references without locking the entire state.
///
/// Lock ordering convention (when multiple locks are needed):
///   `config` -> `php_manager` -> `site_manager` -> `supervisor`
///
/// `add_lock` is a coarse mutex held for the duration of an `Add` handler. Concurrent
/// `hearth add` invocations against the same site would otherwise race on `.env`
/// editing and supervisor registration (scratchpad 796 H8).
struct DaemonState {
    supervisor: Arc<Mutex<ServiceSupervisor>>,
    config: Arc<Mutex<HearthConfig>>,
    site_manager: Arc<Mutex<SiteManager>>,
    php_manager: Arc<Mutex<PhpManager>>,
    add_lock: Arc<Mutex<()>>,
    /// Shared php-config engine (daemon socket handler + MCP tools route
    /// through the same instance; its op lock serializes both).
    php_engine: Arc<PhpConfigEngine>,
    /// Injected config persistence path — handlers must never call the
    /// global-path `HearthConfig::save()`.
    config_path: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("hearth=info")
        .init();

    info!("Hearth daemon starting...");

    // Load config
    let mut config = HearthConfig::load().context("Failed to load config")?;

    // Ensure directories exist before any engine registration. DB engines
    // expect `run/` and `data/` to be present on first start; pre-creating
    // them once per daemon boot keeps the wrapper scripts simple.
    let config_dir = hearth_lib::config_dir();
    let config_path = config_dir.join("config.toml");
    std::fs::create_dir_all(hearth_lib::run_dir())?;
    std::fs::create_dir_all(hearth_lib::log_dir())?;
    std::fs::create_dir_all(hearth_lib::data_dir())?;

    // Boot-time prune: drop AddedPackage entries whose vendor dir no longer exists on
    // disk (closes the gap left by `hearth remove` being out of v0.3.0 scope).
    let (kept, dropped) = prune_added_packages(&config.added_packages);
    if !dropped.is_empty() {
        info!(count = dropped.len(), "pruned stale AddedPackage entries from config");
        config.added_packages = kept;
        if let Err(e) = config.save_to(&config_path) {
            warn!(error = %e, "failed to persist pruned config");
        }
    }

    // Capture read-only fields before wrapping config in Arc<Mutex<>>
    let tld = config.tld.clone();
    let dump_port = config.dump_port;
    let mcp_port = config.mcp_port;

    let config = Arc::new(Mutex::new(config));

    // Shared php-config engine + boot hook: pending-journal recovery, guarded
    // legacy INI migration, and channel reconcile run BEFORE default_services
    // so the supervisor only ever sees reconciled state.
    let php_engine = Arc::new(PhpConfigEngine::new(
        Arc::clone(&config),
        config_path.clone(),
        config_dir.clone(),
        ProviderRoots::detect(),
        Arc::new(|| hearth_lib::service::manager::is_herd_running()),
        std::time::Duration::from_secs(5),
    ));
    // Boot hard gate (F4): a failed OR hard-refused reconcile disables every
    // PHP launch (FPM + workers) while the daemon keeps serving truthful
    // status and non-PHP services.
    let php_launches_enabled =
        match hearth_lib::php::engine::require_reconciled(php_engine.boot_sync().await) {
            Ok(_) => true,
            Err(e) => {
                error!(error = %e, "php-config boot reconcile failed — PHP launches disabled");
                false
            }
        };

    // Build supervisor with default services.
    let mut supervisor = ServiceSupervisor::new();
    {
        let cfg = config.lock().await;
        for svc in default_services(&cfg, &config_dir, php_launches_enabled) {
            supervisor.register(svc);
        }
    }

    // Valet home directories for site enumeration (Valet + Herd)
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    let mut valet_homes = vec![home.join(".config/valet")];
    let herd_valet = home.join("Library/Application Support/Herd/config/valet");
    if herd_valet.exists() {
        valet_homes.push(herd_valet);
    }

    let state = Arc::new(DaemonState {
        supervisor: Arc::new(Mutex::new(supervisor)),
        config,
        site_manager: Arc::new(Mutex::new(SiteManager::with_homes(valet_homes, tld))),
        php_manager: Arc::new(Mutex::new(PhpManager::new(hearth_lib::config_dir()))),
        add_lock: Arc::new(Mutex::new(())),
        php_engine,
        config_path,
    });

    // Remove stale socket
    let socket_path = hearth_lib::socket::socket_path();
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)?;
    }
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Bind Unix socket
    let listener = UnixListener::bind(&socket_path)
        .context("Failed to bind Unix socket")?;
    info!(path = %socket_path.display(), "listening on Unix socket");

    // Write PID file
    let pid_path = hearth_lib::run_dir().join("daemon.pid");
    std::fs::write(&pid_path, std::process::id().to_string())?;

    // Spawn dump server
    tokio::spawn(async move {
        if let Err(e) = hearth_lib::dump::run_dump_server(dump_port).await {
            error!(error = %e, "dump server failed");
        }
    });

    // Spawn health check loop
    let health_supervisor = Arc::clone(&state.supervisor);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let mut sup = health_supervisor.lock().await;
            sup.health_check();
        }
    });

    // Spawn MCP Streamable HTTP server
    let mcp_supervisor = Arc::clone(&state.supervisor);
    let mcp_site_manager = Arc::clone(&state.site_manager);
    let mcp_php_manager = Arc::clone(&state.php_manager);
    let mcp_config = Arc::clone(&state.config);
    let mcp_engine = Arc::clone(&state.php_engine);
    tokio::spawn(async move {
        let mcp_server_config = StreamableHttpServerConfig::default();

        let sup = mcp_supervisor;
        let sm = mcp_site_manager;
        let pm = mcp_php_manager;
        let cfg = mcp_config;
        let engine = mcp_engine;

        let service: StreamableHttpService<HearthMcpServer, LocalSessionManager> =
            StreamableHttpService::new(
                move || {
                    Ok(HearthMcpServer::new(
                        Arc::clone(&sup),
                        Arc::clone(&sm),
                        Arc::clone(&pm),
                        Arc::clone(&cfg),
                        Arc::clone(&engine),
                    ))
                },
                Arc::new(LocalSessionManager::default()),
                mcp_server_config,
            );

        let router = axum::Router::new().nest_service("/mcp", service);

        let bind_addr = format!("127.0.0.1:{mcp_port}");
        match tokio::net::TcpListener::bind(&bind_addr).await {
            Ok(tcp_listener) => {
                info!(port = mcp_port, "MCP Streamable HTTP server listening");
                if let Err(e) = axum::serve(tcp_listener, router).await {
                    error!(error = %e, "MCP server stopped with error");
                }
            }
            Err(e) => {
                warn!(
                    port = mcp_port,
                    error = %e,
                    "failed to bind MCP server — MCP tools unavailable"
                );
            }
        }
    });

    // Handle client connections
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let st = Arc::clone(&state);
                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, st).await {
                        error!(error = %e, "client handler error");
                    }
                });
            }
            Err(e) => {
                error!(error = %e, "accept error");
            }
        }
    }
}

async fn handle_client(
    stream: tokio::net::UnixStream,
    state: Arc<DaemonState>,
) -> anyhow::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    while reader.read_line(&mut line).await? > 0 {
        let request: DaemonRequest = serde_json::from_str(line.trim())?;
        let response = process_request(request, &state).await;

        let response_json = serde_json::to_string(&response)?;
        writer.write_all(response_json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;

        line.clear();
    }

    Ok(())
}

/// Parse a CLI engine token (`mysql`, `pg`, `postgres`, ...) into a DB
/// `ServiceKind`. Rejects non-DB kinds (e.g. `nginx`) so `hearth db start nginx`
/// fails fast.
fn parse_db_engine(name: &str) -> Result<ServiceKind, String> {
    let kind: ServiceKind = name
        .parse()
        .map_err(|e: String| e)?;
    if !kind.is_db() {
        return Err(format!("{name} is not a DB engine"));
    }
    Ok(kind)
}

fn engines_for(target: Option<String>) -> Result<Vec<ServiceKind>, String> {
    match target {
        None => Ok(ServiceKind::db_engines().to_vec()),
        Some(name) => Ok(vec![parse_db_engine(&name)?]),
    }
}

fn port_for(config: &HearthConfig, kind: ServiceKind) -> u16 {
    match kind {
        ServiceKind::Mysql => config.mysql_port,
        ServiceKind::Postgresql => config.postgres_port,
        ServiceKind::Redis => config.redis_port,
        _ => 0,
    }
}

fn data_dir_for(kind: ServiceKind) -> PathBuf {
    let segment = match kind {
        ServiceKind::Mysql => "mysql",
        ServiceKind::Postgresql => "postgresql",
        ServiceKind::Redis => "redis",
        _ => "unknown",
    };
    hearth_lib::data_dir().join(segment)
}

async fn db_start(state: &Arc<DaemonState>, engine: Option<String>) -> DaemonResponse {
    let kinds = match engines_for(engine) {
        Ok(k) => k,
        Err(e) => return DaemonResponse::Error { message: e },
    };

    // Snapshot ports + registration state under config + supervisor locks
    // (config first per lock ordering).
    let port_map: Vec<(ServiceKind, u16)> = {
        let cfg = state.config.lock().await;
        kinds.iter().map(|k| (*k, port_for(&cfg, *k))).collect()
    };

    let mut sup = state.supervisor.lock().await;
    let mut started: Vec<String> = Vec::new();

    for (kind, port) in port_map {
        let registered = sup.status().contains_key(&kind);
        // Already running? Treat as no-op.
        let already_running = matches!(
            sup.status().get(&kind),
            Some(ServiceState::Running { .. })
        );
        if already_running {
            started.push(format!("{kind} already running"));
            continue;
        }

        // P0 #5: re-probe inside the dispatch handler. Surfaces typed Conflict
        // for the "Herd-up-AFTER-Hearth" race so the CLI prints clearly instead
        // of burning the circuit breaker.
        if port != 0 && port_in_use("127.0.0.1", port) {
            return DaemonResponse::Conflict {
                engine: kind.name().to_string(),
                port,
                owner_hint: None,
            };
        }

        if !registered {
            // Engine module not landed yet OR binary not found. Soft-fail with
            // a clear message so smoke tests + downstream blocks can grep it.
            started.push(format!("{kind} not registered"));
            continue;
        }

        match sup.start_service(kind) {
            Ok(()) => started.push(format!("{kind} started")),
            Err(e) => started.push(format!("{kind} start failed: {e}")),
        }
    }

    DaemonResponse::Ok {
        message: Some(started.join("; ")),
    }
}

async fn db_stop(state: &Arc<DaemonState>, engine: Option<String>) -> DaemonResponse {
    let kinds = match engines_for(engine) {
        Ok(k) => k,
        Err(e) => return DaemonResponse::Error { message: e },
    };

    let mut sup = state.supervisor.lock().await;
    let mut stopped: Vec<String> = Vec::new();
    for kind in kinds {
        if !sup.status().contains_key(&kind) {
            stopped.push(format!("{kind} not registered"));
            continue;
        }
        match sup.stop_service(kind) {
            Ok(()) => stopped.push(format!("{kind} stopped")),
            Err(e) => stopped.push(format!("{kind} stop failed: {e}")),
        }
    }
    DaemonResponse::Ok {
        message: Some(stopped.join("; ")),
    }
}

async fn db_status(state: &Arc<DaemonState>) -> DaemonResponse {
    let ports: Vec<(ServiceKind, u16)> = {
        let cfg = state.config.lock().await;
        ServiceKind::db_engines()
            .iter()
            .map(|k| (*k, port_for(&cfg, *k)))
            .collect()
    };

    let sup = state.supervisor.lock().await;
    let mut engines: Vec<DbEngineStatus> = Vec::with_capacity(3);

    for (kind, port) in ports {
        let (state_str, pid) = match sup.status().get(&kind) {
            Some(ServiceState::Running { pid }) => ("running".to_string(), Some(*pid)),
            Some(ServiceState::Starting) => ("starting".to_string(), None),
            Some(ServiceState::Stopped) => ("stopped".to_string(), None),
            Some(ServiceState::Failed { reason }) => (format!("failed: {reason}"), None),
            None => ("not_registered".to_string(), None),
        };

        let port_busy = port_in_use("127.0.0.1", port);
        // If we're running this engine, "port busy" means *us*, not a conflict.
        let we_run_it = matches!(sup.status().get(&kind), Some(ServiceState::Running { .. }));
        let conflict_port = port_busy && !we_run_it;

        engines.push(DbEngineStatus {
            engine: kind.name().to_string(),
            state: state_str,
            pid,
            port,
            data_dir: data_dir_for(kind).display().to_string(),
            conflict_port,
            owner_hint: None,
        });
    }

    DaemonResponse::DbStatus { engines }
}

async fn process_request(
    request: DaemonRequest,
    state: &Arc<DaemonState>,
) -> DaemonResponse {
    match request {
        DaemonRequest::Ping => DaemonResponse::Pong,

        DaemonRequest::Start => {
            let mut sup = state.supervisor.lock().await;
            match sup.start_all() {
                Ok(()) => DaemonResponse::Ok {
                    message: Some("All services started".to_string()),
                },
                Err(e) => DaemonResponse::Error {
                    message: e.to_string(),
                },
            }
        }

        DaemonRequest::Stop => {
            let mut sup = state.supervisor.lock().await;
            match sup.stop_all() {
                Ok(()) => DaemonResponse::Ok {
                    message: Some("All services stopped".to_string()),
                },
                Err(e) => DaemonResponse::Error {
                    message: e.to_string(),
                },
            }
        }

        DaemonRequest::Status => {
            let sup = state.supervisor.lock().await;
            let services = sup
                .services_detailed()
                .into_iter()
                .map(|d| hearth_lib::socket::ServiceStatus {
                    name: d.display_name,
                    state: format!("{:?}", d.state),
                    pid: match d.state {
                        hearth_lib::service::ServiceState::Running { pid } => Some(pid),
                        _ => None,
                    },
                })
                .collect();

            DaemonResponse::Status { services }
        }

        DaemonRequest::Link { path: _, name } => {
            match hearth_lib::valet::ValetCli::link(name.as_deref()) {
                Ok(msg) => DaemonResponse::Ok { message: Some(msg) },
                Err(e) => DaemonResponse::Error { message: e.to_string() },
            }
        }

        DaemonRequest::Unlink { name } => {
            match hearth_lib::valet::ValetCli::unlink(&name) {
                Ok(()) => DaemonResponse::Ok { message: Some(format!("Unlinked {}", name)) },
                Err(e) => DaemonResponse::Error { message: e.to_string() },
            }
        }

        DaemonRequest::Secure { name } => {
            match hearth_lib::valet::ValetCli::secure(&name) {
                Ok(()) => DaemonResponse::Ok { message: Some(format!("Secured {}", name)) },
                Err(e) => DaemonResponse::Error { message: e.to_string() },
            }
        }

        DaemonRequest::Unsecure { name } => {
            match hearth_lib::valet::ValetCli::unsecure(&name) {
                Ok(()) => DaemonResponse::Ok { message: Some(format!("Unsecured {}", name)) },
                Err(e) => DaemonResponse::Error { message: e.to_string() },
            }
        }

        DaemonRequest::Sites => {
            let sm = state.site_manager.lock().await;
            match sm.list_sites() {
                Ok(sites) => {
                    let site_infos = sites
                        .into_iter()
                        .map(|site| hearth_lib::socket::SiteInfo {
                            name: site.name,
                            path: site.path.to_string_lossy().to_string(),
                            secured: site.secured,
                            php_version: site.php_version,
                        })
                        .collect();
                    DaemonResponse::Sites { sites: site_infos }
                }
                Err(e) => DaemonResponse::Error { message: e.to_string() },
            }
        }

        DaemonRequest::Park { path } => {
            match hearth_lib::valet::ValetCli::park(&path) {
                Ok(msg) => {
                    // Lock config (first in ordering)
                    let mut cfg = state.config.lock().await;
                    let park_path = PathBuf::from(&path);
                    if !cfg.parked_paths.contains(&park_path) {
                        cfg.parked_paths.push(park_path);
                        if let Err(e) = cfg.save_to(&state.config_path) {
                            return DaemonResponse::Error {
                                message: format!("parked but failed to save config: {e}"),
                            };
                        }
                    }
                    DaemonResponse::Ok { message: Some(msg) }
                }
                Err(e) => DaemonResponse::Error { message: e.to_string() },
            }
        }

        DaemonRequest::PhpList => {
            // Lock config first, then php_manager (ordering: config -> php_manager)
            let cfg = state.config.lock().await;
            let pm = state.php_manager.lock().await;
            let versions = pm
                .installed_versions_with_paths()
                .into_iter()
                .map(|(version, path)| hearth_lib::socket::PhpVersionInfo {
                    active: version == cfg.default_php,
                    version,
                    path: path.to_string_lossy().to_string(),
                })
                .collect();
            DaemonResponse::PhpVersions { versions }
        }

        DaemonRequest::Restart { service } => {
            let mut sup = state.supervisor.lock().await;
            match service {
                None => {
                    if let Err(e) = sup.stop_all() {
                        return DaemonResponse::Error {
                            message: format!("stop failed: {e}"),
                        };
                    }
                    match sup.start_all() {
                        Ok(()) => DaemonResponse::Ok {
                            message: Some("All services restarted".to_string()),
                        },
                        Err(e) => DaemonResponse::Error {
                            message: format!("start failed: {e}"),
                        },
                    }
                }
                Some(name) => {
                    let kind: hearth_lib::service::ServiceKind = match name.parse() {
                        Ok(k) => k,
                        Err(e) => return DaemonResponse::Error { message: e },
                    };
                    if let Err(e) = sup.stop_service(kind) {
                        return DaemonResponse::Error {
                            message: format!("stop {name} failed: {e}"),
                        };
                    }
                    match sup.start_service(kind) {
                        Ok(()) => DaemonResponse::Ok {
                            message: Some(format!("Restarted {name}")),
                        },
                        Err(e) => DaemonResponse::Error {
                            message: format!("start {name} failed: {e}"),
                        },
                    }
                }
            }
        }

        DaemonRequest::PhpConfig { version, key, value } => {
            // Legacy protocol window: mapped to a V2 Set, answered with a
            // legacy Ok/Error response (old CLIs keep working).
            php_config::php_config_legacy(state, version, key, value).await
        }

        DaemonRequest::PhpConfigV2 { action } => php_config::php_config(state, action).await,

        DaemonRequest::DbStart { engine } => {
            db_start(state, engine).await
        }

        DaemonRequest::DbStop { engine } => {
            db_stop(state, engine).await
        }

        DaemonRequest::DbStatus => {
            db_status(state).await
        }

        DaemonRequest::Add {
            package,
            site_path,
            cwd: cli_cwd,
            answers,
            no_supervise,
            dry_run,
        } => {
            // Hold the add lock for the duration so concurrent Add invocations
            // (e.g. scripted `hearth add horizon && hearth add telescope`) don't race
            // on .env editing or supervisor registration.
            let _guard = state.add_lock.lock().await;

            // Snapshot config (read-only fields) under the config lock, then drop.
            // Lock order: config -> site_manager. apply_recipe runs lock-free.
            let (composer_phar_cfg, default_php_clone) = {
                let cfg = state.config.lock().await;
                (cfg.composer_phar.clone(), cfg.default_php.clone())
            };

            // Compose-phar fallback: if config doesn't have it, probe the host. Lets
            // existing installs work without re-running `hearth install`.
            let composer_phar = composer_phar_cfg
                .or_else(hearth_lib::add::composer::resolve_composer_phar);

            // Resolve site context under site_manager lock.
            let site_path_buf = PathBuf::from(&site_path);
            // Use the CLI's cwd, not the daemon's. Daemon may be running from `/` via
            // launchd / `brew services`; the CLI knows where the user actually invoked.
            let cwd = if cli_cwd.is_empty() {
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
            } else {
                PathBuf::from(&cli_cwd)
            };
            let snapshot_config = HearthConfig {
                default_php: default_php_clone,
                ..HearthConfig::default()
            };
            let config_dir = hearth_lib::config_dir();
            let ctx_resolution = {
                let sm = state.site_manager.lock().await;
                let explicit: Option<&std::path::Path> = if site_path.is_empty() {
                    None
                } else {
                    Some(&site_path_buf)
                };
                hearth_lib::add::site_context::resolve(
                    explicit,
                    &cwd,
                    &sm,
                    &snapshot_config,
                    &config_dir,
                )
            };
            let site_ctx = match ctx_resolution {
                Ok(s) => s,
                Err(e) => {
                    return DaemonResponse::Error {
                        message: format!("hearth add {package}: {e}"),
                    };
                }
            };

            // Build the per-request log file path so the CLI can `tail -f` it.
            let log_path = hearth_lib::log_dir().join(format!("add-{package}.log"));

            // Pre-launch reconcile, HARD-GATED (F4): composer/artisan must
            // never run against unreconciled channel state. Probe cache makes
            // a repeat call cheap.
            if !dry_run
                && let Err(e) = hearth_lib::php::engine::require_reconciled(
                    state
                        .php_engine
                        .apply(hearth_lib::socket::PhpConfigAction::Sync)
                        .await,
                )
            {
                return DaemonResponse::Error {
                    message: format!("hearth add {package}: {e}"),
                };
            }

            let recipe_ctx = hearth_lib::add::recipe::RecipeContext {
                site_path: site_ctx.site.path.clone(),
                site_name: site_ctx.site.name.clone(),
                php_binary: site_ctx.php_binary.clone(),
                composer_phar,
                no_supervise,
                dry_run,
                log_path: Some(log_path),
                php_version: Some(site_ctx.php_version.clone()),
                scan_env: Some(hearth_lib::php::scan_dir_env(
                    &config_dir,
                    &site_ctx.php_version,
                )),
                now: chrono::Utc::now(),
            };

            let outcome = match hearth_lib::add::apply_recipe(&package, &recipe_ctx, &answers) {
                Ok(o) => o,
                Err(e) => {
                    return DaemonResponse::Error {
                        message: format!("hearth add {package} failed: {e}"),
                    };
                }
            };

            // Supervised packages (Horizon, Reverb) are persisted to config and registered
            // with the supervisor. Telescope/Pulse return supervised = None and skip both.
            if let Some(ref spec) = outcome.supervised
                && !dry_run
            {
                let kind = match spec.package.as_str() {
                    "horizon" => ServiceKind::Horizon,
                    "reverb" => ServiceKind::Reverb,
                    other => {
                        return DaemonResponse::Error {
                            message: format!("unknown supervised package: {other}"),
                        };
                    }
                };

                // Multi-site check + persist under config lock. v0.3.0 ships single-instance
                // Horizon and Reverb; multi-site is v0.3.1.
                let now = chrono::Utc::now();
                let pkg = AddedPackage {
                    package: spec.package.clone(),
                    site_path: spec.cwd.clone(),
                    site_name: spec.site_name.clone(),
                    command: spec.command.clone(),
                    args: spec.args.clone(),
                    php_version: spec.php_version.clone(),
                    installed_at: now,
                };
                {
                    let mut cfg = state.config.lock().await;
                    let existing_other_site = cfg.added_packages.iter().find(|p| {
                        p.package == spec.package && p.site_name != spec.site_name
                    });
                    if let Some(existing) = existing_other_site {
                        return DaemonResponse::Error {
                            message: format!(
                                "{} is already supervised for site `{}`; v0.3.0 ships \
                                 single-site only — multi-site support arrives in v0.3.1",
                                spec.package, existing.site_name
                            ),
                        };
                    }
                    // Remove an existing same-site entry (idempotent re-run) before pushing.
                    cfg.added_packages
                        .retain(|p| !(p.package == spec.package && p.site_name == spec.site_name));
                    cfg.added_packages.push(pkg.clone());
                    if let Err(e) = cfg.save_to(&state.config_path) {
                        return DaemonResponse::Error {
                            message: format!(
                                "recipe ran but failed to persist AddedPackage: {e}"
                            ),
                        };
                    }
                }
                // Drop config lock before acquiring supervisor (lock order).

                if !no_supervise {
                    let mut sup = state.supervisor.lock().await;
                    // Same constructor as boot rehydration — worker gets the
                    // Belt-E scan-dir env for its resolved PHP version.
                    let svc = hearth_lib::service::manager::worker_service(
                        kind,
                        &pkg,
                        &config_dir,
                        &ProviderRoots::detect(),
                    );
                    sup.register(svc);
                    if let Err(e) = sup.start_service(kind) {
                        warn!(
                            package = %package,
                            error = %e,
                            "supervised worker registered but failed to start; \
                             will retry on next health check tick",
                        );
                    }
                }
            }

            DaemonResponse::Ok {
                message: Some(hearth_lib::add::format_outcome(&package, &outcome)),
            }
        }

        DaemonRequest::PhpSwitch { version } => {
            // Shared switch path (engine-injected paths, hard-gated reconcile,
            // FPM service rebuilt with the new version's env — F3/F4).
            match hearth_lib::php::engine::switch_php_version(
                &state.supervisor,
                &state.php_engine,
                &version,
            )
            .await
            {
                Ok(message) => DaemonResponse::Ok {
                    message: Some(message),
                },
                Err(message) => DaemonResponse::Error { message },
            }
        }
    }
}
