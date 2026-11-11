use super::*;

const SELECT: &str = "id,parentFolderId,subject,internetMessageId,internetMessageHeaders,from,sender,replyTo,toRecipients,ccRecipients,bccRecipients,receivedDateTime,sentDateTime,isRead,flag,hasAttachments,conversationId";

pub(super) async fn folders(client: &NativeClient, account: &Account) -> Result<Vec<Folder>> {
    let mut result = Vec::new();
    let mut pending = std::collections::VecDeque::from([
        "/mailFolders?$top=100&includeHiddenFolders=false".to_owned(),
    ]);
    while let Some(path) = pending.pop_front() {
        let page = client
            .json(account, Method::GET, &path, None, false)
            .await?;
        for folder in page["value"].as_array().into_iter().flatten() {
            let Some(id) = folder["id"].as_str() else {
                continue;
            };
            result.push(Folder {
                id: id.into(),
                name: folder["displayName"].as_str().unwrap_or(id).into(),
                kind: None,
            });
            if folder["childFolderCount"].as_u64().unwrap_or(0) > 0 {
                pending.push_back(format!(
                    "/mailFolders/{}/childFolders?$top=100",
                    segment(id)
                ));
            }
        }
        if let Some(next) = page["@odata.nextLink"].as_str() {
            pending.push_back(next.into());
        }
        if result.len() > 10_000 {
            return Err(Error::Unsupported(
                "mailbox folder count exceeds safety limit".into(),
            ));
        }
    }
    Ok(result)
}

fn known_sent_folder(account: &Account, folder: &str) -> Option<bool> {
    if folder.eq_ignore_ascii_case("sentitems")
        || account
            .config
            .sent_folder
            .as_deref()
            .is_some_and(|sent| sent == folder)
    {
        return Some(true);
    }
    if [
        "inbox",
        "archive",
        "deleteditems",
        "drafts",
        "junkemail",
        "outbox",
        "msgfolderroot",
        "recoverableitemsdeletions",
    ]
    .iter()
    .any(|known| folder.eq_ignore_ascii_case(known))
    {
        return Some(false);
    }
    None
}

async fn is_sent_folder(client: &NativeClient, account: &Account, folder: &str) -> Result<bool> {
    if let Some(sent) = known_sent_folder(account, folder) {
        return Ok(sent);
    }
    match client
        .json(
            account,
            Method::GET,
            "/mailFolders/sentitems?$select=id",
            None,
            false,
        )
        .await
    {
        Ok(sent) => Ok(sent["id"].as_str() == Some(folder)),
        Err(
            Error::NotFound
            | Error::OAuth {
                retryable: false, ..
            },
        ) => Ok(false),
        Err(error) => Err(error),
    }
}

fn eligibility_field(sent: bool) -> &'static str {
    if sent {
        "sentDateTime"
    } else {
        "receivedDateTime"
    }
}

fn eligibility_at(value: &Value, sent: bool) -> Result<DateTime<Utc>> {
    value[eligibility_field(sent)]
        .as_str()
        .and_then(|date| DateTime::parse_from_rfc3339(date).ok())
        .map(|date| date.with_timezone(&Utc))
        .ok_or_else(|| Error::Unavailable("provider message omitted eligibility date".into()))
}

