// Vercel AI SDK middleware seam. Contract-tested against a minimal fake model
// conforming to the LanguageModelV2 middleware interface pinned in
// fixtures/ai-sdk-model.d.ts (mirrors ai@5.0.0) — the real `ai` package is not a
// dependency. A virtual-clock backend makes backoff instant while recording it.

import test from "node:test";
import assert from "node:assert/strict";
import { AsyncEngine, virtualClock } from "../src/engine.mjs";
import { applyPackDefaults } from "../src/defaults.mjs";
import { resolveDevCache } from "../src/packs/llm.mjs";
import { keelMiddleware, providerTarget, classifyModelError } from "../src/packs/ai-sdk.mjs";

const llmBackend = (env = {}) => {
  const b = new AsyncEngine(virtualClock());
  b.configure(resolveDevCache(applyPackDefaults({}), env));
  return b;
};

test("providerTarget derives llm:<provider> from the model provider id", () => {
  assert.equal(providerTarget({ provider: "openai.chat" }), "llm:openai");
  assert.equal(providerTarget({ provider: "anthropic.messages" }), "llm:anthropic");
  assert.equal(providerTarget({ provider: "google.generative-ai" }), "llm:google");
  assert.equal(providerTarget({ provider: "openai" }), "llm:openai");
  assert.equal(providerTarget({}), "llm:unknown");
});

test("classifyModelError maps provider errors like the fetch seam", () => {
  const rl = classifyModelError(Object.assign(new Error("rl"), {
    statusCode: 429,
    responseHeaders: { "retry-after": "2" },
  }));
  assert.equal(rl.class, "http");
  assert.equal(rl.http_status, 429);
  assert.equal(rl.retry_after_ms, 2000);
  assert.equal(classifyModelError(Object.assign(new Error(), { name: "TimeoutError" })).class, "timeout");
  assert.equal(classifyModelError(new TypeError("fetch failed")).class, "conn");
  assert.equal(classifyModelError(new Error("weird")).class, "other");
});

test("wrapGenerate retries a 429 with a fake model (Retry-After honored)", async () => {
  const mw = keelMiddleware({ backend: llmBackend() });
  let calls = 0;
  const doGenerate = async () => {
    calls++;
    if (calls === 1)
      throw Object.assign(new Error("rate limited"), {
        statusCode: 429,
        responseHeaders: { "retry-after": "1" },
      });
    return { text: "hello", finishReason: "stop" };
  };
  const result = await mw.wrapGenerate({
    doGenerate,
    params: { prompt: "hi", temperature: 0 },
    model: { provider: "openai.chat", modelId: "gpt-x" },
  });
  assert.equal(calls, 2, "retried exactly once");
  assert.equal(result.text, "hello");
  assert.equal(result.keelOutcome.result, "ok");
  assert.equal(result.keelOutcome.attempts, 2);
  // wait = max(schedule 500ms, Retry-After 1000ms) = 1000ms.
  assert.deepEqual(result.keelOutcome.waits_ms, [1000]);
});

test("wrapGenerate dev cache replays identical params (model called once)", async () => {
  const mw = keelMiddleware({ backend: llmBackend() });
  let calls = 0;
  const doGenerate = async () => {
    calls++;
    return { text: `cached-${calls}` };
  };
  const model = { provider: "openai", modelId: "m" };
  const r1 = await mw.wrapGenerate({
    doGenerate,
    params: { prompt: [{ role: "user", content: "hi" }], temperature: 0.7 },
    model,
  });
  // key order differs but content is identical → same stable hash → cache hit.
  const r2 = await mw.wrapGenerate({
    doGenerate,
    params: { temperature: 0.7, prompt: [{ role: "user", content: "hi" }] },
    model,
  });
  assert.equal(calls, 1, "second identical call served from dev cache");
  assert.equal(r1.text, "cached-1");
  assert.equal(r2.text, "cached-1");
  assert.equal(r2.keelOutcome.from_cache, true);
});

test("wrapGenerate dev cache is inert under KEEL_ENV=prod", async () => {
  const mw = keelMiddleware({ backend: llmBackend({ KEEL_ENV: "prod" }) });
  let calls = 0;
  const doGenerate = async () => ({ text: `x${++calls}` });
  const params = { prompt: "hi" };
  const model = { provider: "openai" };
  await mw.wrapGenerate({ doGenerate, params, model });
  const r2 = await mw.wrapGenerate({ doGenerate, params, model });
  assert.equal(calls, 2, "no dev cache in prod");
  assert.equal(r2.keelOutcome.from_cache, false);
});

test("wrapStream wraps establishment, not chunks; the raw stream passes through", async () => {
  const mw = keelMiddleware({ backend: llmBackend() });
  let starts = 0;
  const rawStream = new ReadableStream({
    start(c) {
      c.enqueue("chunk-a");
      c.enqueue("chunk-b");
      c.close();
    },
  });
  const doStream = async () => {
    starts++;
    if (starts === 1) throw Object.assign(new Error("overloaded"), { statusCode: 503 });
    return { stream: rawStream, request: {}, response: {} };
  };
  const established = await mw.wrapStream({
    doStream,
    params: { prompt: "hi" },
    model: { provider: "anthropic.messages", modelId: "claude" },
  });
  assert.equal(starts, 2, "establishment retried on 503");
  assert.equal(established.stream, rawStream, "the raw stream is returned unchanged (chunks not re-wrapped)");
  assert.equal(established.keelOutcome.attempts, 2);
  // chunks flow through untouched — Keel does not observe/retry mid-stream.
  const seen = [];
  for await (const c of established.stream) seen.push(c);
  assert.deepEqual(seen, ["chunk-a", "chunk-b"]);
});

test("wrapStream never dev-caches: a second identical stream re-establishes", async () => {
  const mw = keelMiddleware({ backend: llmBackend() });
  let starts = 0;
  const makeResult = () => ({ stream: new ReadableStream({ start: (c) => c.close() }) });
  const doStream = async () => {
    starts++;
    return makeResult();
  };
  const model = { provider: "openai" };
  await mw.wrapStream({ doStream, params: { prompt: "hi" }, model });
  await mw.wrapStream({ doStream, params: { prompt: "hi" }, model });
  assert.equal(starts, 2, "streams are established each time (never replayed from cache)");
});

test("final failure re-throws the original provider error with keelOutcome", async () => {
  const backend = new AsyncEngine(virtualClock());
  backend.configure({ target: { "llm:openai": { retry: { attempts: 1 } } } });
  const mw = keelMiddleware({ backend });
  const boom = Object.assign(new Error("bad request"), { statusCode: 400 }); // 4xx: not retryable
  const doGenerate = async () => {
    throw boom;
  };
  await assert.rejects(
    () => mw.wrapGenerate({ doGenerate, params: {}, model: { provider: "openai" } }),
    (e) => {
      assert.equal(e, boom, "original error identity preserved");
      assert.equal(e.keelOutcome.error.code, "KEEL-E015");
      return true;
    }
  );
});

test("keelMiddleware is a transparent pass-through when no backend is active", async () => {
  const mw = keelMiddleware(); // no injected backend; global backend is null in tests
  let called = 0;
  const doGenerate = async () => {
    called++;
    return { text: "raw" };
  };
  const r = await mw.wrapGenerate({ doGenerate, params: {}, model: { provider: "openai" } });
  assert.equal(called, 1);
  assert.equal(r.text, "raw");
  assert.equal(r.keelOutcome, undefined, "no keelOutcome attached when disabled");
});
