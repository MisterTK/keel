#!/usr/bin/env bash
# lro-poll — "the poll that looked adopted and did nothing" (#128 / #139).
# One app, two acts, identical code; the policies differ by one key.
# A RUNNING google.longrunning.Operation omits `done` entirely (proto3 JSON
# drops a false bool), so a poll block without `absent = "pending"` fails open
# on the missing field and returns the still-running body on attempt one.
# Deterministic: faultproxy serves 3 running bodies then the terminal one.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/../.." && pwd)"

PY="${KEEL_PYTHON:-}"
if [ -z "$PY" ]; then
  if [ -x "$REPO/.venv/bin/python3" ]; then PY="$REPO/.venv/bin/python3"; else PY="python3"; fi
fi
export PYTHONPATH="$REPO/python/keel/src:$REPO/python/keel-core-stub${PYTHONPATH:+:$PYTHONPATH}"
export PYTHONUNBUFFERED=1  # keep the app's stdout interleaved with Keel's stderr

PORT_FILE="$(mktemp)"
"$PY" "$REPO/tools/faultproxy/faultproxy.py" \
  --scenario "$SCRIPT_DIR/scenario.json" --port 0 --port-file "$PORT_FILE" >/dev/null 2>&1 &
FP_PID=$!
trap 'kill "$FP_PID" 2>/dev/null || true' EXIT
for _ in $(seq 1 50); do [ -s "$PORT_FILE" ] && break; sleep 0.1; done
PORT="$(cat "$PORT_FILE")"
URL="http://127.0.0.1:${PORT}/v1/projects/demo/locations/us-central1/operations/123"

count() { "$PY" -c "import json,urllib.request;print(len(json.load(urllib.request.urlopen('http://127.0.0.1:${PORT}/__faultproxy__/log'))))"; }
reset() { curl -s -X POST "http://127.0.0.1:${PORT}/__faultproxy__/reset" >/dev/null; }

echo "Same app.py, same faultproxy. The two policies differ by exactly this:"
diff -U0 "$SCRIPT_DIR/keel.without-absent.toml" "$SCRIPT_DIR/keel.with-absent.toml" \
  | sed -n '4,$p' | sed 's/^/   /' || true
echo

act() { # act <policy-file> <expect-ok|expect-fail>
  local work; work="$(mktemp -d)"
  cp "$SCRIPT_DIR/$1" "$work/keel.toml"
  reset
  local before; before="$(count)"
  set +e
  ( cd "$work" && KEEL_DEMO_URL="$URL" "$PY" -m keel run "$SCRIPT_DIR/app.py" )
  local rc=$?
  set -e
  local after; after="$(count)"
  echo "   upstream GETs this act: $((after - before))"
  if [ "$2" = "expect-fail" ]; then
    [ "$rc" -ne 0 ] && echo "   ✗ the poll handed back a still-running body as if terminal; the app broke on it" \
                    || echo "   (unexpected success)"
  else
    [ "$rc" -eq 0 ] && echo "   ✓ same code, one added key — polled to terminal" \
                    || echo "   (unexpected failure)"
  fi
}

echo "== 1) poll WITHOUT absent (a pre-CCR-11 block) =="
echo "   (expect a KeyError: that IS the defect — code that trusted the poll)"
act keel.without-absent.toml expect-fail

echo
echo "== 2) same code, absent = \"pending\" added =="
act keel.with-absent.toml expect-ok
