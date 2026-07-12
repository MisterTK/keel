// The llm: pack: adapter-pack shape, the policy-merge semantics, and dev-cache
// resolution. The counter assertions are the cross-language parity contract with
// the Python twin (Task 11): same policy in → same report counters out.

import test from "node:test";
import assert from "node:assert/strict";
import { AsyncEngine, virtualClock } from "../src/engine.mjs";
import {
  level0Defaults,
  applyPackDefaults,
  outboundDefaults,
  llmDefaults,
} from "../src/defaults.mjs";
import { llmPack, resolveDevCache, DEV_CACHE_TTL } from "../src/packs/llm.mjs";

const ok = (payload) => async () => ({ status: "ok", payload });
const req = (target, args_hash) => ({ v: 1, target, op: target, idempotent: true, args_hash });

test("llmPack implements the adapter-pack four operations", () => {
  assert.deepEqual(llmPack.detect(), { matched: true, name: "llm", confidence: "pinned" });
  assert.deepEqual(llmPack.seams(), []); // no seam of its own — targets come from other seams
  const t = llmPack.targets();
  assert.equal(t.length, 1);
  assert.equal(t[0].pattern, "llm:<provider>");
  assert.equal(t[0].kind, "llm");
  assert.deepEqual(llmPack.defaults(), { defaults: { llm: llmDefaults() } });
});

test("applyPackDefaults merges pack defaults UNDER user config", () => {
  // empty policy → gets the full pack outbound + llm layers.
  const empty = applyPackDefaults({});
  assert.deepEqual(empty.defaults.outbound, outboundDefaults());
  assert.deepEqual(empty.defaults.llm, llmDefaults());

  // a user key replaces the pack's key wholesale; the pack fills the rest.
  const u = applyPackDefaults({ defaults: { llm: { retry: { attempts: 2 } } }, target: { x: {} } });
  assert.deepEqual(u.defaults.llm.retry, { attempts: 2 }, "user retry wins wholesale");
  assert.deepEqual(u.defaults.llm.cache, { mode: "dev" }, "pack cache kept");
  assert.deepEqual(u.defaults.llm.breaker, { failures: 5, cooldown: "30s" }, "pack breaker kept");
  assert.deepEqual(u.target, { x: {} }, "target tables untouched");

  // idempotent on the Level 0 pack itself.
  assert.deepEqual(applyPackDefaults(level0Defaults()), level0Defaults());

  // never mutates its input.
  const orig = { defaults: { llm: { retry: { attempts: 9 } } } };
  applyPackDefaults(orig);
  assert.deepEqual(orig, { defaults: { llm: { retry: { attempts: 9 } } } });
});

test("resolveDevCache: mode=dev → ttl off-prod, removed in prod, explicit ttl kept", () => {
  const raw = () => ({ target: { "llm:openai": { cache: { mode: "dev" } } } });
  assert.deepEqual(resolveDevCache(raw(), {}).target["llm:openai"].cache, { ttl: DEV_CACHE_TTL });
  assert.equal(
    resolveDevCache(raw(), { KEEL_ENV: "prod" }).target["llm:openai"].cache,
    undefined,
    "dev cache is inert in prod"
  );
  // an explicit user ttl is preserved (mode stripped).
  const withTtl = { defaults: { llm: { cache: { mode: "dev", ttl: "5m" } } } };
  assert.deepEqual(resolveDevCache(withTtl, {}).defaults.llm.cache, { ttl: "5m" });
  // non-dev caches and inputs are untouched.
  const plain = { target: { svc: { cache: { ttl: "10s" } } } };
  assert.deepEqual(resolveDevCache(plain, { KEEL_ENV: "prod" }), plain);
});

test("resolveDevCache: persistent flag adds scope=persistent off-prod (Task 14 item 1)", () => {
  const raw = () => ({ target: { "llm:openai": { cache: { mode: "dev" } } } });
  // native + journal ⇒ cross-run replay: the dev cache resolves to a persistent scope.
  assert.deepEqual(resolveDevCache(raw(), {}, { persistent: true }).target["llm:openai"].cache, {
    ttl: DEV_CACHE_TTL,
    scope: "persistent",
  });
  // default (stub / in-memory) is unchanged: no scope key.
  assert.deepEqual(resolveDevCache(raw(), {}).target["llm:openai"].cache, { ttl: DEV_CACHE_TTL });
  // still inert in prod even when persistent is available.
  assert.equal(
    resolveDevCache(raw(), { KEEL_ENV: "prod" }, { persistent: true }).target["llm:openai"].cache,
    undefined
  );
  // an explicitly-set scope is preserved, not overwritten.
  const scoped = { defaults: { llm: { cache: { mode: "dev", scope: "memory" } } } };
  assert.deepEqual(resolveDevCache(scoped, {}, { persistent: true }).defaults.llm.cache, {
    ttl: DEV_CACHE_TTL,
    scope: "memory",
  });
});

