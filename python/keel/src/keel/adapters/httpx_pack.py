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
import time
import weakref
from typing import Any, Callable

from .. import _runtime
from . import _http
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
        for host_name, provider in _http.LLM_HOST_PROVIDERS.items()
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

def _judge(request: Any) -> tuple[str, str, bool, str | None]:
    method = request.method
    url = request.url
    host = url.host
    # Pattern-aware target selection (docs/targeting.md): exact host key, else
    # the most specific matching host/URL pattern key, else the bare host.
    target = _http.resolve_policy_target(
        method, host, scheme=url.scheme, port=url.port, path=url.path
    )
    op = f"{method} {host}{url.path}"
    idem_header = _http.idempotency_header(target)
    idempotent = _http.is_idempotent(method, request.headers.keys(), idem_header)
    hash_ = _http.derive_args_hash(target, method, str(url), _buffered_body(request))
    return target, op, idempotent, hash_


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


# --- sync seam ---------------------------------------------------------------

def _run_sync(do_call: Callable[[], Any], request: Any) -> Any:
    backend = _runtime.get_backend()
    if backend is None:
        return do_call()  # disabled / uninstalled: transparent
    discovery = _runtime.get_discovery()
    target, op, idempotent, hash_ = _judge(request)
    env = _http.build_request(target, op, idempotent, hash_)
    # Buffer the body ONLY when a cache ttl is actually configured for the target
    # (mirrors Node's fetch gate): with no cache there is nothing to store, so a
    # streaming/SSE GET must pass through unbuffered at Level 0.
    cacheable = hash_ is not None and _http.cache_configured(target)
    # Live objects kept side-band so the core payload stays JSON: the ok winner,
    # the last superseded transient (closed on supersede), and a thrown error.
    live: dict[str, Any] = {"ok": None, "transient": None, "exc": None}

    def effect(_attempt: int) -> dict[str, Any]:
        try:
            resp = do_call()
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
        return {"status": "ok", "payload": _ok_payload(resp, cacheable)}

    started = time.perf_counter()
    outcome = backend.execute(env, effect)
    latency_ms = round((time.perf_counter() - started) * 1000)
    if discovery is not None:
        discovery.record(target, outcome, latency_ms)

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
        return _run_sync(lambda: orig(self, request), request)

    handle_request.__keel_wrapped__ = True  # type: ignore[attr-defined]
    return handle_request


def _sync_instance_wrapper(orig_bound: Callable[[Any], Any]) -> Callable[[Any], Any]:
    def handle_request(request: Any) -> Any:
        return _run_sync(lambda: orig_bound(request), request)

    handle_request.__keel_wrapped__ = True  # type: ignore[attr-defined]
    return handle_request


def _close_sync(resp: Any) -> None:
    try:
        resp.close()
    except Exception:
        pass


# --- async seam --------------------------------------------------------------

async def _run_async(make_coro: Callable[[], Any], request: Any) -> Any:
    backend = _runtime.get_backend()
    if backend is None:
        return await make_coro()  # disabled / uninstalled: transparent
    discovery = _runtime.get_discovery()
    target, op, idempotent, hash_ = _judge(request)
    env = _http.build_request(target, op, idempotent, hash_)
    # Buffer the body ONLY when a cache ttl is actually configured for the target
    # (mirrors Node's fetch gate): with no cache there is nothing to store, so a
    # streaming/SSE GET must pass through unbuffered at Level 0.
    cacheable = hash_ is not None and _http.cache_configured(target)
    live: dict[str, Any] = {"ok": None, "transient": None, "exc": None}
    exec_async = getattr(backend, "execute_async", None)

    started = time.perf_counter()
    if callable(exec_async):
        # NATIVE async path (Task 14 item 3): drive the effect directly on the
        # caller's loop via keel_core.execute_async — no worker thread, no
        # run_coroutine_threadsafe. The real async core awaits our coroutine.
        async def aeffect(_attempt: int) -> dict[str, Any]:
            try:
                resp = await make_coro()
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
            return {"status": "ok", "payload": await _ok_payload_async(resp, cacheable)}

        outcome = await exec_async(env, aeffect)
    else:
        # STUB async path: the synchronous stub cannot await, so each attempt is
        # driven in a worker thread that marshals the await back onto this loop.
        loop = asyncio.get_running_loop()

        def effect(_attempt: int) -> dict[str, Any]:
            future = asyncio.run_coroutine_threadsafe(make_coro(), loop)
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
            payload = asyncio.run_coroutine_threadsafe(_ok_payload_async(resp, cacheable), loop).result()
            return {"status": "ok", "payload": payload}

        outcome = await loop.run_in_executor(None, lambda: backend.execute(env, effect))
    latency_ms = round((time.perf_counter() - started) * 1000)
    if discovery is not None:
        discovery.record(target, outcome, latency_ms)

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
        return await _run_async(lambda: orig(self, request), request)

    handle_async_request.__keel_wrapped__ = True  # type: ignore[attr-defined]
    return handle_async_request


def _async_instance_wrapper(orig_bound: Callable[[Any], Any]) -> Callable[[Any], Any]:
    async def handle_async_request(request: Any) -> Any:
        return await _run_async(lambda: orig_bound(request), request)

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
