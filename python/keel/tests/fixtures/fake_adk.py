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


class FakeEventActions:
    """Structural twin of ``google.adk.events.event_actions.EventActions``:
    ``KeelSessionService``/the base ``append_event`` implementation only
    ever read ``.state_delta`` off it (design doc issue #15 §3.1)."""

    def __init__(self, *, state_delta: dict[str, Any] | None = None, **extra: Any) -> None:
        self.state_delta: dict[str, Any] = dict(state_delta) if state_delta else {}
        for key, value in extra.items():
            setattr(self, key, value)

    def __eq__(self, other: object) -> bool:
        return isinstance(other, FakeEventActions) and vars(self) == vars(other)

    def __repr__(self) -> str:
        return f"FakeEventActions({vars(self)!r})"


class FakeSession:
    """Structural twin of ``google.adk.sessions.session.Session`` — fields
    ``app_name``/``user_id``/``id``/``state``/``events``/``last_update_time``
    (no ``created_at``; confirmed against the real 2.4.0 package by the
    ``KeelSessionService`` implementation phase). Supports
    ``.model_copy(deep=False)``, the one pydantic-model method
    ``adk_pack._copy_session_light`` calls on it."""

    def __init__(
        self,
        *,
        app_name: str,
        user_id: str,
        id: str,
        state: dict[str, Any] | None = None,
        events: list[Any] | None = None,
        last_update_time: float = 0.0,
    ) -> None:
        self.app_name = app_name
        self.user_id = user_id
        self.id = id
        self.state: dict[str, Any] = dict(state) if state else {}
        self.events: list[Any] = list(events) if events else []
        self.last_update_time = last_update_time

    def model_copy(self, *, deep: bool = False) -> "FakeSession":
        return FakeSession(
            app_name=self.app_name,
            user_id=self.user_id,
            id=self.id,
            state=self.state,
            events=self.events,
            last_update_time=self.last_update_time,
        )

    def __eq__(self, other: object) -> bool:
        return isinstance(other, FakeSession) and vars(self) == vars(other)

    def __repr__(self) -> str:
        return f"FakeSession({vars(self)!r})"


class FakeBaseSessionService:
    """Structural (non-ABC-enforced) twin of ``google.adk.sessions.
    base_session_service.BaseSessionService``: the real class's own
    ``append_event`` has a CONCRETE base implementation (applies the event's
    ``state_delta`` to ``session.state``, appends the event to
    ``session.events``, no-ops entirely for a ``partial`` event) — mirrored
    here so ``KeelSessionService``'s ``await super().append_event(...)`` call
    (design doc issue #15 §3.1) has a real body to run against offline.
    Every OTHER method is abstract in the real class; ``KeelSessionService``
    overrides all of them, so this fake never needs concrete bodies for the
    rest (mirrors ``FakeEvent``/``FakeTool``'s own "just enough surface"
    philosophy)."""

    async def append_event(self, session: Any, event: Any) -> Any:
        if getattr(event, "partial", False):
            return event
        actions = getattr(event, "actions", None)
        if actions is not None and getattr(actions, "state_delta", None):
            for key, value in actions.state_delta.items():
                session.state[key] = value
        session.events.append(event)
        timestamp = getattr(event, "timestamp", None)
        if timestamp is not None:
            session.last_update_time = timestamp
        return event

    async def get_session(
        self, *, app_name: str, user_id: str, session_id: str, config: Any | None = None
    ) -> Any:
        raise NotImplementedError

    async def create_session(
        self,
        *,
        app_name: str,
        user_id: str,
        state: dict[str, Any] | None = None,
        session_id: str | None = None,
    ) -> Any:
        raise NotImplementedError

    async def delete_session(self, *, app_name: str, user_id: str, session_id: str) -> None:
        raise NotImplementedError

    async def list_sessions(self, *, app_name: str, user_id: str | None = None) -> Any:
        raise NotImplementedError


class FakeListSessionsResponse:
    """Structural twin of ``google.adk.sessions.base_session_service.
    ListSessionsResponse``."""

    def __init__(self, *, sessions: list[Any] | None = None) -> None:
        self.sessions: list[Any] = list(sessions) if sessions else []

    def __eq__(self, other: object) -> bool:
        return isinstance(other, FakeListSessionsResponse) and self.sessions == other.sessions

    def __repr__(self) -> str:
        return f"FakeListSessionsResponse(sessions={self.sessions!r})"


