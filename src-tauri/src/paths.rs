use std::path::{Path, PathBuf};

use tauri::{AppHandle, Manager};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PathError {
    #[error("the operating system did not provide an application data directory")]
    AppDataUnavailable,
    #[error("the bundled Silero VAD asset was not found")]
    VadAssetMissing,
    #[error("failed to create application directory: {0}")]
    CreateDirectory(#[from] std::io::Error),
}

#[derive(Clone, Debug)]
pub struct AppPaths {
    pub database: PathBuf,
    pub vad_asset: PathBuf,
}

impl AppPaths {
    pub fn resolve(app: &AppHandle) -> Result<Self, PathError> {
        let root = app
            .path()
            .app_data_dir()
            .map_err(|_| PathError::AppDataUnavailable)?;
        std::fs::create_dir_all(&root)?;
        let vad_asset = resolve_vad_asset(app)?;
        Ok(Self {
            database: root.join("reco.sqlite3"),
            vad_asset,
        })
    }
}

fn resolve_vad_asset(app: &AppHandle) -> Result<PathBuf, PathError> {
    let resource = app
        .path()
        .resource_dir()
        .map_err(|_| PathError::VadAssetMissing)?
        .join("vad/silero_vad.onnx");
    let development = Path::new(env!("CARGO_MANIFEST_DIR")).join("vad/silero_vad.onnx");
    [resource, development]
        .into_iter()
        .find(|path| path.is_file())
        .ok_or(PathError::VadAssetMissing)
}
