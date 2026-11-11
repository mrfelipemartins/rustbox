use super::config::{Mode, Provider, ProviderRegistration};
use crate::error::{Error, Result};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use futures_util::StreamExt;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use oauth2::{AuthUrl, ClientId, CsrfToken, RedirectUrl, Scope, basic::BasicClient};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::time::Duration;
use uuid::Uuid;

const GOOGLE_TOKEN: &str = "https://oauth2.googleapis.com/token";
const GOOGLE_SCOPE: &str = "https://www.googleapis.com/auth/gmail.modify";
const MICROSOFT_SCOPE: &str = "https://graph.microsoft.com/Mail.ReadWrite https://graph.microsoft.com/Mail.Send offline_access";
const MICROSOFT_SHARED_SCOPE: &str = "https://graph.microsoft.com/Mail.ReadWrite.Shared https://graph.microsoft.com/Mail.Send.Shared offline_access";

#[derive(Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: u64,
}

pub fn oauth_error(code: &'static str, retryable: bool) -> Error {
    Error::OAuth { code, retryable }
}

fn required(value: &Option<String>) -> Result<&str> {
    value
        .as_deref()
        .ok_or_else(|| oauth_error("configuration_error", false))
}

pub fn token_endpoint(registration: &ProviderRegistration) -> Result<String> {
    match registration.provider {
        Provider::Google => Ok(GOOGLE_TOKEN.into()),
        Provider::Microsoft => {
            let tenant = required(&registration.tenant)?;
            if tenant.is_empty()
                || !tenant
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.'))
            {
                return Err(oauth_error("configuration_error", false));
            }
            Ok(format!(
                "https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token"
            ))
        }
    }
}

pub fn authorization_url(
    registration: &ProviderRegistration,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
    login_hint: Option<&str>,
) -> Result<String> {
    if registration.mode != Mode::Delegated
        || !registration
            .redirect_uris
            .iter()
            .any(|uri| uri == redirect_uri)
    {
        return Err(Error::Invalid(
            "redirect URI is not configured for this delegated client".into(),
        ));
    }
    let endpoint = match registration.provider {
        Provider::Google => "https://accounts.google.com/o/oauth2/v2/auth".into(),
        Provider::Microsoft => token_endpoint(registration)?.replace("/token", "/authorize"),
    };
    let client = BasicClient::new(ClientId::new(required(&registration.client_id)?.to_owned()))
        .set_auth_uri(
            AuthUrl::new(endpoint).map_err(|_| oauth_error("configuration_error", false))?,
        )
        .set_redirect_uri(
            RedirectUrl::new(redirect_uri.to_owned())
                .map_err(|_| Error::Invalid("invalid redirect URI".into()))?,
        );
    let mut request = client
        .authorize_url(|| CsrfToken::new(state.to_owned()))
        .add_extra_param("code_challenge", challenge)
        .add_extra_param("code_challenge_method", "S256");
    match registration.provider {
        Provider::Google => {
            request = request
                .add_scope(Scope::new(GOOGLE_SCOPE.into()))
                .add_extra_param("access_type", "offline")
                .add_extra_param("prompt", "consent");
        }
        Provider::Microsoft => {
            for scope in (if registration.shared {
                MICROSOFT_SHARED_SCOPE
            } else {
                MICROSOFT_SCOPE
            })
            .split_whitespace()
            {
                request = request.add_scope(Scope::new(scope.into()));
            }
        }
    }
    if let Some(hint) = login_hint {
        if hint.len() > 320 || hint.contains(['\0', '\r', '\n']) {
            return Err(Error::Invalid("invalid login hint".into()));
        }
        request = request.add_extra_param("login_hint", hint);
    }
    Ok(request.url().0.into())
}

