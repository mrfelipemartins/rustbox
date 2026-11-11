use std::{collections::VecDeque, sync::Arc, time::Duration};

use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{NaiveDateTime, Utc};
use lettre::{
    Address, Message,
    address::Envelope,
    message::{Attachment as MimeAttachment, Mailbox, MultiPart, SinglePart, header::ContentType},
    transport::smtp::{
        authentication::{Credentials, Mechanism},
        client::{AsyncSmtpConnection, Certificate, TlsParameters},
        extension::ClientId,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, Row};
use tokio::{task::JoinSet, time::timeout};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    accounts::{Accounts, Command},
    config::Config,
    db::{Db, MailServer},
    error::{Error, Result},
};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SendRequest {
    pub from: String,
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    pub subject: String,
    #[serde(default)]
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub references: Vec<String>,
    #[serde(default)]
    pub reply_to: Vec<String>,
    #[serde(default)]
    pub reply_to_message_id: Option<String>,
    #[serde(default)]
    pub retry_of_send_id: Option<String>,
    pub text: Option<String>,
    pub html: Option<String>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub filename: String,
    pub content_type: String,
    pub data_base64: String,
    #[serde(default)]
    pub disposition: Disposition,
    #[serde(default)]
    pub content_id: Option<String>,
}

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    #[default]
    Attachment,
    Inline,
}

#[derive(Debug, Serialize, FromRow)]
pub struct SendStatus {
    pub id: String,
    pub account_id: String,
    pub status: String,
    pub attempts: u32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    pub last_error: Option<String>,
    pub message_id: String,
    pub append_status: String,
    pub append_last_error: Option<String>,
    pub append_reconciliation_state: Option<String>,
    pub append_reconciliation_evidence: Option<Value>,
    pub append_attempts: u32,
    pub append_recoverable_until: Option<NaiveDateTime>,
    pub provider_id: Option<String>,
    pub graph_phase: Option<String>,
    pub reconciliation_state: Option<String>,
    pub resolution: Option<String>,
    pub reconciliation_evidence: Option<Value>,
    pub resolution_reason: Option<String>,
    pub resolution_evidence: Option<Value>,
    pub submitted_at: Option<NaiveDateTime>,
    pub resolved_at: Option<NaiveDateTime>,
}

#[derive(FromRow)]
struct Job {
    id: String,
    account_id: String,
    status: String,
    attempts: u32,
    expires_at: NaiveDateTime,
    append_mailbox: Option<String>,
    append_status: String,
    append_revision: u64,
}

#[derive(Deserialize, Serialize)]
struct EnvelopeData {
    from: String,
    to: Vec<String>,
}

struct Prepared {
    payload: Vec<u8>,
    message_id: String,
    evidence: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Accepted,
    Retry(&'static str),
    RetryAfter(&'static str, u64),
    Failed(&'static str),
    Unknown,
}

fn invalid(message: &str) -> Error {
    Error::Invalid(message.to_owned())
}

fn valid_identifier(value: &str) -> bool {
    value.len() <= 998
        && value.starts_with('<')
        && value.ends_with('>')
        && value[1..value.len() - 1]
            .split_once('@')
            .is_some_and(|(a, b)| !a.is_empty() && !b.is_empty())
        && !value[1..value.len() - 1]
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>'))
}

fn valid_content_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && !value
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>'))
}

pub fn content_fingerprint(raw: &[u8]) -> Result<String> {
    let parsed = crate::mail::parse(raw)?;
    let message = mail_parser::MessageParser::default()
        .parse(raw)
        .ok_or_else(|| invalid("invalid MIME"))?;
    let attachments: Vec<String> = message
        .attachments()
        .map(|p| hex::encode(Sha256::digest(p.contents())))
        .collect();
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(&json!({"subject":parsed["subject"],"text":parsed["text"],"html":parsed["html"],"attachments":attachments})).map_err(|_|Error::Internal("fingerprint serialization failed".into()))?)))
}

fn prepare(request: SendRequest, max_bytes: usize) -> Result<Prepared> {
    if request.to.len() + request.cc.len() + request.bcc.len() > 100 {
        return Err(invalid("at most 100 recipients are allowed"));
    }
    if request.to.is_empty() && request.cc.is_empty() && request.bcc.is_empty() {
        return Err(invalid("at least one recipient is required"));
    }
    if request.text.is_none() && request.html.is_none() {
        return Err(invalid("text or html is required"));
    }
    if request.attachments.len() > 100 {
        return Err(invalid("at most 100 attachments are allowed"));
    }
    for value in std::iter::once(&request.subject)
        .chain(std::iter::once(&request.from))
        .chain(request.to.iter())
        .chain(request.cc.iter())
        .chain(request.bcc.iter())
        .chain(request.reply_to.iter())
    {
        if value.chars().any(char::is_control) {
            return Err(invalid("header values cannot contain control characters"));
        }
    }
    if request
        .in_reply_to
        .as_deref()
        .is_some_and(|v| !valid_identifier(v))
        || request.references.iter().any(|v| !valid_identifier(v))
        || request.references.len() > 100
    {
        return Err(invalid("invalid reply message identifiers"));
    }
    let from: Mailbox = request
        .from
        .parse()
        .map_err(|_| invalid("invalid from address"))?;
    let sender = from.email.to_string();
    let message_id = format!("<{}@rustbox.local>", Uuid::new_v4());
    let mut builder = Message::builder()
        .from(from)
        .subject(request.subject)
        .message_id(Some(message_id.clone()));
    if let Some(id) = request.in_reply_to {
        builder = builder.in_reply_to(id);
    }
    if !request.references.is_empty() {
        builder = builder.references(request.references.join(" "));
    }
    for value in request.reply_to {
        builder = builder.reply_to(
            value
                .parse()
                .map_err(|_| invalid("invalid reply-to address"))?,
        );
    }
    let mut recipients = Vec::new();
    for value in request.to {
        let mailbox: Mailbox = value.parse().map_err(|_| invalid("invalid to address"))?;
        recipients.push(mailbox.email.to_string());
        builder = builder.to(mailbox);
    }
    for value in request.cc {
        let mailbox: Mailbox = value.parse().map_err(|_| invalid("invalid cc address"))?;
        recipients.push(mailbox.email.to_string());
        builder = builder.cc(mailbox);
    }
    for value in request.bcc {
        let mailbox: Mailbox = value.parse().map_err(|_| invalid("invalid bcc address"))?;
        recipients.push(mailbox.email.to_string());
        builder = builder.bcc(mailbox);
    }
    let mut inline = Vec::new();
    let mut attachments = Vec::new();
    let mut content_ids = std::collections::HashSet::new();
    for attachment in request.attachments {
        if attachment.filename.contains(['\r', '\n', '\0']) || attachment.filename.len() > 255 {
            return Err(invalid("invalid attachment filename"));
        }
        let data = STANDARD
            .decode(attachment.data_base64)
            .map_err(|_| invalid("invalid attachment base64"))?;
        if attachment.content_type.contains(['\r', '\n', '\0']) {
            return Err(invalid("invalid attachment content type"));
        }
        let content_type = ContentType::parse(&attachment.content_type)
            .map_err(|_| invalid("invalid attachment content type"))?;
        match attachment.disposition {
            Disposition::Inline => {
                if request.html.is_none() {
                    return Err(invalid("inline attachments require an HTML body"));
                }
                let id = attachment
                    .content_id
                    .filter(|v| valid_content_id(v))
                    .ok_or_else(|| invalid("inline attachments require a valid content_id"))?;
                if !content_ids.insert(id.clone()) {
                    return Err(invalid("duplicate inline content_id"));
                }
                inline.push(
                    MimeAttachment::new_inline_with_name(id, attachment.filename)
                        .body(data, content_type),
                );
            }
            Disposition::Attachment => {
                if attachment.content_id.is_some() {
                    return Err(invalid(
                        "content_id is only supported for inline attachments",
                    ));
                }
                attachments.push(MimeAttachment::new(attachment.filename).body(data, content_type));
            }
        }
    }
    let mut body = if let Some(html) = request.html {
        let mut related = MultiPart::related().singlepart(SinglePart::html(html));
        for part in inline {
            related = related.singlepart(part);
        }
        if let Some(text) = request.text {
            MultiPart::alternative()
                .singlepart(SinglePart::plain(text))
                .multipart(related)
        } else {
            related
        }
    } else {
        MultiPart::alternative().singlepart(SinglePart::plain(request.text.unwrap_or_default()))
    };
    if !attachments.is_empty() {
        let mut mixed = MultiPart::mixed().multipart(body);
        for part in attachments {
            mixed = mixed.singlepart(part);
        }
        body = mixed;
    }
    let message = builder
        .multipart(body)
        .map_err(|_| invalid("invalid message"))?;
    let raw = message.formatted();
    if raw.len() > max_bytes {
        return Err(Error::SendLimitExceeded {
            backend: "configured",
            maximum_mime_bytes: max_bytes,
            actual_mime_bytes: raw.len(),
        });
    }
    let fingerprint = content_fingerprint(&raw)?;
    let evidence = json!({"from":sender,"recipients":recipients,"content_fingerprint":fingerprint});
    let mut payload = serde_json::to_vec(&EnvelopeData {
        from: sender,
        to: recipients,
    })
    .map_err(|_| Error::Internal("envelope serialization failed".to_owned()))?;
    payload.push(b'\n');
    payload.extend_from_slice(&raw);
    Ok(Prepared {
        payload,
        message_id,
        evidence,
    })
}

fn decode_payload(payload: &[u8]) -> Result<(Envelope, &[u8])> {
    let position = payload
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or_else(|| Error::Internal("invalid stored send payload".to_owned()))?;
    let data: EnvelopeData = serde_json::from_slice(&payload[..position])
        .map_err(|_| Error::Internal("stored envelope decode failed".to_owned()))?;
    let from: Address = data
        .from
        .parse()
        .map_err(|_| Error::Internal("invalid stored envelope".to_owned()))?;
    let to = data
        .to
        .into_iter()
        .map(|address| address.parse::<Address>())
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| Error::Internal("invalid stored envelope".to_owned()))?;
    let envelope = Envelope::new(Some(from), to)
        .map_err(|_| Error::Internal("invalid stored envelope".to_owned()))?;
    Ok((envelope, &payload[position + 1..]))
}

