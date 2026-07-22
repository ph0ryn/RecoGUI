mod api_types;
mod app_events;
mod application_commands;
pub mod application_core;
mod audio_capture;
pub mod bindings;
mod native_export;
mod paths;
mod system_sleep;

use std::sync::Arc;

use app_events::TauriEventSink;
use application_core::{ApplicationCore, ApplicationCoreConfig};
use paths::AppPaths;
use tauri::{Manager, WindowEvent};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let invoke_contract = bindings::builder();
    let event_contract = bindings::builder();
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
        .setup(move |app| {
            let paths = AppPaths::resolve(app.handle())?;
            let core = tauri::async_runtime::block_on(ApplicationCore::start(
                ApplicationCoreConfig {
                    database_path: paths.database,
                    worker: paths.worker,
                    vad_asset: paths.vad_asset,
                },
                Arc::new(TauriEventSink::new(app.handle().clone())),
            ))?;
            system_sleep::install(core.clone());
            app.manage(core);
            event_contract.mount_events(app);
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let app = window.app_handle().clone();
                let core = window.state::<ApplicationCore>().inner().clone();
                tauri::async_runtime::spawn(async move {
                    match core.request_close().await {
                        Ok(true) => {
                            match core
                                .shutdown(application_core::domain::LifecycleStopReason::AppQuit)
                                .await
                            {
                                Ok(()) => app.exit(0),
                                Err(error) => {
                                    let _ = core
                                        .report_close_failure(
                                            application_core::contract::app_error(&error),
                                        )
                                        .await;
                                }
                            }
                        }
                        Ok(false) => {}
                        Err(error) => {
                            if core
                                .report_close_failure(application_core::contract::app_error(&error))
                                .await
                                .is_err()
                            {
                                app.exit(1);
                            }
                        }
                    }
                });
            }
        })
        .invoke_handler(invoke_contract.invoke_handler())
        .run(tauri::generate_context!())
        .expect("error while running Tauri application");
}
