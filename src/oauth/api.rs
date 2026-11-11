use super::{
    config::Mode,
    store::{self, AuthorizationSecret},
};
use crate::{
    api::{AppId, AppState},
    error::{Error, Result},
};
use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/oauth/clients", get(clients))
        .route("/v1/oauth/authorizations", post(start))
        .route("/v1/oauth/authorizations/{id}", get(status).delete(cancel))
        .route("/v1/oauth/authorizations/{id}/complete", post(complete))
        .route("/v1/oauth/credentials", post(create))
        .route(
            "/v1/oauth/credentials/{id}",
            get(credential).put(replace).delete(delete),
        )
}

async fn clients(State(s): State<AppState>) -> Json<Value> {
    Json(
        json!({"clients":s.config.oauth.providers.iter().map(|(id,p)|json!({
        "id":id,"provider":p.provider,"mode":p.mode,"shared":p.shared,"redirect_uris":p.redirect_uris
    })).collect::<Vec<_>>()}),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    client_id: String,
    redirect_uri: String,
    credential_id: Option<String>,
    login_hint: Option<String>,
}

fn random_secret() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

async fn start(
    State(s): State<AppState>,
    Extension(app): Extension<AppId>,
    Json(input): Json<Start>,
) -> Result<(StatusCode, Json<Value>)> {
    let p = s.oauth.registration(&input.client_id)?;
    if p.mode != Mode::Delegated || !p.redirect_uris.contains(&input.redirect_uri) {
        return Err(Error::Invalid(
            "OAuth client or redirect URI does not support this authorization".into(),
        ));
    }
    if input
        .login_hint
        .as_ref()
        .is_some_and(|v| v.len() > 320 || v.contains(['\r', '\n', '\0']))
    {
        return Err(Error::Invalid("invalid login hint".into()));
    }
    let target = match &input.credential_id {
        Some(id) => {
            let c = store::get(&s.db, id).await?;
            if c.client_id != input.client_id {
                return Err(Error::Invalid(
                    "reauthorization must use the same OAuth registration".into(),
                ));
            }
            Some(c)
        }
        None => None,
    };
    let secret = AuthorizationSecret {
        client_id: input.client_id,
        redirect_uri: input.redirect_uri,
        state: random_secret(),
        verifier: random_secret(),
        target_id: target.as_ref().map(|v| v.id.clone()),
        target_revision: target.map(|v| v.revision),
    };
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(secret.verifier.as_bytes()));
    let url = s.oauth.authorization_url(
        &secret.client_id,
        &secret.redirect_uri,
        &secret.state,
        &challenge,
        input.login_hint.as_deref(),
    )?;
    let session = store::authorization_create(&s.db, &app.0, &secret).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"id":session.id,"authorization_url":url,"expires_at":session.expires_at})),
    ))
}

async fn status(
    State(s): State<AppState>,
    Extension(app): Extension<AppId>,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    Ok(Json(
        store::authorization_get(&s.db, &id, &app.0).await?.public(),
    ))
}

