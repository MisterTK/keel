// The mcp: pack. Deterministic in-process tests (virtual clock) cover
// retry/breaker/target derivation/reversibility; a child-process test drives a
// scripted fake MCP server over stdio (newline JSON-RPC) to prove the
// load-bearing behavior: a hung server times out per policy and degrades
// gracefully (KEEL-E010) instead of freezing the agent.

import test from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { AsyncEngine, virtualClock, realClock } from "../src/engine.mjs";
import { applyPackDefaults } from "../src/defaults.mjs";
import {
  makeWrappedRequest,
  patchClientRequest,
  classifyMcpError,
  isIdempotentMcpMethod,
  mcpPack,
} from "../src/packs/mcp.mjs";

const serverPath = fileURLToPath(new URL("../fixtures/fake-mcp-server.mjs", import.meta.url));

function clientWith(original, backend, name = "svc") {
  return { getServerVersion: () => ({ name, version: "0" }), request: makeWrappedRequest(original, { backend }) };
}

test("classifyMcpError maps timeout/conn/other", () => {
  assert.equal(classifyMcpError(Object.assign(new Error(), { name: "TimeoutError" })), "timeout");
  assert.equal(classifyMcpError(Object.assign(new Error(), { name: "AbortError" })), "timeout");
  assert.equal(classifyMcpError(Object.assign(new Error("x"), { code: -32001 })), "timeout");
  assert.equal(classifyMcpError(Object.assign(new Error("x"), { code: "ECONNREFUSED" })), "conn");
  assert.equal(classifyMcpError(new Error("transport closed")), "conn");
  assert.equal(classifyMcpError(new Error("bad tool args")), "other");
});

test("mcpPack implements the adapter-pack four operations", () => {
  const p = mcpPack({ cwd: "/nonexistent-project" });
  assert.deepEqual(p.detect(), { matched: false }); // SDK absent in this repo
  const seams = p.seams();
  assert.equal(seams[0].patchPoint, "Client.prototype.request");
  assert.match(seams[0].whyStable, /both client transports|all client transports/);
  assert.equal(p.targets()[0].kind, "mcp");
  assert.deepEqual(p.defaults(), {}); // mcp: inherits [defaults.outbound]
});

test("isIdempotentMcpMethod: reads retry; tools/call + unknown do not", () => {
  for (const m of [
    "initialize",
    "ping",
    "tools/list",
    "resources/list",
    "resources/templates/list",
    "resources/read",
    "prompts/list",
    "prompts/get",
    "completion/complete",
  ])
    assert.equal(isIdempotentMcpMethod(m), true, m);
  for (const m of ["tools/call", "logging/setLevel", "resources/subscribe", "frobnicate", "", undefined])
    assert.equal(isIdempotentMcpMethod(m), false, String(m));
});

