import concurrent.futures
import io
import json
import math
import os
import pathlib
import platform
import random
import threading
import time

import soak


STAGES = [int(value) for value in os.environ.get("CAPACITY_STAGES", "1000,2000,4000,8000,12000,16000,20000,24000").split(",")]
STAGE_SECONDS = int(os.environ.get("CAPACITY_STAGE_SECONDS", "60"))
MIN_AVAILABLE = int(float(os.environ.get("CAPACITY_MIN_AVAILABLE_GIB", "6")) * 1024 ** 3)
STOP_FILE = pathlib.Path(os.environ.get("CAPACITY_STOP_FILE", str(soak.RESULTS / "capacity.stop")))
FILE_LOCK = threading.Lock()
REPORT = {
    "kind": os.environ.get("CAPACITY_KIND", "real_dovecot_connection_capacity_ramp"),
    "requested_goal_accounts": 1000000,
    "stage_targets": STAGES,
    "stage_seconds": STAGE_SECONDS,
    "incoming_messages_per_second": soak.RATE,
    "last_validated_accounts": 0,
    "last_validated_is_lower_bound": True,
    "million_account_test_performed": False,
    "status": "starting",
    "stages": [],
    "hardware": {"platform": platform.platform(), "cpu_count": os.cpu_count(), "machine": platform.machine()},
}


class StopRun(Exception):
    def __init__(self, kind, reason, details=None):
        self.kind = kind
        self.reason = reason
        self.details = details or {}
        super().__init__(reason)


class TrackedOperations:
    def __init__(self):
        self.pool = concurrent.futures.ThreadPoolExecutor(max_workers=4)
        self.lock = threading.Lock()
        self.futures = set()

    def submit(self, function, *args):
        future = self.pool.submit(function, *args)
        with self.lock:
            self.futures.add(future)
        future.add_done_callback(self.completed)
        return future

    def completed(self, future):
        with self.lock:
            self.futures.discard(future)

    def pending(self):
        with self.lock:
            return len(self.futures)

    def shutdown(self):
        self.pool.shutdown(wait=False, cancel_futures=True)


class RecordedWebhook(soak.Webhook):
    def do_POST(self):
        original = self.rfile
        body = original.read(int(self.headers.get("Content-Length", "0")))
        self.rfile = io.BytesIO(body)
        try:
            super().do_POST()
        finally:
            self.rfile = original
            try:
                event = json.loads(body)
            except (ValueError, TypeError):
                event = {"invalid_json": True}
            append_json("capacity-events.jsonl", {"received_at": time.time(), "event": event})


def append_json(filename, value):
    with FILE_LOCK:
        with (soak.RESULTS / filename).open("a") as output:
            output.write(json.dumps(value) + "\n")


def save_report():
    temporary = soak.RESULTS / "capacity-report.json.tmp"
    temporary.write_text(json.dumps(REPORT, indent=2) + "\n")
    temporary.replace(soak.RESULTS / "capacity-report.json")


def progress(kind, **values):
    value = {"time": time.time(), "kind": kind, **values}
    print(json.dumps(value), flush=True)
    append_json("capacity-progress.jsonl", value)


def host_memory():
    values = {}
    for line in pathlib.Path("/proc/meminfo").read_text().splitlines():
        name, amount = line.split(":", 1)
        if name in ("MemAvailable", "MemTotal", "SwapFree", "SwapTotal"):
            values[name] = int(amount.split()[0]) * 1024
    return values


def guard():
    if STOP_FILE.exists():
        detail = STOP_FILE.read_text()[:4096]
        raise StopRun("host_guard", "external resource guard requested stop", {"stop_file": str(STOP_FILE), "guard_detail": detail})
    memory = host_memory()
    available = memory.get("MemAvailable", 0)
    if available < MIN_AVAILABLE:
        raise StopRun("host_guard", "available host memory below configured reserve", {"available_bytes": available, "minimum_available_bytes": MIN_AVAILABLE})


