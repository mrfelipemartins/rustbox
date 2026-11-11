# Getting started

[Documentation](README.md) · [Rustbox](../README.md)

Install Rust and MySQL 8.0+ or use Docker Compose. Configure MySQL with `max_allowed_packet=64M`. Generate secrets with `openssl rand -base64 32` for `ENCRYPTION_KEY` and `openssl rand -hex 32` for each app key.

Copy [`config.example.yaml`](../config.example.yaml) to `config.yaml` and [`.env.example`](../.env.example) to `.env`. Fill in `DATABASE_URL`, `ENCRYPTION_KEY`, and `MAIN_APP_API_KEY`. The binary automatically loads `.env` beside the selected YAML file; run `cargo run --release` or `cargo run --release -- --config /path/to/config.yaml`. For Docker Compose, also set `MYSQL_PASSWORD` and `MYSQL_ROOT_PASSWORD` in `.env`. Use URL-safe database passwords for the generated connection URL.

Migrations run at startup. Keep the encryption key stable and backed up separately from MySQL; changing it makes stored credentials and queued mail unreadable. Run only one Rustbox instance for a database. A systemd unit is available in `deploy/rustbox.service`.

The default bind address is `127.0.0.1:8080`; the example YAML uses `0.0.0.0:8080` for containers, whose published port is restricted to localhost. Authenticate with `Authorization: Bearer <app-key>`. Keys identify callers; all apps can access all accounts. Place the API behind your internal TLS proxy if exposing it beyond localhost. `/health/live`, `/health/ready`, and `/openapi.json` are public; `/metrics` requires authentication.

## Start with Docker Compose

Run these commands from the repository root:

```sh
cp config.example.yaml config.yaml
cp .env.example .env
openssl rand -base64 32
openssl rand -hex 32
```

Use the generated values for `ENCRYPTION_KEY` and `MAIN_APP_API_KEY`. Add `MYSQL_PASSWORD` and `MYSQL_ROOT_PASSWORD` to `.env` with separate strong, URL-safe passwords. Then start the service:

```sh
docker compose up --build -d
curl --fail http://127.0.0.1:8080/health/ready
```

Next, [connect an account](api.md#connect-an-account). See [configuration](configuration.md) for available settings. Native builds require Rust 1.88 or newer. Run all documented commands from the repository root.

