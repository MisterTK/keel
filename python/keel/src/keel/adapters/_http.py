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
import contextvars
import hashlib
import json
import re
import uuid
from datetime import datetime, timezone
from email.utils import parsedate_to_datetime
from types import MappingProxyType
from typing import Any, Callable, Iterable

from .. import _runtime
from .._errors import KeelError

ENVELOPE_VERSION = 1

_DURATION_RE = re.compile(r"^\s*(\d+)\s*(ms|s|m|h)\s*$")
_DURATION_S = {"ms": 0.001, "s": 1.0, "m": 60.0, "h": 3600.0}

#: Marker key identifying a serialized HTTP response envelope (the JSON the core
#: carries as `payload`; see `response_envelope`).
RESPONSE_MARK = "__keel_http__"

#: Host → LLM provider, for DECLARATIVE use only (each HTTP pack's
#: ``targets()`` enumerates one ``TargetDecl`` per known provider host, for
#: doctor/``keel init`` documentation). Actual target RESOLUTION no longer
#: consults this dict — as of Task 10/SP-1 it happens inside the backend
#: (the native core, or ``keel_core_stub.KeelCoreStub``'s own
#: ``_LLM_HOST_PROVIDERS``/suffix rule), which every pack now calls via
#: ``_runtime.get_backend().resolve_target(...)``. Kept here (same name, same
#: values) since several framework packs' docstrings reference it by this
#: exact dotted path; still a cross-language parity contract with the Node
#: front end (``LLM_HOST_PROVIDERS`` in judge.mjs) and the backend's own
#: copies — extend all three in lockstep, since adding a host here changes
#: which default pack applies (and which host each pack's ``targets()``
#: declares).
LLM_HOST_PROVIDERS: MappingProxyType[str, str] = MappingProxyType(
    {
        "api.openai.com": "openai",
        "api.anthropic.com": "anthropic",
        "generativelanguage.googleapis.com": "google-genai",
        "aiplatform.googleapis.com": "google-genai",
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


#: True while a higher-level Keel seam (requests' ``HTTPAdapter.send``,
#: botocore's ``BaseClient._make_api_call``) is driving the underlying
#: transport for one attempt. Lower-level adapters (urllib3) check this and
#: pass through untouched, so one intercepted call is exactly one core
#: ``execute`` — never a Keel retry loop nested inside another.
_SEAM_OWNED: contextvars.ContextVar[bool] = contextvars.ContextVar(
    "keel_http_seam_owned", default=False
)


def run_owned(fn: Callable[[], Any]) -> Any:
    """Run one attempt's underlying library call with the seam-ownership flag
    set. A ``ContextVar`` set and reset around the call is correct in every
    execution shape the packs use: the flag is scoped to whichever thread (or
    async task) actually runs the effect, so a stub worker-thread attempt and a
    native in-loop attempt both mark exactly their own downstream calls."""
    token = _SEAM_OWNED.set(True)
    try:
        return fn()
    finally:
        _SEAM_OWNED.reset(token)


def seam_owned() -> bool:
    """True when a higher-level Keel seam already owns the in-flight call (the
    double-wrap guard for adapters whose library is also used *by* a wrapped
    library — urllib3 under requests/botocore)."""
    return _SEAM_OWNED.get()


def resolve_layer(target: str, key: str) -> Any:
    """The resolved value of policy layer ``key`` for ``target`` (or ``None``).

    Reads the active backend's ``layer(target, key)`` (parity with Node, whose
    packs read ``backend.layer``). Resolution — exact ``[target."…"]`` wins, then
    ``[defaults.llm]`` for an ``llm:`` target, then ``[defaults.outbound]`` — is
    owned by the backend (the stub's public ``layer``; the native core's own
    ``layer`` binding, exposed directly since Task 10/SP-1 dropped the
    ``_backend._NativeBackend`` wrapper), so the two front ends make identical
    per-target judgments for the same policy."""
    backend = _runtime.get_backend()
    layer = getattr(backend, "layer", None)
    return layer(target, key) if callable(layer) else None


def resolve_timeout_s(target: str) -> float | None:
    """The resolved policy ``timeout`` for ``target``, in seconds, or
    ``None`` if unconfigured/unparseable. Shared by every SYNC HTTP pack
    (httpx, requests, urllib.request): ``engine.rs``'s policy-layer timer
    (``run_one_attempt``) races the awaited future to enforce ``timeout``,
    which cannot preempt a blocking call on a sync binding — so a sync pack
    must inject this value as a call-level deadline into its own library's
    timeout mechanism, or a configured ``timeout`` is silently inert for
    every synchronous caller (issue #32)."""
    value = resolve_layer(target, "timeout")
    if not isinstance(value, str):
        return None
    m = _DURATION_RE.match(value)
    return int(m.group(1)) * _DURATION_S[m.group(2)] if m else None


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


def poll_configured(target: str) -> bool:
    """True iff a ``poll`` table resolves for ``target`` (CCR-3). Polling
    judges the response BODY, so a poll-configured call must buffer it into
    the envelope (``body_b64``) exactly as a cache ttl does."""
    return isinstance(resolve_layer(target, "poll"), dict)


def buffer_body_configured(target: str) -> bool:
    """The body-buffering gate at the seam: cache ttl OR poll table. Mirrors
    Node's fetch gate."""
    return cache_configured(target) or poll_configured(target)


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


def new_idempotency_key() -> str:
    """Mint one opaque idempotency key (contracts/adapter-pack.md
    "Idempotency-key injection" rule 2: ONE per logical call, minted before the
    first attempt). A plain module-level function — not a class/closure — so
    tests get a deterministic source via ``monkeypatch.setattr(_http,
    "new_idempotency_key", lambda: "fixed")``; production mints a fresh UUID4."""
    return uuid.uuid4().hex


def step_key(target: str, args_hash: str | None) -> str:
    """The `(target)#(args_hash)` key identifying a Tier 2 effect step —
    matches `FlowHandle::step_key` in crates/keel-core/src/flow.rs exactly
    (`"-"` for a missing `args_hash`), so a peek here lands on the journal row
    a resumed step would occupy."""
    return f"{target}#{args_hash if args_hash is not None else '-'}"


def peek_recorded_idempotency_key(target: str, args_hash: str | None) -> str | None:
    """The idempotency key recorded for this step's NEXT execution, when a
    Tier 2 flow is open and that record is a crashed (`running`) step under
    this step's key (contracts/adapter-pack.md "Idempotency-key injection"
    rule 3) — `None` outside a flow, on a backend with no peek surface (the
    stub; a bare Tier 1 core), or when nothing is recorded. Call this BEFORE
    building the outgoing request, so a resumed call injects the SAME key its
    crashed predecessor did instead of minting a fresh one."""
    if not _runtime.in_active_flow():
        return None
    backend = _runtime.get_backend()
    peek = getattr(backend, "recorded_idempotency_key", None)
    return peek(step_key(target, args_hash)) if callable(peek) else None


def call_execute(
    backend: Any,
    request: dict[str, Any],
    effect: Callable[[int], dict[str, Any]],
    idempotency_key: str | None,
) -> dict[str, Any]:
    """`backend.execute(request, effect)`, threading `idempotency_key` through
    ONLY while a Tier 2 flow is open (contracts/adapter-pack.md rule 3): the
    parameter is native/flow-only, so passing it outside a flow — where the
    backend may be the stub, which does not accept it — would break the plain
    (non-flow) Tier 1 path this must never touch."""
    if _runtime.in_active_flow():
        return backend.execute(request, effect, idempotency_key=idempotency_key)  # type: ignore[no-any-return]
    return backend.execute(request, effect)  # type: ignore[no-any-return]


def resolve_idempotency_injection(
    method: str,
    header_names: Iterable[str],
    configured_header: str | None,
    *,
    recorded_key: str | None = None,
) -> str | None:
    """The idempotency key to INJECT for this call, or ``None`` to inject
    nothing (contracts/adapter-pack.md "Idempotency-key injection"):

      * ``None`` when the method is already idempotent (injection only ever
        targets an unsafe method — rule 1), no ``idempotency.header`` is
        configured for the target, or the caller already supplied the
        configured header — a caller-supplied key always wins; adapters never
        overwrite one.
      * otherwise ``recorded_key`` when given (a Tier 2 resume's key,
        journaled with the crashed step — rule 3 — reused verbatim so the
        re-execution is deduplicable on the provider side), else a freshly
        minted one (:func:`new_idempotency_key`; stable across Tier 1 retries
        because the caller mints/injects it once, before the first attempt).

    The returned key is not folded into ``args_hash`` by any caller of this
    function — rule 5 (it would otherwise fence Tier 2 replay)."""
    if configured_header is None or method in IDEMPOTENT_METHODS:
        return None
    present = {h.lower() for h in header_names}
    if configured_header.lower() in present:
        return None
    return recorded_key if recorded_key is not None else new_idempotency_key()


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
    "resolve_timeout_s",
    "idempotency_header",
    "cache_configured",
    "poll_configured",
    "buffer_body_configured",
    "is_idempotent",
    "new_idempotency_key",
    "step_key",
    "peek_recorded_idempotency_key",
    "call_execute",
    "resolve_idempotency_injection",
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
