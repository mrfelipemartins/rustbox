use rustbox::{
    crypto::Crypto,
    db::Db,
    oauth::store::{self, AuthorizationSecret},
};
use sqlx::{Row, mysql::MySqlPoolOptions};

async fn database() -> anyhow::Result<Db> {
    let pool = MySqlPoolOptions::new()
        .max_connections(5)
        .connect(&std::env::var("TEST_DATABASE_URL")?)
        .await?;
    sqlx::migrate!().run(&pool).await?;
    Ok(Db::new(pool, Crypto::new(&[42; 32])?))
}

#[tokio::test]
#[ignore = "requires a dedicated MySQL database in TEST_DATABASE_URL"]
async fn credential_encryption_rotation_and_revision_guard() -> anyhow::Result<()> {
    let db = database().await?;
    let c = store::create(&db, "google-test", Some("sensitive-refresh-token"), None).await?;
    let row = sqlx::query("SELECT secret FROM oauth_credentials WHERE id=?")
        .bind(&c.id)
        .fetch_one(&db.pool)
        .await?;
    let encrypted: Vec<u8> = row.try_get("secret")?;
    assert!(!String::from_utf8_lossy(&encrypted).contains("sensitive-refresh-token"));
    assert!(!c.public().to_string().contains("sensitive-refresh-token"));
    assert!(store::save_refresh(&db, &c.id, c.revision, Some("rotated")).await?);
    assert!(store::save_refresh(&db, &c.id, c.revision, None).await?);
    assert_eq!(
        store::get(&db, &c.id).await?.refresh_token.as_deref(),
        Some("rotated")
    );
    let next = store::replace(&db, &c.id, c.revision, Some("reauthorized"), None).await?;
    assert_eq!(next.revision, c.revision + 1);
    assert!(!store::save_refresh(&db, &c.id, c.revision, Some("stale")).await?);
    store::set_health(&db, &c.id, c.revision, "failed", "stale_error").await?;
    assert_eq!(store::get(&db, &c.id).await?.health, "healthy");
    store::delete(&db, &c.id).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires a dedicated MySQL database in TEST_DATABASE_URL"]
async fn authorization_owner_expiry_claim_and_restart_recovery() -> anyhow::Result<()> {
    let db = database().await?;
    let secret = AuthorizationSecret {
        client_id: "google-test".into(),
        redirect_uri: "https://app.example/callback".into(),
        state: "random-state".into(),
        verifier: "secret-verifier".into(),
        target_id: None,
        target_revision: None,
    };
    let a = store::authorization_create(&db, "owner-app", &secret).await?;
    assert!(
        store::authorization_get(&db, &a.id, "another-app")
            .await
            .is_err()
    );
    let result = tokio::join!(
        store::authorization_claim(&db, &a.id, "owner-app"),
        store::authorization_claim(&db, &a.id, "owner-app")
    );
    assert_ne!(result.0.is_ok(), result.1.is_ok());
    store::recover(&db).await?;
    let recovered = store::authorization_get(&db, &a.id, "owner-app").await?;
    assert_eq!(recovered.state, "uncertain");
    assert!(recovered.secret.is_none());
    let expired = store::authorization_create(&db, "owner-app", &secret).await?;
    sqlx::query(
        "UPDATE oauth_authorizations SET expires_at=UTC_TIMESTAMP(6)-INTERVAL 1 SECOND WHERE id=?",
    )
    .bind(&expired.id)
    .execute(&db.pool)
    .await?;
    assert!(
        store::authorization_claim(&db, &expired.id, "owner-app")
            .await
            .is_err()
    );
    assert_eq!(
        store::authorization_get(&db, &expired.id, "owner-app")
            .await?
            .public()["state"],
        "expired"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires a dedicated MySQL database in TEST_DATABASE_URL"]
async fn bindings_guard_deletion_and_health_events_are_atomic_and_deduplicated()
-> anyhow::Result<()> {
    let db = database().await?;
    let c = store::create(&db, "google-test", Some("refresh"), None).await?;
    let input = serde_json::from_value(serde_json::json!({
        "name":"OAuth integration test",
        "imap":{"host":"imap.gmail.com","port":993,"username":"mail@example.com","auth":{"type":"oauth2","credential_id":c.id}},
        "smtp":{"host":"smtp.gmail.com","port":465,"username":"mail@example.com","auth":{"type":"oauth2","credential_id":c.id}}
    }))?;
    let account = db.create_account(input).await?;
    assert!(store::delete(&db, &c.id).await.is_err());
    store::set_health(
        &db,
        &c.id,
        c.revision,
        "reauthorization_required",
        "invalid_grant",
    )
    .await?;
    store::set_health(
        &db,
        &c.id,
        c.revision,
        "reauthorization_required",
        "invalid_grant",
    )
    .await?;
    let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE account_id=? AND event_type='account.reauthorization_required'").bind(&account.id).fetch_one(&db.pool).await?;
    assert_eq!(events, 1);
    assert_eq!(
        store::get(&db, &c.id).await?.health,
        "reauthorization_required"
    );
    db.transition(
        &account.id,
        "auth_failed",
        "account.auth_failed",
        Some("credentials_rejected"),
        false,
        None,
    )
    .await?;
    store::replace(&db, &c.id, c.revision, Some("new-refresh"), None).await?;
    assert_eq!(
        db.get_account(&account.id).await?.state.as_str(),
        "disconnected"
    );
    db.smtp_validation_revision(
        &account.id,
        &account.config.smtp,
        Some(c.revision),
        true,
        None,
    )
    .await?;
    assert_eq!(db.get_account(&account.id).await?.smtp_state, "unvalidated");
    db.smtp_validation_revision(
        &account.id,
        &account.config.smtp,
        Some(c.revision + 1),
        true,
        None,
    )
    .await?;
    assert_eq!(db.get_account(&account.id).await?.smtp_state, "validated");
    let restored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE account_id=? AND event_type='account.authorization_restored'").bind(&account.id).fetch_one(&db.pool).await?;
    assert_eq!(restored, 1);
    db.delete_account(&account.id).await?;
    store::delete(&db, &c.id).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires a dedicated MySQL database in TEST_DATABASE_URL"]
async fn completion_is_atomic_and_replay_keeps_the_same_credential() -> anyhow::Result<()> {
    let db = database().await?;
    let secret = AuthorizationSecret {
        client_id: "google-test".into(),
        redirect_uri: "https://app.example/callback".into(),
        state: "random-state".into(),
        verifier: "secret-verifier".into(),
        target_id: None,
        target_revision: None,
    };
    let session = store::authorization_create(&db, "owner-app", &secret).await?;
    store::authorization_claim(&db, &session.id, "owner-app").await?;
    let c = store::authorization_commit(&db, &session.id, &secret, "refresh").await?;
    let completed = store::authorization_get(&db, &session.id, "owner-app").await?;
    assert_eq!(completed.state, "completed");
    assert_eq!(completed.credential_id.as_deref(), Some(c.id.as_str()));
    assert!(completed.secret.is_none());
    assert!(
        store::authorization_commit(&db, &session.id, &secret, "duplicate")
            .await
            .is_err()
    );
    let reauthorization = AuthorizationSecret {
        target_id: Some(c.id.clone()),
        target_revision: Some(c.revision),
        ..secret
    };
    let session = store::authorization_create(&db, "owner-app", &reauthorization).await?;
    store::authorization_claim(&db, &session.id, "owner-app").await?;
    store::replace(&db, &c.id, c.revision, Some("newer-credentials"), None).await?;
    assert!(
        store::authorization_commit(&db, &session.id, &reauthorization, "stale")
            .await
            .is_err()
    );
    assert_eq!(
        store::get(&db, &c.id).await?.refresh_token.as_deref(),
        Some("newer-credentials")
    );
    assert_eq!(
        store::authorization_get(&db, &session.id, "owner-app")
            .await?
            .state,
        "exchanging"
    );
    store::delete(&db, &c.id).await?;
    Ok(())
}
