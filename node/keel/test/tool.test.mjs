// The tool: semantic target pack (dx-spec §4.1) and its wrap API. The
// load-bearing judgment is Level 0's hard rule: a tool call is NON-idempotent
// by default — observed, never retried (KEEL-E014) — with retry an explicit
// wrap-site opt-in (idempotent: true). Deterministic in-process tests against
// the AsyncEngine on a virtual clock (no real sleeps); defaults inheritance is
// asserted against the composed Level 0 policy (applyPackDefaults): with no
// [defaults.tool] in the frozen pack, tool: inherits [defaults.outbound].

import test from "node:test";
import assert from "node:assert/strict";
import { AsyncEngine, virtualClock, KeelError } from "../src/engine.mjs";
import { applyPackDefaults } from "../src/defaults.mjs";
import {
  classifyToolError,
  isValidToolName,
  toolPack,
  toolTarget,
  wrapTool,
} from "../src/packs/tool.mjs";

function engine(policy) {
  const backend = new AsyncEngine(virtualClock());
  backend.configure(policy);
  return backend;
}

test("tool name grammar matches the frozen targetKey; invalid names throw E001", () => {
  for (const name of ["web_search", "a", "0day", "_x", "a.b-c_d", "Search2"]) {
    assert.equal(isValidToolName(name), true, name);
    assert.equal(toolTarget(name), `tool:${name}`);
  }
  for (const name of ["", "get weather", "-lead", ".dot", "a/b", "a:b", "täl", null, 7]) {
    assert.equal(isValidToolName(name), false, String(name));
  }
  assert.throws(
    () => wrapTool("get weather", () => null),
    (e) => e instanceof KeelError && e.code === "KEEL-E001"
  );
  assert.throws(
    () => wrapTool("ok_name", "not a function"),
    (e) => e instanceof KeelError && e.code === "KEEL-E001"
  );
});

test("classifyToolError maps timeout/cancelled/conn/other", () => {
  assert.equal(classifyToolError(Object.assign(new Error(), { name: "TimeoutError" })), "timeout");
  // Caller cancellation (AbortController) is `cancelled`, not retryable: an
  // aborted tool call ends immediately (KEEL-E015 path).
  assert.equal(classifyToolError(Object.assign(new Error(), { name: "AbortError" })), "cancelled");
  assert.equal(classifyToolError(Object.assign(new Error("x"), { code: "ECONNRESET" })), "conn");
  assert.equal(classifyToolError(Object.assign(new Error("x"), { code: "ENOTFOUND" })), "conn");
  assert.equal(classifyToolError(new TypeError("bad tool args")), "other");
  assert.equal(classifyToolError(undefined), "other");
});

test("toolPack implements the adapter-pack four operations", () => {
  assert.deepEqual(toolPack.detect(), { matched: true, name: "tool", confidence: "pinned" });
  assert.deepEqual(toolPack.seams(), []); // the seam belongs to the framework pack
  const [decl] = toolPack.targets();
  assert.equal(decl.pattern, "tool:<name>");
  assert.equal(decl.kind, "tool");
  assert.match(decl.idempotencyRule, /non-idempotent by default/);
  // No [defaults.tool] in the frozen pack: tool: inherits [defaults.outbound].
  assert.deepEqual(toolPack.defaults(), {});
});

test("tool: default is non-idempotent — conn error observed, not retried (KEEL-E014)", async () => {
  const backend = engine({
    target: { "tool:charge_card": { retry: { attempts: 3, schedule: "fixed(1ms)", on: ["conn"] } } },
  });
  let n = 0;
  const reset = Object.assign(new Error("connection reset"), { code: "ECONNRESET" });
  const charge = wrapTool(
    "charge_card",
    async () => {
      n++;
      throw reset;
    },
    { backend }
  );
  let caught;
  try {
    await charge();
  } catch (e) {
    caught = e;
  }
  assert.equal(n, 1, "a side-effecting tool is NOT retried, even with retry configured");
  assert.equal(caught, reset, "original error propagates unchanged");
  assert.equal(caught.keelOutcome.error.code, "KEEL-E014");
});

test("tool: a plain tool bug is class other → propagates unchanged (KEEL-E015)", async () => {
  const backend = engine({
    target: { "tool:charge_card": { retry: { attempts: 3, schedule: "fixed(1ms)", on: ["conn"] } } },
  });
  const bug = new TypeError("bad tool args");
  const charge = wrapTool(
    "charge_card",
    async () => {
      throw bug;
    },
    { backend }
  );
  let caught;
  try {
    await charge();
  } catch (e) {
    caught = e;
  }
  assert.equal(caught, bug);
  assert.equal(caught.keelOutcome.error.code, "KEEL-E015");
});

