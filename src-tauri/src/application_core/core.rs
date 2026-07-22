use std::{collections::HashMap, path::PathBuf, sync::Arc};

use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::{
    api_types as api,
    app_events::EventSink,
    application_core::{
        config::{RuntimePipelineConfig, default_pipeline_config, parse_pipeline_config},
        contract,
        domain::{
            DeleteSession, HistoryQuery as StoreHistoryQuery, LifecycleStopReason, NewQueueItem,
            NewSegment, NewSession, PendingQueueSource, SelectedModel, SessionState, SourceKind,
        },
        error::CoreError,
        media::{FileFingerprint, fingerprint_file},
        pipeline::{
            PipelineEnd, PipelineEvent, PipelineHandle, PipelineSource, PipelineSpec,
            shutdown_worker, spawn_pipeline,
        },
        store::Store,
        worker::{CachedModel, WorkerProcess, WorkerProcessConfig},
    },
    audio_capture::{AudioCaptureManager, CaptureSource},
    native_export::{
        CancellationToken, ExportError, ExportSegment, ExportSession, ExportStage,
        export_sessions_with_progress, render_sessions_for_clipboard,
    },
};

const CORE_CHANNEL_CAPACITY: usize = 64;
const PIPELINE_EVENT_CAPACITY: usize = 8;
const SNAPSHOT_HISTORY_LIMIT: u32 = 100;
const SEGMENT_PAGE_LIMIT: u32 = 500;

#[derive(Clone, Debug)]
pub struct ApplicationCoreConfig {
    pub database_path: PathBuf,
    pub worker: WorkerProcessConfig,
    pub vad_asset: PathBuf,
}

#[derive(Clone)]
pub struct ApplicationCore {
    sender: mpsc::Sender<CoreCommand>,
    audio: Arc<AudioCaptureManager>,
}

impl ApplicationCore {
    pub async fn start(
        config: ApplicationCoreConfig,
        events: Arc<dyn EventSink>,
    ) -> Result<Self, CoreError> {
        let store = Store::open(&config.database_path)?;
        store.recover_startup().await?;
        let selected_model = store.selected_model().await?;
        let (sender, receiver) = mpsc::channel(CORE_CHANNEL_CAPACITY);
        let (pipeline_events, mut pipeline_receiver) = mpsc::channel(PIPELINE_EVENT_CAPACITY);
        let forward = sender.clone();
        tokio::spawn(async move {
            while let Some(event) = pipeline_receiver.recv().await {
                if forward
                    .send(CoreCommand::Pipeline(Box::new(event)))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let audio = Arc::new(AudioCaptureManager::default());
        let actor = CoreActor {
            store,
            worker_config: config.worker,
            vad_asset: config.vad_asset,
            audio: audio.clone(),
            events,
            sender: sender.clone(),
            pipeline_events,
            sequence: 0,
            active: None,
            resume_preparation: None,
            auto_advance: false,
            queue_language: None,
            queue_preparation: None,
            selected_model,
            model_state: None,
            cached_models: Vec::new(),
            model_list_reply: None,
            exports: HashMap::new(),
            reusable_worker: None,
            worker_cleanups: 0,
            shutdown: None,
        };
        tokio::spawn(actor.run(receiver));
        Ok(Self { sender, audio })
    }

    pub async fn app_snapshot(&self) -> Result<api::AppSnapshot, CoreError> {
        self.request(|reply| CoreCommand::Snapshot { reply }).await
    }

    pub async fn model_list(&self) -> Result<api::ModelList, CoreError> {
        self.request(|reply| CoreCommand::ModelList { reply }).await
    }

    pub async fn model_select(
        &self,
        model: api::ModelReference,
    ) -> Result<api::ModelState, CoreError> {
        self.request(|reply| CoreCommand::ModelSelect { model, reply })
            .await
    }

    pub async fn start_live(
        &self,
        input: api::StartLiveSession,
    ) -> Result<api::SessionDetail, CoreError> {
        let source = match &input.source {
            api::LiveSource::Microphone { device_id } => CaptureSource::Microphone {
                device_id: device_id.clone(),
            },
            api::LiveSource::SystemAudio => CaptureSource::SystemAudio,
        };
        let preflight_audio = self.audio.clone();
        let preflight_source = source.clone();
        let source =
            tokio::task::spawn_blocking(move || preflight_audio.resolve_source(&preflight_source))
                .await
                .map_err(|error| CoreError::BlockingTask(error.to_string()))?
                .map_err(|error| CoreError::AudioNormalize(error.to_string()))?;
        let token = self
            .audio
            .reserve_start()
            .map_err(|error| CoreError::AudioNormalize(error.to_string()))?;
        let cancellation_token = token.clone();
        let result = self
            .request(|reply| CoreCommand::StartLive {
                input,
                source,
                token,
                reply,
            })
            .await;
        if result.is_err() {
            self.audio.cancel_reserved(&cancellation_token);
        }
        result
    }

    pub async fn pause_session(
        &self,
        input: api::SessionMutation,
    ) -> Result<api::SessionDetail, CoreError> {
        self.request(|reply| CoreCommand::FinishSession {
            input,
            end: PipelineEnd::Pause,
            reply,
        })
        .await
    }

    pub async fn stop_session(
        &self,
        input: api::SessionMutation,
    ) -> Result<api::SessionDetail, CoreError> {
        self.request(|reply| CoreCommand::FinishSession {
            input,
            end: PipelineEnd::UserStop,
            reply,
        })
        .await
    }

    pub async fn resume_session(
        &self,
        input: api::SessionMutation,
    ) -> Result<api::SessionDetail, CoreError> {
        self.request(|reply| CoreCommand::ResumeSession { input, reply })
            .await
    }

    pub async fn enqueue_files(
        &self,
        paths: Vec<PathBuf>,
        language: Option<String>,
    ) -> Result<api::QueueSnapshot, CoreError> {
        let items = tokio::task::spawn_blocking(move || prepare_queue_items(paths))
            .await
            .map_err(|error| CoreError::BlockingTask(error.to_string()))??;
        self.request(|reply| CoreCommand::EnqueueFiles {
            items,
            language,
            reply,
        })
        .await
    }

    pub async fn queue_snapshot(&self) -> Result<api::QueueSnapshot, CoreError> {
        self.request(|reply| CoreCommand::QueueSnapshot { reply })
            .await
    }

    pub async fn reorder_queue(
        &self,
        input: api::QueueReorder,
    ) -> Result<api::QueueSnapshot, CoreError> {
        self.request(|reply| CoreCommand::ReorderQueue { input, reply })
            .await
    }

    pub async fn remove_queue_item(
        &self,
        input: api::QueueRemove,
    ) -> Result<api::QueueSnapshot, CoreError> {
        self.request(|reply| CoreCommand::RemoveQueueItem { input, reply })
            .await
    }

    pub async fn clear_queue(
        &self,
        input: api::QueueRevision,
    ) -> Result<api::QueueSnapshot, CoreError> {
        self.request(|reply| CoreCommand::ClearQueue { input, reply })
            .await
    }

    pub async fn start_queue(
        &self,
        input: api::QueueStart,
    ) -> Result<api::QueueSnapshot, CoreError> {
        self.request(|reply| CoreCommand::StartQueue { input, reply })
            .await
    }

    pub async fn pause_queue(&self) -> Result<api::QueueSnapshot, CoreError> {
        self.request(|reply| CoreCommand::PauseQueue { reply })
            .await
    }

    pub async fn history(&self, input: api::HistoryQuery) -> Result<api::HistoryPage, CoreError> {
        self.request(|reply| CoreCommand::History { input, reply })
            .await
    }

    pub async fn history_get(
        &self,
        input: api::HistoryDetailQuery,
    ) -> Result<api::SessionDetail, CoreError> {
        self.request(|reply| CoreCommand::HistoryGet { input, reply })
            .await
    }

    pub async fn history_rename(
        &self,
        input: api::HistoryRename,
    ) -> Result<api::SessionSummary, CoreError> {
        self.request(|reply| CoreCommand::HistoryRename { input, reply })
            .await
    }

    pub async fn history_delete(&self, input: api::HistoryDelete) -> Result<(), CoreError> {
        self.request(|reply| CoreCommand::HistoryDelete { input, reply })
            .await
    }

    pub async fn history_render(&self, input: api::HistoryRender) -> Result<String, CoreError> {
        self.request(|reply| CoreCommand::HistoryRender { input, reply })
            .await
    }

    pub async fn export_start(
        &self,
        input: api::ExportStart,
        destination: PathBuf,
    ) -> Result<api::ExportStartResult, CoreError> {
        self.request(|reply| CoreCommand::ExportStart {
            input,
            destination,
            reply,
        })
        .await
    }

    pub async fn export_cancel(&self, operation_id: String) -> Result<(), CoreError> {
        self.request(|reply| CoreCommand::ExportCancel {
            operation_id,
            reply,
        })
        .await
    }

    pub async fn request_close(&self) -> Result<bool, CoreError> {
        self.request(|reply| CoreCommand::RequestClose { reply })
            .await
    }

    pub async fn shutdown(&self, reason: LifecycleStopReason) -> Result<(), CoreError> {
        self.request(|reply| CoreCommand::Shutdown { reason, reply })
            .await
    }

    pub async fn report_close_failure(&self, error: api::AppError) -> Result<(), CoreError> {
        self.request(|reply| CoreCommand::ReportCloseFailure { error, reply })
            .await
    }

    async fn request<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<Result<T, CoreError>>) -> CoreCommand,
    ) -> Result<T, CoreError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(build(reply))
            .await
            .map_err(|_| CoreError::StoreClosed)?;
        response.await.map_err(|_| CoreError::StoreClosed)?
    }
}

