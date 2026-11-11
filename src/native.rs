use crate::{
    accounts::Accounts,
    config::Config,
    db::{Account, Backend, Db},
    error::{Error, Result},
    providers::{NativeClient, ProviderMessage},
};
use chrono::{NaiveDateTime, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{MySql, Row, Transaction};
use std::{collections::HashSet, sync::Arc, time::Duration};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub struct Runtime {
    pub db: Arc<Db>,
    pub config: Arc<Config>,
    pub accounts: Arc<Accounts>,
    pub client: NativeClient,
}
impl Runtime {
    pub fn new(db: Arc<Db>, config: Arc<Config>, accounts: Arc<Accounts>) -> Self {
        let client = NativeClient::new(config.clone(), accounts.oauth.clone());
        Self {
            db,
            config,
            accounts,
            client,
        }
    }
    pub async fn schedule(&self, id: &str) -> Result<String> {
        let account = self.db.get_account(id).await?;
        if account.state == "paused" {
            return Err(Error::Conflict("account is paused".into()));
        }
        let mut tx = self.db.pool.begin().await?;
        sqlx::query("SELECT id FROM accounts WHERE id=? FOR UPDATE")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO native_state(account_id,wake_generation) VALUES(?,1) ON DUPLICATE KEY UPDATE wake_generation=wake_generation+1,next_sync_at=UTC_TIMESTAMP(6)").bind(id).execute(&mut *tx).await?;
        if let Some(run) = sqlx::query_scalar::<_, String>(
            "SELECT id FROM sync_runs WHERE account_id=? AND state IN ('queued','running') LIMIT 1",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        {
            tx.commit().await?;
            return Ok(run);
        }
        let run = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO sync_runs(id,account_id) VALUES(?,?)")
            .bind(&run)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO native_state(account_id) VALUES(?) ON DUPLICATE KEY UPDATE next_sync_at=UTC_TIMESTAMP(6)").bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(run)
    }
    pub async fn run(self: Arc<Self>, shutdown: CancellationToken) -> Result<()> {
        sqlx::query("UPDATE sync_runs SET state='queued' WHERE state='running'")
            .execute(&self.db.pool)
            .await?;
        let mut tasks = JoinSet::new();
        let mut active = HashSet::new();
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        let mut refresh = tokio::time::interval(Duration::from_secs(30));
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = refresh.tick() => {
                    for account in self.db.list_accounts().await? {
                        if account.config.backend.is_native() {
                            sqlx::query("INSERT IGNORE INTO native_state(account_id) VALUES(?)")
                                .bind(&account.id).execute(&self.db.pool).await?;
                        }
                    }
                }
                _ = tick.tick() => {
                    let ids:Vec<String>=sqlx::query_scalar("SELECT n.account_id FROM native_state n JOIN accounts a ON a.id=n.account_id WHERE n.next_sync_at<=UTC_TIMESTAMP(6) AND a.state NOT IN ('paused','auth_failed') ORDER BY n.next_sync_at LIMIT 1000")
                        .fetch_all(&self.db.pool).await?;
                    for id in ids {
                        if tasks.len()>=self.config.native.concurrency { break; }
                        if active.insert(id.clone()) {
                            let runtime=self.clone();
                            tasks.spawn(async move { let result=runtime.cycle(&id).await; (id,result) });
                        }
                    }
                }
                completed = tasks.join_next(), if !tasks.is_empty() => {
                    if let Some(result)=completed {
                        let (id,result)=result.map_err(|_|Error::Internal("native worker failed".into()))?;
                        active.remove(&id);
                        if let Err(error)=result { self.failed(&id,error).await?; }
                    }
                }
            }
        }
        while let Some(result) = tasks.join_next().await {
            if result.is_err() {
                tracing::warn!("native task failed during shutdown");
            }
        }
        Ok(())
    }
    async fn failed(&self, id: &str, error: Error) -> Result<()> {
        if matches!(error, Error::NotFound) {
            // Only deletion of the local account makes a missing resource harmless.
            match self.db.get_account(id).await {
                Err(Error::NotFound) => return Ok(()),
                Err(error) => return Err(error),
                Ok(_) => {}
            }
        }
        if matches!(
            error,
            Error::OAuth {
                retryable: false,
                ..
            }
        ) {
            let transition = self
                .db
                .transition(
                    id,
                    "auth_failed",
                    "account.auth_failed",
                    Some(sanitized(&error)),
                    false,
                    None,
                )
                .await;
            match transition {
                Ok(_) => {}
                Err(Error::NotFound) => return Ok(()),
                Err(e) => return Err(e),
            }
        }
        tracing::warn!(account_id=%id,error=%error,"native sync failed");
        let delay = match error {
            Error::Throttled(seconds) => seconds.clamp(1, 86400),
            _ => self.config.native.poll_secs,
        };
        sqlx::query("UPDATE native_state SET last_error=?,next_sync_at=UTC_TIMESTAMP(6)+INTERVAL ? SECOND WHERE account_id=?")
            .bind(sanitized(&error)).bind(delay).bind(id).execute(&self.db.pool).await?;
        sqlx::query("UPDATE sync_runs SET state='failed',finished_at=UTC_TIMESTAMP(6) WHERE account_id=? AND state='running'")
            .bind(id).execute(&self.db.pool).await?;
        Ok(())
    }
    async fn cycle(&self, id: &str) -> Result<()> {
        let generation: u64 =
            sqlx::query_scalar("SELECT wake_generation FROM native_state WHERE account_id=?")
                .bind(id)
                .fetch_one(&self.db.pool)
                .await?;
        let account = self.db.get_account(id).await?;
        if account.state == "paused" {
            return Ok(());
        }
        sqlx::query("UPDATE sync_runs SET state='running' WHERE account_id=? AND state='queued'")
            .bind(id)
            .execute(&self.db.pool)
            .await?;
        if account.config.backend.is_native()
            && matches!(account.state.as_str(), "created" | "disconnected")
        {
            self.client.validate_account(&account).await?;
        }
        if !account.config.backend.is_native() {
            self.accounts
                .execute(id, crate::imap::Command::Sync)
                .await?;
            let mut tx = self.db.pool.begin().await?;
            self.guard(&mut tx, &account).await?;
            let current: u64 = sqlx::query_scalar(
                "SELECT wake_generation FROM native_state WHERE account_id=? FOR UPDATE",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            sqlx::query("UPDATE native_state SET last_sync_at=UTC_TIMESTAMP(6),last_error=NULL,next_sync_at=IF(?, '9999-01-01 00:00:00',UTC_TIMESTAMP(6)) WHERE account_id=?").bind(current==generation).bind(id).execute(&mut *tx).await?;
            sqlx::query("UPDATE sync_runs SET state=?,finished_at=IF(?,UTC_TIMESTAMP(6),NULL) WHERE account_id=? AND state='running'").bind(if current==generation{"completed"}else{"queued"}).bind(current==generation).bind(id).execute(&mut *tx).await?;
            tx.commit().await?;
            return Ok(());
        }
        let folders = if account.config.backend == Backend::Google {
            vec![String::new()]
        } else if account.config.watched_folders.is_empty() {
            vec!["inbox".into()]
        } else {
            account.config.watched_folders.clone()
        };
        for folder in folders {
            self.sync_folder(&account, &folder).await?;
        }
        self.renew_subscription(&account).await;
        let has_push: bool = sqlx::query_scalar::<_, Option<bool>>(
            "SELECT subscription_expires_at>UTC_TIMESTAMP(6) FROM native_state WHERE account_id=?",
        )
        .bind(id)
        .fetch_one(&self.db.pool)
        .await?
        .unwrap_or(false);
        let delay = if has_push {
            self.config.native.safety_poll_secs
        } else {
            self.config.native.poll_secs
        };
        let mut tx = self.db.pool.begin().await?;
        self.guard(&mut tx, &account).await?;
        sqlx::query("UPDATE native_state SET last_sync_at=UTC_TIMESTAMP(6),last_error=NULL,next_sync_at=IF(wake_generation=?,UTC_TIMESTAMP(6)+INTERVAL ? SECOND,UTC_TIMESTAMP(6)) WHERE account_id=?").bind(generation).bind(delay+u64::from(rand::random::<u8>()%4)).bind(id).execute(&mut *tx).await?;
        let current_generation: u64 = sqlx::query_scalar(
            "SELECT wake_generation FROM native_state WHERE account_id=? FOR UPDATE",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("UPDATE sync_runs SET state=?,finished_at=IF(?,UTC_TIMESTAMP(6),NULL) WHERE account_id=? AND state='running'").bind(if current_generation==generation {"completed"}else{"queued"}).bind(current_generation==generation).bind(id).execute(&mut *tx).await?;
        if account.state != "ready" {
            sqlx::query(
                "UPDATE accounts SET state='ready',last_error=NULL WHERE id=? AND state<>'paused'",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
            self.db
                .emit(&mut tx, id, "account.ready", json!({"state":"ready"}))
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
    async fn guard(&self, tx: &mut Transaction<'_, MySql>, account: &Account) -> Result<()> {
        let row = sqlx::query("SELECT state,config FROM accounts WHERE id=? FOR UPDATE")
            .bind(&account.id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(Error::NotFound)?;
        let state: String = row.try_get("state")?;
        let bytes: Vec<u8> = row.try_get("config")?;
        let current: Value = serde_json::from_slice(&self.db.crypto.decrypt(&bytes)?)
            .map_err(|_| Error::Internal("invalid account configuration".into()))?;
        let expected = serde_json::to_value(&account.config)
            .map_err(|_| Error::Internal("invalid account configuration".into()))?;
        if state == "paused" || current != expected {
            return Err(Error::Conflict(
                "account changed during synchronization".into(),
            ));
        }
        Ok(())
    }
    async fn sync_folder(&self, account: &Account, folder: &str) -> Result<()> {
        let id = &account.id;
        sqlx::query("INSERT IGNORE INTO native_checkpoints(account_id,folder) VALUES(?,?)")
            .bind(id)
            .bind(folder)
            .execute(&self.db.pool)
            .await?;
        let row = sqlx::query("SELECT * FROM native_checkpoints WHERE account_id=? AND folder=?")
            .bind(id)
            .bind(folder)
            .fetch_one(&self.db.pool)
            .await?;
        let recovery: bool = row.try_get("recovery")?;
        let mut cursor: Option<String> = row.try_get("sync_cursor")?;
        let mut next: Option<String> = row.try_get("next_page")?;
        let last_full: Option<NaiveDateTime> = row.try_get("last_full_at")?;
        let delegated_shared = self.client.delegated_shared(account).await?;
        let full = cursor.is_none()
            || (delegated_shared
                && last_full.is_none_or(|d| {
                    (Utc::now().naive_utc() - d).num_seconds()
                        >= self.config.native.reconcile_secs as i64
                }));
        let mut scan: Option<String> = row.try_get("scan_id")?;
        if full && scan.is_none() {
            scan = Some(Uuid::new_v4().to_string());
        }
        for _ in 0..1000 {
            let page = self
                .client
                .sync_page(account, folder, cursor.as_deref(), next.as_deref(), full)
                .await?;
            if page.reset {
                sqlx::query("UPDATE native_checkpoints SET sync_cursor=NULL,next_page=NULL,scan_id=NULL,recovery=TRUE WHERE account_id=? AND folder=?").bind(id).bind(folder).execute(&self.db.pool).await?;
                return Err(Error::Unavailable(
                    "provider cursor reset; recovery scheduled".into(),
                ));
            }
            let mut tx = self.db.pool.begin().await?;
            self.guard(&mut tx, account).await?;
            for message in &page.messages {
                let eligibility_at = message
                    .metadata
                    .get("eligibility_at")
                    .and_then(Value::as_str)
                    .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
                    .map(|v| v.with_timezone(&Utc))
                    .unwrap_or(message.received_at);
                if account
                    .config
                    .sync_from
                    .is_some_and(|cutoff| eligibility_at < cutoff)
                {
                    continue;
                }
                if account.config.backend == Backend::Google {
                    let watched = if account.config.watched_folders.is_empty() {
                        vec!["INBOX".into()]
                    } else {
                        account.config.watched_folders.clone()
                    };
                    if !message.locations.iter().any(|l| watched.contains(l)) {
                        self.remove(&mut tx, id, &message.id, "").await?;
                        continue;
                    }
                }
                self.persist(
                    &mut tx,
                    account,
                    message,
                    if recovery {
                        "recovery"
                    } else if cursor.is_none() {
                        "initial_sync"
                    } else if full {
                        "recovery"
                    } else {
                        "live"
                    },
                )
                .await?;
                if let Some(scan) = &scan {
                    sqlx::query("INSERT INTO native_seen(account_id,folder,scan_id,provider_hash) VALUES(?,?,?,?) ON DUPLICATE KEY UPDATE scan_id=VALUES(scan_id)").bind(id).bind(folder).bind(scan).bind(Sha256::digest(message.id.as_bytes()).to_vec()).execute(&mut *tx).await?;
                }
            }
            for removed in &page.removed {
                self.remove(&mut tx, id, removed, folder).await?;
            }
            if page.next.is_none() && full {
                let rows=sqlx::query("SELECT m.provider_id FROM mail_messages m JOIN message_locations l ON l.message_id=m.id WHERE m.account_id=? AND l.active=TRUE AND (?='' OR l.folder=?)").bind(id).bind(folder).bind(folder).fetch_all(&mut *tx).await?;
                for row in rows {
                    let provider_id: String = row.try_get_unchecked("provider_id")?;
                    let seen:Option<i32>=sqlx::query_scalar("SELECT 1 FROM native_seen WHERE account_id=? AND folder=? AND scan_id=? AND provider_hash=?").bind(id).bind(folder).bind(&scan).bind(Sha256::digest(provider_id.as_bytes()).to_vec()).fetch_optional(&mut *tx).await?;
                    if seen.is_none() {
                        self.remove(&mut tx, id, &provider_id, folder).await?;
                    }
                }
            }
            sqlx::query("UPDATE native_checkpoints SET sync_cursor=COALESCE(?,sync_cursor),next_page=?,scan_id=?,last_full_at=IF(?,UTC_TIMESTAMP(6),last_full_at),recovery=IF(? IS NULL,FALSE,recovery) WHERE account_id=? AND folder=?").bind(&page.cursor).bind(&page.next).bind(if page.next.is_some(){scan.as_ref()}else{None}).bind(page.next.is_none()&&full).bind(&page.next).bind(id).bind(folder).execute(&mut *tx).await?;
            if cursor.is_none() && page.next.is_none() {
                self.db
                    .emit(
                        &mut tx,
                        id,
                        "account.sync_completed",
                        json!({"folder":folder,"sync_from":account.config.sync_from}),
                    )
                    .await?;
            }
            tx.commit().await?;
            next = page.next;
            if let Some(c) = page.cursor {
                cursor = Some(c);
            }
            if next.is_none() {
                return Ok(());
            }
        }
        Err(Error::Unavailable(
            "sync pagination limit reached; continuation persisted".into(),
        ))
    }
    async fn persist(
        &self,
        tx: &mut Transaction<'_, MySql>,
        a: &Account,
        m: &ProviderMessage,
        reason: &str,
    ) -> Result<()> {
        let old=sqlx::query("SELECT id,flags,removed_at FROM mail_messages WHERE account_id=? AND provider_id=? FOR UPDATE").bind(&a.id).bind(&m.id).fetch_optional(&mut **tx).await?;
        let id = old
            .as_ref()
            .map(|r| r.try_get::<String, _>("id"))
            .transpose()?
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let previous_locations:Vec<String>=sqlx::query_scalar("SELECT CONVERT(folder USING utf8mb4) COLLATE utf8mb4_unicode_ci FROM message_locations WHERE message_id=? AND active=TRUE ORDER BY folder").bind(&id).fetch_all(&mut **tx).await?;
        let mut locations = m.locations.clone();
        locations.sort();
        let flags = json!(m.flags);
        let metadata = &m.metadata;
        sqlx::query("INSERT INTO mail_messages(id,account_id,backend,provider_id,metadata,flags,received_at,size,discovery_reason) VALUES(?,?,?,?,?,?,?,?,?) ON DUPLICATE KEY UPDATE metadata=JSON_MERGE_PATCH(metadata,VALUES(metadata)),flags=VALUES(flags),received_at=VALUES(received_at),size=VALUES(size),removed_at=NULL,updated_at=UTC_TIMESTAMP(6)").bind(&id).bind(&a.id).bind(a.config.backend.as_str()).bind(&m.id).bind(metadata).bind(&flags).bind(m.received_at.naive_utc()).bind(m.size).bind(reason).execute(&mut **tx).await?;
        sqlx::query("UPDATE message_locations SET active=FALSE WHERE message_id=?")
            .bind(&id)
            .execute(&mut **tx)
            .await?;
        for folder in &m.locations {
            sqlx::query("INSERT INTO message_locations(message_id,folder,provider_id) VALUES(?,?,?) ON DUPLICATE KEY UPDATE active=TRUE,provider_id=VALUES(provider_id)").bind(&id).bind(folder).bind(&m.id).execute(&mut **tx).await?;
        }
        if old.is_some() && previous_locations != locations {
            self.db.emit(tx,&a.id,"message.locations_changed",json!({"message_id":id,"locations":locations,"previous_locations":previous_locations})).await?;
        }
        if old.is_none() {
            self.db
                .emit(
                    tx,
                    &a.id,
                    "message.created",
                    json!({"message_id":id,"discovery_reason":reason,"content_state":"pending"}),
                )
                .await?;
        } else if old
            .as_ref()
            .and_then(|r| r.try_get::<Value, _>("flags").ok())
            .as_ref()
            != Some(&flags)
        {
            self.db
                .emit(
                    tx,
                    &a.id,
                    "message.flags_changed",
                    json!({"message_id":id,"flags":flags}),
                )
                .await?;
        }
        Ok(())
    }
    async fn remove(
        &self,
        tx: &mut Transaction<'_, MySql>,
        account: &str,
        provider: &str,
        folder: &str,
    ) -> Result<()> {
        let id: Option<String> =
            sqlx::query_scalar("SELECT id FROM mail_messages WHERE account_id=? AND provider_id=?")
                .bind(account)
                .bind(provider)
                .fetch_optional(&mut **tx)
                .await?;
        if let Some(id) = id {
            sqlx::query("UPDATE message_locations SET active=FALSE WHERE message_id=? AND (?='' OR folder=?)").bind(&id).bind(folder).bind(folder).execute(&mut **tx).await?;
            let remaining: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM message_locations WHERE message_id=? AND active=TRUE",
            )
            .bind(&id)
            .fetch_one(&mut **tx)
            .await?;
            if remaining == 0 {
                let done=sqlx::query("UPDATE mail_messages SET removed_at=UTC_TIMESTAMP(6) WHERE id=? AND removed_at IS NULL").bind(&id).execute(&mut **tx).await?;
                if done.rows_affected() > 0 {
                    self.db
                        .emit(tx, account, "message.removed", json!({"message_id":id}))
                        .await?;
                }
            }
        }
        Ok(())
    }
    async fn renew_subscription(&self, a: &Account) {
        if a.config.backend == Backend::Microsoft
            && a.config.shared
            && let Some(id) = &a.config.credential_id
            && let Ok(credential) = crate::oauth::store::get(&self.db, id).await
            && self
                .accounts
                .oauth
                .registration(&credential.client_id)
                .is_ok_and(|r| r.mode == crate::oauth::config::Mode::Delegated)
        {
            return;
        }
        let credential = match a.config.credential_id.as_deref() {
            Some(id) => crate::oauth::store::get(&self.db, id).await.ok(),
            None => None,
        };
        let gmail_topic = credential
            .as_ref()
            .and_then(|c| self.config.native.gmail_topics.get(&c.client_id))
            .or(self.config.native.gmail_topic.as_ref());
        if a.config.backend == Backend::Google
            && (gmail_topic.is_none()
                || self.config.native.pubsub_email.is_none()
                || self.config.native.pubsub_audience.is_none())
        {
            let _=sqlx::query("UPDATE native_state SET subscription_error='push_not_configured' WHERE account_id=?").bind(&a.id).execute(&self.db.pool).await;
            return;
        }
        let Some(base) = &self.config.native.public_url else {
            return;
        };
        let result:Result<()>=async {
   let row=sqlx::query("SELECT subscription_id,subscription_expires_at,subscription_secret FROM native_state WHERE account_id=?").bind(&a.id).fetch_one(&self.db.pool).await?;
   let expires:Option<NaiveDateTime>=row.try_get("subscription_expires_at")?;if expires.is_some_and(|e|e>Utc::now().naive_utc()+chrono::Duration::hours(12)){return Ok(());}
   let old:Option<String>=row.try_get("subscription_id")?;
   let encrypted:Option<Vec<u8>>=row.try_get("subscription_secret")?;
   let secret=match encrypted {Some(e)=>String::from_utf8(self.db.crypto.decrypt(&e)?).map_err(|_|Error::Internal("invalid subscription secret".into()))?,None=>Uuid::new_v4().to_string()};
   let subscription=self.client.subscription(a,base,old.as_deref(),&secret,self.config.native.gmail_topic.as_deref()).await?;
   sqlx::query("UPDATE native_state SET subscription_id=?,subscription_secret=?,subscription_expires_at=?,subscription_error=NULL WHERE account_id=?").bind(subscription.id).bind(self.db.crypto.encrypt(secret.as_bytes())?).bind(subscription.expires_at.naive_utc()).bind(&a.id).execute(&self.db.pool).await?;Ok(())
  }.await;
        if let Err(e) = result {
            let _ = sqlx::query("UPDATE native_state SET subscription_error=? WHERE account_id=?")
                .bind(sanitized(&e))
                .bind(&a.id)
                .execute(&self.db.pool)
                .await;
        }
    }
}
pub fn sanitized(error: &Error) -> &'static str {
    match error {
        Error::OAuth { code, .. } => code,
        Error::Throttled(_) => "throttled",
        Error::ProviderNotFound { .. } => "provider_resource_not_found",
        Error::NotFound => "not_found",
        _ => "provider_unavailable",
    }
}

#[cfg(test)]
#[path = "native_guard_tests.rs"]
mod guard_tests;
