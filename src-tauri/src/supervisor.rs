use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    pin::Pin,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use serde_json::Value;
use tauri::{AppHandle, Emitter};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{Mutex, RwLock, oneshot},
    time::timeout,
};
use uuid::Uuid;

use crate::{
    paths::AppPaths,
    protocol::{
        EngineConnectionState, EngineStateSnapshot, HostEngineEvent, IncomingEnvelope,
        IncomingType, MAX_LINE_BYTES, PROTOCOL_VERSION, ProtocolError, RequestEnvelope,
    },
};

const EVENT_CHANNEL: &str = "engine://event";
const STATE_CHANNEL: &str = "engine://state";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);
const RESTART_WINDOW: Duration = Duration::from_secs(60);
const MAX_RESTARTS: usize = 3;

type PendingRequests = HashMap<String, oneshot::Sender<Result<Value, SupervisorError>>>;

#[derive(Debug, Error, Clone)]
pub enum SupervisorError {
    #[error("the Reco engine is not available: {0}")]
    Unavailable(String),
    #[error("failed to communicate with the Reco engine: {0}")]
    Io(String),
    #[error("the Reco engine did not respond in time")]
    Timeout,
    #[error("the Reco engine rejected the request: {code}: {message}")]
    Rejected { code: String, message: String },
    #[error("the Reco engine returned an invalid protocol message: {0}")]
    Protocol(String),
}

impl From<std::io::Error> for SupervisorError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

struct Runtime {
    child: Arc<Mutex<Child>>,
    stdin: Arc<Mutex<ChildStdin>>,
    pid: u32,
}

struct SupervisorInner {
    app: AppHandle,
    paths: AppPaths,
    start_lock: Mutex<()>,
    runtime: Mutex<Option<Runtime>>,
    pending: Mutex<PendingRequests>,
    state: RwLock<EngineConnectionState>,
    sequence: AtomicU64,
    last_incoming_sequence: AtomicU64,
    last_activity: Mutex<Option<Instant>>,
    restart_attempts: Mutex<VecDeque<Instant>>,
    intentionally_stopping: RwLock<bool>,
}

#[derive(Clone)]
pub struct EngineSupervisor {
    inner: Arc<SupervisorInner>,
}

impl EngineSupervisor {
    pub fn new(app: AppHandle, paths: AppPaths) -> Self {
        Self {
            inner: Arc::new(SupervisorInner {
                app,
                paths,
                start_lock: Mutex::new(()),
                runtime: Mutex::new(None),
                pending: Mutex::new(HashMap::new()),
                state: RwLock::new(EngineConnectionState::Offline),
                sequence: AtomicU64::new(0),
                last_incoming_sequence: AtomicU64::new(0),
                last_activity: Mutex::new(None),
                restart_attempts: Mutex::new(VecDeque::new()),
                intentionally_stopping: RwLock::new(false),
            }),
        }
    }

    pub fn paths(&self) -> &AppPaths {
        &self.inner.paths
    }

    pub async fn is_running(&self) -> bool {
        self.inner.runtime.lock().await.is_some()
    }

    pub async fn state_snapshot(&self) -> EngineStateSnapshot {
        let state = *self.inner.state.read().await;
        let pid = self
            .inner
            .runtime
            .lock()
            .await
            .as_ref()
            .map(|runtime| runtime.pid);
        let last_heartbeat_age_ms = self
            .inner
            .last_activity
            .lock()
            .await
            .map(|last| last.elapsed().as_millis().min(u128::from(u64::MAX)) as u64);
        EngineStateSnapshot {
            state,
            pid,
            last_heartbeat_age_ms,
        }
    }

    pub fn ensure_started(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<(), SupervisorError>> + Send + '_>> {
        Box::pin(async move {
            let _start_guard = self.inner.start_lock.lock().await;
            if self.inner.runtime.lock().await.is_some() {
                return Ok(());
            }
            if !self.inner.paths.engine_executable.is_file() {
                return Err(SupervisorError::Unavailable(format!(
                    "expected bundled executable {}",
                    self.inner.paths.engine_executable.display()
                )));
            }
            self.set_state(EngineConnectionState::Starting).await;
            *self.inner.intentionally_stopping.write().await = false;

            let mut child = Command::new(&self.inner.paths.engine_executable)
                .arg("serve")
                .arg("--protocol-version")
                .arg(PROTOCOL_VERSION.to_string())
                .arg("--database")
                .arg(&self.inner.paths.database)
                .arg("--models-directory")
                .arg(&self.inner.paths.models)
                .arg("--logs-directory")
                .arg(&self.inner.paths.logs)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()?;
            let pid = child
                .id()
                .ok_or_else(|| SupervisorError::Unavailable("engine PID is unavailable".into()))?;
            let stdin = child.stdin.take().ok_or_else(|| {
                SupervisorError::Unavailable("engine stdin is unavailable".into())
            })?;
            let stdout = child.stdout.take().ok_or_else(|| {
                SupervisorError::Unavailable("engine stdout is unavailable".into())
            })?;
            let stderr = child.stderr.take().ok_or_else(|| {
                SupervisorError::Unavailable("engine stderr is unavailable".into())
            })?;
            let child = Arc::new(Mutex::new(child));
            self.inner
                .last_incoming_sequence
                .store(0, Ordering::Relaxed);
            *self.inner.runtime.lock().await = Some(Runtime {
                child: child.clone(),
                stdin: Arc::new(Mutex::new(stdin)),
                pid,
            });
            *self.inner.last_activity.lock().await = Some(Instant::now());
            self.set_state(EngineConnectionState::Ready).await;

            let reader_supervisor = self.clone();
            tauri::async_runtime::spawn(async move {
                reader_supervisor.read_stdout(stdout).await;
            });
            let stderr_app = self.inner.app.clone();
            tauri::async_runtime::spawn(async move {
                drain_stderr(stderr_app, stderr).await;
            });
            let monitor_supervisor = self.clone();
            tauri::async_runtime::spawn(async move {
                monitor_supervisor.monitor(child).await;
            });
            Ok(())
        })
    }

