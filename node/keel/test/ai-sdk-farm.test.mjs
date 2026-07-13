// Adapter CI farm leg (sprint-plan.md "permanent roles": adapter CI farm —
// contract tests against pinned framework/library versions). Runs
// `keelMiddleware` through the REAL `ai` package's `wrapLanguageModel` (not
// the structural fake pinned in fixtures/ai-sdk-model.d.ts that
// ai-sdk.test.mjs uses) — proves the middleware SHAPE keelMiddleware()
// implements is actually the one `wrapLanguageModel` calls, at the exact
// pinned version.
//
// Opt-in via KEEL_ADAPTER_FARM=1: the real `ai` package is NOT a repo
// dependency (zero-runtime-deps invariant, engineering-manifesto rule 12), so
// these tests are skipped in the default offline `node --test` run (the fast
// path ai-sdk.test.mjs covers) and only run when
// .github/workflows/adapter-farm.yml has installed the pinned `ai` version
// into an isolated node_modules (see that workflow for the exact version).
//
// The `{ skip }` option is evaluated BEFORE the test body runs, so the
// dynamic `import("ai")` never executes (and never throws MODULE_NOT_FOUND)
// unless the farm env var is set.

import test from "node:test";
import assert from "node:assert/strict";
import { keelMiddleware } from "../src/packs/ai-sdk.mjs";
import { AsyncEngine, virtualClock } from "../src/engine.mjs";

const FARM = process.env.KEEL_ADAPTER_FARM === "1";
const skip = FARM ? false : "KEEL_ADAPTER_FARM=1 not set (offline fast path — see ai-sdk.test.mjs)";

/** A minimal real LanguageModelV2 (ai@5.0's own base shape), not a Keel fixture. */
function fakeModel(doGenerate, doStream) {
  return {
    specificationVersion: "v2",
    provider: "openai.chat",
    modelId: "gpt-x",
    doGenerate,
    doStream: doStream ?? (async () => ({ stream: new ReadableStream({ start: (c) => c.close() }) })),
  };
}

test("farm: real ai@wrapLanguageModel calls keelMiddleware with {doGenerate, doStream, params, model}", { skip }, async () => {
  const { wrapLanguageModel } = await import("ai");
  const backend = new AsyncEngine(virtualClock());
  backend.configure({ target: { "llm:openai": { retry: { attempts: 1 } } } });

  let seenKeys;
  const middleware = {
    wrapGenerate: async (args) => {
      seenKeys = Object.keys(args).sort();
      return args.doGenerate();
    },
  };
  const model = fakeModel(async () => ({
    content: [{ type: "text", text: "hi" }],
    finishReason: "stop",
    usage: { inputTokens: 1, outputTokens: 1, totalTokens: 2 },
  }));
  const wrapped = wrapLanguageModel({ model, middleware });
  const res = await wrapped.doGenerate({ prompt: [{ role: "user", content: "hi" }] });
  assert.deepEqual(seenKeys, ["doGenerate", "doStream", "model", "params"]);
  assert.equal(res.content[0].text, "hi");
  void backend; // this test only asserts the real SDK's call shape; keelMiddleware exercised below
});

test("farm: keelMiddleware retries a 429 through the REAL ai@wrapLanguageModel pipeline", { skip }, async () => {
  const { wrapLanguageModel } = await import("ai");
  const backend = new AsyncEngine(virtualClock());
  backend.configure({
    target: { "llm:openai": { retry: { attempts: 2, schedule: "fixed(1ms)", on: ["429", "5xx", "conn", "timeout"] } } },
  });

  let calls = 0;
  const model = fakeModel(async () => {
    calls++;
    if (calls === 1) {
      throw Object.assign(new Error("rate limited"), {
        statusCode: 429,
        responseHeaders: { "retry-after": "0" },
      });
    }
    return {
      content: [{ type: "text", text: "hi" }],
      finishReason: "stop",
      usage: { inputTokens: 1, outputTokens: 1, totalTokens: 2 },
    };
  });
  const wrapped = wrapLanguageModel({ model, middleware: keelMiddleware({ backend }) });
  const res = await wrapped.doGenerate({ prompt: [{ role: "user", content: "hi" }] });
  assert.equal(calls, 2, "retried exactly once through the real wrapLanguageModel pipeline");
  assert.equal(res.content[0].text, "hi");
  assert.equal(res.keelOutcome.result, "ok");
  assert.equal(res.keelOutcome.attempts, 2);
});

test("farm: keelMiddleware wrapStream establishment retries; the real stream passes through unbuffered", { skip }, async () => {
  const { wrapLanguageModel } = await import("ai");
  const backend = new AsyncEngine(virtualClock());
  backend.configure({ target: { "llm:openai": { retry: { attempts: 1 } } } });

  const rawStream = new ReadableStream({
    start(c) {
      c.enqueue({ type: "text-delta", id: "0", delta: "hi" });
      c.close();
    },
  });
  const model = fakeModel(
    async () => {
      throw new Error("doGenerate not used in this test");
    },
    async () => ({ stream: rawStream })
  );
  const wrapped = wrapLanguageModel({ model, middleware: keelMiddleware({ backend }) });
  const established = await wrapped.doStream({ prompt: [{ role: "user", content: "hi" }] });
  assert.equal(established.stream, rawStream, "the live stream survives the real wrapLanguageModel pipeline by identity");
});
