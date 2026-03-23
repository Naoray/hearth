use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use hearth_lib::socket::{DaemonRequest, DaemonResponse};

#[derive(Parser)]
#[command(name = "hearth", about = "Unified Laravel development command center")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start all services
    Start,
    /// Stop all services
    Stop,
    /// Restart services (optionally a specific one)
    Restart {
        /// Service name to restart (e.g., nginx, php-fpm)
        service: Option<String>,
    },
    /// Show status of all services
    Status,

    /// Link the current directory as a site
    Link {
        /// Custom name for the site (defaults to directory name)
        name: Option<String>,
    },
    /// Unlink a site
    Unlink {
        /// Site name to unlink
        name: String,
    },
    /// Park a directory so all subdirectories become sites (defaults to current directory)
    Park {
        /// Directory to park (defaults to current directory)
        path: Option<PathBuf>,
    },
    /// List all linked sites
    Sites,

    /// Secure a site with SSL
    Secure {
        /// Site name to secure
        name: String,
    },
    /// Remove SSL from a site
    Unsecure {
        /// Site name to unsecure
        name: String,
    },

    /// PHP version management
    Php {
        #[command(subcommand)]
        command: PhpCommands,
    },

    /// Laravel project management
    Laravel {
        #[command(subcommand)]
        command: LaravelCommands,
    },

    /// Open Mailpit UI in browser
    Mail,

    /// Stream dump server output
    Dump,

    /// Start MCP stdio bridge (for IDE integration)
    Mcp,

    /// Manage the hearth daemon process
    Daemon {
        #[command(subcommand)]
        command: DaemonCommands,
    },

    /// First-time setup (installs Valet, configures DNS, trusts CA)
    Install,
}

#[derive(Subcommand)]
enum DaemonCommands {
    /// Start the daemon in the background
    Start,
    /// Stop the running daemon
    Stop,
    /// Check if the daemon is running
    Status,
}

#[derive(Subcommand)]
enum PhpCommands {
    /// Switch to a PHP version (e.g., `hearth php use 8.4`)
    Use {
        /// PHP version (e.g., "8.4")
        version: String,
    },
    /// List installed PHP versions
    List,
    /// Set a php.ini config value (e.g., `hearth php config memory_limit 512M`)
    Config {
        /// INI key (e.g., memory_limit, upload_max_filesize)
        key: String,
        /// Value to set
        value: String,
    },
}

#[derive(Subcommand)]
enum LaravelCommands {
    /// Create a new Laravel project
    New {
        /// Project name
        name: String,
    },
    /// Update the Laravel installer
    Update,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let request = match cli.command {
        Commands::Start => DaemonRequest::Start,
        Commands::Stop => DaemonRequest::Stop,
        Commands::Restart { service } => DaemonRequest::Restart { service },
        Commands::Status => DaemonRequest::Status,
        Commands::Link { name } => DaemonRequest::Link {
            path: std::env::current_dir()?.to_string_lossy().to_string(),
            name,
        },
        Commands::Unlink { name } => DaemonRequest::Unlink { name },
        Commands::Park { path } => DaemonRequest::Park {
            path: path
                .unwrap_or(std::env::current_dir()?)
                .to_string_lossy()
                .to_string(),
        },
        Commands::Sites => DaemonRequest::Sites,
        Commands::Secure { name } => DaemonRequest::Secure { name },
        Commands::Unsecure { name } => DaemonRequest::Unsecure { name },
        Commands::Php { command } => match command {
            PhpCommands::Use { version } => DaemonRequest::PhpSwitch { version },
            PhpCommands::List => DaemonRequest::PhpList,
            PhpCommands::Config { key, value } => DaemonRequest::PhpConfig {
                version: "active".to_string(), // daemon resolves to current version
                key,
                value,
            },
        },
        Commands::Daemon { command } => {
            return run_daemon(command).await;
        }
        Commands::Install => {
            // Install is handled directly, not through the daemon
            return run_install().await;
        }
        Commands::Laravel { command } => {
            return run_laravel(command).await;
        }
        Commands::Mail => {
            // Open Mailpit UI directly
            let config = hearth_lib::config::HearthConfig::load()?;
            let url = format!("http://127.0.0.1:{}", config.mail_ui_port);
            opener::open(&url).context("Failed to open browser")?;
            println!("Opened Mailpit at {}", url);
            return Ok(());
        }
        Commands::Dump => {
            let config = hearth_lib::config::HearthConfig::load()?;
            println!("Connecting to dump server on port {}...", config.dump_port);
            hearth_lib::dump::stream_dumps(config.dump_port).await?;
            return Ok(());
        }
        Commands::Mcp => {
            let config = hearth_lib::config::HearthConfig::load()?;
            let mcp_url = format!("http://127.0.0.1:{}/mcp", config.mcp_port);
            eprintln!("Hearth MCP stdio bridge → {}", mcp_url);
            return run_mcp_bridge(&mcp_url).await;
        }
    };

