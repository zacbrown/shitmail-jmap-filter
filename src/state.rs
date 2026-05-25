use crate::domain::cache::atomic_write;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct PersistedState {
    pub email_state: Option<String>,
}

#[derive(Clone)]
pub struct StateStore {
    path: PathBuf,
    inner: Arc<Mutex<PersistedState>>,
}

impl StateStore {
    pub async fn load(path: PathBuf) -> Result<Self> {
        let state = match tokio::fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice::<PersistedState>(&bytes).unwrap_or_default(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => PersistedState::default(),
            Err(e) => return Err(e).context("reading state file"),
        };
        Ok(Self { path, inner: Arc::new(Mutex::new(state)) })
    }

    pub async fn email_state(&self) -> Option<String> {
        self.inner.lock().await.email_state.clone()
    }

    pub async fn set_email_state(&self, new_state: String) -> Result<()> {
        let snapshot = {
            let mut g = self.inner.lock().await;
            g.email_state = Some(new_state);
            g.clone()
        };
        atomic_write(&self.path, &serde_json::to_vec(&snapshot)?).await
    }
}
