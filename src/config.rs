use anyhow::{anyhow, Context, Result};
use std::env;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub fastmail_api_token: String,
    pub jmap_base_url: String,
    pub jmap_session_host: String,
    pub quarantine_mailbox_name: String,
    pub max_domain_age_days: i64,
    pub retry_interval: Duration,
    pub retry_max_attempts: u32,
    pub state_dir: PathBuf,
    pub psl_path: PathBuf,
    pub trusted_authserv_ids: Vec<String>,
    pub healthz_port: u16,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let fastmail_api_token = env::var("FASTMAIL_API_TOKEN")
            .context("FASTMAIL_API_TOKEN env var is required")?;
        if fastmail_api_token.trim().is_empty() {
            return Err(anyhow!("FASTMAIL_API_TOKEN must not be empty"));
        }

        let jmap_base_url = env::var("JMAP_BASE_URL")
            .unwrap_or_else(|_| "https://api.fastmail.com".to_string());

        let jmap_session_host = reqwest::Url::parse(&jmap_base_url)
            .context("JMAP_BASE_URL is not a valid URL")?
            .host_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("JMAP_BASE_URL has no host"))?;

        let quarantine_mailbox_name =
            env::var("QUARANTINE_MAILBOX_NAME").unwrap_or_else(|_| "quarantine".to_string());

        let max_domain_age_days = env::var("MAX_DOMAIN_AGE_DAYS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(365);

        let retry_interval_min: u64 = env::var("RETRY_INTERVAL_MIN")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(15);
        let retry_interval = Duration::from_secs(retry_interval_min * 60);

        let retry_max_attempts: u32 = env::var("RETRY_MAX_ATTEMPTS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(32);

        let state_dir =
            PathBuf::from(env::var("STATE_DIR").unwrap_or_else(|_| "/data".to_string()));

        let psl_path = PathBuf::from(
            env::var("PSL_PATH").unwrap_or_else(|_| "data/public_suffix_list.dat".to_string()),
        );

        let trusted_authserv_ids = env::var("TRUSTED_AUTHSERV_IDS")
            .unwrap_or_else(|_| "fastmail.com,messagingengine.com".to_string())
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();

        let healthz_port: u16 = env::var("HEALTHZ_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8080);

        Ok(Self {
            fastmail_api_token,
            jmap_base_url,
            jmap_session_host,
            quarantine_mailbox_name,
            max_domain_age_days,
            retry_interval,
            retry_max_attempts,
            state_dir,
            psl_path,
            trusted_authserv_ids,
            healthz_port,
        })
    }
}
