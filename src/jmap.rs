use anyhow::{anyhow, Context, Result};
use jmap_client::client::{Client, Credentials};
use jmap_client::email::{Header, HeaderForm, HeaderValue, Property};
use jmap_client::mailbox::{self, Role};

use crate::config::Config;

pub struct Jmap {
    pub client: Client,
    pub inbox_id: String,
    pub quarantine_id: String,
}

#[derive(Debug, Clone)]
pub struct EmailFacts {
    pub mailbox_ids: Vec<String>,
    pub from_addresses: Vec<String>,
    pub auth_results_headers: Vec<String>,
}

impl Jmap {
    pub async fn connect(cfg: &Config) -> Result<Self> {
        let client = Client::new()
            .credentials(Credentials::bearer(cfg.fastmail_api_token.clone()))
            .follow_redirects([cfg.jmap_session_host.clone()])
            .connect(&cfg.jmap_base_url)
            .await
            .context("connecting to JMAP server")?;

        let inbox_id = mailbox_id_by_role(&client, Role::Inbox)
            .await?
            .ok_or_else(|| anyhow!("no Inbox mailbox on account"))?;

        let quarantine_id =
            ensure_mailbox(&client, &cfg.quarantine_mailbox_name).await?;

        Ok(Self { client, inbox_id, quarantine_id })
    }

    pub async fn get_email_facts(&self, id: &str) -> Result<Option<EmailFacts>> {
        let ar_header = Header {
            name: "Authentication-Results".to_string(),
            form: HeaderForm::Raw,
            all: true,
        };
        let email = self
            .client
            .email_get(
                id,
                Some(vec![
                    Property::MailboxIds,
                    Property::From,
                    Property::Header(ar_header.clone()),
                ]),
            )
            .await
            .context("email/get")?;
        let Some(email) = email else {
            return Ok(None);
        };

        let mailbox_ids: Vec<String> =
            email.mailbox_ids().into_iter().map(|s| s.to_string()).collect();
        let from_addresses: Vec<String> = email
            .from()
            .map(|addrs| addrs.iter().map(|a| a.email().to_string()).collect())
            .unwrap_or_default();
        let auth_results_headers: Vec<String> = match email.header(&ar_header) {
            Some(HeaderValue::AsTextAll(v)) => v.clone(),
            Some(HeaderValue::AsText(s)) => vec![s.clone()],
            _ => Vec::new(),
        };

        Ok(Some(EmailFacts {
            mailbox_ids,
            from_addresses,
            auth_results_headers,
        }))
    }

    pub async fn move_to_quarantine(&self, id: &str) -> Result<()> {
        self.client
            .email_set_mailboxes(id, [&self.quarantine_id])
            .await
            .context("email/set mailboxes")?;
        Ok(())
    }

    /// Returns (created_ids, new_state).
    pub async fn email_changes(&self, since_state: &str) -> Result<(Vec<String>, String)> {
        let mut resp = self
            .client
            .email_changes(since_state.to_string(), None)
            .await
            .context("email/changes")?;
        let created = resp.take_created();
        let updated = resp.take_updated();
        let mut all = created;
        all.extend(updated);
        all.sort();
        all.dedup();
        Ok((all, resp.new_state().to_string()))
    }

}

async fn mailbox_id_by_role(client: &Client, role: Role) -> Result<Option<String>> {
    let mut resp = client
        .mailbox_query(Some(mailbox::query::Filter::role(role)), None::<Vec<_>>)
        .await
        .context("mailbox/query by role")?;
    Ok(resp.take_ids().pop())
}

async fn mailbox_id_by_name(client: &Client, name: &str) -> Result<Option<String>> {
    let mut resp = client
        .mailbox_query(
            Some(mailbox::query::Filter::name(name.to_string())),
            None::<Vec<_>>,
        )
        .await
        .context("mailbox/query by name")?;
    Ok(resp.take_ids().pop())
}

async fn ensure_mailbox(client: &Client, name: &str) -> Result<String> {
    if let Some(id) = mailbox_id_by_name(client, name).await? {
        return Ok(id);
    }
    let mut mbox = client
        .mailbox_create(name.to_string(), None::<String>, Role::None)
        .await
        .context("mailbox/create quarantine")?;
    Ok(mbox.take_id())
}

