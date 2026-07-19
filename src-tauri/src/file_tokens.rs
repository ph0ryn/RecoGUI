use std::{collections::HashMap, path::PathBuf, sync::Arc};

use serde::Serialize;
use tokio::sync::RwLock;
use uuid::Uuid;

#[derive(Debug, Clone, Default)]
pub struct FileTokenStore {
    inner: Arc<RwLock<HashMap<String, PathBuf>>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectedFile {
    pub token: String,
    pub display_name: String,
}

impl FileTokenStore {
    pub async fn insert(&self, path: PathBuf) -> SelectedFile {
        let token = Uuid::new_v4().to_string();
        let display_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Selected file".to_owned());
        self.inner.write().await.insert(token.clone(), path);
        SelectedFile {
            token,
            display_name,
        }
    }

    pub async fn resolve(&self, token: &str) -> Option<PathBuf> {
        self.inner.read().await.get(token).cloned()
    }

    pub async fn remove(&self, token: &str) -> Option<PathBuf> {
        self.inner.write().await.remove(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tokens_do_not_expose_paths() {
        let store = FileTokenStore::default();
        let path = PathBuf::from("/private/example.wav");
        let selected = store.insert(path.clone()).await;

        assert!(!selected.token.contains("private"));
        assert_eq!(selected.display_name, "example.wav");
        assert_eq!(store.resolve(&selected.token).await, Some(path));
    }
}
