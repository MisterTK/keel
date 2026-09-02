// #85: a process launched with cwd in a subdirectory of the real project
// root (uvicorn-style `cwd=agents/`) makes `loadPolicy(cwd)` find no
// keel.toml and silently fall back to Level 0 defaults — the banner used to
// read as a perfectly normal run, hiding that the adopter's policy never
// loaded. These are real child-process runs (like disable-identity.test.mjs)
// because the bootstrap's module-level `installed` guard makes a second
// in-process installKeel() call a no-op, and the whole point here is the
// banner actually printed by a freshly-started process for a given cwd.

import test from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync, realpathSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const hookUrl = new URL("../hook.mjs", import.meta.url).href;
const appPath = fileURLToPath(new URL("../fixtures/hello.mjs", import.meta.url));

function run(cwd, env = {}) {
  const cleanEnv = { ...process.env };
  delete cleanEnv.KEEL_DISABLE;
  delete cleanEnv.KEEL_QUIET;
  delete cleanEnv.KEEL_BACKEND;
  return spawnSync(process.execPath, ["--import", hookUrl, appPath], {
    cwd,
    env: { ...cleanEnv, ...env },
    encoding: "utf8",
  });
}

function esc(s) {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

test("defaults banner names a parent keel.toml and the KEEL_CWD fix", () => {
  const root = mkdtempSync(join(tmpdir(), "keel-banner-parent-"));
  try {
    writeFileSync(join(root, "keel.toml"), "");
    const sub = join(root, "agents", "worker");
    mkdirSync(sub, { recursive: true });
    // spawnSync's `cwd` is resolved by the OS (getcwd() drops any symlink
    // hops, e.g. macOS's /var → /private/var) before the child ever sees
    // it, so the banner's paths must be compared against the REALPATH, not
    // the possibly-symlinked path `mkdtempSync` handed back.
    const realRoot = realpathSync(root);
    const realSub = realpathSync(sub);

    const proc = run(sub);
    assert.equal(proc.status, 7, proc.stderr);
    const lines = proc.stderr.split("\n").filter((l) => l.startsWith("keel ▸"));
    assert.equal(lines.length, 1, `expected exactly one banner line in: ${proc.stderr}`);
    assert.match(lines[0], new RegExp(`found keel\\.toml at ${esc(realRoot)}`));
    assert.match(lines[0], new RegExp(`running from ${esc(realSub)}`));
    assert.match(lines[0], new RegExp(`set KEEL_CWD=${esc(realRoot)} to load it`));
    assert.ok(!lines[0].includes("`keel init` to customize"), `should not show the usual nudge: ${lines[0]}`);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("defaults banner stays unchanged when no keel.toml exists above cwd", () => {
  const root = mkdtempSync(join(tmpdir(), "keel-banner-noparent-"));
  try {
    const proc = run(root);
    assert.equal(proc.status, 7, proc.stderr);
    // Environment-dependent seams (e.g. an ai-sdk detection off this repo's
    // own node_modules) may add to the "wrapped …" list — pin only the
    // decision this test is actually about: the tail stays the ordinary
    // `keel init` nudge, byte-unchanged, with no parent-policy text.
    assert.match(proc.stderr, /^keel ▸ wrapped .* with production defaults — `keel init` to customize\n/m);
    assert.ok(!proc.stderr.includes("found keel.toml"), `unexpected parent-policy text: ${proc.stderr}`);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("KEEL_QUIET suppresses the banner even with a parent keel.toml", () => {
  const root = mkdtempSync(join(tmpdir(), "keel-banner-quiet-"));
  try {
    writeFileSync(join(root, "keel.toml"), "");
    const sub = join(root, "sub");
    mkdirSync(sub);

    const proc = run(sub, { KEEL_QUIET: "1" });
    assert.equal(proc.status, 7, proc.stderr);
    assert.ok(!proc.stderr.includes("keel ▸"), `expected no banner: ${proc.stderr}`);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
