/**
 * Preload entry: `node --import keelrun/hook app.mjs`.
 *
 * Runs on the main thread before the entrypoint. Top-level await ensures the
 * backend is configured and the fetch seam + ESM loader are installed before
 * the app (and its dependencies) are imported. A config error (KEEL-E001)
 * rejects here, so Node aborts before running the app — a loud, correct failure.
 *
 * Tier 2: if `process.argv[1]` (the script Node is about to run as its main
 * module) is a designated flow entrypoint, we import it and run it ourselves,
 * as a durable flow, INSTEAD of letting Node load it normally — see
 * `src/flow.mjs`'s module docs for why `runAsFlow` then always terminates the
 * process rather than returning control to Node's own module loader.
 */

import { installKeel } from "./src/bootstrap.mjs";
import { matchFlow, runAsFlow } from "./src/flow.mjs";

const state = await installKeel();
if (state.enabled && process.argv[1]) {
  const entry = matchFlow(process.argv[1], process.cwd(), state.flowEntrypoints ?? []);
  if (entry) {
    await runAsFlow(process.argv[1], entry, state.backend, process.argv.slice(2));
    // unreachable: runAsFlow always calls process.exit() (see its module docs).
  }
}
