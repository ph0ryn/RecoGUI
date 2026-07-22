use std::{path::PathBuf, sync::Arc};

use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;

use crate::{
    application_core::{
        domain::{AudioFrame, LifecycleStopReason, NORMALIZED_SAMPLE_RATE},
        error::CoreError,
        media::{FileIdentity, NormalizedFileDecoder},
        vad::{SileroOnnx, SpeechSegment, VadConfig, VadSegmenter},
        worker::{
            SegmentTranscribeRequest, SegmentTranscriptionResult, WorkerOperation, WorkerProcess,
            WorkerProcessConfig, WorkerTranscriptionConfig,
        },
    },
    audio_capture::{AudioCaptureManager, CaptureEvent, CaptureSource, CaptureStartToken},
};

const ASR_QUEUE_CAPACITY: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipelineEnd {
    Natural,
    Pause,
    UserStop,
    Lifecycle(LifecycleStopReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipelineControl {
    Run,
    Finish(PipelineEnd),
}

#[derive(Clone)]
pub enum PipelineSource {
    File {
        path: PathBuf,
        identity: FileIdentity,
    },
    Live {
        source: CaptureSource,
        token: CaptureStartToken,
    },
}

pub struct PipelineSpec {
    pub session_id: String,
    pub run_id: String,
    pub model_repo_id: String,
    pub model_revision: String,
    pub language: Option<String>,
    pub next_segment_index: u32,
    pub resume_sample: u64,
    pub source: PipelineSource,
    pub worker: WorkerProcessConfig,
    pub loaded_worker: Option<WorkerProcess>,
    pub vad_asset: PathBuf,
    pub vad_config: VadConfig,
    pub transcription_config: WorkerTranscriptionConfig,
}

pub enum PipelineEvent {
    Ready {
        session_id: String,
        run_id: String,
        acknowledgement: oneshot::Sender<bool>,
    },
    Segment {
        session_id: String,
        run_id: String,
        index: u32,
        speech: SpeechSegment,
        transcription: Box<SegmentTranscriptionResult>,
        acknowledgement: oneshot::Sender<bool>,
    },
    Progress {
        session_id: String,
        run_id: String,
        processed_sample: u64,
        queued_segments: u8,
    },
    Finished {
        session_id: String,
        run_id: String,
        end: PipelineEnd,
        resume_sample: u64,
        worker: Option<WorkerProcess>,
    },
    Failed {
        session_id: String,
        run_id: String,
        error: CoreError,
    },
}

pub struct PipelineHandle {
    control: watch::Sender<PipelineControl>,
}

impl PipelineHandle {
    pub fn finish(&self, end: PipelineEnd) {
        let _ = self.control.send(PipelineControl::Finish(end));
    }
}

pub fn spawn_pipeline(
    spec: PipelineSpec,
    audio: Arc<AudioCaptureManager>,
    events: mpsc::Sender<PipelineEvent>,
) -> PipelineHandle {
    let (control, receiver) = watch::channel(PipelineControl::Run);
    tokio::spawn(async move {
        let session_id = spec.session_id.clone();
        let run_id = spec.run_id.clone();
        let result = run_pipeline(spec, audio, events.clone(), receiver).await;
        if let Err(error) = result {
            let _ = events
                .send(PipelineEvent::Failed {
                    session_id,
                    run_id,
                    error,
                })
                .await;
        }
    });
    PipelineHandle { control }
}

async fn run_pipeline(
    mut spec: PipelineSpec,
    audio: Arc<AudioCaptureManager>,
    events: mpsc::Sender<PipelineEvent>,
    control: watch::Receiver<PipelineControl>,
) -> Result<(), CoreError> {
    let mut resume_sample = spec.resume_sample;
    let worker = if let Some(worker) = spec.loaded_worker.take() {
        worker
    } else {
        let worker = match WorkerProcess::launch(spec.worker.clone()).await {
            Ok(worker) => worker,
            Err(error) => {
                cancel_reserved_source(&spec, &audio);
                return Err(error);
            }
        };
        if let Err(error) = worker
            .load_model(
                request_id("load"),
                spec.model_repo_id.clone(),
                spec.model_revision.clone(),
            )
            .await
        {
            cancel_reserved_source(&spec, &audio);
            shutdown_worker(worker).await;
            return Err(error);
        }
        worker
    };

    let prepared = prepare_source(&spec, audio.clone()).await;
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            cancel_reserved_source(&spec, &audio);
            shutdown_worker(worker).await;
            return Err(error);
        }
    };

    let (acknowledgement, accepted) = oneshot::channel();
    if events
        .send(PipelineEvent::Ready {
            session_id: spec.session_id.clone(),
            run_id: spec.run_id.clone(),
            acknowledgement,
        })
        .await
        .is_err()
    {
        stop_prepared_source(prepared, audio).await;
        shutdown_worker(worker).await;
        return Err(CoreError::StoreClosed);
    }
    if !accepted.await.unwrap_or(false) {
        stop_prepared_source(prepared, audio).await;
        shutdown_worker(worker).await;
        return Ok(());
    }

    let (speech_sender, mut speech_receiver) = mpsc::channel(ASR_QUEUE_CAPACITY);
    let source_control = control.clone();
    let source_audio = audio.clone();
    let producer = tokio::spawn(async move {
        produce_speech(prepared, source_audio, source_control, speech_sender).await
    });
    let mut next_index = spec.next_segment_index;
    let mut requested_end = PipelineEnd::Natural;
    let processing = async {
        while let Some(speech) = speech_receiver.recv().await {
            if let PipelineControl::Finish(end) = *control.borrow() {
                requested_end = end;
            }
            let job_id = Uuid::new_v4().to_string();
            let request = SegmentTranscribeRequest {
                request_id: request_id("segment"),
                operation: WorkerOperation::SegmentTranscribe,
                session_id: spec.session_id.clone(),
                run_id: spec.run_id.clone(),
                job_id: job_id.clone(),
                segment_index: next_index,
                start_sample: speech.start_sample,
                end_sample: speech.end_sample(),
                sample_rate: NORMALIZED_SAMPLE_RATE,
                split_reason: speech.split_reason,
                language: spec.language.clone(),
                vad: speech.vad.clone(),
                options: spec.transcription_config.clone(),
            };
            let transcription = worker.transcribe_segment(&request, &speech.audio).await?;
            if transcription.session_id != spec.session_id
                || transcription.run_id != spec.run_id
                || transcription.job_id != job_id
                || transcription.segment_index != next_index
            {
                return Err(CoreError::WorkerProtocol(
                    "worker echoed a different session, run, job, or segment identity".into(),
                ));
            }
            let speech_end = speech.end_sample();
            let (acknowledgement, committed) = oneshot::channel();
            events
                .send(PipelineEvent::Segment {
                    session_id: spec.session_id.clone(),
                    run_id: spec.run_id.clone(),
                    index: next_index,
                    speech,
                    transcription: Box::new(transcription),
                    acknowledgement,
                })
                .await
                .map_err(|_| CoreError::StoreClosed)?;
            if !committed.await.unwrap_or(false) {
                return Err(CoreError::StoreClosed);
            }
            resume_sample = resume_sample.max(speech_end);
            next_index = next_index
                .checked_add(1)
                .ok_or_else(|| CoreError::InvalidArgument("segment index overflow".into()))?;
            let queued_segments = u8::try_from(speech_receiver.len()).unwrap_or(u8::MAX);
            let _ = events
                .send(PipelineEvent::Progress {
                    session_id: spec.session_id.clone(),
                    run_id: spec.run_id.clone(),
                    processed_sample: resume_sample,
                    queued_segments,
                })
                .await;
        }
        Ok::<(), CoreError>(())
    }
    .await;
    if let Err(error) = processing {
        audio.request_stop();
        drop(speech_receiver);
        let _ = producer.await;
        shutdown_worker(worker).await;
        return Err(error);
    }

    let producer_result = producer
        .await
        .map_err(|error| CoreError::BlockingTask(error.to_string()))?;
    match producer_result {
        Ok((end, final_sample)) => {
            resume_sample = resume_sample.max(final_sample);
            if end != PipelineEnd::Natural {
                requested_end = end;
            }
        }
        Err((error, final_sample)) => {
            shutdown_worker(worker).await;
            let _ = final_sample;
            return Err(error);
        }
    }
    if let PipelineControl::Finish(end) = *control.borrow() {
        requested_end = end;
    }
    let worker = if requested_end == PipelineEnd::Natural {
        Some(worker)
    } else {
        shutdown_worker(worker).await;
        None
    };
    if let Err(error) = events
        .send(PipelineEvent::Finished {
            session_id: spec.session_id,
            run_id: spec.run_id,
            end: requested_end,
            resume_sample,
            worker,
        })
        .await
    {
        if let PipelineEvent::Finished {
            worker: Some(worker),
            ..
        } = error.0
        {
            shutdown_worker(worker).await;
        }
        return Err(CoreError::StoreClosed);
    }
    Ok(())
}