enum CoreCommand {
    Snapshot {
        reply: Reply<api::AppSnapshot>,
    },
    ModelList {
        reply: Reply<api::ModelList>,
    },
    ModelListFinished {
        result: Result<Vec<CachedModel>, CoreError>,
    },
    ModelSelect {
        model: api::ModelReference,
        reply: Reply<api::ModelState>,
    },
    StartLive {
        input: api::StartLiveSession,
        source: CaptureSource,
        token: crate::audio_capture::CaptureStartToken,
        reply: Reply<api::SessionDetail>,
    },
    FinishSession {
        input: api::SessionMutation,
        end: PipelineEnd,
        reply: Reply<api::SessionDetail>,
    },
    ResumeSession {
        input: api::SessionMutation,
        reply: Reply<api::SessionDetail>,
    },
    ResumePrepared {
        job_id: String,
        result: Result<PreparedResume, CoreError>,
    },
    EnqueueFiles {
        items: Vec<NewQueueItem>,
        language: Option<String>,
        reply: Reply<api::QueueSnapshot>,
    },
    QueueSnapshot {
        reply: Reply<api::QueueSnapshot>,
    },
    ReorderQueue {
        input: api::QueueReorder,
        reply: Reply<api::QueueSnapshot>,
    },
    RemoveQueueItem {
        input: api::QueueRemove,
        reply: Reply<api::QueueSnapshot>,
    },
    ClearQueue {
        input: api::QueueRevision,
        reply: Reply<api::QueueSnapshot>,
    },
    StartQueue {
        input: api::QueueStart,
        reply: Reply<api::QueueSnapshot>,
    },
    PauseQueue {
        reply: Reply<api::QueueSnapshot>,
    },
    History {
        input: api::HistoryQuery,
        reply: Reply<api::HistoryPage>,
    },
    HistoryGet {
        input: api::HistoryDetailQuery,
        reply: Reply<api::SessionDetail>,
    },
    HistoryRename {
        input: api::HistoryRename,
        reply: Reply<api::SessionSummary>,
    },
    HistoryDelete {
        input: api::HistoryDelete,
        reply: Reply<()>,
    },
    HistoryRender {
        input: api::HistoryRender,
        reply: Reply<String>,
    },
    ExportStart {
        input: api::ExportStart,
        destination: PathBuf,
        reply: Reply<api::ExportStartResult>,
    },
    ExportCancel {
        operation_id: String,
        reply: Reply<()>,
    },
    ExportProgress {
        operation_id: String,
        phase: api::ExportPhase,
        completed_items: u32,
        total_items: u32,
        current_session_id: Option<String>,
    },
    ExportFinished {
        operation_id: String,
        result: Result<Vec<String>, ExportTaskFailure>,
    },
    Pipeline(Box<PipelineEvent>),
    RequestClose {
        reply: Reply<bool>,
    },
    Shutdown {
        reason: LifecycleStopReason,
        reply: Reply<()>,
    },
    ReportCloseFailure {
        error: api::AppError,
        reply: Reply<()>,
    },
    WorkerReleased,
    QueuePrepared {
        job_id: String,
        candidate: PendingQueueSource,
        model: SelectedModel,
        result: Result<FileFingerprint, CoreError>,
    },
}

type Reply<T> = oneshot::Sender<Result<T, CoreError>>;

struct ActiveSession {
    session_id: String,
    run_id: String,
    row_version: u64,
    pipeline: PipelineHandle,
    resume_sample: u64,
    preparing: bool,
    pending_end: Option<PipelineEnd>,
    start_reply: Option<Reply<api::SessionDetail>>,
    finish_reply: Option<Reply<api::SessionDetail>>,
}

struct ResumePreparation {
    job_id: String,
    session_id: String,
    reply: Reply<api::SessionDetail>,
}

struct PreparedResume {
    context: crate::application_core::domain::ResumeContext,
    model_revision: String,
    source: PipelineSource,
    config: RuntimePipelineConfig,
}

struct ShutdownState {
    reason: LifecycleStopReason,
    reply: Reply<()>,
}

struct ExportTaskFailure {
    session_ids: Vec<String>,
    error: ExportError,
}

struct CoreActor {
    store: Store,
    worker_config: WorkerProcessConfig,
    vad_asset: PathBuf,
    audio: Arc<AudioCaptureManager>,
    events: Arc<dyn EventSink>,
    sender: mpsc::Sender<CoreCommand>,
    pipeline_events: mpsc::Sender<PipelineEvent>,
    sequence: u64,
    active: Option<ActiveSession>,
    resume_preparation: Option<ResumePreparation>,
    auto_advance: bool,
    queue_language: Option<String>,
    queue_preparation: Option<(String, String)>,
    selected_model: Option<SelectedModel>,
    model_state: Option<api::ModelState>,
    cached_models: Vec<CachedModel>,
    model_list_reply: Option<Reply<api::ModelList>>,
    exports: HashMap<String, CancellationToken>,
    reusable_worker: Option<WorkerProcess>,
    worker_cleanups: usize,
    shutdown: Option<ShutdownState>,
}

impl CoreActor {
    async fn run(mut self, mut receiver: mpsc::Receiver<CoreCommand>) {
        while let Some(command) = receiver.recv().await {
            self.handle(command).await;
        }
        if let Some(active) = &self.active {
            active
                .pipeline
                .finish(PipelineEnd::Lifecycle(LifecycleStopReason::AppQuit));
        }
        for cancellation in self.exports.values() {
            cancellation.cancel();
        }
    }

    async fn handle(&mut self, command: CoreCommand) {
        match command {
            CoreCommand::Snapshot { reply } => respond(reply, self.snapshot().await),
            CoreCommand::ModelList { reply } => self.start_model_list(reply),
            CoreCommand::ModelListFinished { result } => self.finish_model_list(result),
            CoreCommand::ModelSelect { model, reply } => {
                respond(reply, self.select_model(model).await)
            }
            CoreCommand::StartLive {
                input,
                source,
                token,
                reply,
            } => self.start_live(input, source, token, reply).await,
            CoreCommand::FinishSession { input, end, reply } => {
                self.finish_session(input, end, reply).await
            }
            CoreCommand::ResumeSession { input, reply } => self.start_resume(input, reply),
            CoreCommand::ResumePrepared { job_id, result } => {
                self.finish_resume_preparation(job_id, result).await;
            }
            CoreCommand::EnqueueFiles {
                items,
                language,
                reply,
            } => respond(reply, self.enqueue(items, language).await),
            CoreCommand::QueueSnapshot { reply } => respond(reply, self.queue_snapshot().await),
            CoreCommand::ReorderQueue { input, reply } => {
                respond(reply, self.reorder_queue(input).await)
            }
            CoreCommand::RemoveQueueItem { input, reply } => {
                respond(reply, self.remove_queue_item(input).await)
            }
            CoreCommand::ClearQueue { input, reply } => {
                respond(reply, self.clear_queue(input).await)
            }
            CoreCommand::StartQueue { input, reply } => {
                respond(reply, self.start_queue(input).await)
            }
            CoreCommand::PauseQueue { reply } => respond(reply, self.pause_queue().await),
            CoreCommand::History { input, reply } => {
                let store = self.store.clone();
                tokio::spawn(async move {
                    respond(reply, query_history(store, input).await);
                });
            }
            CoreCommand::HistoryGet { input, reply } => {
                let store = self.store.clone();
                tokio::spawn(async move {
                    respond(reply, get_history_detail(store, input).await);
                });
            }
            CoreCommand::HistoryRename { input, reply } => {
                respond(reply, self.history_rename(input).await)
            }
            CoreCommand::HistoryDelete { input, reply } => {
                respond(reply, self.history_delete(input).await)
            }
            CoreCommand::HistoryRender { input, reply } => {
                let store = self.store.clone();
                tokio::spawn(async move {
                    respond(reply, render_history(store, input).await);
                });
            }
            CoreCommand::ExportStart {
                input,
                destination,
                reply,
            } => respond(reply, self.start_export(input, destination).await),
            CoreCommand::ExportCancel {
                operation_id,
                reply,
            } => respond(reply, self.cancel_export(&operation_id)),
            CoreCommand::ExportProgress {
                operation_id,
                phase,
                completed_items,
                total_items,
                current_session_id,
            } => self.report_export_progress(
                operation_id,
                phase,
                completed_items,
                total_items,
                current_session_id,
            ),
            CoreCommand::ExportFinished {
                operation_id,
                result,
            } => self.finish_export(operation_id, result),
            CoreCommand::Pipeline(event) => self.handle_pipeline(*event).await,
            CoreCommand::RequestClose { reply } => respond(reply, self.request_close()),
            CoreCommand::Shutdown { reason, reply } => self.shutdown(reason, reply).await,
            CoreCommand::ReportCloseFailure { error, reply } => {
                self.report_close_failure(error);
                respond(reply, Ok(()));
            }
            CoreCommand::WorkerReleased => {
                self.worker_cleanups = self.worker_cleanups.saturating_sub(1);
                self.complete_shutdown_if_idle();
            }
            CoreCommand::QueuePrepared {
                job_id,
                candidate,
                model,
                result,
            } => {
                self.finish_queue_preparation(job_id, candidate, model, result)
                    .await;
            }
        }
    }

