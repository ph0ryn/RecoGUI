mod commands;
mod file_tokens;
mod paths;
mod protocol;
mod supervisor;

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use file_tokens::FileTokenStore;
use paths::AppPaths;
use supervisor::EngineSupervisor;
use tauri::{Manager, WindowEvent};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
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
            app.manage(EngineSupervisor::new(app.handle().clone(), paths));
            app.manage(FileTokenStore::default());
            app.manage(Arc::new(AtomicBool::new(false)));
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let closing = window.state::<Arc<AtomicBool>>();
                if closing.swap(true, Ordering::SeqCst) {
                    return;
                }
                api.prevent_close();
                let app = window.app_handle().clone();
                let supervisor = window.state::<EngineSupervisor>().inner().clone();
                tauri::async_runtime::spawn(async move {
                    supervisor.shutdown().await;
                    app.exit(0);
                });
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::host_get_info,
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
            commands::select_export_destination,
            commands::session_start,
            commands::session_stop,
            commands::session_cancel,
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
