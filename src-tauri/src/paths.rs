use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::{AppHandle, Manager};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PathError {
    #[error("the operating system did not provide an application data directory")]
    AppDataUnavailable,
    #[error("the bundled Reco engine project was not found")]
    EngineProjectMissing,
    #[error("failed to create application directory: {0}")]
    CreateDirectory(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub root: PathBuf,
    pub database: PathBuf,
    #[allow(dead_code)]
    pub backups: PathBuf,
    pub assets: PathBuf,
    pub logs: PathBuf,
    #[allow(dead_code)]
    pub engine_lock: PathBuf,
    pub engine_environment: PathBuf,
    pub engine_project: PathBuf,
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
        let assets = root.join("assets");
        let logs = root.join("logs");
        for directory in [&root, &backups, &assets, &logs] {
            std::fs::create_dir_all(directory)?;
        }

        let engine_project = resolve_engine_project(app)?;
        let engine_environment = root.join("python-env");
        Ok(Self {
            database: root.join("reco.sqlite3"),
            engine_lock: root.join("engine.lock"),
            root,
            backups,
            assets,
            logs,
            engine_environment,
            engine_project,
        })
    }

    pub fn public(&self) -> PublicAppPaths {
        PublicAppPaths {
            database_directory: self.root.to_string_lossy().into_owned(),
            log_directory: self.logs.to_string_lossy().into_owned(),
        }
    }
}

fn resolve_engine_project(app: &AppHandle) -> Result<PathBuf, PathError> {
    let resource_path = app
        .path()
        .resource_dir()
        .map_err(|_| PathError::EngineProjectMissing)?
        .join("src-python");
    if resource_path.join("pyproject.toml").is_file() {
        return Ok(resource_path);
    }

    let development_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")))
        .join("src-python");
    if development_path.join("pyproject.toml").is_file() {
        return Ok(development_path);
    }

    // Keep application startup recoverable. The supervisor reports the missing
    // project only when the engine is requested.
    Ok(resource_path)
}
