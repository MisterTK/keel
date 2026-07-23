// Shared conformance driver for the Node cores (conformance/scenarios/).
//
// The same scenarios run against every keel-core implementation. This module
// owns the scenario format and execution semantics (conformance/README.md) so
// that BOTH the pure-JS stub test and the native (napi addon) test drive them
// identically — the only difference is the injected core object. Each core must
// expose the synchronous surface: `configure(policy)`, `execute(request,
// effect)`, `report()`, and the harness-only `advanceClock(ms)`.
//
// It lives beside the packages (not under a `test/` dir) so Node's test runner
// does not auto-discover it as an empty test file.

import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";

export const scenariosDir = new URL("../../conformance/scenarios/", import.meta.url);

/** Subset match: objects require listed keys to match recursively; arrays
 *  must match exactly; scalars must be equal. Returns mismatch strings. */
export function subsetMismatches(actual, expected, path = "$") {
  if (expected !== null && typeof expected === "object" && !Array.isArray(expected)) {
    if (actual === null || typeof actual !== "object" || Array.isArray(actual))
      return [`${path}: expected object, got ${JSON.stringify(actual)}`];
    const out = [];
    for (const [k, v] of Object.entries(expected)) {
      if (!(k in actual)) out.push(`${path}.${k}: missing (expected ${JSON.stringify(v)})`);
      else out.push(...subsetMismatches(actual[k], v, `${path}.${k}`));
    }
    return out;
  }
  if (Array.isArray(expected)) {
    if (!Array.isArray(actual) || actual.length !== expected.length)
      return [`${path}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`];
    return expected.flatMap((e, i) => subsetMismatches(actual[i], e, `${path}[${i}]`));
  }
  return Object.is(actual, expected) || actual === expected
    ? []
    : [`${path}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`];
}

/** Load every scenario JSON, sorted by filename (stable order). */
export function loadScenarios() {
  return readdirSync(scenariosDir)
    .filter((f) => f.endsWith(".json"))
    .sort()
    .map((file) => JSON.parse(readFileSync(new URL(file, scenariosDir), "utf8")));
}

/**
 * Drive one scenario against a freshly-built `core`. `isKeelError(e)` decides
 * whether a thrown value is a policy error (with a stable `.code`) versus an
 * unexpected failure to propagate. Returns a list of human-readable mismatches
 * (empty ⇒ pass).
 */
export function runScenario(core, isKeelError, scenario) {
  // Tier 2 (durable flows) is real-core only; the Node cores skip it cleanly.
  if ((scenario.tier ?? 1) !== 1) return [];
  const wantCfgErr = scenario.expect_configure_error;
  try {
    core.configure(scenario.policy);
    if (wantCfgErr) return [`configure: expected ${wantCfgErr}, but configure succeeded`];
  } catch (e) {
    if (!isKeelError(e)) throw e;
    if (!wantCfgErr) return [`configure: unexpected error ${e.message}`];
    return e.code === wantCfgErr ? [] : [`configure: expected ${wantCfgErr}, got ${e.code}`];
  }

  const failures = [];
  scenario.steps.forEach((step, i) => {
    const label = `step[${i}]`;
    if ("advance_ms" in step) {
      core.advanceClock(step.advance_ms);
    } else if ("report_expect" in step) {
      failures.push(
        ...subsetMismatches(core.report(), step.report_expect).map((m) => `${label} report: ${m}`)
      );
    } else if ("call" in step) {
      const call = step.call;
      const request = { v: 1, target: call.target, op: call.target, ...(call.request ?? {}) };
      const script = call.effect ?? [];
      let consumed = 0;
      const outcome = core.execute(request, (attempt) => {
        assert.ok(
          consumed < script.length,
          `${label}: effect script exhausted (attempt ${attempt}, scripted ${script.length})`
        );
        return script[consumed++];
      });
      if (consumed !== script.length)
        failures.push(
          `${label}: effect script not fully consumed (${consumed}/${script.length} attempts used)`
        );
      failures.push(
        ...subsetMismatches(outcome, call.expect ?? {}).map((m) => `${label} outcome: ${m}`)
      );
    } else if ("resolve" in step) {
      const r = step.resolve;
      const got = core.resolveTarget(r.method, r.host, r.scheme ?? null, r.port ?? null, r.path ?? null);
      if (got !== step.expect)
        failures.push(`${label}: resolve got ${JSON.stringify(got)}, want ${JSON.stringify(step.expect)}`);
    } else if ("layer" in step) {
      const l = step.layer;
      // The Node stub returns `undefined` (not `null`) for an unset layer key;
      // scenario JSON's `"expect": null` parses to JS `null` via JSON.parse.
      // Normalize before comparing or `layer` steps expecting `null` spuriously
      // fail (`undefined !== null`). `expect` can also be an object (e.g. an
      // idempotency table), so compare deeply rather than with `!==`.
      const got = core.layer(l.target, l.key) ?? null;
      try {
        assert.deepStrictEqual(got, step.expect);
      } catch {
        failures.push(`${label}: layer got ${JSON.stringify(got)}, want ${JSON.stringify(step.expect)}`);
      }
    } else {
      failures.push(`${label}: unknown step ${JSON.stringify(Object.keys(step))}`);
    }
  });
  return failures;
}

/**
 * Register one `node:test` case per scenario. `makeCore()` builds a fresh core
 * on a virtual clock at 0 (stub natively; native via the harness-only
 * `{ paused: true }` flag), so the configure/execute/report/advance loop is
 * identical for both.
 */
export function registerConformance(test, { label, makeCore, isKeelError }) {
  for (const scenario of loadScenarios()) {
    // Tier 2 (durable flows) is real-core only; the Node cores don't implement
    // it, so mark those scenarios SKIPPED (visible in the run, like the Python
    // runner's "N tier-2 skipped") rather than trivially passing.
    if ((scenario.tier ?? 1) !== 1) {
      test(
        `${label} conformance: ${scenario.name} (tier ${scenario.tier})`,
        { skip: "tier 2 durable flows: real-core only" },
        () => {}
      );
      continue;
    }
    test(`${label} conformance: ${scenario.name}`, () => {
      const failures = runScenario(makeCore(), isKeelError, scenario);
      assert.deepEqual(failures, [], `\n${failures.join("\n")}`);
    });
  }
}
