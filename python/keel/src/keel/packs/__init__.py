"""Semantic framework/provider packs (adapter-pack contract).

Unlike the *library* adapters in ``keel.adapters`` (httpx/requests), which own a
monkey-patched seam, the *provider* packs here (``openai_pack``,
``anthropic_pack``) are SEMANTIC: their targets are produced by the transport
seams (the ``llm:<provider>`` host map) and they contribute only (a) a
policy-defaults fragment merged UNDER user config and (b) — for the generic
``llm`` pack — the dev-cache resolution. They carry zero resilience logic of
their own (adapter-pack rule 3).

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

The front end folds each PRESENT provider pack's ``defaults()`` fragment into
the policy at bootstrap (``defaults < packs < user``); the generic ``llm`` pack
supplies ``resolve_dev_cache``.
"""

from __future__ import annotations

from typing import Any

from . import (
    adk_pack,
    anthropic_pack,
    crewai_pack,
    openai_agents_pack,
    openai_pack,
    pydantic_ai_pack,
)
from .llm import DEV_CACHE_TTL, llm_pack, resolve_dev_cache
from .tool import is_valid_tool_name, tool_pack, wrap_tool

#: Registration order = report order (stable, deterministic).
PROVIDER_PACKS = (openai_pack, anthropic_pack)

#: Registration order = report order (stable, deterministic). Physical
#: activation is `keel.adapters.install_adapters` (via `_framework_packs`);
#: this tuple is the discoverability/reporting twin of `PROVIDER_PACKS`.
FRAMEWORK_PACKS = (adk_pack, crewai_pack, openai_agents_pack, pydantic_ai_pack)


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
    "pydantic_ai_pack",
    "openai_agents_pack",
    "crewai_pack",
    "present_provider_defaults",
    "is_valid_tool_name",
    "tool_pack",
    "wrap_tool",
]
