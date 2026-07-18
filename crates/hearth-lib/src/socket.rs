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
    /// Set a PHP config value (legacy, retained for one release).
    ///
    /// Old CLIs keep working against a new daemon: the daemon maps this to
    /// [`PhpConfigAction::Set`] with `Version`/`Active` scope and answers with
    /// a legacy `Ok`/`Error` response. Removal: earliest release after next.
    PhpConfig { version: String, key: String, value: String },
    /// Typed php-config action (V2 protocol window).
    PhpConfigV2 { action: PhpConfigAction },
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
    /// Typed php-config report (V2 protocol window).
    PhpConfigReport(PhpConfigOutcome),
    /// Pong (health check response)
    Pong,
}

/// Typed php-config action carried by [`DaemonRequest::PhpConfigV2`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action")]
pub enum PhpConfigAction {
    /// Persist a directive into the canonical store, then reconcile channels.
    Set { scope: PhpScope, key: String, value: String },
    /// Remove a directive from the canonical store, then reconcile channels.
    Unset { scope: PhpScope, key: String },
    /// Report configured (and, where materialized, observed) values. Read-only.
    Show { key: Option<String> },
    /// Per-target coverage table. Read-only; only reports `sync pending`.
    Status,
    /// Force journal recovery + legacy migration + reconcile.
    Sync,
    /// Remove every Hearth-written channel file (manifest-tracked, exact-hash).
    Unmanage,
}

/// Which slice of the canonical store a Set/Unset targets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope")]
pub enum PhpScope {
    /// `[php_ini.global]` — applies to every version.
    Global,
    /// `[php_ini.overrides."X.Y"]` — one version's override table.
    Version { version: String },
    /// The daemon's current `default_php` version.
    Active,
}

/// One row in a php-config report: a discovered target in one launch context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhpTargetRow {
    /// `hearth` | `herd` | `homebrew`
    pub provider: String,
    /// `8.4`
    pub version: String,
    /// `cli` | `fpm`
    pub sapi: String,
    /// `normal` | `sanitized` | `launched`
    pub context: String,
    /// Scan-dir channel path for this context, when probed.
    pub channel: Option<String>,
    /// Truthful coverage tier: classification joined with the channel file's
    /// last outcome. Never claims universal coverage.
    pub coverage: String,
    /// Configured value from the canonical store (Show/Set/Unset with a key).
    pub configured: Option<String>,
    /// Observed value. In this release only `materialized` observation exists;
    /// launch-probed and live-observed values arrive with todos #2321-C/#2343.
    pub observed: Option<String>,
    /// Observation vocabulary state: `configured` | `materialized` |
    /// `launch-probed` | `live-observed` | `n/a`.
    pub observed_state: String,
}

/// Typed per-file reconcile result (wire form of the library's WriteOutcome).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result")]
pub enum FileWriteResult {
    Written,
    Unchanged,
    Deleted,
    Refused { reason: String },
    Failed { error: String },
}

/// One channel file touched (or refused) by a reconcile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileOutcome {
    pub path: String,
    pub result: FileWriteResult,
}

/// What happened to the supervised php-fpm service after a config mutation.
/// `NotRegistered`/`LaunchBlocked` are informational (exit 0); `Failed` on a
/// registered service is an error (nonzero exit).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome")]
pub enum FpmRestartOutcome {
    Restarted,
    NotRegistered { herd_hint: bool },
    LaunchBlocked { reason: String },
    Failed { message: String },
    NotAttempted,
}

