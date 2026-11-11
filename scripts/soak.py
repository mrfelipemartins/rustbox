import base64
import concurrent.futures
import email.policy
import email.utils
import hashlib
import hmac
import http.server
import imaplib
import json
import math
import os
import pathlib
import platform
import random
import socketserver
import ssl
import statistics
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from email.message import EmailMessage


API = os.environ.get("SOAK_API_URL", "http://rustbox:8080")
KEY = os.environ["SOAK_API_KEY"]
SECRET = os.environ["SOAK_WEBHOOK_SECRET"].encode()
ACCOUNT_COUNT = int(os.environ.get("SOAK_ACCOUNTS", "1000"))
DURATION = int(os.environ.get("SOAK_SECONDS", "86400"))
RATE = float(os.environ.get("SOAK_RATE", "10"))
RESULTS = pathlib.Path(os.environ.get("SOAK_RESULTS", "/results"))
CA = os.environ.get("SOAK_CA", "/certs/ca.crt")
LOCK = threading.Lock()
READY = set()
PENDING = {}
LATENCIES = []
METRICS = []
ERRORS = []
SEEN = set()
COUNTERS = {"injected": 0, "received": 0, "operations": 0, "smtp_accepted": 0, "send_checks": 0}
OPERATIONS = concurrent.futures.ThreadPoolExecutor(max_workers=4)
STOP = threading.Event()


def api(path, method="GET", body=None, raw=False, headers=None):
    data = None if body is None else json.dumps(body).encode()
    headers = dict(headers or {})
    if path == "/v1/accounts" and method == "POST":
        headers.setdefault("Idempotency-Key", "soak-" + hashlib.sha256(data).hexdigest())
    request = urllib.request.Request(API + path, data=data, method=method, headers={
        "Authorization": "Bearer " + KEY,
        "Content-Type": "application/json",
        **(headers or {}),
    })
    with urllib.request.urlopen(request, timeout=90) as response:
        payload = response.read()
        return payload if raw else json.loads(payload) if payload else None


def error(kind, exception):
    with LOCK:
        ERRORS.append({"time": time.time(), "kind": kind, "error": str(exception)[:200]})


def exercise(account_id, data, trash):
    try:
        reference = {"message_id": data["message_id"]}
        message_path = f"/v1/accounts/{account_id}/messages/{data['message_id']}"
        content = api(message_path + "/content")
        attachment = api(message_path + "/attachments/0", raw=True)
        if "Synthetic soak message." not in content.get("text", "") or attachment != b"soak attachment":
            raise RuntimeError("on-demand content did not match injected message")
        api(f"/v1/accounts/{account_id}/flags", "PATCH", {**reference, "flags": ["\\Seen"]})
        if trash:
            api(f"/v1/accounts/{account_id}/trash", "POST", reference)
        else:
            api(f"/v1/accounts/{account_id}/move", "POST", {**reference, "destination": "Archive"})
        send_path = f"/v1/accounts/{account_id}/send"
        send_body = {
            "from": "sender@soak.local", "to": ["recipient@soak.local"],
            "subject": "Outgoing soak", "text": "Asynchronous send check",
            "attachments": [{"filename": "sample.txt", "content_type": "text/plain", "data_base64": base64.b64encode(b"outgoing attachment").decode()}],
        }
        headers = {"Idempotency-Key": str(uuid.uuid4())}
        job = api(send_path, "POST", send_body, headers=headers)
        repeated = api(send_path, "POST", send_body, headers=headers)
        if job["id"] != repeated["id"]:
            raise RuntimeError("idempotent send returned another job")
        deadline = time.monotonic() + 30
        while True:
            status = api("/v1/sends/" + job["id"])
            if status["status"] == "accepted" and status["append_status"] == "complete":
                break
            if status["status"] in ("failed", "unknown") or status["append_status"] in ("failed", "unknown"):
                raise RuntimeError("send or Sent append failed: " + json.dumps(status))
            if time.monotonic() > deadline:
                raise RuntimeError("send did not finish within 30 seconds")
            time.sleep(0.25)
        with LOCK:
            COUNTERS["operations"] += 1
            COUNTERS["send_checks"] += 1
    except Exception as exception:
        error("mail_operation", exception)


