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
"""

from __future__ import annotations

import base64
import functools
import importlib.metadata
import importlib.util
import time
from typing import Any, Callable
from urllib.parse import urlsplit

from .. import _runtime
from . import _http
from ._pack import Detection, Seam, TargetDecl

MODULE = "requests"
NAME = "requests"

_PINNED = ("2.31", "2.32")

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

def _judge(request: Any) -> tuple[str, str, bool, str | None]:
    method = (request.method or "GET").upper()
    url = request.url
    parts = urlsplit(url)
    host = parts.hostname or ""
    target = _http.resolve_target(host)
    op = f"{method} {host}{parts.path}"
    idem_header = _idempotency_header(target)
    idempotent = _http.is_idempotent(method, request.headers.keys(), idem_header)
    # A prepared body is bytes/str (buffered) or None; derive_args_hash ignores a
    # streaming (generator/file) body, so this never consumes an upload stream.
    # LLM POSTs get a canonicalized-JSON-body cache key (dev-cache exception).
    hash_ = _http.derive_args_hash(target, method, url, request.body)
    return target, op, idempotent, hash_


def _idempotency_header(target: str) -> str | None:
    backend = _runtime.get_backend()
    layer = getattr(backend, "layer", None)
    if not callable(layer):
        return None
    idem = layer(target, "idempotency")
    return idem.get("header") if isinstance(idem, dict) else None


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


# --- seam --------------------------------------------------------------------

def _run_send(do_call: Callable[[], Any], request: Any) -> Any:
    backend = _runtime.get_backend()
    if backend is None:
        return do_call()  # disabled / uninstalled: transparent
    discovery = _runtime.get_discovery()
    target, op, idempotent, hash_ = _judge(request)
    env = _http.build_request(target, op, idempotent, hash_)
    cacheable = hash_ is not None
    live: dict[str, Any] = {"ok": None, "transient": None, "exc": None}

    def effect(_attempt: int) -> dict[str, Any]:
        try:
            resp = do_call()
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


def _send_wrapper(orig: Callable[..., Any]) -> Callable[..., Any]:
    @functools.wraps(orig)
    def send(self: Any, request: Any, *args: Any, **kwargs: Any) -> Any:
        return _run_send(lambda: orig(self, request, *args, **kwargs), request)

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
