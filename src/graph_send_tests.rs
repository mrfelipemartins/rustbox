use super::*;
use crate::{accounts::Accounts, config::Config, crypto::Crypto, graph_store::GraphPhase, smtp};
use axum::{
    Json,
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::Utc;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;

#[derive(Clone)]
struct Peer {
    base: String,
    state: Arc<Mutex<PeerState>>,
}
#[derive(Default)]
struct PeerState {
    base_mime: Vec<u8>,
    content: Value,
    attachments: Vec<Uploaded>,
    pending: Option<Value>,
    bytes: Vec<u8>,
    ranges: Vec<String>,
    creates: usize,
    fail_create: bool,
    sessions: usize,
    sends: usize,
    fail_first_chunk: bool,
    fail_final_chunk: bool,
    fail_final_send: bool,
    is_draft: bool,
    hide_attachments: bool,
}
struct Uploaded {
    metadata: Value,
    data: Vec<u8>,
}
fn response(status: StatusCode, value: Value) -> Response {
    (status, Json(value)).into_response()
}
fn draft(state: &PeerState) -> Value {
    json!({"id":"draft-1","isDraft":state.is_draft,"internetMessageId":"<provider-message@example.com>","@odata.etag":"W/\"draft-version\""})
}
fn content(raw: &[u8]) -> Value {
    let parsed = crate::mail::parse(raw).unwrap();
    let addresses = |field: &str| {
        parsed[field]
            .as_array()
            .into_iter()
            .flatten()
            .map(|v| json!({"emailAddress":{"name":v["name"],"address":v["address"]}}))
            .collect::<Vec<_>>()
    };
    json!({"subject":parsed["subject"],"body":{"contentType":if parsed["html"].is_null(){"Text"}else{"HTML"},"content":if parsed["html"].is_null(){parsed["text"].clone()}else{parsed["html"].clone()}},"from":addresses("from").into_iter().next(),"sender":addresses("from").into_iter().next(),"toRecipients":addresses("to"),"ccRecipients":addresses("cc"),"bccRecipients":addresses("bcc"),"replyTo":addresses("reply_to"),"internetMessageHeaders":parsed["raw_headers"]})
}
async fn provider(
    State(peer): State<Peer>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri.path();
    let mut state = peer.state.lock().await;
    if path == "/upload" {
        assert!(headers.get("authorization").is_none());
        if method == Method::GET {
            if state.pending.is_none() {
                return StatusCode::NOT_FOUND.into_response();
            }
            return response(
                StatusCode::OK,
                json!({"nextExpectedRanges":[format!("{}-",state.bytes.len())]}),
            );
        }
        assert_eq!(method, Method::PUT);
        let range = headers["content-range"].to_str().unwrap().to_owned();
        let (positions, total) = range
            .strip_prefix("bytes ")
            .unwrap()
            .split_once('/')
            .unwrap();
        let (start, end) = positions.split_once('-').unwrap();
        assert_eq!(start.parse::<usize>().unwrap(), state.bytes.len());
        assert_eq!(
            end.parse::<usize>().unwrap() + 1,
            state.bytes.len() + body.len()
        );
        let total = total.parse::<usize>().unwrap();
        state.ranges.push(range);
        state.bytes.extend_from_slice(&body);
        let finished = state.bytes.len() == total;
        if finished {
            let pending = state.pending.take().unwrap();
            let id = format!("attachment-{}", state.attachments.len());
            let metadata = json!({"id":id,"name":pending["name"],"size":state.bytes.len(),"contentType":pending["contentType"],"isInline":pending["isInline"],"contentId":pending["contentId"]});
            let data = std::mem::take(&mut state.bytes);
            state.attachments.push(Uploaded { metadata, data });
            if state.fail_final_chunk {
                state.fail_final_chunk = false;
                return StatusCode::BAD_GATEWAY.into_response();
            }
            return response(StatusCode::CREATED, json!({"id":id}));
        }
        if state.fail_first_chunk {
            state.fail_first_chunk = false;
            return StatusCode::BAD_GATEWAY.into_response();
        }
        return response(
            StatusCode::OK,
            json!({"nextExpectedRanges":[format!("{}-",state.bytes.len())]}),
        );
    }
    assert_eq!(headers["authorization"], "Bearer fixture-access-token");
    assert_eq!(headers["prefer"], "IdType=\"ImmutableId\"");
    if method == Method::POST && path == "/messages" {
        state.creates += 1;
        state.is_draft = true;
        state.base_mime = STANDARD.decode(&body).unwrap();
        state.content = content(&state.base_mime);
        if state.fail_create {
            state.fail_create = false;
            return StatusCode::BAD_GATEWAY.into_response();
        }
        return response(StatusCode::CREATED, draft(&state));
    }
    if method == Method::GET && path == "/messages/draft-1" {
        if uri.query().is_some_and(|q| q.contains("body")) {
            return response(StatusCode::OK, state.content.clone());
        }
        return response(StatusCode::OK, draft(&state));
    }
    if method == Method::GET && path == "/mailFolders/drafts/messages" {
        let mut value = draft(&state);
        let marker = String::from_utf8_lossy(&state.base_mime)
            .lines()
            .find_map(|line| line.strip_prefix("X-Rustbox-Send-Id: "))
            .unwrap_or_default()
            .to_owned();
        value["internetMessageHeaders"] = json!([{"name":"X-Rustbox-Send-Id","value":marker}]);
        return response(StatusCode::OK, json!({"value":[value]}));
    }
    if path == "/messages/draft-1/attachments/createUploadSession" {
        assert_eq!(method, Method::POST);
        state.sessions += 1;
        let value: Value = serde_json::from_slice(&body).unwrap();
        state.pending = Some(value["AttachmentItem"].clone());
        state.bytes.clear();
        return response(
            StatusCode::CREATED,
            json!({"uploadUrl":format!("{}/upload?authtoken=private-session-token",peer.base),"expirationDateTime":(Utc::now()+chrono::Duration::hours(1)).to_rfc3339(),"nextExpectedRanges":["0-"]}),
        );
    }
    if path == "/messages/draft-1/attachments" {
        if method == Method::GET {
            let attachments = if state.hide_attachments {
                Vec::new()
            } else {
                state
                    .attachments
                    .iter()
                    .map(|a| a.metadata.clone())
                    .collect::<Vec<_>>()
            };
            return response(StatusCode::OK, json!({"value":attachments}));
        }
        assert_eq!(method, Method::POST);
        let value: Value = serde_json::from_slice(&body).unwrap();
        let data = STANDARD
            .decode(value["contentBytes"].as_str().unwrap())
            .unwrap();
        let id = format!("attachment-{}", state.attachments.len());
        let metadata = json!({"id":id,"name":value["name"],"size":data.len(),"contentType":value["contentType"],"isInline":value["isInline"],"contentId":value["contentId"]});
        state.attachments.push(Uploaded { metadata, data });
        return response(StatusCode::CREATED, json!({"id":id}));
    }
    if method == Method::GET && path.ends_with("/$value") {
        let id = path.trim_end_matches("/$value").rsplit('/').next().unwrap();
        let attachment = state
            .attachments
            .iter()
            .find(|a| a.metadata["id"] == id)
            .unwrap();
        return (StatusCode::OK, attachment.data.clone()).into_response();
    }
    if method == Method::POST && path == "/messages/draft-1/send" {
        state.sends += 1;
        state.is_draft = false;
        if state.fail_final_send {
            state.fail_final_send = false;
            return StatusCode::BAD_GATEWAY.into_response();
        }
        return StatusCode::ACCEPTED.into_response();
    }
    panic!("unexpected fixture route {method} {uri}");
}

struct Fixture {
    db: Arc<Db>,
    config: Arc<Config>,
    account: Account,
    client: NativeClient,
    peer: Peer,
    server: tokio::task::JoinHandle<()>,
}
impl Fixture {
    async fn new() -> anyhow::Result<Self> {
        let database_url = std::env::var("TEST_DATABASE_URL")?;
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(10)
            .connect(&database_url)
            .await?;
        sqlx::migrate!().run(&pool).await?;
        let db = Arc::new(Db::new(pool, Crypto::new(&[42; 32])?));
        let config = Arc::new(Config {
            database_url,
            listen: "127.0.0.1:0".parse()?,
            api_keys: HashMap::new(),
            encryption_key: vec![42; 32],
            allow_private_networks: false,
            tls_ca_file: None,
            log_filter: String::new(),
            max_message_bytes: 25 * 1024 * 1024,
            imap_concurrency: 4,
            smtp_concurrency: 4,
            webhook_concurrency: 4,
            command_timeout_secs: 10,
            webhooks: Default::default(),
            native: Default::default(),
            cache: Default::default(),
            oauth: serde_json::from_value(
                json!({"providers":{"graph-send-fixture":{"provider":"microsoft","mode":"application","tenant":"test-tenant","client_id":"test-client","client_secret":"test-secret"}}}),
            )?,
        });
        let accounts = Accounts::new(db.clone(), config.clone());
        let credential = crate::oauth::store::create(&db, "graph-send-fixture", None, None).await?;
        accounts
            .oauth
            .seed(
                &credential,
                &crate::oauth::TokenResponse {
                    access_token: "fixture-access-token".into(),
                    refresh_token: None,
                    expires_in: 3600,
                },
            )
            .await?;
        let input = serde_json::from_value(
            json!({"name":"Graph send fixture","backend":"microsoft","credential_id":credential.id,"target_mailbox":"shared@example.com","shared":true}),
        )?;
        let account = db.create_account(input).await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let peer = Peer {
            base: format!("http://{address}"),
            state: Arc::new(Mutex::new(PeerState::default())),
        };
        let cloned = peer.clone();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new()
                    .fallback(provider)
                    .with_state(cloned)
                    .layer(axum::extract::DefaultBodyLimit::max(8 * 1024 * 1024)),
            )
            .await
            .unwrap();
        });
        let client = NativeClient::with_test_endpoint(
            config.clone(),
            accounts.oauth.clone(),
            peer.base.clone(),
        );
        Ok(Self {
            db,
            config,
            account,
            client,
            peer,
            server,
        })
    }
    async fn enqueue(&self, key: &str) -> anyhow::Result<(String, Vec<u8>)> {
        let request: smtp::SendRequest = serde_json::from_value(
            json!({"from":"shared@example.com","to":["recipient@example.com"],"cc":["copy@example.com"],"bcc":["hidden@example.com"],"reply_to":["reply@example.com"],"subject":"Graph upload regression","in_reply_to":"<parent@example.com>","references":["<ancestor@example.com>","<parent@example.com>"],"text":"Plain text body","html":"<p>HTML body</p><img src=\"cid:logo@example.com\">","attachments":[{"filename":"large.bin","content_type":"application/octet-stream","data_base64":STANDARD.encode(vec![0x5au8;4_000_000])},{"filename":"logo.png","content_type":"image/png","data_base64":STANDARD.encode(b"image-bytes"),"disposition":"inline","content_id":"logo@example.com"}]}),
        )?;
        let status = smtp::enqueue(
            &self.db,
            &self.config,
            &self.account.id,
            "fixture",
            key,
            request,
        )
        .await?;
        let encrypted: Vec<u8> =
            sqlx::query_scalar("SELECT payload FROM send_blobs WHERE send_id=?")
                .bind(&status.id)
                .fetch_one(&self.db.pool)
                .await?;
        let payload = self.db.crypto.decrypt(&encrypted)?;
        let offset = payload
            .iter()
            .position(|b| *b == b'\n')
            .ok_or_else(|| anyhow::anyhow!("payload envelope missing"))?;
        let mut raw = b"Bcc: hidden@example.com\r\n".to_vec();
        raw.extend_from_slice(&payload[offset + 1..]);
        Ok((status.id, raw))
    }
    async fn close(self) -> anyhow::Result<()> {
        let credential = self.account.config.credential_id.clone().unwrap();
        self.db.delete_account(&self.account.id).await?;
        crate::oauth::store::delete(&self.db, &credential).await?;
        self.server.abort();
        Ok(())
    }
}

