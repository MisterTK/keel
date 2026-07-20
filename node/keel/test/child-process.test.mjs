// The `child_process` pack: runtime-native `cmd:` durable-flow dispatch for
// synchronous `spawnSync`/`execFileSync` (issue #27, Node half of chunk-8).
//
// Offline + always-on (child_process is a builtin — no library to install, same
// stdlib-exception reasoning as the urllib pack). Layers:
//   * pure matching/identity (compileCmdMatchers/matchArgv/argsHashWithCwd),
//   * the wrappers against a FakeFlowBackend (no native addon needed) — every
//     on_busy/replay/dead branch + the spawnSync-vs-execFileSync throw asymmetry,
//   * the require/named-import/default-import CONSUMER MATRIX against the REAL
//     patched `node:child_process` object (design §3.2: the patch-vs-import
//     mismatch is a silent no-op, so it is pinned here).

import test from "node:test";
import assert from "node:assert/strict";
import { spawnSync as realSpawnSync, execFileSync as realExecFileSync } from "node:child_process";
import { createRequire } from "node:module";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { markFlowEntered, markFlowExited } from "../src/runtime.mjs";
import {
  compileCmdMatchers,
  matchArgv,
  argsHashWithCwd,
  codeHash,
  resolveProgram,
  makeWrappedSpawnSync,
  makeWrappedExecFileSync,
  patchChildProcess,
  installChildProcessPack,
  KeelCmdFlowReplayUnsupportedError,
  KeelCmdFlowBusyError,
  KeelCmdFlowDeadError,
} from "../src/packs/child-process.mjs";

// A rule table that matches `[<any>, "-e", <any>]` — so `node -e <script>`
// matches regardless of the absolute node path in `process.execPath`.
const NODE_E_FLOWS = {
  "cmd:t": { name: "cmd:t", argvPatterns: ["*", "-e", "*"], onBusy: "skip" },
};
function nodeECompiled(onBusy = "skip") {
  return compileCmdMatchers({ "cmd:t": { name: "cmd:t", argvPatterns: ["*", "-e", "*"], onBusy } });
}

/**
 * A scriptable Tier-2 backend. `responses` is a queue consumed one per
 * `enterFlow`: `{ replay }` returns that flow info, `{ throw: "KEEL-EXXX" }`
 * throws a coded error; an empty queue defaults to a fresh live flow.
 */
class FakeFlowBackend {
  entered = [];
  exited = [];
  persistent = true;
  #responses;
  #throwOnExit;
  constructor(responses = [], { throwOnExit = false } = {}) {
    this.#responses = [...responses];
    this.#throwOnExit = throwOnExit;
  }
  enterFlow(entrypoint, argsHash, opts = {}) {
    this.entered.push({ entrypoint, argsHash, opts });
    const r = this.#responses.length ? this.#responses.shift() : { replay: false };
    if (r.throw) {
      const e = new Error(`${r.throw}: injected`);
      e.code = r.throw;
      throw e;
    }
    return {
      flow_id: `${entrypoint}#${argsHash}#`,
      status: r.replay ? "completed" : "running",
      replay: Boolean(r.replay),
    };
  }
  exitFlow(status) {
    this.exited.push(status);
    if (this.#throwOnExit) {
      const e = new Error("complete_flow failed (KEEL-E040)");
      e.code = "KEEL-E040";
      throw e;
    }
  }
}

// ---------------------------------------------------------------------------
// compileCmdMatchers / matchArgv
// ---------------------------------------------------------------------------

test("matchArgv: positive exact + literal segments", () => {
  const c = compileCmdMatchers({ "cmd:etl": { name: "cmd:etl", argvPatterns: ["python", "etl.py"], onBusy: "skip" } });
  assert.equal(matchArgv(c, ["python", "etl.py"])?.name, "cmd:etl");
});

test("matchArgv: negative (value mismatch) and arity mismatch never match", () => {
  const c = compileCmdMatchers({ "cmd:etl": { name: "cmd:etl", argvPatterns: ["python", "etl.py"], onBusy: "skip" } });
  assert.equal(matchArgv(c, ["python", "other.py"]), null);
  assert.equal(matchArgv(c, ["python", "etl.py", "--flag"]), null, "a * matches ONE position — arity is exact");
  assert.equal(matchArgv(c, ["python"]), null);
});

