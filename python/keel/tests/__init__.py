"""Test support: make `keel` and `keel_core_stub` importable without an install
(the acceptance command is `python3 -m unittest discover python/keel`), and
provide helpers for the subprocess (child-process) tests.

Importing this package (which `unittest discover` does before importing any
test module) is what puts the src layout and the sibling stub on `sys.path`.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

_HERE = Path(__file__).resolve().parent
PKG_SRC = _HERE.parent / "src"  # python/keel/src
FIXTURES = _HERE / "fixtures"
REPO_ROOT = _HERE.parents[2]  # worktree root
STUB = REPO_ROOT / "python" / "keel-core-stub"
CONTRACTS = REPO_ROOT / "contracts"

for _p in (PKG_SRC, STUB, FIXTURES):
    s = str(_p)
    if s not in sys.path:
        sys.path.insert(0, s)


def child_env(**extra: str) -> dict[str, str]:
    """A clean environment for spawned `python -m keel run` children: Keel
    toggles removed, `PYTHONPATH` pointed at the package src + the stub so the
    child resolves `keel`/`keel_core_stub` without an install."""
    env = dict(os.environ)
    for k in ("KEEL_DISABLE", "KEEL_BACKEND", "KEEL_QUIET"):
        env.pop(k, None)
    parts = [str(PKG_SRC), str(STUB), str(FIXTURES)]
    if env.get("PYTHONPATH"):
        parts.append(env["PYTHONPATH"])
    env["PYTHONPATH"] = os.pathsep.join(parts)
    env.update(extra)
    return env