class FakeGetSessionConfig:
    """Structural twin of ``google.adk.sessions.base_session_service.
    GetSessionConfig`` (design doc issue #15 §4/§6 item 1: both fields
    optional). ``adk_pack`` never imports this class itself (``config`` is
    duck-typed via ``getattr``) — provided for tests that want a
    natural-looking constructor instead of a bare namespace object."""

    def __init__(
        self, *, num_recent_events: int | None = None, after_timestamp: float | None = None
    ) -> None:
        self.num_recent_events = num_recent_events
        self.after_timestamp = after_timestamp


class FakeAlreadyExistsError(Exception):
    """Structural twin of ``google.adk.errors.already_exists_error.
    AlreadyExistsError``."""


class FakeBlob:
    """Structural twin of ``google.genai.types.Blob``: raw bytes + mime
    type — the shape ``adk_pack._encode_part``/``_decode_part`` read/build
    for inline (binary) ``Part`` content (design doc issue #15 §3.1)."""

    def __init__(self, *, data: bytes | None = None, mime_type: str | None = None, **extra: Any) -> None:
        self.data = data
        self.mime_type = mime_type
        for key, value in extra.items():
            setattr(self, key, value)

    def __eq__(self, other: object) -> bool:
        return isinstance(other, FakeBlob) and vars(self) == vars(other)

    def __repr__(self) -> str:
        return f"FakeBlob({vars(self)!r})"


class FakePart:
    """Structural twin of ``google.genai.types.Part`` — enough fields for
    ``adk_pack._encode_part``/``_decode_part``'s three cases (plain text /
    inline binary data / ``model_dump`` fallback for everything else, e.g.
    ``function_call``/``function_response``). ``model_dump`` mirrors the
    real ``google.genai._common.BaseModel`` shape closely enough for the
    pack's own encode/decode round trip: ``exclude_none=True`` drops unset
    fields, ``mode=\"json\"`` is accepted (bytes-safety is NOT reproduced
    here — no test in this suite round-trips ``thought_signature`` bytes
    through JSON mode; the real package's ``ser_json_bytes=\"base64\"``/
    ``val_json_bytes=\"base64\"`` behavior is verified separately, directly
    against the real 2.4.0 package, per ``_encode_part``'s own docstring)."""

    def __init__(
        self,
        *,
        text: str | None = None,
        inline_data: Any = None,
        thought: bool | None = None,
        thought_signature: Any = None,
        function_call: dict[str, Any] | None = None,
        function_response: dict[str, Any] | None = None,
        **extra: Any,
    ) -> None:
        self.text = text
        self.inline_data = inline_data
        self.thought = thought
        self.thought_signature = thought_signature
        self.function_call = function_call
        self.function_response = function_response
        for key, value in extra.items():
            setattr(self, key, value)

    def model_dump(self, *, exclude_none: bool = False, mode: str = "python") -> dict[str, Any]:
        data: dict[str, Any] = {
            "text": self.text,
            "inline_data": (
                {"data": self.inline_data.data, "mime_type": self.inline_data.mime_type}
                if self.inline_data is not None
                else None
            ),
            "thought": self.thought,
            "thought_signature": self.thought_signature,
            "function_call": self.function_call,
            "function_response": self.function_response,
        }
        if exclude_none:
            data = {k: v for k, v in data.items() if v is not None}
        return data

    def __eq__(self, other: object) -> bool:
        return isinstance(other, FakePart) and vars(self) == vars(other)

    def __repr__(self) -> str:
        return f"FakePart({vars(self)!r})"


class FakeContent:
    """Structural twin of ``google.genai.types.Content``: ``role`` + a list
    of ``Part``-shaped objects."""

    def __init__(self, *, role: str | None = None, parts: list[Any] | None = None) -> None:
        self.role = role
        self.parts: list[Any] = list(parts) if parts else []

    def __eq__(self, other: object) -> bool:
        return isinstance(other, FakeContent) and self.role == other.role and self.parts == other.parts

    def __repr__(self) -> str:
        return f"FakeContent(role={self.role!r}, parts={self.parts!r})"


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


