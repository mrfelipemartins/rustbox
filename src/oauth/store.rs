use crate::{
    db::Db,
    error::{Error, Result},
};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{MySql, Row, Transaction};
use uuid::Uuid;

#[derive(Clone)]
pub struct Credential {
    pub id: String,
    pub client_id: String,
    pub refresh_token: Option<String>,
    pub subject: Option<String>,
    pub revision: u64,
    pub health: String,
    pub last_error: Option<String>,
}

impl Credential {
    pub fn mode(&self) -> Result<super::config::Mode> {
        match (&self.refresh_token, &self.subject) {
            (Some(_), None) => Ok(super::config::Mode::Delegated),
            (None, Some(_)) => Ok(super::config::Mode::DomainWideDelegation),
            (None, None) => Ok(super::config::Mode::Application),
            _ => Err(Error::Internal(
                "invalid persisted OAuth credential type".into(),
            )),
        }
    }

    pub fn public(&self) -> Value {
        json!({"id":self.id,"client_id":self.client_id,"subject":self.subject,
            "revision":self.revision,"credential_type":self.mode().ok(),"health":self.health,"last_error":self.last_error,"required_action":required_action(&self.health)})
    }
}

#[derive(Serialize, Deserialize)]
struct Secret {
    refresh_token: Option<String>,
    subject: Option<String>,
}

fn encrypted_secret(
    db: &Db,
    refresh_token: Option<&str>,
    subject: Option<&str>,
) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(&Secret {
        refresh_token: refresh_token.map(str::to_owned),
        subject: subject.map(str::to_owned),
    })
    .map_err(|_| Error::Internal("OAuth credential serialization failed".into()))?;
    db.crypto.encrypt(&bytes)
}

pub async fn get(db: &Db, id: &str) -> Result<Credential> {
    let row = sqlx::query("SELECT * FROM oauth_credentials WHERE id=?")
        .bind(id)
        .fetch_optional(&db.pool)
        .await?
        .ok_or(Error::NotFound)?;
    let bytes: Vec<u8> = row.try_get("secret")?;
    let secret: Secret = serde_json::from_slice(&db.crypto.decrypt(&bytes)?)
        .map_err(|_| Error::Internal("invalid OAuth credential".into()))?;
    Ok(Credential {
        id: row.try_get("id")?,
        client_id: String::from_utf8(row.try_get("client_id")?)
            .map_err(|_| Error::Internal("invalid OAuth registration ID".into()))?,
        refresh_token: secret.refresh_token,
        subject: secret.subject,
        revision: row.try_get("revision")?,
        health: row.try_get("health")?,
        last_error: row.try_get("last_error")?,
    })
}

pub async fn create(
    db: &Db,
    client_id: &str,
    refresh_token: Option<&str>,
    subject: Option<&str>,
) -> Result<Credential> {
    let id = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO oauth_credentials (id,client_id,secret) VALUES (?,?,?)")
        .bind(&id)
        .bind(client_id)
        .bind(encrypted_secret(db, refresh_token, subject)?)
        .execute(&db.pool)
        .await?;
    get(db, &id).await
}

