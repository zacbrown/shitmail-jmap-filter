use crate::domain::cache::atomic_write;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::cmp::min;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const BACKOFF_CAP: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryEntry {
    pub id: String,
    pub registrable_domain: String,
    pub first_seen_at: DateTime<Utc>,
    pub attempts: u32,
    pub next_attempt_at: DateTime<Utc>,
    pub last_error: Option<String>,
}

#[derive(Clone)]
pub struct RetryQueue {
    path: PathBuf,
    base_interval: Duration,
    max_attempts: u32,
    inner: Arc<Mutex<Vec<RetryEntry>>>,
}

impl RetryQueue {
    pub async fn load(path: PathBuf, base_interval: Duration, max_attempts: u32) -> Result<Self> {
        let entries = match tokio::fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice::<Vec<RetryEntry>>(&bytes).unwrap_or_default(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).context("reading retry queue"),
        };
        Ok(Self {
            path,
            base_interval,
            max_attempts,
            inner: Arc::new(Mutex::new(entries)),
        })
    }

    pub async fn enqueue(&self, id: String, registrable_domain: String, error: String) -> Result<()> {
        let now = Utc::now();
        let entry = RetryEntry {
            id,
            registrable_domain,
            first_seen_at: now,
            attempts: 0,
            next_attempt_at: now + chrono::Duration::from_std(self.base_interval).unwrap_or_default(),
            last_error: Some(error),
        };
        let mut g = self.inner.lock().await;
        g.retain(|e| e.id != entry.id);
        g.push(entry);
        let snapshot = g.clone();
        drop(g);
        atomic_write(&self.path, &serde_json::to_vec(&snapshot)?).await
    }

    /// Returns the entries whose `next_attempt_at` is in the past.
    pub async fn due(&self) -> Vec<RetryEntry> {
        let now = Utc::now();
        let g = self.inner.lock().await;
        g.iter().filter(|e| e.next_attempt_at <= now).cloned().collect()
    }

    pub async fn remove(&self, id: &str) -> Result<()> {
        let mut g = self.inner.lock().await;
        let before = g.len();
        g.retain(|e| e.id != id);
        if g.len() == before {
            return Ok(());
        }
        let snapshot = g.clone();
        drop(g);
        atomic_write(&self.path, &serde_json::to_vec(&snapshot)?).await
    }

    /// Bumps an entry's attempt count and reschedules. Returns true if
    /// the cap was reached and the entry was dropped.
    pub async fn bump(&self, id: &str, error: String) -> Result<bool> {
        let mut g = self.inner.lock().await;
        let Some(idx) = g.iter().position(|e| e.id == id) else {
            return Ok(false);
        };
        g[idx].attempts += 1;
        g[idx].last_error = Some(error);
        let exhausted = g[idx].attempts >= self.max_attempts;
        if exhausted {
            g.remove(idx);
        } else {
            let mult = 1u64 << min(g[idx].attempts, 16);
            let backoff = min(self.base_interval.saturating_mul(mult as u32), BACKOFF_CAP);
            g[idx].next_attempt_at =
                Utc::now() + chrono::Duration::from_std(backoff).unwrap_or_default();
        }
        let snapshot = g.clone();
        drop(g);
        atomic_write(&self.path, &serde_json::to_vec(&snapshot)?).await?;
        Ok(exhausted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration as StdDuration;

    struct TempDir(std::path::PathBuf);
    fn tempdir() -> TempDir {
        let p = std::env::temp_dir().join(format!("shitmail-test-{}", uniq()));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
    impl TempDir {
        fn path(&self, leaf: &str) -> std::path::PathBuf {
            self.0.join(leaf)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn fresh_queue() -> (TempDir, RetryQueue) {
        let dir = tempdir();
        let q = RetryQueue::load(dir.path("retry.json"), StdDuration::from_secs(900), 4)
            .await
            .unwrap();
        (dir, q)
    }
    fn uniq() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        nanos.wrapping_add(N.fetch_add(1, Ordering::Relaxed))
    }

    #[tokio::test]
    async fn enqueue_and_due() {
        let (_dir, q) = fresh_queue().await;
        q.enqueue("a".into(), "example.com".into(), "boom".into()).await.unwrap();
        // Just enqueued — next_attempt_at is in the future, so not due.
        assert_eq!(q.due().await.len(), 0);
    }

    #[tokio::test]
    async fn bump_exhausts_after_max() {
        let (_dir, q) = fresh_queue().await;
        q.enqueue("a".into(), "example.com".into(), "boom".into()).await.unwrap();
        assert!(!q.bump("a", "still boom".into()).await.unwrap()); // 1
        assert!(!q.bump("a", "still boom".into()).await.unwrap()); // 2
        assert!(!q.bump("a", "still boom".into()).await.unwrap()); // 3
        let exhausted = q.bump("a", "still boom".into()).await.unwrap(); // 4 == max
        assert!(exhausted);
        assert_eq!(q.due().await.len(), 0);
    }

    #[tokio::test]
    async fn remove_drops_entry() {
        let (_dir, q) = fresh_queue().await;
        q.enqueue("a".into(), "example.com".into(), "boom".into()).await.unwrap();
        q.remove("a").await.unwrap();
        assert_eq!(q.due().await.len(), 0);
    }
}
