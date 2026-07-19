use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use command_group::{CommandGroup, GroupChild};
use tokio::time;
use tracing::{error, info, warn};

use super::{CircuitBreaker, ServiceKind, ServiceState};

/// How to bring a service down cleanly.
///
/// Default is fine for stateless services (mailpit, nginx, redis). DB engines
/// that hold open WAL/InnoDB state need a longer grace window — SIGKILLing a
/// mid-flush mysqld shreds `ib_logfile`. Postgres prefers `pg_ctl stop -m fast`
/// for a coordinated checkpoint before any signal is sent.
#[derive(Debug, Clone)]
pub enum ShutdownStrategy {
    /// SIGTERM to the process group, 5s grace, then SIGKILL.
    Default,
    /// SIGTERM, `grace_ms` wait, then SIGKILL. For mysqld (~20s).
    LongGrace { grace_ms: u64 },
    /// `pg_ctl stop -m fast -D <datadir>` first; falls back to SIGTERM + 20s
    /// grace + SIGKILL if pg_ctl fails or isn't on disk.
    Postgres {
        datadir: PathBuf,
        pg_ctl_binary: PathBuf,
    },
}

impl ShutdownStrategy {
    fn grace(&self) -> Duration {
        match self {
            Self::Default => Duration::from_secs(5),
            Self::LongGrace { grace_ms } => Duration::from_millis(*grace_ms),
            Self::Postgres { .. } => Duration::from_secs(20),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FpmLaunchConf {
    HearthOwned {
        conf: PathBuf,
        conf_sha256: String,
        probe_sha256: String,
        listen: PathBuf,
    },
    UserManaged {
        conf: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FpmLaunchSnapshot {
    pub generation: u64,
    pub group_id: Option<u32>,
    pub conf: FpmLaunchConf,
    pub started_at: std::time::SystemTime,
    pub running: bool,
}

pub struct LaunchStamp {
    generation: u64,
}

/// Manages a single supervised service process.
///
/// Each service runs in its own process group via `command-group`. Shutdown is
/// driven by [`ShutdownStrategy`]; see that enum for engine-specific tuning.
pub struct ManagedService {
    pub kind: ServiceKind,
    pub state: ServiceState,
    pub child: Option<GroupChild>,
    pub circuit_breaker: CircuitBreaker,
    pub shutdown_strategy: ShutdownStrategy,
    command: String,
    args: Vec<String>,
    /// Working directory for the spawned process. None = inherit daemon cwd.
    /// Required for supervised Laravel workers (Horizon, Reverb) which must run
    /// inside the site root so `artisan` finds the app bootstrap.
    cwd: Option<PathBuf>,
    /// Optional site name; surfaces in `display_name()` as `horizon[shopfront]`.
    /// Set for `hearth add`-managed Horizon/Reverb instances; `None` for the
    /// shared system services (nginx, php-fpm, dnsmasq, mailpit, dump-server,
    /// the DB engines).
    site_name: Option<String>,
    /// Extra environment variables for the spawned child (e.g. the Belt-E
    /// `PHP_INI_SCAN_DIR` pair for PHP workers/FPM). Empty = inherit only.
    env: Vec<(String, String)>,
    fpm_launch: Option<FpmLaunchConf>,
    launch_generation: Option<u64>,
    /// Trustworthy Hearth-owned start metadata: recorded at the moment THIS
    /// supervisor spawned the child, cleared only once termination is
    /// positively confirmed. Never inferred from ambient process evidence
    /// (titles, argv, /proc) — that entire evidence class is banned
    /// (review 5594).
    started_at: Option<std::time::SystemTime>,
    /// C4-1 test-only injected stop-failure seam — deterministic failure
    /// without impossible-to-trigger OS errors; production behavior is
    /// unchanged (`cfg(test)` only). The injected failure returns BEFORE any
    /// mutation, so child/state/timestamp retention is exercised for real.
    #[cfg(test)]
    pub(crate) fail_next_stop: Option<String>,
}

impl ManagedService {
    pub fn new(kind: ServiceKind, command: String, args: Vec<String>) -> Self {
        Self {
            kind,
            state: ServiceState::Stopped,
            child: None,
            circuit_breaker: CircuitBreaker::new(3, Duration::from_secs(60)),
            shutdown_strategy: ShutdownStrategy::Default,
            command,
            args,
            cwd: None,
            site_name: None,
            env: Vec::new(),
            fpm_launch: None,
            launch_generation: None,
            started_at: None,
            #[cfg(test)]
            fail_next_stop: None,
        }
    }

    /// Builder: set the shutdown strategy for this service.
    pub fn with_shutdown_strategy(mut self, strategy: ShutdownStrategy) -> Self {
        self.shutdown_strategy = strategy;
        self
    }

    /// Builder: extra environment variables for the spawned child. Process
    /// group/supervision behavior is unchanged — env only affects the spawn.
    pub fn with_env(mut self, env: Vec<(String, String)>) -> Self {
        self.env = env;
        self
    }

    pub fn with_fpm_launch_conf(mut self, conf: FpmLaunchConf) -> Self {
        self.fpm_launch = Some(conf);
        self
    }

    pub fn fpm_launch_conf(&self) -> Option<&FpmLaunchConf> {
        self.fpm_launch.as_ref()
    }

    /// Create a managed service that runs in a specific working directory.
    ///
    /// Used by supervised Laravel workers (Horizon, Reverb) which must run inside
    /// the site root so `php artisan` finds the application bootstrap.
    pub fn with_cwd(kind: ServiceKind, command: String, args: Vec<String>, cwd: PathBuf) -> Self {
        Self {
            kind,
            state: ServiceState::Stopped,
            child: None,
            circuit_breaker: CircuitBreaker::new(3, Duration::from_secs(60)),
            shutdown_strategy: ShutdownStrategy::Default,
            command,
            args,
            cwd: Some(cwd),
            site_name: None,
            env: Vec::new(),
            fpm_launch: None,
            launch_generation: None,
            started_at: None,
            #[cfg(test)]
            fail_next_stop: None,
        }
    }

    /// Like `with_cwd` but also records a site name for display purposes.
    pub fn with_cwd_and_site(
        kind: ServiceKind,
        command: String,
        args: Vec<String>,
        cwd: PathBuf,
        site_name: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            state: ServiceState::Stopped,
            child: None,
            circuit_breaker: CircuitBreaker::new(3, Duration::from_secs(60)),
            shutdown_strategy: ShutdownStrategy::Default,
            command,
            args,
            cwd: Some(cwd),
            site_name: Some(site_name.into()),
            env: Vec::new(),
            fpm_launch: None,
            launch_generation: None,
            started_at: None,
            #[cfg(test)]
            fail_next_stop: None,
        }
    }

    /// Working directory the service will be spawned in, if any.
    pub fn cwd(&self) -> Option<&std::path::Path> {
        self.cwd.as_deref()
    }

    /// Optional site name carried by per-site Laravel workers (Horizon, Reverb).
    pub fn site_name(&self) -> Option<&str> {
        self.site_name.as_deref()
    }

    /// Extra environment variables applied to the spawned child.
    pub fn env(&self) -> &[(String, String)] {
        &self.env
    }

    /// When THIS supervisor spawned the current child (`None` when stopped).
    /// The trustworthy Hearth-owned start metadata behind the truthful
    /// `pending restart` marker — never derived from ambient process facts.
    pub fn started_at(&self) -> Option<std::time::SystemTime> {
        self.started_at
    }

    /// The command this service spawns.
    pub fn command(&self) -> &str {
        &self.command
    }

    /// User-facing label used in `hearth status` and tracing. Adds the site name
    /// in brackets when present so two Laravel sites with Horizon render as
    /// `horizon[shopfront]` / `horizon[chatapp]` rather than two indistinguishable
    /// `horizon` rows.
    pub fn display_name(&self) -> String {
        match &self.site_name {
            Some(site) => format!("{}[{}]", self.kind.name(), site),
            None => self.kind.name().to_string(),
        }
    }

    /// Start the service in a new process group.
    pub fn start(&mut self, stamp: LaunchStamp) -> anyhow::Result<()> {
        info!(service = %self.kind, "starting service");

        let mut cmd = std::process::Command::new(&self.command);
        cmd.args(&self.args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(ref dir) = self.cwd {
            cmd.current_dir(dir);
        }
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        let child = cmd.group_spawn()?;

        let pid = child.id();
        self.child = Some(child);
        self.state = ServiceState::Running { pid };
        self.started_at = Some(std::time::SystemTime::now());
        self.launch_generation = Some(stamp.generation);

        info!(service = %self.kind, pid, "service started");
        Ok(())
    }

    fn clear_launch_metadata(&mut self) {
        self.started_at = None;
        self.launch_generation = None;
    }

    /// Stop the service. Strategy:
    /// - `Postgres` → `pg_ctl stop -m fast` first, then signals on fallback.
    /// - All others → SIGTERM to the process group, poll until grace expires,
    ///   then SIGKILL.
    ///
    /// C4-1 (review 5650 r4): stopping is a TRANSACTIONAL state transition.
    /// The child/process-group handle and `started_at` stay tracked until
    /// termination is positively confirmed (reaped exit or confirmed-dead
    /// group); every unresolved failure — non-ESRCH SIGTERM, `try_wait`,
    /// final SIGKILL, final wait — propagates as an error WITH the handle,
    /// timestamp, and truthful state retained so the caller can retry.
    /// `Stopped` is only ever recorded after proof.
    pub fn stop(&mut self) -> anyhow::Result<()> {
        if self.child.is_none() {
            self.state = ServiceState::Stopped;
            self.clear_launch_metadata();
            return Ok(());
        }

        #[cfg(test)]
        if let Some(reason) = self.fail_next_stop.take() {
            anyhow::bail!("injected stop failure: {reason}");
        }

        let pid = self.child.as_ref().map(|c| c.id()).unwrap_or_default();
        info!(service = %self.kind, pid, "stopping service");

        // Fast path: an already-exited child (macOS returns EPERM — not
        // ESRCH — for signals to a fully-zombie group, so signaling first
        // would misread a dead group as a signal failure) is reaped and
        // finalized without any signal.
        match self.child.as_mut().expect("checked above").try_wait() {
            Ok(Some(_status)) => {
                info!(service = %self.kind, "child had already exited");
                self.child = None;
                self.state = ServiceState::Stopped;
                self.clear_launch_metadata();
                return Ok(());
            }
            Ok(None) => {}
            Err(e) => {
                anyhow::bail!(
                    "could not verify {} liveness before stop (try_wait: {e}) — child \
                     remains supervised for retry",
                    self.kind
                );
            }
        }

        let mut needs_signal = true;

        if let ShutdownStrategy::Postgres {
            datadir,
            pg_ctl_binary,
        } = &self.shutdown_strategy
        {
            // `pg_ctl stop -m fast` does a clean shutdown with a checkpoint.
            match std::process::Command::new(pg_ctl_binary)
                .args(["stop", "-m", "fast", "-w", "-D"])
                .arg(datadir)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
            {
                Ok(status) if status.success() => {
                    info!(service = %self.kind, "pg_ctl stop succeeded");
                    needs_signal = false;
                }
                Ok(status) => {
                    warn!(
                        service = %self.kind,
                        code = ?status.code(),
                        "pg_ctl stop returned non-zero; falling back to SIGTERM"
                    );
                }
                Err(e) => {
                    warn!(
                        service = %self.kind,
                        error = %e,
                        "pg_ctl invocation failed; falling back to SIGTERM"
                    );
                }
            }
        }

        if needs_signal {
            let pgid = nix::unistd::Pid::from_raw(pid as i32);
            if let Err(e) = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGTERM) {
                // ESRCH ("no such process") means the group is already gone
                // — the wait below reaps and confirms. For anything else,
                // re-check liveness once: a child that exited between the
                // fast path and the signal also reads as a signal error
                // (EPERM on macOS). A LIVE child we cannot signal is a
                // genuine unresolved failure: keep everything and report.
                if e != nix::errno::Errno::ESRCH {
                    match self.child.as_mut().expect("checked above").try_wait() {
                        Ok(Some(_status)) => {
                            info!(service = %self.kind, "child exited during stop");
                            self.child = None;
                            self.state = ServiceState::Stopped;
                            self.clear_launch_metadata();
                            return Ok(());
                        }
                        _ => {
                            anyhow::bail!(
                                "SIGTERM to {} process group {pid} failed ({e}) with the \
                                 child still live/unresolved — child remains supervised \
                                 for retry",
                                self.kind
                            );
                        }
                    }
                }
            }
        }

        let grace = self.shutdown_strategy.grace();
        let deadline = Instant::now() + grace;
        let child = self.child.as_mut().expect("checked above");
        loop {
            match child.try_wait() {
                Ok(Some(_status)) => {
                    // Confirmed exit — the ONLY place (besides the post-kill
                    // wait) that finalizes the transition.
                    info!(service = %self.kind, "service stopped cleanly");
                    self.child = None;
                    self.state = ServiceState::Stopped;
                    self.clear_launch_metadata();
                    return Ok(());
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => {
                    anyhow::bail!(
                        "could not verify {} termination (try_wait: {e}) — child remains \
                         supervised for retry",
                        self.kind
                    );
                }
            }
        }

        warn!(
            service = %self.kind,
            grace_ms = grace.as_millis() as u64,
            "service did not stop within grace window — sending SIGKILL"
        );
        if let Err(e) = child.kill() {
            // Same zombie-race tolerance as SIGTERM: confirm liveness before
            // treating the failure as unresolved.
            if let Ok(Some(_status)) = child.try_wait() {
                self.child = None;
                self.state = ServiceState::Stopped;
                self.clear_launch_metadata();
                return Ok(());
            }
            anyhow::bail!(
                "SIGKILL to {} process group failed ({e}) — child remains supervised for retry",
                self.kind
            );
        }
        match child.wait() {
            Ok(_status) => {
                self.child = None;
                self.state = ServiceState::Stopped;
                self.clear_launch_metadata();
                Ok(())
            }
            Err(e) => anyhow::bail!(
                "could not reap {} after SIGKILL ({e}) — child remains supervised for retry",
                self.kind
            ),
        }
    }

    /// Check if the child process is still alive.
    /// Returns true if running, false if exited or not started.
    pub fn is_alive(&mut self) -> bool {
        if let Some(ref mut child) = self.child {
            match child.try_wait() {
                Ok(Some(_status)) => {
                    // Process exited
                    self.child = None;
                    false
                }
                Ok(None) => true, // Still running
                Err(_) => false,
            }
        } else {
            false
        }
    }
}

/// The ServiceSupervisor owns all managed services and runs the health
/// check loop.
///
/// ```text
/// ServiceSupervisor
///  ├── nginx          (GroupChild, process group)
///  ├── php-fpm-84     (GroupChild, process group)
///  ├── dnsmasq        (GroupChild, process group)
///  ├── mailpit        (GroupChild, process group)
///  └── dump-server    (GroupChild, process group)
///
/// On shutdown: SIGTERM → 5s wait → SIGKILL for each group
/// ```
pub struct ServiceSupervisor {
    services: HashMap<ServiceKind, ManagedService>,
    health_interval: Duration,
    launch_generation_counter: u64,
    /// C3-1/C6-1 (review 5650): the typed current-ownership probe for
    /// PHP-FPM — ONE coherent policy enforced INSIDE the supervisor so
    /// every mutation path (boot, register/reconfigure, generic
    /// start/restart, health supervision, config restart, switch
    /// transaction) obeys it without per-caller audits.
    ///
    /// INVARIANT (C6-1): the supervisor ALWAYS holds a probe. Construction
    /// initializes it fail-CLOSED to `Unknown("FPM ownership probe is not
    /// configured")` — missing evidence is never treated as proven Unowned,
    /// and every FPM activation refuses until explicit typed evidence is
    /// installed via [`Self::set_fpm_ownership_probe`].
    fpm_ownership: ExternalFpmOwnership,
}

/// Current external (Herd) ownership of PHP-FPM (C4-2). Boolean evidence is
/// banned: a failed probe is `Unknown`, and every activation policy treats
/// `Unknown` fail-CLOSED (never start/register/reconfigure FPM), reporting
/// "ownership unknown" instead of pretending Herd absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FpmOwnership {
    Owned,
    Unowned,
    /// The probe could not produce evidence; carries a non-sensitive
    /// diagnostic (exit status / IO error class — never environment
    /// contents).
    Unknown(String),
}

/// Injectable current-ownership probe for PHP-FPM (production:
/// `service::manager::current_fpm_ownership`; tests/smoke: the typed
/// isolated seam through the same function).
pub type ExternalFpmOwnership = std::sync::Arc<dyn Fn() -> FpmOwnership + Send + Sync>;

/// Outcome of the atomic FPM replacement transaction (C6-2).
#[derive(Debug)]
pub enum FpmReplaceOutcome {
    /// Old FPM stopped (if any), replacement registered and started.
    Replaced,
    /// Ownership snapshot said Herd owns FPM — zero mutation.
    SkippedOwned,
    /// Ownership snapshot was Unknown — zero mutation, fail-closed.
    SkippedOwnershipUnknown(String),
    /// Preparing the replacement failed BEFORE any child was touched. A
    /// running old child is left untouched; a stale non-running
    /// registration is removed (`removed_stale`).
    BuildFailed { reason: String, removed_stale: bool },
    /// The old child could not be confirmed stopped — it remains fully
    /// supervised (C4-1); no replacement happened.
    StopFailed { error: String },
    /// Old child stopped and the replacement registered, but its start
    /// failed — the replacement stays supervised in `Failed` state.
    StartFailed { error: String },
}

/// C7-1 (review 5650 r8): outcome of the atomic in-place FPM restart
/// transaction (`restart_fpm_service`). Skips and refusals happen BEFORE
/// any mutation; failures report the exact retained/partial state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FpmRestartTxOutcome {
    /// Old child stopped and started again — one coherent transaction.
    Restarted,
    /// Ownership snapshot said Herd owns FPM — zero mutation.
    SkippedOwned,
    /// Ownership snapshot was Unknown — zero mutation, fail-closed.
    SkippedOwnershipUnknown(String),
    /// No FPM registration exists — nothing to restart, zero mutation.
    NotRegistered,
    /// The old child could not be confirmed stopped — it remains fully
    /// supervised (C4-1); no start was attempted.
    StopFailed { error: String },
    /// The stop succeeded but the restart's start failed — the
    /// registration stays supervised in truthful `Failed` state; no child
    /// is running and no orphan exists.
    StartFailed { error: String },
}

impl Default for ServiceSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl ServiceSupervisor {
    pub fn new() -> Self {
        Self {
            services: HashMap::new(),
            health_interval: Duration::from_secs(5),
            launch_generation_counter: 0,
            // C6-1: fail-CLOSED until real evidence is installed — a
            // probe-less supervisor refuses every FPM activation.
            fpm_ownership: std::sync::Arc::new(|| {
                FpmOwnership::Unknown("FPM ownership probe is not configured".to_string())
            }),
        }
    }

    /// Install the current-Herd-ownership probe for PHP-FPM (C3-1/C6-1) —
    /// production wires `manager::current_fpm_ownership`; tests install
    /// explicit typed evidence (there is no way back to the unconfigured
    /// state, and no fail-open default exists).
    pub fn set_fpm_ownership_probe(&mut self, probe: ExternalFpmOwnership) {
        self.fpm_ownership = probe;
    }

    /// Current PHP-FPM ownership, consulted live on every FPM-mutating path.
    fn current_fpm_ownership(&self) -> FpmOwnership {
        (self.fpm_ownership)()
    }

    fn mint_launch_stamp(&mut self) -> LaunchStamp {
        self.launch_generation_counter = self
            .launch_generation_counter
            .checked_add(1)
            .expect("launch generation counter exhausted");
        LaunchStamp {
            generation: self.launch_generation_counter,
        }
    }

    /// May Hearth activate (start/restart) its own FPM right now? `Unknown`
    /// fails CLOSED (C4-2).
    fn fpm_activation_blocked(&self) -> Option<String> {
        match self.current_fpm_ownership() {
            FpmOwnership::Unowned => None,
            FpmOwnership::Owned => Some("php-fpm is owned by Herd".to_string()),
            FpmOwnership::Unknown(diag) => Some(format!(
                "php-fpm ownership is unknown ({diag}); FPM activation skipped fail-closed"
            )),
        }
    }

    /// Register a service to be supervised.
    ///
    /// C5-1B: FPM registration re-checks TYPED current ownership inside the
    /// supervisor boundary immediately before mutation — higher-level checks
    /// are TOCTOU-prone. Owned/Unknown refuse with zero mutation; there is
    /// no raw insertion bypass.
    pub fn register(&mut self, service: ManagedService) -> anyhow::Result<()> {
        if service.kind == ServiceKind::PhpFpm
            && let Some(reason) = self.fpm_activation_blocked()
        {
            anyhow::bail!("php-fpm registration refused: {reason}");
        }
        self.services.insert(service.kind, service);
        Ok(())
    }

    /// Start all registered services.
    ///
    /// Services that fail to start (e.g., binary not found) are logged and
    /// skipped — one broken service doesn't prevent the others from running.
    pub fn start_all(&mut self) -> anyhow::Result<()> {
        let kinds: Vec<ServiceKind> = self.services.keys().copied().collect();
        for kind in kinds {
            // C3-1/C4-2: generic start never launches Hearth FPM while Herd
            // currently owns it OR while ownership is unknown (fail-closed)
            // — other services start normally.
            if kind == ServiceKind::PhpFpm
                && let Some(reason) = self.fpm_activation_blocked()
            {
                info!("php-fpm start skipped: {reason}");
                continue;
            }
            let stamp = self.mint_launch_stamp();
            if let Some(svc) = self.services.get_mut(&kind) {
                if let Err(e) = svc.start(stamp) {
                    warn!(service = %kind, error = %e, "failed to start service, skipping");
                    svc.state = ServiceState::Failed {
                        reason: e.to_string(),
                    };
                }
            }
        }
        Ok(())
    }

    /// Stop all services, killing entire process groups. Every service is
    /// attempted; failures are AGGREGATED and returned (C4-1) — never a
    /// false blanket success while a child could not be confirmed stopped.
    pub fn stop_all(&mut self) -> anyhow::Result<()> {
        let mut failures = Vec::new();
        for (kind, svc) in self.services.iter_mut() {
            if let Err(e) = svc.stop() {
                warn!(service = %kind, error = %e, "stop failed");
                failures.push(format!("{kind}: {e}"));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            anyhow::bail!("failed to stop: {}", failures.join("; "))
        }
    }

    /// Start a specific service. Returns an error if the service is not registered.
    /// Starting Hearth FPM while Herd currently owns it — or while ownership
    /// is unknown (fail-closed, C4-2) — is refused with an actionable error.
    pub fn start_service(&mut self, kind: ServiceKind) -> anyhow::Result<()> {
        if kind == ServiceKind::PhpFpm
            && let Some(reason) = self.fpm_activation_blocked()
        {
            anyhow::bail!("{reason} — Hearth will not start its own FPM");
        }
        let stamp = self.mint_launch_stamp();
        let svc = self
            .services
            .get_mut(&kind)
            .ok_or_else(|| anyhow::anyhow!("service {kind} is not registered"))?;
        svc.circuit_breaker.reset();
        svc.start(stamp)
    }

    /// Stop a specific service. Returns an error if the service is not registered.
    pub fn stop_service(&mut self, kind: ServiceKind) -> anyhow::Result<()> {
        let svc = self
            .services
            .get_mut(&kind)
            .ok_or_else(|| anyhow::anyhow!("service {kind} is not registered"))?;
        svc.stop()
    }

    /// Replace a service's command configuration (e.g., for PHP version
    /// switch). The service must be stopped before reconfiguring. `env`
    /// REPLACES the stored child environment — a version switch must never
    /// retain the previous version's scan-dir env.
    /// C5-1B: FPM reconfiguration re-checks TYPED current ownership inside
    /// the supervisor boundary — Owned/Unknown refuse with zero mutation.
    pub fn reconfigure_service(
        &mut self,
        kind: ServiceKind,
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    ) -> anyhow::Result<()> {
        if kind == ServiceKind::PhpFpm
            && let Some(reason) = self.fpm_activation_blocked()
        {
            anyhow::bail!("php-fpm reconfiguration refused: {reason}");
        }
        if let Some(svc) = self.services.get_mut(&kind) {
            svc.command = command;
            svc.args = args;
            svc.env = env;
            svc.circuit_breaker.reset();
        }
        Ok(())
    }

    /// Deregister a service (e.g., a launch-blocked FPM after a version
    /// switch). Returns true when a registration was removed. Callers stop
    /// the service first.
    pub fn remove_service(&mut self, kind: ServiceKind) -> bool {
        self.services.remove(&kind).is_some()
    }

    /// C6-2 (review 5650 r7): ATOMIC PHP-FPM replacement — the version
    /// switch's entire FPM decision/mutation as ONE supervisor-owned
    /// transaction under ONE authoritative typed ownership snapshot taken
    /// immediately before any mutation.
    ///
    /// Semantics:
    /// - Owned/Unknown snapshot → zero mutation (no stop/remove/register/
    ///   start); old registration, child, timestamp, and state unchanged.
    /// - Unowned snapshot governs the WHOLE stop→replace→start — there is
    ///   deliberately no second recheck that could refuse after destructive
    ///   mutation; an ownership change after the snapshot is handled by the
    ///   next health tick (relinquish path).
    /// - `build` prepares the replacement BEFORE the old child is touched.
    ///   Its failure never stops a RUNNING child; a stale NON-running
    ///   registration is removed (launch-blocked switch contract).
    /// - A stop failure keeps the old child fully supervised (C4-1
    ///   transactional semantics) and aborts the replacement.
    /// - A start failure after a successful stop reports the exact partial
    ///   state: the replacement registration is retained (supervisable,
    ///   `Failed`) — never reported as "untouched", never an orphan.
    pub fn replace_fpm_service(
        &mut self,
        build: impl FnOnce() -> Result<ManagedService, String>,
    ) -> FpmReplaceOutcome {
        // ONE authoritative snapshot immediately before mutation.
        match self.current_fpm_ownership() {
            FpmOwnership::Owned => return FpmReplaceOutcome::SkippedOwned,
            FpmOwnership::Unknown(diag) => {
                return FpmReplaceOutcome::SkippedOwnershipUnknown(diag);
            }
            FpmOwnership::Unowned => {}
        }

        // Prepare the replacement BEFORE touching the old child.
        let replacement = match build() {
            Ok(svc) => svc,
            Err(reason) => {
                let old_running = self
                    .services
                    .get(&ServiceKind::PhpFpm)
                    .is_some_and(|svc| svc.child.is_some());
                if old_running {
                    return FpmReplaceOutcome::BuildFailed {
                        reason,
                        removed_stale: false,
                    };
                }
                let removed_stale = self.services.remove(&ServiceKind::PhpFpm).is_some();
                return FpmReplaceOutcome::BuildFailed {
                    reason,
                    removed_stale,
                };
            }
        };
        debug_assert_eq!(replacement.kind, ServiceKind::PhpFpm);

        // Stop the old child transactionally (C4-1) — failure keeps it.
        if let Some(old) = self.services.get_mut(&ServiceKind::PhpFpm)
            && let Err(e) = old.stop()
        {
            return FpmReplaceOutcome::StopFailed {
                error: e.to_string(),
            };
        }

        // Replace metadata + start under the SAME snapshot. This insert is
        // part of the snapshot-guarded transaction, not a public raw path.
        self.services.insert(ServiceKind::PhpFpm, replacement);
        let stamp = self.mint_launch_stamp();
        let svc = self
            .services
            .get_mut(&ServiceKind::PhpFpm)
            .expect("just inserted");
        svc.circuit_breaker.reset();
        match svc.start(stamp) {
            Ok(()) => FpmReplaceOutcome::Replaced,
            Err(e) => {
                svc.state = ServiceState::Failed {
                    reason: e.to_string(),
                };
                FpmReplaceOutcome::StartFailed {
                    error: e.to_string(),
                }
            }
        }
    }

    /// C7-1 (review 5650 r8): ATOMIC PHP-FPM restart — every restart that
    /// can touch FPM is ONE supervisor-owned transaction under ONE
    /// authoritative typed ownership snapshot taken immediately before any
    /// mutation. Owned/Unknown refuse BEFORE the stop, leaving the old
    /// registration, child handle, timestamp, and state untouched; Unowned
    /// performs the coherent stop/start with NO post-stop ownership recheck
    /// (a later flip is the health handoff's job). A failed stop keeps the
    /// old child fully supervised (C4 semantics); a failed start after a
    /// successful stop retains a truthful `Failed` registration and never
    /// creates an orphan. Callers must NOT compose the public
    /// `stop_service`/`stop_all` with the guarded starts for FPM.
    pub fn restart_fpm_service(&mut self) -> FpmRestartTxOutcome {
        // ONE authoritative snapshot immediately before mutation.
        match self.current_fpm_ownership() {
            FpmOwnership::Owned => return FpmRestartTxOutcome::SkippedOwned,
            FpmOwnership::Unknown(diag) => {
                return FpmRestartTxOutcome::SkippedOwnershipUnknown(diag);
            }
            FpmOwnership::Unowned => {}
        }
        let Some(svc) = self.services.get_mut(&ServiceKind::PhpFpm) else {
            return FpmRestartTxOutcome::NotRegistered;
        };
        if let Err(e) = svc.stop() {
            return FpmRestartTxOutcome::StopFailed {
                error: e.to_string(),
            };
        }
        svc.circuit_breaker.reset();
        let stamp = self.mint_launch_stamp();
        let svc = self
            .services
            .get_mut(&ServiceKind::PhpFpm)
            .expect("registration checked above");
        match svc.start(stamp) {
            Ok(()) => FpmRestartTxOutcome::Restarted,
            Err(e) => {
                svc.state = ServiceState::Failed {
                    reason: e.to_string(),
                };
                FpmRestartTxOutcome::StartFailed {
                    error: e.to_string(),
                }
            }
        }
    }

    /// C7-1: all-services restart. PHP-FPM goes through the atomic
    /// ownership transaction above; every other registered service is
    /// stopped and started normally, with per-service failures collected
    /// instead of aborting the rest. The caller renders the aggregate
    /// truthfully — an FPM skip/refusal must never read as "restarted".
    pub fn restart_all(&mut self) -> (Option<FpmRestartTxOutcome>, Vec<(ServiceKind, String)>) {
        let mut errors: Vec<(ServiceKind, String)> = Vec::new();
        let kinds: Vec<ServiceKind> = self.services.keys().copied().collect();
        for kind in kinds {
            if kind == ServiceKind::PhpFpm {
                continue;
            }
            let stopped = if let Some(svc) = self.services.get_mut(&kind) {
                if let Err(e) = svc.stop() {
                    errors.push((kind, format!("stop failed: {e}")));
                    false
                } else {
                    svc.circuit_breaker.reset();
                    true
                }
            } else {
                false
            };
            if !stopped {
                continue;
            }
            let stamp = self.mint_launch_stamp();
            let svc = self
                .services
                .get_mut(&kind)
                .expect("registration checked above");
            if let Err(e) = svc.start(stamp) {
                svc.state = ServiceState::Failed {
                    reason: e.to_string(),
                };
                errors.push((kind, format!("start failed: {e}")));
            }
        }
        let fpm = if self.services.contains_key(&ServiceKind::PhpFpm) {
            Some(self.restart_fpm_service())
        } else {
            None
        };
        (fpm, errors)
    }

    /// Read access to one registered service (command/args/env inspection).
    pub fn service(&self, kind: ServiceKind) -> Option<&ManagedService> {
        self.services.get(&kind)
    }

    pub fn fpm_launch_snapshot(&self) -> Option<FpmLaunchSnapshot> {
        let service = self.services.get(&ServiceKind::PhpFpm)?;
        Some(FpmLaunchSnapshot {
            generation: service.launch_generation?,
            group_id: service.child.as_ref().map(GroupChild::id),
            conf: service.fpm_launch.clone()?,
            started_at: service.started_at?,
            running: matches!(service.state, ServiceState::Running { .. }),
        })
    }

    /// C4-1 test-only: arm the injected stop-failure seam for one service.
    #[cfg(test)]
    pub(crate) fn inject_stop_failure(&mut self, kind: ServiceKind, reason: &str) {
        if let Some(svc) = self.services.get_mut(&kind) {
            svc.fail_next_stop = Some(reason.to_string());
        }
    }

    /// Run one health check pass. Returns services that crashed.
    pub fn health_check(&mut self) -> Vec<ServiceKind> {
        let mut crashed = Vec::new();

        let kinds: Vec<ServiceKind> = self.services.keys().copied().collect();
        for kind in kinds {
            // C3-1/C4-1/C4-2: health supervision obeys CURRENT ownership.
            // While Herd owns FPM, Hearth's OWN still-running FPM child is
            // relinquished (Hearth stops only its own process group — it
            // never signals Herd's process); a failed handoff stop keeps the
            // child fully supervised with its truthful state and is retried
            // on later ticks — never a false Stopped, never a replacement.
            // While ownership is UNKNOWN, the conservative policy is: no
            // restart, no relinquish, no replacement — the running child
            // stays supervised until evidence returns.
            let fpm_ownership = if kind == ServiceKind::PhpFpm {
                self.current_fpm_ownership()
            } else {
                FpmOwnership::Unowned
            };
            let mut restart = false;
            if let Some(svc) = self.services.get_mut(&kind) {
                match fpm_ownership {
                    FpmOwnership::Owned => {
                        if svc.child.is_some() {
                            info!(
                                "Herd owns PHP-FPM — relinquishing Hearth's own supervised FPM child"
                            );
                            if let Err(e) = svc.stop() {
                                warn!(
                                    error = %e,
                                    "PHP-FPM handoff stop FAILED — the child remains \
                                     supervised with its truthful state; handoff will be \
                                     retried on the next health tick"
                                );
                            }
                        } else if matches!(svc.state, ServiceState::Running { .. }) {
                            // Crashed while Herd owns it: record truthfully,
                            // no replacement child.
                            svc.state = ServiceState::Stopped;
                            svc.clear_launch_metadata();
                        }
                        continue;
                    }
                    FpmOwnership::Unknown(diag) => {
                        warn!(
                            "php-fpm ownership unknown ({diag}) — health supervision \
                             fail-closed: no restart, no relinquish this tick"
                        );
                        continue;
                    }
                    FpmOwnership::Unowned => {}
                }
                if matches!(svc.state, ServiceState::Running { .. }) && !svc.is_alive() {
                    warn!(service = %kind, "service crashed");

                    if svc.circuit_breaker.record_failure() {
                        error!(service = %kind, "circuit breaker tripped — service marked as failed");
                        svc.state = ServiceState::Failed {
                            reason: "Too many restarts (3 failures in 60s)".to_string(),
                        };
                    } else {
                        restart = true;
                    }

                    crashed.push(kind);
                }
            }
            if restart {
                info!(service = %kind, "attempting restart");
                let stamp = self.mint_launch_stamp();
                let svc = self
                    .services
                    .get_mut(&kind)
                    .expect("health-checked service remains registered");
                if let Err(e) = svc.start(stamp) {
                    error!(service = %kind, error = %e, "restart failed");
                    svc.state = ServiceState::Failed {
                        reason: e.to_string(),
                    };
                }
            }
        }

        crashed
    }

    /// Run the health check loop (call from async context).
    pub async fn run_health_loop(&mut self) {
        let mut interval = time::interval(self.health_interval);
        loop {
            interval.tick().await;
            self.health_check();
        }
    }

    /// Get the current state of all services.
    pub fn status(&self) -> HashMap<ServiceKind, &ServiceState> {
        self.services.iter().map(|(k, v)| (*k, &v.state)).collect()
    }

    /// Same as `status()` but carries each service's display name (with site bracket
    /// for per-site Laravel workers). Used by the daemon for `hearth status` output.
    pub fn services_detailed(&self) -> Vec<ServiceDetail> {
        self.services
            .values()
            .map(|svc| ServiceDetail {
                display_name: svc.display_name(),
                state: svc.state.clone(),
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct ServiceDetail {
    pub display_name: String,
    pub state: ServiceState,
}

impl Drop for ServiceSupervisor {
    fn drop(&mut self) {
        // Guarantee: all child processes are killed when the supervisor exits,
        // even during panics. This is the core safety property that prevents
        // orphan processes.
        let _ = self.stop_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_supervisor_with_service() -> ServiceSupervisor {
        let mut sup = ServiceSupervisor::new();
        // Use `true` as a harmless command that exits immediately
        let svc = ManagedService::new(ServiceKind::Nginx, "true".to_string(), vec![]);
        sup.register(svc).unwrap();
        sup
    }

    #[test]
    fn start_service_errors_on_unregistered() {
        let mut sup = ServiceSupervisor::new();
        let result = sup.start_service(ServiceKind::DumpServer);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not registered"));
    }

    #[test]
    fn stop_service_errors_on_unregistered() {
        let mut sup = ServiceSupervisor::new();
        let result = sup.stop_service(ServiceKind::Redis);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not registered"));
    }

    #[test]
    fn start_service_succeeds_for_registered() {
        let mut sup = make_supervisor_with_service();
        let result = sup.start_service(ServiceKind::Nginx);
        assert!(result.is_ok());
    }

    #[test]
    fn stop_service_succeeds_for_registered() {
        let mut sup = make_supervisor_with_service();
        // Start then stop
        sup.start_service(ServiceKind::Nginx).unwrap();
        let result = sup.stop_service(ServiceKind::Nginx);
        assert!(result.is_ok());
    }

    #[test]
    fn reconfigure_service_updates_command() {
        let mut sup = make_supervisor_with_service();
        sup.reconfigure_service(
            ServiceKind::Nginx,
            "/usr/sbin/nginx-new".to_string(),
            vec!["-g".to_string(), "daemon off;".to_string()],
            vec![("A".to_string(), "1".to_string())],
        )
        .unwrap();
        let svc = sup.services.get(&ServiceKind::Nginx).unwrap();
        assert_eq!(svc.command, "/usr/sbin/nginx-new");
        assert_eq!(svc.args, vec!["-g", "daemon off;"]);
        assert_eq!(svc.env, vec![("A".to_string(), "1".to_string())]);
    }

    #[test]
    fn reconfigure_replaces_env_never_retains_old() {
        let mut sup = ServiceSupervisor::new();
        sup.set_fpm_ownership_probe(unowned_probe());
        sup.register(
            ManagedService::new(ServiceKind::PhpFpm, "php-fpm".to_string(), vec![]).with_env(vec![
                (
                    "PHP_INI_SCAN_DIR".to_string(),
                    ":/old/8.3/conf.d".to_string(),
                ),
            ]),
        )
        .unwrap();
        sup.reconfigure_service(
            ServiceKind::PhpFpm,
            "/new/php-fpm".to_string(),
            vec![],
            vec![(
                "PHP_INI_SCAN_DIR".to_string(),
                ":/new/8.4/conf.d".to_string(),
            )],
        )
        .unwrap();
        let svc = sup.services.get(&ServiceKind::PhpFpm).unwrap();
        assert_eq!(
            svc.env,
            vec![(
                "PHP_INI_SCAN_DIR".to_string(),
                ":/new/8.4/conf.d".to_string()
            )]
        );
    }

    #[test]
    fn remove_service_deregisters() {
        let mut sup = make_supervisor_with_service();
        assert!(sup.remove_service(ServiceKind::Nginx));
        assert!(!sup.remove_service(ServiceKind::Nginx));
        assert!(sup.services.is_empty());
    }

    #[test]
    fn reconfigure_unregistered_service_is_noop() {
        let mut sup = ServiceSupervisor::new();
        // Should not panic
        sup.reconfigure_service(
            ServiceKind::Redis,
            "redis-server".to_string(),
            vec![],
            vec![],
        )
        .unwrap();
        assert!(sup.services.is_empty());
    }

    #[test]
    fn status_returns_registered_services() {
        let sup = make_supervisor_with_service();
        let status = sup.status();
        assert_eq!(status.len(), 1);
        assert!(status.contains_key(&ServiceKind::Nginx));
        assert!(matches!(status[&ServiceKind::Nginx], ServiceState::Stopped));
    }

    #[test]
    fn register_replaces_existing_service() {
        let mut sup = ServiceSupervisor::new();
        let svc1 = ManagedService::new(ServiceKind::Nginx, "nginx-old".to_string(), vec![]);
        sup.register(svc1).unwrap();
        let svc2 = ManagedService::new(ServiceKind::Nginx, "nginx-new".to_string(), vec![]);
        sup.register(svc2).unwrap();
        assert_eq!(sup.services.len(), 1);
        assert_eq!(sup.services[&ServiceKind::Nginx].command, "nginx-new");
    }

    #[test]
    fn managed_service_new_has_no_cwd() {
        let svc = ManagedService::new(ServiceKind::Nginx, "true".to_string(), vec![]);
        assert!(svc.cwd().is_none());
    }

    #[test]
    fn managed_service_with_cwd_stores_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = ManagedService::with_cwd(
            ServiceKind::Nginx,
            "true".to_string(),
            vec![],
            tmp.path().to_path_buf(),
        );
        assert_eq!(svc.cwd(), Some(tmp.path()));
    }

    #[test]
    fn managed_service_with_cwd_and_site_renders_display_name() {
        let svc = ManagedService::with_cwd_and_site(
            ServiceKind::Horizon,
            "/php".to_string(),
            vec!["artisan".to_string(), "horizon".to_string()],
            std::path::PathBuf::from("/site"),
            "shopfront",
        );
        assert_eq!(svc.display_name(), "horizon[shopfront]");
        assert_eq!(svc.site_name(), Some("shopfront"));
    }

    #[test]
    fn managed_service_display_name_falls_back_to_kind_name() {
        let svc = ManagedService::new(ServiceKind::Nginx, "nginx".to_string(), vec![]);
        assert_eq!(svc.display_name(), "nginx");
        assert!(svc.site_name().is_none());
    }

    #[test]
    fn managed_service_with_cwd_spawns_in_directory() {
        // Use `pwd` to verify the child's working directory.
        let tmp = tempfile::TempDir::new().unwrap();
        // Canonicalize to mirror what macOS does (symlinks under /var → /private/var, etc.)
        let cwd = std::fs::canonicalize(tmp.path()).unwrap();
        let marker = cwd.join("pwd-output");

        let mut svc = ManagedService::with_cwd(
            ServiceKind::Nginx,
            "sh".to_string(),
            vec!["-c".to_string(), "pwd > pwd-output".to_string()],
            cwd.clone(),
        );
        svc.start(LaunchStamp { generation: 1 }).unwrap();
        // Allow the child to write the file
        std::thread::sleep(std::time::Duration::from_millis(200));
        let _ = svc.stop();

        let contents =
            std::fs::read_to_string(&marker).expect("child should have written pwd-output in cwd");
        let observed = std::path::PathBuf::from(contents.trim());
        let observed = std::fs::canonicalize(&observed).unwrap_or(observed);
        assert_eq!(observed, cwd);
    }

    #[test]
    fn with_env_sets_child_environment() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = std::fs::canonicalize(tmp.path()).unwrap();

        let mut svc = ManagedService::with_cwd(
            ServiceKind::Nginx,
            "sh".to_string(),
            vec![
                "-c".to_string(),
                "printf %s \"$PHP_INI_SCAN_DIR\" > env-output".to_string(),
            ],
            cwd.clone(),
        )
        .with_env(vec![(
            "PHP_INI_SCAN_DIR".to_string(),
            ":/tmp/hearth/php/8.4/conf.d".to_string(),
        )]);
        svc.start(LaunchStamp { generation: 1 }).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let _ = svc.stop();

        let contents = std::fs::read_to_string(cwd.join("env-output"))
            .expect("child should have written env-output");
        assert_eq!(contents, ":/tmp/hearth/php/8.4/conf.d");
    }

    /// Task 8: the trustworthy Hearth-owned FPM start metadata — recorded at
    /// spawn by THIS supervisor, cleared on stop, never inferred from
    /// ambient process facts.
    #[test]
    fn started_at_recorded_on_start_cleared_on_stop() {
        let mut svc = ManagedService::new(
            ServiceKind::PhpFpm,
            "/bin/sleep".to_string(),
            vec!["30".to_string()],
        );
        assert!(svc.started_at().is_none(), "no spawn yet");

        let before = std::time::SystemTime::now();
        svc.start(LaunchStamp { generation: 1 }).unwrap();
        let started = svc.started_at().expect("spawn recorded");
        let after = std::time::SystemTime::now();
        assert!(
            started >= before && started <= after,
            "started_at is the spawn moment"
        );

        svc.stop().unwrap();
        assert!(svc.started_at().is_none(), "cleared on stop");
    }

    // ---- C3-1 (review 5650 r3): current-ownership policy in the supervisor ----

    fn owned_flag() -> (
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        ExternalFpmOwnership,
    ) {
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe_flag = std::sync::Arc::clone(&flag);
        let probe: ExternalFpmOwnership = std::sync::Arc::new(move || {
            if probe_flag.load(std::sync::atomic::Ordering::SeqCst) {
                FpmOwnership::Owned
            } else {
                FpmOwnership::Unowned
            }
        });
        (flag, probe)
    }

    /// C6-1: explicit isolated Unowned evidence for fixtures — probe-less
    /// supervisors refuse all FPM mutation by construction.
    fn unowned_probe() -> ExternalFpmOwnership {
        std::sync::Arc::new(|| FpmOwnership::Unowned)
    }

    fn sleeper(kind: ServiceKind) -> ManagedService {
        ManagedService::new(kind, "/bin/sleep".to_string(), vec!["30".to_string()])
    }

    /// Registered (stopped) FPM + Herd owns it: generic start_all skips FPM
    /// while other services still start; explicit start_service refuses.
    #[test]
    fn generic_start_skips_fpm_under_external_ownership() {
        let (flag, probe) = owned_flag();
        let mut sup = ServiceSupervisor::new();
        sup.set_fpm_ownership_probe(probe);
        sup.register(sleeper(ServiceKind::PhpFpm)).unwrap();
        sup.register(sleeper(ServiceKind::Mailpit)).unwrap();
        flag.store(true, std::sync::atomic::Ordering::SeqCst);

        sup.start_all().unwrap();
        let fpm = sup.service(ServiceKind::PhpFpm).unwrap();
        assert!(fpm.started_at().is_none(), "no FPM child under Herd");
        assert!(matches!(fpm.state, ServiceState::Stopped));
        let other = sup.service(ServiceKind::Mailpit).unwrap();
        assert!(
            other.started_at().is_some(),
            "non-conflicting services start normally"
        );

        let err = sup.start_service(ServiceKind::PhpFpm).unwrap_err();
        assert!(err.to_string().contains("Herd"), "got: {err}");
        assert!(
            sup.service(ServiceKind::PhpFpm)
                .unwrap()
                .started_at()
                .is_none(),
            "explicit start refused with zero spawn"
        );

        sup.stop_all().unwrap();
    }

    /// Running Hearth FPM + Herd becomes live: the next health tick
    /// relinquishes Hearth's OWN child exactly once (state becomes truthful
    /// Stopped, spawn record cleared) and never restarts it afterwards.
    #[test]
    fn health_relinquishes_running_fpm_once_when_ownership_appears() {
        let (flag, probe) = owned_flag();
        let mut sup = ServiceSupervisor::new();
        sup.set_fpm_ownership_probe(probe);
        sup.register(sleeper(ServiceKind::PhpFpm)).unwrap();
        sup.start_service(ServiceKind::PhpFpm).unwrap();
        assert!(
            sup.service(ServiceKind::PhpFpm)
                .unwrap()
                .started_at()
                .is_some()
        );

        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        sup.health_check();
        let fpm = sup.service(ServiceKind::PhpFpm).unwrap();
        assert!(
            matches!(fpm.state, ServiceState::Stopped),
            "{:?}",
            fpm.state
        );
        assert!(fpm.started_at().is_none(), "own child relinquished");

        // Subsequent ticks are no-ops — never restarted while Herd owns it.
        sup.health_check();
        sup.health_check();
        let fpm = sup.service(ServiceKind::PhpFpm).unwrap();
        assert!(matches!(fpm.state, ServiceState::Stopped));
        assert!(fpm.started_at().is_none());
    }

    /// C4-1: stopping an already-exited (zombie) child reaps and finalizes
    /// without misreading macOS's EPERM-for-zombie-groups as a failure.
    #[test]
    fn stop_reaps_already_exited_child_without_false_failure() {
        let mut svc = ManagedService::new(
            ServiceKind::PhpFpm,
            "/bin/sh".to_string(),
            vec!["-c".to_string(), "exit 0".to_string()],
        );
        svc.start(LaunchStamp { generation: 1 }).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        svc.stop().unwrap();
        assert!(matches!(svc.state, ServiceState::Stopped));
        assert!(svc.started_at().is_none());
    }

    /// C4-1: an injected stop failure is TRANSACTIONAL — child handle,
    /// spawn timestamp, and truthful Running state are all retained, and a
    /// later stop succeeds.
    #[test]
    fn injected_stop_failure_is_transactional() {
        let mut sup = ServiceSupervisor::new();
        sup.set_fpm_ownership_probe(unowned_probe());
        sup.register(sleeper(ServiceKind::PhpFpm)).unwrap();
        sup.start_service(ServiceKind::PhpFpm).unwrap();
        let started = sup
            .service(ServiceKind::PhpFpm)
            .unwrap()
            .started_at()
            .expect("running");

        sup.inject_stop_failure(ServiceKind::PhpFpm, "sigterm denied");
        let err = sup.stop_service(ServiceKind::PhpFpm).unwrap_err();
        assert!(err.to_string().contains("injected stop failure"), "{err}");
        let svc = sup.service(ServiceKind::PhpFpm).unwrap();
        assert!(
            matches!(svc.state, ServiceState::Running { .. }),
            "no false Stopped: {:?}",
            svc.state
        );
        assert_eq!(svc.started_at(), Some(started), "timestamp retained");
        assert!(svc.child.is_some(), "child handle retained for retry");

        // Retry without injection completes the transition.
        sup.stop_service(ServiceKind::PhpFpm).unwrap();
        let svc = sup.service(ServiceKind::PhpFpm).unwrap();
        assert!(matches!(svc.state, ServiceState::Stopped));
        assert!(svc.started_at().is_none());
        assert!(svc.child.is_none());
    }

    /// C4-1: a FAILED handoff stop keeps the child fully supervised with
    /// its truthful state (no false Stopped, no replacement) and a later
    /// health tick completes the handoff.
    #[test]
    fn health_handoff_stop_failure_keeps_supervision_and_retries() {
        let (flag, probe) = owned_flag();
        let mut sup = ServiceSupervisor::new();
        sup.set_fpm_ownership_probe(probe);
        sup.register(sleeper(ServiceKind::PhpFpm)).unwrap();
        sup.start_service(ServiceKind::PhpFpm).unwrap();
        let started = sup
            .service(ServiceKind::PhpFpm)
            .unwrap()
            .started_at()
            .expect("running");

        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        sup.inject_stop_failure(ServiceKind::PhpFpm, "handoff sigterm denied");
        sup.health_check();
        let svc = sup.service(ServiceKind::PhpFpm).unwrap();
        assert!(
            matches!(svc.state, ServiceState::Running { .. }),
            "failed handoff must not mark false Stopped: {:?}",
            svc.state
        );
        assert_eq!(
            svc.started_at(),
            Some(started),
            "same spawn — no replacement child"
        );
        assert!(svc.child.is_some(), "child still supervised");

        // Next tick (no injection) completes the handoff exactly once.
        sup.health_check();
        let svc = sup.service(ServiceKind::PhpFpm).unwrap();
        assert!(matches!(svc.state, ServiceState::Stopped));
        assert!(svc.started_at().is_none());
        assert!(svc.child.is_none(), "handoff completed, no orphan handle");
    }

    /// C4-1: stop_all aggregates failures instead of returning false
    /// success, while still stopping the other services.
    #[test]
    fn stop_all_aggregates_failures_but_stops_others() {
        let mut sup = ServiceSupervisor::new();
        sup.set_fpm_ownership_probe(unowned_probe());
        sup.register(sleeper(ServiceKind::PhpFpm)).unwrap();
        sup.register(sleeper(ServiceKind::Mailpit)).unwrap();
        sup.start_service(ServiceKind::PhpFpm).unwrap();
        sup.start_service(ServiceKind::Mailpit).unwrap();

        sup.inject_stop_failure(ServiceKind::PhpFpm, "stuck group");
        let err = sup.stop_all().unwrap_err();
        assert!(err.to_string().contains("php-fpm"), "{err}");
        let other = sup.service(ServiceKind::Mailpit).unwrap();
        assert!(
            matches!(other.state, ServiceState::Stopped),
            "other services still stopped: {:?}",
            other.state
        );
        // The failed service is still supervised and stoppable.
        sup.stop_service(ServiceKind::PhpFpm).unwrap();
    }

    /// C5-1B TOCTOU: ownership flips to Owned AFTER a legitimate Unowned
    /// registration — the boundary re-check refuses further register/
    /// reconfigure with zero mutation; the existing registration stands and
    /// non-FPM services are unaffected.
    #[test]
    fn register_and_reconfigure_refuse_fpm_after_flip_to_owned() {
        let (flag, probe) = owned_flag();
        let mut sup = ServiceSupervisor::new();
        sup.set_fpm_ownership_probe(probe);
        sup.register(sleeper(ServiceKind::PhpFpm)).unwrap(); // Unowned: normal

        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        let err = sup.register(sleeper(ServiceKind::PhpFpm)).unwrap_err();
        assert!(err.to_string().contains("owned by Herd"), "{err}");
        let err = sup
            .reconfigure_service(ServiceKind::PhpFpm, "/bin/echo".to_string(), vec![], vec![])
            .unwrap_err();
        assert!(err.to_string().contains("owned by Herd"), "{err}");
        let fpm = sup.service(ServiceKind::PhpFpm).unwrap();
        assert_eq!(fpm.command(), "/bin/sleep", "zero mutation on refusal");
        assert!(fpm.started_at().is_none(), "never spawned, no leak");

        // Non-FPM registration/reconfiguration unaffected under Owned.
        sup.register(sleeper(ServiceKind::Mailpit)).unwrap();
        sup.reconfigure_service(
            ServiceKind::Mailpit,
            "/bin/echo".to_string(),
            vec![],
            vec![],
        )
        .unwrap();
    }

    /// C4-2: unknown ownership fails CLOSED on every supervisor path — no
    /// start, no restart, no relinquish, no replacement.
    #[test]
    fn unknown_ownership_fails_closed_everywhere() {
        let probe: ExternalFpmOwnership = std::sync::Arc::new(|| {
            FpmOwnership::Unknown("ownership probe (pgrep) failed with status 2".to_string())
        });
        let mut sup = ServiceSupervisor::new();
        // C6-1: register under EXPLICIT Unowned evidence, THEN swap in the
        // Unknown probe — registering probe-less or under Unknown is refused.
        sup.set_fpm_ownership_probe(unowned_probe());
        sup.register(sleeper(ServiceKind::PhpFpm)).unwrap();
        sup.register(sleeper(ServiceKind::Mailpit)).unwrap();
        sup.set_fpm_ownership_probe(std::sync::Arc::clone(&probe));

        // Registration and reconfiguration are refused at the boundary
        // under Unknown, with zero mutation.
        let err = sup.register(sleeper(ServiceKind::PhpFpm)).unwrap_err();
        assert!(err.to_string().contains("registration refused"), "{err}");
        let original_command = sup
            .service(ServiceKind::PhpFpm)
            .unwrap()
            .command()
            .to_string();
        let err = sup
            .reconfigure_service(ServiceKind::PhpFpm, "/bin/echo".to_string(), vec![], vec![])
            .unwrap_err();
        assert!(err.to_string().contains("reconfiguration refused"), "{err}");
        assert_eq!(
            sup.service(ServiceKind::PhpFpm).unwrap().command(),
            original_command,
            "zero mutation on refusal"
        );

        // Generic start: FPM skipped, others start.
        sup.start_all().unwrap();
        assert!(
            sup.service(ServiceKind::PhpFpm)
                .unwrap()
                .started_at()
                .is_none()
        );
        assert!(
            sup.service(ServiceKind::Mailpit)
                .unwrap()
                .started_at()
                .is_some()
        );

        // Explicit start: actionable refusal, zero spawn.
        let err = sup.start_service(ServiceKind::PhpFpm).unwrap_err();
        assert!(err.to_string().contains("unknown"), "{err}");
        assert!(
            sup.service(ServiceKind::PhpFpm)
                .unwrap()
                .started_at()
                .is_none()
        );

        // Health with a RUNNING child under unknown ownership: conservative —
        // the child stays supervised, no relinquish, no restart.
        let mut owned_sup = ServiceSupervisor::new();
        sup.stop_all().unwrap();
        owned_sup.set_fpm_ownership_probe(unowned_probe());
        owned_sup.register(sleeper(ServiceKind::PhpFpm)).unwrap();
        owned_sup.start_service(ServiceKind::PhpFpm).unwrap();
        let started = owned_sup
            .service(ServiceKind::PhpFpm)
            .unwrap()
            .started_at()
            .expect("running");
        owned_sup.set_fpm_ownership_probe(probe);
        owned_sup.health_check();
        let svc = owned_sup.service(ServiceKind::PhpFpm).unwrap();
        assert!(matches!(svc.state, ServiceState::Running { .. }));
        assert_eq!(svc.started_at(), Some(started), "no relinquish, no respawn");
        let _ = owned_sup.stop_all();
    }

    /// Crashed FPM (recorded Running, child gone) + Herd owns it: the health
    /// tick must not start a replacement child.
    #[test]
    fn health_never_replaces_crashed_fpm_under_external_ownership() {
        let (flag, probe) = owned_flag();
        let mut sup = ServiceSupervisor::new();
        sup.set_fpm_ownership_probe(probe);
        // A child that exits immediately: recorded Running, actually gone.
        sup.register(ManagedService::new(
            ServiceKind::PhpFpm,
            "/bin/sh".to_string(),
            vec!["-c".to_string(), "exit 0".to_string()],
        ))
        .unwrap();
        sup.start_service(ServiceKind::PhpFpm).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));

        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        sup.health_check();
        let fpm = sup.service(ServiceKind::PhpFpm).unwrap();
        assert!(
            !matches!(fpm.state, ServiceState::Running { .. }),
            "no replacement child under Herd: {:?}",
            fpm.state
        );
        assert!(fpm.started_at().is_none(), "no new spawn recorded");
    }

    /// Ownership false: behavior is exactly the pre-C3-1 behavior — health
    /// restarts a crashed service, start_all starts FPM.
    #[test]
    fn ownership_false_keeps_existing_behavior() {
        let (_flag, probe) = owned_flag(); // stays false
        let mut sup = ServiceSupervisor::new();
        sup.set_fpm_ownership_probe(probe);
        sup.register(ManagedService::new(
            ServiceKind::PhpFpm,
            "/bin/sh".to_string(),
            vec!["-c".to_string(), "exit 0".to_string()],
        ))
        .unwrap();
        sup.start_all().unwrap();
        assert!(
            sup.service(ServiceKind::PhpFpm)
                .unwrap()
                .started_at()
                .is_some()
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
        let crashed = sup.health_check();
        assert!(crashed.contains(&ServiceKind::PhpFpm), "crash detected");
        // Restart attempted (spawn recorded again by the restart path).
        assert!(
            sup.service(ServiceKind::PhpFpm)
                .unwrap()
                .started_at()
                .is_some(),
            "herd-absent crash restart unchanged"
        );
        let _ = sup.stop_all();
    }

    // ---- C6-1 (review 5650 r7): probe-less supervisor fails closed ----

    /// A freshly constructed supervisor carries NO ownership evidence: the
    /// built-in probe reports Unknown("… not configured") and every FPM
    /// mutation path (register, reconfigure, direct start, replace) refuses
    /// fail-closed with zero mutation, while non-FPM services operate
    /// normally. Explicit Unowned evidence then unlocks FPM. Fails at head
    /// 79d54d2 (probe-less `Option` treated missing evidence as Unowned).
    #[test]
    fn unconfigured_supervisor_fails_closed_on_every_fpm_path() {
        let mut sup = ServiceSupervisor::new();
        match sup.current_fpm_ownership() {
            FpmOwnership::Unknown(diag) => {
                assert!(diag.contains("not configured"), "{diag}")
            }
            other => panic!("expected Unknown(not configured), got {other:?}"),
        }

        let err = sup.register(sleeper(ServiceKind::PhpFpm)).unwrap_err();
        assert!(err.to_string().contains("not configured"), "{err}");
        assert!(sup.service(ServiceKind::PhpFpm).is_none(), "zero mutation");

        let err = sup
            .reconfigure_service(ServiceKind::PhpFpm, "/bin/echo".to_string(), vec![], vec![])
            .unwrap_err();
        assert!(err.to_string().contains("not configured"), "{err}");

        let err = sup.start_service(ServiceKind::PhpFpm).unwrap_err();
        assert!(err.to_string().contains("not configured"), "{err}");

        match sup.replace_fpm_service(|| panic!("build must not run under Unknown")) {
            FpmReplaceOutcome::SkippedOwnershipUnknown(diag) => {
                assert!(diag.contains("not configured"), "{diag}")
            }
            other => panic!("expected SkippedOwnershipUnknown, got {other:?}"),
        }

        // Non-FPM services are entirely unaffected by the missing probe…
        sup.register(sleeper(ServiceKind::Mailpit)).unwrap();
        sup.start_all().unwrap();
        assert!(
            sup.service(ServiceKind::Mailpit)
                .unwrap()
                .started_at()
                .is_some()
        );
        sup.stop_all().unwrap();

        // …and explicit Unowned evidence unlocks normal FPM operation.
        sup.set_fpm_ownership_probe(unowned_probe());
        sup.register(sleeper(ServiceKind::PhpFpm)).unwrap();
        sup.start_service(ServiceKind::PhpFpm).unwrap();
        sup.stop_service(ServiceKind::PhpFpm).unwrap();
    }

    // ---- C6-2 (review 5650 r7): atomic FPM replacement transaction ----

    /// Ownership flips to Owned BEFORE the transaction snapshot: truthful
    /// skip with ZERO mutation — the build closure never runs and the old
    /// child keeps running untouched.
    #[test]
    fn replace_fpm_skips_with_zero_mutation_when_snapshot_sees_owned() {
        let (flag, probe) = owned_flag();
        let mut sup = ServiceSupervisor::new();
        sup.set_fpm_ownership_probe(probe);
        sup.register(sleeper(ServiceKind::PhpFpm)).unwrap();
        sup.start_service(ServiceKind::PhpFpm).unwrap();
        let started = sup
            .service(ServiceKind::PhpFpm)
            .unwrap()
            .started_at()
            .expect("running");

        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        match sup.replace_fpm_service(|| panic!("build must not run under Owned")) {
            FpmReplaceOutcome::SkippedOwned => {}
            other => panic!("expected SkippedOwned, got {other:?}"),
        }
        let svc = sup.service(ServiceKind::PhpFpm).unwrap();
        assert_eq!(svc.started_at(), Some(started), "old child untouched");
        assert!(
            matches!(svc.state, ServiceState::Running { .. }),
            "{:?}",
            svc.state
        );

        flag.store(false, std::sync::atomic::Ordering::SeqCst);
        sup.stop_all().unwrap();
    }

    /// Unowned snapshot: one coherent transaction — the old child is
    /// stopped and the replacement is registered AND started with the NEW
    /// command/args/env. A later flip is the health handoff's job.
    #[test]
    fn replace_fpm_replaces_old_child_with_new_service_under_unowned() {
        let mut sup = ServiceSupervisor::new();
        sup.set_fpm_ownership_probe(unowned_probe());
        sup.register(sleeper(ServiceKind::PhpFpm)).unwrap();
        sup.start_service(ServiceKind::PhpFpm).unwrap();

        let outcome = sup.replace_fpm_service(|| {
            Ok(ManagedService::new(
                ServiceKind::PhpFpm,
                "/bin/sleep".to_string(),
                vec!["37".to_string()],
            )
            .with_env(vec![(
                "PHP_INI_SCAN_DIR".to_string(),
                ":/new/8.4/conf.d".to_string(),
            )]))
        });
        assert!(
            matches!(outcome, FpmReplaceOutcome::Replaced),
            "{outcome:?}"
        );
        let svc = sup.service(ServiceKind::PhpFpm).unwrap();
        assert_eq!(svc.args, vec!["37"], "replacement config took effect");
        assert_eq!(
            svc.env,
            vec![(
                "PHP_INI_SCAN_DIR".to_string(),
                ":/new/8.4/conf.d".to_string()
            )]
        );
        assert!(svc.started_at().is_some(), "replacement running");
        assert!(
            matches!(svc.state, ServiceState::Running { .. }),
            "{:?}",
            svc.state
        );
        sup.stop_all().unwrap();
    }

    /// A failing stop aborts the transaction truthfully: `StopFailed`, the
    /// old child remains fully supervised and running, and the replacement
    /// is never inserted.
    #[test]
    fn replace_fpm_stop_failure_keeps_old_child_and_registration() {
        let mut sup = ServiceSupervisor::new();
        sup.set_fpm_ownership_probe(unowned_probe());
        sup.register(sleeper(ServiceKind::PhpFpm)).unwrap();
        sup.start_service(ServiceKind::PhpFpm).unwrap();
        let started = sup
            .service(ServiceKind::PhpFpm)
            .unwrap()
            .started_at()
            .expect("running");

        sup.inject_stop_failure(ServiceKind::PhpFpm, "sigterm denied");
        let outcome = sup.replace_fpm_service(|| {
            Ok(ManagedService::new(
                ServiceKind::PhpFpm,
                "/bin/echo".to_string(),
                vec![],
            ))
        });
        match outcome {
            FpmReplaceOutcome::StopFailed { error } => {
                assert!(error.contains("injected stop failure"), "{error}")
            }
            other => panic!("expected StopFailed, got {other:?}"),
        }
        let svc = sup.service(ServiceKind::PhpFpm).unwrap();
        assert!(
            matches!(svc.state, ServiceState::Running { .. }),
            "no false Stopped: {:?}",
            svc.state
        );
        assert_eq!(svc.started_at(), Some(started), "old child intact");
        assert!(svc.child.is_some(), "child handle retained for retry");
        assert_ne!(svc.command, "/bin/echo", "replacement never inserted");
        sup.stop_all().unwrap();
    }

    /// Build failure with a RUNNING old child: `BuildFailed` with
    /// `removed_stale: false` — a broken build must never cost a working
    /// FPM.
    #[test]
    fn replace_fpm_build_failure_never_stops_running_old_child() {
        let mut sup = ServiceSupervisor::new();
        sup.set_fpm_ownership_probe(unowned_probe());
        sup.register(sleeper(ServiceKind::PhpFpm)).unwrap();
        sup.start_service(ServiceKind::PhpFpm).unwrap();
        let started = sup
            .service(ServiceKind::PhpFpm)
            .unwrap()
            .started_at()
            .expect("running");

        let outcome =
            sup.replace_fpm_service(|| Err("launch-blocked: missing fpm config".to_string()));
        match outcome {
            FpmReplaceOutcome::BuildFailed {
                reason,
                removed_stale,
            } => {
                assert!(reason.contains("launch-blocked"), "{reason}");
                assert!(!removed_stale, "running child never removed");
            }
            other => panic!("expected BuildFailed, got {other:?}"),
        }
        let svc = sup.service(ServiceKind::PhpFpm).unwrap();
        assert_eq!(svc.started_at(), Some(started), "old child untouched");
        sup.stop_all().unwrap();
    }

    /// Build failure with a stale NON-RUNNING registration removes it and
    /// says so; with no registration at all, `removed_stale` is false.
    #[test]
    fn replace_fpm_build_failure_removes_only_stale_non_running_registration() {
        let mut sup = ServiceSupervisor::new();
        sup.set_fpm_ownership_probe(unowned_probe());
        sup.register(sleeper(ServiceKind::PhpFpm)).unwrap();

        let outcome = sup.replace_fpm_service(|| Err("boom".to_string()));
        assert!(
            matches!(
                outcome,
                FpmReplaceOutcome::BuildFailed {
                    removed_stale: true,
                    ..
                }
            ),
            "{outcome:?}"
        );
        assert!(
            sup.service(ServiceKind::PhpFpm).is_none(),
            "stale registration removed"
        );

        let outcome = sup.replace_fpm_service(|| Err("boom".to_string()));
        assert!(
            matches!(
                outcome,
                FpmReplaceOutcome::BuildFailed {
                    removed_stale: false,
                    ..
                }
            ),
            "{outcome:?}"
        );
    }

    /// Start failure of the replacement: truthful PARTIAL state — the
    /// replacement stays registered in `Failed`, never reported as
    /// "untouched" and never the old service resurrected.
    #[test]
    fn replace_fpm_start_failure_reports_truthful_partial_state() {
        let mut sup = ServiceSupervisor::new();
        sup.set_fpm_ownership_probe(unowned_probe());
        sup.register(sleeper(ServiceKind::PhpFpm)).unwrap();
        sup.start_service(ServiceKind::PhpFpm).unwrap();

        let outcome = sup.replace_fpm_service(|| {
            Ok(ManagedService::new(
                ServiceKind::PhpFpm,
                "/nonexistent/php-fpm-definitely-missing".to_string(),
                vec![],
            ))
        });
        match outcome {
            FpmReplaceOutcome::StartFailed { error } => {
                assert!(!error.is_empty(), "diagnostic surfaced")
            }
            other => panic!("expected StartFailed, got {other:?}"),
        }
        let svc = sup.service(ServiceKind::PhpFpm).unwrap();
        assert_eq!(
            svc.command, "/nonexistent/php-fpm-definitely-missing",
            "the replacement (not the old service) is what remains"
        );
        assert!(
            matches!(svc.state, ServiceState::Failed { .. }),
            "truthful Failed state: {:?}",
            svc.state
        );
        assert!(svc.started_at().is_none(), "never recorded as running");
    }

    // ---- C7-1 (review 5650 r8): atomic FPM restart transaction ----

    /// Owned or Unknown at the restart snapshot: truthful skip BEFORE any
    /// stop — old registration, child, timestamp, and state untouched.
    /// Fails at head 8b06320 (no transaction existed; restart composed
    /// public stop with the guarded start and stopped the child first).
    #[test]
    fn restart_fpm_tx_skips_zero_mutation_under_owned_and_unknown() {
        let (flag, probe) = owned_flag();
        let mut sup = ServiceSupervisor::new();
        sup.set_fpm_ownership_probe(probe);
        sup.register(sleeper(ServiceKind::PhpFpm)).unwrap();
        sup.start_service(ServiceKind::PhpFpm).unwrap();
        let started = sup
            .service(ServiceKind::PhpFpm)
            .unwrap()
            .started_at()
            .expect("running");

        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        match sup.restart_fpm_service() {
            FpmRestartTxOutcome::SkippedOwned => {}
            other => panic!("expected SkippedOwned, got {other:?}"),
        }
        {
            let svc = sup.service(ServiceKind::PhpFpm).unwrap();
            assert_eq!(svc.started_at(), Some(started), "old child untouched");
            assert!(
                matches!(svc.state, ServiceState::Running { .. }),
                "{:?}",
                svc.state
            );
        }

        flag.store(false, std::sync::atomic::Ordering::SeqCst);
        sup.set_fpm_ownership_probe(std::sync::Arc::new(|| {
            FpmOwnership::Unknown("ownership probe (pgrep) failed with status 2".to_string())
        }));
        match sup.restart_fpm_service() {
            FpmRestartTxOutcome::SkippedOwnershipUnknown(diag) => {
                assert!(diag.contains("pgrep"), "{diag}")
            }
            other => panic!("expected SkippedOwnershipUnknown, got {other:?}"),
        }
        {
            let svc = sup.service(ServiceKind::PhpFpm).unwrap();
            assert_eq!(svc.started_at(), Some(started), "old child untouched");
            assert!(
                matches!(svc.state, ServiceState::Running { .. }),
                "{:?}",
                svc.state
            );
        }
        sup.set_fpm_ownership_probe(unowned_probe());
        sup.stop_all().unwrap();
    }

    /// An unconfigured (default-probe) supervisor refuses the restart
    /// transaction fail-closed with zero mutation.
    #[test]
    fn restart_fpm_tx_unconfigured_supervisor_refuses_zero_mutation() {
        let mut sup = ServiceSupervisor::new();
        match sup.restart_fpm_service() {
            FpmRestartTxOutcome::SkippedOwnershipUnknown(diag) => {
                assert!(diag.contains("not configured"), "{diag}")
            }
            other => panic!("expected SkippedOwnershipUnknown, got {other:?}"),
        }
        assert!(sup.services.is_empty(), "zero mutation");
    }

    /// Unowned snapshot: one coherent stop/start of the SAME registration —
    /// fresh spawn, same command, Running state.
    #[test]
    fn restart_fpm_tx_restarts_under_unowned() {
        let mut sup = ServiceSupervisor::new();
        sup.set_fpm_ownership_probe(unowned_probe());
        sup.register(sleeper(ServiceKind::PhpFpm)).unwrap();
        sup.start_service(ServiceKind::PhpFpm).unwrap();
        let old_pid = match sup.service(ServiceKind::PhpFpm).unwrap().state {
            ServiceState::Running { pid } => pid,
            ref other => panic!("running expected, got {other:?}"),
        };

        match sup.restart_fpm_service() {
            FpmRestartTxOutcome::Restarted => {}
            other => panic!("expected Restarted, got {other:?}"),
        }
        let svc = sup.service(ServiceKind::PhpFpm).unwrap();
        match svc.state {
            ServiceState::Running { pid } => assert_ne!(pid, old_pid, "fresh spawn"),
            ref other => panic!("running expected, got {other:?}"),
        }
        assert!(svc.started_at().is_some());
        sup.stop_all().unwrap();

        // Nothing registered: truthful NotRegistered, zero mutation.
        let mut empty = ServiceSupervisor::new();
        empty.set_fpm_ownership_probe(unowned_probe());
        match empty.restart_fpm_service() {
            FpmRestartTxOutcome::NotRegistered => {}
            other => panic!("expected NotRegistered, got {other:?}"),
        }
    }

    /// A failing stop aborts the restart BEFORE any start: the old child
    /// remains fully supervised with its truthful state (C4 semantics).
    #[test]
    fn restart_fpm_tx_stop_failure_keeps_old_child() {
        let mut sup = ServiceSupervisor::new();
        sup.set_fpm_ownership_probe(unowned_probe());
        sup.register(sleeper(ServiceKind::PhpFpm)).unwrap();
        sup.start_service(ServiceKind::PhpFpm).unwrap();
        let started = sup
            .service(ServiceKind::PhpFpm)
            .unwrap()
            .started_at()
            .expect("running");

        sup.inject_stop_failure(ServiceKind::PhpFpm, "sigterm denied");
        match sup.restart_fpm_service() {
            FpmRestartTxOutcome::StopFailed { error } => {
                assert!(error.contains("injected stop failure"), "{error}")
            }
            other => panic!("expected StopFailed, got {other:?}"),
        }
        let svc = sup.service(ServiceKind::PhpFpm).unwrap();
        assert!(
            matches!(svc.state, ServiceState::Running { .. }),
            "no false Stopped: {:?}",
            svc.state
        );
        assert_eq!(svc.started_at(), Some(started), "old child intact");
        assert!(svc.child.is_some(), "child handle retained for retry");
        sup.stop_all().unwrap();
    }

    /// Start failure after a successful stop: truthful `Failed`
    /// registration remains supervised, no child, no orphan — never
    /// reported as restarted or untouched.
    #[test]
    fn restart_fpm_tx_start_failure_reports_truthful_failed_state() {
        // A binary that deletes itself on first run: initial start works,
        // the restart's stop succeeds, the restart's start cannot spawn.
        let tmp = tempfile::TempDir::new().unwrap();
        let bin = tmp.path().join("self-deleting-fpm.sh");
        std::fs::write(&bin, "#!/bin/sh\nrm -f \"$0\"\nexec sleep 30\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut sup = ServiceSupervisor::new();
        sup.set_fpm_ownership_probe(unowned_probe());
        sup.register(ManagedService::new(
            ServiceKind::PhpFpm,
            bin.to_string_lossy().to_string(),
            vec![],
        ))
        .unwrap();
        sup.start_service(ServiceKind::PhpFpm).unwrap();
        // Wait (bounded) until the script has consumed itself — a fixed
        // sleep is racy under full-suite parallel load.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while bin.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(!bin.exists(), "fixture: binary must have deleted itself");

        match sup.restart_fpm_service() {
            FpmRestartTxOutcome::StartFailed { error } => {
                assert!(!error.is_empty(), "diagnostic surfaced")
            }
            other => panic!("expected StartFailed, got {other:?}"),
        }
        let svc = sup.service(ServiceKind::PhpFpm).unwrap();
        assert!(
            matches!(svc.state, ServiceState::Failed { .. }),
            "truthful Failed state: {:?}",
            svc.state
        );
        assert!(svc.child.is_none(), "no child, no orphan");
        assert!(svc.started_at().is_none(), "never recorded as running");
    }

    /// All-services restart: FPM goes through the atomic transaction
    /// (Owned/Unknown skip BEFORE any stop), non-FPM services restart
    /// normally, and the aggregate reports the truth.
    #[test]
    fn restart_all_handles_fpm_atomically_and_non_fpm_normally() {
        let (flag, probe) = owned_flag();
        let mut sup = ServiceSupervisor::new();
        sup.set_fpm_ownership_probe(probe);
        sup.register(sleeper(ServiceKind::PhpFpm)).unwrap();
        sup.register(sleeper(ServiceKind::Mailpit)).unwrap();
        sup.start_service(ServiceKind::PhpFpm).unwrap();
        sup.start_service(ServiceKind::Mailpit).unwrap();
        let fpm_started = sup
            .service(ServiceKind::PhpFpm)
            .unwrap()
            .started_at()
            .expect("running");
        let mailpit_started = sup
            .service(ServiceKind::Mailpit)
            .unwrap()
            .started_at()
            .expect("running");

        // Herd takes ownership: FPM untouched, Mailpit restarts.
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        let (fpm, errors) = sup.restart_all();
        assert!(errors.is_empty(), "{errors:?}");
        assert!(
            matches!(fpm, Some(FpmRestartTxOutcome::SkippedOwned)),
            "{fpm:?}"
        );
        {
            let svc = sup.service(ServiceKind::PhpFpm).unwrap();
            assert_eq!(svc.started_at(), Some(fpm_started), "FPM untouched");
            assert!(matches!(svc.state, ServiceState::Running { .. }));
            let mp = sup.service(ServiceKind::Mailpit).unwrap();
            assert!(mp.started_at().is_some());
            assert_ne!(
                mp.started_at(),
                Some(mailpit_started),
                "non-FPM service actually restarted"
            );
        }

        // Unowned again: FPM restarts coherently too.
        flag.store(false, std::sync::atomic::Ordering::SeqCst);
        let (fpm, errors) = sup.restart_all();
        assert!(errors.is_empty(), "{errors:?}");
        assert!(
            matches!(fpm, Some(FpmRestartTxOutcome::Restarted)),
            "{fpm:?}"
        );
        assert_ne!(
            sup.service(ServiceKind::PhpFpm).unwrap().started_at(),
            Some(fpm_started),
            "FPM restarted under Unowned"
        );
        sup.stop_all().unwrap();
    }

    fn stamped_fpm_service(command: &str, args: Vec<String>) -> ManagedService {
        ManagedService::new(ServiceKind::PhpFpm, command.to_string(), args).with_fpm_launch_conf(
            FpmLaunchConf::HearthOwned {
                conf: PathBuf::from("/tmp/hearth/fpm/php-fpm.conf"),
                conf_sha256: "conf-sha".to_string(),
                probe_sha256: "probe-sha".to_string(),
                listen: PathBuf::from("/tmp/hearth/run/php-fpm.sock"),
            },
        )
    }

    fn stamped_sleeper() -> ManagedService {
        stamped_fpm_service("/bin/sleep", vec!["30".to_string()])
    }

    #[test]
    fn start_all_stamps_fresh_generation() {
        let mut sup = ServiceSupervisor::new();
        sup.set_fpm_ownership_probe(unowned_probe());
        sup.register(stamped_sleeper()).unwrap();
        sup.start_all().unwrap();
        let snapshot = sup.fpm_launch_snapshot().expect("stamped FPM");
        assert!(snapshot.generation > 0);
        assert!(snapshot.group_id.is_some());
        assert!(snapshot.running);
        sup.stop_all().unwrap();
    }

    #[test]
    fn named_start_service_stamps_fresh_generation() {
        let mut sup = ServiceSupervisor::new();
        sup.set_fpm_ownership_probe(unowned_probe());
        sup.register(stamped_sleeper()).unwrap();
        sup.start_service(ServiceKind::PhpFpm).unwrap();
        let first = sup.fpm_launch_snapshot().unwrap().generation;
        sup.stop_service(ServiceKind::PhpFpm).unwrap();
        sup.start_service(ServiceKind::PhpFpm).unwrap();
        let second = sup.fpm_launch_snapshot().unwrap().generation;
        sup.stop_service(ServiceKind::PhpFpm).unwrap();
        sup.start_service(ServiceKind::PhpFpm).unwrap();
        let third = sup.fpm_launch_snapshot().unwrap().generation;
        assert!(first < second && second < third);
        sup.stop_all().unwrap();
    }

    #[test]
    fn replace_fpm_service_stamps_fresh_generation() {
        let mut sup = ServiceSupervisor::new();
        sup.set_fpm_ownership_probe(unowned_probe());
        sup.register(stamped_sleeper()).unwrap();
        sup.start_service(ServiceKind::PhpFpm).unwrap();
        let before = sup.fpm_launch_snapshot().unwrap();
        assert!(matches!(
            sup.replace_fpm_service(|| Ok(stamped_sleeper())),
            FpmReplaceOutcome::Replaced
        ));
        let after = sup.fpm_launch_snapshot().unwrap();
        assert!(after.generation > before.generation);
        assert_ne!(after.group_id, before.group_id);
        sup.stop_all().unwrap();
    }

    #[test]
    fn restart_fpm_service_and_restart_all_stamp_fresh_generations() {
        let mut sup = ServiceSupervisor::new();
        sup.set_fpm_ownership_probe(unowned_probe());
        sup.register(stamped_sleeper()).unwrap();
        sup.register(sleeper(ServiceKind::Mailpit)).unwrap();
        sup.start_all().unwrap();
        let first = sup.fpm_launch_snapshot().unwrap().generation;
        assert_eq!(sup.restart_fpm_service(), FpmRestartTxOutcome::Restarted);
        let second = sup.fpm_launch_snapshot().unwrap().generation;
        let mailpit_before = sup
            .service(ServiceKind::Mailpit)
            .unwrap()
            .launch_generation
            .unwrap();
        let (fpm, errors) = sup.restart_all();
        assert!(errors.is_empty());
        assert_eq!(fpm, Some(FpmRestartTxOutcome::Restarted));
        let third = sup.fpm_launch_snapshot().unwrap().generation;
        let mailpit_after = sup
            .service(ServiceKind::Mailpit)
            .unwrap()
            .launch_generation
            .unwrap();
        assert!(first < second && second < third);
        assert!(
            mailpit_after > mailpit_before,
            "uniform seam stamps non-FPM starts too"
        );
        sup.stop_all().unwrap();
    }

    #[test]
    fn health_respawn_stamps_fresh_generation_and_new_group() {
        let mut sup = ServiceSupervisor::new();
        sup.set_fpm_ownership_probe(unowned_probe());
        sup.register(stamped_fpm_service(
            "/bin/sh",
            vec!["-c".to_string(), "exit 0".to_string()],
        ))
        .unwrap();
        sup.start_service(ServiceKind::PhpFpm).unwrap();
        let before = sup.fpm_launch_snapshot().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let crashed = sup.health_check();
        assert!(crashed.contains(&ServiceKind::PhpFpm));
        let after = sup.fpm_launch_snapshot().unwrap();
        assert!(after.generation > before.generation);
        assert_ne!(after.group_id, before.group_id);
        let _ = sup.stop_all();
    }

    #[test]
    fn generation_cleared_only_on_positively_confirmed_termination() {
        let mut sup = ServiceSupervisor::new();
        sup.set_fpm_ownership_probe(unowned_probe());
        sup.register(stamped_sleeper()).unwrap();
        sup.start_service(ServiceKind::PhpFpm).unwrap();
        let before = sup.fpm_launch_snapshot().unwrap();
        sup.inject_stop_failure(ServiceKind::PhpFpm, "retain launch metadata");
        assert!(sup.stop_service(ServiceKind::PhpFpm).is_err());
        let retained = sup.fpm_launch_snapshot().unwrap();
        assert_eq!(retained.generation, before.generation);
        assert_eq!(retained.group_id, before.group_id);
        assert_eq!(retained.started_at, before.started_at);
        sup.stop_service(ServiceKind::PhpFpm).unwrap();
        assert!(sup.fpm_launch_snapshot().is_none());
    }
}
