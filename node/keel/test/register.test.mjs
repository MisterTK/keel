// keelrun/register — the KEEL_ENABLE-gated preload (WS2, .env-parity twin of
// the Python .pth shim). Spawned as real child processes because the gate
// must be judged at process startup, before user code runs.
import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const REGISTER = join(here, "..", "register.mjs");
const NOOP = join(here, "..", "fixtures", "noop.mjs");

function run(env) {
  return spawnSync(process.execPath, ["--import", REGISTER, NOOP], {
    env: { ...process.env, KEEL_ENABLE: "", KEEL_DISABLE: "", ...env },
    cwd: mkdtempSync(join(tmpdir(), "keel-register-")),
    encoding: "utf8",
  });
}

test("gate off: register is inert (no banner, no keel side effects)", () => {
  const proc = run({});
  assert.equal(proc.status, 0, proc.stderr);
  assert.ok(!proc.stderr.includes("keel ▸"), `unexpected stderr: ${proc.stderr}`);
});

test("gate on: register activates keel (banner) and the app still runs", () => {
  const proc = run({ KEEL_ENABLE: "1" });
  assert.equal(proc.status, 0, proc.stderr);
  assert.ok(proc.stderr.includes("keel ▸"), `expected banner in: ${proc.stderr}`);
});

test("KEEL_DISABLE beats KEEL_ENABLE", () => {
  const proc = run({ KEEL_ENABLE: "1", KEEL_DISABLE: "1" });
  assert.equal(proc.status, 0, proc.stderr);
  assert.ok(!proc.stderr.includes("keel ▸"), `unexpected stderr: ${proc.stderr}`);
});

test("gate tolerates case/whitespace", () => {
  const proc = run({ KEEL_ENABLE: "  TRUE " });
  assert.ok(proc.stderr.includes("keel ▸"), proc.stderr);
});

test("package exports expose ./register", () => {
  const pkg = JSON.parse(
    execFileSync(process.execPath, ["-p", `JSON.stringify(require(${JSON.stringify(join(here, "..", "package.json"))}))`], { encoding: "utf8" })
  );
  assert.deepEqual(pkg.exports["./register"], { import: "./register.mjs" });
});
