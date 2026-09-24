use serde::{Deserialize, Serialize};

use crate::application_core::{
    domain::{NORMALIZED_SAMPLE_RATE, SplitReason, TranscriptionDiagnostics, VadDiagnostics},
    error::CoreError,
};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkerTranscriptionConfig {
    pub generation_tokens_per_second: f32,
    pub max_generation_tokens: u32,
    pub min_generation_tokens: u32,
    pub temperature: f32,
    pub repetition_penalty: Option<f32>,
}

impl WorkerTranscriptionConfig {
    fn validate(&self) -> Result<(), CoreError> {
        if !self.generation_tokens_per_second.is_finite()
            || self.generation_tokens_per_second <= 0.0
            || self.min_generation_tokens == 0
            || self.min_generation_tokens > self.max_generation_tokens
            || !self.temperature.is_finite()
            || self.temperature < 0.0
            || self
                .repetition_penalty
                .is_some_and(|value| !value.is_finite() || value <= 0.0)
        {
            return Err(CoreError::WorkerProtocol(
                "ASR transcription options are invalid".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SegmentTranscribeRequest {
    pub session_id: String,
    pub run_id: String,
    pub job_id: String,
    pub segment_index: u32,
    pub start_sample: u64,
    pub end_sample: u64,
    pub sample_rate: u32,
    pub split_reason: SplitReason,
    pub language: Option<String>,
    pub vad: VadDiagnostics,
    pub options: WorkerTranscriptionConfig,
}

impl SegmentTranscribeRequest {
    pub fn validate_samples(&self, samples: &[f32]) -> Result<(), CoreError> {
        for (name, value) in [
            ("session ID", self.session_id.as_str()),
            ("run ID", self.run_id.as_str()),
            ("job ID", self.job_id.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(CoreError::WorkerProtocol(format!("{name} is required")));
            }
        }
        if self
            .language
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(CoreError::WorkerProtocol(
                "language must be null or a nonempty string".into(),
            ));
        }
        self.vad
            .validate()
            .map_err(|error| CoreError::WorkerProtocol(error.to_string()))?;
        self.options.validate()?;
        if self.sample_rate != NORMALIZED_SAMPLE_RATE || self.end_sample <= self.start_sample {
            return Err(CoreError::WorkerProtocol(
                "ASR requires a positive 16 kHz sample range".into(),
            ));
        }
        let expected = self.end_sample - self.start_sample;
        if expected > 960_000 || samples.len() as u64 != expected {
            return Err(CoreError::WorkerProtocol(
                "ASR sample length is invalid".into(),
            ));
        }
        if samples.iter().any(|sample| !sample.is_finite()) {
            return Err(CoreError::WorkerProtocol(
                "ASR samples must be finite".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CachedModel {
    pub repo_id: String,
    pub revision: String,
    pub size: String,
    pub last_modified: String,
    pub refs: Vec<String>,
    pub supported_languages: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ModelsListResult {
    pub models: Vec<CachedModel>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ModelLoadResult {
    pub repo_id: String,
    pub revision: String,
    pub load_ms: u64,
    pub reused: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ModelUnloadResult {
    pub unloaded: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ShutdownResult {}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SegmentTranscriptionResult {
    pub session_id: String,
    pub run_id: String,
    pub job_id: String,
    pub segment_index: u32,
    pub text: String,
    pub raw_text: String,
    pub language: String,
    pub diagnostics: TranscriptionDiagnostics,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_range_must_match_the_normalized_pcm() {
        let request = SegmentTranscribeRequest {
            session_id: "session".into(),
            run_id: "run".into(),
            job_id: "job".into(),
            segment_index: 0,
            start_sample: 10,
            end_sample: 12,
            sample_rate: NORMALIZED_SAMPLE_RATE,
            split_reason: SplitReason::EndOfInput,
            language: None,
            vad: VadDiagnostics {
                mean_probability: 0.5,
                peak_probability: 0.5,
                speech_ratio: 0.5,
            },
            options: WorkerTranscriptionConfig {
                generation_tokens_per_second: 20.0,
                max_generation_tokens: 2048,
                min_generation_tokens: 64,
                temperature: 0.0,
                repetition_penalty: None,
            },
        };
        assert!(request.validate_samples(&[0.0, 0.0]).is_ok());
        assert!(request.validate_samples(&[0.0]).is_err());
        assert!(request.validate_samples(&[0.0, f32::NAN]).is_err());
    }
}
