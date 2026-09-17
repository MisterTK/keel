"""Deployment-shape checks the RUNTIME can make that doctor cannot (WS4,
issue #90): a durable-flow journal on storage that will not outlive the
instance. One stderr line, once per process, via `_log.emit`."""

from __future__ import annotations

import os
from pathlib import Path
from typing import Any, Mapping

from . import __version__
from .packs import serverless_marker

DOCKERENV = Path("/.dockerenv")


def flows_configured(policy: Mapping[str, Any]) -> bool:
    flows = policy.get("flows")
    if not isinstance(flows, dict):
        return False
    entrypoints = flows.get("entrypoints")
    match = flows.get("match")
    return bool(isinstance(entrypoints, list) and entrypoints) or bool(isinstance(match, dict) and match)


def sqlite_journal_path(policy: Mapping[str, Any], cwd: Path) -> Path | None:
    """The journal's on-disk path when it is SQLite, else None (Postgres)."""
    loc = policy.get("journal")
    if loc is None:
        return cwd / ".keel" / "journal.db"
    if isinstance(loc, str) and loc.startswith("file:"):
        # Lexical-only (parity with Node's `path.resolve`, which never touches
        # the filesystem): `Path.resolve()` would also resolve symlinks, and
        # on macOS `/tmp`/`/var` themselves are symlinks, so that diverges
        # from Node's purely-lexical `resolve()` for the exact same input.
        # This string is for a human-facing warning + a JSON field, not to
        # open a file — canonicalizing symlinks buys nothing and costs
        # cross-language identity.
        return Path(os.path.normpath(os.path.join(str(cwd), loc[len("file:") :])))
    return None


def ephemeral_storage_marker(env: Mapping[str, str], cwd: Path, *, dockerenv: Path = DOCKERENV) -> str | None:
    marker = serverless_marker(env)
    if marker is not None:
        return marker
    if dockerenv.exists():
        return "/.dockerenv"
    if not os.access(cwd, os.W_OK):
        return "read-only cwd"
    return None


def ephemeral_journal_warning(
    policy: Mapping[str, Any], env: Mapping[str, str], cwd: Path, *, dockerenv: Path = DOCKERENV
) -> tuple[str, dict[str, Any]] | None:
    if not flows_configured(policy):
        return None
    journal = sqlite_journal_path(policy, cwd)
    if journal is None:
        return None
    marker = ephemeral_storage_marker(env, cwd, dockerenv=dockerenv)
    if marker is None:
        return None
    text = (
        f"keel ▸ warning: durable flows are configured but the journal is SQLite at {journal} "
        f"on ephemeral storage ({marker}) — flow state will not survive an instance replacement; "
        "mount a volume for .keel/ or use a Postgres journal\n"
    )
    obj = {
        "keel": "warning",
        "code": "journal-ephemeral-storage",
        "journal": str(journal),
        "marker": marker,
        "severity": "WARNING",
        "version": __version__,
    }
    return text, obj
