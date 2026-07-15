"use strict";
/**
 * CJS preload shim: `node --require keelrun/hook app.cjs`.
 *
 * `--require` runs synchronously and cannot await, so this only kicks off the
 * async ESM bootstrap. Interception is armed as soon as the dynamic import
 * resolves (a microtask), which covers the common case but is best-effort for
 * calls made synchronously at the very top of the entrypoint. Prefer `--import`
 * (ESM) when you need the seam active before the first line of user code.
 */

import("./hook.mjs").catch((err) => {
  process.stderr.write(`${err?.message ?? err}\n`);
  process.exitCode = 1;
});
