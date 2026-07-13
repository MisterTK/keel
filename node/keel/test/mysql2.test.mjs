// The `mysql2` pack. Deterministic in-process tests (virtual clock) cover
// retry/breaker/target derivation/reversibility against a plain JS fake
// conforming to fixtures/mysql2-connection.d.ts (the real `mysql2` package is
// never a dependency of Keel or of this test).

import test from "node:test";
import assert from "node:assert/strict";
import { AsyncEngine, virtualClock, realClock } from "../src/engine.mjs";
import { applyPackDefaults } from "../src/defaults.mjs";
import { makeWrappedQuery, patchConnectionQuery, classifyMysql2Error, mysql2Pack } from "../src/packs/mysql2.mjs";

/** A scripted callback-style `query`/`execute` original: finds the trailing
 *  callback and invokes it per a script of outcomes, one per physical
 *  dispatch (the pack calls this once per attempt). */
function scripted(outcomes) {
  let n = 0;
  return function original(...args) {
    const cb = args[args.length - 1];
    const outcome = outcomes[Math.min(n, outcomes.length - 1)];
    n++;
    if (Object.hasOwn(outcome, "ok")) cb(null, outcome.ok, outcome.fields ?? []);
    else cb(outcome.err);
    return { on: () => {} }; // real mysql2 returns a Query/Execute emitter
  };
}

function connectionWith(original, backend, host = "app-db") {
  return {
    config: { host },
    query: makeWrappedQuery(original, { backend }),
    execute: makeWrappedQuery(original, { backend }),
  };
}

test("classifyMysql2Error maps timeout/conn/other", () => {
  assert.equal(classifyMysql2Error(Object.assign(new Error(), { name: "KeelTimeoutError" })), "timeout");
  assert.equal(classifyMysql2Error(Object.assign(new Error("x"), { code: "ECONNREFUSED" })), "conn");
  assert.equal(classifyMysql2Error(Object.assign(new Error("x"), { code: "PROTOCOL_CONNECTION_LOST" })), "conn");
  assert.equal(classifyMysql2Error(Object.assign(new Error("x"), { code: "ER_LOCK_WAIT_TIMEOUT" })), "timeout");
  assert.equal(classifyMysql2Error(Object.assign(new Error("x"), { code: "ER_PARSE_ERROR" })), "other");
  assert.equal(classifyMysql2Error(new Error("boom")), "other");
});

test("mysql2Pack implements the adapter-pack four operations", () => {
  const p = mysql2Pack({ cwd: "/nonexistent-project" });
  assert.deepEqual(p.detect(), { matched: false }); // mysql2 absent in this repo
  const seams = p.seams();
  assert.equal(seams[0].patchPoint, "Connection.prototype.query / Connection.prototype.execute");
  assert.match(seams[0].whyStable, /promise/);
  assert.equal(p.targets()[0].kind, "host");
  assert.deepEqual(p.defaults(), {}); // host targets inherit [defaults.outbound]
});

