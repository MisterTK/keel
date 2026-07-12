#!/usr/bin/env node
/**
 * `keel-node-run <app.mjs> [args...]` — spawn node with the Keel hook preloaded.
 * The public `keel run` CLI (Task 8) dispatches here for Node entrypoints.
 * Transparent: stdio is inherited and the child's exit code is propagated, so
 * wrapping is invisible except for Keel's own resilience + stderr banner.
 */

import { spawnSync } from "node:child_process";

const hookUrl = new URL("../hook.mjs", import.meta.url).href;
const args = process.argv.slice(2);
const result = spawnSync(process.execPath, ["--import", hookUrl, ...args], {
  stdio: "inherit",
  env: process.env,
});

if (result.error) {
  process.stderr.write(`keel-node-run: ${result.error.message}\n`);
  process.exit(1);
}
process.exit(result.status ?? 1);
