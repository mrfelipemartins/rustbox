use crate::{
    db::{Account, Db},
    error::{Error, Result},
    graph_message::{GraphAttachment, GraphMessage},
    graph_store::{self, GraphPhase, GraphState},
    providers::{GraphUploadSession, NativeClient, SentSubmission},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Progress {
    creating_draft: bool,
    content_fingerprint: Option<String>,
    internet_message_id: Option<String>,
    attachments: Vec<AttachmentState>,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct AttachmentState {
    id: Option<String>,
    started: bool,
    session: Option<GraphUploadSession>,
    offset: u64,
    complete: bool,
    final_chunk_pending: bool,
}
async fn save(db: &Db, id: &str, state: &mut GraphState, progress: &Progress) -> Result<()> {
    state.progress = serde_json::to_value(progress)
        .map_err(|_| Error::Internal("Graph progress serialization failed".into()))?;
    graph_store::save(db, id, state).await
}
fn preparation_error(error: Error) -> Error {
    match error {
        Error::UnknownOutcome | Error::Conflict(_) => {
            Error::Unavailable("Graph draft preparation requires reconciliation".into())
        }
        other => other,
    }
}
pub async fn submit(
    db: &Db,
    client: &NativeClient,
    account: &Account,
    id: &str,
    raw: Vec<u8>,
) -> Result<SentSubmission> {
    let mut state = graph_store::load(db, id)
        .await?
        .ok_or_else(|| Error::Internal("Graph send state missing".into()))?;
    match state.phase {
        GraphPhase::Accepted => {
            return Ok(SentSubmission {
                provider_id: state.draft_id,
                thread_id: None,
            });
        }
        GraphPhase::Submitting | GraphPhase::Unknown => return Err(Error::UnknownOutcome),
        GraphPhase::Abandoned => {
            return Err(Error::Invalid(
                "Graph draft preparation was abandoned".into(),
            ));
        }
        _ => {}
    }
    let message = tokio::task::spawn_blocking(move || GraphMessage::parse(&raw))
        .await
        .map_err(|_| Error::Internal("Graph MIME preparation task failed".into()))??;
    let mut progress: Progress = serde_json::from_value(state.progress.clone())
        .map_err(|_| Error::Internal("invalid Graph send progress".into()))?;
    progress
        .attachments
        .resize_with(message.attachments.len(), AttachmentState::default);
    if state.draft_id.is_none() {
        if progress.creating_draft {
            let matches = client
                .graph_find_drafts(account, id)
                .await
                .map_err(preparation_error)?;
            match matches.as_slice() {
                [draft] if draft.is_draft => {
                    state.draft_id = Some(draft.id.clone());
                    progress.internet_message_id = draft.internet_message_id.clone();
                    save(db, id, &mut state, &progress).await?;
                }
                [] => {
                    return Err(Error::Unavailable(
                        "Graph draft creation is not yet reconciled".into(),
                    ));
                }
                _ => {
                    return Err(Error::Conflict(
                        "Graph draft creation has ambiguous evidence".into(),
                    ));
                }
            }
        } else {
            progress.creating_draft = true;
            save(db, id, &mut state, &progress).await?;
            let mut base = format!("X-Rustbox-Send-Id: {id}\r\n").into_bytes();
            base.extend_from_slice(&message.base_mime);
            match client.graph_create_draft(account, &base).await {
                Ok(draft) => {
                    state.draft_id = Some(draft.id);
                    progress.internet_message_id = draft.internet_message_id;
                    save(db, id, &mut state, &progress).await?;
                }
                Err(error) => {
                    if !matches!(error, Error::UnknownOutcome) {
                        progress.creating_draft = false;
                        save(db, id, &mut state, &progress).await?;
                    }
                    return Err(preparation_error(error));
                }
            }
        }
    }
    let draft = state
        .draft_id
        .clone()
        .ok_or_else(|| Error::Internal("Graph draft missing".into()))?;
    let current = client
        .graph_get_draft(account, &draft)
        .await
        .map_err(preparation_error)?;
    if !current.is_draft {
        state.phase = GraphPhase::Unknown;
        state.progress = json!({});
        graph_store::save(db, id, &mut state).await?;
        return Err(Error::UnknownOutcome);
    }
    let content = content_fingerprint(client.graph_draft_content(account, &draft).await?).await?;
    if let Some(expected) = &progress.content_fingerprint {
        if expected != &content {
            return Err(Error::Conflict(
                "Graph draft content changed outside Rustbox".into(),
            ));
        }
    } else {
        progress.content_fingerprint = Some(content);
        save(db, id, &mut state, &progress).await?;
    }
    for index in 0..progress.attachments.len() {
        if progress.attachments[index].complete && progress.attachments[index].id.is_none() {
            let used: HashSet<String> = progress
                .attachments
                .iter()
                .filter_map(|part| part.id.clone())
                .collect();
            for item in client.graph_list_attachments(account, &draft).await? {
                let Some(found) = item["id"].as_str() else {
                    continue;
                };
                if !used.contains(found)
                    && same(client, account, &draft, &item, &message.attachments[index]).await?
                {
                    progress.attachments[index].id = Some(found.to_owned());
                    save(db, id, &mut state, &progress).await?;
                    break;
                }
            }
            if progress.attachments[index].id.is_none() {
                return Err(Error::Unavailable(
                    "Graph completed attachment is not yet observable".into(),
                ));
            }
        }
    }
    for (index, attachment) in message.attachments.iter().enumerate() {
        if progress.attachments[index].complete {
            continue;
        }
        if progress.attachments[index].started
            && let Some(found) =
                find_attachment(client, account, &draft, attachment, &progress).await?
        {
            let part = &mut progress.attachments[index];
            part.id = Some(found);
            part.complete = true;
            part.session = None;
            save(db, id, &mut state, &progress).await?;
            continue;
        }
        if attachment.data.len() < 3_000_000 {
            if progress.attachments[index].started {
                return Err(Error::Unavailable(
                    "Graph attachment creation is not yet reconciled".into(),
                ));
            }
            progress.attachments[index].started = true;
            save(db, id, &mut state, &progress).await?;
            match client
                .graph_add_small_attachment(account, &draft, attachment)
                .await
            {
                Ok(attachment_id) => {
                    let part = &mut progress.attachments[index];
                    part.id = Some(attachment_id);
                    part.complete = true;
                    save(db, id, &mut state, &progress).await?;
                }
                Err(error) => {
                    if !matches!(error, Error::UnknownOutcome) {
                        progress.attachments[index].started = false;
                        save(db, id, &mut state, &progress).await?;
                    }
                    return Err(preparation_error(error));
                }
            }
            continue;
        }
        if let Some(session) = progress.attachments[index].session.clone() {
            if session.expires_at <= chrono::Utc::now() {
                progress.attachments[index].session = None;
                progress.attachments[index].offset = 0;
            } else {
                match client
                    .graph_upload_status(&session.url, attachment.data.len() as u64)
                    .await
                {
                    Ok(status) => {
                        progress.attachments[index].offset = status.next_offset;
                        if status.next_offset < attachment.data.len() as u64 {
                            progress.attachments[index].final_chunk_pending = false;
                        }
                    }
                    Err(Error::NotFound) => {
                        progress.attachments[index].session = None;
                        progress.attachments[index].offset = 0;
                    }
                    Err(error) => return Err(preparation_error(error)),
                }
            }
        }
        if progress.attachments[index].session.is_none() {
            if progress.attachments[index].final_chunk_pending {
                return Err(Error::Unavailable(
                    "Graph final attachment chunk requires reconciliation".into(),
                ));
            }
            let session = client
                .graph_create_upload(account, &draft, attachment)
                .await
                .map_err(preparation_error)?;
            let part = &mut progress.attachments[index];
            part.offset = session.next_offset;
            part.session = Some(session);
            part.started = true;
            save(db, id, &mut state, &progress).await?;
        }
        while progress.attachments[index].offset < attachment.data.len() as u64 {
            let part = &progress.attachments[index];
            let session = part
                .session
                .as_ref()
                .ok_or_else(|| Error::Internal("Graph upload session missing".into()))?;
            let start = part.offset as usize;
            let end = (start + 3_276_800).min(attachment.data.len());
            let upload_url = session.url.clone();
            if end == attachment.data.len() {
                progress.attachments[index].final_chunk_pending = true;
                save(db, id, &mut state, &progress).await?;
            }
            let uploaded = client
                .graph_upload_chunk(
                    &upload_url,
                    &attachment.data[start..end],
                    start as u64,
                    attachment.data.len() as u64,
                )
                .await
                .map_err(preparation_error)?;
            let part = &mut progress.attachments[index];
            if uploaded.next_offset < part.offset
                || uploaded.next_offset > attachment.data.len() as u64
            {
                return Err(Error::Unavailable("invalid Graph upload progress".into()));
            }
            if uploaded.next_offset == part.offset && !uploaded.complete {
                return Err(Error::Unavailable("Graph upload did not advance".into()));
            }
            part.offset = uploaded.next_offset;
            if uploaded.complete {
                part.complete = true;
                part.id = uploaded.attachment_id;
                part.session = None;
            }
            save(db, id, &mut state, &progress).await?;
            if progress.attachments[index].complete {
                break;
            }
        }
        if !progress.attachments[index].complete {
            let found = find_attachment(client, account, &draft, attachment, &progress).await?;
            let Some(found) = found else {
                return Err(Error::Unavailable(
                    "Graph upload completion requires reconciliation".into(),
                ));
            };
            let part = &mut progress.attachments[index];
            part.id = Some(found);
            part.complete = true;
            part.session = None;
            save(db, id, &mut state, &progress).await?;
        }
    }
    verify_inventory(client, account, &draft, &message.attachments).await?;
    if progress.content_fingerprint.as_deref()
        != Some(
            content_fingerprint(client.graph_draft_content(account, &draft).await?)
                .await?
                .as_str(),
        )
    {
        return Err(Error::Conflict(
            "Graph draft content changed outside Rustbox".into(),
        ));
    }
    state.phase = GraphPhase::Ready;
    save(db, id, &mut state, &progress).await?;
    let current = client
        .graph_get_draft(account, &draft)
        .await
        .map_err(preparation_error)?;
    if !current.is_draft {
        state.phase = GraphPhase::Unknown;
        state.progress = json!({});
        graph_store::save(db, id, &mut state).await?;
        return Err(Error::UnknownOutcome);
    }
    state.phase = GraphPhase::Submitting;
    save(db, id, &mut state, &progress).await?;
    match client.graph_send_draft(account, &draft).await {
        Ok(()) => {
            state.phase = GraphPhase::Accepted;
            state.progress = json!({"internet_message_id":progress.internet_message_id});
            graph_store::save(db, id, &mut state).await?;
            Ok(SentSubmission {
                provider_id: Some(draft),
                thread_id: None,
            })
        }
        Err(Error::UnknownOutcome) => {
            state.phase = GraphPhase::Unknown;
            state.progress = json!({"internet_message_id":progress.internet_message_id});
            graph_store::save(db, id, &mut state).await?;
            Err(Error::UnknownOutcome)
        }
        Err(error) => {
            state.phase = GraphPhase::Ready;
            save(db, id, &mut state, &progress).await?;
            Err(error)
        }
    }
}
async fn content_fingerprint(value: Value) -> Result<String> {
    tokio::task::spawn_blocking(move || -> Result<String> {
        let mut normalized = serde_json::Map::new();
        for key in [
            "subject",
            "body",
            "from",
            "sender",
            "toRecipients",
            "ccRecipients",
            "bccRecipients",
            "replyTo",
        ] {
            normalized.insert(key.into(), value[key].clone());
        }
        let mut headers = Vec::new();
        for header in value["internetMessageHeaders"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(name) = header["name"].as_str() {
                let name = name.to_ascii_lowercase();
                if [
                    "message-id",
                    "in-reply-to",
                    "references",
                    "reply-to",
                    "from",
                    "to",
                    "cc",
                    "bcc",
                    "subject",
                    "x-rustbox-send-id",
                ]
                .contains(&name.as_str())
                {
                    headers.push(json!({"name":name,"value":header["value"]}));
                }
            }
        }
        headers.sort_by_key(|header| header["name"].as_str().unwrap_or_default().to_owned());
        normalized.insert("headers".into(), json!(headers));
        let bytes = serde_json::to_vec(&normalized)
            .map_err(|_| Error::Internal("Graph draft verification failed".into()))?;
        Ok(hex::encode(Sha256::digest(bytes)))
    })
    .await
    .map_err(|_| Error::Internal("Graph draft verification task failed".into()))?
}
async fn hash(bytes: Vec<u8>) -> Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || Sha256::digest(bytes).to_vec())
        .await
        .map_err(|_| Error::Internal("attachment fingerprint task failed".into()))
}
async fn same(
    client: &NativeClient,
    account: &Account,
    draft: &str,
    metadata: &Value,
    expected: &GraphAttachment,
) -> Result<bool> {
    if metadata["name"].as_str() != Some(expected.name.as_str())
        || metadata["size"].as_u64() != Some(expected.data.len() as u64)
        || metadata["isInline"].as_bool().unwrap_or(false) != expected.is_inline
        || expected.content_id.as_deref().is_some_and(|id| {
            metadata["contentId"]
                .as_str()
                .map(|v| v.trim_matches(['<', '>']))
                != Some(id.trim_matches(['<', '>']))
        })
    {
        return Ok(false);
    }
    if let Some(content_type) = metadata["contentType"].as_str()
        && !content_type.eq_ignore_ascii_case(&expected.content_type)
    {
        return Ok(false);
    }
    let id = metadata["id"]
        .as_str()
        .ok_or_else(|| Error::Unavailable("Graph attachment identity missing".into()))?;
    let actual = client.graph_attachment_bytes(account, draft, id).await?;
    Ok(hash(actual).await? == hash(expected.data.clone()).await?)
}
async fn find_attachment(
    client: &NativeClient,
    account: &Account,
    draft: &str,
    expected: &GraphAttachment,
    progress: &Progress,
) -> Result<Option<String>> {
    let used: HashSet<&str> = progress
        .attachments
        .iter()
        .filter_map(|a| a.id.as_deref())
        .collect();
    let mut found = None;
    for item in client.graph_list_attachments(account, draft).await? {
        let Some(id) = item["id"].as_str() else {
            continue;
        };
        if !used.contains(id) && same(client, account, draft, &item, expected).await? {
            if found.is_some() {
                return Err(Error::Conflict(
                    "ambiguous Graph attachment evidence".into(),
                ));
            }
            found = Some(id.to_owned());
        }
    }
    Ok(found)
}
async fn verify_inventory(
    client: &NativeClient,
    account: &Account,
    draft: &str,
    expected: &[GraphAttachment],
) -> Result<()> {
    let inventory = client.graph_list_attachments(account, draft).await?;
    if inventory.len() != expected.len() {
        return Err(Error::Conflict(
            "Graph attachment inventory does not match the queued message".into(),
        ));
    }
    let mut used = HashSet::new();
    for attachment in expected {
        let mut matched = None;
        for (index, item) in inventory.iter().enumerate() {
            if !used.contains(&index) && same(client, account, draft, item, attachment).await? {
                matched = Some(index);
                break;
            }
        }
        let Some(index) = matched else {
            return Err(Error::Conflict(
                "Graph attachment content does not match the queued message".into(),
            ));
        };
        used.insert(index);
    }
    Ok(())
}
pub async fn cleanup(db: &Db, client: &NativeClient, account: &Account, id: &str) -> Result<()> {
    let Some(mut state) = graph_store::load(db, id).await? else {
        return Ok(());
    };
    if !matches!(state.phase, GraphPhase::Preparing | GraphPhase::Ready) {
        return Ok(());
    }
    if let Some(draft) = &state.draft_id {
        client.graph_delete_draft(account, draft).await?;
    } else {
        for draft in client.graph_find_drafts(account, id).await? {
            client.graph_delete_draft(account, &draft.id).await?;
        }
    }
    state.phase = GraphPhase::Abandoned;
    state.progress = json!({});
    graph_store::save(db, id, &mut state).await
}

#[cfg(test)]
#[path = "graph_send_tests.rs"]
mod tests;
