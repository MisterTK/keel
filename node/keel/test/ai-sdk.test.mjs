// Vercel AI SDK middleware seam. Contract-tested against a minimal fake model
// conforming to the LanguageModelV2 middleware interface pinned in
// fixtures/ai-sdk-model.d.ts (mirrors ai@5.0.0) — the real `ai` package is not a
// dependency. A virtual-clock backend makes backoff instant while recording it.

import test from "node:test";
import assert from "node:assert/strict";
import { AsyncEngine, virtualClock } from "../src/engine.mjs";
import { applyPackDefaults, llmDefaults } from "../src/defaults.mjs";
import { resolveDevCache } from "../src/packs/llm.mjs";
import {
  keelMiddleware,
  providerTarget,
  classifyModelError,
  aiSdkPack,
} from "../src/packs/ai-sdk.mjs";

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
  // Caller cancellation (AbortController) is `cancelled`, not `timeout`: excluded
  // from the default retry.on so an aborted generate/stream ends immediately.
  assert.equal(classifyModelError(Object.assign(new Error(), { name: "AbortError" })).class, "cancelled");
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

test("aiSdkPack implements the adapter-pack four operations", () => {
  const p = aiSdkPack({ cwd: "/nonexistent-project" });
  const d = p.detect();
  // `ai` is not a dependency anywhere in this repo, so a project-scoped
  // resolution normally reports absent; `resolveFrom`'s second tier (mirrors
  // mcpPack's own "resolve from Keel's own deps" fallback) walks up from this
  // module's OWN location, so on a machine with an unrelated `ai` install
  // somewhere above the checkout, detection can legitimately succeed —
  // assert the shape either way rather than pin a filesystem-dependent value.
  if (d.matched) {
    assert.equal(d.name, "ai");
    assert.ok(["pinned", "best_effort"].includes(d.confidence));
  } else {
    assert.deepEqual(d, { matched: false });
  }
  const seams = p.seams();
  assert.equal(seams.length, 1);
  assert.equal(seams[0].patchPoint, "wrapLanguageModel middleware (wrapGenerate/wrapStream)");
  assert.match(seams[0].whyStable, /API seam, not a monkey patch/);
  const targets = p.targets();
  assert.equal(targets.length, 1);
  assert.equal(targets[0].pattern, "llm:<provider>");
  assert.equal(targets[0].kind, "llm");
  assert.match(targets[0].argsHashRule, /generateObject/);
  assert.match(targets[0].argsHashRule, /streamObject/);
  assert.deepEqual(p.defaults(), { defaults: { llm: llmDefaults() } });
});

// generateObject/streamObject coverage: ai@5's LanguageModelV2Middleware has
// exactly wrapGenerate/wrapStream (verified against the SDK's own middleware
// docs — see the module doc comment); generateObject/streamObject route
// through the SAME doGenerate/doStream calls as generateText/streamText, with
// an object-mode marker (e.g. `responseFormat`) inside the opaque `params`.
// These tests drive that shape explicitly so all four ops are demonstrably
// covered by the two hooks above, not just asserted in a comment.
test("wrapGenerate covers generateObject: an object-mode call dev-caches like generateText", async () => {
  const mw = keelMiddleware({ backend: llmBackend() });
  let calls = 0;
  const doGenerate = async () => {
    calls++;
    return { content: [{ type: "text", text: `{"ok":${calls}}` }] };
  };
  const model = { provider: "openai.chat", modelId: "gpt-x" };
  // The shape generateObject passes doGenerate: same params bag, plus a
  // responseFormat marker — opaque to Keel, just more hashed material.
  const objectParams = {
    prompt: [{ role: "user", content: "give me json" }],
    responseFormat: { type: "json", schema: { type: "object", properties: { ok: { type: "number" } } } },
  };
  const r1 = await mw.wrapGenerate({ doGenerate, params: objectParams, model });
  const r2 = await mw.wrapGenerate({ doGenerate, params: { ...objectParams }, model });
  assert.equal(calls, 1, "identical generateObject-shaped params replay from the dev cache");
  assert.deepEqual(r2.content, r1.content);
  assert.equal(r2.keelOutcome.from_cache, true);
});

test("wrapGenerate covers generateObject: a retried 429 recovers exactly like generateText", async () => {
  const mw = keelMiddleware({ backend: llmBackend() });
  let calls = 0;
  const doGenerate = async () => {
    calls++;
    if (calls === 1) throw Object.assign(new Error("rate limited"), { statusCode: 429 });
    return { content: [{ type: "text", text: '{"ok":true}' }] };
  };
  const result = await mw.wrapGenerate({
    doGenerate,
    params: { prompt: "hi", responseFormat: { type: "json" } },
    model: { provider: "openai.chat", modelId: "gpt-x" },
  });
  assert.equal(calls, 2, "retried exactly once, same as a generateText 429");
  assert.equal(result.keelOutcome.result, "ok");
  assert.equal(result.keelOutcome.attempts, 2);
});

test("wrapStream covers streamObject: establishment retries, chunks pass through unchanged", async () => {
  const mw = keelMiddleware({ backend: llmBackend() });
  let starts = 0;
  const rawStream = new ReadableStream({
    start(c) {
      c.enqueue({ type: "object-delta", objectDelta: { ok: true } });
      c.close();
    },
  });
  const doStream = async () => {
    starts++;
    if (starts === 1) throw Object.assign(new Error("overloaded"), { statusCode: 503 });
    return { stream: rawStream };
  };
  // The shape streamObject passes doStream: same params bag + responseFormat.
  const established = await mw.wrapStream({
    doStream,
    params: { prompt: "hi", responseFormat: { type: "json" } },
    model: { provider: "openai.chat", modelId: "gpt-x" },
  });
  assert.equal(starts, 2, "establishment retried on 503, same as a streamText 503");
  assert.equal(established.stream, rawStream, "the raw object-mode stream is untouched");
  const seen = [];
  for await (const c of established.stream) seen.push(c);
  assert.deepEqual(seen, [{ type: "object-delta", objectDelta: { ok: true } }]);
});

test("wrapStream covers streamObject: never dev-cached, just like streamText", async () => {
  const mw = keelMiddleware({ backend: llmBackend() });
  let starts = 0;
  const doStream = async () => {
    starts++;
    return { stream: new ReadableStream({ start: (c) => c.close() }) };
  };
  const params = { prompt: "hi", responseFormat: { type: "json" } };
  const model = { provider: "openai" };
  await mw.wrapStream({ doStream, params, model });
  await mw.wrapStream({ doStream, params: { ...params }, model });
  assert.equal(starts, 2, "identical streamObject calls each re-establish (never replayed)");
});
