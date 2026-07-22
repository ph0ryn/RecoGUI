use serde::{Deserialize, Serialize};
use specta::Type;

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AppError {
    pub code: AppErrorCode,
    pub message: String,
    pub recoverable: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum AppErrorCode {
    InvalidInput,
    NotFound,
    SessionBusy,
    SessionNotActive,
    SessionNotResumable,
    QueueActive,
    QueueRevisionConflict,
    ModelUnavailable,
    UnsupportedLanguage,
    PermissionDenied,
    InputDeviceUnavailable,
    CaptureUnavailable,
    SourceUnavailable,
    SourceChanged,
    DatabaseUnavailable,
    WorkerUnavailable,
    WorkerProtocolError,
    SnapshotChanged,
    ExportNotActive,
    Internal,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum SessionStatus {
    Preparing,
    Running,
    Pausing,
    Paused,
    Stopping,
    Completed,
    Stopped,
    Failed,
    Abandoned,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum InputKind {
    File,
    Microphone,
    SystemAudio,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum ResumeMode {
    None,
    Paused,
    RetryFile,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ModelReference {
    pub repo_id: String,
    pub revision: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RecordedModelReference {
    pub repo_id: String,
    pub revision: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub status: SessionStatus,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub duration_ms: f64,
    pub input_kind: InputKind,
    pub input_name: String,
    pub requested_language: Option<String>,
    pub detected_languages: Vec<String>,
    pub model: RecordedModelReference,
    pub row_version: String,
    pub segment_count: u32,
    pub recognized_segment_count: u32,
    pub character_count: u32,
    pub snippet: Option<String>,
    pub resume_mode: ResumeMode,
    pub error: Option<AppError>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum SplitReason {
    Silence,
    AdaptiveSplit,
    EndOfInput,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptSegment {
    pub index: u32,
    pub start_ms: f64,
    pub end_ms: f64,
    pub split_reason: SplitReason,
    pub language: String,
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SessionDetail {
    #[serde(flatten)]
    pub summary: SessionSummary,
    pub segments: Vec<TranscriptSegment>,
    pub next_segment_offset: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct HistoryPage {
    pub items: Vec<SessionSummary>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum HistorySort {
    Newest,
    Oldest,
    Longest,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct HistoryQuery {
    pub cursor: Option<String>,
    pub query: Option<String>,
    pub statuses: Vec<SessionStatus>,
    pub input_kinds: Vec<InputKind>,
    pub sort: HistorySort,
    pub limit: u16,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct HistoryDetailQuery {
    pub session_id: String,
    pub segment_offset: u32,
    pub segment_limit: u16,
    pub expected_row_version: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum QueueItemStatus {
    Pending,
    Invalid,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct QueueItem {
    pub id: String,
    pub display_name: String,
    pub status: QueueItemStatus,
    pub added_at: String,
    pub updated_at: String,
    pub error: Option<AppError>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct QueueSnapshot {
    pub revision: String,
    pub auto_advance_enabled: bool,
    pub items: Vec<QueueItem>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum ModelStatus {
    Checking,
    Unselected,
    Unavailable,
    Ready,
    Error,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ModelState {
    pub status: ModelStatus,
    pub selected: Option<ModelReference>,
    pub error: Option<AppError>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct CachedModelRevision {
    #[serde(flatten)]
    pub reference: ModelReference,
    pub last_modified: String,
    pub refs: Vec<String>,
    pub size: String,
    pub supported_languages: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ModelList {
    pub models: Vec<CachedModelRevision>,
    pub state: ModelState,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AudioInput {
    pub channels: u16,
    pub id: String,
    pub is_default: bool,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum LiveSource {
    Microphone { device_id: Option<String> },
    SystemAudio,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct StartLiveSession {
    pub language: Option<String>,
    pub title: Option<String>,
    pub source: LiveSource,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AppSnapshot {
    pub sequence: String,
    pub active_session: Option<SessionDetail>,
    pub history: HistoryPage,
    pub queue: QueueSnapshot,
    pub model: ModelState,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SessionMutation {
    pub session_id: String,
    pub expected_row_version: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct QueueAddFiles {
    pub language: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct QueueAddFilesResult {
    pub canceled: bool,
    pub queue: QueueSnapshot,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct QueueRevision {
    pub expected_revision: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct QueueReorder {
    pub item_ids: Vec<String>,
    pub expected_revision: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct QueueRemove {
    pub item_id: String,
    pub expected_revision: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct QueueStart {
    pub expected_revision: String,
    pub language: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct HistoryRename {
    pub session_id: String,
    pub expected_row_version: String,
    pub title: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SessionVersion {
    pub session_id: String,
    pub expected_row_version: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct HistoryDelete {
    pub sessions: Vec<SessionVersion>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct HistoryRender {
    pub session_ids: Vec<String>,
    pub format: ExportFormat,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ExportStart {
    pub session_ids: Vec<String>,
    pub format: ExportFormat,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ExportCancel {
    pub operation_id: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum ExportFormat {
    Txt,
    TimestampedTxt,
    Markdown,
    Json,
    Srt,
    Vtt,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ExportStartResult {
    pub canceled: bool,
    pub operation_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum ExportPhase {
    Rendering,
    Writing,
    Publishing,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ExportProgress {
    pub operation_id: String,
    pub phase: ExportPhase,
    pub completed_items: u32,
    pub total_items: u32,
    pub current_session_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ExportFailure {
    pub session_id: String,
    pub error: AppError,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ExportCompletion {
    pub operation_id: String,
    pub canceled: bool,
    pub exported_session_ids: Vec<String>,
    pub failures: Vec<ExportFailure>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum CloseResolution {
    Cancel,
    StopAndQuit,
    ForceQuit,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum AppEvent {
    #[serde(rename = "session.upserted")]
    SessionUpserted {
        sequence: String,
        session: SessionSummary,
    },
    #[serde(rename = "segment.committed")]
    SegmentCommitted {
        sequence: String,
        session_id: String,
        row_version: String,
        segment_count: u32,
        recognized_segment_count: u32,
        character_count: u32,
        duration_ms: f64,
        segment: TranscriptSegment,
    },
    #[serde(rename = "session.progress")]
    SessionProgress {
        sequence: String,
        session_id: String,
        run_id: String,
        processed_audio_ms: f64,
        total_audio_ms: Option<f64>,
        queued_segments: u8,
    },
    #[serde(rename = "sessions.deleted")]
    SessionsDeleted {
        sequence: String,
        session_ids: Vec<String>,
    },
    #[serde(rename = "queue.changed")]
    QueueChanged {
        sequence: String,
        queue: QueueSnapshot,
    },
    #[serde(rename = "model.changed")]
    ModelChanged { sequence: String, model: ModelState },
    #[serde(rename = "export.progress")]
    ExportProgress {
        sequence: String,
        progress: ExportProgress,
    },
    #[serde(rename = "export.finished")]
    ExportFinished {
        sequence: String,
        result: ExportCompletion,
    },
    #[serde(rename = "close.confirmationRequired")]
    CloseConfirmationRequired {
        sequence: String,
        active_session_id: Option<String>,
        active_export_ids: Vec<String>,
    },
    #[serde(rename = "close.forceRequired")]
    CloseForceRequired {
        sequence: String,
        active_session_id: Option<String>,
        error: AppError,
    },
    #[serde(rename = "notification.error")]
    NotificationError { sequence: String, error: AppError },
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn event_is_a_camel_case_discriminated_union() {
        let event = AppEvent::SessionProgress {
            sequence: "9007199254740993".into(),
            session_id: "session-1".into(),
            run_id: "run-2".into(),
            processed_audio_ms: 1_250.5,
            total_audio_ms: Some(2_000.0),
            queued_segments: 2,
        };

        assert_eq!(
            serde_json::to_value(event).unwrap(),
            json!({
                "type": "session.progress",
                "sequence": "9007199254740993",
                "sessionId": "session-1",
                "runId": "run-2",
                "processedAudioMs": 1250.5,
                "totalAudioMs": 2000.0,
                "queuedSegments": 2
            })
        );
    }

    #[test]
    fn live_source_never_accepts_a_file() {
        let source = serde_json::from_value::<LiveSource>(json!({ "type": "file" }));

        assert!(source.is_err());
    }
}
