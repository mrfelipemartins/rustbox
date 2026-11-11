use crate::{
    api::AppState,
    error::{Error, Result},
};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::StreamExt;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use serde_json::Value;
use sqlx::Row;
use std::{
    collections::HashMap,
    sync::OnceLock,
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::sync::{Mutex, Semaphore};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/notifications/microsoft", post(microsoft))
        .route("/notifications/microsoft/lifecycle", post(microsoft))
        .route("/notifications/google", post(google))
        .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024))
}
#[derive(Deserialize)]
pub struct Challenge {
    #[serde(rename = "validationToken")]
    token: Option<String>,
}
async fn microsoft(
    State(s): State<AppState>,
    Query(challenge): Query<Challenge>,
    body: String,
) -> Result<Response> {
    if let Some(token) = challenge.token {
        if token.len() > 4096 {
            return Err(Error::Invalid("invalid validation token".into()));
        }
        return Ok(([(header::CONTENT_TYPE, "text/plain")], token).into_response());
    }
    let body: Value =
        serde_json::from_str(&body).map_err(|_| Error::Invalid("invalid notification".into()))?;
    let values = body
        .get("value")
        .and_then(Value::as_array)
        .filter(|v| v.len() <= 1000)
        .ok_or_else(|| Error::Invalid("invalid notifications".into()))?;
    for item in values {
        let Some(subscription) = item.get("subscriptionId").and_then(Value::as_str) else {
            continue;
        };
        let row = sqlx::query(
            "SELECT account_id,subscription_secret FROM native_state WHERE subscription_id=?",
        )
        .bind(subscription)
        .fetch_optional(&s.db.pool)
        .await?;
        if let Some(row) = row {
            let encrypted: Vec<u8> = row.try_get("subscription_secret")?;
            let secret = s.db.crypto.decrypt(&encrypted)?;
            let supplied = item
                .get("clientState")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !bool::from(secret.as_slice().ct_eq(supplied.as_bytes())) {
                return Err(Error::Unauthorized);
            }
            let account: String = row.try_get("account_id")?;
            if item.get("lifecycleEvent").is_some() {
                sqlx::query("UPDATE native_state SET subscription_expires_at=UTC_TIMESTAMP(6) WHERE account_id=?").bind(&account).execute(&s.db.pool).await?;
            }
            match s.runtime.schedule(&account).await {
                Ok(_) | Err(Error::Conflict(_)) => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(StatusCode::ACCEPTED.into_response())
}
#[derive(Deserialize)]
struct Claims {
    email: String,
    email_verified: bool,
}
#[derive(Deserialize)]
struct Keys {
    keys: Vec<Jwk>,
}
#[derive(Deserialize)]
struct Jwk {
    kid: String,
    n: String,
    e: String,
}
type KeyCache = Option<(Instant, HashMap<String, DecodingKey>)>;
static KEY_REQUESTS: Semaphore = Semaphore::const_new(1);
static KEYS: OnceLock<Mutex<KeyCache>> = OnceLock::new();
async fn verify(s: &AppState, token: &str) -> Result<()> {
    let audience = s
        .config
        .native
        .pubsub_audience
        .as_deref()
        .ok_or(Error::Unauthorized)?;
    let expected = s
        .config
        .native
        .pubsub_email
        .as_deref()
        .ok_or(Error::Unauthorized)?;
    let header = decode_header(token).map_err(|_| Error::Unauthorized)?;
    if header.alg != Algorithm::RS256 {
        return Err(Error::Unauthorized);
    }
    let kid = header.kid.ok_or(Error::Unauthorized)?;
    let keys = KEYS.get_or_init(|| Mutex::new(None));
    let cached = {
        let cache = keys.lock().await;
        cache
            .as_ref()
            .filter(|(at, _)| at.elapsed() < Duration::from_secs(3600))
            .and_then(|(_, keys)| keys.get(&kid))
            .cloned()
    };
    let key = match cached {
        Some(key) => key,
        None => {
            let _permit = KEY_REQUESTS
                .try_acquire()
                .map_err(|_| Error::Unavailable("push key validation busy".into()))?;
            {
                let cache = keys.lock().await;
                if cache
                    .as_ref()
                    .is_some_and(|(at, _)| at.elapsed() < Duration::from_secs(30))
                {
                    return Err(Error::Unauthorized);
                }
            }
            let url = "https://www.googleapis.com/oauth2/v3/certs";
            let client = crate::security::safe_http_client(url, false, None).await?;
            let response = client
                .get(url)
                .send()
                .await
                .map_err(|_| Error::Unavailable("push signing keys unavailable".into()))?;
            if !response.status().is_success() {
                return Err(Error::Unavailable("push signing keys unavailable".into()));
            }
            let mut bytes = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk
                    .map_err(|_| Error::Unavailable("push signing keys unavailable".into()))?;
                if bytes.len() + chunk.len() > 65536 {
                    return Err(Error::Unauthorized);
                }
                bytes.extend_from_slice(&chunk);
            }
            let jwks: Keys = serde_json::from_slice(&bytes).map_err(|_| Error::Unauthorized)?;
            let mut values = HashMap::new();
            for jwk in jwks.keys {
                let key = DecodingKey::from_rsa_components(&jwk.n, &jwk.e)
                    .map_err(|_| Error::Unauthorized)?;
                values.insert(jwk.kid, key);
            }
            let key = values.get(&kid).cloned();
            *keys.lock().await = Some((Instant::now(), values));
            key.ok_or(Error::Unauthorized)?
        }
    };
    verify_claims(token, &key, audience, expected)
}

fn verify_claims(token: &str, key: &DecodingKey, audience: &str, expected: &str) -> Result<()> {
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_audience(&[audience]);
    validation.set_issuer(&["accounts.google.com", "https://accounts.google.com"]);
    let claims = decode::<Claims>(token, key, &validation)
        .map_err(|_| Error::Unauthorized)?
        .claims;
    if !claims.email_verified || claims.email != expected {
        return Err(Error::Unauthorized);
    }
    Ok(())
}

async fn google(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<StatusCode> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|v| v.len() < 16384)
        .ok_or(Error::Unauthorized)?;
    verify(&s, token).await?;
    let encoded = body
        .pointer("/message/data")
        .and_then(Value::as_str)
        .filter(|v| v.len() < 65536)
        .ok_or_else(|| Error::Invalid("invalid push message".into()))?;
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| Error::Invalid("invalid push data".into()))?;
    let data: Value =
        serde_json::from_slice(&bytes).map_err(|_| Error::Invalid("invalid push data".into()))?;
    let mailbox = data
        .get("emailAddress")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Invalid("mailbox missing".into()))?;
    for a in s.db.list_accounts().await? {
        if a.config.backend == crate::db::Backend::Google
            && a.config
                .target_mailbox
                .as_deref()
                .is_some_and(|m| m.eq_ignore_ascii_case(mailbox))
            && a.state != "paused"
        {
            s.runtime.schedule(&a.id).await?;
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use chrono::Utc;
    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde_json::json;

    fn claims() -> Value {
        json!({"iss":"https://accounts.google.com","aud":"https://rustbox.example/notifications/google","email":"push@test.iam.gserviceaccount.com","email_verified":true,"exp":Utc::now().timestamp()+300,"iat":Utc::now().timestamp()})
    }
    fn signed(claims: &Value) -> String {
        encode(
            &Header::new(Algorithm::RS256),
            claims,
            &EncodingKey::from_rsa_pem(include_bytes!("oauth/testdata/key.pem")).unwrap(),
        )
        .unwrap()
    }
    fn check(token: &str) -> Result<()> {
        let key = DecodingKey::from_rsa_pem(include_bytes!("oauth/testdata/public.pem")).unwrap();
        verify_claims(
            token,
            &key,
            "https://rustbox.example/notifications/google",
            "push@test.iam.gserviceaccount.com",
        )
    }
    #[test]
    fn pubsub_identity_requires_verified_signature_audience_issuer_email_and_expiry() {
        assert!(check(&signed(&claims())).is_ok());
        for (field, value) in [
            ("iss", json!("https://attacker.test")),
            ("aud", json!("https://elsewhere.test")),
            ("email", json!("attacker@test")),
            ("email_verified", json!(false)),
            ("exp", json!(Utc::now().timestamp() - 3600)),
        ] {
            let mut invalid = claims();
            invalid[field] = value;
            assert!(
                check(&signed(&invalid)).is_err(),
                "accepted invalid {field}"
            );
        }
        let valid = signed(&claims());
        let mut parts: Vec<_> = valid.split('.').map(str::to_owned).collect();
        let mut forged = claims();
        forged["email"] = json!("another@test");
        parts[1] = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&forged).unwrap());
        assert!(check(&parts.join(".")).is_err());
        let mut missing = claims();
        missing.as_object_mut().unwrap().remove("exp");
        assert!(check(&signed(&missing)).is_err());
    }
}
