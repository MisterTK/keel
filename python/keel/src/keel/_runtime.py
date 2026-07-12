"""Process-global runtime state shared between the import hook's wrappers and
the bootstrap. Mirrors the Node front end's `runtime.mjs`: the generated
wrappers run wherever the user's code runs, so they reach the configured
backend + discovery store through this module rather than a captured closure.

When Keel is disabled (or never installed) `get_backend()` is None and every
wrapper falls through to the original function unchanged.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from ._backend import Backend
    from ._discovery import Discovery

_backend: "Backend | None" = None
_discovery: "Discovery | None" = None


def set_runtime(backend: "Backend | None", discovery: "Discovery | None") -> None:
    global _backend, _discovery
    _backend = backend
    _discovery = discovery


def clear_runtime() -> None:
    """Reset to the disabled state (used by `uninstall_keel` and tests)."""
    global _backend, _discovery
    _backend = None
    _discovery = None


def get_backend() -> "Backend | None":
    return _backend


def get_discovery() -> "Discovery | None":
    return _discovery