test("mcp: target is mcp:<server>; idempotency is method-keyed; never cached", async () => {
  const captured = [];
  const backend = {
    kind: "fake",
    configure() {},
    layer: () => undefined,
    report: () => ({ v: 1, clock_ms: 0, targets: {} }),
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
  const client = clientWith(async () => ({ pong: true }), backend, "weather");
  await client.request({ method: "tools/list", params: {} }, null, {});
  await client.request({ method: "tools/call", params: { name: "x" } }, null, {});
  assert.equal(captured[0].target, "mcp:weather");
  assert.equal(captured[0].idempotent, true, "tools/list is read-ish → idempotent");
  assert.equal(captured[1].idempotent, false, "tools/call is side-effecting → non-idempotent");
  assert.equal(captured[0].args_hash, null);
  assert.equal(captured[1].args_hash, null);
});

test("mcp: tools/call is observed, not retried (KEEL-E014); original propagates", async () => {
  const backend = new AsyncEngine(virtualClock());
  backend.configure({
    target: { "mcp:svc": { retry: { attempts: 3, schedule: "fixed(1ms)", on: ["conn"] } } },
  });
  let n = 0;
  const reset = Object.assign(new Error("connection reset"), { code: "ECONNRESET" });
  const original = async function () {
    n++;
    throw reset;
  };
  const client = clientWith(original, backend);
  let caught;
  try {
    await client.request({ method: "tools/call", params: { name: "charge" } }, null, {});
  } catch (e) {
    caught = e;
  }
  assert.equal(n, 1, "a side-effecting tools/call is NOT retried, even with retry configured");
  assert.equal(caught, reset, "original error propagates unchanged");
  assert.equal(caught.keelOutcome.error.code, "KEEL-E014");
  assert.deepStrictEqual(backend.report().targets["mcp:svc"], {
    attempts: 1,
    breaker_opens: 0,
    breaker_state: "closed",
    cache_hits: 0,
    calls: 1,
    failures: 1,
    retries: 0,
    successes: 0,
    throttled: 0,
  });
});

test("mcp: resources/list is retried per defaults (read-ish, inherits outbound)", async () => {
  const backend = new AsyncEngine(virtualClock());
  backend.configure(applyPackDefaults({})); // mcp:svc inherits defaults.outbound (retry on conn)
  let n = 0;
  const original = async function () {
    if (++n === 1) throw Object.assign(new Error("connection reset"), { code: "ECONNRESET" });
    return { resources: [] };
  };
  const client = clientWith(original, backend);
  const res = await client.request({ method: "resources/list", params: {} }, null, {});
  assert.deepEqual(res, { resources: [] });
  assert.equal(n, 2, "read-ish method retried per defaults.outbound");
  const t = backend.report().targets["mcp:svc"];
  assert.equal(t.attempts, 2);
  assert.equal(t.retries, 1);
  assert.equal(t.successes, 1);
});

test("mcp: retries a conn error then succeeds (counters)", async () => {
  const backend = new AsyncEngine(virtualClock());
  backend.configure({
    target: { "mcp:svc": { retry: { attempts: 3, schedule: "fixed(1ms)", on: ["conn"] } } },
  });
  let n = 0;
  const original = async function () {
    if (++n === 1) throw new Error("connection closed");
    return { tools: [] };
  };
  const client = clientWith(original, backend);
  const res = await client.request({ method: "tools/list", params: {} }, null, {});
  assert.deepEqual(res, { tools: [] });
  assert.equal(n, 2);
  assert.deepStrictEqual(backend.report().targets["mcp:svc"], {
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

test("mcp: breaker opens after repeated failures then fails fast (KEEL-E012)", async () => {
  const backend = new AsyncEngine(virtualClock());
  backend.configure({
    target: { "mcp:svc": { retry: { attempts: 1, on: ["conn"] }, breaker: { failures: 2, cooldown: "30s" } } },
  });
  let attempts = 0;
  const original = async function () {
    attempts++;
    throw Object.assign(new Error("refused"), { code: "ECONNREFUSED" });
  };
  const client = clientWith(original, backend);
  await assert.rejects(() => client.request({ method: "x" }, null, {}));
  await assert.rejects(() => client.request({ method: "x" }, null, {}));
  await assert.rejects(
    () => client.request({ method: "x" }, null, {}),
    (e) => {
      assert.equal(e.keelOutcome.error.code, "KEEL-E012");
      return true;
    }
  );
  assert.equal(attempts, 2, "third call fails fast — the transport is not touched");
  const t = backend.report().targets["mcp:svc"];
  assert.equal(t.breaker_opens, 1);
  assert.equal(t.breaker_state, "open");
});

test("patchClientRequest patches and reverses; no backend → pass-through", async () => {
  class FakeClient {
    async request(r) {
      return { ok: r.method };
    }
    getServerVersion() {
      return { name: "svc" };
    }
  }
  const uninstall = patchClientRequest(FakeClient); // global backend is null → pass-through
  assert.equal(FakeClient.prototype.request.__keelWrapped, true);
  const r = await new FakeClient().request({ method: "ping" }, null, {});
  assert.deepEqual(r, { ok: "ping" }, "pass-through when no backend is active");
  // second patch is a no-op.
  const noop = patchClientRequest(FakeClient);
  noop();
  assert.equal(FakeClient.prototype.request.__keelWrapped, true, "no-op patch did not disturb the wrap");
  uninstall();
  assert.equal(FakeClient.prototype.request.__keelWrapped, undefined, "uninstall restored the original");
});

// --- child-process tests over a real stdio JSON-RPC transport ----------------

/** Minimal MCP client whose `request` speaks newline JSON-RPC to the fake server
 *  over stdio and honors an AbortSignal (like the real SDK's request). */
class StdioClient {
  #child;
  #pending = new Map();
  #buf = "";
  #nextId = 1;
  #name;
  sent = 0;
  constructor(child, name) {
    this.#child = child;
    this.#name = name;
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (chunk) => this.#onData(chunk));
  }
  getServerVersion() {
    return { name: this.#name, version: "0" };
  }
  #onData(chunk) {
    this.#buf += chunk;
    let nl;
    while ((nl = this.#buf.indexOf("\n")) >= 0) {
      const line = this.#buf.slice(0, nl);
      this.#buf = this.#buf.slice(nl + 1);
      if (!line.trim()) continue;
      const msg = JSON.parse(line);
      const p = this.#pending.get(msg.id);
      if (!p) continue;
      this.#pending.delete(msg.id);
      if (msg.error) p.reject(Object.assign(new Error(msg.error.message), { code: msg.error.code }));
      else p.resolve(msg.result);
    }
  }
  request(request, _schema, options) {
    const id = this.#nextId++;
    return new Promise((resolve, reject) => {
      if (options?.signal) {
        if (options.signal.aborted) return reject(options.signal.reason ?? new DOMException("aborted", "AbortError"));
        options.signal.addEventListener(
          "abort",
          () => {
            this.#pending.delete(id);
            reject(options.signal.reason ?? new DOMException("aborted", "AbortError"));
          },
          { once: true }
        );
      }
      this.#pending.set(id, { resolve, reject });
      this.sent++;
      this.#child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method: request.method, params: request.params }) + "\n");
    });
  }
  close() {
    this.#child.kill();
  }
}

test("mcp: hung server times out per policy and degrades gracefully (KEEL-E010)", async () => {
  const backend = new AsyncEngine(realClock());
  backend.configure({
    target: { "mcp:fake-mcp": { timeout: "60ms", retry: { attempts: 2, schedule: "fixed(1ms)", on: ["timeout"] } } },
  });
  const child = spawn(process.execPath, [serverPath], { stdio: ["pipe", "pipe", "ignore"] });
  const client = new StdioClient(child, "fake-mcp");
  const request = makeWrappedRequest(client.request.bind(client), { backend });
  try {
    let caught;
    try {
      // a read-ish method → idempotent → Keel imposes its per-attempt timeout.
      await request.call(client, { method: "tools/list", params: { mode: "hang" } }, null, {});
    } catch (e) {
      caught = e;
    }
    assert.ok(caught, "a hung request must reject, not freeze");
    assert.equal(caught.keelOutcome.error.code, "KEEL-E010");
    assert.equal(caught.keelOutcome.error.class, "timeout");
    assert.equal(caught.keelOutcome.attempts, 2, "retried per policy before giving up");
    assert.equal(client.sent, 2, "the server received both attempts");
  } finally {
    client.close();
  }
});

test("mcp: a non-transient server error propagates unchanged (KEEL-E015, over stdio)", async () => {
  const backend = new AsyncEngine(realClock());
  backend.configure({ target: { "mcp:fake-mcp": { retry: { attempts: 3, on: ["timeout"] } } } });
  const child = spawn(process.execPath, [serverPath], { stdio: ["pipe", "pipe", "ignore"] });
  const client = new StdioClient(child, "fake-mcp");
  const request = makeWrappedRequest(client.request.bind(client), { backend });
  try {
    let caught;
    try {
      await request.call(client, { method: "boom", params: { mode: "error" } }, null, {});
    } catch (e) {
      caught = e;
    }
    assert.ok(caught, "a server error must surface");
    assert.equal(caught.message, "scripted error", "original error propagates unchanged");
    assert.equal(caught.keelOutcome.error.code, "KEEL-E015"); // class other → not retryable
    assert.equal(client.sent, 1, "a non-transient error is not retried");
  } finally {
    client.close();
  }
});
