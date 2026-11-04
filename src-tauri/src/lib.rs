mod commands;
mod config;
mod copier;
mod device;
mod error;
mod meta;
mod models;
mod scanner;
mod state;

#[cfg(test)]
mod tests;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(state::AppState::new())
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::set_library_root,
            commands::list_devices,
            commands::scan_card,
            commands::start_copy,
            commands::cancel_copy,
            commands::pause_copy,
            commands::library_stats,
            commands::open_in_explorer
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
