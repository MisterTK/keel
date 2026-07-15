"""The Google ADK framework pack (dx-spec §4.2): a ``keel`` plugin registered
automatically at import time via ADK's own plugin/callback API.

ADK's vendor-documented gap (dx-spec §4): *"no built-in retry/backoff; error
handling is callback-based and left to the developer."* Verified directly
against the real ``google-adk`` package (2.4.0, throwaway venv — never a repo
dependency; adapter-pack rule 1 forbids importing a library that is not
present *and in use*): ``google.genai``'s ``SyncHttpxClient``/
``AsyncHttpxClient`` subclass ``httpx.Client``/``httpx.AsyncClient`` without
overriding ``__init__``, and its own retry helper defaults to
``stop_after_attempt(1)`` (never retries) unless the caller explicitly opts
into ``HttpRetryOptions`` — i.e. ADK genuinely retries nothing on its own,
matching the vendor claim exactly.

Two Keel target classes come out of this pack, split by where the seam lives:

* ``llm:google-genai`` — **needs no code here.** ADK's Gemini model backend
  rides the ``google-genai`` SDK, which rides ``httpx``; ``keel.adapters.
  httpx_pack`` already intercepts every ``httpx.Client``/``AsyncClient``
  request and ``adapters._http.LLM_HOST_PROVIDERS`` already host-maps
  ``generativelanguage.googleapis.com`` → ``llm:google-genai``. This pack
  declares the target (below) for documentation/doctor visibility only — the
  same "declared but not owned" pattern ``openai_pack``/``anthropic_pack`` use
  for their own ``llm:<provider>`` targets.
* ``tool:<name>`` — **is this pack's actual seam.** Every ADK tool invocation
  is wrapped through :func:`keel.packs.tool.wrap_tool` at ADK's own
  ``before_tool_callback`` plugin hook (below), giving every tool call
  discovery/breaker/timeout coverage for free and turning "on_tool_error
  wiring" from hand-written callback code into policy (dx-spec §4.2) — with
  retry itself opt-in exactly like every other ``tool:`` call site (Level 0
  hard rule; see "Idempotency" below).

Honest limitation (do not oversell): a raw Gemini ``generateContent`` call is
an HTTP POST, so it is non-idempotent by the same Level 0 hard rule — observed
on a 429, not retried (KEEL-E014) — until the separate idempotency-key
injection follow-up (``contracts/adapter-pack.md`` "Idempotency-key
injection", ``docs/ccr/0002-*.md``) lands in the httpx/requests adapters. That
follow-up is a different task's territory; this pack does not implement it.
What IS real and shippable today is tool-call resilience (below) and any
``llm:google-genai`` call that already carries a recognized idempotency
header (e.g. one supplied by the caller) or is itself a safe GET.

Zero-code-change seam: plugin auto-registration, not a runner wrapper
----------------------------------------------------------------------
The brief poses a choice: monkey-patch ``BaseTool.run_async`` (or wrap
whatever object the user passes to ``Runner``), or register a ``BasePlugin``
automatically. This pack takes the **plugin** path:

* ADK already ships a first-class, documented, versioned extension point for
  exactly this (``google.adk.plugins.base_plugin.BasePlugin``) — patching a
  tool's private execution method would reach for something ADK never
  promised to keep stable.
* A plugin is *reversible* by construction: ``PluginManager`` exposes
  ``get_plugin``/``register_plugin`` (never private state), so attaching and
  detaching Keel's plugin never mutates anything ADK doesn't already expose.
* It composes with a user's own plugins/callbacks instead of shadowing them:
  ``PluginManager._run_callbacks`` runs registered plugins in order and stops
  at the first non-``None`` result, so a user-authored ``before_tool_callback``
  (agent-level or another plugin) still runs exactly where it always did.

The patch point is ``google.adk.runners.Runner.__init__`` — the single
construction chokepoint, verified against the real package: ``InMemoryRunner.
__init__`` forwards to ``Runner.__init__`` via ``super().__init__(...)``, and
``agent=``/``node=``/``app=`` all resolve (``Runner._resolve_app``) into one
``App`` *before* ``self.plugin_manager = PluginManager(plugins=app.plugins,
...)`` is built. Patching post-construction and reading back
``self.plugin_manager`` (rather than rewriting the ``plugins=``/``app=``
keyword arguments pre-init) covers every construction shape uniformly —
including the modern, recommended ``Runner(app=App(plugins=[...]))`` shape,
where ``plugins=`` itself is documented as deprecated.

Idempotency (Level 0 hard rule, mirrors ``keel.packs.tool``): a tool runs
arbitrary side-effecting code, so every ADK tool call is wrapped with
``idempotent=False`` — always. This pack auto-wraps *every* plugin-visible
tool call generically; unlike a hand-written ``wrap_tool(..., idempotent=True)``
call authored by the tool's own implementer, a framework pack has no way to
assert a given tool is safe to re-invoke. Every call still gets
breaker/timeout/discovery coverage (a real win — this is exactly the
resilience ADK's own vendor gap says does not exist today); retry itself
stays off until the tool's own author opts in some other way (there is no
keel.toml knob for this in v0.1, the same documented decision ``tool.py``
makes).

``before_tool_callback`` is the whole seam, not ``on_tool_error_callback``:
ADK's own tool-execution code (``flows/llm_flows/functions.py``,
``_execute_single_function_call_async``) runs ``before_tool_callback`` FIRST
and skips the real ``tool.run_async`` entirely once a plugin returns non-
``None`` — so driving the full ``wrap_tool`` attempt loop from inside
``before_tool_callback`` (short-circuiting with the eventual result, or
letting a terminal failure raise) is the only way for Keel's breaker/timeout
to guard the FIRST attempt too, not just retries after ADK's own call already
failed. A terminal failure raised here surfaces to the caller as
``RuntimeError`` (ADK's ``PluginManager._run_callbacks`` wraps any exception a
plugin callback raises, chaining the original as ``__cause__``) — a framework
convention outside this pack's control, not a Keel behavior change.
"""

