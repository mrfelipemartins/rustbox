use crate::{
    config::Config,
    db::Db,
    error::{Error, Result},
    security,
};
use chrono::{DateTime, NaiveDateTime, Utc};
use hmac::{Hmac, Mac};
use serde::Serialize;
use serde_json::Value;
use sha2::Sha256;
use sqlx::Row;
use std::{sync::Arc, time::Duration};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Serialize)]
pub struct Event {
    pub id: u64,
    pub schema_version: u32,
    pub account_id: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub observed_at: DateTime<Utc>,
    pub data: Value,
}

#[derive(Serialize)]
pub struct EventPage {
    pub events: Vec<Event>,
    pub next_cursor: u64,
}

fn decode_event(row: &sqlx::mysql::MySqlRow) -> Result<Event> {
    Ok(Event {
        id: row.try_get("id")?,
        schema_version: row.try_get("schema_version")?,
        account_id: row.try_get("account_id")?,
        event_type: row.try_get("event_type")?,
        observed_at: row.try_get::<NaiveDateTime, _>("observed_at")?.and_utc(),
        data: row.try_get::<sqlx::types::Json<Value>, _>("payload")?.0,
    })
}

pub async fn list(db: &Db, after: u64, limit: u32) -> Result<EventPage> {
    let mut tx = db.pool.begin().await?;
    let row = sqlx::query("SELECT last_id,pruned_through FROM event_sequence WHERE id=1")
        .fetch_one(&mut *tx)
        .await?;
    let pruned: u64 = row.try_get("pruned_through")?;
    let last: u64 = row.try_get("last_id")?;
    if after < pruned {
        return Err(Error::CursorExpired {
            minimum_cursor: pruned,
            current_cursor: last,
        });
    }
    if after > last {
        return Err(Error::Invalid(
            "event cursor exceeds current sequence".into(),
        ));
    }
    let rows = sqlx::query("SELECT id,schema_version,account_id,event_type,observed_at,payload FROM events WHERE id>? ORDER BY id LIMIT ?").bind(after).bind(limit.clamp(1,1000)).fetch_all(&mut *tx).await?;
    let events: Vec<Event> = rows.iter().map(decode_event).collect::<Result<_>>()?;
    let next_cursor = events.last().map_or(after, |event| event.id);
    tx.commit().await?;
    Ok(EventPage {
        events,
        next_cursor,
    })
}

struct Delivery {
    event: Event,
    url: String,
    secret: Vec<u8>,
    attempts: u32,
}

async fn claim(db: &Db) -> Result<Option<Delivery>> {
    let mut tx = db.pool.begin().await?;
    let row = sqlx::query("SELECT * FROM events WHERE delivery_state='pending' AND next_attempt_at<=UTC_TIMESTAMP(6) ORDER BY next_attempt_at,id LIMIT 1 FOR UPDATE SKIP LOCKED").fetch_optional(&mut *tx).await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let event = decode_event(&row)?;
    let url: String = row.try_get("webhook_url")?;
    let encrypted: Vec<u8> = row.try_get("webhook_secret")?;
    let secret = db.crypto.decrypt(&encrypted)?;
    let attempts: u32 = row.try_get("attempt_count")?;
    sqlx::query("UPDATE events SET delivery_state='delivering',claimed_at=UTC_TIMESTAMP(6),attempt_count=attempt_count+1 WHERE id=?").bind(event.id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(Delivery {
        event,
        url,
        secret,
        attempts: attempts.saturating_add(1),
    }))
}

fn signature(secret: &[u8], timestamp: i64, body: &[u8]) -> Result<String> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret)
        .map_err(|_| Error::Internal("invalid webhook signing key".into()))?;
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    Ok(format!(
        "t={timestamp},v1={}",
        hex::encode(mac.finalize().into_bytes())
    ))
}

async fn deliver(db: Db, delivery: Delivery, config: Arc<Config>) -> Result<()> {
    let id = delivery.event.id;
    let deadline = delivery.event.observed_at
        + chrono::Duration::hours(i64::from(config.webhooks.retry_window_hours));
    if Utc::now() >= deadline {
        sqlx::query("UPDATE events SET delivery_state='expired' WHERE id=?")
            .bind(id)
            .execute(&db.pool)
            .await?;
        return Ok(());
    }
    let body = serde_json::to_vec(&delivery.event)
        .map_err(|_| Error::Internal("event serialization failed".into()))?;
    let timestamp = Utc::now().timestamp();
    let signature = signature(&delivery.secret, timestamp, &body)?;
    let status = match security::safe_http_client(
        &delivery.url,
        config.allow_private_networks,
        config.tls_ca_file.as_deref(),
    )
    .await
    {
        Ok(client) => client
            .post(&delivery.url)
            .timeout(Duration::from_secs(config.webhooks.timeout_secs))
            .header("content-type", "application/json")
            .header("x-rustbox-event-id", id.to_string())
            .header("x-rustbox-signature", signature)
            .body(body)
            .send()
            .await
            .ok()
            .map(|response| response.status()),
        Err(_) => None,
    };
    if status.is_some_and(|status| status.is_success()) {
        sqlx::query("UPDATE events SET delivery_state='delivered',delivered_at=UTC_TIMESTAMP(6),last_status=? WHERE id=?").bind(status.map(|status|status.as_u16())).bind(id).execute(&db.pool).await?;
    } else {
        let seconds = retry_delay(delivery.attempts);
        let next = (Utc::now() + chrono::Duration::seconds(seconds)).min(deadline);
        sqlx::query(
            "UPDATE events SET delivery_state='pending',last_status=?,next_attempt_at=? WHERE id=?",
        )
        .bind(status.map(|status| status.as_u16()))
        .bind(next.naive_utc())
        .bind(id)
        .execute(&db.pool)
        .await?;
        tracing::warn!(
            event_id = id,
            attempt = delivery.attempts,
            "webhook delivery failed"
        );
    }
    Ok(())
}

