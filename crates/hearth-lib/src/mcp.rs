use std::sync::Arc;

use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Implementation, ServerCapabilities, ServerInfo,
};
use rmcp::{ServerHandler, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::config::HearthConfig;
use crate::php::PhpManager;
use crate::service::supervisor::ServiceSupervisor;
use crate::service::{ServiceKind, ServiceState};
use crate::site::SiteManager;
use crate::valet::ValetCli;

// ---------------------------------------------------------------------------
// Parameter types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct PhpSwitchParams {
    /// The PHP version to switch to (e.g., "8.3")
    pub version: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SiteLinkParams {
    /// Absolute path to the project directory
    pub path: String,
    /// Optional site name (defaults to directory name)
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SiteUnlinkParams {
    /// Name of the site to unlink
    pub name: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ServiceRestartParams {
    /// Name of the service to restart (e.g., "nginx", "php-fpm"). Omit to restart all.
    #[serde(default)]
    pub service: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct PhpConfigParams {
    /// The php.ini key to set (e.g., "memory_limit")
    pub key: String,
    /// The value to set (e.g., "512M")
    pub value: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct DbActionParams {
    /// Engine name: `mysql`, `postgres`, `redis`. Omit to target all DB engines.
    #[serde(default)]
    pub engine: Option<String>,
}

// ---------------------------------------------------------------------------
// HearthMcpServer
// ---------------------------------------------------------------------------

/// MCP server exposing Hearth's development environment management tools.
///
/// Holds `Arc<Mutex<T>>` references to the underlying managers. The `Mutex`
/// is `tokio::sync::Mutex` because locks may be held across `.await` points.
///
/// Lock ordering convention (when multiple locks are needed):
///   `config` -> `php_manager` -> `site_manager` -> `supervisor`
#[derive(Clone)]
pub struct HearthMcpServer {
    pub supervisor: Arc<Mutex<ServiceSupervisor>>,
    pub site_manager: Arc<Mutex<SiteManager>>,
    pub php_manager: Arc<Mutex<PhpManager>>,
    pub config: Arc<Mutex<HearthConfig>>,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

/// Resolve a DB engine name (or absence) into the supervised kinds the MCP
/// tool should act on. None → all three DB engines; explicit non-DB kinds
/// (e.g. `nginx`) are rejected.
fn db_engine_targets(engine: Option<&str>) -> Result<Vec<ServiceKind>, String> {
    match engine {
        None => Ok(ServiceKind::db_engines().to_vec()),
        Some(name) => {
            let kind: ServiceKind = name.parse().map_err(|e: String| e)?;
            if !kind.is_db() {
                return Err(format!("{name} is not a DB engine"));
            }
            Ok(vec![kind])
        }
    }
}

impl HearthMcpServer {
    /// Create a new `HearthMcpServer` wired to the given managers.
    pub fn new(
        supervisor: Arc<Mutex<ServiceSupervisor>>,
        site_manager: Arc<Mutex<SiteManager>>,
        php_manager: Arc<Mutex<PhpManager>>,
        config: Arc<Mutex<HearthConfig>>,
    ) -> Self {
        let tool_router = Self::tool_router();
        Self {
            supervisor,
            site_manager,
            php_manager,
            config,
            tool_router,
        }
    }
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

#[tool_router]
impl HearthMcpServer {
    /// List all supervised services and their current state.
    #[tool(name = "hearth_status", description = "List all Hearth services and their current state (running, stopped, failed)")]
    async fn hearth_status(&self) -> Result<String, String> {
        let sup = self.supervisor.lock().await;
        let status = sup.status();
        if status.is_empty() {
            return Ok("No services registered.".to_string());
        }
        let mut lines: Vec<String> = status
            .iter()
            .map(|(kind, state)| {
                let state_str = match state {
                    ServiceState::Stopped => "stopped".to_string(),
                    ServiceState::Starting => "starting".to_string(),
                    ServiceState::Running { pid } => format!("running (pid {pid})"),
                    ServiceState::Failed { reason } => format!("failed: {reason}"),
                };
                format!("{kind}: {state_str}")
            })
            .collect();
        lines.sort();
        Ok(lines.join("\n"))
    }

    /// List all linked Valet sites.
    #[tool(name = "hearth_sites", description = "List all linked Valet sites with their paths, TLS status, and PHP version")]
    async fn hearth_sites(&self) -> Result<String, String> {
        let sm = self.site_manager.lock().await;
        match sm.list_sites() {
            Ok(sites) => {
                if sites.is_empty() {
                    Ok("No sites linked.".to_string())
                } else {
                    let lines: Vec<String> = sites
                        .iter()
                        .map(|s| {
                            let tls = if s.secured { " [TLS]" } else { "" };
                            let php = s
                                .php_version
                                .as_deref()
                                .map(|v| format!(" (PHP {v})"))
                                .unwrap_or_default();
                            format!("{}{tls}{php} -> {}", s.name, s.path.display())
                        })
                        .collect();
                    Ok(lines.join("\n"))
                }
            }
            Err(e) => Err(format!("Failed to list sites: {e}")),
        }
    }

    /// List installed PHP versions with their binary paths.
    #[tool(name = "hearth_php_list", description = "List installed PHP versions and their binary paths")]
    async fn hearth_php_list(&self) -> Result<String, String> {
        let pm = self.php_manager.lock().await;
        let versions = pm.installed_versions_with_paths();
        if versions.is_empty() {
            Ok("No PHP versions found.".to_string())
        } else {
            let lines: Vec<String> = versions
                .iter()
                .map(|(v, p)| format!("PHP {v}: {}", p.display()))
                .collect();
            Ok(lines.join("\n"))
        }
    }

    /// Switch the active PHP version (stops php-fpm, reconfigures, restarts).
    #[tool(name = "hearth_php_switch", description = "Switch the active PHP version for php-fpm")]
    async fn hearth_php_switch(
        &self,
        Parameters(params): Parameters<PhpSwitchParams>,
    ) -> Result<String, String> {
        let version = &params.version;
        let config_dir = crate::config_dir();

        // Resolve the php-fpm binary using the same resolver as the daemon
        let fpm_binary =
            crate::php::resolver::resolve_phpfpm_binary(version, &config_dir).ok_or_else(|| {
                format!("PHP {version} php-fpm binary not found")
            })?;

        // Lock ordering: config first, then supervisor
        {
            let mut cfg = self.config.lock().await;
            cfg.default_php = version.clone();
            cfg.save()
                .map_err(|e| format!("Failed to save config: {e}"))?;
        }

        // Stop php-fpm, reconfigure with full args, restart
        let mut sup = self.supervisor.lock().await;
        let _ = sup.stop_service(ServiceKind::PhpFpm);
        sup.reconfigure_service(
            ServiceKind::PhpFpm,
            fpm_binary.to_string_lossy().to_string(),
            vec![
                "--nodaemonize".to_string(),
                format!(
                    "--fpm-config={}",
                    config_dir.join("fpm/php-fpm.conf").display()
                ),
            ],
        );
        sup.start_service(ServiceKind::PhpFpm)
            .map_err(|e| format!("Failed to start php-fpm {version}: {e}"))?;

        Ok(format!("Switched to PHP {version}"))
    }

    /// Link a directory as a Valet site.
    #[tool(name = "hearth_site_link", description = "Link a directory as a Valet site")]
    async fn hearth_site_link(
        &self,
        Parameters(params): Parameters<SiteLinkParams>,
    ) -> Result<String, String> {
        // Run valet link in a blocking task since it calls an external process.
        // Uses link_in() with Command::current_dir() instead of set_current_dir()
        // to avoid mutating global process state (safe for concurrent MCP requests).
        let name = params.name.clone();
        let path = params.path.clone();
        let result = tokio::task::spawn_blocking(move || {
            ValetCli::link_in(&path, name.as_deref())
        })
        .await
        .map_err(|e| format!("Task join error: {e}"))?;

        match result {
            Ok(output) => Ok(format!("Site linked: {output}")),
            Err(e) => Err(format!("Failed to link site: {e}")),
        }
    }

    /// Unlink a Valet site.
    #[tool(name = "hearth_site_unlink", description = "Unlink a Valet site by name")]
    async fn hearth_site_unlink(
        &self,
        Parameters(params): Parameters<SiteUnlinkParams>,
    ) -> Result<String, String> {
        let name = params.name.clone();
        let result = tokio::task::spawn_blocking(move || ValetCli::unlink(&name))
            .await
            .map_err(|e| format!("Task join error: {e}"))?;

        match result {
            Ok(()) => Ok(format!("Site '{}' unlinked", params.name)),
            Err(e) => Err(format!("Failed to unlink site '{}': {e}", params.name)),
        }
    }

    /// Restart one or all services.
    #[tool(name = "hearth_service_restart", description = "Restart a specific service or all services")]
    async fn hearth_service_restart(
        &self,
        Parameters(params): Parameters<ServiceRestartParams>,
    ) -> Result<String, String> {
        let mut sup = self.supervisor.lock().await;

        if let Some(svc_name) = &params.service {
            let kind: ServiceKind = svc_name
                .parse()
                .map_err(|e: String| e)?;
            sup.stop_service(kind)
                .map_err(|e| format!("Failed to stop {svc_name}: {e}"))?;
            sup.start_service(kind)
                .map_err(|e| format!("Failed to start {svc_name}: {e}"))?;
            Ok(format!("Service '{svc_name}' restarted"))
        } else {
            sup.stop_all()
                .map_err(|e| format!("Failed to stop services: {e}"))?;
            sup.start_all()
                .map_err(|e| format!("Failed to start services: {e}"))?;
            Ok("All services restarted".to_string())
        }
    }

    /// Start a DB engine (mysql, postgres, redis) or all DB engines.
    #[tool(name = "hearth_db_start", description = "Start a database engine (mysql, postgres, redis) or all DB engines")]
    async fn hearth_db_start(
        &self,
        Parameters(params): Parameters<DbActionParams>,
    ) -> Result<String, String> {
        let kinds = db_engine_targets(params.engine.as_deref())?;
        let mut sup = self.supervisor.lock().await;
        let mut summary: Vec<String> = Vec::new();
        for kind in kinds {
            if !sup.status().contains_key(&kind) {
                summary.push(format!("{kind}: not registered"));
                continue;
            }
            if matches!(sup.status().get(&kind), Some(ServiceState::Running { .. })) {
                summary.push(format!("{kind}: already running"));
                continue;
            }
            match sup.start_service(kind) {
                Ok(()) => summary.push(format!("{kind}: started")),
                Err(e) => summary.push(format!("{kind}: start failed: {e}")),
            }
        }
        Ok(summary.join("\n"))
    }

    /// Stop a DB engine or all DB engines.
    #[tool(name = "hearth_db_stop", description = "Stop a database engine or all DB engines")]
    async fn hearth_db_stop(
        &self,
        Parameters(params): Parameters<DbActionParams>,
    ) -> Result<String, String> {
        let kinds = db_engine_targets(params.engine.as_deref())?;
        let mut sup = self.supervisor.lock().await;
        let mut summary: Vec<String> = Vec::new();
        for kind in kinds {
            if !sup.status().contains_key(&kind) {
                summary.push(format!("{kind}: not registered"));
                continue;
            }
            match sup.stop_service(kind) {
                Ok(()) => summary.push(format!("{kind}: stopped")),
                Err(e) => summary.push(format!("{kind}: stop failed: {e}")),
            }
        }
        Ok(summary.join("\n"))
    }

    /// Report status for the three DB engines.
    #[tool(name = "hearth_db_status", description = "Report state, pid, port, and data dir for each registered DB engine")]
    async fn hearth_db_status(&self) -> Result<String, String> {
        let cfg = self.config.lock().await;
        let mysql_port = cfg.mysql_port;
        let postgres_port = cfg.postgres_port;
        let redis_port = cfg.redis_port;
        drop(cfg);

        let sup = self.supervisor.lock().await;
        let mut lines: Vec<String> = Vec::new();
        for kind in ServiceKind::db_engines() {
            let port = match kind {
                ServiceKind::Mysql => mysql_port,
                ServiceKind::Postgresql => postgres_port,
                ServiceKind::Redis => redis_port,
                _ => 0,
            };
            let state_str = match sup.status().get(&kind) {
                Some(ServiceState::Running { pid }) => format!("running (pid {pid})"),
                Some(ServiceState::Starting) => "starting".to_string(),
                Some(ServiceState::Stopped) => "stopped".to_string(),
                Some(ServiceState::Failed { reason }) => format!("failed: {reason}"),
                None => "not registered".to_string(),
            };
            lines.push(format!("{kind} (port {port}): {state_str}"));
        }
        Ok(lines.join("\n"))
    }

    /// Set a php.ini value for the active PHP version and restart php-fpm.
    #[tool(name = "hearth_php_config", description = "Set a php.ini configuration value for the active PHP version and restart php-fpm")]
    async fn hearth_php_config(
        &self,
        Parameters(params): Parameters<PhpConfigParams>,
    ) -> Result<String, String> {
        let version = {
            let cfg = self.config.lock().await;
            cfg.default_php.clone()
        };

        {
            let pm = self.php_manager.lock().await;
            pm.set_ini_value(&version, &params.key, &params.value)
                .map_err(|e| format!("Failed to set php.ini value: {e}"))?;
        }

        // Restart php-fpm to pick up the change
        let mut sup = self.supervisor.lock().await;
        let _ = sup.stop_service(ServiceKind::PhpFpm);
        sup.start_service(ServiceKind::PhpFpm)
            .map_err(|e| format!("Failed to restart php-fpm: {e}"))?;

        Ok(format!(
            "Set {}={} in PHP {} php.ini and restarted php-fpm",
            params.key, params.value, version
        ))
    }
}

// ---------------------------------------------------------------------------
// ServerHandler implementation
// ---------------------------------------------------------------------------

impl ServerHandler for HearthMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_server_info(Implementation::new(
            "hearth",
            env!("CARGO_PKG_VERSION"),
        ))
        .with_instructions(
            "Hearth is a unified Laravel development command center. \
             Use these tools to manage services, sites, and PHP versions.",
        )
    }

    fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> impl Future<Output = Result<rmcp::model::ListToolsResult, rmcp::ErrorData>> + Send + '_ {
        let tools = self.tool_router.list_all();
        std::future::ready(Ok(rmcp::model::ListToolsResult {
            tools,
            ..Default::default()
        }))
    }

    fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> impl Future<Output = Result<CallToolResult, rmcp::ErrorData>> + Send + '_ {
        let tool_call_context = rmcp::handler::server::tool::ToolCallContext::new(
            self, request, context,
        );
        self.tool_router.call(tool_call_context)
    }

    fn get_tool(&self, name: &str) -> Option<rmcp::model::Tool> {
        self.tool_router.get(name).cloned()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::supervisor::ManagedService;

    fn make_server() -> HearthMcpServer {
        let mut sup = ServiceSupervisor::new();
        sup.register(ManagedService::new(
            ServiceKind::Nginx,
            "true".to_string(),
            vec![],
        ));
        sup.register(ManagedService::new(
            ServiceKind::PhpFpm,
            "true".to_string(),
            vec![],
        ));

        let tmp_valet = tempfile::TempDir::new().expect("tempdir");
        let sm = SiteManager::new(tmp_valet.path().to_path_buf(), "test".to_string());

        let tmp_php = tempfile::TempDir::new().expect("tempdir");
        let pm = PhpManager::new(tmp_php.path().to_path_buf());

        let config = HearthConfig::default();

        HearthMcpServer::new(
            Arc::new(Mutex::new(sup)),
            Arc::new(Mutex::new(sm)),
            Arc::new(Mutex::new(pm)),
            Arc::new(Mutex::new(config)),
        )
    }

    #[tokio::test]
    async fn status_tool_returns_services() {
        let server = make_server();
        let result: Result<String, String> = server.hearth_status().await;
        let text = result.expect("hearth_status should succeed");
        assert!(text.contains("nginx"), "should contain nginx, got: {text}");
        assert!(
            text.contains("php-fpm"),
            "should contain php-fpm, got: {text}"
        );
    }

    #[tokio::test]
    async fn sites_tool_returns_empty_list() {
        let server = make_server();
        let result: Result<String, String> = server.hearth_sites().await;
        let text = result.expect("hearth_sites should succeed");
        assert!(
            text.contains("No sites linked"),
            "should report no sites, got: {text}"
        );
    }

    #[tokio::test]
    async fn php_list_tool_works() {
        // Create a server with a mock PHP version in the cache
        let mut sup = ServiceSupervisor::new();
        sup.register(ManagedService::new(
            ServiceKind::PhpFpm,
            "true".to_string(),
            vec![],
        ));

        let tmp_php = tempfile::TempDir::new().expect("tempdir");
        let php_dir = tmp_php.path().join("php/8.3");
        std::fs::create_dir_all(&php_dir).expect("create php dir");
        std::fs::write(php_dir.join("php"), "fake-binary").expect("write php binary");
        let pm = PhpManager::new(tmp_php.path().to_path_buf());

        let tmp_valet = tempfile::TempDir::new().expect("tempdir");
        let sm = SiteManager::new(tmp_valet.path().to_path_buf(), "test".to_string());

        let server = HearthMcpServer::new(
            Arc::new(Mutex::new(sup)),
            Arc::new(Mutex::new(sm)),
            Arc::new(Mutex::new(pm)),
            Arc::new(Mutex::new(HearthConfig::default())),
        );

        let result: Result<String, String> = server.hearth_php_list().await;
        let text = result.expect("hearth_php_list should succeed");
        assert!(
            text.contains("PHP 8.3"),
            "should contain PHP 8.3, got: {text}"
        );
    }

    #[tokio::test]
    async fn php_switch_errors_on_nonexistent_version() {
        let server = make_server();
        let result: Result<String, String> = server
            .hearth_php_switch(Parameters(PhpSwitchParams {
                version: "99.99".to_string(),
            }))
            .await;

        let err = result.expect_err("should return an error for nonexistent PHP version");
        assert!(
            err.contains("not found"),
            "should mention not found, got: {err}"
        );
    }

    #[tokio::test]
    async fn server_info_is_correct() {
        let server = make_server();
        let info = ServerHandler::get_info(&server);
        assert_eq!(info.server_info.name, "hearth");
        assert!(info.capabilities.tools.is_some());
    }

    #[tokio::test]
    async fn tool_router_lists_eleven_tools() {
        let server = make_server();
        let tools = server.tool_router.list_all();
        let names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
        assert_eq!(
            tools.len(),
            11,
            "Expected 11 tools, got {}: {:?}",
            tools.len(),
            names
        );
        for required in [
            "hearth_status",
            "hearth_sites",
            "hearth_php_list",
            "hearth_php_switch",
            "hearth_site_link",
            "hearth_site_unlink",
            "hearth_service_restart",
            "hearth_php_config",
            "hearth_db_start",
            "hearth_db_stop",
            "hearth_db_status",
        ] {
            assert!(
                names.iter().any(|n| n == required),
                "missing tool {required}, have: {names:?}"
            );
        }
    }

    #[tokio::test]
    async fn db_start_rejects_unknown_engine() {
        let server = make_server();
        let result = server
            .hearth_db_start(Parameters(DbActionParams {
                engine: Some("nginx".to_string()),
            }))
            .await;
        let err = result.expect_err("nginx is not a DB engine");
        assert!(err.contains("not a DB engine"), "got: {err}");
    }

    #[tokio::test]
    async fn db_status_reports_three_engines() {
        let server = make_server();
        let text = server.hearth_db_status().await.expect("status ok");
        assert!(text.contains("mysql"), "got: {text}");
        assert!(text.contains("postgresql"), "got: {text}");
        assert!(text.contains("redis"), "got: {text}");
        // Default ports surface in the report.
        assert!(text.contains("3306"));
        assert!(text.contains("5432"));
        assert!(text.contains("6379"));
    }

    #[tokio::test]
    async fn db_stop_unknown_engine_errors_cleanly() {
        let server = make_server();
        let result = server
            .hearth_db_stop(Parameters(DbActionParams {
                engine: Some("flux-capacitor".to_string()),
            }))
            .await;
        let err = result.expect_err("unknown engine errors");
        assert!(err.to_lowercase().contains("unknown"), "got: {err}");
    }
}