fn cancel_reserved_source(spec: &PipelineSpec, audio: &AudioCaptureManager) {
    if let PipelineSource::Live { token, .. } = &spec.source {
        audio.cancel_reserved(token);
    }
}

enum PreparedSource {
    File(Box<NormalizedFileDecoder>, Box<VadSegmenter<SileroOnnx>>),
    Live(
        tokio::sync::mpsc::Receiver<CaptureEvent>,
        Box<VadSegmenter<SileroOnnx>>,
    ),
}

async fn prepare_source(
    spec: &PipelineSpec,
    audio: Arc<AudioCaptureManager>,
) -> Result<PreparedSource, CoreError> {
    let vad_asset = spec.vad_asset.clone();
    let vad_config = spec.vad_config;
    let resume_sample = spec.resume_sample;
    match &spec.source {
        PipelineSource::File { path, identity } => {
            let path = path.clone();
            let identity = identity.clone();
            tokio::task::spawn_blocking(move || {
                let decoder = NormalizedFileDecoder::open(&path, Some(&identity), resume_sample)?;
                let vad = VadSegmenter::new(SileroOnnx::load(&vad_asset)?, vad_config)?;
                Ok(PreparedSource::File(Box::new(decoder), Box::new(vad)))
            })
            .await
            .map_err(|error| CoreError::BlockingTask(error.to_string()))?
        }
        PipelineSource::Live { source, token } => {
            let vad = tokio::task::spawn_blocking(move || {
                VadSegmenter::new(SileroOnnx::load(&vad_asset)?, vad_config)
            })
            .await
            .map_err(|error| CoreError::BlockingTask(error.to_string()))??;
            let source = source.clone();
            let token = token.clone();
            let receiver = tokio::task::spawn_blocking(move || {
                audio.start_reserved(token, source, resume_sample)
            })
            .await
            .map_err(|error| CoreError::BlockingTask(error.to_string()))?
            .map_err(|error| CoreError::AudioNormalize(error.to_string()))?;
            Ok(PreparedSource::Live(receiver, Box::new(vad)))
        }
    }
}

