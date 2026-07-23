#!/usr/bin/env node
/**
 * Dedicated micro-bench for the `resolveTarget`/`layer` FFI crossing (issue
 * #50): `scripts/bench-overhead.sh`'s `overhead` criterion bench measures
 * `Engine::execute`'s in-process path only (per its own docstring, "the same
 * shape `keel-ffi`'s `keel_execute` drives") — it never crosses into napi's
 * actual method-dispatch/argument-marshalling path, which now runs once per
 * outbound HTTP call (SP-1's `backend.resolveTarget(...)`/`.layer(...)`,
 * called from `fetch.mjs`'s wrapped `fetch`). This script drives the REAL
 * native-addon call path a Node caller uses, so what's measured is the
 * actual FFI-crossing cost, not an inference from a different (in-process)
 * benchmark.
 *
 * Skips gracefully — `nativeLoaded: false`, no measurement — when the addon
 * isn't built locally (`cargo build -p keel-node --release`), mirroring
 * measure-startup.mjs's own native-addon check exactly.
 *
 * Methodology mirrors crates/keel-core/benches/support.rs's `median_ns`
 * exactly: 101 samples of 256-call batches, median of the batch means, so
 * the two languages' numbers are directly comparable.
 *
 * Two ways to use this module:
 *   - run directly (`node scripts/measure-resolve-target-ffi.mjs [--json
 *     <path>]`) for a local/manual measurement, printing a
 *     `[resolve_target ffi]` line and optionally emitting a JSON artifact
 *     (mirrors scripts/bench-overhead.sh's two-phase pattern);
 *   - imported (`measureResolveTargetFfi()`) by anything that wants the raw
 *     result object.
 */

import { writeFileSync } from "node:fs";

const INNER = 256;
const SAMPLES = 101;

/** Median nanoseconds per invocation of `op`, over `SAMPLES` batches of
 *  `INNER` calls each — the same median-of-batch-means estimator as the
 *  Rust bench's `median_ns` / the Python twin's `median_ns`. */
function medianNs(op) {
  for (let i = 0; i < INNER; i++) op();
  const samples = [];
  for (let s = 0; s < SAMPLES; s++) {
    const start = process.hrtime.bigint();
    for (let i = 0; i < INNER; i++) op();
    const elapsedNs = process.hrtime.bigint() - start;
    samples.push(Number(elapsedNs / BigInt(INNER)));
  }
  samples.sort((a, b) => a - b);
  return samples[Math.floor(SAMPLES / 2)];
}

/** `{nativeLoaded, resolveTargetNs, layerNs}` — the latter two `null` when
 *  the native addon isn't built (nothing to measure). */
export async function measureResolveTargetFfi() {
  let KeelCore;
  let loaded = false;
  try {
    ({ KeelCore, loaded } = await import("../../keel-core-native/index.mjs"));
  } catch {
    loaded = false; // keel-core-native has no package.json export outside the workspace checkout
  }
  if (!loaded) {
    return { nativeLoaded: false, resolveTargetNs: null, layerNs: null };
  }
  const core = new KeelCore();
  const resolveTargetNs = medianNs(() => core.resolveTarget("GET", "api.openai.com"));
  const layerNs = medianNs(() => core.layer("llm:openai", "retry"));
  return { nativeLoaded: true, resolveTargetNs, layerNs };
}

export function formatSummary({ nativeLoaded, resolveTargetNs, layerNs }) {
  if (!nativeLoaded) return "[resolve_target ffi] node native (skipped: no addon)";
  return `[resolve_target ffi] node resolveTarget ${resolveTargetNs} ns | layer ${layerNs} ns`;
}

async function main() {
  const args = process.argv.slice(2);
  const jsonIdx = args.indexOf("--json");
  const jsonPath = jsonIdx >= 0 ? args[jsonIdx + 1] : null;
  const result = await measureResolveTargetFfi();
  process.stderr.write(formatSummary(result) + "\n");
  if (jsonPath) {
    writeFileSync(jsonPath, `${JSON.stringify(result, Object.keys(result).sort(), 2)}\n`);
    process.stderr.write(`measure-resolve-target-ffi: artifact at ${jsonPath}\n`);
  }
}

if (import.meta.url === `file://${process.argv[1]}`) {
  main().catch((e) => {
    process.stderr.write(`measure-resolve-target-ffi failed: ${e.message}\n`);
    process.exitCode = 1;
  });
}
