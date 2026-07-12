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

import base64
import hashlib
import json
from datetime import datetime, timezone
from email.utils import parsedate_to_datetime
from types import MappingProxyType
from typing import Any, Callable, Iterable

from .. import _runtime
from .._errors import KeelError

ENVELOPE_VERSION = 1

#: Marker key identifying a serialized HTTP response envelope (the JSON the core
#: carries as `payload`; see `response_envelope`).
RESPONSE_MARK = "__keel_http__"

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


def resolve_layer(target: str, key: str) -> Any:
    """The resolved value of policy layer ``key`` for ``target`` (or ``None``).

    Reads the active backend's ``layer(target, key)`` (parity with Node, whose
    packs read ``backend.layer``). Resolution — exact ``[target."…"]`` wins, then
    ``[defaults.llm]`` for an ``llm:`` target, then ``[defaults.outbound]`` — is
    owned by the backend (the stub's public ``layer``; the native core wrapped by
    ``_backend._NativeBackend.layer``), so the two front ends make identical
    per-target judgments for the same policy."""
    backend = _runtime.get_backend()
    layer = getattr(backend, "layer", None)
    return layer(target, key) if callable(layer) else None


def idempotency_header(target: str) -> str | None:
    """The target's configured ``idempotency.header`` (policy knob), or ``None``
    to use the default idempotency-key header set. Honored under BOTH backends
    now that resolution reads the configured policy from the runtime."""
    idem = resolve_layer(target, "idempotency")
    return idem.get("header") if isinstance(idem, dict) else None


def cache_configured(target: str) -> bool:
    """True iff a cache ttl is actually resolved for ``target`` — the gate for
    buffering a response body at the seam. Mirrors Node's fetch gate
    (``hash != null && isTable(cacheCfg) && cacheCfg.ttl !== undefined``): a call
    with no configured cache never has its (possibly streaming) body read, since
    there is nothing to store. The LLM dev cache resolves ``mode="dev"`` to a
    concrete ttl at bootstrap, so ``llm:`` POST bodies are still buffered."""
    cache = resolve_layer(target, "cache")
    return isinstance(cache, dict) and cache.get("ttl") is not None


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


def _canonical_json(body: bytes | str | None) -> bytes | None:
    """Canonical bytes for a request body used as a dev-cache key. Returns the
    key-sorted, whitespace-free JSON encoding when the body parses as JSON, the
    raw bytes verbatim when it does not, or ``None`` when there is no buffered
    body (a streaming body — generator/file — is not cache-replayable).

    Canonicalizing means two semantically-identical prompts replay from cache
    even when the client serialized their JSON with different key order or
    spacing (and gives httpx/requests an identical cache key for the same call)."""
    if isinstance(body, (bytes, bytearray)):
        raw = bytes(body)
    elif isinstance(body, str):
        raw = body.encode("utf-8")
    else:
        return None  # None / stream / generator / file-like: not replayable
    try:
        parsed = json.loads(raw)
    except (ValueError, TypeError):
        return raw  # not JSON: hash the raw bytes verbatim
    return json.dumps(
        parsed, sort_keys=True, separators=(",", ":"), ensure_ascii=False
    ).encode("utf-8")


def derive_args_hash(
    target: str, method: str, url: str, body: bytes | str | None
) -> str | None:
    """Cache-key material for one intercepted call, or ``None`` to disable
    caching for it.

      * idempotent GET      → ``sha256(method + url [+ buffered body])`` (as before).
      * LLM POST (``llm:*``) → the dev-cache exception: ``sha256`` over
        ``(method, url, canonicalized JSON body)``. This enables dev-loop REPLAY
        of an identical prompt; it does NOT make the call retryable — idempotency
        is a separate judgment, still ``False`` for a bare POST (a cache *lookup*
        needs no idempotency; a *retry* does). A streaming/unbuffered body yields
        ``None`` (a live stream is not cache-replayable).
      * everything else     → ``None``.
    """
    if method == "GET":
        return args_hash(method, url, body)
    if method == "POST" and target.startswith("llm:"):
        canon = _canonical_json(body)
        return args_hash(method, url, canon) if canon is not None else None
    return None


def _parse_http_date(s: str) -> datetime | None:
    """A Retry-After HTTP-date. RFC 9110 mandates IMF-fixdate (RFC 5322), but
    servers commonly emit ISO-8601 timestamps too; Node honors those via
    Date.parse, so we accept both for cross-front-end parity (Node matches)."""
    try:
        when = parsedate_to_datetime(s)  # RFC 5322 / IMF-fixdate
    except (TypeError, ValueError):
        when = None
    if when is not None:
        return when
    # ISO-8601 fallback (e.g. "2026-07-12T10:00:00Z"); normalize a trailing Z.
    iso = f"{s[:-1]}+00:00" if s.endswith(("Z", "z")) else s
    try:
        return datetime.fromisoformat(iso)
    except ValueError:
        return None


