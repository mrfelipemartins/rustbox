use crate::{
    accounts::Accounts,
    config::Config,
    db::{AccountInput, Db},
    error::{Error, Result},
    events,
    imap::{Command, Reply},
    security, smtp,
};
use axum::{
    Extension, Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, patch, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;
use std::sync::Arc;
use subtle::ConstantTimeEq;

#[derive(Clone)]
pub struct AppState {
    pub runtime: Arc<crate::native::Runtime>,
    pub db: Arc<Db>,
    pub config: Arc<Config>,
    pub accounts: Arc<Accounts>,
    pub oauth: Arc<crate::oauth::OAuth>,
    pub smtp_validation_slots: Arc<tokio::sync::Semaphore>,
}

#[derive(Clone)]
pub(crate) struct AppId(pub(crate) String);

pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .merge(crate::oauth::api::routes())
        .route("/v1/accounts", get(list_accounts).post(create_account))
        .route(
            "/v1/accounts/{id}",
            get(get_account).put(update_account).delete(delete_account),
        )
        .route("/v1/accounts/{id}/pause", post(pause_account))
        .route("/v1/accounts/{id}/resume", post(resume_account))
        .route("/v1/accounts/{id}/folders", get(folders))
        .route("/v1/accounts/{id}/messages", get(messages))
        .route(
            "/v1/accounts/{id}/messages/{message_id}",
            get(message_status),
        )
        .route(
            "/v1/accounts/{id}/messages/{message_id}/content",
            get(content),
        )
        .route(
            "/v1/accounts/{id}/messages/{message_id}/attachments/{index}",
            get(attachment),
        )
        .route("/v1/accounts/{id}/flags", patch(flags))
        .route("/v1/accounts/{id}/move", post(move_message))
        .route("/v1/accounts/{id}/trash", post(trash))
        .route("/v1/accounts/{id}/send", post(send))
        .route("/v1/sends/{id}", get(send_status))
        .route("/v1/accounts/{id}/sync", post(sync_account))
        .route("/v1/syncs/{id}", get(sync_status))
        .route("/v1/accounts/{id}/labels", patch(labels))
        .route("/v1/sends/{id}/reconcile", post(reconcile_send))
        .route("/v1/sends/{id}/resolve", post(resolve_send))
        .route("/v1/sends/{id}/append/reconcile", post(reconcile_append))
        .route("/v1/sends/{id}/append/retry", post(retry_append))
        .route("/v1/events", get(list_events))
        .route("/metrics", get(metrics))
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate));
    Router::new()
        .merge(protected)
        .merge(crate::notifications::routes())
        .route("/health/live", get(|| async { StatusCode::OK }))
        .route("/health/ready", get(ready))
        .route(
            "/openapi.json",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "application/json")],
                    include_str!("../openapi.json"),
                )
            }),
        )
        .layer(DefaultBodyLimit::max(
            state.config.max_message_bytes.saturating_mul(2),
        ))
        .with_state(state)
}