    fn model_state(&self) -> api::ModelState {
        self.model_state.clone().unwrap_or_else(|| api::ModelState {
            status: if self.selected_model.is_some() {
                api::ModelStatus::Checking
            } else {
                api::ModelStatus::Unselected
            },
            selected: self
                .selected_model
                .as_ref()
                .map(|model| api::ModelReference {
                    repo_id: model.repo_id.clone(),
                    revision: model.revision.clone(),
                }),
            error: None,
        })
    }

    async fn snapshot(&self) -> Result<api::AppSnapshot, CoreError> {
        let active_session = if let Some(active) = &self.active {
            Some(contract::detail(
                self.store
                    .session_snapshot(&active.session_id, 0, SEGMENT_PAGE_LIMIT)
                    .await?,
            ))
        } else {
            None
        };
        let history = contract::history_page(
            self.store
                .history(StoreHistoryQuery {
                    limit: SNAPSHOT_HISTORY_LIMIT,
                    ..StoreHistoryQuery::default()
                })
                .await?,
        )?;
        Ok(api::AppSnapshot {
            sequence: self.sequence.to_string(),
            active_session,
            history,
            queue: self.queue_snapshot().await?,
            model: self.model_state(),
        })
    }

    fn start_model_list(&mut self, reply: Reply<api::ModelList>) {
        if self.active.is_some() && !self.cached_models.is_empty() {
            respond(
                reply,
                Ok(api::ModelList {
                    models: self
                        .cached_models
                        .clone()
                        .into_iter()
                        .map(contract::cached_model)
                        .collect(),
                    state: self.model_state(),
                }),
            );
            return;
        }
        if self.model_list_reply.is_some() {
            respond(
                reply,
                Err(CoreError::InvalidState {
                    entity: "model catalog",
                    expected: "one in-flight refresh".into(),
                    actual: "refresh already in progress".into(),
                }),
            );
            return;
        }
        self.model_state = Some(api::ModelState {
            status: api::ModelStatus::Checking,
            selected: self
                .selected_model
                .as_ref()
                .map(|model| api::ModelReference {
                    repo_id: model.repo_id.clone(),
                    revision: model.revision.clone(),
                }),
            error: None,
        });
        self.emit_model();
        self.model_list_reply = Some(reply);
        let worker_config = self.worker_config.clone();
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let result = query_cached_models(worker_config).await;
            let _ = sender.send(CoreCommand::ModelListFinished { result }).await;
        });
    }

    fn finish_model_list(&mut self, result: Result<Vec<CachedModel>, CoreError>) {
        let Some(reply) = self.model_list_reply.take() else {
            return;
        };
        let models = match result {
            Ok(models) => models,
            Err(error) => {
                self.model_state = Some(api::ModelState {
                    status: api::ModelStatus::Error,
                    selected: self.model_state().selected,
                    error: Some(contract::app_error(&error)),
                });
                self.emit_model();
                respond(reply, Err(error));
                self.complete_shutdown_if_idle();
                return;
            }
        };
        self.cached_models = models;
        let selected = self
            .selected_model
            .as_ref()
            .map(|model| api::ModelReference {
                repo_id: model.repo_id.clone(),
                revision: model.revision.clone(),
            });
        let selected_available = selected.as_ref().is_some_and(|selected| {
            self.cached_models.iter().any(|candidate| {
                candidate.repo_id == selected.repo_id && candidate.revision == selected.revision
            })
        });
        self.model_state = Some(api::ModelState {
            status: match (&selected, selected_available) {
                (None, _) => api::ModelStatus::Unselected,
                (Some(_), true) => api::ModelStatus::Ready,
                (Some(_), false) => api::ModelStatus::Unavailable,
            },
            selected,
            error: None,
        });
        self.emit_model();
        respond(
            reply,
            Ok(api::ModelList {
                models: self
                    .cached_models
                    .clone()
                    .into_iter()
                    .map(contract::cached_model)
                    .collect(),
                state: self.model_state(),
            }),
        );
        self.complete_shutdown_if_idle();
    }

    async fn select_model(
        &mut self,
        model: api::ModelReference,
    ) -> Result<api::ModelState, CoreError> {
        if self.active.is_some() || self.resume_preparation.is_some() {
            return Err(CoreError::InvalidState {
                entity: "session",
                expected: "idle before changing model".into(),
                actual: "active or resume preparation in progress".into(),
            });
        }
        if self.auto_advance || self.queue_preparation.is_some() || self.reusable_worker.is_some() {
            return Err(CoreError::InvalidState {
                entity: "queue",
                expected: "paused before changing model".into(),
                actual: "auto advance is active".into(),
            });
        }
        if self.cached_models.is_empty() {
            return Err(CoreError::InvalidState {
                entity: "model catalog",
                expected: "models.list before model.select".into(),
                actual: "catalog has not been loaded".into(),
            });
        }
        if !self.cached_models.iter().any(|candidate| {
            candidate.repo_id == model.repo_id && candidate.revision == model.revision
        }) {
            return Err(CoreError::WorkerUnavailable(format!(
                "selected model revision is not cached: {}@{}",
                model.repo_id, model.revision
            )));
        }
        let selected = SelectedModel {
            repo_id: model.repo_id.clone(),
            revision: model.revision.clone(),
        };
        self.store.set_selected_model(selected.clone()).await?;
        self.selected_model = Some(selected);
        self.model_state = Some(api::ModelState {
            status: api::ModelStatus::Ready,
            selected: Some(model),
            error: None,
        });
        self.emit_model();
        Ok(self.model_state())
    }

    async fn start_live(
        &mut self,
        input: api::StartLiveSession,
        source: CaptureSource,
        token: crate::audio_capture::CaptureStartToken,
        reply: Reply<api::SessionDetail>,
    ) {
        let Some(model) = self.selected_model.clone() else {
            respond(
                reply,
                Err(CoreError::WorkerUnavailable("no model is selected".into())),
            );
            return;
        };
        if self.model_state().status != api::ModelStatus::Ready {
            respond(
                reply,
                Err(CoreError::WorkerUnavailable(
                    "the selected model revision has not been verified as available".into(),
                )),
            );
            return;
        }
        if self.active.is_some() || self.resume_preparation.is_some() {
            respond(
                reply,
                Err(CoreError::InvalidState {
                    entity: "session",
                    expected: "idle".into(),
                    actual: "active or resume preparation in progress".into(),
                }),
            );
            return;
        }
        if self.auto_advance || self.queue_preparation.is_some() {
            respond(
                reply,
                Err(CoreError::InvalidState {
                    entity: "queue",
                    expected: "paused before starting live capture".into(),
                    actual: "auto advance is active".into(),
                }),
            );
            return;
        }
        let pipeline_config = match default_pipeline_config() {
            Ok(config) => config,
            Err(error) => {
                respond(reply, Err(error));
                return;
            }
        };
        let source_kind = match source {
            CaptureSource::Microphone { .. } => SourceKind::Microphone,
            CaptureSource::SystemAudio => SourceKind::SystemAudio,
        };
        let device_id = match &source {
            CaptureSource::Microphone { device_id } => device_id.clone(),
            CaptureSource::SystemAudio => None,
        };
        let session_id = Uuid::new_v4().to_string();
        let title = input.title.unwrap_or_else(|| match source_kind {
            SourceKind::Microphone => "Microphone recording".into(),
            SourceKind::SystemAudio => "Desktop audio recording".into(),
            SourceKind::File => unreachable!(),
        });
        let receipt = self
            .store
            .create_session(NewSession {
                session_id: session_id.clone(),
                source_kind,
                source_display_name: match source_kind {
                    SourceKind::Microphone => device_id
                        .clone()
                        .unwrap_or_else(|| "Default microphone".into()),
                    SourceKind::SystemAudio => "Desktop audio".into(),
                    SourceKind::File => unreachable!(),
                },
                source_fingerprint: None,
                source_path: None,
                source_device_id: device_id,
                model: model.repo_id.clone(),
                model_revision: Some(model.revision.clone()),
                language: input.language.clone(),
                title,
                config: pipeline_config.persisted,
            })
            .await;
        let receipt = match receipt {
            Ok(receipt) => receipt,
            Err(error) => {
                respond(reply, Err(error));
                return;
            }
        };
        let run_id = Uuid::new_v4().to_string();
        let pipeline = self.spawn(PipelineSpec {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            model_repo_id: model.repo_id,
            model_revision: model.revision,
            language: input.language,
            next_segment_index: 0,
            resume_sample: 0,
            source: PipelineSource::Live { source, token },
            worker: self.worker_config.clone(),
            loaded_worker: None,
            vad_asset: self.vad_asset.clone(),
            vad_config: pipeline_config.vad,
            transcription_config: pipeline_config.transcription,
        });
        self.active = Some(ActiveSession {
            session_id,
            run_id,
            row_version: receipt.row_version,
            pipeline,
            resume_sample: 0,
            preparing: true,
            pending_end: None,
            start_reply: Some(reply),
            finish_reply: None,
        });
    }

    async fn finish_session(
        &mut self,
        input: api::SessionMutation,
        end: PipelineEnd,
        reply: Reply<api::SessionDetail>,
    ) {
        let expected = match contract::parse_decimal(&input.expected_row_version, "rowVersion") {
            Ok(expected) => expected,
            Err(error) => {
                respond(reply, Err(error));
                return;
            }
        };
        if self.active.is_none() && end == PipelineEnd::UserStop {
            let result = async {
                self.store.stop_paused(&input.session_id, expected).await?;
                let detail = contract::detail(
                    self.store
                        .session_snapshot(&input.session_id, 0, SEGMENT_PAGE_LIMIT)
                        .await?,
                );
                self.emit(|sequence| api::AppEvent::SessionUpserted {
                    sequence,
                    session: detail.summary.clone(),
                });
                Ok(detail)
            }
            .await;
            respond(reply, result);
            return;
        }
        let Some(active) = self.active.as_mut() else {
            respond(
                reply,
                Err(CoreError::InvalidState {
                    entity: "session",
                    expected: "active".into(),
                    actual: "idle".into(),
                }),
            );
            return;
        };
        if active.session_id != input.session_id || active.row_version != expected {
            respond(
                reply,
                Err(CoreError::StaleVersion {
                    entity: "session",
                    expected,
                    actual: active.row_version,
                }),
            );
            return;
        }
        if active.pending_end.is_some() {
            respond(
                reply,
                Err(CoreError::InvalidState {
                    entity: "session",
                    expected: "running without a pending transition".into(),
                    actual: "transition already pending".into(),
                }),
            );
            return;
        }
        let transition = match (end, active.preparing) {
            (PipelineEnd::UserStop | PipelineEnd::Lifecycle(_), true) => {
                self.store
                    .begin_stop_preparing(&active.session_id, active.row_version)
                    .await
            }
            (PipelineEnd::Pause, _) => {
                self.store
                    .begin_pause(&active.session_id, active.row_version)
                    .await
            }
            (PipelineEnd::UserStop | PipelineEnd::Lifecycle(_), false) => {
                self.store
                    .begin_stop(&active.session_id, active.row_version)
                    .await
            }
            (PipelineEnd::Natural, _) => Err(CoreError::InvalidArgument(
                "natural completion cannot be requested".into(),
            )),
        };
        match transition {
            Ok(receipt) => {
                active.row_version = receipt.row_version;
                active.pipeline.finish(end);
                active.pending_end = Some(end);
                active.finish_reply = Some(reply);
                self.emit_active_upsert().await;
            }
            Err(error) => respond(reply, Err(error)),
        }
    }

    fn start_resume(&mut self, input: api::SessionMutation, reply: Reply<api::SessionDetail>) {
        if self.active.is_some() || self.resume_preparation.is_some() {
            respond(
                reply,
                Err(CoreError::InvalidState {
                    entity: "session",
                    expected: "one idle active slot".into(),
                    actual: "active or resume preparation in progress".into(),
                }),
            );
            return;
        }
        if self.auto_advance || self.queue_preparation.is_some() || self.reusable_worker.is_some() {
            respond(
                reply,
                Err(CoreError::InvalidState {
                    entity: "queue",
                    expected: "paused before resuming a session".into(),
                    actual: "auto advance is active".into(),
                }),
            );
            return;
        }
        let expected = match contract::parse_decimal(&input.expected_row_version, "rowVersion") {
            Ok(value) => value,
            Err(error) => {
                respond(reply, Err(error));
                return;
            }
        };
        let job_id = Uuid::new_v4().to_string();
        self.resume_preparation = Some(ResumePreparation {
            job_id: job_id.clone(),
            session_id: input.session_id.clone(),
            reply,
        });
        let store = self.store.clone();
        let audio = self.audio.clone();
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let result = prepare_resume(store, audio, input.session_id, expected).await;
            let _ = sender
                .send(CoreCommand::ResumePrepared { job_id, result })
                .await;
        });
    }

    async fn finish_resume_preparation(
        &mut self,
        job_id: String,
        result: Result<PreparedResume, CoreError>,
    ) {
        let Some(pending) = self.resume_preparation.take() else {
            cancel_prepared_resume(result, &self.audio);
            return;
        };
        if pending.job_id != job_id {
            self.resume_preparation = Some(pending);
            cancel_prepared_resume(result, &self.audio);
            return;
        }
        if self.shutdown.is_some() {
            cancel_prepared_resume(result, &self.audio);
            respond(
                pending.reply,
                Err(CoreError::InvalidState {
                    entity: "session",
                    expected: "resume preparation to finish before lifecycle shutdown".into(),
                    actual: "shutdown requested".into(),
                }),
            );
            self.complete_shutdown_if_idle();
            return;
        }
        let prepared = match result {
            Ok(prepared) => prepared,
            Err(error) => {
                respond(pending.reply, Err(error));
                return;
            }
        };
        if self.active.is_some() || pending.session_id != prepared.context.session_id {
            cancel_pipeline_source(&prepared.source, &self.audio);
            respond(
                pending.reply,
                Err(CoreError::InvalidState {
                    entity: "session",
                    expected: "reserved resume slot".into(),
                    actual: "active slot changed during preparation".into(),
                }),
            );
            return;
        }
        let context = prepared.context;
        let receipt = if context.state == SessionState::Paused {
            self.store
                .resume_paused(&context.session_id, context.row_version)
                .await
        } else {
            self.store
                .retry_failed_file(&context.session_id, context.row_version)
                .await
        };
        let receipt = match receipt {
            Ok(receipt) => receipt,
            Err(error) => {
                cancel_pipeline_source(&prepared.source, &self.audio);
                respond(pending.reply, Err(error));
                return;
            }
        };
        let language = (context.language != "Auto").then_some(context.language.clone());
        let run_id = Uuid::new_v4().to_string();
        let pipeline = self.spawn(PipelineSpec {
            session_id: context.session_id.clone(),
            run_id: run_id.clone(),
            model_repo_id: context.model,
            model_revision: prepared.model_revision,
            language,
            next_segment_index: context.next_segment_index,
            resume_sample: context.resume_sample,
            source: prepared.source,
            worker: self.worker_config.clone(),
            loaded_worker: None,
            vad_asset: self.vad_asset.clone(),
            vad_config: prepared.config.vad,
            transcription_config: prepared.config.transcription,
        });
        self.active = Some(ActiveSession {
            session_id: context.session_id,
            run_id,
            row_version: receipt.row_version,
            pipeline,
            resume_sample: context.resume_sample,
            preparing: true,
            pending_end: None,
            start_reply: Some(pending.reply),
            finish_reply: None,
        });
    }

    async fn enqueue(
        &mut self,
        items: Vec<NewQueueItem>,
        language: Option<String>,
    ) -> Result<api::QueueSnapshot, CoreError> {
        let before = self.store.queue_snapshot().await?;
        let was_empty_and_idle =
            before.items.is_empty() && self.active.is_none() && self.resume_preparation.is_none();
        let snapshot = self.store.enqueue(items).await?;
        self.queue_language = language;
        let start_result = if was_empty_and_idle && !snapshot.items.is_empty() {
            self.auto_advance = true;
            self.start_next_queue_item().await
        } else {
            Ok(())
        };
        let snapshot = self.queue_snapshot().await?;
        self.emit_queue(snapshot.clone());
        start_result?;
        Ok(snapshot)
    }

    async fn queue_snapshot(&self) -> Result<api::QueueSnapshot, CoreError> {
        Ok(contract::queue(
            self.store.queue_snapshot().await?,
            self.auto_advance,
        ))
    }

    async fn reorder_queue(
        &mut self,
        input: api::QueueReorder,
    ) -> Result<api::QueueSnapshot, CoreError> {
        let revision = contract::parse_decimal(&input.expected_revision, "queue revision")?;
        self.store.reorder_queue(input.item_ids, revision).await?;
        self.queue_preparation = None;
        if self.active.is_none() && self.auto_advance {
            self.start_next_queue_item().await?;
        }
        let snapshot = self.queue_snapshot().await?;
        self.emit_queue(snapshot.clone());
        Ok(snapshot)
    }

    async fn remove_queue_item(
        &mut self,
        input: api::QueueRemove,
    ) -> Result<api::QueueSnapshot, CoreError> {
        let revision = contract::parse_decimal(&input.expected_revision, "queue revision")?;
        self.store
            .remove_queue_item(input.item_id, revision)
            .await?;
        self.queue_preparation = None;
        if self.active.is_none() && self.auto_advance {
            self.start_next_queue_item().await?;
        }
        let snapshot = self.queue_snapshot().await?;
        self.emit_queue(snapshot.clone());
        Ok(snapshot)
    }

    async fn clear_queue(
        &mut self,
        input: api::QueueRevision,
    ) -> Result<api::QueueSnapshot, CoreError> {
        let revision = contract::parse_decimal(&input.expected_revision, "queue revision")?;
        let stored = self.store.clear_queue(revision).await?;
        self.auto_advance = false;
        self.queue_preparation = None;
        self.release_reusable_worker();
        let snapshot = contract::queue(stored, false);
        self.emit_queue(snapshot.clone());
        Ok(snapshot)
    }

    async fn start_queue(
        &mut self,
        input: api::QueueStart,
    ) -> Result<api::QueueSnapshot, CoreError> {
        if self.resume_preparation.is_some() {
            return Err(CoreError::InvalidState {
                entity: "session",
                expected: "resume preparation to finish before starting the queue".into(),
                actual: "resume preparation in progress".into(),
            });
        }
        let expected = contract::parse_decimal(&input.expected_revision, "queue revision")?;
        let current = self.store.queue_snapshot().await?;
        if current.revision != expected {
            return Err(CoreError::StaleVersion {
                entity: "queue",
                expected,
                actual: current.revision,
            });
        }
        self.auto_advance = true;
        self.queue_language = input.language;
        if self.active.is_none() {
            self.start_next_queue_item().await?;
        }
        let snapshot = self.queue_snapshot().await?;
        self.emit_queue(snapshot.clone());
        Ok(snapshot)
    }

    async fn pause_queue(&mut self) -> Result<api::QueueSnapshot, CoreError> {
        self.auto_advance = false;
        self.queue_preparation = None;
        self.release_reusable_worker();
        let snapshot = self.queue_snapshot().await?;
        self.emit_queue(snapshot.clone());
        Ok(snapshot)
    }

    async fn start_next_queue_item(&mut self) -> Result<(), CoreError> {
        if self.active.is_some()
            || self.resume_preparation.is_some()
            || !self.auto_advance
            || self.queue_preparation.is_some()
        {
            return Ok(());
        }
        let Some(candidate) = self.store.next_pending_queue_source().await? else {
            self.auto_advance = false;
            self.release_reusable_worker();
            return Ok(());
        };
        let Some(model) = self.selected_model.clone() else {
            self.auto_advance = false;
            self.release_reusable_worker();
            return Err(CoreError::WorkerUnavailable("no model is selected".into()));
        };
        if self.model_state().status != api::ModelStatus::Ready {
            self.auto_advance = false;
            self.release_reusable_worker();
            return Err(CoreError::WorkerUnavailable(
                "the selected model revision has not been verified as available".into(),
            ));
        }
        let job_id = Uuid::new_v4().to_string();
        self.queue_preparation = Some((job_id.clone(), candidate.item_id.clone()));
        let path = PathBuf::from(&candidate.source_path);
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let result = fingerprint_file_async(path).await;
            let _ = sender
                .send(CoreCommand::QueuePrepared {
                    job_id,
                    candidate,
                    model,
                    result,
                })
                .await;
        });
        Ok(())
    }

    async fn finish_queue_preparation(
        &mut self,
        job_id: String,
        candidate: PendingQueueSource,
        model: SelectedModel,
        result: Result<FileFingerprint, CoreError>,
    ) {
        let current = self.queue_preparation.take();
        let is_current = current.as_ref().is_some_and(|(current_job, current_item)| {
            current_job == &job_id && current_item == &candidate.item_id
        });
        if !is_current {
            self.queue_preparation = current;
            return;
        }
        if self.active.is_some() || !self.auto_advance {
            self.release_reusable_worker();
            self.complete_shutdown_if_idle();
            return;
        }
        let action = match result {
            Ok(fingerprint) if fingerprint.value == candidate.source_fingerprint => {
                self.claim_file(candidate, fingerprint, model).await
            }
            Ok(_) | Err(CoreError::FileChanged) => self
                .store
                .invalidate_queue_item(
                    candidate.item_id,
                    "sourceChanged",
                    "The queued audio file changed after it was added",
                )
                .await
                .map(|_| ()),
            Err(error) => self
                .store
                .invalidate_queue_item(candidate.item_id, "sourceUnavailable", error.to_string())
                .await
                .map(|_| ()),
        };
        if let Err(error) = action {
            self.auto_advance = false;
            self.release_reusable_worker();
            self.emit_error(&error);
            self.complete_shutdown_if_idle();
            return;
        }
        if self.active.is_none() {
            if let Ok(queue) = self.queue_snapshot().await {
                self.emit_queue(queue);
            }
            if let Err(error) = self.start_next_queue_item().await {
                self.auto_advance = false;
                self.release_reusable_worker();
                self.emit_error(&error);
            }
        }
        self.complete_shutdown_if_idle();
    }

    async fn claim_file(
        &mut self,
        candidate: PendingQueueSource,
        fingerprint: FileFingerprint,
        model: SelectedModel,
    ) -> Result<(), CoreError> {
        let session_id = Uuid::new_v4().to_string();
        let language = self.queue_language.clone();
        let pipeline_config = default_pipeline_config()?;
        let receipt = self
            .store
            .claim_queue_item(
                &candidate.item_id,
                NewSession {
                    session_id: session_id.clone(),
                    source_kind: SourceKind::File,
                    source_display_name: candidate.display_name.clone(),
                    source_fingerprint: Some(candidate.source_fingerprint),
                    source_path: Some(candidate.source_path.clone()),
                    source_device_id: None,
                    model: model.repo_id.clone(),
                    model_revision: Some(model.revision.clone()),
                    language: language.clone(),
                    title: candidate.display_name,
                    config: pipeline_config.persisted,
                },
            )
            .await?;
        let run_id = Uuid::new_v4().to_string();
        let loaded_worker = self.reusable_worker.take();
        let pipeline = self.spawn(PipelineSpec {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            model_repo_id: model.repo_id,
            model_revision: model.revision,
            language,
            next_segment_index: 0,
            resume_sample: 0,
            source: PipelineSource::File {
                path: PathBuf::from(candidate.source_path),
                identity: fingerprint.identity,
            },
            worker: self.worker_config.clone(),
            loaded_worker,
            vad_asset: self.vad_asset.clone(),
            vad_config: pipeline_config.vad,
            transcription_config: pipeline_config.transcription,
        });
        self.active = Some(ActiveSession {
            session_id,
            run_id,
            row_version: receipt.row_version,
            pipeline,
            resume_sample: 0,
            preparing: true,
            pending_end: None,
            start_reply: None,
            finish_reply: None,
        });
        let queue = self.queue_snapshot().await?;
        self.emit_queue(queue);
        Ok(())
    }

    async fn history_rename(
        &mut self,
        input: api::HistoryRename,
    ) -> Result<api::SessionSummary, CoreError> {
        let expected = contract::parse_decimal(&input.expected_row_version, "rowVersion")?;
        self.store
            .rename_session(&input.session_id, expected, input.title)
            .await?;
        let snapshot = self.store.session_snapshot(&input.session_id, 0, 1).await?;
        let summary = contract::snapshot_summary(&snapshot);
        self.emit(|sequence| api::AppEvent::SessionUpserted {
            sequence,
            session: summary.clone(),
        });
        Ok(summary)
    }

    async fn history_delete(&mut self, input: api::HistoryDelete) -> Result<(), CoreError> {
        let mut sessions = Vec::with_capacity(input.sessions.len());
        for session in input.sessions {
            if self
                .active
                .as_ref()
                .is_some_and(|active| active.session_id == session.session_id)
            {
                return Err(CoreError::InvalidState {
                    entity: "session",
                    expected: "inactive before deletion".into(),
                    actual: "active".into(),
                });
            }
            sessions.push(DeleteSession {
                session_id: session.session_id,
                expected_row_version: contract::parse_decimal(
                    &session.expected_row_version,
                    "rowVersion",
                )?,
            });
        }
        let session_ids = sessions
            .iter()
            .map(|session| session.session_id.clone())
            .collect::<Vec<_>>();
        self.store.delete_sessions(sessions).await?;
        self.emit(|sequence| api::AppEvent::SessionsDeleted {
            sequence,
            session_ids,
        });
        Ok(())
    }

    async fn start_export(
        &mut self,
        input: api::ExportStart,
        destination: PathBuf,
    ) -> Result<api::ExportStartResult, CoreError> {
        if input.session_ids.is_empty() {
            return Err(CoreError::InvalidArgument(
                "export requires at least one session".into(),
            ));
        }
        let operation_id = Uuid::new_v4().to_string();
        let cancellation = CancellationToken::new();
        self.exports
            .insert(operation_id.clone(), cancellation.clone());
        self.emit(|sequence| api::AppEvent::ExportProgress {
            sequence,
            progress: api::ExportProgress {
                operation_id: operation_id.clone(),
                phase: api::ExportPhase::Rendering,
                completed_items: 0,
                total_items: u32::try_from(input.session_ids.len()).unwrap_or(u32::MAX),
                current_session_id: input.session_ids.first().cloned(),
            },
        });
        let store = self.store.clone();
        let sender = self.pipeline_sender();
        let result_operation_id = operation_id.clone();
        tokio::spawn(async move {
            let selected_session_ids = input.session_ids.clone();
            let total_items = u32::try_from(selected_session_ids.len()).unwrap_or(u32::MAX);
            let result = async {
                let mut sessions = Vec::with_capacity(input.session_ids.len());
                for (index, session_id) in input.session_ids.iter().enumerate() {
                    if cancellation.is_cancelled() {
                        return Err(ExportError::Cancelled);
                    }
                    sessions.push(
                        load_export_session(&store, session_id)
                            .await
                            .map_err(|error| ExportError::Io(std::io::Error::other(error)))?,
                    );
                    let _ = sender
                        .send(CoreCommand::ExportProgress {
                            operation_id: result_operation_id.clone(),
                            phase: api::ExportPhase::Rendering,
                            completed_items: u32::try_from(index + 1).unwrap_or(u32::MAX),
                            total_items,
                            current_session_id: input.session_ids.get(index + 1).cloned(),
                        })
                        .await;
                }
                let progress_sender = sender.clone();
                let progress_operation_id = result_operation_id.clone();
                let exported = tokio::task::spawn_blocking(move || {
                    export_sessions_with_progress(
                        &sessions,
                        input.format,
                        destination,
                        Some(&cancellation),
                        |stage| {
                            let phase = match stage {
                                ExportStage::Writing => api::ExportPhase::Writing,
                                ExportStage::Publishing => api::ExportPhase::Publishing,
                            };
                            let _ = progress_sender.blocking_send(CoreCommand::ExportProgress {
                                operation_id: progress_operation_id.clone(),
                                phase,
                                completed_items: total_items,
                                total_items,
                                current_session_id: None,
                            });
                        },
                    )
                })
                .await
                .map_err(|error| ExportError::Io(std::io::Error::other(error)))??;
                Ok(exported.exported_session_ids)
            }
            .await
            .map_err(|error| ExportTaskFailure {
                session_ids: selected_session_ids,
                error,
            });
            let _ = sender
                .send(CoreCommand::ExportFinished {
                    operation_id: result_operation_id,
                    result,
                })
                .await;
        });
        Ok(api::ExportStartResult {
            canceled: false,
            operation_id: Some(operation_id),
        })
    }

    fn cancel_export(&mut self, operation_id: &str) -> Result<(), CoreError> {
        let cancellation = self
            .exports
            .get(operation_id)
            .ok_or_else(|| CoreError::NotFound {
                entity: "export",
                id: operation_id.into(),
            })?;
        cancellation.cancel();
        Ok(())
    }

    fn report_export_progress(
        &mut self,
        operation_id: String,
        phase: api::ExportPhase,
        completed_items: u32,
        total_items: u32,
        current_session_id: Option<String>,
    ) {
        if !self.exports.contains_key(&operation_id) {
            return;
        }
        self.emit(|sequence| api::AppEvent::ExportProgress {
            sequence,
            progress: api::ExportProgress {
                operation_id,
                phase,
                completed_items,
                total_items,
                current_session_id,
            },
        });
    }

    fn finish_export(
        &mut self,
        operation_id: String,
        result: Result<Vec<String>, ExportTaskFailure>,
    ) {
        self.exports.remove(&operation_id);
        let completion = match result {
            Ok(exported_session_ids) => api::ExportCompletion {
                operation_id: operation_id.clone(),
                canceled: false,
                exported_session_ids,
                failures: Vec::new(),
            },
            Err(ExportTaskFailure {
                error: ExportError::Cancelled,
                ..
            }) => api::ExportCompletion {
                operation_id: operation_id.clone(),
                canceled: true,
                exported_session_ids: Vec::new(),
                failures: Vec::new(),
            },
            Err(failure) => api::ExportCompletion {
                operation_id: operation_id.clone(),
                canceled: false,
                exported_session_ids: Vec::new(),
                failures: failure
                    .session_ids
                    .into_iter()
                    .map(|session_id| api::ExportFailure {
                        session_id,
                        error: api::AppError {
                            code: api::AppErrorCode::Internal,
                            message: failure.error.to_string(),
                            recoverable: true,
                        },
                    })
                    .collect(),
            },
        };
        self.emit(|sequence| api::AppEvent::ExportFinished {
            sequence,
            result: completion,
        });
        self.complete_shutdown_if_idle();
    }

    async fn handle_pipeline(&mut self, event: PipelineEvent) {
        match event {
            PipelineEvent::Ready {
                session_id,
                run_id,
                acknowledgement,
            } => {
                let valid = self.is_active_run(&session_id, &run_id);
                if !valid {
                    let _ = acknowledgement.send(false);
                    return;
                }
                if self
                    .active
                    .as_ref()
                    .and_then(|active| active.pending_end)
                    .is_some()
                {
                    let _ = acknowledgement.send(true);
                    return;
                }
                let (id, version) = {
                    let active = self.active.as_ref().expect("active run was checked");
                    (active.session_id.clone(), active.row_version)
                };
                match self.store.start_running(&id, version).await {
                    Ok(receipt) => {
                        if let Some(active) = self.active.as_mut() {
                            active.row_version = receipt.row_version;
                            active.preparing = false;
                        }
                        self.emit_active_upsert().await;
                        let detail = self.active_detail().await;
                        if let Some(reply) = self
                            .active
                            .as_mut()
                            .and_then(|active| active.start_reply.take())
                        {
                            respond(reply, detail);
                        }
                        let _ = acknowledgement.send(true);
                    }
                    Err(error) => {
                        let _ = acknowledgement.send(false);
                        self.fail_active(error).await;
                    }
                }
            }
            PipelineEvent::Segment {
                session_id,
                run_id,
                index,
                speech,
                transcription,
                acknowledgement,
            } => {
                if !self.is_active_run(&session_id, &run_id) {
                    let _ = acknowledgement.send(false);
                    return;
                }
                let version = self
                    .active
                    .as_ref()
                    .expect("active run was checked")
                    .row_version;
                let segment = NewSegment {
                    index,
                    start_sample: speech.start_sample,
                    end_sample: speech.end_sample(),
                    split_reason: speech.split_reason,
                    text: transcription.text,
                    raw_text: Some(transcription.raw_text),
                    language: transcription.language,
                    vad: speech.vad,
                    transcription: transcription.diagnostics,
                    decode_ms: 0,
                    queue_wait_ms: 0,
                };
                let committed_sample = segment.end_sample;
                match self
                    .store
                    .append_segment(&session_id, version, segment)
                    .await
                {
                    Ok(receipt) => {
                        if let Some(active) = self.active.as_mut() {
                            active.row_version = receipt.row_version;
                            active.resume_sample = active.resume_sample.max(committed_sample);
                        }
                        let segment = contract::segment(receipt.segment);
                        self.emit(|sequence| api::AppEvent::SegmentCommitted {
                            sequence,
                            session_id: receipt.session_id,
                            row_version: receipt.row_version.to_string(),
                            segment_count: receipt.total_segments,
                            recognized_segment_count: receipt.recognized_segments,
                            character_count: u32::try_from(receipt.characters).unwrap_or(u32::MAX),
                            duration_ms: receipt.media_duration_ms as f64,
                            segment,
                        });
                        let _ = acknowledgement.send(true);
                    }
                    Err(error) => {
                        let _ = acknowledgement.send(false);
                        self.fail_active(error).await;
                    }
                }
            }
            PipelineEvent::Progress {
                session_id,
                run_id,
                processed_sample,
                queued_segments,
            } => {
                if self.is_active_run(&session_id, &run_id) {
                    if let Some(active) = self.active.as_mut() {
                        active.resume_sample = active.resume_sample.max(processed_sample);
                    }
                    self.emit(|sequence| api::AppEvent::SessionProgress {
                        sequence,
                        session_id,
                        run_id,
                        processed_audio_ms: processed_sample as f64 / 16.0,
                        total_audio_ms: None,
                        queued_segments,
                    });
                }
            }
            PipelineEvent::Finished {
                session_id,
                run_id,
                end,
                resume_sample,
                worker,
            } => {
                if self.is_active_run(&session_id, &run_id) {
                    self.reusable_worker = worker;
                    self.finish_active(end, resume_sample).await;
                    if self.active.is_none() && self.queue_preparation.is_none() {
                        self.release_reusable_worker();
                    }
                } else if let Some(worker) = worker {
                    self.release_worker(worker);
                }
            }
            PipelineEvent::Failed {
                session_id,
                run_id,
                error,
            } => {
                if self.is_active_run(&session_id, &run_id) {
                    if let Some(reason) = self.shutdown.as_ref().map(|shutdown| shutdown.reason) {
                        let resume_sample = self
                            .active
                            .as_ref()
                            .map_or(0, |active| active.resume_sample);
                        self.emit_error(&error);
                        self.finish_active(PipelineEnd::Lifecycle(reason), resume_sample)
                            .await;
                    } else {
                        self.fail_active(error).await;
                    }
                }
            }
        }
    }

    async fn finish_active(&mut self, end: PipelineEnd, resume_sample: u64) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        let id = active.session_id.clone();
        let version = active.row_version;
        let pending_end = active.pending_end.unwrap_or(end);
        let shutdown_reason = self.shutdown.as_ref().map(|shutdown| shutdown.reason);
        let result = match (pending_end, shutdown_reason) {
            (PipelineEnd::Pause, Some(reason)) => {
                match self.store.commit_paused(&id, version, resume_sample).await {
                    Ok(paused) => {
                        self.store
                            .complete_lifecycle_stop(&id, paused.row_version, reason)
                            .await
                    }
                    Err(error) => Err(error),
                }
            }
            (PipelineEnd::UserStop | PipelineEnd::Lifecycle(_), Some(reason)) => {
                self.store
                    .complete_lifecycle_stop(&id, version, reason)
                    .await
            }
            (PipelineEnd::Natural, _) => self.store.complete_running(&id, version).await,
            (PipelineEnd::Pause, None) => {
                self.store.commit_paused(&id, version, resume_sample).await
            }
            (PipelineEnd::UserStop, None) => self.store.complete_stopping(&id, version).await,
            (PipelineEnd::Lifecycle(reason), None) => {
                self.store
                    .complete_lifecycle_stop(&id, version, reason)
                    .await
            }
        };
        match result {
            Ok(_) => {
                let detail = self
                    .store
                    .session_snapshot(&id, 0, SEGMENT_PAGE_LIMIT)
                    .await
                    .map(contract::detail);
                if let Ok(detail) = &detail {
                    let summary = detail.summary.clone();
                    self.emit(|sequence| api::AppEvent::SessionUpserted {
                        sequence,
                        session: summary,
                    });
                }
                let mut active = self.active.take().expect("active session still exists");
                if let Some(reply) = active.finish_reply.take() {
                    respond(reply, detail);
                }
                if let Some(reply) = active.start_reply.take() {
                    respond(
                        reply,
                        Err(CoreError::InvalidState {
                            entity: "session",
                            expected: "running".into(),
                            actual: "finished during preparation".into(),
                        }),
                    );
                }
                if self.auto_advance
                    && pending_end != PipelineEnd::Pause
                    && let Err(error) = self.start_next_queue_item().await
                {
                    self.auto_advance = false;
                    self.emit_error(&error);
                }
                self.complete_shutdown_if_idle();
            }
            Err(error) => self.fail_active(error).await,
        }
    }

    async fn fail_active(&mut self, error: CoreError) {
        let Some(active) = self.active.take() else {
            return;
        };
        debug_assert!(active.start_reply.is_none() || active.finish_reply.is_none());
        let pending_reply = active.start_reply.or(active.finish_reply);
        active.pipeline.finish(PipelineEnd::UserStop);
        let fatal = is_fatal_core_error(&error);
        if fatal {
            self.auto_advance = false;
        }
        let failed = self
            .store
            .fail_session(
                &active.session_id,
                active.row_version,
                error.code(),
                error.to_string(),
            )
            .await;
        if let Ok(receipt) = failed
            && let Ok(snapshot) = self.store.session_snapshot(&receipt.session_id, 0, 1).await
        {
            let summary = contract::snapshot_summary(&snapshot);
            self.emit(|sequence| api::AppEvent::SessionUpserted {
                sequence,
                session: summary,
            });
        }
        self.emit_error(&error);
        if self.auto_advance
            && !fatal
            && let Err(next_error) = self.start_next_queue_item().await
        {
            self.auto_advance = false;
            self.emit_error(&next_error);
        }
        self.complete_shutdown_if_idle();
        if let Some(reply) = pending_reply {
            respond(reply, Err(error));
        }
    }

    async fn active_detail(&self) -> Result<api::SessionDetail, CoreError> {
        let active = self
            .active
            .as_ref()
            .ok_or_else(|| CoreError::InvalidState {
                entity: "session",
                expected: "active".into(),
                actual: "idle".into(),
            })?;
        Ok(contract::detail(
            self.store
                .session_snapshot(&active.session_id, 0, SEGMENT_PAGE_LIMIT)
                .await?,
        ))
    }

    async fn emit_active_upsert(&mut self) {
        let summary = match self.active_detail().await {
            Ok(detail) => detail.summary,
            Err(error) => {
                self.emit_error(&error);
                return;
            }
        };
        self.emit(|sequence| api::AppEvent::SessionUpserted {
            sequence,
            session: summary,
        });
    }

    fn is_active_run(&self, session_id: &str, run_id: &str) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.session_id == session_id && active.run_id == run_id)
    }

    fn spawn(&self, spec: PipelineSpec) -> PipelineHandle {
        spawn_pipeline(spec, self.audio.clone(), self.pipeline_events.clone())
    }

    fn pipeline_sender(&self) -> mpsc::Sender<CoreCommand> {
        self.sender.clone()
    }

    fn request_close(&mut self) -> Result<bool, CoreError> {
        let requires_confirmation =
            self.active.is_some() || self.resume_preparation.is_some() || !self.exports.is_empty();
        if requires_confirmation {
            let active_session_id = self
                .active
                .as_ref()
                .map(|active| active.session_id.clone())
                .or_else(|| {
                    self.resume_preparation
                        .as_ref()
                        .map(|pending| pending.session_id.clone())
                });
            let active_export_ids = self.exports.keys().cloned().collect();
            self.emit(|sequence| api::AppEvent::CloseConfirmationRequired {
                sequence,
                active_session_id,
                active_export_ids,
            });
        }
        Ok(!requires_confirmation)
    }

    fn report_close_failure(&mut self, error: api::AppError) {
        let active_session_id = self
            .active
            .as_ref()
            .map(|active| active.session_id.clone())
            .or_else(|| {
                self.resume_preparation
                    .as_ref()
                    .map(|pending| pending.session_id.clone())
            });
        self.emit(|sequence| api::AppEvent::CloseForceRequired {
            sequence,
            active_session_id,
            error,
        });
    }

    async fn shutdown(&mut self, reason: LifecycleStopReason, reply: Reply<()>) {
        self.auto_advance = false;
        self.queue_preparation = None;
        self.release_reusable_worker();
        for cancellation in self.exports.values() {
            cancellation.cancel();
        }
        if self.shutdown.is_some() {
            respond(
                reply,
                Err(CoreError::InvalidState {
                    entity: "application",
                    expected: "one shutdown request".into(),
                    actual: "shutdown already pending".into(),
                }),
            );
            return;
        }
        if let Some(active) = self.active.as_mut() {
            if active.pending_end.is_some() {
                active.pipeline.finish(PipelineEnd::Lifecycle(reason));
                self.shutdown = Some(ShutdownState { reason, reply });
                return;
            }
            let transition = if active.preparing {
                self.store
                    .begin_stop_preparing(&active.session_id, active.row_version)
                    .await
            } else {
                self.store
                    .begin_stop(&active.session_id, active.row_version)
                    .await
            };
            match transition {
                Ok(receipt) => {
                    active.row_version = receipt.row_version;
                    active.pending_end = Some(PipelineEnd::Lifecycle(reason));
                    active.pipeline.finish(PipelineEnd::Lifecycle(reason));
                    self.shutdown = Some(ShutdownState { reason, reply });
                    self.emit_active_upsert().await;
                }
                Err(error) => respond(reply, Err(error)),
            }
        } else if self.exports.is_empty()
            && self.worker_cleanups == 0
            && self.model_list_reply.is_none()
            && self.resume_preparation.is_none()
        {
            respond(reply, Ok(()));
        } else {
            self.shutdown = Some(ShutdownState { reason, reply });
        }
    }

    fn complete_shutdown_if_idle(&mut self) {
        if self.active.is_none()
            && self.exports.is_empty()
            && self.worker_cleanups == 0
            && self.model_list_reply.is_none()
            && self.resume_preparation.is_none()
            && let Some(shutdown) = self.shutdown.take()
        {
            let _ = shutdown.reason;
            respond(shutdown.reply, Ok(()));
        }
    }

    fn emit_model(&mut self) {
        let model = self.model_state();
        self.emit(|sequence| api::AppEvent::ModelChanged { sequence, model });
    }

    fn release_reusable_worker(&mut self) {
        if let Some(worker) = self.reusable_worker.take() {
            self.release_worker(worker);
        }
    }

    fn release_worker(&mut self, worker: WorkerProcess) {
        self.worker_cleanups = self.worker_cleanups.saturating_add(1);
        let sender = self.sender.clone();
        tokio::spawn(async move {
            shutdown_worker(worker).await;
            let _ = sender.send(CoreCommand::WorkerReleased).await;
        });
    }

    fn emit_queue(&mut self, queue: api::QueueSnapshot) {
        self.emit(|sequence| api::AppEvent::QueueChanged { sequence, queue });
    }

    fn emit_error(&mut self, error: &CoreError) {
        let error = contract::app_error(error);
        self.emit(|sequence| api::AppEvent::NotificationError { sequence, error });
    }

    fn emit(&mut self, build: impl FnOnce(String) -> api::AppEvent) {
        self.sequence = self
            .sequence
            .checked_add(1)
            .expect("application event sequence exhausted");
        self.events.emit(build(self.sequence.to_string()));
    }
}

