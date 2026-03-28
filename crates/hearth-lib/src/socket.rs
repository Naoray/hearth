use serde::{Deserialize, Serialize};

use crate::add::AddAnswers;

/// Messages sent from CLI/GUI clients to the daemon over Unix socket.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DaemonRequest {
    /// Start all services
    Start,
    /// Stop all services
    Stop,
    /// Restart all services (or a specific one)
    Restart { service: Option<String> },
    /// Get status of all services
    Status,
    /// Link a site (delegates to Valet)
    Link { path: String, name: Option<String> },
    /// Unlink a site
    Unlink { name: String },
    /// Park a directory
    Park { path: String },
    /// Switch PHP version
    PhpSwitch { version: String },
    /// List installed PHP versions
    PhpList,
    /// Set a PHP config value
    PhpConfig { version: String, key: String, value: String },
    /// Secure a site with SSL
    Secure { name: String },
    /// Unsecure a site
    Unsecure { name: String },
    /// List all sites
    Sites,
    /// Ping (health check)
    Ping,
    /// Start a DB engine (mysql, postgres, redis) or all DB engines when `engine` is None.
    DbStart { engine: Option<String> },
    /// Stop a DB engine, or all DB engines when `engine` is None.
    DbStop { engine: Option<String> },
    /// Report status of all DB engines (registered + not registered).
    DbStatus,
    /// Install a Laravel package via a recipe (horizon | telescope | pulse | reverb).
    /// CLI collected all prompt answers up-front; daemon never opens a TTY.
    /// `cwd` is the CLI's working directory at invocation time — daemon uses it for
    /// cwd-walk site auto-detect when `site_path` is empty.
    Add {
        package: String,
        site_path: String,
        cwd: String,
        answers: AddAnswers,
        no_supervise: bool,
        dry_run: bool,
    },
}

/// Responses from the daemon to clients.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DaemonResponse {
    /// Success with optional message
    Ok { message: Option<String> },
    /// Error with description
    Error { message: String },
    /// Service status report
    Status { services: Vec<ServiceStatus> },
    /// List of sites
    Sites { sites: Vec<SiteInfo> },
    /// List of PHP versions
    PhpVersions { versions: Vec<PhpVersionInfo> },
    /// DB engine status report (one row per known engine).
    DbStatus { engines: Vec<DbEngineStatus> },
    /// Typed port-collision response from a Db dispatch handler.
    Conflict {
        engine: String,
        port: u16,
        owner_hint: Option<String>,
    },
    /// Pong (health check response)
    Pong,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub name: String,
    pub state: String,
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteInfo {
    pub name: String,
    pub path: String,
    pub secured: bool,
    pub php_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhpVersionInfo {
    pub version: String,
    pub path: String,
    pub active: bool,
}

/// One row in a `DbStatus` response.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DbEngineStatus {
    /// Canonical engine name: `mysql`, `postgresql`, `redis`.
    pub engine: String,
    /// One of: `running`, `stopped`, `starting`, `failed:<reason>`, `not_registered`.
    pub state: String,
    /// PID iff state is `running`.
    pub pid: Option<u32>,
    /// Configured port for this engine.
    pub port: u16,
    /// Absolute path to this engine's data directory.
    pub data_dir: String,
    /// True when the port is bound by a process Hearth is *not* supervising
    /// (e.g. Herd Pro, Homebrew services, ad-hoc engine).
    pub conflict_port: bool,
    /// Optional hint about who owns the conflicting port (e.g. a launchd label).
    pub owner_hint: Option<String>,
}

/// Socket path for daemon communication.
pub fn socket_path() -> std::path::PathBuf {
    crate::config_dir().join("hearth.sock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_serde_round_trip() {
        let cases: Vec<DaemonRequest> = vec![
            DaemonRequest::Start,
            DaemonRequest::Stop,
            DaemonRequest::Ping,
            DaemonRequest::Status,
            DaemonRequest::Sites,
            DaemonRequest::PhpList,
            DaemonRequest::Restart { service: None },
            DaemonRequest::Restart { service: Some("nginx".to_string()) },
            DaemonRequest::Link { path: "/tmp/site".to_string(), name: Some("mysite".to_string()) },
            DaemonRequest::Unlink { name: "mysite".to_string() },
            DaemonRequest::Park { path: "/home/sites".to_string() },
            DaemonRequest::PhpSwitch { version: "8.3".to_string() },
            DaemonRequest::PhpConfig { version: "active".to_string(), key: "memory_limit".to_string(), value: "512M".to_string() },
            DaemonRequest::Secure { name: "mysite".to_string() },
            DaemonRequest::Unsecure { name: "mysite".to_string() },
            DaemonRequest::DbStart { engine: None },
            DaemonRequest::DbStart { engine: Some("postgres".to_string()) },
            DaemonRequest::DbStop { engine: None },
            DaemonRequest::DbStop { engine: Some("redis".to_string()) },
            DaemonRequest::DbStatus,
            DaemonRequest::Add {
                package: "telescope".to_string(),
                site_path: "/Users/me/Sites/blog".to_string(),
                cwd: "/Users/me/Sites/blog/app".to_string(),
                answers: AddAnswers {
                    telescope_enable_in_prod: Some(false),
                    ..Default::default()
                },
                no_supervise: false,
                dry_run: true,
            },
        ];

        for request in cases {
            let json = serde_json::to_string(&request).unwrap();
            let deserialized: DaemonRequest = serde_json::from_str(&json).unwrap();
            // Verify round-trip produces valid JSON
            let json2 = serde_json::to_string(&deserialized).unwrap();
            assert_eq!(json, json2);
        }
    }

    #[test]
    fn response_serde_round_trip() {
        let cases: Vec<DaemonResponse> = vec![
            DaemonResponse::Pong,
            DaemonResponse::Ok { message: None },
            DaemonResponse::Ok { message: Some("done".to_string()) },
            DaemonResponse::Error { message: "failed".to_string() },
            DaemonResponse::Status {
                services: vec![ServiceStatus {
                    name: "nginx".to_string(),
                    state: "Running".to_string(),
                    pid: Some(1234),
                }],
            },
            DaemonResponse::Sites {
                sites: vec![SiteInfo {
                    name: "mysite".to_string(),
                    path: "/tmp/site".to_string(),
                    secured: true,
                    php_version: Some("8.4".to_string()),
                }],
            },
            DaemonResponse::PhpVersions {
                versions: vec![PhpVersionInfo {
                    version: "8.4".to_string(),
                    path: "/usr/bin/php".to_string(),
                    active: true,
                }],
            },
            DaemonResponse::DbStatus {
                engines: vec![DbEngineStatus {
                    engine: "postgresql".to_string(),
                    state: "running".to_string(),
                    pid: Some(4242),
                    port: 5432,
                    data_dir: "/Users/x/.config/hearth/data/postgresql".to_string(),
                    conflict_port: false,
                    owner_hint: None,
                }],
            },
            DaemonResponse::Conflict {
                engine: "mysql".to_string(),
                port: 3306,
                owner_hint: Some("homebrew.mxcl.mysql".to_string()),
            },
        ];

        for response in cases {
            let json = serde_json::to_string(&response).unwrap();
            let deserialized: DaemonResponse = serde_json::from_str(&json).unwrap();
            let json2 = serde_json::to_string(&deserialized).unwrap();
            assert_eq!(json, json2);
        }
    }
}