def interruptible_sleep(seconds):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        guard()
        time.sleep(min(0.5, max(0, deadline - time.monotonic())))


def read_metrics(phase, target, started):
    guard()
    try:
        raw = soak.api("/metrics", raw=True).decode()
        values = {line.split()[0]: float(line.split()[1]) for line in raw.splitlines() if line and not line.startswith("#")}
    except Exception as exception:
        raise StopRun("infrastructure_failure", "metrics endpoint unavailable", {"error": str(exception)[:200]}) from exception
    values.update({"time": time.time(), "phase": phase, "target_accounts": target, "elapsed": time.monotonic() - started})
    values.update({"host_" + name + "_bytes": value for name, value in host_memory().items()})
    with soak.LOCK:
        values.update({"injected": soak.COUNTERS["injected"], "received": soak.COUNTERS["received"], "pending_messages": len(soak.PENDING), "ready_accounts": len(soak.READY)})
    append_json("capacity-metrics.jsonl", values)
    return values


def first_errors():
    with soak.LOCK:
        return list(soak.ERRORS[:100])


def onboard_stage(accounts, target):
    started = time.monotonic()
    initial_count = len(accounts)
    new_accounts = target - initial_count
    setup_timeout = max(300, math.ceil(new_accounts * 5 / 128) + 300)
    deadline = started + setup_timeout
    pool = concurrent.futures.ThreadPoolExecutor(max_workers=16)
    pending = {}
    next_index = initial_count
    progress("onboarding_started", target_accounts=target, new_accounts=new_accounts, timeout_seconds=setup_timeout)
    try:
        while len(accounts) < target:
            guard()
            if time.monotonic() > deadline:
                raise StopRun("setup_timeout", "account creation did not finish before onboarding deadline", {"created_accounts": len(accounts), "target_accounts": target, "timeout_seconds": setup_timeout})
            while len(pending) < 16 and next_index < target:
                future = pool.submit(soak.onboard, next_index)
                pending[future] = next_index
                next_index += 1
            completed, _ = concurrent.futures.wait(pending, timeout=0.5, return_when=concurrent.futures.FIRST_COMPLETED)
            for future in completed:
                index = pending.pop(future)
                try:
                    accounts.append(future.result())
                except Exception as exception:
                    raise StopRun("setup_failure", "account creation request failed", {"account_index": index, "error": str(exception)[:200]}) from exception
            if len(accounts) % 1000 < len(completed):
                progress("accounts_created", created_accounts=len(accounts), target_accounts=target)
        while True:
            guard()
            metric = read_metrics("onboarding", target, started)
            ready = metric["ready_accounts"] >= target
            connected = metric.get("rustbox_connected_accounts") == target
            drained = metric.get("rustbox_webhook_queue_depth", 0) <= 5
            errors = first_errors()
            if errors:
                raise StopRun("setup_failure", "fixture errors during onboarding", {"errors": errors})
            if ready and connected and drained:
                return {"new_accounts": new_accounts, "onboarding_seconds": time.monotonic() - started, "setup_timeout_seconds": setup_timeout, "ready_accounts": metric["ready_accounts"], "connected_accounts": metric["rustbox_connected_accounts"]}
            if time.monotonic() > deadline:
                raise StopRun("setup_timeout", "accounts did not connect, synchronize, and drain lifecycle webhooks before deadline", {"ready_accounts": metric["ready_accounts"], "connected_accounts": metric.get("rustbox_connected_accounts"), "webhook_queue_depth": metric.get("rustbox_webhook_queue_depth"), "target_accounts": target, "timeout_seconds": setup_timeout})
            interruptible_sleep(5)
    finally:
        pool.shutdown(wait=False, cancel_futures=True)


def reset_window():
    if soak.OPERATIONS.pending():
        raise StopRun("infrastructure_failure", "previous stage operations have not drained")
    with soak.LOCK:
        if soak.PENDING:
            raise StopRun("infrastructure_failure", "previous stage messages have not drained")
        soak.LATENCIES.clear()
        soak.METRICS.clear()
        for name in soak.COUNTERS:
            soak.COUNTERS[name] = 0


