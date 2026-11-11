# Native providers and reliable integration

Rustbox supports `imap`, `microsoft` (Microsoft Graph), and `google` (Gmail API) accounts. Native Microsoft and Google accounts use their HTTPS APIs for receiving, mutations, and sending. Their permissions and organizational policies still apply.

## Accounts and authorization

Create accounts using an `Idempotency-Key` header. Identical requests from the same API application return the original account; changing the payload with the same key returns `409`. A deleted account's provisioning key remains a tombstone for 30 days.

```json
{
  "name": "Support mailbox",
  "backend": "microsoft",
  "credential_id": "credential-uuid",
  "target_mailbox": "support@example.com",
  "authorizing_identity": "delegate@example.com",
  "shared": true,
  "sync_from": "2026-01-01T00:00:00Z",
  "watched_folders": ["inbox"],
  "sent_folder": "sentitems"
}
```

`authorizing_identity` describes the principal supplying access; `target_mailbox` is the actual mailbox. OAuth success does not establish mailbox permission. Shared mailbox read access, Send As/Send on Behalf, and Sent-copy placement are separate permissions and behaviors.

Configure OAuth registrations in the unified YAML. Google uses `https://www.googleapis.com/auth/gmail.modify`. Microsoft delegated registrations use Graph `Mail.ReadWrite` and `Mail.Send`; set `shared: true` on the registration to request `Mail.ReadWrite.Shared` and `Mail.Send.Shared`. Application registrations request `https://graph.microsoft.com/.default` and require administrator grants for Graph application permissions. Exchange IMAP/SMTP tokens and Graph tokens are different resources.

Google supports owner OAuth and Workspace domain-wide delegation with the credential subject matching the target mailbox. Gmail browser delegation does not establish API authorization. The service verifies the authenticated profile mailbox instead of assuming an arbitrary target address is accessible.

## Synchronization and notifications

Set `native.public_url` to a public HTTPS URL. Graph notification and lifecycle callbacks use `/notifications/microsoft` and `/notifications/microsoft/lifecycle`. Gmail uses `/notifications/google`; configure `native.gmail_topic`, `native.pubsub_audience`, and `native.pubsub_email` for the externally provisioned Pub/Sub topic and authenticated push service account.

Graph own-mailbox and application access use immutable IDs and per-folder delta. Delegated shared mailboxes use listing polls and full reconciliation because shared delegated permissions cannot subscribe to notifications. Gmail uses mailbox history and watches. Push notifications wake durable synchronization; they are not authoritative message records. Subscription failures fall back to polling.

Default scheduling is 30 seconds without push, five-minute safety polling with push, and ten-minute full reconciliation. Throttling and folder count can increase lag; honor provider quotas rather than treating the interval as an unconditional delivery guarantee.

`POST /v1/accounts/{id}/sync` returns `202` with a synchronization-run ID and coalesces overlapping work. Read the run with `GET /v1/syncs/{id}`.

`sync_from` is an inclusive cutoff persisted at account creation. Microsoft Inbox and ordinary folders use `receivedDateTime`; Sent Items uses `sentDateTime`, including configured or resolved opaque Sent folder IDs. Metadata preserves actual `received_at` and `sent_at`, and exposes `eligibility_at` with `eligibility_time_basis`. Gmail and generic IMAP use received/internal time. Omitting it resolves the configured/default 30-day initial window once. Cursor resets rebuild the persisted window rather than moving the cutoff forward. `message.created` carries `initial_sync`, `live`, or `recovery` discovery context. Messages deleted before observation cannot be recovered.

## Message identity and content

List messages at `/v1/accounts/{id}/messages` and inspect one at `/v1/accounts/{id}/messages/{message_id}`. Rustbox IDs identify message records, while locations contain provider/folder/UID information.

Graph immutable IDs survive moves within the same mailbox, with exceptions for archive or cross-mailbox operations. Gmail message IDs survive label changes; labels are memberships rather than duplicate messages. IMAP preserves identity for confirmed moves with reliable destination mappings; external moves or UIDVALIDITY resets can remain ambiguous. RFC Message-ID alone never establishes identity.

Retrieve parsed content at `/v1/accounts/{id}/messages/{message_id}/content`, raw MIME with `?raw=true`, and an attachment at `/v1/accounts/{id}/messages/{message_id}/attachments/{index}`. Parsed metadata includes linkage, structured addresses, dates, classification headers, and attachment MIME properties. Classification headers are untrusted reported evidence.

`message.created` reports metadata with content pending. `message.content_ready` promises cached retrieval until `available_until`. The encrypted cache defaults to seven days, 25 MiB per message, and 10 GiB total. Capacity exhaustion delays new readiness events and never evicts an existing promise early. Missing and oversized content have explicit states; after cache expiry, unavailable provider content returns `410 content_unavailable`.

Mutations use `/v1/accounts/{id}/flags`, `/move`, `/trash`, and `/labels`, with a Rustbox `message_id` in the body. Gmail move requires a source label and destination label; unrelated memberships are preserved. Explicit label modification accepts `add` and `remove` arrays.

## Sending limits

Account reads expose `capabilities.sending`. These are Rustbox's supported send-path limits; lower mailbox policies, recipient policies, and quotas still apply.

| Backend | Total MIME ceiling before outer transport encoding | Per-request ceiling | Implemented path |
|---|---:|---:|---|
| Microsoft | 209,715,200 bytes (200 MiB), further limited by configuration | 4,194,304 bytes per Graph control request | MIME draft, individual attachments, upload sessions, then draft send |
| Google | 26,214,400 bytes (25 MiB) | 35,000,000 bytes | Base64url MIME inside JSON sent to Gmail `messages.send` |
| IMAP/SMTP | Configured `mail.max_message_bytes` | Not applicable | SMTP submission; provider maximum is unknown |

