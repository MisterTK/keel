#!/usr/bin/env bash
# Run the Node front end's startup-budget measurement and emit a deterministic
# CI artifact: target/bench-node-startup.json (sorted keys; timing VALUES are
# real measurements, not fixed, but the shape/keys never vary). DX invariant 8
# (Node half): `node --import keelrun/hook` adds <100ms to process startup at
# p50 — this is the repeatable-script counterpart to
# node/keel/test/startup-budget.test.mjs's CI assert, mirroring
# scripts/bench-overhead.sh's own "measure, then emit" shape.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="$repo_root/target/bench-node-startup.json"
mkdir -p "$repo_root/target"

node "$repo_root/node/keel/scripts/measure-startup.mjs" --json "$out"

echo "bench-node-startup: artifact at $out"
cat "$out"
