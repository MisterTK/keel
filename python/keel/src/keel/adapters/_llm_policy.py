"""LLM budget caps + model fallback chains (DX spec §4.1) — FRONT-END
enforcement of the two frozen ``llm:`` target policy knobs
(``contracts/policy.schema.json`` ``$defs/budget`` / ``targetPolicy.fallback``):

    budget   = "$5/run"                              -> per-run spend cap
    fallback = ["gpt-4o-mini", "claude-haiku-4.5"]    -> model fallback chain

DESIGN (decided, not re-litigated here): both knobs are enforced entirely at
the front-end ``llm:`` seams (this module + ``httpx_pack.py`` +
``requests_pack.py``), NOT in the core/FFI/stub. The core has no concept of
"budget" or "model" — it executes an opaque effect and applies
retry/breaker/cache/rate to typed AttemptResults. Reusing KEEL-E012
(breaker-open) for a budget trip is a front-end CHOICE (dx-spec: ``budget =
"$5/run" -> breaker trips``), not a new core error path: this module
synthesizes an outcome shaped exactly like the core/stub's own breaker-open
outcome, so ``keel status``/``.keel/discovery.db`` accounting
(``breaker_opens``, ``failures``) picks up a budget block for free, with zero
crates/ changes.

Cross-language parity note: this is the Python twin of
``node/keel/src/llm-policy.mjs``. Both MUST stay in lockstep — same price
table, same usage-normalization key order, same fallback trigger rule, same
v0.1 rewrite limitation (below).

---------------------------------------------------------------------------
Per-run spend accounting
---------------------------------------------------------------------------
"Per run" = this process's lifetime (Tier 1 has no run-id concept; matches the
dev-cache's session-scoped philosophy). The ledger is a plain in-memory dict,
reset only by ``reset_llm_budgets()`` (tests) or process exit. A budget cap is
READ from the effective policy at every call (``resolve_layer(target,
"budget")``), so changing ``keel.toml`` and reconfiguring changes the cap
live; spend already recorded this run is never retroactively erased.

Usage is read from the provider's OWN response — a DELIBERATE, narrowly
scoped exception to the adapter packs' "never read the response body" rule,
gated strictly on the target actually carrying a ``budget`` (an operator who
never sets ``budget`` gets byte-transparent bodies exactly as before). Prices
are an ESTIMATE (a maintained table below) — provider pricing drifts; this is
not a billing-accurate ledger, it is a resilience guardrail.

---------------------------------------------------------------------------
Fallback re-dispatch
---------------------------------------------------------------------------
Fallback triggers on any TERMINAL call failure for the target EXCEPT
breaker-open (KEEL-E012) — which also covers our own budget synthesis above,
matching dx-spec's "NOT on budget exhaustion": chasing a fallback into an open
breaker (or an exhausted budget) would defeat the point of failing fast. It
does NOT trigger on success, and it does not trigger on a cache hit (nothing
failed).

v0.1 LIMITATION (honest, documented, not silently pretended away): model
rewriting at the generic httpx/requests transport seam can only change the
MODEL on the SAME host/endpoint the original request already targeted — it
rewrites either a JSON body's top-level ``model`` field (OpenAI/Anthropic
chat/messages shape) or a Google ``.../models/<model>:generate...`` URL path
segment. It CANNOT construct a request for a genuinely different provider
(different auth headers, different endpoint, different request/response
shape). A ``fallback`` chain that names a model from a different provider than
the one that failed is sent to the SAME provider with that (unrecognized)
model name, which the provider will typically reject with its own 4xx — a
safe, honest failure, not silent data loss, but not the cross-provider magic
the dx-spec's ``fallback = ["gemini-2.5-pro", "claude-sonnet-4.5"]`` example
suggests either. True cross-provider fallback needs a seam that already knows
how to build a request per provider (Node's ``packs/ai-sdk.mjs``
``keelMiddleware({ models })`` supports that; Python has no equivalent
provider-agnostic SDK middleware seam in v0.1).
"""

from __future__ import annotations

import json
import re
from typing import Any, Mapping
from urllib.parse import quote

# --- budget: parsing, pricing, ledger ----------------------------------------

_BUDGET_RE = re.compile(r"^\$([0-9]+(?:\.[0-9]+)?)/run$")


