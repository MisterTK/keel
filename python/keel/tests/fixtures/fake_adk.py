"""A structural fake of the ``google.adk`` shapes ``keel.packs.adk_pack``
depends on — signatures verified against the real ``google-adk`` package
(2.4.0) in a throwaway venv (never a repo dependency; adapter-pack rule 1).

Mirrors ``node/keel/test/fixtures/ai-sdk-model.d.ts``'s role for the AI SDK
pack: a pinned shape reference so pack tests run fully offline, with no
framework dependency added to any manifest. Verified against the real
package:

* ``BasePlugin.__init__(self, name: str)``; every callback is
  ``async def foo(self, *, ...) -> Optional[...]``, keyword-only.
* ``PluginManager.register_plugin(plugin)`` raises ``ValueError`` on a
  ``plugin.name`` clash; ``get_plugin(name)`` returns the match or ``None``.
* ``Runner.__init__(self, *, app=None, app_name=None, agent=None, node=None,
  plugins=None, ...)`` resolves onto ``self.plugin_manager = PluginManager(
  plugins=<resolved plugins>, ...)`` — ``InMemoryRunner.__init__(self,
  agent=None, *, app_name=None, plugins=None, app=None, ...)`` forwards to it
  via ``super().__init__(...)``.
* ``BaseTool.run_async(self, *, args: dict, tool_context) -> Any``.
* ``Runner.run_async(self, *, user_id: str, session_id: str, invocation_id:
  Optional[str] = None, new_message=None, ...) -> AsyncGenerator[Event,
  None]``; ``Runner.run(...)`` is the synchronous bridge, draining
  ``run_async`` over its own event loop.
"""

from __future__ import annotations

import sys
import types
from importlib.machinery import ModuleSpec
from typing import Any, Callable


class FakeBasePlugin:
    """Structural twin of ``google.adk.plugins.base_plugin.BasePlugin``: every
    callback defaults to a no-op returning ``None`` (ADK's own base class
    behavior — "the base class provides default `pass` implementations")."""

    def __init__(self, name: str) -> None:
        self.name = name

    async def on_user_message_callback(self, **_: Any) -> Any:
        return None

    async def before_run_callback(self, **_: Any) -> Any:
        return None

    async def after_run_callback(self, **_: Any) -> None:
        return None

    async def on_event_callback(self, **_: Any) -> Any:
        return None

    async def before_agent_callback(self, **_: Any) -> Any:
        return None

    async def after_agent_callback(self, **_: Any) -> Any:
        return None

    async def before_tool_callback(
        self, *, tool: Any, tool_args: dict[str, Any], tool_context: Any
    ) -> dict[str, Any] | None:
        return None

    async def after_tool_callback(
        self, *, tool: Any, tool_args: dict[str, Any], tool_context: Any, result: Any
    ) -> dict[str, Any] | None:
        return None

    async def before_model_callback(self, *, callback_context: Any, llm_request: Any) -> Any:
        return None

    async def after_model_callback(self, *, callback_context: Any, llm_response: Any) -> Any:
        return None

    async def on_model_error_callback(
        self, *, callback_context: Any, llm_request: Any, error: Exception
    ) -> Any:
        return None

    async def on_tool_error_callback(
        self, *, tool: Any, tool_args: dict[str, Any], tool_context: Any, error: Exception
    ) -> dict[str, Any] | None:
        return None


class FakePluginManager:
    """Structural twin of ``google.adk.plugins.plugin_manager.PluginManager``:
    only the two methods ``adk_pack`` actually calls."""

    def __init__(self, plugins: list[Any] | None = None) -> None:
        self.plugins: list[Any] = []
        for plugin in plugins or []:
            self.register_plugin(plugin)

    def register_plugin(self, plugin: Any) -> None:
        if any(p.name == plugin.name for p in self.plugins):
            raise ValueError(f"Plugin with name '{plugin.name}' already registered.")
        self.plugins.append(plugin)

    def get_plugin(self, name: str) -> Any | None:
        return next((p for p in self.plugins if p.name == name), None)