class Webhook(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        try:
            signature = dict(part.split("=", 1) for part in self.headers["x-rustbox-signature"].split(","))
            expected = hmac.new(SECRET, signature["t"].encode() + b"." + body, hashlib.sha256).hexdigest()
            valid = hmac.compare_digest(expected, signature["v1"]) and abs(time.time() - int(signature["t"])) < 300
            if not valid:
                raise ValueError("invalid webhook signature")
            event = json.loads(body)
            exercise_data = None
            with LOCK:
                if event["id"] not in SEEN:
                    SEEN.add(event["id"])
                    if event["type"] == "account.sync_completed":
                        READY.add(event["account_id"])
                    if event["type"] == "message.created":
                        data = event["data"]
                        token = data.get("metadata", {}).get("message_id")
                        started = PENDING.pop(token, None)
                        if started is not None:
                            LATENCIES.append(time.monotonic() - started)
                            COUNTERS["received"] += 1
                            if COUNTERS["received"] % 50 == 1:
                                exercise_data = (event["account_id"], data, COUNTERS["received"] % 100 == 51)
            if exercise_data:
                OPERATIONS.submit(exercise, *exercise_data)
            self.send_response(204)
            self.end_headers()
        except Exception as exception:
            error("webhook", exception)
            self.send_response(400)
            self.end_headers()

    def log_message(self, *_args):
        pass


class SMTP(socketserver.StreamRequestHandler):
    def handle(self):
        self.connection.settimeout(60)
        self.wfile.write(b"220 soak ESMTP\r\n")
        while True:
            line = self.rfile.readline(8192)
            if not line:
                return
            command = line.split(b" ", 1)[0].strip().upper()
            if command == b"EHLO":
                self.wfile.write(b"250-soak\r\n250-8BITMIME\r\n250 AUTH PLAIN\r\n")
            elif command == b"AUTH":
                self.wfile.write(b"235 authenticated\r\n")
            elif command == b"DATA":
                self.wfile.write(b"354 send content\r\n")
                while True:
                    line = self.rfile.readline(1024 * 1024)
                    if not line:
                        return
                    if line == b".\r\n":
                        break
                with LOCK:
                    COUNTERS["smtp_accepted"] += 1
                self.wfile.write(b"250 accepted\r\n")
            elif command == b"QUIT":
                self.wfile.write(b"221 bye\r\n")
                return
            elif command in (b"MAIL", b"RCPT", b"NOOP", b"RSET"):
                self.wfile.write(b"250 ok\r\n")
            else:
                self.wfile.write(b"500 unsupported\r\n")


class SMTPServer(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True

    def get_request(self):
        connection, address = super().get_request()
        connection.settimeout(30)
        return self.context.wrap_socket(connection, server_side=True), address


def start_receivers():
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain("/certs/server.crt", "/certs/server.key")
    webhook = http.server.ThreadingHTTPServer(("0.0.0.0", 9443), Webhook)
    webhook.socket = context.wrap_socket(webhook.socket, server_side=True)
    smtp = SMTPServer(("0.0.0.0", 1465), SMTP)
    smtp.context = context
    for server in (webhook, smtp):
        threading.Thread(target=server.serve_forever, daemon=True).start()
    return webhook, smtp


def imap_port(index):
    ports = [int(value) for value in os.environ.get("SOAK_IMAP_PORTS", "993").split(",")]
    return ports[index % len(ports)]


def onboard(index):
    username = f"account{index}"
    account = api("/v1/accounts", "POST", {
        "name": username,
        "imap": {"host": "dovecot", "port": imap_port(index), "username": username, "password": "soak-password"},
        "smtp": {"host": "runner", "port": 1465, "username": username, "password": "soak-password"},
        "backfill_days": 30,
        "trash_folder": "Trash",
        "sent_folder": "Sent",
        "watched_folders": [],
        "webhook_url": "https://runner:9443/events",
        "webhook_secret": SECRET.decode(),
    })
    return {"id": account["id"], "username": username, "imap_port": imap_port(index)}


def inject(account):
    token = f"<{uuid.uuid4()}@soak.local>"
    message = EmailMessage(policy=email.policy.SMTP)
    message["From"] = "sender@soak.local"
    message["To"] = account["username"] + "@soak.local"
    message["Subject"] = "Rustbox soak"
    message["Message-ID"] = token
    message["Date"] = email.utils.formatdate(usegmt=True)
    message.set_content("Synthetic soak message.\n" + "x" * 4096)
    message.add_attachment(b"soak attachment", maintype="application", subtype="octet-stream", filename="sample.bin")
    try:
        context = ssl.create_default_context(cafile=CA)
        with imaplib.IMAP4_SSL("dovecot", account.get("imap_port", 993), ssl_context=context, timeout=30) as connection:
            connection.login(account["username"], "soak-password")
            with LOCK:
                PENDING[token] = time.monotonic()
            status, _ = connection.append("INBOX", None, None, message.as_bytes())
            if status != "OK":
                raise RuntimeError("IMAP append failed")
            with LOCK:
                COUNTERS["injected"] += 1
    except Exception as exception:
        with LOCK:
            PENDING.pop(token, None)
        error("injection", exception)


def sample_metrics():
    while not STOP.is_set():
        try:
            response = api("/metrics", raw=True).decode()
            values = {line.split()[0]: float(line.split()[1]) for line in response.splitlines() if line and not line.startswith("#")}
            values["elapsed"] = time.monotonic() - STARTED
            with LOCK:
                values["injected"] = COUNTERS["injected"]
                values["received"] = COUNTERS["received"]
                values["pending_messages"] = len(PENDING)
                METRICS.append(values)
            with (RESULTS / "metrics.jsonl").open("a") as output:
                output.write(json.dumps(values) + "\n")
        except Exception as exception:
            error("metrics", exception)
        STOP.wait(30)


def percentile(values, fraction):
    return sorted(values)[min(len(values) - 1, math.ceil(len(values) * fraction) - 1)] if values else None


def report(elapsed):
    with LOCK:
        rss = [sample["process_resident_memory_bytes"] for sample in METRICS if sample["elapsed"] > 300]
        quarter = max(1, len(rss) // 4)
        growth = statistics.median(rss[-quarter:]) - statistics.median(rss[:quarter]) if len(rss) >= 4 else None
        p95 = percentile(LATENCIES, 0.95)
        backlog = [sample.get("rustbox_webhook_queue_depth", 0) for sample in METRICS[-10:]]
        result = {
            "duration_seconds": elapsed,
            "configuration": {"accounts": ACCOUNT_COUNT, "requested_seconds": DURATION, "incoming_per_second": RATE},
            "hardware": {"platform": platform.platform(), "cpu_count": os.cpu_count(), "machine": platform.machine(), "mem_total_kib": next((int(line.split()[1]) for line in pathlib.Path("/proc/meminfo").read_text().splitlines() if line.startswith("MemTotal:")), None)},
            "counters": COUNTERS,
            "p95_injection_to_webhook_seconds": p95,
            "pending_messages": len(PENDING),
            "rss_growth_after_warmup_bytes": growth,
            "errors": ERRORS[:100],
            "error_count": len(ERRORS),
            "maximum_recent_webhook_backlog": max(backlog, default=0),
        }
        result["passed"] = (
            elapsed >= DURATION and COUNTERS["injected"] >= max(1, DURATION * RATE * 0.95) and len(PENDING) == 0
            and COUNTERS["received"] == COUNTERS["injected"] and not ERRORS
            and COUNTERS["send_checks"] > 0 and COUNTERS["smtp_accepted"] == COUNTERS["send_checks"]
            and p95 is not None and p95 < 5
            and (growth is not None and growth < 64 * 1024 * 1024 if DURATION >= 1800 else True)
            and all(sample.get("rustbox_connected_accounts") == ACCOUNT_COUNT for sample in METRICS)
            and max(backlog, default=0) < max(50, RATE * 5)
        )
    (RESULTS / "report.json").write_text(json.dumps(result, indent=2))
    print(json.dumps(result, indent=2), flush=True)
    return result["passed"]


def main():
    global STARTED
    if ACCOUNT_COUNT < 1 or DURATION < 1 or RATE <= 0:
        raise ValueError("accounts, seconds, and rate must be positive")
    RESULTS.mkdir(parents=True, exist_ok=True)
    receivers = start_receivers()
    deadline = time.monotonic() + 180
    while True:
        try:
            existing = api("/v1/accounts")["accounts"]
            break
        except Exception:
            if time.monotonic() > deadline:
                raise
            time.sleep(2)
    if existing:
        raise RuntimeError("Soak requires a fresh database; remove the soak stack volumes before starting")
    with concurrent.futures.ThreadPoolExecutor(max_workers=16) as pool:
        accounts = list(pool.map(onboard, range(ACCOUNT_COUNT)))
    deadline = time.monotonic() + max(300, ACCOUNT_COUNT)
    while len(READY) < ACCOUNT_COUNT:
        if time.monotonic() > deadline:
            raise RuntimeError(f"Only {len(READY)}/{ACCOUNT_COUNT} accounts completed initial sync")
        time.sleep(2)
    STARTED = time.monotonic()
    monitor = threading.Thread(target=sample_metrics, daemon=True)
    monitor.start()
    limiter = threading.BoundedSemaphore(64)
    try:
        with concurrent.futures.ThreadPoolExecutor(max_workers=16) as pool:
            scheduled = STARTED
            while time.monotonic() - STARTED < DURATION:
                limiter.acquire()
                future = pool.submit(inject, random.choice(accounts))
                future.add_done_callback(lambda _: limiter.release())
                scheduled += 1 / RATE
                time.sleep(max(0, scheduled - time.monotonic()))
        deadline = time.monotonic() + 120
        while PENDING and time.monotonic() < deadline:
            time.sleep(1)
        OPERATIONS.shutdown(wait=True)
    finally:
        STOP.set()
        monitor.join(timeout=100)
        for receiver in receivers:
            receiver.shutdown()
    passed = report(time.monotonic() - STARTED)
    raise SystemExit(0 if passed else 1)


if __name__ == "__main__":
    main()
