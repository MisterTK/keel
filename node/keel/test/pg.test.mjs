// The `pg` pack. Deterministic in-process tests (virtual clock) cover
// retry/breaker/target derivation/reversibility against a plain JS fake
// conforming to fixtures/pg-client.d.ts (the real `pg` package is never a
// dependency of Keel or of this test).

import test from "node:test";
import assert from "node:assert/strict";
import { AsyncEngine, virtualClock, realClock } from "../src/engine.mjs";
import { applyPackDefaults } from "../src/defaults.mjs";
import { makeWrappedQuery, patchClientQuery, classifyPgError, pgPack } from "../src/packs/pg.mjs";

function clientWith(original, backend, host = "db.example.com") {
  return {
    connectionParameters: { host },
    query: makeWrappedQuery(original, { backend }),
  };
}

test("classifyPgError maps timeout/conn/other", () => {
  assert.equal(classifyPgError(Object.assign(new Error(), { name: "KeelTimeoutError" })), "timeout");
  assert.equal(classifyPgError(Object.assign(new Error("x"), { code: "ECONNREFUSED" })), "conn");
  assert.equal(classifyPgError(Object.assign(new Error("x"), { code: "08006" })), "conn"); // connection exception
  assert.equal(classifyPgError(Object.assign(new Error("x"), { code: "57P01" })), "conn"); // admin shutdown
  assert.equal(classifyPgError(Object.assign(new Error("x"), { code: "57014" })), "timeout"); // query_canceled
  assert.equal(classifyPgError(Object.assign(new Error("x"), { code: "53300" })), "conn"); // too_many_connections
  assert.equal(classifyPgError(Object.assign(new Error("x"), { code: "42601" })), "other"); // syntax error
  assert.equal(classifyPgError(new Error("boom")), "other");
});

test("pgPack implements the adapter-pack four operations", () => {
  const p = pgPack({ cwd: "/nonexistent-project" });
  assert.deepEqual(p.detect(), { matched: false }); // pg absent in this repo
  const seams = p.seams();
  assert.equal(seams[0].patchPoint, "Client.prototype.query");
  assert.match(seams[0].whyStable, /Pool\.query/);
  assert.equal(p.targets()[0].kind, "host");
  assert.deepEqual(p.defaults(), {}); // host targets inherit [defaults.outbound]
});

test("pg: target is the connection host; SELECT is idempotent, everything else is not; never cached", async () => {
  const captured = [];
  const backend = {
    kind: "fake",
    configure() {},
    layer: () => undefined,
    async execute(req, effect) {
      captured.push(req);
      const r = await effect();
      return {
        v: 1,
        result: "ok",
        payload: r.payload,
        attempts: 1,
        from_cache: false,
        waits_ms: [],
        throttled: false,
        throttle_wait_ms: 0,
        breaker: "closed",
        trace_id: "t",
      };
    },
  };
  const client = clientWith(async () => ({ rows: [] }), backend, "primary.db.internal");
  await client.query("SELECT * FROM users WHERE id = $1", [1]);
  await client.query("INSERT INTO users (name) VALUES ($1)", ["ana"]);
  assert.equal(captured[0].target, "primary.db.internal");
  assert.equal(captured[0].idempotent, true, "SELECT is idempotent");
  assert.equal(captured[1].idempotent, false, "INSERT is not idempotent");
  assert.equal(captured[0].args_hash, null);
  assert.equal(captured[1].args_hash, null);
});

test("pg: a SUBMITTABLE argument (pg-cursor/pg-query-stream/raw Query) is forwarded untouched", async () => {
  const backend = {
    execute() {
      throw new Error("must not be called for a submittable");
    },
  };
  const submittable = { submit() {}, tag: "cursor" };
  const original = (arg) => arg; // real pg returns the submittable unchanged
  const client = clientWith(original, backend);
  const result = await client.query(submittable);
  assert.equal(result, submittable, "the exact submittable instance is returned, untouched");
});

test("pg: a callback invocation is forwarded untouched (never wrapped)", async () => {
  const backend = {
    execute() {
      throw new Error("must not be called for a callback-style invocation");
    },
  };
  let calledWith;
  const original = (text, values, cb) => {
    calledWith = { text, values };
    cb(null, { rows: [] });
    return undefined; // real pg returns undefined when a callback is given
  };
  const client = clientWith(original, backend);
  let cbResult;
  const ret = client.query("SELECT 1", [], (err, res) => {
    cbResult = { err, res };
  });
  assert.equal(ret, undefined);
  assert.deepEqual(calledWith, { text: "SELECT 1", values: [] });
  assert.deepEqual(cbResult, { err: null, res: { rows: [] } });
});

