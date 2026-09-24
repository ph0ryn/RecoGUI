use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::application_core::{error::CoreError, worker::CachedModel};

pub struct ModelCatalog {
    entries: Vec<(CachedModel, PathBuf)>,
}

impl ModelCatalog {
    pub fn scan() -> Result<Self, CoreError> {
        Self::scan_at(&cache_directory()?)
    }

    fn scan_at(cache: &Path) -> Result<Self, CoreError> {
        if !cache.exists() {
            return Ok(Self {
                entries: Vec::new(),
            });
        }
        let mut entries = Vec::new();
        for repo in fs::read_dir(cache)? {
            let repo = repo?;
            let repo_path = repo.path();
            if !repo_path.is_dir() {
                continue;
            }
            let name = repo.file_name().to_string_lossy().into_owned();
            let Some(repo_id) = name
                .strip_prefix("models--")
                .map(|name| name.replace("--", "/"))
            else {
                continue;
            };
            let snapshot_root = repo_path.join("snapshots");
            if !snapshot_root.is_dir() {
                continue;
            }
            let refs = read_refs(&repo_path.join("refs"))?;
            for snapshot in fs::read_dir(snapshot_root)? {
                let snapshot = snapshot?;
                let path = snapshot.path();
                if !path.is_dir() {
                    continue;
                }
                let revision = snapshot.file_name().to_string_lossy().into_owned();
                let config_path = path.join("config.json");
                if !config_path.is_file() {
                    continue;
                }
                let config: Value =
                    serde_json::from_slice(&fs::read(&config_path)?).map_err(|error| {
                        CoreError::WorkerUnavailable(format!("{}: {error}", config_path.display()))
                    })?;
                if config.get("model_type").and_then(Value::as_str) != Some("qwen3_asr") {
                    continue;
                }
                let languages = config
                    .get("support_languages")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .fold(Vec::<String>::new(), |mut values, language| {
                        if !values.iter().any(|value| value == language) {
                            values.push(language.to_owned());
                        }
                        values
                    });
                let modified: OffsetDateTime = fs::metadata(&path)?.modified()?.into();
                let last_modified = modified
                    .format(&Rfc3339)
                    .map_err(|error| CoreError::WorkerUnavailable(error.to_string()))?;
                let size = format_size(directory_size(&path)?);
                let mut matching_refs = refs
                    .iter()
                    .filter(|(_, hash)| hash == &revision)
                    .map(|(name, _)| name.clone())
                    .collect::<Vec<_>>();
                matching_refs.sort();
                entries.push((
                    CachedModel {
                        repo_id: repo_id.clone(),
                        revision,
                        size,
                        last_modified,
                        refs: matching_refs,
                        supported_languages: languages,
                    },
                    path,
                ));
            }
        }
        entries.sort_by(|left, right| {
            (&left.0.repo_id, &left.0.revision).cmp(&(&right.0.repo_id, &right.0.revision))
        });
        Ok(Self { entries })
    }

    pub fn models(&self) -> Vec<CachedModel> {
        self.entries
            .iter()
            .map(|(model, _)| model.clone())
            .collect()
    }

    pub fn resolve(&self, repo_id: &str, revision: &str) -> Option<&Path> {
        self.entries
            .iter()
            .find(|(model, _)| model.repo_id == repo_id && model.revision == revision)
            .map(|(_, path)| path.as_path())
    }
}

fn cache_directory() -> Result<PathBuf, CoreError> {
    if let Some(path) = std::env::var_os("HF_HUB_CACHE") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("HF_HOME") {
        return Ok(PathBuf::from(path).join("hub"));
    }
    if let Some(path) = std::env::var_os("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(path).join("huggingface/hub"));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| CoreError::WorkerUnavailable("HOME is not set".into()))?;
    Ok(PathBuf::from(home).join(".cache/huggingface/hub"))
}

fn read_refs(directory: &Path) -> Result<Vec<(String, String)>, CoreError> {
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut refs = Vec::new();
    let mut pending = vec![directory.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(current)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.is_file() {
                let name = path
                    .strip_prefix(directory)
                    .expect("ref is inside the root")
                    .to_string_lossy()
                    .into_owned();
                refs.push((name, fs::read_to_string(path)?.trim().to_owned()));
            }
        }
    }
    Ok(refs)
}

fn directory_size(directory: &Path) -> Result<u64, CoreError> {
    let mut size = 0;
    let mut pending = vec![directory.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(current)? {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.is_file() {
                size += fs::metadata(path)?.len();
            }
        }
    }
    Ok(size)
}

fn format_size(size: u64) -> String {
    let mut value = size as f64;
    for unit in ["B", "KB", "MB", "GB", "TB"] {
        if value < 1000.0 || unit == "TB" {
            return if unit == "B" {
                format!("{size}B")
            } else {
                format!("{value:.1}{unit}")
            };
        }
        value /= 1000.0;
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_only_qwen3_asr_revisions_and_resolves_exact_hash() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("models--test--model");
        let snapshot = repo.join("snapshots/abcd");
        fs::create_dir_all(&snapshot).unwrap();
        fs::create_dir_all(repo.join("refs")).unwrap();
        fs::write(
            snapshot.join("config.json"),
            r#"{"model_type":"qwen3_asr","support_languages":["Japanese","Japanese","English"]}"#,
        )
        .unwrap();
        fs::write(snapshot.join("model.safetensors"), [0u8; 4]).unwrap();
        fs::write(repo.join("refs/main"), "abcd\n").unwrap();

        let catalog = ModelCatalog::scan_at(temp.path()).unwrap();
        let models = catalog.models();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].repo_id, "test/model");
        assert_eq!(models[0].revision, "abcd");
        assert_eq!(models[0].refs, ["main"]);
        assert_eq!(models[0].supported_languages, ["Japanese", "English"]);
        assert_eq!(
            catalog.resolve("test/model", "abcd"),
            Some(snapshot.as_path())
        );
        assert_eq!(catalog.resolve("test/model", "other"), None);
    }
}
