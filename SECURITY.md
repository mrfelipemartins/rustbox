# Security policy

## Reporting vulnerabilities

Do not disclose exploitable vulnerabilities, credentials, or private mail content in public issues or pull requests.

Use the repository's **Security → Advisories → Report a vulnerability** option when private vulnerability reporting is enabled. If that option is unavailable, open an issue asking the maintainer to enable a private reporting channel, without including vulnerability details.

Include the affected revision, a minimal reproduction using synthetic data, expected impact, and any suggested mitigation. No response-time commitment or supported-version matrix is currently established.

## Deployment boundaries

Rustbox is intended for trusted backend callers. Every configured app key can access every account. Deploy one instance per database, protect API access with TLS, and preserve the encryption key alongside secure database backups.

Keep `ALLOW_PRIVATE_NETWORKS` disabled outside isolated tests. Treat email HTML as untrusted and sanitize it before rendering. See [deployment guidance](docs/deployment.md) and [API behavior](docs/api.md).
