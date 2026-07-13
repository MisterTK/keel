"""Semantic framework/provider packs (adapter-pack contract).

Unlike the *library* adapters in ``keel.adapters`` (httpx/requests), which own a
monkey-patched seam, most packs here are SEMANTIC: their targets are produced by
the transport seams (the ``llm:<provider>`` host map) or by a framework pack's
wrap site (``tool:<name>`` via ``tool.wrap_tool``) and they contribute only
(a) a policy-defaults fragment merged UNDER user config and (b) — for the
generic ``llm`` pack — the dev-cache resolution. They carry zero resilience
logic of their own (adapter-pack rule 3). ``mcp_pack`` is the one exception: it
DOES own a seam (``mcp.client.session.ClientSession.send_request``), patched at
bootstrap via :func:`install_mcp_pack`, exactly like the Node front end's
``mcp:`` pack.

The front end folds each PRESENT provider pack's ``defaults()`` fragment into
the policy at bootstrap (``defaults < packs < user``); the generic ``llm`` pack
supplies ``resolve_dev_cache``.
"""

from __future__ import annotations

from typing import Any

from . import anthropic_pack, google_genai_pack, mcp_pack, openai_pack
from .llm import DEV_CACHE_TTL, llm_pack, resolve_dev_cache
from .mcp_pack import install_mcp_pack
from .tool import is_valid_tool_name, tool_pack, wrap_tool

#: Registration order = report order (stable, deterministic).
PROVIDER_PACKS = (openai_pack, anthropic_pack, google_genai_pack)


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
    "google_genai_pack",
    "present_provider_defaults",
    "is_valid_tool_name",
    "tool_pack",
    "wrap_tool",
    "mcp_pack",
    "install_mcp_pack",
]
