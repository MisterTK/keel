#!/usr/bin/env node
/**
 * `keel [args...]` — npm entry point for the `keel` CLI binary (run/doctor/
 * init/status/mcp/explain/...). Resolves the prebuilt binary for the running
 * platform (an optionalDependency of this package — see package.json and
 * scripts/cli-prebuild.sh) and re-execs it, inheriting stdio and propagating
 * the child's exit code/signal exactly like node/keel/bin/keel-node-run.mjs
 * does for the Node front end's own run helper.
 *
 * Resolution order (first that exists wins):
 *   1. The installed per-platform prebuild package, e.g.
 *      `keelrun-cli-darwin-arm64` (npm only installs the ONE optional
 *      dependency matching the running os/cpu; the other four are silently
 *      skipped).
 *   2. `../../../target/{release,debug}/keel[.exe]` — the from-source dev
 *      flow (`cargo build -p keelrun-cli --release`), no copy needed.
 */

import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const BIN_NAME = process.platform === "win32" ? "keel.exe" : "keel";

// npm platform-package suffix for the running process — matches the
// `keelrun-cli-<platformKey>` optional dependencies and scripts/cli-prebuild.sh.
// `null` for combinations we don't build: falls through to the dev-flow
// candidates below. Linux keys say "musl" because that's how the binaries
// are built (static, so they run on glibc hosts too — see cli-binaries in
// .github/workflows/release.yml), not because glibc hosts are excluded.
function platformKey() {
  const { platform, arch } = process;
  if (platform === "darwin" && arch === "x64") return "darwin-x64";
  if (platform === "darwin" && arch === "arm64") return "darwin-arm64";
  if (platform === "linux" && arch === "x64") return "linux-x64-musl";
  if (platform === "linux" && arch === "arm64") return "linux-arm64-musl";
  if (platform === "win32" && arch === "x64") return "win32-x64";
  return null;
}

/** Resolve the installed `keelrun-cli-<platformKey>` package's binary via
 * normal Node module resolution. Returns `[]` off the 5 built platforms or
 * when the optional dependency wasn't installed (unmet optional deps are
 * silently skipped by npm, so this must never throw). */
function resolvePlatformPackage() {
  const key = platformKey();
  if (key == null) return [];
  try {
    const require = createRequire(import.meta.url);
    return [require.resolve(`keelrun-cli-${key}`)];
  } catch {
    return []; // not installed (unmet optional dep, or a from-source checkout)
  }
}

// Search order: the installed platform package, then dev target/ outputs.
const CANDIDATES = [
  ...resolvePlatformPackage(),
  new URL(`../../../target/release/${BIN_NAME}`, import.meta.url),
  new URL(`../../../target/debug/${BIN_NAME}`, import.meta.url),
];

function resolveBinary() {
  for (const candidate of CANDIDATES) {
    const path = typeof candidate === "string" ? candidate : fileURLToPath(candidate);
    if (existsSync(path)) return path;
  }
  return null;
}

const bin = resolveBinary();
if (bin == null) {
  process.stderr.write(
    "keel: no prebuilt binary for this platform, and no local build at target/{release,debug}/keel.\n" +
      "Build one with `cargo build -p keelrun-cli --release`, or file an issue if your platform should be supported.\n",
  );
  process.exit(1);
}

// Re-exec the resolved binary, inheriting stdio. A crash needs its
// termination signal to actually reach whoever invoked `keel` (a shell, a
// script, a test asserting on `kill -9`) — re-raise it on ourselves rather
// than falling through to a synthetic exit code, mirroring
// keel-node-run.mjs's same pattern for the Node front end's run helper.
const result = spawnSync(bin, process.argv.slice(2), {
  stdio: "inherit",
  env: process.env,
});

if (result.error) {
  process.stderr.write(`keel: ${result.error.message}\n`);
  process.exit(1);
}
if (result.signal) {
  process.kill(process.pid, result.signal);
  process.exit(128);
}
process.exit(result.status ?? 1);
