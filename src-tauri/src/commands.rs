use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tauri::State;

use crate::{
    file_tokens::{FileTokenStore, SelectedFile},
    paths::PublicAppPaths,
    protocol::EngineStateSnapshot,
    supervisor::EngineSupervisor,
};

type CommandResult<T> = Result<T, String>;

const ALLOWED_ENGINE_COMMANDS: &[&str] = &[
    "engine.getState",
    "engine.shutdown",
    "model.getState",
    "model.download",
    "model.cancelDownload",
    "model.verify",
    "model.load",
    "model.delete",
    "audio.listInputs",
    "queue.getState",
    "queue.enqueueFiles",
    "queue.reorder",
    "queue.remove",
    "queue.clear",
    "queue.start",
    "queue.pause",
    "session.start",
    "session.stop",
    "session.pause",
    "session.resume",
    "history.list",
    "history.get",
    "history.search",
    "history.delete",
    "history.deleteMany",
    "history.export",
    "history.exportMany",
    "history.cancelExport",
];

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCommand {
    pub session_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStartCommand {
    pub title: Option<String>,
    pub source: SessionSource,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SessionSource {
    Microphone { device_id: Option<String> },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueEnqueueFilesCommand {
    pub files: Vec<QueueFileToken>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueFileToken {
    pub source_token: String,
    pub display_name: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueReorderCommand {
    pub revision: u64,
    pub item_ids: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueRemoveCommand {
    pub item_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryListCommand {
    pub cursor: Option<String>,
    pub limit: Option<u16>,
    pub status: Option<String>,
    pub source: Option<String>,
    pub started_after: Option<String>,
    pub started_before: Option<String>,
    pub sort: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryGetCommand {
    pub session_id: String,
    pub segment_offset: Option<u64>,
    pub segment_limit: Option<u16>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistorySearchCommand {
    pub query: String,
    pub cursor: Option<String>,
    pub limit: Option<u16>,
    pub status: Option<String>,
    pub source: Option<String>,
    pub started_after: Option<String>,
    pub started_before: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIdsCommand {
    pub session_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelDownloadCommand {
    pub allow_cellular: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportCommand {
    pub session_ids: Vec<String>,
    pub format: String,
    pub destination_token: String,
    pub overwrite: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportCancelCommand {
    pub operation_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostInfo {
    pub version: String,
    pub paths: PublicAppPaths,
}

fn validate_page_limit(limit: Option<u16>, maximum: u16) -> CommandResult<()> {
    if limit.is_some_and(|limit| limit == 0 || limit > maximum) {
        return Err(format!("limit must be between 1 and {maximum}"));
    }
    Ok(())
}

fn contains_forbidden_path_key(value: &Value) -> bool {
    match value {
        Value::Object(object) => object.iter().any(|(key, value)| {
            matches!(key.as_str(), "path" | "destination") || contains_forbidden_path_key(value)
        }),
        Value::Array(values) => values.iter().any(contains_forbidden_path_key),
        _ => false,
    }
}

async fn request(
    supervisor: &EngineSupervisor,
    command: &str,
    session_id: Option<String>,
    payload: Value,
) -> CommandResult<Value> {
    supervisor
        .request(command, session_id, payload)
        .await
        .map_err(|error| error.to_string())
}

/// Stable frontend gateway for the versioned engine protocol. The command is
/// allow-listed here; this is not a generic process or shell bridge.
#[tauri::command]
pub async fn engine_request(
    command: String,
    mut payload: Value,
    supervisor: State<'_, EngineSupervisor>,
    tokens: State<'_, FileTokenStore>,
) -> CommandResult<Value> {
    if !ALLOWED_ENGINE_COMMANDS.contains(&command.as_str()) {
        return Err(format!("unsupported engine command: {command}"));
    }
    if contains_forbidden_path_key(&payload) {
        return Err("raw filesystem paths are not accepted from the frontend".into());
    }
    let object = payload
        .as_object_mut()
        .ok_or_else(|| "engine payload must be an object".to_owned())?;
    let mut source_tokens_to_remove = Vec::new();

    if command == "session.start"
        && let Some(source) = object.get_mut("source").and_then(Value::as_object_mut)
        && source.get("type").and_then(Value::as_str) == Some("file")
    {
        return Err("file sources must be added through the processing queue".into());
    }
    if command == "queue.enqueueFiles" {
        let file_values = object
            .remove("files")
            .and_then(|value| value.as_array().cloned())
            .ok_or_else(|| "queue enqueue requires files".to_owned())?;
        let files = file_values
            .into_iter()
            .map(|value| {
                serde_json::from_value::<QueueFileToken>(value)
                    .map_err(|_| "queue files require sourceToken and displayName".to_owned())
            })
            .collect::<CommandResult<Vec<_>>>()?;
        source_tokens_to_remove = files.iter().map(|file| file.source_token.clone()).collect();
        object.insert(
            "files".into(),
            json!(resolve_queue_files(&tokens, &files).await?),
        );
    }
    if matches!(command.as_str(), "history.export" | "history.exportMany") {
        let token = object
            .remove("destinationToken")
            .and_then(|value| value.as_str().map(ToOwned::to_owned))
            .ok_or_else(|| "exports require destinationToken".to_owned())?;
        let path = tokens
            .resolve(&token)
            .await
            .ok_or_else(|| "unknown or expired destination token".to_owned())?;
        object.insert("destination".into(), json!(path));
    }

    let session_id = object
        .get("sessionId")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let result = request(&supervisor, &command, session_id, payload).await;
    if result.is_ok() {
        for token in source_tokens_to_remove {
            tokens.remove(&token).await;
        }
    }
    result
}

#[tauri::command]
pub async fn host_get_info(supervisor: State<'_, EngineSupervisor>) -> CommandResult<HostInfo> {
    Ok(HostInfo {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        paths: supervisor.paths().public(),
    })
}

#[tauri::command]
pub async fn engine_get_state(
    supervisor: State<'_, EngineSupervisor>,
) -> CommandResult<EngineStateSnapshot> {
    Ok(supervisor.state_snapshot().await)
}

#[tauri::command]
pub async fn engine_start(supervisor: State<'_, EngineSupervisor>) -> CommandResult<()> {
    supervisor
        .ensure_started()
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn engine_shutdown(supervisor: State<'_, EngineSupervisor>) -> CommandResult<()> {
    supervisor.shutdown().await;
    Ok(())
}

#[tauri::command]
pub async fn model_get_state(supervisor: State<'_, EngineSupervisor>) -> CommandResult<Value> {
    request(&supervisor, "model.getState", None, json!({})).await
}

#[tauri::command]
pub async fn model_download(
    input: ModelDownloadCommand,
    supervisor: State<'_, EngineSupervisor>,
) -> CommandResult<Value> {
    request(
        &supervisor,
        "model.download",
        None,
        json!({ "allowCellular": input.allow_cellular.unwrap_or(false) }),
    )
    .await
}

macro_rules! empty_engine_command {
    ($function:ident, $wire_name:literal) => {
        #[tauri::command]
        pub async fn $function(supervisor: State<'_, EngineSupervisor>) -> CommandResult<Value> {
            request(&supervisor, $wire_name, None, json!({})).await
        }
    };
}

empty_engine_command!(model_cancel_download, "model.cancelDownload");
empty_engine_command!(model_verify, "model.verify");
empty_engine_command!(model_load, "model.load");
empty_engine_command!(model_delete, "model.delete");
empty_engine_command!(audio_list_inputs, "audio.listInputs");

#[tauri::command]
pub async fn select_audio_file(
    tokens: State<'_, FileTokenStore>,
) -> CommandResult<Option<SelectedFile>> {
    let selected = rfd::AsyncFileDialog::new()
        .set_title("Select an audio file")
        .add_filter(
            "Audio",
            &["wav", "mp3", "m4a", "flac", "aac", "ogg", "opus"],
        )
        .pick_file()
        .await;
    Ok(match selected {
        Some(file) => Some(tokens.insert(file.path().to_path_buf()).await),
        None => None,
    })
}

#[tauri::command]
pub async fn select_audio_files(
    tokens: State<'_, FileTokenStore>,
) -> CommandResult<Vec<SelectedFile>> {
    let selected = rfd::AsyncFileDialog::new()
        .set_title("Select audio files")
        .add_filter(
            "Audio",
            &["wav", "mp3", "m4a", "flac", "aac", "ogg", "opus"],
        )
        .pick_files()
        .await;
    let mut files = Vec::new();
    for file in selected.unwrap_or_default() {
        files.push(tokens.insert(file.path().to_path_buf()).await);
    }
    Ok(files)
}

async fn resolve_queue_files(
    tokens: &FileTokenStore,
    queue_files: &[QueueFileToken],
) -> CommandResult<Vec<Value>> {
    if queue_files.is_empty() {
        return Err("at least one source file is required".into());
    }
    let mut files = Vec::with_capacity(queue_files.len());
    for file in queue_files {
        let path = tokens
            .resolve(&file.source_token)
            .await
            .ok_or_else(|| "unknown or expired source token".to_owned())?;
        if file.display_name.trim().is_empty() {
            return Err("queue file displayName must not be empty".into());
        }
        files.push(json!({ "path": path, "displayName": file.display_name }));
    }
    Ok(files)
}

empty_engine_command!(queue_get_state, "queue.getState");
empty_engine_command!(queue_clear, "queue.clear");
empty_engine_command!(queue_start, "queue.start");
empty_engine_command!(queue_pause, "queue.pause");

#[tauri::command]
pub async fn queue_enqueue_files(
    input: QueueEnqueueFilesCommand,
    supervisor: State<'_, EngineSupervisor>,
    tokens: State<'_, FileTokenStore>,
) -> CommandResult<Value> {
    let files = resolve_queue_files(&tokens, &input.files).await?;
    let result = request(
        &supervisor,
        "queue.enqueueFiles",
        None,
        json!({ "files": files }),
    )
    .await;
    if result.is_ok() {
        for file in input.files {
            tokens.remove(&file.source_token).await;
        }
    }
    result
}

#[tauri::command]
pub async fn queue_reorder(
    input: QueueReorderCommand,
    supervisor: State<'_, EngineSupervisor>,
) -> CommandResult<Value> {
    if input.item_ids.is_empty() {
        return Err("queue order must contain at least one item".into());
    }
    request(&supervisor, "queue.reorder", None, json!(input)).await
}

#[tauri::command]
pub async fn queue_remove(
    input: QueueRemoveCommand,
    supervisor: State<'_, EngineSupervisor>,
) -> CommandResult<Value> {
    request(&supervisor, "queue.remove", None, json!(input)).await
}

#[tauri::command]
pub async fn select_export_destination(
    suggested_name: String,
    extension: String,
    tokens: State<'_, FileTokenStore>,
) -> CommandResult<Option<SelectedFile>> {
    if suggested_name.contains(['/', '\\']) || extension.contains(['/', '\\', '.']) {
        return Err("invalid export file name or extension".into());
    }
    let selected = rfd::AsyncFileDialog::new()
        .set_title("Export transcript")
        .set_file_name(format!("{suggested_name}.{extension}"))
        .add_filter("Export", &[extension.as_str()])
        .save_file()
        .await;
    Ok(match selected {
        Some(file) => Some(tokens.insert(file.path().to_path_buf()).await),
        None => None,
    })
}

#[tauri::command]
pub async fn session_start(
    input: SessionStartCommand,
    supervisor: State<'_, EngineSupervisor>,
) -> CommandResult<Value> {
    let source = match input.source {
        SessionSource::Microphone { device_id } => {
            json!({ "type": "microphone", "deviceId": device_id })
        }
    };
    request(
        &supervisor,
        "session.start",
        None,
        json!({ "title": input.title, "source": source }),
    )
    .await
}

#[tauri::command]
pub async fn session_stop(
    input: SessionCommand,
    supervisor: State<'_, EngineSupervisor>,
) -> CommandResult<Value> {
    request(
        &supervisor,
        "session.stop",
        Some(input.session_id),
        json!({}),
    )
    .await
}

#[tauri::command]
pub async fn session_pause(
    input: SessionCommand,
    supervisor: State<'_, EngineSupervisor>,
) -> CommandResult<Value> {
    request(
        &supervisor,
        "session.pause",
        Some(input.session_id),
        json!({}),
    )
    .await
}

#[tauri::command]
pub async fn session_resume(
    input: SessionCommand,
    supervisor: State<'_, EngineSupervisor>,
) -> CommandResult<Value> {
    request(
        &supervisor,
        "session.resume",
        Some(input.session_id),
        json!({}),
    )
    .await
}

#[tauri::command]
pub async fn history_list(
    input: HistoryListCommand,
    supervisor: State<'_, EngineSupervisor>,
) -> CommandResult<Value> {
    validate_page_limit(input.limit, 100)?;
    request(&supervisor, "history.list", None, json!(input)).await
}

#[tauri::command]
pub async fn history_get(
    input: HistoryGetCommand,
    supervisor: State<'_, EngineSupervisor>,
) -> CommandResult<Value> {
    validate_page_limit(input.segment_limit, 500)?;
    let session_id = input.session_id.clone();
    request(&supervisor, "history.get", Some(session_id), json!(input)).await
}

#[tauri::command]
pub async fn history_search(
    input: HistorySearchCommand,
    supervisor: State<'_, EngineSupervisor>,
) -> CommandResult<Value> {
    validate_page_limit(input.limit, 100)?;
    if input.query.trim().is_empty() {
        return Err("search query must not be empty".into());
    }
    request(&supervisor, "history.search", None, json!(input)).await
}

#[tauri::command]
pub async fn history_delete(
    input: SessionCommand,
    supervisor: State<'_, EngineSupervisor>,
) -> CommandResult<Value> {
    request(
        &supervisor,
        "history.delete",
        Some(input.session_id),
        json!({}),
    )
    .await
}

#[tauri::command]
pub async fn history_delete_many(
    input: SessionIdsCommand,
    supervisor: State<'_, EngineSupervisor>,
) -> CommandResult<Value> {
    if input.session_ids.is_empty() {
        return Err("at least one session is required".into());
    }
    request(&supervisor, "history.deleteMany", None, json!(input)).await
}

#[tauri::command]
pub async fn history_export(
    input: ExportCommand,
    supervisor: State<'_, EngineSupervisor>,
    tokens: State<'_, FileTokenStore>,
) -> CommandResult<Value> {
    if input.session_ids.is_empty() {
        return Err("at least one session is required".into());
    }
    let destination = tokens
        .resolve(&input.destination_token)
        .await
        .ok_or_else(|| "unknown or expired destination token".to_owned())?;
    let command = if input.session_ids.len() == 1 {
        "history.export"
    } else {
        "history.exportMany"
    };
    let result = request(
        &supervisor,
        command,
        None,
        json!({
            "sessionIds": input.session_ids,
            "format": input.format,
            "destination": destination,
            "overwrite": input.overwrite
        }),
    )
    .await;
    if result.is_ok() {
        tokens.remove(&input.destination_token).await;
    }
    result
}

#[tauri::command]
pub async fn history_cancel_export(
    input: ExportCancelCommand,
    supervisor: State<'_, EngineSupervisor>,
) -> CommandResult<Value> {
    request(
        &supervisor,
        "history.cancelExport",
        None,
        json!({ "operationId": input.operation_id }),
    )
    .await
}

#[allow(dead_code)]
fn _path_is_not_exposed(_: PathBuf) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_page_bounds() {
        assert!(validate_page_limit(None, 100).is_ok());
        assert!(validate_page_limit(Some(1), 100).is_ok());
        assert!(validate_page_limit(Some(100), 100).is_ok());
        assert!(validate_page_limit(Some(0), 100).is_err());
        assert!(validate_page_limit(Some(101), 100).is_err());
    }

    #[test]
    fn only_known_protocol_commands_are_allowed() {
        assert!(ALLOWED_ENGINE_COMMANDS.contains(&"session.start"));
        assert!(ALLOWED_ENGINE_COMMANDS.contains(&"queue.enqueueFiles"));
        assert!(ALLOWED_ENGINE_COMMANDS.contains(&"queue.pause"));
        assert!(!ALLOWED_ENGINE_COMMANDS.contains(&"shell.execute"));
    }

    #[tokio::test]
    async fn queue_files_resolve_tokens_without_exposing_them() {
        let tokens = FileTokenStore::default();
        let first = tokens.insert(PathBuf::from("/private/first.wav")).await;
        let second = tokens.insert(PathBuf::from("/private/second.wav")).await;
        let files = vec![
            QueueFileToken {
                source_token: first.token,
                display_name: first.display_name,
            },
            QueueFileToken {
                source_token: second.token,
                display_name: second.display_name,
            },
        ];

        let resolved = resolve_queue_files(&tokens, &files).await.unwrap();

        assert_eq!(resolved[0]["path"], "/private/first.wav");
        assert_eq!(resolved[1]["path"], "/private/second.wav");
        assert!(
            resolved
                .iter()
                .all(|file| file.get("sourceToken").is_none())
        );
    }

    #[test]
    fn rejects_nested_raw_paths() {
        assert!(contains_forbidden_path_key(&json!({
            "source": { "type": "file", "path": "/private/file.wav" }
        })));
        assert!(!contains_forbidden_path_key(&json!({
            "source": { "type": "file", "sourceToken": "opaque" }
        })));
    }

    #[test]
    fn history_get_uses_numeric_segment_offset() {
        let value = serde_json::to_value(HistoryGetCommand {
            session_id: "session-1".into(),
            segment_offset: Some(500),
            segment_limit: Some(500),
        })
        .unwrap();

        assert_eq!(value["segmentOffset"], 500);
        assert_eq!(value["segmentLimit"], 500);
        assert!(value.get("segmentCursor").is_none());
    }
}
