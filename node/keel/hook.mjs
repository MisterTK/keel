/**
 * Preload entry: `node --import keel/hook app.mjs`.
 *
 * Runs on the main thread before the entrypoint. Top-level await ensures the
 * backend is configured and the fetch seam + ESM loader are installed before
 * the app (and its dependencies) are imported. A config error (KEEL-E001)
 * rejects here, so Node aborts before running the app — a loud, correct failure.
 */

import { installKeel } from "./src/bootstrap.mjs";

await installKeel();
