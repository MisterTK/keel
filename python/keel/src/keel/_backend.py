"""Backend selection, isolated behind one seam (architecture §5.1, "the swap").

A backend exposes `configure(policy)`, `execute(request, effect)`, and
`report()` — the four-operation core surface the stub defines (advance_clock
is test-only and unused here). Selection:

  * native (`keel_core`) — the eventual PyO3 module (Task 6/14); probed by
    import and required to expose `configure`/`execute`. It may not exist
    yet, so a failed import is normal.
  * stub (`keel_core_stub.KeelCoreStub`) — the in-repo pure-Python core.

`KEEL_BACKEND` overrides selection:
  * `stub`   → force the stub, never probe native
  * `native` → require `keel_core`; raise KEEL-E040 if not loadable
  * `auto` / unset → native if loadable, else the stub
"""

from __future__ import annotations

import importlib
import os
from pathlib import Path
from typing import Any, Mapping, Protocol, runtime_checkable

from ._errors import KeelError


@runtime_checkable
class Backend(Protocol):
    def configure(self, policy: dict[str, Any]) -> None: ...

    def execute(self, request: dict[str, Any], effect: Any) -> dict[str, Any]: ...

    def report(self) -> dict[str, Any]: ...


def _journal_path(cwd: str | Path | None, env: Mapping[str, str]) -> str | None:
    """Where the native core attaches its journal (persistent dev cache + Tier 2).
    `KEEL_JOURNAL` overrides the path; an explicit empty value disables it."""
    override = env.get("KEEL_JOURNAL")
    if override is not None:
        return override or None  # empty string ⇒ no journal
    base = Path(cwd) if cwd is not None else Path.cwd()
    return str(base / ".keel" / "journal.db")


def _try_load_native(journal_path: str | None) -> Backend | None:
    try:
        mod = importlib.import_module("keel_core")
    except ImportError:
        return None
    ctor = getattr(mod, "KeelCore", None) or getattr(mod, "Core", None)
    if ctor is None:
        return None
    inst = None
    if journal_path:
        try:
            inst = ctor(journal_path=journal_path)  # attaches the persistent journal
        except Exception:
            inst = None  # journal open failed — fall back to an in-memory native core
    if inst is None:
        inst = ctor()
    if callable(getattr(inst, "configure", None)) and callable(getattr(inst, "execute", None)):
        # The native KeelCore already exposes configure/execute/execute_async/
        # report/persistent, so it IS the backend — no wrapper needed.
        return inst  # type: ignore[return-value]
    return None


def load_backend(
    preferred: str | None = None,
    *,
    cwd: str | Path | None = None,
    env: Mapping[str, str] | None = None,
) -> Backend:
    """Resolve the runtime backend per `KEEL_BACKEND` (or `preferred`). Under a
    native backend, a journal is attached at `<cwd>/.keel/journal.db` (created on
    demand) so the dev cache's `scope=persistent` replays across runs."""
    environ = env if env is not None else os.environ
    choice = (preferred if preferred is not None else environ.get("KEEL_BACKEND", "auto")) or "auto"
    if choice not in ("auto", "native", "stub"):
        raise KeelError("KEEL-E040", f"KEEL_BACKEND must be auto|native|stub, got {choice!r}")

    if choice != "stub":
        native = _try_load_native(_journal_path(cwd, environ))
        if native is not None:
            return native
        if choice == "native":
            raise KeelError(
                "KEEL-E040",
                "KEEL_BACKEND=native requested but the keel_core native module is not loadable",
            )

    from keel_core_stub import KeelCoreStub

    return KeelCoreStub()
