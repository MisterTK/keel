// Smoke test for demos/node-service: executes the shipped demo app against
// tools/faultproxy so the Node demo can't silently rot. Bare `fetch` dies on
// the 500; `keel run` (keel-node-run) retries it and it prints "service ok".
//
// Needs python3 (for faultproxy). Skips cleanly when python3 is unavailable
// (e.g. a node-only CI runner) — the demo is still covered where python3 exists.

import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, existsSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { setTimeout as sleep } from "node:timers/promises";

const HERE = fileURLToPath(new URL(".", import.meta.url));
const REPO = join(HERE, "..", "..", "..");
const PY = process.env.KEEL_PYTHON || "python3";
const hasPython = spawnSync(PY, ["--version"]).status === 0;
const APP = join(REPO, "demos", "node-service", "app.mjs");
const SCENARIO = join(REPO, "demos", "node-service", "scenario.json");
const FAULTPROXY = join(REPO, "tools", "faultproxy", "faultproxy.py");
const NODE_RUN = join(REPO, "node", "keel", "bin", "keel-node-run.mjs");

test("node-service demo: bare fails on 500, keel run survives", { skip: !hasPython }, async () => {
  const work = mkdtempSync(join(tmpdir(), "keel-node-demo-"));
  const portFile = join(work, "port");
  const proxy = spawn(PY, [FAULTPROXY, "--scenario", SCENARIO, "--port", "0", "--port-file", portFile]);
  try {
    for (let i = 0; i < 50 && !existsSync(portFile); i++) await sleep(100);
    const port = readFileSync(portFile, "utf8").trim();
    const url = `http://127.0.0.1:${port}/svc`;
    const env = { ...process.env, KEEL_DEMO_URL: url, KEEL_QUIET: "1" };

    const bare = spawnSync(process.execPath, [APP], { env, encoding: "utf8" });
    assert.notEqual(bare.status, 0, "bare fetch must throw on the 500");

    // Rewind faultproxy so `keel run` sees 500-then-200 again.
    spawnSync(PY, ["-c", `import urllib.request; urllib.request.urlopen(urllib.request.Request('http://127.0.0.1:${port}/__faultproxy__/reset', method='POST'))`]);

    const keeled = spawnSync(process.execPath, [NODE_RUN, APP], { env, encoding: "utf8" });
    assert.equal(keeled.status, 0, keeled.stderr);
    assert.match(keeled.stdout, /service ok/);
  } finally {
    proxy.kill();
    rmSync(work, { recursive: true, force: true });
  }
});
