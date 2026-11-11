mod google;
mod graph_upload;
pub use graph_upload::{
    GRAPH_CONTROL_REQUEST_BYTES, GRAPH_MAX_ATTACHMENT_BYTES, GRAPH_MAX_BASE_MIME_BYTES,
    GRAPH_SMALL_ATTACHMENT_LIMIT, GRAPH_UPLOAD_CHUNK_BYTES, GraphDraft, GraphUploadProgress,
    GraphUploadSession,
};
mod microsoft;

use crate::{
    config::Config,
    db::{Account, Backend},
    error::{Error, Result},
    oauth::{
        OAuth,
        config::{Mode, Provider},
        store,
    },
};
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Semaphore};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Folder {
    pub id: String,
    pub name: String,
    pub kind: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderMessage {
    pub id: String,
    pub locations: Vec<String>,
    pub metadata: Value,
    pub flags: Vec<String>,
    pub received_at: DateTime<Utc>,
    pub size: u64,
}

#[derive(Default)]
pub struct SyncPage {
    pub messages: Vec<ProviderMessage>,
    pub removed: Vec<String>,
    pub next: Option<String>,
    pub cursor: Option<String>,
    pub reset: bool,
}

pub struct SentSubmission {
    pub provider_id: Option<String>,
    pub thread_id: Option<String>,
}

pub struct Subscription {
    pub id: String,
    pub expires_at: DateTime<Utc>,
    pub history_id: Option<String>,
}

struct ProviderState {
    slots: Semaphore,
    cooldowns: Mutex<HashMap<String, Instant>>,
    requests: AtomicU64,
    throttled: AtomicU64,
}
static GOOGLE_STATE: OnceLock<ProviderState> = OnceLock::new();
static MICROSOFT_STATE: OnceLock<ProviderState> = OnceLock::new();

fn provider_state(backend: Backend, concurrency: usize) -> &'static ProviderState {
    let state = if backend == Backend::Google {
        &GOOGLE_STATE
    } else {
        &MICROSOFT_STATE
    };
    state.get_or_init(|| ProviderState {
        slots: Semaphore::new(concurrency.max(1)),
        cooldowns: Mutex::new(HashMap::new()),
        requests: AtomicU64::new(0),
        throttled: AtomicU64::new(0),
    })
}

pub fn metrics() -> Value {
    let values = |state: &OnceLock<ProviderState>| {
        state.get().map(|s|json!({"requests":s.requests.load(Ordering::Relaxed),"throttled":s.throttled.load(Ordering::Relaxed)})).unwrap_or_else(||json!({"requests":0,"throttled":0}))
    };
    json!({"google":values(&GOOGLE_STATE),"microsoft":values(&MICROSOFT_STATE)})
}

fn normalized_headers(headers: &[Value]) -> Result<Value> {
    let mut raw = String::from("MIME-Version: 1.0\r\n");
    for header in headers {
        if let (Some(name), Some(value)) = (header["name"].as_str(), header["value"].as_str())
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !name.is_empty()
        {
            raw.push_str(name);
            raw.push_str(": ");
            raw.push_str(value);
            raw.push_str("\r\n");
        }
    }
    raw.push_str("\r\n");
    let mut metadata = crate::mail::metadata(raw.as_bytes())?;
    metadata["raw_headers"] = json!(headers);
    Ok(metadata)
}

#[derive(Clone)]
pub struct NativeClient {
    config: Arc<Config>,
    oauth: Arc<OAuth>,
    #[cfg(test)]
    endpoint: Option<String>,
}

