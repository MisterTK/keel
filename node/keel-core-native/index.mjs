/**
 * keel-core-native: loader for the napi addon that wraps keel-core::Engine
 * (crates/keel-node). It exposes the same object surface as node/keel-core-stub
 * — `KeelCore` with `configure` / `execute` / `executeAsync` / `report` and the
 * harness-only `advanceClock` + `new KeelCore({ paused: true })` — so it is a
 * drop-in native backend behind node/keel/src/backend.mjs (which probes
 * `KeelCoreNative`).
 *
 * Binaries are NOT committed. Build the addon locally, then this loader finds
 * it. Two supported flows:
 *
 *   Canonical (copy into the package):
 *     cargo build -p keel-node --release
 *     cp ../../target/release/libkeel_node.dylib keel-core-native.node   # .so / .dll on Linux / Windows
 *
 *   Dev convenience (no copy — loaded straight from target/):
 *     cargo build -p keel-node --release        # or `npm run build` here
 *
 * When no binary is present, `KeelCore` is `undefined` and `loaded` is false;
 * callers (the front-end backend probe, the conformance test) degrade cleanly.
 */

import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";

// Platform-specific cdylib filename produced by `cargo build -p keel-node`.
const CDYLIB =
  process.platform === "win32"
    ? "keel_node.dll"
    : process.platform === "darwin"
      ? "libkeel_node.dylib"
      : "libkeel_node.so";

// Search order: the canonical installed artifact, then dev target/ outputs.
const CANDIDATES = [
  new URL("./keel-core-native.node", import.meta.url),
  new URL(`../../target/release/${CDYLIB}`, import.meta.url),
  new URL(`../../target/debug/${CDYLIB}`, import.meta.url),
];

function loadNative() {
  for (const url of CANDIDATES) {
    const path = fileURLToPath(url);
    if (!existsSync(path)) continue;
    try {
      // process.dlopen loads a native addon from any path/extension (unlike
      // `require`, which only handles `.node`), so a cargo-built cdylib works
      // directly — no rename required for the dev flow.
      const module = { exports: {} };
      process.dlopen(module, path);
      return module.exports;
    } catch {
      // Incompatible ABI / not an addon — try the next candidate.
    }
  }
  return null;
}

const native = loadNative();

/** True when the native addon loaded (build it first, otherwise false). */
export const loaded = native != null;

/** The native core class, or `undefined` if the addon is not built. */
export const KeelCore = native?.KeelCore;

/** Alias probed by node/keel/src/backend.mjs (`mod.KeelCoreNative`). */
export const KeelCoreNative = native?.KeelCore;

export default native?.KeelCore;
