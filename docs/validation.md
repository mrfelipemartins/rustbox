# Validation record

[Documentation](README.md) · [Test procedures](testing.md)

This is a historical snapshot recorded on 2026-11-11, not a live run status. Artifact paths below are relative to the repository root; generated reports are not shipped with the repository.

Validation performed on 2026-11-11 with Rust 1.99.1, MySQL 8.0.46 for native integration tests, and MySQL 8.4 / Dovecot 2.3.21.1 in Docker for end-to-end checks. Host: x86_64 Linux, 2 logical CPUs, approximately 31 GiB RAM. Rustbox, Dovecot, MySQL, and the test runner share this host.

## Completed checks

- `cargo fmt --check`: passed.
- `cargo clippy --all-targets -- -D warnings`: passed.
- `cargo test --all-targets`: 20 tests passed; two database-dependent tests are intentionally ignored by default.
- Both ignored tests passed against a disposable live MySQL database. Coverage includes account lifecycle deduplication, atomic rollback, commit-ordered cursors, restart recovery, metadata pagination and exact mailbox identity, stale SMTP validation, send idempotency, and interrupted-send handling.
- Scripted protocol tests passed for IDLE cancellation/DONE/session reuse, connection loss, partial BODY.PEEK reads, targeted move/expunge, SMTP recipient rejection before DATA, and ambiguous post-DATA disconnects.
- Ten-account Dovecot smoke: 60 seconds at one incoming email/second, all 60 emails observed, p95 injection-to-webhook latency 0.591 seconds, no errors or backlog. Two complete content/attachment/flag/move-or-trash checks and two asynchronous send/idempotency/Sent-append checks passed.

- 1,000-account Dovecot smoke on the final source image: 60 seconds at 10 incoming emails/second, 600/600 emails observed, p95 injection-to-webhook latency 0.738 seconds. Twelve complete mailbox-operation checks and twelve send/idempotency/Sent-append checks passed. All 1,000 sessions stayed connected, no errors or reconnects, sampled RSS 80.49–80.82 MB, maximum sampled webhook backlog one.

Generated reports are in `tests/soak/results/`; they are deliberately excluded from version control. Test secrets, certificates, and database/mail volumes are isolated from the production deployment.

## Environment tuning

Docker, Docker Compose, and native build tools were installed on the host. Dovecot's 1,000-session test requires increasing `fs.inotify.max_user_instances` from the original 128 to 4096. The full-run wrapper restores the original value after stopping its containers. The test Compose file raises file-descriptor limits; Dovecot service client limits are sized for the workload.

A short test cannot establish 24-hour memory stability. The full soak must produce a passing `report.json` before treating that acceptance criterion as verified.

## Full soak interrupted for capacity testing

The 24-hour run was started after the successful 1,000-account smoke, using a fresh synthetic database: 1,000 accounts and ten incoming messages per second. Startup connected all 1,000 accounts. The background wrapper PID is recorded in `tests/soak/results/soak-24h.pid`; progress is in `soak-24h.log` and `metrics.jsonl`. Completion writes `report.json` and `soak-24h.exit`. The wrapper stops the test containers and restores the original inotify-instance limit afterward.

The user subsequently requested a capacity ramp. The 24-hour run was intentionally stopped after roughly 51 minutes; its latest samples are preserved in `interrupted-24h-summary.json` and `interrupted-24h-metrics.jsonl`. It did not complete, so its 24-hour stability criterion remains unverified.

## Guarded capacity ramp results

Completed on 2026-11-11 on the same two-CPU, approximately 31-GiB host. Application settings were unchanged. Each stage received ten incoming emails/second for 60 seconds after onboarding and lifecycle-webhook delivery settled, with content, attachment, flags, move/trash, send, idempotency, and Sent-append checks.

