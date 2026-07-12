/**
 * Process-wide runtime state, shared between the bootstrap (`--import keel/hook`,
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
