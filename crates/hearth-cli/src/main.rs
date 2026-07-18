use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use hearth_lib::add::AddAnswers;
use hearth_lib::socket::{
    DaemonRequest, DaemonResponse, DbEngineStatus, FileWriteResult, FpmRestartOutcome,
};

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
#[command(
    name = "hearth",
    version,
    about = "Unified Laravel development command center"
)]
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
    /// Manage PHP INI configuration (e.g., `hearth php config memory_limit 512M`,
    /// `hearth php config --global memory_limit 1G`, `hearth php config --status`)
    Config {
        /// INI key (e.g., memory_limit, upload_max_filesize)
        key: Option<String>,
        /// Value to set
        value: Option<String>,
        /// Target the global store applied to every PHP version
        #[arg(long)]
        global: bool,
        /// Target one version's override table (e.g., --php 8.3)
        #[arg(long)]
        php: Option<String>,
        /// Show configured (and materialized) values instead of setting
        #[arg(long)]
        show: bool,
        /// Remove a directive from the selected scope
        #[arg(long)]
        unset: bool,
        /// Per-target coverage/status table
        #[arg(long)]
        status: bool,
        /// Force journal recovery + reconcile of channel files
        #[arg(long)]
        sync: bool,
        /// Remove every Hearth-written channel file
        #[arg(long)]
        unmanage: bool,
    },
    /// Run a PHP command with Hearth's scan-dir env at the final launch
    /// boundary (e.g. `hearth php exec -- artisan test`). Covers launchers
    /// that clear the environment — but NOT descendants that later clear env
    /// themselves and exec an absolute PHP path; use a Homebrew/Hearth PHP
    /// build for those.
    Exec {
        /// PHP version to run (defaults to the configured default_php)
        #[arg(long = "php")]
        php: Option<String>,
        /// Arguments passed to the PHP binary verbatim
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<std::ffi::OsString>,
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
    Stop { engine: Option<String> },
    /// Restart a DB engine (stop + start).
    Restart { engine: Option<String> },
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
            PhpCommands::Config {
                key,
                value,
                global,
                php,
                show,
                unset,
                status,
                sync,
                unmanage,
            } => {
                // Client-side validation of the whole flag/action matrix
                // BEFORE any daemon I/O.
                match build_php_config_action(
                    key, value, global, php, show, unset, status, sync, unmanage,
                ) {
                    Ok(action) => DaemonRequest::PhpConfigV2 { action },
                    Err(msg) => {
                        eprintln!("Error: {msg}");
                        std::process::exit(2);
                    }
                }
            }
            PhpCommands::Exec { php, args } => {
                return run_php_exec(php, args).await;
            }
        },
        Commands::Db { command } => {
            if run_db(command).await? == ExitClass::Failure {
                std::process::exit(1);
            }
            return Ok(());
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
            if run_add(package, site, yes, no_supervise, dry_run).await? == ExitClass::Failure {
                std::process::exit(1);
            }
            return Ok(());
        }
    };

    let response = send_to_daemon(request).await?;
    // print_response is a pure renderer (returns ExitClass); main owns the
    // only process-exit application in the binary.
    if print_response(response) == ExitClass::Failure {
        std::process::exit(1);
    }

    Ok(())
}

/// Build the typed php-config action from the CLI flag matrix, or an
/// actionable error. Exactly one action; `--global` ⊻ `--php`; scope flags
/// only for Set/Unset/Show (plan 5560 §2.5).
#[allow(clippy::too_many_arguments)]
fn build_php_config_action(
    key: Option<String>,
    value: Option<String>,
    global: bool,
    php: Option<String>,
    show: bool,
    unset: bool,
    status: bool,
    sync: bool,
    unmanage: bool,
) -> Result<hearth_lib::socket::PhpConfigAction, String> {
    use hearth_lib::socket::{PhpConfigAction, PhpScope};

    let action_flags = [show, unset, status, sync, unmanage]
        .iter()
        .filter(|f| **f)
        .count();
    if action_flags > 1 {
        return Err(
            "pass at most one of --show, --unset, --status, --sync, --unmanage".to_string(),
        );
    }
    if global && php.is_some() {
        return Err("--global and --php are mutually exclusive".to_string());
    }
    let scope = if global {
        PhpScope::Global
    } else if let Some(version) = php.clone() {
        PhpScope::Version { version }
    } else {
        PhpScope::Active
    };

    if status || sync || unmanage {
        let name = if status {
            "--status"
        } else if sync {
            "--sync"
        } else {
            "--unmanage"
        };
        if key.is_some() || value.is_some() {
            return Err(format!("{name} takes no key or value"));
        }
        if global || php.is_some() {
            return Err(format!("{name} takes no --global/--php scope"));
        }
        return Ok(if status {
            PhpConfigAction::Status
        } else if sync {
            PhpConfigAction::Sync
        } else {
            PhpConfigAction::Unmanage
        });
    }

    if show {
        if value.is_some() {
            return Err("--show takes an optional key but no value".to_string());
        }
        return Ok(PhpConfigAction::Show { key });
    }

    if unset {
        if value.is_some() {
            return Err("--unset removes a directive; it takes no value".to_string());
        }
        let key = key.ok_or_else(|| "--unset requires a key".to_string())?;
        return Ok(PhpConfigAction::Unset { scope, key });
    }

    // Implicit Set: both key and value required.
    match (key, value) {
        (Some(key), Some(value)) => Ok(PhpConfigAction::Set { scope, key, value }),
        _ => Err(
            "provide <key> <value> to set, or one of --show/--status/--sync/--unmanage".to_string(),
        ),
    }
}

