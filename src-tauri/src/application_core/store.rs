use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    thread::{self, JoinHandle},
    time::Duration,
};

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use time::{OffsetDateTime, macros::format_description};
use tokio::sync::{mpsc, oneshot};

use super::{
    domain::{
        DeleteSession, HistoryCursor, HistoryPage, HistoryQuery, HistorySort, LifecycleStopReason,
        NORMALIZED_SAMPLE_RATE, NewQueueItem, NewSegment, NewSession, QueueItem, QueueItemState,
        QueueSnapshot, ResumeContext, SegmentMutationReceipt, SegmentRecord, SelectedModel,
        SessionMutationReceipt, SessionSnapshot, SessionState, SessionSummary, SourceKind,
        SplitReason,
    },
    error::CoreError,
};

pub const APPLICATION_SCHEMA_VERSION: u32 = 5;
const WRITER_CHANNEL_CAPACITY: usize = 64;
const BUSY_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
enum SessionOperation {
    StartRunning,
    BeginPause,
    CommitPaused { resume_sample: u64 },
    BeginStop,
    CompleteRunning,
    CompleteStopping,
    StopPaused,
    ResumePaused,
    RetryFailed,
    Fail { code: String, message: String },
    CompleteLifecycleStop { reason: LifecycleStopReason },
}

/// A cloneable handle to the single SQLite writer and independent read snapshots.
#[derive(Clone)]
pub struct Store {
    inner: Arc<StoreInner>,
    database_path: Arc<PathBuf>,
}

