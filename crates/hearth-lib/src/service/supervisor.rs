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
        }
    }

    /// Builder: set the shutdown strategy for this service.
    pub fn with_shutdown_strategy(mut self, strategy: ShutdownStrategy) -> Self {
        self.shutdown_strategy = strategy;
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
        let child = cmd.group_spawn()?;

        let pid = child.id();
        self.child = Some(child);
        self.state = ServiceState::Running { pid };

        info!(service = %self.kind, pid, "service started");
        Ok(())
    }

    /// Stop the service. Strategy:
    /// - `Postgres` → `pg_ctl stop -m fast` first, then signals on fallback.
    /// - All others → SIGTERM to the process group, poll until grace expires,
    ///   then SIGKILL.
    pub fn stop(&mut self) -> anyhow::Result<()> {
        let Some(mut child) = self.child.take() else {
            self.state = ServiceState::Stopped;
            return Ok(());
        };

        let pid = child.id();
        info!(service = %self.kind, pid, "stopping service");

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
                // ESRCH ("no such process") is fine — child already exited.
                if e != nix::errno::Errno::ESRCH {
                    warn!(service = %self.kind, error = %e, "SIGTERM to group failed");
                }
            }
        }

        let grace = self.shutdown_strategy.grace();
        let deadline = Instant::now() + grace;
        loop {
            match child.try_wait()? {
                Some(_status) => {
                    info!(service = %self.kind, "service stopped cleanly");
                    self.state = ServiceState::Stopped;
                    return Ok(());
                }
                None => {
                    if Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }

        warn!(
            service = %self.kind,
            grace_ms = grace.as_millis() as u64,
            "service did not stop within grace window — sending SIGKILL"
        );
        let _ = child.kill();
        let _ = child.wait();
        self.state = ServiceState::Stopped;
        Ok(())
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

    /// Stop all services, killing entire process groups.
    pub fn stop_all(&mut self) -> anyhow::Result<()> {
        for (_, svc) in self.services.iter_mut() {
            let _ = svc.stop();
        }
        Ok(())
    }

    /// Start a specific service. Returns an error if the service is not registered.
    pub fn start_service(&mut self, kind: ServiceKind) -> anyhow::Result<()> {
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

    /// Replace a service's command configuration (e.g., for PHP version switch).
    /// The service must be stopped before reconfiguring.
    pub fn reconfigure_service(&mut self, kind: ServiceKind, command: String, args: Vec<String>) {
        if let Some(svc) = self.services.get_mut(&kind) {
            svc.command = command;
            svc.args = args;
            svc.circuit_breaker.reset();
        }
    }

    /// Run one health check pass. Returns services that crashed.
    pub fn health_check(&mut self) -> Vec<ServiceKind> {
        let mut crashed = Vec::new();

        let kinds: Vec<ServiceKind> = self.services.keys().copied().collect();
        for kind in kinds {
            if let Some(svc) = self.services.get_mut(&kind) {
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
        );
        let svc = sup.services.get(&ServiceKind::Nginx).unwrap();
        assert_eq!(svc.command, "/usr/sbin/nginx-new");
        assert_eq!(svc.args, vec!["-g", "daemon off;"]);
    }

    #[test]
    fn reconfigure_unregistered_service_is_noop() {
        let mut sup = ServiceSupervisor::new();
        // Should not panic
        sup.reconfigure_service(
            ServiceKind::Redis,
            "redis-server".to_string(),
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
}
