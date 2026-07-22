use crate::{
    api_types as api,
    application_core::{
        domain::{self, SessionState, SourceKind},
        error::CoreError,
        worker::CachedModel,
    },
};

pub fn app_error(error: &CoreError) -> api::AppError {
    let code = match error {
        CoreError::InvalidArgument(_) => api::AppErrorCode::InvalidInput,
        CoreError::NotFound { .. } => api::AppErrorCode::NotFound,
        CoreError::StaleVersion { entity, .. } if *entity == "queue" => {
            api::AppErrorCode::QueueRevisionConflict
        }
        CoreError::StaleVersion { .. } => api::AppErrorCode::SnapshotChanged,
        CoreError::InvalidState { entity, .. } if *entity == "session" => {
            api::AppErrorCode::SessionNotResumable
        }
        CoreError::InvalidState { .. } => api::AppErrorCode::InvalidInput,
        CoreError::UnsupportedSchema { .. }
        | CoreError::MissingSchemaMetadata
        | CoreError::InvalidDatabase(_)
        | CoreError::Database(_)
        | CoreError::StoreClosed => api::AppErrorCode::DatabaseUnavailable,
        CoreError::UnsupportedAudio(_) | CoreError::AudioDecode(_) => {
            api::AppErrorCode::SourceUnavailable
        }
        CoreError::AudioNormalize(_) | CoreError::Vad(_) | CoreError::InvalidVadAsset => {
            api::AppErrorCode::CaptureUnavailable
        }
        CoreError::FileChanged => api::AppErrorCode::SourceChanged,
        CoreError::WorkerUnavailable(_) | CoreError::WorkerExited(_) | CoreError::WorkerClosed => {
            api::AppErrorCode::WorkerUnavailable
        }
        CoreError::WorkerProtocol(_) | CoreError::WorkerUnresponsive => {
            api::AppErrorCode::WorkerProtocolError
        }
        CoreError::WorkerResponse { .. } => api::AppErrorCode::WorkerUnavailable,
        CoreError::Io(_) | CoreError::BlockingTask(_) => api::AppErrorCode::Internal,
    };
    api::AppError {
        code,
        message: error.to_string(),
        recoverable: error.recoverable(),
    }
}

pub fn parse_decimal(value: &str, name: &str) -> Result<u64, CoreError> {
    if value.is_empty() || (value.len() > 1 && value.starts_with('0')) {
        return Err(CoreError::InvalidArgument(format!(
            "{name} must be a canonical decimal string"
        )));
    }
    value
        .parse::<u64>()
        .map_err(|_| CoreError::InvalidArgument(format!("{name} is outside the u64 range")))
}

pub const fn status(value: SessionState) -> api::SessionStatus {
    match value {
        SessionState::Preparing => api::SessionStatus::Preparing,
        SessionState::Running => api::SessionStatus::Running,
        SessionState::Pausing => api::SessionStatus::Pausing,
        SessionState::Paused => api::SessionStatus::Paused,
        SessionState::Stopping => api::SessionStatus::Stopping,
        SessionState::Completed => api::SessionStatus::Completed,
        SessionState::Stopped => api::SessionStatus::Stopped,
        SessionState::Failed => api::SessionStatus::Failed,
        SessionState::Abandoned => api::SessionStatus::Abandoned,
    }
}

pub const fn source_kind(value: SourceKind) -> api::InputKind {
    match value {
        SourceKind::File => api::InputKind::File,
        SourceKind::Microphone => api::InputKind::Microphone,
        SourceKind::SystemAudio => api::InputKind::SystemAudio,
    }
}

pub const fn split_reason(value: domain::SplitReason) -> api::SplitReason {
    match value {
        domain::SplitReason::Silence => api::SplitReason::Silence,
        domain::SplitReason::AdaptiveSplit => api::SplitReason::AdaptiveSplit,
        domain::SplitReason::EndOfInput => api::SplitReason::EndOfInput,
    }
}

pub fn summary(value: domain::SessionSummary) -> api::SessionSummary {
    let resume_mode = match (value.state, value.source_kind) {
        (SessionState::Paused, _) => api::ResumeMode::Paused,
        (SessionState::Failed, SourceKind::File) => api::ResumeMode::RetryFile,
        _ => api::ResumeMode::None,
    };
    let error = value.error_message.map(|message| api::AppError {
        code: api::AppErrorCode::Internal,
        message,
        recoverable: resume_mode != api::ResumeMode::None,
    });
    api::SessionSummary {
        id: value.session_id,
        title: value.title,
        status: status(value.state),
        started_at: value.started_at,
        ended_at: value.ended_at,
        duration_ms: value.media_duration_ms as f64,
        input_kind: source_kind(value.source_kind),
        input_name: value.source_display_name,
        requested_language: (value.language != "Auto").then_some(value.language),
        detected_languages: value.detected_languages,
        model: api::RecordedModelReference {
            repo_id: value.model,
            revision: value.model_revision,
        },
        row_version: value.row_version.to_string(),
        segment_count: value.total_segments,
        recognized_segment_count: value.recognized_segments,
        character_count: u32::try_from(value.characters).unwrap_or(u32::MAX),
        snippet: value.snippet,
        resume_mode,
        error,
    }
}

