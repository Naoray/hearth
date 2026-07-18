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

    /// Create a managed service that runs in a specific working directory.
    ///
    /// Used by supervised Laravel workers (Horizon, Reverb) which must run inside
    /// the site root so `php artisan` finds the application bootstrap.
    pub fn with_cwd(
        kind: ServiceKind,
        command: String,
        args: Vec<String>,
        cwd: PathBuf,
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
            site_name: None,
            env: Vec::new(),
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
    pub fn start(&mut self) -> anyhow::Result<()> {
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

        info!(service = %self.kind, pid, "service started");
        Ok(())
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
            self.started_at = None;
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
                self.started_at = None;
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

        if let ShutdownStrategy::Postgres { datadir, pg_ctl_binary } = &self.shutdown_strategy {
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
                            self.started_at = None;
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
                    self.started_at = None;
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
                self.started_at = None;
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
                self.started_at = None;
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
    /// C3-1 (review 5650 r3): injectable "does an external tool (Herd)
    /// currently own PHP-FPM?" probe — ONE coherent ownership policy
    /// enforced INSIDE the supervisor so every mutation path (boot,
    /// generic start/restart, health supervision, config restart, switch)
    /// obeys it without per-caller audits. `None` = never externally owned.
    fpm_ownership: Option<ExternalFpmOwnership>,
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
            fpm_ownership: None,
        }
    }

    /// Install the current-Herd-ownership probe for PHP-FPM (C3-1).
    pub fn set_fpm_ownership_probe(&mut self, probe: ExternalFpmOwnership) {
        self.fpm_ownership = Some(probe);
    }

    /// Current PHP-FPM ownership, consulted live on every FPM-mutating
    /// path; `Unowned` when no probe is installed.
    fn current_fpm_ownership(&self) -> FpmOwnership {
        self.fpm_ownership
            .as_ref()
            .map(|probe| probe())
            .unwrap_or(FpmOwnership::Unowned)
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
    pub fn register(&mut self, service: ManagedService) {
        self.services.insert(service.kind, service);
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
            if let Some(svc) = self.services.get_mut(&kind) {
                if let Err(e) = svc.start() {
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
        let svc = self
            .services
            .get_mut(&kind)
            .ok_or_else(|| anyhow::anyhow!("service {kind} is not registered"))?;
        svc.circuit_breaker.reset();
        svc.start()
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
    pub fn reconfigure_service(
        &mut self,
        kind: ServiceKind,
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    ) {
        if let Some(svc) = self.services.get_mut(&kind) {
            svc.command = command;
            svc.args = args;
            svc.env = env;
            svc.circuit_breaker.reset();
        }
    }

    /// Deregister a service (e.g., a launch-blocked FPM after a version
    /// switch). Returns true when a registration was removed. Callers stop
    /// the service first.
    pub fn remove_service(&mut self, kind: ServiceKind) -> bool {
        self.services.remove(&kind).is_some()
    }

    /// Read access to one registered service (command/args/env inspection).
    pub fn service(&self, kind: ServiceKind) -> Option<&ManagedService> {
        self.services.get(&kind)
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
                            svc.started_at = None;
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
                        info!(service = %kind, "attempting restart");
                        if let Err(e) = svc.start() {
                            error!(service = %kind, error = %e, "restart failed");
                            svc.state = ServiceState::Failed {
                                reason: e.to_string(),
                            };
                        }
                    }

                    crashed.push(kind);
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
        self.services
            .iter()
            .map(|(k, v)| (*k, &v.state))
            .collect()
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
        sup.register(svc);
        sup
    }

    #[test]
    fn start_service_errors_on_unregistered() {
        let mut sup = ServiceSupervisor::new();
        let result = sup.start_service(ServiceKind::DumpServer);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not registered")
        );
    }

    #[test]
    fn stop_service_errors_on_unregistered() {
        let mut sup = ServiceSupervisor::new();
        let result = sup.stop_service(ServiceKind::Redis);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not registered")
        );
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
        );
        let svc = sup.services.get(&ServiceKind::Nginx).unwrap();
        assert_eq!(svc.command, "/usr/sbin/nginx-new");
        assert_eq!(svc.args, vec!["-g", "daemon off;"]);
        assert_eq!(svc.env, vec![("A".to_string(), "1".to_string())]);
    }

    #[test]
    fn reconfigure_replaces_env_never_retains_old() {
        let mut sup = ServiceSupervisor::new();
        sup.register(
            ManagedService::new(ServiceKind::PhpFpm, "php-fpm".to_string(), vec![])
                .with_env(vec![(
                    "PHP_INI_SCAN_DIR".to_string(),
                    ":/old/8.3/conf.d".to_string(),
                )]),
        );
        sup.reconfigure_service(
            ServiceKind::PhpFpm,
            "/new/php-fpm".to_string(),
            vec![],
            vec![(
                "PHP_INI_SCAN_DIR".to_string(),
                ":/new/8.4/conf.d".to_string(),
            )],
        );
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
        );
        assert!(sup.services.is_empty());
    }

    #[test]
    fn status_returns_registered_services() {
        let sup = make_supervisor_with_service();
        let status = sup.status();
        assert_eq!(status.len(), 1);
        assert!(status.contains_key(&ServiceKind::Nginx));
        assert!(matches!(
            status[&ServiceKind::Nginx],
            ServiceState::Stopped
        ));
    }

    #[test]
    fn register_replaces_existing_service() {
        let mut sup = ServiceSupervisor::new();
        let svc1 = ManagedService::new(ServiceKind::Nginx, "nginx-old".to_string(), vec![]);
        sup.register(svc1);
        let svc2 = ManagedService::new(ServiceKind::Nginx, "nginx-new".to_string(), vec![]);
        sup.register(svc2);
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
        svc.start().unwrap();
        // Allow the child to write the file
        std::thread::sleep(std::time::Duration::from_millis(200));
        let _ = svc.stop();

        let contents = std::fs::read_to_string(&marker)
            .expect("child should have written pwd-output in cwd");
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
        svc.start().unwrap();
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
        svc.start().unwrap();
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
        sup.register(sleeper(ServiceKind::PhpFpm));
        sup.register(sleeper(ServiceKind::Mailpit));
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
        sup.register(sleeper(ServiceKind::PhpFpm));
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
        svc.start().unwrap();
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
        sup.register(sleeper(ServiceKind::PhpFpm));
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
        sup.register(sleeper(ServiceKind::PhpFpm));
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
        sup.register(sleeper(ServiceKind::PhpFpm));
        sup.register(sleeper(ServiceKind::Mailpit));
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

    /// C4-2: unknown ownership fails CLOSED on every supervisor path — no
    /// start, no restart, no relinquish, no replacement.
    #[test]
    fn unknown_ownership_fails_closed_everywhere() {
        let probe: ExternalFpmOwnership = std::sync::Arc::new(|| {
            FpmOwnership::Unknown("ownership probe (pgrep) failed with status 2".to_string())
        });
        let mut sup = ServiceSupervisor::new();
        sup.set_fpm_ownership_probe(std::sync::Arc::clone(&probe));
        sup.register(sleeper(ServiceKind::PhpFpm));
        sup.register(sleeper(ServiceKind::Mailpit));

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
        owned_sup.register(sleeper(ServiceKind::PhpFpm));
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
        ));
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
        ));
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
}
