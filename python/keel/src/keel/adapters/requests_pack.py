"""The requests adapter pack: resilience for every ``requests`` call through
the documented Transport Adapter seam.

Seam (narrowest stable): ``requests.adapters.HTTPAdapter.send`` — requests'
documented adapter API. Every ``Session`` request dispatches through the
mounted adapter's ``send`` (the default Session mounts ``HTTPAdapter`` for
http:// and https://), so patching the class covers standard usage; a custom
adapter subclass that calls ``super().send()`` is covered too.

The wrapper reads the backend + discovery store from the process runtime at
call time, so ``uninstall`` / ``KEEL_DISABLE`` makes it a transparent
passthrough. Judgment is shared with the httpx pack and the Node twin via
``_http``. Classification only ever reads ``status_code`` and the
``Retry-After`` header — never the body — so the caller receives the real,
untouched ``requests.Response`` (success-path byte-transparency).

Double-wrap guard: requests vendors its own use of urllib3's connection pools,
so a ``requests`` call also passes through the ``urllib3`` pack's seam
(``urllib3_pack``) if both are installed. The actual library call is run under
``_http.run_owned`` so the urllib3 pack sees ``seam_owned()`` and passes that
inner call straight through — one intercepted request is exactly one Keel
``execute``, never a retry loop nested inside another (``_http`` module docs).

LLM budget caps + model fallback chains (``_llm_policy``) ride this SAME seam
for ``llm:*`` POST targets: a request may be re-dispatched through
``call_with`` one or more times (a copied+rewritten ``PreparedRequest`` per
fallback hop), and a configured ``budget`` can block a call before ANY hop is
dispatched. See ``_llm_policy`` for the full design and its documented v0.1
limitations.
"""

from __future__ import annotations

import base64
import functools
import importlib.metadata
import importlib.util
import json
import time
from typing import Any, Callable
from urllib.parse import urlsplit

from .. import _runtime
from .._errors import KeelError
from . import _http, _llm_policy
from ._pack import Detection, Seam, TargetDecl

MODULE = "requests"
NAME = "requests"

_PINNED = ("2.33", "2.34")

_installed = False
_orig: dict[str, Any] = {}


# --- contract operations -----------------------------------------------------

def detect() -> Detection:
    if importlib.util.find_spec(MODULE) is None:
        return Detection(matched=False)
    try:
        version = importlib.metadata.version(MODULE)
    except importlib.metadata.PackageNotFoundError:
        version = ""
    confidence = "pinned" if _is_pinned(version) else "best_effort"
    return Detection(matched=True, name=NAME, version=version, confidence=confidence)


def seams() -> list[Seam]:
    return [
        Seam(
            patch_point="requests.adapters.HTTPAdapter.send",
            upstream_api="requests Transport Adapter API: HTTPAdapter.send(request, ...) -> Response",
            why_stable=(
                "HTTPAdapter.send is requests' documented adapter seam; every "
                "Session request dispatches through the mounted adapter's send()."
            ),
        ),
    ]


def targets() -> list[TargetDecl]:
    host = TargetDecl(
        pattern="<request host>",
        kind="host",
        idempotency_rule="GET/HEAD/OPTIONS/TRACE/PUT/DELETE, or an Idempotency-Key header on POST/PATCH",
        args_hash_rule="sha256(method + url) for idempotent GET; None otherwise",
    )
    llm = [
        TargetDecl(
            pattern=f"llm:{provider}",
            kind="llm",
            idempotency_rule=f"host {host_name} maps to llm:{provider}; idempotency as for host targets",
            args_hash_rule=(
                "sha256(method + url) for idempotent GET; sha256 over "
                "(method, url, canonicalized JSON body) for LLM POST "
                "(dev-cache replay); None otherwise"
            ),
        )
        for host_name, provider in _http.LLM_HOST_PROVIDERS.items()
    ]
    return [host, *llm]


def defaults() -> dict[str, Any]:
    """No pack-specific fragment: targets inherit ``defaults.outbound`` /
    ``defaults.llm`` from the Level 0 pack."""
    return {}


