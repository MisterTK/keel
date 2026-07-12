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
  // --- Time-dependent scenarios (advanceMs jumps the virtual clock BEFORE the
  // call, on BOTH the stub and the engine identically). Their whole point is to
  // guard the clock-driven transitions — breaker cooldown, cache TTL expiry,
  // rate window boundaries — that the time-independent scenarios above never
  // reach, so a divergence in how either side reads/advances its clock fails
  // here rather than shipping.
  {
    name: "breaker half-open recovers after cooldown, then serves normally",
    policy: { target: { svc: { retry: { attempts: 1 }, breaker: { failures: 1, cooldown: "1s" } } } },
    calls: [
      { request: req("svc"), script: [err({ class: "http", http_status: 500 })] }, // trips breaker
      { request: req("svc"), script: [] }, // E012 fail-fast while open
      // Advance past the 1s cooldown: the next call is a half-open probe.
      { request: req("svc"), advanceMs: 1000, script: [ok("probe")] }, // probe succeeds → closes
      { request: req("svc"), script: [ok("after")] }, // breaker closed again
    ],
  },
  {
    name: "cache entry expires after its TTL and the effect re-fires",
    policy: { target: { svc: { cache: { ttl: "60s" }, retry: { attempts: 1 } } } },
    calls: [
      { request: req("svc", { args_hash: "h" }), script: [ok({ v: 1 })] }, // miss → cached
      { request: req("svc", { args_hash: "h" }), script: [] }, // hit (effect skipped)
      // Advance to exactly the expiry boundary (now == expires ⇒ expired): miss.
      { request: req("svc", { args_hash: "h" }), advanceMs: 60000, script: [ok({ v: 2 })] },
      { request: req("svc", { args_hash: "h" }), script: [] }, // hit on the re-cached value
    ],
  },
  {
    name: "rate window resets after the window elapses",
    policy: { target: { svc: { rate: "1/min", retry: { attempts: 1 } } } },
    calls: [
      { request: req("svc"), script: [ok("a")] }, // window 0, count 1
      // Jump to the next 60s window: the second call is NOT throttled.
      { request: req("svc"), advanceMs: 60000, script: [ok("b")] },
    ],
  },
  {
    name: "unsupported envelope version → E004 (effect never runs)",
    policy: level0Defaults(),
    calls: [
      { request: req("api.example.com", { v: 2 }), script: [ok("never")] },
    ],
  },
];

function runStub(policy, calls) {
  const s = new KeelCoreStub();
  s.configure(policy);
  const outcomes = calls.map((c) => {
    if (c.advanceMs) s.advanceClock(c.advanceMs);
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
    if (c.advanceMs) e.advanceClock(c.advanceMs);
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
