use super::*;
use crate::crypto::Crypto;
use axum::{Form, Json, extract::State, http::StatusCode};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Default)]
struct Peer {
    requests: Arc<AtomicUsize>,
    mode: Arc<AtomicUsize>,
    started: Arc<tokio::sync::Notify>,
}

async fn token_peer(
    State(peer): State<Peer>,
    Form(form): Form<HashMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    peer.requests.fetch_add(1, Ordering::SeqCst);
    peer.started.notify_one();
    tokio::time::sleep(Duration::from_millis(100)).await;
    match peer.mode.load(Ordering::SeqCst) {
        1 => (
            StatusCode::BAD_REQUEST,
            Json(
                json!({"error":"invalid_grant","error_description":"secret must never reach callers"}),
            ),
        ),
        2 => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"temporarily_unavailable"})),
        ),
        _ => {
            let refresh = form
                .get("refresh_token")
                .map(String::as_str)
                .unwrap_or("application");
            (
                StatusCode::OK,
                Json(
                    json!({"access_token":format!("{refresh}-access"),"refresh_token":format!("{refresh}-next"),"expires_in":3600}),
                ),
            )
        }
    }
}

fn registration(mode: &str) -> ProviderRegistration {
    serde_json::from_value(json!({"provider":"microsoft","mode":mode,"tenant":"test-tenant","client_id":"test-client","client_secret":"secret"})).unwrap()
}

async fn manager(endpoint: String) -> anyhow::Result<OAuth> {
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(20)
        .connect(&std::env::var("TEST_DATABASE_URL")?)
        .await?;
    sqlx::migrate!().run(&pool).await?;
    let db = Arc::new(Db::new(pool, Crypto::new(&[42; 32])?));
    let config = Config {
        cache: Default::default(),
        native: Default::default(),
        database_url: String::new(),
        listen: "127.0.0.1:0".parse()?,
        api_keys: HashMap::new(),
        encryption_key: vec![42; 32],
        allow_private_networks: false,
        tls_ca_file: None,
        log_filter: String::new(),
        max_message_bytes: 1024,
        imap_concurrency: 8,
        smtp_concurrency: 8,
        webhook_concurrency: 8,
        command_timeout_secs: 60,
        webhooks: crate::config::WebhookConfig::default(),
        oauth: config::OAuthConfig {
            providers: [
                ("delegated".into(), registration("delegated")),
                ("application".into(), registration("application")),
            ]
            .into_iter()
            .collect(),
            concurrency: 4,
        },
    };
    let mut manager = OAuth::new(db, Arc::new(config));
    Arc::get_mut(&mut manager.inner).unwrap().test_endpoint = Some(endpoint);
    Ok(manager)
}

