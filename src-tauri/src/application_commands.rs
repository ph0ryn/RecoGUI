use std::path::PathBuf;

use tauri::{AppHandle, State};

use crate::{
    api_types::{
        AppError, AppSnapshot, AudioInput, CachedModelRevision, CloseResolution, ExportCancel,
        ExportFormat, ExportStart, ExportStartResult, HistoryDelete, HistoryDetailQuery,
        HistoryPage, HistoryQuery, HistoryRename, HistoryRender, ModelList, ModelReference,
        ModelState, QueueAddFiles, QueueAddFilesResult, QueueRemove, QueueReorder, QueueRevision,
        QueueSnapshot, QueueStart, SessionDetail, SessionMutation, SessionSummary,
        StartLiveSession,
    },
    application_core::{ApplicationCore, contract},
    audio_capture,
};

type CommandResult<T> = Result<T, AppError>;

#[tauri::command]
#[specta::specta]
pub async fn app_get_snapshot(core: State<'_, ApplicationCore>) -> CommandResult<AppSnapshot> {
    core.app_snapshot()
        .await
        .map_err(|error| contract::app_error(&error))
}

#[tauri::command]
#[specta::specta]
pub async fn model_list(core: State<'_, ApplicationCore>) -> CommandResult<ModelList> {
    core.model_list()
        .await
        .map_err(|error| contract::app_error(&error))
}

#[tauri::command]
#[specta::specta]
pub async fn model_select(
    input: ModelReference,
    core: State<'_, ApplicationCore>,
) -> CommandResult<ModelState> {
    core.model_select(input)
        .await
        .map_err(|error| contract::app_error(&error))
}

#[tauri::command]
#[specta::specta]
pub async fn audio_list_inputs() -> CommandResult<Vec<AudioInput>> {
    tokio::task::spawn_blocking(audio_capture::list_input_devices)
        .await
        .map_err(|error| AppError {
            code: crate::api_types::AppErrorCode::Internal,
            message: error.to_string(),
            recoverable: true,
        })?
        .map(|inputs| {
            inputs
                .into_iter()
                .map(|input| AudioInput {
                    channels: input.channels,
                    id: input.id,
                    is_default: input.is_default,
                    name: input.name,
                })
                .collect()
        })
        .map_err(|error| AppError {
            code: crate::api_types::AppErrorCode::InputDeviceUnavailable,
            message: error.to_string(),
            recoverable: true,
        })
}

#[tauri::command]
#[specta::specta]
pub async fn session_start(
    input: StartLiveSession,
    core: State<'_, ApplicationCore>,
) -> CommandResult<SessionDetail> {
    core.start_live(input)
        .await
        .map_err(|error| contract::app_error(&error))
}

macro_rules! session_mutation_command {
    ($name:ident, $method:ident) => {
        #[tauri::command]
        #[specta::specta]
        pub async fn $name(
            input: SessionMutation,
            core: State<'_, ApplicationCore>,
        ) -> CommandResult<SessionDetail> {
            core.$method(input)
                .await
                .map_err(|error| contract::app_error(&error))
        }
    };
}

session_mutation_command!(session_pause, pause_session);
session_mutation_command!(session_resume, resume_session);
session_mutation_command!(session_stop, stop_session);

#[tauri::command]
#[specta::specta]
pub async fn queue_get(core: State<'_, ApplicationCore>) -> CommandResult<QueueSnapshot> {
    core.queue_snapshot()
        .await
        .map_err(|error| contract::app_error(&error))
}

#[tauri::command]
#[specta::specta]
pub async fn queue_add_files(
    input: QueueAddFiles,
    core: State<'_, ApplicationCore>,
) -> CommandResult<QueueAddFilesResult> {
    let files = rfd::AsyncFileDialog::new()
        .add_filter(
            "Audio",
            crate::application_core::media::SUPPORTED_AUDIO_EXTENSIONS,
        )
        .pick_files()
        .await;
    let Some(files) = files else {
        return Ok(QueueAddFilesResult {
            canceled: true,
            queue: core
                .queue_snapshot()
                .await
                .map_err(|error| contract::app_error(&error))?,
        });
    };
    let paths = files
        .into_iter()
        .map(|file| file.path().to_path_buf())
        .collect();
    Ok(QueueAddFilesResult {
        canceled: false,
        queue: core
            .enqueue_files(paths, input.language)
            .await
            .map_err(|error| contract::app_error(&error))?,
    })
}

