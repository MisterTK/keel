/**
 * Preload entry: `node --import keelrun/hook app.mjs`.
 *
 * Runs on the main thread before the entrypoint. Top-level await ensures the
 * backend is configured and the fetch seam + ESM loader are installed before
 * the app (and its dependencies) are imported. A config error (KEEL-E001)
 * rejects here, so Node aborts before running the app — a loud, correct failure.
 *
 * `KEEL_CWD` relocates where `keel.toml` is loaded from (see the call below).
 *
 * Tier 2: if `process.argv[1]` (the script Node is about to run as its main
 * module) is a designated flow entrypoint, we import it and run it ourselves,
 * as a durable flow, INSTEAD of letting Node load it normally — see
 * `src/flow.mjs`'s module docs for why `runAsFlow` then always terminates the
 * process rather than returning control to Node's own module loader.
 */

import { installKeel } from "./src/bootstrap.mjs";
import { matchFlow, runAsFlow } from "./src/flow.mjs";

// `KEEL_CWD` relocates the CONFIG ROOT — the directory `keel.toml` is read
// from — without moving the process's real working directory. It is what a
// launcher that owns cwd (agents-cli, uvicorn, a container ENTRYPOINT) needs
// when the policy lives in the deployable app directory. `keel run` already
// exports it to every child it spawns (`crates/keel-cli/src/run.rs`'s
// `activation_env`) expecting exactly this, and Python's `.pth` shim
// (`keel/_auto.py`) already honors it — Node honoring it too is the parity
// this env var was always documented to have. Unset or empty falls back to
// the real cwd (so `keel run`'s own child, where the two are equal, is
// unaffected). Only CONFIG resolution moves: `matchFlow` below still resolves
// `process.argv[1]` against the real cwd, exactly as Node itself does.
const state = await installKeel({ cwd: process.env.KEEL_CWD || process.cwd() });
if (state.enabled && process.argv[1]) {
  const entry = matchFlow(process.argv[1], process.cwd(), state.flowEntrypoints ?? []);
  if (entry) {
    await runAsFlow(process.argv[1], entry, state.backend, process.argv.slice(2));
    // unreachable: runAsFlow always calls process.exit() (see its module docs).
  }
}
