// #129 fast-follow (review item 1): `bootstrap.mjs` sets `meta.backend`, and
// `summary.mjs::formatSummaryJson` spreads `meta` wholesale into the
// `KEEL_LOG_FORMAT=json` *summary* line — so the summary line should carry
// `backend` too, exactly like the activation line already does (and exactly
// like the Python front end's twin: `_STATE.meta["backend"]` flows into
// `_summary.py`, pinned end-to-end by
// `test_auto.py::test_backend_is_named_end_to_end_under_the_stub`).
//
// The shared conformance corpus (`conformance/console_summary_json/
// 05-stub-backend.json`) already expects `"backend":"stub"` in the summary
// object, but its drivers feed a `meta` object DIRECTLY to the formatter —
// they exercise formatSummaryJson, never the front end's construction of
// `meta` — so a front end that built the wrong `meta` (or never folded
// `backend` into it) would pass the corpus and still ship broken. This test
// goes through the real bootstrap/exit-flush path (a real child process, real
// stderr) specifically to close that gap.

import test from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const bootstrapUrl = new URL("../src/bootstrap.mjs", import.meta.url).href;

test("a real child process's KEEL_LOG_FORMAT=json summary line names the resolved backend", () => {
  const dir = mkdtempSync(join(tmpdir(), "keel-bootstrap-summary-"));
  const childPath = join(dir, "child.mjs");
  writeFileSync(
    childPath,
    `import { installKeel } from ${JSON.stringify(bootstrapUrl)};\n` +
      `await installKeel({ cwd: process.cwd(), env: process.env });\n` +
      `process.exit(0);\n`
  );
  try {
    const result = spawnSync(process.execPath, [childPath], {
      cwd: dir,
      env: { ...process.env, KEEL_BACKEND: "stub", KEEL_LOG_FORMAT: "json" },
      encoding: "utf8",
    });
    assert.equal(result.status, 0, result.stderr);
    const lines = result.stderr.split("\n").filter((l) => l.trim());
    const objs = lines.map((l) => JSON.parse(l));
    const kinds = objs.map((o) => o.keel);
    assert.deepEqual(kinds, ["activation", "summary"], result.stderr);
    assert.equal(objs[1].backend, "stub", result.stderr);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