| Connected accounts | Messages received | p95 webhook latency | Peak Rustbox RSS | Result |
|---:|---:|---:|---:|:---|
| 1,000 | 600/600 | 0.758 s | 77.2 MiB | Passed |
| 2,000 | 600/600 | 0.748 s | 137.6 MiB | Passed |
| 4,000 | 600/600 | 0.759 s | 261.7 MiB | Passed |
| 8,000 | 600/600 | 0.759 s | 509.5 MiB | Passed |
| 12,000 | 600/600 | 0.744 s | 753.3 MiB | Passed |
| 16,000 | 600/600 | 0.730 s | 1009.1 MiB | Passed |

The highest fully validated stage was **16,000 simultaneous accounts**. All measured stages passed with no errors. These short windows establish a workload-specific lower bound, not an exact maximum or long-term stability guarantee.

The next stage targeted 20,000 accounts. The guard stopped onboarding after 19,581 successful account-creation responses when Dovecot exceeded 90% of its 24-GiB container limit (about 21.67 GiB). That created-account count is not a verified simultaneous-connection count. Host available RAM was 7.47 GiB at the stop. The stopping condition was fixture memory, so the run does not establish Rustbox's maximum on a larger or distributed environment.

One million accounts were **not tested**. The original 24-hour soak was intentionally interrupted for this ramp; that stability criterion remains unverified. The capacity wrapper exited with code 2 for a guard stop, stopped all test containers, and restored `fs.inotify.max_user_instances=128`. Synthetic database/mail volumes and reports remain available for inspection.

Reports: `tests/soak/capacity-results/capacity-report.json`, individual `stage-*.json`, `host-metrics.jsonl`, `capacity-metrics.jsonl`, and `capacity-events.jsonl`. The harness and guard additionally pass 11 no-network Python tests (`python3 -m unittest discover -s tests -p test_capacity.py`).

## Lightweight TLS simulator run

Replaced Dovecot in a separate simulator test project with one asyncio process. Real TLS, IMAP IDLE/DONE, UID fetch, content/attachment retrieval, mutations, and Sent append remain exercised. Eight IMAP ports spread connection tuples; application concurrency settings remain unchanged. Simulator resources are measured separately, and the simulator deliberately implements only the synthetic workload subset.

The optimized 1,000-account stage passed: 600/600 emails observed, p95 webhook latency 0.381 seconds, 0 errors. The simulator used approximately 62 MiB and one process during this stage, compared with about 1.1 GiB and roughly 1,000 processes for Dovecot. The initial Python-default-buffer experiment is preserved separately; the optimized run uses a 4-KiB TLS read buffer.

At the time of this record, the larger ramp was running in the background. Results and current stage are recorded in `tests/soak/simulator-results/capacity-report.json`; logs and host-resource samples are in the same directory. This record does not establish an upper limit. Previous Dovecot results are preserved. Fourteen Python helper tests pass; Compose and shell configuration checks pass. No additional Rust production dependency was introduced.

## OAuth and unified YAML configuration — 2026-11-11

- Formatting and Clippy with warnings denied passed.
- The normal Rust suite passed 44 tests. All eight database-dependent tests passed against a dedicated MySQL 8.0.46 database, including local OAuth token peers.
- All 15 Python fixture tests passed, including synchronized token expiry and reconnects for 100 simulator clients.
- One hundred concurrent token requests produced one provider request. Tests also covered refresh rotation, cancellation, revocation, stale results, application-token sharing, assertion signatures, authorization-session recovery, durable account resume, and SMTP revision guards.
- A native-binary smoke test passed startup from YAML with an adjacent `.env`, readiness, protected OAuth endpoints, redacted client discovery, PKCE URL creation, application binding, state rejection, session cancellation, OpenAPI serving, and graceful shutdown.
- No real Google or Microsoft account was authorized in this run. Provider consent, tenant policies, administrator grants, and live mailbox acceptance remain opt-in checks requiring operator credentials. The 100-client fixture is a correctness check, not a new capacity benchmark or soak result.

## Native APIs and reliable mail integration — 2026-11-11