test("matchArgv: single-* wildcard matches any value in that position", () => {
  const c = compileCmdMatchers({ "cmd:g": { name: "cmd:g", argvPatterns: ["git", "commit", "*"], onBusy: "skip" } });
  assert.equal(matchArgv(c, ["git", "commit", "-m"])?.name, "cmd:g");
  assert.equal(matchArgv(c, ["git", "commit", "--amend"])?.name, "cmd:g");
  assert.equal(matchArgv(c, ["git", "push", "-m"]), null);
});

test("matchArgv: partial-* within a segment (prefix*)", () => {
  const c = compileCmdMatchers({ "cmd:node": { name: "cmd:node", argvPatterns: ["node", "*.mjs"], onBusy: "skip" } });
  assert.equal(matchArgv(c, ["node", "run.mjs"])?.name, "cmd:node");
  assert.equal(matchArgv(c, ["node", "run.js"]), null);
});

test("matchArgv: case-SENSITIVE (argv values, unlike hostnames)", () => {
  const c = compileCmdMatchers({ "cmd:c": { name: "cmd:c", argvPatterns: ["MyBin"], onBusy: "skip" } });
  assert.equal(matchArgv(c, ["MyBin"])?.name, "cmd:c");
  assert.equal(matchArgv(c, ["mybin"]), null);
});

test("matchArgv: tie-break prefers fewer wildcards, then more literal, then key", () => {
  const c = compileCmdMatchers({
    "cmd:wild": { name: "cmd:wild", argvPatterns: ["python", "*"], onBusy: "skip" },
    "cmd:exact": { name: "cmd:exact", argvPatterns: ["python", "etl.py"], onBusy: "skip" },
  });
  // Both match ["python","etl.py"]; the zero-wildcard rule wins.
  assert.equal(matchArgv(c, ["python", "etl.py"])?.name, "cmd:exact");
  // Only the wildcard rule matches a different second arg.
  assert.equal(matchArgv(c, ["python", "web.py"])?.name, "cmd:wild");
});

test("compileCmdMatchers: rule-less cmd: entrypoints (empty argv) are dropped", () => {
  const c = compileCmdMatchers({ "cmd:bare": { name: "cmd:bare", argvPatterns: [], onBusy: "skip" } });
  assert.equal(c.length, 0);
  assert.equal(matchArgv(c, ["bare"]), null);
});

// ---------------------------------------------------------------------------
// identity
// ---------------------------------------------------------------------------

test("argsHashWithCwd: deterministic, argv- and cwd-sensitive, 16 hex wide", () => {
  const a = argsHashWithCwd(["uvx", "server"], "/app");
  const b = argsHashWithCwd(["uvx", "server"], "/app");
  const diffArgv = argsHashWithCwd(["uvx", "other"], "/app");
  const diffCwd = argsHashWithCwd(["uvx", "server"], "/other");
  assert.equal(a, b);
  assert.notEqual(a, diffArgv);
  assert.notEqual(a, diffCwd, "cwd is part of the in-process identity (diverges from keel exec)");
  assert.match(a, /^[0-9a-f]{16}$/);
});

test("codeHash: stable and argv-sensitive", () => {
  const env = { PATH: "" };
  assert.equal(codeHash(["/bin/echo", "x"], env), codeHash(["/bin/echo", "x"], env));
  assert.notEqual(codeHash(["/bin/echo", "x"], env), codeHash(["/bin/echo", "y"], env));
  assert.match(codeHash(["/bin/echo", "x"], env), /^[0-9a-f]{16}$/);
});

