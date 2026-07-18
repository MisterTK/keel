"""The aiohttp adapter pack: resilience for every ``aiohttp.ClientSession``
call, zero code changes, through its own request seam.

Seam (narrowest stable): ``aiohttp.ClientSession._request`` — every
``ClientSession`` verb (``get``/``post``/``put``/``patch``/``delete``/
``head``/``options``) is a thin wrapper that calls
``self._request(method, url, **kwargs)`` and returns its awaitable, whether
driven directly with ``await`` or through the
``async with session.get(...) as resp:`` context-manager sugar — both funnel
through this one method, so patching it on the class covers every request
shape aiohttp exposes. aiohttp is async-only, so there is no separate sync
seam; the wrapper always drives the async engine path (mirrors ``httpx_pack``'s
async seam — the worker-thread bridge used until the native async core lands
under a given backend — see its module doc for why that shape is correct).

All judgment (target/idempotency/args_hash/error-class) lives in ``_http`` and
is shared with httpx/requests/urllib3 and the Node twin. The one thing this
pack does NOT share with httpx/requests is how it rebuilds a cache-hit
response: ``aiohttp.ClientResponse`` is not a publicly constructible type (its
``__init__`` requires live connection/writer/timer internals owned by the
transport), unlike ``httpx.Response``/``requests.Response``/
``urllib3.HTTPResponse``, which the sibling packs reconstruct exactly. Rather
than reach into aiohttp's private response internals (fragile across
versions — exactly the "adapter fragility" the contract prices in), a cache
HIT is delivered as :class:`_ReplayedResponse`, a small duck-typed stand-in
implementing the subset of the public surface a replayed call actually needs
(status/headers/read/text/json/close/async-context-manager). This is a
narrow, documented gap versus true byte-transparency (``isinstance(resp,
aiohttp.ClientResponse)`` is false for a replayed response), scoped ONLY to
the dev-cache-hit path — a live call always returns the real object,
unchanged.
"""

from __future__ import annotations

import asyncio
import base64
import functools
import importlib.metadata
import importlib.util
import json
import time
from typing import Any, Callable, Iterable

from .. import _runtime
from . import _http
from ._pack import Detection, Seam, TargetDecl

MODULE = "aiohttp"
NAME = "aiohttp"

