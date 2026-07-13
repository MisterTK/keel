"""The CrewAI framework pack (adapter-pack contract).

Seam: ``crewai.tools.structured_tool.CrewStructuredTool.invoke`` /
``.ainvoke`` — CrewAI normalizes every tool an agent is given (the ``@tool``
decorator, a ``BaseTool`` subclass, or a bare ``CrewStructuredTool``) into a
``CrewStructuredTool`` via ``BaseTool.to_structured_tool()`` before handing it
to ``ToolUsage`` (``ToolUsage.tools: list[CrewStructuredTool]``); BOTH the
sync executor (``ToolUsage.use`` → ``_use`` → ``tool.invoke(...)``) and the
async executor (``ToolUsage.ause`` → ``_ause`` → ``await tool.ainvoke(...)``)
dispatch exclusively through these two methods, so patching them covers every
tool call regardless of how the tool was authored — below CrewAI's own
retry-the-LLM-call loop (``ToolUsage._run_attempts`` /
``_max_parsing_attempts``), so a Keel-observed failure never burns a model
turn. Unlike the OpenAI Agents SDK's ``on_invoke_tool`` (which, by default,
formats a raised exception into a string result for the LLM before it ever
reaches a wrapper), ``CrewStructuredTool.invoke``/``ainvoke`` do not catch the
wrapped function's exception themselves — ``ToolUsage._use``/``_ause`` own the
first ``try/except`` — so Keel observes the tool's raw error on every call.
Tool calls are NON-idempotent (Level 0 hard rule: wrapped ``idempotent=False``
— observed, never retried, KEEL-E014; no v0.1 opt-in, see ``packs.tool``), and
the original exception re-raises unchanged so CrewAI's own
parsing-retry/error-formatting (``ToolUsage.on_tool_error``) is untouched.

LLM legs: CrewAI's own ``LLM`` class (``crewai.llm.LLM``) routes model calls
through ``litellm`` by default (lazy-imported to dodge its module-level
``dotenv.load_dotenv()``); litellm places the actual request over each
provider's SDK/httpx, so a call to one of the three hosts the transport seam
already maps (``adapters._http.LLM_HOST_PROVIDERS``: OpenAI, Anthropic, Google
Gemini) still resolves to ``llm:<provider>`` and gets Retry-After-aware retry
+ the dev cache "for free" — but litellm fans out to dozens of OTHER
providers/hosts that are NOT in that map, and those calls surface as a plain
host target (or are missed entirely when a provider's own SDK does not ride
httpx/requests at all) — this pack owns no model-call seam and does not
attempt to enumerate litellm's provider hosts.

Detection requires the ``crewai`` distribution, certified against 1.15.2.
CrewAI's internal tool-dispatch machinery has reshaped across releases more
than pydantic-ai's or the OpenAI Agents SDK's (the gap brief flags this
explicitly), so this pack is a strong candidate for tripping into
``best_effort`` on a version bump — ``install``'s shape-check (``invoke``/
``ainvoke`` must both exist and be callable) does nothing unsafe when that
happens, same as its siblings.
"""

from __future__ import annotations

import functools
from typing import Any, Callable

from ..adapters._pack import Detection, Seam, TargetDecl
from . import _framework
from .tool import is_valid_tool_name, wrap_tool

MODULE = "crewai"
NAME = "crewai"
_DISTS = ("crewai",)
#: Versions this pack certifies (prefix match; dev used 1.15.2). Outside the
#: range detect() reports ``best_effort`` — the pack still tries.
_PINNED = ("1",)

_installed = False
_orig: dict[str, Any] = {}
#: tool name -> wrap_tool-wrapped passthrough, one cache per dispatch path (a
#: sync wrapper and an async wrapper are never interchangeable), reused across
#: calls so per-target state like the breaker sees a stable wrapper.
_sync_runners: dict[str, Callable[..., Any]] = {}
_async_runners: dict[str, Callable[..., Any]] = {}
#: Tool names skipped because they cannot appear as a ``tool:<name>`` policy
#: key (frozen target grammar) — e.g. CrewAI's built-in "Delegate work to
#: coworker"/"Ask question to coworker" tools, whose names contain spaces.
#: Surfaced for doctor.
SKIPPED: set[str] = set()


# --- contract operations -----------------------------------------------------

def detect() -> Detection:
    return _framework.detect_framework(MODULE, NAME, _DISTS, _PINNED, require_dist=True)


def seams() -> list[Seam]:
    return [
        Seam(
            patch_point="crewai.tools.structured_tool.CrewStructuredTool.invoke / .ainvoke",
            upstream_api=(
                "CrewAI structured-tool API: CrewStructuredTool.invoke(input, "
                "config=None, **kwargs) -> Any (\"main method for tool "
                "execution\") and .ainvoke(input, config=None, **kwargs) -> "
                "Awaitable[Any]"
            ),
            why_stable=(
                "every tool an agent runs is normalized to a CrewStructuredTool "
                "(BaseTool.to_structured_tool()) before ToolUsage dispatches it, "
                "and both the sync (use/_use) and async (ause/_ause) executors "
                "call exclusively through invoke/ainvoke — the narrowest point "
                "that covers every tool regardless of how it was authored"
            ),
        )
    ]