/// Exit classification for a php-config report. Nonzero iff persistence
/// failed (arrives as a daemon Error), a channel file was Refused/Failed, or
/// a REGISTERED php-fpm restart failed. NotRegistered / LaunchBlocked /
/// unmanaged-only cells render loudly but exit 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitClass {
    Success,
    Failure,
}

/// Pure renderer: `(stdout, stderr, exit class)`. No process exit here.
fn render_php_config_report(
    outcome: &hearth_lib::socket::PhpConfigOutcome,
) -> (String, String, ExitClass) {
    use std::fmt::Write;

    let mut out = String::new();
    let mut err = String::new();
    let mut exit = ExitClass::Success;

    if outcome.persisted == Some(true) {
        out.push_str("Configuration persisted.\n");
    }

    for file in &outcome.files {
        match &file.result {
            FileWriteResult::Written => {
                let _ = writeln!(out, "written   {}", file.path);
            }
            FileWriteResult::Unchanged => {
                let _ = writeln!(out, "unchanged {}", file.path);
            }
            FileWriteResult::Deleted => {
                let _ = writeln!(out, "deleted   {}", file.path);
            }
            FileWriteResult::Refused { reason } => {
                exit = ExitClass::Failure;
                let _ = writeln!(err, "refused   {} — {reason}", file.path);
            }
            FileWriteResult::Failed { error } => {
                exit = ExitClass::Failure;
                let _ = writeln!(err, "failed    {} — {error}", file.path);
            }
        }
    }

    if outcome.rows.is_empty() {
        if outcome.persisted.is_none() && outcome.files.is_empty() {
            out.push_str("No PHP targets discovered.\n");
        }
    } else {
        let _ = writeln!(
            out,
            "{:<9} {:<7} {:<4} {:<10} {:<48} {}",
            "PROVIDER", "VERSION", "SAPI", "CONTEXT", "CHANNEL", "COVERAGE"
        );
        for row in &outcome.rows {
            let channel = row.channel.as_deref().unwrap_or("—");
            let mut line = format!(
                "{:<9} {:<7} {:<4} {:<10} {:<48} {}",
                row.provider, row.version, row.sapi, row.context, channel, row.coverage
            );
            if let Some(configured) = &row.configured {
                let _ = write!(line, "  configured={configured}");
                match &row.observed {
                    Some(observed) => {
                        let _ = write!(line, " observed={observed} ({})", row.observed_state);
                    }
                    None => {
                        let _ = write!(line, " observed=n/a");
                    }
                }
            }
            let _ = writeln!(out, "{line}");
            if row.coverage.starts_with("UNMANAGED: privileged-dir") {
                let _ = writeln!(
                    out,
                    "  ↳ remediation: root-owned, version-blind; never written by Hearth. \
                     Use `hearth php exec -- <cmd>` for env-clearing launchers, or a \
                     Hearth/Homebrew PHP build."
                );
            }
            if row.coverage.contains("channel blocked") {
                let _ = writeln!(
                    out,
                    "  ↳ remediation: remove or rename the file, then run \
                     `hearth php config --sync`."
                );
            }
        }
        // Scope footer — states the guarantee boundary, never universal.
        let _ = writeln!(
            out,
            "Coverage: Hearth guarantees Hearth-launched processes (env-honoring binaries) \
             and verified user-owned, version-exclusive channels. UNMANAGED/LAUNCH-BLOCKED \
             cells are outside that guarantee."
        );
    }

    match &outcome.fpm {
        FpmRestartOutcome::Restarted => out.push_str("php-fpm restarted\n"),
        FpmRestartOutcome::NotRegistered { herd_hint: true } => {
            out.push_str("php-fpm not restarted: not registered (Herd manages PHP-FPM)\n")
        }
        FpmRestartOutcome::NotRegistered { herd_hint: false } => {
            out.push_str("php-fpm not restarted: not registered\n")
        }
        FpmRestartOutcome::LaunchBlocked { reason } => {
            let _ = writeln!(out, "php-fpm not restarted: LAUNCH-BLOCKED ({reason})");
        }
        FpmRestartOutcome::Failed { message } => {
            exit = ExitClass::Failure;
            let _ = writeln!(err, "Error: php-fpm restart failed: {message}");
        }
        FpmRestartOutcome::NotAttempted => {}
    }

    (out, err, exit)
}

async fn send_to_daemon(request: DaemonRequest) -> anyhow::Result<DaemonResponse> {
    send_to_daemon_at(&hearth_lib::socket::socket_path(), request).await
}

