import json
import os
import pathlib
import subprocess
import time

RESULTS = pathlib.Path(os.environ.get("CAPACITY_RESULTS", "tests/soak/capacity-results"))
PROJECT = os.environ.get("CAPACITY_PROJECT", "capacity")
MIN_AVAILABLE = 6 * 1024**3
START = time.monotonic()


def docker(*arguments):
    return subprocess.check_output(["docker", *arguments], timeout=15, text=True)


def available_memory():
    for line in pathlib.Path("/proc/meminfo").read_text().splitlines():
        if line.startswith("MemAvailable:"):
            return int(line.split()[1]) * 1024
    raise RuntimeError("MemAvailable unavailable")


def host_cpu_ticks():
    names = ("user", "nice", "system", "idle", "iowait", "irq", "softirq", "steal", "guest", "guest_nice")
    fields = pathlib.Path("/proc/stat").read_text().splitlines()[0].split()
    if fields[0] != "cpu":
        raise RuntimeError("aggregate CPU statistics unavailable")
    return dict(zip(names, map(int, fields[1:])))


def snapshot():
    ids = docker("ps", "-q", "--filter", "label=com.docker.compose.project=" + PROJECT).split()
    result = {"elapsed": time.monotonic() - START, "available_memory_bytes": available_memory(), "containers": [], "host_cpu_ticks": host_cpu_ticks(), "clock_ticks_per_second": os.sysconf("SC_CLK_TCK")}
    if not ids:
        return result
    for item in json.loads(docker("inspect", *ids)):
        pid = item["State"]["Pid"]
        if not pid:
            continue
        groups = pathlib.Path(f"/proc/{pid}/cgroup").read_text().splitlines()
        path = next(line.split(":", 2)[2] for line in groups if line.startswith("0::"))
        group = pathlib.Path("/sys/fs/cgroup") / path.lstrip("/")
        current = int((group / "memory.current").read_text())
        limit = item["HostConfig"]["Memory"]
        cpu = dict(line.split() for line in (group / "cpu.stat").read_text().splitlines())
        result["containers"].append({"name": item["Name"], "memory_current_bytes": current, "memory_limit_bytes": limit, "pids": int((group / "pids.current").read_text()), "cpu_usage_usec": int(cpu["usage_usec"]), "cpu_user_usec": int(cpu.get("user_usec", 0)), "cpu_system_usec": int(cpu.get("system_usec", 0))})
    return result


def guard_reason(sample):
    if sample["available_memory_bytes"] < MIN_AVAILABLE:
        return "host available memory below 6 GiB"
    for container in sample["containers"]:
        if container["memory_limit_bytes"] and container["memory_current_bytes"] > container["memory_limit_bytes"] * 0.90:
            return container["name"] + " reached 90% of its memory limit"
        if container["pids"] > 24000:
            return container["name"] + " reached process safety limit"
    if sample["elapsed"] > 14400:
        return "capacity test exceeded four-hour safety deadline"
    return None


def main():
    RESULTS.mkdir(parents=True, exist_ok=True)
    while True:
        try:
            sample = snapshot()
            with (RESULTS / "host-metrics.jsonl").open("a") as output:
                output.write(json.dumps(sample) + "\n")
            reason = guard_reason(sample)
            if reason:
                stop = RESULTS / "capacity.stop"
                if not stop.exists():
                    stop.write_text(json.dumps({"reason": reason, "sample": sample}) + "\n")
                    print(reason, flush=True)
            critical = sample["available_memory_bytes"] < 2 * 1024**3 or any(c["memory_limit_bytes"] and c["memory_current_bytes"] > c["memory_limit_bytes"] * 0.97 for c in sample["containers"])
            if critical:
                for item in sample["containers"]:
                    if "dovecot" in item["name"]:
                        docker("stop", "-t", "3", item["name"].lstrip("/"))
        except Exception as exception:
            print(type(exception).__name__ + ": " + str(exception), flush=True)
            (RESULTS / "capacity.stop").write_text("host guard could not verify resource headroom\n")
        time.sleep(5)


if __name__ == "__main__":
    main()
