/**
 * Public programmatic surface for the `keel` Node package. The primary entry is
 * the preload hook (`node --import keel/hook app.mjs`); this module exposes the
 * same bootstrap for embedding and the pieces useful to tooling/tests.
 */

export { installKeel, isDisabled } from "./src/bootstrap.mjs";
export { loadBackend } from "./src/backend.mjs";
export { loadPolicy, parseToml, extractFunctionTargets } from "./src/policy.mjs";
export { level0Defaults } from "./src/defaults.mjs";
export { LLM_HOST_PROVIDERS } from "./src/judge.mjs";
export { KeelError } from "./src/engine.mjs";

export const VERSION = "0.1.0";
