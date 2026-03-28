#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod notifications;
mod tray;

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .invoke_handler(tauri::generate_handler![
            commands::get_status,
            commands::start_services,
            commands::stop_services,
            commands::restart_service,
            commands::get_sites,
            commands::link_site,
            commands::unlink_site,
            commands::secure_site,
            commands::unsecure_site,
            commands::get_php_versions,
            commands::switch_php,
            commands::set_php_config,
            commands::ensure_daemon,
            commands::get_autostart_enabled,
            commands::set_autostart,
        ])
        .setup(|app| {
            tray::setup_tray(app.handle());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
