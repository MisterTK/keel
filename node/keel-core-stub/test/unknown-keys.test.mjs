// The stub must reject unknown policy keys with KEEL-E001 and a path, mirroring
// the real core / Rust stub's `#[serde(deny_unknown_fields)]` and the frozen
// schema's additionalProperties:false. Previously unknown keys were silently
// dropped, so a typo'd `retry = { atempts = 10 }` ran the target on defaults the
// user never asked for — the exact "silent surprise" E001 exists to prevent.

import test from "node:test";
import assert from "node:assert/strict";
import { KeelCoreStub, KeelError } from "../index.mjs";

const rejects = (policy) => {
  const core = new KeelCoreStub();
  try {
    core.configure(policy);
  } catch (e) {
    assert.ok(e instanceof KeelError, `expected KeelError, got ${e}`);
    assert.equal(e.code, "KEEL-E001");
    return e.message;
  }
  assert.fail(`expected configure to reject ${JSON.stringify(policy)}`);
};

test("a typo'd nested key is rejected with its path (the failure scenario)", () => {
  const msg = rejects({ target: { "api.stripe.com": { retry: { atempts: 10 } } } });
  assert.match(msg, /atempts/, "the offending key is named in the error");
});

test("a mistyped layer table is rejected, not dropped", () => {
  rejects({ target: { "api.x": { retrys: {} } } });
});

test("unknown top-level and defaults keys are rejected", () => {
  rejects({ bogus_top: true });
  rejects({ defaults: { outboundd: {} } });
  rejects({ target: { x: { breaker: { failuers: 5 } } } });
  rejects({ target: { x: { cache: { tt1: "10m" } } } });
});

test("valid documents with every schema key still configure cleanly", () => {
  const core = new KeelCoreStub();
  core.configure({
    defaults: {
      outbound: { timeout: "30s", retry: { attempts: 3, on: ["conn"] }, breaker: { failures: 5, cooldown: "15s" } },
      llm: { cache: { ttl: "24h", scope: "persistent" } },
    },
    target: {
      "api.stripe.com": { idempotency: { header: "X-Request-Token" }, rate: "9/s" },
    },
    // Accepted top-level keys (inert in the stub, as in the core).
    journal: "file:.keel/journal.db",
    telemetry: { console: true },
    flows: { entrypoints: [] },
  });
});
