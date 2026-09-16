// Idempotency injection, cache-key derivation, and error classification —
// target resolution (the LLM host map, Vertex regional suffix, and
// `[target]` host/URL-pattern matching) is no longer a front-end judgment as
// of Task 11/SP-1: it moved to the backend (`backend.resolveTarget(...)`),
// covered by Rust unit tests + conformance scenarios 36–38, not here.

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  classifyThrow,
  parseRetryAfter,
  resolveIdempotencyInjection,
  defaultMintIdempotencyKey,
  replayHeaders,
  deriveArgsHash,
  lroShapedPath,
  streamingResponse,
  operationRead,
  isIdempotent,
} from "../src/judge.mjs";

const operationReadCorpus = new URL("../../../conformance/operation_read/cases.json", import.meta.url);

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

// --- idempotency-key injection (contracts/adapter-pack.md "Idempotency-key
// injection"), pinned independently of fetch/HTTP plumbing --------------

test("no configured header means no injection", () => {
  assert.equal(resolveIdempotencyInjection("POST", new Headers(), undefined), null);
});

test("idempotent methods are never injected", () => {
  for (const m of ["GET", "HEAD", "OPTIONS", "PUT", "DELETE", "TRACE"]) {
    assert.equal(resolveIdempotencyInjection(m, new Headers(), "Idempotency-Key"), null, m);
  }
});

test("a caller-supplied key always wins, never overwritten", () => {
  assert.equal(
    resolveIdempotencyInjection("POST", new Headers({ "idempotency-key": "x" }), "Idempotency-Key"),
    null,
  );
  // Case-insensitive match (Headers is already case-insensitive).
  assert.equal(
    resolveIdempotencyInjection("POST", new Headers({ "IDEMPOTENCY-KEY": "x" }), "Idempotency-Key"),
    null,
  );
});

test("an unsafe method with a configured header mints a key via the injectable source", () => {
  const key = resolveIdempotencyInjection("POST", new Headers(), "Idempotency-Key", () => "fixed-key");
  assert.equal(key, "fixed-key");
});

test("defaultMintIdempotencyKey mints distinct opaque values", () => {
  const a = defaultMintIdempotencyKey();
  const b = defaultMintIdempotencyKey();
  assert.notEqual(a, b);
  assert.ok(a);
});

// --- replayHeaders (twin of Python _http.replay_headers, #66) -----------

test("replayHeaders strips the wire-encoding trio case-insensitively", () => {
  const out = replayHeaders([
    ["Content-Type", "application/json"],
    ["Content-Encoding", "gzip"],
    ["content-length", "999"],
    ["Transfer-Encoding", "chunked"],
    ["X-Request-Id", "abc"],
  ]);
  assert.deepEqual(out, [["Content-Type", "application/json"], ["X-Request-Id", "abc"]]);
  assert.deepEqual(replayHeaders(undefined), []);
});

// --- deriveArgsHash: stream-shaped llm POSTs derive no cache key (#84) ---

test("deriveArgsHash: llm POST to a :streamGenerateContent path derives no hash, with or without ?alt=sse", () => {
  // Gemini/Vertex SSE: the URL path itself names the streaming method.
  assert.equal(
    deriveArgsHash(
      "llm:google-genai",
      "POST",
      "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.0-flash:streamGenerateContent?alt=sse",
      JSON.stringify({ contents: [{ parts: [{ text: "hi" }] }] }),
    ),
    null,
  );
  // Without the query string too — the path suffix alone decides.
  assert.equal(
    deriveArgsHash(
      "llm:google-genai",
      "POST",
      "https://example.com/v1/models/m:streamGenerateContent",
      JSON.stringify({ contents: [] }),
    ),
    null,
  );
});

test('deriveArgsHash: llm POST with top-level "stream": true derives no hash', () => {
  // OpenAI/Anthropic convention: top-level "stream": true in the JSON body.
  assert.equal(
    deriveArgsHash(
      "llm:openai",
      "POST",
      "https://api.openai.com/v1/chat/completions",
      JSON.stringify({ model: "gpt-4o", stream: true, messages: [] }),
    ),
    null,
  );
});

test('deriveArgsHash: a string "true" or a nested stream key does NOT suppress the hash', () => {
  assert.match(
    deriveArgsHash(
      "llm:openai",
      "POST",
      "https://api.openai.com/v1/chat/completions",
      JSON.stringify({ model: "gpt-4o", stream: "true", messages: [] }),
    ),
    /^[0-9a-f]{64}$/,
  );
  assert.match(
    deriveArgsHash(
      "llm:openai",
      "POST",
      "https://api.openai.com/v1/chat/completions",
      JSON.stringify({ model: "gpt-4o", nested: { stream: true }, messages: [] }),
    ),
    /^[0-9a-f]{64}$/,
  );
});