    pub async fn request(
        &self,
        command: &str,
        session_id: Option<String>,
        payload: Value,
    ) -> Result<Value, SupervisorError> {
        self.ensure_started().await?;
        let request_id = Uuid::new_v4().to_string();
        let sequence = self.inner.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        let envelope =
            RequestEnvelope::new(request_id.clone(), session_id, sequence, command, payload);
        let mut encoded = serde_json::to_vec(&envelope)
            .map_err(|error| SupervisorError::Protocol(error.to_string()))?;
        if encoded.len() > MAX_LINE_BYTES {
            return Err(SupervisorError::Protocol("request exceeds 8 MiB".into()));
        }
        encoded.push(b'\n');
        let (sender, receiver) = oneshot::channel();
        self.inner
            .pending
            .lock()
            .await
            .insert(request_id.clone(), sender);

        let write_result = async {
            let runtime = self.inner.runtime.lock().await;
            let stdin = runtime
                .as_ref()
                .ok_or_else(|| SupervisorError::Unavailable("engine exited".into()))?
                .stdin
                .clone();
            drop(runtime);
            let mut stdin = stdin.lock().await;
            stdin.write_all(&encoded).await?;
            stdin.flush().await?;
            Ok::<(), SupervisorError>(())
        }
        .await;
        if let Err(error) = write_result {
            self.inner.pending.lock().await.remove(&request_id);
            return Err(error);
        }

        match timeout(REQUEST_TIMEOUT, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(SupervisorError::Unavailable("engine exited".into())),
            Err(_) => {
                self.inner.pending.lock().await.remove(&request_id);
                Err(SupervisorError::Timeout)
            }
        }
    }

    pub async fn shutdown(&self) {
        *self.inner.intentionally_stopping.write().await = true;
        if self.inner.runtime.lock().await.is_none() {
            return;
        }
        let _ = timeout(
            Duration::from_secs(5),
            self.request("engine.shutdown", None, serde_json::json!({})),
        )
        .await;
        if let Some(runtime) = self.inner.runtime.lock().await.as_ref() {
            let _ = runtime.child.lock().await.kill().await;
        }
    }

    pub async fn force_terminate(&self) {
        *self.inner.intentionally_stopping.write().await = true;
        if let Some(runtime) = self.inner.runtime.lock().await.as_ref() {
            let _ = runtime.child.lock().await.kill().await;
        }
    }

    async fn read_stdout(&self, stdout: tokio::process::ChildStdout) {
        let mut reader = BufReader::new(stdout);
        let mut buffer = Vec::new();
        loop {
            buffer.clear();
            match reader.read_until(b'\n', &mut buffer).await {
                Ok(0) => return,
                Ok(_) if buffer.len() > MAX_LINE_BYTES => {
                    self.emit_protocol_failure("engine output exceeds 8 MiB")
                        .await;
                    continue;
                }
                Ok(_) => {}
                Err(error) => {
                    self.emit_protocol_failure(&format!("failed to read engine output: {error}"))
                        .await;
                    return;
                }
            }
            let line = match std::str::from_utf8(&buffer) {
                Ok(line) => line.trim_end(),
                Err(_) => {
                    self.emit_protocol_failure("engine output is not UTF-8")
                        .await;
                    continue;
                }
            };
            let envelope: IncomingEnvelope = match serde_json::from_str(line) {
                Ok(envelope) => envelope,
                Err(error) => {
                    self.emit_protocol_failure(&format!("invalid engine JSON: {error}"))
                        .await;
                    continue;
                }
            };
            if let Err(error) = self.validate_incoming(&envelope) {
                self.emit_protocol_failure(&error.to_string()).await;
                continue;
            }
            *self.inner.last_activity.lock().await = Some(Instant::now());
            if *self.inner.state.read().await == EngineConnectionState::Unresponsive {
                self.set_state(EngineConnectionState::Ready).await;
            }
            self.route_incoming(envelope).await;
        }
    }

