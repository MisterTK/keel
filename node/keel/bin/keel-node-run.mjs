#!/usr/bin/env node
/**
 * `keel-node-run <app.mjs> [args...]` — spawn node with the Keel hook preloaded.
 * The public `keel run` CLI (Task 8) dispatches here for Node entrypoints.
 * Transparent: stdio is inherited and the child's exit code is propagated, so
 * wrapping is invisible except for Keel's own resilience + stderr banner.
 *
 * Tier 2: no special handling needed HERE for a durable-flow target — the
 * child's `--import hook.mjs` preload sees the same `<app.mjs> [args...]` as
 * `process.argv[1]`/`.slice(2)` before Node loads `<app.mjs>` as its main
 * module, and dispatches to `src/flow.mjs`'s `runAsFlow` itself when it
 * matches a `[flows]` entrypoint (see `hook.mjs`).
 *
 * A crash-tested durable flow needs its termination signal to actually reach
 * whoever spawned `keel-node-run` (a shell, `keel flows`, a test asserting on
 * `kill -9`) — losing it behind a synthetic `process.exit(1)` would make
 * every crash look like an ordinary failure. So when the child dies by
 * SIGNAL rather than exiting, we re-raise that SAME signal on ourselves
 * (mirroring `installExitFlush`'s re-raise pattern in `src/bootstrap.mjs`)
 * instead of falling through to `result.status ?? 1`.
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
if (result.signal) {
  process.kill(process.pid, result.signal);
  // Most signals (SIGKILL/SIGSEGV-equivalent) terminate us immediately above
  // and this line never runs; a caught/ignored one falls through to the
  // conventional 128+n shell exit code as a fallback.
  process.exit(128);
}
process.exit(result.status ?? 1);
