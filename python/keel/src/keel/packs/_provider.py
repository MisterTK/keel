"""Shared building blocks for the provider LLM packs (openai / anthropic).

Each provider pack is a uniform adapter-pack module (``detect``/``seams``/
``targets``/``defaults``) that maps its official SDK to an ``llm:<provider>``
target. The SDKs ride httpx, so the interception itself is Task 10's transport
seam — a provider pack owns NO seam of its own (``seams() == []``). It only
(1) reports whether its SDK is importable (``detect`` — from importability only,
never importing user code) and (2) declares the ``llm:<provider>`` target plus
the generic ``[defaults.llm]`` fragment (merged UNDER user config).

The generic ``[defaults.llm]`` layer already encodes the Retry-After-aware retry
each provider wants (retry on conn/timeout/429/5xx; the core waits
``max(schedule_wait, retry_after)``), so per-provider refinement is the identity
today — a seam for future provider-specific tuning.
"""

from __future__ import annotations

import importlib.metadata
import importlib.util
from typing import Any

from .._defaults import llm_defaults
from ..adapters._pack import Detection, Seam, TargetDecl


def detect_pack(
    module: str, name: str, pinned: tuple[str, ...], *, dist_name: str | None = None
) -> Detection:
    """Present iff ``module`` is importable — decided WITHOUT importing it
    (importability + installed version only, per adapter-pack rule 1).

    ``dist_name`` is the PyPI distribution name to resolve the version from,
    when it differs from the importable module name (e.g. the ``google-genai``
    distribution installs the ``google.genai`` module); it defaults to
    ``module`` for the common case where the two coincide (openai, anthropic).
    """
    if importlib.util.find_spec(module) is None:
        return Detection(matched=False)
    try:
        version = importlib.metadata.version(dist_name or module)
    except importlib.metadata.PackageNotFoundError:
        version = ""
    confidence = "pinned" if _is_pinned(version, pinned) else "best_effort"
    return Detection(matched=True, name=name, version=version, confidence=confidence)


def provider_seams() -> list[Seam]:
    return []  # the SDK rides httpx; the transport seam (Task 10) does the wrapping


def provider_targets(provider: str, host: str) -> list[TargetDecl]:
    return [
        TargetDecl(
            pattern=f"llm:{provider}",
            kind="llm",
            idempotency_rule=(
                f"host {host} maps to llm:{provider}; a POST is observed-not-"
                "retried unless it carries an idempotency key (Level 0 hard rule)"
            ),
            args_hash_rule=(
                "sha256 over (method, url, canonicalized JSON body) for LLM POSTs "
                "(dev-cache replay key); sha256(method + url) for idempotent GET; "
                "None for streaming bodies"
            ),
        )
    ]


def provider_defaults() -> dict[str, Any]:
    """The generic ``[defaults.llm]`` layer, merged UNDER user config. Provider
    refinement is the identity today (see module docstring)."""
    return {"defaults": {"llm": llm_defaults()}}


def _is_pinned(version: str, pinned: tuple[str, ...]) -> bool:
    return any(version == p or version.startswith(p + ".") for p in pinned)


__all__ = ["detect_pack", "provider_seams", "provider_targets", "provider_defaults"]
