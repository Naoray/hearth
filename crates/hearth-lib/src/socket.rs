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
        ];

        for response in cases {
            let json = serde_json::to_string(&response).unwrap();
            let deserialized: DaemonResponse = serde_json::from_str(&json).unwrap();
            let json2 = serde_json::to_string(&deserialized).unwrap();
            assert_eq!(json, json2);
        }
    }
}
