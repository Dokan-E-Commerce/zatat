#!/usr/bin/env bash
# Run the three benchmark scenarios against a Pusher-protocol server already
# listening on $HOST:$PORT. Invoke twice — once against a running zatat,
# once against a running Reverb — to diff the outputs.
#
# Usage:  bash run.sh <host> <port> <label>

set -euo pipefail

HOST="${1:-127.0.0.1}"
PORT="${2:-8080}"
LABEL="${3:-server}"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT="$HERE/results/$LABEL"
mkdir -p "$OUT"

command -v wrk  >/dev/null || { echo "wrk not installed — run setup.sh" >&2; exit 1; }
command -v node >/dev/null || { echo "node not installed — run setup.sh" >&2; exit 1; }

if [[ ! -d "$HERE/node_modules" ]]; then
  (cd "$HERE" && npm init -y >/dev/null 2>&1 && npm install --silent ws)
fi

echo "==> [1/3] HTTP POST /events throughput"
node "$HERE/scenarios/gen-wrk-script.mjs" "$HOST" "$PORT" > "$OUT/wrk.lua"
wrk -t2 -c64 -d20s -s "$OUT/wrk.lua" "http://$HOST:$PORT" | tee "$OUT/01-http-throughput.txt"

echo ""
echo "==> [2/3] WebSocket idle connection ramp (target 10,000)"
node "$HERE/scenarios/ws-idle-ramp.mjs" "$HOST" "$PORT" 10000 100 20 | tee "$OUT/02-ws-idle-ramp.json"

echo ""
echo "==> [3/3] Broadcast fan-out latency (1,000 subscribers)"
node "$HERE/scenarios/broadcast-fanout.mjs" "$HOST" "$PORT" 1000 | tee "$OUT/03-broadcast-fanout.json"

echo ""
echo "==> done — results in $OUT"
