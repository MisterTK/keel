/**
 * Process-wide runtime state, shared between the bootstrap (`--import keelrun/hook`,
 * main thread) and the code injected into transformed modules by the ESM loader
 * (`loader-runtime.mjs`, also main thread). Both import THIS module from the
 * same absolute path, so Node's module cache guarantees they see one instance.
 *
 * State is deliberately minimal and mutable-once: the backend and discovery
 * recorder are installed at bootstrap. When Keel is disabled, nothing is set
 * and `getBackend()` returns null (wrappers then fall through to the original).
 */

let backend = null;
let discovery = null;
let enabled = false;

export function setRuntime(next) {
  backend = next.backend ?? null;
  discovery = next.discovery ?? null;
  enabled = next.enabled ?? false;
}

export function getBackend() {
  return backend;
}

export function getDiscovery() {
  return discovery;
}

export function isEnabled() {
  return enabled;
}

// --- durable-flow scope guard ------------------------------------------------
// The native core has ONE active-flow slot. The `keel run` ts: flow path
// (`flow.mjs`'s `runAsFlow`) and the `child_process` pack both open flows on the
// SAME backend, so a matched synchronous `spawnSync` fired from inside a ts:
// flow body would clobber the outer flow's slot. This shared depth counter lets
// the pack detect an already-open flow scope and pass the command through
// unwrapped (its pre-pack behavior — a sync subprocess is never journaled inside
// a ts: flow anyway) instead of corrupting the outer flow.

let flowDepth = 0;

/** Enter a durable-flow scope (called by `runAsFlow` around the flow body). */
export function markFlowEntered() {
  flowDepth += 1;
}

/** Leave a durable-flow scope. Never drops below zero. */
export function markFlowExited() {
  if (flowDepth > 0) flowDepth -= 1;
}

/** True iff a durable flow is currently open on the shared backend. */
export function flowScopeActive() {
  return flowDepth > 0;
}

/** Attach the outcome envelope to an object without altering its shape
 *  (non-enumerable, so JSON/iteration are unchanged — DX invariant 5). */
export function attachOutcome(obj, outcome) {
  if (obj === null || (typeof obj !== "object" && typeof obj !== "function")) return obj;
  try {
    Object.defineProperty(obj, "keelOutcome", {
      value: outcome,
      enumerable: false,
      configurable: true,
      writable: true,
    });
  } catch {
    // frozen/sealed target — leave it untouched rather than risk a throw.
  }
  return obj;
}
