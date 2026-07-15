#!/usr/bin/env node
// License audit for the Node side of the repo (NFR6 / engineering-manifesto
// rule 12: front ends carry zero runtime dependencies). cargo-deny (deny.toml)
// covers the Rust dependency graph; this is its light Node twin.
//
// Two checks, both mechanical and offline (no npm registry calls — the
// allowlist below is asserted, not looked up):
//
//   1. Every package.json below has no "dependencies" field, or an empty one.
//      node/keel (the front end), node/keel-core-stub, node/keel-core-native,
//      and node/keel-cli must stay dependency-free — a runtime dependency
//      there breaks the zero-code-changes cost model and must be a
//      deliberate, reviewed decision, not a stray `npm install --save`.
//      (node/keel-cli's optionalDependencies are its own sibling platform
//      packages, not third-party — a separate mechanism, not checked here.)
//   2. Any "devDependencies" entry is checked against LICENSE_ALLOWLIST below.
//      None exist today (the adapter farm installs the real `ai` /
//      `@modelcontextprotocol/sdk` packages ad hoc into a throwaway
//      node_modules per .github/workflows/adapter-farm.yml, not as a
//      committed devDependency), but the check exists so a future
//      devDependency add cannot silently carry a copyleft/BSL license.
//
// Usage: node scripts/check-licenses.mjs
// Exit 0 all clear, 1 with one line per violation. No dependencies of its own.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const REPO = join(dirname(fileURLToPath(import.meta.url)), "..");

// package.json paths whose runtime "dependencies" must be empty/absent.
const ZERO_DEP_MANIFESTS = [
  "node/keel/package.json",
  "node/keel-core-stub/package.json",
  "node/keel-core-native/package.json",
  "node/keel-cli/package.json",
];

// devDependency name -> [license, reason]. Update when a devDependency is
// deliberately added; never delete an entry just to silence a failure.
const LICENSE_ALLOWLIST = {
  // (empty today — see module doc above)
};

const DISALLOWED_LICENSE_MARKERS = ["GPL", "AGPL", "BSL", "SSPL", "Commons Clause"];

function readJson(relPath) {
  return JSON.parse(readFileSync(join(REPO, relPath), "utf8"));
}

function checkZeroRuntimeDeps() {
  const errors = [];
  for (const rel of ZERO_DEP_MANIFESTS) {
    const pkg = readJson(rel);
    const deps = pkg.dependencies ?? {};
    if (Object.keys(deps).length > 0) {
      errors.push(
        `${rel}: "dependencies" must stay absent/empty (zero runtime deps invariant, ` +
          `engineering-manifesto rule 12); found ${JSON.stringify(deps)}. If this is ` +
          "deliberate, it needs a documented decision, not a silent add."
      );
    }
  }
  return errors;
}

function checkDevDependencyLicenses() {
  const errors = [];
  for (const rel of ZERO_DEP_MANIFESTS) {
    const pkg = readJson(rel);
    const devDeps = pkg.devDependencies ?? {};
    for (const name of Object.keys(devDeps)) {
      const entry = LICENSE_ALLOWLIST[name];
      if (!entry) {
        errors.push(
          `${rel}: devDependency "${name}" is not in scripts/check-licenses.mjs's ` +
            "LICENSE_ALLOWLIST. Add its [license, reason] before landing the dependency."
        );
        continue;
      }
      const [licenseId] = entry;
      if (DISALLOWED_LICENSE_MARKERS.some((m) => licenseId.includes(m))) {
        errors.push(
          `devDependency "${name}" is allowlisted with license "${licenseId}", which ` +
            "matches a disallowed (copyleft/BSL) marker — NFR6 violation."
        );
      }
    }
  }
  return errors;
}

function main() {
  const errors = [...checkZeroRuntimeDeps(), ...checkDevDependencyLicenses()];
  if (errors.length > 0) {
    console.error("check-licenses.mjs: FAILED");
    for (const e of errors) console.error(`  - ${e}`);
    return 1;
  }
  console.log(
    `check-licenses.mjs: OK (${ZERO_DEP_MANIFESTS.length} manifests zero-dep; ` +
      `${Object.keys(LICENSE_ALLOWLIST).length} devDependencies allowlisted)`
  );
  return 0;
}

process.exit(main());
