"""The urllib3 adapter pack: resilience for every direct ``urllib3`` call
through its own connection-pool seam.

Seam (narrowest stable): ``urllib3.HTTPConnectionPool.urlopen`` —
``HTTPSConnectionPool`` inherits it unchanged, and ``PoolManager.urlopen``
resolves a pool then calls this same method, so patching the base class
covers every direct usage style (a bare ``HTTPConnectionPool``, a
``PoolManager``, or any third-party library built directly on urllib3 that
does not vendor/guard its own instance).

Double-wrap guard (the reason this pack is not simply "another HTTP pack"):
``requests`` vendors its OWN use of urllib3's connection pools internally, and
botocore's default HTTP session (``botocore.httpsession.URLLib3Session``) does
too — so a call already judged and retried at the ``requests``/``boto3`` seam
would, without a guard, ALSO pass through this seam, nesting a second Keel
retry loop inside the first. ``requests_pack``/``boto3_pack`` run their
underlying call through ``_http.run_owned``; this pack checks
``_http.seam_owned()`` at the top of the wrapper and, when true, calls straight
through untouched — no judgment, no discovery record, no core `execute` at
all. Only a DIRECT urllib3 call (no owning seam above it) is judged here.

All judgment (target/idempotency/args_hash/error-class) lives in ``_http`` and
is shared with httpx/requests/aiohttp and the Node twin. urllib3 has no
distinct async API, so this pack has a single, synchronous seam.

Retry-compounding note (unlike httpx, which has none, and requests, which
sets ``max_retries=0`` on its adapters by default): urllib3's OWN default
``Retry(total=3)`` already retries connect/read errors internally for
idempotent-ish methods, AND — regardless of ``status_forcelist`` — retries a
429/503/413 response that carries a ``Retry-After`` header
(``Retry.RETRY_AFTER_STATUS_CODES``), all before this seam's ``do_call()``
ever returns or raises. A DIRECT urllib3 caller who leaves those defaults on
can therefore see a transient fault "absorbed" below Keel — reported as one
successful Keel attempt rather than a Keel-level retry — a bounded (not
unbounded) compounding, the same accepted-limitation shape documented in
``boto3_pack`` for botocore's own retry loop. A caller who wants Keel to be
the sole retry authority can pass ``retries=False`` (or a narrower ``Retry``)
per call, exactly as this pack's own test suite does to isolate Keel's retry
behavior.
"""

from __future__ import annotations

import functools
import importlib.metadata
import importlib.util
import inspect
import time
from typing import Any, Callable

from .. import _runtime
from . import _http
from ._pack import Detection, Seam, TargetDecl

MODULE = "urllib3"
NAME = "urllib3"

#: Versions this pack certifies via contract tests (prefix match).
_PINNED = ("2.6", "2.7")

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
            patch_point="urllib3.HTTPConnectionPool.urlopen",
            upstream_api="urllib3 connection-pool API: HTTPConnectionPool.urlopen(method, url, ...) -> BaseHTTPResponse",
            why_stable=(
                "HTTPSConnectionPool inherits urlopen unchanged and "
                "PoolManager.urlopen resolves a pool then calls this same "
                "method, so patching the base class covers every urllib3 "
                "usage style; a call already owned by the requests/boto3 "
                "seam is skipped (see module docs)."
            ),
        ),
    ]


