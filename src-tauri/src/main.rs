#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod index;
mod mft;
mod platform;
mod search;
mod state;
mod types;
use state::AppState;
use std::sync::Arc;
fn main() {
    let app_state = Arc::new(AppState::new());

    tauri::Builder::default()
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            commands::drives::get_available_drives,
            commands::indexing::start_indexing,
            commands::indexing::get_index_status,
            commands::search::search,
            commands::files::open_file,
            commands::files::open_folder,
            commands::files::open_folder_and_select,
            commands::files::delete_file,
            commands::files::rename_file,
            commands::files::copy_path_to_clipboard,
            commands::files::get_file_properties,
        ])
        .run(tauri::generate_context!())
        .expect("Failed to run application");
}