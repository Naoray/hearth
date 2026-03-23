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

    /// First-time setup (installs Valet, configures DNS, trusts CA)
    Install,
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
    println!("  This writes /etc/resolver/test with port {}", config.dns_port);
    let resolver_content = format!(
        "nameserver 127.0.0.1\nport {}\n",
        config.dns_port
    );

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

    println!("\nSetup complete! Run `hearth-daemon` to start the supervisor,");
    println!("then use `hearth start` to bring up services.");
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