#[tokio::test]
#[ignore = "requires a dedicated MySQL database in TEST_DATABASE_URL"]
async fn renewal_coalesces_rotates_backs_off_and_rejects_stale_results() -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let peer = Peer::default();
    let app = axum::Router::new()
        .route("/token", axum::routing::post(token_peer))
        .with_state(peer.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let manager = manager(format!("http://{address}/token")).await?;
    let credential = store::create(&manager.inner.db, "delegated", Some("initial"), None).await?;
    let results =
        futures_util::future::join_all((0..100).map(|_| manager.token(&credential.id, false)))
            .await;
    assert!(
        results
            .into_iter()
            .all(|result| result.is_ok_and(|token| token == "initial-access"))
    );
    assert_eq!(peer.requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        store::get(&manager.inner.db, &credential.id)
            .await?
            .refresh_token
            .as_deref(),
        Some("initial-next")
    );

    peer.mode.store(2, Ordering::SeqCst);
    assert!(matches!(
        manager.token(&credential.id, true).await,
        Err(Error::OAuth {
            retryable: true,
            ..
        })
    ));
    let count = peer.requests.load(Ordering::SeqCst);
    assert!(manager.token(&credential.id, false).await.is_err());
    assert_eq!(peer.requests.load(Ordering::SeqCst), count);
    assert_eq!(
        store::get(&manager.inner.db, &credential.id).await?.health,
        "healthy"
    );

    peer.mode.store(1, Ordering::SeqCst);
    let error = manager.token(&credential.id, true).await.err().unwrap();
    assert!(!error.to_string().contains("secret"));
    assert_eq!(
        store::get(&manager.inner.db, &credential.id).await?.health,
        "reauthorization_required"
    );
    let count = peer.requests.load(Ordering::SeqCst);
    assert!(manager.token(&credential.id, false).await.is_err());
    assert_eq!(peer.requests.load(Ordering::SeqCst), count);

    peer.mode.store(0, Ordering::SeqCst);
    let current = store::get(&manager.inner.db, &credential.id).await?;
    store::replace(
        &manager.inner.db,
        &current.id,
        current.revision,
        Some("old"),
        None,
    )
    .await?;
    let _ = peer.started.notified().now_or_never();
    let cloned = manager.clone();
    let id = credential.id.clone();
    let refresh = tokio::spawn(async move { cloned.token(&id, true).await });
    peer.started.notified().await;
    let current = store::get(&manager.inner.db, &credential.id).await?;
    store::replace(
        &manager.inner.db,
        &current.id,
        current.revision,
        Some("replacement"),
        None,
    )
    .await?;
    assert_eq!(refresh.await??, "replacement-access");
    assert_eq!(
        store::get(&manager.inner.db, &credential.id)
            .await?
            .refresh_token
            .as_deref(),
        Some("replacement-next")
    );
    store::delete(&manager.inner.db, &credential.id).await?;
    server.abort();
    Ok(())
}

#[tokio::test]
#[ignore = "requires a dedicated MySQL database in TEST_DATABASE_URL"]
async fn application_tokens_are_shared_and_refresh_survives_request_cancellation()
-> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let peer = Peer::default();
    let app = axum::Router::new()
        .route("/token", axum::routing::post(token_peer))
        .with_state(peer.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let manager = manager(format!("http://{address}/token")).await?;
    let first = store::create(&manager.inner.db, "application", None, None).await?;
    let second = store::create(&manager.inner.db, "application", None, None).await?;
    let cloned = manager.clone();
    let id = first.id.clone();
    let waiter = tokio::spawn(async move { cloned.token(&id, false).await });
    peer.started.notified().await;
    waiter.abort();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        manager.token(&second.id, false).await?,
        "application-access"
    );
    assert_eq!(peer.requests.load(Ordering::SeqCst), 1);
    assert_eq!(manager.token(&first.id, false).await?, "application-access");
    assert_eq!(
        store::get(&manager.inner.db, &first.id).await?.mode()?,
        Mode::Application
    );
    store::delete(&manager.inner.db, &first.id).await?;
    store::delete(&manager.inner.db, &second.id).await?;
    server.abort();
    Ok(())
}

#[test]
fn token_cache_separates_delegation_subjects_and_credential_revisions() {
    let mut credential = store::Credential {
        id: "one".into(),
        client_id: "client".into(),
        refresh_token: None,
        subject: Some("a@example.com".into()),
        revision: 1,
        health: "healthy".into(),
        last_error: None,
    };
    let mut registration = registration("delegated");
    let old = cache_key(&credential, &registration);
    credential.revision = 2;
    assert_ne!(old, cache_key(&credential, &registration));
    registration.mode = Mode::DomainWideDelegation;
    let first = cache_key(&credential, &registration);
    credential.subject = Some("b@example.com".into());
    assert_ne!(first, cache_key(&credential, &registration));
    registration.mode = Mode::Application;
    let first = cache_key(&credential, &registration);
    credential.id = "two".into();
    credential.subject = None;
    assert_eq!(first, cache_key(&credential, &registration));
    credential.client_id = "other-tenant-registration".into();
    assert_ne!(first, cache_key(&credential, &registration));
}
