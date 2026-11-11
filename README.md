# Rustbox

A self-hosted mail service with Microsoft Graph, Gmail API, generic IMAP/SMTP, and signed webhooks, built with Rust.

Connect personal or authorized shared mailboxes, synchronize messages, retrieve cached content, update mailboxes, and queue outgoing mail through one service.

- Microsoft Graph and Gmail API synchronization with push notifications and polling.
- Generic IMAP IDLE monitoring with metadata synchronization.
- Stable message references, normalized metadata, bounded encrypted content caching, and mailbox mutations.
- Durable outgoing mail with reply linkage, inline images, idempotency keys, and uncertain-send reconciliation.
- Signed webhooks and a replayable event feed.
- Verified TLS and encrypted stored credentials and outgoing payloads.
- OAuth consent helpers, refresh-token import, and automatic token renewal.

Rustbox runs as one process with MySQL; no Redis or object storage is required. It is designed for trusted backend applications: all API keys share access to all accounts. Google and Microsoft accounts use their native APIs; generic IMAP/SMTP accounts support password authentication. OAuth helpers support owner access, authorized shared Microsoft mailboxes, and Workspace impersonation. Multi-instance high availability is not supported.

## Get started

Follow the [setup guide](docs/getting-started.md) to run with Docker Compose or Rust and MySQL, then [connect your first account](docs/api.md#connect-an-account).

## Documentation

- [Configuration](docs/configuration.md) and [deployment](docs/deployment.md)
- [OAuth onboarding and provider setup](docs/oauth.md)
- [Native providers and reliable integration](docs/providers.md)
- [API guide](docs/api.md) and [OpenAPI specification](openapi.json)
- [Events and webhooks](docs/webhooks.md)
- [Architecture and limitations](docs/architecture.md)
- [Testing](docs/testing.md) and [benchmark results](docs/validation.md)

See the [documentation index](docs/README.md) for all guides. Contributions are welcome; start with [CONTRIBUTING.md](CONTRIBUTING.md). For vulnerabilities, see [SECURITY.md](SECURITY.md).

## License

[MIT](LICENSE).