# --- install / uninstall -----------------------------------------------------

def install() -> None:
    global _installed
    if _installed:
        return
    try:
        from requests.adapters import HTTPAdapter
    except ImportError:
        return
    _orig["send"] = HTTPAdapter.send
    HTTPAdapter.send = _send_wrapper(_orig["send"])  # type: ignore[method-assign]
    _installed = True


def uninstall() -> None:
    global _installed
    if not _installed:
        return
    from requests.adapters import HTTPAdapter

    HTTPAdapter.send = _orig["send"]  # type: ignore[method-assign]
    _orig.clear()
    _installed = False


# --- judgment ----------------------------------------------------------------

def _judge(request: Any) -> tuple[str, str, bool, str | None, str | None]:
    method = (request.method or "GET").upper()
    url = request.url
    parts = urlsplit(url)
    host = parts.hostname or ""
    # Pattern-aware target selection (docs/targeting.md): exact host key, else
    # the most specific matching host/URL pattern key, else the bare host.
    target = _http.resolve_policy_target(
        method, host, scheme=parts.scheme, port=parts.port, path=parts.path
    )
    op = f"{method} {host}{parts.path}"
    idem_header = _http.idempotency_header(target)
    # A prepared body is bytes/str (buffered) or None; derive_args_hash ignores a
    # streaming (generator/file) body, so this never consumes an upload stream.
    # LLM POSTs get a canonicalized-JSON-body cache key (dev-cache exception).
    # Derived BEFORE the injection decision: the Tier 2 step key (`target#hash`)
    # a resume-reuse peek looks up (contracts/adapter-pack.md rule 3) must be
    # known before deciding what to inject.
    hash_ = _http.derive_args_hash(target, method, url, request.body)
    recorded_key = _http.peek_recorded_idempotency_key(target, hash_)
    # Injection (contracts/adapter-pack.md "Idempotency-key injection"): mint
    # once, before the first attempt, and set it on the PreparedRequest so
    # every retry attempt resends the SAME header (`do_call` re-sends this
    # exact request object on each attempt — see `_run_send` below). Inside a
    # Tier 2 flow, a crashed predecessor's key (`recorded_key`, peeked above)
    # is reused verbatim instead of minting a fresh one (rule 3).
    injected = _http.resolve_idempotency_injection(
        method, request.headers.keys(), idem_header, recorded_key=recorded_key
    )
    if injected is not None:
        request.headers[idem_header] = injected  # type: ignore[index]
    idempotent = injected is not None or _http.is_idempotent(
        method, request.headers.keys(), idem_header
    )
    return target, op, idempotent, hash_, injected


def _is_response(obj: Any) -> bool:
    import requests

    return isinstance(obj, requests.Response)


def _classify(err: BaseException) -> str:
    import requests.exceptions as rex

    if isinstance(err, rex.Timeout):  # ConnectTimeout / ReadTimeout
        return "timeout"
    if isinstance(err, rex.ConnectionError):
        return "conn"
    return "other"


# --- response (de)serialization for the core payload ------------------------

def _ok_payload(resp: Any, cacheable: bool) -> dict[str, Any]:
    """A JSON envelope of a requests.Response for the core payload. The body
    (`resp.content`, buffered when stream=False) is included only for cacheable
    calls; the live response is returned unchanged on the success path."""
    body = None
    if cacheable:
        try:
            body = resp.content
        except Exception:
            body = None
    try:
        headers = list(resp.headers.items())
    except Exception:
        headers = []
    return _http.response_envelope(resp.status_code, headers, body)


def _rebuild(payload: Any) -> Any:
    """Rebuild a requests.Response from an envelope (a cache-hit replay)."""
    import requests
    from requests.structures import CaseInsensitiveDict

    p = payload if isinstance(payload, dict) else {}
    resp = requests.Response()
    resp.status_code = int(p.get("status", 200))
    resp._content = base64.b64decode(p["body_b64"]) if isinstance(p.get("body_b64"), str) else b""
    resp.headers = CaseInsensitiveDict(dict(p.get("headers", [])))
    resp.encoding = requests.utils.get_encoding_from_headers(resp.headers)
    return resp


