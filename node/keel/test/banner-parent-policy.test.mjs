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
import { createRequire } from "node:module";

const hookUrl = new URL("../hook.mjs", import.meta.url).href;
const appPath = fileURLToPath(new URL("../fixtures/hello.mjs", import.meta.url));

function run(cwd, env = {}) {
  const cleanEnv = { ...process.env };
  delete cleanEnv.KEEL_DISABLE;
  delete cleanEnv.KEEL_QUIET;
  delete cleanEnv.KEEL_BACKEND;
  // The preload honors KEEL_CWD as the config root, so an ambient value would
  // silently relocate every case below off its tempdir. Each test opts in.
  delete cleanEnv.KEEL_CWD;
  return spawnSync(process.execPath, ["--import", hookUrl, appPath], {
    cwd,
    env: { ...cleanEnv, ...env },
    encoding: "utf8",
  });
}

function esc(s) {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/** Keel's own JSON lines, in order — the fixture app writes its own prose to
 *  stderr too, and that has to stay untouched (Keel owns only its own lines). */
function keelJsonLines(stderr) {
  return stderr
    .split("\n")
    .filter((l) => l.startsWith("{"))
    .map((l) => JSON.parse(l))
    .filter((o) => o.keel !== undefined);
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
    assert.match(proc.stderr, /^keel ▸ wrapped .* with production defaults — no keel\.toml in .*; `keel init` to customize\n/m);
    assert.ok(!proc.stderr.includes("found keel.toml"), `unexpected parent-policy text: ${proc.stderr}`);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

// The other half of the #85 story: the banner names KEEL_CWD as the fix, so
// KEEL_CWD has to actually BE a fix on this front end. `keel run` already
// exports it to every child (run.rs `activation_env`) and Python's `.pth`
// shim already honors it; Node's preload used to ignore it entirely, which
// made the banner's advice a dead end for Node adopters.
test("KEEL_CWD relocates the config root the preload loads keel.toml from", () => {
  const root = mkdtempSync(join(tmpdir(), "keel-hook-keelcwd-"));
  try {
    // A function target makes the banner PROVE the file's contents loaded —
    // "policy keel.toml" alone could in principle come from anywhere.
    writeFileSync(join(root, "keel.toml"), '[target."ts:app.mjs#handler"]\ntimeout = "5s"\n');
    const sub = join(root, "agents", "worker");
    mkdirSync(sub, { recursive: true });
    const realRoot = realpathSync(root);

    const proc = run(sub, { KEEL_CWD: realRoot });
    assert.equal(proc.status, 7, proc.stderr);
    const lines = proc.stderr.split("\n").filter((l) => l.startsWith("keel ▸"));
    assert.equal(lines.length, 1, `expected exactly one banner line in: ${proc.stderr}`);
    assert.match(lines[0], new RegExp(`with policy ${esc(join(realRoot, "keel.toml"))}`));
    assert.match(lines[0], /1 function target/);
    assert.ok(
      !lines[0].includes("found keel.toml"),
      `KEEL_CWD loaded the policy, so there is nothing to warn about: ${lines[0]}`
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

// Precedence pin: an empty KEEL_CWD must not resolve the config root to "",
// and with no KEEL_CWD at all the real cwd still wins — this is what keeps
// `keel run`'s own child (where cwd and KEEL_CWD are equal anyway) and every
// plain `--import keelrun/hook` run byte-identical to before.
test("an empty KEEL_CWD falls back to the real cwd", () => {
  const root = mkdtempSync(join(tmpdir(), "keel-hook-keelcwd-empty-"));
  try {
    writeFileSync(join(root, "keel.toml"), "");
    const realRoot = realpathSync(root);
    const proc = run(root, { KEEL_CWD: "" });
    assert.equal(proc.status, 7, proc.stderr);
    assert.match(proc.stderr, new RegExp(`with policy ${esc(join(realRoot, "keel.toml"))}`));
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

test("KEEL_CWD without a keel.toml refuses to activate with one error line", () => {
  const root = mkdtempSync(join(tmpdir(), "keel-strict-cwd-"));
  try {
    const realRoot = realpathSync(root);
    const proc = run(root, { KEEL_CWD: realRoot });
    assert.equal(proc.status, 7, proc.stderr); // the app still runs, keel-free
    const lines = proc.stderr.split("\n").filter((l) => l.startsWith("keel ▸"));
    assert.equal(lines.length, 1, `expected exactly one keel line in: ${proc.stderr}`);
    assert.equal(
      lines[0],
      `keel ▸ error: KEEL_CWD=${realRoot} is set but ${join(realRoot, "keel.toml")} does not exist — ` +
        "Keel NOT activated; the app continues without keel " +
        "(set KEEL_POLICY=optional to run on production defaults instead)"
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

// `ENV KEEL_CWD=/app/` in a Dockerfile is an entirely ordinary input, and
// Python echoes it through `str(Path(cwd))` (which strips the trailing
// separator). Node normalizes the same way at the top of installKeel, so the
// two front ends print the same bytes for the same input — twin:
// python/keel/tests/test_bootstrap_keel_cwd.py
// `test_a_trailing_slash_is_normalized_away_like_python_pathlib`.
test("a trailing slash on KEEL_CWD prints the same bytes as a canonical root", () => {
  const root = mkdtempSync(join(tmpdir(), "keel-strict-cwd-slash-"));
  try {
    const realRoot = realpathSync(root);
    const proc = run(root, { KEEL_CWD: `${realRoot}/` });
    assert.equal(proc.status, 7, proc.stderr);
    const lines = proc.stderr.split("\n").filter((l) => l.startsWith("keel ▸"));
    assert.equal(lines.length, 1, `expected exactly one keel line in: ${proc.stderr}`);
    assert.equal(
      lines[0],
      `keel ▸ error: KEEL_CWD=${realRoot} is set but ${join(realRoot, "keel.toml")} does not exist — ` +
        "Keel NOT activated; the app continues without keel " +
        "(set KEEL_POLICY=optional to run on production defaults instead)"
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

// The same normalization has to reach the JSON `root` field, not just the
// prose line — that field is what a log query in a container actually reads.
test("a trailing slash on KEEL_CWD is normalized in the JSON root field too", () => {
  const root = mkdtempSync(join(tmpdir(), "keel-strict-cwd-slash-json-"));
  try {
    const realRoot = realpathSync(root);
    const proc = run(root, { KEEL_CWD: `${realRoot}/`, KEEL_LOG_FORMAT: "json" });
    assert.equal(proc.status, 7, proc.stderr);
    const objs = keelJsonLines(proc.stderr);
    assert.equal(objs.length, 1, proc.stderr);
    assert.equal(objs[0].keel, "error");
    assert.equal(objs[0].code, "policy-missing-at-keel-cwd");
    assert.equal(objs[0].keel_cwd, realRoot);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("KEEL_POLICY=optional runs on defaults under a stale KEEL_CWD, with a warning", () => {
  const root = mkdtempSync(join(tmpdir(), "keel-optional-cwd-"));
  try {
    const realRoot = realpathSync(root);
    const proc = run(root, { KEEL_CWD: realRoot, KEEL_POLICY: "optional" });
    assert.equal(proc.status, 7, proc.stderr);
    const lines = proc.stderr.split("\n").filter((l) => l.startsWith("keel ▸"));
    assert.equal(lines.length, 1, proc.stderr);
    assert.match(
      lines[0],
      new RegExp(
        `^keel ▸ wrapped .* with production defaults — KEEL_CWD=${esc(realRoot)} is set but ` +
          `${esc(join(realRoot, "keel.toml"))} does not exist \\(KEEL_POLICY=optional\\)$`
      )
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("the defaults banner names the directory it searched", () => {
  const root = mkdtempSync(join(tmpdir(), "keel-banner-root-"));
  try {
    const realRoot = realpathSync(root);
    const proc = run(root);
    assert.equal(proc.status, 7, proc.stderr);
    assert.match(
      proc.stderr,
      new RegExp(`^keel ▸ wrapped .* with production defaults — no keel\\.toml in ${esc(realRoot)}; \`keel init\` to customize\n`, "m")
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

// F10: in a container the only surface that survives is stderr, so
// `KEEL_LOG_FORMAT=json` turns the activation line and the exit summary into
// queryable fields. Unlike the text form the summary prints at zero calls —
// the fixture app makes none, and that zero line is itself the evidence.
test("KEEL_LOG_FORMAT=json emits one JSON object per line", () => {
  const root = mkdtempSync(join(tmpdir(), "keel-json-logs-"));
  try {
    // R10: `note` carries the activation line's em-dash tail whenever the
    // text form has one, and the key is ABSENT when it does not. Pin both
    // sides — the defaults run first, before a keel.toml exists at or above
    // this directory, so the tail is the plain `keel init` nudge.
    const defaults = keelJsonLines(run(root, { KEEL_LOG_FORMAT: "json" }).stderr);
    assert.equal(defaults[0].policy_source, "defaults");
    assert.equal(defaults[0].note, `no keel.toml in ${realpathSync(root)}; \`keel init\` to customize`);
    // #130: Cloud Run fills an entry's severity only from a `severity` field
    // in the payload — a healthy activation must read INFO, not DEFAULT.
    assert.equal(defaults[0].severity, "INFO", JSON.stringify(defaults[0]));

    writeFileSync(join(root, "keel.toml"), "");
    const realRoot = realpathSync(root);
    const proc = run(root, { KEEL_LOG_FORMAT: "json" });
    assert.equal(proc.status, 7, proc.stderr);
    const objs = keelJsonLines(proc.stderr);
    assert.ok(!proc.stderr.includes("keel ▸"), `json mode must emit no prose: ${proc.stderr}`);
    assert.deepEqual(objs.map((o) => o.keel), ["activation", "summary"], proc.stderr);
    assert.equal(objs[0].policy_source, "keel.toml");
    assert.equal(objs[0].policy_path, join(realRoot, "keel.toml"));
    assert.equal(objs[0].root, realRoot);
    assert.equal(objs[0].root_source, "cwd");
    assert.ok(!("note" in objs[0]), `a loaded policy has no em-dash tail — no note key: ${objs[0].note}`);
    assert.equal(objs[0].dev_cache_off, null, "no serverless marker here — the dev cache stayed on");
    // #130: both the activation and the summary line are INFO.
    assert.equal(objs[0].severity, "INFO", proc.stderr);
    assert.equal(objs[1].severity, "INFO", proc.stderr);
    // Which backend resolved has to be a FIELD, not just prose: `emit` writes
    // the object INSTEAD of the text in json mode, so a structured-logging
    // deployment can only index what the object names (F10, the same reason
    // `dev_cache_off` is a field). Python's twin carries the identical key
    // with the identical "native"/"stub" vocabulary.
    // Forced to the in-repo engine so the expected value is exact rather than
    // "whichever of the two this machine happened to load".
    const stubObjs = keelJsonLines(
      run(root, { KEEL_LOG_FORMAT: "json", KEEL_BACKEND: "stub" }).stderr
    );
    assert.equal(stubObjs[0].backend, "stub", JSON.stringify(stubObjs[0]));
    assert.equal(objs[1].calls, 0);
    assert.equal(objs[1].policy_source, "keel.toml");

    // The banner's `(dev cache off: K_SERVICE detected)` suffix, as a named
    // field: a log pipeline can only index what the object names. Twin of
    // python/keel/tests/test_slice1_acceptance.py's (b)/(b2).
    const served = run(root, { KEEL_LOG_FORMAT: "json", K_SERVICE: "render" });
    assert.equal(keelJsonLines(served.stderr)[0].dev_cache_off, "K_SERVICE", served.stderr);
    const prose = run(root, { K_SERVICE: "render" });
    assert.ok(
      prose.stderr.includes("(dev cache off: K_SERVICE detected)"),
      `the text twin must say the same thing: ${prose.stderr}`,
    );

    // The field reads from the RESOLVED state, so it names the explicit
    // override too — wider than the banner's marker-only parenthetical, which
    // stays byte-unchanged (a `dev_cache_off: null` in a KEEL_ENV=prod process
    // would be a positive claim that the dev cache is on, and it is not).
    const explicit = run(root, { KEEL_LOG_FORMAT: "json", KEEL_ENV: "prod" });
    assert.equal(keelJsonLines(explicit.stderr)[0].dev_cache_off, "KEEL_ENV", explicit.stderr);
    const explicitProse = run(root, { KEEL_ENV: "prod" });
    assert.ok(
      !explicitProse.stderr.includes("dev cache off"),
      `the text form stays marker-only prose: ${explicitProse.stderr}`,
    );
    const devWins = run(root, { KEEL_LOG_FORMAT: "json", KEEL_ENV: "dev", K_SERVICE: "render" });
    assert.equal(keelJsonLines(devWins.stderr)[0].dev_cache_off, null, devWins.stderr);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("KEEL_LOG_FORMAT=json turns the refusal into one error object", () => {
  const root = mkdtempSync(join(tmpdir(), "keel-json-refusal-"));
  try {
    const realRoot = realpathSync(root);
    const proc = run(root, { KEEL_CWD: realRoot, KEEL_LOG_FORMAT: "json" });
    assert.equal(proc.status, 7, proc.stderr);
    const objs = keelJsonLines(proc.stderr);
    assert.ok(!proc.stderr.includes("keel ▸"), `json mode must emit no prose: ${proc.stderr}`);
    assert.equal(objs.length, 1, proc.stderr);
    assert.equal(objs[0].keel, "error");
    assert.equal(objs[0].code, "policy-missing-at-keel-cwd");
    assert.equal(objs[0].keel_cwd, realRoot);
    // #130: the refusal is the one line that MUST read ERROR — it must not be
    // indistinguishable from a healthy activation in a severity>=ERROR view.
    assert.equal(objs[0].severity, "ERROR", proc.stderr);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("the policy banner names the file", () => {
  const root = mkdtempSync(join(tmpdir(), "keel-banner-policy-path-"));
  try {
    writeFileSync(join(root, "keel.toml"), "");
    const realRoot = realpathSync(root);
    const proc = run(root);
    assert.equal(proc.status, 7, proc.stderr);
    assert.match(proc.stderr, new RegExp(`with policy ${esc(join(realRoot, "keel.toml"))}`));
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("an activated process leaves one activation row naming its policy", () => {
  const root = mkdtempSync(join(tmpdir(), "keel-activation-row-"));
  try {
    writeFileSync(join(root, "keel.toml"), "");
    const realRoot = realpathSync(root);
    const proc = run(root, { KEEL_CWD: realRoot });
    assert.equal(proc.status, 7, proc.stderr);
    const { DatabaseSync } = createRequire(import.meta.url)("node:sqlite");
    const db = new DatabaseSync(join(realRoot, ".keel", "discovery.db"));
    // node:sqlite rows are null-prototype objects — spread into a plain object
    // so deepEqual compares values, not prototypes (see discovery.test.mjs).
    const row = { ...db.prepare("SELECT language, policy_source, policy_path, keel_cwd FROM activations").get() };
    assert.deepEqual(row, { language: "node", policy_source: "keel.toml", policy_path: join(realRoot, "keel.toml"), keel_cwd: realRoot });
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
