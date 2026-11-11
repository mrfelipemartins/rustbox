pub mod api;
pub mod config;
mod provider;
pub mod store;

pub use provider::TokenResponse;

use crate::{
    config::Config,
    db::{Db, MailServer},
    error::{Error, Result},
};
use config::{Mode, Provider, ProviderRegistration};
use futures_util::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use provider::oauth_error;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Semaphore};

#[derive(Clone, Copy)]
pub enum MailProtocol {
    Imap,
    Smtp,
}

#[derive(Clone)]
pub struct OAuth {
    inner: Arc<Inner>,
}

struct Inner {
    db: Arc<Db>,
    config: Arc<Config>,
    slots: Arc<Semaphore>,
    cache: Mutex<Cache>,
    #[cfg(test)]
    test_endpoint: Option<String>,
}

#[derive(Default)]
struct Cache {
    tokens: HashMap<String, CachedToken>,
    failures: HashMap<String, CachedFailure>,
    last_cleanup: Option<Instant>,
    inflight:
        HashMap<String, Shared<BoxFuture<'static, std::result::Result<CachedToken, Failure>>>>,
}

#[derive(Clone)]
struct CachedToken {
    value: String,
    expires: Instant,
}

#[derive(Clone, Copy)]
struct Failure {
    code: &'static str,
    retryable: bool,
}

impl Failure {
    fn from_error(error: Error) -> Self {
        match error {
            Error::OAuth { code, retryable } => Self { code, retryable },
            Error::Database(_) => Self {
                code: "storage_unavailable",
                retryable: true,
            },
            Error::NotFound => Self {
                code: "credential_changed",
                retryable: true,
            },
            _ => Self {
                code: "configuration_error",
                retryable: false,
            },
        }
    }
    fn error(self) -> Error {
        oauth_error(self.code, self.retryable)
    }
}

struct CachedFailure {
    failure: Failure,
    retry_at: Instant,
    attempts: u32,
}

impl OAuth {
    pub fn new(db: Arc<Db>, config: Arc<Config>) -> Self {
        Self {
            inner: Arc::new(Inner {
                slots: Arc::new(Semaphore::new(config.oauth.concurrency)),
                db,
                config,
                cache: Mutex::new(Cache::default()),
                #[cfg(test)]
                test_endpoint: None,
            }),
        }
    }

    pub async fn validate_registrations(&self) -> Result<()> {
        for (id, registration) in &self.inner.config.oauth.providers {
            provider::validate_material(registration)
                .await
                .map_err(|_| {
                    Error::Invalid(format!(
                        "oauth.providers.{id} contains invalid signing credentials"
                    ))
                })?;
        }
        Ok(())
    }

    pub async fn invalidate(&self, id: &str) -> Result<()> {
        let credential = store::get(&self.inner.db, id).await?;
        let key = cache_key(&credential, self.registration(&credential.client_id)?);
        let mut cache = self.inner.cache.lock().await;
        cache.tokens.remove(&key);
        cache.failures.remove(&key);
        Ok(())
    }

    pub async fn seed(&self, credential: &store::Credential, token: &TokenResponse) -> Result<()> {
        let key = cache_key(credential, self.registration(&credential.client_id)?);
        let mut cache = self.inner.cache.lock().await;
        cache.failures.remove(&key);
        cache.tokens.insert(
            key,
            CachedToken {
                value: token.access_token.clone(),
                expires: Instant::now() + Duration::from_secs(token.expires_in.saturating_sub(60)),
            },
        );
        Ok(())
    }

    pub fn database(&self) -> &Db {
        &self.inner.db
    }

    pub fn registration(&self, id: &str) -> Result<&ProviderRegistration> {
        self.inner
            .config
            .oauth
            .providers
            .get(id)
            .ok_or_else(|| Error::Invalid("unknown OAuth client registration".into()))
    }

    pub fn authorization_url(
        &self,
        client_id: &str,
        redirect_uri: &str,
        state: &str,
        challenge: &str,
        login_hint: Option<&str>,
    ) -> Result<String> {
        provider::authorization_url(
            self.registration(client_id)?,
            redirect_uri,
            state,
            challenge,
            login_hint,
        )
    }

    pub async fn exchange_code(
        &self,
        client_id: &str,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
    ) -> Result<TokenResponse> {
        let registration = self.registration(client_id)?;
        let _permit = self
            .inner
            .slots
            .acquire()
            .await
            .map_err(|_| oauth_error("provider_unavailable", true))?;
        let token = provider::exchange(registration, code, redirect_uri, verifier).await?;
        if token
            .refresh_token
            .as_ref()
            .is_none_or(|value| value.is_empty())
        {
            return Err(oauth_error("refresh_token_missing", false));
        }
        Ok(token)
    }

