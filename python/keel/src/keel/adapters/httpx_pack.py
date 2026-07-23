"""The httpx adapter pack: resilience for every ``httpx`` call, zero code
changes, through the documented transport seam.

Seams (narrowest stable — httpx's own extension point):
  * ``httpx.HTTPTransport.handle_request`` (sync) and
    ``httpx.AsyncHTTPTransport.handle_async_request`` (async) — the single I/O
    chokepoint every client request passes through. Patched on the class, so
    the default transport (the common case) is covered.
  * ``httpx.Client.__init__`` / ``httpx.AsyncClient.__init__`` — wrap the
    transport the client actually holds (including a *custom* transport the
    user passed), so custom transports are covered too.

The wrappers read the backend + discovery store from the process runtime at
call time (never a captured closure), so ``uninstall``/``KEEL_DISABLE`` /
clearing the runtime makes every wrapper an instant, transparent passthrough
(DX invariant 2). All judgment (target/idempotency/args_hash/error-class) lives
in ``_http`` and is shared with the requests pack and the Node twin.

LLM budget caps + model fallback chains (``_llm_policy``) ride this SAME seam
for ``llm:*`` POST targets: a request may be re-dispatched through
``call_with`` one or more times (a fresh ``httpx.Request`` per fallback hop),
and a configured ``budget`` can block a call before ANY hop is dispatched. See
``_llm_policy`` for the full design and its documented v0.1 limitations.

Async note (v0.1): the core decision engine is synchronous (the stub; the
native async core lands in Task 14). To drive the async seam through the same
core — so retry/breaker/cache behavior and parity are identical, not
re-implemented — the async wrapper runs ``backend.execute`` in a worker thread
and marshals each attempt's ``await`` back onto the caller's event loop with
``run_coroutine_threadsafe``. Backoff waits in the stub are virtual (no real
sleep), so the worker thread never blocks on a real timer. The native async
core removes the thread hop.
"""

from __future__ import annotations

import asyncio
import base64
import functools
import importlib.metadata
import importlib.util
import json
import time
import weakref
from typing import Any, Callable

from .. import _runtime
from .._errors import KeelError
from . import _http, _llm_policy
from ._pack import Detection, Seam, TargetDecl

MODULE = "httpx"
NAME = "httpx"

#: Versions this pack certifies via contract tests (prefix match). Outside the
#: range detect() reports ``best_effort`` — the pack still tries.
_PINNED = ("0.27", "0.28")

_installed = False
_orig: dict[str, Any] = {}
#: Custom transport instances we wrapped at client init (so uninstall can
#: restore them by dropping the shadowing instance attribute).
_wrapped_transports: "weakref.WeakKeyDictionary[Any, str]" = weakref.WeakKeyDictionary()


# --- contract operations -----------------------------------------------------

