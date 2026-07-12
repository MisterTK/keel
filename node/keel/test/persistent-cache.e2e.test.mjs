// Task 14 item 1 (load-bearing): cross-RUN dev-cache replay under the native
// core + attached journal. Two SEPARATE `node --import keel/hook` processes hit
// the same LLM-style endpoint with an identical request; the dev cache resolves
// to `scope = "persistent"` (native + journal), so the SECOND run replays from
// `.keel/journal.db` and makes ZERO API calls for the repeated prompt — the
// "repeated run costs ~0" promise, proven end to end across processes.
//
// Auto-skips when the native addon is absent (build: `cargo build -p keel-node
// --release`) — the persistent scope is a native-only capability.

import test from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { spawn } from "node:child_process";
import { mkdtempSync, writeFileSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { loaded as nativeLoaded } from "../../keel-core-native/index.mjs";

const hookUrl = new URL("../hook.mjs", import.meta.url).href;
const gate = nativeLoaded
  ? {}
  : { skip: "keel-core-native binary absent — build with `cargo build -p keel-node --release`" };

function startCountingServer() {
  let hits = 0;
  const server = createServer((_req, res) => {
    hits += 1;
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ reply: "hello", served: hits }));
  });
  return new Promise((resolve) => {
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address();
      resolve({
        url: `http://127.0.0.1:${port}/v1/chat/completions`,
        hits: () => hits,
        close: () => new Promise((r) => server.close(r)),
      });
    });
  });
}

test("dev cache replays across two separate native runs (API calls == 0 on the repeat)", gate, async () => {
  const server = await startCountingServer();
  const dir = mkdtempSync(join(tmpdir(), "keel-persist-"));
  try {
    // A dev cache on the local target: resolveDevCache turns mode="dev" into a
    // concrete ttl and — because the native backend is persistent — scope=persistent.
    writeFileSync(
      join(dir, "keel.toml"),
      `[target."127.0.0.1"]\ncache = { mode = "dev" }\nretry = { attempts = 1 }\n`
    );
    writeFileSync(
      join(dir, "app.mjs"),
      `const r = await fetch(process.env.KEEL_DEMO_URL);\n` +
        `const body = await r.text();\n` +
        `process.stdout.write("BODY:" + body + " CACHE:" + Boolean(r.keelOutcome?.from_cache) + "\\n");\n`
    );

    // MUST be async spawn (not spawnSync): the counting server runs in THIS
    // process's event loop, so blocking it would stop the server from answering
    // the child's fetch.
    const runOnce = () =>
      new Promise((resolve) => {
        const child = spawn(process.execPath, ["--import", hookUrl, "app.mjs"], {
          cwd: dir,
          env: {
            ...process.env,
            KEEL_BACKEND: "native",
            KEEL_QUIET: "1",
            KEEL_ENV: "", // off-prod, so the dev cache is active
            KEEL_DEMO_URL: server.url,
          },
        });
        let stdout = "";
        let stderr = "";
        child.stdout.on("data", (d) => (stdout += d));
        child.stderr.on("data", (d) => (stderr += d));
        child.on("close", (status) => resolve({ status, stdout, stderr }));
      });

    const run1 = await runOnce();
    assert.equal(run1.status, 0, `run1 failed:\n${run1.stderr}`);
    assert.match(run1.stdout, /BODY:\{"reply":"hello","served":1\} CACHE:false/, run1.stdout);
    assert.ok(existsSync(join(dir, ".keel", "journal.db")), "journal.db written on run1");
    assert.equal(server.hits(), 1, "run1 makes exactly one API call");

    const run2 = await runOnce();
    assert.equal(run2.status, 0, `run2 failed:\n${run2.stderr}`);
    // Same body as run1, served from the persistent journal — from_cache=true and
    // NO new API call (the whole point: a repeated prompt costs ~0 across runs).
    assert.match(run2.stdout, /BODY:\{"reply":"hello","served":1\} CACHE:true/, run2.stdout);
    assert.equal(server.hits(), 1, "run2 replays from the persistent cache — 0 API calls");
  } finally {
    rmSync(dir, { recursive: true, force: true });
    await server.close();
  }
});