fn native_mime(envelope: &Envelope, raw: &[u8]) -> Result<Vec<u8>> {
    let parsed = crate::mail::parse(raw)?;
    let visible: std::collections::HashSet<String> = ["to", "cc"]
        .into_iter()
        .flat_map(|key| parsed[key].as_array().into_iter().flatten())
        .filter_map(|v| v["address"].as_str().map(str::to_ascii_lowercase))
        .collect();
    let hidden: Vec<String> = envelope
        .to()
        .iter()
        .map(ToString::to_string)
        .filter(|v| !visible.contains(&v.to_ascii_lowercase()))
        .collect();
    if hidden.is_empty() {
        return Ok(raw.to_vec());
    }
    let mut mime = format!("Bcc: {}\r\n", hidden.join(", ")).into_bytes();
    mime.extend_from_slice(raw);
    Ok(mime)
}

pub async fn enqueue(
    db: &Db,
    config: &Config,
    account_id: &str,
    app_id: &str,
    key: &str,
    request: SendRequest,
) -> Result<SendStatus> {
    if key.is_empty() || key.len() > 128 || key.chars().any(char::is_control) {
        return Err(invalid(
            "idempotency key must contain 1 to 128 printable bytes",
        ));
    }
    let (mut request, request_hash) = tokio::task::spawn_blocking(move || -> Result<_> {
        let bytes = serde_json::to_vec(&request)
            .map_err(|_| Error::Internal("request serialization failed".into()))?;
        let hash = Sha256::digest(bytes).to_vec();
        Ok((request, hash))
    })
    .await
    .map_err(|_| Error::Internal("request fingerprint task failed".into()))??;
    if let Some((id, stored_hash)) = existing(db, account_id, app_id, key).await? {
        if stored_hash != request_hash {
            return Err(Error::Conflict(
                "idempotency key was used with another request".to_owned(),
            ));
        }
        return get(db, &id).await;
    }
    let account = db.get_account(account_id).await?;
    let mut thread_id = None;
    if let Some(source) = &request.reply_to_message_id {
        let source: Option<Value> =
            sqlx::query_scalar("SELECT metadata FROM mail_messages WHERE id=? AND account_id=?")
                .bind(source)
                .bind(account_id)
                .fetch_optional(&db.pool)
                .await?;
        let metadata = source.ok_or(Error::NotFound)?;
        let message_id = metadata["message_id"]
            .as_str()
            .filter(|v| valid_identifier(v))
            .ok_or_else(|| invalid("source message has no usable RFC Message-ID"))?
            .to_owned();
        if request
            .in_reply_to
            .as_ref()
            .is_some_and(|v| v != &message_id)
        {
            return Err(invalid("in_reply_to conflicts with source message"));
        }
        let mut references: Vec<String> = metadata["references"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
        if !references.contains(&message_id) {
            references.push(message_id.clone());
        }
        if !request.references.is_empty() && request.references != references {
            return Err(invalid("references conflict with source message"));
        }
        request.in_reply_to = Some(message_id);
        request.references = references;
        thread_id = metadata["thread_id"]
            .as_str()
            .or_else(|| metadata["threadId"].as_str())
            .map(str::to_owned);
    }
    if let Some(retry) = &request.retry_of_send_id {
        let original = get(db, retry).await?;
        if original.account_id != account_id || original.status != "unknown" {
            return Err(invalid(
                "retry_of_send_id must reference an unknown send for this account",
            ));
        }
    }
    let retry_of = request.retry_of_send_id.clone();
    let max_bytes = if account.config.backend.is_native() {
        usize::MAX
    } else {
        config.max_message_bytes
    };
    let limits = crate::sending_limits::SendingLimits::for_backend(account.config.backend, config);
    let backend = account.config.backend;
    if backend == crate::db::Backend::Google {
        crate::sending_limits::validate_google_thread(thread_id.as_deref())?;
    }
    let crypto = db.crypto.clone();
    let prepared = tokio::task::spawn_blocking(move || -> Result<Prepared> {
        let mut prepared = prepare(request, max_bytes).map_err(|e| match e {
            Error::SendLimitExceeded {
                actual_mime_bytes, ..
            } => Error::SendLimitExceeded {
                backend: backend.as_str(),
                maximum_mime_bytes: limits.max_mime_bytes,
                actual_mime_bytes,
            },
            e => e,
        })?;
        let (envelope, raw) = decode_payload(&prepared.payload)?;
        if backend.is_native() {
            let native = native_mime(&envelope, raw)?;
            limits.validate_mime(&native)?;
            if backend == crate::db::Backend::Microsoft {
                crate::graph_message::GraphMessage::parse(&native)?;
            }
        } else {
            limits.validate_mime(raw)?;
        }
        prepared.payload = crypto.encrypt(&prepared.payload)?;
        Ok(prepared)
    })
    .await
    .map_err(|_| Error::Internal("message preparation task failed".to_owned()))??;
    let id = Uuid::new_v4().to_string();
    let mut send_evidence = prepared.evidence;
    send_evidence["backend"] = json!(account.config.backend);
    send_evidence["append_target"] = append_target(&account.config.imap);
    send_evidence["target_mailbox"] = json!(
        account
            .config
            .target_mailbox
            .as_deref()
            .unwrap_or(&account.config.smtp.username)
    );
    let payload = prepared.payload;
    let mut tx = db.pool.begin().await?;
    let inserted = sqlx::query("INSERT INTO sends (id,account_id,app_id,idempotency_key,request_hash,status,attempts,next_attempt_at,created_at,updated_at,expires_at,message_id,append_status,append_mailbox,evidence,thread_id,retry_of_send_id) VALUES (?,?,?,?,?,'queued',0,UTC_TIMESTAMP(6),UTC_TIMESTAMP(6),UTC_TIMESTAMP(6),DATE_ADD(UTC_TIMESTAMP(6),INTERVAL 24 HOUR),?,?,?,?,?,?)")
        .bind(&id).bind(account_id).bind(app_id).bind(key).bind(&request_hash).bind(&prepared.message_id)
        .bind(if account.config.backend==crate::db::Backend::Imap && account.config.sent_folder.is_some() { "pending" } else { "disabled" })
        .bind(if account.config.backend==crate::db::Backend::Imap {account.config.sent_folder.as_ref()} else {None})
        .bind(&send_evidence).bind(thread_id).bind(retry_of)
        .execute(&mut *tx).await;
    if let Err(error) = inserted {
        tx.rollback().await?;
        if error
            .as_database_error()
            .is_some_and(|error| error.is_unique_violation())
            && let Some((id, stored_hash)) = existing(db, account_id, app_id, key).await?
        {
            if stored_hash == request_hash {
                return get(db, &id).await;
            }
            return Err(Error::Conflict(
                "idempotency key was used with another request".to_owned(),
            ));
        }
        return Err(error.into());
    }
    sqlx::query("INSERT INTO send_blobs(send_id,payload) VALUES (?,?)")
        .bind(&id)
        .bind(payload)
        .execute(&mut *tx)
        .await?;
    if backend == crate::db::Backend::Microsoft {
        crate::graph_store::create_in(db, &mut tx, &id).await?;
    }
    tx.commit().await?;
    get(db, &id).await
}

async fn existing(
    db: &Db,
    account_id: &str,
    app_id: &str,
    key: &str,
) -> Result<Option<(String, Vec<u8>)>> {
    Ok(sqlx::query_as(
        "SELECT id,request_hash FROM sends WHERE account_id=? AND app_id=? AND idempotency_key=?",
    )
    .bind(account_id)
    .bind(app_id)
    .bind(key)
    .fetch_optional(&db.pool)
    .await?)
}

pub async fn get(db: &Db, id: &str) -> Result<SendStatus> {
    sqlx::query_as("SELECT id,account_id,status,attempts,created_at,updated_at,last_error,message_id,append_status,append_last_error,append_reconciliation_state,append_reconciliation_evidence,append_attempts,CASE WHEN expires_at>UTC_TIMESTAMP(6) AND EXISTS(SELECT 1 FROM send_blobs b WHERE b.send_id=sends.id) AND append_status IN ('pending','appending','failed','unknown') THEN expires_at ELSE NULL END AS append_recoverable_until,provider_id,(SELECT phase FROM graph_send_state g WHERE g.send_id=sends.id) AS graph_phase,reconciliation_state,resolution,reconciliation_evidence,resolution_reason,resolution_evidence,submitted_at,resolved_at FROM sends WHERE id=?").bind(id).fetch_optional(&db.pool).await?.ok_or(Error::NotFound)
}

fn corroborates(raw: &[u8], message_id: &str, evidence: &Value) -> Result<bool> {
    let metadata = crate::mail::parse(raw)?;
    if metadata["message_id"].as_str() != Some(message_id)
        || content_fingerprint(raw)? != evidence["content_fingerprint"]
    {
        return Ok(false);
    }
    let expected_from = evidence["from"].as_str().unwrap_or_default();
    if !metadata["from"].as_array().into_iter().flatten().any(|v| {
        v["address"]
            .as_str()
            .is_some_and(|a| a.eq_ignore_ascii_case(expected_from))
    }) {
        return Ok(false);
    }
    let expected: std::collections::HashSet<String> = evidence["recipients"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_ascii_lowercase))
        .collect();
    let visible: Vec<String> = ["to", "cc", "bcc"]
        .into_iter()
        .flat_map(|key| metadata[key].as_array().into_iter().flatten())
        .filter_map(|v| v["address"].as_str().map(str::to_ascii_lowercase))
        .collect();
    Ok(!visible.is_empty() && visible.iter().all(|value| expected.contains(value)))
}

async fn corroborates_owned(raw: Vec<u8>, message_id: String, evidence: Value) -> Result<bool> {
    tokio::task::spawn_blocking(move || corroborates(&raw, &message_id, &evidence))
        .await
        .map_err(|_| Error::Internal("send evidence parsing task failed".into()))?
}