async fn send_to_daemon_at(
    socket_path: &std::path::Path,
    request: DaemonRequest,
) -> anyhow::Result<DaemonResponse> {
    let stream = UnixStream::connect(socket_path)
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

    // Protocol window: an old daemon closes without responding to an unknown
    // request variant (EOF → empty line), or answers with a shape this CLI
    // cannot parse. Either way the remediation is the same.
    const MISMATCH: &str =
        "hearth CLI and daemon versions differ — run 'hearth daemon restart' and retry.";
    if line.trim().is_empty() {
        anyhow::bail!("{MISMATCH}");
    }
    let response: DaemonResponse =
        serde_json::from_str(line.trim()).map_err(|_| anyhow::anyhow!("{MISMATCH}"))?;
    Ok(response)
}

/// Render one daemon response. Pure with respect to process control: returns
/// the exit class and NEVER exits — only CLI `main` applies process exits.
#[must_use]
fn print_response(response: DaemonResponse) -> ExitClass {
    match response {
        DaemonResponse::Ok { message } => {
            if let Some(msg) = message {
                println!("{}", msg);
            }
            ExitClass::Success
        }
        DaemonResponse::Error { message } => {
            eprintln!("Error: {}", message);
            ExitClass::Failure
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
            ExitClass::Success
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
            ExitClass::Success
        }
        DaemonResponse::PhpVersions { versions } => {
            for v in versions {
                let marker = if v.active { " *" } else { "" };
                println!("PHP {}{} — {}", v.version, marker, v.path);
            }
            ExitClass::Success
        }
        DaemonResponse::DbStatus { engines } => {
            print_db_status_table(&engines);
            ExitClass::Success
        }
        DaemonResponse::Conflict {
            engine,
            port,
            owner_hint,
        } => {
            let hint = owner_hint
                .as_deref()
                .map(|h| format!(" (owner: {h})"))
                .unwrap_or_default();
            eprintln!(
                "Error: port {port} for {engine} is already in use{hint}\n\
                 Hint: stop the colliding process (e.g. `brew services stop {engine}`) or change the port in ~/.config/hearth/config.toml."
            );
            ExitClass::Failure
        }
        DaemonResponse::PhpConfigReport(outcome) => {
            let (out, err, exit) = render_php_config_report(&outcome);
            print!("{out}");
            eprint!("{err}");
            exit
        }
        DaemonResponse::Pong => {
            println!("Daemon is running");
            ExitClass::Success
        }
    }
}

fn normalize_engine_arg(raw: Option<String>) -> Option<String> {
    match raw {
        None => None,
        Some(s) if s.eq_ignore_ascii_case("all") => None,
        Some(s) => Some(s),
    }
}

