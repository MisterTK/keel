"""Shared HTTP judgments and the seam orchestration, reused by every HTTP
library pack (httpx, requests, …).

These derivations are the Python twin of ``node/keel/src/judge.mjs`` and MUST
stay in parity with it (parity is a feature — the two front ends must reach the
same target/idempotency/args_hash/error-class decisions for the same call):

  * target      — the request host, unless the host maps to an LLM provider, in
    which case the semantic target ``llm:<provider>`` (so the Task 11 llm packs
    get their targets, and ``defaults.llm`` applies, for free).
  * op          — ``"METHOD host/path"`` (no query string), for messages/journal.
  * idempotent  — safe/idempotent methods (GET/HEAD/OPTIONS/TRACE + PUT/DELETE
    per RFC 9110) are retryable; POST/PATCH are NOT retryable (Level 0 hard
    rule) unless an idempotency header is present.
  * args_hash   — a stable SHA-256 over method+url, cache-key material, derived
    ONLY for idempotent GETs; ``None`` otherwise (disables caching for the call).
  * error class — a response status ≥500 or ==429 becomes a typed ``http`` error
    (with Retry-After parsed to ms); every other status (2xx/3xx and non-429
    4xx) passes through UNCHANGED as a success — Keel never turns a real HTTP
    response into a failure (matches conformance scenario 05-non-retryable-4xx).
    Thrown transport errors are classified conn/timeout/other by each pack.

The library packs supply only the tiny library-specific bits (how to read a
request/response, which exceptions mean conn vs timeout, and how to close a
superseded response); everything policy-shaped lives here so the judgment is
written once.
"""

from __future__ import annotations

import hashlib
from datetime import datetime, timezone
from email.utils import parsedate_to_datetime
from types import MappingProxyType
from typing import Any, Callable, Iterable

from .._errors import KeelError

ENVELOPE_VERSION = 1

#: Host → LLM provider. A cross-language parity contract with the Node front
#: end (``LLM_HOST_PROVIDERS`` in judge.mjs); extend in lockstep across
#: languages, since adding a host here changes which default pack applies.
LLM_HOST_PROVIDERS: MappingProxyType[str, str] = MappingProxyType(
    {
        "api.openai.com": "openai",
        "api.anthropic.com": "anthropic",
        "generativelanguage.googleapis.com": "google-genai",
    }
)

#: Methods whose semantics make a retry safe (RFC 9110 §9.2.2). Matches the
#: Node twin's ``IDEMPOTENT_METHODS`` exactly — TRACE is included for parity of
#: the *judgment* even though clients rarely send it. POST/PATCH are absent by
#: design: a POST is retryable only with an idempotency key (below).
IDEMPOTENT_METHODS = frozenset({"GET", "HEAD", "OPTIONS", "PUT", "DELETE", "TRACE"})

#: Header names that mark an otherwise-unsafe request (POST/PATCH) as safe to
#: retry. Parity with the Node twin's ``DEFAULT_IDEMPOTENCY_HEADERS``.
DEFAULT_IDEMPOTENCY_HEADERS = ("idempotency-key", "x-idempotency-key")


def resolve_target(host: str) -> str:
    """The policy target for a host: ``llm:<provider>`` for a known provider
    host, else the bare host string."""
    provider = LLM_HOST_PROVIDERS.get(host)
    return f"llm:{provider}" if provider else host


def is_idempotent(
    method: str,
    header_names: Iterable[str],
    idempotency_header: str | None = None,
) -> bool:
    """Decide retryability. ``header_names`` are the request's header names
    (any case). A POST/PATCH is retryable only when a recognized idempotency
    header is present; ``idempotency_header`` is the target's configured header
    (policy ``idempotency.header``), if any, else the default set is used."""
    if method in IDEMPOTENT_METHODS:
        return True
    present = {h.lower() for h in header_names}
    candidates = (
        (idempotency_header.lower(),)
        if idempotency_header
        else DEFAULT_IDEMPOTENCY_HEADERS
    )
    return any(h in present for h in candidates)


