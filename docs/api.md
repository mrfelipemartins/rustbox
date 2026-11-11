# API guide

[Documentation](README.md) · [Rustbox](../README.md)

Use bearer authentication for `/v1` and metrics. All configured application keys share access to accounts and credentials. The [OpenAPI specification](../openapi.json) defines request and response schemas. Public provider-notification endpoints use provider-specific validation instead of application keys.

## Connect an account

`POST /v1/accounts` requires `Idempotency-Key`. An identical retry by the same API application returns the original account; a different payload under that key returns `409`. Deleted-account keys remain tombstoned for 30 days and do not create replacement monitors.

A Microsoft shared-mailbox account uses a credential authorized by a different user:

```json
{
  "name": "Support",
  "backend": "microsoft",
  "credential_id": "credential-uuid",
  "authorizing_identity": "agent@example.com",
  "target_mailbox": "support@example.com",
  "shared": true,
  "sync_from": "2026-11-01T00:00:00Z",
  "watched_folders": ["inbox", "sentitems"],
  "sent_folder": "sentitems",
  "webhook_url": "https://app.example.com/mail-events",
  "webhook_secret": "replace-with-at-least-32-random-characters"
}
```

Use `backend: "google"` for the Gmail API, with the mailbox owner's credential or an authorized Workspace impersonation credential. Gmail browser delegation alone does not grant API access to another mailbox. Native accounts use Graph or Gmail for reading, mutations, and sending; see [provider setup](providers.md) and [OAuth](oauth.md).

Generic IMAP/SMTP accounts retain explicit server settings:

```json
{
  "name": "Other provider",
  "backend": "imap",
  "imap": {"host":"imap.example.com","port":993,"username":"support@example.com","password":"app-password","tls_mode":"implicit"},
  "smtp": {"host":"smtp.example.com","port":465,"username":"support@example.com","password":"app-password","tls_mode":"implicit"},
  "sync_from": "2026-11-01T00:00:00Z",
  "watched_folders": ["Sent"],
  "trash_folder": "Trash"
}
```

IMAP/SMTP require verified TLS. Private/reserved destinations are blocked unless explicitly enabled for tests. Native notification delivery requires the configured public HTTPS base URL.

`sync_from` is inclusive and persisted. Omitting it resolves the default 30-day window once at account creation. Initialization and recovery discover eligible messages still available at the provider. Messages removed before observation or caching cannot be recovered. The target mailbox and backend identify the account and cannot be changed in place.

Creation returns before initial synchronization finishes. Read authorization, synchronization, subscription, and sending health with `GET /v1/accounts/{id}`. Native accounts report readiness rather than an IMAP connection. `PUT` replaces account configuration; pause/resume use POST. Delete removes service-side data and stops monitoring without deleting provider mail. A submission already in flight may complete.

## Messages and synchronization

- `GET /v1/accounts/{id}/folders` lists provider folders or Gmail labels.
- `GET /v1/accounts/{id}/messages?mailbox=<folder-id>&limit=100` lists indexed metadata. Pass the returned `next_cursor` as `after` to continue the scan. Use the events cursor for ongoing changes.
- `GET /v1/accounts/{id}/messages/{message_id}` returns metadata, locations, content availability, identity confidence, and discovery reason.
- `GET /v1/accounts/{id}/messages/{message_id}/content` returns parsed content; `?raw=true` returns RFC822 MIME.
- `GET /v1/accounts/{id}/messages/{message_id}/attachments/{index}` returns attachment bytes.
- For native accounts, `POST /v1/accounts/{id}/sync` returns `202` with a run `id`; read it with `GET /v1/syncs/{id}`. Concurrent requests coalesce.

Use Rustbox message IDs for operations. Graph immutable IDs preserve identity across supported moves within one mailbox. Gmail labels are memberships of one message. Generic IMAP preserves identity for confirmed moves with reliable UID mappings; external moves or UIDVALIDITY resets can remain uncertain. Rustbox does not merge messages solely by RFC Message-ID.