#[tokio::test]
#[ignore = "requires a dedicated MySQL database in TEST_DATABASE_URL"]
async fn graph_large_inline_and_regular_attachments_submit_once_and_recover_acceptance()
-> anyhow::Result<()> {
    let fixture = Fixture::new().await?;
    let (id, raw) = fixture.enqueue("normal").await?;
    sqlx::query("UPDATE sends SET status='sending' WHERE id=?")
        .bind(&id)
        .execute(&fixture.db.pool)
        .await?;
    let result = submit(
        &fixture.db,
        &fixture.client,
        &fixture.account,
        &id,
        raw.clone(),
    )
    .await?;
    assert_eq!(result.provider_id.as_deref(), Some("draft-1"));
    submit(&fixture.db, &fixture.client, &fixture.account, &id, raw).await?;
    {
        let state = fixture.peer.state.lock().await;
        assert_eq!(state.creates, 1);
        assert_eq!(state.sessions, 1);
        assert_eq!(state.sends, 1);
        assert_eq!(state.ranges.len(), 2);
        assert_eq!(state.attachments.len(), 2);
        assert!(
            state
                .attachments
                .iter()
                .any(|a| a.data == vec![0x5au8; 4_000_000])
        );
        assert!(
            state
                .attachments
                .iter()
                .any(|a| a.metadata["isInline"] == true
                    && a.metadata["contentId"] == "logo@example.com"
                    && a.data == b"image-bytes")
        );
        let metadata = crate::mail::parse(&state.base_mime)?;
        assert_eq!(metadata["in_reply_to"], json!(["<parent@example.com>"]));
        assert_eq!(metadata["bcc"][0]["address"], "hidden@example.com");
        assert!(
            metadata["html"]
                .as_str()
                .unwrap()
                .contains("cid:logo@example.com")
        );
    }
    smtp::recover(&fixture.db).await?;
    let status = smtp::get(&fixture.db, &id).await?;
    assert_eq!(status.status, "accepted");
    assert!(status.submitted_at.is_some());
    fixture.close().await
}

