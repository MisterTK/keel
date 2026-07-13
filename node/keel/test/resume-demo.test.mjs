// The demo that sells the product (dx-spec §1 Level 2): a durable pipeline
// that survives `kill -9` and resumes without re-firing completed steps.
// Node twin of python/keel/tests/test_resume_demo.py.
//
// Orchestrated as a real subprocess (the only honest way to test crash
// recovery):
//   1. Run a 10-step flow that hard-crashes (SIGKILL) right before step 6.
//      Each step appends one line to a shared side-effect log, so the log is
//      a server-side-style invocation count no in-process mock could fake
//      across a `kill -9`.
//   2. After the lease expires, re-run the same `keel-node-run`. Steps 1-5
//      are substituted from the journal (their effects never re-fire — the
//      log gains no duplicate lines); steps 6-10 run live. The flow completes.
//   3. The journal (what `keel flows` reads) shows the flow `completed` with
//      10/10 steps.
//
// Requires the native addon (Tier 2 is native-only); skips cleanly without it.

import test from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, existsSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { loaded as nativeLoaded } from "../../keel-core-native/index.mjs";

const HERE = fileURLToPath(new URL(".", import.meta.url));
const REPO = join(HERE, "..", "..", "..");
const NODE_RUN = join(REPO, "node", "keel", "bin", "keel-node-run.mjs");

const gate = nativeLoaded
  ? {}
  : { skip: "keel-core-native binary absent — build with `cargo build -p keel-node --release`" };

// A 10-step pipeline. `doStep` is a wrapped `ts:` target (zero user changes to
// make it durable): each call is a journaled effect that appends to the
// shared log. `KEEL_DEMO_CRASH_AT=n` self-SIGKILLs right before step n fires.
const PIPELINE = `
import { appendFileSync } from "node:fs";

const LOG = process.env.KEEL_DEMO_LOG;
const CRASH_AT = Number(process.env.KEEL_DEMO_CRASH_AT || "0");

export function doStep(n) {
  appendFileSync(LOG, \`step-\${n}\\n\`);
  return { step: n };
}

export async function main() {
  for (let n = 1; n <= 10; n++) {
    if (CRASH_AT && n === CRASH_AT) process.kill(process.pid, "SIGKILL");
    await doStep(n); // await every intercepted call — see flow.mjs's ordering-rule docs
  }
  console.log("PIPELINE_COMPLETE");
}
`;

const KEEL_TOML = `[flows]
entrypoints = ["ts:pipeline.mjs#main"]

[target."ts:pipeline.mjs#doStep"]
`;

function logSteps(logPath) {
  if (!existsSync(logPath)) return [];
  return readFileSync(logPath, "utf8").split("\n").filter(Boolean);
}

function runOnce(dir, { crashAt } = {}) {
  const env = {
    ...process.env,
    KEEL_DEMO_LOG: join(dir, "effects.log"),
    KEEL_FLOW_LEASE_MS: "800", // short lease so the demo resumes fast
    KEEL_QUIET: "1",
    KEEL_BACKEND: "native",
  };
  if (crashAt !== undefined) env.KEEL_DEMO_CRASH_AT = String(crashAt);
  return spawnSync(process.execPath, [NODE_RUN, "pipeline.mjs"], {
    cwd: dir,
    env,
    encoding: "utf8",
  });
}

async function flowStatus(journalPath) {
  let DatabaseSync;
  try {
    ({ DatabaseSync } = await import("node:sqlite"));
  } catch {
    return null; // node:sqlite unavailable (older Node) — caller skips the check
  }
  const db = new DatabaseSync(journalPath, { readOnly: true });
  try {
    const status = db.prepare("SELECT status FROM flows").get().status;
    const steps = db.prepare("SELECT COUNT(*) AS n FROM steps WHERE kind != 'marker'").get().n;
    return { status, steps };
  } finally {
    db.close();
  }
}

test("kill -9 then resume completes without refiring effects", gate, async () => {
  const dir = mkdtempSync(join(tmpdir(), "keel-node-resume-demo-"));
  try {
    writeFileSync(join(dir, "pipeline.mjs"), PIPELINE);
    writeFileSync(join(dir, "keel.toml"), KEEL_TOML);
    const log = join(dir, "effects.log");

    // Run 1: crash (kill -9) right before step 6.
    const run1 = runOnce(dir, { crashAt: 6 });
    assert.equal(run1.signal, "SIGKILL", `expected SIGKILL; stderr=${run1.stderr}`);
    assert.deepEqual(
      logSteps(log),
      Array.from({ length: 5 }, (_, i) => `step-${i + 1}`),
      "run 1 fired exactly steps 1-5 before the crash"
    );

    // Let run 1's lease expire so the resume can steal it.
    await new Promise((r) => setTimeout(r, 1500));

    // Run 2: resume. Steps 1-5 are substituted (no new log lines); 6-10 fire.
    const run2 = runOnce(dir);
    assert.equal(run2.status, 0, `resume failed; stderr=${run2.stderr}`);
    assert.match(run2.stdout, /PIPELINE_COMPLETE/);
    assert.deepEqual(
      logSteps(log),
      Array.from({ length: 10 }, (_, i) => `step-${i + 1}`),
      "each step fired EXACTLY ONCE across both runs — 1-5 substituted on resume"
    );

    // The journal (what `keel flows` reads) shows the flow completed, 10 steps.
    const status = await flowStatus(join(dir, ".keel", "journal.db"));
    if (status) {
      assert.equal(status.status, "completed");
      assert.equal(status.steps, 10);
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