    fn validate_incoming(&self, envelope: &IncomingEnvelope) -> Result<(), SupervisorError> {
        if envelope.protocol_version != PROTOCOL_VERSION {
            return Err(SupervisorError::Protocol(format!(
                "unsupported protocol version {}",
                envelope.protocol_version
            )));
        }
        let previous = self.inner.last_incoming_sequence.load(Ordering::Relaxed);
        if envelope.sequence <= previous {
            return Err(SupervisorError::Protocol(format!(
                "non-monotonic sequence {} after {}",
                envelope.sequence, previous
            )));
        }
        self.inner
            .last_incoming_sequence
            .store(envelope.sequence, Ordering::Relaxed);
        Ok(())
    }

    async fn route_incoming(&self, envelope: IncomingEnvelope) {
        match envelope.message_type {
            IncomingType::Response => {
                let Some(request_id) = envelope.request_id else {
                    self.emit_protocol_failure("response has no requestId")
                        .await;
                    return;
                };
                let Some(sender) = self.inner.pending.lock().await.remove(&request_id) else {
                    return;
                };
                let result = if envelope.ok == Some(true) {
                    Ok(envelope.payload)
                } else {
                    let error = envelope.error.unwrap_or(ProtocolError {
                        code: "ENGINE_UNKNOWN_ERROR".into(),
                        message: "The engine rejected the request".into(),
                        recoverable: false,
                        details: None,
                    });
                    Err(SupervisorError::Rejected {
                        code: error.code,
                        message: error.message,
                    })
                };
                let _ = sender.send(result);
            }
            IncomingType::Event => {
                let event = HostEngineEvent {
                    event: envelope
                        .event
                        .unwrap_or_else(|| "protocol.unknownEvent".into()),
                    session_id: envelope.session_id,
                    sequence: envelope.sequence,
                    payload: envelope.payload,
                };
                let _ = self.inner.app.emit(EVENT_CHANNEL, event);
            }
        }
    }

    async fn monitor(&self, child: Arc<Mutex<Child>>) {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            match child.lock().await.try_wait() {
                Ok(Some(status)) => {
                    self.handle_exit(format!("engine exited with {status}"))
                        .await;
                    return;
                }
                Ok(None) => {}
                Err(error) => {
                    self.handle_exit(format!("failed to inspect engine: {error}"))
                        .await;
                    return;
                }
            }
            if self
                .inner
                .last_activity
                .lock()
                .await
                .is_some_and(|last| last.elapsed() > HEARTBEAT_TIMEOUT)
            {
                self.set_state(EngineConnectionState::Unresponsive).await;
            }
        }
    }

    async fn handle_exit(&self, reason: String) {
        *self.inner.runtime.lock().await = None;
        let pending = std::mem::take(&mut *self.inner.pending.lock().await);
        for (_, sender) in pending {
            let _ = sender.send(Err(SupervisorError::Unavailable(reason.clone())));
        }
        self.set_state(EngineConnectionState::Offline).await;
        let _ = self.inner.app.emit(
            EVENT_CHANNEL,
            HostEngineEvent {
                event: "engine.exited".into(),
                session_id: None,
                sequence: 0,
                payload: serde_json::json!({ "reason": reason }),
            },
        );
        if *self.inner.intentionally_stopping.read().await {
            return;
        }

        let now = Instant::now();
        let mut attempts = self.inner.restart_attempts.lock().await;
        while attempts
            .front()
            .is_some_and(|attempt| now.duration_since(*attempt) > RESTART_WINDOW)
        {
            attempts.pop_front();
        }
        if attempts.len() >= MAX_RESTARTS {
            drop(attempts);
            self.set_state(EngineConnectionState::RestartLimitReached)
                .await;
            return;
        }
        attempts.push_back(now);
        drop(attempts);
        let supervisor = self.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let _ = supervisor.ensure_started().await;
        });
    }

    async fn set_state(&self, state: EngineConnectionState) {
        let mut current = self.inner.state.write().await;
        if *current == state {
            return;
        }
        *current = state;
        drop(current);
        let _ = self
            .inner
            .app
            .emit(STATE_CHANNEL, self.state_snapshot().await);
    }

    async fn emit_protocol_failure(&self, message: &str) {
        let _ = self.inner.app.emit(
            EVENT_CHANNEL,
            HostEngineEvent {
                event: "operation.failed".into(),
                session_id: None,
                sequence: 0,
                payload: serde_json::json!({
                    "error": {
                        "code": "HOST_PROTOCOL_ERROR",
                        "message": message,
                        "recoverable": false
                    }
                }),
            },
        );
    }
}

async fn drain_stderr(app: AppHandle, stderr: tokio::process::ChildStderr) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        eprintln!("[reco-engine] {line}");
        let _ = app.emit("engine://log", line);
    }
}