test("pg: retries a conn error then succeeds (counters)", async () => {
  const backend = new AsyncEngine(virtualClock());
  backend.configure({
    target: { "db.example.com": { retry: { attempts: 3, schedule: "fixed(1ms)", on: ["conn"] } } },
  });
  let n = 0;
  const original = async () => {
    if (++n === 1) throw Object.assign(new Error("connection reset"), { code: "ECONNRESET" });
    return { rows: [{ id: 1 }] };
  };
  const client = clientWith(original, backend);
  const res = await client.query("SELECT * FROM t");
  assert.deepEqual(res, { rows: [{ id: 1 }] });
  assert.equal(n, 2);
  assert.deepStrictEqual(backend.report().targets["db.example.com"], {
    attempts: 2,
    breaker_opens: 0,
    breaker_state: "closed",
    cache_hits: 0,
    calls: 1,
    failures: 0,
    retries: 1,
    successes: 1,
    throttled: 0,
  });
});

test("pg: a non-idempotent (INSERT) failure is observed, not retried (KEEL-E014); original error propagates", async () => {
  const backend = new AsyncEngine(virtualClock());
  backend.configure({
    target: { "db.example.com": { retry: { attempts: 3, schedule: "fixed(1ms)", on: ["conn"] } } },
  });
  let n = 0;
  const reset = Object.assign(new Error("connection reset"), { code: "ECONNRESET" });
  const original = async () => {
    n++;
    throw reset;
  };
  const client = clientWith(original, backend);
  let caught;
  try {
    await client.query("INSERT INTO t (x) VALUES (1)");
  } catch (e) {
    caught = e;
  }
  assert.equal(n, 1, "a non-idempotent INSERT is NOT retried, even with retry configured");
  assert.equal(caught, reset, "original error propagates unchanged (identity)");
  assert.equal(caught.keelOutcome.error.code, "KEEL-E014");
});

test("pg: breaker opens after repeated failures then fails fast (KEEL-E012)", async () => {
  const backend = new AsyncEngine(virtualClock());
  backend.configure({
    target: {
      "db.example.com": { retry: { attempts: 1, on: ["conn"] }, breaker: { failures: 2, cooldown: "30s" } },
    },
  });
  let attempts = 0;
  const original = async () => {
    attempts++;
    throw Object.assign(new Error("refused"), { code: "ECONNREFUSED" });
  };
  const client = clientWith(original, backend);
  await assert.rejects(() => client.query("SELECT 1"));
  await assert.rejects(() => client.query("SELECT 1"));
  await assert.rejects(
    () => client.query("SELECT 1"),
    (e) => {
      assert.equal(e.keelOutcome.error.code, "KEEL-E012");
      return true;
    }
  );
  assert.equal(attempts, 2, "third call fails fast — the driver is not touched");
});

test("patchClientQuery patches and reverses; no backend → pass-through", async () => {
  class FakeClient {
    constructor() {
      this.connectionParameters = { host: "db.example.com" };
    }
    async query(text) {
      return { rows: [], echo: text };
    }
  }
  const uninstall = patchClientQuery(FakeClient); // global backend is null → pass-through
  assert.equal(FakeClient.prototype.query.__keelWrapped, true);
  const r = await new FakeClient().query("SELECT 1");
  assert.deepEqual(r, { rows: [], echo: "SELECT 1" }, "pass-through when no backend is active");
  const noop = patchClientQuery(FakeClient);
  noop();
  assert.equal(FakeClient.prototype.query.__keelWrapped, true, "no-op patch did not disturb the wrap");
  uninstall();
  assert.equal(FakeClient.prototype.query.__keelWrapped, undefined, "uninstall restored the original");
});

test("pg: a hung query soft-times-out per policy; the abandoned attempt's later rejection never leaks", async () => {
  const backend = new AsyncEngine(realClock());
  backend.configure({
    target: { "db.example.com": { timeout: "15ms", retry: { attempts: 1, on: ["timeout"] } } },
  });
  const original = () =>
    new Promise((_resolve, reject) => {
      setTimeout(() => reject(Object.assign(new Error("late server reply"), { code: "57014" })), 80);
    });
  const client = clientWith(original, backend);
  let caught;
  try {
    await client.query("SELECT pg_sleep(10)");
  } catch (e) {
    caught = e;
  }
  assert.ok(caught, "a soft timeout must reject, not hang forever");
  assert.equal(caught.keelOutcome.error.class, "timeout");
  // Let the abandoned attempt's real (late) rejection settle; if the pack
  // failed to guard it, this would surface as an unhandledRejection and fail
  // the whole test file.
  await new Promise((resolve) => setTimeout(resolve, 100));
});

test("pg: defaults.outbound apply when no target-specific policy is set", async () => {
  const backend = new AsyncEngine(virtualClock());
  backend.configure(applyPackDefaults({})); // db.example.com inherits defaults.outbound
  let n = 0;
  const original = async () => {
    if (++n === 1) throw Object.assign(new Error("connection reset"), { code: "ECONNRESET" });
    return { rows: [] };
  };
  const client = clientWith(original, backend);
  const res = await client.query("SELECT 1");
  assert.deepEqual(res, { rows: [] });
  assert.equal(n, 2, "retried per defaults.outbound (conn is in the default retry.on)");
});
