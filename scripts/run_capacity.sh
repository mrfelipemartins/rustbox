#!/bin/bash
set -u
cd "$(dirname "$0")/.."
results=tests/soak/capacity-results
mkdir -p "$results"
if [ -e "$results/capacity-report.json" ] || [ -e "$results/capacity.stop" ]; then
    printf '%s\n' 'Archive prior capacity results and use a fresh capacity database before rerunning.' >&2
    exit 1
fi
original_inotify=$(sysctl -n fs.inotify.max_user_instances)
guard_pid=
cleanup() {
    result=$?
    if [ -n "$guard_pid" ]; then kill "$guard_pid" 2>/dev/null || true; fi
    docker compose -p capacity --env-file tests/soak/.env -f tests/soak/compose.yaml -f tests/soak/capacity.yaml stop -t 60 >/dev/null 2>&1 || true
    sysctl -w "fs.inotify.max_user_instances=$original_inotify"
    printf '%s\n' "$result" > "$results/capacity.exit"
}
trap cleanup EXIT
printf '%s\n' "$$" > "$results/capacity.pid"
sysctl -w fs.inotify.max_user_instances=32768
python3 scripts/capacity_guard.py > "$results/guard.log" 2>&1 &
guard_pid=$!
docker compose -p capacity --env-file tests/soak/.env -f tests/soak/compose.yaml -f tests/soak/capacity.yaml up --build --abort-on-container-exit --exit-code-from runner