fn respond<T>(reply: Reply<T>, result: Result<T, CoreError>) {
    let _ = reply.send(result);
}

fn prepare_queue_items(paths: Vec<PathBuf>) -> Result<Vec<NewQueueItem>, CoreError> {
    let mut items = Vec::with_capacity(paths.len());
    for path in paths {
        let fingerprint = fingerprint_file(&path)?;
        let display_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| CoreError::InvalidArgument("audio file name is not UTF-8".into()))?
            .to_owned();
        items.push(NewQueueItem {
            item_id: Uuid::new_v4().to_string(),
            display_name,
            source_path: path.to_string_lossy().into_owned(),
            source_fingerprint: fingerprint.value,
        });
    }
    Ok(items)
}

async fn query_cached_models(
    worker_config: WorkerProcessConfig,
) -> Result<Vec<CachedModel>, CoreError> {
    let worker = WorkerProcess::launch(worker_config).await?;
    let listed = worker
        .list_models(format!("models-{}", Uuid::new_v4()))
        .await;
    let shutdown = worker
        .shutdown(format!("shutdown-{}", Uuid::new_v4()))
        .await;
    match listed {
        Ok(result) => {
            shutdown?;
            Ok(result.models)
        }
        Err(error) => {
            let _ = shutdown;
            Err(error)
        }
    }
}

