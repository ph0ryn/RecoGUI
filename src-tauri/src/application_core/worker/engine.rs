use std::{thread, time::Instant};

use qwen3_asr_mlx::{Error as AsrError, Qwen3ASR, SamplingConfig, TranscriptionOutput};
use tokio::sync::{mpsc, oneshot};

use super::{
    ModelLoadResult, ModelUnloadResult, ModelsListResult, SegmentTranscribeRequest,
    SegmentTranscriptionResult, ShutdownResult, model_catalog::ModelCatalog,
};
use crate::application_core::{domain::TranscriptionDiagnostics, error::CoreError};

const DRIVER_CHANNEL_CAPACITY: usize = 16;
const ASR_CHUNK_SAMPLES: usize = 30 * 16_000;

enum DriverCommand {
    ListModels(oneshot::Sender<Result<ModelsListResult, CoreError>>),
    LoadModel {
        repo_id: String,
        revision: String,
        reply: oneshot::Sender<Result<ModelLoadResult, CoreError>>,
    },
    Transcribe {
        request: SegmentTranscribeRequest,
        samples: Vec<f32>,
        reply: oneshot::Sender<Result<SegmentTranscriptionResult, CoreError>>,
    },
    UnloadModel(oneshot::Sender<Result<ModelUnloadResult, CoreError>>),
    Shutdown(oneshot::Sender<Result<ShutdownResult, CoreError>>),
}

/// Owns one MLX model on a dedicated native thread. MLX arrays stay on that thread.
pub struct AsrEngine {
    sender: mpsc::Sender<DriverCommand>,
    task: Option<thread::JoinHandle<()>>,
}

impl AsrEngine {
    pub async fn launch() -> Result<Self, CoreError> {
        let (sender, receiver) = mpsc::channel(DRIVER_CHANNEL_CAPACITY);
        let task = thread::Builder::new()
            .name("reco-asr".into())
            .spawn(move || run_engine(receiver))?;
        Ok(Self {
            sender,
            task: Some(task),
        })
    }

    pub async fn list_models(&self) -> Result<ModelsListResult, CoreError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(DriverCommand::ListModels(reply))
            .await
            .map_err(|_| CoreError::WorkerClosed)?;
        response.await.map_err(|_| CoreError::WorkerClosed)?
    }

    pub async fn load_model(
        &self,
        repo_id: impl Into<String>,
        revision: impl Into<String>,
    ) -> Result<ModelLoadResult, CoreError> {
        let repo_id = repo_id.into();
        let revision = revision.into();
        if repo_id.trim().is_empty() || revision.trim().is_empty() {
            return Err(CoreError::InvalidArgument(
                "model repository and revision are required".into(),
            ));
        }
        let (reply, response) = oneshot::channel();
        self.sender
            .send(DriverCommand::LoadModel {
                repo_id,
                revision,
                reply,
            })
            .await
            .map_err(|_| CoreError::WorkerClosed)?;
        response.await.map_err(|_| CoreError::WorkerClosed)?
    }

    pub async fn transcribe_segment(
        &self,
        request: &SegmentTranscribeRequest,
        samples: &[f32],
    ) -> Result<SegmentTranscriptionResult, CoreError> {
        request.validate_samples(samples)?;
        let (reply, response) = oneshot::channel();
        self.sender
            .send(DriverCommand::Transcribe {
                request: request.clone(),
                samples: samples.to_vec(),
                reply,
            })
            .await
            .map_err(|_| CoreError::WorkerClosed)?;
        response.await.map_err(|_| CoreError::WorkerClosed)?
    }

    pub async fn unload_model(&self) -> Result<ModelUnloadResult, CoreError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(DriverCommand::UnloadModel(reply))
            .await
            .map_err(|_| CoreError::WorkerClosed)?;
        response.await.map_err(|_| CoreError::WorkerClosed)?
    }

    pub async fn shutdown(mut self) -> Result<ShutdownResult, CoreError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(DriverCommand::Shutdown(reply))
            .await
            .map_err(|_| CoreError::WorkerClosed)?;
        let result = response.await.map_err(|_| CoreError::WorkerClosed)?;
        if let Some(task) = self.task.take() {
            tokio::task::spawn_blocking(move || task.join())
                .await
                .map_err(|error| CoreError::BlockingTask(error.to_string()))?
                .map_err(|_| CoreError::WorkerExited("native ASR thread panicked".into()))?;
        }
        result
    }
}