test("deriveArgsHash: a non-streaming llm POST still derives a hash (dev-cache replay path unchanged)", () => {
  assert.match(
    deriveArgsHash(
      "llm:openai",
      "POST",
      "https://api.openai.com/v1/chat/completions",
      JSON.stringify({ model: "gpt-4o", stream: false, messages: [] }),
    ),
    /^[0-9a-f]{64}$/,
  );
  assert.match(
    deriveArgsHash(
      "llm:google-genai",
      "POST",
      "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.0-flash:generateContent",
      JSON.stringify({ contents: [] }),
    ),
    /^[0-9a-f]{64}$/,
  );
});

test("deriveArgsHash: an unparseable URL falls through to the normal hash path, never throws", () => {
  // Python's `urlsplit` never raises; `new URL` throws on a non-absolute URL.
  // The twins must agree, and a cache-key derivation must never become an
  // exception at the seam: an unrecognizable URL simply isn't a streaming
  // shape.
  assert.match(
    deriveArgsHash("llm:openai", "POST", "not a url", JSON.stringify({ model: "gpt-4o" })),
    /^[0-9a-f]{64}$/,
  );
  assert.match(
    deriveArgsHash("llm:openai", "POST", "/v1/chat/completions", JSON.stringify({ model: "x" })),
    /^[0-9a-f]{64}$/,
  );
});

// --- lroShapedPath / deriveArgsHash: LRO submit/poll POST shapes derive no
// cache key (#83) ------------------------------------------------------------

test("lroShapedPath: Google LRO submit/poll verbs, and nothing else", () => {
  for (const p of [
    "/v1/projects/p/locations/us-central1/publishers/google/models/veo-3.1:predictLongRunning",
    "/v1/projects/p/locations/us-central1/publishers/google/models/veo-3.1:fetchPredictOperation",
    "/v1beta1/projects/p/locations/global/operations/op:fetchOperation",
    "/v1/models/m:fetchFooOperation",
  ]) assert.equal(lroShapedPath(p), true, p);
  for (const p of [
    "/v1beta/models/gemini-2.0-flash:generateContent",
    "/v1beta/models/gemini-2.0-flash:streamGenerateContent",
    "/v1/chat/completions",
    "/v1/operations/op1",
    "/v1/models/m:fetch",
    "/v1/models/m:Operation",
  ]) assert.equal(lroShapedPath(p), false, p);
});

test("deriveArgsHash: llm POST LRO submit and poll shapes derive no hash (#83)", () => {
  const base = "https://us-central1-aiplatform.googleapis.com/v1/projects/p/locations/us-central1/publishers/google/models/veo-3.1";
  assert.equal(deriveArgsHash("llm:google-genai", "POST", `${base}:fetchPredictOperation`, JSON.stringify({ operationName: "projects/p/operations/op1" })), null);
  assert.equal(deriveArgsHash("llm:google-genai", "POST", `${base}:predictLongRunning`, JSON.stringify({ instances: [{ prompt: "a cat" }] })), null);
  assert.notEqual(deriveArgsHash("llm:google-genai", "POST", `${base}:generateContent`, JSON.stringify({ contents: [] })), null);
});

// --- streamingResponse: the response-side twin of the args_hash streaming
// exception above (#84) — every fetch response envelope choke point consumes
// this predicate. -----------------------------------------------------------

test("streamingResponse: text/event-stream (any casing, params tolerated) is true; else false", () => {
  assert.equal(streamingResponse("text/event-stream"), true);
  assert.equal(streamingResponse("Text/Event-Stream; charset=utf-8"), true);
  assert.equal(streamingResponse("application/json"), false);
  assert.equal(streamingResponse(null), false);
  assert.equal(streamingResponse(undefined), false);
  assert.equal(streamingResponse(""), false);
});

test("operation-read corpus: every row agrees with the Python front end (CCR-8)", () => {
  const rows = JSON.parse(readFileSync(operationReadCorpus, "utf8"));
  assert.ok(rows.length >= 12);
  for (const row of rows) {
    assert.equal(operationRead(row.host, row.path), row.operation_read, row.name);
    const headers = new Headers(row.headers.map((h) => [h, "x"]));
    assert.equal(isIdempotent(row.method, headers, undefined, row.host, row.path), row.idempotent, row.name);
  }
});

test("injection is skipped for an operation read (CCR-8)", () => {
  const vertex = "us-central1-aiplatform.googleapis.com";
  const fetch = "/v1/projects/p/locations/us-central1/publishers/google/models/veo-3.1:fetchPredictOperation";
  const submit = "/v1/projects/p/locations/us-central1/publishers/google/models/veo-3.1:predictLongRunning";
  assert.equal(resolveIdempotencyInjection("POST", new Headers(), "Idempotency-Key", () => "k", null, vertex, fetch), null);
  assert.equal(resolveIdempotencyInjection("POST", new Headers(), "Idempotency-Key", () => "k", null, vertex, submit), "k");
});
