#!/usr/bin/env bash
# node-service — the Level 0 story for Node. A bare `fetch` dies on a transient
# 500; `keel run` (the Node loader intercepts fetch) retries it and it survives.
# Deterministic: faultproxy serves 500 then 200.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/../.." && pwd)"

PY="${KEEL_PYTHON:-}"
if [ -z "$PY" ]; then
  if [ -x "$REPO/.venv/bin/python3" ]; then PY="$REPO/.venv/bin/python3"; else PY="python3"; fi
fi
NODE_RUN="$REPO/node/keel/bin/keel-node-run.mjs"  # from-source `keel run` for Node

PORT_FILE="$(mktemp)"
"$PY" "$REPO/tools/faultproxy/faultproxy.py" \
  --scenario "$SCRIPT_DIR/scenario.json" --port 0 --port-file "$PORT_FILE" >/dev/null 2>&1 &
FP_PID=$!
trap 'kill "$FP_PID" 2>/dev/null || true' EXIT
for _ in $(seq 1 50); do [ -s "$PORT_FILE" ] && break; sleep 0.1; done
PORT="$(cat "$PORT_FILE")"
URL="http://127.0.0.1:${PORT}/svc"

echo "== 1) bare: node app.mjs (expect FAILURE on the 500) =="
if KEEL_DEMO_URL="$URL" node "$SCRIPT_DIR/app.mjs"; then
  echo "   (unexpected success)"; else echo "   ✗ threw on the transient 500, as a bare script does"; fi

curl -s -X POST "http://127.0.0.1:${PORT}/__faultproxy__/reset" >/dev/null

echo "== 2) keel run app.mjs (expect SURVIVES: 500 retried → 200) =="
KEEL_DEMO_URL="$URL" KEEL_QUIET=1 node "$NODE_RUN" "$SCRIPT_DIR/app.mjs"
echo "   ✓ same code, now resilient"
