use std::sync::Arc;
use std::time::Duration;

use hearth_lib::client::DaemonClient;
use hearth_lib::socket::{DaemonRequest, DaemonResponse, ServiceStatus};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::Mutex;

use crate::notifications::NotificationDebouncer;

/// Tray icon state derived from service health.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TrayState {
    /// All services running
    Green,
    /// Some services running, some stopped
    Yellow,
    /// One or more services failed
    Red,
    /// Daemon unreachable
    Grey,
}

/// Set up the system tray icon and start the status polling loop.
pub fn setup_tray(app: &AppHandle) {
    let tray = app.tray_by_id("main").expect("tray icon not found");

    // Left click: toggle dashboard window
    let app_handle = app.clone();
    tray.on_tray_icon_event(move |_tray, event| {
        if let tauri::tray::TrayIconEvent::Click {
            button: tauri::tray::MouseButton::Left,
            ..
        } = event
        {
            toggle_dashboard(&app_handle);
        }
    });

    // Start background polling
    let app_handle = app.clone();
    tauri::async_runtime::spawn(polling_loop(app_handle));
}

/// Toggle the dashboard window visibility.
pub fn toggle_dashboard(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        if window.is_visible().unwrap_or(false) {
            let _ = window.hide();
        } else {
            let _ = window.show();
            let _ = window.set_focus();
        }
    } else {
        let _ = tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::default())
            .title("Hearth")
            .inner_size(900.0, 600.0)
            .build();
    }
}

/// Query the daemon for service status and derive tray state.
pub async fn poll_status(client: &DaemonClient) -> (TrayState, String, Vec<ServiceStatus>) {
    let resp = client.send(DaemonRequest::Status).await;

    match resp {
        Ok(DaemonResponse::Status { services }) => {
            let total = services.len();
            if total == 0 {
                return (TrayState::Grey, "no services registered".to_string(), vec![]);
            }

            let running = services.iter().filter(|s| s.state == "Running").count();
            let failed = services
                .iter()
                .filter(|s| s.state.starts_with("Failed"))
                .count();

            let (state, tooltip) = if failed > 0 {
                (TrayState::Red, format!("{failed} service(s) failed"))
            } else if running == total {
                (
                    TrayState::Green,
                    format!("all {total} services running"),
                )
            } else if running > 0 {
                (
                    TrayState::Yellow,
                    format!("{running}/{total} services running"),
                )
            } else {
                (TrayState::Yellow, "no services running".to_string())
            };

            (state, tooltip, services)
        }
        _ => (
            TrayState::Grey,
            "daemon unreachable".to_string(),
            Vec::new(),
        ),
    }
}

/// Payload emitted to the frontend on tray status changes.
#[derive(Debug, Clone, serde::Serialize)]
struct TrayStatusPayload {
    state: TrayState,
    tooltip: String,
    services: Vec<ServiceStatus>,
}

/// Check whether the service list changed (by name + state).
fn services_changed(prev: &[ServiceStatus], current: &[ServiceStatus]) -> bool {
    if prev.len() != current.len() {
        return true;
    }
    prev.iter().zip(current.iter()).any(|(a, b)| a.name != b.name || a.state != b.state)
}

/// Background polling loop that updates the tray icon and emits events.
async fn polling_loop(app: AppHandle) {
    let client = DaemonClient::new();
    let debouncer = Arc::new(Mutex::new(NotificationDebouncer::new()));
    let mut prev_services: Vec<ServiceStatus> = Vec::new();
    let mut prev_state: Option<TrayState> = None;

    loop {
        let (state, tooltip, services) = poll_status(&client).await;

        // Only update tray icon and tooltip when state changed
        let state_changed = prev_state != Some(state);
        if state_changed {
            if let Some(tray) = app.tray_by_id("main") {
                let icon_rgba: &[u8] = match state {
                    TrayState::Green => include_bytes!("../icons/tray-green.png"),
                    TrayState::Yellow => include_bytes!("../icons/tray-yellow.png"),
                    TrayState::Red => include_bytes!("../icons/tray-red.png"),
                    TrayState::Grey => include_bytes!("../icons/tray-grey.png"),
                };
                if let Ok(icon) = tauri::image::Image::from_bytes(icon_rgba) {
                    let _ = tray.set_icon(Some(icon));
                }
                let _ = tray.set_tooltip(Some(&tooltip));
            }
        }

        // Only emit event to frontend when services actually changed
        if state_changed || services_changed(&prev_services, &services) {
            let _ = app.emit(
                "tray-status-changed",
                TrayStatusPayload {
                    state,
                    tooltip: tooltip.clone(),
                    services: services.clone(),
                },
            );
        }

        // Check for state transitions and send notifications
        check_state_transitions(&app, &debouncer, &prev_services, &services).await;
        prev_state = Some(state);
        prev_services = services;

        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Detect services that transitioned from Running to Failed and notify.
async fn check_state_transitions(
    app: &AppHandle,
    debouncer: &Arc<Mutex<NotificationDebouncer>>,
    prev: &[ServiceStatus],
    current: &[ServiceStatus],
) {
    for svc in current {
        if svc.state.starts_with("Failed") {
            // Check if it was previously Running
            let was_running = prev.iter().any(|p| p.name == svc.name && p.state == "Running");
            if was_running {
                let mut guard = debouncer.lock().await;
                guard.notify_if_allowed(
                    app,
                    &svc.name,
                    "Service Failed",
                    &format!("{} has stopped unexpectedly", svc.name),
                );
            }
        }
    }
}
