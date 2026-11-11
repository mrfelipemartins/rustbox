# Configuration

[Documentation](README.md) · [Getting started](getting-started.md)

Copy [`config.example.yaml`](../config.example.yaml) to `config.yaml` and [`.env.example`](../.env.example) to `.env`. Rustbox loads `config.yaml` by default. Select a different file with `rustbox --config /etc/rustbox/config.yaml`.

Rustbox loads an optional `.env` beside that YAML file without changing the process environment. Process environment values take precedence. Only values referenced with `${VARIABLE}` in YAML are used; arbitrary environment variables do not implicitly override settings. `$$` produces a literal dollar sign. Missing references, duplicate keys, unknown fields, invalid types, and incompatible provider settings fail startup without printing secret values. Substitution happens after parsing YAML, so punctuation and newlines inside secrets cannot inject configuration fields. Relative paths are resolved against the YAML directory.

| YAML field | Default | Purpose |
|---|---|---|
| `database.url` | Required | MySQL connection URL |
| `api.keys` | Required | Map of app names (1–128 characters) to distinct keys of at least 32 characters |
| `security.encryption_key` | Required | Base64-encoded 32-byte encryption key |
| `server.listen` | `127.0.0.1:8080` | HTTP bind address; example uses `0.0.0.0:8080` for containers |
| `server.command_timeout_secs` | `60` | Command timeout |
| `server.log_filter` | `rustbox=info` | Structured logging filter |
| `security.allow_private_networks` | `false` | Allow private/reserved destinations for isolated tests |
| `security.tls_ca_file` | Unset | Additional PEM certificate authority |
| `mail.max_message_bytes` | `26214400` | Encoded message size limit |
| `workers.imap_concurrency` | `64` | IMAP operation concurrency |
| `workers.smtp_concurrency` | `8` | SMTP worker concurrency |
| `workers.webhook_concurrency` | `32` | Webhook worker concurrency |
| `webhooks.timeout_secs` | `10` | HTTP delivery timeout |
| `webhooks.retry_window_hours` | `24` | Delivery retry window |
| `webhooks.retention_days` | `7` | Event cursor history retention |
| `oauth.concurrency` | `8` | Concurrent provider token requests |
| `oauth.providers` | Empty | Named Google and Microsoft registrations |

Numeric limits must be positive; webhook retention must cover its retry window. All app keys have access to all accounts and are not tenant boundaries. Configuration changes require restart.

Provider examples for delegated authorization, Google domain-wide delegation, and Microsoft application authorization are included in the example YAML. Uncomment only the registrations you intend to serve. Registrations use client secrets, service-account JSON paths, or Microsoft certificate/private-key paths. Keep registration IDs stable because stored OAuth credentials reference them. See [OAuth](oauth.md) for onboarding and administrator prerequisites.

Production Compose reads `.env`, mounts `config.yaml`, and supplies `DATABASE_URL` using its MySQL container hostname. It also requires `MYSQL_PASSWORD` and `MYSQL_ROOT_PASSWORD`; use URL-safe database passwords. Mounted private-key files must be readable by the Rustbox container user, and their paths must exist inside the container.

Generate an encryption key with `openssl rand -base64 32` and an app key with `openssl rand -hex 32`. Protect `.env`, YAML files containing secrets, and signing keys. Back up the encryption key separately from MySQL; replacing it makes existing encrypted data unreadable.

## Native mail and content caching

Set `native.public_url` to the public HTTPS base URL used for provider callbacks. Terminate TLS at a reverse proxy. Native accounts require this configuration.

`native.poll_secs` defaults to 30, `native.safety_poll_secs` to 300, `native.reconcile_secs` to 600, and `native.concurrency` to 16. Requests are bounded per provider; throttled mailboxes honor Retry-After. These intervals are scheduling targets subject to workload and provider limits.

For Gmail push configure `native.gmail_topic`, `native.pubsub_audience`, and `native.pubsub_email`. The topic's project must match the OAuth application project. Installations with multiple Google OAuth projects can supply `native.gmail_topics`, a mapping from registration IDs to topic names, overriding the default topic. Pub/Sub subscriptions and IAM are provisioned by the operator. Accounts without complete push configuration continue polling.

`cache.retention_days` defaults to 7, `cache.max_message_bytes` to 26214400, `cache.capacity_bytes` to 10737418240, and `cache.concurrency` to 4. Capacity counts raw MIME bytes; encryption adds a small storage overhead. Rustbox does not evict promised content before its readiness deadline. Configure MySQL disk capacity and maximum packet size for the selected message limit.
