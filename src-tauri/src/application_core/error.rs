use serde::Serialize;
use thiserror::Error;

/// Stable errors produced by the Rust-owned application core.
#[derive(Debug, Error)]
pub enum CoreError {
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("{entity} was not found: {id}")]
    NotFound { entity: &'static str, id: String },
    #[error("stale {entity} version: expected {expected}, current {actual}")]
    StaleVersion {
        entity: &'static str,
        expected: u64,
        actual: u64,
    },
    #[error("invalid {entity} state: expected {expected}, current {actual}")]
    InvalidState {
        entity: &'static str,
        expected: String,
        actual: String,
    },
    #[error("database schema version {found} is unsupported; expected {expected}")]
    UnsupportedSchema { found: u32, expected: u32 },
    #[error("database schema metadata is missing")]
    MissingSchemaMetadata,
    #[error("database schema is invalid: {0}")]
    InvalidDatabase(String),
    #[error("database operation failed: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("audio format is unsupported: {0}")]
    UnsupportedAudio(String),
    #[error("audio decoding failed: {0}")]
    AudioDecode(String),
    #[error("audio normalization failed: {0}")]
    AudioNormalize(String),
    #[error("audio file changed while it was being processed")]
    FileChanged,
    #[error("Silero VAD asset validation failed")]
    InvalidVadAsset,
    #[error("Silero VAD failed: {0}")]
    Vad(String),
    #[error("worker protocol failed: {0}")]
    WorkerProtocol(String),
    #[error("worker request failed ({code}): {message}")]
    WorkerResponse {
        code: String,
        message: String,
        recoverable: bool,
    },
    #[error("worker transport closed")]
    WorkerClosed,
    #[error("worker became unresponsive after 10 seconds without an incoming frame")]
    WorkerUnresponsive,
    #[error("worker process is unavailable: {0}")]
    WorkerUnavailable(String),
    #[error("worker process exited unexpectedly: {0}")]
    WorkerExited(String),
    #[error("application-core worker thread closed")]
    StoreClosed,
    #[error("application-core blocking task failed: {0}")]
    BlockingTask(String),
}

impl CoreError {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidArgument(_) => "invalidArgument",
            Self::NotFound { .. } => "notFound",
            Self::StaleVersion { .. } => "staleVersion",
            Self::InvalidState { .. } => "invalidState",
            Self::UnsupportedSchema { .. } => "unsupportedSchema",
            Self::MissingSchemaMetadata => "missingSchemaMetadata",
            Self::InvalidDatabase(_) => "invalidDatabase",
            Self::Database(_) => "databaseFailure",
            Self::Io(_) => "filesystemFailure",
            Self::UnsupportedAudio(_) => "unsupportedAudio",
            Self::AudioDecode(_) => "audioDecodeFailure",
            Self::AudioNormalize(_) => "audioNormalizeFailure",
            Self::FileChanged => "audioFileChanged",
            Self::InvalidVadAsset => "invalidVadAsset",
            Self::Vad(_) => "vadFailure",
            Self::WorkerProtocol(_) => "workerProtocolFailure",
            Self::WorkerResponse { .. } => "workerRequestFailure",
            Self::WorkerClosed => "workerClosed",
            Self::WorkerUnresponsive => "workerUnresponsive",
            Self::WorkerUnavailable(_) => "workerUnavailable",
            Self::WorkerExited(_) => "workerExited",
            Self::StoreClosed => "storeClosed",
            Self::BlockingTask(_) => "blockingTaskFailure",
        }
    }

    #[must_use]
    pub const fn recoverable(&self) -> bool {
        matches!(
            self,
            Self::StaleVersion { .. }
                | Self::InvalidState { .. }
                | Self::UnsupportedAudio(_)
                | Self::AudioDecode(_)
                | Self::FileChanged
                | Self::WorkerResponse {
                    recoverable: true,
                    ..
                }
                | Self::WorkerClosed
                | Self::WorkerUnresponsive
                | Self::WorkerUnavailable(_)
                | Self::WorkerExited(_)
        )
    }

    #[must_use]
    pub fn payload(&self) -> ErrorPayload {
        ErrorPayload {
            code: self.code(),
            message: self.to_string(),
            recoverable: self.recoverable(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorPayload {
    pub code: &'static str,
    pub message: String,
    pub recoverable: bool,
}
