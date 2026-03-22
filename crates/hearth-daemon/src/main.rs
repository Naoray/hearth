use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tracing::{error, info};

use hearth_lib::config::HearthConfig;
use hearth_lib::php::PhpManager;
use hearth_lib::service::manager::default_services;
use hearth_lib::service::supervisor::ServiceSupervisor;
use hearth_lib::site::SiteManager;
use hearth_lib::socket::{DaemonRequest, DaemonResponse};

/// Shared state accessible by all daemon request handlers.
struct DaemonState {
    supervisor: ServiceSupervisor,
    config: HearthConfig,
    site_manager: SiteManager,
    php_manager: PhpManager,
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

    let state = Arc::new(Mutex::new(DaemonState {
        supervisor,
        site_manager: SiteManager::new(valet_home, tld),
        php_manager: PhpManager::new(hearth_lib::config_dir()),
        config,
    }));

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
    let dump_port = state.lock().await.config.dump_port;
    tokio::spawn(async move {
        if let Err(e) = hearth_lib::dump::run_dump_server(dump_port).await {
            error!(error = %e, "dump server failed");
        }
    });

    // Spawn health check loop
    let health_state = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let mut s = health_state.lock().await;
            s.supervisor.health_check();
        }
    });

    // Handle client connections
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let st = state.clone();
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
    state: Arc<Mutex<DaemonState>>,
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
    state: &Arc<Mutex<DaemonState>>,
) -> DaemonResponse {
    match request {
        DaemonRequest::Ping => DaemonResponse::Pong,

        DaemonRequest::Start => {
            let mut s = state.lock().await;
            match s.supervisor.start_all() {
                Ok(()) => DaemonResponse::Ok {
                    message: Some("All services started".to_string()),
                },
                Err(e) => DaemonResponse::Error {
                    message: e.to_string(),
                },
            }
        }

        DaemonRequest::Stop => {
            let mut s = state.lock().await;
            match s.supervisor.stop_all() {
                Ok(()) => DaemonResponse::Ok {
                    message: Some("All services stopped".to_string()),
                },
                Err(e) => DaemonResponse::Error {
                    message: e.to_string(),
                },
            }
        }

        DaemonRequest::Status => {
            let s = state.lock().await;
            let services = s
                .supervisor
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
            let s = state.lock().await;
            match s.site_manager.list_sites() {
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
                    let mut s = state.lock().await;
                    let park_path = PathBuf::from(&path);
                    if !s.config.parked_paths.contains(&park_path) {
                        s.config.parked_paths.push(park_path);
                        if let Err(e) = s.config.save() {
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
            let s = state.lock().await;
            let versions = s
                .php_manager
                .installed_versions_with_paths()
                .into_iter()
                .map(|(version, path)| hearth_lib::socket::PhpVersionInfo {
                    active: version == s.config.default_php,
                    version,
                    path: path.to_string_lossy().to_string(),
                })
                .collect();
            DaemonResponse::PhpVersions { versions }
        }

        DaemonRequest::Restart { service } => {
            let mut s = state.lock().await;
            match service {
                None => {
                    if let Err(e) = s.supervisor.stop_all() {
                        return DaemonResponse::Error {
                            message: format!("stop failed: {e}"),
                        };
                    }
                    match s.supervisor.start_all() {
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
                    if let Err(e) = s.supervisor.stop_service(kind) {
                        return DaemonResponse::Error {
                            message: format!("stop {name} failed: {e}"),
                        };
                    }
                    match s.supervisor.start_service(kind) {
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
            let mut s = state.lock().await;
            let resolved_version = if version == "active" {
                s.config.default_php.clone()
            } else {
                version
            };

            if let Err(e) = s.php_manager.set_ini_value(&resolved_version, &key, &value) {
                return DaemonResponse::Error { message: e.to_string() };
            }

            // Restart PHP-FPM to pick up INI changes
            if let Err(e) = s.supervisor.stop_service(hearth_lib::service::ServiceKind::PhpFpm) {
                return DaemonResponse::Error {
                    message: format!("INI updated but php-fpm stop failed: {e}"),
                };
            }
            if let Err(e) = s.supervisor.start_service(hearth_lib::service::ServiceKind::PhpFpm) {
                return DaemonResponse::Error {
                    message: format!("INI updated but php-fpm restart failed: {e}"),
                };
            }

            DaemonResponse::Ok {
                message: Some(format!("Set {key}={value} for PHP {resolved_version} and restarted php-fpm")),
            }
        }

        DaemonRequest::PhpSwitch { version } => {
            let mut s = state.lock().await;
            let config_dir = hearth_lib::config_dir();

            // Resolve the new PHP-FPM binary
            let fpm_binary = match hearth_lib::php::resolver::resolve_phpfpm_binary(&version, &config_dir) {
                Some(path) => path,
                None => {
                    return DaemonResponse::Error {
                        message: format!("PHP {version} php-fpm binary not found"),
                    };
                }
            };

            // Stop current PHP-FPM
            let _ = s.supervisor.stop_service(hearth_lib::service::ServiceKind::PhpFpm);

            // Reconfigure with new binary
            s.supervisor.reconfigure_service(
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
            if let Err(e) = s.supervisor.start_service(hearth_lib::service::ServiceKind::PhpFpm) {
                return DaemonResponse::Error {
                    message: format!("failed to start php-fpm {version}: {e}"),
                };
            }

            // Update config
            s.config.default_php = version.clone();
            if let Err(e) = s.config.save() {
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
