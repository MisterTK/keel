// Breaker rate mode (window + failure_rate + min_calls), parity with the real
// core's `BreakerPolicy` (crates/keel-core-api/src/policy.rs): a rate-mode
// knob without both `window` and `failure_rate` present (and without
// `failures`) is KEEL-E001 at configure time, not a silent degrade to count
// mode on its defaults.

import test from "node:test";
import assert from "node:assert/strict";
import { KeelCoreStub, KeelError } from "../index.mjs";

const rejects = (breaker) => {
  const core = new KeelCoreStub();
  try {
    core.configure({ target: { x: { breaker } } });
  } catch (e) {
    assert.ok(e instanceof KeelError, `expected KeelError, got ${e}`);
    assert.equal(e.code, "KEEL-E001");
    return;
  }
  assert.fail(`expected configure to reject ${JSON.stringify(breaker)}`);
};

test("window alone is half-configured", () => {
  rejects({ window: "30s" });
});

test("failure_rate alone is half-configured", () => {
  rejects({ failure_rate: 0.5 });
});

test("min_calls alone is half-configured", () => {
  rejects({ min_calls: 4 });
});

test("out-of-range failure_rate is rejected", () => {
  for (const rate of [0, -0.1, 1.1, 2]) rejects({ window: "30s", failure_rate: rate });
});

test("non-positive min_calls is rejected", () => {
  rejects({ window: "30s", failure_rate: 0.5, min_calls: 0 });
});

test("both rate knobs together selects rate mode", () => {
  const core = new KeelCoreStub();
  core.configure({ target: { x: { breaker: { window: "30s", failure_rate: 0.5, min_calls: 4 } } } });
});

test("failures alongside rate knobs is still valid count mode", () => {
  // Frozen schema precedence: "Setting `failures` selects count mode" — the
  // rate knobs are inert, not rejected.
  const core = new KeelCoreStub();
  core.configure({
    target: { x: { breaker: { failures: 3, window: "30s", failure_rate: 0.5 } } },
  });
});
