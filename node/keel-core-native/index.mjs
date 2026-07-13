/**
 * keel-core-native: loader for the napi addon that wraps keel-core::Engine
 * (crates/keel-node). It exposes the same object surface as node/keel-core-stub
 * — `KeelCore` with `configure` / `execute` / `executeAsync` / `report` and the
 * harness-only `advanceClock` + `new KeelCore({ paused: true })` — so it is a
 * drop-in native backend behind node/keel/src/backend.mjs (which probes
 * `KeelCoreNative`).
 *
 * Resolution order (first that loads wins):
 *
 *   1. The installed per-platform prebuild package, e.g.
 *      `@keel/core-native-darwin-arm64` (an `optionalDependency` of this
 *      package — see package.json's `napi` config + scripts/napi-prebuild.sh,
 *      which stages these under npm/<platform>/ and is what the release
 *      workflow packs). npm only pulls in the ONE optional dependency that
 *      matches the running os/cpu; the other three are silently skipped.
 *   2. `./keel-core-native.node` next to this file (a binary copied in by
 *      hand, or staged locally without going through npm install).
 *   3. `../../target/{release,debug}/<cdylib>` — the from-source dev flow:
 *      `cargo build -p keel-node --release` (or `npm run build` here), no
 *      copy needed.
 *
 * When no binary is present anywhere, `KeelCore` is `undefined` and `loaded`
 * is false; callers (the front-end backend probe, the conformance test)
 * degrade cleanly.
 */

import { existsSync } from "node:fs";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

// napi-rs platform key for the running process — matches the suffix on the
// `@keel/core-native-<platformKey>` optional dependencies and the
// npm/<platformKey>/ directories staged by scripts/napi-prebuild.sh. `null`
// for combinations we don't build (e.g. win32, arm): falls through to the
// dev-flow candidates below.
function platformKey() {
  const { platform, arch } = process;
  if (platform === "darwin" && arch === "x64") return "darwin-x64";
  if (platform === "darwin" && arch === "arm64") return "darwin-arm64";
  if (platform === "linux" && arch === "x64") return "linux-x64-gnu";
  if (platform === "linux" && arch === "arm64") return "linux-arm64-gnu";
  return null;
}

// Platform-specific cdylib filename produced by `cargo build -p keel-node`.
const CDYLIB =
  process.platform === "win32"
    ? "keel_node.dll"
    : process.platform === "darwin"
      ? "libkeel_node.dylib"
      : "libkeel_node.so";

/** Resolve the installed `@keel/core-native-<platformKey>` package's main
 * file via normal Node module resolution — this is what makes a plain
 * `npm install` on a matching platform find the right prebuild without any
 * env/config. Returns `[]` off the 4 built platforms or when the optional
 * dependency wasn't installed (unmet optional deps are silently skipped by
 * npm, so this must never throw). */
function resolvePlatformPackage() {
  const key = platformKey();
  if (key == null) return [];
  try {
    const require = createRequire(import.meta.url);
    return [require.resolve(`@keel/core-native-${key}`)];
  } catch {
    return []; // not installed (unmet optional dep, or a from-source checkout)
  }
}

// Search order: the installed platform package, the canonical staged
// artifact, then dev target/ outputs.
const CANDIDATES = [
  ...resolvePlatformPackage(),
  new URL("./keel-core-native.node", import.meta.url),
  new URL(`../../target/release/${CDYLIB}`, import.meta.url),
  new URL(`../../target/debug/${CDYLIB}`, import.meta.url),
];

function loadNative() {
  for (const candidate of CANDIDATES) {
    const path = typeof candidate === "string" ? candidate : fileURLToPath(candidate);
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
