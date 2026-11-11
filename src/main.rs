use rustbox::{
    accounts::Accounts,
    api::{self, AppState},
    config::Config,
    crypto::Crypto,
    db::Db,
    events, smtp,
};
use sqlx::mysql::MySqlPoolOptions;
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Arc::new(Config::load()?);
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(tracing_subscriber::EnvFilter::try_new(&config.log_filter)?)
        .init();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    let pool = MySqlPoolOptions::new()
        .max_connections(32)
        .after_connect(|connection, _| {
            Box::pin(async move {
                sqlx::query("SET time_zone = '+00:00'")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect(&config.database_url)
        .await?;
    sqlx::migrate!().run(&pool).await?;
    let db = Arc::new(Db::new(pool, Crypto::new(&config.encryption_key)?));
    rustbox::oauth::store::recover(&db).await?;
    db.recover_accounts().await?;
    smtp::recover(&db).await?;
    let accounts = Arc::new(Accounts::new(db.clone(), config.clone()));
    accounts.oauth.validate_registrations().await?;
    accounts.start_all().await?;
    let shutdown = CancellationToken::new();
    let runtime = Arc::new(rustbox::native::Runtime::new(
        db.clone(),
        config.clone(),
        accounts.clone(),
    ));
    let native_runtime = runtime.clone();
    let native_signal = shutdown.clone();
    let mut native_worker = tokio::spawn(async move {
        tokio::try_join!(
            native_runtime.clone().run(native_signal.clone()),
            rustbox::content::run(native_runtime, native_signal)
        )
        .map(|_| ())
    });
    let mut event_worker =
        tokio::spawn(events::run((*db).clone(), config.clone(), shutdown.clone()));
    let mut smtp_worker = tokio::spawn(smtp::run(
        db.clone(),
        config.clone(),
        accounts.clone(),
        shutdown.clone(),
    ));
    tracing::info!(address=%config.listen,"mail service listening");
    let state = AppState {
        runtime,
        oauth: accounts.oauth.clone(),
        smtp_validation_slots: Arc::new(tokio::sync::Semaphore::new(config.smtp_concurrency)),
        db: db.clone(),
        config,
        accounts: accounts.clone(),
    };
    let signal = shutdown.clone();
    let mut server = tokio::spawn(async move {
        axum::serve(listener, api::router(state))
            .with_graceful_shutdown(signal.cancelled_owned())
            .await
    });
    let mut native_finished = false;
    let mut event_finished = false;
    let mut smtp_finished = false;
    let mut server_finished = false;
    let failure = tokio::select! {
        _ = wait_for_shutdown() => None,
        result = &mut native_worker => {native_finished=true;Some(anyhow::anyhow!("native/content worker exited unexpectedly: {result:?}"))},
        result = &mut event_worker => {
            event_finished = true;
            Some(anyhow::anyhow!("webhook worker exited unexpectedly: {result:?}"))
        }
        result = &mut smtp_worker => {
            smtp_finished = true;
            Some(anyhow::anyhow!("SMTP worker exited unexpectedly: {result:?}"))
        }
        result = &mut server => {
            server_finished = true;
            Some(anyhow::anyhow!("HTTP server exited unexpectedly: {result:?}"))
        }
    };
    shutdown.cancel();
    let drained = tokio::time::timeout(Duration::from_secs(45), async {
        if !native_finished {
            let _ = (&mut native_worker).await;
        }
        if !smtp_finished {
            let _ = (&mut smtp_worker).await;
        }
        if !server_finished {
            let _ = (&mut server).await;
        }
        accounts.shutdown().await;
        if !event_finished {
            let _ = (&mut event_worker).await;
        }
        db.pool.close().await;
    })
    .await;
    if drained.is_err() {
        native_worker.abort();
        smtp_worker.abort();
        event_worker.abort();
        server.abort();
        return Err(
            failure.unwrap_or_else(|| anyhow::anyhow!("graceful shutdown exceeded 45 seconds"))
        );
    }
    if let Some(failure) = failure {
        return Err(failure);
    }
    Ok(())
}

async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! { _=tokio::signal::ctrl_c()=>{}, _=terminate.recv()=>{} }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
