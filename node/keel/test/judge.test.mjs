// Target resolution, including the frozen cross-language llm: host map. These
// exact provider strings are a parity contract with the Python front end.

import test from "node:test";
import assert from "node:assert/strict";
import { resolveTarget, LLM_HOST_PROVIDERS, classifyThrow, parseRetryAfter } from "../src/judge.mjs";

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

test("parseRetryAfter: delta-seconds, RFC 5322 date, and ISO 8601 date (Python parity)", () => {
  const now = Date.parse("2015-10-21T07:00:00Z"); // fixed reference
  // delta-seconds → *1000.
  assert.equal(parseRetryAfter("120", now), 120_000);
  // RFC 5322 / HTTP-date (what email.utils.parsedate_to_datetime parses in Python).
  assert.equal(parseRetryAfter("Wed, 21 Oct 2015 07:28:00 GMT", now), 28 * 60 * 1000);
  // ISO 8601 (Node's Date.parse accepts it; the Python twin accepts it too).
  assert.equal(parseRetryAfter("2015-10-21T07:28:00Z", now), 28 * 60 * 1000);
  // A past date clamps to 0; garbage → undefined; missing → undefined.
  assert.equal(parseRetryAfter("Wed, 21 Oct 2015 06:00:00 GMT", now), 0);
  assert.equal(parseRetryAfter("not-a-date", now), undefined);
  assert.equal(parseRetryAfter(null, now), undefined);
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
