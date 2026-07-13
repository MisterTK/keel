// End-to-end: the eve pack's `tool:<name>` rewrite, exercised through the real
// `--import` + `module.register` path (like loader.e2e.test.mjs), against a
// fake `eve` package (offline fixture: no real `eve` dependency anywhere in
// this repo) that mirrors eve's documented `defineTool({ execute })` shape.

import test from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";

const hookUrl = new URL("../hook.mjs", import.meta.url).href;
const require = createRequire(import.meta.url);
const { DatabaseSync } = require("node:sqlite");

/** Lay out a fixture project with a fake `eve` package (in node_modules) and
 *  one discovered tool module, mirroring eve's documented convention. */
function makeEveProject(toolBody) {
  const dir = mkdtempSync(join(tmpdir(), "keel-eve-"));
  const eveDir = join(dir, "node_modules", "eve");
  mkdirSync(eveDir, { recursive: true });
  mkdirSync(join(dir, "agent", "tools"), { recursive: true });

  writeFileSync(
    join(eveDir, "package.json"),
    JSON.stringify({
      name: "eve",
      version: "0.1.0",
      type: "module",
      exports: { "./tools": "./tools.mjs", "./package.json": "./package.json" },
    })
  );
  writeFileSync(
    join(eveDir, "tools.mjs"),
    `export function defineTool(def) { return { __realEveTool: true, ...def }; }\n`
  );
  writeFileSync(join(dir, "agent", "tools", "get_weather.mjs"), toolBody);
  writeFileSync(
    join(dir, "app.mjs"),
    [
      'import tool from "./agent/tools/get_weather.mjs";',
      'process.stdout.write("DESC:" + tool.description + "\\n");',
      "try {",
      "  const r = await tool.execute({ city: \"nyc\" });",
      '  process.stdout.write("RESULT:" + r + "\\n");',
      "} catch (e) {",
      '  process.stdout.write("THREW:" + e.message + " code=" + (e.keelOutcome?.error?.code ?? e.code) + "\\n");',
      "}",
      "",
    ].join("\n")
  );
  return dir;
}

test("eve pack wraps a discovered tool module: success path is transparent, target is recorded", () => {
  const dir = makeEveProject(
    [
      'import { defineTool } from "eve/tools";',
      "export default defineTool({",
      '  description: "weather lookup",',
      "  async execute({ city }) {",
      '    return "sunny-in-" + city;',
      "  },",
      "});",
      "",
    ].join("\n")
  );
  try {
    const run = spawnSync(process.execPath, ["--import", hookUrl, "app.mjs"], {
      cwd: dir,
      env: { ...process.env, KEEL_DISABLE: undefined, KEEL_QUIET: undefined },
      encoding: "utf8",
    });
    assert.equal(run.status, 0, `child failed:\n${run.stderr}`);
    // description and every other def field pass through untouched.
    assert.match(run.stdout, /DESC:weather lookup/);
    assert.match(run.stdout, /RESULT:sunny-in-nyc/);
    // banner reflects the detected eve pack, on stderr only.
    assert.match(run.stderr, /eve tool modules/);

    const dbPath = join(dir, ".keel", "discovery.db");
    assert.ok(existsSync(dbPath), "discovery.db should be written on exit");
    const db = new DatabaseSync(dbPath);
    try {
      const row = db.prepare("SELECT * FROM discovery WHERE target=?").get("tool:get_weather");
      assert.ok(row, "discovery row for the tool: target");
      assert.equal(row.calls, 1);
      assert.equal(row.successes, 1);
    } finally {
      db.close();
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("eve tools are non-idempotent by default: observed, not retried, even when opted into retry.on (KEEL-E014)", () => {
  const dir = makeEveProject(
    [
      'import { defineTool } from "eve/tools";',
      "export default defineTool({",
      '  description: "flaky tool",',
      "  async execute() {",
      '    throw new Error("boom");',
      "  },",
      "});",
      "",
    ].join("\n")
  );
  try {
    // Explicitly opt tool: into retrying the `other` class — a ts: target
    // would retry under this exact policy (loader-runtime.mjs's own escape
    // hatch); an eve-discovered tool: target must NOT, because it defaults to
    // non-idempotent (no per-target consent was ever given).
    writeFileSync(
      join(dir, "keel.toml"),
      '[target."tool:get_weather"]\nretry = { attempts = 3, schedule = "fixed(1ms)", on = ["other"] }\n'
    );
    const run = spawnSync(process.execPath, ["--import", hookUrl, "app.mjs"], {
      cwd: dir,
      env: { ...process.env, KEEL_DISABLE: undefined, KEEL_QUIET: undefined },
      encoding: "utf8",
    });
    assert.equal(run.status, 0, `child failed:\n${run.stderr}`);
    assert.match(run.stdout, /THREW:boom code=KEEL-E014/);

    const db = new DatabaseSync(join(dir, ".keel", "discovery.db"));
    try {
      const row = db.prepare("SELECT * FROM discovery WHERE target=?").get("tool:get_weather");
      assert.equal(row.calls, 1, "a single attempt — retry.on=[other] was NOT honored");
      assert.equal(row.retries, 0);
      assert.equal(row.failures, 1);
    } finally {
      db.close();
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("KEEL_DISABLE skips the eve rewrite entirely (tool runs unwrapped)", () => {
  const dir = makeEveProject(
    [
      'import { defineTool } from "eve/tools";',
      "export default defineTool({",
      '  description: "d",',
      "  async execute() {",
      '    throw new Error("boom");',
      "  },",
      "});",
      "",
    ].join("\n")
  );
  try {
    const run = spawnSync(process.execPath, ["--import", hookUrl, "app.mjs"], {
      cwd: dir,
      env: { ...process.env, KEEL_DISABLE: "1" },
      encoding: "utf8",
    });
    assert.equal(run.status, 0, run.stderr);
    assert.match(run.stdout, /THREW:boom code=undefined/, "no keelOutcome attached when disabled");
    assert.equal(run.stderr, "", "disabled run emits no banner");
    assert.ok(!existsSync(join(dir, ".keel", "discovery.db")), "no discovery when disabled");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
