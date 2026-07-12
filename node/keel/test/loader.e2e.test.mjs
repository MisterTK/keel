// End-to-end: the ESM loader wraps a fixture module's named function export so
// it routes through the backend. A configured retry (retry.on includes "other")
// turns a fail-once function into a success, proving the transform + main-thread
// runtime + backend are wired together. Discovery rows are written for the
// ts: target. Runs in a child process so the real --import + module.register
// path is exercised.

import test from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, writeFileSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

const hookUrl = new URL("../hook.mjs", import.meta.url).href;
const require = createRequire(import.meta.url);
const { DatabaseSync } = require("node:sqlite");

test("loader wraps a ts: function target and drives it through retry", () => {
  const dir = mkdtempSync(join(tmpdir(), "keel-loader-"));
  try {
    writeFileSync(
      join(dir, "keel.toml"),
      `[target."ts:mod.mjs#flaky"]\nretry = { attempts = 3, schedule = "fixed(1ms)", on = ["other"] }\n`
    );
    writeFileSync(
      join(dir, "mod.mjs"),
      `let n = 0;\nexport async function flaky() {\n  n++;\n  if (n === 1) throw new Error("boom-once");\n  return "ok-after-" + n;\n}\n`
    );
    writeFileSync(
      join(dir, "app.mjs"),
      `import { flaky } from "./mod.mjs";\nconst r = await flaky();\nprocess.stdout.write("RESULT:" + r + "\\n");\n`
    );

    const run = spawnSync(process.execPath, ["--import", hookUrl, "app.mjs"], {
      cwd: dir,
      env: { ...process.env, KEEL_DISABLE: undefined, KEEL_QUIET: undefined },
      encoding: "utf8",
    });

    assert.equal(run.status, 0, `child failed:\n${run.stderr}`);
    // fail-once function succeeded on the 2nd attempt → the wrapper retried it.
    assert.match(run.stdout, /RESULT:ok-after-2/);
    // banner reflects the wrapped function target, on stderr only.
    assert.match(run.stderr, /1 function target/);
    assert.match(run.stderr, /policy keel\.toml/);

    const dbPath = join(dir, ".keel", "discovery.db");
    assert.ok(existsSync(dbPath), "discovery.db should be written on exit");
    const db = new DatabaseSync(dbPath);
    try {
      const row = db.prepare("SELECT * FROM discovery WHERE target=?").get("ts:mod.mjs#flaky");
      assert.ok(row, "discovery row for the ts: target");
      assert.equal(row.calls, 1);
      assert.equal(row.successes, 1);
      assert.equal(row.retries, 1);
    } finally {
      db.close();
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("KEEL_DISABLE skips the loader entirely (function runs unwrapped)", () => {
  const dir = mkdtempSync(join(tmpdir(), "keel-loader-off-"));
  try {
    writeFileSync(
      join(dir, "keel.toml"),
      `[target."ts:mod.mjs#flaky"]\nretry = { attempts = 3, on = ["other"] }\n`
    );
    writeFileSync(
      join(dir, "mod.mjs"),
      `let n = 0;\nexport async function flaky() {\n  n++;\n  if (n === 1) throw new Error("boom-once");\n  return "ok-after-" + n;\n}\n`
    );
    writeFileSync(
      join(dir, "app.mjs"),
      `import { flaky } from "./mod.mjs";\ntry { await flaky(); process.stdout.write("UNEXPECTED\\n"); }\ncatch (e) { process.stdout.write("THREW:" + e.message + "\\n"); }\n`
    );
    const run = spawnSync(process.execPath, ["--import", hookUrl, "app.mjs"], {
      cwd: dir,
      env: { ...process.env, KEEL_DISABLE: "1" },
      encoding: "utf8",
    });
    assert.equal(run.status, 0, run.stderr);
    // Unwrapped: the first call throws and is not retried.
    assert.match(run.stdout, /THREW:boom-once/);
    assert.equal(run.stderr, "", "disabled run emits no banner");
    assert.ok(!existsSync(join(dir, ".keel", "discovery.db")), "no discovery when disabled");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
