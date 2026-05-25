use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

const POSITIVE_TTL_DAYS: i64 = 30;
const NEGATIVE_TTL_HOURS: i64 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    /// Domain registration date if successfully resolved.
    pub registered: Option<DateTime<Utc>>,
    pub fetched_at: DateTime<Utc>,
    pub error: Option<String>,
}

impl CacheEntry {
    pub fn is_fresh(&self, now: DateTime<Utc>) -> bool {
        let age = now - self.fetched_at;
        match self.error {
            None => age <= chrono::Duration::days(POSITIVE_TTL_DAYS),
            Some(_) => age <= chrono::Duration::hours(NEGATIVE_TTL_HOURS),
        }
    }
}

#[derive(Clone)]
pub struct AgeCache {
    inner: Arc<Mutex<Inner>>,
    path: PathBuf,
}

struct Inner {
    entries: HashMap<String, CacheEntry>,
    dirty: bool,
}

impl AgeCache {
    pub async fn load(path: PathBuf) -> Result<Self> {
        let entries = match tokio::fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice::<HashMap<String, CacheEntry>>(&bytes)
                .unwrap_or_default(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(e).context("reading rdap-cache.json"),
        };
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner { entries, dirty: false })),
            path,
        })
    }

    pub async fn get(&self, domain: &str) -> Option<CacheEntry> {
        let guard = self.inner.lock().await;
        guard
            .entries
            .get(domain)
            .filter(|e| e.is_fresh(Utc::now()))
            .cloned()
    }

    pub async fn put(&self, domain: &str, entry: CacheEntry) {
        let mut guard = self.inner.lock().await;
        guard.entries.insert(domain.to_string(), entry);
        guard.dirty = true;
    }

    pub async fn flush(&self) -> Result<()> {
        let snapshot = {
            let mut guard = self.inner.lock().await;
            if !guard.dirty {
                return Ok(());
            }
            guard.dirty = false;
            guard.entries.clone()
        };
        atomic_write(&self.path, &serde_json::to_vec(&snapshot)?).await?;
        Ok(())
    }
}

pub async fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating {:?}", parent))?;
        }
    }
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, data)
        .await
        .with_context(|| format!("writing {:?}", tmp))?;
    tokio::fs::rename(&tmp, path)
        .await
        .with_context(|| format!("renaming to {:?}", path))?;
    Ok(())
}
