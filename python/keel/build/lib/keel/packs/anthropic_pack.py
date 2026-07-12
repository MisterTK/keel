"""The ``llm:anthropic`` provider pack (adapter-pack contract).

The official ``anthropic`` Python SDK rides httpx, so Task 10's transport seam
already intercepts its calls and maps ``api.anthropic.com`` → ``llm:anthropic``
(see ``adapters._http.LLM_HOST_PROVIDERS``). This pack owns no seam of its own;
it declares the ``llm:anthropic`` target and the generic ``[defaults.llm]``
fragment (Retry-After-aware retry + dev cache), merged UNDER user config.
"""

from __future__ import annotations

from typing import Any

from ..adapters._pack import Detection, Seam, TargetDecl
from . import _provider

MODULE = "anthropic"
NAME = "anthropic"
PROVIDER = "anthropic"
HOST = "api.anthropic.com"
#: Versions this pack certifies (prefix match). The anthropic SDK is v0.x.
_PINNED = ("0",)


def detect() -> Detection:
    return _provider.detect_pack(MODULE, NAME, _PINNED)


def seams() -> list[Seam]:
    return _provider.provider_seams()


def targets() -> list[TargetDecl]:
    return _provider.provider_targets(PROVIDER, HOST)


def defaults() -> dict[str, Any]:
    return _provider.provider_defaults()


__all__ = ["MODULE", "NAME", "PROVIDER", "HOST", "detect", "seams", "targets", "defaults"]
