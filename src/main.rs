mod config;
mod dkim;
mod domain;
mod jmap;
mod log;
mod policy;
mod push;
mod retry;
mod state;
mod worker;

use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;

use crate::config::Config;
use crate::domain::cache::AgeCache;
use crate::domain::psl::Psl;
use crate::domain::rdap::RdapClient;
use crate::jmap::Jmap;
use crate::retry::RetryQueue;
use crate::state::StateStore;
use crate::worker::Worker;

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Arc::new(Config::from_env().context("loading config from env")?);
    log_event!(
        "info",
        "boot",
        max_age_days => cfg.max_domain_age_days,
        retry_interval_secs => cfg.retry_interval.as_secs(),
        retry_max_attempts => cfg.retry_max_attempts as u64,
        state_dir => cfg.state_dir.to_string_lossy().into_owned(),
    );

    tokio::fs::create_dir_all(&cfg.state_dir)
        .await
        .with_context(|| format!("creating state directory {:?}", cfg.state_dir))?;

    let psl = Arc::new(
        Psl::load(&cfg.psl_path)
            .with_context(|| format!("loading PSL from {:?}", cfg.psl_path))?,
    );

    let age_cache = AgeCache::load(cfg.state_dir.join("rdap-cache.json"))
        .await
        .context("loading RDAP cache")?;
    let rdap = RdapClient::new(age_cache.clone());

    let state = StateStore::load(cfg.state_dir.join("jmap-state.json"))
        .await
        .context("loading state store")?;

    let retry = RetryQueue::load(
        cfg.state_dir.join("retry-queue.json"),
        cfg.retry_interval,
        cfg.retry_max_attempts,
    )
    .await
    .context("loading retry queue")?;

    let jmap = Arc::new(Jmap::connect(&cfg).await.context("connecting JMAP")?);
    log_event!(
        "info",
        "jmap.ready",
        inbox => jmap.inbox_id.clone(),
        quarantine => jmap.quarantine_id.clone(),
    );

    // Cold start: seed state from "now" so we don't sweep the existing inbox.
    if state.email_state().await.is_none() {
        match jmap.email_changes("").await {
            Ok((_ids, new_state)) => {
                state.set_email_state(new_state.clone()).await?;
                log_event!("info", "state.seeded", email_state => new_state);
            }
            Err(e) => {
                log_event!(
                    "warn",
                    "state.seed_failed",
                    error => format!("{:#}", e),
                    note => "first push event will populate state",
                );
            }
        }
    }

    let (tx, rx) = mpsc::channel::<crate::push::WorkItem>(256);

    let worker = Worker {
        cfg: cfg.clone(),
        jmap: jmap.clone(),
        psl: psl.clone(),
        rdap: rdap.clone(),
        retry: retry.clone(),
    };
    let worker_handle = tokio::spawn(async move {
        worker.run(rx).await;
    });

    let retry_worker = Worker {
        cfg: cfg.clone(),
        jmap: jmap.clone(),
        psl: psl.clone(),
        rdap: rdap.clone(),
        retry: retry.clone(),
    };
    let retry_interval = cfg.retry_interval;
    let retry_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(retry_interval);
        interval.tick().await; // first tick fires immediately; skip it
        loop {
            interval.tick().await;
            retry_worker.retry_due().await;
        }
    });

    let cache_flusher = age_cache.clone();
    let cache_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(e) = cache_flusher.flush().await {
                log_event!("error", "cache.flush_failed", error => format!("{:#}", e));
            }
        }
    });

    let healthz_handle = tokio::spawn(healthz_listener(cfg.healthz_port));

    let push_jmap = jmap.clone();
    let push_state = state.clone();
    let push_tx = tx.clone();
    let push_handle = tokio::spawn(async move {
        push::run(push_jmap, push_tx, push_state).await;
    });

    let mut sigterm = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;
    let mut sigint = signal(SignalKind::interrupt()).context("installing SIGINT handler")?;
    tokio::select! {
        _ = sigterm.recv() => log_event!("info", "shutdown", reason => "sigterm"),
        _ = sigint.recv() => log_event!("info", "shutdown", reason => "sigint"),
    }

    drop(tx);
    push_handle.abort();
    retry_handle.abort();
    cache_handle.abort();
    healthz_handle.abort();
    let _ = age_cache.flush().await;
    let _ = worker_handle.await;
    Ok(())
}

async fn healthz_listener(port: u16) {
    match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(listener) => {
            log_event!("info", "healthz.listening", port => port as u64);
            loop {
                match listener.accept().await {
                    Ok((mut stream, _)) => {
                        let _ = tokio::io::AsyncWriteExt::write_all(
                            &mut stream,
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK",
                        )
                        .await;
                    }
                    Err(e) => {
                        log_event!("warn", "healthz.accept_failed", error => format!("{:#}", e))
                    }
                }
            }
        }
        Err(e) => {
            log_event!("error", "healthz.bind_failed", port => port as u64, error => format!("{:#}", e));
        }
    }
}
