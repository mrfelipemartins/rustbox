use super::*;
use axum::{Json, extract::State, http::Uri};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
struct Peer {
    requests: Arc<AtomicUsize>,
    active: Arc<AtomicUsize>,
    maximum: Arc<AtomicUsize>,
}

async fn graph(State(peer): State<Peer>, uri: Uri) -> Json<Value> {
    peer.requests.fetch_add(1, Ordering::SeqCst);
    let active = peer.active.fetch_add(1, Ordering::SeqCst) + 1;
    peer.maximum.fetch_max(active, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(2)).await;
    peer.active.fetch_sub(1, Ordering::SeqCst);
    if uri.path().ends_with("/delta") {
        Json(
            json!({"value":[],"@odata.deltaLink":"/mailFolders/inbox/messages/delta?cursor=final"}),
        )
    } else {
        Json(json!({"id":"inbox","displayName":"Inbox"}))
    }
}

#[tokio::test]
#[ignore = "requires a dedicated MySQL database in TEST_DATABASE_URL"]
async fn one_thousand_native_mailboxes_complete_synchronization_with_bounded_requests()
-> anyhow::Result<()> {
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(32)
        .connect(&std::env::var("TEST_DATABASE_URL")?)
        .await?;
    sqlx::migrate!().run(&pool).await?;
    let db = Arc::new(crate::db::Db::new(
        pool,
        crate::crypto::Crypto::new(&[42; 32])?,
    ));
    let mut config = super::tests::config();
    config.native.concurrency = 16;
    config.native.poll_secs = 300;
    config
        .oauth
        .providers
        .get_mut("test")
        .ok_or_else(|| anyhow::anyhow!("test registration missing"))?
        .mode = Mode::Application;
    let config = Arc::new(config);
    let accounts = Arc::new(crate::accounts::Accounts::new(db.clone(), config.clone()));
    let credential = store::create(&db, "test", None, None).await?;
    accounts
        .oauth
        .seed(
            &credential,
            &crate::oauth::TokenResponse {
                access_token: "application-token".into(),
                refresh_token: None,
                expires_in: 3600,
            },
        )
        .await?;
    let peer = Peer::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let app = axum::Router::new().fallback(graph).with_state(peer.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let mut runtime = crate::native::Runtime::new(db.clone(), config.clone(), accounts.clone());
    runtime.client = NativeClient::with_test_endpoint(
        config,
        accounts.oauth.clone(),
        format!("http://{address}"),
    );
    let runtime = Arc::new(runtime);
    let mut ids = Vec::with_capacity(1000);
    for index in 0..1000 {
        let input = serde_json::from_value(
            json!({"name":format!("native-load-{index}"),"backend":"microsoft","credential_id":credential.id,"target_mailbox":format!("load-{index}@example.com"),"sync_from":"2026-01-01T00:00:00Z"}),
        )?;
        let account = db.create_account(input).await?;
        runtime.schedule(&account.id).await?;
        ids.push(account.id);
    }
    let shutdown = CancellationToken::new();
    let worker = tokio::spawn(runtime.clone().run(shutdown.clone()));
    let result=tokio::time::timeout(Duration::from_secs(90),async {
        loop {
            let completed:i64=sqlx::query_scalar("SELECT COUNT(*) FROM native_state n JOIN oauth_account_credentials b ON b.account_id=n.account_id WHERE b.credential_id=? AND n.last_sync_at IS NOT NULL").bind(&credential.id).fetch_one(&db.pool).await?;
            if completed>=1000{return Ok::<(),Error>(());}
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }).await;
    shutdown.cancel();
    worker.await??;
    server.abort();
    for id in &ids {
        db.delete_account(id).await?;
    }
    store::delete(&db, &credential.id).await?;
    result??;
    assert!(peer.maximum.load(Ordering::SeqCst) <= 16);
    assert!(peer.requests.load(Ordering::SeqCst) >= 2000);
    println!(
        "1000 simulated Graph mailboxes synchronized; requests={}, peak concurrent={}",
        peer.requests.load(Ordering::SeqCst),
        peer.maximum.load(Ordering::SeqCst)
    );
    Ok(())
}
