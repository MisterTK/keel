// MCP pack against the REAL native core (crates/keel-node), not the stub. Same
// Critical seam as the AI-SDK pack: the live MCP result and the original McpError
// must be held SIDE-BAND, never sent through the core, or the native serde
// round-trip turns a successful call whose result holds a function/stream into
// KEEL-E015 and strips the original error's identity on failure.
//
// Auto-skips when the native addon is absent (build: `cargo build -p keel-node
// --release`).

import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { loadBackend } from "../src/backend.mjs";
import { makeWrappedRequest } from "../src/packs/mcp.mjs";
import { loaded as nativeLoaded } from "../../keel-core-native/index.mjs";

const gate = nativeLoaded
  ? {}
  : { skip: "keel-core-native binary absent — build with `cargo build -p keel-node --release`" };

async function nativeBackend(policy) {
  const dir = mkdtempSync(join(tmpdir(), "keel-mcp-native-"));
  const backend = await loadBackend({ preferred: "native", cwd: dir, env: { KEEL_JOURNAL: "" } });
  assert.equal(backend.kind, "native", "native backend must load for this test");
  backend.configure(policy);
  return { backend, cleanup: () => rmSync(dir, { recursive: true, force: true }) };
}

function clientWith(original, backend, name = "svc") {
  return {
    getServerVersion: () => ({ name, version: "0" }),
    request: makeWrappedRequest(original, { backend }),
  };
}

test("native: a successful MCP result (with a live function) survives the core by identity", gate, async () => {
  const { backend, cleanup } = await nativeBackend({
    target: { "mcp:svc": { retry: { attempts: 1 } } },
  });
  try {
    // The result holds a live function — it would make the native serde fail
    // (KEEL-E015) if the pack sent it through the core instead of side-banding it.
    const live = { tools: [{ name: "x" }], _handle: () => "live" };
    const client = clientWith(async () => live, backend);
    const res = await client.request({ method: "tools/list", params: {} }, null, {});
    assert.equal(res, live, "the live MCP result is returned by identity (function intact)");
    assert.equal(res._handle(), "live");
    assert.equal(res.keelOutcome.result, "ok", "a successful call is not turned into KEEL-E015");
  } finally {
    cleanup();
  }
});

test("native: tools/call error is observed (KEEL-E014) and the original propagates by identity", gate, async () => {
  const { backend, cleanup } = await nativeBackend({
    target: { "mcp:svc": { retry: { attempts: 3, schedule: "fixed(1ms)", on: ["conn"] } } },
  });
  try {
    const reset = Object.assign(new Error("connection reset"), { name: "McpError", code: "ECONNRESET" });
    let n = 0;
    const client = clientWith(async () => {
      n++;
      throw reset;
    }, backend);
    await assert.rejects(
      () => client.request({ method: "tools/call", params: { name: "charge" } }, null, {}),
      (e) => {
        assert.equal(e, reset, "the ORIGINAL McpError identity crosses the native core");
        assert.equal(e.keelOutcome.error.code, "KEEL-E014", "side-effecting tools/call is observed, not retried");
        return true;
      }
    );
    assert.equal(n, 1, "a non-idempotent call is attempted exactly once");
  } finally {
    cleanup();
  }
});

test("native: read-ish method retries a conn error then succeeds (stub-parity)", gate, async () => {
  const { backend, cleanup } = await nativeBackend({
    target: { "mcp:svc": { retry: { attempts: 3, schedule: "fixed(1ms)", on: ["conn"] } } },
  });
  try {
    let n = 0;
    const ok = { resources: [] };
    const client = clientWith(async () => {
      if (++n === 1) throw Object.assign(new Error("connection closed"), { code: "ECONNRESET" });
      return ok;
    }, backend);
    const res = await client.request({ method: "resources/list", params: {} }, null, {});
    assert.equal(res, ok, "the retried live result is returned by identity");
    assert.equal(n, 2, "retried once per policy");
    assert.equal(res.keelOutcome.attempts, 2);
  } finally {
    cleanup();
  }
});
