"""Shared building blocks for the agent-framework packs (pydantic-ai /
openai-agents / crewai).

A framework pack differs from a *provider* pack (``_provider``) in two ways:

* it OWNS a seam — the framework's tool-execution boundary, patched reversibly
  via the lazy adapter finder (``keel.adapters``) so retries happen *below*
  the LLM loop (dx-spec §4.1: a failed tool call is retried without burning
  tokens on a new model turn); the wrapping itself is :func:`keel.packs.tool.
  wrap_tool`, so all resilience behavior flows through the core;
* its module name and its PyPI distribution name may differ (the OpenAI
  Agents SDK installs ``openai-agents`` but imports as ``agents``), so
  detection takes both and can *require* the distribution when the module
  name alone is too generic to claim a match.

The framework's LLM legs need no seam here: model requests ride the provider
SDKs (openai / anthropic / google) or litellm over httpx/requests, so the
transport seam (the backend's ``resolve_target`` LLM host map —
``docs/targeting.md``) already maps them to ``llm:<provider>`` —
Retry-After-aware retry and the dev cache apply without this pack doing
anything. Each pack's ``targets()`` documents that routing.
"""

from __future__ import annotations

import importlib.metadata
import importlib.util

from ..adapters._pack import Detection


def detect_framework(
    module: str,
    name: str,
    dists: tuple[str, ...],
    pinned: tuple[str, ...],
    *,
    require_dist: bool = False,
) -> Detection:
    """Present iff ``module`` is importable — decided WITHOUT importing it
    (importability + installed-distribution metadata only, adapter-pack
    rule 1). ``dists`` are the distribution names to read the version from,
    in preference order (e.g. ``pydantic-ai-slim`` before the ``pydantic-ai``
    metapackage). With ``require_dist`` the pack refuses to match when none
    of them is installed: a bare module named ``agents`` is not evidence of
    the OpenAI Agents SDK, and misdetecting would arm a patch for the wrong
    library."""
    if importlib.util.find_spec(module) is None:
        return Detection(matched=False)
    version = ""
    for dist in dists:
        try:
            version = importlib.metadata.version(dist)
            break
        except importlib.metadata.PackageNotFoundError:
            continue
    if not version and require_dist:
        return Detection(matched=False)
    confidence = "pinned" if _is_pinned(version, pinned) else "best_effort"
    return Detection(matched=True, name=name, version=version, confidence=confidence)


def _is_pinned(version: str, pinned: tuple[str, ...]) -> bool:
    return any(version == p or version.startswith(p + ".") for p in pinned)


__all__ = ["detect_framework"]
