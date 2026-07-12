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
cargo bench -p keel-core --bench overhead

# (2) Deterministic artifact. KEEL_BENCH_EMIT_JSON short-circuits criterion in
#     the bench binary's main; KEEL_BENCH_OUT pins the absolute output path
#     (the bench binary's CWD is the crate dir, not the workspace root).
KEEL_BENCH_EMIT_JSON=1 KEEL_BENCH_OUT="$out" \
  cargo bench -p keel-core --bench overhead

echo "bench-overhead: artifact at $out"
cat "$out"
