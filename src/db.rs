use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{MySql, MySqlPool, Row, Transaction, mysql::MySqlRow};
use uuid::Uuid;

use crate::{
    crypto::Crypto,
    error::{Error, Result},
};

#[derive(Clone)]
pub struct Db {
    pub pool: MySqlPool,
    pub crypto: Crypto,
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailServer {
    pub host: String,
    pub port: u16,
    pub username: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub password: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<MailAuth>,
    #[serde(default = "default_tls_mode")]
    pub tls_mode: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum MailAuth {
    Oauth2 { credential_id: String },
}

impl MailServer {
    pub fn oauth_credential(&self) -> Option<&str> {
        self.auth.as_ref().map(|auth| match auth {
            MailAuth::Oauth2 { credential_id } => credential_id.as_str(),
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    #[default]
    Imap,
    Microsoft,
    Google,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Imap => "imap",
            Self::Microsoft => "microsoft",
            Self::Google => "google",
        }
    }
    pub fn is_native(self) -> bool {
        self != Self::Imap
    }
}

fn default_tls_mode() -> String {
    "implicit".into()
}
fn default_backfill_days() -> u32 {
    30
}

#[derive(Clone, Serialize, Deserialize)]
pub struct AccountInput {
    pub name: String,
    #[serde(default)]
    pub backend: Backend,
    #[serde(default)]
    pub credential_id: Option<String>,
    #[serde(default)]
    pub target_mailbox: Option<String>,
    #[serde(default)]
    pub authorizing_identity: Option<String>,
    #[serde(default)]
    pub shared: bool,
    #[serde(default)]
    pub sync_from: Option<DateTime<Utc>>,
    #[serde(default)]
    pub imap: MailServer,
    #[serde(default)]
    pub smtp: MailServer,
    #[serde(default)]
    pub watched_folders: Vec<String>,
    #[serde(default = "default_backfill_days")]
    pub backfill_days: u32,
    #[serde(default)]
    pub trash_folder: Option<String>,
    #[serde(default)]
    pub sent_folder: Option<String>,
    #[serde(default)]
    pub webhook_url: Option<String>,
    #[serde(default)]
    pub webhook_secret: Option<String>,
}

impl AccountInput {
    pub fn validate(&self) -> Result<()> {
        if self.name.is_empty() || self.name.len() > 255 || self.backfill_days > 36500 {
            return Err(Error::Invalid("invalid name or backfill window".into()));
        }
        if self.backend.is_native() {
            let valid_target = self.target_mailbox.as_deref().is_some_and(|target| {
                target.parse::<lettre::Address>().is_ok()
                    || self.backend == Backend::Microsoft && Uuid::parse_str(target).is_ok()
            });
            if !valid_target {
                return Err(Error::Invalid(
                    "target mailbox requires an email address or Microsoft mailbox UUID".into(),
                ));
            }
            if self
                .credential_id
                .as_ref()
                .is_none_or(|id| Uuid::parse_str(id).is_err())
                || self.target_mailbox.as_ref().is_none_or(|mailbox| {
                    mailbox.is_empty()
                        || mailbox.len() > 320
                        || mailbox.contains(['\r', '\n', '\0', '\x01'])
                })
                || self
                    .authorizing_identity
                    .as_ref()
                    .is_some_and(|value| value.len() > 320 || value.contains(['\r', '\n', '\0']))
            {
                return Err(Error::Invalid(
                    "native accounts require a credential and valid target mailbox".into(),
                ));
            }
        }
        for server in [&self.imap, &self.smtp]
            .into_iter()
            .filter(|_| self.backend == Backend::Imap)
        {
            if server.host.is_empty()
                || server.host.len() > 253
                || server.port == 0
                || server.username.is_empty()
                || server.username.contains(['\r', '\n', '\0', '\x01'])
                || match server.oauth_credential() {
                    Some(id) => !server.password.is_empty() || Uuid::parse_str(id).is_err(),
                    None => server.password.is_empty(),
                }
                || !matches!(server.tls_mode.as_str(), "implicit" | "starttls")
            {
                return Err(Error::Invalid(
                    "mail server requires host, port, credentials and implicit/starttls TLS".into(),
                ));
            }
        }
        if self.watched_folders.len() > 100
            || self
                .watched_folders
                .iter()
                .chain(self.trash_folder.iter())
                .chain(self.sent_folder.iter())
                .any(|name| {
                    name.is_empty()
                        || name.chars().count() > 512
                        || name.contains(['\r', '\n', '\0'])
                })
        {
            return Err(Error::Invalid("invalid folder configuration".into()));
        }
        if self.webhook_url.is_some()
            && self
                .webhook_secret
                .as_ref()
                .is_none_or(|secret| secret.len() < 32)
        {
            return Err(Error::Invalid(
                "webhook secret must contain at least 32 bytes".into(),
            ));
        }
        if let Some(url) = &self.webhook_url {
            let url = reqwest::Url::parse(url)
                .map_err(|_| Error::Invalid("invalid webhook URL".into()))?;
            if url.scheme() != "https"
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.fragment().is_some()
            {
                return Err(Error::Invalid("invalid webhook URL".into()));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    Created,
    Connecting,
    Connected,
    Ready,
    Disconnected,
    AuthFailed,
    Paused,
}

impl LifecycleState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::Ready => "ready",
            Self::Disconnected => "disconnected",
            Self::AuthFailed => "auth_failed",
            Self::Paused => "paused",
        }
    }
    fn parse(value: &str) -> Result<Self> {
        match value {
            "created" => Ok(Self::Created),
            "connecting" => Ok(Self::Connecting),
            "connected" => Ok(Self::Connected),
            "ready" => Ok(Self::Ready),
            "disconnected" => Ok(Self::Disconnected),
            "auth_failed" => Ok(Self::AuthFailed),
            "paused" => Ok(Self::Paused),
            _ => Err(Error::Internal("invalid persisted account state".into())),
        }
    }
}

impl PartialEq<&str> for LifecycleState {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

#[derive(Clone)]
pub struct Account {
    pub id: String,
    pub config: AccountInput,
    pub state: LifecycleState,
    pub smtp_state: String,
    pub last_connected_at: Option<DateTime<Utc>>,
    pub last_disconnected_at: Option<DateTime<Utc>>,
    pub next_retry_at: Option<DateTime<Utc>>,
    pub retry_count: u32,
    pub last_error: Option<String>,
}

impl Account {
    pub fn public(&self) -> Value {
        json!({
            "id":self.id,"name":self.config.name,"backend":self.config.backend,"state":self.state,"smtp_state":self.smtp_state,
            "credential_id":self.config.credential_id,"target_mailbox":self.config.target_mailbox,"authorizing_identity":self.config.authorizing_identity,"shared":self.config.shared,"sync_from":self.config.sync_from,
            "imap":{"host":self.config.imap.host,"port":self.config.imap.port,"username":self.config.imap.username,"tls_mode":self.config.imap.tls_mode,"auth":self.config.imap.auth},
            "smtp":{"host":self.config.smtp.host,"port":self.config.smtp.port,"username":self.config.smtp.username,"tls_mode":self.config.smtp.tls_mode,"auth":self.config.smtp.auth},
            "watched_folders":self.config.watched_folders,"backfill_days":self.config.backfill_days,
            "trash_folder":self.config.trash_folder,"sent_folder":self.config.sent_folder,"webhook_url":self.config.webhook_url,
            "last_connected_at":self.last_connected_at,"last_disconnected_at":self.last_disconnected_at,
            "next_retry_at":self.next_retry_at,"retry_count":self.retry_count,"last_error":self.last_error,
        })
    }
}

fn datetime(row: &MySqlRow, name: &str) -> Result<Option<DateTime<Utc>>> {
    Ok(row
        .try_get::<Option<NaiveDateTime>, _>(name)?
        .map(|date| date.and_utc()))
}

impl Db {
    pub fn new(pool: MySqlPool, crypto: Crypto) -> Self {
        Self { pool, crypto }
    }

    fn decode_account(&self, row: MySqlRow) -> Result<Account> {
        let encrypted: Vec<u8> = row.try_get("config")?;
        let config = serde_json::from_slice(&self.crypto.decrypt(&encrypted)?)
            .map_err(|_| Error::Internal("invalid stored account configuration".into()))?;
        Ok(Account {
            id: row.try_get("id")?,
            config,
            state: LifecycleState::parse(&row.try_get::<String, _>("state")?)?,
            smtp_state: row.try_get("smtp_state")?,
            last_connected_at: datetime(&row, "last_connected_at")?,
            last_disconnected_at: datetime(&row, "last_disconnected_at")?,
            next_retry_at: datetime(&row, "next_retry_at")?,
            retry_count: row.try_get("retry_count")?,
            last_error: row.try_get("last_error")?,
        })
    }

    pub async fn get_account(&self, id: &str) -> Result<Account> {
        let row = sqlx::query("SELECT * FROM accounts WHERE id=?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(Error::NotFound)?;
        self.decode_account(row)
    }

    pub async fn list_accounts(&self) -> Result<Vec<Account>> {
        sqlx::query("SELECT * FROM accounts ORDER BY created_at,id")
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|row| self.decode_account(row))
            .collect()
    }

    pub async fn create_account(&self, mut config: AccountInput) -> Result<Account> {
        config.validate()?;
        config
            .sync_from
            .get_or_insert_with(|| Utc::now() - chrono::Duration::days(30));
        let id = Uuid::new_v4().to_string();
        let plaintext = serde_json::to_vec(&config)
            .map_err(|_| Error::Internal("account serialization failed".into()))?;
        let encrypted = self.crypto.encrypt(&plaintext)?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO accounts (id,config) VALUES (?,?)")
            .bind(&id)
            .bind(encrypted)
            .execute(&mut *tx)
            .await?;
        Self::bind_credentials(&mut tx, &id, &config).await?;
        self.emit(
            &mut tx,
            &id,
            "account.created",
            json!({"previous_state":null,"state":"created"}),
        )
        .await?;
        tx.commit().await?;
        self.get_account(&id).await
    }

    pub async fn create_account_idempotent(
        &self,
        app: &str,
        key: &str,
        mut config: AccountInput,
    ) -> Result<Account> {
        config.validate()?;
        if app.is_empty()
            || app.len() > 128
            || key.is_empty()
            || key.len() > 128
            || key.chars().any(char::is_control)
        {
            return Err(Error::Invalid(
                "Idempotency-Key must contain 1–128 non-control bytes".into(),
            ));
        }
        let request = serde_json::to_vec(&config)
            .map_err(|_| Error::Internal("account serialization failed".into()))?;
        let hash = Sha256::digest(&request).to_vec();
        config
            .sync_from
            .get_or_insert_with(|| Utc::now() - chrono::Duration::days(30));
        let id = Uuid::new_v4().to_string();
        let encrypted = self.crypto.encrypt(
            &serde_json::to_vec(&config)
                .map_err(|_| Error::Internal("account serialization failed".into()))?,
        )?;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "DELETE FROM account_provisioning WHERE deleted_at < UTC_TIMESTAMP(6)-INTERVAL 30 DAY",
        )
        .execute(&mut *tx)
        .await?;
        let inserted = sqlx::query("INSERT IGNORE INTO account_provisioning (app_id,idempotency_key,request_hash,account_id) VALUES (?,?,?,?)")
            .bind(app).bind(key).bind(&hash).bind(&id).execute(&mut *tx).await?.rows_affected() == 1;
        if !inserted {
            let row = sqlx::query("SELECT request_hash,account_id,deleted_at FROM account_provisioning WHERE app_id=? AND idempotency_key=? FOR UPDATE")
                .bind(app).bind(key).fetch_one(&mut *tx).await?;
            if row.try_get::<Vec<u8>, _>("request_hash")? != hash {
                return Err(Error::Conflict(
                    "Idempotency-Key was used for a different account request".into(),
                ));
            }
            if row
                .try_get::<Option<NaiveDateTime>, _>("deleted_at")?
                .is_some()
            {
                return Err(Error::Conflict(
                    "the provisioned account was deleted; use a new Idempotency-Key".into(),
                ));
            }
            let existing: String = row.try_get("account_id")?;
            tx.commit().await?;
            return self.get_account(&existing).await;
        }
        sqlx::query("INSERT INTO accounts (id,config) VALUES (?,?)")
            .bind(&id)
            .bind(encrypted)
            .execute(&mut *tx)
            .await?;
        Self::bind_credentials(&mut tx, &id, &config).await?;
        self.emit(
            &mut tx,
            &id,
            "account.created",
            json!({"previous_state":null,"state":"created"}),
        )
        .await?;
        tx.commit().await?;
        self.get_account(&id).await
    }

    pub async fn update_account(&self, id: &str, mut config: AccountInput) -> Result<Account> {
        config.validate()?;
        let previous_config = self.get_account(id).await?.config;
        if config.backend != previous_config.backend
            || config.target_mailbox != previous_config.target_mailbox
        {
            return Err(Error::Conflict(
                "backend and target mailbox are immutable; provision a new account".into(),
            ));
        }
        if config.sync_from.is_none() {
            config.sync_from = previous_config.sync_from;
        }
        if config.sync_from != previous_config.sync_from {
            return Err(Error::Conflict(
                "sync_from is immutable; provision a new account for another cutoff".into(),
            ));
        }
        let plaintext = serde_json::to_vec(&config)
            .map_err(|_| Error::Internal("account serialization failed".into()))?;
        let encrypted = self.crypto.encrypt(&plaintext)?;
        let mut tx = self.pool.begin().await?;
        let previous: String =
            sqlx::query_scalar("SELECT state FROM accounts WHERE id=? FOR UPDATE")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(Error::NotFound)?;
        let changed = sqlx::query("UPDATE accounts SET config=?,smtp_state='unvalidated',last_failure_event=NULL,last_error=NULL,next_retry_at=NULL,retry_count=0,updated_at=UTC_TIMESTAMP(6) WHERE id=?").bind(encrypted).bind(id).execute(&mut *tx).await?;
        if changed.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        Self::bind_credentials(&mut tx, id, &config).await?;
        self.emit(
            &mut tx,
            id,
            "account.updated",
            json!({"previous_state":previous,"state":previous,"reason":"configuration_change"}),
        )
        .await?;
        tx.commit().await?;
        self.get_account(id).await
    }

    async fn bind_credentials(
        tx: &mut Transaction<'_, MySql>,
        account_id: &str,
        config: &AccountInput,
    ) -> Result<()> {
        sqlx::query("DELETE FROM oauth_account_credentials WHERE account_id=?")
            .bind(account_id)
            .execute(&mut **tx)
            .await?;
        if let Some(id) = config
            .credential_id
            .as_deref()
            .filter(|_| config.backend.is_native())
        {
            let exists: Option<String> =
                sqlx::query_scalar("SELECT id FROM oauth_credentials WHERE id=? FOR SHARE")
                    .bind(id)
                    .fetch_optional(&mut **tx)
                    .await?;
            if exists.is_none() {
                return Err(Error::Invalid("OAuth credential does not exist".into()));
            }
            sqlx::query("INSERT INTO oauth_account_credentials (account_id,protocol,credential_id) VALUES (?,'api',?)").bind(account_id).bind(id).execute(&mut **tx).await?;
        }
        for (protocol, server) in [("imap", &config.imap), ("smtp", &config.smtp)]
            .into_iter()
            .filter(|_| !config.backend.is_native())
        {
            if let Some(id) = server.oauth_credential() {
                let exists: Option<String> =
                    sqlx::query_scalar("SELECT id FROM oauth_credentials WHERE id=? FOR SHARE")
                        .bind(id)
                        .fetch_optional(&mut **tx)
                        .await?;
                if exists.is_none() {
                    return Err(Error::Invalid("OAuth credential does not exist".into()));
                }
                sqlx::query("INSERT INTO oauth_account_credentials (account_id,protocol,credential_id) VALUES (?,?,?)")
                    .bind(account_id).bind(protocol).bind(id).execute(&mut **tx).await?;
            }
        }
        Ok(())
    }

    pub async fn delete_account(&self, id: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT state FROM accounts WHERE id=? FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Error::NotFound)?;
        self.emit(
            &mut tx,
            id,
            "account.deleted",
            json!({"previous_state":row.try_get::<String,_>("state")?,"state":"deleted"}),
        )
        .await?;
        sqlx::query("UPDATE account_provisioning SET deleted_at=UTC_TIMESTAMP(6) WHERE account_id=? AND deleted_at IS NULL").bind(id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM accounts WHERE id=?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn transition(
        &self,
        id: &str,
        state: &str,
        event: &str,
        reason: Option<&str>,
        retryable: bool,
        next_retry: Option<DateTime<Utc>>,
    ) -> Result<()> {
        LifecycleState::parse(state)?;
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT state,last_error,last_failure_event,last_connected_at FROM accounts WHERE id=? FOR UPDATE").bind(id).fetch_optional(&mut *tx).await?.ok_or(Error::NotFound)?;
        let previous: String = row.try_get("state")?;
        let failure = matches!(event, "account.connection_failed" | "account.auth_failed");
        let old_reason: Option<String> = row.try_get("last_error")?;
        let old_failure: Option<String> = row.try_get("last_failure_event")?;
        let emit = should_emit_transition(
            &previous,
            state,
            event,
            reason,
            old_reason.as_deref(),
            old_failure.as_deref(),
        );
        let reconnected = datetime(&row, "last_connected_at")?.is_some();
        sqlx::query("UPDATE accounts SET state=?,last_error=?,last_failure_event=?,next_retry_at=?,retry_count=CASE WHEN ? THEN retry_count+1 WHEN ?='connected' THEN 0 ELSE retry_count END,last_connected_at=CASE WHEN ?='connected' THEN UTC_TIMESTAMP(6) ELSE last_connected_at END,last_disconnected_at=CASE WHEN ?='account.disconnected' THEN UTC_TIMESTAMP(6) ELSE last_disconnected_at END,updated_at=UTC_TIMESTAMP(6) WHERE id=?")
            .bind(state).bind(if failure || event == "account.disconnected" {reason} else if state == "connected" || event == "account.resumed" {None} else {old_reason.as_deref()}).bind(if failure {Some(event)} else if state == "connected" || event == "account.resumed" {None} else {old_failure.as_deref()}).bind(next_retry.map(|date|date.naive_utc())).bind(failure).bind(state).bind(state).bind(event).bind(id).execute(&mut *tx).await?;
        if emit {
            self.emit(&mut tx,id,event,json!({"previous_state":previous,"state":state,"reason":reason,"retryable":retryable,"next_retry_at":next_retry,"reconnected":reconnected})).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn smtp_validation_for(
        &self,
        id: &str,
        expected: &MailServer,
        success: bool,
        reason: Option<&str>,
    ) -> Result<()> {
        self.smtp_validation_revision(id, expected, None, success, reason)
            .await
    }

    pub async fn smtp_validation_revision(
        &self,
        id: &str,
        expected: &MailServer,
        revision: Option<u64>,
        success: bool,
        reason: Option<&str>,
    ) -> Result<()> {
        let state = if success { "validated" } else { "failed" };
        let mut tx = self.pool.begin().await?;
        if let Some(credential) = expected.oauth_credential() {
            let current: Option<u64> =
                sqlx::query_scalar("SELECT revision FROM oauth_credentials WHERE id=? FOR SHARE")
                    .bind(credential)
                    .fetch_optional(&mut *tx)
                    .await?;
            if current.is_none() || current != revision {
                return Ok(());
            }
        }
        let row = sqlx::query("SELECT smtp_state,config FROM accounts WHERE id=? FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Error::NotFound)?;
        let encrypted: Vec<u8> = row.try_get("config")?;
        let current: AccountInput = serde_json::from_slice(&self.crypto.decrypt(&encrypted)?)
            .map_err(|_| Error::Internal("invalid stored account configuration".into()))?;
        if current.smtp != *expected {
            return Ok(());
        }
        let previous: String = row.try_get("smtp_state")?;
        sqlx::query("UPDATE accounts SET smtp_state=?,updated_at=UTC_TIMESTAMP(6) WHERE id=?")
            .bind(state)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        if previous != state {
            self.emit(
                &mut tx,
                id,
                if success {
                    "account.smtp_validated"
                } else {
                    "account.smtp_validation_failed"
                },
                json!({"previous_state":previous,"state":state,"reason":reason}),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn recover_accounts(&self) -> Result<()> {
        for account in self.list_accounts().await? {
            if account.state == "connected" {
                self.transition(
                    &account.id,
                    "disconnected",
                    "account.disconnected",
                    Some("restart_recovery"),
                    true,
                    None,
                )
                .await?;
            }
        }
        sqlx::query("UPDATE events SET delivery_state='pending' WHERE delivery_state='delivering'")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn emit(
        &self,
        tx: &mut Transaction<'_, MySql>,
        account_id: &str,
        event_type: &str,
        payload: Value,
    ) -> Result<u64> {
        let row = sqlx::query("SELECT config FROM accounts WHERE id=?")
            .bind(account_id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(Error::NotFound)?;
        let encrypted: Vec<u8> = row.try_get("config")?;
        let config: AccountInput = serde_json::from_slice(&self.crypto.decrypt(&encrypted)?)
            .map_err(|_| Error::Internal("invalid stored account configuration".into()))?;
        let secret = config
            .webhook_secret
            .as_ref()
            .map(|secret| self.crypto.encrypt(secret.as_bytes()))
            .transpose()?;
        let row = sqlx::query("SELECT last_id FROM event_sequence WHERE id=1 FOR UPDATE")
            .fetch_one(&mut **tx)
            .await?;
        let id = row
            .try_get::<u64, _>("last_id")?
            .checked_add(1)
            .ok_or_else(|| Error::Internal("event sequence exhausted".into()))?;
        sqlx::query("UPDATE event_sequence SET last_id=? WHERE id=1")
            .bind(id)
            .execute(&mut **tx)
            .await?;
        sqlx::query("INSERT INTO events (id,account_id,event_type,payload,webhook_url,webhook_secret,delivery_state) VALUES (?,?,?,?,?,?,?)")
            .bind(id).bind(account_id).bind(event_type).bind(sqlx::types::Json(payload)).bind(&config.webhook_url).bind(secret).bind(if config.webhook_url.is_some(){"pending"}else{"disabled"}).execute(&mut **tx).await?;
        Ok(id)
    }

    pub async fn list_messages(
        &self,
        account_id: &str,
        mailbox: &str,
        after: u64,
        limit: u32,
    ) -> Result<Vec<Value>> {
        let rows = sqlx::query("SELECT uidvalidity,uid,flags,metadata,internal_date,size FROM messages WHERE account_id=? AND mailbox=? AND uid>? ORDER BY uid LIMIT ?").bind(account_id).bind(mailbox).bind(after).bind(limit.clamp(1,500)).fetch_all(&self.pool).await?;
        rows.into_iter().map(|row| Ok(json!({"account_id":account_id,"mailbox":mailbox,"uidvalidity":row.try_get::<u32,_>("uidvalidity")?,"uid":row.try_get::<u32,_>("uid")?,"flags":row.try_get::<sqlx::types::Json<Value>,_>("flags")?.0,"metadata":row.try_get::<sqlx::types::Json<Value>,_>("metadata")?.0,"internal_date":datetime(&row,"internal_date")?,"size":row.try_get::<u32,_>("size")?}))).collect()
    }
}

fn should_emit_transition(
    previous: &str,
    state: &str,
    event: &str,
    reason: Option<&str>,
    old_reason: Option<&str>,
    old_failure: Option<&str>,
) -> bool {
    if matches!(event, "account.connection_failed" | "account.auth_failed") {
        return old_failure != Some(event) || old_reason != reason;
    }
    previous != state
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_failures_are_suppressed() {
        assert!(!should_emit_transition(
            "disconnected",
            "disconnected",
            "account.connection_failed",
            Some("timeout"),
            Some("timeout"),
            Some("account.connection_failed")
        ));
        assert!(should_emit_transition(
            "disconnected",
            "disconnected",
            "account.connection_failed",
            Some("network_error"),
            Some("timeout"),
            Some("account.connection_failed")
        ));
    }

    #[test]
    fn reconnect_and_auth_changes_emit() {
        assert!(should_emit_transition(
            "disconnected",
            "connected",
            "account.connected",
            None,
            None,
            None
        ));
        assert!(should_emit_transition(
            "disconnected",
            "auth_failed",
            "account.auth_failed",
            Some("authentication"),
            Some("timeout"),
            Some("account.connection_failed")
        ));
        assert!(!should_emit_transition(
            "connected",
            "connected",
            "account.connected",
            None,
            None,
            None
        ));
    }
}
