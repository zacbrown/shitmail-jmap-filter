use anyhow::Result;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::dkim;
use crate::domain::psl::Psl;
use crate::domain::rdap::{age_days, RdapClient, RdapError};
use crate::jmap::Jmap;
use crate::log_event;
use crate::policy::should_quarantine;
use crate::push::WorkItem;
use crate::retry::RetryQueue;

pub struct Worker {
    pub cfg: Arc<Config>,
    pub jmap: Arc<Jmap>,
    pub psl: Arc<Psl>,
    pub rdap: RdapClient,
    pub retry: RetryQueue,
}

impl Worker {
    pub async fn run(self, mut rx: mpsc::Receiver<WorkItem>) {
        while let Some(item) = rx.recv().await {
            match item {
                WorkItem::Process(id) => {
                    if let Err(e) = self.process(&id).await {
                        log_event!("error", "email.process_failed", id => id.clone(), error => format!("{:#}", e));
                    }
                }
            }
        }
    }

    async fn process(&self, id: &str) -> Result<()> {
        let started = Instant::now();
        let Some(facts) = self.jmap.get_email_facts(id).await? else {
            log_event!("info", "email.skip", id => id, reason => "not_found");
            return Ok(());
        };
        if !facts.mailbox_ids.iter().any(|m| m == &self.jmap.inbox_id) {
            log_event!("info", "email.skip", id => id, reason => "not_in_inbox");
            return Ok(());
        }

        let from = facts.from_addresses.first().cloned();
        let from_domain = from.as_deref().and_then(domain_of_email);
        let from_registrable = from_domain
            .as_deref()
            .and_then(|d| self.psl.registrable_domain(d));

        let verdict = dkim::extract_verified_domain(
            &facts.auth_results_headers,
            &self.cfg.trusted_authserv_ids,
            from_registrable.as_deref(),
            |d| self.psl.registrable_domain(d),
        );

        let Some(verdict) = verdict else {
            log_event!(
                "info",
                "email.moved",
                id => id,
                from => from.clone().unwrap_or_default(),
                reason => "unsigned",
                latency_ms => started.elapsed().as_millis() as u64,
            );
            self.jmap.move_to_quarantine(id).await?;
            return Ok(());
        };

        let registrable = match self.psl.registrable_domain(&verdict.signing_domain) {
            Some(d) => d,
            None => verdict.signing_domain.clone(),
        };

        let registration = match self.rdap.registration_date(&registrable).await {
            Ok(d) => d,
            Err(e) => {
                let err_str = format!("{}", e);
                log_event!(
                    "warn",
                    "email.deferred",
                    id => id,
                    domain => registrable.clone(),
                    error => err_str.clone(),
                );
                self.retry.enqueue(id.to_string(), registrable, err_str).await?;
                return Ok(());
            }
        };
        let age = age_days(registration);
        let quarantine = should_quarantine(age, self.cfg.max_domain_age_days);

        log_event!(
            "info",
            "email.evaluated",
            id => id,
            from => from.clone().unwrap_or_default(),
            dkim_domain => verdict.signing_domain.clone(),
            aligned => verdict.aligned,
            registrable => registrable.clone(),
            age_days => age,
            registered => registration.to_rfc3339(),
        );

        if quarantine {
            self.jmap.move_to_quarantine(id).await?;
            log_event!(
                "info",
                "email.moved",
                id => id,
                reason => "young_domain",
                age_days => age,
                latency_ms => started.elapsed().as_millis() as u64,
            );
        }
        Ok(())
    }

    pub async fn retry_due(&self) {
        let due = self.retry.due().await;
        for entry in due {
            let id = entry.id.clone();
            // Still in inbox?
            let facts = match self.jmap.get_email_facts(&id).await {
                Ok(Some(f)) => f,
                Ok(None) => {
                    log_event!("info", "retry.dropped", id => id.clone(), reason => "not_found");
                    let _ = self.retry.remove(&id).await;
                    continue;
                }
                Err(e) => {
                    log_event!("error", "retry.get_failed", id => id.clone(), error => format!("{:#}", e));
                    continue;
                }
            };
            if !facts.mailbox_ids.iter().any(|m| m == &self.jmap.inbox_id) {
                log_event!("info", "retry.dropped", id => id.clone(), reason => "moved");
                let _ = self.retry.remove(&id).await;
                continue;
            }

            match self.rdap.registration_date(&entry.registrable_domain).await {
                Ok(registration) => {
                    let age = age_days(registration);
                    let quarantine = should_quarantine(age, self.cfg.max_domain_age_days);
                    log_event!(
                        "info",
                        "retry.resolved",
                        id => id.clone(),
                        registrable => entry.registrable_domain.clone(),
                        age_days => age,
                        action => if quarantine { "quarantine" } else { "keep" },
                    );
                    if quarantine {
                        if let Err(e) = self.jmap.move_to_quarantine(&id).await {
                            log_event!("error", "retry.move_failed", id => id.clone(), error => format!("{:#}", e));
                            continue;
                        }
                    }
                    let _ = self.retry.remove(&id).await;
                }
                Err(RdapError::UnknownTld(_)) | Err(RdapError::Lookup(_)) | Err(RdapError::NoRegistrationEvent) => {
                    match self.retry.bump(&id, "rdap_failed".into()).await {
                        Ok(true) => {
                            log_event!("warn", "retry.exhausted", id => id.clone(), registrable => entry.registrable_domain.clone());
                        }
                        Ok(false) => {
                            log_event!("info", "retry.backoff", id => id.clone(), attempts => entry.attempts as u64 + 1);
                        }
                        Err(e) => {
                            log_event!("error", "retry.persist_failed", id => id.clone(), error => format!("{:#}", e));
                        }
                    }
                }
            }
        }
    }
}

fn domain_of_email(addr: &str) -> Option<String> {
    addr.rsplit_once('@').map(|(_, d)| d.trim().trim_matches('>').to_ascii_lowercase())
}