async fn authenticate(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response> {
    let supplied = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(Error::Unauthorized)?;
    let app = state
        .config
        .api_keys
        .iter()
        .find(|(_, key)| bool::from(key.as_bytes().ct_eq(supplied.as_bytes())))
        .map(|(app, _)| app.clone())
        .ok_or(Error::Unauthorized)?;
    request.extensions_mut().insert(AppId(app));
    Ok(next.run(request).await)
}

async fn validate_account(
    config: &Config,
    oauth: &crate::oauth::OAuth,
    input: &AccountInput,
) -> Result<()> {
    input.validate()?;
    if input.backend.is_native() {
        if config.native.public_url.is_none() {
            return Err(Error::Invalid(
                "native.public_url is required for native accounts".into(),
            ));
        }
        let id = input
            .credential_id
            .as_deref()
            .ok_or_else(|| Error::Invalid("credential required".into()))?;
        let credential = crate::oauth::store::get(oauth.database(), id).await?;
        let expected = if input.backend == crate::db::Backend::Google {
            crate::oauth::config::Provider::Google
        } else {
            crate::oauth::config::Provider::Microsoft
        };
        if oauth.registration(&credential.client_id)?.provider != expected {
            return Err(Error::Invalid("credential provider mismatch".into()));
        }
        return Ok(());
    }
    for (protocol, server) in [
        (crate::oauth::MailProtocol::Imap, &input.imap),
        (crate::oauth::MailProtocol::Smtp, &input.smtp),
    ] {
        oauth.validate_mail_server(server, protocol).await?;
        security::resolve_public(&server.host, server.port, config.allow_private_networks).await?;
    }
    if let Some(url) = &input.webhook_url {
        security::safe_http_client(
            url,
            config.allow_private_networks,
            config.tls_ca_file.as_deref(),
        )
        .await?;
    }
    Ok(())
}

async fn account_public(s: &AppState, account: &crate::db::Account) -> Result<Value> {
    let mut value = account.public();
    for (protocol, server) in [
        ("imap", &account.config.imap),
        ("smtp", &account.config.smtp),
    ] {
        if let Some(id) = server.oauth_credential() {
            let credential = crate::oauth::store::get(&s.db, id).await?;
            value[protocol]["authorization"] = json!({"health":credential.health,"last_error":credential.last_error,"client_id":credential.client_id,"required_action":crate::oauth::store::required_action(&credential.health)});
        }
    }
    if !account.config.backend.is_native() {
        value["capabilities"] = json!({"identity":"location_with_confirmed_moves","labels":false,"synchronization":"idle_with_reconciliation"});
    }
    if account.config.backend.is_native() {
        if let Some(object) = value.as_object_mut() {
            object.remove("imap");
            object.remove("smtp");
            object.remove("smtp_state");
        }
        let delegated_shared = s
            .runtime
            .client
            .delegated_shared(account)
            .await
            .unwrap_or(account.config.shared);
        value["capabilities"] = json!({"identity":"mailbox_provider_id","labels":account.config.backend==crate::db::Backend::Google,"push_supported":!delegated_shared});
        let row=sqlx::query("SELECT last_sync_at,last_error,subscription_expires_at,subscription_error FROM native_state WHERE account_id=?").bind(&account.id).fetch_optional(&s.db.pool).await?;
        if let Some(row) = row {
            value["synchronization"] = json!({"last_success_at":row.try_get::<Option<chrono::NaiveDateTime>,_>("last_sync_at")?.map(|d|d.and_utc()),"error":row.try_get::<Option<String>,_>("last_error")?,"poll_interval_secs":s.config.native.poll_secs,"strategy":if delegated_shared{"polling"}else{"push_with_polling"}});
            value["subscription"] = json!({"expires_at":row.try_get::<Option<chrono::NaiveDateTime>,_>("subscription_expires_at")?.map(|d|d.and_utc()),"error":row.try_get::<Option<String>,_>("subscription_error")?});
        }
        if let Some(id) = &account.config.credential_id {
            value["authorization"] = crate::oauth::store::get(&s.db, id).await?.public();
        }
        value["sending"] = json!({"state":account.smtp_state});
    }
    value["capabilities"]["sending"] = serde_json::to_value(
        crate::sending_limits::SendingLimits::for_backend(account.config.backend, &s.config),
    )
    .map_err(|_| Error::Internal("sending capabilities serialization failed".into()))?;
    Ok(value)
}

async fn list_accounts(State(s): State<AppState>) -> Result<Json<Value>> {
    let mut accounts = Vec::new();
    for account in s.db.list_accounts().await? {
        accounts.push(account_public(&s, &account).await?);
    }
    Ok(Json(json!({"accounts":accounts})))
}
async fn get_account(State(s): State<AppState>, Path(id): Path<String>) -> Result<Json<Value>> {
    Ok(Json(
        account_public(&s, &s.db.get_account(&id).await?).await?,
    ))
}

async fn create_account(
    State(s): State<AppState>,
    Extension(app): Extension<AppId>,
    headers: axum::http::HeaderMap,
    Json(input): Json<AccountInput>,
) -> Result<impl IntoResponse> {
    validate_account(&s.config, &s.oauth, &input).await?;
    let key = headers
        .get("Idempotency-Key")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| Error::Invalid("Idempotency-Key is required".into()))?;
    let account = s.db.create_account_idempotent(&app.0, key, input).await?;
    if account.state == "created" {
        s.accounts
            .restart(&account.id, "configuration_change")
            .await?;
    }
    validate_smtp(s.clone(), account.id.clone());
    Ok((
        StatusCode::CREATED,
        Json(account_public(&s, &account).await?),
    ))
}