fn retry_delay(attempt: u32) -> i64 {
    let base = 5_i64
        .saturating_mul(2_i64.saturating_pow(attempt.min(10)))
        .min(3600);
    base + i64::from(rand::random::<u8>()) % (base / 4 + 1)
}

pub async fn prune(db: &Db) -> Result<()> {
    prune_retained(db, 7).await
}

async fn prune_retained(db: &Db, retention_days: u32) -> Result<()> {
    for _ in 0..100 {
        let mut tx = db.pool.begin().await?;
        sqlx::query("SELECT last_id FROM event_sequence WHERE id=1 FOR UPDATE")
            .fetch_one(&mut *tx)
            .await?;
        let threshold = Utc::now() - chrono::Duration::days(i64::from(retention_days));
        let ids: Vec<u64> =
            sqlx::query_scalar("SELECT id FROM events WHERE observed_at<? ORDER BY id LIMIT 1000")
                .bind(threshold.naive_utc())
                .fetch_all(&mut *tx)
                .await?;
        let Some(max) = ids.last().copied() else {
            return Ok(());
        };
        sqlx::query(
            "UPDATE event_sequence SET pruned_through=GREATEST(pruned_through,?) WHERE id=1",
        )
        .bind(max)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM events WHERE id<=?")
            .bind(max)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        if ids.len() < 1000 {
            break;
        }
        tokio::task::yield_now().await;
    }
    Ok(())
}

pub async fn run(db: Db, config: Arc<Config>, shutdown: CancellationToken) -> Result<()> {
    let mut tasks = JoinSet::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut recovery = tokio::time::interval(Duration::from_secs(60));
    let mut housekeeping = tokio::time::interval(Duration::from_secs(3600));
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = recovery.tick() => {
                if let Err(error) = sqlx::query("UPDATE events SET delivery_state='pending' WHERE delivery_state='delivering' AND claimed_at<UTC_TIMESTAMP(6)-INTERVAL ? SECOND").bind(config.webhooks.timeout_secs.saturating_add(30).max(60)).execute(&db.pool).await { tracing::error!(%error,"webhook recovery failed"); }
            },
            _ = housekeeping.tick() => {
                if let Err(error) = prune_retained(&db, config.webhooks.retention_days).await { tracing::error!(%error,"event retention failed"); }
                if let Err(error) = crate::oauth::store::cleanup(&db).await { tracing::error!(%error,"OAuth retention failed"); }
            },
            _ = tick.tick() => {
                while tasks.len() < config.webhook_concurrency {
                    match claim(&db).await {
                        Ok(Some(delivery)) => {
                            let db = db.clone();
                            let config = config.clone();
                            tasks.spawn(async move {deliver(db,delivery,config).await});
                        },
                        Ok(None) => break,
                        Err(error) => { tracing::error!(%error,"webhook claim failed"); break; },
                    }
                }
            },
            completed = tasks.join_next(), if !tasks.is_empty() => {
                match completed {
                    Some(Ok(Err(error))) => tracing::error!(%error,"webhook delivery persistence failed"),
                    Some(Err(error)) => tracing::error!(%error,"webhook task failed"),
                    _ => {},
                }
            },
        }
    }
    while let Some(result) = tasks.join_next().await {
        if !matches!(result, Ok(Ok(()))) {
            tracing::warn!("webhook task did not finish cleanly");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_covers_timestamp_and_exact_body() {
        let secret = b"01234567890123456789012345678901";
        let a = signature(secret, 100, b"{}").unwrap();
        assert_eq!(a, signature(secret, 100, b"{}").unwrap());
        assert_ne!(a, signature(secret, 101, b"{}").unwrap());
        assert_ne!(a, signature(secret, 100, b"{ }").unwrap());
        assert!(a.starts_with("t=100,v1="));
    }

    #[test]
    fn retries_are_bounded() {
        assert!((10..=12).contains(&retry_delay(1)));
        assert!((3600..=4500).contains(&retry_delay(100)));
    }
}