- Formatting, locked builds, and Clippy with warnings denied passed.
- Normal Rust checks passed 53 tests. All 16 database-dependent checks passed against the dedicated MySQL test database. Coverage includes provisioning conflicts and tombstones, synchronization cutoff recovery, commit guards, cache promises/capacity, unknown-send resolution, token renewal, and native transport behavior.
- All 15 Python fixture tests passed after updating the soak harness to use stable message IDs and provisioning idempotency keys.
- A simulated Graph test synchronized 1,000 mailbox accounts through the native scheduler: 2,000 HTTP requests, peak concurrent requests 14, bounded by the configured 16. Its library database-test batch completed in 28.24 seconds including fixture setup and cleanup. This is a functional load check with empty simulated mailboxes, not a live-provider throughput benchmark or a soak test.
- A separate test verified durable independent scheduling for 1,000 accounts and duplicate request coalescing.
- The native-binary smoke passed YAML/.env startup, API authentication, account provisioning retries/deletion tombstones, pause behavior, stable-message listing, Graph challenge handling, unauthorized Pub/Sub rejection, metrics/OpenAPI, and graceful shutdown.
- Graph delta regression coverage verifies that initialization avoids the provider's 5,000-message filtered-delta cap and resolves partial message updates. Transport tests retain immutable IDs and never retry ambiguous submission.
- Live Graph own/shared/application authorization, Send As/Send on Behalf, Sent-copy policy, Gmail owner/Workspace authorization, and real provider push acceptance were not executed: no provider credentials or tenant configuration were supplied. These checks remain required before a production migration; see the provider guide.

## Sent eligibility, copy recovery, and sending limits

- Formatting, locked builds, Clippy with warnings denied, 80 Rust tests (including 20 MySQL-dependent checks), and all 15 Python fixtures passed.
- Microsoft regression coverage verifies opposite received/sent timestamps around the cutoff, both shared-list filters, configured and resolved opaque Sent IDs, and native message persistence. The persistence regression also covers reading case-sensitive provider IDs and locations through SQLx.
- IMAP copy tests verify encrypted artifact retention, definite and uncertain outcomes, restart recovery, copy-only retry, evidence completion, expiry, and configuration-target protection while delivery remains accepted.
- Sending-limit tests verify exact Graph/base64 and Gmail/JSON boundaries, lower configured limits, native Bcc overhead, and rejection before queueing.
- A native-binary HTTP smoke passed capability exposure on account creation, structured size-limit rejection, append recovery routes, OpenAPI serving, and graceful shutdown.
- Graph large-attachment upload sessions and Gmail media/resumable upload paths remain unsupported. The advertised limits apply to Rustbox's implemented direct-send paths; provider policy may be lower. No live provider credentials were exercised.

## Microsoft draft and large-attachment uploads

- Formatting, locked builds, and Clippy with warnings denied passed. The combined Rust suites passed 99 tests, including 29 database-dependent checks; all 15 Python fixtures passed.
- Local Graph/MySQL tests sent a 4 MB file plus an inline image through draft creation, attachment upload sessions, sequential chunks, inventory verification, and one final submission. Bcc, reply headers, body encodings, MIME types, and Content-IDs were preserved.
- Recovery tests cover lost draft creation acknowledgements, authoritative offsets after lost chunk acknowledgements, expired completed sessions, uncertain final chunks with no visible attachment, external draft edits, preparation restart, accepted-phase restart, and ambiguous final submission without a second send.
- Upload progress and preauthenticated URLs are encrypted. The final submission marker and timestamp commit before the send action. Upload secrets expire with the 24-hour job deadline while immutable IDs and phase metadata remain with the send record.
- Capability and boundary tests cover the configurable 200 MiB total-MIME ceiling, 3 MB base-draft limit, 150 MB decoded attachment limit, per-request/chunk limits, and lower configured service limits. The default remains 25 MiB total MIME; Exchange policy may be lower.
- These are local protocol and persistence checks, not live Microsoft tenant acceptance. Shared/delegated upload behavior remains subject to Microsoft's documented known issue and requires live validation.
