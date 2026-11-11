//! Bounded, asynchronous, deliberately allowlisted operational log persistence.
//!
//! Free-form error values are never persisted. Messages are accepted only when
//! they match a reviewed static call-site message; unknown messages become a
//! generic notice. Stdout keeps its normal tracing behavior.
use crate::{
    config::AdminLogConfig,
    db::Db,
    error::{Error, Result},
};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::{MySql, QueryBuilder, Row};
use std::{
    collections::BTreeMap,
    fmt,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{
    Event, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
};
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};

static DROPPED: AtomicU64 = AtomicU64::new(0);
pub fn dropped_records() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}
const SINK_TARGET: &str = "rustbox::admin_logs";
const SAFE_MESSAGES: &[&str] = &[
    "mail service listening",
    "native task failed during shutdown",
    "native sync failed",
    "webhook delivery failed",
    "webhook recovery failed",
    "event retention failed",
    "OAuth retention failed",
    "webhook claim failed",
    "webhook delivery persistence failed",
    "webhook task failed",
    "webhook task did not finish cleanly",
    "account persistence unavailable; retrying",
    "content cache processing failed",
    "native sync scheduling failed",
    "SMTP validation failed",
    "mutation succeeded; synchronization wake deferred",
    "expired Graph draft cleanup deferred",
    "Graph draft cleanup deferred",
    "send processing interrupted; reconciling persisted outcome",
    "Graph upload secret cleanup failed",
    "send artifact cleanup failed",
    "send metadata cleanup failed",
    "send job claim failed",
];

#[derive(Clone, Default)]
struct Fields(BTreeMap<String, Value>);
impl Fields {
    fn text(&mut self, name: &str, value: &str) {
        match name {
            "message" => {
                self.0.insert(
                    name.into(),
                    Value::String(if SAFE_MESSAGES.contains(&value) {
                        value.into()
                    } else {
                        "Operational event (unreviewed details omitted)".into()
                    }),
                );
            }
            "account_id" | "send_id" | "sync_id" | "message_id" => {
                if let Ok(id) = uuid::Uuid::parse_str(value) {
                    self.0.insert(name.into(), Value::String(id.to_string()));
                }
            }
            _ => {} // No free-form provider errors, URLs, subjects, or secrets.
        }
    }
}
impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.text(field.name(), value);
    }
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if matches!(
            field.name(),
            "message" | "account_id" | "send_id" | "sync_id" | "message_id"
        ) {
            self.text(field.name(), &format!("{value:?}"));
        }
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        if matches!(
            field.name(),
            "event_id" | "attempt" | "attempts" | "status" | "count" | "elapsed_ms"
        ) {
            self.0.insert(field.name().into(), json!(value));
        }
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        if value >= 0 {
            self.record_u64(field, value as u64);
        }
    }
}