pub async fn exchange(
    registration: &ProviderRegistration,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<TokenResponse> {
    if registration.mode != Mode::Delegated
        || !registration
            .redirect_uris
            .iter()
            .any(|uri| uri == redirect_uri)
    {
        return Err(Error::Invalid(
            "invalid delegated authorization request".into(),
        ));
    }
    let mut form = vec![
        ("grant_type".into(), "authorization_code".into()),
        ("code".into(), code.into()),
        ("redirect_uri".into(), redirect_uri.into()),
        ("code_verifier".into(), verifier.into()),
    ];
    add_client(registration, &mut form).await?;
    request_token(&token_endpoint(registration)?, &form).await
}

pub async fn renew(
    registration: &ProviderRegistration,
    refresh_token: Option<&str>,
    subject: Option<&str>,
) -> Result<TokenResponse> {
    let form = renewal_form(registration, refresh_token, subject).await?;
    let mut token = request_token(&token_endpoint(registration)?, &form).await?;
    if registration.mode != Mode::Delegated {
        token.refresh_token = None;
    }
    Ok(token)
}

async fn renewal_form(
    registration: &ProviderRegistration,
    refresh_token: Option<&str>,
    subject: Option<&str>,
) -> Result<Vec<(String, String)>> {
    let mut form = Vec::new();
    match registration.mode {
        Mode::Delegated => {
            let refresh = refresh_token
                .filter(|v| !v.is_empty())
                .ok_or_else(|| oauth_error("reauthorization_required", false))?;
            form.push(("grant_type".into(), "refresh_token".into()));
            form.push(("refresh_token".into(), refresh.into()));
            add_client(registration, &mut form).await?;
        }
        Mode::DomainWideDelegation => {
            let subject = subject.filter(|v| valid_subject(v)).ok_or_else(|| {
                Error::Invalid("domain-wide delegation requires a mailbox subject".into())
            })?;
            let path = registration
                .service_account_file
                .as_ref()
                .ok_or_else(|| oauth_error("configuration_error", false))?;
            let bytes = tokio::fs::read(path)
                .await
                .map_err(|_| oauth_error("configuration_error", false))?;
            let subject = subject.to_owned();
            let assertion = tokio::task::spawn_blocking(move || google_assertion(&bytes, &subject))
                .await
                .map_err(|_| oauth_error("configuration_error", false))??;
            form.push((
                "grant_type".into(),
                "urn:ietf:params:oauth:grant-type:jwt-bearer".into(),
            ));
            form.push(("assertion".into(), assertion));
        }
        Mode::Application => {
            form.push(("grant_type".into(), "client_credentials".into()));
            form.push((
                "scope".into(),
                "https://graph.microsoft.com/.default".into(),
            ));
            add_client(registration, &mut form).await?;
        }
    }
    Ok(form)
}

#[cfg(test)]
pub async fn renew_test(
    registration: &ProviderRegistration,
    refresh_token: Option<&str>,
    subject: Option<&str>,
    endpoint: &str,
) -> Result<TokenResponse> {
    let form = renewal_form(registration, refresh_token, subject).await?;
    let mut token = request_token_with_client(&reqwest::Client::new(), endpoint, &form).await?;
    if registration.mode != Mode::Delegated {
        token.refresh_token = None;
    }
    Ok(token)
}

pub async fn validate_material(registration: &ProviderRegistration) -> Result<()> {
    match registration.mode {
        Mode::DomainWideDelegation => {
            let path = registration
                .service_account_file
                .as_ref()
                .ok_or_else(|| oauth_error("configuration_error", false))?;
            let bytes = tokio::fs::read(path)
                .await
                .map_err(|_| oauth_error("configuration_error", false))?;
            tokio::task::spawn_blocking(move || {
                google_assertion(&bytes, "validation@example.invalid")
            })
            .await
            .map_err(|_| oauth_error("configuration_error", false))??;
        }
        Mode::Application if registration.certificate_file.is_some() => {
            let mut form = Vec::new();
            add_client(registration, &mut form).await?;
        }
        _ => {}
    }
    Ok(())
}

pub fn valid_subject(subject: &str) -> bool {
    subject.len() <= 320
        && subject
            .split_once('@')
            .is_some_and(|(local, domain)| !local.is_empty() && !domain.is_empty())
        && !subject.chars().any(|c| c.is_control() || c.is_whitespace())
}

async fn add_client(
    registration: &ProviderRegistration,
    form: &mut Vec<(String, String)>,
) -> Result<()> {
    let client_id = required(&registration.client_id)?;
    form.push(("client_id".into(), client_id.into()));
    if let Some(secret) = &registration.client_secret {
        form.push(("client_secret".into(), secret.clone()));
    } else {
        let cert_path = registration
            .certificate_file
            .as_ref()
            .ok_or_else(|| oauth_error("configuration_error", false))?;
        let key_path = registration
            .private_key_file
            .as_ref()
            .ok_or_else(|| oauth_error("configuration_error", false))?;
        let cert = tokio::fs::read(cert_path)
            .await
            .map_err(|_| oauth_error("configuration_error", false))?;
        let key = tokio::fs::read(key_path)
            .await
            .map_err(|_| oauth_error("configuration_error", false))?;
        let client_id = client_id.to_owned();
        let endpoint = token_endpoint(registration)?;
        let assertion = tokio::task::spawn_blocking(move || {
            microsoft_assertion(&client_id, &endpoint, &cert, &key)
        })
        .await
        .map_err(|_| oauth_error("configuration_error", false))??;
        form.push((
            "client_assertion_type".into(),
            "urn:ietf:params:oauth:client-assertion-type:jwt-bearer".into(),
        ));
        form.push(("client_assertion".into(), assertion));
    }
    Ok(())
}

fn google_assertion(bytes: &[u8], subject: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct ServiceAccount {
        client_email: String,
        private_key: String,
        private_key_id: Option<String>,
    }
    let account: ServiceAccount =
        serde_json::from_slice(bytes).map_err(|_| oauth_error("configuration_error", false))?;
    let now = Utc::now().timestamp();
    let mut header = Header::new(Algorithm::RS256);
    header.kid = account.private_key_id;
    let claims = json!({"iss":account.client_email,"sub":subject,"scope":GOOGLE_SCOPE,"aud":GOOGLE_TOKEN,"iat":now,"exp":now+3600});
    let key = EncodingKey::from_rsa_pem(account.private_key.as_bytes())
        .map_err(|_| oauth_error("configuration_error", false))?;
    jsonwebtoken::encode(&header, &claims, &key)
        .map_err(|_| oauth_error("configuration_error", false))
}

fn microsoft_assertion(client_id: &str, endpoint: &str, cert: &[u8], key: &[u8]) -> Result<String> {
    let cert = rustls_pemfile::certs(&mut std::io::BufReader::new(cert))
        .next()
        .transpose()
        .map_err(|_| oauth_error("configuration_error", false))?
        .ok_or_else(|| oauth_error("configuration_error", false))?;
    let now = Utc::now().timestamp();
    let mut header = Header::new(Algorithm::PS256);
    header.x5t_s256 = Some(URL_SAFE_NO_PAD.encode(Sha256::digest(cert.as_ref())));
    let claims = json!({"iss":client_id,"sub":client_id,"aud":endpoint,"iat":now,"nbf":now-30,"exp":now+300,"jti":Uuid::new_v4().to_string()});
    let key =
        EncodingKey::from_rsa_pem(key).map_err(|_| oauth_error("configuration_error", false))?;
    jsonwebtoken::encode(&header, &claims, &key)
        .map_err(|_| oauth_error("configuration_error", false))
}

async fn request_token(endpoint: &str, form: &[(String, String)]) -> Result<TokenResponse> {
    let client = crate::security::safe_http_client(endpoint, false, None)
        .await
        .map_err(|_| oauth_error("provider_unavailable", true))?;
    request_token_with_client(&client, endpoint, form).await
}

async fn request_token_with_client(
    client: &reqwest::Client,
    endpoint: &str,
    form: &[(String, String)],
) -> Result<TokenResponse> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let response = client
            .post(endpoint)
            .form(form)
            .send()
            .await
            .map_err(|_| oauth_error("provider_unavailable", true))?;
        let status = response.status();
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| oauth_error("provider_unavailable", true))?;
            if body.len() + chunk.len() > 65536 {
                return Err(oauth_error("invalid_provider_response", true));
            }
            body.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            let error = serde_json::from_slice::<serde_json::Value>(&body).ok();
            let code = error
                .as_ref()
                .and_then(|v| v.get("error"))
                .and_then(|v| v.as_str());
            return Err(classify_error(status.as_u16(), code));
        }
        let token: TokenResponse = serde_json::from_slice(&body)
            .map_err(|_| oauth_error("invalid_provider_response", true))?;
        if token.access_token.is_empty()
            || token.access_token.contains(['\r', '\n', '\0', '\u{1}'])
            || token.expires_in <= 60
            || token.expires_in > 86400 * 7
        {
            return Err(oauth_error("invalid_provider_response", true));
        }
        Ok(token)
    })
    .await
    .map_err(|_| oauth_error("provider_unavailable", true))?
}