test("tool: idempotent opt-in inherits defaults.outbound and retries conn", async () => {
  // Composed Level 0 policy: no [defaults.tool] exists, so tool:lookup
  // resolves to [defaults.outbound] (retry on conn). Waits are virtual.
  const backend = engine(applyPackDefaults({}));
  let n = 0;
  const lookup = wrapTool(
    "lookup",
    async () => {
      if (++n === 1) throw Object.assign(new Error("reset"), { code: "ECONNRESET" });
      return "found";
    },
    { idempotent: true, backend }
  );
  assert.equal(await lookup(), "found");
  assert.equal(n, 2, "retried per defaults.outbound");
  const t = backend.report().targets["tool:lookup"];
  assert.equal(t.retries, 1);
  assert.equal(t.successes, 1);
});

test("tool: success returns the live object (identity), outcome attached non-enumerably", async () => {
  const backend = engine(applyPackDefaults({}));
  const sentinel = { nested: [1, 2] };
  const read = wrapTool("read_cfg", async () => sentinel, { idempotent: true, backend });
  const out = await read();
  assert.equal(out, sentinel, "live value delivered by identity");
  assert.equal(out.keelOutcome.result, "ok");
  assert.deepEqual(Object.keys(out), ["nested"], "attachment is non-enumerable");
});

test("tool: idempotent + explicit cache ttl replays identical args from cache", async () => {
  const backend = engine({ target: { "tool:lookup": { cache: { ttl: "60s" } } } });
  let n = 0;
  const lookup = wrapTool("lookup", async (key) => ({ key, n: ++n }), { idempotent: true, backend });
  const first = await lookup("k");
  const second = await lookup("k");
  assert.equal(n, 1, "identical args replay from cache");
  assert.deepEqual(second, { key: "k", n: 1 });
  assert.deepEqual(first, { key: "k", n: 1 });
  assert.equal((await lookup("other")).n, 2, "different args miss the cache");
});

test("tool: non-idempotent is never cached, even with cache configured", async () => {
  const backend = engine({ target: { "tool:send_mail": { cache: { ttl: "60s" } } } });
  let n = 0;
  const send = wrapTool("send_mail", async () => `sent-${++n}`, { backend });
  assert.equal(await send(), "sent-1");
  assert.equal(await send(), "sent-2");
  assert.equal(n, 2, "side-effecting tool executed every call");
});

test("tool: breaker fast-fail synthesizes KeelError (KEEL-E012, no side-band original)", async () => {
  const backend = engine({
    target: {
      "tool:lookup": {
        retry: { attempts: 1, schedule: "fixed(1ms)", on: ["conn"] },
        breaker: { failures: 2, cooldown: "30s" },
      },
    },
  });
  let n = 0;
  const lookup = wrapTool(
    "lookup",
    async () => {
      n++;
      throw Object.assign(new Error("refused"), { code: "ECONNREFUSED" });
    },
    { idempotent: true, backend }
  );
  await assert.rejects(() => lookup());
  await assert.rejects(() => lookup());
  await assert.rejects(
    () => lookup(),
    (e) => {
      assert.ok(e instanceof KeelError);
      assert.equal(e.code, "KEEL-E012");
      assert.equal(e.keelOutcome.error.code, "KEEL-E012");
      return true;
    }
  );
  assert.equal(n, 2, "open breaker fails fast — the tool is not touched");
});

test("tool: discovery observes each call under tool:<name>", async () => {
  const backend = engine(applyPackDefaults({}));
  const seen = [];
  const discovery = { observe: (target, outcome) => seen.push([target, outcome.result]) };
  const ok = wrapTool("ok", async () => 1, { idempotent: true, backend, discovery });
  await ok();
  assert.deepEqual(seen, [["tool:ok", "ok"]]);
});

test("wrapTool: no backend → transparent pass-through; markers set", async () => {
  let called = 0;
  async function myTool(x) {
    called++;
    return x * 2;
  }
  const wrapped = wrapTool("my_tool", myTool); // global runtime not installed
  assert.equal(await wrapped(21), 42);
  assert.equal(called, 1);
  assert.equal(wrapped.__keelWrapped, true);
  assert.equal(wrapped.__keelTarget, "tool:my_tool");
  assert.equal(wrapped.name, "myTool", "wrapper keeps the tool's name for introspection");
});
