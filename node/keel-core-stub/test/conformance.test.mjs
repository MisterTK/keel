// Node harness for the shared conformance suite (conformance/scenarios/).
// The same scenarios run against the Rust and Python stubs; the real core
// must pass them too. Format and semantics: conformance/README.md.

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { KeelCoreStub, KeelError } from "../index.mjs";

const scenariosDir = new URL("../../../conformance/scenarios/", import.meta.url);

/** Subset match: objects require listed keys to match recursively; arrays
 *  must match exactly; scalars must be equal. Returns mismatch strings. */
function subsetMismatches(actual, expected, path = "$") {
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

function runScenario(scenario) {
  const core = new KeelCoreStub();
  const wantCfgErr = scenario.expect_configure_error;
  try {
    core.configure(scenario.policy);
    if (wantCfgErr) return [`configure: expected ${wantCfgErr}, but configure succeeded`];
  } catch (e) {
    if (!(e instanceof KeelError)) throw e;
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
    } else {
      failures.push(`${label}: unknown step ${JSON.stringify(Object.keys(step))}`);
    }
  });
  return failures;
}

for (const file of readdirSync(scenariosDir).filter((f) => f.endsWith(".json")).sort()) {
  const scenario = JSON.parse(readFileSync(new URL(file, scenariosDir), "utf8"));
  test(`conformance: ${scenario.name}`, () => {
    const failures = runScenario(scenario);
    assert.deepEqual(failures, [], `\n${failures.join("\n")}`);
  });
}