async fn prepare_resume(
    store: Store,
    audio: Arc<AudioCaptureManager>,
    session_id: String,
    expected_row_version: u64,
) -> Result<PreparedResume, CoreError> {
    let context = store.resume_context(&session_id).await?;
    if context.row_version != expected_row_version {
        return Err(CoreError::StaleVersion {
            entity: "session",
            expected: expected_row_version,
            actual: context.row_version,
        });
    }
    let model_revision = context.model_revision.clone().ok_or_else(|| {
        CoreError::WorkerUnavailable("saved session has no exact model revision".into())
    })?;
    let config = parse_pipeline_config(&context.config)?;
    let source = match context.source_kind {
        SourceKind::File => {
            let path = PathBuf::from(context.source_path.as_ref().ok_or_else(|| {
                CoreError::InvalidArgument("saved file session has no path".into())
            })?);
            let fingerprint = fingerprint_file_async(path.clone()).await?;
            if Some(fingerprint.value.as_str()) != context.source_fingerprint.as_deref() {
                return Err(CoreError::FileChanged);
            }
            PipelineSource::File {
                path,
                identity: fingerprint.identity,
            }
        }
        SourceKind::Microphone => {
            let device_id = context.source_device_id.clone().ok_or_else(|| {
                CoreError::InvalidArgument(
                    "saved microphone session has no exact platform device UID".into(),
                )
            })?;
            prepare_resume_live(
                audio,
                CaptureSource::Microphone {
                    device_id: Some(device_id),
                },
            )
            .await?
        }
        SourceKind::SystemAudio => prepare_resume_live(audio, CaptureSource::SystemAudio).await?,
    };
    Ok(PreparedResume {
        context,
        model_revision,
        source,
        config,
    })
}