def measure_stage(accounts, target):
    reset_window()
    soak.ACCOUNT_COUNT = target
    soak.DURATION = STAGE_SECONDS
    started = time.monotonic()
    soak.STARTED = started
    metrics = [read_metrics("measurement", target, started)]
    next_metric = started + 5
    next_injection = started
    pool = concurrent.futures.ThreadPoolExecutor(max_workers=16)
    pending = set()
    progress("measurement_started", target_accounts=target, duration_seconds=STAGE_SECONDS, incoming_messages_per_second=soak.RATE)
    try:
        while time.monotonic() - started < STAGE_SECONDS:
            guard()
            pending = {future for future in pending if not future.done()}
            if len(pending) < 32 and time.monotonic() >= next_injection:
                pending.add(pool.submit(soak.inject, random.choice(accounts)))
                next_injection += 1 / soak.RATE
            if time.monotonic() >= next_metric:
                metrics.append(read_metrics("measurement", target, started))
                next_metric = time.monotonic() + 5
            if first_errors():
                break
            time.sleep(0.01)
        injection_seconds = time.monotonic() - started
        drain_deadline = time.monotonic() + 120
        while True:
            guard()
            pending = {future for future in pending if not future.done()}
            with soak.LOCK:
                messages_pending = len(soak.PENDING)
            if not pending and messages_pending == 0 and soak.OPERATIONS.pending() == 0:
                break
            if time.monotonic() > drain_deadline:
                break
            if time.monotonic() >= next_metric:
                metrics.append(read_metrics("drain", target, started))
                next_metric = time.monotonic() + 5
            time.sleep(0.25)
        final_metric = read_metrics("drain", target, started)
        metrics.append(final_metric)
        with soak.LOCK:
            counters = dict(soak.COUNTERS)
            latencies = list(soak.LATENCIES)
            errors = list(soak.ERRORS)
            remaining = len(soak.PENDING)
        p95 = soak.percentile(latencies, 0.95)
        expected = STAGE_SECONDS * soak.RATE
        workload_samples = [sample for sample in metrics if sample["phase"] == "measurement"]
        backlog_limit = max(50, soak.RATE * 5)
        backlog = [sample.get("rustbox_webhook_queue_depth", 0) for sample in workload_samples]
        failures = []
        if injection_seconds < STAGE_SECONDS:
            failures.append("measurement_ended_early")
        if counters["injected"] < expected * 0.95:
            failures.append("injection_rate_below_target")
        if remaining or counters["received"] != counters["injected"]:
            failures.append("messages_not_reconciled")
        if p95 is None or p95 >= 5:
            failures.append("webhook_p95_exceeded")
        if errors:
            failures.append("fixture_or_operation_errors")
        if counters["send_checks"] == 0 or counters["smtp_accepted"] != counters["send_checks"] or soak.OPERATIONS.pending():
            failures.append("send_checks_incomplete")
        if any(sample.get("rustbox_connected_accounts") != target for sample in metrics):
            failures.append("connected_account_count_changed")
        if any(all(value >= backlog_limit for value in backlog[index:index + 3]) for index in range(max(0, len(backlog) - 2))):
            failures.append("sustained_webhook_backlog")
        if final_metric.get("rustbox_webhook_queue_depth", 0) >= backlog_limit:
            failures.append("webhook_backlog_not_drained")
        rss = [sample["process_resident_memory_bytes"] for sample in metrics]
        return {
            "target_accounts": target,
            "measurement_seconds": injection_seconds,
            "total_seconds_with_drain": time.monotonic() - started,
            "incoming_messages_per_second": soak.RATE,
            "counters": counters,
            "p95_injection_to_webhook_seconds": p95,
            "pending_messages": remaining,
            "minimum_connected_accounts": min(sample.get("rustbox_connected_accounts", 0) for sample in metrics),
            "rss_start_bytes": rss[0],
            "rss_end_bytes": rss[-1],
            "rss_peak_bytes": max(rss),
            "minimum_host_available_bytes": min(sample.get("host_MemAvailable_bytes", 0) for sample in metrics),
            "maximum_webhook_backlog": max(backlog, default=0),
            "errors": errors[:100],
            "error_count": len(errors),
            "failure_reasons": failures,
            "passed": not failures,
            "memory_stability_verified": False,
        }
    finally:
        pool.shutdown(wait=False, cancel_futures=True)


