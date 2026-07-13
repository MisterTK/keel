"""Semantic framework/provider packs (adapter-pack contract).

Unlike the *library* adapters in ``keel.adapters`` (httpx/requests), which own a
monkey-patched seam, most packs here are SEMANTIC: their targets are produced by
the transport seams (the ``llm:<provider>`` host map) or by a framework pack's
wrap site (``tool:<name>`` via ``tool.wrap_tool``) and they contribute only
(a) a policy-defaults fragment merged UNDER user config and (b) — for the
generic ``llm`` pack — the dev-cache resolution. They carry zero resilience
logic of their own (adapter-pack rule 3).

``adk_pack`` is the one exception with a real seam of its own (it registers a
plugin on every constructed ADK ``Runner``) — it lives here rather than under
``keel.adapters`` because, like ``tool``, it is a FRAMEWORK pack (adapter-pack
contract), not a transport library; ``keel.adapters.install_adapters(extra=...)``
arms it lazily through the same mechanism as httpx/requests (see ``bootstrap.
install_keel``).

The front end folds each PRESENT provider pack's ``defaults()`` fragment into
the policy at bootstrap (``defaults < packs < user``); the generic ``llm`` pack
supplies ``resolve_dev_cache``.
"""

from __future__ import annotations

from typing import Any

from . import adk_pack, anthropic_pack, openai_pack
from .llm import DEV_CACHE_TTL, llm_pack, resolve_dev_cache
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
    "adk_pack",
    "llm_pack",
    "resolve_dev_cache",
    "openai_pack",
    "anthropic_pack",
    "present_provider_defaults",
    "is_valid_tool_name",
    "tool_pack",
    "wrap_tool",
]
