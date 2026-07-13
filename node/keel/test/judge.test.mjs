// Target resolution, including the frozen cross-language llm: host map. These
// exact provider strings are a parity contract with the Python front end.

import test from "node:test";
import assert from "node:assert/strict";
import {
  resolveTarget,
  resolvePolicyTarget,
  compileOutboundMatchers,
  LLM_HOST_PROVIDERS,
  classifyThrow,
  parseRetryAfter,
  resolveIdempotencyInjection,
  defaultMintIdempotencyKey,
} from "../src/judge.mjs";

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

// --- outbound host/URL-pattern targets (docs/targeting.md) -------------------
//
// A cross-language parity contract with the Python twin
// (`keel._targets.compile_outbound_targets` / `resolve_outbound`,
// python/keel/tests/test_targets.py) — the assertions below pin the exact
// grammar, precedence, and tie-break rules in both languages identically.

test("compileOutboundMatchers: a bare host with no metacharacters is exact", () => {
  const compiled = compileOutboundMatchers({ target: { "api.example.com": {} } });
  assert.deepEqual([...compiled.exact], ["api.example.com"]);
  assert.equal(compiled.patterns.length, 0);
});

test("compileOutboundMatchers: class-prefixed keys are never outbound", () => {
  const compiled = compileOutboundMatchers({
    target: {
      "py:pipeline.enrich.*": {},
      "ts:jobs/nightly.ts#run": {},
      "rs:pkg::mod": {},
      "llm:openai": {},
      "tool:search": {},
      "mcp:fs": {},
    },
  });
  assert.equal(compiled.exact.size, 0);
  assert.equal(compiled.patterns.length, 0);
});

test("compileOutboundMatchers: a wildcard host, method, port, or path makes a pattern", () => {
  const compiled = compileOutboundMatchers({
    target: {
      "*.internal.corp": {},
      "GET api.catalog.internal/*": {},
      "api.stripe.com:443": {},
      "api.partner.com/v1/*": {},
    },
  });
  assert.equal(compiled.exact.size, 0);
  assert.deepEqual(
    new Set(compiled.patterns.map((p) => p.key)),
    new Set(["*.internal.corp", "GET api.catalog.internal/*", "api.stripe.com:443", "api.partner.com/v1/*"])
  );
});

test("compileOutboundMatchers: an absent or non-table target field compiles empty", () => {
  assert.deepEqual(compileOutboundMatchers({}), { exact: new Set(), patterns: [] });
  assert.deepEqual(compileOutboundMatchers({ target: "nope" }), { exact: new Set(), patterns: [] });
  assert.deepEqual(compileOutboundMatchers({ target: ["a"] }), { exact: new Set(), patterns: [] });
});

test("resolvePolicyTarget: no compiled table behaves exactly like resolveTarget", () => {
  assert.equal(
    resolvePolicyTarget(null, { method: "GET", hostname: "api.stripe.com" }),
    "api.stripe.com"
  );
  assert.equal(
    resolvePolicyTarget(null, { method: "POST", hostname: "api.openai.com" }),
    "llm:openai"
  );
});

test("resolvePolicyTarget: the llm: host map wins even over an installed pattern", () => {
  const compiled = compileOutboundMatchers({ target: { "*.openai.com": {} } });
  assert.equal(
    resolvePolicyTarget(compiled, { method: "POST", hostname: "api.openai.com" }),
    "llm:openai"
  );
});

test("resolvePolicyTarget: exact host key beats a matching pattern", () => {
  const compiled = compileOutboundMatchers({
    target: { "api.internal": {}, "*.internal": {} },
  });
  assert.equal(
    resolvePolicyTarget(compiled, { method: "GET", hostname: "api.internal" }),
    "api.internal"
  );
});

test("resolvePolicyTarget: host wildcard crosses dots and falls back when unmatched", () => {
  const compiled = compileOutboundMatchers({ target: { "*.internal.corp": {} } });
  assert.equal(
    resolvePolicyTarget(compiled, { method: "GET", hostname: "a.b.internal.corp" }),
    "*.internal.corp"
  );
  assert.equal(
    resolvePolicyTarget(compiled, { method: "GET", hostname: "internal.corp" }),
    "internal.corp"
  );
});