struct StoreInner {
    sender: Option<mpsc::Sender<WriterCommand>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for StoreInner {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

enum WriterCommand {
    CreateSession {
        session: NewSession,
        reply: oneshot::Sender<Result<SessionMutationReceipt, CoreError>>,
    },
    SessionOperation {
        session_id: String,
        expected_row_version: u64,
        operation: SessionOperation,
        reply: oneshot::Sender<Result<SessionMutationReceipt, CoreError>>,
    },
    AppendSegment {
        session_id: String,
        expected_row_version: u64,
        segment: NewSegment,
        reply: oneshot::Sender<Result<SegmentMutationReceipt, CoreError>>,
    },
    RecoverStartup {
        reply: oneshot::Sender<Result<usize, CoreError>>,
    },
    SetSelectedModel {
        model: SelectedModel,
        reply: oneshot::Sender<Result<(), CoreError>>,
    },
    Enqueue {
        items: Vec<NewQueueItem>,
        reply: oneshot::Sender<Result<QueueSnapshot, CoreError>>,
    },
    ReorderQueue {
        item_ids: Vec<String>,
        expected_revision: u64,
        reply: oneshot::Sender<Result<QueueSnapshot, CoreError>>,
    },
    RemoveQueueItem {
        item_id: String,
        expected_revision: u64,
        reply: oneshot::Sender<Result<QueueSnapshot, CoreError>>,
    },
    ClearQueue {
        expected_revision: u64,
        reply: oneshot::Sender<Result<QueueSnapshot, CoreError>>,
    },
    InvalidateQueueItem {
        item_id: String,
        code: String,
        message: String,
        reply: oneshot::Sender<Result<QueueSnapshot, CoreError>>,
    },
    ClaimQueueItem {
        item_id: String,
        session: NewSession,
        reply: oneshot::Sender<Result<SessionMutationReceipt, CoreError>>,
    },
    RenameSession {
        session_id: String,
        expected_row_version: u64,
        title: String,
        reply: oneshot::Sender<Result<SessionMutationReceipt, CoreError>>,
    },
    DeleteSessions {
        sessions: Vec<DeleteSession>,
        reply: oneshot::Sender<Result<usize, CoreError>>,
    },
}

impl Store {
    pub fn open(database_path: impl Into<PathBuf>) -> Result<Self, CoreError> {
        let database_path = database_path.into();
        if let Some(parent) = database_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let writer_path = database_path.clone();
        let (sender, mut receiver) = mpsc::channel(WRITER_CHANNEL_CAPACITY);
        let (ready_sender, ready_receiver) = std::sync::mpsc::sync_channel(1);
        let writer_thread = thread::Builder::new()
            .name("recogui-database-writer".into())
            .spawn(move || {
                let mut connection = match initialize_writer(&writer_path) {
                    Ok(connection) => {
                        let _ = ready_sender.send(Ok(()));
                        connection
                    }
                    Err(error) => {
                        let _ = ready_sender.send(Err(error));
                        return;
                    }
                };

                while let Some(command) = receiver.blocking_recv() {
                    match command {
                        WriterCommand::CreateSession { session, reply } => {
                            let _ = reply.send(create_session(&mut connection, session));
                        }
                        WriterCommand::SessionOperation {
                            session_id,
                            expected_row_version,
                            operation,
                            reply,
                        } => {
                            let _ = reply.send(apply_session_operation(
                                &mut connection,
                                &session_id,
                                expected_row_version,
                                operation,
                            ));
                        }
                        WriterCommand::AppendSegment {
                            session_id,
                            expected_row_version,
                            segment,
                            reply,
                        } => {
                            let _ = reply.send(append_segment(
                                &mut connection,
                                &session_id,
                                expected_row_version,
                                &segment,
                            ));
                        }
                        WriterCommand::RecoverStartup { reply } => {
                            let _ = reply.send(recover_startup(&mut connection));
                        }
                        WriterCommand::SetSelectedModel { model, reply } => {
                            let _ = reply.send(set_selected_model(&mut connection, &model));
                        }
                        WriterCommand::Enqueue { items, reply } => {
                            let _ = reply.send(enqueue(&mut connection, &items));
                        }
                        WriterCommand::ReorderQueue {
                            item_ids,
                            expected_revision,
                            reply,
                        } => {
                            let _ = reply.send(reorder_queue(
                                &mut connection,
                                &item_ids,
                                expected_revision,
                            ));
                        }
                        WriterCommand::RemoveQueueItem {
                            item_id,
                            expected_revision,
                            reply,
                        } => {
                            let _ = reply.send(remove_queue_item(
                                &mut connection,
                                &item_id,
                                expected_revision,
                            ));
                        }
                        WriterCommand::ClearQueue {
                            expected_revision,
                            reply,
                        } => {
                            let _ = reply.send(clear_queue(&mut connection, expected_revision));
                        }
                        WriterCommand::InvalidateQueueItem {
                            item_id,
                            code,
                            message,
                            reply,
                        } => {
                            let _ = reply.send(invalidate_queue_item(
                                &mut connection,
                                &item_id,
                                &code,
                                &message,
                            ));
                        }
                        WriterCommand::ClaimQueueItem {
                            item_id,
                            session,
                            reply,
                        } => {
                            let _ =
                                reply.send(claim_queue_item(&mut connection, &item_id, session));
                        }
                        WriterCommand::RenameSession {
                            session_id,
                            expected_row_version,
                            title,
                            reply,
                        } => {
                            let _ = reply.send(rename_session(
                                &mut connection,
                                &session_id,
                                expected_row_version,
                                &title,
                            ));
                        }
                        WriterCommand::DeleteSessions { sessions, reply } => {
                            let _ = reply.send(delete_sessions(&mut connection, &sessions));
                        }
                    }
                }
            })?;

        match ready_receiver.recv() {
            Ok(Ok(())) => Ok(Self {
                inner: Arc::new(StoreInner {
                    sender: Some(sender),
                    thread: Some(writer_thread),
                }),
                database_path: Arc::new(database_path),
            }),
            Ok(Err(error)) => {
                let _ = writer_thread.join();
                Err(error)
            }
            Err(error) => {
                let _ = writer_thread.join();
                Err(CoreError::BlockingTask(error.to_string()))
            }
        }
    }

    pub async fn create_session(
        &self,
        session: NewSession,
    ) -> Result<SessionMutationReceipt, CoreError> {
        let (reply, response) = oneshot::channel();
        self.sender()
            .send(WriterCommand::CreateSession { session, reply })
            .await
            .map_err(|_| CoreError::StoreClosed)?;
        response.await.map_err(|_| CoreError::StoreClosed)?
    }

    async fn session_operation(
        &self,
        session_id: impl Into<String>,
        expected_row_version: u64,
        operation: SessionOperation,
    ) -> Result<SessionMutationReceipt, CoreError> {
        let (reply, response) = oneshot::channel();
        self.sender()
            .send(WriterCommand::SessionOperation {
                session_id: session_id.into(),
                expected_row_version,
                operation,
                reply,
            })
            .await
            .map_err(|_| CoreError::StoreClosed)?;
        response.await.map_err(|_| CoreError::StoreClosed)?
    }

    pub async fn start_running(
        &self,
        session_id: impl Into<String>,
        expected_row_version: u64,
    ) -> Result<SessionMutationReceipt, CoreError> {
        self.session_operation(
            session_id,
            expected_row_version,
            SessionOperation::StartRunning,
        )
        .await
    }

    pub async fn begin_pause(
        &self,
        session_id: impl Into<String>,
        expected_row_version: u64,
    ) -> Result<SessionMutationReceipt, CoreError> {
        self.session_operation(
            session_id,
            expected_row_version,
            SessionOperation::BeginPause,
        )
        .await
    }

    pub async fn commit_paused(
        &self,
        session_id: impl Into<String>,
        expected_row_version: u64,
        resume_sample: u64,
    ) -> Result<SessionMutationReceipt, CoreError> {
        self.session_operation(
            session_id,
            expected_row_version,
            SessionOperation::CommitPaused { resume_sample },
        )
        .await
    }

    pub async fn begin_stop(
        &self,
        session_id: impl Into<String>,
        expected_row_version: u64,
    ) -> Result<SessionMutationReceipt, CoreError> {
        self.session_operation(
            session_id,
            expected_row_version,
            SessionOperation::BeginStop,
        )
        .await
    }

    pub async fn complete_running(
        &self,
        session_id: impl Into<String>,
        expected_row_version: u64,
    ) -> Result<SessionMutationReceipt, CoreError> {
        self.session_operation(
            session_id,
            expected_row_version,
            SessionOperation::CompleteRunning,
        )
        .await
    }

    pub async fn complete_stopping(
        &self,
        session_id: impl Into<String>,
        expected_row_version: u64,
    ) -> Result<SessionMutationReceipt, CoreError> {
        self.session_operation(
            session_id,
            expected_row_version,
            SessionOperation::CompleteStopping,
        )
        .await
    }

    pub async fn stop_paused(
        &self,
        session_id: impl Into<String>,
        expected_row_version: u64,
    ) -> Result<SessionMutationReceipt, CoreError> {
        self.session_operation(
            session_id,
            expected_row_version,
            SessionOperation::StopPaused,
        )
        .await
    }

    pub async fn complete_lifecycle_stop(
        &self,
        session_id: impl Into<String>,
        expected_row_version: u64,
        reason: LifecycleStopReason,
    ) -> Result<SessionMutationReceipt, CoreError> {
        self.session_operation(
            session_id,
            expected_row_version,
            SessionOperation::CompleteLifecycleStop { reason },
        )
        .await
    }

    pub async fn resume_paused(
        &self,
        session_id: impl Into<String>,
        expected_row_version: u64,
    ) -> Result<SessionMutationReceipt, CoreError> {
        self.session_operation(
            session_id,
            expected_row_version,
            SessionOperation::ResumePaused,
        )
        .await
    }

    pub async fn retry_failed_file(
        &self,
        session_id: impl Into<String>,
        expected_row_version: u64,
    ) -> Result<SessionMutationReceipt, CoreError> {
        self.session_operation(
            session_id,
            expected_row_version,
            SessionOperation::RetryFailed,
        )
        .await
    }

    pub async fn fail_session(
        &self,
        session_id: impl Into<String>,
        expected_row_version: u64,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Result<SessionMutationReceipt, CoreError> {
        self.session_operation(
            session_id,
            expected_row_version,
            SessionOperation::Fail {
                code: code.into(),
                message: message.into(),
            },
        )
        .await
    }

    pub async fn append_segment(
        &self,
        session_id: impl Into<String>,
        expected_row_version: u64,
        segment: NewSegment,
    ) -> Result<SegmentMutationReceipt, CoreError> {
        let (reply, response) = oneshot::channel();
        self.sender()
            .send(WriterCommand::AppendSegment {
                session_id: session_id.into(),
                expected_row_version,
                segment,
                reply,
            })
            .await
            .map_err(|_| CoreError::StoreClosed)?;
        response.await.map_err(|_| CoreError::StoreClosed)?
    }

    pub async fn session_snapshot(
        &self,
        session_id: impl Into<String>,
        segment_offset: u32,
        segment_limit: u32,
    ) -> Result<SessionSnapshot, CoreError> {
        if !(1..=500).contains(&segment_limit) {
            return Err(CoreError::InvalidArgument(
                "segment page size must be between 1 and 500".into(),
            ));
        }
        let path = self.database_path.as_ref().clone();
        let session_id = session_id.into();
        tokio::task::spawn_blocking(move || {
            read_session_snapshot(&path, &session_id, segment_offset, segment_limit)
        })
        .await
        .map_err(|error| CoreError::BlockingTask(error.to_string()))?
    }

    pub async fn recover_startup(&self) -> Result<usize, CoreError> {
        let (reply, response) = oneshot::channel();
        self.sender()
            .send(WriterCommand::RecoverStartup { reply })
            .await
            .map_err(|_| CoreError::StoreClosed)?;
        response.await.map_err(|_| CoreError::StoreClosed)?
    }

    pub async fn set_selected_model(&self, model: SelectedModel) -> Result<(), CoreError> {
        let (reply, response) = oneshot::channel();
        self.sender()
            .send(WriterCommand::SetSelectedModel { model, reply })
            .await
            .map_err(|_| CoreError::StoreClosed)?;
        response.await.map_err(|_| CoreError::StoreClosed)?
    }

    pub async fn selected_model(&self) -> Result<Option<SelectedModel>, CoreError> {
        let path = self.database_path.as_ref().clone();
        tokio::task::spawn_blocking(move || read_selected_model(&path))
            .await
            .map_err(|error| CoreError::BlockingTask(error.to_string()))?
    }

    pub async fn enqueue(&self, items: Vec<NewQueueItem>) -> Result<QueueSnapshot, CoreError> {
        let (reply, response) = oneshot::channel();
        self.sender()
            .send(WriterCommand::Enqueue { items, reply })
            .await
            .map_err(|_| CoreError::StoreClosed)?;
        response.await.map_err(|_| CoreError::StoreClosed)?
    }

    pub async fn reorder_queue(
        &self,
        item_ids: Vec<String>,
        expected_revision: u64,
    ) -> Result<QueueSnapshot, CoreError> {
        let (reply, response) = oneshot::channel();
        self.sender()
            .send(WriterCommand::ReorderQueue {
                item_ids,
                expected_revision,
                reply,
            })
            .await
            .map_err(|_| CoreError::StoreClosed)?;
        response.await.map_err(|_| CoreError::StoreClosed)?
    }

    pub async fn remove_queue_item(
        &self,
        item_id: impl Into<String>,
        expected_revision: u64,
    ) -> Result<QueueSnapshot, CoreError> {
        let (reply, response) = oneshot::channel();
        self.sender()
            .send(WriterCommand::RemoveQueueItem {
                item_id: item_id.into(),
                expected_revision,
                reply,
            })
            .await
            .map_err(|_| CoreError::StoreClosed)?;
        response.await.map_err(|_| CoreError::StoreClosed)?
    }

    pub async fn clear_queue(&self, expected_revision: u64) -> Result<QueueSnapshot, CoreError> {
        let (reply, response) = oneshot::channel();
        self.sender()
            .send(WriterCommand::ClearQueue {
                expected_revision,
                reply,
            })
            .await
            .map_err(|_| CoreError::StoreClosed)?;
        response.await.map_err(|_| CoreError::StoreClosed)?
    }

    pub async fn invalidate_queue_item(
        &self,
        item_id: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Result<QueueSnapshot, CoreError> {
        let (reply, response) = oneshot::channel();
        self.sender()
            .send(WriterCommand::InvalidateQueueItem {
                item_id: item_id.into(),
                code: code.into(),
                message: message.into(),
                reply,
            })
            .await
            .map_err(|_| CoreError::StoreClosed)?;
        response.await.map_err(|_| CoreError::StoreClosed)?
    }

    pub async fn claim_queue_item(
        &self,
        item_id: impl Into<String>,
        session: NewSession,
    ) -> Result<SessionMutationReceipt, CoreError> {
        let (reply, response) = oneshot::channel();
        self.sender()
            .send(WriterCommand::ClaimQueueItem {
                item_id: item_id.into(),
                session,
                reply,
            })
            .await
            .map_err(|_| CoreError::StoreClosed)?;
        response.await.map_err(|_| CoreError::StoreClosed)?
    }

    pub async fn queue_snapshot(&self) -> Result<QueueSnapshot, CoreError> {
        let path = self.database_path.as_ref().clone();
        tokio::task::spawn_blocking(move || {
            let connection = open_reader(&path)?;
            queue_snapshot_from(&connection)
        })
        .await
        .map_err(|error| CoreError::BlockingTask(error.to_string()))?
    }

    pub async fn resume_context(
        &self,
        session_id: impl Into<String>,
    ) -> Result<ResumeContext, CoreError> {
        let path = self.database_path.as_ref().clone();
        let session_id = session_id.into();
        tokio::task::spawn_blocking(move || read_resume_context(&path, &session_id))
            .await
            .map_err(|error| CoreError::BlockingTask(error.to_string()))?
    }

    pub async fn history(&self, query: HistoryQuery) -> Result<HistoryPage, CoreError> {
        let path = self.database_path.as_ref().clone();
        tokio::task::spawn_blocking(move || read_history(&path, &query))
            .await
            .map_err(|error| CoreError::BlockingTask(error.to_string()))?
    }

    pub async fn rename_session(
        &self,
        session_id: impl Into<String>,
        expected_row_version: u64,
        title: impl Into<String>,
    ) -> Result<SessionMutationReceipt, CoreError> {
        let (reply, response) = oneshot::channel();
        self.sender()
            .send(WriterCommand::RenameSession {
                session_id: session_id.into(),
                expected_row_version,
                title: title.into(),
                reply,
            })
            .await
            .map_err(|_| CoreError::StoreClosed)?;
        response.await.map_err(|_| CoreError::StoreClosed)?
    }

    pub async fn delete_sessions(&self, sessions: Vec<DeleteSession>) -> Result<usize, CoreError> {
        let (reply, response) = oneshot::channel();
        self.sender()
            .send(WriterCommand::DeleteSessions { sessions, reply })
            .await
            .map_err(|_| CoreError::StoreClosed)?;
        response.await.map_err(|_| CoreError::StoreClosed)?
    }

    fn sender(&self) -> &mpsc::Sender<WriterCommand> {
        self.inner
            .sender
            .as_ref()
            .expect("store sender exists while its shared inner value is alive")
    }
}

fn initialize_writer(path: &Path) -> Result<Connection, CoreError> {
    let existing = path.metadata().is_ok_and(|metadata| metadata.len() > 0);
    if existing {
        let probe = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        validate_existing_schema(&probe)?;
        verify_integrity(&probe)?;
    }

    let mut connection = Connection::open(path)?;
    configure_writer(&connection)?;
    if !existing {
        connection.execute_batch(SCHEMA)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.pragma_update(None, "user_version", APPLICATION_SCHEMA_VERSION)?;
        transaction.execute(
            "INSERT INTO app_metadata (key, value) VALUES ('schema_version', ?1)",
            [APPLICATION_SCHEMA_VERSION.to_string()],
        )?;
        transaction.commit()?;
        validate_existing_schema(&connection)?;
        verify_integrity(&connection)?;
    }
    Ok(connection)
}

fn configure_writer(connection: &Connection) -> Result<(), CoreError> {
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    Ok(())
}

fn open_reader(path: &Path) -> Result<Connection, CoreError> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "query_only", "ON")?;
    Ok(connection)
}

fn verify_integrity(connection: &Connection) -> Result<(), CoreError> {
    let foreign_key_failure = connection
        .query_row("PRAGMA foreign_key_check", [], |_| Ok(()))
        .optional()?;
    if foreign_key_failure.is_some() {
        return Err(CoreError::InvalidDatabase(
            "foreign_key_check reported a violation".into(),
        ));
    }
    let result =
        connection.pragma_query_value(None, "integrity_check", |row| row.get::<_, String>(0))?;
    if result != "ok" {
        return Err(CoreError::InvalidDatabase(format!(
            "integrity_check failed: {result}"
        )));
    }
    Ok(())
}

fn validate_existing_schema(connection: &Connection) -> Result<(), CoreError> {
    let version =
        connection.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))?;
    if version != APPLICATION_SCHEMA_VERSION {
        return Err(CoreError::UnsupportedSchema {
            found: version,
            expected: APPLICATION_SCHEMA_VERSION,
        });
    }

