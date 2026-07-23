#!/usr/bin/env bash
# Run the keel-core per-call overhead benchmark and emit a deterministic CI
# artifact: target/bench-overhead.json = {p50_ns per case}, sorted keys, no
# timestamps. NFR2 / DX invariant 8 (≤10µs per wrapped call) made measurable.
#
# Two phases: (1) the full criterion statistical run (human-facing report +
# regression detection, release-optimized `bench` profile); (2) a deterministic
# re-run of the same four cases in emit-JSON mode that writes the artifact.
#
# CI wiring (invoking this + uploading the artifact) is Task 17; this script's
# only job is to produce the file. `cargo` is expected on PATH.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

out="$repo_root/target/bench-overhead.json"
mkdir -p "$repo_root/target"

# (1) Statistical benchmark. The ≤10µs claim is read from this report.
cargo bench -p keelrun-core --bench overhead

# (2) Deterministic artifact. KEEL_BENCH_EMIT_JSON short-circuits criterion in
#     the bench binary's main; KEEL_BENCH_OUT pins the absolute output path
#     (the bench binary's CWD is the crate dir, not the workspace root).
KEEL_BENCH_EMIT_JSON=1 KEEL_BENCH_OUT="$out" \
  cargo bench -p keelrun-core --bench overhead

echo "bench-overhead: artifact at $out"
cat "$out"

# (3) The resolve_target/layer FFI-crossing micro-bench (issue #50): (1)/(2)
#     above measure Engine::execute's in-process path only (per support.rs's
#     own docstring) — never the PyO3/napi call-dispatch/argument-marshalling
#     path that now runs once per outbound HTTP call (SP-1's
#     `_runtime.get_backend().resolve_target(...)`/`.layer(...)`). Each
#     language's script gracefully reports "skipped: no wheel/addon" when its
#     native module isn't built locally (this script never builds one),
#     mirroring bench-node-startup.sh's own addon-optional convention; `cat`
#     below still shows the artifact either way. `python3`/`node` are
#     expected on PATH, same as every other script in this repo.
ffi_out="$repo_root/target/bench-resolve-target-ffi.json"
py_ffi_tmp="$(mktemp)"
node_ffi_tmp="$(mktemp)"
python3 "$repo_root/python/keel/scripts/measure_resolve_target_ffi.py" --json "$py_ffi_tmp"
node "$repo_root/node/keel/scripts/measure-resolve-target-ffi.mjs" --json "$node_ffi_tmp"
python3 -c '
import json, sys
py_path, node_path, out_path = sys.argv[1:]
combined = {"python": json.load(open(py_path)), "node": json.load(open(node_path))}
with open(out_path, "w") as f:
    json.dump(combined, f, sort_keys=True, indent=2)
    f.write("\n")
' "$py_ffi_tmp" "$node_ffi_tmp" "$ffi_out"
rm -f "$py_ffi_tmp" "$node_ffi_tmp"

echo "bench-overhead: FFI-crossing artifact at $ffi_out"
cat "$ffi_out"
