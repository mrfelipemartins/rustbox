use crate::{
    error::{Error, Result},
    imap::{Command, Reply},
    native::Runtime,
};
use chrono::{NaiveDateTime, Utc};
use serde_json::{Value, json};
use sqlx::Row;
use std::{collections::HashSet, sync::Arc, time::Duration};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

pub async fn fetch(runtime: &Runtime, account: &str, message: &str) -> Result<Vec<u8>> {
    let owned: Option<String> =
        sqlx::query_scalar("SELECT id FROM mail_messages WHERE id=? AND account_id=?")
            .bind(message)
            .bind(account)
            .fetch_optional(&runtime.db.pool)
            .await?;
    if owned.is_none() {
        return Err(Error::NotFound);
    }
    if let Some(bytes) = sqlx::query_scalar::<_, Vec<u8>>(
        "SELECT payload FROM content_cache WHERE message_id=? AND available_until>UTC_TIMESTAMP(6)",
    )
    .bind(message)
    .fetch_optional(&runtime.db.pool)
    .await?
    {
        let crypto = runtime.db.crypto.clone();
        return tokio::task::spawn_blocking(move || crypto.decrypt(&bytes))
            .await
            .map_err(|_| Error::Internal("content decryption task failed".into()))?;
    }
    provider_fetch(runtime, account, message)
        .await
        .map_err(|e| match e {
            Error::NotFound | Error::Invalid(_) => {
                Error::ContentUnavailable("provider_content_missing".into())
            }
            e => e,
        })
}
async fn provider_fetch(runtime: &Runtime, account: &str, message: &str) -> Result<Vec<u8>> {
    let a = runtime.db.get_account(account).await?;
    if a.config.backend.is_native() {
        let provider: String =
            sqlx::query_scalar("SELECT CONVERT(provider_id USING utf8mb4) COLLATE utf8mb4_unicode_ci FROM mail_messages WHERE id=? AND account_id=?")
                .bind(message)
                .bind(account)
                .fetch_one(&runtime.db.pool)
                .await?;
        return runtime.client.fetch(&a, &provider).await;
    }
    let rows = sqlx::query(
        "SELECT folder,uidvalidity,uid FROM message_locations WHERE message_id=? AND active=TRUE",
    )
    .bind(message)
    .fetch_all(&runtime.db.pool)
    .await?;
    for row in rows {
        let result = runtime
            .accounts
            .execute(
                account,
                Command::Content {
                    mailbox: row.try_get_unchecked("folder")?,
                    uidvalidity: row.try_get("uidvalidity")?,
                    uid: row.try_get("uid")?,
                },
            )
            .await;
        match result {
            Ok(Reply::Content(raw)) => return Ok(raw),
            Err(Error::NotFound | Error::Invalid(_)) => continue,
            Err(e) => return Err(e),
            _ => {}
        }
    }
    Err(Error::ContentUnavailable("no_current_location".into()))
}
pub async fn run(runtime: Arc<Runtime>, shutdown: CancellationToken) -> Result<()> {
    let mut jobs = JoinSet::new();
    let mut active = HashSet::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut cleanup = tokio::time::interval(Duration::from_secs(60));
    loop {
        tokio::select! {
         _=shutdown.cancelled()=>break,
         _=cleanup.tick()=>{let mut tx=runtime.db.pool.begin().await?;sqlx::query("SELECT used_bytes FROM cache_budget WHERE id=1 FOR UPDATE").fetch_one(&mut *tx).await?;sqlx::query("DELETE FROM content_cache WHERE available_until<=UTC_TIMESTAMP(6)").execute(&mut *tx).await?;sqlx::query("UPDATE cache_budget SET used_bytes=(SELECT CAST(COALESCE(SUM(size),0) AS UNSIGNED) FROM content_cache) WHERE id=1").execute(&mut *tx).await?;sqlx::query("UPDATE mail_messages SET content_state='expired' WHERE content_state='ready' AND id NOT IN (SELECT message_id FROM content_cache)").execute(&mut *tx).await?;sqlx::query("DELETE FROM account_provisioning WHERE deleted_at<UTC_TIMESTAMP(6)-INTERVAL 30 DAY").execute(&mut *tx).await?;tx.commit().await?;},
         _=tick.tick()=>{let rows=sqlx::query("SELECT id,account_id FROM mail_messages WHERE content_state='pending' AND content_next_attempt_at<=UTC_TIMESTAMP(6) ORDER BY created_at LIMIT 100").fetch_all(&runtime.db.pool).await?;for row in rows {if jobs.len()>=runtime.config.cache.concurrency{break;}let id:String=row.try_get("id")?;if active.insert(id.clone()){let account:String=row.try_get("account_id")?;let rt=runtime.clone();jobs.spawn(async move {let result=cache(&rt,&account,&id).await;(id,result)});}}},
         result=jobs.join_next(),if !jobs.is_empty()=>{match result {Some(Ok((id,result)))=>{active.remove(&id);if let Err(e)=result{tracing::warn!(message_id=%id,error=%e,"content cache processing failed");}},Some(Err(_))=>return Err(Error::Internal("content worker failed".into())),_=>{}}}
        }
    }
    while jobs.join_next().await.is_some() {}
    Ok(())
}
async fn cache(rt: &Runtime, account: &str, id: &str) -> Result<()> {
    let row = sqlx::query("SELECT size,created_at,content_attempts FROM mail_messages WHERE id=?")
        .bind(id)
        .fetch_optional(&rt.db.pool)
        .await?;
    let Some(row) = row else {
        return Ok(());
    };
    let size: u64 = row.try_get("size")?;
    if size > rt.config.cache.max_message_bytes as u64 {
        return unavailable(rt, account, id, "too_large").await;
    }
    let raw = match provider_fetch(rt, account, id).await {
        Ok(raw) => raw,
        Err(Error::Unsupported(_)) => return unavailable(rt, account, id, "too_large").await,
        Err(Error::NotFound | Error::ContentUnavailable(_) | Error::Invalid(_)) => {
            return unavailable(rt, account, id, "provider_content_missing").await;
        }
        Err(_) => {
            let created: NaiveDateTime = row.try_get("created_at")?;
            if created < Utc::now().naive_utc() - chrono::Duration::hours(24) {
                return unavailable(rt, account, id, "fetch_retry_expired").await;
            }
            let attempts: u32 = row.try_get("content_attempts")?;
            let delay = (5_u64 * 2_u64.pow(attempts.min(9))).min(3600);
            sqlx::query("UPDATE mail_messages SET content_attempts=content_attempts+1,content_error='fetch_failed',content_next_attempt_at=UTC_TIMESTAMP(6)+INTERVAL ? SECOND WHERE id=?").bind(delay).bind(id).execute(&rt.db.pool).await?;
            return Ok(());
        }
    };
    if raw.len() > rt.config.cache.max_message_bytes {
        return unavailable(rt, account, id, "too_large").await;
    }
    store(rt, account, id, raw).await
}

