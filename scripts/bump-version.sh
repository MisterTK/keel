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
for path in (
    "node/keel/package.json",
    "node/keel-core-stub/package.json",
    "node/keel-core-native/package.json",
):
    sub_once(path, r'^  "version": "[^"]+",$', f'  "version": "{new}",')
# Per-platform napi prebuild packages (node/keel-core-native/npm/*/package.json).
for path in sorted(str(p) for p in Path("node/keel-core-native/npm").glob("*/package.json")):
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
