use std::{
    ffi::OsStr,
    fs,
    os::{
        fd::OwnedFd,
        unix::{fs::PermissionsExt, net::UnixStream as StdUnixStream},
    },
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use command_fds::{CommandFdExt, FdMapping};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    net::UnixStream,
    process::{Child, Command},
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::{Instant, MissedTickBehavior, interval_at, sleep_until, timeout},
};

use super::{
    ModelLoadRequest, ModelLoadResult, ModelUnloadRequest, ModelUnloadResult, ModelsListRequest,
    ModelsListResult, RequestMetadata, SegmentTranscribeRequest, SegmentTranscriptionResult,
    ShutdownRequest, ShutdownResult, WorkerHello, WorkerOperation, WorkerSupervisor,
};
use crate::application_core::error::CoreError;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const DRIVER_CHANNEL_CAPACITY: usize = 16;
const DIAGNOSTIC_CHANNEL_CAPACITY: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerProcessConfig {
    pub project_directory: PathBuf,
    pub environment_directory: PathBuf,
    pub archive_path: PathBuf,
}

impl WorkerProcessConfig {
    pub fn validate(&self) -> Result<(), CoreError> {
        for (name, path) in [
            (
                "worker pyproject",
                self.project_directory.join("pyproject.toml"),
            ),
            ("worker lockfile", self.project_directory.join("uv.lock")),
            ("worker archive", self.archive_path.clone()),
        ] {
            if !path.is_file() {
                return Err(CoreError::WorkerUnavailable(format!(
                    "{name} was not found at {}",
                    path.display()
                )));
            }
        }
        if self.environment_directory.as_os_str().is_empty() {
            return Err(CoreError::WorkerUnavailable(
                "worker environment path is empty".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerDiagnosticStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerDiagnostic {
    pub stream: WorkerDiagnosticStream,
    pub text: String,
}

enum DriverCommand {
    Request {
        metadata: Value,
        binary: Vec<u8>,
        reply: oneshot::Sender<Result<Value, CoreError>>,
    },
    Shutdown {
        metadata: Value,
        reply: oneshot::Sender<Result<Value, CoreError>>,
    },
}

/// AppHandle-independent owner of the isolated ASR worker process.
pub struct WorkerProcess {
    sender: Option<mpsc::Sender<DriverCommand>>,
    task: Option<JoinHandle<()>>,
    diagnostics: Option<mpsc::Receiver<WorkerDiagnostic>>,
    hello: WorkerHello,
}

impl WorkerProcess {
    /// Synchronize the frozen environment, start the archive, and require an explicit Hello.
    pub async fn launch(config: WorkerProcessConfig) -> Result<Self, CoreError> {
        config.validate()?;
        if let Some(parent) = config.environment_directory.parent() {
            fs::create_dir_all(parent)?;
        }
        let uv_executable = resolve_uv_executable()
            .ok_or_else(|| CoreError::WorkerUnavailable("uv was not found in PATH".into()))?;
        sync_environment(&config, &uv_executable).await?;

        let (host_socket, child_socket) = StdUnixStream::pair()?;
        host_socket.set_nonblocking(true)?;
        let host_socket = UnixStream::from_std(host_socket)?;
        let mut command = worker_command(&config, &uv_executable, child_socket.into())?;
        let mut child = command.spawn()?;
        let (diagnostic_sender, diagnostic_receiver) = mpsc::channel(DIAGNOSTIC_CHANNEL_CAPACITY);
        if let Some(stdout) = child.stdout.take() {
            spawn_diagnostic_drain(
                stdout,
                WorkerDiagnosticStream::Stdout,
                diagnostic_sender.clone(),
            );
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_diagnostic_drain(stderr, WorkerDiagnosticStream::Stderr, diagnostic_sender);
        }

        let mut supervisor = WorkerSupervisor::attach(host_socket);
        let hello = match supervisor.handshake().await {
            Ok(hello) => hello.clone(),
            Err(error) => {
                terminate_child(&mut child).await;
                return Err(error);
            }
        };
        let (sender, receiver) = mpsc::channel(DRIVER_CHANNEL_CAPACITY);
        let task = tokio::spawn(run_driver(supervisor, child, receiver));
        Ok(Self {
            sender: Some(sender),
            task: Some(task),
            diagnostics: Some(diagnostic_receiver),
            hello,
        })
    }

    #[must_use]
    pub fn hello(&self) -> &WorkerHello {
        &self.hello
    }

    pub fn take_diagnostics(&mut self) -> Option<mpsc::Receiver<WorkerDiagnostic>> {
        self.diagnostics.take()
    }

    pub async fn list_models(
        &self,
        request_id: impl Into<String>,
    ) -> Result<ModelsListResult, CoreError> {
        self.request(
            &ModelsListRequest {
                request_id: request_id.into(),
                operation: WorkerOperation::ModelsList,
            },
            Vec::new(),
        )
        .await
    }

    pub async fn load_model(
        &self,
        request_id: impl Into<String>,
        repo_id: impl Into<String>,
        revision: impl Into<String>,
    ) -> Result<ModelLoadResult, CoreError> {
        self.request(
            &ModelLoadRequest {
                request_id: request_id.into(),
                operation: WorkerOperation::ModelLoad,
                repo_id: repo_id.into(),
                revision: revision.into(),
            },
            Vec::new(),
        )
        .await
    }

    pub async fn transcribe_segment(
        &self,
        request: &SegmentTranscribeRequest,
        samples: &[f32],
    ) -> Result<SegmentTranscriptionResult, CoreError> {
        let mut binary = Vec::with_capacity(std::mem::size_of_val(samples));
        for sample in samples {
            binary.extend_from_slice(&sample.to_le_bytes());
        }
        self.request(request, binary).await
    }

    pub async fn unload_model(
        &self,
        request_id: impl Into<String>,
    ) -> Result<ModelUnloadResult, CoreError> {
        self.request(
            &ModelUnloadRequest {
                request_id: request_id.into(),
                operation: WorkerOperation::ModelUnload,
            },
            Vec::new(),
        )
        .await
    }

    pub async fn shutdown(
        mut self,
        request_id: impl Into<String>,
    ) -> Result<ShutdownResult, CoreError> {
        let request = ShutdownRequest {
            request_id: request_id.into(),
            operation: WorkerOperation::Shutdown,
        };
        request.validate_binary(&[])?;
        let metadata = serde_json::to_value(request)
            .map_err(|error| CoreError::WorkerProtocol(error.to_string()))?;
        let (reply, response) = oneshot::channel();
        let sender = self.sender.take().ok_or(CoreError::WorkerClosed)?;
        sender
            .send(DriverCommand::Shutdown { metadata, reply })
            .await
            .map_err(|_| CoreError::WorkerClosed)?;
        drop(sender);
        let result = response.await.map_err(|_| CoreError::WorkerClosed)?;
        if let Some(task) = self.task.take() {
            task.await
                .map_err(|error| CoreError::BlockingTask(error.to_string()))?;
        }
        decode_result(result?)
    }

    async fn request<T, R>(&self, request: &T, binary: Vec<u8>) -> Result<R, CoreError>
    where
        T: RequestMetadata,
        R: DeserializeOwned,
    {
        request.validate_binary(&binary)?;
        let metadata = serde_json::to_value(request)
            .map_err(|error| CoreError::WorkerProtocol(error.to_string()))?;
        let (reply, response) = oneshot::channel();
        self.sender
            .as_ref()
            .ok_or(CoreError::WorkerClosed)?
            .send(DriverCommand::Request {
                metadata,
                binary,
                reply,
            })
            .await
            .map_err(|_| CoreError::WorkerClosed)?;
        decode_result(response.await.map_err(|_| CoreError::WorkerClosed)??)
    }
}

fn decode_result<T: DeserializeOwned>(value: Value) -> Result<T, CoreError> {
    serde_json::from_value(value).map_err(|error| CoreError::WorkerProtocol(error.to_string()))
}

enum IdleEvent {
    Command(Option<DriverCommand>),
    Incoming(Result<(), CoreError>),
    Child(Result<std::process::ExitStatus, std::io::Error>),
    Heartbeat,
    Inactive,
}

async fn run_driver(
    mut supervisor: WorkerSupervisor,
    mut child: Child,
    mut receiver: mpsc::Receiver<DriverCommand>,
) {
    let mut heartbeat = interval_at(Instant::now() + HEARTBEAT_INTERVAL, HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        let deadline = supervisor.incoming_deadline();
        let event = tokio::select! {
            command = receiver.recv() => IdleEvent::Command(command),
            incoming = supervisor.receive_idle() => IdleEvent::Incoming(incoming),
            status = child.wait() => IdleEvent::Child(status),
            _ = heartbeat.tick() => IdleEvent::Heartbeat,
            () = sleep_until(deadline) => IdleEvent::Inactive,
        };
        match event {
            IdleEvent::Command(Some(DriverCommand::Request {
                metadata,
                binary,
                reply,
            })) => {
                let result = supervisor.request_value(metadata, &binary).await;
                let fatal = result.as_ref().is_err_and(is_fatal_worker_error);
                let _ = reply.send(result);
                if fatal {
                    break;
                }
            }
            IdleEvent::Command(Some(DriverCommand::Shutdown { metadata, reply })) => {
                let result = supervisor.request_value(metadata, &[]).await;
                let result = match result {
                    Ok(value) => match timeout(GRACEFUL_SHUTDOWN_TIMEOUT, child.wait()).await {
                        Ok(Ok(status)) if status.success() => Ok(value),
                        Ok(Ok(status)) => Err(CoreError::WorkerExited(status.to_string())),
                        Ok(Err(error)) => Err(CoreError::Io(error)),
                        Err(_) => {
                            terminate_child(&mut child).await;
                            Ok(value)
                        }
                    },
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
                break;
            }
            IdleEvent::Command(None) => break,
            IdleEvent::Incoming(Ok(())) => {}
            IdleEvent::Incoming(Err(_)) | IdleEvent::Inactive => break,
            IdleEvent::Child(Ok(_)) | IdleEvent::Child(Err(_)) => break,
            IdleEvent::Heartbeat => {
                if supervisor.send_heartbeat().await.is_err() {
                    break;
                }
            }
        }
    }
    terminate_child(&mut child).await;
}

fn is_fatal_worker_error(error: &CoreError) -> bool {
    matches!(
        error,
        CoreError::WorkerProtocol(_)
            | CoreError::WorkerClosed
            | CoreError::WorkerUnresponsive
            | CoreError::Io(_)
    )
}

async fn terminate_child(child: &mut Child) {
    match child.try_wait() {
        Ok(Some(_)) => {}
        Ok(None) | Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }
}

async fn sync_environment(
    config: &WorkerProcessConfig,
    uv_executable: &Path,
) -> Result<(), CoreError> {
    let output = sync_command(config, uv_executable).output().await?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr);
    let detail = detail.trim();
    Err(CoreError::WorkerUnavailable(if detail.is_empty() {
        format!("uv sync exited with {}", output.status)
    } else {
        format!("uv sync failed: {detail}")
    }))
}

fn sync_command(config: &WorkerProcessConfig, uv_executable: &Path) -> Command {
    let mut command = Command::new(uv_executable);
    command
        .arg("sync")
        .arg("--project")
        .arg(&config.project_directory)
        .arg("--frozen")
        .arg("--no-dev")
        .arg("--no-install-project")
        .env("UV_PROJECT_ENVIRONMENT", &config.environment_directory);
    command
}

fn worker_command(
    config: &WorkerProcessConfig,
    uv_executable: &Path,
    worker_socket: OwnedFd,
) -> Result<Command, CoreError> {
    let mut command = Command::new(uv_executable);
    command
        .arg("run")
        .arg("--project")
        .arg(&config.project_directory)
        .arg("--frozen")
        .arg("--no-dev")
        .arg("--no-sync")
        .arg("python")
        .arg(&config.archive_path)
        .arg("--ipc-fd")
        .arg("3")
        .env("UV_PROJECT_ENVIRONMENT", &config.environment_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
        .fd_mappings(vec![FdMapping {
            parent_fd: worker_socket,
            child_fd: 3,
        }])
        .map_err(|error| CoreError::WorkerUnavailable(error.to_string()))?;
    Ok(command)
}

fn resolve_uv_executable() -> Option<PathBuf> {
    resolve_uv_executable_from(std::env::var_os("PATH").as_deref(), is_executable_file)
}

fn resolve_uv_executable_from(
    search_path: Option<&OsStr>,
    is_executable: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    search_path
        .into_iter()
        .flat_map(std::env::split_paths)
        .map(|directory| directory.join("uv"))
        .find(|candidate| is_executable(candidate))
}

fn is_executable_file(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn spawn_diagnostic_drain<R>(
    mut reader: R,
    stream: WorkerDiagnosticStream,
    sender: mpsc::Sender<WorkerDiagnostic>,
) where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buffer = [0_u8; 4096];
        loop {
            match reader.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(length) => {
                    let _ = sender.try_send(WorkerDiagnostic {
                        stream,
                        text: String::from_utf8_lossy(&buffer[..length]).into_owned(),
                    });
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    fn config() -> WorkerProcessConfig {
        WorkerProcessConfig {
            project_directory: PathBuf::from(
                "/Applications/RecoGUI.app/Contents/Resources/src-python",
            ),
            environment_directory: PathBuf::from("/tmp/recogui/asr-python"),
            archive_path: PathBuf::from(
                "/Applications/RecoGUI.app/Contents/Resources/src-python/reco-asr-worker.pyz",
            ),
        }
    }

    #[test]
    fn frozen_sync_uses_only_the_dedicated_runtime_environment() {
        let config = config();
        let uv = Path::new("/opt/homebrew/bin/uv");
        let command = sync_command(&config, uv);
        let command = command.as_std();
        let arguments = command.get_args().collect::<Vec<_>>();
        assert_eq!(command.get_program(), uv.as_os_str());
        assert_eq!(arguments.first(), Some(&OsStr::new("sync")));
        assert!(arguments.contains(&OsStr::new("--frozen")));
        assert!(arguments.contains(&OsStr::new("--no-dev")));
        assert!(arguments.contains(&OsStr::new("--no-install-project")));
        assert_eq!(
            command
                .get_envs()
                .find(|(key, _)| *key == OsStr::new("UV_PROJECT_ENVIRONMENT"))
                .and_then(|(_, value)| value),
            Some(config.environment_directory.as_os_str())
        );
    }

    #[test]
    fn worker_runs_the_archive_through_frozen_uv_and_fd_three() {
        let config = config();
        let uv = Path::new("/opt/homebrew/bin/uv");
        let (_host, worker) = StdUnixStream::pair().unwrap();
        let command = worker_command(&config, uv, worker.into()).unwrap();
        let command = command.as_std();
        let arguments = command.get_args().collect::<Vec<_>>();
        assert_eq!(arguments.first(), Some(&OsStr::new("run")));
        assert!(arguments.contains(&OsStr::new("--frozen")));
        assert!(arguments.contains(&OsStr::new("--no-dev")));
        assert!(arguments.contains(&OsStr::new("--no-sync")));
        assert!(arguments.contains(&OsStr::new("python")));
        assert!(arguments.contains(&config.archive_path.as_os_str()));
        assert!(
            arguments
                .windows(2)
                .any(|pair| { pair == [OsStr::new("--ipc-fd"), OsStr::new("3")] })
        );
    }

    #[test]
    fn uv_resolution_is_path_only_and_preserves_path_order() {
        let search_path = std::env::join_paths(["/usr/bin", "/opt/homebrew/bin"]).unwrap();
        let expected = PathBuf::from("/opt/homebrew/bin/uv");
        assert_eq!(
            resolve_uv_executable_from(Some(&search_path), |candidate| candidate == expected),
            Some(expected)
        );
        assert_eq!(
            resolve_uv_executable_from(Some(&OsString::from("/usr/bin")), |_| false),
            None
        );
    }
}
