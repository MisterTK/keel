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


class _NativeBackend:
    """Front-end adapter over the native ``keel_core`` core, adding one thing the
    PyO3 surface does not expose: ``layer(target, key)``, resolved from the
    configured policy exactly as the stub's ``_layer`` / Node's
    ``NativeBackend.layer`` do. The adapter packs read ``backend.layer`` to honor
    ``idempotency.header`` and gate cache-body buffering; without this the knob is
    dead under native (a Python/Node parity break). Every other attribute
    (``execute``/``execute_async``/``report``/``persistent``/the flow surface)
    delegates straight through, so the swap is transparent."""

    def __init__(self, core: Any) -> None:
        self._core = core
        self._policy: dict[str, Any] = {}

    def configure(self, policy: dict[str, Any]) -> None:
        self._policy = policy if isinstance(policy, dict) else {}
        self._core.configure(policy)

    def layer(self, target: str, key: str) -> Any:
        t = self._policy.get("target")
        if isinstance(t, dict) and isinstance(t.get(target), dict) and key in t[target]:
            return t[target][key]
        defaults = self._policy.get("defaults")
        if not isinstance(defaults, dict):
            return None
        if target.startswith("llm:"):
            llm = defaults.get("llm")
            if isinstance(llm, dict) and key in llm:
                return llm[key]
        outbound = defaults.get("outbound")
        return outbound.get(key) if isinstance(outbound, dict) else None

    def __getattr__(self, name: str) -> Any:
        # Delegate everything else to the native core (execute, execute_async,
        # report, persistent, enter_flow/exit_flow/journal_*, advance_clock, …).
        return getattr(self._core, name)


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
        # The native KeelCore exposes configure/execute/execute_async/report/
        # persistent; wrap it only to add `layer(target, key)` (idempotency.header
        # + cache-ttl gate parity), delegating everything else.
        return _NativeBackend(inst)  # type: ignore[return-value]
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
        # A user env-var mistake, not a Keel bug — E001 (config-level), not E040.
        raise KeelError(
            "KEEL-E001",
            f"KEEL_BACKEND must be one of auto, native, or stub (got {choice!r}); "
            "unset it or correct the value",
        )

    if choice != "stub":
        native = _try_load_native(_journal_path(cwd, environ))
        if native is not None:
            return native
        if choice == "native":
            # A missing build/install is a user-environment problem, not a Keel
            # bug — E001 with a concrete next step, never E040 ("file an issue").
            raise KeelError(
                "KEEL-E001",
                "KEEL_BACKEND=native requires the keel_core native module, which is not "
                "installed; build it (`maturin develop -m crates/keel-py/Cargo.toml`) or "
                "unset KEEL_BACKEND to use the pure-Python stub",
            )

    from keel_core_stub import KeelCoreStub

    return KeelCoreStub()
