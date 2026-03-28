use tauri::AppHandle;
use tauri_plugin_autostart::ManagerExt;

/// Enable auto-start on login by default if not already configured.
///
/// Called once during app setup. If the launch agent is not yet enabled,
/// this enables it so Hearth starts automatically on boot.
pub fn setup_autostart(app: &AppHandle) {
    let launcher = app.autolaunch();

    match launcher.is_enabled() {
        Ok(true) => {
            tracing::info!("login item already enabled");
        }
        Ok(false) => {
            tracing::info!("enabling login item (default on)");
            if let Err(e) = launcher.enable() {
                tracing::warn!(error = %e, "failed to enable login item");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to check login item status");
        }
    }
}
