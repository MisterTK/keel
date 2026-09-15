// The `refused` module-level latch (bootstrap.mjs) is, by design, an
// unconditional per-process singleton — like the pre-existing `installed`
// latch next to it, it has no reset hook, and (also like `installed`) a set
// value answers EVERY later installKeel() call regardless of that call's own
// args. That makes it incompatible with sharing a process with any other
// in-process installKeel() test (bootstrap-packs.test.mjs's test relies on a
// SUCCESSFUL install persisting; a refused install here would corrupt it, and
// vice versa) — hence its own file, so node --test's per-file process
// isolation keeps the two apart without inventing a reset mechanism.

import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { installKeel } from "../src/bootstrap.mjs";

test("KEEL_CWD refusal is remembered module-wide — a second call returns the same object without printing again", async () => {
  const dir = mkdtempSync(join(tmpdir(), "keel-bootstrap-refuse-"));
  const writes = [];
  const originalWrite = process.stderr.write.bind(process.stderr);
  process.stderr.write = (chunk) => {
    writes.push(String(chunk));
    return true;
  };
  try {
    const first = await installKeel({ cwd: dir, env: {}, cwdSource: "KEEL_CWD" });
    const second = await installKeel({ cwd: dir, env: {}, cwdSource: "KEEL_CWD" });
    assert.deepEqual(first, { enabled: false, reason: "policy-missing-at-keel-cwd", root: dir });
    assert.deepEqual(second, { enabled: false, reason: "policy-missing-at-keel-cwd", root: dir });
    const keelLines = writes.join("").split("\n").filter((l) => l.startsWith("keel ▸"));
    assert.equal(keelLines.length, 1, `expected exactly one keel line across both calls: ${writes.join("")}`);
  } finally {
    process.stderr.write = originalWrite;
    rmSync(dir, { recursive: true, force: true });
  }
});
