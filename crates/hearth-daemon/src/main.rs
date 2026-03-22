use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use hearth_lib::config::HearthConfig;
use hearth_lib::mcp::HearthMcpServer;
use hearth_lib::php::PhpManager;
use hearth_lib::service::manager::default_services;
use hearth_lib::service::supervisor::ServiceSupervisor;
use hearth_lib::site::SiteManager;
use hearth_lib::socket::{DaemonRequest, DaemonResponse};

use rmcp::transport::StreamableHttpServerConfig;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::StreamableHttpService;

/// Shared state accessible by all daemon request handlers.
///
/// Each field is individually wrapped in `Arc<Mutex<T>>` so that the MCP
/// server can hold cloned references without locking the entire state.
///
/// Lock ordering convention (when multiple locks are needed):
///   `config` -> `php_manager` -> `site_manager` -> `supervisor`
struct DaemonState {
    supervisor: Arc<Mutex<ServiceSupervisor>>,
    config: Arc<Mutex<HearthConfig>>,
    site_manager: Arc<Mutex<SiteManager>>,
    php_manager: Arc<Mutex<PhpManager>>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("hearth=info")
        .init();

    info!("Hearth daemon starting...");

    // Load config
    let config = HearthConfig::load().context("Failed to load config")?;

    // Ensure directories exist
    let config_dir = hearth_lib::config_dir();
    std::fs::create_dir_all(hearth_lib::run_dir())?;
    std::fs::create_dir_all(hearth_lib::log_dir())?;

    // Build supervisor with default services
    let mut supervisor = ServiceSupervisor::new();
    for svc in default_services(&config, &config_dir) {
        supervisor.register(svc);
    }

    // Valet home for site enumeration
    let valet_home = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".config/valet");
    let tld = config.tld.clone();

    // Capture ports before wrapping config in Arc<Mutex<>>
    let dump_port = config.dump_port;
    let mcp_port = config.mcp_port;

    let state = Arc::new(DaemonState {
        supervisor: Arc::new(Mutex::new(supervisor)),
        config: Arc::new(Mutex::new(config)),
        site_manager: Arc::new(Mutex::new(SiteManager::new(valet_home, tld))),
        php_manager: Arc::new(Mutex::new(PhpManager::new(hearth_lib::config_dir()))),
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
    tokio::spawn(async move {
        let mcp_server_config = StreamableHttpServerConfig::default();

        let sup = mcp_supervisor;
        let sm = mcp_site_manager;
        let pm = mcp_php_manager;
        let cfg = mcp_config;

        let service: StreamableHttpService<HearthMcpServer, LocalSessionManager> =
            StreamableHttpService::new(
                move || {
                    Ok(HearthMcpServer::new(
                        Arc::clone(&sup),
                        Arc::clone(&sm),
                        Arc::clone(&pm),
                        Arc::clone(&cfg),
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
                .status()
                .iter()
                .map(|(kind, svc_state)| {
                    hearth_lib::socket::ServiceStatus {
                        name: kind.name().to_string(),
                        state: format!("{:?}", svc_state),
                        pid: match svc_state {
                            hearth_lib::service::ServiceState::Running { pid } => Some(*pid),
                            _ => None,
                        },
                    }
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
                        if let Err(e) = cfg.save() {
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
            // Lock ordering: config -> php_manager -> supervisor
            let resolved_version = {
                let cfg = state.config.lock().await;
                if version == "active" {
                    cfg.default_php.clone()
                } else {
                    version
                }
            };
            // Drop config lock before acquiring php_manager

            {
                let pm = state.php_manager.lock().await;
                if let Err(e) = pm.set_ini_value(&resolved_version, &key, &value) {
                    return DaemonResponse::Error { message: e.to_string() };
                }
            }
            // Drop php_manager lock before acquiring supervisor

            let mut sup = state.supervisor.lock().await;
            // Restart PHP-FPM to pick up INI changes
            if let Err(e) = sup.stop_service(hearth_lib::service::ServiceKind::PhpFpm) {
                return DaemonResponse::Error {
                    message: format!("INI updated but php-fpm stop failed: {e}"),
                };
            }
            if let Err(e) = sup.start_service(hearth_lib::service::ServiceKind::PhpFpm) {
                return DaemonResponse::Error {
                    message: format!("INI updated but php-fpm restart failed: {e}"),
                };
            }

            DaemonResponse::Ok {
                message: Some(format!("Set {key}={value} for PHP {resolved_version} and restarted php-fpm")),
            }
        }

        DaemonRequest::PhpSwitch { version } => {
            let config_dir = hearth_lib::config_dir();

            // Resolve the new PHP-FPM binary (no lock needed)
            let fpm_binary = match hearth_lib::php::resolver::resolve_phpfpm_binary(&version, &config_dir) {
                Some(path) => path,
                None => {
                    return DaemonResponse::Error {
                        message: format!("PHP {version} php-fpm binary not found"),
                    };
                }
            };

            // Lock supervisor (last in ordering — no other locks needed)
            let mut sup = state.supervisor.lock().await;

            // Stop current PHP-FPM
            let _ = sup.stop_service(hearth_lib::service::ServiceKind::PhpFpm);

            // Reconfigure with new binary
            sup.reconfigure_service(
                hearth_lib::service::ServiceKind::PhpFpm,
                fpm_binary.to_string_lossy().to_string(),
                vec![
                    "--nodaemonize".to_string(),
                    format!(
                        "--fpm-config={}",
                        config_dir.join("fpm/php-fpm.conf").display()
                    ),
                ],
            );

            // Start new PHP-FPM
            if let Err(e) = sup.start_service(hearth_lib::service::ServiceKind::PhpFpm) {
                return DaemonResponse::Error {
                    message: format!("failed to start php-fpm {version}: {e}"),
                };
            }

            // Drop supervisor lock before acquiring config lock
            drop(sup);

            // Update config
            let mut cfg = state.config.lock().await;
            cfg.default_php = version.clone();
            if let Err(e) = cfg.save() {
                return DaemonResponse::Error {
                    message: format!("switched but failed to save config: {e}"),
                };
            }

            DaemonResponse::Ok {
                message: Some(format!("Switched to PHP {version}")),
            }
        }
    }
}