fn run_engine(mut receiver: mpsc::Receiver<DriverCommand>) {
    let mut loaded: Option<(String, String, Qwen3ASR)> = None;
    while let Some(command) = receiver.blocking_recv() {
        match command {
            DriverCommand::ListModels(reply) => {
                let result = ModelCatalog::scan().map(|catalog| ModelsListResult {
                    models: catalog.models(),
                });
                let _ = reply.send(result);
            }
            DriverCommand::LoadModel {
                repo_id,
                revision,
                reply,
            } => {
                let reused = loaded
                    .as_ref()
                    .is_some_and(|(repo, hash, _)| repo == &repo_id && hash == &revision);
                let result = if reused {
                    Ok(ModelLoadResult {
                        repo_id,
                        revision,
                        load_ms: 0,
                        reused: true,
                    })
                } else {
                    drop(loaded.take());
                    load_model(&repo_id, &revision).map(|(model, load_ms)| {
                        loaded = Some((repo_id.clone(), revision.clone(), model));
                        ModelLoadResult {
                            repo_id,
                            revision,
                            load_ms,
                            reused: false,
                        }
                    })
                };
                let _ = reply.send(result);
            }
            DriverCommand::Transcribe {
                request,
                samples,
                reply,
            } => {
                let result = loaded
                    .as_mut()
                    .ok_or_else(|| CoreError::WorkerUnavailable("no ASR model is loaded".into()))
                    .and_then(|(_, _, model)| transcribe(model, &request, &samples));
                let _ = reply.send(result);
            }
            DriverCommand::UnloadModel(reply) => {
                let _ = reply.send(Ok(ModelUnloadResult {
                    unloaded: loaded.take().is_some(),
                }));
            }
            DriverCommand::Shutdown(reply) => {
                drop(loaded.take());
                let _ = reply.send(Ok(ShutdownResult {}));
                break;
            }
        }
    }
}

fn load_model(repo_id: &str, revision: &str) -> Result<(Qwen3ASR, u64), CoreError> {
    let catalog = ModelCatalog::scan()?;
    let path = catalog.resolve(repo_id, revision).ok_or_else(|| {
        CoreError::WorkerUnavailable(format!(
            "Qwen3-ASR revision {repo_id}@{revision} is not cached"
        ))
    })?;
    let started = Instant::now();
    let model = Qwen3ASR::load(path).map_err(|error| CoreError::WorkerResponse {
        code: "modelLoadFailure".into(),
        message: error.to_string(),
        recoverable: false,
    })?;
    Ok((model, elapsed_ms(started)))
}

