"""Semantic framework/provider packs (adapter-pack contract).

Unlike the *library* adapters in ``keel.adapters`` (httpx/requests), which own a
monkey-patched seam, the packs here are SEMANTIC: their targets are produced by
the transport seams (the ``llm:<provider>`` host map) or by a framework pack's
wrap site (``tool:<name>`` via ``tool.wrap_tool``) and they contribute only
(a) a policy-defaults fragment merged UNDER user config and (b) — for the
generic ``llm`` pack — the dev-cache resolution. They carry zero resilience
logic of their own (adapter-pack rule 3).

The front end folds each PRESENT provider pack's ``defaults()`` fragment into
the policy at bootstrap (``defaults < packs < user``); the generic ``llm`` pack
supplies ``resolve_dev_cache``.

``langgraph_pack`` is the one exception living in this package: it DOES own a
real monkey-patched seam (``StateGraph.add_node``), so it is also registered
in ``keel.adapters.PACKS`` for lazy install-on-import, exactly like httpx/
requests. It lives here rather than in ``keel.adapters`` because its OTHER
half — the `KeelSaver` checkpointer — is a framework-pack API surface (like
`tool.wrap_tool`), not a library adapter.
"""

from __future__ import annotations

from typing import Any

from . import anthropic_pack, langgraph_pack, openai_pack
from .llm import DEV_CACHE_TTL, llm_pack, resolve_dev_cache
from .langgraph_pack import KeelSaver
from .tool import is_valid_tool_name, tool_pack, wrap_tool

#: Registration order = report order (stable, deterministic).
PROVIDER_PACKS = (openai_pack, anthropic_pack)


def present_provider_defaults() -> list[dict[str, Any]]:
    """The ``defaults()`` fragments of every provider pack whose SDK is present
    (importable). Fed to ``_defaults.apply_pack_defaults`` as the ``packs`` merge
    layer. Never imports an absent SDK (``detect`` uses importability only)."""
    return [pack.defaults() for pack in PROVIDER_PACKS if pack.detect().matched]


__all__ = [
    "PROVIDER_PACKS",
    "DEV_CACHE_TTL",
    "llm_pack",
    "resolve_dev_cache",
    "openai_pack",
    "anthropic_pack",
    "present_provider_defaults",
    "is_valid_tool_name",
    "tool_pack",
    "wrap_tool",
    "langgraph_pack",
    "KeelSaver",
]