# --- LLM budget + fallback helpers (mirrors httpx_pack.py) ------------------

def _llm_generate_gate(target: str, method: str) -> tuple[bool, int | None]:
    is_llm = target.startswith("llm:") and method == "POST"
    cap = _llm_policy.parse_budget_cents(_http.resolve_layer(target, "budget")) if is_llm else None
    return is_llm, cap


def _llm_fallback_chain(is_llm: bool, target: str) -> list[str]:
    if not is_llm:
        return []
    cfg = _http.resolve_layer(target, "fallback")
    return [m for m in cfg if isinstance(m, str) and m] if isinstance(cfg, list) else []


def _budget_blocked_error(target: str, cap_cents: int, discovery: Any) -> KeelError:
    spent = _llm_policy.spent_cents(target)
    message = _llm_policy.budget_message(target, cap_cents, spent)
    outcome = _llm_policy.budget_blocked_outcome(message)
    if discovery is not None:
        discovery.record(target, outcome, 0)
    return KeelError("KEEL-E012", message)


def _record_llm_spend(target: str, request: Any, payload: Any) -> None:
    """Best-effort: price a live response's usage (from its buffered JSON body
    — a deliberate, narrowly scoped exception, see `_llm_policy`) and record it
    against `target`'s per-run ledger. Never raises."""
    if not isinstance(payload, dict):
        return
    b64 = payload.get("body_b64")
    if not isinstance(b64, str):
        return
    try:
        parsed = json.loads(base64.b64decode(b64))
    except Exception:
        return
    usage = _llm_policy.normalize_usage(parsed)
    if usage is None:
        return
    model = _llm_policy.derive_request_model(request.url, request.body)
    _llm_policy.record_spend(target, _llm_policy.estimate_cost_usd(model, usage))


def _next_hop_request(request: Any, next_model: str) -> Any | None:
    """A copied+rewritten `PreparedRequest` for the next fallback hop, or
    `None` when the current request's shape can't be rewritten (see
    `_llm_policy`). `PreparedRequest.copy()` is requests' own supported API for
    this (used internally for redirects), so this needs no library internals."""
    rewritten = _llm_policy.rewrite_model(request.url, request.body, next_model)
    if rewritten is None:
        return None
    new_url, new_body = rewritten
    next_request = request.copy()
    next_request.url = new_url
    next_request.body = new_body
    body_len = len(new_body) if isinstance(new_body, (bytes, bytearray)) else len(new_body.encode("utf-8"))
    next_request.headers["Content-Length"] = str(body_len)
    return next_request


# --- seam --------------------------------------------------------------------

def _compose_requests_timeout(policy_s: float | None, caller_timeout: Any) -> Any:
    """Tighter wins, in requests' own ``timeout=`` shape: ``None`` (block
    forever, requests' default) or a bare float apply to both connect+read;
    a ``(connect, read)`` tuple composes per leg. A configured policy timeout
    always wins over "no caller timeout"; two real numbers take the min."""
    if policy_s is None:
        return caller_timeout
    if isinstance(caller_timeout, tuple) and len(caller_timeout) == 2:
        connect, read = caller_timeout
        return (
            policy_s if connect is None else min(float(connect), policy_s),
            policy_s if read is None else min(float(read), policy_s),
        )
    if isinstance(caller_timeout, (int, float)):
        return min(float(caller_timeout), policy_s)
    return policy_s  # None, or an unrecognized shape: policy wins


