#!/usr/bin/env bash
# durable-pipeline — the demo that sells the product (dx-spec §1 Level 2).
# A 10-step flow is `kill -9`'d before step 6; re-running resumes from the
# journal: steps 1-5 are substituted (no duplicate log lines), 6-10 run live.
# Requires the native core (Tier 2 is native-only).
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/../.." && pwd)"

PY="${KEEL_PYTHON:-}"
if [ -z "$PY" ]; then
  if [ -x "$REPO/.venv/bin/python3" ]; then PY="$REPO/.venv/bin/python3"; else PY="python3"; fi
fi
export PYTHONPATH="$REPO/python/keel/src:$REPO/python/keel-core-stub${PYTHONPATH:+:$PYTHONPATH}"
if ! "$PY" -c "import keel_core" 2>/dev/null; then
  echo "durable flows need the native core. Build it: maturin develop -m crates/keel-py"; exit 1
fi

WORK="$(mktemp -d)"
cp "$SCRIPT_DIR/pipeline.py" "$SCRIPT_DIR/keel.toml" "$WORK/"
cd "$WORK"
LOG="$WORK/effects.log"
export KEEL_DEMO_LOG="$LOG" KEEL_FLOW_LEASE_MS=800 KEEL_QUIET=1 KEEL_BACKEND=native

echo "== run 1: crash (kill -9) right before step 6 =="
set +e
KEEL_DEMO_CRASH_AT=6 "$PY" -m keel run pipeline.py
echo "   exit: $? (SIGKILL)  |  steps fired: $(wc -l < "$LOG" | tr -d ' ')  (expect 5)"
set -e

echo "   waiting for the crashed run's lease to expire..."
sleep 1.2

echo "== run 2: resume — steps 1-5 substituted, 6-10 run live =="
"$PY" -m keel run pipeline.py
echo "   steps fired across BOTH runs: $(wc -l < "$LOG" | tr -d ' ')  (expect 10 — each exactly once)"

if [ -x "$REPO/target/debug/keel" ]; then
  echo "== keel flows =="
  "$REPO/target/debug/keel" flows
fi