def targets() -> list[TargetDecl]:
    host = TargetDecl(
        pattern="<connection-pool host>",
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
    """Patch the urllib3 seam. Idempotent; a no-op if urllib3 is not
    importable."""
    global _installed
    if _installed:
        return
    try:
        import urllib3
    except ImportError:
        return
    _orig["urlopen"] = urllib3.HTTPConnectionPool.urlopen
    urllib3.HTTPConnectionPool.urlopen = _urlopen_wrapper(_orig["urlopen"])  # type: ignore[method-assign]
    _installed = True


def uninstall() -> None:
    global _installed
    if not _installed:
        return
    import urllib3

    urllib3.HTTPConnectionPool.urlopen = _orig["urlopen"]  # type: ignore[method-assign]
    _orig.clear()
    _installed = False


# --- judgment ----------------------------------------------------------------


def _header_names(headers: Any) -> tuple[str, ...]:
    if headers is None:
        return ()
    keys = getattr(headers, "keys", None)
    if callable(keys):
        return tuple(keys())
    try:
        return tuple(k for k, _ in headers)
    except (TypeError, ValueError):
        return ()


def _judge(pool: Any, method: str, url: str, headers: Any, body: Any) -> tuple[str, str, bool, str | None]:
    method = method.upper()
    host = getattr(pool, "host", "") or ""
    target = _http.resolve_target(host)
    op = f"{method} {host}{url}"
    idem_header = _http.idempotency_header(target)
    idempotent = _http.is_idempotent(method, _header_names(headers), idem_header)
    scheme = getattr(pool, "scheme", None) or "http"
    port = getattr(pool, "port", None)
    netloc = f"{host}:{port}" if port else host
    full_url = f"{scheme}://{netloc}{url}"
    # Only a buffered (bytes/str) body is hashed — a file-like/generator body
    # (chunked upload) is never consumed just to derive a cache key.
    hash_body = body if isinstance(body, (bytes, bytearray, str)) else None
    hash_ = _http.derive_args_hash(target, method, full_url, hash_body)
    return target, op, idempotent, hash_


def _classify(err: BaseException) -> str:
    import urllib3.exceptions as ue

    cause: BaseException = err
    if isinstance(err, ue.MaxRetryError) and err.reason is not None:
        cause = err.reason  # unwrap urllib3's own exhausted-retry wrapper
    if isinstance(cause, (ue.ConnectTimeoutError, ue.ReadTimeoutError, TimeoutError)):
        return "timeout"
    if isinstance(cause, (ue.NewConnectionError, ue.ProtocolError, ConnectionError, OSError)):
        return "conn"
    return "other"


# --- response (de)serialization for the core payload ------------------------


def _ok_payload(resp: Any, cacheable: bool) -> dict[str, Any]:
    body = None
    if cacheable:
        try:
            body = resp.data  # already buffered (preload_content=True checked by the caller)
        except Exception:
            body = None
    try:
        headers = list(resp.headers.items())
    except Exception:
        headers = []
    return _http.response_envelope(resp.status, headers, body)


def _rebuild(payload: Any) -> Any:
    """Rebuild a ``urllib3.HTTPResponse`` from an envelope (a cache-hit
    replay), via its public, documented constructor — unlike some client
    response types (see ``aiohttp_pack``), this one is meant to be built by
    hand (urllib3's own test suite does exactly this)."""
    import urllib3

    p = payload if isinstance(payload, dict) else {}
    body = _b64decode(p.get("body_b64"))
    return urllib3.HTTPResponse(
        body=body,
        headers=p.get("headers", []),
        status=int(p.get("status", 200)),
        preload_content=True,
    )


def _b64decode(value: Any) -> bytes:
    import base64

    return base64.b64decode(value) if isinstance(value, str) else b""


def _close(resp: Any) -> None:
    try:
        resp.release_conn()
    except Exception:
        pass
    try:
        resp.close()
    except Exception:
        pass


# --- seam ---------------------------------------------------------------------


def _run(pool: Any, do_call: Callable[[], Any], method: str, url: str, headers: Any, body: Any, preload_content: bool) -> Any:
    backend = _runtime.get_backend()
    if backend is None:
        return do_call()  # disabled / uninstalled: transparent
    discovery = _runtime.get_discovery()
    target, op, idempotent, hash_ = _judge(pool, method, url, headers, body)
    env = _http.build_request(target, op, idempotent, hash_)
    # Buffer the body ONLY when a cache ttl or a poll table is configured AND
    # the caller did not ask for a streamed (unbuffered) response — never
    # force-read a stream the caller explicitly opted out of buffering.
    cacheable = hash_ is not None and preload_content and _http.buffer_body_configured(target)
    live: dict[str, Any] = {"ok": None, "transient": None, "exc": None}

    def effect(_attempt: int) -> dict[str, Any]:
        try:
            resp = do_call()
        except Exception as err:
            live["exc"] = err
            return _http.thrown_error(err, _classify(err))
        live["exc"] = None
        if _http.is_transient_status(resp.status):
            if live["transient"] is not None and live["transient"] is not resp:
                _close(live["transient"])
            live["transient"] = resp
            return _http.transient_error(resp.status, resp.headers.get("Retry-After"))
        if live["transient"] is not None and live["transient"] is not resp:
            _close(live["transient"])
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
        _close(live["transient"])
    if action == "return":
        return value
    raise value


def _urlopen_wrapper(orig: Callable[..., Any]) -> Callable[..., Any]:
    sig = inspect.signature(orig)

    @functools.wraps(orig)
    def urlopen(self: Any, method: str, url: str, *args: Any, **kwargs: Any) -> Any:
        if _http.seam_owned():
            # A higher seam (requests/boto3) already owns this attempt —
            # pass straight through, no judgment, no second retry loop.
            return orig(self, method, url, *args, **kwargs)
        headers: Any = None
        body: Any = None
        preload_content = True
        try:
            bound = sig.bind(self, method, url, *args, **kwargs)
            bound.apply_defaults()
            headers = bound.arguments.get("headers")
            body = bound.arguments.get("body")
            preload_content = bound.arguments.get("preload_content", True)
        except TypeError:
            pass  # signature mismatch (future urllib3): best-effort judgment
        return _run(
            self,
            lambda: orig(self, method, url, *args, **kwargs),
            method,
            url,
            headers,
            body,
            preload_content if isinstance(preload_content, bool) else True,
        )

    urlopen.__keel_wrapped__ = True  # type: ignore[attr-defined]
    return urlopen


def _is_pinned(version: str) -> bool:
    return any(version == p or version.startswith(p + ".") for p in _PINNED)


__all__ = ["MODULE", "NAME", "detect", "seams", "targets", "defaults", "install", "uninstall"]