Normalized metadata includes Message-ID, In-Reply-To, ordered References, From, Sender, Reply-To, visible To/Cc/Bcc, dates, raw headers, classification headers, and attachment filename/type/disposition/Content-ID/size. Missing Bcc stays missing. Classification headers are reported evidence rather than security verdicts. HTML remains untrusted and must be sanitized by the consuming application.

Subscribe to `message.created` for metadata discovery and `message.content_ready` for cached availability. Readiness includes `available_until`; content remains available through that timestamp even if the provider message moves or disappears. Defaults are seven days, a 25 MiB message limit, and a 10 GiB raw-MIME cache budget. Full capacity delays readiness without evicting promised content. Oversized or vanished content produces explicit unavailability; after expiry, provider retrieval may return `410 content_unavailable`.

## Mutations

Flags, move, trash, and Gmail labels are synchronous:

```http
PATCH /v1/accounts/{id}/flags
```

```json
{"message_id":"rustbox-message-uuid","flags":["\\Seen"]}
```

`POST /v1/accounts/{id}/move` takes `message_id`, `destination`, and optional `source`. For Gmail, it removes the specified source label and adds the destination without deleting other labels. `PATCH /v1/accounts/{id}/labels` accepts `message_id`, `add`, and `remove` label-ID arrays. `POST /v1/accounts/{id}/trash` takes `message_id`.

IMAP moves require UID MOVE or a safe UIDPLUS fallback; broad EXPUNGE is never used. Ambiguous mutation failures return `unknown_outcome`; inspect current locations before retrying. Native mutations schedule reconciliation afterward.

## Sending and replies

`POST /v1/accounts/{id}/send` requires `Idempotency-Key` and returns `202` with a durable send job:

```json
{
  "from":"support@example.com",
  "to":["customer@example.net"],
  "reply_to":["Support <support@example.com>"],
  "subject":"Re: Question",
  "reply_to_message_id":"rustbox-message-uuid",
  "text":"Here is the answer.",
  "html":"<p>Here is the answer.</p><img src=\"cid:logo@example.com\">",
  "attachments":[{"filename":"logo.png","content_type":"image/png","data_base64":"...","disposition":"inline","content_id":"logo@example.com"}]
}
```

`reply_to_message_id` derives RFC linkage and Gmail threading from the source. Alternatively provide `in_reply_to` as a bracketed RFC Message-ID and `references` as an ordered array. Contradictory linkage is rejected. `reply_to` contains addresses for the Reply-To header. Attachment disposition defaults to `attachment`; inline parts require HTML and unique Content-IDs without angle brackets. Header injection and malformed identifiers are rejected.

`GET /v1/sends/{id}` reports submission, provider identifiers, reconciliation, and resolution separately. Acceptance means the provider accepted submission, not recipient delivery. Definite temporary failures retry for up to 24 hours and respect throttling. Ambiguous submissions become `unknown` and never resend automatically. IMAP Sent append is opt-in; native Sent copies are provider-managed. An append failure never resends accepted mail.

`POST /v1/sends/{id}/reconcile` searches accessible Sent locations using the exact generated RFC Message-ID plus sender, recipients, and content/attachment evidence. Results are `matched`, `ambiguous`, `not_found`, or `unavailable`. A matched Sent copy does not prove recipient delivery. Absence never means retrying is safe.

`POST /v1/sends/{id}/resolve` records `{"decision":"unresolved","reason":"Investigation closed","evidence":{"ticket":"case-123"}}`. Decision `sent` requires matched reconciliation evidence. Resolution preserves the original unknown submission outcome and never submits mail. Deliberately retry using a new send request/key with `retry_of_send_id` referencing the original.