class FakeLlmRequest:
    """Structural twin of ``google.adk.models.llm_request.LlmRequest``:
    ``adk_pack``'s ``on_model_error`` fallback reads only ``.model`` off it
    (a plain model-name string, per the real ADK field, verified against the
    2.4.0 package) to resolve the FAILING model's class for the same-class
    skip rule — the object itself is passed through unchanged to a fallback
    hop's ``generate_content_async``."""

    def __init__(self, model: str | None = None, **extra: Any) -> None:
        self.model = model
        for key, value in extra.items():
            setattr(self, key, value)

    def __eq__(self, other: object) -> bool:
        return isinstance(other, FakeLlmRequest) and vars(self) == vars(other)

    def __repr__(self) -> str:
        return f"FakeLlmRequest({vars(self)!r})"


class FakeLlmResponse:
    """Structural twin of ``google.adk.models.llm_response.LlmResponse`` —
    enough of a shell to assert identity/content across a fallback hop."""

    def __init__(self, content: Any = None, **extra: Any) -> None:
        self.content = content
        for key, value in extra.items():
            setattr(self, key, value)

    def __eq__(self, other: object) -> bool:
        return isinstance(other, FakeLlmResponse) and vars(self) == vars(other)

    def __repr__(self) -> str:
        return f"FakeLlmResponse({vars(self)!r})"


class FakeModel:
    """Structural twin of a ``google.adk.models.base_llm.BaseLlm`` backend:
    ``generate_content_async`` is an async generator yielding a scripted
    response sequence (or raising a scripted exception on first drive),
    recording every ``llm_request`` it was driven with — enough to assert a
    fallback hop actually ran (or didn't)."""

    def __init__(self, *, responses: list[Any] | None = None, error: Exception | None = None) -> None:
        self.responses = list(responses or [])
        self.error = error
        self.calls: list[Any] = []

    async def generate_content_async(self, llm_request: Any, stream: bool = False) -> Any:
        self.calls.append(llm_request)
        if self.error is not None:
            raise self.error
        for resp in self.responses:
            yield resp


class FakeGemini(FakeModel):
    """A fake google-genai-backed model class — the same \"provider class\"
    a failing Gemini call's own model would resolve to, for exercising
    decision 7's same-class skip rule (a same-provider chain entry is left
    for the transport seam to have already chased)."""


class FakeClaude(FakeModel):
    """A fake distinct-provider model class, for exercising a genuine
    cross-provider fallback hop."""


