// AI-SDK middleware against the REAL native core (crates/keel-node), not the
// stub. This is the seam the whole-branch review flagged Critical: the pack must
// keep the live doGenerate/doStream result and the live provider error SIDE-BAND
// and send only a JSON-safe payload through the core — otherwise the native
// serde round-trip kills a live ReadableStream (KEEL-E015 on a SUCCESSFUL call)
// and strips Error identity (breaking rethrow, DX invariant 5).
//
// Auto-skips when the native addon is absent (build: `cargo build -p keel-node
// --release`).

import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { loadBackend } from "../src/backend.mjs";
import { keelMiddleware } from "../src/packs/ai-sdk.mjs";
import { loaded as nativeLoaded } from "../../keel-core-native/index.mjs";

const gate = nativeLoaded
  ? {}
  : { skip: "keel-core-native binary absent — build with `cargo build -p keel-node --release`" };

/** An in-memory (KEEL_JOURNAL="") native backend configured with `policy`. */
async function nativeBackend(policy) {
  const dir = mkdtempSync(join(tmpdir(), "keel-aisdk-native-"));
  const backend = await loadBackend({ preferred: "native", cwd: dir, env: { KEEL_JOURNAL: "" } });
  assert.equal(backend.kind, "native", "native backend must load for this test");
  backend.configure(policy);
  return { backend, cleanup: () => rmSync(dir, { recursive: true, force: true }) };
}

test("native: wrapStream — the live ReadableStream survives the core (no KEEL-E015)", gate, async () => {
  const { backend, cleanup } = await nativeBackend({
    target: { "llm:anthropic": { retry: { attempts: 1 } } },
  });
  try {
    const mw = keelMiddleware({ backend });
    const rawStream = new ReadableStream({
      start(c) {
        c.enqueue("a");
        c.enqueue("b");
        c.close();
      },
    });
    const established = await mw.wrapStream({
      doStream: async () => ({ stream: rawStream, request: {}, response: {} }),
      params: { prompt: "hi" },
      model: { provider: "anthropic.messages", modelId: "claude" },
    });
    assert.equal(established.stream, rawStream, "the live stream is returned by identity, not round-tripped");
    assert.equal(established.keelOutcome.result, "ok", "a successful stream is not turned into KEEL-E015");
    const seen = [];
    for await (const c of established.stream) seen.push(c);
    assert.deepEqual(seen, ["a", "b"], "chunks flow through untouched");
  } finally {
    cleanup();
  }
});

test("native: wrapGenerate — a provider error is re-thrown with its original identity", gate, async () => {
  const { backend, cleanup } = await nativeBackend({
    target: { "llm:openai": { retry: { attempts: 1 } } }, // 4xx not retryable anyway
  });
  try {
    const mw = keelMiddleware({ backend });
    const boom = Object.assign(new Error("bad request"), { name: "APICallError", statusCode: 400 });
    await assert.rejects(
      () => mw.wrapGenerate({ doGenerate: async () => { throw boom; }, params: {}, model: { provider: "openai" } }),
      (e) => {
        assert.equal(e, boom, "the ORIGINAL provider error identity crosses the native core");
        assert.equal(e.keelOutcome.error.code, "KEEL-E015", "classified http 4xx → terminal, not retried");
        return true;
      }
    );
  } finally {
    cleanup();
  }
});

test("native: wrapGenerate — live result identity on success; JSON replay on a dev-cache hit", gate, async () => {
  const { backend, cleanup } = await nativeBackend({
    target: { "llm:openai": { retry: { attempts: 1 }, cache: { ttl: "24h" } } },
  });
  try {
    const mw = keelMiddleware({ backend });
    let calls = 0;
    // A result carrying a Date (survives JSON.stringify but is NOT serde-native)
    // and a live function property (would make the native serde fail if it crossed).
    const live = { text: "hello", when: new Date(0), _cb: () => 42 };
    const doGenerate = async () => {
      calls++;
      return live;
    };
    const params = { prompt: [{ role: "user", content: "hi" }], temperature: 0.7 };
    const model = { provider: "openai", modelId: "m" };

    const r1 = await mw.wrapGenerate({ doGenerate, params, model });
    assert.equal(r1, live, "live success returns the real object by identity (function + Date intact)");
    assert.equal(r1._cb(), 42);
    assert.equal(r1.keelOutcome.from_cache, false);

    // key order differs, content identical → same stable hash → cache hit.
    const r2 = await mw.wrapGenerate({
      doGenerate,
      params: { temperature: 0.7, prompt: [{ role: "user", content: "hi" }] },
      model,
    });
    assert.equal(calls, 1, "second identical call served from the native in-memory dev cache");
    assert.equal(r2.keelOutcome.from_cache, true);
    assert.equal(r2.text, "hello", "the JSON payload replays the serializable fields");
  } finally {
    cleanup();
  }
});

test("native: wrapGenerate — 429 retry then success (stub-parity outcome shape)", gate, async () => {
  const { backend, cleanup } = await nativeBackend({
    target: {
      "llm:openai": { retry: { attempts: 2, schedule: "fixed(1ms)", on: ["429", "5xx", "conn", "timeout"] } },
    },
  });
  try {
    const mw = keelMiddleware({ backend });
    let calls = 0;
    const ok = { text: "hi" };
    const r = await mw.wrapGenerate({
      doGenerate: async () => {
        if (++calls === 1) throw Object.assign(new Error("rl"), { statusCode: 429 });
        return ok;
      },
      params: { prompt: "hi" },
      model: { provider: "openai" },
    });
    assert.equal(calls, 2, "retried exactly once");
    assert.equal(r, ok, "the retried live result is returned by identity");
    assert.equal(r.keelOutcome.result, "ok");
    assert.equal(r.keelOutcome.attempts, 2);
  } finally {
    cleanup();
  }
});
