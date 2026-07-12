// Environment-variable parsing parity with the Python front end: KEEL_DISABLE /
// KEEL_QUIET are trimmed + lowercased before the {1,true,yes} check, and
// KEEL_BACKEND is validated loudly (auto|native|stub) instead of silently
// falling back to auto.

import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { isDisabled } from "../src/bootstrap.mjs";
import { loadBackend } from "../src/backend.mjs";
import { KeelError } from "../src/engine.mjs";

test("isDisabled trims + lowercases like the Python twin's .strip().lower()", () => {
  for (const v of ["1", "true", "yes", " TRUE ", "Yes", "  1\t", "YES"])
    assert.equal(isDisabled({ KEEL_DISABLE: v }), true, JSON.stringify(v));
  for (const v of ["", "0", "no", "false", "off", "  ", undefined])
    assert.equal(isDisabled({ KEEL_DISABLE: v }), false, JSON.stringify(v));
  assert.equal(isDisabled({}), false);
});

test("loadBackend rejects an invalid KEEL_BACKEND with KEEL-E040 (parity with Python)", async () => {
  await assert.rejects(
    () => loadBackend({ preferred: "banana" }),
    (e) => {
      assert.ok(e instanceof KeelError);
      assert.equal(e.code, "KEEL-E040");
      assert.match(e.message, /auto\|native\|stub/);
      return true;
    }
  );
});

test("KEEL_BACKEND=stub forces the in-repo engine; empty/unset normalizes to auto", async () => {
  const stub = await loadBackend({ preferred: "stub" });
  assert.equal(stub.kind, "node-stub");

  const dir = mkdtempSync(join(tmpdir(), "keel-envparity-"));
  try {
    // Empty string → "auto": must resolve a backend (native if built, else stub)
    // WITHOUT throwing. KEEL_JOURNAL="" keeps any native core in-memory.
    const auto = await loadBackend({ preferred: "", cwd: dir, env: { KEEL_JOURNAL: "" } });
    assert.ok(["native", "node-stub"].includes(auto.kind), `auto resolved to ${auto.kind}`);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