impl NativeClient {
    pub fn new(config: Arc<Config>, oauth: Arc<OAuth>) -> Self {
        Self {
            config,
            oauth,
            #[cfg(test)]
            endpoint: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_test_endpoint(
        config: Arc<Config>,
        oauth: Arc<OAuth>,
        endpoint: String,
    ) -> Self {
        let mut client = Self::new(config, oauth);
        client.endpoint = Some(endpoint);
        client
    }

    pub async fn validate_account(&self, account: &Account) -> Result<Value> {
        let id = account
            .config
            .credential_id
            .as_deref()
            .ok_or_else(|| Error::Invalid("native account requires a credential".into()))?;
        let credential = store::get(self.oauth.database(), id).await?;
        let registration = self.oauth.registration(&credential.client_id)?;
        let expected = match account.config.backend {
            Backend::Google => Provider::Google,
            Backend::Microsoft => Provider::Microsoft,
            Backend::Imap => return Err(Error::Invalid("native backend required".into())),
        };
        if registration.provider != expected {
            return Err(Error::Invalid(
                "credential provider does not match account backend".into(),
            ));
        }
        if account.config.backend == Backend::Microsoft
            && account.config.shared
            && registration.mode == Mode::Delegated
            && !registration.shared
        {
            return Err(Error::Invalid(
                "shared mailboxes require a shared Microsoft OAuth registration".into(),
            ));
        }
        if account.config.backend == Backend::Google {
            let profile = self
                .json(account, Method::GET, "/profile", None, false)
                .await?;
            let target = target(account)?;
            if !profile
                .get("emailAddress")
                .and_then(Value::as_str)
                .is_some_and(|mailbox| mailbox.eq_ignore_ascii_case(target))
            {
                return Err(Error::OAuth {
                    code: "target_mailbox_not_authorized",
                    retryable: false,
                });
            }
            if registration.mode == Mode::DomainWideDelegation
                && !credential
                    .subject
                    .as_deref()
                    .is_some_and(|subject| subject.eq_ignore_ascii_case(target))
            {
                return Err(Error::OAuth {
                    code: "target_mailbox_not_authorized",
                    retryable: false,
                });
            }
            Ok(profile)
        } else {
            self.json(
                account,
                Method::GET,
                "/mailFolders/inbox?$select=id,displayName",
                None,
                false,
            )
            .await
        }
    }

    pub async fn delegated_shared(&self, account: &Account) -> Result<bool> {
        if account.config.backend != Backend::Microsoft || !account.config.shared {
            return Ok(false);
        }
        let credential = store::get(
            self.oauth.database(),
            account
                .config
                .credential_id
                .as_deref()
                .ok_or_else(native_required)?,
        )
        .await?;
        Ok(self.oauth.registration(&credential.client_id)?.mode == Mode::Delegated)
    }

    pub async fn folders(&self, account: &Account) -> Result<Vec<Folder>> {
        match account.config.backend {
            Backend::Google => google::folders(self, account).await,
            Backend::Microsoft => microsoft::folders(self, account).await,
            Backend::Imap => Err(native_required()),
        }
    }
    pub async fn sync_page(
        &self,
        account: &Account,
        folder: &str,
        cursor: Option<&str>,
        next: Option<&str>,
        full: bool,
    ) -> Result<SyncPage> {
        match account.config.backend {
            Backend::Google => google::sync_page(self, account, cursor, next, full).await,
            Backend::Microsoft => {
                microsoft::sync_page(self, account, folder, cursor, next, full).await
            }
            Backend::Imap => Err(native_required()),
        }
    }
    pub async fn fetch(&self, account: &Account, provider_id: &str) -> Result<Vec<u8>> {
        match account.config.backend {
            Backend::Google => google::fetch(self, account, provider_id).await,
            Backend::Microsoft => {
                self.request(
                    account,
                    Method::GET,
                    &format!("/messages/{}/$value", segment(provider_id)),
                    None,
                    None,
                    false,
                )
                .await
            }
            Backend::Imap => Err(native_required()),
        }
    }
    pub async fn flags(
        &self,
        account: &Account,
        provider_id: &str,
        flags: &[String],
    ) -> Result<()> {
        match account.config.backend {
            Backend::Google => google::flags(self, account, provider_id, flags).await,
            Backend::Microsoft => microsoft::flags(self, account, provider_id, flags).await,
            Backend::Imap => Err(native_required()),
        }
    }
    pub async fn move_message(
        &self,
        account: &Account,
        provider_id: &str,
        source: &str,
        destination: &str,
    ) -> Result<ProviderMessage> {
        match account.config.backend {
            Backend::Google => {
                google::move_message(self, account, provider_id, source, destination).await
            }
            Backend::Microsoft => {
                microsoft::move_message(self, account, provider_id, destination).await
            }
            Backend::Imap => Err(native_required()),
        }
    }
    pub async fn trash(&self, account: &Account, provider_id: &str) -> Result<()> {
        match account.config.backend {
            Backend::Google => {
                self.json(
                    account,
                    Method::POST,
                    &format!("/messages/{}/trash", segment(provider_id)),
                    Some(json!({})),
                    false,
                )
                .await?;
                Ok(())
            }
            Backend::Microsoft => {
                microsoft::move_message(self, account, provider_id, "deleteditems").await?;
                Ok(())
            }
            Backend::Imap => Err(native_required()),
        }
    }
    pub async fn labels(
        &self,
        account: &Account,
        provider_id: &str,
        add: &[String],
        remove: &[String],
    ) -> Result<ProviderMessage> {
        if account.config.backend != Backend::Google {
            return Err(Error::Unsupported(
                "labels are supported by Gmail accounts".into(),
            ));
        }
        google::labels(self, account, provider_id, add, remove).await
    }
    pub async fn send(
        &self,
        account: &Account,
        raw: &[u8],
        thread_id: Option<&str>,
    ) -> Result<SentSubmission> {
        crate::sending_limits::validate(account.config.backend, &self.config, raw)?;
        match account.config.backend {
            Backend::Google => google::send(self, account, raw, thread_id).await,
            Backend::Microsoft => Err(Error::Unsupported(
                "Graph sending requires a durable send job".into(),
            )),
            Backend::Imap => Err(native_required()),
        }
    }
    pub async fn reconcile_candidates(
        &self,
        account: &Account,
        message_id: &str,
    ) -> Result<Vec<ProviderMessage>> {
        match account.config.backend {
            Backend::Google => google::reconcile(self, account, message_id).await,
            Backend::Microsoft => microsoft::reconcile(self, account, message_id).await,
            Backend::Imap => Err(native_required()),
        }
    }
    pub async fn subscription(
        &self,
        account: &Account,
        public_base: &str,
        existing: Option<&str>,
        client_state: &str,
        gmail_topic: Option<&str>,
    ) -> Result<Subscription> {
        match account.config.backend {
            Backend::Google => google::subscription(self, account, gmail_topic).await,
            Backend::Microsoft => {
                microsoft::subscription(self, account, public_base, existing, client_state).await
            }
            Backend::Imap => Err(native_required()),
        }
    }

    async fn json(
        &self,
        account: &Account,
        method: Method,
        path: &str,
        body: Option<Value>,
        submission: bool,
    ) -> Result<Value> {
        let mutating = !matches!(method, Method::GET | Method::HEAD);
        let raw = body
            .map(|v| serde_json::to_vec(&v))
            .transpose()
            .map_err(|_| Error::Internal("provider request serialization failed".into()))?;
        let bytes = self
            .request(
                account,
                method,
                path,
                raw.as_deref(),
                Some("application/json"),
                submission,
            )
            .await?;
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&bytes).map_err(|_| {
            if submission || mutating {
                Error::UnknownOutcome
            } else {
                Error::Unavailable("invalid provider response".into())
            }
        })
    }
    async fn request(
        &self,
        account: &Account,
        method: Method,
        path: &str,
        body: Option<&[u8]>,
        content_type: Option<&str>,
        submission: bool,
    ) -> Result<Vec<u8>> {
        self.request_headers(account, method, path, body, content_type, submission, &[])
            .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn request_headers(
        &self,
        account: &Account,
        method: Method,
        path: &str,
        body: Option<&[u8]>,
        content_type: Option<&str>,
        submission: bool,
        headers: &[(&str, &str)],
    ) -> Result<Vec<u8>> {
        let mutating = !matches!(method, Method::GET | Method::HEAD);
        let state = provider_state(account.config.backend, self.config.native.concurrency);
        let _permit = state
            .slots
            .acquire()
            .await
            .map_err(|_| Error::Unavailable("provider request scheduler stopped".into()))?;
        let cooldown_key = format!(
            "{}:{}",
            account.config.credential_id.as_deref().unwrap_or_default(),
            target(account)?
        );
        {
            let mut cooldowns = state.cooldowns.lock().await;
            cooldowns.retain(|_, until| *until > Instant::now());
            if let Some(until) = cooldowns.get(&cooldown_key) {
                return Err(Error::Throttled(
                    until
                        .saturating_duration_since(Instant::now())
                        .as_secs()
                        .max(1),
                ));
            }
        }

        let id = account
            .config
            .credential_id
            .as_deref()
            .ok_or_else(|| Error::Invalid("native account requires credentials".into()))?;
        let url = self.url(account, path)?;
        for force in [false, true] {
            let token = self.oauth.token(id, force).await?;
            #[cfg(test)]
            let client = if self.endpoint.is_some() {
                reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .map_err(|_| Error::Internal("test HTTP client failed".into()))?
            } else {
                crate::security::safe_http_client(&url, false, None).await?
            };
            #[cfg(not(test))]
            let client = crate::security::safe_http_client(&url, false, None).await?;
            let mut request = client
                .request(method.clone(), &url)
                .bearer_auth(token)
                .header("Prefer", "IdType=\"ImmutableId\"");
            for (name, value) in headers {
                request = request.header(*name, *value);
            }
            if let Some(content_type) = content_type {
                request = request.header(reqwest::header::CONTENT_TYPE, content_type);
            }
            if let Some(body) = body {
                request = request.body(body.to_vec());
            }
            state.requests.fetch_add(1, Ordering::Relaxed);
            let response = request.send().await.map_err(|_| {
                if submission || mutating {
                    Error::UnknownOutcome
                } else {
                    Error::Unavailable("provider request failed".into())
                }
            })?;
            if response.status() == StatusCode::UNAUTHORIZED && !force {
                if submission {
                    return Err(Error::OAuth {
                        code: "access_token_rejected",
                        retryable: true,
                    });
                }
                continue;
            }
            match response.status().as_u16() {
                200..=299 => {}
                401 => {
                    return Err(Error::OAuth {
                        code: "access_token_rejected",
                        retryable: false,
                    });
                }
                403 => {
                    return Err(Error::OAuth {
                        code: "permission_denied",
                        retryable: false,
                    });
                }
                400 if path.contains("/delta")
                    || path.contains("deltatoken")
                    || path.contains("skiptoken") =>
                {
                    let mut body = Vec::new();
                    let mut stream = response.bytes_stream();
                    while let Some(chunk) = stream.next().await {
                        let chunk = chunk.map_err(|_| {
                            Error::Unavailable("provider cursor response interrupted".into())
                        })?;
                        if body.len() + chunk.len() > 16_384 {
                            return Err(Error::Invalid(
                                "provider rejected synchronization request".into(),
                            ));
                        }
                        body.extend_from_slice(&chunk);
                    }
                    let error = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
                    let code = error["error"]["code"]
                        .as_str()
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    if code.contains("syncstate")
                        || code.contains("deltatoken")
                        || code.contains("resync")
                    {
                        return Err(Error::Conflict("provider cursor expired".into()));
                    }
                    return Err(Error::Invalid(
                        "provider rejected synchronization request".into(),
                    ));
                }
                404 => return Err(Error::NotFound),
                410 => return Err(Error::Conflict("provider cursor expired".into())),
                429 => {
                    let seconds = response
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .map(|value| {
                            value
                                .parse::<u64>()
                                .ok()
                                .or_else(|| {
                                    DateTime::parse_from_rfc2822(value).ok().map(|date| {
                                        (date.with_timezone(&Utc) - Utc::now()).num_seconds().max(1)
                                            as u64
                                    })
                                })
                                .unwrap_or(30)
                        })
                        .unwrap_or(30);
                    state.throttled.fetch_add(1, Ordering::Relaxed);
                    let until = Instant::now()
                        .checked_add(Duration::from_secs(seconds))
                        .unwrap_or_else(|| Instant::now() + Duration::from_secs(86400));
                    let mut cooldowns = state.cooldowns.lock().await;
                    if cooldowns.len() >= 100_000
                        && let Some(key) = cooldowns.keys().next().cloned()
                    {
                        cooldowns.remove(&key);
                    }
                    cooldowns.insert(cooldown_key.clone(), until);
                    return Err(Error::Throttled(seconds));
                }
                500..=599 => {
                    return Err(if submission || mutating {
                        Error::UnknownOutcome
                    } else {
                        Error::Unavailable("provider temporarily unavailable".into())
                    });
                }
                _ => return Err(Error::Invalid("provider rejected request".into())),
            }
            let maximum = self
                .config
                .max_message_bytes
                .saturating_mul(2)
                .max(1024 * 1024);
            let mut bytes = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| {
                    if submission || mutating {
                        Error::UnknownOutcome
                    } else {
                        Error::Unavailable("provider response interrupted".into())
                    }
                })?;
                if bytes.len() + chunk.len() > maximum {
                    return Err(if submission || mutating {
                        Error::UnknownOutcome
                    } else {
                        Error::Unsupported(
                            "provider content exceeds configured message limit".into(),
                        )
                    });
                }
                bytes.extend_from_slice(&chunk);
            }
            return Ok(bytes);
        }
        Err(Error::OAuth {
            code: "access_token_rejected",
            retryable: false,
        })
    }
    fn url(&self, account: &Account, path: &str) -> Result<String> {
        #[cfg(test)]
        if let Some(endpoint) = &self.endpoint {
            return Ok(format!("{endpoint}{path}"));
        }
        let host = match account.config.backend {
            Backend::Microsoft => "graph.microsoft.com",
            Backend::Google => "gmail.googleapis.com",
            Backend::Imap => return Err(native_required()),
        };
        if path.starts_with("https://") {
            let url = url::Url::parse(path)
                .map_err(|_| Error::Invalid("invalid provider continuation URL".into()))?;
            if url.host_str() != Some(host)
                || url.port_or_known_default() != Some(443)
                || !url.username().is_empty()
                || url.password().is_some()
                || url.fragment().is_some()
            {
                return Err(Error::Invalid(
                    "invalid provider continuation destination".into(),
                ));
            }
            return Ok(path.into());
        }
        let prefix = match account.config.backend {
            Backend::Microsoft => {
                format!("https://{host}/v1.0/users/{}", segment(target(account)?))
            }
            Backend::Google => format!("https://{host}/gmail/v1/users/me"),
            Backend::Imap => return Err(native_required()),
        };
        Ok(format!("{prefix}{path}"))
    }
}

