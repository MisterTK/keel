// upTo/andThen schedule composition (contracts/schedule-grammar.ebnf),
// semantics normatively pinned in conformance/README.md ("Schedule algebra").
// Mirrors crates/keel-core-api/src/policy.rs's Schedule tests and the Python
// stub's ScheduleCompositionTest exactly (parity rule).

import test from "node:test";
import assert from "node:assert/strict";
import { KeelCoreStub, KeelError } from "../index.mjs";

const rejects = (schedule) => {
  const core = new KeelCoreStub();
  try {
    core.configure({ target: { x: { retry: { schedule } } } });
  } catch (e) {
    assert.ok(e instanceof KeelError, `expected KeelError, got ${e}`);
    assert.equal(e.code, "KEEL-E001");
    return;
  }
  assert.fail(`expected ${JSON.stringify(schedule)} to be rejected`);
};

const waits = (schedule, attempts) => {
  const core = new KeelCoreStub();
  core.configure({ target: { x: { retry: { attempts, schedule, on: ["timeout"] } } } });
  const out = core.execute(
    { v: 1, target: "x", op: "x", idempotent: true },
    () => ({ status: "error", class: "timeout" }),
  );
  assert.equal(out.attempts, attempts);
  return out.waits_ms;
};

test("the spec's own composed example parses", () => {
  new KeelCoreStub().configure({
    target: { x: { retry: { schedule: "exp(1s, x2, max 5m) upTo 10m andThen fixed(1m)" } } },
  });
});

test("extra whitespace around upTo/andThen still parses", () => {
  new KeelCoreStub().configure({
    target: {
      x: {
        retry: {
          schedule: "exp(1s, x2, max 5m)  upTo  10m  andThen  fixed(1m)",
        },
      },
    },
  });
});

test("hands off when the next natural wait would overshoot the bound", () => {
  // 1s + 2s = 3s fits; the natural 4s would overshoot the 4s bound.
  assert.deepEqual(waits("exp(1s, x2) upTo 4s andThen fixed(500ms)", 6), [
    1000, 2000, 500, 500, 500,
  ]);
});

test("an exact fit stays in the segment; a segment whose bound is below its first wait cascades", () => {
  // Three 1s waits fill `upTo 3s` exactly; `fixed(10s) upTo 5s`'s own first
  // wait already exceeds its bound, so it contributes zero waits and the
  // handoff cascades straight to the fixed(250ms) tail.
  assert.deepEqual(
    waits("fixed(1s) upTo 3s andThen fixed(10s) upTo 5s andThen fixed(250ms)", 7),
    [1000, 1000, 1000, 250, 250, 250],
  );
});

test("exp restarts at local attempt 1 after a handoff", () => {
  // attempts=6 so all 5 waits (through the 6th, terminal attempt) show up.
  assert.deepEqual(waits("fixed(1s) upTo 2s andThen exp(100ms, x3)", 6), [
    1000, 1000, 100, 300, 900,
  ]);
});

test("shape rule: upTo must bound every segment except the last, and never the last", () => {
  rejects("fixed(1s) andThen fixed(2s)"); // non-final unbounded: never hands off
  rejects("exp(1s, x2, max 5m) upTo 10m"); // final bounded: attempts past it have no wait
  rejects("fixed(1s) upTo 3s andThen fixed(2s) andThen fixed(4s)");
  rejects("fixed(1s) upTo 3s andThen fixed(2s) upTo 5s");
});

test("broken composition syntax is a plain parse rejection", () => {
  rejects("fixed(1s) upTo");
  rejects("upTo 3s andThen fixed(1s)");
  rejects("fixed(1s) upTo 1s upTo 2s andThen fixed(1s)");
  rejects("fixed(1s) andThen");
  rejects("andThen fixed(1s)");
  rejects("fixed(1s) upTo 3s fixed(2s)");
});
