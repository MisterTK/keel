#!/usr/bin/env python3
"""Assert every version declaration in the repo agrees with the single source.

The source of truth is `[workspace.package] version` in the root Cargo.toml
(all crates inherit it; the keelrun-core wheel reads it via maturin's
`dynamic = ["version"]`). Everything that cannot inherit it mechanically is
listed here and checked byte-for-byte:

  - python/keel/pyproject.toml            [project] version
  - python/keel-core-stub/pyproject.toml  [project] version
  - node/keel/package.json                version
  - node/keel-core-stub/package.json      version
  - node/keel-core-native/package.json    version
  - python/keel/src/keel/__init__.py      __version__
  - node/keel/index.mjs                   VERSION
  - crates/keel-py/pyproject.toml         must stay dynamic (no restated version)
  - crates/keel-cli/pyproject.toml        must stay dynamic (no restated version)
  - node/keel-core-native/npm/*/package.json  version (per-platform prebuilds)
  - python/keel/pyproject.toml            keelrun-core==<version> dependency pin

Usage: check-versions.py [--tag vX.Y.Z]
`--tag` additionally asserts a release tag matches (for the release workflow:
tag `vX.Y.Z` must equal every manifest). Exit 0 all-agree, 1 with one line per
mismatch otherwise. Stdlib-only; deterministic output.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import tomllib
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent


def workspace_version() -> str:
    with (REPO / "Cargo.toml").open("rb") as f:
        return tomllib.load(f)["workspace"]["package"]["version"]


def declarations() -> list[tuple[str, str]]:
    """(location, declared version) for every non-inheriting declaration."""
    found: list[tuple[str, str]] = []

    for rel in ("python/keel/pyproject.toml", "python/keel-core-stub/pyproject.toml"):
        with (REPO / rel).open("rb") as f:
            found.append((f"{rel} [project] version", tomllib.load(f)["project"]["version"]))

    for rel in (
        "node/keel/package.json",
        "node/keel-core-stub/package.json",
        "node/keel-core-native/package.json",
    ):
        found.append((f"{rel} version", json.loads((REPO / rel).read_text())["version"]))

    # Per-platform napi prebuild packages (scripts/napi-prebuild.sh writes the
    # binaries; the manifests are checked in — see node/keel-core-native/npm/).
    for pkg in sorted((REPO / "node/keel-core-native/npm").glob("*/package.json")):
        rel = pkg.relative_to(REPO).as_posix()
        found.append((f"{rel} version", json.loads(pkg.read_text())["version"]))

    init = (REPO / "python/keel/src/keel/__init__.py").read_text()
    m = re.search(r'^__version__ = "([^"]+)"$', init, re.MULTILINE)
    found.append(
        ("python/keel/src/keel/__init__.py __version__", m.group(1) if m else "<missing>")
    )

    index = (REPO / "node/keel/index.mjs").read_text()
    m = re.search(r'^export const VERSION = "([^"]+)";$', index, re.MULTILINE)
    found.append(("node/keel/index.mjs VERSION", m.group(1) if m else "<missing>"))

    return found


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", help="release tag that must equal the version (vX.Y.Z)")
    args = parser.parse_args()

    want = workspace_version()
    errors: list[str] = []

    for location, got in declarations():
        if got != want:
            errors.append(f"{location} is {got}, workspace Cargo.toml says {want}")

    # keel-py / keel-cli must not restate the version: maturin derives both
    # from their crate (workspace version), matching every other crate.
    for rel in ("crates/keel-py/pyproject.toml", "crates/keel-cli/pyproject.toml"):
        with (REPO / rel).open("rb") as f:
            project = tomllib.load(f)["project"]
        if "version" in project or "version" not in project.get("dynamic", []):
            errors.append(
                f'{rel} must declare dynamic = ["version"] and no [project] '
                "version (maturin reads the crate version)"
            )

    # The front-end wheel pins the native core wheel to the same version.
    with (REPO / "python/keel/pyproject.toml").open("rb") as f:
        deps = tomllib.load(f)["project"].get("dependencies", [])
    pin = f"keelrun-core=={want}"
    if pin not in deps:
        errors.append(f"python/keel/pyproject.toml dependencies must pin {pin!r} (got {deps!r})")

    if args.tag is not None and args.tag != f"v{want}":
        errors.append(f"tag {args.tag} does not match workspace version {want} (expected v{want})")

    if errors:
        print(f"check-versions: FAIL against workspace version {want}")
        for line in errors:
            print(f"  - {line}")
        print("  fix: run scripts/bump-version.sh or align the manifest by hand")
        return 1

    print(f"check-versions: OK — all declarations agree on {want}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