class FakeEvent:
    """Structural twin of ``google.adk.events.event.Event``: adk_pack's
    Runner-flow wrap reads only ``.invocation_id`` off an event (to
    correlate the flow via ``backend.journal_random``), so that is the only
    attribute this fake sets by default — any other keyword becomes an
    attribute too, for tests that want to assert byte-transparency of
    richer event shapes."""

    def __init__(self, invocation_id: str | None = None, **extra: Any) -> None:
        self.invocation_id = invocation_id
        for key, value in extra.items():
            setattr(self, key, value)

    def __eq__(self, other: object) -> bool:
        return isinstance(other, FakeEvent) and vars(self) == vars(other)

    def __repr__(self) -> str:
        return f"FakeEvent({vars(self)!r})"


class FakeRunner:
    """Structural twin of ``google.adk.runners.Runner``: real ADK resolves
    ``agent=``/``node=``/``app=`` into one ``App`` before building
    ``self.plugin_manager = PluginManager(plugins=app.plugins, ...)`` — this
    fake skips the App indirection (irrelevant to the pack under test) but
    preserves the externally-observable contract: after construction,
    ``self.plugin_manager`` holds every plugin passed via ``plugins=`` (or via
    a fake ``app.plugins``, if supplied instead).

    ``run_async`` is an async generator yielding ``events`` in order (queued
    at construction via ``events=``) — matching the real ``Runner.run_async``
    keyword-only signature (``user_id``, ``session_id``, ``invocation_id=
    None``, ``new_message=None``, plus ``**kwargs`` tolerance for forward
    compatibility). An item that is a ``BaseException`` instance is raised
    instead of yielded, modeling a mid-stream failure. ``run`` is the
    synchronous bridge real ADK provides: it drains ``self.run_async(...)``
    over a fresh event loop — i.e. it calls through whatever ``run_async``
    PRESENTLY resolves to (patched or not), exactly like the real bridge, so
    a class-level ``Runner.run_async`` patch covers ``run()`` callers too
    without ``run`` itself ever needing a separate patch."""

    def __init__(
        self,
        *,
        app: Any = None,
        app_name: str | None = None,
        agent: Any = None,
        node: Any = None,
        plugins: list[Any] | None = None,
        events: list[Any] | None = None,
        **_: Any,
    ) -> None:
        self.app = app
        self.app_name = app_name
        self.agent = agent
        resolved = list(plugins or [])
        if app is not None:
            resolved.extend(getattr(app, "plugins", None) or [])
        self.plugin_manager = FakePluginManager(plugins=resolved)
        self.events = list(events or [])

    async def run_async(
        self,
        *,
        user_id: str,
        session_id: str,
        invocation_id: str | None = None,
        new_message: Any = None,
        **kwargs: Any,
    ) -> Any:
        for item in self.events:
            if isinstance(item, BaseException):
                raise item
            yield item

    def run(
        self,
        *,
        user_id: str,
        session_id: str,
        invocation_id: str | None = None,
        new_message: Any = None,
        **kwargs: Any,
    ) -> list[Any]:
        import asyncio

        async def _drain() -> list[Any]:
            return [
                event
                async for event in self.run_async(
                    user_id=user_id,
                    session_id=session_id,
                    invocation_id=invocation_id,
                    new_message=new_message,
                    **kwargs,
                )
            ]

        return asyncio.run(_drain())


class FakeInMemoryRunner(FakeRunner):
    """Structural twin of ``google.adk.runners.InMemoryRunner``: forwards to
    ``Runner.__init__`` via ``super().__init__(...)``, exactly like the real
    class — this is the behavior ``adk_pack`` relies on (patching only
    ``Runner.__init__`` covers both)."""

    def __init__(
        self,
        agent: Any = None,
        *,
        app_name: str | None = None,
        plugins: list[Any] | None = None,
        app: Any = None,
        **kw: Any,
    ) -> None:
        super().__init__(agent=agent, app_name=app_name, plugins=plugins, app=app, **kw)


class FakeApp:
    """Structural twin of ``google.adk.apps.app.App``: enough to exercise the
    ``Runner(app=App(plugins=[...]))`` construction shape."""

    def __init__(self, *, name: str = "app", root_agent: Any = None, plugins: list[Any] | None = None) -> None:
        self.name = name
        self.root_agent = root_agent
        self.plugins = list(plugins or [])


