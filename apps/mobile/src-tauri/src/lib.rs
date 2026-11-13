//! h4rn mobile shell.
//!
//! Same contract as the desktop shell: a thin Tauri layer over the shared
//! React bundle, with QR scanning, deep links, dialogs and URL opening as the
//! only native additions.

/// Build and run the mobile shell.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_barcode_scanner::init())
        .setup(|_app| Ok(()))
        .run(tauri::generate_context!())
        .expect("error while running the h4rn mobile shell");
}
