use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

use super::cache::{AgeCache, CacheEntry};

#[derive(Debug)]
pub enum RdapError {
    UnknownTld(String),
    NoRegistrationEvent,
    Lookup(String),
}

impl std::fmt::Display for RdapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RdapError::UnknownTld(t) => write!(f, "no RDAP service for TLD .{}", t),
            RdapError::NoRegistrationEvent => write!(f, "RDAP response had no registration event"),
            RdapError::Lookup(s) => write!(f, "RDAP lookup failed: {}", s),
        }
    }
}

impl std::error::Error for RdapError {}

#[derive(Debug, Deserialize)]
struct RdapBootstrap {
    services: Vec<(Vec<String>, Vec<String>)>,
}

#[derive(Debug, Deserialize)]
struct RdapDomain {
    #[serde(default)]
    events: Vec<RdapEvent>,
}

#[derive(Debug, Deserialize)]
struct RdapEvent {
    #[serde(rename = "eventAction")]
    event_action: String,
    #[serde(rename = "eventDate")]
    event_date: DateTime<Utc>,
}

#[derive(Clone)]
pub struct RdapClient {
    http: reqwest::Client,
    bootstrap: Arc<RwLock<HashMap<String, String>>>,
    cache: AgeCache,
}

impl RdapClient {
    pub fn new(cache: AgeCache) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent("shitmail-jmap-filter/0.1")
            .build()
            .expect("building reqwest client");
        Self { http, bootstrap: Arc::new(RwLock::new(HashMap::new())), cache }
    }

    /// Returns the registration `DateTime` for `registrable_domain`.
    /// Caches positive and negative results.
    pub async fn registration_date(
        &self,
        registrable_domain: &str,
    ) -> Result<DateTime<Utc>, RdapError> {
        if let Some(hit) = self.cache.get(registrable_domain).await {
            return match (hit.registered, hit.error) {
                (Some(d), _) => Ok(d),
                (None, Some(msg)) if msg == "unknown_tld" => {
                    let tld = registrable_domain.rsplit('.').next().unwrap_or("").to_string();
                    Err(RdapError::UnknownTld(tld))
                }
                (None, Some(msg)) => Err(RdapError::Lookup(msg)),
                (None, None) => Err(RdapError::NoRegistrationEvent),
            };
        }

        match self.lookup(registrable_domain).await {
            Ok(date) => {
                self.cache
                    .put(
                        registrable_domain,
                        CacheEntry { registered: Some(date), fetched_at: Utc::now(), error: None },
                    )
                    .await;
                Ok(date)
            }
            Err(err) => {
                let err_str = match &err {
                    RdapError::UnknownTld(_) => "unknown_tld".to_string(),
                    other => other.to_string(),
                };
                self.cache
                    .put(
                        registrable_domain,
                        CacheEntry { registered: None, fetched_at: Utc::now(), error: Some(err_str) },
                    )
                    .await;
                Err(err)
            }
        }
    }

    async fn lookup(&self, registrable_domain: &str) -> Result<DateTime<Utc>, RdapError> {
        let tld = registrable_domain
            .rsplit('.')
            .next()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| RdapError::Lookup(format!("malformed domain {:?}", registrable_domain)))?;

        let base = self
            .registry_for(tld)
            .await
            .map_err(|e| RdapError::Lookup(e.to_string()))?
            .ok_or_else(|| RdapError::UnknownTld(tld.to_string()))?;

        let url = format!("{}domain/{}", ensure_trailing_slash(&base), registrable_domain);
        let resp = self
            .http
            .get(&url)
            .header("Accept", "application/rdap+json")
            .send()
            .await
            .map_err(|e| RdapError::Lookup(format!("request: {}", e)))?;

        if !resp.status().is_success() {
            return Err(RdapError::Lookup(format!("HTTP {} from {}", resp.status(), url)));
        }
        let body: RdapDomain = resp
            .json()
            .await
            .map_err(|e| RdapError::Lookup(format!("json decode: {}", e)))?;

        body.events
            .into_iter()
            .find(|e| e.event_action.eq_ignore_ascii_case("registration"))
            .map(|e| e.event_date)
            .ok_or(RdapError::NoRegistrationEvent)
    }

    async fn registry_for(&self, tld: &str) -> Result<Option<String>> {
        {
            let map = self.bootstrap.read().await;
            if !map.is_empty() {
                return Ok(map.get(tld).cloned());
            }
        }
        let mut map = self.bootstrap.write().await;
        if map.is_empty() {
            let bs: RdapBootstrap = self
                .http
                .get("https://data.iana.org/rdap/dns.json")
                .send()
                .await
                .context("fetching RDAP bootstrap")?
                .json()
                .await
                .context("decoding RDAP bootstrap")?;
            for (tlds, urls) in bs.services {
                if let Some(url) = urls.into_iter().next() {
                    for t in tlds {
                        map.insert(t.to_ascii_lowercase(), url.clone());
                    }
                }
            }
        }
        Ok(map.get(tld).cloned())
    }
}

fn ensure_trailing_slash(s: &str) -> String {
    if s.ends_with('/') {
        s.to_string()
    } else {
        format!("{}/", s)
    }
}

pub fn age_days(registered: DateTime<Utc>) -> i64 {
    (Utc::now() - registered).num_days()
}
