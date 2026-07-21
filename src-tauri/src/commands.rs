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
    File { source_token: String },
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

    if command == "session.start"
        && let Some(source) = object.get_mut("source").and_then(Value::as_object_mut)
        && source.get("type").and_then(Value::as_str) == Some("file")
    {
        let token = source
            .remove("sourceToken")
            .and_then(|value| value.as_str().map(ToOwned::to_owned))
            .ok_or_else(|| "file sources require sourceToken".to_owned())?;
        let path = tokens
            .resolve(&token)
            .await
            .ok_or_else(|| "unknown or expired source token".to_owned())?;
        source.insert("path".into(), json!(path));
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
    request(&supervisor, &command, session_id, payload).await
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
    tokens: State<'_, FileTokenStore>,
) -> CommandResult<Value> {
    let source = match input.source {
        SessionSource::Microphone { device_id } => {
            json!({ "type": "microphone", "deviceId": device_id })
        }
        SessionSource::File { source_token } => {
            let path = tokens
                .resolve(&source_token)
                .await
                .ok_or_else(|| "unknown or expired source token".to_owned())?;
            json!({ "type": "file", "path": path })
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
        assert!(!ALLOWED_ENGINE_COMMANDS.contains(&"shell.execute"));
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
