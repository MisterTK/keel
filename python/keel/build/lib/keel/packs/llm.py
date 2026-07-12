"""The generic ``llm:`` provider defaults pack + dev-cache resolution.

The Python twin of ``node/keel/src/packs/llm.mjs`` (Task 11 items 1–2): the
SAME defaults fragment, the SAME merge semantics (see ``_defaults.apply_pack_defaults``),
and the SAME dev-cache resolution. Both front ends must agree, so the rules
here are the cross-language contract — change them in lockstep.

The pack carries ZERO resilience logic (adapter-pack rule 3): it only
(a) declares the generic ``[defaults.llm]`` fragment (merged UNDER user config)
and (b) resolves the dev-mode response cache into a concrete cache directive the
core understands. All retry/backoff/breaker/cache behavior runs in the core.

Dev-cache resolution (DX spec §4.1; defaults.toml ``cache = { mode = "dev" }``):
  * off-prod (``KEEL_ENV != "prod"``) the dev cache is a dev-loop response
    cache — an identical prompt replays from cache during development (fast
    iteration, ~0 API spend). We resolve ``mode = "dev"`` to a concrete
    session-length ttl (``DEV_CACHE_TTL``); the cache key is the call's
    ``args_hash`` (target + canonicalized request, derived at the seam), and the
    replay is served from the persistent journal cache.
  * in prod the cache layer is dropped entirely (production never serves stale
    replays).

The ttl value is an in-process implementation detail — the CONTRACT is the
semantics (active off-prod, inert in prod, keyed by target + args_hash), which
is what the Node twin's counters also assert.
"""

from __future__ import annotations

import os
from copy import deepcopy
from typing import Any, Mapping

from .._defaults import llm_defaults
from ..adapters._pack import Detection, Seam, TargetDecl

#: Dev-loop cache lifetime. The value is not itself a parity contract (the
#: semantics are); it only needs to outlast a dev session. Mirrors Node's
#: ``DEV_CACHE_TTL``.
DEV_CACHE_TTL = "24h"


def _is_prod(env: Mapping[str, str] | None) -> bool:
    env = env if env is not None else os.environ
    return str(env.get("KEEL_ENV", "")).strip().lower() == "prod"


class _LlmPack:
    """The ``llm:`` adapter pack — the four uniform operations (adapter-pack.md).

    A semantic pack, not a library shim: it applies whenever an intercepted call
    resolves to an ``llm:<provider>`` target (the httpx/requests host map in
    ``adapters._http``). It always "matches" — there is no external library
    version to pin — so confidence is ``pinned``.
    """

    def detect(self) -> Detection:
        return Detection(matched=True, name="llm", confidence="pinned")

    def seams(self) -> list[Seam]:
        # No seam of its own: ``llm:`` targets are produced by the HTTP transport
        # seams (host map), not by a pack-owned patch point.
        return []

    def targets(self) -> list[TargetDecl]:
        return [
            TargetDecl(
                pattern="llm:<provider>",
                kind="llm",
                idempotency_rule=(
                    "an LLM call inherits HTTP idempotency at the transport seam: "
                    "a POST is observed-not-retried unless it carries an "
                    "idempotency key (Level 0 hard rule). The dev-cache args_hash "
                    "is orthogonal to retry — it enables replay, not retry."
                ),
                args_hash_rule=(
                    "sha256 over (method, url, canonicalized JSON body) for LLM "
                    "POSTs (dev-cache replay key); sha256(method + url) for "
                    "idempotent GET; None for unbuffered/streaming bodies"
                ),
            )
        ]

    def defaults(self) -> dict[str, Any]:
        """Policy fragment merged UNDER user config: the generic
        ``[defaults.llm]`` layer."""
        return {"defaults": {"llm": llm_defaults()}}


#: The generic ``llm:`` pack singleton (mirrors Node's ``llmPack``).
llm_pack = _LlmPack()


def resolve_dev_cache(
    policy: dict[str, Any],
    env: Mapping[str, str] | None = None,
    *,
    persistent: bool = False,
) -> dict[str, Any]:
    """Resolve dev-mode caches into concrete cache directives the core
    understands, honoring ``KEEL_ENV``. Walks every ``cache`` layer that could
    apply (``defaults.llm``, ``defaults.outbound``, and each ``[target."…"]``):

      * ``cache = { mode = "dev" }``  → off-prod: ``cache = { ttl = "<DEV_CACHE_TTL>" }``
                                        (an explicit user ttl is preserved), plus
                                        ``scope = "persistent"`` when ``persistent``
                                        is set (native + journal) so identical
                                        prompts replay across RUNS, not just within
                                        one process;
                                      → prod:    the cache layer is removed (inert).
      * any other cache layer is left exactly as-is.

    Returns a NEW policy; the input is never mutated. Mirrors Node's
    ``resolveDevCache``.
    """
    prod = _is_prod(env)
    out = deepcopy(policy) if isinstance(policy, dict) else {}

    def resolve_on(owner: Any) -> None:
        if not isinstance(owner, dict):
            return
        cache = owner.get("cache")
        if not isinstance(cache, dict) or cache.get("mode") != "dev":
            return
        if prod:
            owner.pop("cache", None)  # dev cache is inert in prod
            return
        nxt = {k: v for k, v in cache.items() if k != "mode"}
        nxt.setdefault("ttl", DEV_CACHE_TTL)
        if persistent and "scope" not in nxt:
            nxt["scope"] = "persistent"  # cross-run replay under native + journal
        owner["cache"] = nxt

    defaults = out.get("defaults")
    if isinstance(defaults, dict):
        resolve_on(defaults.get("llm"))
        resolve_on(defaults.get("outbound"))
    targets = out.get("target")
    if isinstance(targets, dict):
        for t in targets.values():
            resolve_on(t)
    return out


__all__ = ["DEV_CACHE_TTL", "llm_pack", "resolve_dev_cache"]
