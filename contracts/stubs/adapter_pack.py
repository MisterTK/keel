"""Adapter-pack contract, Python form — contracts-v1.

See ../adapter-pack.md for semantics. Real packs live in-tree in keel-py;
this Protocol is the frozen shape they implement.
"""

from dataclasses import dataclass, field
from typing import Any, Literal, Protocol


@dataclass(frozen=True)
class Detection:
    matched: bool
    name: str = ""                      # e.g. "httpx", "openai", "google-adk"
    version: str = ""                   # installed version, "" if unknown
    confidence: Literal["pinned", "best_effort"] = "best_effort"


@dataclass(frozen=True)
class Seam:
    patch_point: str                    # e.g. "httpx.HTTPTransport.handle_request"
    upstream_api: str                   # the documented API this relies on
    why_stable: str                     # printed verbatim by `keel doctor`


@dataclass(frozen=True)
class TargetDecl:
    pattern: str                        # target id or pattern, e.g. "llm:openai"
    kind: Literal["host", "function", "llm", "tool", "mcp"]
    idempotency_rule: str               # how `idempotent` is derived at the seam
    args_hash_rule: str                 # how `args_hash` is derived at the seam


class AdapterPack(Protocol):
    """The four operations every pack implements. No retry/backoff/breaker
    logic lives here — all behavior flows through the core."""

    def detect(self) -> Detection: ...

    def seams(self) -> list[Seam]: ...

    def targets(self) -> list[TargetDecl]: ...

    def defaults(self) -> dict[str, Any]:
        """Policy fragment (keel.toml JSON form, per policy.schema.json),
        merged UNDER user configuration."""
        ...


__all__ = ["AdapterPack", "Detection", "Seam", "TargetDecl"]