class FakeTool:
    """Structural twin of a ``google.adk.tools.base_tool.BaseTool``
    (``FunctionTool``-shaped): ``run_async`` drives ``func`` (sync or async),
    recording every invocation for assertions."""

    def __init__(self, name: str, func: Callable[..., Any]) -> None:
        self.name = name
        self.func = func
        self.calls = 0

    async def run_async(self, *, args: dict[str, Any], tool_context: Any) -> Any:
        self.calls += 1
        import inspect

        if inspect.iscoroutinefunction(self.func):
            return await self.func(**args)
        return self.func(**args)


class FakeSlottedTool:
    """A ``__slots__``-restricted tool: ``setattr(tool, "run_async", …)``
    raises ``AttributeError``, exercising adk_pack's rebind-refusal fallback
    (the plugin-loop path). Same observable surface as ``FakeTool``."""

    __slots__ = ("name", "func", "calls")

    def __init__(self, name: str, func: Callable[..., Any]) -> None:
        self.name = name
        self.func = func
        self.calls = 0

    async def run_async(self, *, args: dict[str, Any], tool_context: Any) -> Any:
        self.calls += 1
        import inspect

        if inspect.iscoroutinefunction(self.func):
            return await self.func(**args)
        return self.func(**args)


class McpTool(FakeTool):
    """Structural twin of ``google.adk.tools.mcp_tool.mcp_tool.McpTool`` for
    adk_pack's MRO-name-based detection (`_is_mcp_tool`): under ADK's
    graceful error handling (`_MCP_GRACEFUL_ERROR_HANDLING`), a failed MCP
    call RETURNS ``{"error": "<message>"}`` instead of raising — the class
    name (not this fixture's module path) is the detection key."""


def _fake_module(name: str, **attrs: Any) -> types.ModuleType:
    mod = types.ModuleType(name)
    mod.__spec__ = ModuleSpec(name, loader=None, is_package=True)
    mod.__path__ = []  # marks it as a package for dotted submodule resolution
    for key, value in attrs.items():
        setattr(mod, key, value)
    return mod


#: The exact `sys.modules` keys this fixture ever sets — used to snapshot and
#: precisely restore state (never touches unrelated `google.*` modules a real
#: environment might have installed, e.g. `google.protobuf`).
_MODULE_NAMES = (
    "google",
    "google.adk",
    "google.adk.runners",
    "google.adk.plugins",
    "google.adk.plugins.base_plugin",
)


class FakeAdkModules:
    """Context manager installing the fake ``google.adk`` package tree into
    ``sys.modules`` for the duration of a test, then restoring the exact prior
    state (present or absent) for each of the touched module names — safe to
    use even when a REAL ``google`` namespace package is already installed
    for an unrelated reason (verified on this dev machine: a bare ``google``
    PEP 420 namespace package, from some other ``google-*`` distribution, was
    already present; this fixture leaves it untouched)."""

    def __init__(self) -> None:
        self._saved: dict[str, types.ModuleType | None] = {}

    def __enter__(self) -> "FakeAdkModules":
        for name in _MODULE_NAMES:
            self._saved[name] = sys.modules.get(name)

        google_mod = self._saved["google"] or _fake_module("google")
        adk_mod = _fake_module("google.adk")
        runners_mod = _fake_module(
            "google.adk.runners", Runner=FakeRunner, InMemoryRunner=FakeInMemoryRunner
        )
        plugins_pkg = _fake_module("google.adk.plugins")
        base_plugin_mod = _fake_module("google.adk.plugins.base_plugin", BasePlugin=FakeBasePlugin)

        google_mod.adk = adk_mod
        adk_mod.runners = runners_mod
        adk_mod.plugins = plugins_pkg
        plugins_pkg.base_plugin = base_plugin_mod

        sys.modules["google"] = google_mod
        sys.modules["google.adk"] = adk_mod
        sys.modules["google.adk.runners"] = runners_mod
        sys.modules["google.adk.plugins"] = plugins_pkg
        sys.modules["google.adk.plugins.base_plugin"] = base_plugin_mod
        return self

    def __exit__(self, *exc: Any) -> None:
        for name in _MODULE_NAMES:
            original = self._saved.get(name)
            if original is None:
                sys.modules.pop(name, None)
            else:
                sys.modules[name] = original


__all__ = [
    "FakeAdkModules",
    "FakeApp",
    "FakeBasePlugin",
    "FakeEvent",
    "FakeInMemoryRunner",
    "FakePluginManager",
    "FakeRunner",
    "FakeTool",
    "FakeSlottedTool",
    "McpTool",
]
