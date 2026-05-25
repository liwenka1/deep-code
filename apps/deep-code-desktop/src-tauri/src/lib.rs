mod runtime_bridge;

use runtime_bridge::{ensure_runtime_server, get_runtime_info};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|_app| {
            tauri::async_runtime::spawn(async {
                if let Err(error) = ensure_runtime_server().await {
                    eprintln!("failed to start embedded runtime: {error}");
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![get_runtime_info])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
