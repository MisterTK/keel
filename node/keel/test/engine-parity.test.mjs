// Parity guard: the AsyncEngine's decisions must be byte-identical to the real
// KeelCoreStub on the same scripted attempt sequences. Two implementations of
// the same semantics is how a conformance suite rots (manifesto §14); this test
// makes any divergence in the layer chain / terminal codes / envelope fail.

import test from "node:test";
import assert from "node:assert/strict";
import { KeelCoreStub } from "../../keel-core-stub/index.mjs";
import { AsyncEngine, virtualClock } from "../src/engine.mjs";
import { level0Defaults } from "../src/defaults.mjs";

const req = (target, extra = {}) => ({ v: 1, target, op: target, idempotent: true, ...extra });
const err = (o) => ({ status: "error", ...o });
const ok = (payload) => ({ status: "ok", payload });

const SCENARIOS = [
  {
    name: "success first attempt",
    policy: level0Defaults(),
    calls: [{ request: req("api.example.com"), script: [ok({ ok: true })] }],
  },
  {
    name: "503 then ok (retry)",
    policy: level0Defaults(),
    calls: [
      {
        request: req("api.example.com"),
        script: [err({ class: "http", http_status: 503, message: "boom" }), ok(1)],
      },
    ],
  },
  {
    name: "429 Retry-After overrides schedule",
    policy: level0Defaults(),
    calls: [
      {
        request: req("api.example.com"),
        script: [err({ class: "http", http_status: 429, retry_after_ms: 5000 }), ok(1)],
      },
    ],
  },
  {
    name: "POST non-idempotent observed not retried (E014)",
    policy: level0Defaults(),
    calls: [
      {
        request: req("api.example.com", { idempotent: false }),
        script: [err({ class: "http", http_status: 503 })],
      },
    ],
  },
  {
    name: "non-retryable class (E015)",
    policy: level0Defaults(),
    calls: [
      { request: req("api.example.com"), script: [err({ class: "http", http_status: 404 })] },
    ],
  },
  {
    name: "attempts exhausted (E010) with backoff waits",
    policy: level0Defaults(),
    calls: [
      {
        request: req("api.example.com"),
        script: [
          err({ class: "http", http_status: 500 }),
          err({ class: "http", http_status: 500 }),
          err({ class: "http", http_status: 500 }),
        ],
      },
    ],
  },
  {
    name: "breaker opens then fails fast (E012)",
    policy: { target: { svc: { retry: { attempts: 1 }, breaker: { failures: 1 } } } },
    calls: [
      { request: req("svc"), script: [err({ class: "http", http_status: 500 })] },
      { request: req("svc"), script: [] },
    ],
  },
  {
    name: "rate limit throttles second call",
    policy: { target: { svc: { rate: "1/min", retry: { attempts: 1 } } } },
    calls: [
      { request: req("svc"), script: [ok("a")] },
      { request: req("svc"), script: [ok("b")] },
    ],
  },
  {
    name: "cache hit skips the effect",
    policy: { target: { svc: { cache: { ttl: "60s" }, retry: { attempts: 1 } } } },
    calls: [
      { request: req("svc", { args_hash: "h" }), script: [ok({ v: 1 })] },
      { request: req("svc", { args_hash: "h" }), script: [] },
    ],
  },
];

function runStub(policy, calls) {
  const s = new KeelCoreStub();
  s.configure(policy);
  const outcomes = calls.map((c) => {
    let i = 0;
    return s.execute(c.request, () => c.script[i++]);
  });
  return { outcomes, report: s.report() };
}

async function runEngine(policy, calls) {
  const e = new AsyncEngine(virtualClock());
  e.configure(policy);
  const outcomes = [];
  for (const c of calls) {
    let i = 0;
    outcomes.push(await e.execute(c.request, async () => c.script[i++]));
  }
  return { outcomes, report: e.report() };
}

for (const s of SCENARIOS) {
  test(`parity: ${s.name}`, async () => {
    const stub = runStub(s.policy, s.calls);
    const eng = await runEngine(s.policy, s.calls);
    assert.deepStrictEqual(eng.outcomes, stub.outcomes, "outcomes diverged from stub");
    // Compare the WHOLE report ({v, clock_ms, targets}) so shape drift (e.g. a
    // missing clock_ms) can never hide behind a targets-only comparison.
    assert.deepStrictEqual(eng.report, stub.report, "reports diverged from stub");
  });
}