    for (object_type, name) in [
        ("table", "app_metadata"),
        ("table", "app_sessions"),
        ("table", "app_segments"),
        ("table", "app_session_search"),
        ("table", "app_queue_items"),
        ("index", "app_sessions_started_at"),
        ("index", "app_queue_items_position"),
    ] {
        let exists = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = ?1 AND name = ?2 AND sql IS NOT NULL",
                params![object_type, name],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(CoreError::InvalidDatabase(format!(
                "required {object_type} is missing: {name}"
            )));
        }
    }

    validate_columns(connection, "app_metadata", &["key", "value"])?;
    validate_columns(
        connection,
        "app_sessions",
        &[
            "session_id",
            "state",
            "end_reason",
            "title",
            "source_kind",
            "source_display_name",
            "source_fingerprint",
            "source_path",
            "source_device_id",
            "model",
            "model_revision",
            "language",
            "detected_languages_json",
            "sample_rate",
            "config_json",
            "started_at",
            "ended_at",
            "updated_at",
            "media_duration_ms",
            "total_segments",
            "recognized_segments",
            "characters",
            "error_code",
            "error_message",
            "resume_sample",
            "row_version",
        ],
    )?;
    validate_columns(
        connection,
        "app_segments",
        &[
            "session_id",
            "segment_index",
            "start_sample",
            "end_sample",
            "split_reason",
            "text",
            "raw_text",
            "language",
            "diagnostics_json",
        ],
    )?;
    validate_columns(
        connection,
        "app_queue_items",
        &[
            "item_id",
            "position",
            "display_name",
            "source_path",
            "source_fingerprint",
            "state",
            "error_code",
            "error_message",
            "created_at",
            "updated_at",
        ],
    )?;

    let sessions_sql = object_sql(connection, "table", "app_sessions")?;
    for fragment in [
        "'preparing','running','pausing','paused','stopping','completed','stopped','failed','abandoned'",
        "check (resume_sample >= 0)",
        "row_version integer not null default 1",
    ] {
        if !sessions_sql.contains(fragment) {
            return Err(CoreError::InvalidDatabase(format!(
                "app_sessions is missing required constraint: {fragment}"
            )));
        }
    }
    let segments_sql = object_sql(connection, "table", "app_segments")?;
    if !segments_sql.contains("references app_sessions(session_id) on delete cascade")
        || !segments_sql.contains("primary key (session_id, segment_index)")
    {
        return Err(CoreError::InvalidDatabase(
            "app_segments foreign key or primary key differs from schema v5".into(),
        ));
    }
    let fts_sql = object_sql(connection, "table", "app_session_search")?;
    if !fts_sql.contains("using fts5")
        || !fts_sql.contains("session_id unindexed")
        || !fts_sql.contains("tokenize='unicode61'")
    {
        return Err(CoreError::InvalidDatabase(
            "app_session_search is not the schema v5 FTS5 table".into(),
        ));
    }
    let started_index = object_sql(connection, "index", "app_sessions_started_at")?;
    if !started_index.contains("on app_sessions(started_at desc, session_id desc)") {
        return Err(CoreError::InvalidDatabase(
            "app_sessions_started_at has an unexpected definition".into(),
        ));
    }
    let queue_index = object_sql(connection, "index", "app_queue_items_position")?;
    if !queue_index.contains("create unique index")
        || !queue_index.contains("on app_queue_items(position)")
    {
        return Err(CoreError::InvalidDatabase(
            "app_queue_items_position has an unexpected definition".into(),
        ));
    }

    let metadata_version = connection
        .query_row(
            "SELECT value FROM app_metadata WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or(CoreError::MissingSchemaMetadata)?;
    if metadata_version != APPLICATION_SCHEMA_VERSION.to_string() {
        return Err(CoreError::InvalidDatabase(
            "app_metadata schema_version does not match PRAGMA user_version".into(),
        ));
    }

    if connection
        .query_row(
            r#"
            SELECT session_id FROM app_sessions
            WHERE json_valid(config_json) = 0 OR json_type(config_json) != 'object'
               OR json_valid(detected_languages_json) = 0
               OR json_type(detected_languages_json) != 'array'
               OR EXISTS (
                 SELECT 1 FROM json_each(app_sessions.detected_languages_json)
                 WHERE json_each.type != 'text' OR json_each.value = ''
               )
            LIMIT 1
            "#,
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .is_some()
    {
        return Err(CoreError::InvalidDatabase(
            "app_sessions contains invalid persisted JSON".into(),
        ));
    }
    if connection
        .query_row(
            "SELECT session_id FROM app_segments WHERE json_valid(diagnostics_json) = 0 OR json_type(diagnostics_json) != 'object' LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .is_some()
    {
        return Err(CoreError::InvalidDatabase(
            "app_segments contains invalid diagnostics JSON".into(),
        ));
    }
    if let Some(value) = connection
        .query_row(
            "SELECT value FROM app_metadata WHERE key = 'selected_model'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        let model: SelectedModel = serde_json::from_str(&value).map_err(|error| {
            CoreError::InvalidDatabase(format!("selected_model metadata is invalid: {error}"))
        })?;
        validate_selected_model(&model).map_err(|error| {
            CoreError::InvalidDatabase(format!("selected_model metadata is invalid: {error}"))
        })?;
    }
    if let Some(value) = connection
        .query_row(
            "SELECT value FROM app_metadata WHERE key = 'queue_revision'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        value.parse::<u64>().map_err(|_| {
            CoreError::InvalidDatabase("queue_revision metadata is not a u64".into())
        })?;
    }
    connection.query_row(
        "SELECT count(*) FROM app_session_search WHERE app_session_search MATCH 'schema_validation_token'",
        [],
        |_| Ok(()),
    )?;
    Ok(())
}

fn validate_columns(
    connection: &Connection,
    table: &'static str,
    expected: &[&str],
) -> Result<(), CoreError> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let actual = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    if actual != expected {
        return Err(CoreError::InvalidDatabase(format!(
            "{table} columns differ from schema v5: {actual:?}"
        )));
    }
    Ok(())
}

fn object_sql(
    connection: &Connection,
    object_type: &'static str,
    name: &'static str,
) -> Result<String, CoreError> {
    let sql = connection.query_row(
        "SELECT sql FROM sqlite_master WHERE type = ?1 AND name = ?2",
        params![object_type, name],
        |row| row.get::<_, String>(0),
    )?;
    Ok(sql
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" "))
}

fn validate_new_session(session: &NewSession) -> Result<(), CoreError> {
    for (name, value) in [
        ("session ID", session.session_id.as_str()),
        ("title", session.title.trim()),
        ("source display name", session.source_display_name.trim()),
        ("model", session.model.trim()),
    ] {
        if value.is_empty() {
            return Err(CoreError::InvalidArgument(format!(
                "{name} must not be empty"
            )));
        }
    }
    if session.title.chars().count() > 200 {
        return Err(CoreError::InvalidArgument(
            "session title must not exceed 200 characters".into(),
        ));
    }
    if !session.config.is_object() {
        return Err(CoreError::InvalidArgument(
            "session config must be a JSON object".into(),
        ));
    }
    if session.source_kind == SourceKind::File
        && (session.source_path.as_deref().is_none_or(str::is_empty)
            || session
                .source_fingerprint
                .as_deref()
                .is_none_or(str::is_empty))
    {
        return Err(CoreError::InvalidArgument(
            "file sessions require a private path and fingerprint".into(),
        ));
    }
    Ok(())
}

fn create_session(
    connection: &mut Connection,
    session: NewSession,
) -> Result<SessionMutationReceipt, CoreError> {
    validate_new_session(&session)?;
    let config = serde_json::to_string(&session.config)
        .map_err(|error| CoreError::InvalidArgument(error.to_string()))?;
    let language = session.language.as_deref().unwrap_or("Auto");
    let now = utc_now()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute(
        r#"
        INSERT INTO app_sessions (
          session_id, state, title, source_kind, source_display_name,
          source_fingerprint, source_path, source_device_id,
          model, model_revision, language, sample_rate,
          config_json, started_at, updated_at
        ) VALUES (
          ?1, 'preparing', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13
        )
        "#,
        params![
            session.session_id,
            session.title,
            session.source_kind.as_str(),
            session.source_display_name,
            session.source_fingerprint,
            session.source_path,
            session.source_device_id,
            session.model,
            session.model_revision,
            language,
            NORMALIZED_SAMPLE_RATE,
            config,
            now,
        ],
    )?;
    update_search(&transaction, &session.session_id)?;
    let receipt = mutation_receipt(&transaction, &session.session_id)?;
    transaction.commit()?;
    Ok(receipt)
}

