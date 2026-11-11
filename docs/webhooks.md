# Events and webhooks

[Documentation](README.md) · [Rustbox](../README.md)

Events include `account.created`, `account.updated`, `account.connecting`, `account.connected`, `account.disconnected`, `account.connection_failed`, `account.auth_failed`, `account.reauthorization_required`, `account.authorization_failed`, `account.authorization_restored`, `account.paused`, `account.resumed`, `account.deleted`, `account.smtp_validated`, `account.smtp_validation_failed`, `account.ready`, `account.sending_validated`, `account.sending_failed`, `account.sync_completed`, `account.sync_reset`, `message.created`, `message.content_ready`, `message.content_unavailable`, `message.locations_changed`, `message.flags_changed`, `message.removed`, `send.accepted`, `send.failed`, `send.unknown`, and `send.resolved`.

`account.connected` includes `reconnected`; SMTP validation and sync completion remain separate. Identical connection failures are suppressed within a recovery cycle. Authentication rejection stops retries until credentials change or the account is resumed. IDLE renewals do not generate disconnect events. Graceful shutdown persists disconnection; crash recovery records the time the stale connection state was discovered.

Webhook JSON has `id`, `schema_version`, `account_id`, `type`, `observed_at`, and `data`. The headers are:

```text
X-Rustbox-Event-Id: 123
X-Rustbox-Signature: t=<unix-seconds>,v1=<hex-digest>
```

Compute HMAC-SHA256 with the webhook secret over the timestamp, a literal period, and the exact request body bytes. Compare signatures in constant time, enforce a timestamp tolerance, and deduplicate event IDs. Retries carry a new signature timestamp. Return any 2xx to acknowledge. Delivery is at least once, unordered, with a ten-second request timeout and approximately 24 hours of backoff.

`GET /v1/events?after=123&limit=100` returns events in commit order plus `next_cursor`. Events remain seven days, including after webhook retries stop. An expired cursor returns HTTP 410 with `minimum_cursor` and `current_cursor`; capture the current cursor, resynchronize metadata, then consume events after that cursor. Retained deletion events retain the webhook destination and encrypted signing secret independently of the account row.

## Native accounts and content readiness

Native Graph/Gmail accounts emit `account.ready` after successful synchronization. Authorization, subscription status, and sending health remain independent. `account.sending_validated` follows observed submission acceptance; read access alone does not validate sending permissions. Generic IMAP accounts retain connection/disconnection events.

`message.created` announces metadata discovery with a Rustbox message ID and discovery reason (`initial_sync`, `live`, or `recovery`). It does not promise that a body has already been cached. `message.content_ready` includes `message_id` and `available_until`; consumers requiring reliable content retrieval should wait for this event and fetch before its deadline. Existing readiness promises are not evicted early to make space. `message.content_unavailable` reports a sanitized reason such as provider disappearance, message size, or expired fetch retries. Rustbox cannot archive content removed before it was fetched.

`message.locations_changed` identifies the same stable message at changed provider locations. For IMAP, identity confidence can remain limited after external moves and UIDVALIDITY resets. `send.resolved` records an operator resolution separately from the original submission result; it does not imply resubmission or recipient delivery.

Provider push callbacks are separate from outgoing application webhooks. Graph notifications validate subscription/client state, and Gmail Pub/Sub push validates the signed service-account identity. Rustbox durably schedules reconciliation before acknowledging valid hints. Provider notifications can be duplicated, delayed, or missed; subscription renewal and polling recover available changes.

Sent-copy recovery preserves SMTP acceptance. `send.append_failed` reports the separate copy outcome and recovery deadline, `send.append_reconciled` reports observed evidence, and `send.append_retry_requested` records a deliberate copy-only retry. These events never indicate a second SMTP submission.