fn target(account: &Account) -> Result<&str> {
    account
        .config
        .target_mailbox
        .as_deref()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Error::Invalid("native account requires target_mailbox".into()))
}
fn native_required() -> Error {
    Error::Invalid("native provider account required".into())
}
fn segment(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}
fn query(path: &str, pairs: &[(&str, String)]) -> String {
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        s.append_pair(k, v);
    }
    format!("{path}?{}", s.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(backend: Backend) -> Account {
        Account { id:uuid::Uuid::new_v4().to_string(),config:serde_json::from_value(json!({"name":"native","backend":backend,"target_mailbox":"mailbox@example.com","credential_id":uuid::Uuid::new_v4().to_string(),"sync_from":"2026-01-01T00:00:00Z"})).unwrap(),state:crate::db::LifecycleState::Created,smtp_state:"unvalidated".into(),last_connected_at:None,last_disconnected_at:None,next_retry_at:None,retry_count:0,last_error:None }
    }

    pub(super) fn config() -> Config {
        Config {native:Default::default(),cache:Default::default(),database_url:String::new(),listen:"127.0.0.1:0".parse().unwrap(),api_keys:Default::default(),encryption_key:vec![42;32],allow_private_networks:false,tls_ca_file:None,log_filter:String::new(),max_message_bytes:1024*1024,imap_concurrency:8,smtp_concurrency:8,webhook_concurrency:8,command_timeout_secs:60,webhooks:Default::default(),oauth:serde_json::from_value(json!({"providers":{"test":{"provider":"microsoft","mode":"delegated","tenant":"common","client_id":"client","client_secret":"secret"}}})).unwrap() }
    }

    #[tokio::test]
    async fn continuation_cannot_exfiltrate_bearer_tokens() {
        let config = Arc::new(config());
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .connect_lazy("mysql://localhost/test")
            .unwrap();
        let db = Arc::new(crate::db::Db::new(
            pool,
            crate::crypto::Crypto::new(&[42; 32]).unwrap(),
        ));
        let client = NativeClient::new(config.clone(), Arc::new(OAuth::new(db, config)));
        let account = account(Backend::Microsoft);
        assert!(client.url(&account, "https://evil.example/steal").is_err());
        assert!(
            client
                .url(&account, "https://graph.microsoft.com:8443/steal")
                .is_err()
        );
        assert!(
            client
                .url(&account, "https://user@graph.microsoft.com/v1.0/me")
                .is_err()
        );
        assert!(
            client
                .url(
                    &account,
                    "https://graph.microsoft.com/v1.0/me/messages?$skiptoken=opaque"
                )
                .is_ok()
        );
        assert!(
            client
                .url(&account, "/messages")
                .unwrap()
                .contains("mailbox%40example.com")
        );
    }

    #[tokio::test]
    #[ignore = "requires a dedicated MySQL database in TEST_DATABASE_URL"]
    async fn native_transport_uses_immutable_ids_and_never_retries_ambiguous_send()
    -> anyhow::Result<()> {
        use axum::{
            body::Bytes,
            extract::State,
            http::{HeaderMap, StatusCode},
            response::IntoResponse,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};
        #[derive(Clone)]
        struct Peer(Arc<AtomicUsize>);
        async fn peer(
            State(peer): State<Peer>,
            headers: HeaderMap,
            uri: axum::http::Uri,
            method: Method,
            body: Bytes,
        ) -> axum::response::Response {
            assert_eq!(headers["authorization"], "Bearer test-token");
            assert_eq!(headers["prefer"], "IdType=\"ImmutableId\"");
            peer.0.fetch_add(1, Ordering::SeqCst);
            if method == Method::GET && body.is_empty() {
                if uri.path().ends_with("/delta") {
                    let url = url::Url::parse(&format!("http://localhost{uri}")).unwrap();
                    assert!(!url.query_pairs().any(|(name, _)| name == "$filter"));
                    return axum::Json(json!({"value":[{"id":"provider","isRead":true}],"@odata.deltaLink":"/mailFolders/inbox/messages/delta?cursor=done"})).into_response();
                }
                if uri.path().ends_with("/messages/provider") {
                    return axum::Json(json!({"id":"provider","parentFolderId":"actual-inbox-id","receivedDateTime":"2026-11-11T00:00:00Z","isRead":true})).into_response();
                }
                (StatusCode::OK, axum::Json(json!({"value":[]}))).into_response()
            } else {
                StatusCode::BAD_GATEWAY.into_response()
            }
        }
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .connect(&std::env::var("TEST_DATABASE_URL")?)
            .await?;
        sqlx::migrate!().run(&pool).await?;
        let db = Arc::new(crate::db::Db::new(
            pool,
            crate::crypto::Crypto::new(&[42; 32])?,
        ));
        let credential = store::create(&db, "test", Some("refresh"), None).await?;
        let config = Arc::new(config());
        let oauth = Arc::new(OAuth::new(db.clone(), config.clone()));
        oauth
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
        let count = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new()
            .fallback(peer)
            .with_state(Peer(count.clone()));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = NativeClient::with_test_endpoint(config, oauth, format!("http://{address}"));
        let mut account = account(Backend::Microsoft);
        account.config.credential_id = Some(credential.id.clone());
        assert!(client.folders(&account).await?.is_empty());
        account.config.sync_from =
            Some(chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")?.with_timezone(&Utc));
        let page = client
            .sync_page(&account, "inbox", None, None, true)
            .await?;
        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.messages[0].locations, vec!["inbox"]);
        assert_eq!(page.messages[0].flags, vec!["\\Seen"]);
        assert!(matches!(
            client.graph_send_draft(&account, "draft-id").await,
            Err(Error::UnknownOutcome)
        ));
        assert_eq!(count.load(Ordering::SeqCst), 4);
        store::delete(&db, &credential.id).await?;
        server.abort();
        Ok(())
    }
}

#[cfg(test)]
#[path = "load_tests.rs"]
mod load_tests;
