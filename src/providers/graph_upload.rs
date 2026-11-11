use super::*;
use crate::graph_message::GraphAttachment;
use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE};

pub const GRAPH_SMALL_ATTACHMENT_LIMIT: usize = 3_000_000;
pub const GRAPH_MAX_ATTACHMENT_BYTES: usize = 150_000_000;
pub const GRAPH_UPLOAD_CHUNK_BYTES: usize = 3_276_800;
pub const GRAPH_CONTROL_REQUEST_BYTES: usize = 4 * 1024 * 1024;
pub const GRAPH_MAX_BASE_MIME_BYTES: usize = 3_000_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GraphDraft {
    pub id: String,
    pub internet_message_id: Option<String>,
    pub is_draft: bool,
    pub etag: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct GraphUploadSession {
    pub url: String,
    pub expires_at: DateTime<Utc>,
    pub next_offset: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GraphUploadProgress {
    pub next_offset: u64,
    pub complete: bool,
    pub attachment_id: Option<String>,
}

fn graph_account(account: &Account) -> Result<()> {
    if account.config.backend != Backend::Microsoft {
        return Err(Error::Invalid(
            "Graph draft operations require a Microsoft account".into(),
        ));
    }
    Ok(())
}

fn draft(value: Value, uncertain: bool) -> Result<GraphDraft> {
    let invalid = || {
        if uncertain {
            Error::UnknownOutcome
        } else {
            Error::Unavailable("Graph draft response omitted identity or state".into())
        }
    };
    let id = value["id"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(invalid)?;
    let is_draft = value["isDraft"].as_bool().ok_or_else(invalid)?;
    Ok(GraphDraft {
        id: id.into(),
        internet_message_id: value["internetMessageId"].as_str().map(str::to_owned),
        is_draft,
        etag: value["@odata.etag"].as_str().map(str::to_owned),
    })
}

fn next_offset(value: &Value, total: u64) -> Result<u64> {
    let ranges = value["nextExpectedRanges"]
        .as_array()
        .filter(|ranges| ranges.len() == 1)
        .ok_or_else(|| {
            Error::Unavailable("upload session returned invalid expected ranges".into())
        })?;
    let range = ranges[0].as_str().ok_or_else(|| {
        Error::Unavailable("upload session returned invalid expected range".into())
    })?;
    let start = range
        .split('-')
        .next()
        .and_then(|start| start.parse::<u64>().ok())
        .filter(|start| *start <= total)
        .ok_or_else(|| Error::Unavailable("upload session returned invalid offset".into()))?;
    Ok(start)
}

impl NativeClient {
    async fn graph_json(
        &self,
        account: &Account,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        graph_account(account)?;
        let body = body
            .map(|value| serde_json::to_vec(&value))
            .transpose()
            .map_err(|_| Error::Internal("Graph request serialization failed".into()))?;
        if body
            .as_ref()
            .is_some_and(|body| body.len() > GRAPH_CONTROL_REQUEST_BYTES)
        {
            return Err(Error::Unsupported(
                "Graph draft control request exceeds 4 MiB".into(),
            ));
        }
        let mutating = method != Method::GET;
        let bytes = self
            .request(
                account,
                method,
                path,
                body.as_deref(),
                Some("application/json"),
                false,
            )
            .await?;
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&bytes).map_err(|_| {
            if mutating {
                Error::UnknownOutcome
            } else {
                Error::Unavailable("invalid Graph draft response".into())
            }
        })
    }

    pub async fn graph_create_draft(
        &self,
        account: &Account,
        base_mime: &[u8],
    ) -> Result<GraphDraft> {
        graph_account(account)?;
        if base_mime.len() > GRAPH_MAX_BASE_MIME_BYTES + 128 {
            return Err(Error::Unsupported(
                "Graph draft body exceeds the supported 3,000,000-byte MIME limit".into(),
            ));
        }
        let encoded = STANDARD.encode(base_mime);
        if encoded.len() > GRAPH_CONTROL_REQUEST_BYTES {
            return Err(Error::Unsupported(
                "Graph draft request exceeds 4 MiB".into(),
            ));
        }
        let bytes = self
            .request(
                account,
                Method::POST,
                "/messages",
                Some(encoded.as_bytes()),
                Some("text/plain"),
                false,
            )
            .await?;
        let value = serde_json::from_slice(&bytes).map_err(|_| Error::UnknownOutcome)?;
        let draft = draft(value, true)?;
        if !draft.is_draft {
            return Err(Error::UnknownOutcome);
        }
        Ok(draft)
    }

    pub async fn graph_get_draft(&self, account: &Account, draft_id: &str) -> Result<GraphDraft> {
        draft(
            self.graph_json(
                account,
                Method::GET,
                &query(
                    &format!("/messages/{}", segment(draft_id)),
                    &[("$select", "id,isDraft,internetMessageId".into())],
                ),
                None,
            )
            .await?,
            false,
        )
    }

    pub async fn graph_draft_content(&self, account: &Account, draft_id: &str) -> Result<Value> {
        self.graph_json(account,Method::GET,&query(&format!("/messages/{}",segment(draft_id)),&[("$select","subject,body,from,sender,toRecipients,ccRecipients,bccRecipients,replyTo,internetMessageHeaders".into())]),None).await
    }

    pub async fn graph_find_drafts(
        &self,
        account: &Account,
        send_id: &str,
    ) -> Result<Vec<GraphDraft>> {
        let mut path = Some(query(
            "/mailFolders/drafts/messages",
            &[
                (
                    "$select",
                    "id,isDraft,internetMessageId,internetMessageHeaders".into(),
                ),
                ("$top", "100".into()),
            ],
        ));
        let mut result = Vec::new();
        let mut pages = 0;
        while let Some(next) = path {
            pages += 1;
            if pages > 100 {
                return Err(Error::Unsupported(
                    "draft marker search exceeded its pagination limit".into(),
                ));
            }
            let value = self.graph_json(account, Method::GET, &next, None).await?;
            for item in value["value"].as_array().into_iter().flatten() {
                let marker = item["internetMessageHeaders"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|header| {
                        header["name"]
                            .as_str()
                            .is_some_and(|name| name.eq_ignore_ascii_case("X-Rustbox-Send-Id"))
                            && header["value"].as_str() == Some(send_id)
                    });
                if marker {
                    let found = draft(item.clone(), false)?;
                    if found.is_draft {
                        result.push(found);
                    }
                }
            }
            path = value["@odata.nextLink"].as_str().map(str::to_owned);
        }
        Ok(result)
    }

    pub async fn graph_delete_draft(&self, account: &Account, draft_id: &str) -> Result<()> {
        let draft = self.graph_get_draft(account, draft_id).await?;
        if !draft.is_draft {
            return Err(Error::Conflict("Graph message is no longer a draft".into()));
        }
        let etag = draft.etag.ok_or_else(|| {
            Error::Conflict("Graph draft has no concurrency token for safe deletion".into())
        })?;
        self.request_headers(
            account,
            Method::DELETE,
            &format!("/messages/{}", segment(draft_id)),
            None,
            None,
            false,
            &[("If-Match", &etag)],
        )
        .await?;
        Ok(())
    }

    pub async fn graph_add_small_attachment(
        &self,
        account: &Account,
        draft_id: &str,
        attachment: &GraphAttachment,
    ) -> Result<String> {
        if attachment.data.len() >= GRAPH_SMALL_ATTACHMENT_LIMIT {
            return Err(Error::Unsupported(
                "Graph attachments of 3 MB or larger require an upload session".into(),
            ));
        }
        let mut body = json!({"@odata.type":"#microsoft.graph.fileAttachment","name":attachment.name,"contentType":attachment.content_type,"isInline":attachment.is_inline,"contentBytes":STANDARD.encode(&attachment.data)});
        if let Some(content_id) = &attachment.content_id {
            body["contentId"] = json!(content_id);
        }
        let value = self
            .graph_json(
                account,
                Method::POST,
                &format!("/messages/{}/attachments", segment(draft_id)),
                Some(body),
            )
            .await?;
        value["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .ok_or(Error::UnknownOutcome)
    }

    pub async fn graph_create_upload(
        &self,
        account: &Account,
        draft_id: &str,
        attachment: &GraphAttachment,
    ) -> Result<GraphUploadSession> {
        if attachment.data.len() < GRAPH_SMALL_ATTACHMENT_LIMIT
            || attachment.data.len() > GRAPH_MAX_ATTACHMENT_BYTES
        {
            return Err(Error::Unsupported(
                "Graph upload sessions support attachments from 3 MB to 150 MB".into(),
            ));
        }
        let mut item = json!({"attachmentType":"file","name":attachment.name,"contentType":attachment.content_type,"isInline":attachment.is_inline,"size":attachment.data.len()});
        if let Some(content_id) = &attachment.content_id {
            item["contentId"] = json!(content_id);
        }
        let value = self
            .graph_json(
                account,
                Method::POST,
                &format!(
                    "/messages/{}/attachments/createUploadSession",
                    segment(draft_id)
                ),
                Some(json!({"AttachmentItem":item})),
            )
            .await?;
        let url = value["uploadUrl"]
            .as_str()
            .ok_or(Error::UnknownOutcome)?
            .to_owned();
        self.validate_upload_url(&url)?;
        let expires_at = value["expirationDateTime"]
            .as_str()
            .and_then(|date| DateTime::parse_from_rfc3339(date).ok())
            .map(|date| date.with_timezone(&Utc))
            .filter(|date| *date > Utc::now())
            .ok_or(Error::UnknownOutcome)?;
        Ok(GraphUploadSession {
            url,
            expires_at,
            next_offset: next_offset(&value, attachment.data.len() as u64)?,
        })
    }

    pub async fn graph_list_attachments(
        &self,
        account: &Account,
        draft_id: &str,
    ) -> Result<Vec<Value>> {
        let mut path = Some(query(
            &format!("/messages/{}/attachments", segment(draft_id)),
            &[
                (
                    "$select",
                    "id,name,size,contentType,isInline,contentId".into(),
                ),
                ("$top", "100".into()),
            ],
        ));
        let mut result = Vec::new();
        let mut pages = 0;
        while let Some(next) = path {
            pages += 1;
            if pages > 100 {
                return Err(Error::Unsupported(
                    "Graph attachment listing exceeded its pagination limit".into(),
                ));
            }
            let value = self.graph_json(account, Method::GET, &next, None).await?;
            result.extend(value["value"].as_array().into_iter().flatten().cloned());
            if result.len() > 1000 {
                return Err(Error::Unsupported("too many draft attachments".into()));
            }
            path = value["@odata.nextLink"].as_str().map(str::to_owned);
        }
        Ok(result)
    }

    pub async fn graph_attachment_bytes(
        &self,
        account: &Account,
        draft_id: &str,
        attachment_id: &str,
    ) -> Result<Vec<u8>> {
        graph_account(account)?;
        self.request(
            account,
            Method::GET,
            &format!(
                "/messages/{}/attachments/{}/$value",
                segment(draft_id),
                segment(attachment_id)
            ),
            None,
            None,
            false,
        )
        .await
    }

    pub async fn graph_sent_candidate(
        &self,
        account: &Account,
        draft_id: &str,
    ) -> Result<Option<ProviderMessage>> {
        let value=match self.json(account,Method::GET,&format!("/messages/{}?$select=id,parentFolderId,internetMessageId,internetMessageHeaders,receivedDateTime,sentDateTime,isDraft,from,sender,toRecipients,ccRecipients,bccRecipients,replyTo,subject,isRead,flag",segment(draft_id)),None,false).await {
            Ok(value)=>value,Err(Error::NotFound | Error::ProviderNotFound { .. })=>return Ok(None),Err(error)=>return Err(error),
        };
        if value["isDraft"].as_bool() != Some(false) {
            return Ok(None);
        }
        let folder = account.config.sent_folder.as_deref().unwrap_or("sentitems");
        let sent = self
            .json(
                account,
                Method::GET,
                &format!("/mailFolders/{}?$select=id", segment(folder)),
                None,
                false,
            )
            .await?;
        if value["parentFolderId"].as_str() != sent["id"].as_str() || sent["id"].is_null() {
            return Ok(None);
        }
        Ok(Some(super::microsoft::decode(&value)?))
    }
    pub async fn graph_send_draft(&self, account: &Account, draft_id: &str) -> Result<()> {
        graph_account(account)?;
        self.request(
            account,
            Method::POST,
            &format!("/messages/{}/send", segment(draft_id)),
            Some(b""),
            None,
            true,
        )
        .await?;
        Ok(())
    }

    fn validate_upload_url(&self, value: &str) -> Result<()> {
        let invalid = || Error::Invalid("invalid Graph upload destination".into());
        if value.len() > 16384 {
            return Err(invalid());
        }
        let url = url::Url::parse(value).map_err(|_| invalid())?;
        if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
            return Err(invalid());
        }
        #[cfg(test)]
        if let Some(endpoint) = &self.endpoint {
            let endpoint = url::Url::parse(endpoint).map_err(|_| invalid())?;
            if url.scheme() == endpoint.scheme()
                && url.host_str() == endpoint.host_str()
                && url.port_or_known_default() == endpoint.port_or_known_default()
            {
                return Ok(());
            }
        }
        if url.scheme() != "https"
            || url.port_or_known_default() != Some(443)
            || !matches!(
                url.host_str(),
                Some("outlook.office.com" | "outlook.office365.com")
            )
        {
            return Err(invalid());
        }
        Ok(())
    }

    pub async fn graph_upload_status(
        &self,
        upload_url: &str,
        total: u64,
    ) -> Result<GraphUploadProgress> {
        if total == 0 || total > GRAPH_MAX_ATTACHMENT_BYTES as u64 {
            return Err(Error::Invalid("invalid attachment upload size".into()));
        }
        self.upload_request(upload_url, None, 0, total).await
    }

    pub async fn graph_upload_chunk(
        &self,
        upload_url: &str,
        bytes: &[u8],
        start: u64,
        total: u64,
    ) -> Result<GraphUploadProgress> {
        let end = start
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| Error::Invalid("invalid attachment upload range".into()))?;
        if bytes.is_empty()
            || bytes.len() > GRAPH_UPLOAD_CHUNK_BYTES
            || total > GRAPH_MAX_ATTACHMENT_BYTES as u64
            || end > total
        {
            return Err(Error::Invalid("invalid attachment upload range".into()));
        }
        self.upload_request(upload_url, Some(bytes), start, total)
            .await
    }

    async fn upload_request(
        &self,
        upload_url: &str,
        bytes: Option<&[u8]>,
        start: u64,
        total: u64,
    ) -> Result<GraphUploadProgress> {
        self.validate_upload_url(upload_url)?;
        let state = provider_state(Backend::Microsoft, self.config.native.concurrency);
        let _permit = state
            .slots
            .acquire()
            .await
            .map_err(|_| Error::Unavailable("provider upload scheduler stopped".into()))?;
        #[cfg(test)]
        let client = if self.endpoint.is_some() {
            reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(120))
                .build()
                .map_err(|_| Error::Internal("upload client initialization failed".into()))?
        } else {
            crate::security::safe_http_client(upload_url, false, None).await?
        };
        #[cfg(not(test))]
        let client = crate::security::safe_http_client(upload_url, false, None).await?;
        let mut request = client
            .request(
                if bytes.is_some() {
                    Method::PUT
                } else {
                    Method::GET
                },
                upload_url,
            )
            .timeout(Duration::from_secs(120));
        if let Some(bytes) = bytes {
            request = request
                .header(CONTENT_TYPE, "application/octet-stream")
                .header(CONTENT_LENGTH, bytes.len())
                .header(
                    CONTENT_RANGE,
                    format!("bytes {start}-{}/{total}", start + bytes.len() as u64 - 1),
                )
                .body(bytes.to_vec());
        }
        state.requests.fetch_add(1, Ordering::Relaxed);
        let response = request.send().await.map_err(|_| {
            Error::Unavailable(
                "attachment upload request interrupted; inspect session state before continuing"
                    .into(),
            )
        })?;
        let status = response.status();
        match status.as_u16() {
            200..=299 => {},
            404 | 410 => return Err(Error::NotFound),
            401 | 403 => return Err(Error::Unavailable("attachment upload session is no longer authorized".into())),
            409 | 416 => return Err(Error::Conflict("attachment upload range must be reconciled".into())),
            429 => {
                state.throttled.fetch_add(1,Ordering::Relaxed);
                let seconds=response.headers().get("retry-after").and_then(|v|v.to_str().ok()).and_then(|v|v.parse::<u64>().ok()).unwrap_or(30).min(86400);
                return Err(Error::Throttled(seconds));
            }
            500..=599 => return Err(Error::Unavailable("attachment upload temporarily unavailable; inspect session state before continuing".into())),
            _=>return Err(Error::Invalid("attachment upload was rejected".into())),
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk=chunk.map_err(|_|Error::Unavailable("attachment upload response interrupted; inspect session state before continuing".into()))?;
            if body.len() + chunk.len() > 65536 {
                return Err(Error::Unavailable(
                    "attachment upload returned an oversized response".into(),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        let value = if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body).map_err(|_| {
                Error::Unavailable("attachment upload returned invalid state".into())
            })?
        };
        if bytes.is_some() && status == StatusCode::CREATED {
            return Ok(GraphUploadProgress {
                next_offset: total,
                complete: true,
                attachment_id: value["id"].as_str().map(str::to_owned),
            });
        }
        Ok(GraphUploadProgress {
            next_offset: next_offset(&value, total)?,
            complete: false,
            attachment_id: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Bytes, extract::State, http::HeaderMap, response::IntoResponse};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn client(endpoint: Option<String>) -> NativeClient {
        let config = Arc::new(super::super::tests::config());
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .connect_lazy("mysql://localhost/test")
            .unwrap();
        let db = Arc::new(crate::db::Db::new(
            pool,
            crate::crypto::Crypto::new(&[42; 32]).unwrap(),
        ));
        let oauth = Arc::new(OAuth::new(db, config.clone()));
        match endpoint {
            Some(endpoint) => NativeClient::with_test_endpoint(config, oauth, endpoint),
            None => NativeClient::new(config, oauth),
        }
    }

    #[tokio::test]
    async fn upload_urls_are_restricted_without_exposing_secret_urls() {
        let client = client(None);
        for url in [
            "https://outlook.office.com/upload?authtoken=secret",
            "https://outlook.office365.com/upload?authtoken=secret",
        ] {
            assert!(client.validate_upload_url(url).is_ok());
        }
        for url in [
            "http://outlook.office.com/upload?authtoken=secret",
            "https://outlook.office.com.attacker.example/upload?authtoken=secret",
            "https://graph.microsoft.com/upload?authtoken=secret",
            "https://outlook.office.com:8443/upload?authtoken=secret",
            "https://user:secret@outlook.office.com/upload",
            "https://127.0.0.1/upload?authtoken=secret",
            "https://outlook.office.com/upload#secret",
        ] {
            let error = client.validate_upload_url(url).unwrap_err().to_string();
            assert!(!error.contains("secret"));
            assert!(!error.contains(url));
        }
    }

    #[test]
    fn upload_offsets_accept_documented_forms_and_reject_gaps() {
        assert_eq!(
            next_offset(&json!({"nextExpectedRanges":["3200000-"]}), 4_000_000).unwrap(),
            3_200_000
        );
        assert_eq!(
            next_offset(&json!({"nextExpectedRanges":["3200000"]}), 4_000_000).unwrap(),
            3_200_000
        );
        for value in [
            json!({}),
            json!({"nextExpectedRanges":[]}),
            json!({"nextExpectedRanges":["a-"]}),
            json!({"nextExpectedRanges":["5000000-"]}),
            json!({"nextExpectedRanges":["0-2","4-"]}),
        ] {
            assert!(next_offset(&value, 4_000_000).is_err());
        }
    }

    #[derive(Clone, Default)]
    struct Peer {
        offset: Arc<AtomicUsize>,
        requests: Arc<AtomicUsize>,
    }

    async fn upload_peer(
        State(peer): State<Peer>,
        method: Method,
        headers: HeaderMap,
        body: Bytes,
    ) -> axum::response::Response {
        assert!(!headers.contains_key("authorization"));
        assert!(!headers.contains_key("prefer"));
        peer.requests.fetch_add(1, Ordering::SeqCst);
        if method == Method::GET {
            return axum::Json(
                json!({"nextExpectedRanges":[format!("{}-",peer.offset.load(Ordering::SeqCst))]}),
            )
            .into_response();
        }
        assert_eq!(method, Method::PUT);
        assert_eq!(headers["content-type"], "application/octet-stream");
        assert_eq!(headers["content-length"], body.len().to_string());
        match peer.offset.load(Ordering::SeqCst) {
            0 => {
                assert_eq!(headers["content-range"], "bytes 0-4/10");
                assert_eq!(&body[..], b"first");
                peer.offset.store(5, Ordering::SeqCst);
                StatusCode::BAD_GATEWAY.into_response()
            }
            5 => {
                assert_eq!(headers["content-range"], "bytes 5-9/10");
                assert_eq!(&body[..], b"final");
                peer.offset.store(10, Ordering::SeqCst);
                (
                    StatusCode::CREATED,
                    axum::Json(json!({"id":"attachment-1"})),
                )
                    .into_response()
            }
            _ => panic!("completed bytes must never be uploaded again"),
        }
    }

    #[tokio::test]
    async fn upload_resumes_from_authoritative_state_without_bearer_tokens() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let peer = Peer::default();
        let app = axum::Router::new()
            .route("/upload", axum::routing::any(upload_peer))
            .with_state(peer.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = client(Some(format!("http://{address}")));
        let url = format!("http://{address}/upload?authtoken=test-secret");
        assert_eq!(
            client
                .graph_upload_status(&url, 10)
                .await
                .unwrap()
                .next_offset,
            0
        );
        assert!(matches!(
            client.graph_upload_chunk(&url, b"first", 0, 10).await,
            Err(Error::Unavailable(_))
        ));
        let status = client.graph_upload_status(&url, 10).await.unwrap();
        assert_eq!(status.next_offset, 5);
        assert!(!status.complete);
        let status = client
            .graph_upload_chunk(&url, b"final", 5, 10)
            .await
            .unwrap();
        assert!(status.complete);
        assert_eq!(status.next_offset, 10);
        assert_eq!(status.attachment_id.as_deref(), Some("attachment-1"));
        assert_eq!(peer.requests.load(Ordering::SeqCst), 4);
        server.abort();
    }

    #[tokio::test]
    async fn upload_range_validation_happens_before_network() {
        let client = client(None);
        let url = "https://outlook.office.com/upload?authtoken=secret";
        assert!(client.graph_upload_chunk(url, &[], 0, 10).await.is_err());
        assert!(client.graph_upload_chunk(url, b"x", 10, 10).await.is_err());
        assert!(
            client
                .graph_upload_chunk(url, b"x", u64::MAX, 10)
                .await
                .is_err()
        );
        assert!(
            client
                .graph_upload_chunk(url, &vec![0; GRAPH_UPLOAD_CHUNK_BYTES + 1], 0, 10_000_000)
                .await
                .is_err()
        );
    }
}

#[cfg(test)]
mod draft_tests {
    use super::*;
    use axum::{
        body::Bytes,
        extract::State,
        http::{HeaderMap, Uri},
        response::IntoResponse,
    };

    #[derive(Clone)]
    struct Peer {
        endpoint: String,
    }
    async fn peer(
        State(peer): State<Peer>,
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> axum::response::Response {
        assert_eq!(headers["authorization"], "Bearer test-token");
        assert_eq!(headers["prefer"], "IdType=\"ImmutableId\"");
        let draft = json!({"id":"draft-1","isDraft":true,"internetMessageId":"<message@example.com>","@odata.etag":"W/\"version-1\""});
        if method == Method::DELETE {
            assert_eq!(headers["if-match"], "W/\"version-1\"");
            return StatusCode::NO_CONTENT.into_response();
        }
        if uri.path() == "/messages" {
            assert_eq!(method, Method::POST);
            assert_eq!(headers["content-type"], "text/plain");
            assert_eq!(
                STANDARD.decode(&body).unwrap(),
                b"X-Rustbox-Send-Id: send-1\r\nSubject: draft\r\n\r\nbody"
            );
            return (StatusCode::CREATED, axum::Json(draft)).into_response();
        }
        if uri.path().ends_with("/attachments/createUploadSession") {
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["AttachmentItem"]["size"], GRAPH_SMALL_ATTACHMENT_LIMIT);
            assert_eq!(body["AttachmentItem"]["contentId"], "inline-image");
            assert_eq!(body["AttachmentItem"]["isInline"], true);
            return (StatusCode::CREATED,axum::Json(json!({"uploadUrl":format!("{}/upload?authtoken=test-secret",peer.endpoint),"expirationDateTime":(Utc::now()+chrono::Duration::hours(1)).to_rfc3339(),"nextExpectedRanges":["0-"]}))).into_response();
        }
        if uri.path().ends_with("/attachments") {
            if method == Method::GET {
                return axum::Json(json!({"value":[{"id":"small-1","name":"inline.png","size":5,"isInline":true,"contentId":"inline-image","contentType":"image/png"}]})).into_response();
            }
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["name"], "inline.png");
            assert_eq!(body["isInline"], true);
            assert_eq!(body["contentId"], "inline-image");
            assert_eq!(
                STANDARD
                    .decode(body["contentBytes"].as_str().unwrap())
                    .unwrap(),
                b"image"
            );
            return (StatusCode::CREATED, axum::Json(json!({"id":"small-1"}))).into_response();
        }
        if uri.path() == "/mailFolders/drafts/messages" {
            let mut found = draft.clone();
            found["internetMessageHeaders"] =
                json!([{"name":"x-rustbox-send-id","value":"send-1"}]);
            return axum::Json(json!({"value":[draft,found]})).into_response();
        }
        if uri.path().ends_with("/send") {
            return StatusCode::BAD_GATEWAY.into_response();
        }
        if uri.path().ends_with("/$value") {
            return b"image".to_vec().into_response();
        }
        axum::Json(draft).into_response()
    }

    #[tokio::test]
    #[ignore = "requires a dedicated MySQL database in TEST_DATABASE_URL"]
    async fn draft_upload_setup_preserves_mime_and_inline_metadata_and_final_send_is_ambiguous()
    -> anyhow::Result<()> {
        let config = Arc::new(super::super::tests::config());
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .connect(&std::env::var("TEST_DATABASE_URL")?)
            .await?;
        sqlx::migrate!().run(&pool).await?;
        let db = Arc::new(crate::db::Db::new(
            pool,
            crate::crypto::Crypto::new(&[42; 32])?,
        ));
        let credential = store::create(&db, "test", Some("refresh"), None).await?;
        let oauth = Arc::new(OAuth::new(db.clone(), config.clone()));
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
        let endpoint = format!("http://{address}");
        let app = axum::Router::new().fallback(peer).with_state(Peer {
            endpoint: endpoint.clone(),
        });
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = NativeClient::with_test_endpoint(config, oauth, endpoint);
        let account = Account {
            id: uuid::Uuid::new_v4().to_string(),
            config: serde_json::from_value(
                json!({"name":"draft-test","backend":"microsoft","credential_id":credential.id,"target_mailbox":"mailbox@example.com"}),
            )?,
            state: crate::db::LifecycleState::Created,
            smtp_state: "unvalidated".into(),
            last_connected_at: None,
            last_disconnected_at: None,
            next_retry_at: None,
            retry_count: 0,
            last_error: None,
        };
        let draft = client
            .graph_create_draft(
                &account,
                b"X-Rustbox-Send-Id: send-1\r\nSubject: draft\r\n\r\nbody",
            )
            .await?;
        assert!(draft.is_draft);
        assert_eq!(draft.id, "draft-1");
        assert_eq!(
            client
                .graph_get_draft(&account, &draft.id)
                .await?
                .internet_message_id
                .as_deref(),
            Some("<message@example.com>")
        );
        assert_eq!(client.graph_find_drafts(&account, "send-1").await?.len(), 1);
        let mut attachment = GraphAttachment {
            name: "inline.png".into(),
            content_type: "image/png".into(),
            content_id: Some("inline-image".into()),
            is_inline: true,
            data: b"image".to_vec(),
        };
        assert_eq!(
            client
                .graph_add_small_attachment(&account, &draft.id, &attachment)
                .await?,
            "small-1"
        );
        assert_eq!(
            client
                .graph_list_attachments(&account, &draft.id)
                .await?
                .len(),
            1
        );
        assert_eq!(
            client
                .graph_attachment_bytes(&account, &draft.id, "small-1")
                .await?,
            b"image"
        );
        attachment.data = vec![0; GRAPH_SMALL_ATTACHMENT_LIMIT];
        let session = client
            .graph_create_upload(&account, &draft.id, &attachment)
            .await?;
        assert_eq!(session.next_offset, 0);
        assert!(session.expires_at > Utc::now());
        assert!(matches!(
            client.graph_send_draft(&account, &draft.id).await,
            Err(Error::UnknownOutcome)
        ));
        client.graph_delete_draft(&account, &draft.id).await?;
        store::delete(&db, &credential.id).await?;
        server.abort();
        Ok(())
    }
}
