//! Browser sessions and administrative projections. Existing mail operations are shared with /v1.
use crate::{
    api::{self, AppId, AppState},
    db::AccountInput,
    error::{Error, Result},
};
use axum::{
    Extension, Json, Router,
    extract::{ConnectInfo, Path, Query, Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, put},
};
use chrono::{DateTime, Utc};
use rand::{RngCore, rngs::OsRng};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{MySql, QueryBuilder, Row};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;

const COOKIE: &str = "rustbox_admin";
#[derive(Clone)]
struct Session {
    application: String,
    csrf: String,
    expires: DateTime<Utc>,
}
struct Sessions {
    entries: Mutex<HashMap<String, Session>>,
    attempts: Mutex<(Instant, u32)>,
    started: Instant,
}
fn secret() -> String {
    let mut bytes = [0; 32];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

pub fn routes(state: AppState) -> Router<AppState> {
    let sessions = Arc::new(Sessions {
        entries: Mutex::new(HashMap::new()),
        attempts: Mutex::new((Instant::now(), 0)),
        started: Instant::now(),
    });
    let protected = Router::new()
        .route("/session", get(session).delete(logout))
        .route("/overview", get(overview))
        .route("/system", get(system))
        .route("/mailboxes", get(mailboxes))
        .route("/mailboxes/{id}", put(save_mailbox))
        .route("/credentials", get(credentials))
        .route("/sends", get(sends))
        .route("/syncs", get(syncs))
        .route("/events", get(events))
        .route("/deliveries", get(deliveries))
        .route("/logs", get(logs))
        .merge(api::operations())
        .route_layer(middleware::from_fn_with_state(state, authenticate));
    Router::new()
        .nest(
            "/admin/api",
            protected.route("/session", axum::routing::post(login)),
        )
        .layer(Extension(sessions))
        .layer(middleware::from_fn(no_store))
}
async fn no_store(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response
}
fn cookie_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|part| part.trim().strip_prefix("rustbox_admin="))
}
async fn authenticate(
    State(state): State<AppState>,
    Extension(store): Extension<Arc<Sessions>>,
    mut request: Request,
    next: Next,
) -> Response {
    let found = cookie_token(request.headers())
        .and_then(|token| store.entries.lock().ok()?.get(token).cloned());
    let Some(session) = found
        .filter(|s| s.expires > Utc::now() && state.config.api_keys.contains_key(&s.application))
    else {
        return Error::Unauthorized.into_response();
    };
    if !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) {
        let valid = request
            .headers()
            .get("x-csrf-token")
            .and_then(|h| h.to_str().ok())
            .is_some_and(|v| bool::from(v.as_bytes().ct_eq(session.csrf.as_bytes())));
        if !valid {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error":{"code":"csrf_failed","message":"CSRF token required"}})),
            )
                .into_response();
        }
    }
    request
        .extensions_mut()
        .insert(AppId(session.application.clone()));
    request.extensions_mut().insert(session);
    next.run(request).await
}
#[derive(Deserialize)]
struct Login {
    api_key: String,
}
fn transport(headers: &HeaderMap, peer: Option<SocketAddr>) -> Result<bool> {
    // TLS terminates at the reverse proxy; Secure cookies enforce HTTPS in the browser.
    if headers
        .get("x-forwarded-proto")
        .is_some_and(|h| h == "https")
    {
        return Ok(true);
    }
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let local_host = host == "localhost"
        || host.starts_with("localhost:")
        || host == "127.0.0.1"
        || host.starts_with("127.0.0.1:")
        || host == "[::1]"
        || host.starts_with("[::1]:");
    if local_host && peer.is_some_and(|p| p.ip().is_loopback()) {
        return Ok(false);
    }
    Err(Error::Invalid(
        "Admin login requires HTTPS (HTTP is supported on loopback only)".into(),
    ))
}
fn session_json(s: &Session) -> Value {
    json!({"application":s.application,"csrf_token":s.csrf,"expires_at":s.expires})
}
async fn login(
    State(state): State<AppState>,
    Extension(store): Extension<Arc<Sessions>>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    Json(input): Json<Login>,
) -> Response {
    let secure = match transport(&headers, peer.map(|p| p.0.0)) {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };
    {
        let mut attempts = store.attempts.lock().unwrap();
        if attempts.0.elapsed() >= Duration::from_secs(60) {
            *attempts = (Instant::now(), 0);
        }
        if attempts.1 >= 20 {
            return (StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER,"60")], Json(json!({"error":{"code":"login_rate_limited","message":"Too many login attempts; try again in one minute"}}))).into_response();
        }
        attempts.1 += 1;
    }
    let Some(application) = state
        .config
        .api_keys
        .iter()
        .find(|(_, key)| bool::from(key.as_bytes().ct_eq(input.api_key.as_bytes())))
        .map(|(app, _)| app.clone())
    else {
        return Error::Unauthorized.into_response();
    };
    let session = Session {
        application,
        csrf: secret(),
        expires: Utc::now() + chrono::Duration::hours(state.config.admin.session_hours as i64),
    };
    let token = secret();
    let mut entries = store.entries.lock().unwrap();
    entries.retain(|_, s| s.expires > Utc::now());
    if entries.len() >= 1000 {
        return Error::Unavailable("Session capacity reached".into()).into_response();
    }
    entries.insert(token.clone(), session.clone());
    let cookie = format!(
        "{COOKIE}={token}; Path=/admin; HttpOnly; SameSite=Strict; Max-Age={}{}",
        state.config.admin.session_hours * 3600,
        if secure { "; Secure" } else { "" }
    );
    ([(header::SET_COOKIE, cookie)], Json(session_json(&session))).into_response()
}
async fn session(Extension(s): Extension<Session>) -> Json<Value> {
    Json(session_json(&s))
}
async fn logout(
    Extension(store): Extension<Arc<Sessions>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(token) = cookie_token(&headers) {
        store.entries.lock().unwrap().remove(token);
    }
    (
        [(
            header::SET_COOKIE,
            format!("{COOKIE}=; Path=/admin; HttpOnly; SameSite=Strict; Max-Age=0"),
        )],
        StatusCode::NO_CONTENT,
    )
}
#[derive(Default, Deserialize)]
struct List {
    #[serde(default = "one")]
    page: u32,
    #[serde(default = "fifty")]
    per_page: u32,
    q: Option<String>,
    provider: Option<String>,
    state: Option<String>,
    status: Option<String>,
    account_id: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
}
fn one() -> u32 {
    1
}
fn fifty() -> u32 {
    50
}
fn page(items: Vec<Value>, total: u64, q: &List) -> Value {
    json!({"items":items,"total":total,"page":q.page.max(1),"per_page":q.per_page.clamp(1,100)})
}
fn window(items: Vec<Value>, q: &List) -> Value {
    let total = items.len() as u64;
    let skip = (q.page.max(1) as usize - 1) * q.per_page.clamp(1, 100) as usize;
    page(
        items
            .into_iter()
            .skip(skip)
            .take(q.per_page.clamp(1, 100) as usize)
            .collect(),
        total,
        q,
    )
}
fn matches(value: &Value, key: &str, filter: &Option<String>) -> bool {
    filter
        .as_ref()
        .is_none_or(|s| s.is_empty() || value[key].as_str() == Some(s))
}
async fn mailboxes(State(s): State<AppState>, Query(q): Query<List>) -> Result<Json<Value>> {
    let search = q.q.as_ref().map(|q| q.to_lowercase());
    let accounts =
        s.db.list_accounts()
            .await?
            .into_iter()
            .filter(|account| {
                q.provider.as_ref().is_none_or(|provider| {
                    provider.is_empty() || account.config.backend.as_str() == provider
                }) && q
                    .state
                    .as_ref()
                    .is_none_or(|state| state.is_empty() || account.state.as_str() == state)
                    && search.as_ref().is_none_or(|q| {
                        format!(
                            "{} {} {}",
                            account.config.name,
                            account.config.target_mailbox.as_deref().unwrap_or(""),
                            account.config.imap.username
                        )
                        .to_lowercase()
                        .contains(q)
                    })
            })
            .collect::<Vec<_>>();
    let total = accounts.len() as u64;
    let size = q.per_page.clamp(1, 100) as usize;
    let skip = (q.page.max(1) as usize - 1) * size;
    let mut items = Vec::new();
    for account in accounts.into_iter().skip(skip).take(size) {
        items.push(api::account_public(&s, &account).await?);
    }
    Ok(Json(page(items, total, &q)))
}
async fn save_mailbox(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(mut input): Json<AccountInput>,
) -> Result<Json<Value>> {
    let old = s.db.get_account(&id).await?;
    if input.imap.password.is_empty() && input.imap.auth.is_none() {
        input.imap.password = old.config.imap.password;
    }
    if input.smtp.password.is_empty() && input.smtp.auth.is_none() {
        input.smtp.password = old.config.smtp.password;
    }
    if input
        .webhook_secret
        .as_ref()
        .is_none_or(|secret| secret.is_empty())
        && input.webhook_url.is_some()
    {
        input.webhook_secret = old.config.webhook_secret;
    }
    api::update_account(State(s), Path(id), Json(input)).await
}
async fn credentials(State(s): State<AppState>, Query(q): Query<List>) -> Result<Json<Value>> {
    let rows = sqlx::query("SELECT id FROM oauth_credentials ORDER BY created_at DESC,id")
        .fetch_all(&s.db.pool)
        .await?;
    let accounts = s.db.list_accounts().await?;
    let mut items = Vec::new();
    for row in rows {
        let id: String = row.try_get("id")?;
        let credential = crate::oauth::store::get(&s.db, &id).await?;
        let mut value = credential.public();
        value["provider"] = s
            .config
            .oauth
            .providers
            .get(&credential.client_id)
            .map(|p| json!(p.provider))
            .unwrap_or(Value::Null);
        value["mailboxes"] = json!(
            accounts
                .iter()
                .filter(|a| a.config.credential_id.as_deref() == Some(&id)
                    || a.config.imap.oauth_credential() == Some(&id)
                    || a.config.smtp.oauth_credential() == Some(&id))
                .map(|a| json!({"id":a.id,"name":a.config.name}))
                .collect::<Vec<_>>()
        );
        if !matches(&value, "provider", &q.provider) || !matches(&value, "health", &q.state) {
            continue;
        }
        if q.q.as_ref().is_some_and(|q| {
            !format!(
                "{} {} {}",
                id,
                credential.client_id,
                credential.subject.as_deref().unwrap_or("")
            )
            .to_lowercase()
            .contains(&q.to_lowercase())
        }) {
            continue;
        }
        items.push(value);
    }
    Ok(Json(window(items, &q)))
}
fn filters<'a>(
    builder: &mut QueryBuilder<'a, MySql>,
    q: &'a List,
    account: &str,
    state: &str,
    time: &str,
) {
    if let Some(id) = q.account_id.as_ref().filter(|v| !v.is_empty()) {
        builder.push(" AND ").push(account).push("=").push_bind(id);
    }
    if let Some(value) = q
        .status
        .as_ref()
        .or(q.state.as_ref())
        .filter(|v| !v.is_empty())
    {
        builder.push(" AND ").push(state).push("=").push_bind(value);
    }
    if let Some(from) = q.from {
        builder
            .push(" AND ")
            .push(time)
            .push(">=")
            .push_bind(from.naive_utc());
    }
    if let Some(to) = q.to {
        builder
            .push(" AND ")
            .push(time)
            .push("<=")
            .push_bind(to.naive_utc());
    }
}
fn limits(builder: &mut QueryBuilder<MySql>, q: &List) {
    builder
        .push(" LIMIT ")
        .push_bind(q.per_page.clamp(1, 100))
        .push(" OFFSET ")
        .push_bind((q.page.max(1) as u64 - 1) * q.per_page.clamp(1, 100) as u64);
}
async fn sends(State(s): State<AppState>, Query(q): Query<List>) -> Result<Json<Value>> {
    // Provider is stored in the encrypted account configuration, not in send rows.
    let provider_accounts = if let Some(provider) = q.provider.as_ref().filter(|v| !v.is_empty()) {
        Some(
            s.db.list_accounts()
                .await?
                .into_iter()
                .filter(|a| a.config.backend.as_str() == provider)
                .map(|a| a.id)
                .collect::<Vec<_>>(),
        )
    } else {
        None
    };
    let build = |select: &str| {
        let mut b = QueryBuilder::<MySql>::new(select);
        b.push(" FROM sends WHERE 1=1");
        filters(&mut b, &q, "account_id", "status", "created_at");
        if let Some(ids) = &provider_accounts {
            b.push(" AND account_id IN (");
            let mut list = b.separated(",");
            for id in ids {
                list.push_bind(id.clone());
            }
            list.push("NULL");
            list.push_unseparated(")");
        }
        b
    };
    let total: i64 = build("SELECT COUNT(*)")
        .build_query_scalar()
        .fetch_one(&s.db.pool)
        .await?;
    let mut b = build("SELECT id");
    b.push(" ORDER BY created_at DESC,id DESC");
    limits(&mut b, &q);
    let rows = b.build().fetch_all(&s.db.pool).await?;
    let mut items = Vec::new();
    for row in rows {
        let id: String = row.try_get("id")?;
        let mut v = serde_json::to_value(crate::smtp::get(&s.db, &id).await?)
            .map_err(|_| Error::Internal("send serialization failed".into()))?;
        let a = sqlx::query("SELECT account_id,created_at FROM sends WHERE id=?")
            .bind(id)
            .fetch_one(&s.db.pool)
            .await?;
        v["account_id"] = json!(a.try_get::<String, _>("account_id")?);
        v["created_at"] = json!(
            a.try_get::<chrono::NaiveDateTime, _>("created_at")?
                .and_utc()
        );
        items.push(v);
    }
    Ok(Json(page(items, total as u64, &q)))
}
async fn syncs(State(s): State<AppState>, Query(q): Query<List>) -> Result<Json<Value>> {
    let build = |select: &str| {
        let mut b = QueryBuilder::<MySql>::new(select);
        b.push(" FROM sync_runs WHERE 1=1");
        filters(&mut b, &q, "account_id", "state", "created_at");
        b
    };
    let total: i64 = build("SELECT COUNT(*)")
        .build_query_scalar()
        .fetch_one(&s.db.pool)
        .await?;
    let mut b = build("SELECT id,account_id,state,created_at,finished_at");
    b.push(" ORDER BY created_at DESC,id DESC");
    limits(&mut b, &q);
    let rows = b.build().fetch_all(&s.db.pool).await?;
    let mut items = Vec::new();
    for r in rows {
        items.push(json!({"id":r.try_get::<String,_>("id")?,"account_id":r.try_get::<String,_>("account_id")?,"state":r.try_get::<String,_>("state")?,"created_at":r.try_get::<chrono::NaiveDateTime,_>("created_at")?.and_utc(),"finished_at":r.try_get::<Option<chrono::NaiveDateTime>,_>("finished_at")?.map(|t|t.and_utc())}));
    }
    let checkpoints = if let Some(id) = &q.account_id {
        sqlx::query("SELECT mailbox,updated_at,backfill_complete FROM mailbox_checkpoints WHERE account_id=? ORDER BY mailbox").bind(id).fetch_all(&s.db.pool).await?.into_iter().map(|r|Ok(json!({"mailbox":r.try_get::<String,_>("mailbox")?,"updated_at":r.try_get::<chrono::NaiveDateTime,_>("updated_at")?.and_utc(),"backfill_complete":r.try_get::<bool,_>("backfill_complete")?}))).collect::<Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    let mut result = page(items, total as u64, &q);
    result["imap_checkpoints"] = json!(checkpoints);
    Ok(Json(result))
}
async fn event_list(s: &AppState, q: &List, delivery: bool) -> Result<Value> {
    let build = |select: &str| {
        let mut b = QueryBuilder::<MySql>::new(select);
        b.push(" FROM events WHERE 1=1");
        filters(&mut b, q, "account_id", "delivery_state", "observed_at");
        if delivery {
            b.push(" AND webhook_url IS NOT NULL");
        }
        if let Some(kind) = q.kind.as_ref().filter(|v| !v.is_empty()) {
            b.push(" AND event_type=").push_bind(kind);
        }
        if let Some(search) = q.q.as_ref().filter(|v| !v.is_empty()) {
            b.push(" AND (event_type LIKE ")
                .push_bind(format!("%{search}%"))
                .push(" OR CAST(payload AS CHAR) LIKE ")
                .push_bind(format!("%{search}%"))
                .push(")");
        }
        b
    };
    let total: i64 = build("SELECT COUNT(*)")
        .build_query_scalar()
        .fetch_one(&s.db.pool)
        .await?;
    let mut b = build(
        "SELECT id,schema_version,account_id,event_type,observed_at,payload,webhook_url,delivery_state,attempt_count,last_status,last_error,next_attempt_at,delivered_at",
    );
    b.push(" ORDER BY id DESC");
    limits(&mut b, q);
    let mut items = Vec::new();
    for r in b.build().fetch_all(&s.db.pool).await? {
        items.push(json!({"id":r.try_get::<u64,_>("id")?,"schema_version":r.try_get::<u32,_>("schema_version")?,"account_id":r.try_get::<String,_>("account_id")?,"type":r.try_get::<String,_>("event_type")?,"observed_at":r.try_get::<chrono::NaiveDateTime,_>("observed_at")?.and_utc(),"data":r.try_get::<sqlx::types::Json<Value>,_>("payload")?.0,"webhook_url":r.try_get::<Option<String>,_>("webhook_url")?,"delivery_state":r.try_get::<String,_>("delivery_state")?,"attempt_count":r.try_get::<u32,_>("attempt_count")?,"last_status":r.try_get::<Option<u16>,_>("last_status")?,"last_error":r.try_get::<Option<String>,_>("last_error")?,"next_attempt_at":r.try_get::<chrono::NaiveDateTime,_>("next_attempt_at")?.and_utc(),"delivered_at":r.try_get::<Option<chrono::NaiveDateTime>,_>("delivered_at")?.map(|t|t.and_utc())}));
    }
    Ok(page(items, total as u64, q))
}
async fn events(State(s): State<AppState>, Query(q): Query<List>) -> Result<Json<Value>> {
    Ok(Json(event_list(&s, &q, false).await?))
}
async fn deliveries(State(s): State<AppState>, Query(q): Query<List>) -> Result<Json<Value>> {
    Ok(Json(event_list(&s, &q, true).await?))
}
async fn logs(
    State(s): State<AppState>,
    Query(q): Query<crate::admin_logs::LogQuery>,
) -> Result<Json<Value>> {
    Ok(Json(
        crate::admin_logs::list(&s.db, q, &s.config.admin.logs).await?,
    ))
}
async fn overview(
    State(s): State<AppState>,
    Extension(store): Extension<Arc<Sessions>>,
) -> Result<Json<Value>> {
    let mut counts = serde_json::Map::new();
    let mut providers = serde_json::Map::new();
    let mut attention = Vec::new();
    for account in s.db.list_accounts().await? {
        let state = counts.entry(account.state.as_str()).or_insert(json!(0));
        *state = json!(state.as_u64().unwrap_or(0) + 1);
        let p = providers
            .entry(account.config.backend.as_str())
            .or_insert(json!(0));
        *p = json!(p.as_u64().unwrap_or(0) + 1);
        if account.last_error.is_some()
            || account.smtp_state == "failed"
            || matches!(account.state.as_str(), "auth_failed" | "disconnected")
        {
            attention.push(json!({"kind":"mailbox","id":account.id,"name":account.config.name,"state":account.state,"error":account.last_error}));
        }
    }
    let row=sqlx::query("SELECT (SELECT COUNT(*) FROM sends WHERE status IN ('queued','sending')) AS sends_queued,(SELECT COUNT(*) FROM sends WHERE status='unknown') AS sends_unknown,(SELECT COUNT(*) FROM events WHERE webhook_url IS NOT NULL AND delivery_state IN ('pending','delivering')) AS webhook_backlog,(SELECT used_bytes FROM cache_budget WHERE id=1) AS cache_bytes,(SELECT COUNT(*) FROM mail_messages WHERE content_state='pending') AS content_pending,(SELECT MAX(TIMESTAMPDIFF(SECOND,last_sync_at,UTC_TIMESTAMP(6))) FROM native_state) AS native_sync_age_seconds,(SELECT MAX(TIMESTAMPDIFF(SECOND,updated_at,UTC_TIMESTAMP(6))) FROM mailbox_checkpoints) AS imap_checkpoint_age_seconds").fetch_one(&s.db.pool).await?;
    let credentials = sqlx::query(
        "SELECT id,health,last_error FROM oauth_credentials WHERE health<>'healthy' LIMIT 50",
    )
    .fetch_all(&s.db.pool)
    .await?;
    for r in credentials {
        attention.push(json!({"kind":"credential","id":r.try_get::<String,_>("id")?,"state":r.try_get::<String,_>("health")?,"error":r.try_get::<Option<String>,_>("last_error")?}));
    }
    let native_errors = sqlx::query("SELECT account_id,last_error,subscription_error FROM native_state WHERE last_error IS NOT NULL OR subscription_error IS NOT NULL LIMIT 100").fetch_all(&s.db.pool).await?;
    for r in native_errors {
        let id: String = r.try_get("account_id")?;
        for (column, state) in [
            ("last_error", "synchronization_error"),
            ("subscription_error", "subscription_error"),
        ] {
            if let Some(error) = r.try_get::<Option<String>, _>(column)? {
                attention.push(json!({"kind":"mailbox", "id":id, "state":state,"error":error}));
            }
        }
    }
    let authorization_required: i64 = sqlx::query_scalar("SELECT COUNT(DISTINCT ac.account_id) FROM oauth_account_credentials ac JOIN oauth_credentials c ON c.id=ac.credential_id WHERE c.health<>'healthy'").fetch_one(&s.db.pool).await?;
    let unknowns = sqlx::query(
        "SELECT id,account_id FROM sends WHERE status='unknown' ORDER BY created_at DESC LIMIT 50",
    )
    .fetch_all(&s.db.pool)
    .await?;
    for r in unknowns {
        attention.push(json!({"kind":"send","id":r.try_get::<String,_>("id")?,"account_id":r.try_get::<String,_>("account_id")?,"state":"unknown"}));
    }
    Ok(Json(
        json!({"ready":true,"database":"ready","version":env!("CARGO_PKG_VERSION"),"uptime_seconds":store.started.elapsed().as_secs(),"observed_at":Utc::now(),"mailboxes_by_state":counts,"mailboxes_authorization_required":authorization_required,"mailboxes_by_provider":providers,"sends_queued":row.try_get::<i64,_>("sends_queued")?,"sends_unknown":row.try_get::<i64,_>("sends_unknown")?,"webhook_backlog":row.try_get::<i64,_>("webhook_backlog")?,"cache_bytes":row.try_get::<u64,_>("cache_bytes")?,"cache_capacity_bytes":s.config.cache.capacity_bytes,"content_pending":row.try_get::<i64,_>("content_pending")?,"native_sync_age_seconds":row.try_get::<Option<i64>,_>("native_sync_age_seconds")?,"imap_checkpoint_age_seconds":row.try_get::<Option<i64>,_>("imap_checkpoint_age_seconds")?,"attention":attention,"recent_events":event_list(&s,&List{per_page:10,..Default::default()},false).await?["items"]}),
    ))
}
async fn system(
    State(s): State<AppState>,
    Extension(store): Extension<Arc<Sessions>>,
) -> Json<Value> {
    let database = sqlx::query("SELECT 1").execute(&s.db.pool).await.is_ok();
    let mut applications = s.config.api_keys.keys().collect::<Vec<_>>();
    applications.sort();
    Json(
        json!({"version":env!("CARGO_PKG_VERSION"),"uptime_seconds":store.started.elapsed().as_secs(),"database_ready":database,"observed_at":Utc::now(),"applications":applications,"workers":{"imap":s.config.imap_concurrency,"smtp":s.config.smtp_concurrency,"webhook":s.config.webhook_concurrency,"native":s.config.native.concurrency,"cache":s.config.cache.concurrency},"cache":{"capacity_bytes":s.config.cache.capacity_bytes,"retention_days":s.config.cache.retention_days,"max_message_bytes":s.config.cache.max_message_bytes},"native":{"public_url":s.config.native.public_url,"poll_secs":s.config.native.poll_secs,"safety_poll_secs":s.config.native.safety_poll_secs,"reconcile_secs":s.config.native.reconcile_secs},"mail":{"max_message_bytes":s.config.max_message_bytes},"webhooks":{"timeout_secs":s.config.webhooks.timeout_secs,"retry_window_hours":s.config.webhooks.retry_window_hours,"retention_days":s.config.webhooks.retention_days},"admin":{"session_hours":s.config.admin.session_hours,"logs":{"retention_days":s.config.admin.logs.retention_days,"capacity_bytes":s.config.admin.logs.capacity_bytes,"max_record_bytes":s.config.admin.logs.max_record_bytes}},"providers":s.config.oauth.providers.iter().map(|(id,p)|json!({"id":id,"provider":p.provider,"mode":p.mode,"shared":p.shared,"redirect_uris":p.redirect_uris})).collect::<Vec<_>>(),"configuration_requires_restart":true,"log_dropped_records":crate::admin_logs::dropped_records()}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{accounts::Accounts, config::Config, crypto::Crypto, db::Db};
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;

    fn state(enabled: bool, pool: Option<sqlx::MySqlPool>) -> AppState {
        let admin = crate::config::AdminConfig {
            enabled,
            ..Default::default()
        };
        let config = Arc::new(Config {
            database_url: String::new(),
            listen: "127.0.0.1:8080".parse().unwrap(),
            api_keys: HashMap::from([
                ("first".into(), "a".repeat(32)),
                ("second".into(), "b".repeat(32)),
            ]),
            encryption_key: vec![7; 32],
            allow_private_networks: false,
            tls_ca_file: None,
            log_filter: "info".into(),
            max_message_bytes: 1024,
            imap_concurrency: 1,
            smtp_concurrency: 1,
            webhook_concurrency: 1,
            command_timeout_secs: 1,
            webhooks: Default::default(),
            oauth: Default::default(),
            native: Default::default(),
            cache: Default::default(),
            admin,
        });
        let db = Arc::new(Db::new(
            pool.unwrap_or_else(|| {
                sqlx::mysql::MySqlPoolOptions::new()
                    .connect_lazy("mysql://root@127.0.0.1/test")
                    .unwrap()
            }),
            Crypto::new(&[7; 32]).unwrap(),
        ));
        let accounts = Arc::new(Accounts::new(db.clone(), config.clone()));
        AppState {
            runtime: Arc::new(crate::native::Runtime::new(
                db.clone(),
                config.clone(),
                accounts.clone(),
            )),
            db,
            config,
            oauth: accounts.oauth.clone(),
            accounts,
            smtp_validation_slots: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }
    fn login_request(key: &str) -> Request<Body> {
        let mut r = Request::builder()
            .method("POST")
            .uri("/admin/api/session")
            .header("host", "localhost:8080")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({"api_key":key}).to_string()))
            .unwrap();
        r.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:43210".parse::<SocketAddr>().unwrap(),
        ));
        r
    }
    async fn decode(response: Response) -> Value {
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap()
    }
    async fn login_as(router: &Router, key: &str) -> (String, Value) {
        let r = router.clone().oneshot(login_request(key)).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let cookie = r.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        (cookie, decode(r).await)
    }
    #[tokio::test]
    async fn every_key_can_login_sessions_require_csrf_and_logout_invalidates() {
        let app = api::router(state(true, None));
        for (application, key) in [("first", "a".repeat(32)), ("second", "b".repeat(32))] {
            let (cookie, session) = login_as(&app, &key).await;
            assert_eq!(session["application"], application);
            assert!(!session.to_string().contains(&key));
            let r = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/admin/api/session")
                        .header(header::COOKIE, &cookie)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::OK);
            let r = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("DELETE")
                        .uri("/admin/api/session")
                        .header(header::COOKIE, &cookie)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::FORBIDDEN);
            let r = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("DELETE")
                        .uri("/admin/api/session")
                        .header(header::COOKIE, &cookie)
                        .header("x-csrf-token", session["csrf_token"].as_str().unwrap())
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::NO_CONTENT);
            let r = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/admin/api/session")
                        .header(header::COOKIE, &cookie)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        }
        assert_eq!(
            app.oneshot(login_request(&"z".repeat(32)))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    #[tokio::test]
    async fn disabled_routes_are_404_and_restart_clears_sessions() {
        let disabled = api::router(state(false, None));
        for path in [
            "/admin",
            "/admin/mailboxes",
            "/admin/api/session",
            "/admin/api/overview",
        ] {
            assert_eq!(
                disabled
                    .clone()
                    .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                    .await
                    .unwrap()
                    .status(),
                StatusCode::NOT_FOUND
            );
        }
        let first = api::router(state(true, None));
        let (cookie, _) = login_as(&first, &"a".repeat(32)).await;
        let fresh = api::router(state(true, None));
        let r = fresh
            .oneshot(
                Request::builder()
                    .uri("/admin/api/session")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }
    #[tokio::test]
    async fn expired_sessions_rejected_and_login_attempts_limited() {
        let s = state(true, None);
        let store = Arc::new(Sessions {
            entries: Mutex::new(HashMap::from([(
                "token".into(),
                Session {
                    application: "first".into(),
                    csrf: "csrf".into(),
                    expires: Utc::now() - chrono::Duration::seconds(1),
                },
            )])),
            attempts: Mutex::new((Instant::now(), 0)),
            started: Instant::now(),
        });
        let app = Router::new()
            .route("/session", get(session))
            .route_layer(middleware::from_fn_with_state(s.clone(), authenticate))
            .layer(Extension(store))
            .with_state(s);
        let r = app
            .oneshot(
                Request::builder()
                    .uri("/session")
                    .header(header::COOKIE, "rustbox_admin=token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        let app = api::router(state(true, None));
        for _ in 0..20 {
            assert_eq!(
                app.clone()
                    .oneshot(login_request("bad"))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::UNAUTHORIZED
            );
        }
        assert_eq!(
            app.oneshot(login_request("bad")).await.unwrap().status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }
    #[test]
    fn plain_http_requires_loopback_host_and_peer() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, "localhost:8080".parse().unwrap());
        assert!(!transport(&h, Some("127.0.0.1:1".parse().unwrap())).unwrap());
        assert!(transport(&h, Some("192.0.2.1:1".parse().unwrap())).is_err());
        assert!(transport(&h, None).is_err());
        h.insert(header::HOST, "rustbox.example".parse().unwrap());
        assert!(transport(&h, Some("127.0.0.1:1".parse().unwrap())).is_err());
        h.insert("x-forwarded-proto", "https".parse().unwrap());
        assert!(transport(&h, None).unwrap());
    }
    #[tokio::test]
    async fn browser_sessions_and_bearer_apis_are_isolated_and_mutations_require_csrf() {
        let app = api::router(state(true, None));
        let (cookie, _) = login_as(&app, &"a".repeat(32)).await;
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/accounts")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/api/system")
                    .header(header::AUTHORIZATION, format!("Bearer {}", "a".repeat(32)))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        for path in [
            "/admin/api/v1/accounts/example/pause",
            "/admin/api/v1/accounts/example/send",
            "/admin/api/v1/sends/example/resolve",
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header(header::COOKIE, &cookie)
                        .header("x-csrf-token", "wrong")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        }
    }

    #[tokio::test]
    #[ignore = "requires TEST_DATABASE_URL pointing at a dedicated migrated MySQL database"]
    async fn mysql_admin_lists_filters_and_redacts_secrets() {
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .connect(&std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL"))
            .await
            .unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        let s = state(true, Some(pool.clone()));
        let name = format!("admin-test-{}", uuid::Uuid::new_v4());
        let input:AccountInput=serde_json::from_value(json!({"name":name,"backend":"imap","imap":{"host":"imap.example.com","port":993,"username":"test@example.com","password":"never-display-imap-password","tls_mode":"implicit"},"smtp":{"host":"smtp.example.com","port":465,"username":"test@example.com","password":"never-display-smtp-password","tls_mode":"implicit"},"webhook_url":"https://example.com/webhook","webhook_secret":"never-display-webhook-secret-at-least-32"})).unwrap();
        let account = s.db.create_account(input).await.unwrap();
        let send_id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO sends(id,account_id,app_id,idempotency_key,request_hash,status,next_attempt_at,created_at,updated_at,expires_at,message_id,append_status) VALUES(?,?,'first',?,?,'unknown',UTC_TIMESTAMP(6),UTC_TIMESTAMP(6),UTC_TIMESTAMP(6),DATE_ADD(UTC_TIMESTAMP(6),INTERVAL 1 DAY),'<admin-test@rustbox.local>','not_requested')").bind(&send_id).bind(&account.id).bind(&send_id).bind(vec![0u8;32]).execute(&pool).await.unwrap();
        let app = api::router(s.clone());
        let (cookie, session) = login_as(&app, &"a".repeat(32)).await;
        for path in [
            format!("/admin/api/mailboxes?q={name}&provider=imap"),
            format!(
                "/admin/api/sends?account_id={}&provider=imap&status=unknown",
                account.id
            ),
            format!("/admin/api/syncs?account_id={}", account.id),
            format!("/admin/api/events?account_id={}", account.id),
            format!("/admin/api/deliveries?account_id={}", account.id),
            "/admin/api/credentials".into(),
            "/admin/api/logs".into(),
            "/admin/api/overview".into(),
            "/admin/api/system".into(),
        ] {
            let r = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(&path)
                        .header(header::COOKIE, &cookie)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = r.status();
            let value = decode(r).await;
            assert_eq!(status, StatusCode::OK, "{path}: {value}");
            let text = value.to_string();
            for secret in [
                "never-display-imap-password",
                "never-display-smtp-password",
                "never-display-webhook-secret-at-least-32",
                &"a".repeat(32),
            ] {
                assert!(!text.contains(secret), "{path} leaked a secret");
            }
            if path.contains("sends?") {
                assert_eq!(value["total"], 1);
                assert_eq!(value["items"][0]["id"], send_id);
            }
            if path.contains("mailboxes?") {
                assert_eq!(value["total"], 1);
                assert_eq!(value["items"][0]["id"], account.id);
            }
        }
        // Empty pages retain the filtered total; SQL-looking input remains data.
        for (path, total, length) in [
            (
                format!("/admin/api/sends?account_id={}&status=accepted", account.id),
                0,
                0,
            ),
            (
                format!("/admin/api/sends?account_id={}&provider=google", account.id),
                0,
                0,
            ),
            (
                format!(
                    "/admin/api/sends?account_id={}&page=2&per_page=1",
                    account.id
                ),
                1,
                0,
            ),
            (
                format!(
                    "/admin/api/sends?account_id={}&from=2099-01-01T00:00:00Z",
                    account.id
                ),
                0,
                0,
            ),
            (
                "/admin/api/sends?account_id=%27%20OR%201%3D1%20--".into(),
                0,
                0,
            ),
            (
                format!(
                    "/admin/api/events?account_id={}&type=account.created",
                    account.id
                ),
                1,
                1,
            ),
            (
                format!(
                    "/admin/api/events?account_id={}&type=%27%20OR%201%3D1%20--",
                    account.id
                ),
                0,
                0,
            ),
            (
                format!(
                    "/admin/api/deliveries?account_id={}&status=delivered",
                    account.id
                ),
                0,
                0,
            ),
            (
                format!(
                    "/admin/api/deliveries?account_id={}&status=pending",
                    account.id
                ),
                1,
                1,
            ),
            (
                format!("/admin/api/mailboxes?q={name}&provider=google"),
                0,
                0,
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(&path)
                        .header(header::COOKIE, &cookie)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let data = decode(response).await;
            assert_eq!(status, StatusCode::OK, "{path}: {data}");
            assert_eq!(data["total"], total, "{path}: {data}");
            assert_eq!(
                data["items"].as_array().unwrap().len(),
                length,
                "{path}: {data}"
            );
        }
        let invalid_date = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/api/sends?from=not-a-date")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid_date.status(), StatusCode::BAD_REQUEST);
        // Resolution records a decision without changing or requeuing submission.
        for (decision, expected) in [
            ("sent", StatusCode::CONFLICT),
            ("unresolved", StatusCode::OK),
            ("unresolved", StatusCode::CONFLICT),
        ] {
            let response = app.clone().oneshot(Request::builder().method("POST")
                .uri(format!("/admin/api/v1/sends/{send_id}/resolve"))
                .header(header::COOKIE,&cookie).header("x-csrf-token",session["csrf_token"].as_str().unwrap())
                .header(header::CONTENT_TYPE,"application/json")
                .body(Body::from(json!({"decision":decision,"reason":"Operator reviewed","evidence":{"ticket":"admin-test"}}).to_string()))
                .unwrap()).await.unwrap();
            let status = response.status();
            let data = decode(response).await;
            assert_eq!(status, expected, "{decision}: {data}");
            let saved = crate::smtp::get(&s.db, &send_id).await.unwrap();
            assert_eq!(saved.status, "unknown");
            assert_eq!(saved.attempts, 0);
        }
        let saved = crate::smtp::get(&s.db, &send_id).await.unwrap();
        assert_eq!(saved.resolution.as_deref(), Some("unresolved"));
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sends WHERE account_id=?")
            .bind(&account.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
        sqlx::query("DELETE FROM events WHERE account_id=?")
            .bind(&account.id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM accounts WHERE id=?")
            .bind(&account.id)
            .execute(&pool)
            .await
            .unwrap();
    }
}