test("resolvePolicyTarget: host comparison is case-insensitive", () => {
  const compiled = compileOutboundMatchers({ target: { "*.Internal.Corp": {} } });
  assert.equal(
    resolvePolicyTarget(compiled, { method: "GET", hostname: "DB.INTERNAL.CORP" }),
    "*.Internal.Corp"
  );
});

test("resolvePolicyTarget: path glob crosses slashes and is case-sensitive", () => {
  const compiled = compileOutboundMatchers({ target: { "api.catalog.internal/*": {} } });
  assert.equal(
    resolvePolicyTarget(compiled, { method: "GET", hostname: "api.catalog.internal", path: "/a/b/c" }),
    "api.catalog.internal/*"
  );
  const caseSensitive = compileOutboundMatchers({ target: { "api.x/A/*": {} } });
  assert.equal(
    resolvePolicyTarget(caseSensitive, { method: "GET", hostname: "api.x", path: "/a/y" }),
    "api.x"
  );
});

test("resolvePolicyTarget: a method prefix must match exactly (case-insensitive input)", () => {
  const compiled = compileOutboundMatchers({ target: { "POST api.example.com": {} } });
  assert.equal(
    resolvePolicyTarget(compiled, { method: "GET", hostname: "api.example.com" }),
    "api.example.com"
  );
  assert.equal(
    resolvePolicyTarget(compiled, { method: "POST", hostname: "api.example.com" }),
    "POST api.example.com"
  );
  assert.equal(
    resolvePolicyTarget(compiled, { method: "post", hostname: "api.example.com" }),
    "POST api.example.com"
  );
});

test("resolvePolicyTarget: a :port key must equal the effective port", () => {
  const compiled = compileOutboundMatchers({ target: { "api.example.com:443": {} } });
  assert.equal(
    resolvePolicyTarget(compiled, { method: "GET", hostname: "api.example.com", scheme: "https" }),
    "api.example.com:443"
  );
  assert.equal(
    resolvePolicyTarget(compiled, { method: "GET", hostname: "api.example.com", scheme: "http" }),
    "api.example.com"
  );
  const explicit = compileOutboundMatchers({ target: { "api.example.com:8443": {} } });
  assert.equal(
    resolvePolicyTarget(explicit, {
      method: "GET",
      hostname: "api.example.com",
      scheme: "https",
      port: 8443,
    }),
    "api.example.com:8443"
  );
});

test("resolvePolicyTarget: the most specific matching pattern wins", () => {
  // Both patterns carry one wildcard; the method+path-qualified key has more
  // literal characters and a method prefix, so it wins even though the host
  // wildcard also matches.
  const compiled = compileOutboundMatchers({
    target: { "*.example.com": {}, "GET api.example.com/*": {} },
  });
  assert.equal(
    resolvePolicyTarget(compiled, { method: "GET", hostname: "api.example.com", path: "/v1/x" }),
    "GET api.example.com/*"
  );
});

test("resolvePolicyTarget: tie-break is lexicographic and independent of declaration order", () => {
  const p1 = "api.example.com/x/*";
  const p2 = "api.example.com/*/y"; // same wildcard count and literal length as p1
  const req = { method: "GET", hostname: "api.example.com", path: "/x/y" };
  assert.equal(resolvePolicyTarget(compileOutboundMatchers({ target: { [p1]: {}, [p2]: {} } }), req), p2);
  assert.equal(resolvePolicyTarget(compileOutboundMatchers({ target: { [p2]: {}, [p1]: {} } }), req), p2);
});

test("resolvePolicyTarget: no pattern matches falls back to the bare host", () => {
  const compiled = compileOutboundMatchers({ target: { "other.example.com": {} } });
  assert.equal(
    resolvePolicyTarget(compiled, { method: "GET", hostname: "api.example.com" }),
    "api.example.com"
  );
});
