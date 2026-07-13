// The `ioredis` pack. Deterministic in-process tests (virtual clock) cover
// retry/breaker/target derivation/reversibility against a plain JS fake
// conforming to fixtures/ioredis-client.d.ts (the real `ioredis` package is
// never a dependency of Keel or of this test).

import test from "node:test";
import assert from "node:assert/strict";
import { AsyncEngine, virtualClock, realClock } from "../src/engine.mjs";
import { applyPackDefaults } from "../src/defaults.mjs";
import {
  makeWrappedSendCommand,
  patchSendCommand,
  classifyRedisError,
  isIdempotentRedisCommand,
  ioredisPack,
} from "../src/packs/ioredis.mjs";

/** A plain JS fake of ioredis's documented `Command` shape
 *  (fixtures/ioredis-client.d.ts): a name, args, and a promise that settles
 *  exactly once via `resolve`/`reject`. */
class FakeCommand {
  constructor(name, args = []) {
    this.name = name;
    this.args = args;
    this.promise = new Promise((resolve, reject) => {
      this.resolve = resolve;
      this.reject = reject;
    });
  }
}

/** A scripted `sendCommand` original: settles each dispatched command per a
 *  script of outcomes (one per physical dispatch — the pack calls this once
 *  per attempt, with the ORIGINAL command on attempt 1 and a fresh clone on
 *  every retry). */
function scripted(outcomes) {
  let n = 0;
  return function original(command) {
    const outcome = outcomes[Math.min(n, outcomes.length - 1)];
    n++;
    if (Object.hasOwn(outcome, "ok")) command.resolve(outcome.ok);
    else command.reject(outcome.err);
  };
}

function clientWith(original, backend, host = "cache.example.com") {
  return {
    options: { host },
    sendCommand: makeWrappedSendCommand(original, { backend }),
  };
}

test("classifyRedisError maps timeout/conn/other", () => {
  assert.equal(classifyRedisError(Object.assign(new Error(), { name: "KeelTimeoutError" })), "timeout");
  assert.equal(classifyRedisError(Object.assign(new Error("x"), { code: "ECONNREFUSED" })), "conn");
  assert.equal(classifyRedisError(Object.assign(new Error("x"), { name: "MaxRetriesPerRequestError" })), "conn");
  assert.equal(classifyRedisError(new Error("Connection is closed.")), "conn");
  assert.equal(classifyRedisError(new Error("WRONGTYPE Operation against a key")), "other");
});

test("isIdempotentRedisCommand: reads retry; mutations, pub/sub, transactions, and unknowns do not", () => {
  for (const c of ["get", "GET", "mget", "exists", "ttl", "pttl", "hgetall", "smembers", "scan", "ping"])
    assert.equal(isIdempotentRedisCommand(c), true, c);
  for (const c of ["set", "del", "incr", "expire", "eval", "subscribe", "multi", "exec", "flushall", "frobnicate", ""])
    assert.equal(isIdempotentRedisCommand(c), false, c);
});

test("ioredisPack implements the adapter-pack four operations", () => {
  const p = ioredisPack({ cwd: "/nonexistent-project" });
  assert.deepEqual(p.detect(), { matched: false }); // ioredis absent in this repo
  const seams = p.seams();
  assert.equal(seams[0].patchPoint, "Redis.prototype.sendCommand");
  assert.equal(p.targets()[0].kind, "host");
  assert.deepEqual(p.defaults(), {}); // host targets inherit [defaults.outbound]
});

test("ioredis: target is the connection host; GET is idempotent, SET is not; never cached", async () => {
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
  const client = clientWith(scripted([{ ok: "v" }, { ok: "OK" }]), backend, "redis.internal");
  await client.sendCommand(new FakeCommand("get", ["k"]));
  await client.sendCommand(new FakeCommand("set", ["k", "v"]));
  assert.equal(captured[0].target, "redis.internal");
  assert.equal(captured[0].idempotent, true, "GET is idempotent");
  assert.equal(captured[1].idempotent, false, "SET is not idempotent");
  assert.equal(captured[0].args_hash, null);
  assert.equal(captured[1].args_hash, null);
});

test("ioredis: a non-plain command shape is forwarded untouched", async () => {
  const backend = {
    execute() {
      throw new Error("must not be called for a non-plain command");
    },
  };
  let seen;
  const original = (cmd, stream) => {
    seen = { cmd, stream };
  };
  const client = clientWith(original, backend);
  const weird = { not: "a command" };
  const ret = client.sendCommand(weird, "stream-marker");
  assert.equal(ret, undefined, "the original's return value is forwarded as-is");
  assert.deepEqual(seen, { cmd: weird, stream: "stream-marker" });
});