    pub async fn validate_import(
        &self,
        client_id: &str,
        refresh_token: Option<&str>,
        subject: Option<&str>,
    ) -> Result<TokenResponse> {
        let registration = self.registration(client_id)?;
        match registration.mode {
            Mode::Delegated if refresh_token.is_none_or(str::is_empty) || subject.is_some() => {
                return Err(Error::Invalid(
                    "delegated credentials require a refresh token and no subject".into(),
                ));
            }
            Mode::DomainWideDelegation
                if refresh_token.is_some()
                    || subject.is_none_or(|v| !provider::valid_subject(v)) =>
            {
                return Err(Error::Invalid(
                    "domain-wide delegation requires a mailbox subject and no refresh token".into(),
                ));
            }
            Mode::Application if refresh_token.is_some() || subject.is_some() => {
                return Err(Error::Invalid(
                    "application credentials require neither refresh token nor subject".into(),
                ));
            }
            _ => {}
        }
        let _permit = self
            .inner
            .slots
            .acquire()
            .await
            .map_err(|_| oauth_error("provider_unavailable", true))?;
        provider::renew(registration, refresh_token, subject)
            .await
            .map_err(|error| service_error(registration, error))
    }

    pub async fn validate_server(
        &self,
        id: &str,
        host: &str,
        protocol: MailProtocol,
    ) -> Result<()> {
        let credential = store::get(&self.inner.db, id).await?;
        let registration = self.registration(&credential.client_id)?;
        if credential.mode()? != registration.mode {
            return Err(oauth_error("configuration_error", false));
        }
        if !allowed_host(registration.provider, host, protocol) {
            return Err(Error::Invalid(
                "OAuth credentials may only authenticate supported provider mail hosts".into(),
            ));
        }
        Ok(())
    }

    pub async fn validate_mail_server(
        &self,
        server: &MailServer,
        protocol: MailProtocol,
    ) -> Result<()> {
        let Some(id) = server.oauth_credential() else {
            return Ok(());
        };
        self.validate_server(id, &server.host, protocol).await?;
        let credential = store::get(&self.inner.db, id).await?;
        if self.registration(&credential.client_id)?.mode == Mode::DomainWideDelegation
            && credential
                .subject
                .as_deref()
                .is_none_or(|subject| !subject.eq_ignore_ascii_case(&server.username))
        {
            return Err(Error::Invalid(
                "mailbox username must match the delegation subject".into(),
            ));
        }
        if server.username.contains(['\0', '\u{1}', '\r', '\n']) {
            return Err(Error::Invalid("invalid OAuth mailbox username".into()));
        }
        Ok(())
    }