async fn prepare_resume_live(
    audio: Arc<AudioCaptureManager>,
    source: CaptureSource,
) -> Result<PipelineSource, CoreError> {
    let resolver = audio.clone();
    let checked = source.clone();
    let source = tokio::task::spawn_blocking(move || resolver.resolve_source(&checked))
        .await
        .map_err(|error| CoreError::BlockingTask(error.to_string()))?
        .map_err(|error| CoreError::AudioNormalize(error.to_string()))?;
    let token = audio
        .reserve_start()
        .map_err(|error| CoreError::AudioNormalize(error.to_string()))?;
    Ok(PipelineSource::Live { source, token })
}

fn cancel_prepared_resume(result: Result<PreparedResume, CoreError>, audio: &AudioCaptureManager) {
    if let Ok(prepared) = result {
        cancel_pipeline_source(&prepared.source, audio);
    }
}

fn cancel_pipeline_source(source: &PipelineSource, audio: &AudioCaptureManager) {
    if let PipelineSource::Live { token, .. } = source {
        audio.cancel_reserved(token);
    }
}

async fn fingerprint_file_async(path: PathBuf) -> Result<FileFingerprint, CoreError> {
    tokio::task::spawn_blocking(move || fingerprint_file(&path))
        .await
        .map_err(|error| CoreError::BlockingTask(error.to_string()))?
}

