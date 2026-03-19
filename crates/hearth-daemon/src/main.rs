use std::sync::Arc;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tracing::{error, info};

use hearth_lib::config::HearthConfig;
use hearth_lib::service::manager::default_services;
use hearth_lib::service::supervisor::ServiceSupervisor;
use hearth_lib::socket::{DaemonRequest, DaemonResponse};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("hearth=info")
        .init();

    info!("Hearth daemon starting...");

    // Load config
    let config = HearthConfig::load().context("Failed to load config")?;

    // Ensure directories exist
    let _config_dir = hearth_lib::config_dir();
    std::fs::create_dir_all(hearth_lib::run_dir())?;
    std::fs::create_dir_all(hearth_lib::log_dir())?;

    // Build supervisor with default services
    let mut supervisor = ServiceSupervisor::new();
    for svc in default_services(&config) {
        supervisor.register(svc);
    }

    let supervisor = Arc::new(Mutex::new(supervisor));

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

    // Spawn health check loop
    let health_supervisor = supervisor.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let mut sup = health_supervisor.lock().await;
            sup.health_check();
        }
    });

    // Handle client connections
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let sup = supervisor.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, sup).await {
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
    supervisor: Arc<Mutex<ServiceSupervisor>>,
) -> anyhow::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    while reader.read_line(&mut line).await? > 0 {
        let request: DaemonRequest = serde_json::from_str(line.trim())?;
        let response = process_request(request, &supervisor).await;

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
    supervisor: &Arc<Mutex<ServiceSupervisor>>,
) -> DaemonResponse {
    match request {
        DaemonRequest::Ping => DaemonResponse::Pong,

        DaemonRequest::Start => {
            let mut sup = supervisor.lock().await;
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
            let mut sup = supervisor.lock().await;
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
            let sup = supervisor.lock().await;
            let services = sup
                .status()
                .iter()
                .map(|(kind, state)| {
                    hearth_lib::socket::ServiceStatus {
                        name: kind.name().to_string(),
                        state: format!("{:?}", state),
                        pid: match state {
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

        // TODO: implement remaining handlers
        _ => DaemonResponse::Error {
            message: "Not yet implemented".to_string(),
        },
    }
}
