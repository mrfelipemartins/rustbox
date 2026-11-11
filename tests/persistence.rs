use chrono::Utc;
use rustbox::{
    crypto::Crypto,
    db::{AccountInput, Db, MailServer},
    events,
};
use serde_json::json;
use sqlx::{Row, mysql::MySqlPoolOptions};
use std::time::Duration;

fn input() -> AccountInput {
    let server = MailServer {
        auth: None,
        host: "mail.example.com".into(),
        port: 993,
        username: "test@example.com".into(),
        password: "test-password".into(),
        tls_mode: "implicit".into(),
    };
    AccountInput {
        backend: Default::default(),
        credential_id: None,
        target_mailbox: None,
        authorizing_identity: None,
        shared: false,
        sync_from: None,
        name: "database integration test".into(),
        imap: server.clone(),
        smtp: MailServer {
            port: 465,
            ..server
        },
        watched_folders: vec![],
        backfill_days: 30,
        trash_folder: Some("Trash".into()),
        sent_folder: None,
        webhook_url: Some("https://hooks.example.com/events".into()),
        webhook_secret: Some("01234567890123456789012345678901".into()),
    }
}

#[tokio::test]
#[ignore = "requires a dedicated MySQL database in TEST_DATABASE_URL"]
async fn transactional_lifecycle_events_and_cursor() -> anyhow::Result<()> {
    let url = std::env::var("TEST_DATABASE_URL")?;
    let pool = MySqlPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await?;
    sqlx::migrate!().run(&pool).await?;
    let db = Db::new(pool, Crypto::new(&[42; 32])?);
    let account = db.create_account(input()).await?;
    assert!(!account.public().to_string().contains("test-password"));
    db.smtp_validation_for(&account.id, &account.config.smtp, true, None)
        .await?;
    let mut changed = account.config.clone();
    changed.smtp.password = "new-test-password".into();
    db.update_account(&account.id, changed.clone()).await?;
    db.smtp_validation_for(
        &account.id,
        &account.config.smtp,
        false,
        Some("old_credentials"),
    )
    .await?;
    assert_eq!(db.get_account(&account.id).await?.smtp_state, "unvalidated");
    db.smtp_validation_for(&account.id, &changed.smtp, true, None)
        .await?;
    assert_eq!(db.get_account(&account.id).await?.smtp_state, "validated");
    db.transition(
        &account.id,
        "connecting",
        "account.connecting",
        None,
        true,
        None,
    )
    .await?;
    db.transition(
        &account.id,
        "disconnected",
        "account.connection_failed",
        Some("timeout"),
        true,
        Some(Utc::now()),
    )
    .await?;
    db.transition(
        &account.id,
        "disconnected",
        "account.connection_failed",
        Some("timeout"),
        true,
        Some(Utc::now()),
    )
    .await?;
    let failures: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM events WHERE account_id=? AND event_type='account.connection_failed'",
    )
    .bind(&account.id)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(failures, 1);
    assert_eq!(db.get_account(&account.id).await?.retry_count, 2);
    db.transition(
        &account.id,
        "connected",
        "account.connected",
        None,
        false,
        None,
    )
    .await?;
    db.recover_accounts().await?;
    let recovered = db.get_account(&account.id).await?;
    assert_eq!(recovered.state.as_str(), "disconnected");
    assert_eq!(recovered.last_error.as_deref(), Some("restart_recovery"));
    let recovery: sqlx::types::Json<serde_json::Value> = sqlx::query_scalar("SELECT payload FROM events WHERE account_id=? AND event_type='account.disconnected' ORDER BY id DESC LIMIT 1").bind(&account.id).fetch_one(&db.pool).await?;
    assert_eq!(recovery.0["reason"], "restart_recovery");
    let before: u64 = sqlx::query_scalar("SELECT last_id FROM event_sequence WHERE id=1")
        .fetch_one(&db.pool)
        .await?;
    let mut tx = db.pool.begin().await?;
    sqlx::query("INSERT INTO mailbox_checkpoints(account_id,mailbox,uidvalidity,last_uid) VALUES (?,'INBOX',1,42)").bind(&account.id).execute(&mut *tx).await?;
    db.emit(&mut tx, &account.id, "message.created", json!({"uid":42}))
        .await?;
    assert!(events::list(&db, before, 100).await?.events.is_empty());
    tx.rollback().await?;
    let checkpoint: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM mailbox_checkpoints WHERE account_id=?")
            .bind(&account.id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(checkpoint, 0);
    assert!(events::list(&db, before, 100).await?.events.is_empty());
    for mailbox in ["Folder", "Folder "] {
        sqlx::query("INSERT INTO mailbox_checkpoints(account_id,mailbox,uidvalidity,last_uid) VALUES (?,?,1,0)").bind(&account.id).bind(mailbox).execute(&db.pool).await?;
    }
    let folders: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM mailbox_checkpoints WHERE account_id=?")
            .bind(&account.id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(folders, 2);

    for uid in [1_u32, u32::MAX] {
        sqlx::query("INSERT INTO messages(account_id,mailbox,uidvalidity,uid,flags,metadata,internal_date,size) VALUES (?,'Folder',?,?,JSON_ARRAY('\\\\Seen'),JSON_OBJECT('subject','metadata test'),UTC_TIMESTAMP(6),?)").bind(&account.id).bind(u32::MAX).bind(uid).bind(u32::MAX).execute(&db.pool).await?;
    }
    let messages = db.list_messages(&account.id, "Folder", 0, 1).await?;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["uid"], 1);
    assert_eq!(messages[0]["uidvalidity"], u32::MAX);
    assert_eq!(messages[0]["size"], u32::MAX);
    assert_eq!(messages[0]["flags"], json!(["\\Seen"]));
    assert_eq!(messages[0]["metadata"]["subject"], "metadata test");
    assert!(messages[0]["internal_date"].as_str().is_some());
    let next = db.list_messages(&account.id, "Folder", 1, 100).await?;
    assert_eq!(next.len(), 1);
    assert_eq!(next[0]["uid"], u32::MAX);
    assert!(
        db.list_messages(&account.id, "Folder ", 0, 100)
            .await?
            .is_empty()
    );
    let mut tx1 = db.pool.begin().await?;
    let first = db
        .emit(&mut tx1, &account.id, "test.first", json!({}))
        .await?;
    let second_db = db.clone();
    let second_account = account.id.clone();
    let task = tokio::spawn(async move {
        let mut tx2 = second_db.pool.begin().await?;
        let second = second_db
            .emit(&mut tx2, &second_account, "test.second", json!({}))
            .await?;
        tx2.commit().await?;
        Ok::<u64, rustbox::error::Error>(second)
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!task.is_finished());
    tx1.commit().await?;
    let second = task.await??;
    assert!(second > first);
    let page = events::list(&db, before, 100).await?;
    assert!(page.events.iter().any(|event| event.id == first));
    assert!(page.events.iter().any(|event| event.id == second));
    db.delete_account(&account.id).await?;
    let row = sqlx::query("SELECT webhook_url,webhook_secret FROM events WHERE account_id=? AND event_type='account.deleted'").bind(&account.id).fetch_one(&db.pool).await?;
    assert_eq!(
        row.get::<String, _>("webhook_url"),
        "https://hooks.example.com/events"
    );
    assert_eq!(
        db.crypto
            .decrypt(&row.get::<Vec<u8>, _>("webhook_secret"))?,
        b"01234567890123456789012345678901"
    );
    Ok(())
}
