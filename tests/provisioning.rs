use rustbox::{
    crypto::Crypto,
    db::{AccountInput, Db},
    error::Error,
};
use serde_json::json;
use sqlx::mysql::MySqlPoolOptions;

fn input() -> AccountInput {
    serde_json::from_value(json!({"name":"idempotent account",
        "imap":{"host":"mail.example.com","port":993,"username":"mail@test","password":"secret"},
        "smtp":{"host":"mail.example.com","port":465,"username":"mail@test","password":"secret"}}))
    .unwrap()
}

#[tokio::test]
#[ignore = "requires a dedicated MySQL database in TEST_DATABASE_URL"]
async fn provisioning_concurrent_retries_cutoff_and_deletion_tombstone() -> anyhow::Result<()> {
    let pool = MySqlPoolOptions::new()
        .max_connections(8)
        .connect(&std::env::var("TEST_DATABASE_URL")?)
        .await?;
    sqlx::migrate!().run(&pool).await?;
    let db = Db::new(pool, Crypto::new(&[42; 32])?);
    let key = uuid::Uuid::new_v4().to_string();
    let (first, second) = tokio::join!(
        db.create_account_idempotent("app", &key, input()),
        db.create_account_idempotent("app", &key, input())
    );
    let first = first?;
    let second = second?;
    assert_eq!(first.id, second.id);
    assert!(first.config.sync_from.is_some());
    assert_eq!(first.config.sync_from, second.config.sync_from);
    let events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM events WHERE account_id=? AND event_type='account.created'",
    )
    .bind(&first.id)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(events, 1);
    let mut conflict = input();
    conflict.name = "different".into();
    assert!(matches!(
        db.create_account_idempotent("app", &key, conflict).await,
        Err(Error::Conflict(_))
    ));
    let updated = db.update_account(&first.id, input()).await?;
    assert_eq!(updated.config.sync_from, first.config.sync_from);
    let mut changed = input();
    changed.sync_from = Some(chrono::Utc::now());
    assert!(matches!(
        db.update_account(&first.id, changed).await,
        Err(Error::Conflict(_))
    ));
    db.delete_account(&first.id).await?;
    assert!(matches!(
        db.create_account_idempotent("app", &key, input()).await,
        Err(Error::Conflict(_))
    ));
    let other = db
        .create_account_idempotent("other-app", &key, input())
        .await?;
    assert_ne!(first.id, other.id);
    db.delete_account(&other.id).await?;
    Ok(())
}
