use std::{collections::HashSet, time::Duration};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::{
    net::{UnixStream, unix::OwnedReadHalf, unix::OwnedWriteHalf},
    time::{Instant, MissedTickBehavior, interval_at, sleep_until},
};

use crate::application_core::{
    domain::{NORMALIZED_SAMPLE_RATE, SplitReason, VadDiagnostics},
    error::CoreError,
    worker::{FrameKind, read_frame, write_frame},
};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub enum WorkerOperation {
    #[serde(rename = "models.list")]
    ModelsList,
    #[serde(rename = "model.load")]
    ModelLoad,
    #[serde(rename = "segment.transcribe")]
    SegmentTranscribe,
    #[serde(rename = "model.unload")]
    ModelUnload,
    #[serde(rename = "shutdown")]
    Shutdown,
}

impl WorkerOperation {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ModelsList => "models.list",
            Self::ModelLoad => "model.load",
            Self::SegmentTranscribe => "segment.transcribe",
            Self::ModelUnload => "model.unload",
            Self::Shutdown => "shutdown",
        }
    }
}

pub trait RequestMetadata: Serialize {
    fn request_id(&self) -> &str;
    fn operation(&self) -> WorkerOperation;
    fn validate_binary(&self, binary: &[u8]) -> Result<(), CoreError>;
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct WorkerHello {
    pub worker_version: String,
    pub capabilities: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Heartbeat {}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct ModelsListRequest {
    pub request_id: String,
    pub operation: WorkerOperation,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct ModelLoadRequest {
    pub request_id: String,
    pub operation: WorkerOperation,
    pub repo_id: String,
    pub revision: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct ModelUnloadRequest {
    pub request_id: String,
    pub operation: WorkerOperation,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct WorkerTranscriptionConfig {
    pub generation_tokens_per_second: f32,
    pub max_generation_tokens: u32,
    pub min_generation_tokens: u32,
    pub temperature: f32,
    pub repetition_penalty: Option<f32>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct SegmentTranscribeRequest {
    pub request_id: String,
    pub operation: WorkerOperation,
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

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct ShutdownRequest {
    pub request_id: String,
    pub operation: WorkerOperation,
}

macro_rules! empty_binary_request {
    ($type:ty, $operation:expr) => {
        impl RequestMetadata for $type {
            fn request_id(&self) -> &str {
                &self.request_id
            }

            fn operation(&self) -> WorkerOperation {
                $operation
            }

            fn validate_binary(&self, binary: &[u8]) -> Result<(), CoreError> {
                validate_operation(self.operation, $operation)?;
                validate_nonempty("requestId", &self.request_id)?;
                if binary.is_empty() {
                    Ok(())
                } else {
                    Err(CoreError::WorkerProtocol(format!(
                        "{} does not accept a binary payload",
                        $operation.as_str()
                    )))
                }
            }
        }
    };
}

empty_binary_request!(ModelsListRequest, WorkerOperation::ModelsList);
empty_binary_request!(ModelUnloadRequest, WorkerOperation::ModelUnload);
empty_binary_request!(ShutdownRequest, WorkerOperation::Shutdown);

impl RequestMetadata for ModelLoadRequest {
    fn request_id(&self) -> &str {
        &self.request_id
    }

    fn operation(&self) -> WorkerOperation {
        WorkerOperation::ModelLoad
    }

    fn validate_binary(&self, binary: &[u8]) -> Result<(), CoreError> {
        validate_operation(self.operation, WorkerOperation::ModelLoad)?;
        validate_nonempty("requestId", &self.request_id)?;
        validate_nonempty("repoId", &self.repo_id)?;
        validate_nonempty("revision", &self.revision)?;
        if binary.is_empty() {
            Ok(())
        } else {
            Err(CoreError::WorkerProtocol(
                "model.load does not accept a binary payload".into(),
            ))
        }
    }
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
                "segment.transcribe options are invalid".into(),
            ));
        }
        Ok(())
    }
}

impl RequestMetadata for SegmentTranscribeRequest {
    fn request_id(&self) -> &str {
        &self.request_id
    }

    fn operation(&self) -> WorkerOperation {
        WorkerOperation::SegmentTranscribe
    }

    fn validate_binary(&self, binary: &[u8]) -> Result<(), CoreError> {
        validate_operation(self.operation, WorkerOperation::SegmentTranscribe)?;
        for (name, value) in [
            ("requestId", self.request_id.as_str()),
            ("sessionId", self.session_id.as_str()),
            ("runId", self.run_id.as_str()),
            ("jobId", self.job_id.as_str()),
        ] {
            validate_nonempty(name, value)?;
        }
        if self
            .language
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(CoreError::WorkerProtocol(
                "language must be null or a non-empty string".into(),
            ));
        }
        self.vad
            .validate()
            .map_err(|error| CoreError::WorkerProtocol(error.to_string()))?;
        self.options.validate()?;
        if self.sample_rate != NORMALIZED_SAMPLE_RATE || self.end_sample <= self.start_sample {
            return Err(CoreError::WorkerProtocol(
                "segment.transcribe requires a positive 16 kHz sample range".into(),
            ));
        }
        let samples = self.end_sample - self.start_sample;
        if samples > 960_000 {
            return Err(CoreError::WorkerProtocol(
                "segment.transcribe exceeds the 960000-sample limit".into(),
            ));
        }
        let expected = usize::try_from(samples)
            .ok()
            .and_then(|samples| samples.checked_mul(size_of::<f32>()))
            .ok_or_else(|| CoreError::WorkerProtocol("segment payload size overflow".into()))?;
        if expected != binary.len() {
            return Err(CoreError::WorkerProtocol(format!(
                "segment payload has {} bytes instead of {expected}",
                binary.len()
            )));
        }
        if binary.chunks_exact(size_of::<f32>()).any(|sample| {
            !f32::from_le_bytes(sample.try_into().expect("four-byte PCM chunk")).is_finite()
        }) {
            return Err(CoreError::WorkerProtocol(
                "segment payload contains a non-finite PCM sample".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct WorkerError {
    pub code: String,
    pub message: String,
    pub recoverable: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct WorkerResponse {
    pub request_id: String,
    pub operation: WorkerOperation,
    pub ok: bool,
    pub result: Option<Value>,
    pub error: Option<WorkerError>,
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct ModelUnloadResult {
    pub unloaded: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
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
    pub diagnostics: crate::application_core::domain::TranscriptionDiagnostics,
}

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const INACTIVITY_TIMEOUT: Duration = Duration::from_secs(10);

/// Sequential, one-in-flight RASR transport attached to an inherited full-duplex socket.
pub struct WorkerSupervisor {
    reader: OwnedReadHalf,
    writer: OwnedWriteHalf,
    hello: Option<WorkerHello>,
    last_heartbeat: Option<Instant>,
    last_incoming: Instant,
    request_ids: HashSet<String>,
    heartbeat_interval: Duration,
    inactivity_timeout: Duration,
}

impl WorkerSupervisor {
    #[must_use]
    pub fn attach(stream: UnixStream) -> Self {
        Self::attach_with_timing(stream, HEARTBEAT_INTERVAL, INACTIVITY_TIMEOUT)
    }

    fn attach_with_timing(
        stream: UnixStream,
        heartbeat_interval: Duration,
        inactivity_timeout: Duration,
    ) -> Self {
        let (reader, writer) = stream.into_split();
        Self {
            reader,
            writer,
            hello: None,
            last_heartbeat: None,
            last_incoming: Instant::now(),
            request_ids: HashSet::new(),
            heartbeat_interval,
            inactivity_timeout,
        }
    }

    pub async fn handshake(&mut self) -> Result<&WorkerHello, CoreError> {
        if self.hello.is_some() {
            return self
                .hello
                .as_ref()
                .ok_or_else(|| CoreError::WorkerProtocol("worker hello disappeared".into()));
        }
        let frame = tokio::time::timeout(self.inactivity_timeout, read_frame(&mut self.reader))
            .await
            .map_err(|_| CoreError::WorkerUnresponsive)??;
        self.note_incoming(&frame)?;
        if frame.kind != FrameKind::Hello || !frame.binary.is_empty() {
            return Err(CoreError::WorkerProtocol(
                "the first worker frame must be a metadata-only Hello".into(),
            ));
        }
        let hello: WorkerHello = frame.metadata_as()?;
        validate_nonempty("workerVersion", &hello.worker_version)?;
        let expected = [
            "models.list",
            "model.load",
            "segment.transcribe",
            "model.unload",
            "shutdown",
        ];
        let actual: HashSet<_> = hello.capabilities.iter().map(String::as_str).collect();
        if hello.capabilities.len() != expected.len()
            || actual.len() != expected.len()
            || actual != expected.into_iter().collect()
        {
            return Err(CoreError::WorkerProtocol(
                "worker Hello does not advertise the complete RASR v1 capability set".into(),
            ));
        }
        self.hello = Some(hello);
        self.hello
            .as_ref()
            .ok_or_else(|| CoreError::WorkerProtocol("worker hello disappeared".into()))
    }

    pub async fn request<T, R>(&mut self, request: &T, binary: &[u8]) -> Result<R, CoreError>
    where
        T: RequestMetadata,
        R: DeserializeOwned,
    {
        let metadata = serde_json::to_value(request)
            .map_err(|error| CoreError::WorkerProtocol(error.to_string()))?;
        let result = self.request_value(metadata, binary).await?;
        serde_json::from_value(result).map_err(|error| CoreError::WorkerProtocol(error.to_string()))
    }

    pub(crate) async fn request_value(
        &mut self,
        metadata: Value,
        binary: &[u8],
    ) -> Result<Value, CoreError> {
        if self.hello.is_none() {
            return Err(CoreError::WorkerProtocol(
                "worker request was sent before Hello".into(),
            ));
        }
        let (request_id, operation) = request_identity(&metadata)?;
        if request_id.is_empty() {
            return Err(CoreError::WorkerProtocol(
                "worker request ID must not be empty".into(),
            ));
        }
        if !self.request_ids.insert(request_id.to_owned()) {
            return Err(CoreError::WorkerProtocol(format!(
                "duplicate worker request ID: {request_id}"
            )));
        }
        let validated_operation = validate_request_metadata(metadata.clone(), binary)?;
        if validated_operation != operation {
            return Err(CoreError::WorkerProtocol(
                "worker request operation changed during validation".into(),
            ));
        }
        write_frame(&mut self.writer, FrameKind::Request, &metadata, binary).await?;
        let mut heartbeat = interval_at(
            Instant::now() + self.heartbeat_interval,
            self.heartbeat_interval,
        );
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            let deadline = self.last_incoming + self.inactivity_timeout;
            let frame = tokio::select! {
                frame = read_frame(&mut self.reader) => frame?,
                _ = heartbeat.tick() => {
                    self.send_heartbeat().await?;
                    continue;
                }
                () = sleep_until(deadline) => return Err(CoreError::WorkerUnresponsive),
            };
            self.note_incoming(&frame)?;
            match frame.kind {
                FrameKind::Heartbeat => {
                    // Shape and payload are checked by `note_incoming`.
                }
                FrameKind::Response => {
                    if !frame.binary.is_empty() {
                        return Err(CoreError::WorkerProtocol(
                            "Response must not carry binary data".into(),
                        ));
                    }
                    let response: WorkerResponse = frame.metadata_as()?;
                    if response.request_id != request_id || response.operation != operation {
                        return Err(CoreError::WorkerProtocol(
                            "worker response correlation does not match the active request".into(),
                        ));
                    }
                    return decode_response(response);
                }
                FrameKind::Hello | FrameKind::Request => {
                    return Err(CoreError::WorkerProtocol(
                        "worker sent a frame kind that is invalid in the host direction".into(),
                    ));
                }
            }
        }
    }

    pub async fn send_heartbeat(&mut self) -> Result<(), CoreError> {
        write_frame(&mut self.writer, FrameKind::Heartbeat, &Heartbeat {}, &[]).await
    }

    pub(crate) async fn receive_idle(&mut self) -> Result<(), CoreError> {
        let frame = read_frame(&mut self.reader).await?;
        self.note_incoming(&frame)?;
        if frame.kind != FrameKind::Heartbeat {
            return Err(CoreError::WorkerProtocol(
                "worker sent a non-Heartbeat frame without an active request".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn incoming_deadline(&self) -> Instant {
        self.last_incoming + self.inactivity_timeout
    }

    #[must_use]
    pub fn heartbeat_is_fresh(&self, maximum_age: Duration) -> bool {
        self.last_heartbeat
            .is_some_and(|last| last.elapsed() <= maximum_age)
    }

    fn note_incoming(
        &mut self,
        frame: &crate::application_core::worker::RasrFrame,
    ) -> Result<(), CoreError> {
        self.last_incoming = Instant::now();
        if frame.kind == FrameKind::Heartbeat {
            if !frame.binary.is_empty() {
                return Err(CoreError::WorkerProtocol(
                    "Heartbeat must not carry binary data".into(),
                ));
            }
            let _: Heartbeat = frame.metadata_as()?;
            self.last_heartbeat = Some(self.last_incoming);
        }
        Ok(())
    }
}

fn request_identity(metadata: &Value) -> Result<(&str, WorkerOperation), CoreError> {
    let object = metadata
        .as_object()
        .ok_or_else(|| CoreError::WorkerProtocol("request metadata must be an object".into()))?;
    let request_id = object
        .get("requestId")
        .and_then(Value::as_str)
        .ok_or_else(|| CoreError::WorkerProtocol("requestId must be a string".into()))?;
    let operation = object
        .get("operation")
        .cloned()
        .ok_or_else(|| CoreError::WorkerProtocol("operation is required".into()))?;
    let operation = serde_json::from_value(operation)
        .map_err(|error| CoreError::WorkerProtocol(error.to_string()))?;
    Ok((request_id, operation))
}

pub fn validate_request_metadata(
    metadata: Value,
    binary: &[u8],
) -> Result<WorkerOperation, CoreError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Identity {
        operation: WorkerOperation,
    }

    let identity: Identity = serde_json::from_value(metadata.clone())
        .map_err(|error| CoreError::WorkerProtocol(error.to_string()))?;
    match identity.operation {
        WorkerOperation::ModelsList => {
            let request: ModelsListRequest = serde_json::from_value(metadata)
                .map_err(|error| CoreError::WorkerProtocol(error.to_string()))?;
            request.validate_binary(binary)?;
        }
        WorkerOperation::ModelLoad => {
            let request: ModelLoadRequest = serde_json::from_value(metadata)
                .map_err(|error| CoreError::WorkerProtocol(error.to_string()))?;
            request.validate_binary(binary)?;
        }
        WorkerOperation::SegmentTranscribe => {
            let request: SegmentTranscribeRequest = serde_json::from_value(metadata)
                .map_err(|error| CoreError::WorkerProtocol(error.to_string()))?;
            request.validate_binary(binary)?;
        }
        WorkerOperation::ModelUnload => {
            let request: ModelUnloadRequest = serde_json::from_value(metadata)
                .map_err(|error| CoreError::WorkerProtocol(error.to_string()))?;
            request.validate_binary(binary)?;
        }
        WorkerOperation::Shutdown => {
            let request: ShutdownRequest = serde_json::from_value(metadata)
                .map_err(|error| CoreError::WorkerProtocol(error.to_string()))?;
            request.validate_binary(binary)?;
        }
    }
    Ok(identity.operation)
}

fn validate_operation(actual: WorkerOperation, expected: WorkerOperation) -> Result<(), CoreError> {
    if actual == expected {
        Ok(())
    } else {
        Err(CoreError::WorkerProtocol(format!(
            "request operation must be {}",
            expected.as_str()
        )))
    }
}

fn validate_nonempty(name: &str, value: &str) -> Result<(), CoreError> {
    if value.trim().is_empty() {
        Err(CoreError::WorkerProtocol(format!(
            "{name} must be a non-empty string"
        )))
    } else {
        Ok(())
    }
}

fn decode_response<R: DeserializeOwned>(response: WorkerResponse) -> Result<R, CoreError> {
    match (response.ok, response.result, response.error) {
        (true, Some(result), None) => serde_json::from_value(result)
            .map_err(|error| CoreError::WorkerProtocol(error.to_string())),
        (false, None, Some(error)) => Err(CoreError::WorkerResponse {
            code: error.code,
            message: error.message,
            recoverable: error.recoverable,
        }),
        _ => Err(CoreError::WorkerProtocol(
            "worker Response must contain exactly one matching result or error".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use serde_json::json;

    use super::*;
    use crate::application_core::worker::{RasrFrame, read_frame};

    fn hello() -> WorkerHello {
        WorkerHello {
            worker_version: "test-worker".into(),
            capabilities: [
                "models.list",
                "model.load",
                "segment.transcribe",
                "model.unload",
                "shutdown",
            ]
            .map(str::to_string)
            .to_vec(),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn explicit_hello_precedes_a_correlated_typed_response() {
        let (host, mut worker) = UnixStream::pair().unwrap();
        let worker_task = tokio::spawn(async move {
            write_frame(&mut worker, FrameKind::Hello, &hello(), &[])
                .await
                .unwrap();
            let request = read_frame(&mut worker).await.unwrap();
            let metadata: ModelsListRequest = request.metadata_as().unwrap();
            assert_eq!(metadata.operation, WorkerOperation::ModelsList);
            write_frame(&mut worker, FrameKind::Heartbeat, &Heartbeat {}, &[])
                .await
                .unwrap();
            write_frame(
                &mut worker,
                FrameKind::Response,
                &WorkerResponse {
                    request_id: metadata.request_id,
                    operation: WorkerOperation::ModelsList,
                    ok: true,
                    result: Some(json!({"models": []})),
                    error: None,
                },
                &[],
            )
            .await
            .unwrap();
        });

        let mut supervisor = WorkerSupervisor::attach(host);
        assert_eq!(
            supervisor.handshake().await.unwrap().worker_version,
            "test-worker"
        );
        let result: Value = supervisor
            .request(
                &ModelsListRequest {
                    request_id: "request-one".into(),
                    operation: WorkerOperation::ModelsList,
                },
                &[],
            )
            .await
            .unwrap();
        assert_eq!(result, json!({"models": []}));
        assert!(supervisor.heartbeat_is_fresh(Duration::from_secs(1)));
        worker_task.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn transcription_payload_length_is_checked_before_writing() {
        let (host, _worker) = UnixStream::pair().unwrap();
        let request = SegmentTranscribeRequest {
            request_id: "request".into(),
            operation: WorkerOperation::SegmentTranscribe,
            session_id: "session".into(),
            run_id: "run".into(),
            job_id: "job".into(),
            segment_index: 0,
            start_sample: 0,
            end_sample: 512,
            sample_rate: 16_000,
            split_reason: SplitReason::Silence,
            language: None,
            vad: VadDiagnostics {
                mean_probability: 0.5,
                peak_probability: 0.9,
                speech_ratio: 0.75,
            },
            options: WorkerTranscriptionConfig {
                generation_tokens_per_second: 20.0,
                max_generation_tokens: 2_048,
                min_generation_tokens: 64,
                temperature: 0.0,
                repetition_penalty: None,
            },
        };
        let mut supervisor = WorkerSupervisor::attach(host);
        supervisor.hello = Some(hello());
        assert!(
            supervisor
                .request::<_, Value>(&request, &[0; 4])
                .await
                .is_err()
        );
        let mut one_sample = request;
        one_sample.end_sample = 1;
        assert!(one_sample.validate_binary(&f32::NAN.to_le_bytes()).is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn active_request_sends_heartbeats_without_an_inference_timeout() {
        let (host, mut worker) = UnixStream::pair().unwrap();
        let worker_task = tokio::spawn(async move {
            write_frame(&mut worker, FrameKind::Hello, &hello(), &[])
                .await
                .unwrap();
            let request = read_frame(&mut worker).await.unwrap();
            let metadata: ModelsListRequest = request.metadata_as().unwrap();
            let heartbeat = read_frame(&mut worker).await.unwrap();
            assert_eq!(heartbeat.kind, FrameKind::Heartbeat);
            let _: Heartbeat = heartbeat.metadata_as().unwrap();
            write_frame(
                &mut worker,
                FrameKind::Response,
                &WorkerResponse {
                    request_id: metadata.request_id,
                    operation: WorkerOperation::ModelsList,
                    ok: true,
                    result: Some(json!({"models": []})),
                    error: None,
                },
                &[],
            )
            .await
            .unwrap();
        });
        let mut supervisor = WorkerSupervisor::attach_with_timing(
            host,
            Duration::from_millis(5),
            Duration::from_millis(100),
        );
        supervisor.handshake().await.unwrap();
        let result: ModelsListResult = supervisor
            .request(
                &ModelsListRequest {
                    request_id: "heartbeat-request".into(),
                    operation: WorkerOperation::ModelsList,
                },
                &[],
            )
            .await
            .unwrap();
        assert!(result.models.is_empty());
        worker_task.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn no_incoming_frame_marks_the_worker_unresponsive() {
        let (host, mut worker) = UnixStream::pair().unwrap();
        let worker_task = tokio::spawn(async move {
            write_frame(&mut worker, FrameKind::Hello, &hello(), &[])
                .await
                .unwrap();
            let _request = read_frame(&mut worker).await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        let mut supervisor = WorkerSupervisor::attach_with_timing(
            host,
            Duration::from_millis(5),
            Duration::from_millis(30),
        );
        supervisor.handshake().await.unwrap();
        assert!(matches!(
            supervisor
                .request::<_, ModelsListResult>(
                    &ModelsListRequest {
                        request_id: "unresponsive-request".into(),
                        operation: WorkerOperation::ModelsList,
                    },
                    &[],
                )
                .await,
            Err(CoreError::WorkerUnresponsive)
        ));
        worker_task.abort();
    }

    #[test]
    fn response_shape_requires_exactly_one_result_or_error() {
        let invalid = WorkerResponse {
            request_id: "request".into(),
            operation: WorkerOperation::Shutdown,
            ok: true,
            result: None,
            error: None,
        };
        assert!(decode_response::<Value>(invalid).is_err());
    }

    #[test]
    fn shared_rasr_fixture_validates_every_metadata_shape() {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Fixture {
            valid: Vec<FixtureFrame>,
            invalid_metadata: Vec<FixtureFrame>,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct FixtureFrame {
            name: String,
            kind: u16,
            metadata: Value,
            binary_hex: String,
        }

        let fixture: Fixture =
            serde_json::from_str(include_str!("../../../../fixtures/rasr-v1/frames.json")).unwrap();
        for frame in fixture.valid {
            let binary = decode_hex(&frame.binary_hex);
            match frame.kind {
                1 => {
                    let hello: WorkerHello = serde_json::from_value(frame.metadata).unwrap();
                    assert_eq!(hello.capabilities.len(), 5, "{}", frame.name);
                }
                2 => {
                    validate_request_metadata(frame.metadata, &binary)
                        .unwrap_or_else(|error| panic!("{}: {error}", frame.name));
                }
                3 => {
                    let response: WorkerResponse = serde_json::from_value(frame.metadata).unwrap();
                    let result: SegmentTranscriptionResult = decode_response(response).unwrap();
                    assert_eq!(result.job_id, "job-opaque", "{}", frame.name);
                }
                4 => {
                    let _: Heartbeat = serde_json::from_value(frame.metadata).unwrap();
                    assert!(binary.is_empty(), "{}", frame.name);
                }
                other => panic!("unknown fixture frame kind {other}"),
            }
        }
        for frame in fixture.invalid_metadata {
            let binary = decode_hex(&frame.binary_hex);
            assert!(
                validate_request_metadata(frame.metadata, &binary).is_err(),
                "{}",
                frame.name
            );
        }
    }

    fn decode_hex(value: &str) -> Vec<u8> {
        assert!(value.len().is_multiple_of(2));
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let pair = std::str::from_utf8(pair).unwrap();
                u8::from_str_radix(pair, 16).unwrap()
            })
            .collect()
    }

    #[allow(dead_code)]
    fn assert_frame_is_send(_: RasrFrame) {}
}
