// DX invariant 8 (Node half): `keel run` (`node --import keelrun/hook`) adds
// <100ms to process startup at p50, measured in CI. Mirrors
// python/keel/tests/test_run.py's StartupBudgetTest exactly: minimum of a few
// runs (the stable estimator against scheduler/GC noise), asserted against a
// generous 250ms CI budget (not the 100ms target — shared runners are noisy;
// the printed number is the real signal a human reads, same reasoning as the
// Python test and the `startup-budget` CI job's own comment).
//
// Skippable on slow/loaded CI via KEEL_SKIP_STARTUP_BUDGET=1 (per the task
// brief) — spawns 5-10 child processes, which can be too slow/flaky on a
// heavily oversubscribed shared runner; the measurement is a NON-REQUIRED
// signal (mirrors the `startup-budget` GitHub Actions job already being
// non-required for the same reason).

import test from "node:test";
import assert from "node:assert/strict";
import { measureStartupBudget, formatSummary } from "../scripts/measure-startup.mjs";

const skip = ["1", "true", "yes"].includes(String(process.env.KEEL_SKIP_STARTUP_BUDGET ?? "").toLowerCase())
  ? { skip: "KEEL_SKIP_STARTUP_BUDGET set" }
  : {};

test("node --import keelrun/hook stays under the startup budget", skip, async () => {
  const result = await measureStartupBudget();
  process.stderr.write(`${formatSummary(result)}\n`);
  assert.ok(
    result.stubAddedMs < result.budgetMs,
    `stub startup budget exceeded: +${result.stubAddedMs.toFixed(1)} ms (budget <${result.budgetMs} ms)`
  );
  if (result.nativeAddedMs != null) {
    assert.ok(
      result.nativeAddedMs < result.budgetMs,
      `native startup budget exceeded: +${result.nativeAddedMs.toFixed(1)} ms (budget <${result.budgetMs} ms)`
    );
  }
});
