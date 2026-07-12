// Discovery persists the backend's per-target report into .keel/discovery.db
// via node:sqlite (zero native deps). Verifies schema, accumulated counters,
// and host evidence.

import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";
import { AsyncEngine, virtualClock } from "../src/engine.mjs";
import { createDiscovery } from "../src/discovery.mjs";
import { level0Defaults } from "../src/defaults.mjs";

const require = createRequire(import.meta.url);
const { DatabaseSync } = require("node:sqlite");

test("flush writes an aggregate row per target with correct counters", async () => {
  const dir = mkdtempSync(join(tmpdir(), "keel-disc-"));
  try {
    const backend = new AsyncEngine(virtualClock());
    backend.configure(level0Defaults());
    // one success, one retry-then-success, one non-idempotent failure
    await backend.execute(
      { v: 1, target: "api.example.com", op: "GET", idempotent: true },
      async () => ({ status: "ok", payload: "a" })
    );
    let i = 0;
    await backend.execute(
      { v: 1, target: "api.example.com", op: "GET", idempotent: true },
      async () => (i++ === 0 ? { status: "error", class: "http", http_status: 503 } : { status: "ok", payload: "b" })
    );
    await backend.execute(
      { v: 1, target: "api.example.com", op: "POST", idempotent: false },
      async () => ({ status: "error", class: "http", http_status: 503 })
    );

    const discovery = createDiscovery(dir);
    discovery.observe("api.example.com", "api.example.com");
    const wrote = discovery.flushSync(backend.report());
    assert.equal(wrote, true);

    const db = new DatabaseSync(join(dir, ".keel", "discovery.db"));
    try {
      const row = db.prepare("SELECT * FROM discovery WHERE target=?").get("api.example.com");
      assert.equal(row.calls, 3);
      assert.equal(row.successes, 2);
      assert.equal(row.failures, 1);
      assert.equal(row.retries, 1);
      assert.equal(row.attempts, 4); // 1 + 2 + 1
      assert.deepEqual(JSON.parse(row.hosts), ["api.example.com"]);
      const meta = db.prepare("SELECT value FROM meta WHERE key='schema_version'").get();
      assert.equal(meta.value, "1");
    } finally {
      db.close();
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("flush is a no-op (returns false) when there is nothing observed", () => {
  const dir = mkdtempSync(join(tmpdir(), "keel-disc-"));
  try {
    const discovery = createDiscovery(dir);
    assert.equal(discovery.flushSync({ v: 1, targets: {} }), false);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
