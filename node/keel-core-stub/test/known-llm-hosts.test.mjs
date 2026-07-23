// `KeelCoreStub.knownLlmHosts()` (issue #49): a class-level enumeration of
// the same LLM host map `resolveTarget`'s tier 1 consults, so front-end packs
// can list every known provider host (`keel doctor`/`keel init` documentation
// output) without holding their own copy. Every pair it lists must agree with
// `resolveTarget`'s single-lookup form.

import test from "node:test";
import assert from "node:assert/strict";
import { KeelCoreStub } from "../index.mjs";

test("knownLlmHosts is a static, no-instance-required accessor", () => {
  const hosts = KeelCoreStub.knownLlmHosts();
  assert.ok(Array.isArray(hosts));
  assert.ok(hosts.length > 0);
});

test("knownLlmHosts includes every documented provider host", () => {
  const hostSet = new Set(KeelCoreStub.knownLlmHosts().map(([host]) => host));
  for (const host of ["api.openai.com", "api.anthropic.com", "generativelanguage.googleapis.com"]) {
    assert.ok(hostSet.has(host), `expected ${host} in knownLlmHosts()`);
  }
});

test("every knownLlmHosts pair agrees with resolveTarget's single-lookup form", () => {
  const core = new KeelCoreStub();
  for (const [host, provider] of KeelCoreStub.knownLlmHosts()) {
    assert.equal(core.resolveTarget("GET", host), `llm:${provider}`);
  }
});
