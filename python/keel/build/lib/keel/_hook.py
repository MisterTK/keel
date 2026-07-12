"""The `sys.meta_path` import hook that wraps `py:` function targets.

At import time, for each module named by a `py:` target, the hook lets the
real import machinery build and execute the module, then replaces the
matching module-level functions with wrappers (`_wrap.wrap_function`). User
source is never touched; the wrapper carries the original's metadata via
`functools.wraps`, so the module looks unchanged to callers and introspection.

Modules already imported before the hook installs are wrapped retroactively.
`remove_import_hook` restores the finder-free state (uninstall-clean); it does
not un-wrap already-wrapped modules, but with the runtime cleared those
wrappers become transparent passthroughs.
"""

from __future__ import annotations

import fnmatch
import importlib.abc
import sys
import types
from importlib.machinery import ModuleSpec
from typing import Any, Iterable, Sequence

from ._policy import FunctionTarget
from ._wrap import is_wrapped, wrap_function


def _build_index(targets: Iterable[FunctionTarget]) -> dict[str, list[tuple[str, str]]]:
    """module name → [(policy key, function-name glob), ...] in declaration
    order, so first-match-wins is stable."""
    index: dict[str, list[tuple[str, str]]] = {}
    for t in targets:
        index.setdefault(t.module, []).append((t.key, t.func_glob))
    return index


def _wrappable(module: types.ModuleType, name: str, obj: Any) -> bool:
    if not isinstance(obj, types.FunctionType):
        return False  # only plain module-level `def`s (not classes/builtins/C)
    if is_wrapped(obj):
        return False
    if name.startswith("__") and name.endswith("__"):
        return False  # never wrap dunders
    # Only functions actually defined in this module — not re-exported aliases.
    return getattr(obj, "__module__", None) == module.__name__


def wrap_module(
    module: types.ModuleType, entries: Sequence[tuple[str, str]]
) -> list[str]:
    """Wrap matching module-level functions in place. Returns the concrete
    `op` ids wrapped (for the startup banner). First matching entry wins."""
    wrapped: list[str] = []
    mod_name = getattr(module, "__name__", None)
    if not mod_name:
        return wrapped
    for name, obj in list(vars(module).items()):
        if not _wrappable(module, name, obj):
            continue
        for key, func_glob in entries:
            if fnmatch.fnmatchcase(name, func_glob):
                op = f"py:{mod_name}.{name}"
                setattr(module, name, wrap_function(key, op, obj))
                wrapped.append(op)
                break
    return wrapped


class KeelFinder(importlib.abc.MetaPathFinder):
    """A meta-path finder that intercepts only the modules named by `py:`
    targets, delegates discovery to the real finders, and wraps the loader so
    matching functions are wrapped right after the module executes."""

    def __init__(self, index: dict[str, list[tuple[str, str]]]) -> None:
        self._index = index

    def find_spec(
        self,
        fullname: str,
        path: Sequence[str] | None = None,
        target: types.ModuleType | None = None,
    ) -> ModuleSpec | None:
        entries = self._index.get(fullname)
        if entries is None:
            return None  # not a target module: normal import, zero overhead
        spec = self._real_spec(fullname, path, target)
        if spec is None or spec.loader is None:
            return None
        inner = spec.loader
        if not hasattr(inner, "exec_module"):
            return None  # legacy/opaque loader: do nothing safe (leave as-is)
        spec.loader = _WrappingLoader(inner, entries)
        return spec

    def _real_spec(
        self,
        fullname: str,
        path: Sequence[str] | None,
        target: types.ModuleType | None,
    ) -> ModuleSpec | None:
        for finder in sys.meta_path:
            if finder is self or isinstance(finder, KeelFinder):
                continue
            found = finder.find_spec(fullname, path, target)
            if found is not None:
                return found
        return None


class _WrappingLoader(importlib.abc.Loader):
    """Delegates to the real loader, then wraps matching functions. Restores
    the module's `__loader__`/`__spec__.loader` to the real loader so the
    wrapping leaves no trace on the module object (transparency)."""

    def __init__(
        self, inner: importlib.abc.Loader, entries: Sequence[tuple[str, str]]
    ) -> None:
        self._inner = inner
        self._entries = entries

    def create_module(self, spec: ModuleSpec) -> types.ModuleType | None:
        create = getattr(self._inner, "create_module", None)
        return create(spec) if create is not None else None

    def exec_module(self, module: types.ModuleType) -> None:
        # Present the real loader to the module during execution, so a loader
        # that inspects module.__loader__ sees itself, not our wrapper.
        module.__loader__ = self._inner
        if module.__spec__ is not None:
            module.__spec__.loader = self._inner
        self._inner.exec_module(module)  # type: ignore[attr-defined]
        wrap_module(module, self._entries)


def install_import_hook(targets: Iterable[FunctionTarget]) -> KeelFinder | None:
    """Install the finder at the front of `sys.meta_path` and retroactively
    wrap already-imported target modules. Returns the finder (for removal), or
    None when there are no `py:` targets to wrap."""
    index = _build_index(targets)
    if not index:
        return None
    finder = KeelFinder(index)
    sys.meta_path.insert(0, finder)
    for mod_name, entries in index.items():
        existing = sys.modules.get(mod_name)
        if isinstance(existing, types.ModuleType):
            wrap_module(existing, entries)
    return finder


def remove_import_hook(finder: KeelFinder | None) -> None:
    if finder is not None:
        try:
            sys.meta_path.remove(finder)
        except ValueError:
            pass