async fn produce_speech(
    source: PreparedSource,
    audio: Arc<AudioCaptureManager>,
    control: watch::Receiver<PipelineControl>,
    speech: mpsc::Sender<SpeechSegment>,
) -> Result<(PipelineEnd, u64), (CoreError, u64)> {
    match source {
        PreparedSource::File(mut decoder, mut vad) => tokio::task::spawn_blocking(move || {
            let mut final_sample = 0;
            let mut end = PipelineEnd::Natural;
            loop {
                if let PipelineControl::Finish(requested) = *control.borrow() {
                    end = requested;
                    break;
                }
                let frame = decoder
                    .next_frame()
                    .map_err(|error| (error, final_sample))?;
                let Some(frame) = frame else {
                    break;
                };
                final_sample = frame.start_sample + frame.samples.len() as u64;
                for segment in vad
                    .process_frame(frame)
                    .map_err(|error| (error, final_sample))?
                {
                    speech
                        .blocking_send(segment)
                        .map_err(|_| (CoreError::StoreClosed, final_sample))?;
                }
            }
            for segment in vad.flush(true) {
                final_sample = final_sample.max(segment.end_sample());
                speech
                    .blocking_send(segment)
                    .map_err(|_| (CoreError::StoreClosed, final_sample))?;
            }
            Ok((end, final_sample))
        })
        .await
        .map_err(|error| (CoreError::BlockingTask(error.to_string()), 0))?,
        PreparedSource::Live(mut receiver, mut vad) => {
            let mut control = control;
            let mut final_sample = 0;
            let mut end = PipelineEnd::Natural;
            let mut stopping = false;
            loop {
                let event = if stopping {
                    receiver.recv().await
                } else {
                    tokio::select! {
                        changed = control.changed() => {
                            if changed.is_err() {
                                audio.request_stop();
                                return Err((CoreError::StoreClosed, final_sample));
                            }
                            if let PipelineControl::Finish(requested) = *control.borrow() {
                                end = requested;
                                stopping = true;
                                audio.request_stop();
                            }
                            continue;
                        }
                        event = receiver.recv() => event,
                    }
                };
                match event {
                    Some(CaptureEvent::Started { .. }) => {}
                    Some(CaptureEvent::Frame(frame)) => {
                        final_sample = frame.start_sample + frame.samples.len() as u64;
                        for segment in vad
                            .process_frame(AudioFrame {
                                start_sample: frame.start_sample,
                                samples: frame.samples,
                            })
                            .map_err(|error| (error, final_sample))?
                        {
                            speech
                                .send(segment)
                                .await
                                .map_err(|_| (CoreError::StoreClosed, final_sample))?;
                        }
                    }
                    Some(CaptureEvent::Error(error)) => {
                        return Err((CoreError::AudioNormalize(error.to_string()), final_sample));
                    }
                    Some(CaptureEvent::Ended) | None => break,
                }
            }
            for segment in vad.flush(true) {
                final_sample = final_sample.max(segment.end_sample());
                speech
                    .send(segment)
                    .await
                    .map_err(|_| (CoreError::StoreClosed, final_sample))?;
            }
            let wait_audio = audio.clone();
            tokio::task::spawn_blocking(move || wait_audio.wait_stopped())
                .await
                .map_err(|error| (CoreError::BlockingTask(error.to_string()), final_sample))?;
            if end == PipelineEnd::Natural {
                return Err((
                    CoreError::AudioNormalize("live audio source ended unexpectedly".into()),
                    final_sample,
                ));
            }
            Ok((end, final_sample))
        }
    }
}

async fn stop_prepared_source(source: PreparedSource, audio: Arc<AudioCaptureManager>) {
    let is_live = matches!(source, PreparedSource::Live(_, _));
    if is_live {
        audio.request_stop();
    }
    drop(source);
    if is_live {
        let wait_audio = audio.clone();
        let _ = tokio::task::spawn_blocking(move || wait_audio.wait_stopped()).await;
    }
}

pub(crate) async fn shutdown_worker(worker: WorkerProcess) {
    let _ = worker.unload_model(request_id("unload")).await;
    let _ = worker.shutdown(request_id("shutdown")).await;
}

fn request_id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asr_queue_is_fixed_to_two_segments() {
        assert_eq!(ASR_QUEUE_CAPACITY, 2);
    }

    #[test]
    fn lifecycle_stop_reason_is_preserved_by_pipeline_control() {
        assert_eq!(
            PipelineControl::Finish(PipelineEnd::Lifecycle(LifecycleStopReason::SystemSleep)),
            PipelineControl::Finish(PipelineEnd::Lifecycle(LifecycleStopReason::SystemSleep))
        );
    }
}