async fn query_history(
    store: Store,
    input: api::HistoryQuery,
) -> Result<api::HistoryPage, CoreError> {
    let query = contract::history_query(input)?;
    contract::history_page(store.history(query).await?)
}

async fn get_history_detail(
    store: Store,
    input: api::HistoryDetailQuery,
) -> Result<api::SessionDetail, CoreError> {
    if !(1..=500).contains(&input.segment_limit) {
        return Err(CoreError::InvalidArgument(
            "segmentLimit must be between 1 and 500".into(),
        ));
    }
    let snapshot = store
        .session_snapshot(
            input.session_id,
            input.segment_offset,
            u32::from(input.segment_limit),
        )
        .await?;
    if let Some(expected) = input.expected_row_version {
        let expected = contract::parse_decimal(&expected, "rowVersion")?;
        if expected != snapshot.row_version {
            return Err(CoreError::StaleVersion {
                entity: "session",
                expected,
                actual: snapshot.row_version,
            });
        }
    }
    Ok(contract::detail(snapshot))
}

async fn render_history(store: Store, input: api::HistoryRender) -> Result<String, CoreError> {
    if input.session_ids.is_empty() {
        return Err(CoreError::InvalidArgument(
            "clipboard rendering requires at least one session".into(),
        ));
    }
    let mut sessions = Vec::with_capacity(input.session_ids.len());
    for session_id in &input.session_ids {
        sessions.push(load_export_session(&store, session_id).await?);
    }
    tokio::task::spawn_blocking(move || render_sessions_for_clipboard(&sessions, input.format))
        .await
        .map_err(|error| CoreError::BlockingTask(error.to_string()))?
        .map_err(|error| CoreError::InvalidArgument(error.to_string()))
}