Optional IMAP Sent-copy recovery uses the original encrypted MIME artifact and never calls SMTP again. `status` remains `accepted`; `append_status` separately reports `failed` for a definite rejection/non-start, `unknown` after an uncertain or interrupted APPEND, `complete`, or `expired`. `append_last_error`, `append_attempts`, `append_reconciliation_state`, `append_reconciliation_evidence`, and `append_recoverable_until` expose recovery status.

- `POST /v1/sends/{id}/append/reconcile` searches the original Sent folder using the exact Message-ID plus sender, recipients, and content evidence. A confirmed match completes the copy and releases the artifact.
- `POST /v1/sends/{id}/append/retry` with `{}` queues a definite failed copy again. An unknown copy is first reconciled. Ambiguous/unavailable reconciliation blocks retry; absence alone also blocks it. After a conclusive `not_found`, an intentional retry requires `{"allow_duplicate_copy":true,"reason":"Accept a possible duplicate Sent copy"}`. This acknowledgement can create a duplicate copy, never another delivered email.

Failed or unknown append artifacts remain recoverable only until the original job's 24-hour payload deadline, not 24 hours after each retry. Successful copies release their artifact immediately. Expired artifacts are purged at startup and within the worker's 60-second cleanup interval. Send/idempotency metadata remains for 30 days.

Account reads expose `capabilities.sending`, including the effective MIME-byte limit after applying the configured service limit, supported request encoding and request-byte cap, and whether large-attachment upload sessions are supported. Microsoft reports `large_attachment_upload_supported:true`; Gmail and SMTP report false. Native sends include Bcc headers in the measured MIME. Oversized sends return `413 send_limit_exceeded` with backend, maximum MIME bytes, and actual MIME bytes before a job is queued. Attachment bytes alone cannot determine fit because headers and MIME encoding consume space; mailbox policy may impose a lower limit. See [provider limits](providers.md#sending-limits).

### Microsoft draft and attachment preparation

Graph sends create a service-owned draft, attach each file separately, and then submit that draft. Files below 3,000,000 decoded bytes use the attachment endpoint; larger files use resumable upload sessions with sequential chunks up to 3,276,800 bytes. Inline disposition, Content-ID, reply headers, and Bcc are preserved. Do not edit Rustbox-owned drafts in Outlook while jobs are preparing; changes to message content or attachment inventory block submission.

`GET /v1/sends/{id}` includes `graph_phase`: `preparing`, `ready`, `submitting`, `accepted`, `unknown`, or `abandoned` for Graph jobs, null for other backends. `provider_id` records the immutable draft/message ID. Upload URLs and progress secrets remain encrypted and are never returned. The persisted submission marker and `submitted_at` are written immediately before the final send action. Interrupted preparation resumes; interrupted final submission becomes unknown and never automatically resends.

When Graph changes the RFC Message-ID, Rustbox retains the generated ID and records provider correlation internally. Sent reconciliation first checks the immutable draft/message ID in the configured Sent folder and still requires matching sender, recipients, and message-content evidence. Otherwise it searches the known provider Message-ID. A draft alone is not Sent evidence.

Payloads and upload secrets remain bounded by the original 24-hour job deadline. Terminal failures and expiry attempt conditional cleanup of known unsent drafts. Unknown/submitting messages are never deleted by that cleanup. Provider permission failures may leave a draft requiring operator cleanup; its immutable ID remains available. Job phase metadata is retained with the send record after upload secrets expire.

Microsoft supports a 200 MiB Rustbox total-MIME ceiling, a 150,000,000-byte decoded file ceiling, and a 3,000,000-byte base-draft ceiling excluding attachments; the effective total is reduced by `mail.max_message_bytes` (25 MiB by default). Draft/body and decoded-file violations return `413 send_component_limit_exceeded` with `component`, `maximum_bytes`, and `actual_bytes`. These limits describe the implemented flow; Exchange mailbox policy can be lower. See [provider limits](providers.md#sending-limits).
