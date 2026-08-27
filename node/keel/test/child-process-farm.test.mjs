// Farm certification for the `child_process` pack (issue #27, Node half of
// chunk-8). Gated on `KEEL_ADAPTER_FARM=1` and the native core being built.
//
// child_process is a stdlib builtin, so — like the urllib pack — there is no
// pinned third-party version to certify. What this leg DOES certify against the
// live Node + the real native binary is the end-to-end stack the offline test
// can only fake: the `createRequire("node:child_process")` patch mechanism, a
// REAL synchronous `spawnSync` driven through a REAL native durable flow over a
// REAL on-disk journal, and the at-most-once DISPATCH guarantee across a
// simulated process restart — which since issue #42 means the recorded result
// is SUBSTITUTED (no second process), not refused.
//
// Run locally:
//   KEEL_ADAPTER_FARM=1 node --test node/keel/test/child-process-farm.test.mjs

import test from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { loadBackend } from "../src/backend.mjs";
import { compileCmdMatchers, patchChildProcess, argsHashWithCwd } from "../src/packs/child-process.mjs";
import { loaded as nativeLoaded } from "../../keel-core-native/index.mjs";

const FARM = ["1", "true", "yes"].includes(String(process.env.KEEL_ADAPTER_FARM ?? "").toLowerCase());
const gate =
  FARM && nativeLoaded
    ? {}
    : { skip: FARM ? "keel-core-native binary absent — build with `cargo build -p keel-node --release`" : "KEEL_ADAPTER_FARM!=1" };

test(
  "farm: a matched spawnSync runs as a real durable flow, and a same-identity re-run is SUBSTITUTED (at-most-once)",
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
    // The child APPENDS one byte to a witness file on every real execution, and
    // writes the same stdout each time. Because the argv (and therefore the flow
    // identity) must be byte-identical across both runs, the witness file — not
    // a differing stdout — is what proves the second dispatch spawned nothing.
    const witness = join(dir, "ran.txt");
    const script = `require('fs').appendFileSync(${JSON.stringify(witness)},'x');process.stdout.write('run1')`;
    const argv = [process.execPath, "-e", script];
    const timesRan = () => {
      try {
        return readFileSync(witness, "utf8").length;
      } catch {
        return 0;
      }
    };

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
    assert.equal(timesRan(), 1, "sanity: the child executed exactly once so far");

    // --- Run 2: a NEW core over the SAME journal (models a process restart). ---
    // The identity (argv + cwd) is Completed, so the core substitutes the
    // recorded step instead of firing the effect: the caller gets run 1's real
    // result back and NO second process is spawned (issue #42 — this used to
    // throw KeelCmdFlowReplayUnsupportedError, the v1 FFI limit).
    const b2 = await loadBackend({ preferred: "native", cwd: dir, env });
    b2.configure({});
    const undo2 = patchChildProcess(cp, { compiled, backend: b2, env });
    let r2;
    try {
      r2 = cp.spawnSync(argv[0], argv.slice(1), { encoding: "utf8" });
    } finally {
      undo2();
    }
    assert.equal(r2.status, 0, "the recorded exit status is substituted");
    assert.equal(r2.stdout, "run1", "the recorded stdout is substituted verbatim");
    assert.equal(r2.stderr, "", "the recorded (empty) stderr round-trips as a string, not a Buffer");
    assert.equal(timesRan(), 1, "AT-MOST-ONCE: the child did NOT run a second time");

    // The identity is cwd-inclusive: a differing argv is a fresh flow, not fenced.
    assert.notEqual(
      argsHashWithCwd(argv, dir),
      argsHashWithCwd([process.execPath, "-e", "different"], dir)
    );
  }
);
