use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::{AppHandle, Manager};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PathError {
    #[error("the operating system did not provide an application data directory")]
    AppDataUnavailable,
    #[error("the bundled Reco engine was not found")]
    EngineMissing,
    #[error("failed to create application directory: {0}")]
    CreateDirectory(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub root: PathBuf,
    pub database: PathBuf,
    #[allow(dead_code)]
    pub backups: PathBuf,
    pub models: PathBuf,
    pub logs: PathBuf,
    #[allow(dead_code)]
    pub engine_lock: PathBuf,
    pub engine_executable: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicAppPaths {
    pub database_directory: String,
    pub log_directory: String,
}

impl AppPaths {
    pub fn resolve(app: &AppHandle) -> Result<Self, PathError> {
        let root = app
            .path()
            .app_data_dir()
            .map_err(|_| PathError::AppDataUnavailable)?;
        let backups = root.join("backups");
        let models = root.join("models");
        let logs = root.join("logs");
        for directory in [&root, &backups, &models, &logs] {
            std::fs::create_dir_all(directory)?;
        }

        let engine_executable = resolve_engine_executable(app)?;
        Ok(Self {
            database: root.join("reco.sqlite3"),
            engine_lock: root.join("engine.lock"),
            root,
            backups,
            models,
            logs,
            engine_executable,
        })
    }

    pub fn public(&self) -> PublicAppPaths {
        PublicAppPaths {
            database_directory: self.root.to_string_lossy().into_owned(),
            log_directory: self.logs.to_string_lossy().into_owned(),
        }
    }
}

fn resolve_engine_executable(app: &AppHandle) -> Result<PathBuf, PathError> {
    let file_name = if cfg!(windows) {
        "reco-engine.exe"
    } else {
        "reco-engine"
    };
    let resource_path = app
        .path()
        .resource_dir()
        .map_err(|_| PathError::EngineMissing)?
        .join("sidecar")
        .join(file_name);
    if resource_path.is_file() {
        return Ok(resource_path);
    }

    // Development builds may place the exact same fixed binary next to Cargo.toml.
    let development_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("sidecar")
        .join(file_name);
    if development_path.is_file() {
        return Ok(development_path);
    }
    // Return the canonical resource location so the host can start without the
    // development sidecar and report a recoverable availability error on use.
    Ok(resource_path)
}