def _run_send(
    orig: Callable[..., Any], self: Any, request: Any, args: tuple[Any, ...], kwargs: dict[str, Any]
) -> Any:
    backend = _runtime.get_backend()
    if backend is None:
        return orig(self, request, *args, **kwargs)  # disabled / uninstalled: transparent
    discovery = _runtime.get_discovery()

    current = request
    hop = 0
    live: dict[str, Any] = {"ok": None, "transient": None, "exc": None}
    outcome: dict[str, Any] = {}

    while True:
        target, op, idempotent, hash_, injected = _judge(current)
        env = _http.build_request(target, op, idempotent, hash_)
        is_llm, cap_cents = _llm_generate_gate(target, current.method or "GET")
        if hop == 0 and cap_cents is not None and _llm_policy.spent_cents(target) >= cap_cents:
            raise _budget_blocked_error(target, cap_cents, discovery)
        track_usage = cap_cents is not None
        # Buffer the body ONLY when a cache ttl or a poll table is actually
        # configured (mirrors Node's fetch gate) OR usage accounting needs it;
        # with neither, a stream=True GET is never force-read at the seam.
        cacheable = hash_ is not None and _http.buffer_body_configured(target)
        buffer_body = cacheable or track_usage
        live = {"ok": None, "transient": None, "exc": None}

        # requests' timeout is a `send()` kwarg, not part of the PreparedRequest
        # (issue #32) — compose the resolved policy timeout into a per-attempt
        # effective kwargs dict rather than baking a fixed call_with closure.
        effective_kwargs = dict(kwargs)
        policy_timeout_s = _http.resolve_timeout_s(target)
        if policy_timeout_s is not None:
            effective_kwargs["timeout"] = _compose_requests_timeout(
                policy_timeout_s, kwargs.get("timeout")
            )

        def effect(
            _attempt: int,
            _current: Any = current,
            _buffer: bool = buffer_body,
            _kwargs: dict[str, Any] = effective_kwargs,
        ) -> dict[str, Any]:
            try:
                # requests dispatches through urllib3 under the hood (its vendored
                # connection pools); `run_owned` marks this attempt so the urllib3
                # pack's own seam sees `seam_owned()` and passes straight through
                # instead of judging (and retrying) the same call a second time.
                resp = _http.run_owned(lambda: orig(self, _current, *args, **_kwargs))
            except Exception as err:
                live["exc"] = err
                return _http.thrown_error(err, _classify(err))
            live["exc"] = None
            if _http.is_transient_status(resp.status_code):
                if live["transient"] is not None and live["transient"] is not resp:
                    _close(live["transient"])
                live["transient"] = resp
                return _http.transient_error(resp.status_code, resp.headers.get("Retry-After"))
            if live["transient"] is not None and live["transient"] is not resp:
                _close(live["transient"])
            live["transient"] = None
            live["ok"] = resp
            return {"status": "ok", "payload": _ok_payload(resp, _buffer)}

        started = time.perf_counter()
        outcome = _http.call_execute(backend, env, effect, injected)
        latency_ms = round((time.perf_counter() - started) * 1000)
        if discovery is not None:
            discovery.record(target, outcome, latency_ms)

        if outcome.get("result") == "ok":
            if track_usage and not outcome.get("from_cache"):
                _record_llm_spend(target, current, outcome.get("payload"))
            break

        chain = _llm_fallback_chain(is_llm, target)
        if hop >= len(chain) or not _llm_policy.should_fallback(outcome.get("error")):
            break
        next_request = _next_hop_request(current, chain[hop])
        if next_request is None:
            break
        current = next_request
        hop += 1

    action, value = _http.deliver(
        outcome,
        ok_response=live["ok"],
        transient_response=live["transient"],
        exc=live["exc"],
        rebuild=_rebuild,
    )
    if action == "raise" and live["transient"] is not None and live["transient"] is not value:
        _close(live["transient"])
    if action == "return":
        return value
    raise value


def _send_wrapper(orig: Callable[..., Any]) -> Callable[..., Any]:
    @functools.wraps(orig)
    def send(self: Any, request: Any, *args: Any, **kwargs: Any) -> Any:
        return _run_send(orig, self, request, args, kwargs)

    send.__keel_wrapped__ = True  # type: ignore[attr-defined]
    return send


def _close(resp: Any) -> None:
    try:
        resp.close()
    except Exception:
        pass


def _is_pinned(version: str) -> bool:
    return any(version == p or version.startswith(p + ".") for p in _PINNED)


__all__ = ["MODULE", "NAME", "detect", "seams", "targets", "defaults", "install", "uninstall"]
