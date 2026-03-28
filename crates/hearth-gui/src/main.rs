#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;

fn main() {
    tauri::Builder::default()
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
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
