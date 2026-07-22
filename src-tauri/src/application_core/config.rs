use serde::{Deserialize, Serialize};

use crate::application_core::{
    error::CoreError, vad::VadConfig, worker::WorkerTranscriptionConfig,
};

const ASR_QUEUE_CAPACITY: u32 = 2;

#[derive(Clone, Debug)]
pub struct RuntimePipelineConfig {
    pub persisted: serde_json::Value,
    pub vad: VadConfig,
    pub transcription: WorkerTranscriptionConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedPipelineConfig {
    vad: VadConfig,
    transcription: PersistedTranscriptionConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedTranscriptionConfig {
    generation_tokens_per_sec: f32,
    max_generation_tokens: u32,
    min_generation_tokens: u32,
    temperature: f32,
    repetition_penalty: Option<f32>,
    max_transcription_queue_size: u32,
    interrupted_worker_shutdown_timeout_seconds: f32,
    failed_worker_shutdown_timeout_seconds: f32,
}

impl Default for PersistedPipelineConfig {
    fn default() -> Self {
        Self {
            vad: VadConfig::default(),
            transcription: PersistedTranscriptionConfig {
                generation_tokens_per_sec: 20.0,
                max_generation_tokens: 2_048,
                min_generation_tokens: 64,
                temperature: 0.0,
                repetition_penalty: None,
                max_transcription_queue_size: ASR_QUEUE_CAPACITY,
                interrupted_worker_shutdown_timeout_seconds: 30.0,
                failed_worker_shutdown_timeout_seconds: 2.0,
            },
        }
    }
}

pub fn default_pipeline_config() -> Result<RuntimePipelineConfig, CoreError> {
    resolve(PersistedPipelineConfig::default())
}

pub fn parse_pipeline_config(
    value: &serde_json::Value,
) -> Result<RuntimePipelineConfig, CoreError> {
    let persisted = serde_json::from_value(value.clone()).map_err(|error| {
        CoreError::InvalidArgument(format!("saved session config is invalid: {error}"))
    })?;
    resolve(persisted)
}

fn resolve(persisted: PersistedPipelineConfig) -> Result<RuntimePipelineConfig, CoreError> {
    let vad = persisted.vad.validate()?;
    let transcription = &persisted.transcription;
    if !transcription.generation_tokens_per_sec.is_finite()
        || transcription.generation_tokens_per_sec <= 0.0
        || transcription.min_generation_tokens == 0
        || transcription.min_generation_tokens > transcription.max_generation_tokens
        || !transcription.temperature.is_finite()
        || transcription.temperature < 0.0
        || transcription
            .repetition_penalty
            .is_some_and(|value| !value.is_finite() || value <= 0.0)
        || transcription.max_transcription_queue_size != ASR_QUEUE_CAPACITY
        || !transcription
            .interrupted_worker_shutdown_timeout_seconds
            .is_finite()
        || transcription.interrupted_worker_shutdown_timeout_seconds <= 0.0
        || !transcription
            .failed_worker_shutdown_timeout_seconds
            .is_finite()
        || transcription.failed_worker_shutdown_timeout_seconds <= 0.0
    {
        return Err(CoreError::InvalidArgument(
            "saved transcription config is inconsistent".into(),
        ));
    }
    let persisted_value = serde_json::to_value(&persisted).map_err(|error| {
        CoreError::InvalidArgument(format!("session config cannot be serialized: {error}"))
    })?;
    Ok(RuntimePipelineConfig {
        persisted: persisted_value,
        vad,
        transcription: WorkerTranscriptionConfig {
            generation_tokens_per_second: transcription.generation_tokens_per_sec,
            max_generation_tokens: transcription.max_generation_tokens,
            min_generation_tokens: transcription.min_generation_tokens,
            temperature: transcription.temperature,
            repetition_penalty: transcription.repetition_penalty,
        },
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn canonical_config_matches_the_schema_v5_python_shape() {
        let config = default_pipeline_config().unwrap();

        assert_eq!(config.persisted["vad"]["max_segment_duration_ms"], 60_000);
        assert_eq!(
            config.persisted["transcription"]["max_transcription_queue_size"],
            2
        );
        assert_eq!(config.transcription.max_generation_tokens, 2_048);
    }

    #[test]
    fn saved_config_is_strict_and_never_falls_back() {
        assert!(parse_pipeline_config(&json!({"vad": {}})).is_err());
        let mut config = default_pipeline_config().unwrap().persisted;
        config["transcription"]["max_transcription_queue_size"] = json!(3);
        assert!(parse_pipeline_config(&config).is_err());
    }
}
