// Async bridge test for the napi addon (sprint risk item: tokio↔libuv).
//
// Exercises `KeelCore.executeAsync` end to end on a real (non-paused) engine
// with a RETRYING scenario: an `async (attempt) => object` effect fails the
// first (idempotent) attempt with a retryable `conn` error, the engine backs off
// on real time, and the second attempt succeeds — proving the returned Promise
// resolves with the outcome and the async effect coroutine is genuinely awaited
// on the caller's libuv loop. Mirrors crates/keel-py/tests/test_async_bridge.py.
//
// Auto-skips when the addon binary is absent (build: `cargo build -p keel-node
// --release`).

import test from "node:test";
import assert from "node:assert/strict";
import { KeelCore, loaded } from "../index.mjs";

const gate = loaded
  ? {}
  : { skip: "keel-core-native binary absent — build with `cargo build -p keel-node --release`" };

test("async bridge: retry round-trip resolves via libuv", gate, async () => {
  const core = new KeelCore(); // non-paused: real (tiny) backoff
  core.configure({
    defaults: {
      outbound: {
        timeout: "5s",
        // fixed(1ms) keeps the real backoff tiny but nonzero.
        retry: { attempts: 3, schedule: "fixed(1ms)", on: ["conn", "5xx"] },
      },
    },
  });

  const seen = [];
  const effect = async (attempt) => {
    seen.push(attempt);
    // Yield to the loop to prove we are genuinely awaited on it.
    await new Promise((resolve) => setTimeout(resolve, 0));
    if (attempt === 1) return { status: "error", class: "conn", message: "connection reset" };
    return { status: "ok", payload: { attempt } };
  };

  const outcome = await core.executeAsync(
    { v: 1, target: "api.example.com", op: "GET api.example.com/items", idempotent: true, args_hash: "h1" },
    effect
  );

  assert.equal(outcome.result, "ok", JSON.stringify(outcome));
  assert.equal(outcome.attempts, 2, JSON.stringify(outcome));
  assert.deepEqual(outcome.payload, { attempt: 2 }, JSON.stringify(outcome));
  assert.equal(outcome.from_cache, false);
  assert.deepEqual(outcome.waits_ms, [1], JSON.stringify(outcome));
  assert.deepEqual(seen, [1, 2]);
});

test("async bridge: non-idempotent failure is not retried (KEEL-E014)", gate, async () => {
  const core = new KeelCore();
  core.configure({ defaults: { outbound: { retry: { attempts: 3, on: ["conn"] } } } });

  let calls = 0;
  const effect = async () => {
    calls += 1;
    return { status: "error", class: "conn", message: "reset" };
  };

  const outcome = await core.executeAsync(
    { v: 1, target: "api.example.com", op: "POST x", idempotent: false },
    effect
  );

  assert.equal(outcome.result, "error", JSON.stringify(outcome));
  assert.equal(outcome.error.code, "KEEL-E014", JSON.stringify(outcome));
  assert.equal(outcome.attempts, 1);
  assert.equal(calls, 1);
});
