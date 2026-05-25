use anyhow::Result;
use futures_util::StreamExt;
use jmap_client::event_source::PushNotification;
use jmap_client::DataType;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::jmap::Jmap;
use crate::log_event;

#[derive(Debug, Clone)]
pub enum WorkItem {
    Process(String),
}

pub async fn run(jmap: Arc<Jmap>, tx: mpsc::Sender<WorkItem>, state: crate::state::StateStore) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match subscribe_and_dispatch(&jmap, &tx, &state).await {
            Ok(()) => {
                log_event!("info", "push.disconnected", reason => "stream_ended");
            }
            Err(e) => {
                log_event!("warn", "push.disconnected", reason => format!("{:#}", e));
            }
        }
        log_event!("info", "push.reconnect_wait", seconds => backoff.as_secs());
        tokio::time::sleep(backoff).await;
        backoff = std::cmp::min(backoff * 2, Duration::from_secs(30));
    }
}

async fn subscribe_and_dispatch(
    jmap: &Jmap,
    tx: &mpsc::Sender<WorkItem>,
    state: &crate::state::StateStore,
) -> Result<()> {
    let mut stream = jmap
        .client
        .event_source(
            Some(vec![DataType::Email, DataType::Mailbox]),
            false,
            Some(60),
            None,
        )
        .await?;
    log_event!("info", "push.connected", types => "Email,Mailbox");

    while let Some(event) = stream.next().await {
        let event = match event {
            Ok(e) => e,
            Err(e) => {
                log_event!("warn", "push.event_error", error => format!("{:#}", e));
                continue;
            }
        };
        match event {
            PushNotification::StateChange(changes) => {
                let account_id = jmap.client.default_account_id().to_string();
                let new_email_state = changes
                    .changes(&account_id)
                    .and_then(|mut iter| {
                        iter.find_map(|(t, s)| {
                            if matches!(t, DataType::Email) { Some(s.to_string()) } else { None }
                        })
                    });
                if let Some(new_state) = new_email_state {
                    log_event!("info", "push.event", account => account_id.clone(), new_email_state => new_state.clone());
                    if state.email_state().await.is_none() {
                        if let Err(e) = state.set_email_state(new_state.clone()).await {
                            log_event!("error", "state.persist_failed", error => format!("{:#}", e));
                        } else {
                            log_event!("info", "state.bootstrapped_from_push", email_state => new_state);
                        }
                    } else {
                        sweep(jmap, tx, state).await;
                    }
                }
            }
            other => {
                log_event!("debug", "push.other_event", payload => format!("{:?}", other));
            }
        }
    }
    Ok(())
}

async fn sweep(jmap: &Jmap, tx: &mpsc::Sender<WorkItem>, state: &crate::state::StateStore) {
    let Some(since) = state.email_state().await else {
        log_event!("warn", "push.sweep_skipped", reason => "no_state");
        return;
    };
    match jmap.email_changes(&since).await {
        Ok((ids, new_state)) => {
            if !ids.is_empty() {
                log_event!("info", "push.sweep", count => ids.len() as u64);
            }
            for id in ids {
                if tx.send(WorkItem::Process(id)).await.is_err() {
                    log_event!("error", "push.dispatch_closed");
                    return;
                }
            }
            if let Err(e) = state.set_email_state(new_state).await {
                log_event!("error", "state.persist_failed", error => format!("{:#}", e));
            }
        }
        Err(e) => {
            log_event!("error", "push.sweep_failed", error => format!("{:#}", e));
        }
    }
}
