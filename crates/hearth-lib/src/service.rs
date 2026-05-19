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

    /// True for the three DB engines supervised by `hearth db`.
    pub fn is_db(&self) -> bool {
        matches!(self, Self::Mysql | Self::Postgresql | Self::Redis)
    }

    /// All DB engine kinds, in canonical order (matches `hearth db status` rows).
    pub fn db_engines() -> [ServiceKind; 3] {
        [Self::Mysql, Self::Postgresql, Self::Redis]
    }
}

impl std::fmt::Display for ServiceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

impl std::str::FromStr for ServiceKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "nginx" => Ok(Self::Nginx),
            "php-fpm" | "phpfpm" | "php" => Ok(Self::PhpFpm),
            "dnsmasq" | "dns" => Ok(Self::Dnsmasq),
            "mysql" => Ok(Self::Mysql),
            "redis" => Ok(Self::Redis),
            "postgresql" | "postgres" | "pg" => Ok(Self::Postgresql),
            "mailpit" | "mail" => Ok(Self::Mailpit),
            "dump-server" | "dump" => Ok(Self::DumpServer),
            _ => Err(format!("unknown service: {s}")),
        }
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
    fn service_kind_from_str_valid() {
        assert_eq!("nginx".parse::<ServiceKind>().unwrap(), ServiceKind::Nginx);
        assert_eq!("php-fpm".parse::<ServiceKind>().unwrap(), ServiceKind::PhpFpm);
        assert_eq!("php".parse::<ServiceKind>().unwrap(), ServiceKind::PhpFpm);
        assert_eq!("dnsmasq".parse::<ServiceKind>().unwrap(), ServiceKind::Dnsmasq);
        assert_eq!("dns".parse::<ServiceKind>().unwrap(), ServiceKind::Dnsmasq);
        assert_eq!("mysql".parse::<ServiceKind>().unwrap(), ServiceKind::Mysql);
        assert_eq!("redis".parse::<ServiceKind>().unwrap(), ServiceKind::Redis);
        assert_eq!("postgresql".parse::<ServiceKind>().unwrap(), ServiceKind::Postgresql);
        assert_eq!("postgres".parse::<ServiceKind>().unwrap(), ServiceKind::Postgresql);
        assert_eq!("pg".parse::<ServiceKind>().unwrap(), ServiceKind::Postgresql);
        assert_eq!("mailpit".parse::<ServiceKind>().unwrap(), ServiceKind::Mailpit);
        assert_eq!("mail".parse::<ServiceKind>().unwrap(), ServiceKind::Mailpit);
        assert_eq!("dump-server".parse::<ServiceKind>().unwrap(), ServiceKind::DumpServer);
        assert_eq!("dump".parse::<ServiceKind>().unwrap(), ServiceKind::DumpServer);
    }

    #[test]
    fn service_kind_from_str_case_insensitive() {
        assert_eq!("NGINX".parse::<ServiceKind>().unwrap(), ServiceKind::Nginx);
        assert_eq!("Nginx".parse::<ServiceKind>().unwrap(), ServiceKind::Nginx);
    }

    #[test]
    fn service_kind_from_str_unknown() {
        assert!("foobar".parse::<ServiceKind>().is_err());
    }

    #[test]
    fn service_kind_display_round_trips() {
        let kinds = [
            ServiceKind::Nginx,
            ServiceKind::PhpFpm,
            ServiceKind::Dnsmasq,
            ServiceKind::Mysql,
            ServiceKind::Redis,
            ServiceKind::Postgresql,
            ServiceKind::Mailpit,
            ServiceKind::DumpServer,
        ];
        for kind in kinds {
            let name = kind.to_string();
            let parsed: ServiceKind = name.parse().unwrap();
            assert_eq!(parsed, kind);
        }
    }

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
