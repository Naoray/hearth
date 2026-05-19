use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use hearth_lib::add::AddAnswers;
use hearth_lib::socket::{DaemonRequest, DaemonResponse, DbEngineStatus};

/// Closed enum of packages accepted by `hearth add`. Validated at parse time so users
/// get a clean clap error rather than a runtime "unknown package".
#[derive(Clone, Copy, Debug, ValueEnum)]
enum AddPackage {
    Horizon,
    Telescope,
    Pulse,
    Reverb,
}

impl AddPackage {
    fn as_key(&self) -> &'static str {
        match self {
            AddPackage::Horizon => "horizon",
            AddPackage::Telescope => "telescope",
            AddPackage::Pulse => "pulse",
            AddPackage::Reverb => "reverb",
        }
    }
}

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

    /// Database engine management (mysql, postgres, redis)
    Db {
        #[command(subcommand)]
        command: DbCommands,
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

    /// Install a known Laravel package (horizon, telescope, pulse, reverb).
    ///
    /// Walks `composer require` + `php artisan` against the Laravel site at --site
    /// (or the first linked site reached by walking up from cwd). Horizon and Reverb
    /// additionally register a supervised worker that survives daemon restart.
    Add {
        /// Package to install
        #[arg(value_enum)]
        package: AddPackage,
        /// Absolute path to a linked Laravel site (defaults to walking up from cwd)
        #[arg(long)]
        site: Option<PathBuf>,
        /// Accept all prompt defaults non-interactively
        #[arg(long, short = 'y')]
        yes: bool,
        /// Install + configure but do not register supervised workers
        #[arg(long)]
        no_supervise: bool,
        /// Print planned actions; touch no files, run no commands
        #[arg(long)]
        dry_run: bool,
    },
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
enum DbCommands {
    /// Start a DB engine (mysql, postgres, redis, or all)
    Start {
        /// Engine name. Omit or pass `all` for every DB engine.
        engine: Option<String>,
    },
    /// Stop a DB engine, or all DB engines.
    Stop {
        engine: Option<String>,
    },
    /// Restart a DB engine (stop + start).
    Restart {
        engine: Option<String>,
    },
    /// Report status of all DB engines.
    Status {
        /// Emit machine-readable JSON instead of a table.
        #[arg(long)]
        json: bool,
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
        Commands::Db { command } => {
            return run_db(command).await;
        }
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
        Commands::Add {
            package,
            site,
            yes,
            no_supervise,
            dry_run,
        } => {
            return run_add(package, site, yes, no_supervise, dry_run).await;
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
        .context("Daemon not running. Start with: hearth daemon start")?;

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
        DaemonResponse::DbStatus { engines } => {
            print_db_status_table(&engines);
        }
        DaemonResponse::Conflict { engine, port, owner_hint } => {
            let hint = owner_hint
                .as_deref()
                .map(|h| format!(" (owner: {h})"))
                .unwrap_or_default();
            eprintln!(
                "Error: port {port} for {engine} is already in use{hint}\n\
                 Hint: stop the colliding process (e.g. `brew services stop {engine}`) or change the port in ~/.config/hearth/config.toml."
            );
            std::process::exit(1);
        }
        DaemonResponse::Pong => println!("Daemon is running"),
    }
}

fn normalize_engine_arg(raw: Option<String>) -> Option<String> {
    match raw {
        None => None,
        Some(s) if s.eq_ignore_ascii_case("all") => None,
        Some(s) => Some(s),
    }
}

async fn run_db(command: DbCommands) -> anyhow::Result<()> {
    match command {
        DbCommands::Start { engine } => {
            let response = send_to_daemon(DaemonRequest::DbStart {
                engine: normalize_engine_arg(engine),
            })
            .await?;
            print_response(response);
        }
        DbCommands::Stop { engine } => {
            let response = send_to_daemon(DaemonRequest::DbStop {
                engine: normalize_engine_arg(engine),
            })
            .await?;
            print_response(response);
        }
        DbCommands::Restart { engine } => {
            let target = normalize_engine_arg(engine);
            let _ = send_to_daemon(DaemonRequest::DbStop {
                engine: target.clone(),
            })
            .await?;
            let response = send_to_daemon(DaemonRequest::DbStart { engine: target }).await?;
            print_response(response);
        }
        DbCommands::Status { json } => {
            let response = send_to_daemon(DaemonRequest::DbStatus).await?;
            match response {
                DaemonResponse::DbStatus { engines } if json => {
                    println!("{}", serde_json::to_string(&engines)?);
                }
                DaemonResponse::DbStatus { engines } => print_db_status_table(&engines),
                other => print_response(other),
            }
        }
    }
    Ok(())
}

fn print_db_status_table(engines: &[DbEngineStatus]) {
    println!(
        "{:<12} {:<18} {:<6} {:<8} {}",
        "ENGINE", "STATE", "PORT", "PID", "DATA_DIR"
    );
    println!("{}", "-".repeat(80));
    for row in engines {
        let pid = row.pid.map(|p| p.to_string()).unwrap_or_default();
        let mut state = row.state.clone();
        if row.conflict_port {
            state = format!("{state} [conflict]");
        }
        println!(
            "{:<12} {:<18} {:<6} {:<8} {}",
            row.engine, state, row.port, pid, row.data_dir
        );
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
    let mut config = hearth_lib::config::HearthConfig::default();
    // Resolve composer.phar up-front so `hearth add` can shell out via the site's PHP
    // (avoids the brew composer wrapper, which uses the system PHP — scratchpad 796 B3).
    config.composer_phar = hearth_lib::add::composer::resolve_composer_phar();
    if let Some(ref phar) = config.composer_phar {
        println!("  Resolved composer.phar → {}", phar.display());
    } else {
        println!("  Warning: composer.phar not found on host. `hearth add` will probe at request time.");
    }
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

/// Drive `hearth add <package>` — interactive prompts when on a TTY, deterministic
/// defaults under `--yes`. Sends the populated `AddAnswers` to the daemon and prints
/// the response.
///
/// `IsTerminal` from stdlib (Rust 1.70+) avoids the deprecated `atty` crate.
async fn run_add(
    package: AddPackage,
    site: Option<PathBuf>,
    yes: bool,
    no_supervise: bool,
    dry_run: bool,
) -> anyhow::Result<()> {
    use std::io::IsTerminal;

    if !yes && !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "refusing to prompt on non-TTY; pass --yes to accept defaults"
        );
    }

    let site_path = site
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    let answers = match package {
        AddPackage::Telescope => telescope_prompts(yes)?,
        AddPackage::Horizon => horizon_prompts(yes)?,
        AddPackage::Reverb => reverb_prompts(yes)?,
        AddPackage::Pulse => anyhow::bail!("hearth add pulse: recipe arrives in v0.3.0 Task 6"),
    };

    // Up-front message so the user knows composer-require can take 30-90s. The actual
    // log file is written by the daemon once composer finishes (streaming protocol is
    // Phase 4 per plan §R2 / hearth-add plan).
    let log_path = hearth_lib::log_dir().join(format!("add-{}.log", package.as_key()));
    if !dry_run {
        eprintln!(
            "► running composer require + artisan (can take 30-90s; daemon log: {})",
            log_path.display()
        );
    }

    let request = DaemonRequest::Add {
        package: package.as_key().to_string(),
        site_path,
        answers,
        no_supervise,
        dry_run,
    };

    let response = send_to_daemon(request).await?;
    print_response(response);
    Ok(())
}

/// Collect Telescope-specific answers. With `--yes`, takes defaults:
/// telescope_environments = ["local"], telescope_enable_in_prod = false.
fn telescope_prompts(yes: bool) -> anyhow::Result<AddAnswers> {
    let mut answers = AddAnswers::default();
    if yes {
        answers.telescope_environments = Some(vec!["local".to_string()]);
        answers.telescope_enable_in_prod = Some(false);
        return Ok(answers);
    }

    let env_choices = ["local", "staging", "production"];
    let defaults = [true, false, false];
    let selected_idx = dialoguer::MultiSelect::new()
        .with_prompt("Enable Telescope in which environments? (space to toggle, enter to accept)")
        .items(&env_choices)
        .defaults(&defaults)
        .interact()?;
    let selected: Vec<String> = selected_idx
        .iter()
        .map(|&i| env_choices[i].to_string())
        .collect();
    answers.telescope_environments = Some(selected);

    let in_prod = dialoguer::Confirm::new()
        .with_prompt("Will Telescope run in production?")
        .default(false)
        .interact()?;
    answers.telescope_enable_in_prod = Some(in_prod);

    Ok(answers)
}

/// Collect Horizon-specific answers. `--yes` defaults: connection=redis,
/// environment=local, max_processes=3.
fn horizon_prompts(yes: bool) -> anyhow::Result<AddAnswers> {
    let mut answers = AddAnswers::default();
    if yes {
        answers.horizon_connection = Some("redis".to_string());
        answers.horizon_environment = Some("local".to_string());
        answers.horizon_max_processes = Some(3);
        return Ok(answers);
    }

    let connections = ["redis", "database", "sqs", "beanstalkd"];
    let idx = dialoguer::Select::new()
        .with_prompt("Queue connection?")
        .items(&connections)
        .default(0)
        .interact()?;
    answers.horizon_connection = Some(connections[idx].to_string());

    let env: String = dialoguer::Input::new()
        .with_prompt("Horizon environment name")
        .default("local".to_string())
        .interact_text()?;
    answers.horizon_environment = Some(env);

    let max_processes: i64 = dialoguer::Input::new()
        .with_prompt("Max processes per supervisor (1-64)")
        .default(3)
        .validate_with(|n: &i64| -> Result<(), &str> {
            if (1..=64).contains(n) {
                Ok(())
            } else {
                Err("must be 1-64")
            }
        })
        .interact_text()?;
    answers.horizon_max_processes = Some(max_processes);

    Ok(answers)
}

/// Collect Reverb-specific answers. With `--yes`: host=0.0.0.0, port=8080
/// (auto-bump 8080..=8099 if busy), scheme=http. Bracketed-error if all 20 ports
/// in the range are busy.
fn reverb_prompts(yes: bool) -> anyhow::Result<AddAnswers> {
    use hearth_lib::add::prompt::{first_free_port, port_is_free};
    let mut answers = AddAnswers::default();

    if yes {
        answers.reverb_host = Some("0.0.0.0".to_string());
        answers.reverb_scheme = Some("http".to_string());
        // Auto-bump if 8080 busy (scratchpad 796 B6).
        let port = if port_is_free(8080) {
            8080
        } else {
            first_free_port(8080, 8099, 20)
                .ok_or_else(|| anyhow::anyhow!(
                    "ports 8080..=8099 are all busy; pass --site and rerun without --yes"
                ))?
        };
        answers.reverb_port = Some(port);
        return Ok(answers);
    }

    let host: String = dialoguer::Input::new()
        .with_prompt("Reverb host")
        .default("0.0.0.0".to_string())
        .interact_text()?;
    answers.reverb_host = Some(host);

    let port: u16 = loop {
        let candidate: u16 = dialoguer::Input::new()
            .with_prompt("Reverb port")
            .default(8080)
            .validate_with(|n: &u16| -> Result<(), &str> {
                if (1024..=65535).contains(n) {
                    Ok(())
                } else {
                    Err("must be 1024-65535")
                }
            })
            .interact_text()?;
        if port_is_free(candidate) {
            break candidate;
        }
        if dialoguer::Confirm::new()
            .with_prompt(format!("Port {candidate} is busy. Try {}?", candidate + 1))
            .default(true)
            .interact()?
        {
            // Loop and re-prompt with the suggested port as the next default. We do
            // this by writing the prompt again rather than threading state through.
            continue;
        } else {
            anyhow::bail!("Reverb port {candidate} busy; rerun and choose a free port");
        }
    };
    answers.reverb_port = Some(port);

    let hostname: String = dialoguer::Input::new()
        .with_prompt("Reverb hostname (sent to the JS client)")
        .allow_empty(true)
        .interact_text()?;
    if !hostname.is_empty() {
        answers.reverb_hostname = Some(hostname);
    }

    let schemes = ["http", "https"];
    let idx = dialoguer::Select::new()
        .with_prompt("Reverb scheme")
        .items(&schemes)
        .default(0)
        .interact()?;
    answers.reverb_scheme = Some(schemes[idx].to_string());

    Ok(answers)
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