def parse_retry_after(value: str | None, now: datetime | None = None) -> int | None:
    """Parse a ``Retry-After`` header to milliseconds. Supports delta-seconds (an
    integer) and an HTTP-date — RFC 5322/IMF-fixdate AND ISO-8601, to match the
    Node twin — and returns ``None`` for anything unparseable. A past date clamps
    to 0."""
    if value is None:
        return None
    s = value.strip()
    if s.isascii() and s.isdigit():  # ASCII digits only, matching Node's /^\d+$/
        return int(s) * 1000
    when = _parse_http_date(s)
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


def response_envelope(
    status: int, headers: Iterable[tuple[str, str]], body: bytes | None
) -> dict[str, Any]:
    """A JSON-serializable envelope of an HTTP response for the core ``payload``.

    The core requires a JSON ``payload`` (contracts/core_api.rs) — the real
    native core cannot round-trip a live ``Response`` object, only the stub can.
    So a response never crosses the boundary as the payload: we send this
    envelope and keep the live ``Response`` side-band, returning the live object
    on the success path (byte-transparent) and rebuilding one only on a CACHE HIT
    (in-process or, under the persistent journal, across runs). ``body`` is
    buffered (base64) only for cacheable calls, so a non-cached call is never
    forced to read a streaming body it won't replay."""
    env: dict[str, Any] = {
        RESPONSE_MARK: ENVELOPE_VERSION,
        "status": int(status),
        "headers": [[k, v] for k, v in headers],
    }
    if body is not None:
        env["body_b64"] = base64.b64encode(bytes(body)).decode("ascii")
    return env


def transient_error(http_status: int, retry_after: str | None) -> dict[str, Any]:
    """An ``AttemptResult`` error for a transient HTTP response (5xx/429). The
    live response is kept side-band by the pack (not sent through the core, which
    cannot serialize it); the pack hands back the real response after retries
    exhaust."""
    return {
        "status": "error",
        "class": "http",
        "http_status": http_status,
        "retry_after_ms": parse_retry_after(retry_after),
        "message": f"HTTP {http_status}",
    }


def thrown_error(err: BaseException, cls: str) -> dict[str, Any]:
    """An ``AttemptResult`` error for a thrown transport exception. The exception
    is kept side-band for unchanged re-raise (DX invariant 5) — it is not sent
    through the core (which cannot serialize it)."""
    return {"status": "error", "class": cls, "message": str(err)}


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


def deliver(
    outcome: dict[str, Any],
    *,
    ok_response: Any,
    transient_response: Any,
    exc: BaseException | None,
    rebuild: Callable[[Any], Any],
) -> tuple[str, Any]:
    """Turn a core outcome + the pack's side-band live objects into a delivery
    decision. The core payload is JSON, so the pack — not the core — owns the
    live objects:

      * ok, live call    → the real response, unchanged (byte-transparency).
      * ok, cache hit    → a response rebuilt from the envelope (in-process, or
                           across runs under the persistent journal).
      * error            → the last thrown transport exception (re-raised), or
                           the last real 5xx/429 response (returned) after
                           retries exhaust, else a synthesized ``KeelError``.

    The chosen live original is re-attached to ``outcome["error"]["original"]``
    for DX/consistency — the core never carries it (it isn't serializable)."""
    if outcome.get("result") == "ok":
        if outcome.get("from_cache"):
            return ("return", attach_outcome(rebuild(outcome.get("payload")), outcome))
        return ("return", attach_outcome(ok_response, outcome))
    err = outcome.get("error")
    original = exc if isinstance(exc, BaseException) else transient_response
    if isinstance(err, dict) and original is not None:
        err["original"] = original
    if isinstance(exc, BaseException):
        return ("raise", attach_outcome(exc, outcome))
    if transient_response is not None:
        return ("return", attach_outcome(transient_response, outcome))
    e = err or {}
    synthetic = KeelError(e.get("code") or "KEEL-E040", e.get("message") or "keel: request failed")
    return ("raise", attach_outcome(synthetic, outcome))


__all__ = [
    "ENVELOPE_VERSION",
    "RESPONSE_MARK",
    "LLM_HOST_PROVIDERS",
    "IDEMPOTENT_METHODS",
    "DEFAULT_IDEMPOTENCY_HEADERS",
    "resolve_layer",
    "idempotency_header",
    "cache_configured",
    "resolve_target",
    "is_idempotent",
    "args_hash",
    "derive_args_hash",
    "parse_retry_after",
    "is_transient_status",
    "build_request",
    "response_envelope",
    "transient_error",
    "thrown_error",
    "attach_outcome",
    "deliver",
]
