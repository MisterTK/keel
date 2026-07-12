"""The adapter-pack contract shape, re-declared for the runtime.

This mirrors ``contracts/stubs/adapter_pack.py`` (contracts-v1) exactly — the
frozen Protocol every pack implements. It is re-declared here rather than
imported so the installed ``keel`` package stays self-contained and
stdlib-only (the contracts/ tree is not a runtime dependency). A parity test
asserts these dataclasses match the frozen contract field-for-field.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Literal, Protocol


@dataclass(frozen=True)
class Detection:
    matched: bool
    name: str = ""
    version: str = ""
    confidence: Literal["pinned", "best_effort"] = "best_effort"


@dataclass(frozen=True)
class Seam:
    patch_point: str
    upstream_api: str
    why_stable: str


@dataclass(frozen=True)
class TargetDecl:
    pattern: str
    kind: Literal["host", "function", "llm", "tool", "mcp"]
    idempotency_rule: str
    args_hash_rule: str


class AdapterPack(Protocol):
    """The four contract operations every pack implements. No
    retry/backoff/breaker logic lives in a pack — all behavior flows through
    the core. (Physical patching is a separate ``install``/``uninstall`` pair.)"""

    def detect(self) -> Detection: ...

    def seams(self) -> list[Seam]: ...

    def targets(self) -> list[TargetDecl]: ...

    def defaults(self) -> dict[str, Any]: ...


__all__ = ["AdapterPack", "Detection", "Seam", "TargetDecl"]