async fn run_db(command: DbCommands) -> anyhow::Result<ExitClass> {
    let exit = match command {
        DbCommands::Start { engine } => {
            let response = send_to_daemon(DaemonRequest::DbStart {
                engine: normalize_engine_arg(engine),
            })
            .await?;
            print_response(response)
        }
        DbCommands::Stop { engine } => {
            let response = send_to_daemon(DaemonRequest::DbStop {
                engine: normalize_engine_arg(engine),
            })
            .await?;
            print_response(response)
        }
        DbCommands::Restart { engine } => {
            let target = normalize_engine_arg(engine);
            let _ = send_to_daemon(DaemonRequest::DbStop {
                engine: target.clone(),
            })
            .await?;
            let response = send_to_daemon(DaemonRequest::DbStart { engine: target }).await?;
            print_response(response)
        }
        DbCommands::Status { json } => {
            let response = send_to_daemon(DaemonRequest::DbStatus).await?;
            match response {
                DaemonResponse::DbStatus { engines } if json => {
                    println!("{}", serde_json::to_string(&engines)?);
                    ExitClass::Success
                }
                DaemonResponse::DbStatus { engines } => {
                    print_db_status_table(&engines);
                    ExitClass::Success
                }
                other => print_response(other),
            }
        }
    };
    Ok(exit)
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

/// `hearth php exec [--php X.Y] -- <args…>` — daemon-free shim. Reconciles
/// channel files for the selected version, then replaces this process with
/// the resolved PHP binary carrying the Belt-E scan-dir env (reconstructed at
/// this final launch boundary). A descendant that clears env and execs an
/// absolute PHP path bypasses the shim by design — use a Homebrew/Hearth PHP
/// build for that case.
async fn run_php_exec(
    version: Option<String>,
    args: Vec<std::ffi::OsString>,
) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;

    // One validated runtime-mode/root snapshot per operation (B5-2/B5-4):
    // config root and write authority come from the same construction.
    let provider_roots = hearth_lib::php::targets::ProviderRoots::detect()
        .map_err(|e| anyhow::anyhow!("provider-root configuration invalid: {e}"))?;
    let config_dir = provider_roots.hearth.clone();
    let config_path = config_dir.join("config.toml");
    let config = hearth_lib::config::HearthConfig::load_from(&config_path)?;
    let version = version.unwrap_or_else(|| config.default_php.clone());

    // Reconcile first — same engine as the daemon; the journaled manifest
    // keeps a concurrent daemon reconcile safe.
    let engine = hearth_lib::php::engine::PhpConfigEngine::new(
        std::sync::Arc::new(tokio::sync::Mutex::new(config)),
        config_path,
        config_dir.clone(),
        provider_roots,
        std::sync::Arc::new(|| hearth_lib::service::manager::is_herd_running()),
        std::time::Duration::from_secs(5),
    );
    // Hard gate (F4): never exec PHP against unreconciled channel state.
    hearth_lib::php::engine::require_reconciled(
        engine
            .apply(hearth_lib::socket::PhpConfigAction::Sync)
            .await,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    let binary = hearth_lib::php::resolver::resolve_php_binary(&version, &config_dir)
        .with_context(|| format!("PHP {version} binary not found"))?;
    let (key, value) = hearth_lib::php::scan_dir_env(&config_dir, &version);
    // exec() only returns on failure — on success this process IS php, so no
    // unsupervised child is ever created.
    let err = std::process::Command::new(&binary)
        .args(&args)
        .env(key, value)
        .exec();
    Err(anyhow::anyhow!(
        "failed to exec {}: {err}",
        binary.display()
    ))
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
    // One validated runtime-mode/root snapshot for the whole install
    // (B5-2/B5-4): config root and write authority come from the same
    // construction; a root failure aborts before any directory creation.
    let provider_roots = hearth_lib::php::targets::ProviderRoots::detect()
        .map_err(|e| anyhow::anyhow!("provider-root configuration invalid: {e}"))?;
    let config_dir = provider_roots.hearth.clone();
    let config_path = config_dir.join("config.toml");
    std::fs::create_dir_all(&config_dir)?;
    // Load-and-preserve: re-running `hearth install` must keep existing
    // php_ini, added_packages, and port settings. `load_from()` returns
    // defaults only when no config exists, and errors (rather than
    // clobbering) on a corrupt file.
    let mut config = hearth_lib::config::HearthConfig::load_from(&config_path)?;
    // Resolve composer.phar up-front so `hearth add` can shell out via the site's PHP
    // (avoids the brew composer wrapper, which uses the system PHP — scratchpad 796 B3).
    config.composer_phar = hearth_lib::add::composer::resolve_composer_phar();
    if let Some(ref phar) = config.composer_phar {
        println!("  Resolved composer.phar → {}", phar.display());
    } else {
        println!(
            "  Warning: composer.phar not found on host. `hearth add` will probe at request time."
        );
    }
    config.save_to(&config_path)?;
    println!("  Config saved to {}", config_dir.display());

    // One-shot daemon-free php-config reconcile (plan 5560 §2.3 install hook):
    // recovers any pending journal and materializes channel files for the
    // preserved php_ini settings.
    let engine = hearth_lib::php::engine::PhpConfigEngine::new(
        std::sync::Arc::new(tokio::sync::Mutex::new(
            hearth_lib::config::HearthConfig::load_from(&config_path)?,
        )),
        config_path.clone(),
        config_dir.clone(),
        provider_roots,
        std::sync::Arc::new(|| hearth_lib::service::manager::is_herd_running()),
        std::time::Duration::from_secs(5),
    );
    // Hard gate (F4): a Refused/Failed channel outcome aborts install with a
    // nonzero exit (the preserved config stays persisted; rerun after fixing
    // the reported path).
    hearth_lib::php::engine::require_reconciled(engine.boot_sync().await)
        .map_err(|e| anyhow::anyhow!("php-config reconcile failed: {e}"))?;

    // 3. Setup DNS resolver (requires sudo)
    println!("\n[3/3] Setting up DNS resolver (requires sudo)...");

    let herd_running = hearth_lib::service::manager::is_herd_running();
    let resolver_content = if herd_running {
        // Herd manages its own dnsmasq on default port 53 — don't override it
        println!(
            "  Herd detected — writing /etc/resolver/test without custom port (using Herd's dnsmasq on port 53)"
        );
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
        .args([
            "bash",
            "-c",
            &format!("echo '{}' > /etc/resolver/test", resolver_content.trim()),
        ])
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
        // B6-1 (review 5594 rev6): mutating lifecycle commands obtain ONE
        // validated root snapshot BEFORE any mkdir/log-create/remove/kill/
        // spawn — a root-validation failure is a typed error with zero
        // mutation. Status stays on the read-only seam: pid-file read +
        // signal-0 probe + socket ping; no mutation, no launch.
        DaemonCommands::Start => daemon_start(&DaemonLifecyclePaths::detect()?).await,
        DaemonCommands::Stop => daemon_stop(&DaemonLifecyclePaths::detect()?).await,
        DaemonCommands::Status => daemon_status().await,
    }
}

/// Validated daemon-lifecycle paths (review 5594 B6-1): the log dir, pid
/// file, and socket spellings all derive from the validated `roots.hearth`
/// of a single `ProviderRoots` snapshot — never from a second raw
/// `config_dir()`/`log_dir()`/`run_dir()`/`socket_path()` env read.
struct DaemonLifecyclePaths {
    log_dir: std::path::PathBuf,
    pid_file: std::path::PathBuf,
    socket: std::path::PathBuf,
}

impl DaemonLifecyclePaths {
    fn from_roots(roots: &hearth_lib::php::targets::ProviderRoots) -> Self {
        Self {
            log_dir: roots.hearth.join("log"),
            pid_file: roots.hearth.join("run").join("daemon.pid"),
            socket: roots.hearth.join("hearth.sock"),
        }
    }

    fn detect() -> anyhow::Result<Self> {
        let roots = hearth_lib::php::targets::ProviderRoots::detect()
            .map_err(|e| anyhow::anyhow!("provider-root configuration invalid: {e}"))?;
        Ok(Self::from_roots(&roots))
    }
}

/// Read a daemon PID file and check whether that process is alive.
/// Signal 0 checks liveness without sending an actual signal — probe only.
fn daemon_pid_at(pid_file: &std::path::Path) -> Option<u32> {
    let content = std::fs::read_to_string(pid_file).ok()?;
    let pid: u32 = content.trim().parse().ok()?;

    let nix_pid = nix::unistd::Pid::from_raw(pid as i32);
    let alive = nix::sys::signal::kill(nix_pid, None).is_ok();
    if alive { Some(pid) } else { None }
}

/// Read-only liveness probe on the raw path seam (status/display only —
/// mutating lifecycle paths go through [`DaemonLifecyclePaths`]).
fn daemon_pid() -> Option<u32> {
    daemon_pid_at(&hearth_lib::run_dir().join("daemon.pid"))
}

async fn daemon_start(paths: &DaemonLifecyclePaths) -> anyhow::Result<()> {
    if let Some(pid) = daemon_pid_at(&paths.pid_file) {
        println!("Daemon is already running (PID {})", pid);
        return Ok(());
    }

    std::fs::create_dir_all(&paths.log_dir)?;

    let stdout_file = std::fs::File::create(paths.log_dir.join("daemon.out.log"))?;
    let stderr_file = std::fs::File::create(paths.log_dir.join("daemon.err.log"))?;

    // Resolve hearth-daemon as a sibling of the current `hearth` binary first,
    // then fall back to PATH. Without sibling-first resolution, a stale brew
    // install of `hearth-daemon` on PATH shadows a fresh `cargo install` build
    // and the CLI silently talks to the wrong daemon (variants mismatch).
    let daemon_bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("hearth-daemon")))
        .filter(|p| p.is_file())
        .map(|p| p.into_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("hearth-daemon"));
    let mut command = std::process::Command::new(&daemon_bin);
    command
        .stdin(std::process::Stdio::null())
        .stdout(stdout_file)
        .stderr(stderr_file);
    // Detach from the CLI's session so the daemon survives the parent shell's
    // SIGHUP. Without setsid the daemon dies with whichever PTY started it —
    // visible under tight session managers (codex, tmux detach) but masked
    // under interactive zsh/bash. setsid also installs the daemon as its own
    // session leader, which the supervisor relies on for process-group reaping.
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            // SAFETY: setsid is async-signal-safe; safe to call between fork and exec.
            nix::unistd::setsid()
                .map(|_| ())
                .map_err(|e| std::io::Error::from_raw_os_error(e as i32))
        });
    }
    let child = command.spawn().context(
        "Failed to start hearth-daemon. Is it installed next to the `hearth` binary or on PATH?",
    )?;

    let pid = child.id();
    println!("Daemon starting (PID {})...", pid);

    // Wait briefly for the socket to appear — same validated spelling the
    // spawned daemon derives from its own boot snapshot.
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if paths.socket.exists() {
            println!("Daemon is running.");
            return Ok(());
        }
    }

    println!(
        "Daemon started but socket not yet available. Check logs at {}",
        paths.log_dir.display()
    );
    Ok(())
}