def parse_budget_cents(spec: Any) -> int | None:
    """Parse the frozen ``budget`` grammar (``^\\$[0-9]+(\\.[0-9]+)?/run$``) to a
    cap in CENTS, or ``None`` when absent/malformed (never raises — an
    unparseable budget string is a policy-validation concern, not this
    module's)."""
    if not isinstance(spec, str):
        return None
    m = _BUDGET_RE.match(spec.strip())
    if not m:
        return None
    try:
        dollars = float(m.group(1))
    except ValueError:
        return None
    return round(dollars * 100)


#: USD per 1,000,000 tokens, by model-name PREFIX (longest-prefix-wins so e.g.
#: "gpt-4o-mini" doesn't fall through to a shorter "gpt-4o" entry). THIS TABLE
#: DRIFTS — provider pricing changes independently of Keel releases; treat it
#: as a maintained ESTIMATE for budget enforcement, not a billing source of
#: truth. Update alongside the Node twin (``llm-policy.mjs``) when it goes
#: stale.
PRICE_TABLE_USD_PER_MILLION: Mapping[str, Mapping[str, float]] = {
    "gpt-4o-mini": {"input": 0.15, "output": 0.6},
    "gpt-4o": {"input": 2.5, "output": 10},
    "gpt-4.1-mini": {"input": 0.4, "output": 1.6},
    "gpt-4.1": {"input": 2, "output": 8},
    "gpt-5-mini": {"input": 0.25, "output": 2},
    "gpt-5": {"input": 1.25, "output": 10},
    "claude-haiku-4.5": {"input": 1, "output": 5},
    "claude-sonnet-4.5": {"input": 3, "output": 15},
    "claude-opus-4.5": {"input": 15, "output": 75},
    "gemini-2.5-flash": {"input": 0.3, "output": 2.5},
    "gemini-2.5-pro": {"input": 1.25, "output": 10},
}

#: Used for any model not in the table above, so budget enforcement degrades
#: gracefully instead of silently never counting an unrecognized model.
#: Deliberately conservative (high) — an unknown model trips the cap sooner
#: rather than risking silent overspend.
DEFAULT_PRICE_USD_PER_MILLION: Mapping[str, float] = {"input": 10, "output": 30}


def _price_for(model: str | None) -> Mapping[str, float]:
    name = str(model or "").lower()
    best: tuple[str, Mapping[str, float]] | None = None
    for prefix, price in PRICE_TABLE_USD_PER_MILLION.items():
        if name.startswith(prefix) and (best is None or len(prefix) > len(best[0])):
            best = (prefix, price)
    return best[1] if best is not None else DEFAULT_PRICE_USD_PER_MILLION


def estimate_cost_usd(model: str | None, usage: Mapping[str, Any] | None) -> float:
    """Estimated USD cost of one call's usage, given the model name (may be
    ``None``/unknown — falls back to ``DEFAULT_PRICE_USD_PER_MILLION``)."""
    if not usage:
        return 0.0
    price = _price_for(model)
    in_cost = (usage.get("input_tokens", 0) or 0) / 1_000_000 * price["input"]
    out_cost = (usage.get("output_tokens", 0) or 0) / 1_000_000 * price["output"]
    return in_cost + out_cost


def normalize_usage(obj: Any) -> dict[str, int] | None:
    """Normalize a provider response body into ``{"input_tokens", "output_tokens"}``,
    or ``None`` when no recognizable usage is present. Handles, in priority
    order:
      * OpenAI chat/completions: ``{"usage": {"prompt_tokens", "completion_tokens"}}``
      * Anthropic messages:      ``{"usage": {"input_tokens", "output_tokens"}}``
      * Google generateContent:  ``{"usageMetadata": {"promptTokenCount", "candidatesTokenCount"}}``
    """
    if not isinstance(obj, dict):
        return None
    u = obj.get("usage") if isinstance(obj.get("usage"), dict) else obj.get("usageMetadata")
    if not isinstance(u, dict):
        return None
    input_tokens = u.get("input_tokens", u.get("prompt_tokens", u.get("promptTokenCount")))
    output_tokens = u.get("output_tokens", u.get("completion_tokens", u.get("candidatesTokenCount")))
    if input_tokens is None and output_tokens is None:
        return None
    return {"input_tokens": int(input_tokens or 0), "output_tokens": int(output_tokens or 0)}


# Per-process ("per-run") spend ledger, keyed by target. Cents (ints) to avoid
# float drift across many small additions.
_ledger: dict[str, int] = {}


