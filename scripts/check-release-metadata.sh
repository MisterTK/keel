#!/usr/bin/env bash
# Release-metadata gate: every package artifact must be buildable and
# installable-from-artifact. Default mode runs the cheap checks (seconds, no
# compiles beyond `cargo package --list`); `--full` additionally builds and
# installs the real artifacts (npm tarball, keelrun wheel, keelrun-core
# sdist) into throwaway prefixes — minutes, compiles the workspace.
#
#   scripts/check-release-metadata.sh          # cheap: versions, vendored
#                                              # parity, cargo package metadata
#   scripts/check-release-metadata.sh --full   # + npm pack/install smoke,
#                                              # wheel + sdist build/install
#
# The release workflow should run `--full`; CI runs the cheap mode.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

full=0
if [[ "${1:-}" == "--full" ]]; then
  full=1
elif [[ $# -gt 0 ]]; then
  echo "usage: scripts/check-release-metadata.sh [--full]" >&2
  exit 2
fi

fail() {
  echo "check-release-metadata: FAIL — $*" >&2
  exit 1
}

# --- 1. one version everywhere -----------------------------------------------
python3 scripts/check-versions.py

# --- 2. vendored copies are byte-identical to their single source ------------
vendored=(
  "contracts/core_api.rs crates/keel-core-api/contract/core_api.rs"
  "contracts/journal.sql crates/keel-journal/contract/journal.sql"
  "contracts/error-codes.json crates/keel-cli/contract/error-codes.json"
  "contracts/defaults.toml crates/keel-cli/contract/defaults.toml"
  "python/keel-core-stub/keel_core_stub/__init__.py python/keel/src/keel_core_stub/__init__.py"
  "node/keel-core-stub/index.mjs node/keel/src/vendor/keel-core-stub/index.mjs"
)
for pair in "${vendored[@]}"; do
  # shellcheck disable=SC2086 # intentional word split: "source vendored-copy"
  set -- $pair
  cmp -s "$1" "$2" || fail "vendored copy $2 drifted from $1; run scripts/sync-vendored.sh"
done
echo "check-release-metadata: vendored copies OK (${#vendored[@]} files)"

# --- 3. publishable crates package self-contained ----------------------------
# --allow-dirty: this is a metadata check, not a release; the release workflow
# packages from a clean tag. Each crate's package must carry its vendored
# contract files (that is what makes the .crate compile without the repo).
check_crate() {
  local crate="$1"
  shift
  local listing
  listing="$(cargo package --list --allow-dirty -p "$crate")"
  local f
  for f in "$@"; do
    grep -qx "$f" <<<"$listing" || fail "cargo package -p $crate does not include $f"
  done
}
check_crate keel-core-api contract/core_api.rs build.rs README.md
check_crate keel-journal contract/journal.sql build.rs README.md
check_crate keelrun-core src/engine.rs README.md
check_crate keelrun-cli contract/error-codes.json contract/defaults.toml build.rs README.md
check_crate keel-macros README.md
check_crate keelrun README.md
# keelrun-cli's integration tests embed repo-level fixtures; they must stay out.
if cargo package --list --allow-dirty -p keelrun-cli | grep -q '^tests/'; then
  fail "keelrun-cli package must exclude tests/ (they embed repo-level fixtures)"
fi
echo "check-release-metadata: cargo package listings OK (6 crates)"

# --- 4. manifest invariants ---------------------------------------------------
# The npm package must ship the vendored stub (src/ is in files[]).
node -e '
const pkg = require("./node/keel/package.json");
if (!pkg.files.includes("src/")) {
  console.error("node/keel package.json files[] must include src/ (vendored stub)");
  process.exit(1);
}
' || fail "node/keel package.json invariant"
echo "check-release-metadata: manifest invariants OK"

if [[ "$full" -ne 1 ]]; then
  echo "check-release-metadata: OK (cheap checks; pass --full for artifact smoke tests)"
  exit 0
fi

# ==============================================================================
# --full: build the real artifacts and install each into a throwaway prefix.
# ==============================================================================
workdir="$(mktemp -d "${TMPDIR:-/tmp}/keel-release-metadata.XXXXXX")"
trap 'rm -rf "$workdir"' EXIT

# --- 5. npm tarball: pack, install into a fresh project, import, wrap a call --
echo "check-release-metadata: [full] npm tarball smoke..."
tarball="$(cd node/keel && npm pack --pack-destination "$workdir" --silent)"
mkdir -p "$workdir/npm-proj"
(
  cd "$workdir/npm-proj"
  npm init -y >/dev/null 2>&1
  npm install --silent --no-audit --no-fund "$workdir/$tarball"
  KEEL_BACKEND=stub node --input-type=module -e '
    import { loadBackend, KeelError, VERSION } from "keelrun";
    const backend = await loadBackend();
    backend.configure({});
    const outcome = await backend.execute(
      { v: 1, target: "smoke.example", op: "GET smoke.example/x", idempotent: true },
      async () => ({ status: "ok", payload: { n: 1 } }),
    );
    if (outcome.result !== "ok") throw new Error(`stub execute failed: ${JSON.stringify(outcome)}`);
    if (typeof KeelError !== "function" || !VERSION) throw new Error("exports broken");
    console.log(`npm tarball import OK (keelrun ${VERSION}, backend ${backend.kind})`);
  '
)

# --- 6. keelrun wheel: build, install (no deps: keelrun-core is unpublished), import
echo "check-release-metadata: [full] python wheel smoke..."
python3 -m venv "$workdir/venv-wheel"
"$workdir/venv-wheel/bin/pip" wheel --quiet --disable-pip-version-check \
  --no-deps -w "$workdir/dist" python/keel
# --no-deps: the keelrun-core wheel is not on an index yet; the vendored stub
# must carry the import regardless (that is the point of this check).
"$workdir/venv-wheel/bin/pip" install --quiet --disable-pip-version-check \
  --no-deps "$workdir"/dist/keelrun-*.whl
(
  cd "$workdir"
  KEEL_BACKEND=stub "$workdir/venv-wheel/bin/python" - <<'EOF'
import importlib.metadata

import keel
from keel._backend import load_backend

backend = load_backend("stub")
backend.configure({})
outcome = backend.execute(
    {"v": 1, "target": "smoke.example", "op": "GET smoke.example/x", "idempotent": True},
    lambda attempt: {"status": "ok", "payload": {"n": 1}},
)
assert outcome["result"] == "ok", outcome
requires = importlib.metadata.requires("keelrun") or []
assert any(r.startswith("keelrun-core==") for r in requires), requires
print(f"python wheel import OK (keelrun {keel.__version__}, backend stub)")
EOF
)

# --- 7. keelrun-core sdist: build with maturin, compile-install from the sdist
# Proves the sdist is self-contained: the vendored contract copies (not the
# repo's contracts/) are what the include!/include_str! macros compile.
echo "check-release-metadata: [full] keelrun-core sdist smoke (compiles the core)..."
python3 -m venv "$workdir/venv-sdist"
"$workdir/venv-sdist/bin/pip" install --quiet --disable-pip-version-check "maturin>=1.7,<2"
"$workdir/venv-sdist/bin/maturin" sdist -m crates/keel-py/Cargo.toml -o "$workdir/sdist"
# --no-build-isolation: maturin is already in the venv; avoids a network fetch.
# The maturin build backend shells out to the `maturin` binary — put the venv's
# bin dir on PATH so the subprocess finds it.
PATH="$workdir/venv-sdist/bin:$PATH" \
  "$workdir/venv-sdist/bin/pip" install --quiet --disable-pip-version-check \
  --no-build-isolation "$workdir"/sdist/keelrun_core-*.tar.gz
"$workdir/venv-sdist/bin/python" - <<'EOF'
import keel_core

core = keel_core.KeelCore()
core.configure({})
print("keelrun-core sdist compile+import OK")
EOF

echo "check-release-metadata: OK (full)"
