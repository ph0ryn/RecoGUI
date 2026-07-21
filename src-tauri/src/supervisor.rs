use std::{
    collections::{HashMap, VecDeque},
    ffi::OsStr,
    future::Future,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
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
            if !self
                .inner
                .paths
                .engine_project
                .join("pyproject.toml")
                .is_file()
            {
                return Err(SupervisorError::Unavailable(format!(
                    "expected bundled engine project {}",
                    self.inner.paths.engine_project.display()
                )));
            }
            self.set_state(EngineConnectionState::Starting).await;
            *self.inner.intentionally_stopping.write().await = false;

            let uv_executable = resolve_uv_executable().ok_or_else(|| {
                SupervisorError::Unavailable("uv installation was not found".into())
            })?;
            let mut child = engine_command(&self.inner.paths, &uv_executable).spawn()?;
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
            *self.inner.last_activity.lock().await = None;

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
        let is_starting = *self.inner.state.read().await == EngineConnectionState::Starting;
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

        if is_starting {
            return match receiver.await {
                Ok(result) => result,
                Err(_) => Err(SupervisorError::Unavailable("engine exited".into())),
            };
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
            self.request("queue.pause", None, serde_json::json!({})),
        )
        .await;
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
            if matches!(
                *self.inner.state.read().await,
                EngineConnectionState::Starting | EngineConnectionState::Unresponsive
            ) {
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

fn engine_command(paths: &AppPaths, uv_executable: &Path) -> Command {
    let mut command = Command::new(uv_executable);
    command
        .arg("run")
        .arg("--project")
        .arg(&paths.engine_project)
        .arg("--frozen")
        .arg("--no-dev")
        .arg("reco-engine")
        .arg("serve")
        .arg("--protocol-version")
        .arg(PROTOCOL_VERSION.to_string())
        .arg("--database")
        .arg(&paths.database)
        .arg("--assets-directory")
        .arg(&paths.assets)
        .arg("--logs-directory")
        .arg(&paths.logs)
        .env("UV_PROJECT_ENVIRONMENT", &paths.engine_environment)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

fn resolve_uv_executable() -> Option<PathBuf> {
    resolve_uv_executable_from(
        std::env::var_os("PATH").as_deref(),
        std::env::var_os("HOME").as_deref().map(Path::new),
        is_executable_file,
    )
}

fn resolve_uv_executable_from(
    path: Option<&OsStr>,
    home: Option<&Path>,
    is_executable: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let mut candidates = path
        .into_iter()
        .flat_map(std::env::split_paths)
        .map(|directory| directory.join("uv"))
        .collect::<Vec<_>>();

    if let Some(home) = home {
        candidates.push(home.join(".local/bin/uv"));
        candidates.push(home.join(".nix-profile/bin/uv"));
        if let Some(user_name) = home.file_name() {
            candidates.push(
                Path::new("/etc/profiles/per-user")
                    .join(user_name)
                    .join("bin/uv"),
            );
        }
    }

    candidates.extend(
        [
            "/opt/homebrew/bin/uv",
            "/usr/local/bin/uv",
            "/opt/local/bin/uv",
        ]
        .into_iter()
        .map(PathBuf::from),
    );
    candidates
        .into_iter()
        .find(|candidate| is_executable(candidate))
}

fn is_executable_file(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

async fn drain_stderr(app: AppHandle, stderr: tokio::process::ChildStderr) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        eprintln!("[reco-engine] {line}");
        let _ = app.emit("engine://log", line);
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, path::PathBuf};

    use super::*;

    #[test]
    fn engine_command_uses_frozen_runtime_dependencies() {
        let root = PathBuf::from("/tmp/reco-app-data");
        let paths = AppPaths {
            root: root.clone(),
            database: root.join("reco.sqlite3"),
            backups: root.join("backups"),
            assets: root.join("assets"),
            logs: root.join("logs"),
            engine_lock: root.join("engine.lock"),
            engine_environment: root.join("python-env"),
            engine_project: PathBuf::from(
                "/Applications/RecoGUI.app/Contents/Resources/src-python",
            ),
        };

        let uv_executable = PathBuf::from("/opt/homebrew/bin/uv");
        let command = engine_command(&paths, &uv_executable);
        let command = command.as_std();
        let arguments = command.get_args().collect::<Vec<_>>();
        let project_argument = arguments
            .iter()
            .position(|argument| *argument == OsStr::new("--project"))
            .and_then(|index| arguments.get(index + 1));

        assert_eq!(command.get_program(), uv_executable.as_os_str());
        assert_eq!(project_argument, Some(&paths.engine_project.as_os_str()));
        assert!(arguments.contains(&OsStr::new("--frozen")));
        assert!(arguments.contains(&OsStr::new("--no-dev")));
        assert_eq!(
            command
                .get_envs()
                .find(|(key, _)| *key == OsStr::new("UV_PROJECT_ENVIRONMENT"))
                .and_then(|(_, value)| value),
            Some(paths.engine_environment.as_os_str())
        );
    }

    #[test]
    fn resolves_uv_from_nix_profile_when_finder_path_is_minimal() {
        let finder_path = std::env::join_paths(["/usr/bin", "/bin", "/usr/sbin", "/sbin"]).unwrap();
        let expected = PathBuf::from("/etc/profiles/per-user/test-user/bin/uv");

        let resolved = resolve_uv_executable_from(
            Some(&finder_path),
            Some(Path::new("/Users/test-user")),
            |candidate| candidate == expected,
        );

        assert_eq!(resolved, Some(expected));
    }

    #[test]
    fn resolves_uv_from_standard_user_install_location() {
        let expected = PathBuf::from("/Users/test-user/.local/bin/uv");

        let resolved = resolve_uv_executable_from(
            Some(OsString::from("/usr/bin:/bin").as_os_str()),
            Some(Path::new("/Users/test-user")),
            |candidate| candidate == expected,
        );

        assert_eq!(resolved, Some(expected));
    }

    #[test]
    fn prefers_uv_available_on_process_path() {
        let path = std::env::join_paths(["/custom/bin", "/usr/bin"]).unwrap();
        let expected = PathBuf::from("/custom/bin/uv");

        let resolved = resolve_uv_executable_from(
            Some(&path),
            Some(Path::new("/Users/test-user")),
            |candidate| candidate == expected,
        );

        assert_eq!(resolved, Some(expected));
    }
}