def record_spend(target: str, usd: float) -> None:
    """Record ``usd`` (an estimated cost) against ``target``'s running spend."""
    if not usd or usd <= 0:
        return
    _ledger[target] = _ledger.get(target, 0) + round(usd * 100)


def spent_cents(target: str) -> int:
    """Cents spent against ``target`` so far this run."""
    return _ledger.get(target, 0)


def reset_llm_budgets() -> None:
    """Test-only: clear all recorded spend (a fresh "run")."""
    _ledger.clear()


def budget_message(target: str, cap_cents: int, spent: int) -> str:
    """The what/why/next message for a budget-exceeded block (DX invariant:
    first line human, KEEL-E0NN code, concrete next step)."""
    cap = cap_cents / 100
    spent_usd = spent / 100
    return (
        f"LLM budget cap ${cap:.2f}/run for {target} is exhausted (spent ${spent_usd:.2f} this run); "
        "the call was blocked before dispatch, like an open circuit breaker. Raise "
        f'target."{target}".budget (or defaults.llm.budget) in keel.toml, or reduce request volume.'
    )


def budget_blocked_outcome(message: str) -> dict[str, Any]:
    """An outcome dict shaped exactly like the core/stub's own breaker-open
    outcome, so ``discovery.record`` (breaker_opens / failures accounting) and
    the httpx/requests seams' existing ``deliver`` logic treat a budget block
    identically to a real breaker trip."""
    return {
        "v": 1,
        "result": "error",
        "attempts": 0,
        "from_cache": False,
        "waits_ms": [],
        "throttled": False,
        "throttle_wait_ms": 0,
        "breaker": "open",
        "error": {"code": "KEEL-E012", "class": "other", "message": message},
    }


# --- fallback: model derivation + rewriting ----------------------------------


def _parse_json_body(body: bytes | str | None) -> Any:
    if body is None:
        return None
    try:
        raw = body.decode("utf-8") if isinstance(body, (bytes, bytearray)) else body
        return json.loads(raw)
    except (ValueError, TypeError, UnicodeDecodeError):
        return None


_GENAI_MODEL_URL = re.compile(r"/models/([^/:?]+):")


def derive_request_model(url: str, body: bytes | str | None) -> str | None:
    """The model name a request targets, or ``None`` when it can't be
    determined (see the module doc's v0.1 rewrite limitation)."""
    parsed = _parse_json_body(body)
    if isinstance(parsed, dict) and isinstance(parsed.get("model"), str):
        return parsed["model"]
    m = _GENAI_MODEL_URL.search(url or "")
    return m.group(1) if m else None


def rewrite_model(url: str, body: bytes | str | None, new_model: str) -> tuple[str, bytes | str] | None:
    """Rewrite a request to target ``new_model`` for the next fallback hop.
    Returns ``(url, body)``, or ``None`` when the request shape is
    unrecognized (fallback then stops there; the CURRENT failure is delivered
    to the caller — see the module doc's v0.1 limitation on cross-provider
    fallback). The returned body is the SAME TYPE (``str``/``bytes``) as the
    input body, so callers don't need to special-case either."""
    parsed = _parse_json_body(body)
    if isinstance(parsed, dict) and isinstance(parsed.get("model"), str):
        parsed["model"] = new_model
        new_body = json.dumps(parsed)
        if isinstance(body, (bytes, bytearray)):
            new_body = new_body.encode("utf-8")
        return url, new_body
    if _GENAI_MODEL_URL.search(url or ""):
        return _GENAI_MODEL_URL.sub(f"/models/{quote(new_model, safe='')}:", url), body
    return None


_NO_FALLBACK_CODES = frozenset({"KEEL-E012"})


def should_fallback(error: Mapping[str, Any] | None) -> bool:
    """Whether a terminal call failure should chase the next model in a
    fallback chain. Excludes breaker-open (KEEL-E012) — real trips AND our own
    budget synthesis both fail fast on purpose; chasing a fallback would
    defeat that."""
    if not error:
        return False
    return error.get("code") not in _NO_FALLBACK_CODES


__all__ = [
    "parse_budget_cents",
    "PRICE_TABLE_USD_PER_MILLION",
    "DEFAULT_PRICE_USD_PER_MILLION",
    "estimate_cost_usd",
    "normalize_usage",
    "record_spend",
    "spent_cents",
    "reset_llm_budgets",
    "budget_message",
    "budget_blocked_outcome",
    "derive_request_model",
    "rewrite_model",
    "should_fallback",
]
