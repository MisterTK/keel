"""Level 0 embedded smart-defaults pack (DX spec §1) + the policy-merge helper.

The policy applied when no keel.toml is present. It MIRRORS
contracts/defaults.toml verbatim (that file is the frozen source of truth);
we embed rather than read it so the installed package is self-contained
(stdlib-only, works offline). Drift against the contract is caught by the
parity test, which parses contracts/defaults.toml with `tomllib` and asserts
deep equality when the repo file is present.

`apply_pack_defaults` layers these defaults (and any provider-pack fragments)
UNDER the user's keel.toml — the Python twin of `applyPackDefaults` in
node/keel/src/defaults.mjs; with no pack fragments it is identical to the Node
twin, and both front ends must agree (merge parity is a cross-language
contract).

The Level 0 hard rules — never change success-path semantics; never retry
non-idempotent calls; do nothing if a call can't be wrapped safely — are
BEHAVIOR, not config, and are enforced in the front end / backend, not here.
"""

from __future__ import annotations

from copy import deepcopy
from typing import Any, Iterable


def outbound_defaults() -> dict[str, Any]:
    """`[defaults.outbound]` — any intercepted network call."""
    return {
        "timeout": "30s",
        "retry": {
            "attempts": 3,
            "schedule": "exp(200ms, x2, max 30s, jitter)",
            "on": ["conn", "timeout", "429", "5xx"],
        },
        "breaker": {"failures": 5, "cooldown": "15s"},
    }


def llm_defaults() -> dict[str, Any]:
    """`[defaults.llm]` — any `llm:*` target; the generic LLM pack layer.

    Retry-After-aware retry (retry on conn/timeout/429/5xx; the core waits
    `max(schedule_wait, retry_after)`) plus the dev-loop response cache
    (`cache = { mode = "dev" }`, resolved to a concrete ttl off-prod and
    dropped in prod by `keel.packs.llm.resolve_dev_cache`).
    """
    return {
        "timeout": "120s",
        "retry": {
            "attempts": 6,
            "schedule": "exp(500ms, x2, max 60s, jitter)",
            "on": ["conn", "timeout", "429", "5xx"],
        },
        "breaker": {"failures": 5, "cooldown": "30s"},
        "cache": {"mode": "dev"},
    }


def level0_defaults() -> dict[str, Any]:
    """The embedded Level 0 policy, as the dict the backend's `configure`
    expects (identical shape to keel.toml parsed to JSON)."""
    return {"defaults": {"outbound": outbound_defaults(), "llm": llm_defaults()}}


def _table(v: Any) -> dict[str, Any]:
    return v if isinstance(v, dict) else {}


def apply_pack_defaults(
    policy: dict[str, Any],
    pack_fragments: Iterable[dict[str, Any]] = (),
) -> dict[str, Any]:
    """Merge Level 0 defaults, then provider-pack fragments, then the user
    policy — precedence ``defaults < packs < user`` — filling in any
    ``defaults.outbound`` / ``defaults.llm`` key a higher layer did not set,
    while a higher layer replaces a key it DOES set wholesale (the
    per-layer-wholesale semantics of the defaults.toml header, matching the
    engine's own per-key layer resolution).

    Target tables are left untouched — the engine resolves target →
    defaults.llm → defaults.outbound precedence per key at execute time.

    Provider packs (``keel.packs.openai_pack`` / ``anthropic_pack``) currently
    emit the generic ``[defaults.llm]`` layer, so folding their fragments is the
    identity over the embedded defaults; the parameter is the seam by which a
    future per-provider refinement would take effect.

    Returns a NEW policy; the input is never mutated. Idempotent on
    ``level0_defaults()``. Mirrors Node's ``applyPackDefaults`` (empty
    ``pack_fragments`` → identical result).
    """
    out = deepcopy(policy) if isinstance(policy, dict) else {}
    user_defaults = _table(out.get("defaults"))

    pack_outbound: dict[str, Any] = {}
    pack_llm: dict[str, Any] = {}
    for frag in pack_fragments:
        frag_defaults = _table(frag.get("defaults")) if isinstance(frag, dict) else {}
        pack_outbound.update(_table(frag_defaults.get("outbound")))
        pack_llm.update(_table(frag_defaults.get("llm")))

    out["defaults"] = {
        **user_defaults,
        "outbound": {
            **outbound_defaults(),
            **pack_outbound,
            **_table(user_defaults.get("outbound")),
        },
        "llm": {
            **llm_defaults(),
            **pack_llm,
            **_table(user_defaults.get("llm")),
        },
    }
    return out