pub async fn replace(
    db: &Db,
    id: &str,
    revision: u64,
    refresh_token: Option<&str>,
    subject: Option<&str>,
) -> Result<Credential> {
    let mut tx = db.pool.begin().await?;
    let old: Option<(u64, String)> =
        sqlx::query_as("SELECT revision,health FROM oauth_credentials WHERE id=? FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let (current, health) = old.ok_or(Error::NotFound)?;
    if current != revision {
        return Err(Error::Conflict(
            "OAuth credential changed; retry authorization".into(),
        ));
    }
    sqlx::query("UPDATE oauth_credentials SET secret=?,revision=revision+1,health='healthy',last_error=NULL,updated_at=UTC_TIMESTAMP(6) WHERE id=?")
        .bind(encrypted_secret(db,refresh_token,subject)?).bind(id).execute(&mut *tx).await?;
    if health != "healthy" {
        emit_health(db, &mut tx, id, &health, "healthy", "credentials_replaced").await?;
    }
    resume_persisted_accounts(db, &mut tx, id).await?;
    tx.commit().await?;
    get(db, id).await
}

pub async fn save_refresh(
    db: &Db,
    id: &str,
    revision: u64,
    refresh_token: Option<&str>,
) -> Result<bool> {
    let credential = get(db, id).await?;
    if credential.revision != revision {
        return Ok(false);
    }
    let encrypted = encrypted_secret(
        db,
        refresh_token.or(credential.refresh_token.as_deref()),
        credential.subject.as_deref(),
    )?;
    Ok(sqlx::query("UPDATE oauth_credentials SET secret=?,updated_at=UTC_TIMESTAMP(6) WHERE id=? AND revision=?")
        .bind(encrypted).bind(id).bind(revision).execute(&db.pool).await?.rows_affected() == 1)
}

pub async fn set_health(
    db: &Db,
    id: &str,
    revision: u64,
    health: &str,
    reason: &str,
) -> Result<()> {
    if !matches!(health, "healthy" | "reauthorization_required" | "failed") {
        return Err(Error::Internal("invalid OAuth health state".into()));
    }
    let mut tx = db.pool.begin().await?;
    let old: Option<(u64, String, Option<String>)> = sqlx::query_as(
        "SELECT revision,health,last_error FROM oauth_credentials WHERE id=? FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((current, previous, last_error)) = old else {
        return Ok(());
    };
    let desired_error = if health == "healthy" {
        None
    } else {
        Some(reason)
    };
    if current != revision || (previous == health && last_error.as_deref() == desired_error) {
        return Ok(());
    }
    sqlx::query(
        "UPDATE oauth_credentials SET health=?,last_error=?,updated_at=UTC_TIMESTAMP(6) WHERE id=?",
    )
    .bind(health)
    .bind(desired_error)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    if previous != health {
        emit_health(db, &mut tx, id, &previous, health, reason).await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn emit_health(
    db: &Db,
    tx: &mut Transaction<'_, MySql>,
    id: &str,
    previous: &str,
    health: &str,
    reason: &str,
) -> Result<()> {
    let accounts: Vec<String> = sqlx::query_scalar("SELECT DISTINCT account_id FROM oauth_account_credentials WHERE credential_id=? ORDER BY account_id")
        .bind(id).fetch_all(&mut **tx).await?;
    let event = match health {
        "healthy" => "account.authorization_restored",
        "reauthorization_required" => "account.reauthorization_required",
        _ => "account.authorization_failed",
    };
    for account in accounts {
        db.emit(tx,&account,event,json!({"credential_id":id,"previous_state":previous,"state":health,"reason":reason,"required_action":required_action(health)})).await?;
    }
    Ok(())
}

pub async fn affected_accounts(db: &Db, id: &str) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar("SELECT DISTINCT account_id FROM oauth_account_credentials WHERE credential_id=? ORDER BY account_id")
        .bind(id).fetch_all(&db.pool).await?)
}

pub async fn delete(db: &Db, id: &str) -> Result<()> {
    let result = sqlx::query("DELETE FROM oauth_credentials WHERE id=?")
        .bind(id)
        .execute(&db.pool)
        .await;
    match result {
        Ok(done) if done.rows_affected() == 0 => Err(Error::NotFound),
        Ok(_) => Ok(()),
        Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("1451") => Err(
            Error::Conflict("OAuth credential is used by an account".into()),
        ),
        Err(error) => Err(error.into()),
    }
}

#[derive(Serialize, Deserialize)]
pub struct AuthorizationSecret {
    pub client_id: String,
    pub redirect_uri: String,
    pub state: String,
    pub verifier: String,
    pub target_id: Option<String>,
    pub target_revision: Option<u64>,
}

pub struct Authorization {
    pub id: String,
    pub state: String,
    pub credential_id: Option<String>,
    pub expires_at: DateTime<Utc>,
    pub secret: Option<AuthorizationSecret>,
}

impl Authorization {
    pub fn public(&self) -> Value {
        let state = if self.state == "pending" && self.expires_at <= Utc::now() {
            "expired"
        } else {
            &self.state
        };
        json!({"id":self.id,"state":state,"credential_id":self.credential_id,"expires_at":self.expires_at})
    }
}

pub async fn authorization_create(
    db: &Db,
    app: &str,
    secret: &AuthorizationSecret,
) -> Result<Authorization> {
    cleanup(db).await?;
    let id = Uuid::new_v4().to_string();
    let bytes = serde_json::to_vec(secret)
        .map_err(|_| Error::Internal("OAuth session serialization failed".into()))?;
    sqlx::query("INSERT INTO oauth_authorizations (id,app_id,secret,expires_at) VALUES (?,?,?,UTC_TIMESTAMP(6)+INTERVAL 10 MINUTE)")
        .bind(&id).bind(app).bind(db.crypto.encrypt(&bytes)?).execute(&db.pool).await?;
    authorization_get(db, &id, app).await
}

pub async fn authorization_get(db: &Db, id: &str, app: &str) -> Result<Authorization> {
    sqlx::query("UPDATE oauth_authorizations SET state=IF(state='exchanging','uncertain','expired'),secret=NULL WHERE id=? AND app_id=? AND state IN ('pending','exchanging') AND expires_at<=UTC_TIMESTAMP(6)")
        .bind(id).bind(app).execute(&db.pool).await?;
    let row = sqlx::query("SELECT * FROM oauth_authorizations WHERE id=? AND app_id=?")
        .bind(id)
        .bind(app)
        .fetch_optional(&db.pool)
        .await?
        .ok_or(Error::NotFound)?;
    let bytes: Option<Vec<u8>> = row.try_get("secret")?;
    let secret = bytes
        .map(|bytes| {
            serde_json::from_slice(&db.crypto.decrypt(&bytes)?)
                .map_err(|_| Error::Internal("invalid OAuth session".into()))
        })
        .transpose()?;
    Ok(Authorization {
        id: row.try_get("id")?,
        state: row.try_get("state")?,
        credential_id: row.try_get("credential_id")?,
        expires_at: row.try_get::<NaiveDateTime, _>("expires_at")?.and_utc(),
        secret,
    })
}

pub async fn authorization_claim(db: &Db, id: &str, app: &str) -> Result<()> {
    let done = sqlx::query("UPDATE oauth_authorizations SET state='exchanging' WHERE id=? AND app_id=? AND state='pending' AND expires_at>UTC_TIMESTAMP(6)")
        .bind(id).bind(app).execute(&db.pool).await?;
    if done.rows_affected() != 1 {
        return Err(Error::Conflict(
            "OAuth authorization is expired or already consumed".into(),
        ));
    }
    Ok(())
}

pub async fn authorization_finish(
    db: &Db,
    id: &str,
    state: &str,
    credential_id: Option<&str>,
) -> Result<()> {
    sqlx::query("UPDATE oauth_authorizations SET state=?,credential_id=?,secret=NULL WHERE id=? AND state='exchanging'")
        .bind(state).bind(credential_id).bind(id).execute(&db.pool).await?;
    Ok(())
}

pub async fn authorization_cancel(db: &Db, id: &str, app: &str) -> Result<()> {
    let changed = sqlx::query("UPDATE oauth_authorizations SET state='cancelled',secret=NULL WHERE id=? AND app_id=? AND state='pending'")
        .bind(id).bind(app).execute(&db.pool).await?;
    if changed.rows_affected() == 0 {
        return Err(Error::Conflict(
            "OAuth authorization cannot be cancelled".into(),
        ));
    }
    Ok(())
}

pub async fn recover(db: &Db) -> Result<()> {
    sqlx::query(
        "UPDATE oauth_authorizations SET state='uncertain',secret=NULL WHERE state='exchanging'",
    )
    .execute(&db.pool)
    .await?;
    Ok(())
}

pub async fn authorization_commit(
    db: &Db,
    session_id: &str,
    secret: &AuthorizationSecret,
    refresh_token: &str,
) -> Result<Credential> {
    let mut tx = db.pool.begin().await?;
    let state: Option<String> =
        sqlx::query_scalar("SELECT state FROM oauth_authorizations WHERE id=? FOR UPDATE")
            .bind(session_id)
            .fetch_optional(&mut *tx)
            .await?;
    if state.as_deref() != Some("exchanging") {
        return Err(Error::Conflict(
            "OAuth authorization is no longer exchanging".into(),
        ));
    }
    let encrypted = encrypted_secret(db, Some(refresh_token), None)?;
    let credential_id = if let (Some(id), Some(revision)) =
        (&secret.target_id, secret.target_revision)
    {
        let current: Option<(u64, String)> =
            sqlx::query_as("SELECT revision,health FROM oauth_credentials WHERE id=? FOR UPDATE")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        let (current, health) = current.ok_or(Error::NotFound)?;
        if current != revision {
            return Err(Error::Conflict(
                "OAuth credential changed; begin authorization again".into(),
            ));
        }
        sqlx::query("UPDATE oauth_credentials SET secret=?,revision=revision+1,health='healthy',last_error=NULL,updated_at=UTC_TIMESTAMP(6) WHERE id=?")
            .bind(encrypted).bind(id).execute(&mut *tx).await?;
        if health != "healthy" {
            emit_health(db, &mut tx, id, &health, "healthy", "credentials_replaced").await?;
        }
        resume_persisted_accounts(db, &mut tx, id).await?;
        id.clone()
    } else {
        let id = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO oauth_credentials (id,client_id,secret) VALUES (?,?,?)")
            .bind(&id)
            .bind(&secret.client_id)
            .bind(encrypted)
            .execute(&mut *tx)
            .await?;
        id
    };
    sqlx::query(
        "UPDATE oauth_authorizations SET state='completed',credential_id=?,secret=NULL WHERE id=?",
    )
    .bind(&credential_id)
    .bind(session_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    get(db, &credential_id).await
}

pub fn required_action(health: &str) -> Option<&'static str> {
    match health {
        "reauthorization_required" => Some("reauthorize"),
        "failed" => Some("check_provider_configuration"),
        _ => None,
    }
}

pub async fn cleanup(db: &Db) -> Result<()> {
    sqlx::query("UPDATE oauth_authorizations SET state=IF(state='exchanging','uncertain','expired'),secret=NULL WHERE state IN ('pending','exchanging') AND expires_at<=UTC_TIMESTAMP(6)")
        .execute(&db.pool).await?;
    sqlx::query(
        "DELETE FROM oauth_authorizations WHERE expires_at < UTC_TIMESTAMP(6) - INTERVAL 1 DAY",
    )
    .execute(&db.pool)
    .await?;
    Ok(())
}

async fn resume_persisted_accounts(
    db: &Db,
    tx: &mut Transaction<'_, MySql>,
    credential_id: &str,
) -> Result<()> {
    let ids: Vec<String> = sqlx::query_scalar("SELECT DISTINCT account_id FROM oauth_account_credentials WHERE credential_id=? ORDER BY account_id")
        .bind(credential_id).fetch_all(&mut **tx).await?;
    for id in ids {
        let changed = sqlx::query("UPDATE accounts SET state='disconnected',last_error=NULL,last_failure_event=NULL,next_retry_at=NULL,retry_count=0,updated_at=UTC_TIMESTAMP(6) WHERE id=? AND state='auth_failed'")
            .bind(&id).execute(&mut **tx).await?;
        if changed.rows_affected() > 0 {
            db.emit(tx,&id,"account.resumed",json!({"previous_state":"auth_failed","state":"disconnected","reason":"credentials_replaced"})).await?;
        }
    }
    Ok(())
}
