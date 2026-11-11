use super::*;
use crate::crypto::Crypto;
use std::collections::HashMap;

#[tokio::test]
#[ignore = "requires a dedicated MySQL database in TEST_DATABASE_URL"]
async fn account_commit_guard_rejects_pause_configuration_change_and_deletion() -> anyhow::Result<()>
{
    let database_url = std::env::var("TEST_DATABASE_URL")?;
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(5)
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
    let runtime = Runtime::new(db.clone(), config, accounts);
    let credential = crate::oauth::store::create(&db, "guard-test", Some("refresh"), None).await?;
    let input = serde_json::from_value(
        json!({"name":"guard","backend":"microsoft","credential_id":credential.id,"target_mailbox":"target@example.com","shared":true}),
    )?;
    let account = db.create_account(input).await?;
    let run = runtime.schedule(&account.id).await?;
    sqlx::query("UPDATE sync_runs SET state='running' WHERE id=?")
        .bind(&run)
        .execute(&db.pool)
        .await?;
    runtime
        .failed(
            &account.id,
            Error::ProviderNotFound {
                code: "ErrorItemNotFound".into(),
                request_id: "test-request".into(),
            },
        )
        .await?;
    let failure: (String, bool) = sqlx::query_as(
        "SELECT last_error,next_sync_at>UTC_TIMESTAMP(6) FROM native_state WHERE account_id=?",
    )
    .bind(&account.id)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(failure, ("provider_resource_not_found".into(), true));
    let completed: (String, bool) =
        sqlx::query_as("SELECT state,finished_at IS NOT NULL FROM sync_runs WHERE id=?")
            .bind(&run)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(completed, ("failed".into(), true));
    // A missing resource must not disappear silently while the account still exists.
    runtime.failed(&account.id, Error::NotFound).await?;
    let failure: String =
        sqlx::query_scalar("SELECT last_error FROM native_state WHERE account_id=?")
            .bind(&account.id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(failure, "not_found");
    let mut tx = db.pool.begin().await?;
    runtime.guard(&mut tx, &account).await?;
    tx.rollback().await?;
    db.transition(&account.id, "paused", "account.paused", None, false, None)
        .await?;
    let mut tx = db.pool.begin().await?;
    assert!(matches!(
        runtime.guard(&mut tx, &account).await,
        Err(Error::Conflict(_))
    ));
    tx.rollback().await?;
    db.transition(
        &account.id,
        "disconnected",
        "account.resumed",
        None,
        false,
        None,
    )
    .await?;
    let mut updated = account.config.clone();
    updated.name = "changed while fetching".into();
    let updated = db.update_account(&account.id, updated).await?;
    let mut tx = db.pool.begin().await?;
    assert!(matches!(
        runtime.guard(&mut tx, &account).await,
        Err(Error::Conflict(_))
    ));
    tx.rollback().await?;
    let mut tx = db.pool.begin().await?;
    runtime.guard(&mut tx, &updated).await?;
    tx.rollback().await?;
    db.delete_account(&account.id).await?;
    let mut tx = db.pool.begin().await?;
    assert!(runtime.guard(&mut tx, &account).await.is_err());
    tx.rollback().await?;
    crate::oauth::store::delete(&db, &credential.id).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires a dedicated MySQL database in TEST_DATABASE_URL"]
async fn microsoft_sent_cutoff_survives_native_persistence_filter() -> anyhow::Result<()> {
    use axum::{Json, http::Uri};
    async fn peer(uri: Uri) -> Json<Value> {
        let parent = if uri.path().contains("/sentitems/") {
            "sent-provider-id"
        } else {
            "inbox-provider-id"
        };
        Json(json!({"value":[
            {"id":"received-old-sent-new","parentFolderId":parent,"receivedDateTime":"2026-01-10T00:00:00Z","sentDateTime":"2026-01-20T00:00:00Z"},
            {"id":"received-new-sent-old","parentFolderId":parent,"receivedDateTime":"2026-01-20T00:00:00Z","sentDateTime":"2026-01-10T00:00:00Z"}
        ],"@odata.deltaLink":"/mailFolders/inbox/messages/delta?cursor=done"}))
    }
    let database_url = std::env::var("TEST_DATABASE_URL")?;
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(5)
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
        native: Default::default(),
        cache: Default::default(),
        oauth: serde_json::from_value(
            json!({"providers":{"sent-clock":{"provider":"microsoft","mode":"application","tenant":"test-tenant","client_id":"test-client","client_secret":"test-secret"}}}),
        )?,
    });
    let accounts = Arc::new(Accounts::new(db.clone(), config.clone()));
    let credential = crate::oauth::store::create(&db, "sent-clock", None, None).await?;
    accounts
        .oauth
        .seed(
            &credential,
            &crate::oauth::TokenResponse {
                access_token: "test-token".into(),
                refresh_token: None,
                expires_in: 3600,
            },
        )
        .await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        axum::serve(listener, axum::Router::new().fallback(peer))
            .await
            .unwrap();
    });
    let mut runtime = Runtime::new(db.clone(), config.clone(), accounts.clone());
    runtime.client = NativeClient::with_test_endpoint(
        config,
        accounts.oauth.clone(),
        format!("http://{address}"),
    );
    let input = serde_json::from_value(
        json!({"name":"Sent cutoff persistence","backend":"microsoft","credential_id":credential.id,"target_mailbox":"mailbox@example.com","watched_folders":["sentitems","inbox"],"sync_from":"2026-01-15T00:00:00Z"}),
    )?;
    let account = db.create_account(input).await?;
    runtime.sync_folder(&account, "sentitems").await?;
    let row = sqlx::query(
        "SELECT id,provider_id,received_at,metadata FROM mail_messages WHERE account_id=?",
    )
    .bind(&account.id)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        row.try_get_unchecked::<String, _>("provider_id")?,
        "received-old-sent-new"
    );
    assert_eq!(
        row.try_get::<NaiveDateTime, _>("received_at")?
            .and_utc()
            .to_rfc3339(),
        "2026-01-10T00:00:00+00:00"
    );
    assert_eq!(
        row.try_get::<Value, _>("metadata")?["eligibility_time_basis"],
        "sent"
    );
    let details =
        crate::content::locations(&runtime, &account.id, &row.try_get::<String, _>("id")?).await?;
    assert_eq!(details["locations"][0]["folder"], "sentitems");
    assert_eq!(
        details["locations"][0]["provider_id"],
        "received-old-sent-new"
    );
    runtime.sync_folder(&account, "inbox").await?;
    let rows=sqlx::query("SELECT m.provider_id,l.folder FROM mail_messages m JOIN message_locations l ON l.message_id=m.id WHERE m.account_id=? AND l.active=TRUE ORDER BY l.folder").bind(&account.id).fetch_all(&db.pool).await?;
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].try_get_unchecked::<String, _>("provider_id")?,
        "received-new-sent-old"
    );
    assert_eq!(
        rows[1].try_get_unchecked::<String, _>("provider_id")?,
        "received-old-sent-new"
    );
    db.delete_account(&account.id).await?;
    crate::oauth::store::delete(&db, &credential.id).await?;
    server.abort();
    Ok(())
}