fn transcribe(
    model: &mut Qwen3ASR,
    request: &SegmentTranscribeRequest,
    samples: &[f32],
) -> Result<SegmentTranscriptionResult, CoreError> {
    let started = Instant::now();
    let mut max_tokens = 0;
    let mut generation_tokens = 0;
    let mut prompt_tokens = 0;
    let mut retry_count = 0;
    let mut token_limit_reached = false;
    let mut warning = None;
    let mut raw_text = String::new();
    let mut languages = Vec::<String>::new();
    for chunk in samples.chunks(ASR_CHUNK_SAMPLES) {
        let padded = if chunk.len() < 1_600 && chunk.len() != samples.len() {
            let mut padded = chunk.to_vec();
            padded.resize(1_600, 0.0);
            Some(padded)
        } else {
            None
        };
        let chunk = padded.as_deref().unwrap_or(chunk);
        let (output, budget, retries, chunk_warning) = transcribe_chunk(model, request, chunk)?;
        max_tokens += budget;
        generation_tokens += output.generation_tokens;
        prompt_tokens += output.prompt_tokens;
        retry_count += retries;
        token_limit_reached |= output.token_limit_reached;
        warning = warning.or(chunk_warning);
        if !languages
            .iter()
            .any(|language| language == &output.language)
        {
            languages.push(output.language);
        }
        raw_text.push_str(&output.text);
    }
    if languages.is_empty() {
        return Err(CoreError::WorkerResponse {
            code: "transcriptionFailure".into(),
            message: "audio segment is too short to transcribe".into(),
            recoverable: true,
        });
    }
    if warning.is_none() && token_limit_reached {
        warning = Some("token_limit_reached".into());
    }
    let text = raw_text.trim().to_owned();
    if text.is_empty() {
        warning = Some("empty_text".into());
    }
    Ok(SegmentTranscriptionResult {
        session_id: request.session_id.clone(),
        run_id: request.run_id.clone(),
        job_id: request.job_id.clone(),
        segment_index: request.segment_index,
        text,
        raw_text,
        language: languages.join(", "),
        diagnostics: TranscriptionDiagnostics {
            max_tokens,
            generation_tokens: Some(generation_tokens),
            prompt_tokens: Some(prompt_tokens),
            total_tokens: Some(generation_tokens + prompt_tokens),
            model_total_time_ms: Some(elapsed_ms(started)),
            retry_count,
            token_limit_reached,
            warning,
        },
    })
}

fn transcribe_chunk(
    model: &mut Qwen3ASR,
    request: &SegmentTranscribeRequest,
    samples: &[f32],
) -> Result<(TranscriptionOutput, u32, u32, Option<String>), CoreError> {
    let seconds = samples.len() as f32 / request.sample_rate as f32;
    let estimated = (seconds * request.options.generation_tokens_per_second) as u32;
    let mut max_tokens = estimated.clamp(
        request.options.min_generation_tokens,
        request.options.max_generation_tokens,
    );
    let mut retry_count = 0;
    let mut warning = None;
    let mut output = generate(model, request, samples, max_tokens)?;
    if output.token_limit_reached && max_tokens < request.options.max_generation_tokens {
        let retry_max_tokens = max_tokens
            .saturating_mul(2)
            .min(request.options.max_generation_tokens);
        retry_count = 1;
        match generate(model, request, samples, retry_max_tokens) {
            Ok(retry) if !retry.text.trim().is_empty() => {
                output = retry;
                max_tokens = retry_max_tokens;
            }
            Ok(_) => warning = Some("token_limit_retry_empty".into()),
            Err(CoreError::WorkerResponse {
                recoverable: true, ..
            }) => {
                warning = Some("token_limit_retry_failed".into());
            }
            Err(error) => return Err(error),
        }
    }
    Ok((output, max_tokens, retry_count, warning))
}

fn generate(
    model: &mut Qwen3ASR,
    request: &SegmentTranscribeRequest,
    samples: &[f32],
    max_tokens: u32,
) -> Result<TranscriptionOutput, CoreError> {
    model
        .transcribe_samples_detailed(
            samples,
            request.language.as_deref(),
            &SamplingConfig {
                temperature: request.options.temperature,
                max_tokens: max_tokens as usize,
                repetition_penalty: request.options.repetition_penalty,
            },
        )
        .map_err(|error| {
            let recoverable = matches!(
                error,
                AsrError::InvalidOutput(_) | AsrError::AudioTooShort { .. }
            );
            CoreError::WorkerResponse {
                code: "transcriptionFailure".into(),
                message: error.to_string(),
                recoverable,
            }
        })
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}