    let response = send_to_daemon(request).await?;
    print_response(response);

    Ok(())
}

async fn send_to_daemon(request: DaemonRequest) -> anyhow::Result<DaemonResponse> {
    let socket_path = hearth_lib::socket::socket_path();

    let stream = UnixStream::connect(&socket_path)
        .await
        .context("Daemon not running. Start with: hearth-daemon")?;

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Send request
    let request_json = serde_json::to_string(&request)?;
    writer.write_all(request_json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;

    // Read response
    let mut line = String::new();
    reader.read_line(&mut line).await?;

    let response: DaemonResponse = serde_json::from_str(line.trim())?;
    Ok(response)
}

fn print_response(response: DaemonResponse) {
    match response {
        DaemonResponse::Ok { message } => {
            if let Some(msg) = message {
                println!("{}", msg);
            }
        }
        DaemonResponse::Error { message } => {
            eprintln!("Error: {}", message);
            std::process::exit(1);
        }
        DaemonResponse::Status { services } => {
            println!("{:<15} {:<15} {}", "SERVICE", "STATE", "PID");
            println!("{}", "-".repeat(40));
            for svc in services {
                println!(
                    "{:<15} {:<15} {}",
                    svc.name,
                    svc.state,
                    svc.pid.map(|p| p.to_string()).unwrap_or_default()
                );
            }
        }
        DaemonResponse::Sites { sites } => {
            println!("{:<30} {:<6} {}", "SITE", "SSL", "PATH");
            println!("{}", "-".repeat(70));
            for site in sites {
                println!(
                    "{:<30} {:<6} {}",
                    site.name,
                    if site.secured { "yes" } else { "no" },
                    site.path
                );
            }
        }
        DaemonResponse::PhpVersions { versions } => {
            for v in versions {
                let marker = if v.active { " *" } else { "" };
                println!("PHP {}{} — {}", v.version, marker, v.path);
            }
        }
        DaemonResponse::Pong => println!("Daemon is running"),
    }
}

async fn run_install() -> anyhow::Result<()> {
    println!("Hearth — first-time setup");
    println!("========================");

    // 1. Ensure Valet is installed
    println!("\n[1/3] Checking Valet...");
    if hearth_lib::valet::ValetCli::is_installed() {
        println!("  Valet is already installed.");
    } else {
        println!("  Installing Valet...");
        hearth_lib::valet::ValetCli::install()?;
        println!("  Valet installed.");
    }

    // 2. Create config directory
    println!("\n[2/3] Creating config directory...");
    let config_dir = hearth_lib::config_dir();
    std::fs::create_dir_all(&config_dir)?;
    let config = hearth_lib::config::HearthConfig::default();
    config.save()?;
    println!("  Config saved to {}", config_dir.display());

    // 3. Setup DNS resolver (requires sudo)
    println!("\n[3/3] Setting up DNS resolver (requires sudo)...");

    let herd_running = hearth_lib::service::manager::is_herd_running();
    let resolver_content = if herd_running {
        // Herd manages its own dnsmasq on default port 53 — don't override it
        println!("  Herd detected — writing /etc/resolver/test without custom port (using Herd's dnsmasq on port 53)");
        "nameserver 127.0.0.1\n".to_string()
    } else {
        println!("  Writing /etc/resolver/test with port {}", config.dns_port);
        format!("nameserver 127.0.0.1\nport {}\n", config.dns_port)
    };

    let status = std::process::Command::new("sudo")
        .args(["mkdir", "-p", "/etc/resolver"])
        .status()?;
    if !status.success() {
        anyhow::bail!("Failed to create /etc/resolver/");
    }

    let status = std::process::Command::new("sudo")
        .args(["bash", "-c", &format!(
            "echo '{}' > /etc/resolver/test",
            resolver_content.trim()
        )])
        .status()?;
    if !status.success() {
        anyhow::bail!("Failed to write /etc/resolver/test");
    }

    println!("\nSetup complete! Run `hearth daemon start` to start the daemon,");
    println!("then use `hearth start` to bring up services.");
    Ok(())
}

async fn run_daemon(command: DaemonCommands) -> anyhow::Result<()> {
    match command {
        DaemonCommands::Start => daemon_start().await,
        DaemonCommands::Stop => daemon_stop().await,
        DaemonCommands::Status => daemon_status().await,
    }
}

/// Read the daemon PID file and check if the process is alive.
fn daemon_pid() -> Option<u32> {
    let pid_path = hearth_lib::run_dir().join("daemon.pid");
    let content = std::fs::read_to_string(&pid_path).ok()?;
    let pid: u32 = content.trim().parse().ok()?;

    // Signal 0 checks if process is alive without actually sending a signal
    let nix_pid = nix::unistd::Pid::from_raw(pid as i32);
    let alive = nix::sys::signal::kill(nix_pid, None).is_ok();
    if alive { Some(pid) } else { None }
}

async fn daemon_start() -> anyhow::Result<()> {
    if let Some(pid) = daemon_pid() {
        println!("Daemon is already running (PID {})", pid);
        return Ok(());
    }

    let log_dir = hearth_lib::log_dir();
    std::fs::create_dir_all(&log_dir)?;

    let stdout_file = std::fs::File::create(log_dir.join("daemon.out.log"))?;
    let stderr_file = std::fs::File::create(log_dir.join("daemon.err.log"))?;

    let child = std::process::Command::new("hearth-daemon")
        .stdin(std::process::Stdio::null())
        .stdout(stdout_file)
        .stderr(stderr_file)
        .spawn()
        .context("Failed to start hearth-daemon. Is it installed and on PATH?")?;

    let pid = child.id();
    println!("Daemon starting (PID {})...", pid);

    // Wait briefly for the socket to appear
    let socket_path = hearth_lib::socket::socket_path();
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if socket_path.exists() {
            println!("Daemon is running.");
            return Ok(());
        }
    }

    println!("Daemon started but socket not yet available. Check logs at {}", log_dir.display());
    Ok(())
}

