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

// #130's SECOND refusal: any bootstrap exception puts Keel fully off while
// the host keeps serving. It was unstructured prose at the platform's default
// severity — exactly the invisibility #130 was filed about — while its
// sibling `policy-missing-at-keel-cwd` already read ERROR. Byte-for-byte twin
// of `python/keel/tests/test_auto.py`'s two activation-failure tests.
//
// The failure used here is the forward-compatibility shape CCR-11 makes
// likely: a `keel.toml` carrying a `poll.until` key this Keel does not know
// is KEEL-E001 at configure, and under `--import keelrun/register` that lands
// the whole process unprotected. (An older Keel reads `absent = "pending"`
// exactly this way; a placeholder key reproduces it against the current one.)
const FORWARD_KEY_POLICY =
  '[target."api.example.com"]\n' +
  'poll = { interval = "10s", deadline = "90s", ' +
  'until = { field = "done", terminal = [true], futurekey = "pending" } }\n';

function runWithPolicy(body, env) {
  const dir = mkdtempSync(join(tmpdir(), "keel-register-fail-"));
  writeFileSync(join(dir, "keel.toml"), body);
  return spawnSync(process.execPath, ["--import", REGISTER, NOOP], {
    env: { ...process.env, KEEL_ENABLE: "1", KEEL_DISABLE: "", KEEL_CWD: dir, ...env },
    cwd: dir,
    encoding: "utf8",
  });
}

test("KEEL_LOG_FORMAT=json: an activation failure is an ERROR object, not prose", () => {
  const proc = runWithPolicy(FORWARD_KEY_POLICY, { KEEL_LOG_FORMAT: "json" });
  // Fail-open contract: the host ran regardless of what Keel did.
  assert.equal(proc.status, 0, proc.stderr);
  const objs = proc.stderr
    .split("\n")
    .filter((l) => l.trim())
    .map((l) => JSON.parse(l));
  assert.equal(objs.length, 1, proc.stderr);
  assert.equal(objs[0].keel, "error");
  assert.equal(objs[0].code, "activation-failed");
  assert.equal(objs[0].severity, "ERROR", proc.stderr);
  assert.ok(objs[0].message.includes("KEEL-E001"), proc.stderr);
  assert.ok(Object.hasOwn(objs[0], "keel_cwd"), proc.stderr);
  // json REPLACES the text line; it never accompanies it.
  assert.ok(!proc.stderr.includes("keel ▸"), proc.stderr);
});

test("the text form of an activation failure is unchanged", () => {
  const proc = runWithPolicy("not [valid toml\n", {});
  assert.equal(proc.status, 0, proc.stderr);
  const lines = proc.stderr.split("\n").filter((l) => l.startsWith("keel ▸"));
  assert.equal(lines.length, 1, proc.stderr);
  assert.ok(lines[0].startsWith("keel ▸ auto-activation failed ("), lines[0]);
  assert.ok(lines[0].endsWith("); continuing without keel"), lines[0]);
});

test("package exports expose ./register", () => {
  const pkg = JSON.parse(
    execFileSync(process.execPath, ["-p", `JSON.stringify(require(${JSON.stringify(join(here, "..", "package.json"))}))`], { encoding: "utf8" })
  );
  assert.deepEqual(pkg.exports["./register"], { import: "./register.mjs" });
});
