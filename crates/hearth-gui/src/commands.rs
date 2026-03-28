use hearth_lib::client::DaemonClient;
use hearth_lib::socket::{
    DaemonRequest, DaemonResponse, PhpVersionInfo, ServiceStatus, SiteInfo,
};

fn client() -> DaemonClient {
    DaemonClient::new()
}

fn extract_message(resp: DaemonResponse) -> Result<String, String> {
    match resp {
        DaemonResponse::Ok { message } => Ok(message.unwrap_or_default()),
        DaemonResponse::Error { message } => Err(message),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

#[tauri::command]
pub async fn get_status() -> Result<Vec<ServiceStatus>, String> {
    let resp = client()
        .send(DaemonRequest::Status)
        .await
        .map_err(|e| e.to_string())?;
    match resp {
        DaemonResponse::Status { services } => Ok(services),
        DaemonResponse::Error { message } => Err(message),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

#[tauri::command]
pub async fn start_services() -> Result<String, String> {
    let resp = client()
        .send(DaemonRequest::Start)
        .await
        .map_err(|e| e.to_string())?;
    extract_message(resp)
}

#[tauri::command]
pub async fn stop_services() -> Result<String, String> {
    let resp = client()
        .send(DaemonRequest::Stop)
        .await
        .map_err(|e| e.to_string())?;
    extract_message(resp)
}

#[tauri::command]
pub async fn restart_service(service: String) -> Result<String, String> {
    let resp = client()
        .send(DaemonRequest::Restart {
            service: Some(service),
        })
        .await
        .map_err(|e| e.to_string())?;
    extract_message(resp)
}

#[tauri::command]
pub async fn get_sites() -> Result<Vec<SiteInfo>, String> {
    let resp = client()
        .send(DaemonRequest::Sites)
        .await
        .map_err(|e| e.to_string())?;
    match resp {
        DaemonResponse::Sites { sites } => Ok(sites),
        DaemonResponse::Error { message } => Err(message),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

#[tauri::command]
pub async fn link_site(path: String, name: Option<String>) -> Result<String, String> {
    let resp = client()
        .send(DaemonRequest::Link { path, name })
        .await
        .map_err(|e| e.to_string())?;
    extract_message(resp)
}

#[tauri::command]
pub async fn unlink_site(name: String) -> Result<String, String> {
    let resp = client()
        .send(DaemonRequest::Unlink { name })
        .await
        .map_err(|e| e.to_string())?;
    extract_message(resp)
}

#[tauri::command]
pub async fn secure_site(name: String) -> Result<String, String> {
    let resp = client()
        .send(DaemonRequest::Secure { name })
        .await
        .map_err(|e| e.to_string())?;
    extract_message(resp)
}

#[tauri::command]
pub async fn unsecure_site(name: String) -> Result<String, String> {
    let resp = client()
        .send(DaemonRequest::Unsecure { name })
        .await
        .map_err(|e| e.to_string())?;
    extract_message(resp)
}

#[tauri::command]
pub async fn get_php_versions() -> Result<Vec<PhpVersionInfo>, String> {
    let resp = client()
        .send(DaemonRequest::PhpList)
        .await
        .map_err(|e| e.to_string())?;
    match resp {
        DaemonResponse::PhpVersions { versions } => Ok(versions),
        DaemonResponse::Error { message } => Err(message),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

#[tauri::command]
pub async fn switch_php(version: String) -> Result<String, String> {
    let resp = client()
        .send(DaemonRequest::PhpSwitch { version })
        .await
        .map_err(|e| e.to_string())?;
    extract_message(resp)
}

#[tauri::command]
pub async fn set_php_config(key: String, value: String) -> Result<String, String> {
    let resp = client()
        .send(DaemonRequest::PhpConfig {
            version: "active".to_string(),
            key,
            value,
        })
        .await
        .map_err(|e| e.to_string())?;
    extract_message(resp)
}

#[tauri::command]
pub async fn ensure_daemon() -> Result<String, String> {
    let client = client();

    if client.is_daemon_running().await {
        return Ok("daemon already running".to_string());
    }

    // Spawn hearth-daemon as a background process
    std::process::Command::new("hearth-daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to spawn hearth-daemon: {e}"))?;

    // Wait up to 2 seconds for daemon to respond
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if client.is_daemon_running().await {
            return Ok("daemon started".to_string());
        }
    }

    Err("daemon did not start within 2 seconds".to_string())
}

#[tauri::command]
pub async fn get_autostart_enabled(app: tauri::AppHandle) -> Result<bool, String> {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch()
        .is_enabled()
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn set_autostart(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let launcher = app.autolaunch();
    if enabled {
        launcher.enable().map_err(|e| e.to_string())?;
    } else {
        launcher.disable().map_err(|e| e.to_string())?;
    }
    Ok(())
}
