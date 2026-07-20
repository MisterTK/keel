// Farm certification for the `child_process` pack (issue #27, Node half of
// chunk-8). Gated on `KEEL_ADAPTER_FARM=1` and the native core being built.
//
// child_process is a stdlib builtin, so — like the urllib pack — there is no
// pinned third-party version to certify. What this leg DOES certify against the
// live Node + the real native binary is the end-to-end stack the offline test
// can only fake: the `createRequire("node:child_process")` patch mechanism, a
// REAL synchronous `spawnSync` driven through a REAL native durable flow over a
// REAL on-disk journal, and the at-most-once DISPATCH guarantee across a
// simulated process restart (the documented v1 replay-skip limit surfacing as
// a loud refusal, not a re-run).
//
// Run locally:
//   KEEL_ADAPTER_FARM=1 node --test node/keel/test/child-process-farm.test.mjs

import test from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { loadBackend } from "../src/backend.mjs";
import {
  compileCmdMatchers,
  patchChildProcess,
  argsHashWithCwd,
  KeelCmdFlowReplayUnsupportedError,
} from "../src/packs/child-process.mjs";
import { loaded as nativeLoaded } from "../../keel-core-native/index.mjs";

const FARM = ["1", "true", "yes"].includes(String(process.env.KEEL_ADAPTER_FARM ?? "").toLowerCase());
const gate =
  FARM && nativeLoaded
    ? {}
    : { skip: FARM ? "keel-core-native binary absent — build with `cargo build -p keel-node --release`" : "KEEL_ADAPTER_FARM!=1" };

test(
  "farm: a matched spawnSync runs as a real durable flow, and a same-identity re-run is fenced (at-most-once)",
  gate,
  async (t) => {
    const dir = mkdtempSync(join(tmpdir(), "keel-cp-farm-"));
    t.after(() => rmSync(dir, { recursive: true, force: true }));
    const journalPath = join(dir, "journal.db");
    const env = { KEEL_JOURNAL: journalPath };
    const compiled = compileCmdMatchers({
      "cmd:probe": { name: "cmd:probe", argvPatterns: ["*", "-e", "*"], onBusy: "fail" },
    });
    const cp = createRequire(import.meta.url)("node:child_process");
    const argv = [process.execPath, "-e", "process.stdout.write('run1')"];

    // --- Run 1: fresh native backend over a real journal; the command runs. ---
    const b1 = await loadBackend({ preferred: "native", cwd: dir, env });
    assert.equal(b1.persistent, true, "the native journal attached (persistent)");
    b1.configure({});
    const undo1 = patchChildProcess(cp, { compiled, backend: b1, env });
    try {
      const r = cp.spawnSync(argv[0], argv.slice(1), { encoding: "utf8" });
      assert.equal(r.status, 0);
      assert.equal(r.stdout, "run1", "the real command actually ran under the flow");
    } finally {
      undo1();
    }

    // --- Run 2: a NEW core over the SAME journal (models a process restart). ---
    // The identity (argv + cwd) is Completed, so enterFlow replays — and the
    // pack refuses to re-run rather than silently re-dispatching the command.
    const b2 = await loadBackend({ preferred: "native", cwd: dir, env });
    b2.configure({});
    const undo2 = patchChildProcess(cp, { compiled, backend: b2, env });
    try {
      let reran = false;
      assert.throws(
        () => {
          reran = true;
          // The SAME argv as run 1 → same identity → already Completed → fenced.
          cp.spawnSync(argv[0], argv.slice(1), { encoding: "utf8" });
        },
        (e) => e instanceof KeelCmdFlowReplayUnsupportedError && e.code === "KEEL-E005",
        "a completed identity must be fenced, not re-run"
      );
      assert.ok(reran, "sanity: the wrapped call was actually invoked");
    } finally {
      undo2();
    }

    // The identity is cwd-inclusive: a differing argv is a fresh flow, not fenced.
    assert.notEqual(
      argsHashWithCwd(argv, dir),
      argsHashWithCwd([process.execPath, "-e", "different"], dir)
    );
  }
);
