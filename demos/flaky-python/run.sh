#!/usr/bin/env bash
# flaky-python — the Level 0 hero. A bare httpx script dies on a transient 503;
# `keel run` retries it (zero code changes) and it survives. Deterministic:
# faultproxy serves 503 then 200, no real network.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Prefer the repo venv (has keel importable); else python3 + PYTHONPATH from src.
PY="${KEEL_PYTHON:-}"
if [ -z "$PY" ]; then
  if [ -x "$REPO/.venv/bin/python3" ]; then PY="$REPO/.venv/bin/python3"; else PY="python3"; fi
fi
export PYTHONPATH="$REPO/python/keel/src:$REPO/python/keel-core-stub${PYTHONPATH:+:$PYTHONPATH}"

PORT_FILE="$(mktemp)"
"$PY" "$REPO/tools/faultproxy/faultproxy.py" \
  --scenario "$SCRIPT_DIR/scenario.json" --port 0 --port-file "$PORT_FILE" >/dev/null 2>&1 &
FP_PID=$!
trap 'kill "$FP_PID" 2>/dev/null || true' EXIT
for _ in $(seq 1 50); do [ -s "$PORT_FILE" ] && break; sleep 0.1; done
PORT="$(cat "$PORT_FILE")"
URL="http://127.0.0.1:${PORT}/flaky"

echo "== 1) bare: python app.py (expect FAILURE on the 503) =="
if KEEL_DEMO_URL="$URL" "$PY" "$SCRIPT_DIR/app.py"; then
  echo "   (unexpected success)"; else echo "   ✗ died on the transient 503, as a bare script does"; fi

# Reset faultproxy so run 2 sees the same 503-then-200 sequence.
curl -s -X POST "http://127.0.0.1:${PORT}/__faultproxy__/reset" >/dev/null

echo "== 2) keel run app.py (expect SURVIVES: 503 retried → 200) =="
KEEL_DEMO_URL="$URL" KEEL_QUIET=1 "$PY" -m keel run "$SCRIPT_DIR/app.py"
echo "   ✓ same code, now resilient"
