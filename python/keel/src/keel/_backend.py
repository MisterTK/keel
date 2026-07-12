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
from typing import Any, Protocol, runtime_checkable

from ._errors import KeelError


@runtime_checkable
class Backend(Protocol):
    def configure(self, policy: dict[str, Any]) -> None: ...

    def execute(self, request: dict[str, Any], effect: Any) -> dict[str, Any]: ...

    def report(self) -> dict[str, Any]: ...


def _try_load_native() -> Backend | None:
    try:
        mod = importlib.import_module("keel_core")
    except ImportError:
        return None
    ctor = getattr(mod, "KeelCore", None) or getattr(mod, "Core", None)
    if ctor is None:
        return None
    inst = ctor()
    if callable(getattr(inst, "configure", None)) and callable(getattr(inst, "execute", None)):
        return inst  # type: ignore[return-value]
    return None


def load_backend(preferred: str | None = None) -> Backend:
    """Resolve the runtime backend per `KEEL_BACKEND` (or `preferred`)."""
    choice = (preferred if preferred is not None else os.environ.get("KEEL_BACKEND", "auto")) or "auto"
    if choice not in ("auto", "native", "stub"):
        raise KeelError("KEEL-E040", f"KEEL_BACKEND must be auto|native|stub, got {choice!r}")

    if choice != "stub":
        native = _try_load_native()
        if native is not None:
            return native
        if choice == "native":
            raise KeelError(
                "KEEL-E040",
                "KEEL_BACKEND=native requested but the keel_core native module is not loadable",
            )

    from keel_core_stub import KeelCoreStub

    return KeelCoreStub()