async fn load_export_session(store: &Store, session_id: &str) -> Result<ExportSession, CoreError> {
    let mut offset = 0;
    let mut segments = Vec::new();
    let mut title = None;
    let mut sample_rate = None;
    let mut row_version = None;
    loop {
        let snapshot = store
            .session_snapshot(session_id, offset, SEGMENT_PAGE_LIMIT)
            .await?;
        if let Some(expected) = row_version {
            if snapshot.row_version != expected {
                return Err(CoreError::StaleVersion {
                    entity: "export snapshot",
                    expected,
                    actual: snapshot.row_version,
                });
            }
        } else {
            row_version = Some(snapshot.row_version);
        }
        title.get_or_insert(snapshot.title.clone());
        sample_rate.get_or_insert(snapshot.sample_rate);
        segments.extend(snapshot.segments.into_iter().map(|segment| ExportSegment {
            segment_index: segment.index,
            start_sample: segment.start_sample,
            end_sample: segment.end_sample,
            text: segment.text,
            language: Some(segment.language),
            split_reason: Some(segment.split_reason.as_str().into()),
            raw_text: segment.raw_text,
            diagnostics: Some(segment.diagnostics),
        }));
        let Some(next) = snapshot.next_segment_offset else {
            break;
        };
        offset = next;
    }
    Ok(ExportSession {
        session_id: session_id.into(),
        title: title.unwrap_or_else(|| session_id.into()),
        sample_rate: sample_rate.unwrap_or(16_000),
        segments,
    })
}

fn is_fatal_core_error(error: &CoreError) -> bool {
    matches!(
        error,
        CoreError::Database(_)
            | CoreError::InvalidDatabase(_)
            | CoreError::StoreClosed
            | CoreError::WorkerProtocol(_)
            | CoreError::WorkerClosed
            | CoreError::WorkerUnresponsive
            | CoreError::WorkerUnavailable(_)
            | CoreError::WorkerExited(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_transport_failures_stop_queue_advancement() {
        assert!(is_fatal_core_error(&CoreError::WorkerClosed));
        assert!(is_fatal_core_error(&CoreError::WorkerUnresponsive));
        assert!(is_fatal_core_error(&CoreError::WorkerUnavailable(
            "missing worker".into()
        )));
        assert!(is_fatal_core_error(&CoreError::WorkerExited(
            "signal".into()
        )));
        assert!(!is_fatal_core_error(&CoreError::AudioDecode(
            "bad file".into()
        )));
    }
}
