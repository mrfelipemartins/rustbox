# Soak and capacity testing

[Documentation](README.md) · [Rustbox](../README.md)

Run commands from the repository root. These tests create synthetic infrastructure and can consume substantial host resources.

## Dovecot soak

The Dovecot soak environment is separate from production Compose:

```sh
sh tests/soak/setup.sh
SOAK_ACCOUNTS=10 SOAK_SECONDS=60 SOAK_RATE=1 docker compose --env-file tests/soak/.env -f tests/soak/compose.yaml up --build --abort-on-container-exit --exit-code-from runner
```

For the full run, use `SOAK_ACCOUNTS=1000 SOAK_SECONDS=86400 SOAK_RATE=10`. The harness provides a private CA, Dovecot, MySQL, an HTTPS webhook receiver, and an SMTP sink. It injects mail, verifies webhook signatures, exercises content/attachments/flags/moves/trash plus asynchronous sends/idempotency/Sent append, and writes results under `tests/soak/results`. Test secrets and generated files are ignored. Never use these test credentials or `ALLOW_PRIVATE_NETWORKS=true` as production defaults.

The target is 1,000 connected accounts with stable memory, no sustained backlog, and p95 injected-mail-to-webhook latency under five seconds against the local receiver. Passing unit tests alone does not establish this capacity; inspect the soak report and its actual duration/hardware.

Between soak runs, preserve the reports and reset only the synthetic stack with `docker compose --env-file tests/soak/.env -f tests/soak/compose.yaml down -v`. This deletes the soak database and mail volumes; the harness intentionally refuses a nonempty database. It does not use production Compose volumes.

For 1,000 Dovecot test sessions, ensure the host `fs.inotify.max_user_instances` limit is at least 4096; Ubuntu defaults can be only 128, causing Dovecot to fall back to slower notifications. Record the previous value, temporarily raise it with `sudo sysctl -w fs.inotify.max_user_instances=4096`, and restore it after stopping the test containers. The Compose files also raise file-descriptor limits.

## Guarded host-capacity ramp

`sudo scripts/run_capacity.sh` runs a separate `capacity` Compose project using the same verified-TLS fixture. Stop the ordinary soak first to avoid competing workloads and the shared localhost port. The script requires Docker, Python, and the generated soak certificates/secrets. It preserves the ordinary soak database and reports.

The default stages are 1,000, 2,000, 4,000, 8,000, 12,000, 16,000, 20,000, and 24,000 simultaneously connected accounts, each measured for 60 seconds at ten incoming emails/second after account onboarding and lifecycle-webhook delivery settle. Mailbox operations and sends run alongside delivery. No million-account result is implied by this ramp.

The fixture's Dovecot process limits are increased for the ramp; application concurrency settings remain unchanged. Dedicated container memory limits and a host guard stop the ramp at less than 6 GiB available RAM, 90% of a container's memory cap, or the configured process limit. A critical threshold stops Dovecot to release resources. The wrapper stops test containers and restores the original inotify limit when finished.

Results are written to `tests/soak/capacity-results/capacity-report.json`, with individual stage reports, metrics, received events, host-resource samples, and `run.log`. A passing stage establishes only a tested lower bound for this workload. An onboarding timeout, fixture limit, or resource-guard stop is not proof that Rustbox itself cannot handle more accounts on another host. These short stages do not establish long-term memory stability.

Use `CAPACITY_STAGES` and `CAPACITY_STAGE_SECONDS` to choose a different ramp. Archive existing results and explicitly reset only the synthetic capacity volumes before rerunning:

```sh
sudo docker compose -p capacity --env-file tests/soak/.env -f tests/soak/compose.yaml -f tests/soak/capacity.yaml down -v
```

## Lightweight simulator capacity test

`sudo scripts/run_simulator_capacity.sh` uses a single Python asyncio IMAP simulator instead of Dovecot. It still uses verified TLS and real IMAP connections, UID discovery/fetch, IDLE/DONE, BODY.PEEK, flags, MOVE, and APPEND. The existing end-to-end workload verifies webhooks, content, attachments, synchronous mutations, and asynchronous sending. The Compose service retains the `dovecot` hostname solely to reuse test certificates and fixture addressing; its image and executable are Python, not Dovecot.

Eight listening ports distribute connection tuples so one destination's ephemeral-port range does not stop the ramp around 28,000 connections. File-descriptor limits are raised for the test. Default stages are 1,000, 4,000, 16,000, 32,000, 64,000, 96,000, 128,000, 192,000, and 220,000. These are targets, not verified capacities. Application concurrency settings remain unchanged. The resource guard records simulator and Rustbox memory/CPU separately and stops before host exhaustion. A simulator, database, injection, or onboarding limit must be reported separately from a Rustbox failure.

Results go to `tests/soak/simulator-results/`, including `capacity-report.json`, stage reports, resource samples, and the captured console log. Start with a fresh synthetic `simulator` project database and stop other test stacks first because they share the published localhost port. The previous Dovecot results are preserved. The simulator intentionally implements only the protocol subset and synthetic mailbox workload used here; it is not a replacement for real-provider compatibility tests, historical-mail tests, or a production IMAP server. It retains synthetic messages in memory, so its measured resources remain part of the experiment.

The simulator is pinned to Python 3.13 and sets `asyncio.sslproto.SSLProtocol.max_size` to 4 KiB. Python's default 256-KiB buffer is preallocated per TLS connection; leaving it unchanged would make the fixture the memory bottleneck again. This is a private-runtime setting confined to the test fixture and must be reverified when upgrading Python. TLS verification stays enabled, and record data is handled across multiple reads. The optimized fixture measured about 62 MiB at 1,000 connected accounts in its baseline run.