pub(super) async fn sync_page(
    client: &NativeClient,
    account: &Account,
    folder: &str,
    cursor: Option<&str>,
    next: Option<&str>,
    full: bool,
) -> Result<SyncPage> {
    let delegated_shared = client.delegated_shared(account).await?;
    let sent = is_sent_folder(client, account, folder).await?;
    let cutoff = account
        .config
        .sync_from
        .unwrap_or_else(|| Utc::now() - chrono::Duration::days(30));
    let start = Utc::now();
    let path = if let Some(next) = next {
        next.to_owned()
    } else if delegated_shared {
        let since = if full {
            cutoff
        } else {
            cursor
                .and_then(|v| DateTime::parse_from_rfc3339(v).ok())
                .map(|v| v.with_timezone(&Utc) - chrono::Duration::minutes(5))
                .unwrap_or(cutoff)
        };
        query(
            &format!("/mailFolders/{}/messages", segment(folder)),
            &[
                ("$select", SELECT.into()),
                ("$top", "100".into()),
                (
                    "$filter",
                    format!("{} ge {}", eligibility_field(sent), since.to_rfc3339()),
                ),
            ],
        )
    } else if !full && cursor.is_some() {
        cursor.unwrap_or_default().into()
    } else {
        query(
            &format!("/mailFolders/{}/messages/delta", segment(folder)),
            &[
                ("$select", SELECT.into()),
                ("$top", "100".into()),
                ("$orderby", "receivedDateTime desc".into()),
            ],
        )
    };
    let page = match client.json(account, Method::GET, &path, None, false).await {
        Err(Error::Conflict(_)) => {
            return Ok(SyncPage {
                reset: true,
                ..Default::default()
            });
        }
        result => result?,
    };
    let mut result = SyncPage {
        next: page["@odata.nextLink"].as_str().map(str::to_owned),
        cursor: page["@odata.deltaLink"].as_str().map(str::to_owned),
        ..Default::default()
    };
    for message in page["value"].as_array().into_iter().flatten() {
        if message.get("@removed").is_some() {
            if let Some(id) = message["id"].as_str() {
                result.removed.push(id.into());
            }
            continue;
        }
        let fetched;
        let message = if message["receivedDateTime"].as_str().is_none()
            || message[eligibility_field(sent)].as_str().is_none()
        {
            let id = message["id"]
                .as_str()
                .ok_or_else(|| Error::Unavailable("Graph update omitted identity".into()))?;
            match client
                .json(
                    account,
                    Method::GET,
                    &query(
                        &format!("/messages/{}", segment(id)),
                        &[("$select", SELECT.into())],
                    ),
                    None,
                    false,
                )
                .await
            {
                Ok(value) => {
                    fetched = value;
                    &fetched
                }
                Err(Error::NotFound | Error::ProviderNotFound { .. }) => {
                    result.removed.push(id.into());
                    continue;
                }
                Err(e) => return Err(e),
            }
        } else {
            message
        };
        let eligible_at = eligibility_at(message, sent)?;
        let mut message = decode(message)?;
        message.metadata["eligibility_at"] = json!(eligible_at);
        message.metadata["eligibility_time_basis"] = json!(if sent { "sent" } else { "received" });
        message.locations = vec![folder.to_owned()];
        if eligible_at >= cutoff {
            result.messages.push(message);
        }
    }
    if delegated_shared && result.next.is_none() {
        result.cursor = Some(start.to_rfc3339());
    }
    Ok(result)
}

pub(super) fn decode(value: &Value) -> Result<ProviderMessage> {
    let id = value["id"]
        .as_str()
        .ok_or_else(|| Error::Unavailable("provider message omitted identity".into()))?
        .to_owned();
    let received_at = value["receivedDateTime"]
        .as_str()
        .and_then(|v| DateTime::parse_from_rfc3339(v).ok())
        .map(|v| v.with_timezone(&Utc))
        .ok_or_else(|| Error::Unavailable("provider message omitted received date".into()))?;
    let mut flags = Vec::new();
    if value["isRead"].as_bool() == Some(true) {
        flags.push("\\Seen".into());
    }
    if value["flag"]["flagStatus"].as_str() == Some("flagged") {
        flags.push("\\Flagged".into());
    }
    let headers = value["internetMessageHeaders"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let address = |v: &Value| json!({"name":v["emailAddress"]["name"],"address":v["emailAddress"]["address"]});
    let addresses = |name: &str| {
        value[name]
            .as_array()
            .into_iter()
            .flatten()
            .map(address)
            .collect::<Vec<_>>()
    };
    let mut metadata = normalized_headers(&headers)?;
    if let Some(id) = value["internetMessageId"].as_str() {
        metadata["message_id"] = json!(format!("<{}>", id.trim().trim_matches(['<', '>'])));
    }
    metadata["subject"] = value["subject"].clone();
    if !value["from"].is_null() {
        metadata["from"] = json!([address(&value["from"])]);
    }
    if !value["sender"].is_null() {
        metadata["sender"] = json!([address(&value["sender"])]);
    }
    for (name, source) in [
        ("reply_to", "replyTo"),
        ("to", "toRecipients"),
        ("cc", "ccRecipients"),
        ("bcc", "bccRecipients"),
    ] {
        if value.get(source).is_some() {
            metadata[name] = json!(addresses(source));
        }
    }
    if metadata["date"].is_null() {
        metadata["date"] = value["sentDateTime"].clone();
    }
    metadata["received_at"] = json!(received_at);
    metadata["sent_at"] = value["sentDateTime"].clone();
    metadata["thread_id"] = value["conversationId"].clone();
    metadata["has_attachments"] = value["hasAttachments"].clone();
    Ok(ProviderMessage {
        id,
        locations: value["parentFolderId"]
            .as_str()
            .map(|v| vec![v.into()])
            .unwrap_or_default(),
        metadata,
        flags,
        received_at,
        size: value["size"].as_u64().unwrap_or(0),
    })
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
            "Graph supports Seen and Flagged flags".into(),
        ));
    }
    client.json(account,Method::PATCH,&format!("/messages/{}",segment(id)),Some(json!({"isRead":flags.iter().any(|v|v=="\\Seen"),"flag":{"flagStatus":if flags.iter().any(|v|v=="\\Flagged"){"flagged"}else{"notFlagged"}}})),false).await?;
    Ok(())
}