pub(crate) async fn store(rt: &Runtime, account: &str, id: &str, raw: Vec<u8>) -> Result<()> {
    let bytes = raw.len() as u64;
    let crypto = rt.db.crypto.clone();
    let parsed = tokio::task::spawn_blocking(move || -> Result<_> {
        let parsed = crate::mail::parse(&raw)?;
        Ok((crypto.encrypt(&raw)?, parsed))
    })
    .await
    .map_err(|_| Error::Internal("MIME cache task failed".into()))?;
    let (encrypted, metadata) = match parsed {
        Ok(value) => value,
        Err(_) => return unavailable(rt, account, id, "invalid_mime").await,
    };
    let mut tx = rt.db.pool.begin().await?;
    let used: u64 = sqlx::query_scalar("SELECT used_bytes FROM cache_budget WHERE id=1 FOR UPDATE")
        .fetch_one(&mut *tx)
        .await?;
    if used.saturating_add(bytes) > rt.config.cache.capacity_bytes {
        sqlx::query("UPDATE mail_messages SET content_error='cache_capacity',content_next_attempt_at=UTC_TIMESTAMP(6)+INTERVAL 30 SECOND WHERE id=? AND content_state='pending'").bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        return Ok(());
    }
    let exists: Option<String> = sqlx::query_scalar(
        "SELECT id FROM mail_messages WHERE id=? AND content_state='pending' FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    if exists.is_none() {
        return Ok(());
    }
    let until = Utc::now() + chrono::Duration::days(i64::from(rt.config.cache.retention_days));
    sqlx::query(
        "INSERT INTO content_cache(message_id,payload,size,available_until) VALUES(?,?,?,?)",
    )
    .bind(id)
    .bind(encrypted)
    .bind(bytes)
    .bind(until.naive_utc())
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE cache_budget SET used_bytes=used_bytes+? WHERE id=1")
        .bind(bytes)
        .execute(&mut *tx)
        .await?;
    let mut metadata = metadata;
    if let Some(object) = metadata.as_object_mut() {
        object.remove("text");
        object.remove("html");
    }
    sqlx::query("UPDATE mail_messages SET content_state='ready',content_error=NULL,metadata=JSON_MERGE_PATCH(metadata,?) WHERE id=?").bind(&metadata).bind(id).execute(&mut *tx).await?;
    rt.db
        .emit(
            &mut tx,
            account,
            "message.content_ready",
            json!({"message_id":id,"available_until":until}),
        )
        .await?;
    tx.commit().await?;
    Ok(())
}
async fn unavailable(rt: &Runtime, account: &str, id: &str, reason: &str) -> Result<()> {
    let mut tx = rt.db.pool.begin().await?;
    let changed=sqlx::query("UPDATE mail_messages SET content_state='unavailable',content_error=? WHERE id=? AND content_state='pending'").bind(reason).bind(id).execute(&mut *tx).await?;
    if changed.rows_affected() > 0 {
        rt.db
            .emit(
                &mut tx,
                account,
                "message.content_unavailable",
                json!({"message_id":id,"reason":reason}),
            )
            .await?;
    }
    tx.commit().await?;
    Ok(())
}
pub async fn locations(rt: &Runtime, account: &str, message: &str) -> Result<Value> {
    let row = sqlx::query("SELECT * FROM mail_messages WHERE id=? AND account_id=?")
        .bind(message)
        .bind(account)
        .fetch_optional(&rt.db.pool)
        .await?
        .ok_or(Error::NotFound)?;
    let rows = sqlx::query("SELECT * FROM message_locations WHERE message_id=?")
        .bind(message)
        .fetch_all(&rt.db.pool)
        .await?;
    let locations=rows.iter().map(|r|->Result<Value>{Ok(json!({"folder":r.try_get_unchecked::<String,_>("folder")?,"provider_id":r.try_get_unchecked::<Option<String>,_>("provider_id")?,"uidvalidity":r.try_get::<Option<u32>,_>("uidvalidity")?,"uid":r.try_get::<Option<u32>,_>("uid")?,"active":r.try_get::<bool,_>("active")?}))}).collect::<Result<Vec<_>>>()?;
    let until: Option<NaiveDateTime> =
        sqlx::query_scalar("SELECT available_until FROM content_cache WHERE message_id=?")
            .bind(message)
            .fetch_optional(&rt.db.pool)
            .await?;
    Ok(
        json!({"id":message,"metadata":row.try_get::<Value,_>("metadata")?,"flags":row.try_get::<Value,_>("flags")?,"content_state":row.try_get::<String,_>("content_state")?,"content_error":row.try_get::<Option<String>,_>("content_error")?,"available_until":until.map(|d|d.and_utc()),"locations":locations,"identity_confidence":row.try_get::<String,_>("identity_confidence")?,"discovery_reason":row.try_get::<String,_>("discovery_reason")?,"received_at":row.try_get::<Option<NaiveDateTime>,_>("received_at")?.map(|d|d.and_utc()),"observed_at":row.try_get::<NaiveDateTime,_>("created_at")?.and_utc()}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{accounts::Accounts, config::Config, crypto::Crypto, db::Db};

    #[tokio::test]
    #[ignore = "requires a dedicated MySQL database in TEST_DATABASE_URL"]
    async fn readiness_is_atomic_and_capacity_does_not_evict_promised_content() -> anyhow::Result<()>
    {
        let url = std::env::var("TEST_DATABASE_URL")?;
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await?;
        sqlx::migrate!().run(&pool).await?;
        let db = Arc::new(Db::new(pool, Crypto::new(&[42; 32])?));
        let raw=b"Message-ID: <cache@test>\r\nFrom: person@test\r\nTo: other@test\r\nSubject: Cache\r\n\r\ncontent".to_vec();
        let mut config = Config {
            database_url: url,
            listen: "127.0.0.1:0".parse()?,
            api_keys: Default::default(),
            encryption_key: vec![42; 32],
            allow_private_networks: false,
            tls_ca_file: None,
            log_filter: "info".into(),
            max_message_bytes: 1024 * 1024,
            imap_concurrency: 1,
            smtp_concurrency: 1,
            webhook_concurrency: 1,
            command_timeout_secs: 1,
            webhooks: Default::default(),
            oauth: Default::default(),
            native: Default::default(),
            cache: Default::default(),
        };
        let existing: u64 =
            sqlx::query_scalar("SELECT CAST(COALESCE(SUM(size),0) AS UNSIGNED) FROM content_cache")
                .fetch_one(&db.pool)
                .await?;
        sqlx::query("UPDATE cache_budget SET used_bytes=? WHERE id=1")
            .bind(existing)
            .execute(&db.pool)
            .await?;
        config.cache.capacity_bytes = existing + raw.len() as u64;
        let config = Arc::new(config);
        let accounts = Arc::new(Accounts::new(db.clone(), config.clone()));
        let rt = Runtime::new(db.clone(), config, accounts);
        let input = serde_json::from_value(
            json!({"name":"cache-test","imap":{"host":"imap.example.com","port":993,"username":"person@test","password":"secret"},"smtp":{"host":"smtp.example.com","port":465,"username":"person@test","password":"secret"}}),
        )?;
        let account = db.create_account(input).await?;
        let first = uuid::Uuid::new_v4().to_string();
        let second = uuid::Uuid::new_v4().to_string();
        for id in [&first, &second] {
            sqlx::query("INSERT INTO mail_messages(id,account_id,backend,metadata,flags,size,content_state,discovery_reason,identity_confidence) VALUES(?,?,'imap',JSON_OBJECT(),JSON_ARRAY(),?,'pending','live','location')")
                .bind(id).bind(&account.id).bind(raw.len() as u64).execute(&db.pool).await?;
        }
        store(&rt, &account.id, &first, raw.clone()).await?;
        store(&rt, &account.id, &first, raw.clone()).await?;
        let ready: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM events WHERE account_id=? AND event_type='message.content_ready'",
        )
        .bind(&account.id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(ready, 1);
        let encrypted: Vec<u8> =
            sqlx::query_scalar("SELECT payload FROM content_cache WHERE message_id=?")
                .bind(&first)
                .fetch_one(&db.pool)
                .await?;
        assert!(!String::from_utf8_lossy(&encrypted).contains("Subject: Cache"));
        let until: NaiveDateTime =
            sqlx::query_scalar("SELECT available_until FROM content_cache WHERE message_id=?")
                .bind(&first)
                .fetch_one(&db.pool)
                .await?;
        store(&rt, &account.id, &second, raw.clone()).await?;
        let state: String =
            sqlx::query_scalar("SELECT content_state FROM mail_messages WHERE id=?")
                .bind(&second)
                .fetch_one(&db.pool)
                .await?;
        assert_eq!(state, "pending");
        let preserved: NaiveDateTime =
            sqlx::query_scalar("SELECT available_until FROM content_cache WHERE message_id=?")
                .bind(&first)
                .fetch_one(&db.pool)
                .await?;
        assert_eq!(until, preserved);
        sqlx::query("UPDATE mail_messages SET removed_at=UTC_TIMESTAMP(6) WHERE id=?")
            .bind(&first)
            .execute(&db.pool)
            .await?;
        assert_eq!(fetch(&rt, &account.id, &first).await?, raw);
        sqlx::query("UPDATE content_cache SET available_until=UTC_TIMESTAMP(6)-INTERVAL 1 SECOND WHERE message_id=?").bind(&first).execute(&db.pool).await?;
        assert!(matches!(
            fetch(&rt, &account.id, &first).await,
            Err(Error::ContentUnavailable(_))
        ));
        db.delete_account(&account.id).await?;
        sqlx::query("UPDATE cache_budget SET used_bytes=(SELECT CAST(COALESCE(SUM(size),0) AS UNSIGNED) FROM content_cache) WHERE id=1").execute(&db.pool).await?;
        Ok(())
    }
}
