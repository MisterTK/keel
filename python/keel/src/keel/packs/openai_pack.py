"""The ``llm:openai`` provider pack (adapter-pack contract).

The official ``openai`` Python SDK rides httpx, so Task 10's transport seam
already intercepts its calls and maps ``api.openai.com`` → ``llm:openai``
(see the backend's ``resolve_target`` LLM host map, ``docs/targeting.md``).
This pack owns no seam of its own;
it declares the ``llm:openai`` target and the generic ``[defaults.llm]``
fragment (Retry-After-aware retry + dev cache), merged UNDER user config.
"""

from __future__ import annotations

from typing import Any

from ..adapters._pack import Detection, Seam, TargetDecl
from . import _provider

MODULE = "openai"
NAME = "openai"
PROVIDER = "openai"
HOST = "api.openai.com"
#: Versions this pack certifies (prefix match). The openai SDK is v1.x.
_PINNED = ("1",)


def detect() -> Detection:
    return _provider.detect_pack(MODULE, NAME, _PINNED)


def seams() -> list[Seam]:
    return _provider.provider_seams()


def targets() -> list[TargetDecl]:
    return _provider.provider_targets(PROVIDER, HOST)


def defaults() -> dict[str, Any]:
    return _provider.provider_defaults()


__all__ = ["MODULE", "NAME", "PROVIDER", "HOST", "detect", "seams", "targets", "defaults"]