class FakeLLMRegistry:
    """Structural twin of ``google.adk.models.registry.LLMRegistry`` (real
    ADK: ``resolve(model: str) -> type[BaseLlm]`` / ``new_llm(model: str) ->
    BaseLlm``, both classmethods matching a registered pattern per model
    name). This fake is a plain per-test dict lookup — CLASS-level state,
    shared across every fake ``LLMRegistry`` import, so every test using it
    MUST call ``reset()`` in ``setUp``/``tearDown``.

    ``configure(name, instance)`` registers a model name that resolves
    (``resolve``) to ``instance``'s class and constructs (``new_llm``) to
    ``instance`` itself — the ordinary "this chain entry works" case.
    ``break_new_llm(name, model_class, error)`` registers a name whose
    ``resolve`` succeeds (so the same-class check still sees a real class)
    but whose ``new_llm`` raises ``error`` — the "resolvable name, but its
    provider package isn't actually installed" shape. A name never
    registered at all makes ``resolve`` raise (the "unknown model name"
    shape)."""

    _classes: dict[str, type] = {}
    _instances: dict[str, Any] = {}
    _new_llm_errors: dict[str, Exception] = {}

    @classmethod
    def reset(cls) -> None:
        cls._classes = {}
        cls._instances = {}
        cls._new_llm_errors = {}

    @classmethod
    def configure(cls, name: str, instance: Any, *, model_class: type | None = None) -> None:
        cls._classes[name] = model_class or type(instance)
        cls._instances[name] = instance

    @classmethod
    def break_new_llm(cls, name: str, model_class: type, error: Exception) -> None:
        cls._classes[name] = model_class
        cls._new_llm_errors[name] = error

    @classmethod
    def resolve(cls, model: str) -> type:
        try:
            return cls._classes[model]
        except KeyError:
            raise ValueError(f"Model {model!r} not found.") from None

    @classmethod
    def new_llm(cls, model: str) -> Any:
        if model in cls._new_llm_errors:
            raise cls._new_llm_errors[model]
        try:
            return cls._instances[model]
        except KeyError:
            raise ValueError(f"Model {model!r} not found.") from None


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
    "google.adk.models",
    "google.adk.models.registry",
    # `KeelSessionService` (design doc issue #15 §3.1's `_base_session_service_cls`)
    # imports across all four of these — registered the same way as the two
    # entries above (a fake package + a fake leaf module underneath it).
    "google.adk.sessions",
    "google.adk.sessions.base_session_service",
    "google.adk.events",
    "google.adk.events.event",
    "google.adk.events.event_actions",
    "google.adk.errors",
    "google.adk.errors.already_exists_error",
    # `google.genai` is a SIBLING package to `google.adk` (not a submodule of
    # it) but shares the same `google` namespace root — `KeelSessionService`
    # needs `google.genai.types.{Blob,Content,Part}` for event-content
    # encoding (design §3.1). Faked (never left to a real install) because a
    # real `google-genai` distribution IS present on at least one dev
    # machine this suite runs on (an unrelated dependency of some other
    # `google-*` package) — the same "safe even if a real one exists"
    # discipline this class's own docstring already applies to `google`.
    "google.genai",
    "google.genai.types",
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
        models_pkg = _fake_module("google.adk.models")
        registry_mod = _fake_module("google.adk.models.registry", LLMRegistry=FakeLLMRegistry)
        sessions_pkg = _fake_module(
            "google.adk.sessions", BaseSessionService=FakeBaseSessionService, Session=FakeSession
        )
        base_session_service_mod = _fake_module(
            "google.adk.sessions.base_session_service",
            ListSessionsResponse=FakeListSessionsResponse,
            GetSessionConfig=FakeGetSessionConfig,
        )
        events_pkg = _fake_module("google.adk.events")
        event_mod = _fake_module("google.adk.events.event", Event=FakeEvent)
        event_actions_mod = _fake_module("google.adk.events.event_actions", EventActions=FakeEventActions)
        errors_pkg = _fake_module("google.adk.errors")
        already_exists_error_mod = _fake_module(
            "google.adk.errors.already_exists_error", AlreadyExistsError=FakeAlreadyExistsError
        )
        genai_pkg = _fake_module("google.genai")
        genai_types_mod = _fake_module(
            "google.genai.types", Blob=FakeBlob, Content=FakeContent, Part=FakePart
        )

        google_mod.adk = adk_mod
        adk_mod.runners = runners_mod
        adk_mod.plugins = plugins_pkg
        adk_mod.models = models_pkg
        adk_mod.sessions = sessions_pkg
        adk_mod.events = events_pkg
        adk_mod.errors = errors_pkg
        plugins_pkg.base_plugin = base_plugin_mod
        models_pkg.registry = registry_mod
        sessions_pkg.base_session_service = base_session_service_mod
        events_pkg.event = event_mod
        events_pkg.event_actions = event_actions_mod
        errors_pkg.already_exists_error = already_exists_error_mod
        google_mod.genai = genai_pkg
        genai_pkg.types = genai_types_mod

        sys.modules["google"] = google_mod
        sys.modules["google.adk"] = adk_mod
        sys.modules["google.adk.runners"] = runners_mod
        sys.modules["google.adk.plugins"] = plugins_pkg
        sys.modules["google.adk.plugins.base_plugin"] = base_plugin_mod
        sys.modules["google.adk.models"] = models_pkg
        sys.modules["google.adk.models.registry"] = registry_mod
        sys.modules["google.adk.sessions"] = sessions_pkg
        sys.modules["google.adk.sessions.base_session_service"] = base_session_service_mod
        sys.modules["google.adk.events"] = events_pkg
        sys.modules["google.adk.events.event"] = event_mod
        sys.modules["google.adk.events.event_actions"] = event_actions_mod
        sys.modules["google.adk.errors"] = errors_pkg
        sys.modules["google.adk.errors.already_exists_error"] = already_exists_error_mod
        sys.modules["google.genai"] = genai_pkg
        sys.modules["google.genai.types"] = genai_types_mod
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
    "FakeAlreadyExistsError",
    "FakeApp",
    "FakeBasePlugin",
    "FakeBaseSessionService",
    "FakeBlob",
    "FakeClaude",
    "FakeContent",
    "FakeEvent",
    "FakeEventActions",
    "FakeGemini",
    "FakeGetSessionConfig",
    "FakeInMemoryRunner",
    "FakeLLMRegistry",
    "FakeListSessionsResponse",
    "FakeLlmRequest",
    "FakeLlmResponse",
    "FakeModel",
    "FakePart",
    "FakePluginManager",
    "FakeRunner",
    "FakeSession",
    "FakeTool",
    "FakeSlottedTool",
    "McpTool",
]