test("resolveProgram: path-bearing verbatim; bare name resolved via PATH", () => {
  const dir = mkdtempSync(join(tmpdir(), "keel-cp-path-"));
  try {
    const bin = join(dir, "mytool");
    writeFileSync(bin, "#!/bin/sh\n");
    assert.equal(resolveProgram("/abs/path/tool", { PATH: dir }), "/abs/path/tool");
    assert.equal(resolveProgram("mytool", { PATH: dir }), bin);
    assert.equal(resolveProgram("nonexistent-xyz", { PATH: dir }), "nonexistent-xyz", "unresolvable → verbatim");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// ---------------------------------------------------------------------------
// spawnSync wrapper (never throws for a normal outcome)
// ---------------------------------------------------------------------------

test("spawnSync: a matched call journals a completed flow and returns the real result", () => {
  const fake = new FakeFlowBackend();
  const w = makeWrappedSpawnSync(realSpawnSync, { compiled: nodeECompiled(), backend: fake, env: {} });
  const r = w(process.execPath, ["-e", "process.stdout.write('ok')"], { encoding: "utf8" });
  assert.equal(r.status, 0);
  assert.equal(r.stdout, "ok");
  assert.equal(fake.entered.length, 1);
  assert.equal(fake.entered[0].entrypoint, "cmd:t");
  assert.equal(fake.entered[0].argsHash, argsHashWithCwd([process.execPath, "-e", "process.stdout.write('ok')"], process.cwd()));
  assert.ok(fake.entered[0].opts.codeHash, "codeHash was passed to enterFlow");
  assert.deepEqual(fake.exited, ["completed"]);
});

test("spawnSync: a nonzero exit marks the flow failed and returns the result unchanged (no throw)", () => {
  const fake = new FakeFlowBackend();
  const w = makeWrappedSpawnSync(realSpawnSync, { compiled: nodeECompiled(), backend: fake, env: {} });
  const r = w(process.execPath, ["-e", "process.exit(3)"]);
  assert.equal(r.status, 3);
  assert.deepEqual(fake.exited, ["failed"]);
});

test("spawnSync: a spawn failure (ENOENT) marks failed and returns the error in the result (never swallowed)", () => {
  const fake = new FakeFlowBackend();
  const compiled = compileCmdMatchers({ "cmd:x": { name: "cmd:x", argvPatterns: ["keel-noexist-xyz", "*"], onBusy: "skip" } });
  const w = makeWrappedSpawnSync(realSpawnSync, { compiled, backend: fake, env: {} });
  const r = w("keel-noexist-xyz", ["a"]);
  assert.equal(r.error?.code, "ENOENT");
  assert.equal(r.status, null);
  assert.deepEqual(fake.exited, ["failed"]);
});

test("spawnSync: an unmatched call passes through untouched (no enterFlow)", () => {
  const fake = new FakeFlowBackend();
  const w = makeWrappedSpawnSync(realSpawnSync, { compiled: nodeECompiled(), backend: fake, env: {} });
  const r = w(process.execPath, ["--version"]); // arity 2, no "-e" → no match
  assert.equal(r.status, 0);
  assert.equal(fake.entered.length, 0);
  assert.equal(fake.exited.length, 0);
});

test("spawnSync: shell:true is out of scope — passes through unwrapped", () => {
  const fake = new FakeFlowBackend();
  const w = makeWrappedSpawnSync(realSpawnSync, { compiled: nodeECompiled(), backend: fake, env: {} });
  // (The shell re-parses the argv, so the child's exit code is the shell's, not
  // ours — the point is only that Keel never flow-tracks a shell:true call.)
  const r = w(process.execPath, ["-e", "process.exit(0)"], { shell: true });
  assert.equal(fake.entered.length, 0, "shell:true is never flow-tracked");
  assert.equal(fake.exited.length, 0);
  assert.ok(r && typeof r === "object", "the real spawnSync result is returned unchanged");
});

test("spawnSync: disabled backend (getBackend()===null, no deps.backend) passes through", () => {
  const w = makeWrappedSpawnSync(realSpawnSync, { compiled: nodeECompiled(), env: {} });
  const r = w(process.execPath, ["-e", "process.exit(0)"]); // getBackend() is null in this test file
  assert.equal(r.status, 0); // ran, unwrapped, no crash
});

// ---------------------------------------------------------------------------
// on_busy (KEEL-E030) and dead (KEEL-E032)
// ---------------------------------------------------------------------------

test("on_busy=fail: a busy flow (E030) throws KeelCmdFlowBusyError; the command is NOT run", () => {
  const fake = new FakeFlowBackend([{ throw: "KEEL-E030" }]);
  const w = makeWrappedSpawnSync(realSpawnSync, { compiled: nodeECompiled("fail"), backend: fake, env: {} });
  assert.throws(
    () => w(process.execPath, ["-e", "throw new Error('should not run')"]),
    (e) => e instanceof KeelCmdFlowBusyError && e.code === "KEEL-E030"
  );
  assert.deepEqual(fake.exited, [], "no flow was opened, so none was closed");
});

test("on_busy=skip: a busy flow (E030) runs the real command UNWRAPPED and returns its real result", () => {
  const fake = new FakeFlowBackend([{ throw: "KEEL-E030" }]);
  const w = makeWrappedSpawnSync(realSpawnSync, { compiled: nodeECompiled("skip"), backend: fake, env: {} });
  const r = w(process.execPath, ["-e", "process.stdout.write('ran')"], { encoding: "utf8" });
  assert.equal(r.stdout, "ran", "skip = run unwrapped, NOT a fabricated success");
  assert.deepEqual(fake.exited, [], "skip does not flow-track");
});

test("on_busy=wait: retries enterFlow until the holder clears, then runs (bounded)", () => {
  // First enter is busy; the second succeeds. One WAIT_POLL_MS (~500ms) elapses.
  const fake = new FakeFlowBackend([{ throw: "KEEL-E030" }, { replay: false }]);
  const w = makeWrappedSpawnSync(realSpawnSync, { compiled: nodeECompiled("wait"), backend: fake, env: {} });
  const r = w(process.execPath, ["-e", "process.exit(0)"]);
  assert.equal(r.status, 0);
  assert.equal(fake.entered.length, 2, "it retried after the busy holder cleared");
  assert.deepEqual(fake.exited, ["completed"]);
});

test("dead flow (E032) always throws KeelCmdFlowDeadError — even under on_busy=skip", () => {
  const fake = new FakeFlowBackend([{ throw: "KEEL-E032" }]);
  const w = makeWrappedSpawnSync(realSpawnSync, { compiled: nodeECompiled("skip"), backend: fake, env: {} });
  assert.throws(
    () => w(process.execPath, ["-e", "process.exit(0)"]),
    (e) => e instanceof KeelCmdFlowDeadError && e.code === "KEEL-E032"
  );
});

// ---------------------------------------------------------------------------
// Open Question 1: a completed flow cannot replay-substitute a sync result
// ---------------------------------------------------------------------------

test("replay: a Completed flow throws KeelCmdFlowReplayUnsupportedError; command NOT re-run; handle released", () => {
  const fake = new FakeFlowBackend([{ replay: true }]);
  const w = makeWrappedSpawnSync(realSpawnSync, { compiled: nodeECompiled(), backend: fake, env: {} });
  assert.throws(
    () => w(process.execPath, ["-e", "throw new Error('should not re-run')"]),
    (e) => e instanceof KeelCmdFlowReplayUnsupportedError && e.code === "KEEL-E005"
  );
  // The handle we entered is released (exitFlow completed) so it does not leak.
  assert.deepEqual(fake.exited, ["completed"]);
});

// ---------------------------------------------------------------------------
// nested flow scope: a matched sync call inside an open flow passes through
// ---------------------------------------------------------------------------

test("nested: inside an open durable-flow scope, a matched call runs unwrapped (no clobber)", () => {
  const fake = new FakeFlowBackend();
  const w = makeWrappedSpawnSync(realSpawnSync, { compiled: nodeECompiled(), backend: fake, env: {} });
  markFlowEntered(); // model a keel-run ts: flow being open
  try {
    const r = w(process.execPath, ["-e", "process.exit(0)"]);
    assert.equal(r.status, 0);
    assert.equal(fake.entered.length, 0, "must NOT open a nested flow on the shared slot");
  } finally {
    markFlowExited();
  }
});

// ---------------------------------------------------------------------------
// execFileSync wrapper (THROWS on nonzero exit AND on spawn failure)
// ---------------------------------------------------------------------------

test("execFileSync: success returns stdout and marks the flow completed", () => {
  const fake = new FakeFlowBackend();
  const w = makeWrappedExecFileSync(realExecFileSync, { compiled: nodeECompiled(), backend: fake, env: {} });
  const out = w(process.execPath, ["-e", "process.stdout.write('hi')"], { encoding: "utf8" });
  assert.equal(out, "hi");
  assert.deepEqual(fake.exited, ["completed"]);
});

test("execFileSync: a nonzero exit THROWS the original error unchanged and marks the flow failed", () => {
  const fake = new FakeFlowBackend();
  const w = makeWrappedExecFileSync(realExecFileSync, { compiled: nodeECompiled(), backend: fake, env: {} });
  assert.throws(
    () => w(process.execPath, ["-e", "process.exit(3)"]),
    (e) => e.status === 3 // Node's own execFileSync error shape, propagated verbatim
  );
  assert.deepEqual(fake.exited, ["failed"]);
});

test("execFileSync: a spawn failure (ENOENT) THROWS unchanged and marks failed (never swallowed)", () => {
  const fake = new FakeFlowBackend();
  const compiled = compileCmdMatchers({ "cmd:x": { name: "cmd:x", argvPatterns: ["keel-noexist-xyz", "*"], onBusy: "skip" } });
  const w = makeWrappedExecFileSync(realExecFileSync, { compiled, backend: fake, env: {} });
  assert.throws(
    () => w("keel-noexist-xyz", ["a"]),
    (e) => e.code === "ENOENT"
  );
  assert.deepEqual(fake.exited, ["failed"]);
});

// ---------------------------------------------------------------------------
// journal-write failure on exitFlow degrades to a warning (issue #14)
// ---------------------------------------------------------------------------

test("spawnSync: an exitFlow journal-write failure does not replace the real result", () => {
  const fake = new FakeFlowBackend([], { throwOnExit: true });
  const w = makeWrappedSpawnSync(realSpawnSync, { compiled: nodeECompiled(), backend: fake, env: {} });
  const origWrite = process.stderr.write;
  const lines = [];
  process.stderr.write = (c) => (lines.push(String(c)), true);
  let r;
  try {
    r = w(process.execPath, ["-e", "process.stdout.write('ok')"], { encoding: "utf8" });
  } finally {
    process.stderr.write = origWrite;
  }
  assert.equal(r.stdout, "ok", "the real result survives a journal-write failure");
  assert.ok(lines.some((l) => l.includes("KEEL-E040") && l.includes("not journaled")));
});

// ---------------------------------------------------------------------------
// the require / named-import / default-import CONSUMER MATRIX (design §3.2)
// ---------------------------------------------------------------------------

// The three sentinel programs, one per consumer shape — distinct so each rule's
// entrypoint identifies which consumer routed through the patch.
const MATRIX_FLOWS = {
  "cmd:req": { name: "cmd:req", argvPatterns: ["keel-tag-require", "*"], onBusy: "skip" },
  "cmd:named": { name: "cmd:named", argvPatterns: ["keel-tag-named", "*"], onBusy: "skip" },
  "cmd:def": { name: "cmd:def", argvPatterns: ["keel-tag-default", "*"], onBusy: "skip" },
};

function writeConsumerModules(dir) {
  const files = {
    req: join(dir, "req.mjs"),
    named: join(dir, "named.mjs"),
    def: join(dir, "def.mjs"),
  };
  writeFileSync(
    files.req,
    `import { createRequire } from "node:module";\nconst r = createRequire(import.meta.url);\n` +
      `export const run = () => r("node:child_process").spawnSync("keel-tag-require", ["x"]);\n`
  );
  writeFileSync(
    files.named,
    `import { spawnSync } from "node:child_process";\nexport const run = () => spawnSync("keel-tag-named", ["x"]);\n`
  );
  writeFileSync(
    files.def,
    `import cp from "node:child_process";\nexport const run = () => cp.spawnSync("keel-tag-default", ["x"]);\n`
  );
  return files;
}

test("consumer matrix (in-process): require + default read the LIVE object and observe a runtime patch; a named binding does not", async (t) => {
  // In this test PROCESS the ESM namespace for `node:child_process` is already
  // materialized (the test runner + this file's own top-level named import), so
  // a named binding is fixed to the pre-patch function. `require()` and the
  // default import read the live module object, so they DO observe a patch
  // applied at runtime. (The product's named-import path — patched fresh in
  // `--import` preload before the app graph loads — is proven by the subprocess
  // test below, which is the only faithful reproduction of that ordering.)
  const dir = mkdtempSync(join(tmpdir(), "keel-cp-matrix-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const fake = new FakeFlowBackend();
  const compiled = compileCmdMatchers(MATRIX_FLOWS);
  const cp = createRequire(import.meta.url)("node:child_process");
  const uninstall = patchChildProcess(cp, { compiled, backend: fake, env: {} });
  t.after(uninstall);

  const files = writeConsumerModules(dir);
  const req = await import(new URL(`file://${files.req}`).href);
  const named = await import(new URL(`file://${files.named}`).href);
  const def = await import(new URL(`file://${files.def}`).href);
  req.run();
  named.run();
  def.run();

  const seen = fake.entered.map((e) => e.entrypoint);
  assert.ok(seen.includes("cmd:req"), "require consumer observed the live-object patch");
  assert.ok(seen.includes("cmd:def"), "default-import consumer observed the live-object patch");
  assert.ok(!seen.includes("cmd:named"), "a named binding built pre-patch does not re-read (documented)");
});

test("consumer matrix (subprocess, real preload order): require, named AND default ALL observe the patch", () => {
  // The faithful product reproduction: a fresh child `node --import <loader>
  // <app>` where the loader patches via createRequire BEFORE the app graph
  // loads. The app's own `import { spawnSync }` therefore materializes the
  // namespace AFTER the patch, so even the named binding observes it — the exact
  // reason the pack patches via createRequire in `--import` preload (design
  // §3.2). This is what would silently no-op had the pack patched a facade.
  const dir = mkdtempSync(join(tmpdir(), "keel-cp-preload-"));
  try {
    const packUrl = new URL("../src/packs/child-process.mjs", import.meta.url).href;
    const loader = join(dir, "loader.mjs");
    const app = join(dir, "app.mjs");
    writeFileSync(
      loader,
      `import { createRequire } from "node:module";\n` +
        `import { patchChildProcess, compileCmdMatchers } from ${JSON.stringify(packUrl)};\n` +
        `const cp = createRequire(import.meta.url)("node:child_process");\n` +
        `const compiled = compileCmdMatchers(${JSON.stringify(MATRIX_FLOWS)});\n` +
        `globalThis.__KEEL_SEEN = [];\n` +
        `const backend = { persistent: true, enterFlow(e){ globalThis.__KEEL_SEEN.push(e); return { flow_id: "f", status: "running", replay: false }; }, exitFlow(){} };\n` +
        `patchChildProcess(cp, { compiled, backend, env: {} });\n`
    );
    writeFileSync(
      app,
      `import { spawnSync } from "node:child_process";\n` +
        `import cpDefault from "node:child_process";\n` +
        `import { createRequire } from "node:module";\n` +
        `const req = createRequire(import.meta.url)("node:child_process");\n` +
        `req.spawnSync("keel-tag-require", ["x"]);\n` +
        `spawnSync("keel-tag-named", ["x"]);\n` +
        `cpDefault.spawnSync("keel-tag-default", ["x"]);\n` +
        `process.stdout.write(JSON.stringify(globalThis.__KEEL_SEEN));\n`
    );
    const out = realSpawnSync(process.execPath, ["--import", new URL(`file://${loader}`).href, app], {
      encoding: "utf8",
    });
    assert.equal(out.status, 0, `child failed: ${out.stderr}`);
    const seen = JSON.parse(out.stdout);
    assert.ok(seen.includes("cmd:req"), "require consumer observed the preload patch");
    assert.ok(seen.includes("cmd:named"), "NAMED consumer observed the preload patch (the load-bearing case)");
    assert.ok(seen.includes("cmd:def"), "default-import consumer observed the preload patch");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// ---------------------------------------------------------------------------
// install gating
// ---------------------------------------------------------------------------

test("installChildProcessPack: no matchable cmd: rules → inactive, no patch (near-zero cost)", () => {
  const res = installChildProcessPack({ cmdFlows: {}, backend: new FakeFlowBackend() });
  assert.deepEqual(res, { active: false });
});

test("installChildProcessPack: rules present but a non-flow backend → inactive with a loud notice", () => {
  const cmdFlows = { "cmd:t": { name: "cmd:t", argvPatterns: ["python", "*"], onBusy: "skip" } };
  const origWrite = process.stderr.write;
  const lines = [];
  process.stderr.write = (c) => (lines.push(String(c)), true);
  let res;
  try {
    res = installChildProcessPack({ cmdFlows, backend: { execute() {} }, env: {} }); // no enterFlow → not Tier-2
  } finally {
    process.stderr.write = origWrite;
  }
  assert.equal(res.active, false);
  assert.ok(lines.some((l) => l.includes("KEEL-E005") && l.includes("cmd: interception")));
});

test("installChildProcessPack: rules + a Tier-2 backend → active, patches, and uninstalls cleanly", () => {
  const cmdFlows = { "cmd:t": { name: "cmd:t", argvPatterns: ["keel-noexist-xyz", "*"], onBusy: "skip" } };
  const fakeCp = { spawnSync: () => "orig-spawn", execFileSync: () => "orig-exec" };
  const res = installChildProcessPack({ cmdFlows, backend: new FakeFlowBackend(), childProcessModule: fakeCp });
  assert.equal(res.active, true);
  assert.equal(res.name, "child_process");
  assert.equal(fakeCp.spawnSync.__keelWrapped, true);
  assert.equal(fakeCp.execFileSync.__keelWrapped, true);
  res.uninstall();
  assert.equal(fakeCp.spawnSync(), "orig-spawn", "uninstall restores the originals");
  assert.equal(fakeCp.execFileSync(), "orig-exec");
});