pub async fn reconcile(
    db: &Db,
    config: Arc<Config>,
    accounts: &Accounts,
    id: &str,
) -> Result<Value> {
    let status = get(db, id).await?;
    if status.status != "unknown" || status.resolution.is_some() {
        return Err(Error::Conflict(
            "only unknown sends can be reconciled".into(),
        ));
    }
    let row = sqlx::query("SELECT evidence,submitted_at FROM sends WHERE id=?")
        .bind(id)
        .fetch_one(&db.pool)
        .await?;
    let evidence: Option<Value> = row.try_get("evidence")?;
    let submitted_at: Option<NaiveDateTime> = row.try_get("submitted_at")?;
    let account = db.get_account(&status.account_id).await?;
    let mut matches = Vec::new();
    let mut unavailable = false;
    let mut ambiguous = false;
    if let Some(evidence) = evidence {
        if account.config.backend == crate::db::Backend::Imap {
            if let Some(sent) = &account.config.sent_folder {
                match accounts
                    .execute(
                        &status.account_id,
                        Command::SearchMessageId {
                            mailbox: sent.clone(),
                            message_id: status.message_id.clone(),
                        },
                    )
                    .await
                {
                    Ok(crate::imap::Reply::Matches { uidvalidity, uids }) => {
                        if uids.len() >= 100 {
                            ambiguous = true;
                        }
                        for uid in uids {
                            match accounts
                                .execute(
                                    &status.account_id,
                                    Command::Content {
                                        mailbox: sent.clone(),
                                        uidvalidity,
                                        uid,
                                    },
                                )
                                .await
                            {
                                Ok(crate::imap::Reply::Content(raw)) => {
                                    if corroborates_owned(
                                        raw,
                                        status.message_id.clone(),
                                        evidence.clone(),
                                    )
                                    .await?
                                    {
                                        matches.push(json!({"location":sent,"uidvalidity":uidvalidity,"uid":uid,"evidence":["exact_message_id","sender","recipients","content_fingerprint"]}));
                                    } else {
                                        ambiguous = true;
                                    }
                                }
                                Ok(_) => ambiguous = true,
                                Err(_) => unavailable = true,
                            }
                        }
                    }
                    _ => unavailable = true,
                }
            } else {
                unavailable = true;
            }
        } else {
            let client = crate::providers::NativeClient::new(config, accounts.oauth.clone());
            let expected_message_id = evidence
                .get("provider_message_id")
                .and_then(Value::as_str)
                .unwrap_or(&status.message_id)
                .to_owned();
            let direct = if account.config.backend == crate::db::Backend::Microsoft {
                if let Some(draft_id) = &status.provider_id {
                    client.graph_sent_candidate(&account, draft_id).await
                } else {
                    Ok(None)
                }
            } else {
                Ok(None)
            };
            let candidates = match direct {
                Ok(Some(candidate)) => Ok(vec![candidate]),
                Ok(None) => {
                    client
                        .reconcile_candidates(&account, &expected_message_id)
                        .await
                }
                Err(error) => Err(error),
            };
            match candidates {
                Ok(candidates) => {
                    if candidates.len() > 100 {
                        ambiguous = true;
                    }
                    for candidate in candidates.into_iter().take(100) {
                        let observed = candidate.metadata["sent_at"]
                            .as_str()
                            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                            .map(|date| date.naive_utc())
                            .unwrap_or(candidate.received_at.naive_utc());
                        if submitted_at.is_some_and(|at| (at - observed).num_hours().abs() > 24) {
                            continue;
                        }
                        let expected_id =
                            if account.config.backend == crate::db::Backend::Microsoft
                                && status.provider_id.as_deref() == Some(candidate.id.as_str())
                            {
                                candidate.metadata["message_id"]
                                    .as_str()
                                    .unwrap_or(&expected_message_id)
                            } else {
                                &expected_message_id
                            }
                            .to_owned();
                        match client.fetch(&account, &candidate.id).await {
                            Ok(raw) => {
                                if corroborates_owned(raw, expected_id, evidence.clone()).await? {
                                    matches.push(json!({"provider_id":candidate.id,"locations":candidate.locations,"evidence":["exact_message_id","sender","recipients","content_fingerprint"]}));
                                } else {
                                    ambiguous = true;
                                }
                            }
                            Err(_) => unavailable = true,
                        }
                    }
                }
                Err(_) => unavailable = true,
            }
        }
    } else {
        unavailable = true;
    }
    let state = if matches.len() == 1 && !ambiguous && !unavailable {
        "matched"
    } else if matches.len() > 1 || ambiguous {
        "ambiguous"
    } else if unavailable {
        "unavailable"
    } else {
        "not_found"
    };
    let result = json!({"state":state,"matches":matches,"observed_at":Utc::now(),"meaning":"Sent copy evidence; does not establish recipient delivery","safe_to_retry":false});
    let changed=sqlx::query("UPDATE sends SET reconciliation_state=?,reconciliation_evidence=?,reconciled_at=UTC_TIMESTAMP(6) WHERE id=? AND status='unknown' AND resolution IS NULL")
        .bind(state).bind(&result).bind(id).execute(&db.pool).await?;
    if changed.rows_affected() != 1 {
        return Err(Error::Conflict(
            "send was resolved while reconciliation was running".into(),
        ));
    }
    Ok(result)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveRequest {
    pub decision: String,
    pub reason: String,
    pub evidence: Value,
}

pub async fn resolve(db: &Db, id: &str, input: ResolveRequest) -> Result<SendStatus> {
    if !matches!(input.decision.as_str(), "sent" | "unresolved")
        || input.reason.trim().is_empty()
        || input.reason.len() > 1024
        || input.reason.chars().any(char::is_control)
        || input
            .evidence
            .as_object()
            .is_none_or(|value| value.is_empty())
        || serde_json::to_vec(&input.evidence).map_or(true, |v| v.len() > 65536)
    {
        return Err(invalid(
            "resolution requires decision sent/unresolved, a reason, and bounded evidence",
        ));
    }
    let mut tx = db.pool.begin().await?;
    let row = sqlx::query(
        "SELECT account_id,status,resolution,reconciliation_state FROM sends WHERE id=? FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(Error::NotFound)?;
    if row.try_get::<String, _>("status")? != "unknown"
        || row.try_get::<Option<String>, _>("resolution")?.is_some()
    {
        return Err(Error::Conflict(
            "send is not an unresolved unknown outcome".into(),
        ));
    }
    if input.decision == "sent"
        && row
            .try_get::<Option<String>, _>("reconciliation_state")?
            .as_deref()
            != Some("matched")
    {
        return Err(Error::Conflict(
            "marking sent requires matched reconciliation evidence".into(),
        ));
    }
    sqlx::query("UPDATE sends SET resolution=?,resolution_reason=?,resolution_evidence=?,resolved_at=UTC_TIMESTAMP(6),updated_at=UTC_TIMESTAMP(6) WHERE id=?")
        .bind(&input.decision).bind(&input.reason).bind(&input.evidence).bind(id).execute(&mut *tx).await?;
    let account: String = row.try_get("account_id")?;
    db.emit(
        &mut tx,
        &account,
        "send.resolved",
        json!({"send_id":id,"resolution":input.decision,"reason":input.reason}),
    )
    .await?;
    tx.commit().await?;
    get(db, id).await
}

async fn open_connection(
    server: &MailServer,
    config: &Config,
) -> std::result::Result<AsyncSmtpConnection, Outcome> {
    let addresses =
        crate::security::resolve_public(&server.host, server.port, config.allow_private_networks)
            .await
            .map_err(|error| match error {
                Error::Unavailable(_) => Outcome::Retry("dns_failed"),
                _ => Outcome::Failed("destination_rejected"),
            })?;
    let mut tls = TlsParameters::builder(server.host.clone());
    if let Some(pem) = crate::security::extra_ca_pem(config.tls_ca_file.as_deref())
        .await
        .map_err(|_| Outcome::Failed("tls_configuration"))?
    {
        tls = tls.add_root_certificate(
            Certificate::from_pem(&pem).map_err(|_| Outcome::Failed("tls_configuration"))?,
        );
    }
    let tls = tls
        .build()
        .map_err(|_| Outcome::Failed("tls_configuration"))?;
    let implicit = server.tls_mode == "implicit";
    let mut client = AsyncSmtpConnection::connect_tokio1(
        addresses.as_slice(),
        Some(Duration::from_secs(30)),
        &ClientId::default(),
        if implicit { Some(tls.clone()) } else { None },
        None,
    )
    .await
    .map_err(|error| {
        if error.is_permanent() || error.is_tls() {
            Outcome::Failed("smtp_connect_rejected")
        } else {
            Outcome::Retry("smtp_connect_failed")
        }
    })?;
    if !implicit {
        if !client.can_starttls() {
            return Err(Outcome::Failed("starttls_required"));
        }
        client
            .starttls(tls, &ClientId::default())
            .await
            .map_err(|_| Outcome::Failed("tls_failed"))?;
    }
    Ok(client)
}

fn oauth_outcome(error: Error) -> Outcome {
    match error {
        Error::OAuth {
            code: "reauthorization_required",
            ..
        } => Outcome::Retry("reauthorization_required"),
        Error::OAuth {
            retryable: false, ..
        } => Outcome::Retry("authorization_failed"),
        _ => Outcome::Retry("oauth_unavailable"),
    }
}

async fn connection(
    server: &MailServer,
    config: &Config,
    oauth: &crate::oauth::OAuth,
) -> std::result::Result<AsyncSmtpConnection, Outcome> {
    if let Some(id) = server.oauth_credential() {
        oauth
            .validate_mail_server(server, crate::oauth::MailProtocol::Smtp)
            .await
            .map_err(oauth_outcome)?;
        for force in [false, true] {
            let token = oauth.token(id, force).await.map_err(oauth_outcome)?;
            let mut client = open_connection(server, config).await?;
            match client
                .auth(
                    &[Mechanism::Xoauth2],
                    &Credentials::new(server.username.clone(), token),
                )
                .await
            {
                Ok(_) => return Ok(client),
                Err(error) if error.is_permanent() && !force => continue,
                Err(error) if error.is_permanent() => {
                    return Err(Outcome::Retry("smtp_oauth_permission_denied"));
                }
                Err(_) => return Err(Outcome::Retry("smtp_auth_unavailable")),
            }
        }
        return Err(Outcome::Retry("smtp_auth_failed"));
    }
    let mut client = open_connection(server, config).await?;
    client
        .auth(
            &[Mechanism::Plain, Mechanism::Login],
            &Credentials::new(server.username.clone(), server.password.clone()),
        )
        .await
        .map_err(|error| {
            if error.is_permanent() {
                Outcome::Failed("smtp_auth_failed")
            } else {
                Outcome::Retry("smtp_auth_unavailable")
            }
        })?;
    Ok(client)
}

pub async fn validate(
    db: &Db,
    config: &Config,
    oauth: &crate::oauth::OAuth,
    account_id: &str,
) -> Result<()> {
    let account = db.get_account(account_id).await?;
    let revision = match account.config.smtp.oauth_credential() {
        Some(id) => Some(crate::oauth::store::get(db, id).await?.revision),
        None => None,
    };
    let result = timeout(
        Duration::from_secs(60),
        connection(&account.config.smtp, config, oauth),
    )
    .await;
    match result {
        Ok(Ok(mut client)) => {
            db.smtp_validation_revision(account_id, &account.config.smtp, revision, true, None)
                .await?;
            let _ = timeout(Duration::from_secs(5), client.quit()).await;
        }
        Ok(Err(
            Outcome::Failed(reason) | Outcome::Retry(reason) | Outcome::RetryAfter(reason, _),
        )) => {
            db.smtp_validation_revision(
                account_id,
                &account.config.smtp,
                revision,
                false,
                Some(reason),
            )
            .await?
        }
        _ => {
            db.smtp_validation_revision(
                account_id,
                &account.config.smtp,
                revision,
                false,
                Some("smtp_validation_timeout"),
            )
            .await?
        }
    }
    Ok(())
}

async fn submit(
    server: &MailServer,
    config: &Config,
    oauth: &crate::oauth::OAuth,
    envelope: &Envelope,
    raw: &[u8],
) -> Outcome {
    let mut client = match timeout(Duration::from_secs(60), connection(server, config, oauth)).await
    {
        Ok(Ok(client)) => client,
        Ok(Err(outcome)) => return outcome,
        Err(_) => return Outcome::Retry("smtp_connect_timeout"),
    };
    match timeout(Duration::from_secs(600), client.send(envelope, raw)).await {
        Ok(Ok(_)) => Outcome::Accepted,
        Ok(Err(error)) => classify_send_failure(&error),
        _ => Outcome::Unknown,
    }
}

fn classify_send_failure(error: &lettre::transport::smtp::Error) -> Outcome {
    if error.is_transient() {
        Outcome::Retry("smtp_temporary_rejection")
    } else if error.is_permanent() || error.is_client() {
        Outcome::Failed("smtp_rejected")
    } else {
        Outcome::Unknown
    }
}

async fn claim(db: &Db) -> Result<Option<Job>> {
    let mut tx = db.pool.begin().await?;
    let mut job: Option<Job> = sqlx::query_as("SELECT id,account_id,status,attempts,expires_at,append_mailbox,append_status,append_revision FROM sends WHERE next_attempt_at<=UTC_TIMESTAMP(6) AND (status='queued' OR (status='accepted' AND append_status='pending')) ORDER BY next_attempt_at,id LIMIT 1 FOR UPDATE SKIP LOCKED").fetch_optional(&mut *tx).await?;
    if let Some(job) = &mut job {
        if job.status == "queued" {
            sqlx::query("UPDATE sends SET status='sending',attempts=attempts+1,updated_at=UTC_TIMESTAMP(6) WHERE id=?").bind(&job.id).execute(&mut *tx).await?;
        } else {
            sqlx::query(
                "UPDATE sends SET append_status='appending',append_attempts=append_attempts+1,append_revision=append_revision+1,append_last_error=NULL,append_reconciliation_state=NULL,append_reconciliation_evidence=NULL,updated_at=UTC_TIMESTAMP(6) WHERE id=?",
            )
            .bind(&job.id)
            .execute(&mut *tx)
            .await?;
            job.append_revision = job.append_revision.saturating_add(1);
            job.append_status = "appending".into();
        }
    }
    tx.commit().await?;
    Ok(job)
}

async fn finish(db: &Db, job: &Job, outcome: Outcome) -> Result<()> {
    let mut tx = db.pool.begin().await?;
    let (state, reason, event) = match outcome {
        Outcome::Accepted => ("accepted", None, Some("send.accepted")),
        Outcome::Failed(reason) => ("failed", Some(reason), Some("send.failed")),
        Outcome::Unknown => (
            "unknown",
            Some("submission_outcome_unknown"),
            Some("send.unknown"),
        ),
        Outcome::Retry(reason) | Outcome::RetryAfter(reason, _)
            if job.expires_at > Utc::now().naive_utc() =>
        {
            ("queued", Some(reason), None)
        }
        Outcome::Retry(_) | Outcome::RetryAfter(_, _) => {
            ("failed", Some("retry_window_expired"), Some("send.failed"))
        }
    };
    let delay = match outcome {
        Outcome::RetryAfter(_, seconds) => {
            retry_seconds(job.attempts).max(seconds.min(i64::MAX as u64) as i64)
        }
        _ => retry_seconds(job.attempts),
    };
    sqlx::query("UPDATE sends SET status=?,last_error=?,updated_at=UTC_TIMESTAMP(6),next_attempt_at=DATE_ADD(UTC_TIMESTAMP(6),INTERVAL ? SECOND) WHERE id=?")
        .bind(state).bind(reason).bind(if state == "accepted" { 0 } else { delay }).bind(&job.id).execute(&mut *tx).await?;
    if let Some(event) = event {
        db.emit(
            &mut tx,
            &job.account_id,
            event,
            json!({"send_id":job.id,"status":state,"reason":reason}),
        )
        .await?;
    }
    if state != "queued" && !(state == "accepted" && job.append_mailbox.is_some()) {
        sqlx::query("DELETE FROM send_blobs WHERE send_id=?")
            .bind(&job.id)
            .execute(&mut *tx)
            .await?;
        if state != "accepted" {
            sqlx::query("UPDATE sends SET append_status='disabled' WHERE id=?")
                .bind(&job.id)
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

fn retry_seconds(attempts: u32) -> i64 {
    (5i64.saturating_mul(1i64 << attempts.clamp(0, 10))).min(3600)
}

pub(crate) fn append_target(server: &MailServer) -> Value {
    json!({"host":server.host.to_ascii_lowercase(),"port":server.port,"username":server.username,"tls_mode":server.tls_mode,"credential_id":server.oauth_credential()})
}

async fn stored_append_target(db: &Db, id: &str) -> Result<Option<Value>> {
    let evidence: Option<Value> = sqlx::query_scalar("SELECT evidence FROM sends WHERE id=?")
        .bind(id)
        .fetch_optional(&db.pool)
        .await?
        .ok_or(Error::NotFound)?;
    Ok(evidence
        .and_then(|e| e.get("append_target").cloned())
        .filter(|target| !target.is_null()))
}

async fn append_target_matches(db: &Db, account_id: &str, send_id: &str) -> Result<bool> {
    let evidence: Option<Value> =
        sqlx::query_scalar("SELECT evidence FROM sends WHERE id=? AND account_id=?")
            .bind(send_id)
            .bind(account_id)
            .fetch_optional(&db.pool)
            .await?
            .ok_or(Error::NotFound)?;
    let Some(target) = evidence
        .as_ref()
        .and_then(|e| e.get("append_target"))
        .filter(|value| !value.is_null())
    else {
        return Ok(true);
    };
    let account = db.get_account(account_id).await?;
    Ok(account.config.backend == crate::db::Backend::Imap
        && *target == append_target(&account.config.imap))
}

async fn require_append_target(db: &Db, status: &SendStatus) -> Result<()> {
    if append_target_matches(db, &status.account_id, &status.id).await? {
        return Ok(());
    }
    sqlx::query("UPDATE sends SET append_last_error='append_configuration_changed',updated_at=UTC_TIMESTAMP(6) WHERE id=? AND status='accepted' AND append_status IN ('failed','unknown','expired')").bind(&status.id).execute(&db.pool).await?;
    Err(Error::Conflict(
        "Sent-copy account configuration changed; recovery is blocked".into(),
    ))
}

fn append_result(result: Result<crate::imap::Reply>) -> (&'static str, Option<&'static str>) {
    match result {
        Ok(_) => ("complete", None),
        Err(Error::UnknownOutcome) => ("unknown", Some("append_outcome_unknown")),
        Err(Error::Conflict(_)) => ("failed", Some("append_configuration_changed")),
        Err(Error::Invalid(_) | Error::Unsupported(_)) => ("failed", Some("append_rejected")),
        Err(Error::Unavailable(_)) => ("failed", Some("append_not_started")),
        Err(_) => ("unknown", Some("append_outcome_unknown")),
    }
}

async fn record_append(db: &Db, job: &Job, status: &str, reason: Option<&str>) -> Result<()> {
    let mut tx = db.pool.begin().await?;
    let changed=sqlx::query("UPDATE sends SET append_status=?,append_last_error=?,updated_at=UTC_TIMESTAMP(6) WHERE id=? AND status='accepted' AND append_status='appending' AND append_revision=?")
        .bind(status).bind(reason).bind(&job.id).bind(job.append_revision).execute(&mut *tx).await?;
    if changed.rows_affected() == 0 {
        return Ok(());
    }
    if status == "complete" || job.expires_at <= Utc::now().naive_utc() {
        sqlx::query("DELETE FROM send_blobs WHERE send_id=?")
            .bind(&job.id)
            .execute(&mut *tx)
            .await?;
    }
    if status != "complete" {
        db.emit(&mut tx,&job.account_id,"send.append_failed",json!({"send_id":job.id,"status":"accepted","append_status":status,"reason":reason,"recoverable_until":if job.expires_at>Utc::now().naive_utc(){Some(job.expires_at.and_utc())}else{None}})).await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn append(db: &Db, accounts: &Accounts, job: &Job, raw: Vec<u8>) -> Result<()> {
    let Some(mailbox) = &job.append_mailbox else {
        return record_append(db, job, "failed", Some("append_folder_missing")).await;
    };
    if job.expires_at <= Utc::now().naive_utc() {
        return record_append(db, job, "expired", Some("append_recovery_expired")).await;
    }
    if !append_target_matches(db, &job.account_id, &job.id).await? {
        return record_append(db, job, "failed", Some("append_configuration_changed")).await;
    }
    let (status, reason) = append_result(
        accounts
            .execute(
                &job.account_id,
                Command::Append {
                    mailbox: mailbox.clone(),
                    raw,
                    expected_target: stored_append_target(db, &job.id).await?,
                },
            )
            .await,
    );
    record_append(db, job, status, reason).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppendRetryRequest {
    #[serde(default)]
    pub allow_duplicate_copy: bool,
    pub reason: Option<String>,
}

pub async fn reconcile_append(db: &Db, accounts: &Accounts, id: &str) -> Result<SendStatus> {
    let status = get(db, id).await?;
    if status.status != "accepted"
        || !matches!(
            status.append_status.as_str(),
            "failed" | "unknown" | "expired"
        )
    {
        return Err(Error::Conflict(
            "only failed or unknown accepted-mail copies can be reconciled".into(),
        ));
    }
    require_append_target(db, &status).await?;
    let row = sqlx::query("SELECT append_mailbox,evidence,append_revision FROM sends WHERE id=?")
        .bind(id)
        .fetch_one(&db.pool)
        .await?;
    let mailbox: Option<String> = row.try_get("append_mailbox")?;
    let expected: Option<Value> = row.try_get("evidence")?;
    let revision: u64 = row.try_get("append_revision")?;
    let mut matches = Vec::new();
    let mut unavailable = false;
    let mut ambiguous = false;
    if let (Some(mailbox), Some(expected)) = (mailbox, expected) {
        match accounts
            .execute(
                &status.account_id,
                Command::SearchMessageId {
                    mailbox: mailbox.clone(),
                    message_id: status.message_id.clone(),
                },
            )
            .await
        {
            Ok(crate::imap::Reply::Matches { uidvalidity, uids }) => {
                if uids.len() >= 100 {
                    ambiguous = true;
                }
                for uid in uids {
                    match accounts
                        .execute(
                            &status.account_id,
                            Command::Content {
                                mailbox: mailbox.clone(),
                                uidvalidity,
                                uid,
                            },
                        )
                        .await
                    {
                        Ok(crate::imap::Reply::Content(raw)) => {
                            match corroborates_owned(raw,status.message_id.clone(),expected.clone()).await {
                                Ok(true)=>matches.push(json!({"mailbox":mailbox,"uidvalidity":uidvalidity,"uid":uid,"evidence":["exact_message_id","sender","recipients","content_fingerprint"]})),
                                Ok(false)|Err(_)=>ambiguous=true,
                            }
                        }
                        _ => unavailable = true,
                    }
                }
            }
            _ => unavailable = true,
        }
    } else {
        unavailable = true;
    }
    let state = if ambiguous || matches.len() > 1 {
        "ambiguous"
    } else if unavailable {
        "unavailable"
    } else if matches.len() == 1 {
        "matched"
    } else {
        "not_found"
    };
    let evidence = json!({"result":state,"matches":matches,"observed_at":Utc::now(),"absence_does_not_authorize_retry":true});
    require_append_target(db, &status).await?;
    apply_append_reconciliation(db, &status, revision, state, evidence).await?;
    get(db, id).await
}

async fn apply_append_reconciliation(
    db: &Db,
    status: &SendStatus,
    revision: u64,
    state: &str,
    evidence: Value,
) -> Result<()> {
    let mut tx = db.pool.begin().await?;
    let changed=sqlx::query("UPDATE sends SET append_reconciliation_state=?,append_reconciliation_evidence=?,append_status=IF(?='matched','complete',append_status),append_last_error=IF(?='matched',NULL,append_last_error),updated_at=UTC_TIMESTAMP(6) WHERE id=? AND status='accepted' AND append_status=? AND append_revision=?")
        .bind(state).bind(&evidence).bind(state).bind(state).bind(&status.id).bind(&status.append_status).bind(revision).execute(&mut *tx).await?;
    if changed.rows_affected() == 0 {
        return Err(Error::Conflict(
            "Sent-copy state changed during reconciliation".into(),
        ));
    }
    if state == "matched" {
        sqlx::query("DELETE FROM send_blobs WHERE send_id=?")
            .bind(&status.id)
            .execute(&mut *tx)
            .await?;
    }
    db.emit(&mut tx,&status.account_id,"send.append_reconciled",json!({"send_id":status.id,"status":"accepted","append_status":if state=="matched"{"complete"}else{&status.append_status},"reconciliation":evidence})).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn retry_append(
    db: &Db,
    accounts: &Accounts,
    id: &str,
    input: AppendRetryRequest,
) -> Result<SendStatus> {
    if input
        .reason
        .as_ref()
        .is_some_and(|reason| reason.len() > 1024 || reason.contains(['\r', '\n', '\0']))
    {
        return Err(Error::Invalid(
            "append retry reason must contain at most 1024 bytes without control delimiters".into(),
        ));
    }
    let mut status = get(db, id).await?;
    if status.status != "accepted" || !matches!(status.append_status.as_str(), "failed" | "unknown")
    {
        return Err(Error::Conflict(
            "only failed or unknown accepted-mail copies can be retried".into(),
        ));
    }
    require_append_target(db, &status).await?;
    let revision: u64 = sqlx::query_scalar("SELECT append_revision FROM sends WHERE id=?")
        .bind(id)
        .fetch_one(&db.pool)
        .await?;
    if status.append_status == "unknown" {
        status = reconcile_append(db, accounts, id).await?;
        if status.append_status == "complete" {
            return Ok(status);
        }
        if status.append_reconciliation_state.as_deref() != Some("not_found") {
            return Err(Error::Conflict(
                "Sent-copy reconciliation is inconclusive; retry is blocked".into(),
            ));
        }
        if !input.allow_duplicate_copy
            || input.reason.as_ref().is_none_or(|r| {
                r.trim().is_empty() || r.len() > 1024 || r.contains(['\r', '\n', '\0'])
            })
        {
            return Err(Error::Conflict("Sent absence does not prove APPEND failed; an explicit duplicate-copy acknowledgement and reason are required".into()));
        }
    }
    let mut tx = db.pool.begin().await?;
    let row=sqlx::query("SELECT append_status,append_revision,expires_at,EXISTS(SELECT 1 FROM send_blobs WHERE send_id=sends.id) AS payload_available FROM sends WHERE id=? AND status='accepted' FOR UPDATE").bind(id).fetch_optional(&mut *tx).await?.ok_or(Error::NotFound)?;
    if row.try_get::<String, _>("append_status")? != status.append_status
        || row.try_get::<u64, _>("append_revision")? != revision
    {
        return Err(Error::Conflict(
            "Sent-copy state changed; inspect it before retrying".into(),
        ));
    }
    if row.try_get::<NaiveDateTime, _>("expires_at")? <= Utc::now().naive_utc()
        || !row.try_get::<bool, _>("payload_available")?
    {
        return Err(Error::Conflict(
            "Sent-copy recovery artifact has expired".into(),
        ));
    }
    sqlx::query("UPDATE sends SET append_status='pending',append_revision=append_revision+1,append_last_error=NULL,next_attempt_at=UTC_TIMESTAMP(6),updated_at=UTC_TIMESTAMP(6) WHERE id=?").bind(id).execute(&mut *tx).await?;
    db.emit(&mut tx,&status.account_id,"send.append_retry_requested",json!({"send_id":id,"status":"accepted","allow_duplicate_copy":input.allow_duplicate_copy,"reason":input.reason})).await?;
    tx.commit().await?;
    get(db, id).await
}

async fn expire_payloads(db: &Db) -> Result<()> {
    let mut tx = db.pool.begin().await?;
    sqlx::query("UPDATE sends SET append_status='expired',append_last_error='append_recovery_expired',updated_at=UTC_TIMESTAMP(6) WHERE expires_at<=UTC_TIMESTAMP(6) AND status='accepted' AND append_status IN ('pending','failed','unknown')").execute(&mut *tx).await?;
    sqlx::query("DELETE b FROM send_blobs b JOIN sends s ON s.id=b.send_id WHERE s.expires_at<=UTC_TIMESTAMP(6)").execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

async fn native_sending_health(
    db: &Db,
    account: &crate::db::Account,
    revision: Option<u64>,
    outcome: Outcome,
) -> Result<()> {
    let (state, reason) = match outcome {
        Outcome::Accepted => ("validated", None),
        Outcome::Retry("authorization_failed") => ("failed", Some("authorization_failed")),
        _ => return Ok(()),
    };
    let mut tx = db.pool.begin().await?;
    if let Some(id) = account.config.credential_id.as_deref() {
        let current: Option<u64> =
            sqlx::query_scalar("SELECT revision FROM oauth_credentials WHERE id=? FOR SHARE")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        if current != revision {
            return Ok(());
        }
    }
    let previous: Option<String> =
        sqlx::query_scalar("SELECT smtp_state FROM accounts WHERE id=? FOR UPDATE")
            .bind(&account.id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(previous) = previous else {
        return Ok(());
    };
    if previous == state {
        return Ok(());
    }
    sqlx::query("UPDATE accounts SET smtp_state=?,updated_at=UTC_TIMESTAMP(6) WHERE id=?")
        .bind(state)
        .bind(&account.id)
        .execute(&mut *tx)
        .await?;
    db.emit(
        &mut tx,
        &account.id,
        if state == "validated" {
            "account.sending_validated"
        } else {
            "account.sending_failed"
        },
        json!({"previous_state":previous,"state":state,"reason":reason}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn process(db: &Db, config: Arc<Config>, accounts: &Accounts, job: Job) -> Result<()> {
    if job.expires_at <= Utc::now().naive_utc() && job.status != "accepted" {
        if let Ok(account) = db.get_account(&job.account_id).await
            && account.config.backend == crate::db::Backend::Microsoft
        {
            let client =
                crate::providers::NativeClient::new(config.clone(), accounts.oauth.clone());
            if let Err(error) = crate::graph_send::cleanup(db, &client, &account, &job.id).await {
                tracing::warn!(send_id=%job.id,error=%error,"expired Graph draft cleanup deferred");
            }
        }
        return finish(db, &job, Outcome::Failed("retry_window_expired")).await;
    }
    let encrypted: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT payload FROM send_blobs WHERE send_id=?")
            .bind(&job.id)
            .fetch_optional(&db.pool)
            .await?;
    let Some(encrypted) = encrypted else {
        if job.status == "accepted" {
            return record_append(
                db,
                &job,
                if job.expires_at <= Utc::now().naive_utc() {
                    "expired"
                } else {
                    "failed"
                },
                Some("append_payload_unavailable"),
            )
            .await;
        }
        return finish(db, &job, Outcome::Failed("payload_unavailable")).await;
    };
    let crypto = db.crypto.clone();
    let payload = tokio::task::spawn_blocking(move || crypto.decrypt(&encrypted))
        .await
        .map_err(|_| Error::Internal("payload decryption task failed".to_owned()))??;
    let (envelope, raw) = decode_payload(&payload)?;
    if job.status == "accepted" {
        return append(db, accounts, &job, raw.to_vec()).await;
    }
    let account = db.get_account(&job.account_id).await?;
    let revision = if let Some(id) = account.config.credential_id.as_deref() {
        Some(crate::oauth::store::get(db, id).await?.revision)
    } else {
        None
    };
    if account.config.backend != crate::db::Backend::Microsoft {
        sqlx::query("UPDATE sends SET submitted_at=UTC_TIMESTAMP(6) WHERE id=?")
            .bind(&job.id)
            .execute(&db.pool)
            .await?;
    }
    if account.config.backend == crate::db::Backend::Imap
        && crate::sending_limits::validate(account.config.backend, &config, raw).is_err()
    {
        return finish(db, &job, Outcome::Failed("send_limit_exceeded")).await;
    }
    let outcome = if account.config.backend == crate::db::Backend::Imap {
        submit(
            &account.config.smtp,
            &config,
            &accounts.oauth,
            &envelope,
            raw,
        )
        .await
    } else {
        let client = crate::providers::NativeClient::new(config.clone(), accounts.oauth.clone());
        let thread: Option<String> = sqlx::query_scalar("SELECT thread_id FROM sends WHERE id=?")
            .bind(&job.id)
            .fetch_one(&db.pool)
            .await?;
        let owned_raw = raw.to_vec();
        let native_raw = tokio::task::spawn_blocking(move || native_mime(&envelope, &owned_raw))
            .await
            .map_err(|_| Error::Internal("native MIME preparation task failed".into()))??;
        let submission = if account.config.backend == crate::db::Backend::Microsoft {
            match crate::sending_limits::validate(account.config.backend, &config, &native_raw) {
                Ok(()) => {
                    crate::graph_send::submit(db, &client, &account, &job.id, native_raw).await
                }
                Err(error) => Err(error),
            }
        } else {
            client.send(&account, &native_raw, thread.as_deref()).await
        };
        match submission {
            Ok(submission) => {
                sqlx::query(
                    "UPDATE sends SET provider_id=?,thread_id=COALESCE(?,thread_id) WHERE id=?",
                )
                .bind(submission.provider_id)
                .bind(submission.thread_id)
                .bind(&job.id)
                .execute(&db.pool)
                .await?;
                Outcome::Accepted
            }
            Err(error @ (Error::Database(_) | Error::Internal(_))) => return Err(error),
            Err(Error::UnknownOutcome) => Outcome::Unknown,
            Err(Error::SendLimitExceeded { .. } | Error::SendComponentLimitExceeded { .. }) => {
                Outcome::Failed("send_limit_exceeded")
            }
            Err(Error::Throttled(seconds)) => Outcome::RetryAfter("provider_throttled", seconds),
            Err(Error::Unavailable(_)) => Outcome::Retry("provider_unavailable"),
            Err(Error::OAuth { .. }) => Outcome::Retry("authorization_failed"),
            Err(_) => Outcome::Failed("provider_rejected"),
        }
    };
    if account.config.backend == crate::db::Backend::Microsoft
        && matches!(outcome, Outcome::Failed(_))
    {
        let client = crate::providers::NativeClient::new(config.clone(), accounts.oauth.clone());
        if let Err(error) = crate::graph_send::cleanup(db, &client, &account, &job.id).await {
            tracing::warn!(send_id=%job.id,error=%error,"Graph draft cleanup deferred");
        }
    }
    if account.config.backend.is_native() {
        native_sending_health(db, &account, revision, outcome).await?;
    }
    finish(db, &job, outcome).await
}

async fn recover_job(db: &Db, job: Job) -> Result<()> {
    if job.status == "sending" {
        use crate::graph_store::GraphPhase;
        match crate::graph_store::phase(db, &job.id).await? {
            Some(GraphPhase::Preparing | GraphPhase::Ready) => {
                sqlx::query("UPDATE sends SET status='queued',next_attempt_at=UTC_TIMESTAMP(6),updated_at=UTC_TIMESTAMP(6) WHERE id=? AND status='sending'").bind(&job.id).execute(&db.pool).await?;
            }
            Some(GraphPhase::Accepted) => finish(db, &job, Outcome::Accepted).await?,
            Some(GraphPhase::Abandoned) => {
                finish(db, &job, Outcome::Failed("graph_preparation_abandoned")).await?
            }
            _ => finish(db, &job, Outcome::Unknown).await?,
        }
    } else if job.append_status == "appending" {
        record_append(db, &job, "unknown", Some("interrupted_append")).await?;
    }
    Ok(())
}

async fn recover_id(db: &Db, id: &str) -> Result<()> {
    let job: Option<Job> = sqlx::query_as("SELECT id,account_id,status,attempts,expires_at,append_mailbox,append_status,append_revision FROM sends WHERE id=?")
        .bind(id).fetch_optional(&db.pool).await?;
    if let Some(job) = job {
        recover_job(db, job).await?;
    }
    Ok(())
}

pub async fn recover(db: &Db) -> Result<()> {
    let jobs: Vec<Job> = sqlx::query_as("SELECT id,account_id,status,attempts,expires_at,append_mailbox,append_status,append_revision FROM sends WHERE status='sending' OR append_status='appending'").fetch_all(&db.pool).await?;
    for job in jobs {
        recover_job(db, job).await?;
    }
    expire_payloads(db).await?;
    crate::graph_store::purge_expired_progress(db).await?;
    Ok(())
}

pub async fn run(
    db: Arc<Db>,
    config: Arc<Config>,
    accounts: Arc<Accounts>,
    shutdown: CancellationToken,
) -> Result<()> {
    recover(&db).await?;
    let mut tasks: JoinSet<(String, Result<()>)> = JoinSet::new();
    let mut recovery: VecDeque<String> = VecDeque::new();
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    let mut cleanup = tokio::time::interval(Duration::from_secs(60));
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            result = tasks.join_next(), if !tasks.is_empty() => {
                match result {
                    Some(Ok((id, Err(_)))) => {
                        tracing::error!(send_id = %id, "send processing interrupted; reconciling persisted outcome");
                        recovery.push_back(id);
                    }
                    Some(Err(_)) => return Err(Error::Internal("send task panicked".to_owned())),
                    _ => {}
                }
            }
            _ = cleanup.tick() => {
                if crate::graph_store::purge_expired_progress(&db).await.is_err(){tracing::error!("Graph upload secret cleanup failed");}
                if expire_payloads(&db).await.is_err() {tracing::error!("send artifact cleanup failed");}
                if sqlx::query("DELETE FROM sends WHERE created_at<DATE_SUB(UTC_TIMESTAMP(6),INTERVAL 30 DAY) AND status NOT IN ('queued','sending') AND append_status NOT IN ('pending','appending')").execute(&db.pool).await.is_err() {
                    tracing::error!("send metadata cleanup failed");
                }
            }
            _ = interval.tick(), if tasks.len() < config.smtp_concurrency => {
                if let Some(id) = recovery.front() {
                    if recover_id(&db, id).await.is_ok() { recovery.pop_front(); }
                    continue;
                }
                while tasks.len() < config.smtp_concurrency {
                  match claim(&db).await {
                    Ok(Some(job)) => {
                        let db = db.clone();
                        let config = config.clone();
                        let accounts = accounts.clone();
                        tasks.spawn(async move {
                            let id = job.id.clone();
                            (id, process(&db, config, &accounts, job).await)
                        });
                    }
                    Ok(None) => break,
                    Err(_) => { tracing::error!("send job claim failed"); break; }
                  }
                }
            }
        }
    }
    if timeout(Duration::from_secs(15), async {
        while tasks.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
    recover(&db).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> SendRequest {
        SendRequest {
            from: "sender@example.com".to_owned(),
            to: vec!["recipient@example.com".to_owned()],
            cc: vec![],
            bcc: vec!["hidden@example.com".to_owned()],
            subject: "Test".to_owned(),
            in_reply_to: None,
            references: vec![],
            reply_to: vec![],
            reply_to_message_id: None,
            retry_of_send_id: None,
            text: Some("Hello".to_owned()),
            html: None,
            attachments: vec![],
        }
    }

    #[test]
    fn bcc_stays_in_envelope_but_not_headers() {
        let prepared = prepare(request(), 1024 * 1024).unwrap();
        let (envelope, raw) = decode_payload(&prepared.payload).unwrap();
        assert_eq!(envelope.to().len(), 2);
        assert!(!String::from_utf8_lossy(raw).contains("Bcc:"));
    }

    #[test]
    fn enforces_encoded_size_limit() {
        assert!(prepare(request(), 10).is_err());
    }

    #[test]
    fn rejects_invalid_base64_attachment() {
        let mut message = request();
        message.attachments.push(Attachment {
            filename: "a.txt".to_owned(),
            content_type: "text/plain".to_owned(),
            data_base64: "%%%".to_owned(),
            disposition: Disposition::Attachment,
            content_id: None,
        });
        assert!(prepare(message, 1024 * 1024).is_err());
    }

    #[test]
    fn reply_headers_and_inline_parts_preserve_linkage() {
        let mut message = request();
        message.in_reply_to = Some("<parent@test>".into());
        message.references = vec!["<root@test>".into(), "<parent@test>".into()];
        message.reply_to = vec!["Replies <replies@test>".into()];
        message.html = Some("<p>Hello<img src=\"cid:photo@test\"></p>".into());
        message.attachments.push(Attachment {
            filename: "photo.png".into(),
            content_type: "image/png".into(),
            data_base64: STANDARD.encode(b"image"),
            disposition: Disposition::Inline,
            content_id: Some("photo@test".into()),
        });
        message.attachments.push(Attachment {
            filename: "file.txt".into(),
            content_type: "text/plain".into(),
            data_base64: STANDARD.encode(b"attachment"),
            disposition: Disposition::Attachment,
            content_id: None,
        });
        let prepared = prepare(message, 1024 * 1024).unwrap();
        let (_, raw) = decode_payload(&prepared.payload).unwrap();
        let parsed = crate::mail::parse(raw).unwrap();
        assert_eq!(parsed["in_reply_to"], json!(["<parent@test>"]));
        assert_eq!(
            parsed["references"],
            json!(["<root@test>", "<parent@test>"])
        );
        assert_eq!(parsed["reply_to"][0]["address"], "replies@test");
        let mime = String::from_utf8_lossy(raw);
        assert!(mime.contains("multipart/mixed"));
        assert!(mime.contains("multipart/related"));
        assert!(mime.contains("multipart/alternative"));
        assert!(
            parsed["attachments"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["content_id"] == "photo@test" && p["disposition"] == "inline")
        );
    }

    #[test]
    fn rejects_header_injection_bad_ids_and_duplicate_cids() {
        let mut message = request();
        message.subject = "hello\r\nBcc: attacker@test".into();
        assert!(prepare(message, 1024 * 1024).is_err());
        let mut message = request();
        message.in_reply_to = Some("not-an-id".into());
        assert!(prepare(message, 1024 * 1024).is_err());
        let mut message = request();
        message.html = Some("<img>".into());
        let image = Attachment {
            filename: "a.png".into(),
            content_type: "image/png".into(),
            data_base64: STANDARD.encode(b"a"),
            disposition: Disposition::Inline,
            content_id: Some("same".into()),
        };
        message.attachments = vec![image.clone(), image];
        assert!(prepare(message, 1024 * 1024).is_err());
    }

    #[test]
    fn sent_evidence_requires_exact_id_and_matching_content_sender_recipients() {
        let prepared = prepare(request(), 1024 * 1024).unwrap();
        let (_, raw) = decode_payload(&prepared.payload).unwrap();
        assert!(corroborates(raw, &prepared.message_id, &prepared.evidence).unwrap());
        assert!(!corroborates(raw, "<other@test>", &prepared.evidence).unwrap());
        let mut wrong = prepared.evidence.clone();
        wrong["content_fingerprint"] = json!("wrong");
        assert!(!corroborates(raw, &prepared.message_id, &wrong).unwrap());
        let mut wrong = prepared.evidence.clone();
        wrong["from"] = json!("other@test");
        assert!(!corroborates(raw, &prepared.message_id, &wrong).unwrap());
    }

    #[tokio::test]
    async fn lettre_rejected_recipient_aborts_before_data() {
        use tokio::{
            io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            stream
                .get_mut()
                .write_all(b"220 test ESMTP\r\n")
                .await
                .unwrap();
            let mut commands = Vec::new();
            let mut recipients = 0;
            loop {
                let mut line = String::new();
                if stream.read_line(&mut line).await.unwrap() == 0 {
                    break;
                }
                commands.push(line.clone());
                let reply: &[u8] = if line.starts_with("EHLO") {
                    b"250-test\r\n250 8BITMIME\r\n"
                } else if line.starts_with("RCPT") {
                    recipients += 1;
                    if recipients == 2 {
                        b"550 recipient rejected\r\n"
                    } else {
                        b"250 recipient ok\r\n"
                    }
                } else if line.starts_with("QUIT") {
                    stream
                        .get_mut()
                        .write_all(b"221 goodbye\r\n")
                        .await
                        .unwrap();
                    break;
                } else {
                    b"250 ok\r\n"
                };
                stream.get_mut().write_all(reply).await.unwrap();
            }
            commands
        });
        let mut client = AsyncSmtpConnection::connect_tokio1(
            address,
            Some(Duration::from_secs(5)),
            &ClientId::default(),
            None,
            None,
        )
        .await
        .unwrap();
        let prepared = prepare(request(), 1024 * 1024).unwrap();
        let (envelope, raw) = decode_payload(&prepared.payload).unwrap();
        let error = timeout(Duration::from_secs(5), client.send(&envelope, raw))
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.is_permanent());
        drop(client);
        let commands = timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            commands
                .iter()
                .filter(|command| command.starts_with("RCPT"))
                .count(),
            2
        );
        assert!(!commands.iter().any(|command| command.starts_with("DATA")));
    }

    #[tokio::test]
    async fn connection_lost_after_data_has_unknown_outcome() {
        use tokio::{
            io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            stream
                .get_mut()
                .write_all(b"220 test ESMTP\r\n")
                .await
                .unwrap();
            loop {
                let mut line = String::new();
                if stream.read_line(&mut line).await.unwrap() == 0 {
                    return;
                }
                if line.starts_with("DATA") {
                    stream
                        .get_mut()
                        .write_all(b"354 content\r\n")
                        .await
                        .unwrap();
                    loop {
                        let mut line = String::new();
                        if stream.read_line(&mut line).await.unwrap() == 0 || line == ".\r\n" {
                            return;
                        }
                    }
                }
                let response: &[u8] = if line.starts_with("EHLO") {
                    b"250-test\r\n250 8BITMIME\r\n"
                } else {
                    b"250 ok\r\n"
                };
                stream.get_mut().write_all(response).await.unwrap();
            }
        });
        let mut client = AsyncSmtpConnection::connect_tokio1(
            address,
            Some(Duration::from_secs(5)),
            &ClientId::default(),
            None,
            None,
        )
        .await
        .unwrap();
        let prepared = prepare(request(), 1024 * 1024).unwrap();
        let (envelope, raw) = decode_payload(&prepared.payload).unwrap();
        let error = timeout(Duration::from_secs(5), client.send(&envelope, raw))
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(classify_send_failure(&error), Outcome::Unknown);
        server.await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires TEST_DATABASE_URL pointing to a disposable MySQL database"]
    async fn mysql_idempotency_and_interrupted_send_recovery() -> anyhow::Result<()> {
        use crate::{
            crypto::Crypto,
            db::{AccountInput, MailServer},
        };
        let database_url = std::env::var("TEST_DATABASE_URL")?;
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(4)
            .connect(&database_url)
            .await?;
        sqlx::migrate!().run(&pool).await?;
        let db = Db::new(pool, Crypto::new(&[7; 32])?);
        let server = MailServer {
            auth: None,
            host: "example.com".into(),
            port: 993,
            username: "user".into(),
            password: "secret".into(),
            tls_mode: "implicit".into(),
        };
        let account = db
            .create_account(AccountInput {
                backend: crate::db::Backend::Imap,
                credential_id: None,
                target_mailbox: None,
                authorizing_identity: None,
                shared: false,
                sync_from: None,
                name: "smtp-test".into(),
                imap: server.clone(),
                smtp: server,
                watched_folders: vec![],
                backfill_days: 30,
                trash_folder: None,
                sent_folder: None,
                webhook_url: None,
                webhook_secret: None,
            })
            .await?;
        let config = Config {
            admin: Default::default(),
            oauth: Default::default(),
            native: Default::default(),
            cache: Default::default(),
            webhooks: Default::default(),
            tls_ca_file: None,
            log_filter: "rustbox=info".into(),
            database_url,
            listen: "127.0.0.1:0".parse()?,
            api_keys: Default::default(),
            encryption_key: vec![7; 32],
            allow_private_networks: false,
            max_message_bytes: 1024 * 1024,
            imap_concurrency: 1,
            smtp_concurrency: 1,
            webhook_concurrency: 1,
            command_timeout_secs: 60,
        };
        let first = enqueue(&db, &config, &account.id, "test", "same-key", request()).await?;
        let repeated = enqueue(&db, &config, &account.id, "test", "same-key", request()).await?;
        assert_eq!(first.id, repeated.id);
        for key in ["Same-key", "same-key "] {
            let distinct = enqueue(&db, &config, &account.id, "test", key, request()).await?;
            assert_ne!(first.id, distinct.id);
        }
        let mut changed = request();
        changed.subject = "different".into();
        assert!(matches!(
            enqueue(&db, &config, &account.id, "test", "same-key", changed).await,
            Err(Error::Conflict(_))
        ));
        sqlx::query("UPDATE sends SET status='sending' WHERE id=?")
            .bind(&first.id)
            .execute(&db.pool)
            .await?;
        recover_id(&db, &first.id).await?;
        assert_eq!(get(&db, &first.id).await?.status, "unknown");
        let blobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM send_blobs WHERE send_id=?")
            .bind(&first.id)
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(blobs, 0);
        let events: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM events WHERE account_id=? AND event_type='send.unknown'",
        )
        .bind(&account.id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(events, 1);
        recover_id(&db, &first.id).await?;
        assert_eq!(get(&db, &first.id).await?.status, "unknown");
        assert!(
            resolve(
                &db,
                &first.id,
                ResolveRequest {
                    decision: "sent".into(),
                    reason: "unverified".into(),
                    evidence: json!({"reason":"manual guess"})
                }
            )
            .await
            .is_err()
        );
        let resolved = resolve(
            &db,
            &first.id,
            ResolveRequest {
                decision: "unresolved".into(),
                reason: "operator closed investigation".into(),
                evidence: json!({"ticket":"internal-123"}),
            },
        )
        .await?;
        assert_eq!(resolved.status, "unknown");
        assert_eq!(resolved.resolution.as_deref(), Some("unresolved"));
        let accepted: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM events WHERE account_id=? AND event_type='send.accepted'",
        )
        .bind(&account.id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(accepted, 0);
        sqlx::query("DELETE FROM accounts WHERE id=?")
            .bind(&account.id)
            .execute(&db.pool)
            .await?;
        Ok(())
    }

    #[test]
    fn retry_delay_is_bounded() {
        assert_eq!(retry_seconds(0), 5);
        assert_eq!(retry_seconds(100), 3600);
    }
    #[tokio::test]
    async fn xoauth2_smtp_authenticates_without_submitting_mail() {
        use base64::{Engine, engine::general_purpose::STANDARD};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut peer = BufReader::new(stream);
            peer.get_mut()
                .write_all(b"220 simulator\r\n")
                .await
                .unwrap();
            let mut line = String::new();
            peer.read_line(&mut line).await.unwrap();
            assert!(line.starts_with("EHLO "));
            peer.get_mut()
                .write_all(b"250-simulator\r\n250 AUTH XOAUTH2\r\n")
                .await
                .unwrap();
            line.clear();
            peer.read_line(&mut line).await.unwrap();
            let encoded = line.trim_end().strip_prefix("AUTH XOAUTH2 ").unwrap();
            assert_eq!(
                STANDARD.decode(encoded).unwrap(),
                b"user=person@example.com\x01auth=Bearer token\x01\x01"
            );
            peer.get_mut()
                .write_all(b"235 authenticated\r\n")
                .await
                .unwrap();
            line.clear();
            peer.read_line(&mut line).await.unwrap();
            assert_eq!(line, "QUIT\r\n");
            peer.get_mut().write_all(b"221 bye\r\n").await.unwrap();
        });
        let mut client = AsyncSmtpConnection::connect_tokio1(
            address,
            Some(Duration::from_secs(5)),
            &ClientId::default(),
            None,
            None,
        )
        .await
        .unwrap();
        client
            .auth(
                &[Mechanism::Xoauth2],
                &Credentials::new("person@example.com".into(), "token".into()),
            )
            .await
            .unwrap();
        client.quit().await.unwrap();
        peer.await.unwrap();
    }
    #[test]
    fn append_outcomes_distinguish_definite_rejection_and_uncertainty() {
        assert_eq!(
            append_result(Err(Error::Invalid("rejected".into()))),
            ("failed", Some("append_rejected"))
        );
        assert_eq!(
            append_result(Err(Error::Unavailable("not connected".into()))),
            ("failed", Some("append_not_started"))
        );
        assert_eq!(
            append_result(Err(Error::UnknownOutcome)),
            ("unknown", Some("append_outcome_unknown"))
        );
        assert_eq!(
            append_result(Ok(crate::imap::Reply::Done)),
            ("complete", None)
        );
    }

    #[tokio::test]
    #[ignore = "requires a dedicated MySQL database in TEST_DATABASE_URL"]
    async fn mysql_append_recovery_retains_artifact_without_resubmission() -> anyhow::Result<()> {
        let database_url = std::env::var("TEST_DATABASE_URL")?;
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await?;
        sqlx::migrate!().run(&pool).await?;
        let db = Arc::new(Db::new(pool, crate::crypto::Crypto::new(&[7; 32])?));
        let input = serde_json::from_value(
            json!({"name":"append-recovery","imap":{"host":"example.com","port":993,"username":"test","password":"secret"},"smtp":{"host":"example.com","port":465,"username":"test","password":"secret"},"sent_folder":"Sent"}),
        )?;
        let account = db.create_account(input).await?;
        let config = Arc::new(Config {
            admin: Default::default(),
            database_url,
            listen: "127.0.0.1:0".parse()?,
            api_keys: Default::default(),
            encryption_key: vec![7; 32],
            allow_private_networks: false,
            tls_ca_file: None,
            log_filter: String::new(),
            max_message_bytes: 1024 * 1024,
            imap_concurrency: 1,
            smtp_concurrency: 1,
            webhook_concurrency: 1,
            command_timeout_secs: 5,
            webhooks: Default::default(),
            oauth: Default::default(),
            native: Default::default(),
            cache: Default::default(),
        });
        let accounts = Accounts::new(db.clone(), config.clone());
        let send = enqueue(
            &db,
            &config,
            &account.id,
            "test",
            "append-recovery",
            request(),
        )
        .await?;
        let sending = claim(&db)
            .await?
            .ok_or_else(|| anyhow::anyhow!("job missing"))?;
        assert_eq!(sending.id, send.id);
        finish(&db, &sending, Outcome::Accepted).await?;
        let copying = claim(&db)
            .await?
            .ok_or_else(|| anyhow::anyhow!("append job missing"))?;
        assert_eq!(copying.status, "accepted");
        process(&db, config.clone(), &accounts, copying).await?;
        let failed = get(&db, &send.id).await?;
        assert_eq!(failed.status, "accepted");
        assert_eq!(failed.append_status, "failed");
        assert_eq!(
            failed.append_last_error.as_deref(),
            Some("append_not_started")
        );
        assert!(failed.append_recoverable_until.is_some());
        let blobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM send_blobs WHERE send_id=?")
            .bind(&send.id)
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(blobs, 1);
        let mut changed = account.config.clone();
        changed.imap.username = "different-mailbox".into();
        db.update_account(&account.id, changed).await?;
        assert!(
            retry_append(
                &db,
                &accounts,
                &send.id,
                AppendRetryRequest {
                    allow_duplicate_copy: false,
                    reason: None
                }
            )
            .await
            .is_err()
        );
        assert!(reconcile_append(&db, &accounts, &send.id).await.is_err());
        assert_eq!(
            get(&db, &send.id).await?.append_last_error.as_deref(),
            Some("append_configuration_changed")
        );
        db.update_account(&account.id, account.config.clone())
            .await?;
        let pending = retry_append(
            &db,
            &accounts,
            &send.id,
            AppendRetryRequest {
                allow_duplicate_copy: false,
                reason: None,
            },
        )
        .await?;
        assert_eq!(pending.status, "accepted");
        assert_eq!(pending.append_status, "pending");
        assert_eq!(pending.attempts, 1);
        let copying = claim(&db)
            .await?
            .ok_or_else(|| anyhow::anyhow!("append retry missing"))?;
        assert_eq!(copying.status, "accepted");
        recover_job(&db, copying).await?;
        let unknown = get(&db, &send.id).await?;
        assert_eq!(unknown.status, "accepted");
        assert_eq!(unknown.append_status, "unknown");
        assert_eq!(unknown.append_attempts, 2);
        assert!(
            retry_append(
                &db,
                &accounts,
                &send.id,
                AppendRetryRequest {
                    allow_duplicate_copy: true,
                    reason: Some("intentional copy".into())
                }
            )
            .await
            .is_err()
        );
        assert_eq!(
            get(&db, &send.id)
                .await?
                .append_reconciliation_state
                .as_deref(),
            Some("unavailable")
        );
        let revision: u64 = sqlx::query_scalar("SELECT append_revision FROM sends WHERE id=?")
            .bind(&send.id)
            .fetch_one(&db.pool)
            .await?;
        let unknown = get(&db, &send.id).await?;
        apply_append_reconciliation(&db, &unknown, revision, "not_found", json!({"matches":[]}))
            .await?;
        assert_eq!(get(&db, &send.id).await?.append_status, "unknown");
        let unknown = get(&db, &send.id).await?;
        apply_append_reconciliation(
            &db,
            &unknown,
            revision,
            "matched",
            json!({"matches":[{"uid":1,"uidvalidity":1,"mailbox":"Sent"}]}),
        )
        .await?;
        let complete = get(&db, &send.id).await?;
        assert_eq!(complete.status, "accepted");
        assert_eq!(complete.append_status, "complete");
        assert!(complete.append_recoverable_until.is_none());
        assert!(
            retry_append(
                &db,
                &accounts,
                &send.id,
                AppendRetryRequest {
                    allow_duplicate_copy: false,
                    reason: None
                }
            )
            .await
            .is_err()
        );
        let blobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM send_blobs WHERE send_id=?")
            .bind(&send.id)
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(blobs, 0);
        let second = enqueue(
            &db,
            &config,
            &account.id,
            "test",
            "append-expiry",
            request(),
        )
        .await?;
        let sending = claim(&db)
            .await?
            .ok_or_else(|| anyhow::anyhow!("second job missing"))?;
        finish(&db, &sending, Outcome::Accepted).await?;
        let copying = claim(&db)
            .await?
            .ok_or_else(|| anyhow::anyhow!("second append missing"))?;
        record_append(&db, &copying, "failed", Some("append_rejected")).await?;
        sqlx::query("UPDATE sends SET expires_at=UTC_TIMESTAMP(6)-INTERVAL 1 SECOND WHERE id=?")
            .bind(&second.id)
            .execute(&db.pool)
            .await?;
        expire_payloads(&db).await?;
        let expired = get(&db, &second.id).await?;
        assert_eq!(expired.status, "accepted");
        assert_eq!(expired.append_status, "expired");
        assert!(expired.append_recoverable_until.is_none());
        let blobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM send_blobs WHERE send_id=?")
            .bind(&second.id)
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(blobs, 0);
        assert!(
            retry_append(
                &db,
                &accounts,
                &second.id,
                AppendRetryRequest {
                    allow_duplicate_copy: false,
                    reason: None
                }
            )
            .await
            .is_err()
        );
        let accepted_events: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM events WHERE account_id=? AND event_type='send.accepted'",
        )
        .bind(&account.id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(accepted_events, 2);
        db.delete_account(&account.id).await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires a dedicated MySQL database in TEST_DATABASE_URL"]
    async fn mysql_native_send_limit_includes_hidden_recipients_before_queueing()
    -> anyhow::Result<()> {
        let database_url = std::env::var("TEST_DATABASE_URL")?;
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await?;
        sqlx::migrate!().run(&pool).await?;
        let db = Db::new(pool, crate::crypto::Crypto::new(&[7; 32])?);
        let credential =
            crate::oauth::store::create(&db, "limit-test", Some("refresh"), None).await?;
        let input = serde_json::from_value(
            json!({"name":"send-limit","backend":"microsoft","credential_id":credential.id,"target_mailbox":"target@example.com"}),
        )?;
        let account = db.create_account(input).await?;
        let mut outgoing = request();
        outgoing.bcc = vec!["hidden@example.com".into()];
        let prepared = prepare(outgoing.clone(), usize::MAX)?;
        let (envelope, raw) = decode_payload(&prepared.payload)?;
        assert!(native_mime(&envelope, raw)?.len() > raw.len());
        let raw_len = raw.len();
        let config = Config {
            admin: Default::default(),
            database_url,
            listen: "127.0.0.1:0".parse()?,
            api_keys: Default::default(),
            encryption_key: vec![7; 32],
            allow_private_networks: false,
            tls_ca_file: None,
            log_filter: String::new(),
            max_message_bytes: raw_len,
            imap_concurrency: 1,
            smtp_concurrency: 1,
            webhook_concurrency: 1,
            command_timeout_secs: 5,
            webhooks: Default::default(),
            oauth: Default::default(),
            native: Default::default(),
            cache: Default::default(),
        };
        assert!(
            matches!(enqueue(&db,&config,&account.id,"test","oversized-bcc",outgoing).await,Err(Error::SendLimitExceeded{backend:"microsoft",maximum_mime_bytes,..}) if maximum_mime_bytes==raw_len)
        );
        let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sends WHERE account_id=?")
            .bind(&account.id)
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(queued, 0);
        db.delete_account(&account.id).await?;
        crate::oauth::store::delete(&db, &credential.id).await?;
        Ok(())
    }
}
