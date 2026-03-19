use serde::{Deserialize, Serialize};

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
    /// Pong (health check response)
    Pong,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub name: String,
    pub state: String,
    pub pid: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SiteInfo {
    pub name: String,
    pub path: String,
    pub secured: bool,
    pub php_version: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PhpVersionInfo {
    pub version: String,
    pub path: String,
    pub active: bool,
}

/// Socket path for daemon communication.
pub fn socket_path() -> std::path::PathBuf {
    crate::config_dir().join("hearth.sock")
}