#[tokio::test]
#[ignore = "requires a dedicated MySQL database in TEST_DATABASE_URL"]
async fn graph_lost_chunk_ack_resumes_from_authoritative_offset_and_final_marker_blocks_resend()
-> anyhow::Result<()> {
    let fixture = Fixture::new().await?;
    let (id, raw) = fixture.enqueue("resume").await?;
    fixture.peer.state.lock().await.fail_first_chunk = true;
    assert!(
        submit(
            &fixture.db,
            &fixture.client,
            &fixture.account,
            &id,
            raw.clone()
        )
        .await
        .is_err()
    );
    sqlx::query("UPDATE sends SET status='sending' WHERE id=?")
        .bind(&id)
        .execute(&fixture.db.pool)
        .await?;
    smtp::recover(&fixture.db).await?;
    assert_eq!(smtp::get(&fixture.db, &id).await?.status, "queued");
    submit(&fixture.db, &fixture.client, &fixture.account, &id, raw).await?;
    {
        let state = fixture.peer.state.lock().await;
        assert_eq!(state.sessions, 1);
        assert_eq!(
            state.ranges,
            vec!["bytes 0-3276799/4000000", "bytes 3276800-3999999/4000000"]
        );
        assert_eq!(state.sends, 1);
    }
    let (blocked, raw) = fixture.enqueue("blocked-final").await?;
    let mut state = graph_store::load(&fixture.db, &blocked).await?.unwrap();
    state.phase = GraphPhase::Submitting;
    state.draft_id = Some("another-draft".into());
    graph_store::save(&fixture.db, &blocked, &mut state).await?;
    assert!(matches!(
        submit(
            &fixture.db,
            &fixture.client,
            &fixture.account,
            &blocked,
            raw
        )
        .await,
        Err(Error::UnknownOutcome)
    ));
    sqlx::query("UPDATE sends SET status='sending' WHERE id=?")
        .bind(&blocked)
        .execute(&fixture.db.pool)
        .await?;
    smtp::recover(&fixture.db).await?;
    assert_eq!(smtp::get(&fixture.db, &blocked).await?.status, "unknown");
    assert_eq!(fixture.peer.state.lock().await.sends, 1);
    fixture.close().await
}