    pub async fn token(&self, id: &str, force: bool) -> Result<String> {
        for _ in 0..2 {
            let credential = store::get(&self.inner.db, id).await?;
            let registration = self.registration(&credential.client_id)?;
            if credential.mode()? != registration.mode {
                return Err(oauth_error("configuration_error", false));
            }
            if credential.health == "reauthorization_required" && !force {
                return Err(oauth_error("reauthorization_required", false));
            }
            if credential.health == "failed" && !force {
                let code = if credential.last_error.as_deref() == Some("permission_denied") {
                    "permission_denied"
                } else {
                    "configuration_error"
                };
                return Err(oauth_error(code, false));
            }
            let key = cache_key(&credential, registration);
            let future = {
                let mut cache = self.inner.cache.lock().await;
                let now = Instant::now();
                if cache
                    .last_cleanup
                    .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(60))
                {
                    cache.tokens.retain(|_, token| token.expires > now);
                    cache
                        .failures
                        .retain(|_, failure| failure.retry_at + Duration::from_secs(600) > now);
                    cache.last_cleanup = Some(now);
                }
                if cache
                    .tokens
                    .get(&key)
                    .is_some_and(|token| token.expires <= now)
                {
                    cache.tokens.remove(&key);
                }
                if cache.tokens.len() >= 100_000
                    && let Some(old) = cache.tokens.keys().next().cloned()
                {
                    cache.tokens.remove(&old);
                }
                if cache.failures.len() >= 10_000
                    && let Some(old) = cache.failures.keys().next().cloned()
                {
                    cache.failures.remove(&old);
                }
                if let Some(token) = cache.tokens.get(&key).filter(|_| !force) {
                    futures_util::future::ready(Ok(token.clone()))
                        .boxed()
                        .shared()
                } else if let Some(future) = cache.inflight.get(&key) {
                    future.clone()
                } else {
                    if let Some(failure) = cache.failures.get(&key)
                        && !force
                        && (failure.retry_at > now || !failure.failure.retryable)
                    {
                        return Err(failure.failure.error());
                    }
                    let manager = self.clone();
                    let inflight_key = key.clone();
                    let key = key.clone();
                    let credential = credential.clone();
                    let registration = registration.clone();
                    let task = tokio::spawn(async move {
                        let result = manager
                            .renew(&credential, &registration)
                            .await
                            .map_err(Failure::from_error);
                        let mut cache = manager.inner.cache.lock().await;
                        cache.inflight.remove(&key);
                        match &result {
                            Ok(token) => {
                                cache.tokens.insert(key.clone(), token.clone());
                                cache.failures.remove(&key);
                            }
                            Err(failure) => {
                                cache.tokens.remove(&key);
                                let attempts = cache
                                    .failures
                                    .get(&key)
                                    .map_or(1, |last| last.attempts.saturating_add(1));
                                let seconds = (2_u64.pow(attempts.min(8))
                                    + u64::from(rand::random::<u8>() % 5))
                                .min(300);
                                cache.failures.insert(
                                    key.clone(),
                                    CachedFailure {
                                        failure: *failure,
                                        retry_at: Instant::now() + Duration::from_secs(seconds),
                                        attempts,
                                    },
                                );
                            }
                        }
                        result
                    });
                    let future = async move {
                        task.await.unwrap_or(Err(Failure {
                            code: "provider_unavailable",
                            retryable: true,
                        }))
                    }
                    .boxed()
                    .shared();
                    cache.inflight.insert(inflight_key, future.clone());
                    future
                }
            };
            match future.await {
                Ok(token) => {
                    let current = store::get(&self.inner.db, id).await?;
                    if current.revision != credential.revision {
                        continue;
                    }
                    if credential.health != "healthy" {
                        store::set_health(&self.inner.db, id, credential.revision, "healthy", "")
                            .await?;
                    }
                    return Ok(token.value);
                }
                Err(failure) if failure.code == "credential_changed" => continue,
                Err(failure) => {
                    if !failure.retryable {
                        let health = if failure.code == "reauthorization_required" {
                            "reauthorization_required"
                        } else {
                            "failed"
                        };
                        store::set_health(
                            &self.inner.db,
                            id,
                            credential.revision,
                            health,
                            failure.code,
                        )
                        .await?;
                    }
                    return Err(failure.error());
                }
            }
        }
        Err(oauth_error("credential_changed", true))
    }

    async fn renew(
        &self,
        credential: &store::Credential,
        registration: &ProviderRegistration,
    ) -> Result<CachedToken> {
        let _permit = self
            .inner
            .slots
            .acquire()
            .await
            .map_err(|_| oauth_error("provider_unavailable", true))?;
        #[cfg(test)]
        let result = if let Some(endpoint) = &self.inner.test_endpoint {
            provider::renew_test(
                registration,
                credential.refresh_token.as_deref(),
                credential.subject.as_deref(),
                endpoint,
            )
            .await
        } else {
            provider::renew(
                registration,
                credential.refresh_token.as_deref(),
                credential.subject.as_deref(),
            )
            .await
        };
        #[cfg(not(test))]
        let result = provider::renew(
            registration,
            credential.refresh_token.as_deref(),
            credential.subject.as_deref(),
        )
        .await;
        let token = result.map_err(|error| service_error(registration, error))?;
        if !store::save_refresh(
            &self.inner.db,
            &credential.id,
            credential.revision,
            token.refresh_token.as_deref(),
        )
        .await?
        {
            return Err(oauth_error("credential_changed", true));
        }
        Ok(CachedToken {
            value: token.access_token,
            expires: Instant::now() + Duration::from_secs(token.expires_in.saturating_sub(60)),
        })
    }
}

fn service_error(registration: &ProviderRegistration, error: Error) -> Error {
    if registration.mode != Mode::Delegated
        && matches!(
            error,
            Error::OAuth {
                code: "reauthorization_required",
                ..
            }
        )
    {
        oauth_error("configuration_error", false)
    } else {
        error
    }
}

fn cache_key(credential: &store::Credential, registration: &ProviderRegistration) -> String {
    match registration.mode {
        Mode::Delegated => format!("delegated:{}:{}", credential.id, credential.revision),
        Mode::DomainWideDelegation => format!(
            "delegation:{}:{}",
            credential.client_id,
            credential.subject.as_deref().unwrap_or_default()
        ),
        Mode::Application => format!("application:{}", credential.client_id),
    }
}

fn allowed_host(provider: Provider, host: &str, protocol: MailProtocol) -> bool {
    let host = host.to_ascii_lowercase();
    match (provider, protocol) {
        (Provider::Google, MailProtocol::Imap) => host == "imap.gmail.com",
        (Provider::Google, MailProtocol::Smtp) => host == "smtp.gmail.com",
        (Provider::Microsoft, MailProtocol::Imap) => host == "outlook.office365.com",
        (Provider::Microsoft, MailProtocol::Smtp) => matches!(
            host.as_str(),
            "smtp.office365.com" | "smtp-mail.outlook.com"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mail_tokens_only_go_to_provider_hosts() {
        assert!(allowed_host(
            Provider::Google,
            "IMAP.GMAIL.COM",
            MailProtocol::Imap
        ));
        assert!(!allowed_host(
            Provider::Google,
            "imap.gmail.com.attacker.test",
            MailProtocol::Imap
        ));
        assert!(!allowed_host(
            Provider::Microsoft,
            "imap.gmail.com",
            MailProtocol::Imap
        ));
        assert!(!allowed_host(
            Provider::Google,
            "smtp.gmail.com",
            MailProtocol::Imap
        ));
        assert!(allowed_host(
            Provider::Microsoft,
            "smtp-mail.outlook.com",
            MailProtocol::Smtp
        ));
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod renewal_tests;
