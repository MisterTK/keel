"""The urllib.request adapter pack: resilience for stdlib ``urllib.request``
calls through the ``OpenerDirector.open`` seam.

Seam (narrowest stable): ``urllib.request.OpenerDirector.open`` — every
dispatch path funnels through it: the module-level ``urlopen`` (via the lazy
global ``_opener``), any ``build_opener()`` custom opener, an
``install_opener()`` replacement, and references held before Keel activated
(``from urllib.request import urlopen`` holds the function, which still calls
the patched class method). This is one level deeper than patching the
``urlopen`` name (the seam claude-trader's own e2e shim uses).

The simplest pack in the tree: synchronous only (urllib.request has no async
API — no event-loop bridge), and no LLM budget/fallback loop (a documented v1
limitation vs httpx/requests; provider hosts still resolve to ``llm:*``
targets so their policy layers apply). Judgment is shared with every HTTP
pack via ``_http``.

urllib-specific semantics, preserved exactly (DX invariant 5):

* urllib RAISES ``urllib.error.HTTPError`` for >=400 responses. Transient
  ones (429/5xx) are reported to the core as typed ``http`` errors and the
  ORIGINAL ``HTTPError`` is re-raised unchanged after retries exhaust.
  Non-transient ones (404, ...) are core-level SUCCESSES — Keel never turns a
  real HTTP response into a failure — and are re-raised as urllib itself
  would, including on a cache hit (the replay rebuilds an ``HTTPError``).
* ``urllib.error.URLError`` / timeout exceptions re-raise unchanged
  (classified conn/timeout for retry policy first).
* Byte transparency: the success path returns the REAL live
  ``http.client.HTTPResponse`` (or ``addinfourl``) untouched. The body is
  buffered ONLY when a cache ttl is configured for the target (the httpx
  rule); a buffered response is re-wrapped in a real, fully readable
  ``addinfourl`` over the same bytes.
* Timeout composes tighter-wins: ``open(timeout=...)`` is urllib's native
  per-attempt client timeout, so Keel's ``timeout`` policy layer is enforced
  by passing ``min(caller, policy)`` down — the one pack where the policy
  timeout has a direct transport knob.

Re-entrancy guard: urllib's own handlers (redirect, auth-retry) call
``self.parent.open(...)`` recursively inside an attempt. The attempt runs
under ``_http.run_owned``, and the wrapper passes owned calls straight
through, so one user-level ``open`` — redirects and all — is exactly one Keel
``execute``.

Detection convention exception (mirrored by the CLI's REGISTRY row): stdlib
has no pip pin, so ``detect()`` reports the PYTHON RUNTIME version
(``platform.python_version()``), "pinned" when the interpreter line is one
this pack certifies (``_PINNED``).

Non-HTTP schemes (``file:``, ``ftp:``, ``data:``) pass through unjudged —
Keel's policy vocabulary is host-shaped and none of its layers apply.
"""

from __future__ import annotations

import base64
import email.message
import functools
import http.client
import io
import platform
import socket
import time
import urllib.error
from typing import Any, Callable
from urllib.parse import urlsplit
from urllib.response import addinfourl

from .. import _runtime
from . import _http
from ._pack import Detection, Seam, TargetDecl

MODULE = "urllib.request"
NAME = "urllib.request"

#: Python runtime lines this pack certifies (prefix match) — the stdlib
#: convention exception: there is no pip version to pin, so the "version" is
#: the interpreter's. CI certifies 3.11 (ci.yml/adapter-farm.yml); the upper
#: entries track the current stable lines.
_PINNED = ("3.11", "3.12", "3.13", "3.14")

_installed = False
_orig: dict[str, Any] = {}


# --- contract operations -----------------------------------------------------


def detect() -> Detection:
    """Always present (stdlib). The version reported is the Python runtime's —
    see the module docstring's convention-exception note."""
    version = platform.python_version()
    confidence = "pinned" if _is_pinned(version) else "best_effort"
    return Detection(matched=True, name=NAME, version=version, confidence=confidence)