def targets() -> list[TargetDecl]:
    return [
        TargetDecl(
            pattern="tool:<name>",
            kind="tool",
            idempotency_rule=(
                "every CrewStructuredTool.invoke/ainvoke call maps to "
                "tool:<tool.name>; a framework tool runs arbitrary side-effecting "
                "code, so the pack wraps it idempotent=False — observed, never "
                "retried (KEEL-E014); v0.1 has no opt-in for framework-registered "
                "tools (packs.tool module docs)"
            ),
            args_hash_rule=(
                "None — a non-idempotent tool is never served from cache; names "
                "outside the frozen tool: target grammar (e.g. CrewAI's built-in "
                "\"Delegate work to coworker\") are skipped (passthrough, noted "
                "for doctor) rather than wrapped unroutably"
            ),
        ),
        TargetDecl(
            pattern="llm:<provider>",
            kind="llm",
            idempotency_rule=(
                "crewai.llm.LLM routes model calls through litellm by default; a "
                "call litellm places to a host the transport seam maps "
                "(adapters._http.LLM_HOST_PROVIDERS: OpenAI/Anthropic/Google "
                "Gemini) resolves to llm:<provider> incidentally — this pack owns "
                "no model-call seam and does not cover litellm's other provider "
                "hosts"
            ),
            args_hash_rule=(
                "as the transport seam derives it for a covered host: sha256 over "
                "(method, url, canonicalized JSON body) for LLM POSTs (dev-cache "
                "replay key); an uncovered provider host gets no llm: target at all"
            ),
        ),
    ]


def defaults() -> dict[str, Any]:
    """Empty: tool: targets inherit ``[defaults.outbound]`` (no
    ``[defaults.tool]`` in the frozen pack) and the llm: leg — for the three
    hosts the transport seam knows — already gets ``[defaults.llm]`` from the
    openai/anthropic provider packs."""
    return {}


# --- install / uninstall -----------------------------------------------------

def install() -> None:
    """Patch ``CrewStructuredTool.invoke``/``.ainvoke``. Idempotent; a no-op
    when CrewAI is absent or its seam no longer has the certified shape."""
    global _installed
    if _installed:
        return
    try:
        from crewai.tools.structured_tool import CrewStructuredTool
    except ImportError:
        return
    orig_invoke = getattr(CrewStructuredTool, "invoke", None)
    orig_ainvoke = getattr(CrewStructuredTool, "ainvoke", None)
    if not callable(orig_invoke) or not callable(orig_ainvoke):
        return  # reshaped seam: nothing unsafe

    @functools.wraps(orig_invoke)
    def invoke(self: Any, input: Any, config: Any = None, **kwargs: Any) -> Any:
        name = getattr(self, "name", None)
        if not is_valid_tool_name(name):
            SKIPPED.add(str(name))  # unroutable as a keel.toml key: pass through
            return orig_invoke(self, input, config, **kwargs)
        runner = _sync_runners.get(name)
        if runner is None:
            runner = wrap_tool(name, _sync_passthrough(orig_invoke))
            _sync_runners[name] = runner
        return runner(self, input, config, **kwargs)

    @functools.wraps(orig_ainvoke)
    async def ainvoke(self: Any, input: Any, config: Any = None, **kwargs: Any) -> Any:
        name = getattr(self, "name", None)
        if not is_valid_tool_name(name):
            SKIPPED.add(str(name))  # unroutable as a keel.toml key: pass through
            return await orig_ainvoke(self, input, config, **kwargs)
        runner = _async_runners.get(name)
        if runner is None:
            runner = wrap_tool(name, _async_passthrough(orig_ainvoke))
            _async_runners[name] = runner
        return await runner(self, input, config, **kwargs)

    invoke.__keel_wrapped__ = True  # type: ignore[attr-defined]
    ainvoke.__keel_wrapped__ = True  # type: ignore[attr-defined]
    _orig["invoke"] = orig_invoke
    _orig["ainvoke"] = orig_ainvoke
    CrewStructuredTool.invoke = invoke  # type: ignore[method-assign]
    CrewStructuredTool.ainvoke = ainvoke  # type: ignore[method-assign]
    _installed = True


def uninstall() -> None:
    """Restore the original ``invoke``/``ainvoke`` (patches must be
    reversible)."""
    global _installed
    if not _installed:
        return
    from crewai.tools.structured_tool import CrewStructuredTool

    CrewStructuredTool.invoke = _orig["invoke"]  # type: ignore[method-assign]
    CrewStructuredTool.ainvoke = _orig["ainvoke"]  # type: ignore[method-assign]
    _orig.clear()
    _sync_runners.clear()
    _async_runners.clear()
    SKIPPED.clear()
    _installed = False


def _sync_passthrough(orig: Callable[..., Any]) -> Callable[..., Any]:
    def _invoke(*args: Any, **kwargs: Any) -> Any:
        return orig(*args, **kwargs)

    # A readable `op` for discovery/doctor (wrap_tool uses __qualname__).
    _invoke.__qualname__ = "CrewStructuredTool.invoke"
    return _invoke


def _async_passthrough(orig: Callable[..., Any]) -> Callable[..., Any]:
    async def _invoke(*args: Any, **kwargs: Any) -> Any:
        return await orig(*args, **kwargs)

    _invoke.__qualname__ = "CrewStructuredTool.ainvoke"
    return _invoke


__all__ = [
    "MODULE",
    "NAME",
    "SKIPPED",
    "detect",
    "seams",
    "targets",
    "defaults",
    "install",
    "uninstall",
]
