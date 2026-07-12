/**
 * Backend selection, isolated behind one seam (architecture §5.2, "the swap").
 *
 * Priority:
 *   1. native addon (`keel-core-native`) when loadable — the eventual napi core
 *      (Task 7/14). It is probed by dynamic import and must expose an async
 *      `execute`; it may not exist yet, so failure to load is normal.
 *   2. the in-repo Node backend (AsyncEngine over keel-core-stub semantics).
 *
 * `KEEL_BACKEND` overrides selection:
 *   - `stub`   → force the in-repo engine (never probe native)
 *   - `native` → require the native addon; throw KEEL-E040 if not loadable
 *   - unset    → auto (native if loadable, else engine)
 */

import { AsyncEngine, realClock, KeelError } from "./engine.mjs";

const NATIVE_CANDIDATES = [
  "keel-core-native", // published/installed addon
  "../keel-core-native/index.mjs", // in-repo sibling (worktree), when built
];

async function tryLoadNative() {
  for (const spec of NATIVE_CANDIDATES) {
    try {
      const mod = await import(spec);
      const Ctor = mod.KeelCoreNative ?? mod.default;
      if (typeof Ctor === "function") {
        const inst = new Ctor();
        if (typeof inst.execute === "function" && typeof inst.configure === "function") {
          inst.kind ??= "native";
          return inst;
        }
      }
    } catch {
      // not present / not loadable — expected until the addon ships.
    }
  }
  return null;
}

/**
 * Resolve the runtime backend. Returns an object exposing async
 * `execute(request, effect)`, `configure(policy)`, `layer(target, key)`,
 * and `report()`.
 */
export async function loadBackend({ preferred = process.env.KEEL_BACKEND, clock } = {}) {
  if (preferred !== "stub") {
    const native = await tryLoadNative();
    if (native) return native;
    if (preferred === "native")
      throw new KeelError(
        "KEEL-E040",
        "KEEL_BACKEND=native requested but keel-core-native is not loadable"
      );
  }
  return new AsyncEngine(clock ?? realClock());
}
