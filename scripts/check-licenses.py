#!/usr/bin/env python3
"""License audit for the Python side of the repo (NFR6 / engineering-manifesto
rule 12: front ends carry zero runtime dependencies; anything test-only stays
on a permissive license so the maintenance tax stays honest).

cargo-deny (deny.toml) covers the Rust dependency graph; this is its light
Python twin. Two checks, both mechanical and offline (no PyPI calls — the
allowlist below is asserted, not looked up):

  1. Every front end's `[project].dependencies` in pyproject.toml is `[]`.
     Python's stdlib-only front end (python/keel) and the stub package
     (python/keel-core-stub) must stay this way; a future pack that adds a
     runtime dependency there breaks the zero-code-changes promise's cost
     model and must be a deliberate, reviewed decision — not a stray `pip
     install` a maintainer forgets to undo.
  2. Every dev/test-only dependency (the adapter contract-test farm: httpx,
     requests, ...) is in the LICENSE_ALLOWLIST below with a permissive
     license. Adding a new dev dependency without adding it here fails the
     check — the failure message says exactly what to add.

Usage: check-licenses.py
Exit 0 all clear, 1 with one line per violation. Stdlib-only; deterministic.
"""

from __future__ import annotations

import re
import sys
import tomllib
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

# Front ends / stub packages whose runtime dependency list must be empty.
ZERO_DEP_MANIFESTS = (
    "python/keel/pyproject.toml",
    "python/keel-core-stub/pyproject.toml",
)

# Package (PEP 508 name, lowercased) -> (license SPDX-ish id, why it's here).
# Scope: only `python/keel/pyproject.toml`'s `[project.optional-dependencies]`
# groups (checked below) — these are the packages a contributor installs with
# `pip install -e '.[dev]'`. They are all TEST-ONLY (the adapter contract-test
# farm — see .github/workflows/adapter-farm.yml); none ship in a wheel's
# runtime deps. (Other ad hoc CI-only installs like `jsonschema` for
# conformance/check_schema.py or `maturin` for the wheel build are declared
# directly in .github/workflows/ci.yml, not here, since they are not part of
# any pyproject.toml.) Update this table (never delete a check to make it
# pass) when a pack's `_PINNED` range in python/keel/src/keel/adapters/*.py
# adds a new library.
LICENSE_ALLOWLIST: dict[str, tuple[str, str]] = {
    "httpx": ("BSD-3-Clause", "httpx adapter pack contract tests"),
    "requests": ("Apache-2.0", "requests adapter pack contract tests"),
}

# A dependency is disallowed if its license family is copyleft/BSL, even if a
# future contributor's own note claims otherwise — this list exists so a
# reviewer sees the reasoning right next to the rule, not just the assertion.
DISALLOWED_LICENSE_MARKERS = ("GPL", "AGPL", "BSL", "SSPL", "Commons Clause")

_NAME_RE = re.compile(r"^\s*([A-Za-z0-9_.\-]+)")


def _dep_name(requirement: str) -> str:
    """The bare distribution name from a PEP 508 requirement string, e.g.
    'httpx>=0.27,<0.29' -> 'httpx'."""
    m = _NAME_RE.match(requirement)
    if not m:
        raise ValueError(f"unparsable requirement: {requirement!r}")
    return m.group(1).lower()


def check_zero_runtime_deps() -> list[str]:
    errors = []
    for rel in ZERO_DEP_MANIFESTS:
        path = REPO / rel
        with path.open("rb") as f:
            data = tomllib.load(f)
        deps = data.get("project", {}).get("dependencies", [])
        if deps:
            errors.append(
                f"{rel}: [project].dependencies must stay [] (zero runtime deps "
                f"invariant, engineering-manifesto rule 12); found {deps!r}. If this "
                "is deliberate, it needs a documented decision, not a silent add."
            )
    return errors


def check_dev_dependency_licenses() -> list[str]:
    errors = []
    path = REPO / "python/keel/pyproject.toml"
    with path.open("rb") as f:
        data = tomllib.load(f)
    optional = data.get("project", {}).get("optional-dependencies", {})
    for group, reqs in optional.items():
        for req in reqs:
            name = _dep_name(req)
            entry = LICENSE_ALLOWLIST.get(name)
            if entry is None:
                errors.append(
                    f"python/keel/pyproject.toml [project.optional-dependencies.{group}]: "
                    f"'{req}' is not in scripts/check-licenses.py's LICENSE_ALLOWLIST. "
                    "Add its (license, reason) before landing the dependency."
                )
                continue
            license_id, _reason = entry
            if any(marker in license_id for marker in DISALLOWED_LICENSE_MARKERS):
                errors.append(
                    f"'{name}' is allowlisted with license {license_id!r}, which "
                    "matches a disallowed (copyleft/BSL) marker — NFR6 violation."
                )
    return errors


def main() -> int:
    errors = [*check_zero_runtime_deps(), *check_dev_dependency_licenses()]
    if errors:
        print("check-licenses.py: FAILED", file=sys.stderr)
        for e in errors:
            print(f"  - {e}", file=sys.stderr)
        return 1
    print(
        f"check-licenses.py: OK ({len(ZERO_DEP_MANIFESTS)} manifests zero-dep; "
        f"{len(LICENSE_ALLOWLIST)} dev dependencies allowlisted)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
