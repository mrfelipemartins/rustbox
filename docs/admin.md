# Admin panel

[Documentation](README.md) · [Rustbox](../README.md)

Open `/admin` on the Rustbox server and sign in with any configured application API key. All keys have the same access. The key is exchanged for a server-side session; it is not stored in browser storage. Sessions last eight hours by default and end on logout or server restart.

Use HTTPS outside loopback development. Terminate TLS at a trusted reverse proxy, preserve the public Host and Origin, and set `X-Forwarded-Proto: https` there. Do not expose the upstream HTTP port directly to untrusted clients. The session cookie is HttpOnly, SameSite, and scoped to `/admin`; state-changing requests also require the session's CSRF token. Existing `/v1` integrations continue using bearer authentication.

The panel is enabled by default. To remove both its frontend and its session-authenticated APIs, set this and restart:

```yaml
admin:
  enabled: false
```

## Pages

| Page | Purpose |
|---|---|
| Overview | Current health, queue depths, cache usage, freshness, problems requiring attention, and recent events. |
| Mailboxes | Connect Google, Microsoft, or IMAP/SMTP; inspect health, folders and content; edit, pause/resume, synchronize, delete, or send a test email. |
| Credentials | Connect/import/reauthorize OAuth credentials and inspect linked accounts and configured registrations. |
| Outgoing mail | Inspect submission and Sent-copy outcomes, reconcile uncertain jobs, record resolutions, and recover copies under existing backend rules. |
| Events & Webhooks | Inspect retained business events and outgoing delivery summaries, including attempts, status, and retry timing. |
| Logs | Search persistent sanitized operational records and follow new records. |
| System | Read effective non-secret configuration, worker limits, providers, application names, and runtime information. |

Provider acceptance does not establish recipient delivery. An unknown send is never automatically resent. A Sent-copy retry affects only the mailbox copy. Deleting an account removes Rustbox data and stops monitoring; it does not delete provider mail, and a send already in flight may complete.

Messages show indexed metadata and available cached content. Expired or missing content is reported explicitly. HTML is sanitized and sandboxed with remote resources disabled. This is an administration interface rather than a general mail composer.

## OAuth callback

For delegated authorization, register the exact public URL `https://your-rustbox-host/admin/oauth/callback` with the provider and in the registration's `redirect_uris` configuration. Registrations are edited in YAML and require restart. Select a configured registration in Credentials to start consent. Shared Microsoft mailboxes and Google Workspace impersonation still require the provider permissions described in [OAuth setup](oauth.md).

## Operational logs

Logs are stored in MySQL and remain available after restart. Defaults retain seven days or 1 GiB of record payload, whichever is reached first, with individual records limited to 16 KiB. Database pages and indexes consume additional space. Retention is enforced asynchronously; short-lived overshoot is possible while a batch is written and pruned.

The asynchronous bounded sink keeps stdout logging active and never waits for the database in mail-processing paths. Dropped records are counted, including when its queue is full or persistence fails. The panel displays these counters and retention metadata. Events remain the durable business history; operational logs are best-effort diagnostic records. Secrets, message bodies, and attachments are excluded from persisted records.

## Frontend development and release builds

Install Node.js 24 and Rust 1.88 or newer. From the repository root, build the frontend before compiling Rust, because its production output is embedded in the binary:

```sh
npm ci --prefix admin
npm run build --prefix admin
cargo build --release --locked
```

Docker builds perform these steps automatically, and the runtime image does not include Node.js. Rebuild Rust after changing production frontend assets.

For frontend development, run Rustbox locally and start Vite:

```sh
cargo run
npm run dev --prefix admin
```

The development server proxies admin API requests to the local Rustbox process. Open the Vite URL under `/admin`. Keep development listeners on loopback. UI preferences such as theme may be saved locally; credentials are not.

```sh
npm test --prefix admin
npm run build --prefix admin
cargo test --locked --all-targets
```

Admin reads use `/admin/api`; existing operations are available to authenticated sessions through `/admin/api/v1/*`. List responses use `items`, `total`, `page`, and `per_page`. Requests that mutate data require `X-CSRF-Token`; account creation and test sends retain the existing `Idempotency-Key` requirement. The [OpenAPI specification](../openapi.json) documents these interfaces.
