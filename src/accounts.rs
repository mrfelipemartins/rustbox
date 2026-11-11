use std::{collections::HashMap, sync::Arc, time::Duration};

use chrono::Utc;
use tokio::{
    sync::{Mutex, OwnedMutexGuard, Semaphore, mpsc, oneshot, watch},
    task::JoinHandle,
    time::{Instant, sleep, timeout},
};

use crate::{
    config::Config,
    db::{Account, Db},
    error::{Error, Result},
    imap::{self, ConnectFailure, Connection},
};

pub use crate::imap::{Command, Reply};

struct Request {
    command: Command,
    reply: oneshot::Sender<Result<Reply>>,
}

struct AccountTask {
    commands: mpsc::Sender<Request>,
    stop: watch::Sender<Option<String>>,
    join: JoinHandle<()>,
}

#[derive(Clone)]
pub struct Accounts {
    db: Arc<Db>,
    config: Arc<Config>,
    pub oauth: Arc<crate::oauth::OAuth>,
    tasks: Arc<Mutex<HashMap<String, AccountTask>>>,
    network: Arc<Semaphore>,
    lifecycle: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl Accounts {
    pub fn new(db: Arc<Db>, config: Arc<Config>) -> Self {
        Self {
            oauth: Arc::new(crate::oauth::OAuth::new(db.clone(), config.clone())),
            network: Arc::new(Semaphore::new(config.imap_concurrency)),
            db,
            config,
            tasks: Arc::new(Mutex::new(HashMap::new())),
            lifecycle: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn lifecycle_lock(&self, id: &str) -> OwnedMutexGuard<()> {
        let lock = {
            self.lifecycle
                .lock()
                .await
                .entry(id.to_owned())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        lock.lock_owned().await
    }

    pub async fn start_all(&self) -> Result<()> {
        for account in self.db.list_accounts().await? {
            if !account.config.backend.is_native()
                && account.state != "paused"
                && account.state != "auth_failed"
            {
                self.start(account).await;
            }
        }
        Ok(())
    }

    async fn start(&self, account: Account) {
        let (commands, mut receiver) = mpsc::channel(32);
        let (stop, mut stopped) = watch::channel(None);
        let db = self.db.clone();
        let config = self.config.clone();
        let oauth = self.oauth.clone();
        let network = self.network.clone();
        let id = account.id.clone();
        let key = id.clone();
        let mut tasks = self.tasks.lock().await;
        if tasks.contains_key(&id) {
            return;
        }
        let join = tokio::spawn(async move {
            loop {
                if stopped.borrow().is_some() {
                    break;
                }
                match run(
                    db.clone(),
                    config.clone(),
                    network.clone(),
                    oauth.clone(),
                    &account,
                    &mut receiver,
                    &mut stopped,
                )
                .await
                {
                    Ok(()) => break,
                    Err(_) => {
                        tracing::error!(account_id = %id, "account persistence unavailable; retrying");
                        tokio::select! { _ = stopped.changed() => break, _ = sleep(Duration::from_secs(5)) => {} }
                        if let Ok(current) = db.get_account(&id).await
                            && current.state == "connected"
                        {
                            let _ = db
                                .transition(
                                    &id,
                                    "disconnected",
                                    "account.disconnected",
                                    Some("persistence_error"),
                                    true,
                                    None,
                                )
                                .await;
                        }
                    }
                }
            }
        });
        tasks.insert(
            key,
            AccountTask {
                commands,
                stop,
                join,
            },
        );
    }

    pub async fn restart(&self, id: &str, reason: &str) -> Result<()> {
        self.stop(id, reason).await;
        let account = self.db.get_account(id).await?;
        if !account.config.backend.is_native() && account.state != "paused" {
            self.start(account).await;
        }
        Ok(())
    }

    pub async fn stop(&self, id: &str, reason: &str) {
        let task = self.tasks.lock().await.remove(id);
        if let Some(task) = task {
            let _ = task.stop.send(Some(reason.to_owned()));
            let _ = task.join.await;
        }
    }

    pub async fn shutdown(&self) {
        let tasks = std::mem::take(&mut *self.tasks.lock().await);
        for task in tasks.values() {
            let _ = task.stop.send(Some("shutdown".into()));
        }
        for (_, task) in tasks {
            let _ = task.join.await;
        }
    }

    pub async fn execute(&self, id: &str, command: Command) -> Result<Reply> {
        let commands = self
            .tasks
            .lock()
            .await
            .get(id)
            .map(|task| task.commands.clone())
            .ok_or_else(|| Error::Unavailable("account is not connected".into()))?;
        let mutating = command.mutating();
        let (reply, response) = oneshot::channel();
        commands
            .try_send(Request { command, reply })
            .map_err(|_| Error::Unavailable("account command queue unavailable".into()))?;
        match timeout(
            Duration::from_secs(self.config.command_timeout_secs + 5),
            response,
        )
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => {
                if mutating {
                    Err(Error::UnknownOutcome)
                } else {
                    Err(Error::Unavailable("account disconnected".into()))
                }
            }
            Err(_) => {
                if mutating {
                    Err(Error::UnknownOutcome)
                } else {
                    Err(Error::Unavailable("mail operation timed out".into()))
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run(
    db: Arc<Db>,
    config: Arc<Config>,
    network: Arc<Semaphore>,
    oauth: Arc<crate::oauth::OAuth>,
    account: &Account,
    requests: &mut mpsc::Receiver<Request>,
    stop: &mut watch::Receiver<Option<String>>,
) -> Result<()> {
    let mut failures: u32 = 0;
    db.transition(
        &account.id,
        "connecting",
        "account.connecting",
        None,
        true,
        None,
    )
    .await?;
    loop {
        if stop.borrow().is_some() {
            return Ok(());
        }
        let connected = tokio::select! {
            _ = stop.changed() => return Ok(()),
            result = async {
                let _permit = network.acquire().await.map_err(|_| ConnectFailure::Other(Error::Unavailable("service shutting down".into())))?;
                timeout(Duration::from_secs(config.command_timeout_secs), imap::connect(account, &config, &oauth)).await.unwrap_or_else(|_| Err(ConnectFailure::Other(Error::Unavailable("IMAP connect timeout".into()))))
            } => result,
        };
        match connected {
            Ok(connection) => {
                failures = 0;
                db.transition(
                    &account.id,
                    "connected",
                    "account.connected",
                    None,
                    false,
                    None,
                )
                .await?;
                let outcome =
                    connected_loop(connection, &db, &config, &network, account, requests, stop)
                        .await;
                let reason = stop.borrow().clone();
                if let Some(reason) = reason {
                    db.transition(
                        &account.id,
                        "disconnected",
                        "account.disconnected",
                        Some(&reason),
                        false,
                        None,
                    )
                    .await?;
                    return Ok(());
                }
                let reason = match outcome {
                    Err(Error::Database(_)) => "persistence_error",
                    Err(Error::Unavailable(ref s)) if s.contains("timeout") => "timeout",
                    _ => "network_error",
                };
                db.transition(
                    &account.id,
                    "disconnected",
                    "account.disconnected",
                    Some(reason),
                    true,
                    None,
                )
                .await?;
                db.transition(
                    &account.id,
                    "connecting",
                    "account.connecting",
                    None,
                    true,
                    None,
                )
                .await?;
            }
            Err(
                failure @ (ConnectFailure::Authentication
                | ConnectFailure::Permission
                | ConnectFailure::Other(Error::OAuth {
                    retryable: false, ..
                })),
            ) => {
                db.transition(
                    &account.id,
                    "auth_failed",
                    "account.auth_failed",
                    Some(match failure {
                        ConnectFailure::Permission => "oauth_mailbox_permission_denied",
                        ConnectFailure::Other(Error::OAuth { code, .. }) => code,
                        _ => "credentials_rejected",
                    }),
                    false,
                    None,
                )
                .await?;
                return Ok(());
            }
            Err(ConnectFailure::Other(error)) => {
                failures = failures.saturating_add(1);
                let seconds = retry_delay(failures);
                let reason = match error {
                    Error::Invalid(_) => "invalid_configuration",
                    _ => "connection_failed",
                };
                db.transition(
                    &account.id,
                    "disconnected",
                    "account.connection_failed",
                    Some(reason),
                    true,
                    Some(Utc::now() + chrono::Duration::seconds(seconds as i64)),
                )
                .await?;
                let delay = sleep(Duration::from_secs(seconds));
                tokio::pin!(delay);
                loop {
                    tokio::select! {
                        _ = stop.changed() => return Ok(()),
                        _ = &mut delay => break,
                        request = requests.recv() => {
                            if let Some(request) = request { let _ = request.reply.send(Err(Error::Unavailable("account is reconnecting".into()))); }
                            else { return Ok(()); }
                        }
                    }
                }
            }
        }
    }
}

fn retry_delay(failures: u32) -> u64 {
    let base = 2_u64.saturating_pow(failures.min(8)).min(300);
    base + rand::random::<u64>() % (base / 4 + 1)
}

async fn connected_loop(
    mut connection: Connection,
    db: &Db,
    config: &Config,
    network: &Semaphore,
    account: &Account,
    requests: &mut mpsc::Receiver<Request>,
    stop: &mut watch::Receiver<Option<String>>,
) -> Result<()> {
    let mut next_folders = Instant::now();
    let mut next_full = Instant::now();
    loop {
        let full = Instant::now() >= next_full;
        let include_folders = full || Instant::now() >= next_folders;
        tokio::select! {
            _ = stop.changed() => return Ok(()),
            result = async {
                let _permit = network.acquire().await.map_err(|_| Error::Unavailable("service shutting down".into()))?;
                timeout(Duration::from_secs(120), imap::reconcile(&mut connection, db, account, include_folders, full)).await.map_err(|_| Error::Unavailable("IMAP sync timeout".into()))?
            } => result?,
        }
        if include_folders {
            next_folders = Instant::now() + Duration::from_secs(110 + rand::random::<u64>() % 21);
        }
        if full {
            next_full = Instant::now() + Duration::from_secs(600);
        }
        let request = if connection.idle {
            let mut handle = connection.session.idle();
            timeout(Duration::from_secs(10), handle.init())
                .await
                .map_err(|_| Error::Unavailable("IDLE init timeout".into()))?
                .map_err(|_| Error::Unavailable("IDLE failed".into()))?;
            let request = {
                let (wait, interrupt) = handle.wait();
                tokio::pin!(wait);
                let renewal =
                    Instant::now() + Duration::from_secs(1440 + rand::random::<u64>() % 61);
                tokio::select! {
                    result = &mut wait => {
                        result.map_err(|_| Error::Unavailable("IDLE connection failed".into()))?;
                        None
                    }
                    request = requests.recv() => {
                        drop(interrupt);
                        timeout(Duration::from_secs(2), &mut wait).await.map_err(|_| Error::Unavailable("IDLE cancellation timeout".into()))?.map_err(|_| Error::Unavailable("IDLE cancellation failed".into()))?;
                        request
                    }
                    _ = tokio::time::sleep_until(next_folders.min(renewal)) => {
                        drop(interrupt);
                        timeout(Duration::from_secs(2), &mut wait).await.map_err(|_| Error::Unavailable("IDLE cancellation timeout".into()))?.map_err(|_| Error::Unavailable("IDLE cancellation failed".into()))?;
                        None
                    }
                    _ = stop.changed() => return Ok(()),
                }
            };
            connection.session = timeout(Duration::from_secs(10), handle.done())
                .await
                .map_err(|_| Error::Unavailable("IDLE DONE timeout".into()))?
                .map_err(|_| Error::Unavailable("IDLE DONE failed".into()))?;
            request
        } else {
            tokio::select! {
                request = requests.recv() => request,
                _ = sleep(Duration::from_secs(30)) => None,
                _ = stop.changed() => return Ok(()),
            }
        };
        if let Some(request) = request {
            if request.reply.is_closed() {
                continue;
            }
            let mutating = request.command.mutating();
            let result = tokio::select! {
                _ = stop.changed() => Err(if mutating { Error::UnknownOutcome } else { Error::Unavailable("service shutting down".into()) }),
                result = async {
                    let _permit = network.acquire().await.map_err(|_| Error::Unavailable("service shutting down".into()))?;
                    timeout(Duration::from_secs(config.command_timeout_secs), imap::execute(&mut connection, account, request.command, config, db)).await.map_err(|_| if mutating { Error::UnknownOutcome } else { Error::Unavailable("mail operation timeout".into()) })?
                } => result,
            };
            let broken = matches!(result, Err(Error::UnknownOutcome | Error::Unavailable(_)));
            let _ = request.reply.send(result);
            if broken {
                return Err(Error::Unavailable(
                    "connection requires reconciliation".into(),
                ));
            }
            if mutating {
                next_full = Instant::now();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_backoff_is_bounded() {
        for failure in 1..100 {
            assert!((2..=375).contains(&retry_delay(failure)));
        }
    }
}
