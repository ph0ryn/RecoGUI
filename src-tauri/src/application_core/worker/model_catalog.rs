use std::{
    fs,
    path::{Path, PathBuf},
};

use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::application_core::{error::CoreError, worker::CachedModel};

use super::gguf::string_metadata;

#[derive(Clone, Debug)]
pub(super) struct ModelFiles {
    pub model: PathBuf,
    pub projector: PathBuf,
}

pub struct ModelCatalog {
    entries: Vec<(CachedModel, ModelFiles)>,
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
                let mut decoders = Vec::new();
                let mut projectors = Vec::new();
                for file in fs::read_dir(&path)? {
                    let file = file?.path();
                    if !file.is_file() || file.extension().is_none_or(|ext| ext != "gguf") {
                        continue;
                    }
                    let metadata = string_metadata(&file).map_err(|error| {
                        CoreError::WorkerUnavailable(format!("{}: {error}", file.display()))
                    })?;
                    match metadata
                        .strings
                        .get("general.architecture")
                        .map(String::as_str)
                    {
                        Some("qwen3vl")
                            if metadata
                                .tags
                                .iter()
                                .any(|tag| tag == "automatic-speech-recognition") =>
                        {
                            decoders.push(file)
                        }
                        Some("clip")
                            if metadata
                                .strings
                                .get("clip.audio.projector_type")
                                .map(String::as_str)
                                == Some("qwen3a") =>
                        {
                            projectors.push(file)
                        }
                        _ => {}
                    }
                }
                let modified: OffsetDateTime = fs::metadata(&path)?.modified()?.into();
                let last_modified = modified
                    .format(&Rfc3339)
                    .map_err(|error| CoreError::WorkerUnavailable(error.to_string()))?;

                let mut matching_refs = refs
                    .iter()
                    .filter(|(_, hash)| hash == &revision)
                    .map(|(name, _)| name.clone())
                    .collect::<Vec<_>>();
                matching_refs.sort();
                for model in decoders {
                    let file_name = model
                        .file_name()
                        .and_then(|name| name.to_str())
                        .ok_or_else(|| {
                            CoreError::WorkerUnavailable("GGUF filename is not UTF-8".into())
                        })?
                        .to_owned();
                    let matching = path.join(format!("mmproj-{file_name}"));
                    let projector = if projectors.contains(&matching) {
                        matching
                    } else if projectors.len() == 1 {
                        projectors[0].clone()
                    } else {
                        return Err(CoreError::WorkerUnavailable(format!(
                            "{} requires a matching mmproj-{file_name}, or exactly one Qwen3-ASR projector in its snapshot",
                            model.display()
                        )));
                    };
                    let size =
                        format_size(fs::metadata(&model)?.len() + fs::metadata(&projector)?.len());
                    entries.push((
                        CachedModel {
                            repo_id: repo_id.clone(),
                            revision: revision.clone(),
                            file_name,
                            size,
                            last_modified: last_modified.clone(),
                            refs: matching_refs.clone(),
                            supported_languages: QWEN_LANGUAGES
                                .iter()
                                .map(|value| (*value).to_owned())
                                .collect(),
                        },
                        ModelFiles { model, projector },
                    ));
                }
            }
        }
        entries.sort_by(|left, right| {
            (&left.0.repo_id, &left.0.revision, &left.0.file_name).cmp(&(
                &right.0.repo_id,
                &right.0.revision,
                &right.0.file_name,
            ))
        });
        Ok(Self { entries })
    }

    pub fn models(&self) -> Vec<CachedModel> {
        self.entries
            .iter()
            .map(|(model, _)| model.clone())
            .collect()
    }

    pub(super) fn resolve(
        &self,
        repo_id: &str,
        revision: &str,
        file_name: &str,
    ) -> Option<&ModelFiles> {
        self.entries
            .iter()
            .find(|(model, _)| {
                model.repo_id == repo_id
                    && model.revision == revision
                    && model.file_name == file_name
            })
            .map(|(_, files)| files)
    }
}

// Prompt language names supported by Qwen3-ASR.
const QWEN_LANGUAGES: &[&str] = &[
    "Chinese",
    "English",
    "Cantonese",
    "Arabic",
    "German",
    "French",
    "Spanish",
    "Portuguese",
    "Indonesian",
    "Italian",
    "Korean",
    "Russian",
    "Thai",
    "Vietnamese",
    "Japanese",
    "Turkish",
    "Hindi",
    "Malay",
    "Dutch",
    "Swedish",
    "Danish",
    "Finnish",
    "Polish",
    "Czech",
    "Filipino",
    "Persian",
    "Greek",
    "Romanian",
    "Hungarian",
    "Macedonian",
];

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
    fn lists_gguf_quantizations_without_an_mlx_config() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("models--ggml-org--Qwen3-ASR-0.6B-GGUF");
        let snapshot = repo.join("snapshots/abcd");
        fs::create_dir_all(&snapshot).unwrap();
        fs::create_dir_all(repo.join("refs")).unwrap();
        fs::write(repo.join("refs/main"), "abcd\n").unwrap();
        for name in ["Qwen3-ASR-0.6B-Q8_0.gguf", "Qwen3-ASR-0.6B-bf16.gguf"] {
            super::super::gguf::write_fixture(
                &snapshot.join(name),
                &[("general.architecture", "qwen3vl")],
            );
        }
        super::super::gguf::write_fixture(
            &snapshot.join("mmproj-Qwen3-ASR-0.6B-Q8_0.gguf"),
            &[
                ("general.architecture", "clip"),
                ("clip.audio.projector_type", "qwen3a"),
            ],
        );
        super::super::gguf::write_fixture(
            &snapshot.join("mmproj-Qwen3-ASR-0.6B-bf16.gguf"),
            &[
                ("general.architecture", "clip"),
                ("clip.audio.projector_type", "qwen3a"),
            ],
        );

        let models = ModelCatalog::scan_at(temp.path()).unwrap().models();
        assert_eq!(
            models.len(),
            2,
            "GGUF quantizations must be selectable without config.json"
        );
        assert!(
            models
                .iter()
                .all(|model| model.repo_id == "ggml-org/Qwen3-ASR-0.6B-GGUF")
        );
        let catalog = ModelCatalog::scan_at(temp.path()).unwrap();
        assert_eq!(
            catalog
                .resolve(&models[0].repo_id, "abcd", &models[0].file_name)
                .unwrap()
                .model,
            snapshot.join(&models[0].file_name)
        );
        assert!(
            catalog
                .resolve(&models[0].repo_id, "other", &models[0].file_name)
                .is_none()
        );
        assert!(
            catalog
                .resolve(&models[0].repo_id, "abcd", "missing.gguf")
                .is_none()
        );
    }

    #[test]
    fn rejects_missing_or_ambiguous_projectors() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = temp.path().join("models--test--Qwen3-ASR/snapshots/abcd");
        fs::create_dir_all(&snapshot).unwrap();
        super::super::gguf::write_fixture(
            &snapshot.join("model.gguf"),
            &[("general.architecture", "qwen3vl")],
        );
        assert!(ModelCatalog::scan_at(temp.path()).is_err());
        for name in ["a.gguf", "b.gguf"] {
            super::super::gguf::write_fixture(
                &snapshot.join(name),
                &[
                    ("general.architecture", "clip"),
                    ("clip.audio.projector_type", "qwen3a"),
                ],
            );
        }
        assert!(ModelCatalog::scan_at(temp.path()).is_err());
    }
}
