// Unit tests for the LLM budget-cap + fallback-chain helpers (llm-policy.mjs).
// Seam-level (fetch.mjs / ai-sdk.mjs) integration is covered in fetch.test.mjs
// and ai-sdk.test.mjs.

import test from "node:test";
import assert from "node:assert/strict";
import {
  parseBudgetCents,
  normalizeUsage,
  estimateCostUsd,
  recordSpend,
  spentCents,
  resetLlmBudgets,
  budgetMessage,
  budgetBlockedOutcome,
  deriveRequestModel,
  rewriteModel,
  shouldFallback,
} from "../src/llm-policy.mjs";

test("parseBudgetCents accepts the frozen $N/run grammar only", () => {
  assert.equal(parseBudgetCents("$5/run"), 500);
  assert.equal(parseBudgetCents("$5.50/run"), 550);
  assert.equal(parseBudgetCents("$0.01/run"), 1);
  assert.equal(parseBudgetCents("5/run"), null, "missing $ is not the frozen grammar");
  assert.equal(parseBudgetCents("$5/day"), null, "only /run is admitted");
  assert.equal(parseBudgetCents("100 tokens/run"), null, "no token grammar is frozen");
  assert.equal(parseBudgetCents(undefined), null);
  assert.equal(parseBudgetCents(""), null);
});

test("normalizeUsage handles OpenAI, Anthropic, Google, and AI-SDK usage shapes", () => {
  assert.deepEqual(normalizeUsage({ usage: { prompt_tokens: 10, completion_tokens: 20 } }), {
    inputTokens: 10,
    outputTokens: 20,
  });
  assert.deepEqual(normalizeUsage({ usage: { input_tokens: 5, output_tokens: 7 } }), {
    inputTokens: 5,
    outputTokens: 7,
  });
  assert.deepEqual(normalizeUsage({ usageMetadata: { promptTokenCount: 3, candidatesTokenCount: 4 } }), {
    inputTokens: 3,
    outputTokens: 4,
  });
  // AI SDK v2 usage object, passed directly (no wrapping key).
  assert.deepEqual(normalizeUsage({ inputTokens: 1, outputTokens: 2 }), { inputTokens: 1, outputTokens: 2 });
  assert.deepEqual(normalizeUsage({ promptTokens: 8, completionTokens: 9 }), {
    inputTokens: 8,
    outputTokens: 9,
  });
  assert.equal(normalizeUsage({ reply: "hi" }), null, "no usage field present");
  assert.equal(normalizeUsage(null), null);
});

test("estimateCostUsd prices known models and falls back to a default for unknown ones", () => {
  const usage = { inputTokens: 1_000_000, outputTokens: 1_000_000 };
  assert.equal(estimateCostUsd("gpt-4o-mini", usage), 0.15 + 0.6);
  assert.equal(estimateCostUsd("gpt-4o-mini-2026-01-01", usage), 0.15 + 0.6, "prefix match on dated model ids");
  // "gpt-4o-mini" is a longer, more specific prefix than "gpt-4o" — must win.
  assert.notEqual(estimateCostUsd("gpt-4o-mini", usage), estimateCostUsd("gpt-4o", usage));
  const unknown = estimateCostUsd("some-future-model-nobody-has-priced-yet", usage);
  assert.equal(unknown, 10 + 30, "unrecognized models use the documented conservative default price");
  assert.equal(estimateCostUsd("gpt-4o", null), 0);
});

test("the per-run spend ledger accumulates per target and can be reset", () => {
  resetLlmBudgets();
  assert.equal(spentCents("llm:openai"), 0);
  recordSpend("llm:openai", 1.2345);
  recordSpend("llm:openai", 0.5);
  assert.equal(spentCents("llm:openai"), 173, "cents accumulate and round per addition");
  assert.equal(spentCents("llm:anthropic"), 0, "ledgers are per-target");
  resetLlmBudgets();
  assert.equal(spentCents("llm:openai"), 0);
});

test("budgetMessage and budgetBlockedOutcome carry a human what/why/next and KEEL-E012", () => {
  const msg = budgetMessage("llm:openai", 500, 512);
  assert.match(msg, /\$5\.00\/run/);
  assert.match(msg, /\$5\.12/);
  assert.match(msg, /budget/i);
  const outcome = budgetBlockedOutcome(msg);
  assert.equal(outcome.result, "error");
  assert.equal(outcome.attempts, 0);
  assert.equal(outcome.breaker, "open");
  assert.equal(outcome.error.code, "KEEL-E012");
  assert.equal(outcome.error.message, msg);
});

test("deriveRequestModel reads a JSON body's model field or a Google URL path segment", () => {
  assert.equal(
    deriveRequestModel("https://api.openai.com/v1/chat/completions", JSON.stringify({ model: "gpt-4o" })),
    "gpt-4o"
  );
  assert.equal(
    deriveRequestModel(
      "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:generateContent",
      JSON.stringify({ contents: [] })
    ),
    "gemini-2.5-pro"
  );
  assert.equal(deriveRequestModel("https://api.example.com/x", "not json"), null);
  assert.equal(deriveRequestModel("https://api.example.com/x", undefined), null);
});

test("rewriteModel swaps a JSON body's model field, same URL", () => {
  const r = rewriteModel(
    "https://api.openai.com/v1/chat/completions",
    JSON.stringify({ model: "gpt-4o", temperature: 0 }),
    "gpt-4o-mini"
  );
  assert.equal(r.url, "https://api.openai.com/v1/chat/completions");
  assert.deepEqual(JSON.parse(r.body), { model: "gpt-4o-mini", temperature: 0 });
});

test("rewriteModel swaps a Google URL path segment, same body", () => {
  const r = rewriteModel(
    "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:generateContent",
    JSON.stringify({ contents: [] }),
    "gemini-2.5-flash"
  );
  assert.equal(r.url, "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent");
  assert.equal(r.body, JSON.stringify({ contents: [] }));
});

test("rewriteModel returns null for an unrecognized request shape (honest v0.1 limitation)", () => {
  assert.equal(rewriteModel("https://api.example.com/x", "not json, no model field", "other-model"), null);
  assert.equal(rewriteModel("https://api.example.com/x", undefined, "other-model"), null);
});

test("shouldFallback chases any terminal failure except a breaker-open/budget block", () => {
  assert.equal(shouldFallback({ code: "KEEL-E010" }), true, "attempts exhausted");
  assert.equal(shouldFallback({ code: "KEEL-E014" }), true, "non-idempotent, observed-not-retried");
  assert.equal(shouldFallback({ code: "KEEL-E015" }), true, "non-retryable error class");
  assert.equal(shouldFallback({ code: "KEEL-E012" }), false, "breaker-open (or our budget synthesis)");
  assert.equal(shouldFallback(null), false);
  assert.equal(shouldFallback(undefined), false);
});