def main():
    receivers = []
    accounts = []
    current_stage = None
    soak.RESULTS.mkdir(parents=True, exist_ok=True)
    REPORT["started_at"] = time.time()
    REPORT["hardware"].update(host_memory())
    save_report()
    try:
        if not STAGES or STAGES != sorted(set(STAGES)) or min(STAGES) < 1 or STAGE_SECONDS < 1 or soak.RATE <= 0:
            raise StopRun("configuration_error", "stages must be increasing positive counts and duration/rate must be positive")
        guard()
        soak.OPERATIONS.shutdown(wait=False, cancel_futures=True)
        soak.OPERATIONS = TrackedOperations()
        soak.Webhook = RecordedWebhook
        receivers = soak.start_receivers()
        deadline = time.monotonic() + 180
        while True:
            guard()
            try:
                existing = soak.api("/v1/accounts")["accounts"]
                break
            except Exception:
                if time.monotonic() > deadline:
                    raise StopRun("infrastructure_failure", "API did not become ready within 180 seconds")
                interruptible_sleep(2)
        if existing:
            raise StopRun("configuration_error", "capacity ramp requires a fresh synthetic database")
        REPORT["status"] = "running"
        save_report()
        for target in STAGES:
            current_stage = {"target_accounts": target, "status": "onboarding"}
            REPORT["active_stage"] = current_stage
            save_report()
            setup = onboard_stage(accounts, target)
            current_stage.update(setup)
            current_stage["status"] = "measuring"
            save_report()
            result = {**setup, **measure_stage(accounts, target)}
            current_stage["status"] = "completed" if result["passed"] else "failed"
            current_stage["passed"] = result["passed"]
            REPORT["stages"].append(result)
            (soak.RESULTS / f"stage-{target}.json").write_text(json.dumps(result, indent=2) + "\n")
            if result["passed"]:
                REPORT["last_validated_accounts"] = target
            save_report()
            progress("stage_completed", **result)
            if not result["passed"]:
                raise StopRun("workload_failure", "stage did not meet the measured acceptance criteria", {"target_accounts": target, "failure_reasons": result["failure_reasons"]})
        REPORT["status"] = "all_requested_stages_passed"
        REPORT["stop_reason"] = "ramp ceiling reached; larger counts remain untested"
        return_code = 0
    except StopRun as stopped:
        REPORT["status"] = "stopped"
        REPORT["stop_kind"] = stopped.kind
        REPORT["stop_reason"] = stopped.reason
        REPORT["stop_details"] = stopped.details
        REPORT["created_accounts"] = len(accounts)
        if current_stage:
            current_stage["status"] = "stopped"
        return_code = 2 if stopped.kind == "host_guard" else 1
    except Exception as exception:
        REPORT["status"] = "stopped"
        REPORT["stop_kind"] = "infrastructure_failure"
        REPORT["stop_reason"] = str(exception)[:300]
        REPORT["created_accounts"] = len(accounts)
        return_code = 1
    finally:
        REPORT["finished_at"] = time.time()
        save_report()
        print(json.dumps(REPORT, indent=2), flush=True)
        soak.STOP.set()
        if isinstance(soak.OPERATIONS, TrackedOperations):
            soak.OPERATIONS.shutdown()
        else:
            soak.OPERATIONS.shutdown(wait=False, cancel_futures=True)
        for receiver in receivers:
            receiver.shutdown()
    raise SystemExit(return_code)


if __name__ == "__main__":
    main()