test("mysql2: target is the connection host; SELECT is idempotent, INSERT is not; never cached", async () => {
  const captured = [];
  const backend = {
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
  const conn = connectionWith(scripted([{ ok: [{ id: 1 }] }, { ok: { affectedRows: 1 } }]), backend, "app-db");
  await new Promise((resolve, reject) => conn.query("SELECT * FROM t", (e, r) => (e ? reject(e) : resolve(r))));
  await new Promise((resolve, reject) =>
    conn.query("INSERT INTO t (x) VALUES (1)", (e, r) => (e ? reject(e) : resolve(r)))
  );
  assert.equal(captured[0].target, "app-db");
  assert.equal(captured[0].idempotent, true, "SELECT is idempotent");
  assert.equal(captured[1].idempotent, false, "INSERT is not idempotent");
  assert.equal(captured[0].args_hash, null);
  assert.equal(captured[1].args_hash, null);
});

test("mysql2: a callback-less (streaming) call is forwarded untouched", async () => {
  const backend = {
    execute() {
      throw new Error("must not be called for a streaming call");
    },
  };
  const emitter = { on: () => emitter, tag: "stream" };
  const original = () => emitter;
  const conn = connectionWith(original, backend);
  const ret = conn.query("SELECT * FROM big_table");
  assert.equal(ret, emitter, "the real streaming Query object is returned unchanged");
});

test("mysql2: a wrapped call returns undefined (documented deviation); the callback fires exactly once", async () => {
  const backend = new AsyncEngine(virtualClock());
  backend.configure({ target: { "app-db": { retry: { attempts: 1 } } } });
  const original = scripted([{ ok: [{ id: 1 }] }]);
  const conn = connectionWith(original, backend);
  let calls = 0;
  let seen;
  const ret = conn.query("SELECT 1", (err, results) => {
    calls++;
    seen = { err, results };
  });
  assert.equal(ret, undefined);
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(calls, 1);
  assert.deepEqual(seen, { err: null, results: [{ id: 1 }] });
});

test("mysql2: retries a conn error then succeeds (counters), for both query() and execute()", async () => {
  for (const method of ["query", "execute"]) {
    const backend = new AsyncEngine(virtualClock());
    backend.configure({
      target: { "app-db": { retry: { attempts: 3, schedule: "fixed(1ms)", on: ["conn"] } } },
    });
    const original = scripted([
      { err: Object.assign(new Error("connection lost"), { code: "PROTOCOL_CONNECTION_LOST" }) },
      { ok: [{ id: 1 }] },
    ]);
    const conn = connectionWith(original, backend);
    const result = await new Promise((resolve, reject) =>
      conn[method]("SELECT * FROM t WHERE id = ?", [1], (e, r) => (e ? reject(e) : resolve(r)))
    );
    assert.deepEqual(result, [{ id: 1 }], method);
    assert.deepStrictEqual(backend.report().targets["app-db"], {
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
  }
});

test("mysql2: a non-idempotent (INSERT) failure is observed, not retried (KEEL-E014); original error propagates", async () => {
  const backend = new AsyncEngine(virtualClock());
  backend.configure({
    target: { "app-db": { retry: { attempts: 3, schedule: "fixed(1ms)", on: ["conn"] } } },
  });
  const lost = Object.assign(new Error("connection lost"), { code: "PROTOCOL_CONNECTION_LOST" });
  let calls = 0;
  const original = (...args) => {
    calls++;
    args[args.length - 1](lost);
    return { on: () => {} };
  };
  const conn = connectionWith(original, backend);
  let caught;
  try {
    await new Promise((resolve, reject) =>
      conn.query("INSERT INTO t (x) VALUES (1)", (e, r) => (e ? reject(e) : resolve(r)))
    );
  } catch (e) {
    caught = e;
  }
  assert.equal(calls, 1, "a non-idempotent INSERT is NOT retried, even with retry configured");
  assert.equal(caught, lost, "original error propagates unchanged (identity)");
  assert.equal(caught.keelOutcome.error.code, "KEEL-E014");
});

test("mysql2: breaker opens after repeated failures then fails fast (KEEL-E012)", async () => {
  const backend = new AsyncEngine(virtualClock());
  backend.configure({
    target: { "app-db": { retry: { attempts: 1, on: ["conn"] }, breaker: { failures: 2, cooldown: "30s" } } },
  });
  let attempts = 0;
  const original = (...args) => {
    attempts++;
    args[args.length - 1](Object.assign(new Error("refused"), { code: "ECONNREFUSED" }));
    return { on: () => {} };
  };
  const conn = connectionWith(original, backend);
  const run = () => new Promise((resolve, reject) => conn.query("SELECT 1", (e, r) => (e ? reject(e) : resolve(r))));
  await assert.rejects(run);
  await assert.rejects(run);
  await assert.rejects(run, (e) => {
    assert.equal(e.keelOutcome.error.code, "KEEL-E012");
    return true;
  });
  assert.equal(attempts, 2, "third call fails fast — the driver is not touched");
});

test("patchConnectionQuery patches and reverses both query and execute; no backend → pass-through", async () => {
  class FakeConnection {
    constructor() {
      this.config = { host: "app-db" };
    }
    query(sql, cb) {
      cb(null, [{ echo: sql }]);
      return { on: () => {} };
    }
    execute(sql, cb) {
      cb(null, [{ echo: sql }]);
      return { on: () => {} };
    }
  }
  const uninstall = patchConnectionQuery(FakeConnection); // global backend is null → pass-through
  assert.equal(FakeConnection.prototype.query.__keelWrapped, true);
  assert.equal(FakeConnection.prototype.execute.__keelWrapped, true);
  const conn = new FakeConnection();
  const r = await new Promise((resolve, reject) => conn.query("SELECT 1", (e, v) => (e ? reject(e) : resolve(v))));
  assert.deepEqual(r, [{ echo: "SELECT 1" }], "pass-through when no backend is active");
  const noop = patchConnectionQuery(FakeConnection);
  noop();
  assert.equal(FakeConnection.prototype.query.__keelWrapped, true, "no-op patch did not disturb the wrap");
  uninstall();
  assert.equal(FakeConnection.prototype.query.__keelWrapped, undefined, "uninstall restored query");
  assert.equal(FakeConnection.prototype.execute.__keelWrapped, undefined, "uninstall restored execute");
});

test("mysql2: a hung query soft-times-out per policy; the abandoned attempt's later callback never leaks", async () => {
  const backend = new AsyncEngine(realClock());
  backend.configure({
    target: { "app-db": { timeout: "15ms", retry: { attempts: 1, on: ["timeout"] } } },
  });
  const original = (...args) => {
    const cb = args[args.length - 1];
    setTimeout(() => cb(Object.assign(new Error("late reply"), { code: "ER_LOCK_WAIT_TIMEOUT" })), 80);
    return { on: () => {} };
  };
  const conn = connectionWith(original, backend);
  let caught;
  try {
    await new Promise((resolve, reject) =>
      conn.query("SELECT SLEEP(10)", (e, r) => (e ? reject(e) : resolve(r)))
    );
  } catch (e) {
    caught = e;
  }
  assert.ok(caught, "a soft timeout must reject, not hang forever");
  assert.equal(caught.keelOutcome.error.class, "timeout");
  await new Promise((resolve) => setTimeout(resolve, 100));
});

test("mysql2: defaults.outbound apply when no target-specific policy is set", async () => {
  const backend = new AsyncEngine(virtualClock());
  backend.configure(applyPackDefaults({})); // app-db inherits defaults.outbound
  const original = scripted([
    { err: Object.assign(new Error("connection lost"), { code: "PROTOCOL_CONNECTION_LOST" }) },
    { ok: [{ id: 1 }] },
  ]);
  const conn = connectionWith(original, backend);
  const res = await new Promise((resolve, reject) =>
    conn.query("SELECT 1", (e, r) => (e ? reject(e) : resolve(r)))
  );
  assert.deepEqual(res, [{ id: 1 }], "retried per defaults.outbound (conn is in the default retry.on)");
});

test("mysql2: query text from a {sql, values} options object is classified correctly", async () => {
  const captured = [];
  const backend = {
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
  const conn = connectionWith(scripted([{ ok: [] }]), backend);
  await new Promise((resolve, reject) =>
    conn.query({ sql: "SELECT * FROM t WHERE id = ?", values: [1] }, (e, r) => (e ? reject(e) : resolve(r)))
  );
  assert.equal(captured[0].idempotent, true);
});
