use super::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::collections::HashSet;

pub(super) async fn folders(client: &NativeClient, account: &Account) -> Result<Vec<Folder>> {
    let value = client
        .json(account, Method::GET, "/labels", None, false)
        .await?;
    Ok(value["labels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|label| {
            Some(Folder {
                id: label["id"].as_str()?.into(),
                name: label["name"].as_str()?.into(),
                kind: label["type"].as_str().map(str::to_owned),
            })
        })
        .collect())
}

#[derive(Serialize, Deserialize)]
struct Continuation {
    phase: String,
    token: Option<String>,
    history: String,
}

pub(super) async fn sync_page(
    client: &NativeClient,
    account: &Account,
    cursor: Option<&str>,
    next: Option<&str>,
    full: bool,
) -> Result<SyncPage> {
    let cutoff = account
        .config
        .sync_from
        .unwrap_or_else(|| Utc::now() - chrono::Duration::days(30));
    let state = if let Some(next) = next {
        serde_json::from_str::<Continuation>(next)
            .map_err(|_| Error::Invalid("invalid Gmail continuation".into()))?
    } else if cursor.is_none() || full {
        let profile = client
            .json(account, Method::GET, "/profile", None, false)
            .await?;
        Continuation {
            phase: "list".into(),
            token: None,
            history: profile["historyId"]
                .as_str()
                .ok_or_else(|| Error::Unavailable("Gmail profile omitted history cursor".into()))?
                .into(),
        }
    } else {
        Continuation {
            phase: "history".into(),
            token: None,
            history: cursor.unwrap_or_default().into(),
        }
    };
    let mut pairs = vec![("maxResults", "100".into())];
    if let Some(token) = &state.token {
        pairs.push(("pageToken", token.clone()));
    }
    let path = if state.phase == "list" {
        pairs.push((
            "q",
            format!("after:{}", cutoff.timestamp().saturating_sub(1)),
        ));
        pairs.push(("includeSpamTrash", "true".into()));
        query("/messages", &pairs)
    } else {
        pairs.push(("startHistoryId", state.history.clone()));
        query("/history", &pairs)
    };
    let page = match client.json(account, Method::GET, &path, None, false).await {
        Err(Error::NotFound) if state.phase == "history" => {
            return Ok(SyncPage {
                reset: true,
                ..Default::default()
            });
        }
        result => result?,
    };
    let mut ids = HashSet::new();
    let mut removed = HashSet::new();
    if state.phase == "list" {
        for message in page["messages"].as_array().into_iter().flatten() {
            if let Some(id) = message["id"].as_str() {
                ids.insert(id.to_owned());
            }
        }
    } else {
        for entry in page["history"].as_array().into_iter().flatten() {
            for message in entry["messages"].as_array().into_iter().flatten() {
                if let Some(id) = message["id"].as_str() {
                    ids.insert(id.to_owned());
                }
            }
            for kind in ["messagesAdded", "labelsAdded", "labelsRemoved"] {
                for change in entry[kind].as_array().into_iter().flatten() {
                    if let Some(id) = change["message"]["id"].as_str() {
                        ids.insert(id.to_owned());
                    }
                }
            }
            for change in entry["messagesDeleted"].as_array().into_iter().flatten() {
                if let Some(id) = change["message"]["id"].as_str() {
                    removed.insert(id.to_owned());
                }
            }
        }
    }
    let mut messages = Vec::new();
    for id in ids {
        match message(client, account, &id).await {
            Ok(message) if message.received_at >= cutoff => {
                removed.remove(&id);
                messages.push(message);
            }
            Ok(_) => {}
            Err(Error::NotFound) => {
                removed.insert(id);
            }
            Err(e) => return Err(e),
        }
    }
    let next = page["nextPageToken"]
        .as_str()
        .map(|token| {
            serde_json::to_string(&Continuation {
                phase: state.phase.clone(),
                token: Some(token.into()),
                history: state.history.clone(),
            })
        })
        .transpose()
        .map_err(|_| Error::Internal("Gmail continuation serialization failed".into()))?;
    let cursor = if next.is_none() {
        Some(if state.phase == "list" {
            state.history
        } else {
            page["historyId"].as_str().unwrap_or(&state.history).into()
        })
    } else {
        None
    };
    Ok(SyncPage {
        messages,
        removed: removed.into_iter().collect(),
        next,
        cursor,
        reset: false,
    })
}

async fn message(client: &NativeClient, account: &Account, id: &str) -> Result<ProviderMessage> {
    decode(
        &client
            .json(
                account,
                Method::GET,
                &format!("/messages/{}?format=metadata", segment(id)),
                None,
                false,
            )
            .await?,
    )
}

