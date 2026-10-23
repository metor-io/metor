#!/usr/bin/env bash
# Builds metor and metor-panel in release, runs sim.py in the background, and
# opens a panel on its db with the star-tracker layout. Quitting the panel
# (Cmd-Q) stops the sim. METOR_DEMO_DB_PORT moves the db off 2250.
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"
bin="${CARGO_TARGET_DIR:-$root/target}/release"
db_port="${METOR_DEMO_DB_PORT:-2250}"
pub_port="${METOR_DEMO_PUB_PORT:-2240}"
log="${TMPDIR:-/tmp}/metor-star-demo.log"

for port in "$pub_port" "$db_port"; do
  if nc -z 127.0.0.1 "$port" 2>/dev/null; then
    echo "port $port is in use; stop whatever holds it or set METOR_DEMO_DB_PORT/METOR_DEMO_PUB_PORT" >&2
    exit 1
  fi
done

cargo build --release -p metor-fsw-3 -p metor-panel --bin metor --bin metor-panel

echo "starting sim.py (log: $log); the first run also builds its packs"
METOR_DEPLOY_PORTS="$pub_port,0,$db_port" "$bin/metor" run examples/adcs-fsw3/sim.py >"$log" 2>&1 &
sim=$!
stop_sim() {
  kill -INT "$sim" 2>/dev/null || return 0
  for _ in $(seq 50); do kill -0 "$sim" 2>/dev/null || return 0; sleep 0.1; done
  kill -KILL "$sim" 2>/dev/null || true
}
trap stop_sim EXIT INT TERM

until nc -z 127.0.0.1 "$db_port" 2>/dev/null; do
  if ! kill -0 "$sim" 2>/dev/null; then
    echo "sim.py exited before its db came up:" >&2
    tail -n 30 "$log" >&2
    exit 1
  fi
  sleep 0.5
done

echo "db up on 127.0.0.1:$db_port; opening metor-panel"
"$bin/metor-panel" --connect "127.0.0.1:$db_port" \
  --preset examples/adcs-fsw3/presets/star-tracker.json
