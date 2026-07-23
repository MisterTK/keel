"""The ``llm:google-genai`` provider pack (adapter-pack contract).

The official ``google-genai`` Python SDK (``pip install google-genai``, import
name ``google.genai``) rides httpx for both its sync and async request paths,
so Task 10's transport seam already intercepts its calls — the backend's
``resolve_target`` LLM host map (``docs/targeting.md``) maps both the Gemini
Developer API (``generativelanguage.googleapis.com``) and Vertex AI's global
endpoint (``aiplatform.googleapis.com``, plus any REGIONAL ``<location>-
aiplatform.googleapis.com`` endpoint via its suffix rule) to
``llm:google-genai``. Like the openai/anthropic packs, this pack owns no seam
of its own; it declares the ``llm:google-genai`` target for both host
families and the generic ``[defaults.llm]`` fragment (Retry-After-aware retry
— 429/RESOURCE_EXHAUSTED is a plain HTTP 429 on both APIs, already in
``defaults.llm.retry.on`` — plus the dev-cache args_hash exception), merged
UNDER user config, exactly as the other provider packs do.

The importable module (``google.genai``) and the PyPI distribution name that
carries the version (``google-genai``) differ, so detection uses
``_provider.detect_pack``'s ``dist_name`` override rather than the single-name
shortcut the openai/anthropic packs use.
"""

from __future__ import annotations

from typing import Any

from ..adapters._pack import Detection, Seam, TargetDecl
from . import _provider

MODULE = "google.genai"
NAME = "google-genai"
PROVIDER = "google-genai"
DIST = "google-genai"
#: The Gemini Developer API host; Vertex AI (global + regional) is declared
#: separately below since it is a distinct, documented host family.
HOST = "generativelanguage.googleapis.com"
#: Vertex AI's global endpoint (regional `<location>-aiplatform.googleapis.com`
#: endpoints are matched by `adapters._http`'s suffix rule, not listed here).
VERTEX_HOST = "aiplatform.googleapis.com"
#: Versions this pack certifies (prefix match). The google-genai SDK is v1.x.
_PINNED = ("1",)


def detect() -> Detection:
    return _provider.detect_pack(MODULE, NAME, _PINNED, dist_name=DIST)


def seams() -> list[Seam]:
    return _provider.provider_seams()


def targets() -> list[TargetDecl]:
    # Two TargetDecls (one per host family) documenting both API surfaces the
    # host map routes to this provider; both resolve the identical
    # llm:google-genai policy target.
    return [
        *_provider.provider_targets(PROVIDER, HOST),
        *_provider.provider_targets(PROVIDER, VERTEX_HOST),
    ]


def defaults() -> dict[str, Any]:
    return _provider.provider_defaults()


__all__ = [
    "MODULE",
    "NAME",
    "PROVIDER",
    "HOST",
    "VERTEX_HOST",
    "detect",
    "seams",
    "targets",
    "defaults",
]
