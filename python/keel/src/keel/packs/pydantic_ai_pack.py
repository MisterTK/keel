"""The Pydantic AI framework pack (adapter-pack contract).

Seam: ``pydantic_ai.toolsets.FunctionToolset.call_tool`` — the toolset
interface's single tool-dispatch method (``AbstractToolset.call_tool(name,
tool_args, ctx, tool)``, documented for custom toolsets). Every function tool
— ``@agent.tool``, ``@agent.tool_plain``, ``FunctionToolset(tools=[...])``,
``add_function``/``add_tool`` — executes through it, *below* the model-request
loop, so a Keel-observed failure never burns a model turn. Each dispatch is
routed through :func:`keel.packs.tool.wrap_tool` under ``tool:<name>``: a tool
call is NON-idempotent (Level 0 hard rule — the pack wraps framework tools
with ``idempotent=False``, so an error is observed, never retried, KEEL-E014)
and v0.1 deliberately offers no knob to flip that (see ``packs.tool``).
Framework semantics are untouched: the original exception (including
pydantic-ai's own ``ModelRetry``) re-raises unchanged, so the framework's
retry-prompt loop behaves exactly as without Keel.

LLM legs: pydantic-ai model requests ride the provider SDKs (openai /
anthropic / google-genai / …) over httpx, so the transport seam
(the backend's ``resolve_target`` LLM host map, ``docs/targeting.md``)
already maps them to ``llm:<provider>`` — this pack owns no model-call seam
(see ``targets()``).

Certified against pydantic-ai 2.9.0 (``pydantic-ai-slim``); the ``call_tool``
signature is the v1/v2 toolsets contract. ``install`` shape-checks the seam
and does nothing when the framework has reshaped it ("if a call site cannot
be wrapped safely, do nothing and note it").
"""

from __future__ import annotations

import functools
from typing import Any, Callable

from ..adapters._pack import Detection, Seam, TargetDecl
from . import _framework
from .tool import is_valid_tool_name, wrap_tool

MODULE = "pydantic_ai"
NAME = "pydantic-ai"
#: Distribution names carrying the version, in preference order: the slim
#: package ships the actual ``pydantic_ai`` code; ``pydantic-ai`` is the
#: all-extras metapackage that depends on it (they version-lock).
_DISTS = ("pydantic-ai-slim", "pydantic-ai")
#: Versions this pack certifies (prefix match; dev used 2.9.0). Outside the
#: range detect() reports ``best_effort`` — the pack still tries.
_PINNED = ("2",)

_installed = False
_orig: dict[str, Any] = {}
#: tool name -> wrap_tool-wrapped passthrough (one per target, reused across
#: calls so per-target state like the breaker sees a stable wrapper).
_runners: dict[str, Callable[..., Any]] = {}
#: Tool names skipped because they cannot appear as a ``tool:<name>`` policy
#: key (frozen target grammar) — wrapped calls pass through untouched instead
#: of failing mid-run; surfaced for doctor.
SKIPPED: set[str] = set()


# --- contract operations -----------------------------------------------------

def detect() -> Detection:
    return _framework.detect_framework(MODULE, NAME, _DISTS, _PINNED)


def seams() -> list[Seam]:
    return [
        Seam(
            patch_point="pydantic_ai.toolsets.FunctionToolset.call_tool",
            upstream_api=(
                "pydantic-ai toolsets API: AbstractToolset.call_tool(name, "
                "tool_args, ctx, tool) -> Any"
            ),
            why_stable=(
                "call_tool is the abstract toolset interface's single "
                "tool-dispatch method, documented for custom toolsets; "
                "FunctionToolset is the concrete toolset every registered "
                "function tool (@agent.tool / tool_plain / FunctionToolset) "
                "executes through, below the model-request loop."
            ),
        )
    ]


def targets() -> list[TargetDecl]:
    return [
        TargetDecl(
            pattern="tool:<name>",
            kind="tool",
            idempotency_rule=(
                "every FunctionToolset dispatch maps to tool:<registered tool "
                "name>; a framework tool runs arbitrary side-effecting code, so "
                "the pack wraps it idempotent=False — observed, never retried "
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
            pattern="llm:<provider>",
            kind="llm",
            idempotency_rule=(
                "model requests ride the provider SDK (openai/anthropic/"
                "google-genai/…) over httpx; the transport seam maps known "
                "provider hosts (the backend's resolve_target host map, "
                "docs/targeting.md) to llm:<provider> — this pack owns no "
                "model-call seam"
            ),
            args_hash_rule=(
                "as the transport seam derives it: sha256 over (method, url, "
                "canonicalized JSON body) for LLM POSTs (dev-cache replay key); "
                "unknown provider hosts surface as plain host targets"
            ),
        ),
    ]


def defaults() -> dict[str, Any]:
    """Empty: tool: targets inherit ``[defaults.outbound]`` (no
    ``[defaults.tool]`` in the frozen pack) and the llm: legs already get
    ``[defaults.llm]`` from the provider packs / transport seam."""
    return {}


# --- install / uninstall -----------------------------------------------------

def install() -> None:
    """Patch the toolset dispatch seam. Idempotent; a no-op when pydantic-ai
    is absent or its seam no longer has the certified shape."""
    global _installed
    if _installed:
        return
    try:
        from pydantic_ai.toolsets import FunctionToolset
    except ImportError:
        return
    orig = getattr(FunctionToolset, "call_tool", None)
    if orig is None or not callable(orig):
        return  # reshaped seam: nothing unsafe (adapter-pack detect confidence covers it)

    @functools.wraps(orig)
    async def call_tool(self: Any, name: str, tool_args: dict[str, Any], ctx: Any, tool: Any) -> Any:
        if not is_valid_tool_name(name):
            SKIPPED.add(str(name))  # unroutable as a keel.toml key: pass through
            return await orig(self, name, tool_args, ctx, tool)
        runner = _runners.get(name)
        if runner is None:
            runner = wrap_tool(name, _passthrough(orig))
            _runners[name] = runner
        return await runner(self, name, tool_args, ctx, tool)

    call_tool.__keel_wrapped__ = True  # type: ignore[attr-defined]
    _orig["call_tool"] = orig
    FunctionToolset.call_tool = call_tool  # type: ignore[method-assign]
    _installed = True


def uninstall() -> None:
    """Restore the original ``call_tool`` (patches must be reversible)."""
    global _installed
    if not _installed:
        return
    from pydantic_ai.toolsets import FunctionToolset

    FunctionToolset.call_tool = _orig["call_tool"]  # type: ignore[method-assign]
    _orig.clear()
    _runners.clear()
    SKIPPED.clear()
    _installed = False


def _passthrough(orig: Callable[..., Any]) -> Callable[..., Any]:
    async def _invoke(*args: Any, **kwargs: Any) -> Any:
        return await orig(*args, **kwargs)

    # A readable `op` for discovery/doctor (wrap_tool uses __qualname__).
    _invoke.__qualname__ = "FunctionToolset.call_tool"
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
