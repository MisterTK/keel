#!/usr/bin/env bash
# adk-demo — a real google-adk LlmAgent, one FunctionTool, and a fake, flaky
# completion endpoint. Keel retries the tool's inner httpx.get call BELOW the
# agent loop: 3 upstream calls happen, but the agent itself takes exactly ONE
# turn to invoke the tool — zero extra LLM tokens burned riding out the storm.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/../.." && pwd)"

PY="${KEEL_PYTHON:-}"
if [ -z "$PY" ]; then
  if [ -x "$REPO/.venv/bin/python3" ]; then PY="$REPO/.venv/bin/python3"; else PY="python3"; fi
fi
export PYTHONPATH="$REPO/python/keel/src:$REPO/python/keel-core-stub${PYTHONPATH:+:$PYTHONPATH}"

if ! "$PY" -c "import google.adk" 2>/dev/null; then
  echo "adk-demo needs google-adk (pip install google-adk) — skipping."
  exit 0
fi

PORT_FILE="$(mktemp)"
"$PY" "$REPO/tools/faultproxy/faultproxy.py" \
  --scenario "$SCRIPT_DIR/scenario.json" --port 0 --port-file "$PORT_FILE" >/dev/null 2>&1 &
FP_PID=$!
trap 'kill "$FP_PID" 2>/dev/null || true' EXIT
for _ in $(seq 1 50); do [ -s "$PORT_FILE" ] && break; sleep 0.1; done
PORT="$(cat "$PORT_FILE")"
URL="http://127.0.0.1:${PORT}/v1/complete"

count() { "$PY" -c "import json,urllib.request;print(len(json.load(urllib.request.urlopen('http://127.0.0.1:${PORT}/__faultproxy__/log'))))"; }

WORK="$(mktemp -d)"
cp "$SCRIPT_DIR/keel.toml" "$WORK/keel.toml"
cd "$WORK"

echo "== agent turn: a real ADK LlmAgent rides out a 429 storm below its own loop =="
KEEL_DEMO_URL="$URL" KEEL_QUIET=1 "$PY" -m keel run "$SCRIPT_DIR/agent.py"

CALLS="$(count)"
echo "   upstream calls: ${CALLS} (2x429 + 1x200 — all absorbed inside ONE agent turn)"
if [ "$CALLS" != "3" ]; then
  echo "adk-demo: expected exactly 3 upstream calls, got ${CALLS}" >&2
  exit 1
fi
