# Deployment

[Documentation](README.md) · [Setup](getting-started.md)

Run one Rustbox instance per database with MySQL 8.0 or newer and `max_allowed_packet=256M`. Migrations run automatically at startup.

## Docker Compose

The root [Compose file](../compose.yaml) includes MySQL persistence, a database health check, and a localhost-only published API port. Start it with `docker compose up --build -d` after configuring `config.yaml` and `.env`. Inspect logs with `docker compose logs --tail=100 rustbox`.

Use `docker compose stop` to stop the stack. Do not remove database volumes unless intentionally deleting all stored data.

## Native service

Build with `cargo build --release --locked`. The [systemd unit](../deploy/rustbox.service) expects a dedicated `rustbox` user and group, the binary at `/usr/local/bin/rustbox`, and `/etc/rustbox/config.yaml` with an optional adjacent `/etc/rustbox/.env`. Provision these before installing and enabling the unit. Make configuration and secret files readable by the service user, with restrictive permissions. Relative signing-key and CA paths resolve against `/etc/rustbox`.

## Network and health

Expose the API through a TLS reverse proxy when accessing it beyond localhost. `/health/live`, `/health/ready`, and `/openapi.json` are public. `/metrics` and application endpoints require bearer authentication. Restrict access to trusted backend callers.

IMAP, SMTP, and webhook destinations are validated; keep private-network overrides disabled in production. See [configuration](configuration.md) for custom CAs and limits.

## Backups and upgrades

Back up MySQL and retain the matching encryption key separately. Test restoring both together. Incoming message bodies remain at the mail provider; Rustbox is not a full mail archive.

Before upgrading, back up the database and review migration changes. Automatic migrations may mean restoring a matching database backup is necessary for rollback. Stop the old instance before starting its replacement.

Monitor connection state, send outcomes, webhook backlog, and resource usage. An ambiguous send outcome requires reconciliation before resubmitting; see the [API guide](api.md).

Graph large attachments use drafts and resumable uploads. The service keeps its 25 MiB total-MIME default; increase `mail.max_message_bytes` up to the advertised 200 MiB implementation ceiling when needed. The shipped Compose MySQL packet limit is 256 MiB. Also raise reverse-proxy request limits for the structured base64 request (Rustbox accepts HTTP bodies up to twice the configured MIME limit), and size send-worker concurrency for the memory used by large messages. A larger upload capability does not raise the Exchange mailbox's own limit.