`max_mime_bytes` is the lower of `mail.max_message_bytes` and the implemented native-path ceiling. The service default remains 25 MiB, so Microsoft accounts support attachments larger than 3 MB without changing configuration; increase the configured total limit when larger complete messages are needed. Total MIME size includes headers, bodies, boundaries, attachment transfer encoding, and native Bcc headers.

Microsoft separates the message into a base draft and decoded attachments. The draft preserves RFC Message-ID, reply linkage, addresses, Bcc, and text/HTML bodies. Its `max_draft_mime_bytes` is 3,000,000 bytes. Each decoded file is limited to `max_attachment_bytes:150000000`. Files smaller than 3,000,000 bytes use a single attachment request; larger files use upload sessions with sequential chunks of at most `max_upload_chunk_bytes:3276800`. Inline disposition, Content-ID, MIME type, filename, and decoded bytes are retained. `request_encoding` is `graph_draft_and_attachment_upload`, `encoded_request_limit_scope` is `per_control_request`, and `large_attachment_upload_supported` is true.

The Microsoft attachment ceiling and the total MIME ceiling are different limits. A 150 MB attachment consumes substantially more total MIME space after transfer encoding and requires a sufficiently high `mail.max_message_bytes`. The 200 MiB total is a Rustbox implementation ceiling, not a claim that an Exchange mailbox accepts messages of that size. By default, Exchange may impose a much lower message limit. Microsoft also documents a known issue with large-attachment uploads in shared/delegated mailboxes; validate the actual grant and tenant before relying on that scenario. See [Microsoft's attachment upload guide](https://learn.microsoft.com/en-us/graph/outlook-large-attachments) and [upload-session API](https://learn.microsoft.com/en-us/graph/api/attachment-createuploadsession).

For Gmail, `encoded_request_limit_scope` is `entire_send_request`, and thread IDs are bounded to 256 bytes so the complete JSON request stays within its supported request cap. Gmail media/resumable uploads remain unsupported. Gmail/SMTP `max_attachment_bytes` is null because available attachment space depends on the complete generated MIME. SMTP does not advertise a provider maximum.

Total-MIME overflow returns `413 send_limit_exceeded` with the backend, effective supported MIME limit, and actual MIME size before accepting the send job. Draft and individual-attachment overflow return `413 send_component_limit_exceeded` with the component (`draft_mime` or `decoded_attachment`), its maximum bytes, and actual bytes. These checks account for the exact native MIME including Bcc and are repeated before provider operations.

Large messages increase the HTTP request body, parsing memory, and encrypted database blob size. Raise MySQL `max_allowed_packet` and provision storage for `mail.max_message_bytes`; for example, a 200 MiB configured MIME ceiling needs a database packet limit above the encrypted payload size. The Compose example supplies a 256 MiB packet limit to accommodate the 200 MiB MIME ceiling. Configure the public reverse proxy to accept the structured base64 request size as well.

The Gmail encoding contract follows [Gmail's sending API](https://developers.google.com/workspace/gmail/api/guides/sending). Microsoft draft creation uses [MIME draft creation](https://learn.microsoft.com/en-us/graph/api/user-post-messages), which preserves ordinary mail headers while attachments are added separately.

## Sending and reconciliation

Structured sends support `in_reply_to`, `references`, `reply_to`, and `reply_to_message_id`. Inline attachments use `disposition: "inline"` and `content_id` without surrounding angle brackets. Rustbox builds the MIME structure and validates linkage and header values.

A send job can remain `unknown` after ambiguous submission. `POST /v1/sends/{id}/reconcile` searches accessible Sent locations using the exact generated RFC Message-ID plus corroborating evidence. Results are `matched`, `ambiguous`, `not_found`, or `unavailable`. A Sent copy is evidence, not proof of recipient delivery. Missing Sent content never establishes safe retry.

`POST /v1/sends/{id}/resolve` accepts `decision` (`sent` or `unresolved`), a nonempty `reason`, and `evidence`. A sent resolution requires a stored matching reconciliation result. Resolution never submits mail. An intentional retry is a new send with a new idempotency key and `retry_of_send_id` referencing the original job.

## Provider acceptance

Live acceptance requires operator credentials and tenant setup. Verify Microsoft owner OAuth, shared mailbox delegation with a distinct authorizing user, Send As/Send on Behalf, Sent-copy location, renewal, and permission rejection; Microsoft application notifications; Gmail owner OAuth, Workspace impersonation, Pub/Sub, and history reset; and generic IMAP/SMTP regression behavior. Record actual permissions, policies, lag, throttling, and cache usage. Local provider peers validate deterministic protocol behavior without claiming a live-provider capacity result.

Graph initial delta synchronization deliberately omits a received-date filter: Microsoft limits filtered delta collections to 5,000 messages. Rustbox applies its persisted cutoff locally while following the full initial delta chain, ordering recent messages first. Large folders can therefore require scanning metadata older than the indexed window. Partial update records are resolved through a full message read. See [Microsoft delta guidance](https://learn.microsoft.com/en-us/graph/delta-query-messages).

Graph send preparation is durable: the job saves an immutable draft ID and encrypted upload sessions before proceeding. Chunk acknowledgement loss is reconciled through the session's authoritative offset or attachment evidence. An uncertain last chunk is never blindly uploaded into a new session. Attachment inventory and service-owned draft content are verified before the final send. Only the final submission marker changes crash recovery from resumable preparation to unknown delivery outcome. `graph_phase` exposes this distinction without exposing upload secrets.