#[derive(Serialize)]
struct LogRecord {
    observed_at: DateTime<Utc>,
    level: String,
    component: String,
    account_id: Option<String>,
    send_id: Option<String>,
    sync_id: Option<String>,
    message: String,
    fields: Map<String, Value>,
}
pub struct LogLayer {
    sender: mpsc::Sender<LogRecord>,
    max_record_bytes: usize,
}
pub struct LogReceiver(mpsc::Receiver<LogRecord>);
/// Construct before initializing tracing so startup messages can be buffered.
pub fn channel(config: &AdminLogConfig) -> (LogLayer, LogReceiver) {
    let (sender, receiver) = mpsc::channel(1024);
    (
        LogLayer {
            sender,
            max_record_bytes: config.max_record_bytes,
        },
        LogReceiver(receiver),
    )
}
impl<S> Layer<S> for LogLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(fields);
        }
    }
    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id)
            && let Some(fields) = span.extensions_mut().get_mut::<Fields>()
        {
            values.record(fields);
        }
    }
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let metadata = event.metadata();
        // Database instrumentation from the sink cannot recursively enqueue logs.
        if !metadata.target().starts_with("rustbox") || metadata.target() == SINK_TARGET {
            return;
        }
        let mut fields = Fields::default();
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                if let Some(values) = span.extensions().get::<Fields>() {
                    fields.0.extend(values.0.clone());
                }
            }
        }
        event.record(&mut fields);
        let message = fields
            .0
            .remove("message")
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| "Operational event".into());
        let string = |key: &str| fields.0.get(key).and_then(Value::as_str).map(str::to_owned);
        let mut record = LogRecord {
            observed_at: Utc::now(),
            level: metadata.level().as_str().into(),
            component: metadata.target().chars().take(128).collect(),
            account_id: string("account_id"),
            send_id: string("send_id"),
            sync_id: string("sync_id"),
            message,
            fields: fields.0.into_iter().collect(),
        };
        // Trim whole values rather than cutting JSON or UTF-8 midway.
        while serde_json::to_vec(&record).map_or(usize::MAX, |v| v.len()) > self.max_record_bytes
            && !record.fields.is_empty()
        {
            let key = record.fields.keys().next().cloned().unwrap();
            record.fields.remove(&key);
        }
        if serde_json::to_vec(&record).map_or(usize::MAX, |v| v.len()) > self.max_record_bytes {
            record.message = "Operational event (truncated)".into();
            record.component = "rustbox".into();
            record.account_id = None;
            record.send_id = None;
            record.sync_id = None;
        }
        if self.sender.try_send(record).is_err() {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

async fn prune_tx(tx: &mut sqlx::Transaction<'_, MySql>, config: &AdminLogConfig) -> Result<()> {
    let removed: u64 = sqlx::query_scalar("SELECT CAST(COALESCE(SUM(payload_bytes),0) AS UNSIGNED) FROM admin_logs WHERE observed_at < UTC_TIMESTAMP(6) - INTERVAL ? DAY")
        .bind(config.retention_days).fetch_one(&mut **tx).await?;
    sqlx::query("DELETE FROM admin_logs WHERE observed_at < UTC_TIMESTAMP(6) - INTERVAL ? DAY")
        .bind(config.retention_days)
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE admin_log_budget SET used_bytes=GREATEST(CAST(used_bytes AS SIGNED)-?,0) WHERE id=1").bind(removed).execute(&mut **tx).await?;
    let used: u64 =
        sqlx::query_scalar("SELECT used_bytes FROM admin_log_budget WHERE id=1 FOR UPDATE")
            .fetch_one(&mut **tx)
            .await?;
    if used > config.capacity_bytes {
        let mut remaining = used;
        while remaining > config.capacity_bytes {
            let rows =
                sqlx::query("SELECT id,payload_bytes FROM admin_logs ORDER BY id LIMIT 1000")
                    .fetch_all(&mut **tx)
                    .await?;
            if rows.is_empty() {
                remaining = 0;
                break;
            }
            let mut last = 0u64;
            for row in rows {
                last = row.try_get("id")?;
                remaining =
                    remaining.saturating_sub(u64::from(row.try_get::<u32, _>("payload_bytes")?));
                if remaining <= config.capacity_bytes {
                    break;
                }
            }
            sqlx::query("DELETE FROM admin_logs WHERE id<=?")
                .bind(last)
                .execute(&mut **tx)
                .await?;
        }
        sqlx::query("UPDATE admin_log_budget SET used_bytes=? WHERE id=1")
            .bind(remaining)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}
async fn persist(db: &Db, batch: &[LogRecord], config: &AdminLogConfig) -> Result<()> {
    let mut tx = db.pool.begin().await?;
    sqlx::query("SELECT id FROM admin_log_budget WHERE id=1 FOR UPDATE")
        .execute(&mut *tx)
        .await?;
    let mut bytes = 0u64;
    for record in batch {
        let size = serde_json::to_vec(record)
            .map_err(|_| Error::Internal("log serialization failed".into()))?
            .len() as u32;
        sqlx::query("INSERT INTO admin_logs (observed_at,level,component,account_id,send_id,sync_id,message,fields,payload_bytes) VALUES (?,?,?,?,?,?,?,?,?)")
            .bind(record.observed_at.naive_utc()).bind(&record.level).bind(&record.component)
            .bind(&record.account_id).bind(&record.send_id).bind(&record.sync_id).bind(&record.message)
            .bind(sqlx::types::Json(&record.fields)).bind(size).execute(&mut *tx).await?;
        bytes += u64::from(size);
    }
    sqlx::query("UPDATE admin_log_budget SET used_bytes=used_bytes+? WHERE id=1")
        .bind(bytes)
        .execute(&mut *tx)
        .await?;
    prune_tx(&mut tx, config).await?;
    tx.commit().await?;
    Ok(())
}
/// Sink failures are counted, never logged recursively, and cannot stop workers.
pub async fn run(
    db: std::sync::Arc<Db>,
    mut receiver: LogReceiver,
    config: AdminLogConfig,
    shutdown: CancellationToken,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut cleanup = tokio::time::interval(Duration::from_secs(60));
    loop {
        let stopping = tokio::select! {
            _ = shutdown.cancelled() => true,
            _ = tick.tick() => false,
            _ = cleanup.tick() => {
                let _ = tokio::time::timeout(Duration::from_secs(5), persist(&db, &[], &config)).await;
                false
            }
        };
        let mut batch = Vec::with_capacity(128);
        while batch.len() < 128 {
            match receiver.0.try_recv() {
                Ok(row) => batch.push(row),
                Err(_) => break,
            }
        }
        if !batch.is_empty()
            && !matches!(
                tokio::time::timeout(Duration::from_secs(5), persist(&db, &batch, &config)).await,
                Ok(Ok(()))
            )
        {
            DROPPED.fetch_add(batch.len() as u64, Ordering::Relaxed);
        }
        if stopping {
            DROPPED.fetch_add(receiver.0.len() as u64, Ordering::Relaxed);
            break;
        }
    }
}

#[derive(Default, Deserialize)]
pub struct LogQuery {
    pub page: Option<u32>,
    pub per_page: Option<u32>,
    pub level: Option<String>,
    pub component: Option<String>,
    pub account_id: Option<String>,
    pub send_id: Option<String>,
    pub sync_id: Option<String>,
    pub search: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
}
fn filters<'a>(
    builder: &mut QueryBuilder<'a, MySql>,
    q: &'a LogQuery,
    from: Option<NaiveDateTime>,
    to: Option<NaiveDateTime>,
) {
    for (field, value) in [
        ("level", &q.level),
        ("component", &q.component),
        ("account_id", &q.account_id),
        ("send_id", &q.send_id),
        ("sync_id", &q.sync_id),
    ] {
        if let Some(value) = value.as_ref().filter(|v| !v.is_empty()) {
            builder.push(" AND ").push(field).push("=").push_bind(value);
        }
    }
    if let Some(value) = q.search.as_ref().filter(|v| !v.is_empty()) {
        builder
            .push(" AND (message LIKE ")
            .push_bind(format!("%{}%", value.chars().take(256).collect::<String>()))
            .push(" OR CAST(fields AS CHAR) LIKE ")
            .push_bind(format!("%{}%", value.chars().take(256).collect::<String>()))
            .push(")");
    }
    if let Some(from) = from {
        builder.push(" AND observed_at>=").push_bind(from);
    }
    if let Some(to) = to {
        builder.push(" AND observed_at<=").push_bind(to);
    }
}
pub async fn list(db: &Db, q: LogQuery, config: &AdminLogConfig) -> Result<Value> {
    let page = q.page.unwrap_or(1).clamp(1, 100_000);
    let per_page = q.per_page.unwrap_or(50).clamp(1, 200);
    let date = |value: &Option<String>| -> Result<Option<NaiveDateTime>> {
        value
            .as_ref()
            .filter(|v| !v.is_empty())
            .map(|s| {
                DateTime::parse_from_rfc3339(s)
                    .map(|d| d.with_timezone(&Utc).naive_utc())
                    .map_err(|_| Error::Invalid("log date must be RFC3339".into()))
            })
            .transpose()
    };
    let from = date(&q.from)?;
    let to = date(&q.to)?;
    let mut count = QueryBuilder::<MySql>::new("SELECT COUNT(*) FROM admin_logs WHERE 1=1");
    filters(&mut count, &q, from, to);
    let total: i64 = count.build_query_scalar().fetch_one(&db.pool).await?;
    let mut query = QueryBuilder::<MySql>::new(
        "SELECT id,observed_at,level,component,account_id,send_id,sync_id,message,fields FROM admin_logs WHERE 1=1",
    );
    filters(&mut query, &q, from, to);
    query
        .push(" ORDER BY id DESC LIMIT ")
        .push_bind(per_page)
        .push(" OFFSET ")
        .push_bind(u64::from(page - 1) * u64::from(per_page));
    let rows = query.build().fetch_all(&db.pool).await?;
    let items = rows.into_iter().map(|r| -> Result<Value> { Ok(json!({
        "id":r.try_get::<u64,_>("id")?, "observed_at":r.try_get::<NaiveDateTime,_>("observed_at")?.and_utc(),
        "level":r.try_get::<String,_>("level")?, "component":r.try_get::<String,_>("component")?,
        "account_id":r.try_get::<Option<String>,_>("account_id")?, "send_id":r.try_get::<Option<String>,_>("send_id")?,
        "sync_id":r.try_get::<Option<String>,_>("sync_id")?, "message":r.try_get::<String,_>("message")?,
        "fields":r.try_get::<sqlx::types::Json<Value>,_>("fields")?.0
    })) }).collect::<Result<Vec<_>>>()?;
    let used: u64 = sqlx::query_scalar("SELECT used_bytes FROM admin_log_budget WHERE id=1")
        .fetch_one(&db.pool)
        .await?;
    let oldest: Option<NaiveDateTime> =
        sqlx::query_scalar("SELECT MIN(observed_at) FROM admin_logs")
            .fetch_one(&db.pool)
            .await?;
    Ok(
        json!({ "items":items, "page":page, "per_page":per_page, "total":total,
        "dropped_records":dropped_records(), "retention_days":config.retention_days,
        "capacity_bytes":config.capacity_bytes, "used_bytes":used, "oldest_available_at":oldest.map(|d|d.and_utc()) }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::prelude::*;
    #[test]
    fn rejects_free_form_secrets_and_preserves_safe_correlation() {
        let config = AdminLogConfig::default();
        let (layer, mut rx) = channel(&config);
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(account_id=%"550e8400-e29b-41d4-a716-446655440000", error="Bearer SECRET", password="password", "native sync failed");
            tracing::warn!("provider returned password=SECRET body contents");
            tracing::warn!(
                account_id = "not-a-uuid SECRET",
                token = "secret",
                "webhook delivery failed"
            );
        });
        let row = rx.0.try_recv().unwrap();
        let rendered = serde_json::to_string(&row).unwrap();
        assert!(!rendered.contains("SECRET"));
        assert!(!rendered.contains("Bearer"));
        assert_eq!(
            row.account_id.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        assert_eq!(row.message, "native sync failed");
        assert!(
            !serde_json::to_string(&rx.0.try_recv().unwrap())
                .unwrap()
                .contains("SECRET")
        );
        assert!(rx.0.try_recv().unwrap().account_id.is_none());
    }
    #[test]
    fn bounds_records_and_does_not_capture_sink_or_dependency_events() {
        let (layer, mut rx) = channel(&AdminLogConfig {
            max_record_bytes: 256,
            ..Default::default()
        });
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            tracing::warn!(target:"rustbox::admin_logs", "webhook delivery failed");
            tracing::warn!(target:"sqlx", "query failed password=secret");
            tracing::info!(account_id=%uuid::Uuid::new_v4(),send_id=%uuid::Uuid::new_v4(),sync_id=%uuid::Uuid::new_v4(), "send processing interrupted; reconciling persisted outcome");
        });
        let record = rx.0.try_recv().unwrap();
        assert!(serde_json::to_vec(&record).unwrap().len() <= 256);
        assert!(rx.0.try_recv().is_err());
    }
    #[test]
    fn full_buffer_drops_without_waiting_for_database() {
        let before = dropped_records();
        let (layer, _receiver) = channel(&AdminLogConfig::default());
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            for _ in 0..2048 {
                tracing::warn!("native sync failed");
            }
        });
        assert!(dropped_records() >= before + 1024);
    }

    #[tokio::test]
    #[ignore = "requires a dedicated MySQL database in TEST_DATABASE_URL"]
    async fn persistent_retained_bounded_and_filtered_logs() -> anyhow::Result<()> {
        let url = std::env::var("TEST_DATABASE_URL")?;
        let pool = sqlx::mysql::MySqlPoolOptions::new().connect(&url).await?;
        sqlx::migrate!().run(&pool).await?;
        let db = Db::new(pool, crate::crypto::Crypto::new(&[42; 32])?);
        sqlx::query("DELETE FROM admin_logs")
            .execute(&db.pool)
            .await?;
        sqlx::query("UPDATE admin_log_budget SET used_bytes=0 WHERE id=1")
            .execute(&db.pool)
            .await?;
        let account_id = uuid::Uuid::new_v4().to_string();
        let record = |age: i64, level: &str| LogRecord {
            observed_at: Utc::now() - chrono::Duration::days(age),
            level: level.into(),
            component: "rustbox::native".into(),
            account_id: Some(account_id.clone()),
            send_id: None,
            sync_id: None,
            message: "native sync failed".into(),
            fields: Map::new(),
        };
        let config = AdminLogConfig::default();
        persist(
            &db,
            &[record(8, "WARN"), record(0, "INFO"), record(0, "WARN")],
            &config,
        )
        .await?;
        let page = list(
            &db,
            LogQuery {
                level: Some("WARN".into()),
                account_id: Some(account_id.clone()),
                ..Default::default()
            },
            &config,
        )
        .await?;
        assert_eq!(page["total"], 1);
        assert_eq!(page["items"][0]["account_id"], account_id);
        let page = list(&db, LogQuery::default(), &config).await?;
        assert_eq!(page["total"], 2);
        let measured: u64 = sqlx::query_scalar(
            "SELECT CAST(COALESCE(SUM(payload_bytes),0) AS UNSIGNED) FROM admin_logs",
        )
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(page["used_bytes"], measured);
        // A new receiver/runtime reads committed logs without an in-memory history.
        db.pool.close().await;
        let pool = sqlx::mysql::MySqlPoolOptions::new().connect(&url).await?;
        let db = Db::new(pool, crate::crypto::Crypto::new(&[42; 32])?);
        let restored = list(
            &db,
            LogQuery {
                per_page: Some(1),
                ..Default::default()
            },
            &config,
        )
        .await?;
        assert_eq!(restored["total"], 2);
        assert_eq!(restored["items"].as_array().unwrap().len(), 1);
        let capacity = serde_json::to_vec(&record(0, "WARN"))?.len() as u64 + 8;
        let bounded = AdminLogConfig {
            capacity_bytes: capacity,
            ..config
        };
        persist(&db, &[record(0, "WARN"), record(0, "WARN")], &bounded).await?;
        let page = list(&db, LogQuery::default(), &bounded).await?;
        assert_eq!(page["total"], 1);
        assert!(page["used_bytes"].as_u64().unwrap() <= capacity);
        assert!(page["oldest_available_at"].is_string());
        let matching = list(
            &db,
            LogQuery {
                search: Some("sync failed".into()),
                from: Some((Utc::now() - chrono::Duration::minutes(1)).to_rfc3339()),
                ..Default::default()
            },
            &bounded,
        )
        .await?;
        assert_eq!(matching["total"], 1);
        assert!(
            list(
                &db,
                LogQuery {
                    from: Some("bad date".into()),
                    ..Default::default()
                },
                &bounded
            )
            .await
            .is_err()
        );
        sqlx::query("DELETE FROM admin_logs")
            .execute(&db.pool)
            .await?;
        sqlx::query("UPDATE admin_log_budget SET used_bytes=0 WHERE id=1")
            .execute(&db.pool)
            .await?;
        Ok(())
    }
}
