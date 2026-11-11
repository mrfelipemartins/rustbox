# Development and testing

[Documentation](README.md) · [Rustbox](../README.md)

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
TEST_DATABASE_URL=mysql://root@127.0.0.1:3307/rustbox_test cargo test --all-targets -- --ignored --test-threads=1
```

Use a disposable database for integration tests. Tests exercise IDLE interrupt/DONE/session reuse, disconnect handling, recipient rejection before DATA, encryption authentication, destination filtering, API authentication, lifecycle deduplication, restart recovery, atomic rollback, event cursor ordering, idempotency conflicts, and interrupted send recovery.

Run the Python fixture tests as well:

```sh
python3 -m unittest discover -s tests -p 'test_*.py'
```

The GitHub workflow runs formatting, Clippy, Rust tests, database integration tests, and Python helper tests. Long-running provider and capacity checks are manual; see [benchmark procedures](benchmarks.md). Historical results are in the [validation record](validation.md).

Configuration tests cover YAML parsing, typed environment substitution, adjacent `.env` loading, precedence, relative file paths, duplicate and unknown fields, provider option validation, and secret redaction. The soak stack mounts `tests/soak/config.yaml`; its private-network and custom-CA settings are explicit in that file. Harness-only variables such as `SOAK_SECONDS` and `TEST_DATABASE_URL` continue to configure the test tools rather than the service.

OAuth tests use local token and mail peers, including 100 simultaneous token requests, refresh rotation, revoked grants, stale refresh results, application-token sharing, authorization-session recovery, and Google/Microsoft assertion signatures. The IMAP simulator accepts XOAUTH2 test tokens `simulator:<epoch>`, where epoch is the current Unix time divided by `SIMULATOR_OAUTH_TOKEN_SECONDS` (default 60). Its fixture test expires tokens for 100 clients together and reconnects them with the next epoch. These fixtures measure correctness and renewal coalescing, not production provider capacity. Provider endpoint overrides exist only in Rust test builds.

Native-provider acceptance includes a 1,000-mailbox simulated Graph scheduler test in the ignored MySQL suite. Run database tests serially because cache-budget fixtures share the dedicated database. The public-HTTPS and provider configuration requirements and live acceptance matrix are in [the provider guide](providers.md). IMAP protocol tests cover IDLE reuse, inclusive cutoff recovery, COPYUID identity mapping, and safe expunge. The soak harness uses Rustbox message IDs and account provisioning idempotency keys.

Graph draft/upload tests use a local HTTP peer and MySQL. They cover 4 MB files plus inline images, header/Bcc preservation, sequential chunks, lost draft/chunk acknowledgements, expired-session reconciliation, external draft edits, and ambiguous final submission. Only the final submission phase is unretryable after a crash; upload preparation resumes from encrypted progress. Live Microsoft owner/application/shared grants and tenant attachment policies still require operator acceptance.
