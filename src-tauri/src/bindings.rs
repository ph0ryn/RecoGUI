use std::{fs, path::Path};

use specta_typescript::Typescript;
use tauri_specta::{Builder, ErrorHandlingMode, collect_commands, collect_events};

use crate::{app_events::AppEventEnvelope, application_commands};

pub fn builder() -> Builder<tauri::Wry> {
    Builder::<tauri::Wry>::new()
        .error_handling(ErrorHandlingMode::Throw)
        .commands(collect_commands![
            application_commands::app_get_snapshot,
            application_commands::model_list,
            application_commands::model_select,
            application_commands::audio_list_inputs,
            application_commands::session_start,
            application_commands::session_pause,
            application_commands::session_resume,
            application_commands::session_stop,
            application_commands::queue_get,
            application_commands::queue_add_files,
            application_commands::queue_reorder,
            application_commands::queue_remove,
            application_commands::queue_clear,
            application_commands::queue_start,
            application_commands::queue_pause,
            application_commands::history_query,
            application_commands::history_get,
            application_commands::history_rename,
            application_commands::history_delete,
            application_commands::history_render,
            application_commands::export_start,
            application_commands::export_cancel,
            application_commands::host_resolve_close,
        ])
        .events(collect_events![AppEventEnvelope])
}

pub fn write_typescript(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    builder()
        .export(Typescript::default(), path)
        .map_err(|error| error.to_string())?;

    let generated = fs::read_to_string(path).map_err(|error| error.to_string())?;
    fs::write(path, format!("{}\n", generated.trim_end())).map_err(|error| error.to_string())
}

pub fn check_typescript(path: &Path) -> Result<(), String> {
    let check_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("target/generated-bindings.check.ts");
    write_typescript(&check_path)?;
    let expected = fs::read(path).map_err(|error| error.to_string())?;
    let actual = fs::read(&check_path).map_err(|error| error.to_string())?;
    let _ = fs::remove_file(&check_path);
    if expected == actual {
        Ok(())
    } else {
        Err(format!(
            "generated bindings are stale; run `cargo run --bin generate-bindings -- --write {}`",
            path.display()
        ))
    }
}