async fn daemon_stop() -> anyhow::Result<()> {
    let Some(pid) = daemon_pid() else {
        println!("Daemon is not running.");
        return Ok(());
    };

    // Gracefully stop all supervised services before killing the daemon
    if let Ok(_) = send_to_daemon(DaemonRequest::Stop).await {
        println!("Services stopped.");
    }

    // Send SIGTERM to the daemon process
    let nix_pid = nix::unistd::Pid::from_raw(pid as i32);
    nix::sys::signal::kill(nix_pid, nix::sys::signal::Signal::SIGTERM)
        .context(format!("Failed to send SIGTERM to daemon (PID {})", pid))?;

    println!("Stopping daemon (PID {})...", pid);

    // Wait for the process to exit (up to 5 seconds)
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let alive = nix::sys::signal::kill(nix_pid, None).is_ok();
        if !alive {
            // Clean up stale files
            let _ = std::fs::remove_file(hearth_lib::run_dir().join("daemon.pid"));
            let _ = std::fs::remove_file(hearth_lib::socket::socket_path());
            println!("Daemon stopped.");
            return Ok(());
        }
    }

    println!("Daemon did not stop within 5 seconds. You may need to kill PID {} manually.", pid);
    Ok(())
}

async fn daemon_status() -> anyhow::Result<()> {
    match daemon_pid() {
        Some(pid) => {
            println!("Daemon is running (PID {})", pid);

            // Try to get service status via socket
            match send_to_daemon(DaemonRequest::Ping).await {
                Ok(DaemonResponse::Pong) => println!("Socket is responsive."),
                Ok(_) => println!("Socket connected but unexpected response."),
                Err(_) => println!("Socket is not responsive."),
            }
        }
        None => {
            println!("Daemon is not running.");
        }
    }
    Ok(())
}

async fn run_laravel(command: LaravelCommands) -> anyhow::Result<()> {
    match command {
        LaravelCommands::New { name } => {
            println!("Creating new Laravel project: {}", name);
            let status = std::process::Command::new("laravel")
                .args(["new", &name])
                .status()
                .context("Failed to run laravel installer. Run: composer global require laravel/installer")?;

            if !status.success() {
                anyhow::bail!("laravel new failed");
            }
            println!("Project created at ./{}", name);
        }
        LaravelCommands::Update => {
            println!("Updating Laravel installer...");
            let status = std::process::Command::new("composer")
                .args(["global", "update", "laravel/installer"])
                .status()
                .context("Failed to run composer")?;

            if !status.success() {
                anyhow::bail!("Installer update failed");
            }
            println!("Laravel installer updated.");
        }
    }
    Ok(())
}

/// Stdio-to-HTTP bridge for MCP clients that only support stdio transport.
///
/// Reads JSON-RPC from stdin, POSTs to the daemon's Streamable HTTP endpoint,
/// writes responses to stdout. Exits on stdin EOF.
///
/// Known limitation: uses `resp.text().await` which reads the full response body.
/// MCP Streamable HTTP can use SSE for server-to-client streaming — if the server
/// ever sends multi-event SSE streams (e.g., streaming tool results), this bridge
/// will buffer the entire stream before writing to stdout. Current tools all return
/// single-event responses, so this works fine for now. To support SSE streaming,
/// read the response as a byte stream and forward line-by-line.
async fn run_mcp_bridge(mcp_url: &str) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();

    while reader.read_line(&mut line).await? > 0 {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            line.clear();
            continue;
        }

        match client
            .post(mcp_url)
            .header("Content-Type", "application/json")
            .body(trimmed.to_string())
            .send()
            .await
        {
            Ok(resp) => {
                let body = resp.text().await.unwrap_or_default();
                println!("{}", body);
            }
            Err(e) => {
                eprintln!("MCP bridge error: {}", e);
            }
        }

        line.clear();
    }

    Ok(())
}