fn apply_session_operation(
    connection: &mut Connection,
    session_id: &str,
    expected_row_version: u64,
    operation: SessionOperation,
) -> Result<SessionMutationReceipt, CoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (current_state, current_version, source_kind) =
        current_session_identity(&transaction, session_id)?;
    if current_version != expected_row_version {
        return Err(CoreError::StaleVersion {
            entity: "session",
            expected: expected_row_version,
            actual: current_version,
        });
    }
    let (allowed_states, next_state, end_reason, error_code, error_message, resume_sample) =
        match operation {
            SessionOperation::StartRunning => (
                vec![SessionState::Preparing],
                SessionState::Running,
                None,
                None,
                None,
                None,
            ),
            SessionOperation::BeginPause => (
                vec![SessionState::Running],
                SessionState::Pausing,
                None,
                None,
                None,
                None,
            ),
            SessionOperation::CommitPaused { resume_sample } => (
                vec![SessionState::Pausing],
                SessionState::Paused,
                Some("userPause".to_owned()),
                None,
                None,
                Some(resume_sample),
            ),
            SessionOperation::BeginStop => (
                vec![SessionState::Running],
                SessionState::Stopping,
                None,
                None,
                None,
                None,
            ),
            SessionOperation::CompleteRunning => (
                vec![SessionState::Running],
                SessionState::Completed,
                Some("endOfInput".to_owned()),
                None,
                None,
                None,
            ),
            SessionOperation::CompleteStopping => (
                vec![SessionState::Stopping],
                terminal_state_for_stop(source_kind),
                Some("userStop".to_owned()),
                None,
                None,
                None,
            ),
            SessionOperation::StopPaused => (
                vec![SessionState::Paused],
                terminal_state_for_stop(source_kind),
                Some("userStop".to_owned()),
                None,
                None,
                None,
            ),
            SessionOperation::CompleteLifecycleStop { reason } => (
                vec![SessionState::Stopping, SessionState::Paused],
                SessionState::Stopped,
                Some(reason.as_str().to_owned()),
                None,
                None,
                None,
            ),
            SessionOperation::ResumePaused => (
                vec![SessionState::Paused],
                SessionState::Preparing,
                None,
                None,
                None,
                None,
            ),
            SessionOperation::RetryFailed => {
                if source_kind != SourceKind::File {
                    return Err(CoreError::InvalidState {
                        entity: "session",
                        expected: "failed file session".into(),
                        actual: format!("{} {}", current_state.as_str(), source_kind.as_str()),
                    });
                }
                (
                    vec![SessionState::Failed],
                    SessionState::Preparing,
                    None,
                    None,
                    None,
                    None,
                )
            }
            SessionOperation::Fail { code, message } => (
                vec![
                    SessionState::Preparing,
                    SessionState::Running,
                    SessionState::Pausing,
                    SessionState::Stopping,
                ],
                SessionState::Failed,
                Some(code.clone()),
                Some(code),
                Some(message),
                None,
            ),
        };
    if !allowed_states.contains(&current_state) {
        return Err(CoreError::InvalidState {
            entity: "session",
            expected: allowed_states
                .iter()
                .map(|state| state.as_str())
                .collect::<Vec<_>>()
                .join(" or "),
            actual: current_state.as_str().into(),
        });
    }
    let now = utc_now()?;
    let resume_sample = resume_sample.map(to_sql_u64).transpose()?;
    transaction.execute(
        r#"
        UPDATE app_sessions
        SET state = ?1,
            end_reason = ?2,
            error_code = ?3,
            error_message = ?4,
            resume_sample = CASE
              WHEN ?10 THEN MAX(
                resume_sample,
                COALESCE((SELECT MAX(end_sample) FROM app_segments WHERE app_segments.session_id = app_sessions.session_id), 0)
              )
              ELSE COALESCE(?5, resume_sample)
            END,
            ended_at = CASE WHEN ?6 THEN ?7 ELSE NULL END,
            updated_at = ?7,
            row_version = row_version + 1
        WHERE session_id = ?8 AND row_version = ?9
        "#,
        params![
            next_state.as_str(),
            end_reason,
            error_code,
            error_message,
            resume_sample,
            next_state.is_terminal(),
            now,
            session_id,
            to_sql_u64(expected_row_version)?,
            next_state == SessionState::Failed,
        ],
    )?;
    if next_state.is_terminal() || next_state == SessionState::Paused {
        update_search(&transaction, session_id)?;
    }
    let receipt = mutation_receipt(&transaction, session_id)?;
    transaction.commit()?;
    Ok(receipt)
}

fn append_segment(
    connection: &mut Connection,
    session_id: &str,
    expected_row_version: u64,
    segment: &NewSegment,
) -> Result<SegmentMutationReceipt, CoreError> {
    segment.validate()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (state, current_version) = current_state(&transaction, session_id)?;
    if current_version != expected_row_version {
        return Err(CoreError::StaleVersion {
            entity: "session",
            expected: expected_row_version,
            actual: current_version,
        });
    }
    if !matches!(
        state,
        SessionState::Preparing
            | SessionState::Running
            | SessionState::Pausing
            | SessionState::Stopping
    ) {
        return Err(CoreError::InvalidState {
            entity: "session",
            expected: "writable".into(),
            actual: state.as_str().into(),
        });
    }
    let expected_index = transaction.query_row(
        "SELECT total_segments FROM app_sessions WHERE session_id = ?1",
        [session_id],
        |row| row.get::<_, u32>(0),
    )?;
    if segment.index != expected_index {
        return Err(CoreError::InvalidArgument(format!(
            "segment index must be contiguous: expected {expected_index}, received {}",
            segment.index
        )));
    }

    let diagnostics = serde_json::to_string(segment)
        .map_err(|error| CoreError::InvalidArgument(error.to_string()))?;
    transaction.execute(
        r#"
        INSERT INTO app_segments (
          session_id, segment_index, start_sample, end_sample, split_reason,
          text, raw_text, language, diagnostics_json
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
        "#,
        params![
            session_id,
            segment.index,
            to_sql_u64(segment.start_sample)?,
            to_sql_u64(segment.end_sample)?,
            segment.split_reason.as_str(),
            segment.text,
            segment.raw_text,
            segment.language,
            diagnostics,
        ],
    )?;

    let mut detected_languages: Vec<String> = transaction
        .query_row(
            "SELECT detected_languages_json FROM app_sessions WHERE session_id = ?1",
            [session_id],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|value| serde_json::from_str(&value).ok())
        .unwrap_or_default();
    if !detected_languages
        .iter()
        .any(|value| value == &segment.language)
    {
        detected_languages.push(segment.language.clone());
    }
    let detected_languages = serde_json::to_string(&detected_languages)
        .map_err(|error| CoreError::InvalidArgument(error.to_string()))?;
    let now = utc_now()?;

    transaction.execute(
        r#"
        UPDATE app_sessions
        SET total_segments = total_segments + 1,
            recognized_segments = recognized_segments + CASE WHEN ?1 != '' THEN 1 ELSE 0 END,
            characters = characters + length(?1),
            media_duration_ms = MAX(media_duration_ms, CAST(?2 * 1000 / sample_rate AS INTEGER)),
            detected_languages_json = ?3,
            updated_at = ?4,
            row_version = row_version + 1
        WHERE session_id = ?5 AND row_version = ?6
        "#,
        params![
            segment.text,
            to_sql_u64(segment.end_sample)?,
            detected_languages,
            now,
            session_id,
            to_sql_u64(expected_row_version)?,
        ],
    )?;
    let receipt = segment_receipt(&transaction, session_id, segment)?;
    transaction.commit()?;
    Ok(receipt)
}

fn recover_startup(connection: &mut Connection) -> Result<usize, CoreError> {
    let now = utc_now()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let session_ids = {
        let mut statement = transaction.prepare(
            "SELECT session_id FROM app_sessions WHERE state IN ('preparing','running','pausing','stopping')",
        )?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    transaction.execute(
        r#"
        UPDATE app_sessions
        SET state = 'abandoned', end_reason = 'hostRestart', error_code = 'hostRestart',
            error_message = 'The previous application process ended before the session reached a durable terminal state',
            ended_at = ?1, updated_at = ?1, row_version = row_version + 1
        WHERE state IN ('preparing','running','pausing','stopping')
        "#,
        [&now],
    )?;
    for session_id in &session_ids {
        update_search(&transaction, session_id)?;
    }
    transaction.commit()?;
    Ok(session_ids.len())
}

fn validate_selected_model(model: &SelectedModel) -> Result<(), CoreError> {
    if model.repo_id.trim().is_empty() || model.revision.trim().is_empty() {
        return Err(CoreError::InvalidArgument(
            "model repository and revision must not be empty".into(),
        ));
    }
    Ok(())
}

fn set_selected_model(connection: &mut Connection, model: &SelectedModel) -> Result<(), CoreError> {
    validate_selected_model(model)?;
    let value = serde_json::to_string(model)
        .map_err(|error| CoreError::InvalidArgument(error.to_string()))?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute(
        "INSERT OR REPLACE INTO app_metadata (key, value) VALUES ('selected_model', ?1)",
        [value],
    )?;
    transaction.commit()?;
    Ok(())
}

fn read_selected_model(path: &Path) -> Result<Option<SelectedModel>, CoreError> {
    let connection = open_reader(path)?;
    let value = connection
        .query_row(
            "SELECT value FROM app_metadata WHERE key = 'selected_model'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    value
        .map(|value| {
            serde_json::from_str(&value).map_err(|error| {
                CoreError::InvalidDatabase(format!("selected_model metadata is invalid: {error}"))
            })
        })
        .transpose()
}

fn queue_revision(connection: &Connection) -> Result<u64, CoreError> {
    let value = connection
        .query_row(
            "SELECT value FROM app_metadata WHERE key = 'queue_revision'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    value.map_or(Ok(0), |value| {
        value
            .parse()
            .map_err(|_| CoreError::InvalidDatabase("queue_revision metadata is not a u64".into()))
    })
}

fn increment_queue_revision(transaction: &Transaction<'_>) -> Result<u64, CoreError> {
    let revision = queue_revision(transaction)?.checked_add(1).ok_or_else(|| {
        CoreError::InvalidDatabase("queue_revision reached its maximum value".into())
    })?;
    transaction.execute(
        "INSERT OR REPLACE INTO app_metadata (key, value) VALUES ('queue_revision', ?1)",
        [revision.to_string()],
    )?;
    Ok(revision)
}

fn queue_snapshot_from(connection: &Connection) -> Result<QueueSnapshot, CoreError> {
    let revision = queue_revision(connection)?;
    let mut statement = connection.prepare(
        r#"
        SELECT item_id, display_name, state, error_code, error_message, created_at, updated_at
        FROM app_queue_items ORDER BY position, item_id
        "#,
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
        ))
    })?;
    let mut items = Vec::new();
    for row in rows {
        let (item_id, display_name, state, error_code, error_message, added_at, updated_at) = row?;
        items.push(QueueItem {
            item_id,
            display_name,
            state: QueueItemState::parse(&state)?,
            error_code,
            error_message,
            added_at,
            updated_at,
        });
    }
    Ok(QueueSnapshot { revision, items })
}