def seams() -> list[Seam]:
    return [
        Seam(
            patch_point="urllib.request.OpenerDirector.open",
            upstream_api="urllib.request opener API: OpenerDirector.open(fullurl, data, timeout) -> addinfourl",
            why_stable=(
                "Every urllib.request dispatch path funnels through "
                "OpenerDirector.open: module-level urlopen, build_opener "
                "custom openers, install_opener replacements, and references "
                "held before activation. Stdlib API, stable across the "
                "supported interpreter lines."
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
            args_hash_rule="sha256(method + url) for idempotent GET; None otherwise",
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
    from urllib.request import OpenerDirector

    _orig["open"] = OpenerDirector.open
    OpenerDirector.open = _open_wrapper(_orig["open"])  # type: ignore[method-assign]
    _installed = True


def uninstall() -> None:
    global _installed
    if not _installed:
        return
    from urllib.request import OpenerDirector

    OpenerDirector.open = _orig["open"]  # type: ignore[method-assign]
    _orig.clear()
    _installed = False


# --- judgment ----------------------------------------------------------------


def _judge(fullurl: Any, data: Any) -> tuple[Any, Any, str, str, str, bool, str | None, str | None]:
    """Judgment + idempotency-key injection for one ``open`` call. Returns
    ``(current, pass_data, url, target, op, idempotent, args_hash, injected)``
    where ``current``/``pass_data`` are what the original ``open`` receives on
    every attempt (a str-url POST that needs an injected key is promoted to a
    ``Request`` with ``data`` folded in, so each retry resends the SAME
    header — contracts/adapter-pack.md "Idempotency-key injection")."""
    import urllib.request as ur

    req = fullurl if isinstance(fullurl, ur.Request) else None
    url: str = req.full_url if req is not None else fullurl
    if req is not None:
        # OpenerDirector.open folds a non-None `data` kwarg into the Request
        # (`req.data = data`) BEFORE dispatching — but that fold happens
        # inside `orig`, which we haven't called yet. Judge as if it already
        # happened, or `urlopen(Request(url), data=b"...")` misreads as GET
        # (req.data is still None here) instead of POST.
        #
        # Method resolution is `Request.get_method()`'s job (explicit method
        # wins, else POST-if-body-else-GET) — delegate to it rather than
        # re-deriving the fallback. It keys off `req.data`, so stage the fold
        # locally first; `orig` re-folds the same kwarg on every attempt, so
        # this leaves `req` in exactly the state stdlib's own dispatch would.
        body = data if data is not None else req.data
        req.data = body
        method = req.get_method()
    else:
        body = data
        method = "POST" if data is not None else "GET"
    parts = urlsplit(url)
    host = parts.hostname or ""
    target = _http.resolve_policy_target(
        method, host, scheme=parts.scheme, port=parts.port, path=parts.path
    )
    op = f"{method} {host}{parts.path}"
    idem_header = _http.idempotency_header(target)
    hash_ = _http.derive_args_hash(
        target, method, url, body if isinstance(body, (bytes, str)) else None
    )
    recorded_key = _http.peek_recorded_idempotency_key(target, hash_)
    header_names = [k for k, _ in req.header_items()] if req is not None else []
    injected = _http.resolve_idempotency_injection(
        method, header_names, idem_header, recorded_key=recorded_key
    )
    current, pass_data = fullurl, data
    if injected is not None:
        if req is None:
            req = ur.Request(url, data=data)
            current, pass_data = req, None
        req.add_header(idem_header, injected)  # type: ignore[arg-type]
        current = req
    idempotent = injected is not None or _http.is_idempotent(method, header_names, idem_header)
    return current, pass_data, url, target, op, idempotent, hash_, injected


def _classify(err: BaseException) -> str:
    if isinstance(err, urllib.error.URLError):
        reason = getattr(err, "reason", None)
        return "timeout" if isinstance(reason, TimeoutError) else "conn"
    if isinstance(err, TimeoutError):  # socket.timeout is an alias since 3.10
        return "timeout"
    if isinstance(err, (ConnectionError, http.client.HTTPException)):
        return "conn"
    return "other"


# --- timeout composition -----------------------------------------------------


def _compose_timeout(target: str, caller_timeout: Any) -> Any:
    """Tighter wins. ``open``'s default is the module sentinel (= no explicit
    caller timeout), and ``None`` means block forever — both yield to a
    configured policy timeout; two real numbers take the min. urllib's
    ``timeout`` is per attempt, matching the policy layer's semantics."""
    policy_s = _http.resolve_timeout_s(target)
    if policy_s is None:
        return caller_timeout
    if caller_timeout is socket._GLOBAL_DEFAULT_TIMEOUT or not isinstance(
        caller_timeout, (int, float)
    ):
        return policy_s
    return min(float(caller_timeout), policy_s)


# --- response (de)serialization for the core payload ------------------------


def _rebuild(payload: Any, url: str) -> Any:
    """Rebuild a response from an envelope (a cache-hit replay). Status >=400
    rebuilds an ``HTTPError`` — ``_run_open``'s tail re-raises it, so replayed
    calls keep urllib's raise-on-4xx semantics."""
    p = payload if isinstance(payload, dict) else {}
    code = int(p.get("status", 200))
    body = base64.b64decode(p["body_b64"]) if isinstance(p.get("body_b64"), str) else b""
    headers = email.message.Message()
    for k, v in p.get("headers", []):
        headers[k] = v
    if code >= 400:
        return urllib.error.HTTPError(
            url, code, http.client.responses.get(code, ""), headers, io.BytesIO(body)
        )
    return addinfourl(io.BytesIO(body), headers, url, code)


def _close(obj: Any) -> None:
    try:
        if obj is not None:
            obj.close()
    except Exception:
        pass


# --- seam --------------------------------------------------------------------


def _run_open(orig: Callable[..., Any], self: Any, fullurl: Any, data: Any, timeout: Any) -> Any:
    backend = _runtime.get_backend()
    if backend is None or _http.seam_owned():
        # Disabled/uninstalled: transparent. Owned: a redirect/auth-handler
        # re-entry inside an attempt this pack is already driving.
        return orig(self, fullurl, data, timeout)
    url_probe = fullurl.full_url if hasattr(fullurl, "full_url") else fullurl
    if not isinstance(url_probe, str):
        return orig(self, fullurl, data, timeout)
    probe = urlsplit(url_probe)
    if probe.scheme not in ("http", "https") or not probe.hostname:
        return orig(self, fullurl, data, timeout)  # file:/ftp:/data:: not host-shaped
    discovery = _runtime.get_discovery()

    current, pass_data, url, target, op, idempotent, hash_, injected = _judge(fullurl, data)
    effective_timeout = _compose_timeout(target, timeout)
    env = _http.build_request(target, op, idempotent, hash_)
    cacheable = hash_ is not None and _http.buffer_body_configured(target)
    live: dict[str, Any] = {"ok": None, "exc": None}

    def effect(_attempt: int) -> dict[str, Any]:
        # A prior attempt's live HTTPError (a transient 503, say) is only
        # ever referenced side-band via `live["exc"]`; once this attempt
        # supersedes it — whichever branch below runs — the old one must be
        # closed here, or its buffered body/socket leaks (ResourceWarning).
        prior_exc = live["exc"]
        try:
            resp = _http.run_owned(lambda: orig(self, current, pass_data, effective_timeout))
        except urllib.error.HTTPError as err:
            if _http.is_transient_status(err.code):
                if prior_exc is not None and prior_exc is not err:
                    _close(prior_exc)
                live["exc"] = err
                return _http.transient_error(err.code, err.headers.get("Retry-After"))
            # Non-transient >=400: a real response — a core-level SUCCESS.
            # Kept side-band; `_run_open`'s tail re-raises it (urllib
            # semantics). Body buffered only when cacheable, re-wrapped so the
            # raised error stays fully readable.
            if prior_exc is not None:
                _close(prior_exc)
            live["exc"] = None
            body = None
            err_live: Any = err
            if cacheable:
                try:
                    body = err.read()
                except Exception:
                    body = None
                err_live = urllib.error.HTTPError(
                    url, err.code, err.msg, err.headers, io.BytesIO(body or b"")
                )
            live["ok"] = err_live
            return {
                "status": "ok",
                "payload": _http.response_envelope(err.code, err.headers.items(), body),
            }
        except Exception as err:
            if prior_exc is not None:
                _close(prior_exc)
            live["exc"] = err
            return _http.thrown_error(err, _classify(err))
        if prior_exc is not None:
            _close(prior_exc)
        live["exc"] = None
        if cacheable:
            body = resp.read()
            replacement = addinfourl(
                io.BytesIO(body), resp.headers, resp.geturl(), getattr(resp, "status", 200)
            )
            _close(resp)
            live["ok"] = replacement
            return {
                "status": "ok",
                "payload": _http.response_envelope(
                    getattr(replacement, "status", 200) or 200, resp.headers.items(), body
                ),
            }
        live["ok"] = resp
        return {
            "status": "ok",
            "payload": _http.response_envelope(
                getattr(resp, "status", 200) or 200, resp.headers.items(), None
            ),
        }

    started = time.perf_counter()
    outcome = _http.call_execute(backend, env, effect, injected)
    latency_ms = round((time.perf_counter() - started) * 1000)
    if discovery is not None:
        discovery.record(target, outcome, latency_ms)

    action, value = _http.deliver(
        outcome,
        ok_response=live["ok"],
        transient_response=None,  # urllib RAISES transients; kept in live["exc"]
        exc=live["exc"],
        rebuild=lambda payload: _rebuild(payload, url),
    )
    if action == "return" and isinstance(value, urllib.error.HTTPError):
        raise value  # urllib semantics: >=400 raises (live or cache-replayed)
    if action == "return":
        return value
    raise value


def _open_wrapper(orig: Callable[..., Any]) -> Callable[..., Any]:
    @functools.wraps(orig)
    def open(
        self: Any,
        fullurl: Any,
        data: Any = None,
        timeout: Any = socket._GLOBAL_DEFAULT_TIMEOUT,
    ) -> Any:
        return _run_open(orig, self, fullurl, data, timeout)

    open.__keel_wrapped__ = True  # type: ignore[attr-defined]
    return open


def _is_pinned(version: str) -> bool:
    return any(version == p or version.startswith(p + ".") for p in _PINNED)


__all__ = ["MODULE", "NAME", "detect", "seams", "targets", "defaults", "install", "uninstall"]
