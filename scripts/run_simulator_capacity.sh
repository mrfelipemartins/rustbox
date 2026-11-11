#!/bin/bash
set -u
cd "$(dirname "$0")/.."
results=tests/soak/simulator-results
mkdir -p "$results"
if [ -e "$results/capacity-report.json" ] || [ -e "$results/capacity.stop" ]; then
    printf '%s\n' 'Archive prior simulator results and explicitly reset its test database before rerunning.' >&2
    exit 1
fi
guard_pid=
cleanup() {
    result=$?
    if [ -n "$guard_pid" ]; then kill "$guard_pid" 2>/dev/null || true; fi
    docker compose -p simulator --env-file tests/soak/.env -f tests/soak/compose.yaml -f tests/soak/simulator.yaml stop -t 60 >/dev/null 2>&1 || true
    printf '%s\n' "$result" > "$results/capacity.exit"
}
trap cleanup EXIT
printf '%s\n' "$$" > "$results/capacity.pid"
CAPACITY_PROJECT=simulator CAPACITY_RESULTS="$results" python3 scripts/capacity_guard.py > "$results/guard.log" 2>&1 &
guard_pid=$!
docker compose -p simulator --env-file tests/soak/.env -f tests/soak/compose.yaml -f tests/soak/simulator.yaml up --build --abort-on-container-exit --exit-code-from runner
