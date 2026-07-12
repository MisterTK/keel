// DX invariant: KEEL_DISABLE=1 makes a run byte-identical to one with no hook
// at all — same stdout, same stderr, same exit code. This is the strongest
// "uninstall-clean / no surprise" guarantee, so it is exercised end-to-end in
// child processes.

import test from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const hookUrl = new URL("../hook.mjs", import.meta.url).href;
const appPath = fileURLToPath(new URL("../fixtures/hello.mjs", import.meta.url));

function cleanEnv(extra = {}) {
  const env = { ...process.env };
  delete env.KEEL_DISABLE;
  delete env.KEEL_BACKEND;
  delete env.KEEL_QUIET;
  return { ...env, ...extra };
}

test("KEEL_DISABLE=1 is byte-identical to running without the hook", () => {
  const baseline = spawnSync(process.execPath, [appPath], { env: cleanEnv() });
  const disabled = spawnSync(process.execPath, ["--import", hookUrl, appPath], {
    env: cleanEnv({ KEEL_DISABLE: "1" }),
  });

  assert.equal(disabled.status, baseline.status, "exit code must match");
  assert.deepEqual(disabled.stdout, baseline.stdout, "stdout must be byte-identical");
  assert.deepEqual(disabled.stderr, baseline.stderr, "stderr must be byte-identical");
  assert.equal(baseline.status, 7);
});

test("enabled run keeps stdout clean and puts the banner on stderr", () => {
  const enabled = spawnSync(process.execPath, ["--import", hookUrl, appPath], { env: cleanEnv() });
  // stdout is exactly the program's output — no Keel noise.
  assert.equal(enabled.stdout.toString(), "stdout-line-1\ncomputed 42\n");
  // the banner goes to stderr, alongside the program's own stderr.
  assert.match(enabled.stderr.toString(), /keel ▸ wrapped global fetch/);
  assert.match(enabled.stderr.toString(), /stderr-line-1/);
  assert.equal(enabled.status, 7);
});