#[tokio::test]
#[ignore = "requires a dedicated MySQL database in TEST_DATABASE_URL"]
async fn graph_expired_completed_session_reconciles_attachment_without_reupload()
-> anyhow::Result<()> {
    let fixture = Fixture::new().await?;
    let (id, raw) = fixture.enqueue("final-chunk-ack").await?;
    fixture.peer.state.lock().await.fail_final_chunk = true;
    assert!(
        submit(
            &fixture.db,
            &fixture.client,
            &fixture.account,
            &id,
            raw.clone()
        )
        .await
        .is_err()
    );
    let mut state = graph_store::load(&fixture.db, &id).await?.unwrap();
    for part in state.progress["attachments"].as_array_mut().unwrap() {
        if part.get("session").is_some_and(|v| !v.is_null()) {
            part["session"]["expires_at"] =
                json!((Utc::now() - chrono::Duration::minutes(1)).to_rfc3339());
        }
    }
    graph_store::save(&fixture.db, &id, &mut state).await?;
    submit(&fixture.db, &fixture.client, &fixture.account, &id, raw).await?;
    {
        let state = fixture.peer.state.lock().await;
        assert_eq!(state.creates, 1);
        assert_eq!(state.sessions, 1);
        assert_eq!(state.ranges.len(), 2);
        assert_eq!(state.attachments.len(), 2);
        assert_eq!(state.sends, 1);
    }
    fixture.close().await
}

