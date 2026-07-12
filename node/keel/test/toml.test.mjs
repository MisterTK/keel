// The vendored TOML-subset parser: builds the nested policy object, handles
// quoted/dotted table headers, inline tables, arrays, and fails loudly with a
// line number (KEEL-E001) on bad syntax.

import test from "node:test";
import assert from "node:assert/strict";
import { parseToml, extractFunctionTargets } from "../src/policy.mjs";
import { KeelError } from "../src/engine.mjs";

test("parses a representative keel.toml", () => {
  const text = `
# a comment
journal = "file:.keel/journal.db"

[defaults.outbound]
timeout = "30s"
retry   = { attempts = 3, schedule = "exp(200ms, x2, max 30s, jitter)", on = ["conn", "timeout", "429", "5xx"] }

[target."api.stripe.com"]              # trailing comment
retry       = { attempts = 5 }
idempotency = { header = "Idempotency-Key" }

[target."llm:openai"]
rate = "60/min"
fallback = ["gemini-2.5-pro", "claude-sonnet-4.5"]
`;
  const policy = parseToml(text);
  assert.deepEqual(policy.defaults.outbound.retry.on, ["conn", "timeout", "429", "5xx"]);
  assert.equal(policy.defaults.outbound.timeout, "30s");
  assert.equal(policy.journal, "file:.keel/journal.db");
  assert.equal(policy.target["api.stripe.com"].retry.attempts, 5);
  assert.equal(policy.target["api.stripe.com"].idempotency.header, "Idempotency-Key");
  assert.equal(policy.target["llm:openai"].rate, "60/min");
  assert.deepEqual(policy.target["llm:openai"].fallback, ["gemini-2.5-pro", "claude-sonnet-4.5"]);
});

test("keeps commas and hashes inside quoted strings", () => {
  const policy = parseToml(`[t]\nschedule = "exp(1s, x2, max 5m) # not a comment"\n`);
  assert.equal(policy.t.schedule, "exp(1s, x2, max 5m) # not a comment");
});

test("throws KEEL-E001 with a line number on bad syntax", () => {
  try {
    parseToml(`[defaults.outbound]\ntimeout "30s"\n`); // missing '='
    assert.fail("expected a parse error");
  } catch (e) {
    assert.ok(e instanceof KeelError);
    assert.equal(e.code, "KEEL-E001");
    assert.match(e.message, /line 2/);
  }
});

test("rejects array-of-tables (unsupported subset)", () => {
  assert.throws(() => parseToml(`[[target]]\n`), (e) => e instanceof KeelError);
});

test("extractFunctionTargets parses ts: keys and flags unwrappable ones", () => {
  const policy = {
    target: {
      "ts:jobs/*.mjs#run": {},
      "ts:lib/util.mjs": {}, // no #export → cannot wrap
      "api.example.com": {},
    },
  };
  const fns = extractFunctionTargets(policy);
  assert.equal(fns.length, 2);
  const run = fns.find((f) => f.fn === "run");
  assert.equal(run.glob, "jobs/*.mjs");
  assert.equal(run.target, "ts:jobs/*.mjs#run");
  const noexport = fns.find((f) => f.glob === "lib/util.mjs");
  assert.equal(noexport.fn, null);
  assert.match(noexport.skipped, /export/);
});
