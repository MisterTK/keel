// The `refused` module-level latch (bootstrap.mjs) is, by design, a per-process
// singleton with no reset hook — like the pre-existing `installed` latch next
// to it. It is a PRINT-once latch, though, not a process-wide verdict: it only
// answers a later installKeel() call that names the SAME `(cwd, cwdSource)` it
// was printed for (a different root was never asked about — see the second
// test). Either way it is incompatible with sharing a process with any other
// in-process installKeel() test (bootstrap-packs.test.mjs's test relies on a
// SUCCESSFUL install persisting; a refused install here would corrupt it, and
// vice versa) — hence its own file, so node --test's per-file process
// isolation keeps the two apart without inventing a reset mechanism.

import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { installKeel, normalizeCwd } from "../src/bootstrap.mjs";

/** The shared corpus: every row's expected value is literally what Python's
 *  `str(Path(row))` returns, pinned on the Python side by
 *  test_auto.py::test_path_normalization_corpus_matches_the_node_twin. Note
 *  `/a/../b`: pathlib does NOT resolve `..`, so neither may we (this is why
 *  `path.normalize`, which does, is the wrong tool). POSIX rows only — see
 *  `normalizeCwd`'s note on win32. */
const NORMALIZATION_CORPUS = [
  ["/app/", "/app"],
  ["/app", "/app"],
  ["/app//x/./y", "/app/x/y"],
  ["//app", "//app"],
  ["///app", "/app"],
  [".", "."],
  ["./foo", "foo"],
  ["/", "/"],
  ["a/b/", "a/b"],
  ["/a/../b", "/a/../b"],
  ["", "."],
  ["/a/./b/", "/a/b"],
];

test("normalizeCwd reproduces pathlib's normalization byte for byte", { skip: process.platform === "win32" }, () => {
  for (const [input, expected] of NORMALIZATION_CORPUS) {
    assert.equal(normalizeCwd(input), expected, JSON.stringify(input));
  }
});

test("KEEL_CWD refusal is remembered module-wide — a second call returns the same object without printing again", async () => {
  const dir = mkdtempSync(join(tmpdir(), "keel-bootstrap-refuse-"));
  const writes = [];
  const originalWrite = process.stderr.write.bind(process.stderr);
  process.stderr.write = (chunk) => {
    writes.push(String(chunk));
    return true;
  };
  try {
    const first = await installKeel({ cwd: dir, env: {}, cwdSource: "KEEL_CWD" });
    const second = await installKeel({ cwd: dir, env: {}, cwdSource: "KEEL_CWD" });
    // A trailing separator is normalized away before the latch key is compared
    // (and before anything is printed), exactly as Python's `str(Path(cwd))`
    // does — so this is the SAME root, answered by the latch, not a new one.
    const third = await installKeel({ cwd: `${dir}/`, env: {}, cwdSource: "KEEL_CWD" });
    const expected = { enabled: false, reason: "policy-missing-at-keel-cwd", root: dir };
    assert.deepEqual(first, expected);
    assert.deepEqual(second, expected);
    assert.deepEqual(third, expected);
    const keelLines = writes.join("").split("\n").filter((l) => l.startsWith("keel ▸"));
    assert.equal(keelLines.length, 1, `expected exactly one keel line across all calls: ${writes.join("")}`);
  } finally {
    process.stderr.write = originalWrite;
    rmSync(dir, { recursive: true, force: true });
  }
});

// Runs AFTER the refusal above (same process, same module instance): that is
// the point — the latch must not answer a question it was never asked.
test("a later call naming a DIFFERENT, valid root activates normally", async () => {
  const dir = mkdtempSync(join(tmpdir(), "keel-bootstrap-good-"));
  try {
    writeFileSync(join(dir, "keel.toml"), "");
    const res = await installKeel({ cwd: dir, env: { KEEL_QUIET: "1" }, cwdSource: "cwd" });
    assert.equal(res.enabled, true, JSON.stringify(res));
    assert.notEqual(res.reason, "policy-missing-at-keel-cwd");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
