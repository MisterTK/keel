// The bootstrap's FRAMEWORK_PACKS wiring: every framework/library pack is
// attempted (best-effort), and KEEL_DISABLE short-circuits before any of them
// run. None of pg/ioredis/mysql2/@modelcontextprotocol/sdk are installed in
// this repo, so every pack is expected to report inactive — this test locks
// in that the bootstrap still tries each one and surfaces it in `packs`
// (consumed by the startup banner), rather than special-casing any one pack.

import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { installKeel } from "../src/bootstrap.mjs";

test("installKeel wires every framework pack and disables cleanly under KEEL_DISABLE", async () => {
  const dir = mkdtempSync(join(tmpdir(), "keel-bootstrap-"));
  try {
    const disabled = await installKeel({ cwd: dir, env: { KEEL_DISABLE: "1" } });
    assert.deepEqual(disabled, { enabled: false, reason: "KEEL_DISABLE" });

    const result = await installKeel({ cwd: dir, env: { KEEL_QUIET: "1" } });
    assert.equal(result.enabled, true);
    const labels = result.packs.map((p) => p.label).sort();
    assert.deepEqual(labels, ["ioredis", "mcp: transports", "mysql2", "pg"]);
    for (const p of result.packs) {
      assert.equal(p.active, false, `${p.label} should be inactive — not installed in this repo`);
    }

    // The module-level install guard makes a second call a cheap no-op.
    const again = await installKeel({ cwd: dir, env: {} });
    assert.deepEqual(again, { enabled: true, reason: "already-installed" });
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
