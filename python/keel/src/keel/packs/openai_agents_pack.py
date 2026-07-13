"""The OpenAI Agents SDK framework pack (adapter-pack contract).

Seam: ``agents.FunctionTool.on_invoke_tool`` — the SDK's documented tool-call
dispatch point (the dataclass field the Runner awaits for every function-tool
call: ``on_invoke_tool(tool_context, arguments_json) -> Awaitable[Any]``).
The pack patches ``FunctionTool.__post_init__`` so each tool constructed
after install (the ``@function_tool`` decorator and direct construction both
run it) gets its ``on_invoke_tool`` wrapped per instance with
:func:`keel.packs.tool.wrap_tool` under ``tool:<tool.name>`` — below the LLM
loop, so a Keel-observed failure never burns a model turn. Tool calls are
NON-idempotent (Level 0 hard rule: wrapped ``idempotent=False`` — observed,
never retried, KEEL-E014; no v0.1 opt-in, see ``packs.tool``), and the
original exception re-raises unchanged so the SDK's own failure handling
(``failure_error_function``, guardrails) is untouched.

LLM legs: Runner model calls ride the ``openai`` SDK over httpx, so the
transport seam maps ``api.openai.com`` → ``llm:openai`` (and LiteLLM-routed
models map by their provider host) — this pack owns no model-call seam.

Detection requires the ``openai-agents`` distribution, not just an importable
``agents`` module: that bare name is too generic to claim a match
(``_framework.detect_framework`` ``require_dist``). Certified against
openai-agents 0.18.2; ``on_invoke_tool`` has been the documented FunctionTool
interface since the first release. ``install`` shape-checks and does nothing
when the SDK has reshaped it.
"""

from __future__ import annotations

import dataclasses
import functools
import weakref
from typing import Any, Callable

from .._wrap import WRAPPED_ATTR
from ..adapters._pack import Detection, Seam, TargetDecl
from . import _framework
from .tool import is_valid_tool_name, wrap_tool

MODULE = "agents"
NAME = "openai-agents"
_DISTS = ("openai-agents",)
#: Versions this pack certifies (prefix match; dev used 0.18.2). Outside the
#: range detect() reports ``best_effort`` — the pack still tries.
_PINNED = ("0",)

_installed = False
_orig: dict[str, Any] = {}
#: (weakref to tool, original on_invoke_tool) for every instance we wrapped,
#: so uninstall restores the exact original callable (reversible patches).
#: A list of weakrefs — FunctionTool is an eq-generated dataclass and hence
#: unhashable, so a WeakKeyDictionary cannot hold it.
_wrapped_tools: list[tuple[weakref.ref[Any], Callable[..., Any]]] = []
#: Tool names skipped because they cannot appear as a ``tool:<name>`` policy
#: key (frozen target grammar); surfaced for doctor.
SKIPPED: set[str] = set()


# --- contract operations -----------------------------------------------------

def detect() -> Detection:
    return _framework.detect_framework(MODULE, NAME, _DISTS, _PINNED, require_dist=True)


def seams() -> list[Seam]:
    return [
        Seam(
            patch_point="agents.FunctionTool.__post_init__ (wraps on_invoke_tool per instance)",
            upstream_api=(
                "OpenAI Agents SDK tool API: FunctionTool.on_invoke_tool("
                "tool_context, arguments_json) -> Awaitable[Any]"
            ),
            why_stable=(
                "on_invoke_tool is the documented FunctionTool field the "
                "Runner awaits for every function-tool call; wrapping at "
                "__post_init__ covers @function_tool and direct construction "
                "alike, below the LLM loop."
            ),
        )
    ]


def targets() -> list[TargetDecl]:
    return [
        TargetDecl(
            pattern="tool:<name>",
            kind="tool",
            idempotency_rule=(
                "every FunctionTool invocation maps to tool:<tool.name>; a "
                "framework tool runs arbitrary side-effecting code, so the pack "
                "wraps it idempotent=False — observed, never retried "
                "(KEEL-E014); v0.1 has no opt-in for framework-registered tools "
                "(packs.tool module docs)"
            ),
            args_hash_rule=(
                "None — a non-idempotent tool is never served from cache; names "
                "outside the frozen tool: target grammar are skipped (passthrough, "
                "noted for doctor) rather than wrapped unroutably"
            ),
        ),
        TargetDecl(
            pattern="llm:openai",
            kind="llm",
            idempotency_rule=(
                "Runner model calls ride the openai SDK over httpx; the "
                "transport seam maps api.openai.com → llm:openai (LiteLLM-routed "
                "models map by their provider host) — this pack owns no "
                "model-call seam"
            ),
            args_hash_rule=(
                "as the transport seam derives it: sha256 over (method, url, "
                "canonicalized JSON body) for LLM POSTs (dev-cache replay key)"
            ),
        ),
    ]


def defaults() -> dict[str, Any]:
    """Empty: tool: targets inherit ``[defaults.outbound]`` and the llm: leg
    already gets ``[defaults.llm]`` via the openai provider pack."""
    return {}


# --- install / uninstall -----------------------------------------------------

def install() -> None:
    """Patch ``FunctionTool.__post_init__`` to wrap each new tool's
    ``on_invoke_tool``. Idempotent; a no-op when the SDK is absent or the
    seam no longer has the certified shape."""
    global _installed
    if _installed:
        return
    try:
        import agents
    except ImportError:
        return
    function_tool_cls = getattr(agents, "FunctionTool", None)
    if (
        function_tool_cls is None
        or not dataclasses.is_dataclass(function_tool_cls)
        or not hasattr(function_tool_cls, "__post_init__")
    ):
        return  # reshaped seam: nothing unsafe

    orig_post_init = function_tool_cls.__post_init__

    @functools.wraps(orig_post_init)
    def __post_init__(self: Any) -> None:
        orig_post_init(self)
        _wrap_instance(self)

    __post_init__.__keel_wrapped__ = True  # type: ignore[attr-defined]
    _orig["post_init"] = orig_post_init
    function_tool_cls.__post_init__ = __post_init__
    _installed = True


def uninstall() -> None:
    """Restore ``__post_init__`` and every instance's original
    ``on_invoke_tool`` (patches must be reversible)."""
    global _installed
    if not _installed:
        return
    import agents

    agents.FunctionTool.__post_init__ = _orig["post_init"]
    for ref, original in _wrapped_tools:
        tool = ref()
        if tool is not None:
            tool.on_invoke_tool = original
    _wrapped_tools.clear()
    _orig.clear()
    SKIPPED.clear()
    _installed = False


def _wrap_instance(tool: Any) -> None:
    invoke = getattr(tool, "on_invoke_tool", None)
    if invoke is None or not callable(invoke):
        return  # nothing safe to wrap
    if getattr(invoke, WRAPPED_ATTR, False):
        return  # already wrapped (dataclasses.replace / copy re-runs __post_init__)
    name = getattr(tool, "name", None)
    if not is_valid_tool_name(name):
        SKIPPED.add(str(name))  # unroutable as a keel.toml key: pass through
        return

    async def _invoke(*args: Any, **kwargs: Any) -> Any:
        # `await` uniformly: on_invoke_tool may be an async function or a plain
        # callable returning an awaitable (the field's documented type).
        return await invoke(*args, **kwargs)

    _invoke.__qualname__ = "FunctionTool.on_invoke_tool"
    tool.on_invoke_tool = wrap_tool(name, _invoke)
    _wrapped_tools.append((weakref.ref(tool), invoke))


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