from __future__ import annotations

import functools
import importlib.metadata
import os
import sys
import weakref
from typing import Any, Callable

from ..adapters._pack import Detection, Seam, TargetDecl
from ._provider import module_present
from .tool import is_valid_tool_name, wrap_tool

MODULE = "google.adk"
#: The PyPI distribution name differs from the import name; used for the
#: `importlib.metadata.version` lookup only.
NAME = "google-adk"

#: Versions this pack certifies via contract tests (prefix match), verified
#: directly against the real package. Outside the range `detect()` reports
#: `best_effort` — the pack still tries (adapter-pack rule 2).
_PINNED = ("1", "2")

#: The registered plugin's unique name (`BasePlugin.name` / `PluginManager.
#: get_plugin`/`register_plugin` key).
PLUGIN_NAME = "keel"

_TRUTHY = {"1", "true", "yes"}

_installed = False
_orig: dict[str, Any] = {}
_plugin_singleton: Any = None
_noted_skips: set[str] = set()

#: Marker attribute set on a rebound instance's replacement ``run_async``.
_REBOUND_ATTR = "__keel_adk_rebound__"

#: Sentinel: the tool had NO instance-level ``run_async`` before rebinding
#: (the overwhelmingly normal case — the method lives on the class).
_ABSENT = object()

#: Rebound tool instance → its prior instance-dict ``run_async`` entry (or
#: ``_ABSENT``). Weak keys: a garbage-collected tool needs no restoration.
#: A tool that cannot be weak-referenced still gets rebound — it just cannot
#: be individually restored by ``uninstall()`` (noted in the docstring).
_rebound: "weakref.WeakKeyDictionary[Any, Any]" = weakref.WeakKeyDictionary()


# --- contract operations -----------------------------------------------------


