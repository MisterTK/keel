#!/usr/bin/env bash
# agent-demo — a fake, flaky LLM endpoint. Keel rides out a 429 storm and, off
# KEEL_ENV=prod, dev-caches the completion so the SECOND run replays from the
# journal with ~0 API calls. Cross-run replay needs the native core.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/../.." && pwd)"

PY="${KEEL_PYTHON:-}"
if [ -z "$PY" ]; then
  if [ -x "$REPO/.venv/bin/python3" ]; then PY="$REPO/.venv/bin/python3"; else PY="python3"; fi
fi
export PYTHONPATH="$REPO/python/keel/src:$REPO/python/keel-core-stub${PYTHONPATH:+:$PYTHONPATH}"

if "$PY" -c "import keel_core" 2>/dev/null; then
  echo "native core present → cross-run dev-cache replay will make run 2 ~0 calls"
else
  echo "note: native core not built → run 2 will call again (build: maturin develop -m crates/keel-py)"
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

echo "== run 1 (KEEL_ENV=dev): rides the 429 storm, then caches the completion =="
KEEL_DEMO_URL="$URL" KEEL_ENV="" KEEL_QUIET=1 "$PY" -m keel run "$SCRIPT_DIR/agent.py"
echo "   upstream calls so far: $(count)"

echo "== run 2 (same prompt): served from the dev cache =="
KEEL_DEMO_URL="$URL" KEEL_ENV="" KEEL_QUIET=1 "$PY" -m keel run "$SCRIPT_DIR/agent.py"
echo "   upstream calls total: $(count)   (unchanged ⇒ run 2 was free)"
