// Discovery must survive the way a dev server is actually stopped: Ctrl-C /
// SIGTERM. `process.once("exit")` alone does NOT fire under default signal
// disposition, so a signalled child used to lose every buffered discovery row.
// This drives a real child that buffers a row, installs the exit flush, then is
// SIGTERM'd — and asserts the row landed in .keel/discovery.db AND that the
// signal was not swallowed (the child exits via SIGTERM, not code 0).

import test from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const discoveryUrl = new URL("../src/discovery.mjs", import.meta.url).href;
const bootstrapUrl = new URL("../src/bootstrap.mjs", import.meta.url).href;

test("a SIGTERM'd child still persists buffered discovery rows (and does not swallow the signal)", async () => {
  const dir = mkdtempSync(join(tmpdir(), "keel-sigflush-"));
  const childPath = join(dir, "child.mjs");
  writeFileSync(
    childPath,
    `import { createDiscovery } from ${JSON.stringify(discoveryUrl)};\n` +
      `import { installExitFlush } from ${JSON.stringify(bootstrapUrl)};\n` +
      `const d = createDiscovery(process.env.KEEL_DIR);\n` +
      `d.observe("api.example.com", { v: 1, result: "ok", attempts: 1, from_cache: false }, 5);\n` +
      `installExitFlush(d);\n` +
      `process.stdout.write("READY\\n");\n` +
      `setInterval(() => {}, 1000); // keep the loop alive until signalled\n`
  );
  try {
    const child = spawn(process.execPath, [childPath], {
      cwd: dir,
      env: { ...process.env, KEEL_DIR: dir },
    });
    // Wait for the child to buffer its row and install the flush.
    await new Promise((resolve, reject) => {
      child.stdout.setEncoding("utf8");
      child.stdout.on("data", (d) => d.includes("READY") && resolve());
      child.on("error", reject);
      child.on("exit", () => reject(new Error("child exited before READY")));
    });

    const exited = new Promise((resolve) => child.on("close", (code, signal) => resolve({ code, signal })));
    child.kill("SIGTERM");
    const { code, signal } = await exited;
    // Default disposition preserved: terminated BY the signal, not a clean exit
    // and not a swallowed no-op.
    assert.equal(signal, "SIGTERM", `child should terminate via SIGTERM (got code=${code}, signal=${signal})`);

    // The buffered row must have been flushed to the canonical discovery.db.
    const { DatabaseSync } = await import("node:sqlite");
    const db = new DatabaseSync(join(dir, ".keel", "discovery.db"));
    try {
      const rows = db.prepare("SELECT target, calls, successes FROM discovery").all();
      assert.equal(rows.length, 1, "exactly one target persisted");
      assert.equal(rows[0].target, "api.example.com");
      assert.equal(rows[0].calls, 1);
      assert.equal(rows[0].successes, 1);
    } finally {
      db.close();
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