def detect() -> Detection:
    """Present iff ``google.adk`` is importable — decided WITHOUT importing it
    (importability + installed distribution metadata only, adapter-pack rule
    1)."""
    if not module_present(MODULE):
        return Detection(matched=False)
    try:
        version = importlib.metadata.version(NAME)
    except importlib.metadata.PackageNotFoundError:
        version = ""
    confidence = "pinned" if _is_pinned(version) else "best_effort"
    return Detection(matched=True, name=NAME, version=version, confidence=confidence)


def seams() -> list[Seam]:
    return [
        Seam(
            patch_point="google.adk.runners.Runner.__init__",
            upstream_api=(
                "ADK Runner: Runner(*, app=None, agent=None, node=None, "
                "plugins=None, ...) resolves onto self.plugin_manager: "
                "PluginManager (register_plugin/get_plugin)"
            ),
            why_stable=(
                "Runner.__init__ is ADK's single construction chokepoint — "
                "InMemoryRunner.__init__ forwards to it via super().__init__, "
                "and agent=/node=/app= all resolve into one App before "
                "self.plugin_manager is built. Reading back plugin_manager "
                "post-construction and registering through its own documented "
                "get_plugin/register_plugin API (never poking private state) "
                "covers every construction shape uniformly and stays fully "
                "reversible."
            ),
        )
    ]


def targets() -> list[TargetDecl]:
    return [
        TargetDecl(
            pattern="tool:<name>",
            kind="tool",
            idempotency_rule=(
                "identical to the generic tool: pack (adapter-pack rule 3: no "
                "resilience logic of its own) — non-idempotent by default, "
                "observed not retried (KEEL-E014). This pack auto-wraps every "
                "plugin-visible ADK tool call with idempotent=False always: a "
                "generic framework pack cannot assert a given tool is safe to "
                "re-invoke on the tool author's behalf."
            ),
            args_hash_rule="None (a non-idempotent tool call is never cached)",
        ),
        TargetDecl(
            pattern="llm:google-genai",
            kind="llm",
            idempotency_rule=(
                "not produced by this pack: ADK's Gemini model backend rides "
                "the google-genai SDK, which rides httpx — already "
                "intercepted by keel.adapters.httpx_pack's transport seam and "
                "host-mapped to llm:google-genai (adapters/_http.py "
                "LLM_HOST_PROVIDERS). This pack owns no model-call seam of "
                "its own; declared here for doctor/documentation visibility "
                "only, mirroring openai_pack/anthropic_pack."
            ),
            args_hash_rule="as for the httpx host-mapped llm: target",
        ),
    ]


def defaults() -> dict[str, Any]:
    """Empty: contracts/defaults.toml defines no ``[defaults.adk]`` table, so
    both target classes above inherit through the existing chain
    (``tool:`` → ``[defaults.outbound]``; ``llm:google-genai`` →
    ``[defaults.llm]``) — mirrors the ``tool:``/``mcp:`` packs exactly."""
    return {}


# --- install / uninstall -----------------------------------------------------


def install() -> None:
    """Patch ``Runner.__init__`` so every constructed Runner (any of
    ``agent=``/``node=``/``app=``) gets Keel's plugin auto-registered.
    Idempotent; a no-op if ``google.adk`` is not importable."""
    global _installed
    if _installed:
        return
    try:
        from google.adk.runners import Runner
    except ImportError:
        return
    _orig["init"] = Runner.__init__
    Runner.__init__ = _init_wrapper(_orig["init"])  # type: ignore[method-assign]
    _installed = True


def uninstall() -> None:
    """Restore the original ``Runner.__init__`` and un-shadow every rebound
    tool instance still alive (weak registry — collected tools need nothing).
    Runners already constructed keep whatever plugins they were given
    (mirrors httpx_pack: uninstall un-arms future construction)."""
    global _installed
    _restore_rebound()
    if not _installed:
        return
    try:
        from google.adk.runners import Runner
    except ImportError:
        _orig.clear()
        _installed = False
        return
    Runner.__init__ = _orig["init"]  # type: ignore[method-assign]
    _orig.clear()
    _installed = False