fn classify_error(status: u16, code: Option<&str>) -> Error {
    if status == 429 || status >= 500 {
        return oauth_error("provider_unavailable", true);
    }
    match code {
        Some("invalid_grant" | "interaction_required" | "login_required" | "consent_required") => {
            oauth_error("reauthorization_required", false)
        }
        Some(
            "invalid_client"
            | "unauthorized_client"
            | "invalid_scope"
            | "invalid_request"
            | "unsupported_grant_type",
        ) => oauth_error("configuration_error", false),
        Some("access_denied") => oauth_error("permission_denied", false),
        _ => oauth_error("provider_rejected", false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_assertions_have_verified_signatures_and_isolated_claims() {
        let private = include_bytes!("testdata/key.pem");
        let public = include_bytes!("testdata/public.pem");
        let cert = include_bytes!("testdata/certificate.pem");
        let key = jsonwebtoken::DecodingKey::from_rsa_pem(public).unwrap();
        let account = json!({"client_email":"service@example.com","private_key":String::from_utf8_lossy(private),"private_key_id":"key-1"});
        let token = google_assertion(
            &serde_json::to_vec(&account).unwrap(),
            "mailbox@example.com",
        )
        .unwrap();
        let mut validation = jsonwebtoken::Validation::new(Algorithm::RS256);
        validation.set_audience(&[GOOGLE_TOKEN]);
        validation.set_issuer(&["service@example.com"]);
        let decoded = jsonwebtoken::decode::<serde_json::Value>(&token, &key, &validation).unwrap();
        assert_eq!(decoded.claims["sub"], "mailbox@example.com");
        assert_eq!(decoded.claims["scope"], GOOGLE_SCOPE);
        assert_eq!(decoded.header.kid.as_deref(), Some("key-1"));
        let endpoint = "https://login.microsoftonline.com/tenant/oauth2/v2.0/token";
        let token = microsoft_assertion("app-client", endpoint, cert, private).unwrap();
        let mut validation = jsonwebtoken::Validation::new(Algorithm::PS256);
        validation.set_audience(&[endpoint]);
        validation.set_issuer(&["app-client"]);
        let decoded = jsonwebtoken::decode::<serde_json::Value>(&token, &key, &validation).unwrap();
        assert_eq!(decoded.claims["sub"], "app-client");
        assert_eq!(
            decoded.claims["exp"].as_i64().unwrap() - decoded.claims["iat"].as_i64().unwrap(),
            300
        );
        assert!(
            decoded.claims["jti"]
                .as_str()
                .is_some_and(|value| Uuid::parse_str(value).is_ok())
        );
        let der = rustls_pemfile::certs(&mut std::io::BufReader::new(cert.as_slice()))
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(
            decoded.header.x5t_s256,
            Some(URL_SAFE_NO_PAD.encode(Sha256::digest(der.as_ref())))
        );
    }

    #[test]
    fn provider_errors_are_sanitized_and_classified() {
        assert!(matches!(
            classify_error(400, Some("invalid_grant")),
            Error::OAuth {
                code: "reauthorization_required",
                retryable: false
            }
        ));
        assert!(matches!(
            classify_error(401, Some("invalid_client")),
            Error::OAuth {
                code: "configuration_error",
                retryable: false
            }
        ));
        assert!(matches!(
            classify_error(429, Some("invalid_grant")),
            Error::OAuth {
                retryable: true,
                ..
            }
        ));
        assert!(matches!(
            classify_error(503, None),
            Error::OAuth {
                retryable: true,
                ..
            }
        ));
    }

    #[test]
    fn delegation_subject_rejects_protocol_injection() {
        assert!(valid_subject("mailbox@example.com"));
        assert!(!valid_subject("mailbox@example.com\r\n"));
        assert!(!valid_subject("no-domain"));
    }

    #[tokio::test]
    async fn token_response_parsing_against_local_peer() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/token",
            axum::routing::post(|| async {
                axum::Json(
                    json!({"access_token":"access","expires_in":3600,"refresh_token":"rotated"}),
                )
            }),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::new();
        let token = request_token_with_client(
            &client,
            &format!("http://{address}/token"),
            &[("grant_type".into(), "refresh_token".into())],
        )
        .await
        .unwrap();
        assert_eq!(token.access_token, "access");
        assert_eq!(token.refresh_token.as_deref(), Some("rotated"));
        task.abort();
    }
}