fn enqueue(
    connection: &mut Connection,
    items: &[NewQueueItem],
) -> Result<QueueSnapshot, CoreError> {
    if items.is_empty() {
        return queue_snapshot_from(connection);
    }
    let mut identifiers = std::collections::HashSet::new();
    for item in items {
        if !identifiers.insert(item.item_id.as_str())
            || [
                item.item_id.as_str(),
                item.display_name.as_str(),
                item.source_path.as_str(),
                item.source_fingerprint.as_str(),
            ]
            .iter()
            .any(|value| value.trim().is_empty())
        {
            return Err(CoreError::InvalidArgument(
                "queue items require unique IDs and complete private source metadata".into(),
            ));
        }
    }
    let now = utc_now()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let next_position = transaction.query_row(
        "SELECT COALESCE(MAX(position), -1) + 1 FROM app_queue_items",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    let next_position = from_sql_u64(next_position)?;
    for (offset, item) in items.iter().enumerate() {
        transaction.execute(
            r#"
            INSERT INTO app_queue_items (
              item_id, position, display_name, source_path, source_fingerprint,
              state, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?6)
            "#,
            params![
                item.item_id,
                to_sql_u64(next_position + offset as u64)?,
                item.display_name,
                item.source_path,
                item.source_fingerprint,
                now,
            ],
        )?;
    }
    increment_queue_revision(&transaction)?;
    let snapshot = queue_snapshot_from(&transaction)?;
    transaction.commit()?;
    Ok(snapshot)
}

fn reorder_queue(
    connection: &mut Connection,
    item_ids: &[String],
    expected_revision: u64,
) -> Result<QueueSnapshot, CoreError> {
    let requested: std::collections::HashSet<_> = item_ids.iter().collect();
    if requested.len() != item_ids.len() {
        return Err(CoreError::InvalidArgument(
            "queue order contains duplicate item IDs".into(),
        ));
    }
    let now = utc_now()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current_revision = queue_revision(&transaction)?;
    if current_revision != expected_revision {
        return Err(CoreError::StaleVersion {
            entity: "queue",
            expected: expected_revision,
            actual: current_revision,
        });
    }
    let current = {
        let mut statement = transaction.prepare("SELECT item_id FROM app_queue_items")?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<std::collections::HashSet<_>, _>>()?
    };
    if requested != current.iter().collect() {
        return Err(CoreError::InvalidArgument(
            "queue order must contain every current item exactly once".into(),
        ));
    }
    let offset = transaction.query_row(
        "SELECT COALESCE(MAX(position), -1) + COUNT(*) + 1 FROM app_queue_items",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    transaction.execute(
        "UPDATE app_queue_items SET position = position + ?1",
        [offset],
    )?;
    for (position, item_id) in item_ids.iter().enumerate() {
        transaction.execute(
            "UPDATE app_queue_items SET position = ?1, updated_at = ?2 WHERE item_id = ?3",
            params![to_sql_u64(position as u64)?, now, item_id],
        )?;
    }
    increment_queue_revision(&transaction)?;
    let snapshot = queue_snapshot_from(&transaction)?;
    transaction.commit()?;
    Ok(snapshot)
}

fn remove_queue_item(
    connection: &mut Connection,
    item_id: &str,
    expected_revision: u64,
) -> Result<QueueSnapshot, CoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    require_queue_revision(&transaction, expected_revision)?;
    if transaction.execute("DELETE FROM app_queue_items WHERE item_id = ?1", [item_id])? == 0 {
        return Err(CoreError::NotFound {
            entity: "queue item",
            id: item_id.into(),
        });
    }
    increment_queue_revision(&transaction)?;
    let snapshot = queue_snapshot_from(&transaction)?;
    transaction.commit()?;
    Ok(snapshot)
}

fn clear_queue(
    connection: &mut Connection,
    expected_revision: u64,
) -> Result<QueueSnapshot, CoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    require_queue_revision(&transaction, expected_revision)?;
    if transaction.execute("DELETE FROM app_queue_items", [])? > 0 {
        increment_queue_revision(&transaction)?;
    }
    let snapshot = queue_snapshot_from(&transaction)?;
    transaction.commit()?;
    Ok(snapshot)
}

fn require_queue_revision(
    transaction: &Transaction<'_>,
    expected_revision: u64,
) -> Result<(), CoreError> {
    let actual = queue_revision(transaction)?;
    if actual != expected_revision {
        return Err(CoreError::StaleVersion {
            entity: "queue",
            expected: expected_revision,
            actual,
        });
    }
    Ok(())
}

fn invalidate_queue_item(
    connection: &mut Connection,
    item_id: &str,
    code: &str,
    message: &str,
) -> Result<QueueSnapshot, CoreError> {
    if code.trim().is_empty() || message.trim().is_empty() {
        return Err(CoreError::InvalidArgument(
            "queue failure code and message must not be empty".into(),
        ));
    }
    let now = utc_now()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if transaction.execute(
        "UPDATE app_queue_items SET state = 'invalid', error_code = ?1, error_message = ?2, updated_at = ?3 WHERE item_id = ?4",
        params![code, message, now, item_id],
    )? == 0
    {
        return Err(CoreError::NotFound {
            entity: "queue item",
            id: item_id.into(),
        });
    }
    increment_queue_revision(&transaction)?;
    let snapshot = queue_snapshot_from(&transaction)?;
    transaction.commit()?;
    Ok(snapshot)
}

fn claim_queue_item(
    connection: &mut Connection,
    item_id: &str,
    mut session: NewSession,
) -> Result<SessionMutationReceipt, CoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let item = transaction
        .query_row(
            "SELECT display_name, source_path, source_fingerprint, state FROM app_queue_items WHERE item_id = ?1",
            [item_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| CoreError::NotFound {
            entity: "queue item",
            id: item_id.into(),
        })?;
    if item.3 != QueueItemState::Pending.as_str() {
        return Err(CoreError::InvalidState {
            entity: "queue item",
            expected: QueueItemState::Pending.as_str().into(),
            actual: item.3,
        });
    }
    session.source_kind = SourceKind::File;
    session.source_display_name = item.0;
    session.source_path = Some(item.1);
    session.source_fingerprint = Some(item.2);
    session.source_device_id = None;
    validate_new_session(&session)?;
    let now = utc_now()?;
    let config = serde_json::to_string(&session.config)
        .map_err(|error| CoreError::InvalidArgument(error.to_string()))?;
    transaction.execute(
        r#"
        INSERT INTO app_sessions (
          session_id, state, title, source_kind, source_display_name,
          source_fingerprint, source_path, source_device_id,
          model, model_revision, language, sample_rate, config_json, started_at, updated_at
        ) VALUES (?1, 'preparing', ?2, 'file', ?3, ?4, ?5, NULL, ?6, ?7, ?8, ?9, ?10, ?11, ?11)
        "#,
        params![
            session.session_id,
            session.title,
            session.source_display_name,
            session.source_fingerprint,
            session.source_path,
            session.model,
            session.model_revision,
            session.language.as_deref().unwrap_or("Auto"),
            NORMALIZED_SAMPLE_RATE,
            config,
            now,
        ],
    )?;
    transaction.execute("DELETE FROM app_queue_items WHERE item_id = ?1", [item_id])?;
    increment_queue_revision(&transaction)?;
    update_search(&transaction, &session.session_id)?;
    let receipt = mutation_receipt(&transaction, &session.session_id)?;
    transaction.commit()?;
    Ok(receipt)
}

fn rename_session(
    connection: &mut Connection,
    session_id: &str,
    expected_row_version: u64,
    title: &str,
) -> Result<SessionMutationReceipt, CoreError> {
    let title = title.trim();
    if title.is_empty() || title.chars().count() > 200 {
        return Err(CoreError::InvalidArgument(
            "session title must contain 1..=200 characters".into(),
        ));
    }
    let now = utc_now()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (_, current_version) = current_state(&transaction, session_id)?;
    if current_version != expected_row_version {
        return Err(CoreError::StaleVersion {
            entity: "session",
            expected: expected_row_version,
            actual: current_version,
        });
    }
    transaction.execute(
        "UPDATE app_sessions SET title = ?1, updated_at = ?2, row_version = row_version + 1 WHERE session_id = ?3 AND row_version = ?4",
        params![title, now, session_id, to_sql_u64(expected_row_version)?],
    )?;
    update_search(&transaction, session_id)?;
    let receipt = mutation_receipt(&transaction, session_id)?;
    transaction.commit()?;
    Ok(receipt)
}

