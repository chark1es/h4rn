//! h4rn desktop shell.
//!
//! The Tauri layer is deliberately thin (ADR-001): it owns window creation and
//! the OS side-channels — deep links, file dialogs, opening URLs — while every
//! piece of product behaviour stays in the Rust daemon behind the WebSocket
//! protocol.

/// Build and run the desktop shell.
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_deep_link::init())
        .setup(|app| {
            #[cfg(desktop)]
            {
                use tauri_plugin_deep_link::DeepLinkExt;
                app.deep_link().register_all()?;
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the h4rn desktop shell");
}
