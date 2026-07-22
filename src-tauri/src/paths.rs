use std::path::{Path, PathBuf};

use tauri::{AppHandle, Manager};
use thiserror::Error;

use crate::application_core::worker::WorkerProcessConfig;

#[derive(Debug, Error)]
pub enum PathError {
    #[error("the operating system did not provide an application data directory")]
    AppDataUnavailable,
    #[error("the bundled ASR worker project was not found")]
    WorkerProjectMissing,
    #[error("the bundled Silero VAD asset was not found")]
    VadAssetMissing,
    #[error("failed to create application directory: {0}")]
    CreateDirectory(#[from] std::io::Error),
}

#[derive(Clone, Debug)]
pub struct AppPaths {
    pub database: PathBuf,
    pub worker: WorkerProcessConfig,
    pub vad_asset: PathBuf,
}

impl AppPaths {
    pub fn resolve(app: &AppHandle) -> Result<Self, PathError> {
        let root = app
            .path()
            .app_data_dir()
            .map_err(|_| PathError::AppDataUnavailable)?;
        std::fs::create_dir_all(&root)?;
        let (project_directory, archive_path) = resolve_worker_bundle(app)?;
        let vad_asset = resolve_vad_asset(app)?;
        Ok(Self {
            database: root.join("reco.sqlite3"),
            worker: WorkerProcessConfig {
                project_directory,
                environment_directory: root.join("python-worker-env"),
                archive_path,
            },
            vad_asset,
        })
    }
}

fn resolve_worker_bundle(app: &AppHandle) -> Result<(PathBuf, PathBuf), PathError> {
    let resource_project = app
        .path()
        .resource_dir()
        .map_err(|_| PathError::WorkerProjectMissing)?
        .join("src-python");
    let development_project = workspace_root().join("src-python");
    for (project, archive) in [
        (
            resource_project.clone(),
            resource_project.join("reco-asr-worker.pyz"),
        ),
        (
            development_project.clone(),
            development_project.join("dist/reco-asr-worker.pyz"),
        ),
    ] {
        if project.join("pyproject.toml").is_file()
            && project.join("uv.lock").is_file()
            && archive.is_file()
        {
            return Ok((project, archive));
        }
    }
    Err(PathError::WorkerProjectMissing)
}

fn resolve_vad_asset(app: &AppHandle) -> Result<PathBuf, PathError> {
    let resource = app
        .path()
        .resource_dir()
        .map_err(|_| PathError::VadAssetMissing)?
        .join("models/silero_vad.onnx");
    let development =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("resources/models/silero_vad.onnx");
    [resource, development]
        .into_iter()
        .find(|path| path.is_file())
        .ok_or(PathError::VadAssetMissing)
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")))
        .to_path_buf()
}