pub fn snapshot_summary(value: &domain::SessionSnapshot) -> api::SessionSummary {
    let resume_mode = match (value.state, value.source_kind) {
        (SessionState::Paused, _) => api::ResumeMode::Paused,
        (SessionState::Failed, SourceKind::File) => api::ResumeMode::RetryFile,
        _ => api::ResumeMode::None,
    };
    api::SessionSummary {
        id: value.session_id.clone(),
        title: value.title.clone(),
        status: status(value.state),
        started_at: value.started_at.clone(),
        ended_at: value.ended_at.clone(),
        duration_ms: value.media_duration_ms as f64,
        input_kind: source_kind(value.source_kind),
        input_name: value.source_display_name.clone(),
        requested_language: (value.language != "Auto").then(|| value.language.clone()),
        detected_languages: value.detected_languages.clone(),
        model: api::RecordedModelReference {
            repo_id: value.model.clone(),
            revision: value.model_revision.clone(),
        },
        row_version: value.row_version.to_string(),
        segment_count: value.total_segments,
        recognized_segment_count: value.recognized_segments,
        character_count: u32::try_from(value.characters).unwrap_or(u32::MAX),
        snippet: value
            .segments
            .iter()
            .find(|segment| !segment.text.is_empty())
            .map(|segment| segment.text.clone()),
        resume_mode,
        error: value.error_message.clone().map(|message| api::AppError {
            code: api::AppErrorCode::Internal,
            message,
            recoverable: resume_mode != api::ResumeMode::None,
        }),
    }
}

pub fn segment(value: domain::SegmentRecord) -> api::TranscriptSegment {
    api::TranscriptSegment {
        index: value.index,
        start_ms: value.start_sample as f64 / 16.0,
        end_ms: value.end_sample as f64 / 16.0,
        split_reason: split_reason(value.split_reason),
        language: value.language,
        text: value.text,
    }
}

pub fn detail(value: domain::SessionSnapshot) -> api::SessionDetail {
    api::SessionDetail {
        summary: snapshot_summary(&value),
        segments: value.segments.into_iter().map(segment).collect(),
        next_segment_offset: value.next_segment_offset,
    }
}

pub fn queue(value: domain::QueueSnapshot, auto_advance_enabled: bool) -> api::QueueSnapshot {
    api::QueueSnapshot {
        revision: value.revision.to_string(),
        auto_advance_enabled,
        items: value
            .items
            .into_iter()
            .map(|item| api::QueueItem {
                id: item.item_id,
                display_name: item.display_name,
                status: match item.state {
                    domain::QueueItemState::Pending => api::QueueItemStatus::Pending,
                    domain::QueueItemState::Invalid => api::QueueItemStatus::Invalid,
                },
                added_at: item.added_at,
                updated_at: item.updated_at,
                error: item.error_message.map(|message| api::AppError {
                    code: api::AppErrorCode::SourceUnavailable,
                    message,
                    recoverable: true,
                }),
            })
            .collect(),
    }
}

pub fn history_query(value: api::HistoryQuery) -> Result<domain::HistoryQuery, CoreError> {
    let cursor = value
        .cursor
        .map(|cursor| {
            serde_json::from_str(&cursor)
                .map_err(|_| CoreError::InvalidArgument("history cursor is invalid".into()))
        })
        .transpose()?;
    Ok(domain::HistoryQuery {
        text: value.query,
        states: value.statuses.into_iter().map(domain_status).collect(),
        source_kinds: value.input_kinds.into_iter().map(domain_source).collect(),
        sort: match value.sort {
            api::HistorySort::Newest => domain::HistorySort::Newest,
            api::HistorySort::Oldest => domain::HistorySort::Oldest,
            api::HistorySort::Longest => domain::HistorySort::Longest,
        },
        cursor,
        limit: u32::from(value.limit),
    })
}

pub fn history_page(value: domain::HistoryPage) -> Result<api::HistoryPage, CoreError> {
    Ok(api::HistoryPage {
        items: value.items.into_iter().map(summary).collect(),
        next_cursor: value
            .next_cursor
            .map(|cursor| serde_json::to_string(&cursor))
            .transpose()
            .map_err(|error| CoreError::InvalidArgument(error.to_string()))?,
    })
}

pub const fn domain_status(value: api::SessionStatus) -> domain::SessionState {
    match value {
        api::SessionStatus::Preparing => SessionState::Preparing,
        api::SessionStatus::Running => SessionState::Running,
        api::SessionStatus::Pausing => SessionState::Pausing,
        api::SessionStatus::Paused => SessionState::Paused,
        api::SessionStatus::Stopping => SessionState::Stopping,
        api::SessionStatus::Completed => SessionState::Completed,
        api::SessionStatus::Stopped => SessionState::Stopped,
        api::SessionStatus::Failed => SessionState::Failed,
        api::SessionStatus::Abandoned => SessionState::Abandoned,
    }
}

pub const fn domain_source(value: api::InputKind) -> domain::SourceKind {
    match value {
        api::InputKind::File => SourceKind::File,
        api::InputKind::Microphone => SourceKind::Microphone,
        api::InputKind::SystemAudio => SourceKind::SystemAudio,
    }
}

pub fn cached_model(value: CachedModel) -> api::CachedModelRevision {
    api::CachedModelRevision {
        reference: api::ModelReference {
            repo_id: value.repo_id,
            revision: value.revision,
        },
        last_modified: value.last_modified,
        refs: value.refs,
        size: value.size,
        supported_languages: value.supported_languages,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_contract_rejects_noncanonical_or_unsafe_values() {
        assert_eq!(
            parse_decimal("9007199254740993", "version").unwrap(),
            9_007_199_254_740_993
        );
        assert!(parse_decimal("01", "version").is_err());
        assert!(parse_decimal("-1", "version").is_err());
    }
}
