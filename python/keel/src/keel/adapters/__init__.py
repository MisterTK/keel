"""Library adapter packs and their installer.

A pack (``httpx_pack``, ``requests_pack``, …) is a uniform module implementing
the four adapter-pack contract operations (``detect``/``seams``/``targets``/
``defaults`` — see ``contracts/adapter-pack.md``) plus an ``install``/
``uninstall`` pair that physically patches its library's seams.

Installation is LAZY: a pack is patched only when its library is actually
imported by the program (a ``sys.meta_path`` finder wraps the library's loader
and runs ``install()`` right after the module executes), or immediately if the
library was already imported. A program that never imports httpx/requests pays
nothing — the runtime stays stdlib-only and ``keel run`` startup stays cheap,
honoring the contract rule that a pack never imports its library unless it is
present *and in use*.
"""

from __future__ import annotations

import importlib.abc
import sys
import types
from importlib.machinery import ModuleSpec
from typing import Any, Sequence

from . import aiohttp_pack, boto3_pack, httpx_pack, psycopg_pack, requests_pack, urllib3_pack
from ._pack import Detection, Seam, TargetDecl

#: Registration order = install/report order (stable, deterministic output).
#: Alphabetical by module name. Library adapters only (physically-patched,
#: seam-owning packs that live in this package). Framework packs (pydantic-ai
#: / openai-agents / crewai / …, `keel.packs`) own a seam the same way but are
#: registered via :func:`_framework_packs` — a lazily-imported cross-package
#: reference, so `keel.packs` (which already imports `keel.adapters._pack`)
#: never has to be imported at `keel.adapters` MODULE-import time (no
#: init-order cycle).
PACKS = (aiohttp_pack, boto3_pack, httpx_pack, psycopg_pack, requests_pack, urllib3_pack)


class _State:
    finder: "_AdapterFinder | None" = None


_STATE = _State()


def _framework_packs() -> tuple[Any, ...]:
    """The framework packs, imported lazily (function-call time, never module-
    import time) to avoid a cycle: `keel.packs` submodules already import
    `keel.adapters._pack`, so importing `keel.packs` at the top of THIS module
    would make the two packages import each other mid-initialization."""
    from ..packs import crewai_pack, openai_agents_pack, pydantic_ai_pack

    return (pydantic_ai_pack, openai_agents_pack, crewai_pack)


def _all_packs() -> tuple[Any, ...]:
    return (*PACKS, *_framework_packs())


def available_packs() -> list[Detection]:
    """Detections for every registered pack, present or not (never imports an
    absent library). Feeds `keel doctor` and the startup banner."""
    return [pack.detect() for pack in _all_packs()]


def install_adapters() -> list[Detection]:
    """Arm every present pack: patch already-imported libraries now, and
    register a finder so libraries imported later are patched on import.
    Returns the detections of the present packs (for the banner)."""
    index: dict[str, Any] = {}
    present: list[Detection] = []
    for pack in _all_packs():
        detection = pack.detect()
        if not detection.matched:
            continue
        present.append(detection)
        index[pack.MODULE] = pack
        if pack.MODULE in sys.modules:
            pack.install()  # already imported: patch immediately (retroactive)
    if index and _STATE.finder is None:
        finder = _AdapterFinder(index)
        sys.meta_path.insert(0, finder)
        _STATE.finder = finder
    return present


def uninstall_adapters() -> None:
    """Remove the finder and restore every patched library (test teardown /
    uninstall-clean)."""
    if _STATE.finder is not None:
        try:
            sys.meta_path.remove(_STATE.finder)
        except ValueError:
            pass
        _STATE.finder = None
    for pack in _all_packs():
        pack.uninstall()


class _AdapterFinder(importlib.abc.MetaPathFinder):
    """Intercepts only the top-level library modules a present pack targets,
    delegates discovery to the real finders, and wraps the loader so the pack
    is installed right after the library module executes."""

    def __init__(self, index: dict[str, Any]) -> None:
        self._index = index

    def find_spec(
        self,
        fullname: str,
        path: Sequence[str] | None = None,
        target: types.ModuleType | None = None,
    ) -> ModuleSpec | None:
        pack = self._index.get(fullname)
        if pack is None:
            return None  # not a target library: normal import, zero overhead
        spec = self._real_spec(fullname, path, target)
        if spec is None or spec.loader is None:
            return None
        inner = spec.loader
        if not hasattr(inner, "exec_module"):
            return None  # legacy/opaque loader: do nothing unsafe
        spec.loader = _AdapterLoader(inner, pack)
        return spec

    def _real_spec(
        self,
        fullname: str,
        path: Sequence[str] | None,
        target: types.ModuleType | None,
    ) -> ModuleSpec | None:
        for finder in sys.meta_path:
            if finder is self or isinstance(finder, _AdapterFinder):
                continue
            found = finder.find_spec(fullname, path, target)
            if found is not None:
                return found
        return None


class _AdapterLoader(importlib.abc.Loader):
    """Delegates to the real loader, then installs the pack. Restores the
    module's real loader so the wrapping leaves no trace on the module."""

    def __init__(self, inner: importlib.abc.Loader, pack: Any) -> None:
        self._inner = inner
        self._pack = pack

    def create_module(self, spec: ModuleSpec) -> types.ModuleType | None:
        create = getattr(self._inner, "create_module", None)
        return create(spec) if create is not None else None

    def exec_module(self, module: types.ModuleType) -> None:
        module.__loader__ = self._inner
        if module.__spec__ is not None:
            module.__spec__.loader = self._inner
        self._inner.exec_module(module)  # type: ignore[attr-defined]
        self._pack.install()


__all__ = [
    "PACKS",
    "Detection",
    "Seam",
    "TargetDecl",
    "available_packs",
    "install_adapters",
    "uninstall_adapters",
]
