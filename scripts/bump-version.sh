#!/usr/bin/env bash
# Bump the project version everywhere, atomically. The single source is
# `[workspace.package] version` in the root Cargo.toml; this script rewrites it
# plus every declaration that cannot inherit it, then re-verifies with
# scripts/check-versions.py. Usage: scripts/bump-version.sh X.Y.Z
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

new="${1:?usage: scripts/bump-version.sh X.Y.Z}"
if ! [[ "$new" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$ ]]; then
  echo "bump-version: '$new' is not a semver version (X.Y.Z[-pre])" >&2
  exit 1
fi

old="$(python3 -c '
import tomllib
with open("Cargo.toml", "rb") as f:
    print(tomllib.load(f)["workspace"]["package"]["version"])
')"

NEW="$new" OLD="$old" python3 - <<'EOF'
import os
import re
from pathlib import Path

new, old = os.environ["NEW"], os.environ["OLD"]

def sub_once(path: str, pattern: str, replacement: str) -> None:
    p = Path(path)
    text, n = re.subn(pattern, replacement, p.read_text(), count=1, flags=re.MULTILINE)
    if n != 1:
        raise SystemExit(f"bump-version: no version declaration found in {path}")
    p.write_text(text)
    print(f"  {path}: {old} -> {new}")

sub_once("Cargo.toml", r'^version = "[^"]+"$', f'version = "{new}"')
for path in ("python/keel/pyproject.toml", "python/keel-core-stub/pyproject.toml"):
    sub_once(path, r'^version = "[^"]+"$', f'version = "{new}"')
sub_once(
    "python/keel/pyproject.toml",
    r'"keelrun-core==[^"]+"',
    f'"keelrun-core=={new}"',
)
# Regex, not json round-trip: keeps each package.json's formatting untouched.
# (node/keel-core-native and node/keel-cli are handled separately below,
# together with their optionalDependencies pins, in one pass each.)
for path in (
    "node/keel/package.json",
    "node/keel-core-stub/package.json",
):
    sub_once(path, r'^  "version": "[^"]+",$', f'  "version": "{new}",')
# node/keel-core-native's own version PLUS its optionalDependencies pins (4
# platform packages) — a real bug found the first time this script ran twice
# (2026-07-14/15): a version-field-only bump left these 4 pins stale at the
# old version, which would resolve the wrong (or, once unpublished versions
# age out, no) platform package on install.
napi_pkg = Path("node/keel-core-native/package.json")
napi_text = napi_pkg.read_text()
napi_n = napi_text.count(f'"{old}"')
if napi_n != 5:
    raise SystemExit(
        f"bump-version: expected 5 occurrences of \"{old}\" in {napi_pkg} "
        f"(own version + 4 optionalDependencies pins), found {napi_n}"
    )
napi_pkg.write_text(napi_text.replace(f'"{old}"', f'"{new}"'))
print(f"  {napi_pkg}: {old} -> {new} (5 occurrences, incl. optionalDependencies)")
# Per-platform napi prebuild packages (node/keel-core-native/npm/*/package.json).
for path in sorted(str(p) for p in Path("node/keel-core-native/npm").glob("*/package.json")):
    sub_once(path, r'^  "version": "[^"]+",$', f'  "version": "{new}",')
# node/keel-cli's own version PLUS its 5 optionalDependencies pins (all six
# occurrences of the old version string in this one file are version refs —
# a plain literal replace, not a regex, since the description prose never
# contains a version number).
cli_pkg = Path("node/keel-cli/package.json")
cli_text = cli_pkg.read_text()
cli_n = cli_text.count(f'"{old}"')
if cli_n != 6:
    raise SystemExit(
        f"bump-version: expected 6 occurrences of \"{old}\" in {cli_pkg} "
        f"(own version + 5 optionalDependencies pins), found {cli_n}"
    )
cli_pkg.write_text(cli_text.replace(f'"{old}"', f'"{new}"'))
print(f"  {cli_pkg}: {old} -> {new} (6 occurrences)")
# Per-platform keelrun-cli npm packages (node/keel-cli/npm/*/package.json).
for path in sorted(str(p) for p in Path("node/keel-cli/npm").glob("*/package.json")):
    sub_once(path, r'^  "version": "[^"]+",$', f'  "version": "{new}",')
sub_once(
    "python/keel/src/keel/__init__.py",
    r'^__version__ = "[^"]+"$',
    f'__version__ = "{new}"',
)
sub_once(
    "node/keel/index.mjs",
    r'^export const VERSION = "[^"]+";$',
    f'export const VERSION = "{new}";',
)
# The checked-in draft formula's placeholder tag URL (sha256 stays a
# placeholder here; scripts/render-homebrew-formula.sh fills in the real
# digest for an actual tagged release).
sub_once(
    "packaging/homebrew/keel.rb",
    r'^  url "[^"]*/archive/refs/tags/v[^"]*\.tar\.gz"$',
    f'  url "https://github.com/MisterTK/keel/archive/refs/tags/v{new}.tar.gz"',
)
EOF

# Refresh Cargo.lock's records of the workspace crates at the new version.
cargo update --workspace --quiet

python3 scripts/check-versions.py
echo "bump-version: $old -> $new (remember to commit Cargo.lock)"
