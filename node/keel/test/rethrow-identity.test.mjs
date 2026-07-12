// DX invariant 5: after final failure the ORIGINAL error object propagates
// unchanged — same identity, plus a non-enumerable keelOutcome attachment.
// We install the fetch wrapper over a fake original that rejects with a marked
// error, so identity is provable (the caught object carries our unique marker).

import test from "node:test";
import assert from "node:assert/strict";
import { AsyncEngine, virtualClock } from "../src/engine.mjs";
import { installFetch } from "../src/fetch.mjs";
import { level0Defaults } from "../src/defaults.mjs";

function withFakeFetch(throwing, fn) {
  const globalObj = { fetch: throwing };
  const backend = new AsyncEngine(virtualClock());
  backend.configure(level0Defaults());
  installFetch(backend, null, { globalObj });
  return fn(globalObj.fetch);
}

test("network error is retried to exhaustion then re-thrown unchanged (E010)", async () => {
  const marker = Symbol("original");
  const original = new TypeError("connection refused");
  original[marker] = true;
  let calls = 0;
  const throwing = async () => {
    calls++;
    throw original; // same object every attempt (conn class → retryable)
  };
  await withFakeFetch(throwing, async (wrapped) => {
    await assert.rejects(
      () => wrapped("http://api.example.com/"),
      (e) => {
        assert.equal(e, original, "must be the exact original error object");
        assert.equal(e[marker], true);
        assert.equal(e.keelOutcome.error.code, "KEEL-E010");
        assert.equal(e.keelOutcome.attempts, 3);
        return true;
      }
    );
    assert.equal(calls, 3, "conn errors retried up to the attempt budget");
  });
});

test("non-retryable error is re-thrown unchanged on first attempt (E015)", async () => {
  const original = new Error("teapot"); // name 'Error' → class 'other' → not in default retry.on
  let calls = 0;
  const throwing = async () => {
    calls++;
    throw original;
  };
  await withFakeFetch(throwing, async (wrapped) => {
    await assert.rejects(
      () => wrapped("http://api.example.com/"),
      (e) => {
        assert.equal(e, original);
        assert.equal(e.keelOutcome.error.code, "KEEL-E015");
        assert.equal(e.keelOutcome.attempts, 1);
        return true;
      }
    );
    assert.equal(calls, 1);
  });
});
