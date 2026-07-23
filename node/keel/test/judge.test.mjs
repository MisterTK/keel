// Idempotency injection, cache-key derivation, and error classification —
// target resolution (the LLM host map, Vertex regional suffix, and
// `[target]` host/URL-pattern matching) is no longer a front-end judgment as
// of Task 11/SP-1: it moved to the backend (`backend.resolveTarget(...)`),
// covered by Rust unit tests + conformance scenarios 36–38, not here.

import test from "node:test";
import assert from "node:assert/strict";
import {
  classifyThrow,
  parseRetryAfter,
  resolveIdempotencyInjection,
  defaultMintIdempotencyKey,
} from "../src/judge.mjs";

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