macro_rules! queue_input_command {
    ($name:ident, $input:ty, $method:ident) => {
        #[tauri::command]
        #[specta::specta]
        pub async fn $name(
            input: $input,
            core: State<'_, ApplicationCore>,
        ) -> CommandResult<QueueSnapshot> {
            core.$method(input)
                .await
                .map_err(|error| contract::app_error(&error))
        }
    };
}

queue_input_command!(queue_reorder, QueueReorder, reorder_queue);
queue_input_command!(queue_remove, QueueRemove, remove_queue_item);
queue_input_command!(queue_clear, QueueRevision, clear_queue);
queue_input_command!(queue_start, QueueStart, start_queue);

#[tauri::command]
#[specta::specta]
pub async fn queue_pause(core: State<'_, ApplicationCore>) -> CommandResult<QueueSnapshot> {
    core.pause_queue()
        .await
        .map_err(|error| contract::app_error(&error))
}

#[tauri::command]
#[specta::specta]
pub async fn history_query(
    input: HistoryQuery,
    core: State<'_, ApplicationCore>,
) -> CommandResult<HistoryPage> {
    core.history(input)
        .await
        .map_err(|error| contract::app_error(&error))
}

#[tauri::command]
#[specta::specta]
pub async fn history_get(
    input: HistoryDetailQuery,
    core: State<'_, ApplicationCore>,
) -> CommandResult<SessionDetail> {
    core.history_get(input)
        .await
        .map_err(|error| contract::app_error(&error))
}

#[tauri::command]
#[specta::specta]
pub async fn history_rename(
    input: HistoryRename,
    core: State<'_, ApplicationCore>,
) -> CommandResult<SessionSummary> {
    core.history_rename(input)
        .await
        .map_err(|error| contract::app_error(&error))
}

#[tauri::command]
#[specta::specta]
pub async fn history_delete(
    input: HistoryDelete,
    core: State<'_, ApplicationCore>,
) -> CommandResult<()> {
    core.history_delete(input)
        .await
        .map_err(|error| contract::app_error(&error))
}

#[tauri::command]
#[specta::specta]
pub async fn history_render(
    input: HistoryRender,
    core: State<'_, ApplicationCore>,
) -> CommandResult<String> {
    core.history_render(input)
        .await
        .map_err(|error| contract::app_error(&error))
}

#[tauri::command]
#[specta::specta]
pub async fn export_start(
    input: ExportStart,
    core: State<'_, ApplicationCore>,
) -> CommandResult<ExportStartResult> {
    let extension = if input.session_ids.len() == 1 {
        export_extension(input.format)
    } else {
        "zip"
    };
    let destination = rfd::AsyncFileDialog::new()
        .add_filter("Transcript", &[extension])
        .set_file_name(format!("transcript.{extension}"))
        .save_file()
        .await;
    let Some(destination) = destination else {
        return Ok(ExportStartResult {
            canceled: true,
            operation_id: None,
        });
    };
    core.export_start(input, PathBuf::from(destination.path()))
        .await
        .map_err(|error| contract::app_error(&error))
}

#[tauri::command]
#[specta::specta]
pub async fn export_cancel(
    input: ExportCancel,
    core: State<'_, ApplicationCore>,
) -> CommandResult<()> {
    core.export_cancel(input.operation_id)
        .await
        .map_err(|error| contract::app_error(&error))
}

#[tauri::command]
#[specta::specta]
pub async fn host_resolve_close(
    input: CloseResolution,
    app: AppHandle,
    core: State<'_, ApplicationCore>,
) -> CommandResult<()> {
    match input {
        CloseResolution::Cancel => Ok(()),
        CloseResolution::StopAndQuit => {
            match core
                .shutdown(crate::application_core::domain::LifecycleStopReason::AppQuit)
                .await
            {
                Ok(()) => app.exit(0),
                Err(error) => {
                    let app_error = contract::app_error(&error);
                    core.report_close_failure(app_error)
                        .await
                        .map_err(|report_error| contract::app_error(&report_error))?;
                }
            }
            Ok(())
        }
        CloseResolution::ForceQuit => {
            app.exit(1);
            Ok(())
        }
    }
}

const fn export_extension(format: ExportFormat) -> &'static str {
    match format {
        ExportFormat::Txt | ExportFormat::TimestampedTxt => "txt",
        ExportFormat::Markdown => "md",
        ExportFormat::Json => "json",
        ExportFormat::Srt => "srt",
        ExportFormat::Vtt => "vtt",
    }
}

#[allow(dead_code)]
fn _assert_generated_model_type(_: CachedModelRevision) {}
