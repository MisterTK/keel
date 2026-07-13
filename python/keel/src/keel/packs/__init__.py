"""Semantic framework/provider packs (adapter-pack contract).

Unlike the *library* adapters in ``keel.adapters`` (httpx/requests), which own a
monkey-patched seam, the *provider* packs here (``openai_pack``,
``anthropic_pack``, ``google_genai_pack``) are SEMANTIC: their targets are
produced by the transport seams (the ``llm:<provider>`` host map) and they
contribute only (a) a policy-defaults fragment merged UNDER user config and
(b) — for the generic ``llm`` pack — the dev-cache resolution. They carry zero
resilience logic of their own (adapter-pack rule 3).

The *framework* packs (``pydantic_ai_pack``, ``openai_agents_pack``,
``crewai_pack``, ``adk_pack``) DO own a seam — each patches its framework's
tool-execution (and, for ``adk_pack``, model-call) boundary reversibly,
physically wiring it to :func:`tool.wrap_tool` (``adk_pack`` additionally
registers a plugin on every constructed ADK ``Runner``) — so they are
registered for lazy on-import activation alongside the library adapters
(``keel.adapters._framework_packs``, a lazily-imported cross-package
reference: importing them from THIS module's top level is safe, but the
reverse is not — see that function's docstring) rather than folded into
``PROVIDER_PACKS`` below. Their LLM legs need no seam of their own: model
requests ride the provider SDKs over httpx/requests, so the transport seam
already maps them to ``llm:<provider>`` (each pack's ``targets()`` documents
the routing).

``mcp_pack`` is a third kind of exception: it DOES own a seam
(``mcp.client.session.ClientSession.send_request``), but is patched at
bootstrap directly via :func:`install_mcp_pack` (not through ``PACKS`` or
``_framework_packs``) — exactly like the Node front end's ``mcp:`` pack.

The front end folds each PRESENT provider pack's ``defaults()`` fragment into
the policy at bootstrap (``defaults < packs < user``); the generic ``llm`` pack
supplies ``resolve_dev_cache``.

``langgraph_pack`` is the last exception, living in this package: it DOES own
a real monkey-patched seam (``StateGraph.add_node``). Despite that, it is
registered for lazy on-import activation the SAME way as the tool.wrap_tool
-seam packs above — via ``keel.adapters._framework_packs`` — not in
``keel.adapters.PACKS``: importing it eagerly at ``keel.adapters`` module
scope would recreate the exact cross-package init-order cycle
``_framework_packs`` exists to avoid (``keel.packs.mcp_pack`` needs a name off
``keel.adapters`` that isn't defined until later in that module's own
execution). It lives here rather than in ``keel.adapters`` because its OTHER
half — the `KeelSaver` checkpointer — is a framework-pack API surface (like
`tool.wrap_tool`), not a library adapter.
"""

from __future__ import annotations

from typing import Any

from . import (
    adk_pack,
    anthropic_pack,
    crewai_pack,
    google_genai_pack,
    langgraph_pack,
    mcp_pack,
    openai_agents_pack,
    openai_pack,
    pydantic_ai_pack,
)
from .langgraph_pack import KeelSaver
from .llm import DEV_CACHE_TTL, llm_pack, resolve_dev_cache
from .mcp_pack import install_mcp_pack
from .tool import is_valid_tool_name, tool_pack, wrap_tool

#: Registration order = report order (stable, deterministic).
PROVIDER_PACKS = (openai_pack, anthropic_pack, google_genai_pack)

#: Registration order = report order (stable, deterministic). Physical
#: activation is `keel.adapters.install_adapters` (via `_framework_packs`,
#: which arms every pack listed here plus `langgraph_pack` the identical way);
#: this tuple is the discoverability/reporting twin of `PROVIDER_PACKS`.
FRAMEWORK_PACKS = (
    adk_pack,
    crewai_pack,
    langgraph_pack,
    openai_agents_pack,
    pydantic_ai_pack,
)


def present_provider_defaults() -> list[dict[str, Any]]:
    """The ``defaults()`` fragments of every provider pack whose SDK is present
    (importable). Fed to ``_defaults.apply_pack_defaults`` as the ``packs`` merge
    layer. Never imports an absent SDK (``detect`` uses importability only)."""
    return [pack.defaults() for pack in PROVIDER_PACKS if pack.detect().matched]


__all__ = [
    "PROVIDER_PACKS",
    "FRAMEWORK_PACKS",
    "DEV_CACHE_TTL",
    "adk_pack",
    "llm_pack",
    "resolve_dev_cache",
    "openai_pack",
    "anthropic_pack",
    "google_genai_pack",
    "pydantic_ai_pack",
    "openai_agents_pack",
    "crewai_pack",
    "present_provider_defaults",
    "is_valid_tool_name",
    "tool_pack",
    "wrap_tool",
    "langgraph_pack",
    "KeelSaver",
    "mcp_pack",
    "install_mcp_pack",
]
