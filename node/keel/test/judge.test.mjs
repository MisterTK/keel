// Target resolution, including the frozen cross-language llm: host map. These
// exact provider strings are a parity contract with the Python front end.

import test from "node:test";
import assert from "node:assert/strict";
import { resolveTarget, LLM_HOST_PROVIDERS, classifyThrow } from "../src/judge.mjs";

test("llm: host map resolves to the exact contracted targets", () => {
  assert.equal(resolveTarget("api.openai.com"), "llm:openai");
  assert.equal(resolveTarget("api.anthropic.com"), "llm:anthropic");
  assert.equal(resolveTarget("generativelanguage.googleapis.com"), "llm:google-genai");
});

test("the map has exactly the three contracted providers", () => {
  assert.deepEqual(LLM_HOST_PROVIDERS, {
    "api.openai.com": "openai",
    "api.anthropic.com": "anthropic",
    "generativelanguage.googleapis.com": "google-genai",
  });
});

test("unmapped hosts resolve to the bare hostname", () => {
  assert.equal(resolveTarget("api.stripe.com"), "api.stripe.com");
  assert.equal(resolveTarget("127.0.0.1"), "127.0.0.1");
});

test("classifyThrow: caller AbortError → cancelled; Keel TimeoutError → timeout", () => {
  // A caller's AbortController abort is user cancellation → 'cancelled' (not in
  // default retry.on → immediately terminal), so a stop button flies at once.
  assert.equal(classifyThrow(new DOMException("aborted", "AbortError")), "cancelled");
  assert.equal(classifyThrow(Object.assign(new Error("x"), { name: "AbortError" })), "cancelled");
  // Keel's own per-attempt deadline uses a TimeoutError → 'timeout' (retryable).
  assert.equal(classifyThrow(new DOMException("Keel timeout", "TimeoutError")), "timeout");
  assert.equal(classifyThrow(new TypeError("fetch failed")), "conn");
  assert.equal(classifyThrow(new Error("weird")), "other");
});
