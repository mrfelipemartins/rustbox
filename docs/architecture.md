# Architecture and limits

[Documentation](README.md) · [Rustbox](../README.md)

Rustbox runs as one Tokio process backed by MySQL. Run one instance per database. Configuration lives in one YAML file with optional environment substitution and an adjacent `.env`.

Three concrete backends share message identities, MIME parsing, content caching, events, and durable sending: Microsoft Graph, Gmail API, and generic IMAP/SMTP. Native accounts separate the authorization grant from the target mailbox. Delegated shared Microsoft mailboxes poll; supported own-mailbox and application grants can receive public HTTPS notifications. Gmail uses Pub/Sub and history cursors. Notifications persist wake-ups and trigger authoritative synchronization; safety polling handles missed hints. Provider requests have bounded concurrency and throttling cooldowns.

The default synchronization cutoff is resolved once at account creation to 30 days earlier. An explicit `sync_from` is inclusive and immutable. Initialization and cursor recovery emit eligible new discoveries with their discovery reason. Only messages still available to the provider can be recovered. IMAP INBOX uses IDLE, with optional folders and periodic metadata reconciliation. UIDVALIDITY resets can rediscover messages when stable provider identity is unavailable.

Rustbox message IDs separate identity from current locations. Graph immutable IDs survive moves within a mailbox; Gmail labels are memberships of one message. IMAP identity survives confirmed COPYUID moves, but externally performed moves and UID resets can remain unresolved. IMAP OBJECTID extensions are not currently used. RFC Message-ID is never a sufficient identity key.

Metadata discovery emits `message.created`. A separate worker encrypts complete MIME in a bounded MySQL cache and emits `message.content_ready` only after committing the promised retention deadline. The default cache retains seven days, admits messages up to 25 MiB, and budgets 10 GiB of raw MIME. Capacity pressure defers readiness; it does not evict promised content. After expiration, content availability depends on the provider. This is a bounded cache rather than a full-history archive.

IMAP commands are serialized through account tasks. Native synchronization is serialized per mailbox, while independent API operations use bounded provider requests. Database checkpoints and events commit together. An event counter lock prevents cursor readers skipping late commits. Network operations hold no database transaction.

Sending is asynchronous and idempotent. Ambiguous submission outcomes never automatically retry. Sent-folder reconciliation records evidence independently from submission status; explicit resolution never sends another message. Account provisioning is also idempotent and retains deletion tombstones for 30 days.

OAuth, native-provider push, and a public callback URL require provider-side configuration. Live tenant consent, permissions, quotas, and policies remain installation concerns. HA, Redis, object storage, body search, folder creation/rename/deletion, copy, permanent deletion, public raw-MIME sending, and per-recipient delivery tracking remain excluded.

| Module | Responsibility |
|---|---|
| `api`, `notifications` | Application API and authenticated provider callbacks |
| `accounts`, `imap` | IMAP task ownership and synchronization |
| `native`, `providers` | Native API scheduling and provider operations |
| `mail`, `content` | Normalized MIME and encrypted bounded caching |
| `smtp` | Durable sends, Sent evidence, and resolution |
| `oauth` | Provider registrations, consent, credentials, and renewal |
| `events`, `db` | Transactional persistence, cursor reads, and webhooks |
| `config`, `crypto`, `security` | Typed YAML, encryption, and verified destinations |