#: Versions this pack certifies via contract tests (prefix match).
_PINNED = ("3.13", "3.14")

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
            patch_point="aiohttp.ClientSession._request",
            upstream_api="aiohttp client API: ClientSession._request(method, url, **kwargs) -> ClientResponse",
            why_stable=(
                "Every ClientSession verb (get/post/put/patch/delete/head/"
                "options) is a thin wrapper that calls self._request(...); it "
                "is the single async dispatch point all session traffic "
                "passes through, whether awaited directly or used as "
                "`async with session.get(...) as resp:`."
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
    """No pack-specific fragment: host targets inherit ``defaults.outbound``
    and llm: targets inherit ``defaults.llm`` from the Level 0 pack."""
    return {}


# --- install / uninstall -----------------------------------------------------


def install() -> None:
    """Patch the aiohttp seam. Idempotent; a no-op if aiohttp is not
    importable."""
    global _installed
    if _installed:
        return
    try:
        import aiohttp
    except ImportError:
        return
    _orig["request"] = aiohttp.ClientSession._request
    aiohttp.ClientSession._request = _request_wrapper(_orig["request"])  # type: ignore[method-assign]
    _installed = True


def uninstall() -> None:
    global _installed
    if not _installed:
        return
    import aiohttp

    aiohttp.ClientSession._request = _orig["request"]  # type: ignore[method-assign]
    _orig.clear()
    _installed = False


# --- judgment ----------------------------------------------------------------


def _header_names(headers: Any) -> Iterable[str]:
    if headers is None:
        return ()
    keys = getattr(headers, "keys", None)
    if callable(keys):
        return tuple(keys())
    try:
        return tuple(k for k, _ in headers)
    except (TypeError, ValueError):
        return ()


def _body_for_hash(kwargs: dict[str, Any]) -> bytes | str | None:
    """Cache-key material for a request body, without consuming a stream.
    ``json=`` is a live Python object (not yet serialized); ``_http``'s
    canonicalizer re-parses+re-sorts it regardless of our own serialization
    order, so the exact separators/order used here do not matter. ``data=``
    is hashed only when already a buffered bytes/str — a dict (multipart/
    form), ``FormData``, or a file/stream is never consumed just to derive a
    cache key."""
    json_body = kwargs.get("json")
    if json_body is not None:
        try:
            return json.dumps(json_body)
        except (TypeError, ValueError):
            return None
    data = kwargs.get("data")
    if isinstance(data, (bytes, bytearray)):
        return bytes(data)
    if isinstance(data, str):
        return data
    return None


def _resolve_url(session: Any, str_or_url: Any) -> Any:
    """The absolute request URL, resolved the same way aiohttp itself would
    (honoring ``ClientSession(base_url=...)``). ``_build_url`` is a private
    instance method but has no side effects; if a future aiohttp removes it,
    fall back to treating ``str_or_url`` as already absolute (best-effort,
    matching the ``detect()`` confidence story for an unpinned version)."""
    build = getattr(session, "_build_url", None)
    if callable(build):
        try:
            return build(str_or_url)
        except Exception:
            pass
    return str_or_url


def _judge(session: Any, method: str, str_or_url: Any, kwargs: dict[str, Any]) -> tuple[str, str, bool, str | None]:
    method = method.upper()
    url = _resolve_url(session, str_or_url)
    host = getattr(url, "host", None) or ""
    path = getattr(url, "path", None) or ""
    target = _http.resolve_target(host)
    op = f"{method} {host}{path}"
    idem_header = _http.idempotency_header(target)
    idempotent = _http.is_idempotent(method, _header_names(kwargs.get("headers")), idem_header)
    hash_ = _http.derive_args_hash(target, method, str(url), _body_for_hash(kwargs))
    return target, op, idempotent, hash_


def _classify(err: BaseException) -> str:
    import aiohttp

    if isinstance(err, TimeoutError):  # asyncio.TimeoutError is TimeoutError (3.11+)
        return "timeout"
    if isinstance(err, aiohttp.ClientConnectionError):
        return "conn"
    return "other"


# --- response (de)serialization for the core payload ------------------------


async def _ok_payload(resp: Any, cacheable: bool) -> dict[str, Any]:
    body = None
    if cacheable:
        try:
            body = await resp.read()  # aiohttp caches the buffered body internally
        except Exception:
            body = None
    try:
        headers = list(resp.headers.items())
    except Exception:
        headers = []
    return _http.response_envelope(resp.status, headers, body)


class _CIHeaders:
    """A tiny case-insensitive header mapping for :class:`_ReplayedResponse`
    (avoids a hard ``multidict`` import for the replay-only facade; a live
    call always reads the real aiohttp/multidict headers)."""

    def __init__(self, items: Iterable[tuple[str, str]]) -> None:
        self._items = list(items)

    def get(self, key: str, default: Any = None) -> Any:
        lk = key.lower()
        for k, v in self._items:
            if k.lower() == lk:
                return v
        return default

    def items(self) -> list[tuple[str, str]]:
        return list(self._items)

    def __iter__(self) -> Any:
        return iter(k for k, _ in self._items)

    def __contains__(self, key: str) -> bool:
        return self.get(key) is not None


def _charset(content_type: str | None) -> str | None:
    if not content_type or ";" not in content_type:
        return None
    for part in content_type.split(";")[1:]:
        part = part.strip()
        if part.lower().startswith("charset="):
            return part.split("=", 1)[1].strip().strip('"')
    return None


class _ReplayedResponse:
    """A duck-typed stand-in for ``aiohttp.ClientResponse``, delivered ONLY on
    a dev-cache hit (see module docs for why this is not the real type)."""

    def __init__(self, status: int, headers: list[tuple[str, str]], body: bytes) -> None:
        self.status = status
        self.reason = None
        self.headers = _CIHeaders(headers)
        self._body = body
        self.ok = 200 <= status < 400
        self.method = "GET"
        self.content_type = (self.headers.get("Content-Type") or "").split(";")[0].strip() or None
        self._closed = False

    async def read(self) -> bytes:
        return self._body

    async def text(self, encoding: str | None = None) -> str:
        return self._body.decode(encoding or _charset(self.headers.get("Content-Type")) or "utf-8")

    async def json(self, *, encoding: str | None = None, loads: Callable[[str], Any] = json.loads, **_: Any) -> Any:
        return loads(await self.text(encoding))

    @property
    def closed(self) -> bool:
        return self._closed

    def close(self) -> None:
        self._closed = True

    def release(self) -> None:
        self._closed = True

    def raise_for_status(self) -> None:
        if not self.ok:
            raise _http.KeelError("KEEL-E040", f"keel: replayed response has status {self.status}")

    async def __aenter__(self) -> "_ReplayedResponse":
        return self

    async def __aexit__(self, *exc: Any) -> None:
        self.close()


def _rebuild(payload: Any) -> Any:
    p = payload if isinstance(payload, dict) else {}
    body = base64.b64decode(p["body_b64"]) if isinstance(p.get("body_b64"), str) else b""
    return _ReplayedResponse(int(p.get("status", 200)), p.get("headers", []), body)


async def _release(resp: Any) -> None:
    try:
        resp.release()
    except Exception:
        pass


def _release_threadsafe(resp: Any, loop: asyncio.AbstractEventLoop) -> None:
    try:
        asyncio.run_coroutine_threadsafe(_release(resp), loop).result()
    except Exception:
        pass


# --- seam ---------------------------------------------------------------------


async def _run(self: Any, orig: Callable[..., Any], method: str, str_or_url: Any, kwargs: dict[str, Any]) -> Any:
    backend = _runtime.get_backend()
    if backend is None:
        return await orig(self, method, str_or_url, **kwargs)  # disabled / uninstalled: transparent
    discovery = _runtime.get_discovery()
    target, op, idempotent, hash_ = _judge(self, method, str_or_url, kwargs)
    env = _http.build_request(target, op, idempotent, hash_)
    # Buffer the body ONLY when a cache ttl OR a poll table is actually
    # configured for the target (mirrors Node's fetch gate and the sibling
    # HTTP packs): with neither, there is nothing to store or judge, so a
    # streaming/SSE GET passes through unbuffered at Level 0.
    cacheable = hash_ is not None and _http.buffer_body_configured(target)
    live: dict[str, Any] = {"ok": None, "transient": None, "exc": None}
    exec_async = getattr(backend, "execute_async", None)

    started = time.perf_counter()
    if callable(exec_async):
        # NATIVE async path: the real core awaits our coroutine directly on
        # the caller's loop (mirrors httpx_pack._run_async).
        async def aeffect(_attempt: int) -> dict[str, Any]:
            try:
                resp = await orig(self, method, str_or_url, **kwargs)
            except Exception as err:
                live["exc"] = err
                return _http.thrown_error(err, _classify(err))
            live["exc"] = None
            if _http.is_transient_status(resp.status):
                if live["transient"] is not None and live["transient"] is not resp:
                    await _release(live["transient"])
                live["transient"] = resp
                return _http.transient_error(resp.status, resp.headers.get("Retry-After"))
            if live["transient"] is not None and live["transient"] is not resp:
                await _release(live["transient"])
            live["transient"] = None
            live["ok"] = resp
            return {"status": "ok", "payload": await _ok_payload(resp, cacheable)}

        outcome = await exec_async(env, aeffect)
    else:
        # STUB async path: the synchronous stub cannot await, so each attempt
        # is driven in a worker thread that marshals the await back onto this
        # loop (mirrors httpx_pack._run_async).
        loop = asyncio.get_running_loop()

        def effect(_attempt: int) -> dict[str, Any]:
            future = asyncio.run_coroutine_threadsafe(orig(self, method, str_or_url, **kwargs), loop)
            try:
                resp = future.result()
            except Exception as err:
                live["exc"] = err
                return _http.thrown_error(err, _classify(err))
            live["exc"] = None
            if _http.is_transient_status(resp.status):
                if live["transient"] is not None and live["transient"] is not resp:
                    _release_threadsafe(live["transient"], loop)
                live["transient"] = resp
                return _http.transient_error(resp.status, resp.headers.get("Retry-After"))
            if live["transient"] is not None and live["transient"] is not resp:
                _release_threadsafe(live["transient"], loop)
            live["transient"] = None
            live["ok"] = resp
            payload = asyncio.run_coroutine_threadsafe(_ok_payload(resp, cacheable), loop).result()
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
        await _release(live["transient"])
    if action == "return":
        return value
    raise value


def _request_wrapper(orig: Callable[..., Any]) -> Callable[..., Any]:
    @functools.wraps(orig)
    async def _request(self: Any, method: str, str_or_url: Any, **kwargs: Any) -> Any:
        return await _run(self, orig, method, str_or_url, kwargs)

    _request.__keel_wrapped__ = True  # type: ignore[attr-defined]
    return _request


def _is_pinned(version: str) -> bool:
    return any(version == p or version.startswith(p + ".") for p in _PINNED)


__all__ = ["MODULE", "NAME", "detect", "seams", "targets", "defaults", "install", "uninstall"]
