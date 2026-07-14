#!/usr/bin/env node
/**
 * DX invariant 8 (Node half): `keel run` (`node --import keelrun/hook`) adds
 * <100ms to process startup at p50, measured in CI. Mirrors
 * python/keel/tests/test_run.py's `StartupBudgetTest` methodology exactly, so
 * both front ends report the same shape of number the same way: the MIN of a
 * few runs of the real wall-clock delta between a plain `node noop.mjs` launch
 * and `node --import keelrun/hook noop.mjs` (min, not mean/median — the most
 * stable estimator of pure fixed overhead against scheduler/GC noise).
 *
 * Two ways to use this module:
 *   - imported by test/startup-budget.test.mjs, which asserts the CI budget;
 *   - run directly (`node scripts/measure-startup.mjs [--json <path>]`) for a
 *     local/manual measurement, printing the same `[startup budget]` line
 *     scripts/bench-overhead.sh's Rust twin prints, and optionally emitting a
 *     JSON artifact (mirrors scripts/bench-overhead.sh's two-phase pattern:
 *     the same measurement, one emits JSON).
 *
 * The generous 250ms budget (not the 100ms target) mirrors the Python test's
 * own reasoning verbatim: shared CI runners are noisy; the printed number, not
 * a tight assert, is the actual signal a human reads.
 */

import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
export const NOOP_PATH = join(HERE, "..", "fixtures", "noop.mjs");
export const HOOK_URL = new URL("../hook.mjs", import.meta.url).href;

export const TARGET_MS = 100; // DX invariant 8's own number.
export const BUDGET_MS = 250; // generous CI-noise-tolerant assert (Python parity).

/** Best (minimum) wall-clock ms of `runs` launches of `cmd`. A non-zero exit
 *  is a hard failure — a crashed child would make a budget assert vacuously
 *  pass, exactly the trap python/keel/tests/test_run.py's own comment names. */
function minMs(cmd, { env, cwd, runs = 5 } = {}) {
  let best = Infinity;
  for (let i = 0; i < runs; i++) {
    const started = performance.now();
    const proc = spawnSync(cmd[0], cmd.slice(1), { env, cwd, encoding: "utf8" });
    if (proc.status !== 0) {
      throw new Error(
        `startup-budget child exited ${proc.status} for ${JSON.stringify(cmd)}: ` +
          `${(proc.stderr ?? "").slice(0, 400)}`
      );
    }
    best = Math.min(best, performance.now() - started);
  }
  return best;
}

/**
 * Measure baseline vs. `--import keelrun/hook` startup, on the stub backend
 * always, and the native backend when the built addon is present (skipped
 * otherwise — a worktree without `cargo build -p keel-node --release` has no
 * addon to measure, exactly like the Python leg skips without a built wheel).
 */
export async function measureStartupBudget({ runs = 5 } = {}) {
  const dir = mkdtempSync(join(tmpdir(), "keel-startup-budget-"));
  try {
    const baseEnv = { ...process.env, KEEL_QUIET: "1" };
    const baselineMs = minMs([process.execPath, NOOP_PATH], { env: process.env, cwd: dir, runs });
    const stubMs = minMs([process.execPath, "--import", HOOK_URL, NOOP_PATH], {
      env: { ...baseEnv, KEEL_BACKEND: "stub" },
      cwd: dir,
      runs,
    });
    let nativeAddedMs = null;
    let nativeLoaded = false;
    try {
      ({ loaded: nativeLoaded } = await import("../../keel-core-native/index.mjs"));
    } catch {
      /* keel-core-native has no package.json export outside the workspace checkout — treat as absent */
    }
    if (nativeLoaded) {
      const nativeMs = minMs([process.execPath, "--import", HOOK_URL, NOOP_PATH], {
        env: { ...baseEnv, KEEL_BACKEND: "native" },
        cwd: dir,
        runs,
      });
      nativeAddedMs = nativeMs - baselineMs;
    }
    return { baselineMs, stubAddedMs: stubMs - baselineMs, nativeAddedMs, budgetMs: BUDGET_MS, targetMs: TARGET_MS };
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

export function formatSummary({ baselineMs, stubAddedMs, nativeAddedMs, targetMs, budgetMs }) {
  const nativeStr = nativeAddedMs == null ? "native (skipped: no addon)" : `native +${nativeAddedMs.toFixed(1)} ms`;
  return (
    `[startup budget] baseline ${baselineMs.toFixed(1)} ms | ` +
    `stub +${stubAddedMs.toFixed(1)} ms | ${nativeStr} ` +
    `(target <${targetMs} ms, budget <${budgetMs} ms)`
  );
}

async function main() {
  const args = process.argv.slice(2);
  const jsonIdx = args.indexOf("--json");
  const jsonPath = jsonIdx >= 0 ? args[jsonIdx + 1] : null;
  const result = await measureStartupBudget();
  process.stderr.write(formatSummary(result) + "\n");
  if (jsonPath) {
    writeFileSync(jsonPath, `${JSON.stringify(result, Object.keys(result).sort(), 2)}\n`);
    process.stderr.write(`measure-startup: artifact at ${jsonPath}\n`);
  }
}

if (import.meta.url === `file://${process.argv[1]}`) {
  main().catch((e) => {
    process.stderr.write(`measure-startup failed: ${e.message}\n`);
    process.exitCode = 1;
  });
}