pub(super) async fn move_message(
    client: &NativeClient,
    account: &Account,
    id: &str,
    destination: &str,
) -> Result<ProviderMessage> {
    let moved = client
        .json(
            account,
            Method::POST,
            &format!("/messages/{}/move", segment(id)),
            Some(json!({"destinationId":destination})),
            false,
        )
        .await?;
    decode(&moved).map_err(|_| Error::UnknownOutcome)
}

pub(super) async fn reconcile(
    client: &NativeClient,
    account: &Account,
    message_id: &str,
) -> Result<Vec<ProviderMessage>> {
    let sent = account.config.sent_folder.as_deref().unwrap_or("sentitems");
    let escaped = message_id.replace('\'', "''");
    let mut path = Some(query(
        &format!("/mailFolders/{}/messages", segment(sent)),
        &[
            ("$select", SELECT.into()),
            ("$filter", format!("internetMessageId eq '{escaped}'")),
            ("$top", "100".into()),
        ],
    ));
    let mut result = Vec::new();
    while let Some(next) = path {
        let page = client
            .json(account, Method::GET, &next, None, false)
            .await?;
        for value in page["value"].as_array().into_iter().flatten() {
            result.push(decode(value)?);
        }
        path = page["@odata.nextLink"].as_str().map(str::to_owned);
        if result.len() > 1000 {
            return Err(Error::Unsupported(
                "too many reconciliation candidates".into(),
            ));
        }
    }
    Ok(result)
}