/// Full report for a V2 php-config action. `persisted` is `Some` only for
/// Set/Unset — the persisted/restart split keeps `hearth php config` from
/// reporting failure merely because an optional FPM service could not restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhpConfigOutcome {
    pub persisted: Option<bool>,
    pub rows: Vec<PhpTargetRow>,
    pub files: Vec<FileOutcome>,
    pub fpm: FpmRestartOutcome,
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

    fn all_scopes() -> Vec<PhpScope> {
        vec![
            PhpScope::Global,
            PhpScope::Version { version: "8.4".to_string() },
            PhpScope::Active,
        ]
    }

    #[test]
    fn php_config_action_serde_round_trip_every_variant() {
        let mut cases: Vec<PhpConfigAction> = Vec::new();
        for scope in all_scopes() {
            cases.push(PhpConfigAction::Set {
                scope: scope.clone(),
                key: "memory_limit".to_string(),
                value: "1G".to_string(),
            });
            cases.push(PhpConfigAction::Unset {
                scope,
                key: "opcache.enable".to_string(),
            });
        }
        cases.push(PhpConfigAction::Show { key: None });
        cases.push(PhpConfigAction::Show { key: Some("memory_limit".to_string()) });
        cases.push(PhpConfigAction::Status);
        cases.push(PhpConfigAction::Sync);
        cases.push(PhpConfigAction::Unmanage);

        for action in cases {
            let json = serde_json::to_string(&action).unwrap();
            let back: PhpConfigAction = serde_json::from_str(&json).unwrap();
            assert_eq!(action, back, "round-trip mismatch for {json}");
        }
    }

    #[test]
    fn php_config_v2_request_round_trips_inside_daemon_request() {
        let request = DaemonRequest::PhpConfigV2 {
            action: PhpConfigAction::Set {
                scope: PhpScope::Global,
                key: "memory_limit".to_string(),
                value: "1G".to_string(),
            },
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"type\":\"PhpConfigV2\""), "got: {json}");
        let back: DaemonRequest = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        assert_eq!(json, json2);
    }

    fn all_fpm_outcomes() -> Vec<FpmRestartOutcome> {
        vec![
            FpmRestartOutcome::Restarted,
            FpmRestartOutcome::NotRegistered { herd_hint: true },
            FpmRestartOutcome::NotRegistered { herd_hint: false },
            FpmRestartOutcome::LaunchBlocked {
                reason: "missing fpm config — see todo #2343".to_string(),
            },
            FpmRestartOutcome::Failed { message: "start failed".to_string() },
            FpmRestartOutcome::NotAttempted,
        ]
    }

    #[test]
    fn fpm_restart_outcome_serde_round_trip_every_variant() {
        for outcome in all_fpm_outcomes() {
            let json = serde_json::to_string(&outcome).unwrap();
            let back: FpmRestartOutcome = serde_json::from_str(&json).unwrap();
            assert_eq!(outcome, back, "round-trip mismatch for {json}");
        }
    }

    #[test]
    fn file_write_result_serde_round_trip_every_variant() {
        let cases = vec![
            FileWriteResult::Written,
            FileWriteResult::Unchanged,
            FileWriteResult::Deleted,
            FileWriteResult::Refused { reason: "untracked file".to_string() },
            FileWriteResult::Failed { error: "permission denied".to_string() },
        ];
        for result in cases {
            let json = serde_json::to_string(&result).unwrap();
            let back: FileWriteResult = serde_json::from_str(&json).unwrap();
            assert_eq!(result, back, "round-trip mismatch for {json}");
        }
    }

    #[test]
    fn php_config_report_response_round_trips_with_full_outcome() {
        for fpm in all_fpm_outcomes() {
            let outcome = PhpConfigOutcome {
                persisted: Some(true),
                rows: vec![PhpTargetRow {
                    provider: "homebrew".to_string(),
                    version: "8.5".to_string(),
                    sapi: "cli".to_string(),
                    context: "normal".to_string(),
                    channel: Some("/opt/homebrew/etc/php/8.5/conf.d".to_string()),
                    coverage: "managed (channel+env)".to_string(),
                    configured: Some("1G".to_string()),
                    observed: Some("1G".to_string()),
                    observed_state: "materialized".to_string(),
                }],
                files: vec![FileOutcome {
                    path: "/opt/homebrew/etc/php/8.5/conf.d/zz-hearth.ini".to_string(),
                    result: FileWriteResult::Written,
                }],
                fpm,
            };
            let response = DaemonResponse::PhpConfigReport(outcome.clone());
            let json = serde_json::to_string(&response).unwrap();
            assert!(json.contains("\"type\":\"PhpConfigReport\""), "got: {json}");
            let back: DaemonResponse = serde_json::from_str(&json).unwrap();
            match back {
                DaemonResponse::PhpConfigReport(round) => assert_eq!(outcome, round),
                other => panic!("expected PhpConfigReport, got {other:?}"),
            }
        }
    }

    #[test]
    fn php_config_outcome_none_persisted_round_trips() {
        let outcome = PhpConfigOutcome {
            persisted: None,
            rows: vec![],
            files: vec![],
            fpm: FpmRestartOutcome::NotAttempted,
        };
        let json = serde_json::to_string(&outcome).unwrap();
        let back: PhpConfigOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(outcome, back);
    }
}