pub(crate) fn validate_smtp(s: AppState, id: String) {
    tokio::spawn(async move {
        let Ok(_permit) = s.smtp_validation_slots.acquire().await else {
            return;
        };
        if s.db
            .get_account(&id)
            .await
            .is_ok_and(|a| a.config.backend.is_native())
        {
            let _ = s.runtime.schedule(&id).await;
            return;
        }
        if let Err(error) = smtp::validate(&s.db, &s.config, &s.oauth, &id).await {
            tracing::warn!(account_id=%id,error=%error,"SMTP validation failed");
        }
    });
}

async fn update_account(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<AccountInput>,
) -> Result<Json<Value>> {
    validate_account(&s.config, &s.oauth, &input).await?;
    let _lifecycle = s.accounts.lifecycle_lock(&id).await;
    s.accounts.stop(&id, "configuration_change").await;
    let account = match s.db.update_account(&id, input).await {
        Ok(a) => a,
        Err(e) => {
            s.accounts.restart(&id, "configuration_change").await?;
            return Err(e);
        }
    };
    s.accounts.restart(&id, "configuration_change").await?;
    validate_smtp(s.clone(), id);
    Ok(Json(account_public(&s, &account).await?))
}
async fn delete_account(State(s): State<AppState>, Path(id): Path<String>) -> Result<StatusCode> {
    let _lifecycle = s.accounts.lifecycle_lock(&id).await;
    s.accounts.stop(&id, "deleted").await;
    s.db.delete_account(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn pause_account(State(s): State<AppState>, Path(id): Path<String>) -> Result<Json<Value>> {
    let _lifecycle = s.accounts.lifecycle_lock(&id).await;
    s.accounts.stop(&id, "pause").await;
    s.db.transition(&id, "paused", "account.paused", Some("pause"), false, None)
        .await?;
    Ok(Json(
        account_public(&s, &s.db.get_account(&id).await?).await?,
    ))
}
async fn resume_account(State(s): State<AppState>, Path(id): Path<String>) -> Result<Json<Value>> {
    let account = s.db.get_account(&id).await?;
    let mut credentials = std::collections::BTreeSet::new();
    if let Some(id) = account.config.credential_id.as_deref() {
        credentials.insert(id);
    }
    for server in [&account.config.imap, &account.config.smtp] {
        if let Some(credential_id) = server.oauth_credential() {
            credentials.insert(credential_id);
        }
    }
    for credential_id in credentials {
        s.oauth.invalidate(credential_id).await?;
        s.oauth.token(credential_id, true).await?;
    }
    let _lifecycle = s.accounts.lifecycle_lock(&id).await;
    s.accounts.stop(&id, "resume").await;
    s.db.transition(&id, "disconnected", "account.resumed", None, true, None)
        .await?;
    s.accounts.restart(&id, "resume").await?;
    validate_smtp(s.clone(), id.clone());
    Ok(Json(
        account_public(&s, &s.db.get_account(&id).await?).await?,
    ))
}

async fn folders(State(s): State<AppState>, Path(id): Path<String>) -> Result<Json<Value>> {
    let a = s.db.get_account(&id).await?;
    if a.config.backend.is_native() {
        return Ok(Json(json!({"folders":s.runtime.client.folders(&a).await?})));
    }
    match s.accounts.execute(&id, Command::ListFolders).await? {
        Reply::Folders(f) => Ok(Json(json!({"folders":f}))),
        _ => Err(Error::Internal("unexpected reply".into())),
    }
}
#[derive(Deserialize)]
struct ListQuery {
    mailbox: Option<String>,
    after: Option<String>,
    #[serde(default = "page_size")]
    limit: u32,
}
fn page_size() -> u32 {
    100
}
async fn messages(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>> {
    s.db.get_account(&id).await?;
    let ids:Vec<String>=sqlx::query_scalar("SELECT DISTINCT m.id FROM mail_messages m LEFT JOIN message_locations l ON l.message_id=m.id WHERE m.account_id=? AND m.removed_at IS NULL AND (? IS NULL OR (l.folder=? AND l.active=TRUE)) AND (? IS NULL OR m.id>?) ORDER BY m.id LIMIT ?").bind(&id).bind(&q.mailbox).bind(&q.mailbox).bind(&q.after).bind(&q.after).bind(q.limit.clamp(1,500)).fetch_all(&s.db.pool).await?;
    let mut values = Vec::new();
    for message in &ids {
        values.push(crate::content::locations(&s.runtime, &id, message).await?);
    }
    Ok(Json(json!({"messages":values,"next_cursor":ids.last()})))
}
async fn message_status(
    State(s): State<AppState>,
    Path((id, message)): Path<(String, String)>,
) -> Result<Json<Value>> {
    Ok(Json(
        crate::content::locations(&s.runtime, &id, &message).await?,
    ))
}
#[derive(Deserialize)]
struct ContentQuery {
    #[serde(default)]
    raw: bool,
}
async fn content(
    State(s): State<AppState>,
    Path((id, message)): Path<(String, String)>,
    Query(q): Query<ContentQuery>,
) -> Result<Response> {
    let raw = crate::content::fetch(&s.runtime, &id, &message).await?;
    if q.raw {
        return Ok(([(header::CONTENT_TYPE, "message/rfc822")], raw).into_response());
    }
    let parsed = tokio::task::spawn_blocking(move || crate::mail::parse(&raw))
        .await
        .map_err(|_| Error::Internal("MIME task failed".into()))??;
    let mut parsed = parsed;
    let details = crate::content::locations(&s.runtime, &id, &message).await?;
    parsed["id"] = json!(message);
    parsed["received_at"] = details["received_at"].clone();
    parsed["observed_at"] = details["observed_at"].clone();
    parsed["available_until"] = details["available_until"].clone();
    Ok(Json(parsed).into_response())
}
async fn attachment(
    State(s): State<AppState>,
    Path((id, message, index)): Path<(String, String, u32)>,
) -> Result<Response> {
    let raw = crate::content::fetch(&s.runtime, &id, &message).await?;
    let bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let m = mail_parser::MessageParser::default()
            .parse(&raw)
            .ok_or_else(|| Error::ContentUnavailable("invalid_mime".into()))?;
        Ok(m.attachment(index)
            .ok_or(Error::NotFound)?
            .contents()
            .to_vec())
    })
    .await
    .map_err(|_| Error::Internal("MIME task failed".into()))??;
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CONTENT_DISPOSITION, "attachment"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        Body::from(bytes),
    )
        .into_response())
}
#[derive(Deserialize)]
struct FlagInput {
    message_id: String,
    flags: Vec<String>,
}
#[derive(Deserialize)]
struct MoveInput {
    message_id: String,
    source: Option<String>,
    destination: String,
}
#[derive(Deserialize)]
struct MessageRef {
    message_id: String,
}
async fn locate(
    s: &AppState,
    account: &str,
    message: &str,
) -> Result<(crate::db::Account, String, String, u32, u32)> {
    let a = s.db.get_account(account).await?;
    let row=sqlx::query("SELECT m.provider_id,l.folder,l.uidvalidity,l.uid FROM mail_messages m JOIN message_locations l ON l.message_id=m.id WHERE m.id=? AND m.account_id=? AND l.active=TRUE LIMIT 1").bind(message).bind(account).fetch_optional(&s.db.pool).await?.ok_or(Error::NotFound)?;
    Ok((
        a,
        row.try_get_unchecked::<Option<String>, _>("provider_id")?
            .unwrap_or_default(),
        row.try_get_unchecked("folder")?,
        row.try_get::<Option<u32>, _>("uidvalidity")?.unwrap_or(0),
        row.try_get::<Option<u32>, _>("uid")?.unwrap_or(0),
    ))
}
async fn wake_after(s: &AppState, id: &str) {
    if let Err(error) = s.runtime.schedule(id).await {
        tracing::debug!(account_id=%id,error=%error,"mutation succeeded; synchronization wake deferred");
    }
}
async fn flags(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<FlagInput>,
) -> Result<StatusCode> {
    let (a, provider, folder, validity, uid) = locate(&s, &id, &input.message_id).await?;
    if a.config.backend.is_native() {
        s.runtime.client.flags(&a, &provider, &input.flags).await?;
        wake_after(&s, &id).await;
    } else {
        s.accounts
            .execute(
                &id,
                Command::Flags {
                    mailbox: folder,
                    uidvalidity: validity,
                    uid,
                    flags: input.flags,
                },
            )
            .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}
async fn move_message(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<MoveInput>,
) -> Result<StatusCode> {
    let (a, provider, folder, validity, uid) = locate(&s, &id, &input.message_id).await?;
    if a.config.backend.is_native() {
        s.runtime
            .client
            .move_message(
                &a,
                &provider,
                input.source.as_deref().unwrap_or(&folder),
                &input.destination,
            )
            .await?;
        wake_after(&s, &id).await;
    } else {
        s.accounts
            .execute(
                &id,
                Command::Move {
                    mailbox: folder,
                    uidvalidity: validity,
                    uid,
                    destination: input.destination,
                },
            )
            .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}
async fn trash(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<MessageRef>,
) -> Result<StatusCode> {
    let (a, provider, folder, validity, uid) = locate(&s, &id, &input.message_id).await?;
    if a.config.backend.is_native() {
        s.runtime.client.trash(&a, &provider).await?;
        wake_after(&s, &id).await;
    } else {
        s.accounts
            .execute(
                &id,
                Command::Trash {
                    mailbox: folder,
                    uidvalidity: validity,
                    uid,
                },
            )
            .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}
#[derive(Deserialize)]
struct Labels {
    message_id: String,
    #[serde(default)]
    add: Vec<String>,
    #[serde(default)]
    remove: Vec<String>,
}
async fn labels(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<Labels>,
) -> Result<StatusCode> {
    let (a, provider, _, _, _) = locate(&s, &id, &input.message_id).await?;
    s.runtime
        .client
        .labels(&a, &provider, &input.add, &input.remove)
        .await?;
    wake_after(&s, &id).await;
    Ok(StatusCode::NO_CONTENT)
}
async fn sync_account(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse> {
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"id":s.runtime.schedule(&id).await?})),
    ))
}
async fn sync_status(State(s): State<AppState>, Path(id): Path<String>) -> Result<Json<Value>> {
    let row =
        sqlx::query("SELECT id,account_id,state,created_at,finished_at FROM sync_runs WHERE id=?")
            .bind(id)
            .fetch_optional(&s.db.pool)
            .await?
            .ok_or(Error::NotFound)?;
    Ok(Json(
        json!({"id":row.try_get::<String,_>("id")?,"account_id":row.try_get::<String,_>("account_id")?,"state":row.try_get::<String,_>("state")?,"created_at":row.try_get::<chrono::NaiveDateTime,_>("created_at")?,"finished_at":row.try_get::<Option<chrono::NaiveDateTime>,_>("finished_at")?}),
    ))
}
async fn reconcile_send(State(s): State<AppState>, Path(id): Path<String>) -> Result<Json<Value>> {
    Ok(Json(
        smtp::reconcile(&s.db, s.config.clone(), &s.accounts, &id).await?,
    ))
}
async fn resolve_send(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<smtp::ResolveRequest>,
) -> Result<Json<smtp::SendStatus>> {
    Ok(Json(smtp::resolve(&s.db, &id, input).await?))
}
async fn reconcile_append(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<smtp::SendStatus>> {
    Ok(Json(smtp::reconcile_append(&s.db, &s.accounts, &id).await?))
}
async fn retry_append(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<smtp::AppendRetryRequest>,
) -> Result<Json<smtp::SendStatus>> {
    Ok(Json(
        smtp::retry_append(&s.db, &s.accounts, &id, input).await?,
    ))
}
async fn send(
    State(s): State<AppState>,
    Extension(app): Extension<AppId>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    Json(input): Json<smtp::SendRequest>,
) -> Result<impl IntoResponse> {
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| Error::Invalid("Idempotency-Key header required".into()))?;
    Ok((
        StatusCode::ACCEPTED,
        Json(smtp::enqueue(&s.db, &s.config, &id, &app.0, key, input).await?),
    ))
}
async fn send_status(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<smtp::SendStatus>> {
    Ok(Json(smtp::get(&s.db, &id).await?))
}
#[derive(Deserialize)]
struct EventQuery {
    #[serde(default)]
    after: u64,
    #[serde(default = "page_size")]
    limit: u32,
}
async fn list_events(
    State(s): State<AppState>,
    Query(q): Query<EventQuery>,
) -> Result<impl IntoResponse> {
    Ok(Json(
        events::list(&s.db, q.after, q.limit.clamp(1, 500)).await?,
    ))
}
async fn ready(State(s): State<AppState>) -> Result<StatusCode> {
    sqlx::query("SELECT 1").execute(&s.db.pool).await?;
    Ok(StatusCode::OK)
}
async fn metrics(State(s): State<AppState>) -> Result<String> {
    let connected: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM accounts WHERE state='connected'")
            .fetch_one(&s.db.pool)
            .await?;
    let queued: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM events WHERE delivery_state IN ('pending','delivering')",
    )
    .fetch_one(&s.db.pool)
    .await?;
    let sends: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sends WHERE status IN ('queued','sending')")
            .fetch_one(&s.db.pool)
            .await?;
    let reconnects: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE event_type='account.connected' AND JSON_EXTRACT(payload,'$.reconnected')=true").fetch_one(&s.db.pool).await?;
    let failures: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE attempt_count>0 AND delivery_state IN ('pending','expired')").fetch_one(&s.db.pool).await?;
    let lag: Option<i64> = sqlx::query_scalar(
        "SELECT MAX(TIMESTAMPDIFF(SECOND,updated_at,UTC_TIMESTAMP(6))) FROM mailbox_checkpoints",
    )
    .fetch_one(&s.db.pool)
    .await?;
    let native_lag: Option<i64> = sqlx::query_scalar(
        "SELECT MAX(TIMESTAMPDIFF(SECOND,last_sync_at,UTC_TIMESTAMP(6))) FROM native_state",
    )
    .fetch_one(&s.db.pool)
    .await?;
    let cache_bytes: u64 = sqlx::query_scalar("SELECT used_bytes FROM cache_budget WHERE id=1")
        .fetch_one(&s.db.pool)
        .await?;
    let pending: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM mail_messages WHERE content_state='pending'")
            .fetch_one(&s.db.pool)
            .await?;
    let rss = tokio::fs::read_to_string("/proc/self/status")
        .await
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|line| line.starts_with("VmRSS:"))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|v| v.parse::<u64>().ok())
        })
        .unwrap_or(0)
        * 1024;
    let provider_metrics = crate::providers::metrics();
    Ok(format!(
        "rustbox_provider_requests_google {}\nrustbox_provider_requests_microsoft {}\nrustbox_native_max_sync_age_seconds {}\nrustbox_content_cache_bytes {cache_bytes}\nrustbox_content_pending {pending}\nrustbox_connected_accounts {connected}\nrustbox_webhook_queue_depth {queued}\nprocess_resident_memory_bytes {rss}\nrustbox_send_queue_depth {sends}\nrustbox_reconnections_retained {reconnects}\nrustbox_webhook_failed_events {failures}\nrustbox_max_sync_age_seconds {}\n",
        provider_metrics["google"]["requests"],
        provider_metrics["microsoft"]["requests"],
        native_lag.unwrap_or(0),
        lag.unwrap_or(0)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::Crypto;
    use tower::ServiceExt;

    fn app() -> Router {
        let config = Arc::new(Config {
            database_url: String::new(),
            listen: "127.0.0.1:8080".parse().unwrap(),
            api_keys: std::collections::HashMap::from([("test".into(), "x".repeat(32))]),
            encryption_key: vec![7; 32],
            allow_private_networks: false,
            max_message_bytes: 1024,
            imap_concurrency: 1,
            smtp_concurrency: 1,
            webhook_concurrency: 1,
            command_timeout_secs: 1,
            log_filter: "info".into(),
            tls_ca_file: None,
            webhooks: Default::default(),
            oauth: Default::default(),
            native: Default::default(),
            cache: Default::default(),
        });
        let db = Arc::new(Db::new(
            sqlx::mysql::MySqlPoolOptions::new()
                .connect_lazy("mysql://root@127.0.0.1/test")
                .unwrap(),
            Crypto::new(&[7; 32]).unwrap(),
        ));
        let accounts = Arc::new(Accounts::new(db.clone(), config.clone()));
        router(AppState {
            runtime: Arc::new(crate::native::Runtime::new(
                db.clone(),
                config.clone(),
                accounts.clone(),
            )),
            smtp_validation_slots: Arc::new(tokio::sync::Semaphore::new(1)),
            db,
            config,
            oauth: accounts.oauth.clone(),
            accounts,
        })
    }

    #[tokio::test]
    async fn protects_account_routes_and_metrics() {
        for path in [
            "/v1/accounts",
            "/v1/events",
            "/metrics",
            "/v1/oauth/clients",
            "/v1/oauth/credentials/example",
            "/v1/oauth/authorizations/example",
        ] {
            let response = app()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
    }

    #[tokio::test]
    async fn liveness_and_openapi_are_public() {
        for path in ["/health/live", "/openapi.json"] {
            let response = app()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        let spec: Value = serde_json::from_str(include_str!("../openapi.json")).unwrap();
        assert!(spec["paths"]["/v1/events"].is_object());
    }
}