fn delete_sessions(
    connection: &mut Connection,
    sessions: &[DeleteSession],
) -> Result<usize, CoreError> {
    let identifiers: std::collections::HashSet<_> =
        sessions.iter().map(|session| &session.session_id).collect();
    if identifiers.len() != sessions.len()
        || sessions.is_empty()
        || sessions
            .iter()
            .any(|session| session.session_id.trim().is_empty())
    {
        return Err(CoreError::InvalidArgument(
            "history deletion requires unique session IDs".into(),
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for session in sessions {
        let (state, actual_version) = current_state(&transaction, &session.session_id)?;
        if actual_version != session.expected_row_version {
            return Err(CoreError::StaleVersion {
                entity: "session",
                expected: session.expected_row_version,
                actual: actual_version,
            });
        }
        if !state.is_terminal() && state != SessionState::Paused {
            return Err(CoreError::InvalidState {
                entity: "session",
                expected: "paused or terminal".into(),
                actual: state.as_str().into(),
            });
        }
    }
    for session in sessions {
        transaction.execute(
            "DELETE FROM app_session_search WHERE session_id = ?1",
            [&session.session_id],
        )?;
        let deleted = transaction.execute(
            "DELETE FROM app_sessions WHERE session_id = ?1 AND row_version = ?2",
            params![
                session.session_id,
                to_sql_u64(session.expected_row_version)?
            ],
        )?;
        if deleted != 1 {
            return Err(CoreError::StaleVersion {
                entity: "session",
                expected: session.expected_row_version,
                actual: current_state(&transaction, &session.session_id)?.1,
            });
        }
    }
    transaction.commit()?;
    Ok(sessions.len())
}

fn read_session_snapshot(
    path: &Path,
    session_id: &str,
    segment_offset: u32,
    segment_limit: u32,
) -> Result<SessionSnapshot, CoreError> {
    let mut connection = open_reader(path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
    let raw = transaction
        .query_row(
            r#"
            SELECT state, title, source_kind, source_display_name, model, model_revision,
              language, detected_languages_json, sample_rate, started_at, ended_at, updated_at,
              media_duration_ms, total_segments, recognized_segments, characters, error_code,
              error_message, resume_sample, row_version
            FROM app_sessions WHERE session_id = ?1
            "#,
            [session_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, u32>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, i64>(12)?,
                    row.get::<_, u32>(13)?,
                    row.get::<_, u32>(14)?,
                    row.get::<_, i64>(15)?,
                    row.get::<_, Option<String>>(16)?,
                    row.get::<_, Option<String>>(17)?,
                    row.get::<_, i64>(18)?,
                    row.get::<_, i64>(19)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| CoreError::NotFound {
            entity: "session",
            id: session_id.into(),
        })?;

    let mut statement = transaction.prepare(
        r#"
        SELECT segment_index, start_sample, end_sample, split_reason, text, raw_text,
          language, diagnostics_json
        FROM app_segments
        WHERE session_id = ?1
        ORDER BY segment_index
        LIMIT ?2 OFFSET ?3
        "#,
    )?;
    let rows = statement.query_map(
        params![session_id, segment_limit + 1, segment_offset],
        |row| {
            Ok((
                row.get::<_, u32>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        },
    )?;
    let mut segments = Vec::new();
    for row in rows {
        let (index, start_sample, end_sample, split_reason, text, raw_text, language, diagnostics) =
            row?;
        segments.push(SegmentRecord {
            index,
            start_sample: from_sql_u64(start_sample)?,
            end_sample: from_sql_u64(end_sample)?,
            split_reason: SplitReason::parse(&split_reason)?,
            text,
            raw_text,
            language,
            diagnostics: serde_json::from_str(&diagnostics).map_err(|error| {
                CoreError::Database(rusqlite::Error::FromSqlConversionFailure(
                    diagnostics.len(),
                    rusqlite::types::Type::Text,
                    Box::new(error),
                ))
            })?,
        });
    }
    drop(statement);
    transaction.commit()?;
    let has_more = segments.len() > segment_limit as usize;
    if has_more {
        segments.truncate(segment_limit as usize);
    }
    Ok(SessionSnapshot {
        session_id: session_id.into(),
        state: SessionState::parse(&raw.0)?,
        title: raw.1,
        source_kind: SourceKind::parse(&raw.2)?,
        source_display_name: raw.3,
        model: raw.4,
        model_revision: raw.5,
        language: raw.6,
        detected_languages: parse_string_array(&raw.7, "detected_languages_json")?,
        sample_rate: raw.8,
        started_at: raw.9,
        ended_at: raw.10,
        updated_at: raw.11,
        media_duration_ms: from_sql_u64(raw.12)?,
        total_segments: raw.13,
        recognized_segments: raw.14,
        characters: from_sql_u64(raw.15)?,
        error_code: raw.16,
        error_message: raw.17,
        resume_sample: from_sql_u64(raw.18)?,
        row_version: from_sql_u64(raw.19)?,
        segments,
        next_segment_offset: has_more.then_some(segment_offset + segment_limit),
    })
}

fn read_resume_context(path: &Path, session_id: &str) -> Result<ResumeContext, CoreError> {
    let connection = open_reader(path)?;
    let raw = connection
        .query_row(
            r#"
            SELECT state, source_kind, source_path, source_device_id, source_fingerprint,
              model, model_revision, language, resume_sample, total_segments, row_version
            FROM app_sessions WHERE session_id = ?1
            "#,
            [session_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, u32>(9)?,
                    row.get::<_, i64>(10)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| CoreError::NotFound {
            entity: "session",
            id: session_id.into(),
        })?;
    let state = SessionState::parse(&raw.0)?;
    let source_kind = SourceKind::parse(&raw.1)?;
    if state != SessionState::Paused
        && !(state == SessionState::Failed && source_kind == SourceKind::File)
    {
        return Err(CoreError::InvalidState {
            entity: "session",
            expected: "paused or failed file".into(),
            actual: format!("{} {}", state.as_str(), source_kind.as_str()),
        });
    }
    Ok(ResumeContext {
        session_id: session_id.into(),
        state,
        source_kind,
        source_path: raw.2,
        source_device_id: raw.3,
        source_fingerprint: raw.4,
        model: raw.5,
        model_revision: raw.6,
        language: raw.7,
        resume_sample: from_sql_u64(raw.8)?,
        next_segment_index: raw.9,
        row_version: from_sql_u64(raw.10)?,
    })
}

fn read_history(path: &Path, query: &HistoryQuery) -> Result<HistoryPage, CoreError> {
    let limit = if query.limit == 0 { 50 } else { query.limit };
    if limit > 100
        || query
            .text
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
    {
        return Err(CoreError::InvalidArgument(
            "history query must use a non-empty search and a limit no greater than 100".into(),
        ));
    }
    let connection = open_reader(path)?;
    let mut parameters = Vec::<rusqlite::types::Value>::new();
    let mut clauses = Vec::new();
    let searched = query.text.is_some();
    if let Some(text) = &query.text {
        clauses.push("app_session_search MATCH ?".to_owned());
        parameters.push(text.clone().into());
    }
    if !query.states.is_empty() {
        clauses.push(format!(
            "s.state IN ({})",
            std::iter::repeat_n("?", query.states.len())
                .collect::<Vec<_>>()
                .join(",")
        ));
        parameters.extend(
            query
                .states
                .iter()
                .map(|state| rusqlite::types::Value::Text(state.as_str().into())),
        );
    }
    if !query.source_kinds.is_empty() {
        clauses.push(format!(
            "s.source_kind IN ({})",
            std::iter::repeat_n("?", query.source_kinds.len())
                .collect::<Vec<_>>()
                .join(",")
        ));
        parameters.extend(
            query
                .source_kinds
                .iter()
                .map(|source_kind| rusqlite::types::Value::Text(source_kind.as_str().into())),
        );
    }
    match (&query.sort, &query.cursor) {
        (
            HistorySort::Newest,
            Some(HistoryCursor::Time {
                started_at,
                session_id,
            }),
        ) => {
            clauses.push("(s.started_at < ? OR (s.started_at = ? AND s.session_id < ?))".into());
            parameters.extend([
                started_at.clone().into(),
                started_at.clone().into(),
                session_id.clone().into(),
            ]);
        }
        (
            HistorySort::Oldest,
            Some(HistoryCursor::Time {
                started_at,
                session_id,
            }),
        ) => {
            clauses.push("(s.started_at > ? OR (s.started_at = ? AND s.session_id > ?))".into());
            parameters.extend([
                started_at.clone().into(),
                started_at.clone().into(),
                session_id.clone().into(),
            ]);
        }
        (
            HistorySort::Longest,
            Some(HistoryCursor::Longest {
                media_duration_ms,
                started_at,
                session_id,
            }),
        ) => {
            clauses.push(
                "(s.media_duration_ms < ? OR (s.media_duration_ms = ? AND (s.started_at < ? OR (s.started_at = ? AND s.session_id < ?))))".into(),
            );
            parameters.extend([
                to_sql_u64(*media_duration_ms)?.into(),
                to_sql_u64(*media_duration_ms)?.into(),
                started_at.clone().into(),
                started_at.clone().into(),
                session_id.clone().into(),
            ]);
        }
        (_, None) => {}
        _ => {
            return Err(CoreError::InvalidArgument(
                "history cursor does not match the selected sort".into(),
            ));
        }
    }
    parameters.push(i64::from(limit + 1).into());
    let from = if searched {
        "app_session_search JOIN app_sessions AS s USING (session_id)"
    } else {
        "app_sessions AS s"
    };
    let where_clause = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    let order = match query.sort {
        HistorySort::Newest => "s.started_at DESC, s.session_id DESC",
        HistorySort::Oldest => "s.started_at ASC, s.session_id ASC",
        HistorySort::Longest => "s.media_duration_ms DESC, s.started_at DESC, s.session_id DESC",
    };
    let sql = format!(
        r#"
        SELECT s.session_id, s.state, s.title, s.source_kind, s.source_display_name,
          s.model, s.model_revision, s.language, s.detected_languages_json,
          s.started_at, s.ended_at, s.updated_at, s.media_duration_ms,
          s.total_segments, s.recognized_segments, s.characters, s.error_code,
          s.error_message, s.resume_sample, s.row_version,
          (SELECT text FROM app_segments
             WHERE app_segments.session_id = s.session_id AND text != ''
             ORDER BY segment_index LIMIT 1) AS snippet
        FROM {from} {where_clause}
        ORDER BY {order} LIMIT ?
        "#
    );
    struct RawHistoryRow {
        session_id: String,
        state: String,
        title: String,
        source_kind: String,
        source_display_name: String,
        model: String,
        model_revision: Option<String>,
        language: String,
        detected_languages_json: String,
        started_at: String,
        ended_at: Option<String>,
        updated_at: String,
        media_duration_ms: i64,
        total_segments: u32,
        recognized_segments: u32,
        characters: i64,
        error_code: Option<String>,
        error_message: Option<String>,
        resume_sample: i64,
        row_version: i64,
        snippet: Option<String>,
    }
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(rusqlite::params_from_iter(parameters), |row| {
        Ok(RawHistoryRow {
            session_id: row.get(0)?,
            state: row.get(1)?,
            title: row.get(2)?,
            source_kind: row.get(3)?,
            source_display_name: row.get(4)?,
            model: row.get(5)?,
            model_revision: row.get(6)?,
            language: row.get(7)?,
            detected_languages_json: row.get(8)?,
            started_at: row.get(9)?,
            ended_at: row.get(10)?,
            updated_at: row.get(11)?,
            media_duration_ms: row.get(12)?,
            total_segments: row.get(13)?,
            recognized_segments: row.get(14)?,
            characters: row.get(15)?,
            error_code: row.get(16)?,
            error_message: row.get(17)?,
            resume_sample: row.get(18)?,
            row_version: row.get(19)?,
            snippet: row.get(20)?,
        })
    })?;
    let mut result = Vec::new();
    for row in rows {
        let row = row?;
        result.push(SessionSummary {
            session_id: row.session_id,
            state: SessionState::parse(&row.state)?,
            title: row.title,
            source_kind: SourceKind::parse(&row.source_kind)?,
            source_display_name: row.source_display_name,
            model: row.model,
            model_revision: row.model_revision,
            language: row.language,
            detected_languages: parse_string_array(
                &row.detected_languages_json,
                "detected_languages_json",
            )?,
            started_at: row.started_at,
            ended_at: row.ended_at,
            updated_at: row.updated_at,
            media_duration_ms: from_sql_u64(row.media_duration_ms)?,
            total_segments: row.total_segments,
            recognized_segments: row.recognized_segments,
            characters: from_sql_u64(row.characters)?,
            error_code: row.error_code,
            error_message: row.error_message,
            resume_sample: from_sql_u64(row.resume_sample)?,
            snippet: row.snippet,
            row_version: from_sql_u64(row.row_version)?,
        });
    }
    let has_more = result.len() > limit as usize;
    if has_more {
        result.truncate(limit as usize);
    }
    let next_cursor = has_more.then(|| {
        let last = result
            .last()
            .expect("a history page with an extra row has at least one returned row");
        match query.sort {
            HistorySort::Newest | HistorySort::Oldest => HistoryCursor::Time {
                started_at: last.started_at.clone(),
                session_id: last.session_id.clone(),
            },
            HistorySort::Longest => HistoryCursor::Longest {
                media_duration_ms: last.media_duration_ms,
                started_at: last.started_at.clone(),
                session_id: last.session_id.clone(),
            },
        }
    });
    Ok(HistoryPage {
        items: result,
        next_cursor,
    })
}

fn current_state(
    transaction: &Transaction<'_>,
    session_id: &str,
) -> Result<(SessionState, u64), CoreError> {
    let value = transaction
        .query_row(
            "SELECT state, row_version FROM app_sessions WHERE session_id = ?1",
            [session_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?
        .ok_or_else(|| CoreError::NotFound {
            entity: "session",
            id: session_id.into(),
        })?;
    Ok((SessionState::parse(&value.0)?, from_sql_u64(value.1)?))
}

fn current_session_identity(
    transaction: &Transaction<'_>,
    session_id: &str,
) -> Result<(SessionState, u64, SourceKind), CoreError> {
    let value = transaction
        .query_row(
            "SELECT state, row_version, source_kind FROM app_sessions WHERE session_id = ?1",
            [session_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| CoreError::NotFound {
            entity: "session",
            id: session_id.into(),
        })?;
    Ok((
        SessionState::parse(&value.0)?,
        from_sql_u64(value.1)?,
        SourceKind::parse(&value.2)?,
    ))
}

fn mutation_receipt(
    transaction: &Transaction<'_>,
    session_id: &str,
) -> Result<SessionMutationReceipt, CoreError> {
    let value = transaction.query_row(
        r#"
        SELECT state, row_version, total_segments, recognized_segments, characters,
          media_duration_ms, ended_at
        FROM app_sessions WHERE session_id = ?1
        "#,
        [session_id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, u32>(2)?,
                row.get::<_, u32>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        },
    )?;
    Ok(SessionMutationReceipt {
        session_id: session_id.into(),
        state: SessionState::parse(&value.0)?,
        row_version: from_sql_u64(value.1)?,
        total_segments: value.2,
        recognized_segments: value.3,
        characters: from_sql_u64(value.4)?,
        media_duration_ms: from_sql_u64(value.5)?,
        ended_at: value.6,
    })
}

fn segment_receipt(
    transaction: &Transaction<'_>,
    session_id: &str,
    segment: &NewSegment,
) -> Result<SegmentMutationReceipt, CoreError> {
    let mutation = mutation_receipt(transaction, session_id)?;
    Ok(SegmentMutationReceipt {
        session_id: session_id.into(),
        segment: SegmentRecord {
            index: segment.index,
            start_sample: segment.start_sample,
            end_sample: segment.end_sample,
            split_reason: segment.split_reason,
            text: segment.text.clone(),
            raw_text: segment.raw_text.clone(),
            language: segment.language.clone(),
            diagnostics: serde_json::to_value(segment)
                .map_err(|error| CoreError::InvalidArgument(error.to_string()))?,
        },
        row_version: mutation.row_version,
        total_segments: mutation.total_segments,
        recognized_segments: mutation.recognized_segments,
        characters: mutation.characters,
        media_duration_ms: mutation.media_duration_ms,
    })
}

fn update_search(transaction: &Transaction<'_>, session_id: &str) -> Result<(), CoreError> {
    let title = transaction
        .query_row(
            "SELECT title FROM app_sessions WHERE session_id = ?1",
            [session_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(title) = title else {
        return Ok(());
    };
    let mut statement = transaction
        .prepare("SELECT text FROM app_segments WHERE session_id = ?1 ORDER BY segment_index")?;
    let rows = statement.query_map([session_id], |row| row.get::<_, String>(0))?;
    let text = rows.collect::<Result<Vec<_>, _>>()?.join("\n");
    transaction.execute(
        "DELETE FROM app_session_search WHERE session_id = ?1",
        [session_id],
    )?;
    transaction.execute(
        "INSERT INTO app_session_search (session_id, title, text) VALUES (?1, ?2, ?3)",
        params![session_id, title, text],
    )?;
    Ok(())
}

const fn terminal_state_for_stop(source_kind: SourceKind) -> SessionState {
    match source_kind {
        SourceKind::File => SessionState::Stopped,
        SourceKind::Microphone | SourceKind::SystemAudio => SessionState::Completed,
    }
}

fn utc_now() -> Result<String, CoreError> {
    OffsetDateTime::now_utc()
        .format(format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:6]+00:00"
        ))
        .map_err(|error| CoreError::BlockingTask(format!("could not format UTC time: {error}")))
}

fn to_sql_u64(value: u64) -> Result<i64, CoreError> {
    i64::try_from(value)
        .map_err(|_| CoreError::InvalidArgument("integer exceeds SQLite range".into()))
}

fn from_sql_u64(value: i64) -> Result<u64, CoreError> {
    u64::try_from(value)
        .map_err(|_| CoreError::Database(rusqlite::Error::IntegralValueOutOfRange(0, value)))
}

fn parse_string_array(value: &str, field: &str) -> Result<Vec<String>, CoreError> {
    serde_json::from_str(value)
        .map_err(|error| CoreError::InvalidDatabase(format!("{field} is invalid: {error}")))
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS app_metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS app_sessions (
  session_id TEXT PRIMARY KEY,
  state TEXT NOT NULL CHECK (state IN (
    'preparing','running','pausing','paused','stopping','completed','stopped','failed','abandoned'
  )),
  end_reason TEXT,
  title TEXT NOT NULL,
  source_kind TEXT NOT NULL,
  source_display_name TEXT NOT NULL,
  source_fingerprint TEXT,
  source_path TEXT,
  source_device_id TEXT,
  model TEXT NOT NULL,
  model_revision TEXT,
  language TEXT NOT NULL,
  detected_languages_json TEXT NOT NULL DEFAULT '[]',
  sample_rate INTEGER NOT NULL CHECK (sample_rate > 0),
  config_json TEXT NOT NULL DEFAULT '{}',
  started_at TEXT NOT NULL,
  ended_at TEXT,
  updated_at TEXT NOT NULL,
  media_duration_ms INTEGER NOT NULL DEFAULT 0,
  total_segments INTEGER NOT NULL DEFAULT 0,
  recognized_segments INTEGER NOT NULL DEFAULT 0,
  characters INTEGER NOT NULL DEFAULT 0,
  error_code TEXT,
  error_message TEXT,
  resume_sample INTEGER NOT NULL DEFAULT 0 CHECK (resume_sample >= 0),
  row_version INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS app_sessions_started_at ON app_sessions(started_at DESC, session_id DESC);
CREATE TABLE IF NOT EXISTS app_segments (
  session_id TEXT NOT NULL REFERENCES app_sessions(session_id) ON DELETE CASCADE,
  segment_index INTEGER NOT NULL CHECK (segment_index >= 0),
  start_sample INTEGER NOT NULL CHECK (start_sample >= 0),
  end_sample INTEGER NOT NULL CHECK (end_sample > start_sample),
  split_reason TEXT NOT NULL,
  text TEXT NOT NULL,
  raw_text TEXT,
  language TEXT NOT NULL,
  diagnostics_json TEXT NOT NULL DEFAULT '{}',
  PRIMARY KEY (session_id, segment_index)
);
CREATE VIRTUAL TABLE IF NOT EXISTS app_session_search USING fts5(
  session_id UNINDEXED, title, text, tokenize='unicode61'
);
CREATE TABLE IF NOT EXISTS app_queue_items (
  item_id TEXT PRIMARY KEY,
  position INTEGER NOT NULL CHECK (position >= 0),
  display_name TEXT NOT NULL,
  source_path TEXT NOT NULL,
  source_fingerprint TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('pending','invalid')),
  error_code TEXT,
  error_message TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS app_queue_items_position ON app_queue_items(position);
"#;

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;
    use crate::application_core::domain::{TranscriptionDiagnostics, VadDiagnostics};

    fn new_session(id: &str) -> NewSession {
        NewSession {
            session_id: id.into(),
            source_kind: SourceKind::File,
            source_display_name: "sample.wav".into(),
            source_fingerprint: Some("sha256:deadbeef".into()),
            source_path: Some("/private/sample.wav".into()),
            source_device_id: None,
            model: "owner/model".into(),
            model_revision: Some("revision".into()),
            language: None,
            title: "Sample".into(),
            config: json!({"temperature": 0}),
        }
    }

    fn segment(index: u32, start_sample: u64) -> NewSegment {
        NewSegment {
            index,
            start_sample,
            end_sample: start_sample + 512,
            split_reason: SplitReason::Silence,
            text: "hello".into(),
            raw_text: None,
            language: "English".into(),
            vad: VadDiagnostics {
                mean_probability: 0.5,
                peak_probability: 0.9,
                speech_ratio: 0.75,
            },
            transcription: TranscriptionDiagnostics {
                max_tokens: 64,
                ..TranscriptionDiagnostics::default()
            },
            decode_ms: 12,
            queue_wait_ms: 3,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn writer_commits_segments_atomically_and_reads_a_snapshot() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("reco.sqlite3")).unwrap();
        let created = store
            .create_session(new_session("session-one"))
            .await
            .unwrap();
        assert_eq!(created.row_version, 1);

        let running = store.start_running("session-one", 1).await.unwrap();
        let committed = store
            .append_segment("session-one", running.row_version, segment(0, 0))
            .await
            .unwrap();
        assert_eq!(committed.row_version, 3);

        let snapshot = store.session_snapshot("session-one", 0, 500).await.unwrap();
        assert_eq!(snapshot.row_version, 3);
        assert_eq!(snapshot.total_segments, 1);
        assert_eq!(snapshot.segments[0].text, "hello");
        assert!(snapshot.next_segment_offset.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_cas_and_noncontiguous_segment_leave_the_database_unchanged() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("reco.sqlite3")).unwrap();
        store
            .create_session(new_session("session-two"))
            .await
            .unwrap();

        let error = store.start_running("session-two", 99).await.unwrap_err();
        assert!(matches!(error, CoreError::StaleVersion { .. }));

        let error = store
            .append_segment("session-two", 1, segment(2, 0))
            .await
            .unwrap_err();
        assert!(matches!(error, CoreError::InvalidArgument(_)));
        let snapshot = store.session_snapshot("session-two", 0, 500).await.unwrap();
        assert_eq!(snapshot.row_version, 1);
        assert_eq!(snapshot.total_segments, 0);
    }

    #[test]
    fn rejects_non_v5_database_without_migrating_it() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("legacy.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection.pragma_update(None, "user_version", 4).unwrap();
        connection
            .execute("CREATE TABLE legacy (id INTEGER)", [])
            .unwrap();
        drop(connection);

        assert!(matches!(
            Store::open(path),
            Err(CoreError::UnsupportedSchema {
                found: 4,
                expected: APPLICATION_SCHEMA_VERSION
            })
        ));
    }

    #[test]
    fn rejects_damaged_v5_schema_and_json_without_repairing_them() {
        let directory = tempdir().unwrap();
        let missing_index = directory.path().join("missing-index.sqlite3");
        let connection = Connection::open(&missing_index).unwrap();
        connection
            .execute_batch(include_str!("../../../fixtures/native/schema-v5.sql"))
            .unwrap();
        connection
            .execute("DROP INDEX app_sessions_started_at", [])
            .unwrap();
        drop(connection);
        assert!(matches!(
            Store::open(&missing_index),
            Err(CoreError::InvalidDatabase(_))
        ));
        let connection =
            Connection::open_with_flags(&missing_index, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'app_sessions_started_at'",
                    [],
                    |row| row.get::<_, u32>(0),
                )
                .unwrap(),
            0
        );

        let invalid_json = directory.path().join("invalid-json.sqlite3");
        let connection = Connection::open(&invalid_json).unwrap();
        connection
            .execute_batch(include_str!("../../../fixtures/native/schema-v5.sql"))
            .unwrap();
        connection
            .execute(
                "UPDATE app_sessions SET config_json = '[]' WHERE session_id = 'session-file'",
                [],
            )
            .unwrap();
        drop(connection);
        assert!(matches!(
            Store::open(&invalid_json),
            Err(CoreError::InvalidDatabase(_))
        ));
        let connection =
            Connection::open_with_flags(&invalid_json, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT config_json FROM app_sessions WHERE session_id = 'session-file'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "[]"
        );
    }

    #[test]
    fn bundled_sqlite_supports_the_schema_fts_table() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("reco.sqlite3");
        let store = Store::open(&path).unwrap();
        drop(store);
        let connection = open_reader(&path).unwrap();
        let result = connection
            .query_row(
                "SELECT count(*) FROM app_session_search WHERE app_session_search MATCH 'sample'",
                [],
                |row| row.get::<_, u32>(0),
            )
            .unwrap();
        assert_eq!(result, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn opens_the_canonical_v5_fixture_without_repairing_its_data() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("fixture.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(include_str!("../../../fixtures/native/schema-v5.sql"))
            .unwrap();
        let before = fixture_data_signature(&connection);
        drop(connection);

        let store = Store::open(&path).unwrap();
        let resume = store.resume_context("session-paused").await.unwrap();
        assert_eq!(resume.resume_sample, 16_000);
        assert_eq!(resume.next_segment_index, 1);

        let queue = store.queue_snapshot().await.unwrap();
        assert_eq!(queue.revision, 7);
        assert_eq!(queue.items.len(), 2);
        assert_eq!(queue.items[1].state, QueueItemState::Invalid);

        let model = store.selected_model().await.unwrap().unwrap();
        assert_eq!(model.revision, "0123456789abcdef");

        let first = store
            .history(HistoryQuery {
                text: Some("hello".into()),
                states: vec![SessionState::Completed],
                source_kinds: vec![SourceKind::File],
                sort: HistorySort::Longest,
                cursor: None,
                limit: 1,
            })
            .await
            .unwrap();
        assert_eq!(first.items[0].session_id, "session-file");
        assert_eq!(first.items[0].detected_languages, ["Japanese"]);
        assert_eq!(first.items[0].snippet.as_deref(), Some("hello"));
        drop(store);

        let connection =
            Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert_eq!(fixture_data_signature(&connection), before);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn history_uses_limit_plus_one_and_sort_specific_cursors() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("fixture.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(include_str!("../../../fixtures/native/schema-v5.sql"))
            .unwrap();
        drop(connection);
        let store = Store::open(path).unwrap();

        let first = store
            .history(HistoryQuery {
                sort: HistorySort::Longest,
                limit: 1,
                ..HistoryQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(first.items[0].session_id, "session-file");
        assert!(matches!(
            first.next_cursor,
            Some(HistoryCursor::Longest { .. })
        ));
        let second = store
            .history(HistoryQuery {
                sort: HistorySort::Longest,
                cursor: first.next_cursor,
                limit: 1,
                ..HistoryQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(second.items[0].session_id, "session-paused");
        assert!(second.next_cursor.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn queue_mutations_and_history_deletion_reject_stale_versions() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("reco.sqlite3")).unwrap();
        let queue = store
            .enqueue(vec![NewQueueItem {
                item_id: "queue-one".into(),
                display_name: "queued.wav".into(),
                source_path: "/private/queued.wav".into(),
                source_fingerprint: "sha256:queued".into(),
            }])
            .await
            .unwrap();
        assert!(matches!(
            store
                .remove_queue_item("queue-one", queue.revision - 1)
                .await,
            Err(CoreError::StaleVersion {
                entity: "queue",
                ..
            })
        ));
        let queue = store
            .remove_queue_item("queue-one", queue.revision)
            .await
            .unwrap();
        assert!(matches!(
            store.clear_queue(queue.revision - 1).await,
            Err(CoreError::StaleVersion {
                entity: "queue",
                ..
            })
        ));

        let created = store
            .create_session(new_session("delete-me"))
            .await
            .unwrap();
        let running = store
            .start_running("delete-me", created.row_version)
            .await
            .unwrap();
        let completed = store
            .complete_running("delete-me", running.row_version)
            .await
            .unwrap();
        assert!(matches!(
            store
                .delete_sessions(vec![DeleteSession {
                    session_id: "delete-me".into(),
                    expected_row_version: completed.row_version - 1,
                }])
                .await,
            Err(CoreError::StaleVersion {
                entity: "session",
                ..
            })
        ));
        assert_eq!(
            store
                .delete_sessions(vec![DeleteSession {
                    session_id: "delete-me".into(),
                    expected_row_version: completed.row_version,
                }])
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn recovery_preserves_paused_sessions_and_lifecycle_stop_is_always_stopped() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("reco.sqlite3")).unwrap();
        store
            .create_session(new_session("abandoned"))
            .await
            .unwrap();
        let paused = store.create_session(new_session("paused")).await.unwrap();
        let running = store
            .start_running("paused", paused.row_version)
            .await
            .unwrap();
        let pausing = store
            .begin_pause("paused", running.row_version)
            .await
            .unwrap();
        let paused = store
            .commit_paused("paused", pausing.row_version, 512)
            .await
            .unwrap();
        assert_eq!(store.recover_startup().await.unwrap(), 1);
        assert_eq!(
            store.session_snapshot("paused", 0, 1).await.unwrap().state,
            SessionState::Paused
        );
        let stopped = store
            .complete_lifecycle_stop(
                "paused",
                paused.row_version,
                LifecycleStopReason::SystemSleep,
            )
            .await
            .unwrap();
        assert_eq!(stopped.state, SessionState::Stopped);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn persisted_timestamps_are_python_compatible_and_lexically_ordered() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("fixture.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(include_str!("../../../fixtures/native/schema-v5.sql"))
            .unwrap();
        drop(connection);
        let store = Store::open(path).unwrap();
        store
            .create_session(new_session("new-session"))
            .await
            .unwrap();
        let page = store
            .history(HistoryQuery {
                sort: HistorySort::Newest,
                limit: 100,
                ..HistoryQuery::default()
            })
            .await
            .unwrap();
        let timestamp = &page.items[0].started_at;
        assert_eq!(page.items[0].session_id, "new-session");
        assert_eq!(timestamp.len(), 32);
        assert_eq!(&timestamp[4..5], "-");
        assert_eq!(&timestamp[10..11], "T");
        assert_eq!(&timestamp[19..20], ".");
        assert_eq!(&timestamp[26..], "+00:00");
        assert!(timestamp.as_str() > "2026-01-03T03:04:05.000000+00:00");
    }

    fn fixture_data_signature(connection: &Connection) -> (i64, i64, String, String, String) {
        (
            connection
                .query_row("SELECT COUNT(*) FROM app_sessions", [], |row| row.get(0))
                .unwrap(),
            connection
                .query_row("SELECT COUNT(*) FROM app_segments", [], |row| row.get(0))
                .unwrap(),
            connection
                .query_row(
                    "SELECT state || ':' || row_version FROM app_sessions WHERE session_id = 'session-paused'",
                    [],
                    |row| row.get(0),
                )
                .unwrap(),
            connection
                .query_row(
                    "SELECT value FROM app_metadata WHERE key = 'selected_model'",
                    [],
                    |row| row.get(0),
                )
                .unwrap(),
            connection
                .query_row(
                    "SELECT group_concat(item_id || ':' || state, ',') FROM (SELECT item_id, state FROM app_queue_items ORDER BY position)",
                    [],
                    |row| row.get(0),
                )
                .unwrap(),
        )
    }
}