def _restore_rebound() -> None:
    for tool, prior in list(_rebound.items()):
        try:
            if prior is _ABSENT:
                if "run_async" in tool.__dict__:
                    delattr(tool, "run_async")
            else:
                setattr(tool, "run_async", prior)
        except (AttributeError, TypeError):
            pass  # never let a hostile tool object break uninstall
    _rebound.clear()


def _init_wrapper(orig_init: Callable[..., None]) -> Callable[..., None]:
    @functools.wraps(orig_init)
    def __init__(self: Any, *args: Any, **kwargs: Any) -> None:
        orig_init(self, *args, **kwargs)
        _attach_plugin(self)

    __init__.__keel_wrapped__ = True  # type: ignore[attr-defined]
    return __init__


def _attach_plugin(runner: Any) -> None:
    """Register Keel's plugin on a freshly constructed Runner, unless one is
    already registered (a user's own, or — defensively — a prior
    construction): ``PluginManager.register_plugin`` raises ``ValueError`` on
    a name clash, so this pack checks first rather than relying on a catch."""
    manager = getattr(runner, "plugin_manager", None)
    register = getattr(manager, "register_plugin", None)
    get = getattr(manager, "get_plugin", None)
    if not callable(register) or not callable(get):
        return  # unexpected shape (an unsupported ADK version): do nothing unsafe
    if get(PLUGIN_NAME) is not None:
        return
    register(_plugin())


# --- the plugin ---------------------------------------------------------------


def _plugin() -> Any:
    """The shared ``KeelPlugin`` singleton, built lazily against whatever
    ``BasePlugin`` is importable RIGHT NOW — never at module import time, so
    this module stays stdlib-only when ``google.adk`` is absent (adapter-pack
    rule 1: a pack never imports its library unless present and in use)."""
    global _plugin_singleton
    if _plugin_singleton is None:
        from google.adk.plugins.base_plugin import BasePlugin

        class _KeelPlugin(BasePlugin):  # noqa: keep tiny — one callback, one job
            def __init__(self) -> None:
                super().__init__(name=PLUGIN_NAME)

            async def before_tool_callback(
                self, *, tool: Any, tool_args: dict[str, Any], tool_context: Any
            ) -> dict[str, Any] | None:
                return await _on_before_tool(tool, tool_args, tool_context)

        _plugin_singleton = _KeelPlugin()
    return _plugin_singleton


async def _on_before_tool(tool: Any, tool_args: dict[str, Any], tool_context: Any) -> dict[str, Any] | None:
    """The ``before_tool_callback`` body. Returning ``None`` is the contract
    that keeps ADK's own sequence intact: agent-level before-callbacks still
    run (ADK only skips them on a non-``None`` plugin return), the real call
    happens at ADK's step 3 through our rebound wrapper, and a terminal
    failure raises from the real call — inside ADK's try/except — so user
    ``on_tool_error`` plugins/callbacks (including ADK's own
    ``ReflectAndRetryToolPlugin``) fire exactly as they would unwrapped."""
    name = getattr(tool, "name", None)
    if not is_valid_tool_name(name):
        _note_skip(name)
        return None  # not our target grammar: let ADK invoke it, unwrapped
    if getattr(getattr(tool, "run_async", None), _REBOUND_ATTR, False):
        return None  # already rebound on a prior sight
    if _rebind_tool(tool, name):
        return None  # rebound: ADK proceeds normally, Keel wraps the real call
    # Instance rejects setattr (slots/frozen): keep coverage via the old
    # loop-in-callback path — documented trade-off (agent-level
    # before-callbacks are bypassed for THIS tool only).
    _note_fallback(name)
    return await _call_via_plugin_loop(tool, tool_args, tool_context)