async fn daemon_stop(paths: &DaemonLifecyclePaths) -> anyhow::Result<()> {
    let Some(pid) = daemon_pid_at(&paths.pid_file) else {
        println!("Daemon is not running.");
        return Ok(());
    };

    // Gracefully stop all supervised services before killing the daemon
    if let Ok(_) = send_to_daemon_at(&paths.socket, DaemonRequest::Stop).await {
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
            let _ = std::fs::remove_file(&paths.pid_file);
            let _ = std::fs::remove_file(&paths.socket);
            println!("Daemon stopped.");
            return Ok(());
        }
    }

    println!(
        "Daemon did not stop within 5 seconds. You may need to kill PID {} manually.",
        pid
    );
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
) -> anyhow::Result<ExitClass> {
    use std::io::IsTerminal;

    if !yes && !std::io::stdin().is_terminal() {
        anyhow::bail!("refusing to prompt on non-TTY; pass --yes to accept defaults");
    }

    let site_path = site
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    let answers = match package {
        AddPackage::Telescope => telescope_prompts(yes)?,
        AddPackage::Horizon => horizon_prompts(yes)?,
        AddPackage::Reverb => reverb_prompts(yes)?,
        AddPackage::Pulse => pulse_prompts(yes)?,
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

    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    let request = DaemonRequest::Add {
        package: package.as_key().to_string(),
        site_path,
        cwd,
        answers,
        no_supervise,
        dry_run,
    };

    let response = send_to_daemon(request).await?;
    Ok(print_response(response))
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
            first_free_port(8080, 8099, 20).ok_or_else(|| {
                anyhow::anyhow!(
                    "ports 8080..=8099 are all busy; pass --site and rerun without --yes"
                )
            })?
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

/// Collect Pulse-specific answers. Single prompt: storage driver (default: database).
/// The `pulse_ingest_trim_lottery` advanced prompt is intentionally dropped
/// (dissent D3 in scratchpad 796) — defaults to Pulse's own default of 100.
fn pulse_prompts(yes: bool) -> anyhow::Result<AddAnswers> {
    let mut answers = AddAnswers::default();
    if yes {
        answers.pulse_storage_driver = Some("database".to_string());
        return Ok(answers);
    }
    let drivers = ["database", "redis"];
    let idx = dialoguer::Select::new()
        .with_prompt("Pulse storage driver")
        .items(&drivers)
        .default(0)
        .interact()?;
    answers.pulse_storage_driver = Some(drivers[idx].to_string());
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

#[cfg(test)]
mod tests {
    use super::*;
    use hearth_lib::socket::{
        FileOutcome, PhpConfigAction, PhpConfigOutcome, PhpScope, PhpTargetRow,
    };

    fn build(
        key: Option<&str>,
        value: Option<&str>,
        global: bool,
        php: Option<&str>,
        show: bool,
        unset: bool,
        status: bool,
        sync: bool,
        unmanage: bool,
    ) -> Result<PhpConfigAction, String> {
        build_php_config_action(
            key.map(String::from),
            value.map(String::from),
            global,
            php.map(String::from),
            show,
            unset,
            status,
            sync,
            unmanage,
        )
    }

    #[test]
    fn flag_matrix_rejects_invalid_combinations() {
        // More than one action flag.
        assert!(
            build(
                Some("k"),
                None,
                false,
                None,
                true,
                true,
                false,
                false,
                false
            )
            .is_err()
        );
        assert!(build(None, None, false, None, false, false, true, true, false).is_err());
        // --global with --php.
        assert!(
            build(
                Some("k"),
                Some("v"),
                true,
                Some("8.3"),
                false,
                false,
                false,
                false,
                false
            )
            .is_err()
        );
        // Status/Sync/Unmanage take no key/value/scope.
        assert!(
            build(
                Some("k"),
                None,
                false,
                None,
                false,
                false,
                true,
                false,
                false
            )
            .is_err()
        );
        assert!(
            build(
                None,
                Some("v"),
                false,
                None,
                false,
                false,
                false,
                true,
                false
            )
            .is_err()
        );
        assert!(build(None, None, true, None, false, false, false, false, true).is_err());
        assert!(
            build(
                None,
                None,
                false,
                Some("8.3"),
                false,
                false,
                true,
                false,
                false
            )
            .is_err()
        );
        // --show forbids a value.
        assert!(
            build(
                Some("k"),
                Some("v"),
                false,
                None,
                true,
                false,
                false,
                false,
                false
            )
            .is_err()
        );
        // --unset requires a key, forbids a value.
        assert!(build(None, None, false, None, false, true, false, false, false).is_err());
        assert!(
            build(
                Some("k"),
                Some("v"),
                false,
                None,
                false,
                true,
                false,
                false,
                false
            )
            .is_err()
        );
        // Implicit Set needs both key and value.
        assert!(
            build(
                Some("k"),
                None,
                false,
                None,
                false,
                false,
                false,
                false,
                false
            )
            .is_err()
        );
        assert!(build(None, None, false, None, false, false, false, false, false).is_err());
    }

    #[test]
    fn flag_matrix_builds_expected_actions() {
        // Bare invocation keeps Active-version behavior.
        assert_eq!(
            build(
                Some("memory_limit"),
                Some("1G"),
                false,
                None,
                false,
                false,
                false,
                false,
                false
            ),
            Ok(PhpConfigAction::Set {
                scope: PhpScope::Active,
                key: "memory_limit".to_string(),
                value: "1G".to_string(),
            })
        );
        assert_eq!(
            build(
                Some("k"),
                Some("v"),
                true,
                None,
                false,
                false,
                false,
                false,
                false
            ),
            Ok(PhpConfigAction::Set {
                scope: PhpScope::Global,
                key: "k".to_string(),
                value: "v".to_string(),
            })
        );
        assert_eq!(
            build(
                Some("k"),
                Some("v"),
                false,
                Some("8.3"),
                false,
                false,
                false,
                false,
                false
            ),
            Ok(PhpConfigAction::Set {
                scope: PhpScope::Version {
                    version: "8.3".to_string()
                },
                key: "k".to_string(),
                value: "v".to_string(),
            })
        );
        assert_eq!(
            build(
                Some("k"),
                None,
                true,
                None,
                false,
                true,
                false,
                false,
                false
            ),
            Ok(PhpConfigAction::Unset {
                scope: PhpScope::Global,
                key: "k".to_string(),
            })
        );
        assert_eq!(
            build(None, None, false, None, true, false, false, false, false),
            Ok(PhpConfigAction::Show { key: None })
        );
        assert_eq!(
            build(
                Some("k"),
                None,
                false,
                None,
                true,
                false,
                false,
                false,
                false
            ),
            Ok(PhpConfigAction::Show {
                key: Some("k".to_string())
            })
        );
        assert_eq!(
            build(None, None, false, None, false, false, true, false, false),
            Ok(PhpConfigAction::Status)
        );
        assert_eq!(
            build(None, None, false, None, false, false, false, true, false),
            Ok(PhpConfigAction::Sync)
        );
        assert_eq!(
            build(None, None, false, None, false, false, false, false, true),
            Ok(PhpConfigAction::Unmanage)
        );
    }

    fn row(coverage: &str) -> PhpTargetRow {
        PhpTargetRow {
            provider: "herd".to_string(),
            version: "8.4".to_string(),
            sapi: "cli".to_string(),
            context: "sanitized".to_string(),
            channel: Some("/usr/local/etc/php/conf.d".to_string()),
            coverage: coverage.to_string(),
            configured: None,
            observed: None,
            observed_state: "n/a".to_string(),
        }
    }

    fn outcome_with_rows(rows: Vec<PhpTargetRow>) -> PhpConfigOutcome {
        PhpConfigOutcome {
            persisted: None,
            rows,
            files: vec![],
            fpm: FpmRestartOutcome::NotAttempted,
        }
    }

    #[test]
    fn render_unmanaged_row_with_remediation_and_footer_no_universal_claim() {
        let outcome = outcome_with_rows(vec![row("UNMANAGED: privileged-dir")]);
        let (out, err, exit) = render_php_config_report(&outcome);
        assert!(out.contains("UNMANAGED: privileged-dir"), "got: {out}");
        assert!(out.contains("remediation"), "got: {out}");
        assert!(out.contains("hearth php exec"), "got: {out}");
        assert!(out.contains("Coverage: Hearth guarantees"), "got: {out}");
        assert!(out.contains("outside that guarantee"), "got: {out}");
        assert!(
            !out.to_lowercase().contains("universal"),
            "must never claim universal coverage: {out}"
        );
        assert!(err.is_empty());
        // Unmanaged-only cells exit zero.
        assert_eq!(exit, ExitClass::Success);
    }

    #[test]
    fn render_launch_blocked_row_cites_followup_todo() {
        let outcome = outcome_with_rows(vec![row(
            "LAUNCH-BLOCKED: missing fpm config — see todo #2343",
        )]);
        let (out, _err, exit) = render_php_config_report(&outcome);
        assert!(out.contains("LAUNCH-BLOCKED"), "got: {out}");
        assert!(out.contains("#2343"), "got: {out}");
        assert_eq!(exit, ExitClass::Success);
    }

    #[test]
    fn render_degraded_channel_blocked_row() {
        let outcome = outcome_with_rows(vec![row(
            "managed* — channel blocked: /opt/homebrew/etc/php/8.5/conf.d/zz-hearth.ini",
        )]);
        let (out, _err, _exit) = render_php_config_report(&outcome);
        assert!(out.contains("channel blocked"), "got: {out}");
        assert!(out.contains("--sync"), "got: {out}");
    }

    #[test]
    fn render_four_state_observation_labels() {
        let mut r = row("managed (channel+env)");
        r.configured = Some("1G".to_string());
        r.observed = Some("1G".to_string());
        r.observed_state = "materialized".to_string();
        let (out, _err, _exit) = render_php_config_report(&outcome_with_rows(vec![r]));
        assert!(out.contains("configured=1G"), "got: {out}");
        assert!(out.contains("observed=1G (materialized)"), "got: {out}");

        let mut r = row("managed (channel; env ignored)");
        r.configured = Some("1G".to_string());
        let (out, _err, _exit) = render_php_config_report(&outcome_with_rows(vec![r]));
        assert!(out.contains("observed=n/a"), "got: {out}");
    }

    #[test]
    fn exit_class_rules() {
        // Refused channel file → nonzero.
        let refused = PhpConfigOutcome {
            persisted: Some(true),
            rows: vec![],
            files: vec![FileOutcome {
                path: "/x/zz-hearth.ini".to_string(),
                result: FileWriteResult::Refused {
                    reason: "untracked".to_string(),
                },
            }],
            fpm: FpmRestartOutcome::NotAttempted,
        };
        assert_eq!(render_php_config_report(&refused).2, ExitClass::Failure);

        // Failed channel file → nonzero.
        let failed = PhpConfigOutcome {
            persisted: Some(true),
            rows: vec![],
            files: vec![FileOutcome {
                path: "/x/zz-hearth.ini".to_string(),
                result: FileWriteResult::Failed {
                    error: "io".to_string(),
                },
            }],
            fpm: FpmRestartOutcome::NotAttempted,
        };
        assert_eq!(render_php_config_report(&failed).2, ExitClass::Failure);

        // Registered restart failure → nonzero.
        let fpm_failed = PhpConfigOutcome {
            persisted: Some(true),
            rows: vec![],
            files: vec![],
            fpm: FpmRestartOutcome::Failed {
                message: "boom".to_string(),
            },
        };
        assert_eq!(render_php_config_report(&fpm_failed).2, ExitClass::Failure);

        // NotRegistered / LaunchBlocked are informational → zero.
        for fpm in [
            FpmRestartOutcome::NotRegistered { herd_hint: true },
            FpmRestartOutcome::NotRegistered { herd_hint: false },
            FpmRestartOutcome::LaunchBlocked {
                reason: "missing fpm config — see todo #2343".to_string(),
            },
            FpmRestartOutcome::Restarted,
            FpmRestartOutcome::NotAttempted,
        ] {
            let ok = PhpConfigOutcome {
                persisted: Some(true),
                rows: vec![],
                files: vec![FileOutcome {
                    path: "/x/zz-hearth.ini".to_string(),
                    result: FileWriteResult::Written,
                }],
                fpm,
            };
            assert_eq!(render_php_config_report(&ok).2, ExitClass::Success);
        }
    }

    #[test]
    fn print_response_error_branch_returns_failure_without_exiting() {
        // Reaching the assertion proves no process exit happened.
        let exit = print_response(DaemonResponse::Error {
            message: "boom".to_string(),
        });
        assert_eq!(exit, ExitClass::Failure);
    }

    #[test]
    fn print_response_conflict_branch_returns_failure_without_exiting() {
        let exit = print_response(DaemonResponse::Conflict {
            engine: "mysql".to_string(),
            port: 3306,
            owner_hint: Some("homebrew.mxcl.mysql".to_string()),
        });
        assert_eq!(exit, ExitClass::Failure);
    }

    #[test]
    fn print_response_success_branches_return_success() {
        assert_eq!(
            print_response(DaemonResponse::Ok { message: None }),
            ExitClass::Success
        );
        assert_eq!(print_response(DaemonResponse::Pong), ExitClass::Success);
    }

    #[test]
    fn print_response_php_report_propagates_renderer_exit_class() {
        let refused = PhpConfigOutcome {
            persisted: Some(true),
            rows: vec![],
            files: vec![FileOutcome {
                path: "/x/zz-hearth.ini".to_string(),
                result: FileWriteResult::Refused {
                    reason: "untracked".to_string(),
                },
            }],
            fpm: FpmRestartOutcome::NotAttempted,
        };
        assert_eq!(
            print_response(DaemonResponse::PhpConfigReport(refused)),
            ExitClass::Failure
        );
    }

    #[test]
    fn daemon_lifecycle_paths_derive_from_validated_snapshot() {
        // B6-1: every mutating lifecycle path spelling comes from the
        // validated snapshot's hearth root — no raw env-derived helper.
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let roots = hearth_lib::php::targets::ProviderRoots::isolated(
            &base,
            base.join("hearth"),
            base.join("herd"),
            base.join("homebrew"),
        )
        .unwrap();
        let paths = DaemonLifecyclePaths::from_roots(&roots);
        assert_eq!(paths.log_dir, base.join("hearth/log"));
        assert_eq!(paths.pid_file, base.join("hearth/run/daemon.pid"));
        assert_eq!(paths.socket, base.join("hearth/hearth.sock"));
        for p in [&paths.log_dir, &paths.pid_file, &paths.socket] {
            assert!(p.starts_with(&roots.hearth), "derived from roots.hearth");
        }
    }

    #[test]
    fn daemon_lifecycle_detect_fails_closed_before_any_mutation() {
        // B6-1 red at 2651cdd: a relative HEARTH_CONFIG_DIR made
        // `daemon start` create CWD-relative log dirs/files and spawn the
        // daemon. Post-fix, path construction itself is the gate: detect()
        // returns the typed error and daemon_start/daemon_stop (which only
        // accept an already-validated DaemonLifecyclePaths) are never
        // reached — zero mkdir/create/remove/kill/spawn.
        //
        // This is the only env-mutating test in this crate; exact prior
        // values are restored below (no other test reads these keys).
        let keys = [
            "HEARTH_CONFIG_DIR",
            "HEARTH_ISOLATED_ROOT",
            "HEARTH_HERD_ROOT",
            "HEARTH_HOMEBREW_ROOT",
        ];
        let saved: Vec<(&str, Option<std::ffi::OsString>)> =
            keys.iter().map(|k| (*k, std::env::var_os(k))).collect();

        // SAFETY: single env-mutating test in this binary.
        unsafe {
            std::env::remove_var("HEARTH_ISOLATED_ROOT");
            std::env::remove_var("HEARTH_HERD_ROOT");
            std::env::remove_var("HEARTH_HOMEBREW_ROOT");
            std::env::set_var("HEARTH_CONFIG_DIR", "relative-b6-cfg");
        }
        let relative = DaemonLifecyclePaths::detect();
        // SAFETY: as above.
        unsafe {
            std::env::set_var("HEARTH_HERD_ROOT", "/tmp/lone-override");
            std::env::remove_var("HEARTH_CONFIG_DIR");
        }
        let lone = DaemonLifecyclePaths::detect();
        // SAFETY: as above — restore the exact prior values.
        unsafe {
            for (k, v) in &saved {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }

        let err = relative.err().expect("relative override must fail closed");
        assert!(
            err.to_string()
                .contains("provider-root configuration invalid"),
            "typed actionable error, got: {err}"
        );
        assert!(
            !std::path::Path::new("relative-b6-cfg").exists(),
            "zero mutation on rejection"
        );
        assert!(lone.is_err(), "lone discovery override must fail closed");
    }
}
