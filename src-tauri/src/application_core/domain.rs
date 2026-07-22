use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::error::CoreError;

pub const NORMALIZED_SAMPLE_RATE: u32 = 16_000;
pub const VAD_FRAME_SAMPLES: usize = 512;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SourceKind {
    File,
    Microphone,
    SystemAudio,
}

impl SourceKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Microphone => "microphone",
            Self::SystemAudio => "systemAudio",
        }
    }

    pub fn parse(value: &str) -> Result<Self, CoreError> {
        match value {
            "file" => Ok(Self::File),
            "microphone" => Ok(Self::Microphone),
            "systemAudio" => Ok(Self::SystemAudio),
            _ => Err(CoreError::InvalidArgument(format!(
                "unknown source kind: {value}"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionState {
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

impl SessionState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Running => "running",
            Self::Pausing => "pausing",
            Self::Paused => "paused",
            Self::Stopping => "stopping",
            Self::Completed => "completed",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
            Self::Abandoned => "abandoned",
        }
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Stopped | Self::Failed | Self::Abandoned
        )
    }

    pub fn parse(value: &str) -> Result<Self, CoreError> {
        match value {
            "preparing" => Ok(Self::Preparing),
            "running" => Ok(Self::Running),
            "pausing" => Ok(Self::Pausing),
            "paused" => Ok(Self::Paused),
            "stopping" => Ok(Self::Stopping),
            "completed" => Ok(Self::Completed),
            "stopped" => Ok(Self::Stopped),
            "failed" => Ok(Self::Failed),
            "abandoned" => Ok(Self::Abandoned),
            _ => Err(CoreError::Database(rusqlite::Error::InvalidQuery)),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SplitReason {
    Silence,
    AdaptiveSplit,
    EndOfInput,
}

impl SplitReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Silence => "silence",
            Self::AdaptiveSplit => "adaptive_split",
            Self::EndOfInput => "end_of_input",
        }
    }

    pub fn parse(value: &str) -> Result<Self, CoreError> {
        match value {
            "silence" => Ok(Self::Silence),
            "adaptive_split" => Ok(Self::AdaptiveSplit),
            "end_of_input" => Ok(Self::EndOfInput),
            _ => Err(CoreError::Database(rusqlite::Error::InvalidQuery)),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct VadDiagnostics {
    pub mean_probability: f32,
    pub peak_probability: f32,
    pub speech_ratio: f32,
}

impl VadDiagnostics {
    pub fn validate(&self) -> Result<(), CoreError> {
        if [
            self.mean_probability,
            self.peak_probability,
            self.speech_ratio,
        ]
        .into_iter()
        .all(|value| value.is_finite() && (0.0..=1.0).contains(&value))
        {
            return Ok(());
        }
        Err(CoreError::InvalidArgument(
            "VAD diagnostics must be finite probabilities".into(),
        ))
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TranscriptionDiagnostics {
    pub max_tokens: u32,
    pub generation_tokens: Option<u32>,
    pub prompt_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
    pub model_total_time_ms: Option<u64>,
    pub retry_count: u32,
    pub token_limit_reached: bool,
    pub warning: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSession {
    pub session_id: String,
    pub source_kind: SourceKind,
    pub source_display_name: String,
    pub source_fingerprint: Option<String>,
    pub source_path: Option<String>,
    pub source_device_id: Option<String>,
    pub model: String,
    pub model_revision: Option<String>,
    pub language: Option<String>,
    pub title: String,
    #[serde(default)]
    pub config: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSegment {
    pub index: u32,
    pub start_sample: u64,
    pub end_sample: u64,
    pub split_reason: SplitReason,
    pub text: String,
    pub raw_text: Option<String>,
    pub language: String,
    pub vad: VadDiagnostics,
    pub transcription: TranscriptionDiagnostics,
    pub decode_ms: u64,
    pub queue_wait_ms: u64,
}

impl NewSegment {
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.end_sample <= self.start_sample {
            return Err(CoreError::InvalidArgument(
                "segment end must be greater than its start".into(),
            ));
        }
        if self.language.trim().is_empty() {
            return Err(CoreError::InvalidArgument(
                "segment language must not be empty".into(),
            ));
        }
        if self.end_sample - self.start_sample > 960_000 {
            return Err(CoreError::InvalidArgument(
                "segment must not exceed 960000 normalized samples".into(),
            ));
        }
        self.vad.validate()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SegmentRecord {
    pub index: u32,
    pub start_sample: u64,
    pub end_sample: u64,
    pub split_reason: SplitReason,
    pub text: String,
    pub raw_text: Option<String>,
    pub language: String,
    pub diagnostics: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSnapshot {
    pub session_id: String,
    pub state: SessionState,
    pub title: String,
    pub source_kind: SourceKind,
    pub source_display_name: String,
    pub model: String,
    pub model_revision: Option<String>,
    pub language: String,
    pub detected_languages: Vec<String>,
    pub sample_rate: u32,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub updated_at: String,
    pub media_duration_ms: u64,
    pub total_segments: u32,
    pub recognized_segments: u32,
    pub characters: u64,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub resume_sample: u64,
    pub row_version: u64,
    pub segments: Vec<SegmentRecord>,
    pub next_segment_offset: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMutationReceipt {
    pub session_id: String,
    pub state: SessionState,
    pub row_version: u64,
    pub total_segments: u32,
    pub recognized_segments: u32,
    pub characters: u64,
    pub media_duration_ms: u64,
    pub ended_at: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SegmentMutationReceipt {
    pub session_id: String,
    pub segment: SegmentRecord,
    pub row_version: u64,
    pub total_segments: u32,
    pub recognized_segments: u32,
    pub characters: u64,
    pub media_duration_ms: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ResumeContext {
    pub session_id: String,
    pub state: SessionState,
    pub source_kind: SourceKind,
    pub source_path: Option<String>,
    pub source_device_id: Option<String>,
    pub source_fingerprint: Option<String>,
    pub model: String,
    pub model_revision: Option<String>,
    pub language: String,
    pub resume_sample: u64,
    pub next_segment_index: u32,
    pub row_version: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SelectedModel {
    pub repo_id: String,
    pub revision: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum QueueItemState {
    Pending,
    Invalid,
}

impl QueueItemState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Invalid => "invalid",
        }
    }

    pub fn parse(value: &str) -> Result<Self, CoreError> {
        match value {
            "pending" => Ok(Self::Pending),
            "invalid" => Ok(Self::Invalid),
            _ => Err(CoreError::Database(rusqlite::Error::InvalidQuery)),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NewQueueItem {
    pub item_id: String,
    pub display_name: String,
    pub source_path: String,
    pub source_fingerprint: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct QueueItem {
    pub item_id: String,
    pub display_name: String,
    pub state: QueueItemState,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub added_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct QueueSnapshot {
    pub revision: u64,
    pub items: Vec<QueueItem>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SessionSummary {
    pub session_id: String,
    pub state: SessionState,
    pub title: String,
    pub source_kind: SourceKind,
    pub source_display_name: String,
    pub model: String,
    pub model_revision: Option<String>,
    pub language: String,
    pub detected_languages: Vec<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub updated_at: String,
    pub media_duration_ms: u64,
    pub total_segments: u32,
    pub recognized_segments: u32,
    pub characters: u64,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub resume_sample: u64,
    pub snippet: Option<String>,
    pub row_version: u64,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum HistorySort {
    #[default]
    Newest,
    Oldest,
    Longest,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum HistoryCursor {
    Time {
        started_at: String,
        session_id: String,
    },
    Longest {
        media_duration_ms: u64,
        started_at: String,
        session_id: String,
    },
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct HistoryQuery {
    pub text: Option<String>,
    pub states: Vec<SessionState>,
    pub source_kinds: Vec<SourceKind>,
    pub sort: HistorySort,
    pub cursor: Option<HistoryCursor>,
    pub limit: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct HistoryPage {
    pub items: Vec<SessionSummary>,
    pub next_cursor: Option<HistoryCursor>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LifecycleStopReason {
    AppQuit,
    SystemSleep,
}

impl LifecycleStopReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AppQuit => "appQuit",
            Self::SystemSleep => "systemSleep",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DeleteSession {
    pub session_id: String,
    pub expected_row_version: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioFrame {
    pub start_sample: u64,
    pub samples: Vec<f32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_reason_keeps_distinct_database_and_rasr_spellings() {
        assert_eq!(SplitReason::AdaptiveSplit.as_str(), "adaptive_split");
        assert_eq!(
            serde_json::to_string(&SplitReason::AdaptiveSplit).unwrap(),
            "\"adaptiveSplit\""
        );
        assert_eq!(
            serde_json::from_str::<SplitReason>("\"endOfInput\"").unwrap(),
            SplitReason::EndOfInput
        );
    }
}
