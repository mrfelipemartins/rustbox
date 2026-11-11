use rustbox::{
    accounts::Accounts,
    config::Config,
    crypto::Crypto,
    db::{AccountInput, Db},
    error::Error,
    native::Runtime,
    oauth::store,
};
use serde_json::json;
use sqlx::mysql::MySqlPoolOptions;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

async fn runtime() -> anyhow::Result<Arc<Runtime>> {
    let database_url = std::env::var("TEST_DATABASE_URL")?;
    let pool = MySqlPoolOptions::new()
        .max_connections(20)
        .connect(&database_url)
        .await?;
    sqlx::migrate!().run(&pool).await?;
    let db = Arc::new(Db::new(pool, Crypto::new(&[42; 32])?));
    let config = Arc::new(Config {
        admin: Default::default(),
        database_url,
        listen: "127.0.0.1:0".parse()?,
        api_keys: HashMap::new(),
        encryption_key: vec![42; 32],
        allow_private_networks: false,
        tls_ca_file: None,
        log_filter: String::new(),
        max_message_bytes: 25 * 1024 * 1024,
        imap_concurrency: 4,
        smtp_concurrency: 4,
        webhook_concurrency: 4,
        command_timeout_secs: 10,
        webhooks: Default::default(),
        oauth: Default::default(),
        native: Default::default(),
        cache: Default::default(),
    });
    let accounts = Arc::new(Accounts::new(db.clone(), config.clone()));
    Ok(Arc::new(Runtime::new(db, config, accounts)))
}
fn imap_input() -> AccountInput {
    serde_json::from_value(json!({"name":"cache test","imap":{"host":"mail.example.com","port":993,"username":"mail@test","password":"secret"},"smtp":{"host":"mail.example.com","port":465,"username":"mail@test","password":"secret"}})).unwrap()
}
async fn message(runtime: &Runtime, account: &str, size: u64) -> anyhow::Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO mail_messages(id,account_id,backend,metadata,flags,size) VALUES(?,?,'imap',JSON_OBJECT(),JSON_ARRAY(),?)").bind(&id).bind(account).bind(size).execute(&runtime.db.pool).await?;
    Ok(id)
}

#[tokio::test]
#[ignore = "requires a dedicated MySQL database in TEST_DATABASE_URL"]
async fn content_promise_survives_provider_removal_and_unavailable_states_are_explicit()
-> anyhow::Result<()> {
    let rt = runtime().await?;
    let account = rt.db.create_account(imap_input()).await?;
    let cached = message(&rt, &account.id, 32).await?;
    let raw = b"Message-ID: <cached@test>\r\n\r\npromised body";
    let encrypted = rt.db.crypto.encrypt(raw)?;
    sqlx::query("INSERT INTO content_cache(message_id,payload,size,available_until) VALUES(?,?,?,UTC_TIMESTAMP(6)+INTERVAL 1 HOUR)").bind(&cached).bind(encrypted).bind(raw.len() as u64).execute(&rt.db.pool).await?;
    sqlx::query(
        "UPDATE mail_messages SET content_state='ready',removed_at=UTC_TIMESTAMP(6) WHERE id=?",
    )
    .bind(&cached)
    .execute(&rt.db.pool)
    .await?;
    assert_eq!(
        rustbox::content::fetch(&rt, &account.id, &cached).await?,
        raw
    );
    assert!(matches!(
        rustbox::content::fetch(&rt, "other-account", &cached).await,
        Err(Error::NotFound)
    ));
    let oversized = message(
        &rt,
        &account.id,
        rt.config.cache.max_message_bytes as u64 + 1,
    )
    .await?;
    let missing = message(&rt, &account.id, 12).await?;
    let stop = CancellationToken::new();
    let worker = tokio::spawn(rustbox::content::run(rt.clone(), stop.clone()));
    let mut finished = false;
    for _ in 0..80 {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM mail_messages WHERE id IN (?,?) AND content_state='unavailable'",
        )
        .bind(&oversized)
        .bind(&missing)
        .fetch_one(&rt.db.pool)
        .await?;
        if count == 2 {
            finished = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    stop.cancel();
    worker.await??;
    assert!(finished);
    let reason: String = sqlx::query_scalar("SELECT content_error FROM mail_messages WHERE id=?")
        .bind(&oversized)
        .fetch_one(&rt.db.pool)
        .await?;
    assert_eq!(reason, "too_large");
    assert_eq!(
        rustbox::content::fetch(&rt, &account.id, &cached).await?,
        raw
    );
    let unavailable:i64=sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE account_id=? AND event_type='message.content_unavailable'").bind(&account.id).fetch_one(&rt.db.pool).await?;
    assert_eq!(unavailable, 2);
    sqlx::query("UPDATE content_cache SET available_until=UTC_TIMESTAMP(6)-INTERVAL 1 SECOND WHERE message_id=?").bind(&cached).execute(&rt.db.pool).await?;
    assert!(matches!(
        rustbox::content::fetch(&rt, &account.id, &cached).await,
        Err(Error::ContentUnavailable(_))
    ));
    rt.db.delete_account(&account.id).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires a dedicated MySQL database in TEST_DATABASE_URL"]
async fn one_thousand_accounts_have_independent_durable_coalesced_schedule() -> anyhow::Result<()> {
    let rt = runtime().await?;
    let credential = store::create(&rt.db, "scheduler-test", Some("refresh"), None).await?;
    let mut ids = Vec::new();
    for index in 0..1000 {
        let input: AccountInput = serde_json::from_value(
            json!({"name":format!("scheduler {index}"),"backend":"microsoft","credential_id":credential.id,"target_mailbox":format!("mailbox{index}@example.com"),"shared":true}),
        )?;
        ids.push(rt.db.create_account(input).await?.id);
    }
    let scheduled = futures_util::future::join_all(ids.iter().map(|id| rt.schedule(id))).await;
    let runs = scheduled.into_iter().collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        runs.iter().collect::<std::collections::HashSet<_>>().len(),
        1000
    );
    let repeated = futures_util::future::join_all((0..100).map(|_| rt.schedule(&ids[0]))).await;
    assert!(
        repeated
            .into_iter()
            .all(|run| run.is_ok_and(|run| run == runs[0]))
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sync_runs WHERE account_id=?")
        .bind(&ids[0])
        .fetch_one(&rt.db.pool)
        .await?;
    assert_eq!(count, 1);
    rt.db
        .transition(
            &ids[0],
            "paused",
            "account.paused",
            Some("test"),
            false,
            None,
        )
        .await?;
    assert!(matches!(
        rt.schedule(&ids[0]).await,
        Err(Error::Conflict(_))
    ));
    for id in ids {
        rt.db.delete_account(&id).await?;
    }
    store::delete(&rt.db, &credential.id).await?;
    Ok(())
}