def args_hash(method: str, url: str, body: bytes | str | None = None) -> str:
    """A stable SHA-256 over ``method`` + ``url`` (+ ``body`` when present).
    Cache-key material only, so cross-language byte-identity is not required —
    caches are per-process. Mirrors the Node twin's field order."""
    h = hashlib.sha256()
    h.update(method.encode("utf-8"))
    h.update(b"\n")
    h.update(url.encode("utf-8"))
    if isinstance(body, (bytes, bytearray)):
        h.update(b"\n")
        h.update(bytes(body))
    elif isinstance(body, str):
        h.update(b"\n")
        h.update(body.encode("utf-8"))
    return h.hexdigest()


def parse_retry_after(value: str | None, now: datetime | None = None) -> int | None:
    """Parse a ``Retry-After`` header to milliseconds. Supports the two RFC
    9110 forms — delta-seconds (an integer) and an HTTP-date — and returns
    ``None`` for anything unparseable. A past date clamps to 0."""
    if value is None:
        return None
    s = value.strip()
    if s.isascii() and s.isdigit():  # ASCII digits only, matching Node's /^\d+$/
        return int(s) * 1000
    try:
        when = parsedate_to_datetime(s)
    except (TypeError, ValueError):
        return None
    if when is None:
        return None
    if when.tzinfo is None:
        when = when.replace(tzinfo=timezone.utc)
    now = now or datetime.now(timezone.utc)
    delta_ms = round((when - now).total_seconds() * 1000)
    return max(0, delta_ms)


def is_transient_status(status: int) -> bool:
    """True when a response status should be treated as a transient typed
    error (retried per policy): 429, or any 5xx."""
    return status == 429 or status >= 500


def build_request(target: str, op: str, idempotent: bool, hash_: str | None) -> dict[str, Any]:
    """The core request envelope for one intercepted HTTP call."""
    return {
        "v": ENVELOPE_VERSION,
        "target": target,
        "op": op,
        "idempotent": idempotent,
        "args_hash": hash_,
    }


def transient_error(response: Any, http_status: int, retry_after: str | None) -> dict[str, Any]:
    """An ``AttemptResult`` error for a transient HTTP response (5xx/429). The
    response is round-tripped as ``original`` so the front end can hand the
    caller the real, unchanged response after retries exhaust."""
    return {
        "status": "error",
        "class": "http",
        "http_status": http_status,
        "retry_after_ms": parse_retry_after(retry_after),
        "message": f"HTTP {http_status}",
        "original": response,
    }


def thrown_error(err: BaseException, cls: str) -> dict[str, Any]:
    """An ``AttemptResult`` error for a thrown transport exception, carrying the
    original exception object for unchanged re-raise (DX invariant 5)."""
    return {"status": "error", "class": cls, "message": str(err), "original": err}


def attach_outcome(obj: Any, outcome: dict[str, Any]) -> Any:
    """Attach the core outcome to a returned response / re-raised exception,
    best-effort — never let the attachment interfere with delivery. Setting an
    attribute does not change a response's status/headers/body, so success-path
    byte-transparency is preserved."""
    try:
        obj.keel_outcome = outcome  # type: ignore[attr-defined]
    except Exception:
        pass
    return obj


def deliver(outcome: dict[str, Any], is_response: Callable[[Any], bool]) -> tuple[str, Any]:
    """Turn a core outcome into a delivery decision:

      * ``("return", response)`` — a real response to hand back unchanged (an
        ok payload, or the last real HTTP response after transient retries).
      * ``("raise", exc)``       — an exception to raise unchanged (the original
        transport error) or a synthesized ``KeelError`` (e.g. breaker open
        before any attempt, so no original was captured).
    """
    if outcome.get("result") == "ok":
        return ("return", attach_outcome(outcome.get("payload"), outcome))
    err = outcome.get("error") or {}
    original = err.get("original")
    if original is not None and is_response(original):
        return ("return", attach_outcome(original, outcome))
    if isinstance(original, BaseException):
        return ("raise", attach_outcome(original, outcome))
    synthetic = KeelError(err.get("code") or "KEEL-E040", err.get("message") or "keel: request failed")
    return ("raise", attach_outcome(synthetic, outcome))


__all__ = [
    "ENVELOPE_VERSION",
    "LLM_HOST_PROVIDERS",
    "IDEMPOTENT_METHODS",
    "DEFAULT_IDEMPOTENCY_HEADERS",
    "resolve_target",
    "is_idempotent",
    "args_hash",
    "parse_retry_after",
    "is_transient_status",
    "build_request",
    "transient_error",
    "thrown_error",
    "attach_outcome",
    "deliver",
]