test("ioredis: retries a conn error then succeeds (counters)", async () => {
  const backend = new AsyncEngine(virtualClock());
  backend.configure({
    target: { "cache.example.com": { retry: { attempts: 3, schedule: "fixed(1ms)", on: ["conn"] } } },
  });
  const original = scripted([
    { err: Object.assign(new Error("read ECONNRESET"), { code: "ECONNRESET" }) },
    { ok: "PONG" },
  ]);
  const client = clientWith(original, backend);
  const res = await client.sendCommand(new FakeCommand("get", ["k"]));
  assert.equal(res, "PONG");
  assert.deepStrictEqual(backend.report().targets["cache.example.com"], {
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

test("ioredis: a non-idempotent (SET) failure is observed, not retried (KEEL-E014); original error propagates", async () => {
  const backend = new AsyncEngine(virtualClock());
  backend.configure({
    target: { "cache.example.com": { retry: { attempts: 3, schedule: "fixed(1ms)", on: ["conn"] } } },
  });
  const reset = Object.assign(new Error("read ECONNRESET"), { code: "ECONNRESET" });
  let calls = 0;
  const original = (cmd) => {
    calls++;
    cmd.reject(reset);
  };
  const client = clientWith(original, backend);
  let caught;
  try {
    await client.sendCommand(new FakeCommand("set", ["k", "v"]));
  } catch (e) {
    caught = e;
  }
  assert.equal(calls, 1, "a non-idempotent SET is NOT retried, even with retry configured");
  assert.equal(caught, reset, "original error propagates unchanged (identity)");
  assert.equal(caught.keelOutcome.error.code, "KEEL-E014");
});

test("ioredis: breaker opens after repeated failures then fails fast (KEEL-E012)", async () => {
  const backend = new AsyncEngine(virtualClock());
  backend.configure({
    target: {
      "cache.example.com": { retry: { attempts: 1, on: ["conn"] }, breaker: { failures: 2, cooldown: "30s" } },
    },
  });
  let attempts = 0;
  const original = (cmd) => {
    attempts++;
    cmd.reject(Object.assign(new Error("refused"), { code: "ECONNREFUSED" }));
  };
  const client = clientWith(original, backend);
  await assert.rejects(() => client.sendCommand(new FakeCommand("get", ["k"])));
  await assert.rejects(() => client.sendCommand(new FakeCommand("get", ["k"])));
  await assert.rejects(
    () => client.sendCommand(new FakeCommand("get", ["k"])),
    (e) => {
      assert.equal(e.keelOutcome.error.code, "KEEL-E012");
      return true;
    }
  );
  assert.equal(attempts, 2, "third call fails fast — the driver is not touched");
});

test("ioredis: a pre-attached completion (simulating a user callback) fires with the FIRST attempt only; the returned promise reflects the final retried outcome", async () => {
  const backend = new AsyncEngine(virtualClock());
  backend.configure({
    target: { "cache.example.com": { retry: { attempts: 3, schedule: "fixed(1ms)", on: ["conn"] } } },
  });
  const original = scripted([
    { err: Object.assign(new Error("read ECONNRESET"), { code: "ECONNRESET" }) },
    { ok: "PONG" },
  ]);
  const client = clientWith(original, backend);
  const cmd = new FakeCommand("get", ["k"]);
  let firstOutcome;
  cmd.promise.then(
    (v) => {
      firstOutcome = { ok: v };
    },
    (e) => {
      firstOutcome = { err: e };
    }
  );
  const result = await client.sendCommand(cmd);
  assert.equal(result, "PONG", "the returned (Keel-owned) promise reflects the retried final success");
  await Promise.resolve(); // let the pre-attached handler's microtask run
  assert.ok(firstOutcome.err, "a completion attached to the original command fired with the FIRST attempt's failure");
});

test("patchSendCommand patches and reverses; no backend → pass-through", async () => {
  class FakeRedis {
    constructor() {
      this.options = { host: "cache.example.com" };
    }
    sendCommand(cmd) {
      cmd.resolve(`echo:${cmd.name}`);
      return cmd.promise;
    }
  }
  const uninstall = patchSendCommand(FakeRedis); // global backend is null → pass-through
  assert.equal(FakeRedis.prototype.sendCommand.__keelWrapped, true);
  const r = await new FakeRedis().sendCommand(new FakeCommand("ping", []));
  assert.equal(r, "echo:ping", "pass-through when no backend is active");
  const noop = patchSendCommand(FakeRedis);
  noop();
  assert.equal(FakeRedis.prototype.sendCommand.__keelWrapped, true, "no-op patch did not disturb the wrap");
  uninstall();
  assert.equal(FakeRedis.prototype.sendCommand.__keelWrapped, undefined, "uninstall restored the original");
});

test("ioredis: a hung command soft-times-out per policy; the abandoned attempt's later settlement never leaks", async () => {
  const backend = new AsyncEngine(realClock());
  backend.configure({
    target: { "cache.example.com": { timeout: "15ms", retry: { attempts: 1, on: ["timeout"] } } },
  });
  const original = (cmd) => {
    setTimeout(() => cmd.reject(Object.assign(new Error("late reply"), { code: "ECONNRESET" })), 80);
  };
  const client = clientWith(original, backend);
  let caught;
  try {
    await client.sendCommand(new FakeCommand("get", ["k"]));
  } catch (e) {
    caught = e;
  }
  assert.ok(caught, "a soft timeout must reject, not hang forever");
  assert.equal(caught.keelOutcome.error.class, "timeout");
  await new Promise((resolve) => setTimeout(resolve, 100));
});

test("ioredis: defaults.outbound apply when no target-specific policy is set", async () => {
  const backend = new AsyncEngine(virtualClock());
  backend.configure(applyPackDefaults({})); // cache.example.com inherits defaults.outbound
  const original = scripted([
    { err: Object.assign(new Error("read ECONNRESET"), { code: "ECONNRESET" }) },
    { ok: "v" },
  ]);
  const client = clientWith(original, backend);
  const res = await client.sendCommand(new FakeCommand("get", ["k"]));
  assert.equal(res, "v", "retried per defaults.outbound (conn is in the default retry.on)");
});