pub(super) async fn subscription(
    client: &NativeClient,
    account: &Account,
    public_base: &str,
    existing: Option<&str>,
    client_state: &str,
) -> Result<Subscription> {
    let credential = store::get(
        client.oauth.database(),
        account
            .config
            .credential_id
            .as_deref()
            .ok_or_else(native_required)?,
    )
    .await?;
    if account.config.shared
        && client.oauth.registration(&credential.client_id)?.mode == Mode::Delegated
    {
        return Err(Error::Unsupported(
            "delegated shared mailboxes use polling".into(),
        ));
    }
    let expires = Utc::now() + chrono::Duration::days(2);
    let create_body = json!({"changeType":"created,updated,deleted","notificationUrl":format!("{}/notifications/microsoft",public_base.trim_end_matches('/')),"lifecycleNotificationUrl":format!("{}/notifications/microsoft/lifecycle",public_base.trim_end_matches('/')),"resource":format!("users/{}/messages",segment(target(account)?)),"expirationDateTime":expires.to_rfc3339(),"clientState":client_state});
    let value = if let Some(existing) = existing {
        let renewed = client
            .json(
                account,
                Method::PATCH,
                &format!(
                    "https://graph.microsoft.com/v1.0/subscriptions/{}",
                    segment(existing)
                ),
                Some(json!({"expirationDateTime":expires.to_rfc3339()})),
                false,
            )
            .await;
        match renewed {
            Err(Error::NotFound | Error::ProviderNotFound { .. }) => {
                client
                    .json(
                        account,
                        Method::POST,
                        "https://graph.microsoft.com/v1.0/subscriptions",
                        Some(create_body),
                        false,
                    )
                    .await?
            }
            result => result?,
        }
    } else {
        client
            .json(
                account,
                Method::POST,
                "https://graph.microsoft.com/v1.0/subscriptions",
                Some(create_body),
                false,
            )
            .await?
    };
    Ok(Subscription {
        id: value["id"]
            .as_str()
            .ok_or_else(|| Error::Unavailable("subscription omitted identity".into()))?
            .into(),
        expires_at: value["expirationDateTime"]
            .as_str()
            .and_then(|v| DateTime::parse_from_rfc3339(v).ok())
            .map(|v| v.with_timezone(&Utc))
            .unwrap_or(expires),
        history_id: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn graph_preserves_sender_and_from_and_provider_identity() {
        let message=decode(&json!({"id":"immutable","parentFolderId":"inbox","receivedDateTime":"2026-01-01T00:00:00Z","from":{"emailAddress":{"address":"shared@example.com"}},"sender":{"emailAddress":{"address":"delegate@example.com"}},"isRead":true,"internetMessageHeaders":[{"name":"Auto-Submitted","value":"auto-replied"}]})).unwrap();
        assert_eq!(message.id, "immutable");
        assert_eq!(message.metadata["from"][0]["address"], "shared@example.com");
        assert_eq!(
            message.metadata["sender"][0]["address"],
            "delegate@example.com"
        );
        assert_eq!(
            message.metadata["classification_headers"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(message.flags, vec!["\\Seen"]);
    }
    fn account() -> Account {
        Account {
            id: uuid::Uuid::new_v4().to_string(),
            config: serde_json::from_value(json!({"name":"sent-cutoff","backend":"microsoft","target_mailbox":"shared@example.com","credential_id":uuid::Uuid::new_v4().to_string(),"shared":true,"sync_from":"2026-01-10T00:00:00Z"})).unwrap(),
            state: crate::db::LifecycleState::Created,
            smtp_state: "unvalidated".into(),
            last_connected_at:None,last_disconnected_at:None,next_retry_at:None,retry_count:0,last_error:None,
        }
    }

    #[test]
    fn sent_and_received_cutoffs_are_independent_and_inclusive() {
        let cutoff = DateTime::parse_from_rfc3339("2026-01-10T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let recently_sent = json!({"receivedDateTime":"2026-01-09T23:59:59Z","sentDateTime":"2026-01-10T00:00:00Z"});
        assert!(eligibility_at(&recently_sent, true).unwrap() >= cutoff);
        assert!(eligibility_at(&recently_sent, false).unwrap() < cutoff);
        let recently_received = json!({"receivedDateTime":"2026-01-10T00:00:00Z","sentDateTime":"2026-01-09T23:59:59Z"});
        assert!(eligibility_at(&recently_received, true).unwrap() < cutoff);
        assert!(eligibility_at(&recently_received, false).unwrap() >= cutoff);
        assert!(eligibility_at(&json!({"receivedDateTime":"2026-01-10T00:00:00Z"}), true).is_err());
        assert_eq!(eligibility_field(true), "sentDateTime");
        assert_eq!(eligibility_field(false), "receivedDateTime");
    }

    #[test]
    fn sent_folder_alias_and_configured_opaque_identity_are_recognized() {
        let mut account = account();
        assert_eq!(known_sent_folder(&account, "SentItems"), Some(true));
        assert_eq!(known_sent_folder(&account, "INBOX"), Some(false));
        assert_eq!(known_sent_folder(&account, "opaque-sent-id"), None);
        account.config.sent_folder = Some("opaque-sent-id".into());
        assert_eq!(known_sent_folder(&account, "opaque-sent-id"), Some(true));
    }

    #[tokio::test]
    #[ignore = "requires a dedicated MySQL database in TEST_DATABASE_URL"]
    async fn shared_sent_listing_filters_by_sent_time_and_resolves_opaque_folder()
    -> anyhow::Result<()> {
        use axum::{Json, extract::State, http::Uri};
        use std::sync::atomic::{AtomicUsize, Ordering};
        #[derive(Clone, Default)]
        struct Peer {
            sent_queries: Arc<AtomicUsize>,
            inbox_queries: Arc<AtomicUsize>,
            resolutions: Arc<AtomicUsize>,
        }
        async fn peer(State(peer): State<Peer>, uri: Uri) -> Json<Value> {
            if uri.path().ends_with("/mailFolders/sentitems") {
                peer.resolutions.fetch_add(1, Ordering::SeqCst);
                return Json(json!({"id":"opaque-sent-id"}));
            }
            let sent = uri.path().ends_with("/mailFolders/opaque-sent-id/messages");
            let url = url::Url::parse(&format!("http://localhost{uri}")).unwrap();
            let filter = url
                .query_pairs()
                .find(|(name, _)| name == "$filter")
                .unwrap()
                .1
                .into_owned();
            assert_eq!(
                filter,
                format!(
                    "{} ge 2026-01-10T00:00:00+00:00",
                    if sent {
                        "sentDateTime"
                    } else {
                        "receivedDateTime"
                    }
                )
            );
            if sent {
                peer.sent_queries.fetch_add(1, Ordering::SeqCst);
            } else {
                peer.inbox_queries.fetch_add(1, Ordering::SeqCst);
            }
            Json(json!({"value":[
                {"id":"sent-new","receivedDateTime":"2026-01-09T23:59:59Z","sentDateTime":"2026-01-10T00:00:00Z","isRead":true},
                {"id":"received-new","receivedDateTime":"2026-01-10T00:00:00Z","sentDateTime":"2026-01-09T23:59:59Z","isRead":true}
            ]}))
        }
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .connect(&std::env::var("TEST_DATABASE_URL")?)
            .await?;
        sqlx::migrate!().run(&pool).await?;
        let db = Arc::new(crate::db::Db::new(
            pool,
            crate::crypto::Crypto::new(&[42; 32])?,
        ));
        let mut config = super::super::tests::config();
        config.oauth.providers.get_mut("test").unwrap().shared = true;
        let config = Arc::new(config);
        let oauth = Arc::new(OAuth::new(db.clone(), config.clone()));
        let credential = store::create(&db, "test", Some("refresh"), None).await?;
        oauth
            .seed(
                &credential,
                &crate::oauth::TokenResponse {
                    access_token: "test-token".into(),
                    refresh_token: None,
                    expires_in: 3600,
                },
            )
            .await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let peer_state = Peer::default();
        let app = axum::Router::new()
            .fallback(peer)
            .with_state(peer_state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = NativeClient::with_test_endpoint(config, oauth, format!("http://{address}"));
        let mut account = account();
        account.config.credential_id = Some(credential.id.clone());
        let sent = sync_page(&client, &account, "opaque-sent-id", None, None, true).await?;
        assert_eq!(sent.messages.len(), 1);
        assert_eq!(sent.messages[0].id, "sent-new");
        assert!(sent.messages[0].received_at < account.config.sync_from.unwrap());
        assert_eq!(sent.messages[0].metadata["eligibility_time_basis"], "sent");
        assert_eq!(
            sent.messages[0].metadata["eligibility_at"],
            "2026-01-10T00:00:00Z"
        );
        let inbox = sync_page(&client, &account, "inbox", None, None, true).await?;
        assert_eq!(inbox.messages.len(), 1);
        assert_eq!(inbox.messages[0].id, "received-new");
        assert_eq!(peer_state.resolutions.load(Ordering::SeqCst), 1);
        assert_eq!(peer_state.sent_queries.load(Ordering::SeqCst), 1);
        assert_eq!(peer_state.inbox_queries.load(Ordering::SeqCst), 1);
        store::delete(&db, &credential.id).await?;
        server.abort();
        Ok(())
    }
}