fn decode(value: &Value) -> Result<ProviderMessage> {
    let id = value["id"]
        .as_str()
        .ok_or_else(|| Error::Unavailable("Gmail message omitted identity".into()))?
        .to_owned();
    let received_at = value["internalDate"]
        .as_str()
        .and_then(|v| v.parse::<i64>().ok())
        .and_then(DateTime::<Utc>::from_timestamp_millis)
        .ok_or_else(|| Error::Unavailable("Gmail message omitted internal date".into()))?;
    let locations = value["labelIds"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut flags = Vec::new();
    if !locations.iter().any(|v| v == "UNREAD") {
        flags.push("\\Seen".into());
    }
    if locations.iter().any(|v| v == "STARRED") {
        flags.push("\\Flagged".into());
    }
    if locations.iter().any(|v| v == "DRAFT") {
        flags.push("\\Draft".into());
    }
    let headers = value["payload"]["headers"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut metadata = normalized_headers(&headers)?;
    metadata["received_at"] = json!(received_at);
    metadata["thread_id"] = value["threadId"].clone();
    metadata["snippet"] = value["snippet"].clone();
    Ok(ProviderMessage {
        id,
        locations,
        metadata,
        flags,
        received_at,
        size: value["sizeEstimate"].as_u64().unwrap_or(0),
    })
}

pub(super) async fn fetch(client: &NativeClient, account: &Account, id: &str) -> Result<Vec<u8>> {
    let value = client
        .json(
            account,
            Method::GET,
            &format!("/messages/{}?format=raw", segment(id)),
            None,
            false,
        )
        .await?;
    let raw = value["raw"]
        .as_str()
        .ok_or_else(|| Error::Unavailable("Gmail omitted MIME content".into()))?;
    URL_SAFE_NO_PAD
        .decode(raw.trim_end_matches('='))
        .map_err(|_| Error::Unavailable("invalid Gmail MIME encoding".into()))
}

pub(super) async fn labels(
    client: &NativeClient,
    account: &Account,
    id: &str,
    add: &[String],
    remove: &[String],
) -> Result<ProviderMessage> {
    client
        .json(
            account,
            Method::POST,
            &format!("/messages/{}/modify", segment(id)),
            Some(json!({"addLabelIds":add,"removeLabelIds":remove})),
            false,
        )
        .await?;
    message(client, account, id)
        .await
        .map_err(|_| Error::UnknownOutcome)
}

pub(super) async fn flags(
    client: &NativeClient,
    account: &Account,
    id: &str,
    flags: &[String],
) -> Result<()> {
    if flags
        .iter()
        .any(|v| !matches!(v.as_str(), "\\Seen" | "\\Flagged"))
    {
        return Err(Error::Unsupported(
            "Gmail supports Seen and Flagged flag mutations".into(),
        ));
    }
    let mut add = Vec::new();
    let mut remove = Vec::new();
    if flags.iter().any(|v| v == "\\Seen") {
        remove.push("UNREAD".into());
    } else {
        add.push("UNREAD".into());
    }
    if flags.iter().any(|v| v == "\\Flagged") {
        add.push("STARRED".into());
    } else {
        remove.push("STARRED".into());
    }
    labels(client, account, id, &add, &remove).await?;
    Ok(())
}

pub(super) async fn move_message(
    client: &NativeClient,
    account: &Account,
    id: &str,
    source: &str,
    destination: &str,
) -> Result<ProviderMessage> {
    if source == destination {
        return message(client, account, id).await;
    }
    labels(client, account, id, &[destination.into()], &[source.into()]).await
}

pub(super) async fn send(
    client: &NativeClient,
    account: &Account,
    raw: &[u8],
    thread_id: Option<&str>,
) -> Result<SentSubmission> {
    crate::sending_limits::validate_google_thread(thread_id)?;
    let mut body = json!({"raw":URL_SAFE_NO_PAD.encode(raw)});
    if let Some(thread) = thread_id {
        body["threadId"] = json!(thread);
    }
    let value = client
        .json(account, Method::POST, "/messages/send", Some(body), true)
        .await?;
    Ok(SentSubmission {
        provider_id: value["id"].as_str().map(str::to_owned),
        thread_id: value["threadId"].as_str().map(str::to_owned),
    })
}

pub(super) async fn reconcile(
    client: &NativeClient,
    account: &Account,
    message_id: &str,
) -> Result<Vec<ProviderMessage>> {
    let query_string = format!(
        "in:sent rfc822msgid:{}",
        message_id.trim_matches(['<', '>'])
    );
    let page = client
        .json(
            account,
            Method::GET,
            &query(
                "/messages",
                &[("q", query_string), ("maxResults", "100".into())],
            ),
            None,
            false,
        )
        .await?;
    if page.get("nextPageToken").is_some() {
        return Err(Error::Unsupported(
            "too many reconciliation candidates".into(),
        ));
    }
    let mut messages = Vec::new();
    for candidate in page["messages"].as_array().into_iter().flatten() {
        if let Some(id) = candidate["id"].as_str() {
            messages.push(message(client, account, id).await?);
        }
    }
    Ok(messages)
}

pub(super) async fn subscription(
    client: &NativeClient,
    account: &Account,
    topic: Option<&str>,
) -> Result<Subscription> {
    let topic =
        topic.ok_or_else(|| Error::Unsupported("Gmail Pub/Sub topic is not configured".into()))?;
    let value = client
        .json(
            account,
            Method::POST,
            "/watch",
            Some(json!({"topicName":topic})),
            false,
        )
        .await?;
    let expires = value["expiration"]
        .as_str()
        .and_then(|v| v.parse::<i64>().ok())
        .and_then(DateTime::<Utc>::from_timestamp_millis)
        .ok_or_else(|| Error::Unavailable("Gmail watch omitted expiration".into()))?;
    Ok(Subscription {
        id: target(account)?.into(),
        expires_at: expires,
        history_id: value["historyId"].as_str().map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gmail_identity_is_independent_of_label_memberships() {
        let message=decode(&json!({"id":"stable","threadId":"thread","internalDate":"1767225600000","labelIds":["INBOX","custom"],"sizeEstimate":42,"payload":{"headers":[{"name":"From","value":"Person <person@example.com>"},{"name":"Message-ID","value":"<rfc@example.com>"},{"name":"References","value":"<a@example.com> <b@example.com>"}]}})).unwrap();
        assert_eq!(message.id, "stable");
        assert_eq!(message.locations, vec!["INBOX", "custom"]);
        assert_eq!(message.metadata["from"][0]["address"], "person@example.com");
        assert_eq!(message.metadata["references"].as_array().unwrap().len(), 2);
        assert!(message.flags.contains(&"\\Seen".into()));
        assert!(decode(&json!({"id":"missing-date"})).is_err());
    }
}