async fn cancel(
    State(s): State<AppState>,
    Extension(app): Extension<AppId>,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    store::authorization_get(&s.db, &id, &app.0).await?;
    store::authorization_cancel(&s.db, &id, &app.0).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Complete {
    state: String,
    code: Option<String>,
    error: Option<String>,
}

async fn complete(
    State(s): State<AppState>,
    Extension(app): Extension<AppId>,
    Path(id): Path<String>,
    Json(input): Json<Complete>,
) -> Result<Json<Value>> {
    let session = store::authorization_get(&s.db, &id, &app.0).await?;
    if session.state == "completed" {
        if let Some(credential_id) = &session.credential_id {
            for account_id in store::affected_accounts(&s.db, credential_id).await? {
                let _lifecycle = s.accounts.lifecycle_lock(&account_id).await;
                let account = s.db.get_account(&account_id).await?;
                if account.state == "disconnected" {
                    s.accounts
                        .restart(&account_id, "configuration_change")
                        .await?;
                    crate::api::validate_smtp(s.clone(), account_id);
                }
            }
        }
        return Ok(Json(session.public()));
    }
    if session.state != "pending" || session.expires_at <= chrono::Utc::now() {
        return Err(Error::Conflict(
            "OAuth authorization is expired or consumed; begin again".into(),
        ));
    }
    let secret = session
        .secret
        .ok_or_else(|| Error::Internal("OAuth session secret unavailable".into()))?;
    validate_completion(&input, &secret.state)?;
    store::authorization_claim(&s.db, &id, &app.0).await?;
    if input.error.is_some() {
        store::authorization_finish(&s.db, &id, "denied", None).await?;
        return Ok(Json(
            store::authorization_get(&s.db, &id, &app.0).await?.public(),
        ));
    }
    let token = match s
        .oauth
        .exchange_code(
            &secret.client_id,
            input.code.as_deref().unwrap_or_default(),
            &secret.redirect_uri,
            &secret.verifier,
        )
        .await
    {
        Ok(token) => token,
        Err(error) => {
            store::authorization_finish(&s.db, &id, "failed", None).await?;
            return Err(error);
        }
    };
    let Some(refresh) = token.refresh_token.as_deref().filter(|v| !v.is_empty()) else {
        store::authorization_finish(&s.db, &id, "failed", None).await?;
        return Err(Error::Invalid(
            "provider did not return a refresh token; authorize offline access again".into(),
        ));
    };
    let credential = match store::authorization_commit(&s.db, &id, &secret, refresh).await {
        Ok(credential) => credential,
        Err(error) => {
            store::authorization_finish(&s.db, &id, "failed", None).await?;
            return Err(error);
        }
    };
    s.oauth.seed(&credential, &token).await?;
    if secret.target_id.is_some() {
        resume_accounts(&s, &credential.id).await?;
    }
    Ok(Json(
        store::authorization_get(&s.db, &id, &app.0).await?.public(),
    ))
}

fn validate_completion(input: &Complete, expected_state: &str) -> Result<()> {
    if !bool::from(input.state.as_bytes().ct_eq(expected_state.as_bytes())) {
        return Err(Error::Invalid("OAuth state mismatch".into()));
    }
    if input.code.is_some() == input.error.is_some()
        || input
            .code
            .as_ref()
            .is_some_and(|v| v.is_empty() || v.len() > 16384)
    {
        return Err(Error::Invalid(
            "provide an authorization code or provider denial".into(),
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateCredential {
    client_id: String,
    refresh_token: Option<String>,
    subject: Option<String>,
}

fn validate_credential(mode: &Mode, refresh: Option<&str>, subject: Option<&str>) -> Result<()> {
    if refresh.is_some_and(|v| v.is_empty() || v.len() > 32768)
        || subject.is_some_and(|v| v.is_empty() || v.len() > 320 || v.contains(['\r', '\n', '\0']))
    {
        return Err(Error::Invalid("invalid OAuth credential fields".into()));
    }
    match mode {
        Mode::Delegated if refresh.is_some() && subject.is_none() => Ok(()),
        Mode::DomainWideDelegation if refresh.is_none() && subject.is_some() => Ok(()),
        Mode::Application if refresh.is_none() && subject.is_none() => Ok(()),
        _ => Err(Error::Invalid(
            "credential fields do not match OAuth registration mode".into(),
        )),
    }
}

async fn create(
    State(s): State<AppState>,
    Json(input): Json<CreateCredential>,
) -> Result<(StatusCode, Json<Value>)> {
    let p = s.oauth.registration(&input.client_id)?;
    validate_credential(
        &p.mode,
        input.refresh_token.as_deref(),
        input.subject.as_deref(),
    )?;
    let token = s
        .oauth
        .validate_import(
            &input.client_id,
            input.refresh_token.as_deref(),
            input.subject.as_deref(),
        )
        .await?;
    let c = store::create(
        &s.db,
        &input.client_id,
        token
            .refresh_token
            .as_deref()
            .or(input.refresh_token.as_deref()),
        input.subject.as_deref(),
    )
    .await?;
    s.oauth.seed(&c, &token).await?;
    Ok((StatusCode::CREATED, Json(c.public())))
}

async fn credential(State(s): State<AppState>, Path(id): Path<String>) -> Result<Json<Value>> {
    Ok(Json(store::get(&s.db, &id).await?.public()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceCredential {
    refresh_token: String,
}

async fn replace(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<ReplaceCredential>,
) -> Result<Json<Value>> {
    let old = store::get(&s.db, &id).await?;
    let p = s.oauth.registration(&old.client_id)?;
    if p.mode != Mode::Delegated {
        return Err(Error::Invalid(
            "service credentials are configured in YAML".into(),
        ));
    }
    validate_credential(&p.mode, Some(&input.refresh_token), None)?;
    let token = s
        .oauth
        .validate_import(&old.client_id, Some(&input.refresh_token), None)
        .await?;
    let c = store::replace(
        &s.db,
        &id,
        old.revision,
        token
            .refresh_token
            .as_deref()
            .or(Some(&input.refresh_token)),
        None,
    )
    .await?;
    s.oauth.seed(&c, &token).await?;
    resume_accounts(&s, &id).await?;
    Ok(Json(c.public()))
}

async fn delete(State(s): State<AppState>, Path(id): Path<String>) -> Result<StatusCode> {
    store::delete(&s.db, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn resume_accounts(s: &AppState, credential: &str) -> Result<()> {
    for id in store::affected_accounts(&s.db, credential).await? {
        let _lifecycle = s.accounts.lifecycle_lock(&id).await;
        let account = match s.db.get_account(&id).await {
            Ok(a) => a,
            Err(Error::NotFound) => continue,
            Err(e) => return Err(e),
        };
        if account.state != "paused" {
            s.accounts.restart(&id, "configuration_change").await?;
        }
        crate::api::validate_smtp(s.clone(), id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn completion_rejects_state_and_ambiguous_outcomes() {
        let mut input = Complete {
            state: "expected".into(),
            code: Some("code".into()),
            error: None,
        };
        assert!(validate_completion(&input, "expected").is_ok());
        assert!(validate_completion(&input, "other").is_err());
        input.error = Some("access_denied".into());
        assert!(validate_completion(&input, "expected").is_err());
        input.code = None;
        assert!(validate_completion(&input, "expected").is_ok());
    }
    #[test]
    fn credentials_match_modes() {
        assert!(validate_credential(&Mode::Delegated, Some("refresh"), None).is_ok());
        assert!(validate_credential(&Mode::Delegated, None, None).is_err());
        assert!(validate_credential(&Mode::Application, Some("refresh"), None).is_err());
        assert!(
            validate_credential(&Mode::DomainWideDelegation, None, Some("mail@example.com"))
                .is_ok()
        );
    }
    #[test]
    fn pkce_secrets_are_random_and_valid() {
        let first = random_secret();
        assert_eq!(first.len(), 43);
        assert_ne!(first, random_secret());
        assert_eq!(
            URL_SAFE_NO_PAD
                .encode(Sha256::digest(first.as_bytes()))
                .len(),
            43
        );
    }
}