def _rebind_tool(tool: Any, name: str) -> bool:
    """Rebind ``tool.run_async`` (instance attribute shadowing the class
    method) to a Keel-wrapped version. The breaker still guards attempt 1:
    ``wrap_tool``'s wrapper consults policy before the first underlying
    invoke. Returns ``False`` when the instance rejects attribute assignment."""
    original = tool.run_async  # the bound method, captured pre-shadow

    async def _invoke(*, args: dict[str, Any], tool_context: Any) -> Any:
        return await original(args=args, tool_context=tool_context)

    _invoke.__qualname__ = f"adk.tool.{name}"
    inner = wrap_tool(name, _invoke, idempotent=False)

    @functools.wraps(original)
    async def run_async(*, args: dict[str, Any], tool_context: Any) -> Any:
        return await inner(args=args, tool_context=tool_context)

    setattr(run_async, _REBOUND_ATTR, True)
    prior = tool.__dict__.get("run_async", _ABSENT) if hasattr(tool, "__dict__") else _ABSENT
    try:
        setattr(tool, "run_async", run_async)
    except (AttributeError, TypeError):
        return False
    try:
        _rebound[tool] = prior
    except TypeError:
        pass  # not weakref-able: rebound, but uninstall() cannot restore it
    return True


async def _call_via_plugin_loop(tool: Any, tool_args: dict[str, Any], tool_context: Any) -> dict[str, Any] | None:
    """The ``before_tool_callback`` body (module-level so it needs no ADK
    types): resolve the tool's name against the frozen ``tool:<name>``
    grammar — skip-and-note (Level 0: "if a call site cannot be wrapped
    safely, do nothing" — ``tool.py``'s own guidance for auto-wrap packs) — or
    drive the full retry-eligible call through :func:`keel.packs.tool.
    wrap_tool` and hand back a result ADK's own normalization already treats
    identically to a real call's raw return (``__build_response_event``:
    ``if not isinstance(function_result, dict): function_result = {'result':
    function_result}``) — this pack just applies the same rule one step
    earlier, so the plugin's ``is not None`` short-circuit check is
    unambiguous even for a tool that legitimately returns ``None``."""
    name = getattr(tool, "name", None)
    if not is_valid_tool_name(name):
        _note_skip(name)
        return None  # not our target grammar: let ADK invoke it, unwrapped

    async def _invoke() -> Any:
        return await tool.run_async(args=tool_args, tool_context=tool_context)

    _invoke.__qualname__ = f"adk.tool.{name}"
    result = await wrap_tool(name, _invoke, idempotent=False)()
    return result if isinstance(result, dict) else {"result": result}


def _note_skip(name: object) -> None:
    """Note an unwrappable tool name once (not per-call, to avoid log spam)."""
    key = repr(name)
    if key in _noted_skips:
        return
    _noted_skips.add(key)
    if os.environ.get("KEEL_QUIET", "").strip().lower() in _TRUTHY:
        return
    sys.stderr.write(
        f"keel ▸ adk: tool name {name!r} does not match the tool: target "
        "grammar ([A-Za-z0-9_][A-Za-z0-9_.-]*) — left unwrapped (still runs, "
        "just without Keel's breaker/timeout/discovery coverage)\n"
    )


_noted_fallbacks: set[str] = set()


def _note_fallback(name: str) -> None:
    """Note (once per name) a tool instance that rejected the rebind: Keel
    keeps full coverage via the plugin-loop path, but agent-level
    before_tool_callbacks are bypassed for this tool — worth a line."""
    if name in _noted_fallbacks:
        return
    _noted_fallbacks.add(name)
    if os.environ.get("KEEL_QUIET", "").strip().lower() in _TRUTHY:
        return
    sys.stderr.write(
        f"keel ▸ adk: tool {name!r} rejects attribute rebinding (slots/frozen) — "
        "covered via the plugin loop instead; agent-level before_tool_callbacks "
        "are bypassed for this tool only\n"
    )


def _is_pinned(version: str) -> bool:
    return any(version == p or version.startswith(p + ".") for p in _PINNED)


__all__ = [
    "MODULE",
    "NAME",
    "PLUGIN_NAME",
    "detect",
    "seams",
    "targets",
    "defaults",
    "install",
    "uninstall",
]