#[tokio::test]
#[ignore = "requires a dedicated MySQL database in TEST_DATABASE_URL"]
async fn graph_uncertain_final_chunk_without_visible_attachment_never_restarts_upload()
-> anyhow::Result<()> {
    let fixture = Fixture::new().await?;
    let (id, raw) = fixture.enqueue("uncertain-final-no-evidence").await?;
    fixture.peer.state.lock().await.fail_final_chunk = true;
    assert!(
        submit(
            &fixture.db,
            &fixture.client,
            &fixture.account,
            &id,
            raw.clone()
        )
        .await
        .is_err()
    );
    let mut state = graph_store::load(&fixture.db, &id).await?.unwrap();
    let mut found = false;
    for part in state.progress["attachments"].as_array_mut().unwrap() {
        if part
            .get("session")
            .is_some_and(|session| !session.is_null())
        {
            assert_eq!(part["final_chunk_pending"], true);
            part["session"]["expires_at"] =
                json!((Utc::now() - chrono::Duration::minutes(1)).to_rfc3339());
            found = true;
        }
    }
    assert!(found);
    graph_store::save(&fixture.db, &id, &mut state).await?;
    fixture.peer.state.lock().await.hide_attachments = true;
    assert!(
        submit(&fixture.db, &fixture.client, &fixture.account, &id, raw)
            .await
            .is_err()
    );
    {
        let state = fixture.peer.state.lock().await;
        assert_eq!(state.sessions, 1);
        assert_eq!(state.ranges.len(), 2);
        assert_eq!(state.sends, 0);
    }
    fixture.close().await
}

#[tokio::test]
#[ignore = "requires a dedicated MySQL database in TEST_DATABASE_URL"]
async fn graph_ambiguous_final_submission_is_never_automatically_repeated() -> anyhow::Result<()> {
    let fixture = Fixture::new().await?;
    let (id, raw) = fixture.enqueue("ambiguous-final-submission").await?;
    fixture.peer.state.lock().await.fail_final_send = true;
    sqlx::query("UPDATE sends SET status='sending' WHERE id=?")
        .bind(&id)
        .execute(&fixture.db.pool)
        .await?;
    assert!(matches!(
        submit(
            &fixture.db,
            &fixture.client,
            &fixture.account,
            &id,
            raw.clone()
        )
        .await,
        Err(Error::UnknownOutcome)
    ));
    assert_eq!(
        graph_store::phase(&fixture.db, &id).await?,
        Some(GraphPhase::Unknown)
    );
    assert!(matches!(
        submit(&fixture.db, &fixture.client, &fixture.account, &id, raw).await,
        Err(Error::UnknownOutcome)
    ));
    smtp::recover(&fixture.db).await?;
    assert_eq!(smtp::get(&fixture.db, &id).await?.status, "unknown");
    assert_eq!(fixture.peer.state.lock().await.sends, 1);
    fixture.close().await
}

#[tokio::test]
#[ignore = "requires a dedicated MySQL database in TEST_DATABASE_URL"]
async fn graph_draft_creation_lost_ack_adopts_marker_without_another_draft() -> anyhow::Result<()> {
    let fixture = Fixture::new().await?;
    let (id, raw) = fixture.enqueue("lost-draft-creation").await?;
    fixture.peer.state.lock().await.fail_create = true;
    assert!(matches!(
        submit(
            &fixture.db,
            &fixture.client,
            &fixture.account,
            &id,
            raw.clone()
        )
        .await,
        Err(Error::Unavailable(_))
    ));
    let state = graph_store::load(&fixture.db, &id).await?.unwrap();
    assert_eq!(state.phase, GraphPhase::Preparing);
    assert!(state.draft_id.is_none());
    assert_eq!(state.progress["creating_draft"], true);
    submit(&fixture.db, &fixture.client, &fixture.account, &id, raw).await?;
    {
        let state = fixture.peer.state.lock().await;
        assert_eq!(state.creates, 1);
        assert_eq!(state.sessions, 1);
        assert_eq!(state.attachments.len(), 2);
        assert_eq!(state.sends, 1);
    }
    fixture.close().await
}

#[tokio::test]
#[ignore = "requires a dedicated MySQL database in TEST_DATABASE_URL"]
async fn graph_draft_external_edit_blocks_final_submission() -> anyhow::Result<()> {
    let fixture = Fixture::new().await?;
    let (id, raw) = fixture.enqueue("edited-draft").await?;
    fixture.peer.state.lock().await.fail_first_chunk = true;
    assert!(
        submit(
            &fixture.db,
            &fixture.client,
            &fixture.account,
            &id,
            raw.clone()
        )
        .await
        .is_err()
    );
    fixture.peer.state.lock().await.content["subject"] = json!("Changed by another client");
    assert!(matches!(
        submit(&fixture.db, &fixture.client, &fixture.account, &id, raw).await,
        Err(Error::Conflict(_))
    ));
    assert_eq!(fixture.peer.state.lock().await.sends, 0);
    assert_eq!(
        graph_store::phase(&fixture.db, &id).await?,
        Some(GraphPhase::Preparing)
    );
    fixture.close().await
}