def detect() -> Detection:
    """Present iff ``httpx`` is importable — decided without importing it."""
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
            patch_point="httpx.HTTPTransport.handle_request",
            upstream_api="httpx transport API: BaseTransport.handle_request(request) -> Response",
            why_stable=(
                "The transport is httpx's documented extension point; "
                "handle_request is the single sync I/O chokepoint every client "
                "request passes through."
            ),
        ),
        Seam(
            patch_point="httpx.AsyncHTTPTransport.handle_async_request",
            upstream_api="httpx transport API: AsyncBaseTransport.handle_async_request(request) -> Response",
            why_stable=(
                "The async twin of the transport seam; the single async I/O "
                "chokepoint every AsyncClient request passes through."
            ),
        ),
        Seam(
            patch_point="httpx.Client.__init__ / httpx.AsyncClient.__init__",
            upstream_api="httpx client transport API: Client(transport=...) holds it on ._transport/._mounts",
            why_stable=(
                "Wrapping at client init covers custom transports the class "
                "patch cannot see; it relies only on the documented "
                "transport= argument."
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
        for host_name, provider in _http.known_llm_hosts()
    ]
    return [host, *llm]


def defaults() -> dict[str, Any]:
    """No pack-specific fragment: host targets inherit ``defaults.outbound`` and
    llm: targets inherit ``defaults.llm`` from the Level 0 pack."""
    return {}


# --- install / uninstall -----------------------------------------------------

def install() -> None:
    """Patch the httpx seams. Idempotent; a no-op if httpx is not importable."""
    global _installed
    if _installed:
        return
    try:
        import httpx
    except ImportError:
        return

    _orig["sync_handle"] = httpx.HTTPTransport.handle_request
    _orig["async_handle"] = httpx.AsyncHTTPTransport.handle_async_request
    httpx.HTTPTransport.handle_request = _sync_class_wrapper(_orig["sync_handle"])  # type: ignore[method-assign]
    httpx.AsyncHTTPTransport.handle_async_request = _async_class_wrapper(  # type: ignore[method-assign]
        _orig["async_handle"]
    )

    _orig["sync_init"] = httpx.Client.__init__
    _orig["async_init"] = httpx.AsyncClient.__init__
    httpx.Client.__init__ = _client_init_wrapper(_orig["sync_init"], sync=True)  # type: ignore[method-assign]
    httpx.AsyncClient.__init__ = _client_init_wrapper(_orig["async_init"], sync=False)  # type: ignore[method-assign]

    _installed = True


def uninstall() -> None:
    """Restore every patched httpx original (class methods, client inits, and
    any instance-wrapped custom transports)."""
    global _installed
    if not _installed:
        return
    import httpx

    httpx.HTTPTransport.handle_request = _orig["sync_handle"]  # type: ignore[method-assign]
    httpx.AsyncHTTPTransport.handle_async_request = _orig["async_handle"]  # type: ignore[method-assign]
    httpx.Client.__init__ = _orig["sync_init"]  # type: ignore[method-assign]
    httpx.AsyncClient.__init__ = _orig["async_init"]  # type: ignore[method-assign]

    for transport, attr in list(_wrapped_transports.items()):
        try:
            delattr(transport, attr)
        except AttributeError:
            pass
    _wrapped_transports.clear()
    _orig.clear()
    _installed = False


# --- judgment ----------------------------------------------------------------

def _judge(request: Any) -> tuple[str, str, bool, str | None, str | None]:
    method = request.method
    url = request.url
    host = url.host
    # Pattern-aware target selection (docs/targeting.md): exact host key, else
    # the most specific matching host/URL pattern key, else the bare host —
    # resolved by the backend (native core or stub; see Task 7/SP-1).
    target = _runtime.get_backend().resolve_target(  # type: ignore[union-attr]
        method, host, scheme=url.scheme, port=url.port, path=url.path
    )
    op = f"{method} {host}{url.path}"
    idem_header = _http.idempotency_header(target)
    # args_hash is derived BEFORE the injection decision: the Tier 2 step key
    # (`target#hash`) a resume-reuse peek looks up (contracts/adapter-pack.md
    # rule 3) must be known before deciding what to inject.
    hash_ = _http.derive_args_hash(target, method, str(url), _buffered_body(request))
    recorded_key = _http.peek_recorded_idempotency_key(target, hash_)
    # Injection (contracts/adapter-pack.md "Idempotency-key injection"): mint
    # once, before the first attempt, and set it on the request so every retry
    # attempt resends the SAME header (the request object is reused verbatim
    # by `do_call` on each attempt — see `_run_sync`/`_run_async` below). Inside
    # a Tier 2 flow, a crashed predecessor's key (`recorded_key`, peeked above)
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


def _buffered_body(request: Any) -> bytes | None:
    # Include the body in the cache key only when it is already buffered, so we
    # never consume a single-use streaming request body (matches Node, which
    # hashes only buffered/string bodies). httpx buffers non-streaming request
    # content into `_content` at construction; a streaming body has none. An
    # empty (no-body) GET collapses to None so it hashes identically to the
    # requests/Node judges (no trailing separator).
    return (request.content or None) if hasattr(request, "_content") else None


def _is_response(obj: Any) -> bool:
    import httpx

    return isinstance(obj, httpx.Response)


def _classify(err: BaseException) -> str:
    import httpx

    if isinstance(err, httpx.TimeoutException):
        return "timeout"
    if isinstance(err, httpx.TransportError):  # ConnectError/NetworkError/ProtocolError/…
        return "conn"
    return "other"


# --- response (de)serialization for the core payload ------------------------

def _ok_payload(resp: Any, cacheable: bool) -> dict[str, Any]:
    """A JSON envelope of `resp` for the core payload (sync). The body is
    buffered only for cacheable calls (so a non-cached streaming body is never
    forced); the live response is returned unchanged on the success path."""
    body = None
    if cacheable:
        try:
            body = resp.read()  # buffers the transport stream into resp.content
        except Exception:
            body = None
    return _http.response_envelope(resp.status_code, _headers(resp), body)


async def _ok_payload_async(resp: Any, cacheable: bool) -> dict[str, Any]:
    """Async twin of `_ok_payload` — buffers the body via `aread` on the loop."""
    body = None
    if cacheable:
        try:
            body = await resp.aread()
        except Exception:
            body = None
    return _http.response_envelope(resp.status_code, _headers(resp), body)


def _headers(resp: Any) -> list[tuple[str, str]]:
    try:
        return list(resp.headers.items())
    except Exception:
        return []


def _rebuild(payload: Any) -> Any:
    """Rebuild an httpx.Response from an envelope (a cache-hit replay)."""
    import httpx

    p = payload if isinstance(payload, dict) else {}
    body = base64.b64decode(p["body_b64"]) if isinstance(p.get("body_b64"), str) else b""
    return httpx.Response(status_code=int(p.get("status", 200)), headers=p.get("headers", []), content=body)


# --- LLM budget + fallback helpers (shared by the sync and async seams) -----

def _llm_generate_gate(target: str, method: str) -> tuple[bool, int | None]:
    """Whether `target`/`method` is an ``llm:*`` generate call, and its parsed
    budget cap in cents (``None`` if unset/not an llm target)."""
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
    model = _llm_policy.derive_request_model(str(request.url), _buffered_body(request))
    _llm_policy.record_spend(target, _llm_policy.estimate_cost_usd(model, usage))


def _next_hop_request(request: Any, next_model: str) -> Any | None:
    """A fresh `httpx.Request` for the next fallback hop, or `None` when the
    current request's shape can't be rewritten (see `_llm_policy`). Drops
    `content-length` from the carried-over headers — the rewritten body is a
    different length, and `httpx.Request` recomputes it correctly from
    `content` when the header is absent."""
    import httpx

    rewritten = _llm_policy.rewrite_model(str(request.url), _buffered_body(request), next_model)
    if rewritten is None:
        return None
    new_url, new_body = rewritten
    headers = [(k, v) for k, v in request.headers.items() if k.lower() != "content-length"]
    return httpx.Request(method=request.method, url=new_url, headers=headers, content=new_body)


# --- sync seam ---------------------------------------------------------------

_TIMEOUT_KEYS = ("connect", "read", "write", "pool")


def _inject_policy_timeout(request: Any, target: str) -> None:
    """Inject the resolved policy ``timeout`` (issue #32) as a per-request
    deadline into ``request.extensions["timeout"]`` — the dict httpx's own
    ``Client`` already populates there (``{"connect", "read", "write",
    "pool"}``, each a float or ``None`` for no timeout) and that the
    transport seam we patch (``HTTPTransport.handle_request``) passes
    straight through to httpcore, which is what actually enforces it. A
    Transport has no other way to preempt a blocking call — the async seam
    needs no equivalent because the core's own timer can preempt an awaited
    future. Tighter wins per sub-key, same philosophy as urllib_pack's
    ``_compose_timeout``."""
    policy_s = _http.resolve_timeout_s(target)
    if policy_s is None:
        return
    existing = request.extensions.get("timeout") or {}
    request.extensions["timeout"] = {
        key: policy_s if existing.get(key) is None else min(existing[key], policy_s)
        for key in _TIMEOUT_KEYS
    }


def _run_sync(call_with: Callable[[Any], Any], request: Any) -> Any:
    backend = _runtime.get_backend()
    if backend is None:
        return call_with(request)  # disabled / uninstalled: transparent
    discovery = _runtime.get_discovery()

    current = request
    hop = 0
    live: dict[str, Any] = {"ok": None, "transient": None, "exc": None}
    outcome: dict[str, Any] = {}

    while True:
        target, op, idempotent, hash_, injected = _judge(current)
        env = _http.build_request(target, op, idempotent, hash_)
        _inject_policy_timeout(current, target)
        # Buffer the body ONLY when a cache ttl or a poll table is actually
        # configured for the target (mirrors Node's fetch gate), OR a budget
        # is configured (usage accounting needs the response body — see
        # `_llm_policy`).
        is_llm, cap_cents = _llm_generate_gate(target, current.method)
        if hop == 0 and cap_cents is not None and _llm_policy.spent_cents(target) >= cap_cents:
            raise _budget_blocked_error(target, cap_cents, discovery)
        track_usage = cap_cents is not None
        cacheable = hash_ is not None and _http.buffer_body_configured(target)
        buffer_body = cacheable or track_usage
        live = {"ok": None, "transient": None, "exc": None}

        def effect(_attempt: int, _current: Any = current, _buffer: bool = buffer_body) -> dict[str, Any]:
            try:
                resp = call_with(_current)
            except Exception as err:  # not BaseException: let exit/interrupt fly
                live["exc"] = err
                return _http.thrown_error(err, _classify(err))
            live["exc"] = None
            if _http.is_transient_status(resp.status_code):
                if live["transient"] is not None and live["transient"] is not resp:
                    _close_sync(live["transient"])
                live["transient"] = resp
                return _http.transient_error(resp.status_code, resp.headers.get("retry-after"))
            if live["transient"] is not None and live["transient"] is not resp:
                _close_sync(live["transient"])
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
        _close_sync(live["transient"])  # dangling transient before a thrown transport error
    if action == "return":
        return value
    raise value


def _sync_class_wrapper(orig: Callable[..., Any]) -> Callable[..., Any]:
    @functools.wraps(orig)
    def handle_request(self: Any, request: Any) -> Any:
        return _run_sync(lambda req: orig(self, req), request)

    handle_request.__keel_wrapped__ = True  # type: ignore[attr-defined]
    return handle_request


def _sync_instance_wrapper(orig_bound: Callable[[Any], Any]) -> Callable[[Any], Any]:
    def handle_request(request: Any) -> Any:
        return _run_sync(lambda req: orig_bound(req), request)

    handle_request.__keel_wrapped__ = True  # type: ignore[attr-defined]
    return handle_request


def _close_sync(resp: Any) -> None:
    try:
        resp.close()
    except Exception:
        pass


# --- async seam --------------------------------------------------------------

async def _run_async(call_with: Callable[[Any], Any], request: Any) -> Any:
    backend = _runtime.get_backend()
    if backend is None:
        return await call_with(request)  # disabled / uninstalled: transparent
    discovery = _runtime.get_discovery()
    exec_async = getattr(backend, "execute_async", None)

    current = request
    hop = 0
    live: dict[str, Any] = {"ok": None, "transient": None, "exc": None}
    outcome: dict[str, Any] = {}

    while True:
        target, op, idempotent, hash_, injected = _judge(current)
        env = _http.build_request(target, op, idempotent, hash_)
        is_llm, cap_cents = _llm_generate_gate(target, current.method)
        if hop == 0 and cap_cents is not None and _llm_policy.spent_cents(target) >= cap_cents:
            raise _budget_blocked_error(target, cap_cents, discovery)
        track_usage = cap_cents is not None
        cacheable = hash_ is not None and _http.buffer_body_configured(target)
        buffer_body = cacheable or track_usage
        live = {"ok": None, "transient": None, "exc": None}

        started = time.perf_counter()
        if callable(exec_async):
            # NATIVE async path (Task 14 item 3): drive the effect directly on the
            # caller's loop via keel_core.execute_async — no worker thread, no
            # run_coroutine_threadsafe. The real async core awaits our coroutine.
            async def aeffect(_attempt: int, _current: Any = current, _buffer: bool = buffer_body) -> dict[str, Any]:
                try:
                    resp = await call_with(_current)
                except Exception as err:
                    live["exc"] = err
                    return _http.thrown_error(err, _classify(err))
                live["exc"] = None
                if _http.is_transient_status(resp.status_code):
                    if live["transient"] is not None and live["transient"] is not resp:
                        await _aclose(live["transient"])
                    live["transient"] = resp
                    return _http.transient_error(resp.status_code, resp.headers.get("retry-after"))
                if live["transient"] is not None and live["transient"] is not resp:
                    await _aclose(live["transient"])
                live["transient"] = None
                live["ok"] = resp
                return {"status": "ok", "payload": await _ok_payload_async(resp, _buffer)}

            # Thread the resolved idempotency key through ONLY while a Tier 2
            # flow is open (contracts/adapter-pack.md rule 3) — the parameter
            # is native/flow-only; the plain (non-flow) Tier 1 path is
            # untouched (this branch runs only under the native async core,
            # so `in_active_flow()` here means a real, journal-backed flow).
            if _runtime.in_active_flow():
                outcome = await exec_async(env, aeffect, idempotency_key=injected)
            else:
                outcome = await exec_async(env, aeffect)
        else:
            # STUB async path: the synchronous stub cannot await, so each attempt is
            # driven in a worker thread that marshals the await back onto this loop.
            loop = asyncio.get_running_loop()

            def effect(_attempt: int, _current: Any = current, _buffer: bool = buffer_body) -> dict[str, Any]:
                future = asyncio.run_coroutine_threadsafe(call_with(_current), loop)
                try:
                    resp = future.result()
                except Exception as err:
                    live["exc"] = err
                    return _http.thrown_error(err, _classify(err))
                live["exc"] = None
                if _http.is_transient_status(resp.status_code):
                    if live["transient"] is not None and live["transient"] is not resp:
                        _aclose_threadsafe(live["transient"], loop)
                    live["transient"] = resp
                    return _http.transient_error(resp.status_code, resp.headers.get("retry-after"))
                if live["transient"] is not None and live["transient"] is not resp:
                    _aclose_threadsafe(live["transient"], loop)
                live["transient"] = None
                live["ok"] = resp
                payload = asyncio.run_coroutine_threadsafe(_ok_payload_async(resp, _buffer), loop).result()
                return {"status": "ok", "payload": payload}

            outcome = await loop.run_in_executor(
                None, lambda: _http.call_execute(backend, env, effect, injected)
            )
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
        await _aclose(live["transient"])
    if action == "return":
        return value
    raise value


def _async_class_wrapper(orig: Callable[..., Any]) -> Callable[..., Any]:
    @functools.wraps(orig)
    async def handle_async_request(self: Any, request: Any) -> Any:
        return await _run_async(lambda req: orig(self, req), request)

    handle_async_request.__keel_wrapped__ = True  # type: ignore[attr-defined]
    return handle_async_request


def _async_instance_wrapper(orig_bound: Callable[[Any], Any]) -> Callable[[Any], Any]:
    async def handle_async_request(request: Any) -> Any:
        return await _run_async(lambda req: orig_bound(req), request)

    handle_async_request.__keel_wrapped__ = True  # type: ignore[attr-defined]
    return handle_async_request


async def _aclose(resp: Any) -> None:
    try:
        await resp.aclose()
    except Exception:
        pass


def _aclose_threadsafe(resp: Any, loop: asyncio.AbstractEventLoop) -> None:
    try:
        asyncio.run_coroutine_threadsafe(resp.aclose(), loop).result()
    except Exception:
        pass


# --- client-init transport wrapping (covers custom transports) ---------------

def _client_init_wrapper(orig_init: Callable[..., Any], *, sync: bool) -> Callable[..., Any]:
    @functools.wraps(orig_init)
    def __init__(self: Any, *args: Any, **kwargs: Any) -> None:
        orig_init(self, *args, **kwargs)
        _wrap_client_transports(self, sync=sync)

    __init__.__keel_wrapped__ = True  # type: ignore[attr-defined]
    return __init__


def _wrap_client_transports(client: Any, *, sync: bool) -> None:
    transports = []
    primary = getattr(client, "_transport", None)
    if primary is not None:
        transports.append(primary)
    for mounted in getattr(client, "_mounts", {}).values():
        if mounted is not None:
            transports.append(mounted)
    for transport in transports:
        _wrap_transport_instance(transport, sync=sync)


def _wrap_transport_instance(transport: Any, *, sync: bool) -> None:
    attr = "handle_request" if sync else "handle_async_request"
    handler = getattr(transport, attr, None)
    if handler is None:
        return
    # Already covered — either the default class is patched (bound method's
    # __func__ carries the marker) or we wrapped this instance already.
    if getattr(handler, "__keel_wrapped__", False):
        return
    wrapper = (
        _sync_instance_wrapper(handler) if sync else _async_instance_wrapper(handler)
    )
    try:
        setattr(transport, attr, wrapper)
        _wrapped_transports[transport] = attr
    except (AttributeError, TypeError):
        pass  # slotted/frozen transport: leave unwrapped (Level 0: nothing unsafe)


def _is_pinned(version: str) -> bool:
    return any(version == p or version.startswith(p + ".") for p in _PINNED)


__all__ = ["MODULE", "NAME", "detect", "seams", "targets", "defaults", "install", "uninstall"]