test("resolveDevCache treats KEEL_ENV with surrounding whitespace/case as prod (Python parity)", () => {
  const raw = () => ({ target: { "llm:openai": { cache: { mode: "dev" } } } });
  for (const env of [" prod ", "PROD", "  Prod\t"]) {
    assert.equal(
      resolveDevCache(raw(), { KEEL_ENV: env }).target["llm:openai"].cache,
      undefined,
      `KEEL_ENV=${JSON.stringify(env)} must disable the dev cache (fail-closed toward prod)`
    );
  }
  // a non-prod value still keeps the dev cache active.
  assert.deepEqual(resolveDevCache(raw(), { KEEL_ENV: " dev " }).target["llm:openai"].cache, {
    ttl: DEV_CACHE_TTL,
  });
});

test("dev-cache replay parity: identical calls → cache hit off-prod (counters)", async () => {
  const policy = resolveDevCache({ target: { "llm:openai": { cache: { mode: "dev" }, retry: { attempts: 1 } } } }, {});
  const e = new AsyncEngine(virtualClock());
  e.configure(policy);
  let effectCalls = 0;
  const o1 = await e.execute(req("llm:openai", "h"), async () => {
    effectCalls++;
    return { status: "ok", payload: { text: "hi" } };
  });
  const o2 = await e.execute(req("llm:openai", "h"), async () => {
    effectCalls++;
    return { status: "ok", payload: { text: "SHOULD-NOT-RUN" } };
  });
  assert.equal(effectCalls, 1, "second identical call is served from cache");
  assert.equal(o1.from_cache, false);
  assert.equal(o2.from_cache, true);
  assert.deepEqual(o2.payload, { text: "hi" });
  assert.deepStrictEqual(e.report().targets["llm:openai"], {
    attempts: 1,
    breaker_opens: 0,
    breaker_state: "closed",
    cache_hits: 1,
    calls: 2,
    failures: 0,
    retries: 0,
    successes: 2,
    throttled: 0,
  });
});

test("dev-cache replay parity: inert under KEEL_ENV=prod (counters)", async () => {
  const policy = resolveDevCache(
    { target: { "llm:openai": { cache: { mode: "dev" }, retry: { attempts: 1 } } } },
    { KEEL_ENV: "prod" }
  );
  const e = new AsyncEngine(virtualClock());
  e.configure(policy);
  await e.execute(req("llm:openai", "h"), ok({ text: "a" }));
  const o2 = await e.execute(req("llm:openai", "h"), ok({ text: "b" }));
  assert.equal(o2.from_cache, false, "no dev cache in prod");
  assert.deepStrictEqual(e.report().targets["llm:openai"], {
    attempts: 2,
    breaker_opens: 0,
    breaker_state: "closed",
    cache_hits: 0,
    calls: 2,
    failures: 0,
    retries: 0,
    successes: 2,
    throttled: 0,
  });
});

test("full Level 0 path: applyPackDefaults + resolveDevCache gives a working llm dev cache", async () => {
  const policy = resolveDevCache(applyPackDefaults({}), {});
  assert.deepEqual(policy.defaults.llm.cache, { ttl: DEV_CACHE_TTL });
  const e = new AsyncEngine(virtualClock());
  e.configure(policy); // must validate (delegates to KeelCoreStub)
  await e.execute(req("llm:openai", "h"), ok({ text: "hi" }));
  const o2 = await e.execute(req("llm:openai", "h"), ok({ text: "no" }));
  assert.equal(o2.from_cache, true);
  const t = e.report().targets["llm:openai"];
  assert.equal(t.cache_hits, 1);
  assert.equal(t.calls, 2);
  assert.equal(t.attempts, 1);
});
