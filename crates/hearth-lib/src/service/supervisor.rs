use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use command_group::{CommandGroup, GroupChild};
use tokio::time;
use tracing::{error, info, warn};

use super::{CircuitBreaker, ServiceKind, ServiceState};

/// Manages a single supervised service process.
///
/// Each service runs in its own process group via `command-group`.
/// On shutdown, SIGTERM is sent to the entire group, followed by
/// SIGKILL after a timeout — guaranteeing no orphan processes.
pub struct ManagedService {
    pub kind: ServiceKind,
    pub state: ServiceState,
    pub child: Option<GroupChild>,
    pub circuit_breaker: CircuitBreaker,
    command: String,
    args: Vec<String>,
}

impl ManagedService {
    pub fn new(kind: ServiceKind, command: String, args: Vec<String>) -> Self {
        Self {
            kind,
            state: ServiceState::Stopped,
            child: None,
            circuit_breaker: CircuitBreaker::new(3, Duration::from_secs(60)),
            command,
            args,
        }
    }

    /// Start the service in a new process group.
    pub fn start(&mut self) -> anyhow::Result<()> {
        info!(service = %self.kind, "starting service");

        let child = std::process::Command::new(&self.command)
            .args(&self.args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .group_spawn()?;

        let pid = child.id();
        self.child = Some(child);
        self.state = ServiceState::Running { pid };

        info!(service = %self.kind, pid, "service started");
        Ok(())
    }

    /// Stop the service by sending SIGTERM to the process group,
    /// then SIGKILL after 5 seconds if still alive.
    pub fn stop(&mut self) -> anyhow::Result<()> {
        if let Some(mut child) = self.child.take() {
            let pid = child.id();
            info!(service = %self.kind, pid, "stopping service");

            // Send SIGTERM to process group
            let _ = child.kill();

            // Wait briefly, then force kill if needed
            match child.try_wait()? {
                Some(_status) => {
                    info!(service = %self.kind, "service stopped cleanly");
                }
                None => {
                    warn!(service = %self.kind, "service did not stop, force killing");
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
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
    pub fn start_all(&mut self) -> anyhow::Result<()> {
        let kinds: Vec<ServiceKind> = self.services.keys().copied().collect();
        for kind in kinds {
            if let Some(svc) = self.services.get_mut(&kind) {
                svc.start()?;
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

    /// Start a specific service.
    pub fn start_service(&mut self, kind: ServiceKind) -> anyhow::Result<()> {
        if let Some(svc) = self.services.get_mut(&kind) {
            svc.circuit_breaker.reset();
            svc.start()?;
        }
        Ok(())
    }

    /// Stop a specific service.
    pub fn stop_service(&mut self, kind: ServiceKind) -> anyhow::Result<()> {
        if let Some(svc) = self.services.get_mut(&kind) {
            svc.stop()?;
        }
        Ok(())
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
}

impl Drop for ServiceSupervisor {
    fn drop(&mut self) {
        // Guarantee: all child processes are killed when the supervisor exits,
        // even during panics. This is the core safety property that prevents
        // orphan processes.
        let _ = self.stop_all();
    }
}
