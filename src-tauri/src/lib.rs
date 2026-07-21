mod close_handshake;
mod commands;
mod file_tokens;
mod paths;
mod protocol;
mod supervisor;
mod system_sleep;

use close_handshake::CloseCoordinator;
use file_tokens::FileTokenStore;
use paths::AppPaths;
use supervisor::EngineSupervisor;
use tauri::{Manager, WindowEvent};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_single_instance::init(
            |app, _arguments, _cwd| {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.unminimize();
                    let _ = window.set_focus();
                }
            },
        ))
        .setup(|app| {
            let paths = AppPaths::resolve(app.handle())?;
            let supervisor = EngineSupervisor::new(app.handle().clone(), paths);
            system_sleep::install(supervisor.clone());
            app.manage(supervisor);
            app.manage(FileTokenStore::default());
            app.manage(CloseCoordinator::default());
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let coordinator = window.state::<CloseCoordinator>();
                if coordinator.allows_native_close() {
                    return;
                }
                api.prevent_close();
                let app = window.app_handle().clone();
                let supervisor = window.state::<EngineSupervisor>().inner().clone();
                let coordinator = coordinator.inner().clone();
                tauri::async_runtime::spawn(async move {
                    coordinator.begin(app, supervisor).await;
                });
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::host_get_info,
            close_handshake::host_resolve_close_request,
            commands::engine_request,
            commands::engine_get_state,
            commands::engine_start,
            commands::engine_shutdown,
            commands::model_get_state,
            commands::model_download,
            commands::model_cancel_download,
            commands::model_verify,
            commands::model_load,
            commands::model_delete,
            commands::audio_list_inputs,
            commands::select_audio_file,
            commands::select_audio_files,
            commands::select_export_destination,
            commands::queue_get_state,
            commands::queue_enqueue_files,
            commands::queue_reorder,
            commands::queue_remove,
            commands::queue_clear,
            commands::queue_start,
            commands::queue_pause,
            commands::session_start,
            commands::session_stop,
            commands::session_pause,
            commands::session_resume,
            commands::history_list,
            commands::history_get,
            commands::history_search,
            commands::history_delete,
            commands::history_delete_many,
            commands::history_export,
            commands::history_cancel_export,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
