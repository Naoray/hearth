pub mod manager;
pub mod supervisor;

use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

/// Services that Hearth can supervise
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceKind {
    Nginx,
    PhpFpm,
    Dnsmasq,
    Mysql,
    Redis,
    Postgresql,
    Mailpit,
    DumpServer,
}

impl ServiceKind {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Nginx => "nginx",
            Self::PhpFpm => "php-fpm",
            Self::Dnsmasq => "dnsmasq",
            Self::Mysql => "mysql",
            Self::Redis => "redis",
            Self::Postgresql => "postgresql",
            Self::Mailpit => "mailpit",
            Self::DumpServer => "dump-server",
        }
    }
}

impl std::fmt::Display for ServiceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// Current state of a managed service
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServiceState {
    Stopped,
    Starting,
    Running { pid: u32 },
    Failed { reason: String },
}

/// Circuit breaker state — tracks failures to prevent restart loops.
///
/// ```text
/// ┌─────────┐  crash   ┌──────────┐  3 fails/60s  ┌────────┐
/// │ Running ├─────────►│ Restarting├──────────────►│ Failed │
/// └─────────┘          └─────┬─────┘               └───┬────┘
///                            │ success                  │ manual
///                            ▼                          │ restart
///                       ┌─────────┐                     │
///                       │ Running │◄────────────────────┘
///                       └─────────┘
/// ```
#[derive(Debug)]
pub struct CircuitBreaker {
    /// Maximum failures before tripping
    max_failures: u32,
    /// Time window for counting failures
    window: Duration,
    /// Recent failure timestamps
    failures: Vec<Instant>,
}

impl CircuitBreaker {
    pub fn new(max_failures: u32, window: Duration) -> Self {
        Self {
            max_failures,
            window,
            failures: Vec::new(),
        }
    }

    /// Record a failure. Returns true if the circuit breaker has tripped.
    pub fn record_failure(&mut self) -> bool {
        let now = Instant::now();
        self.failures.retain(|t| now.duration_since(*t) < self.window);
        self.failures.push(now);
        self.failures.len() >= self.max_failures as usize
    }

    /// Reset the circuit breaker (e.g., after manual restart).
    pub fn reset(&mut self) {
        self.failures.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn circuit_breaker_trips_after_max_failures() {
        let mut cb = CircuitBreaker::new(3, Duration::from_secs(60));
        assert!(!cb.record_failure());
        assert!(!cb.record_failure());
        assert!(cb.record_failure()); // 3rd failure trips it
    }

    #[test]
    fn circuit_breaker_resets() {
        let mut cb = CircuitBreaker::new(3, Duration::from_secs(60));
        cb.record_failure();
        cb.record_failure();
        cb.reset();
        assert!(!cb.record_failure()); // reset clears history
    }
}
