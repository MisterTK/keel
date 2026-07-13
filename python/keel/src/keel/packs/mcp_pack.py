"""The ``mcp:`` client-session pack (DX spec §4.1: "mcp:<server> — MCP client
transports (stdio + HTTP). Per-server timeout, retry, breaker; a hung MCP
server degrades gracefully instead of freezing the agent").

The Python twin of ``node/keel/src/packs/mcp.mjs`` — the two front ends make
the same judgments for the same MCP call:

Seam: ``mcp.client.session.ClientSession.send_request`` — the official Python
SDK's (``pip install mcp``) JSON-RPC request/response CORRELATION boundary,
shared by every client transport (stdio, streamable HTTP) and by every
convenience method (``call_tool``, ``list_tools``, ``read_resource``, …), which
all funnel through it. It is patched on ``ClientSession`` specifically —
``BaseSession.send_request`` (``mcp.shared.session``) is also inherited by
``ServerSession``, so patching the base class would instrument outbound
*server* requests too; assigning the wrapper onto ``ClientSession`` shadows the
inherited method for client sessions only, leaving ``ServerSession`` untouched.
The patch mutates the class attribute (reversible: ``uninstall`` restores the
original), so ``uninstall`` = remove the package (DX invariant 2).

Target = ``mcp:<server-name>``. Unlike the Node/TS SDK (whose ``Client``
retains ``getServerVersion()`` after connecting), the pinned mcp Python SDK's
``ClientSession`` does not itself retain server identity once ``initialize()``
returns its ``InitializeResult`` — so this pack captures ``serverInfo.name``
off a successful ``initialize`` response as it passes through the seam and
caches it on the session instance for every later call; a request observed
before that handshake (or on a session whose ``initialize`` we never saw)
resolves to ``mcp:unknown``. Per-server policy comes from
``[target."mcp:<server>"]``, else ``[defaults.outbound]`` (mcp: is not an
``llm:`` target) — so mcp: targets inherit the outbound retry/timeout/breaker
whether or not they are listed.

Idempotency is judged from the JSON-RPC METHOD, not hardcoded (Level 0 hard
rule, dx-spec §1: "never retry non-idempotent calls by default … a Level 0
surprise is a P0 bug"). Read-ish methods (initialize, ping, resources/read,
prompts/get, completion/complete, and any ``…/list``) are idempotent and retry
per policy; ``tools/call`` (arbitrary side effects) and any unknown method are
non-idempotent → observed, not retried (KEEL-E014) — the MCP analogue of the
httpx/requests packs' POST model. There is no per-method retry opt-in in v0.1
(we do not invent policy surface): ``tools/call`` is simply never
auto-retried. Calls are never cached (``args_hash`` is always ``None``) — an
MCP call can be side-effecting.

Timeout: the SDK's own ``send_request`` already accepts a
``request_read_timeout_seconds`` (a ``timedelta``, in the pinned v1.x line)
that races the request and raises on expiry, so — unlike the httpx/tool packs,
which leave per-attempt timeouts to the core's native async path — this pack
injects the target's configured ``timeout`` as that argument on every attempt,
but ONLY for idempotent methods: we never impose a deadline on a possibly-
succeeding side-effecting call. A caller-supplied timeout is honored unchanged
for non-idempotent methods and when no policy timeout is configured. So a hung
server on a read-ish call times out per policy, retries, and finally raises
KEEL-E010 — it degrades gracefully instead of freezing the agent.

Live objects vs. the core boundary: the real MCP result (and any raised
exception) is held side-band exactly as the httpx/tool packs document — the
core requires a JSON payload and cannot round-trip a live (possibly complex,
non-JSON-safe) MCP result, and there is nothing to cache anyway (args_hash is
always None), so the payload sent through the core is always ``None`` and the
live result / original exception drive delivery.

Installation is LAZY, mirroring the httpx/requests adapters (Task 10) rather
than the Node front end's eager ``installMcpPack`` — importing ``mcp`` for
real (its own ``__init__`` pulls in pydantic + anyio + httpx) costs real
wall-clock time (order 100s of ms), so patching it unconditionally at every
``keel run`` would blow the startup-overhead budget for any program that has
the SDK installed but has not imported it yet. :func:`install_mcp_pack`
therefore only PROBES for ``mcp``'s presence (a bare, cheap ``find_spec`` that
does not import it) and, if present, arms the same ``sys.meta_path`` hook the
adapters use (:class:`keel.adapters._AdapterFinder`) so the real patch runs
right after the user's program imports ``mcp`` for its own reasons — or
immediately, if it was already imported before bootstrap ran.
"""

from __future__ import annotations

import asyncio
import importlib.metadata
import importlib.util
import re
import sys
import time
from datetime import timedelta
from typing import Any, Callable

from .. import _runtime
from .._errors import KeelError
from .._wrap import WRAPPED_ATTR, _attach_outcome
from ..adapters import _AdapterFinder
from ..adapters._pack import Detection, Seam, TargetDecl

MODULE = "mcp"
NAME = "mcp"
#: Versions this pack certifies (prefix match). The mcp Python SDK is v1.x.
_PINNED = ("1",)

#: Read-ish MCP request methods that are safe to auto-retry. Everything else —
#: notably `tools/call` (runs arbitrary side-effecting tools) and any unknown
#: method — is non-idempotent: observed, not retried (Level 0 hard rule,
#: KEEL-E014), mirroring the Node pack's IDEMPOTENT_MCP_METHODS. Any method
#: ending in `/list` (tools/list, resources/list, resources/templates/list,
#: prompts/list) is also a read.
_IDEMPOTENT_MCP_METHODS = frozenset(
    {"initialize", "ping", "resources/read", "prompts/get", "completion/complete"}
)

_DURATION_RE = re.compile(r"^(\d+)(ms|s|m|h)$")
_DURATION_MULT_MS = {"ms": 1, "s": 1000, "m": 60_000, "h": 3_600_000}


def is_idempotent_mcp_method(method: object) -> bool:
    """True for a read-ish JSON-RPC method (parity with the Node twin's
    ``isIdempotentMcpMethod``)."""
    if not isinstance(method, str):
        return False
    if method in _IDEMPOTENT_MCP_METHODS:
        return True
    return method.endswith("/list")  # list operations are reads


def classify_mcp_error(err: BaseException) -> str:
    """Classify a raised MCP/transport error into a core error class. Duck-typed
    (no import of the ``mcp`` package's exception types) so classification works
    whether or not the SDK is installed, and across its v1/v2 error shapes
    (``McpError.error.code`` vs. the flattened ``MCPError.code``).

    ``asyncio.CancelledError`` is a ``BaseException`` and never reaches this —
    caller cancellation escapes the wrapper unchanged (parity with the Node
    twin's ``AbortError`` → ``cancelled``, and with the ``tool:`` pack)."""
    if isinstance(err, (TimeoutError, asyncio.TimeoutError)):
        return "timeout"
    code = getattr(err, "code", None)
    if code is None:
        code = getattr(getattr(err, "error", None), "code", None)  # v1 McpError.error.code
    if code in (-32001, 408):  # JSON-RPC RequestTimeout / httpx.codes.REQUEST_TIMEOUT (v1)
        return "timeout"
    if isinstance(err, ConnectionError):
        return "conn"
    if any(k in str(err).lower() for k in ("closed", "disconnect", "connection")):
        return "conn"
    return "other"


def _request_method(request: Any) -> str:
    """The JSON-RPC method name of an outgoing request, or "?" when it cannot
    be read (mirrors the Node twin's ``request?.method ?? "?"``). Concrete SDK
    request types (``PingRequest``, ``CallToolRequest``, …) carry a ``method``
    field directly; a ``RootModel``-wrapped request nests it under ``.root``."""
    method = getattr(request, "method", None)
    if not isinstance(method, str):
        method = getattr(getattr(request, "root", None), "method", None)
    if not isinstance(method, str) and isinstance(request, dict):
        method = request.get("method")
    return method if isinstance(method, str) else "?"


#: Where we cache a server name captured from a successful `initialize`
#: response, set directly on the session instance (see `_remember_server_name`).
_SERVER_NAME_ATTR = "__keel_mcp_server_name__"


def _safe_server_name(session: Any) -> str:
    """The connected server's name, best-effort. Prefers our own cached name
    (see :func:`_remember_server_name` — the pinned v1.x SDK does not itself
    retain server identity on the session after ``initialize()`` returns),
    then a hypothetical ``get_server_version()`` accessor (parity with the
    Node twin's ``client.getServerVersion()?.name``) or a ``server_info``/
    ``_server_info`` property a future SDK version might expose directly;
    falls back to "unknown" (a pre-handshake request, or a session whose
    `initialize` we never observed — e.g. a caller-managed handshake)."""
    cached = getattr(session, _SERVER_NAME_ATTR, None)
    if isinstance(cached, str) and cached:
        return cached
    try:
        get_version = getattr(session, "get_server_version", None)
        if callable(get_version):
            name = getattr(get_version(), "name", None)
            if isinstance(name, str) and name:
                return name
        for attr in ("server_info", "_server_info"):
            name = getattr(getattr(session, attr, None), "name", None)
            if isinstance(name, str) and name:
                return name
    except Exception:
        pass
    return "unknown"


def _remember_server_name(session: Any, method: str, result: Any) -> None:
    """Cache the server name from a successful ``initialize`` response onto
    the session instance. The mcp Python SDK's ``ClientSession.initialize()``
    (unlike the Node/TS SDK's ``getServerVersion()``) does not itself retain
    server identity after returning the ``InitializeResult`` — without this,
    every subsequent call on the session would resolve to ``mcp:unknown`` for
    its whole lifetime."""
    if method != "initialize":
        return
    info = getattr(result, "serverInfo", None) or getattr(result, "server_info", None)
    name = getattr(info, "name", None)
    if isinstance(name, str) and name:
        try:
            setattr(session, _SERVER_NAME_ATTR, name)
        except Exception:
            pass  # best-effort caching only; a slotted/frozen session just stays "unknown"


def _duration_ms(value: Any) -> int | None:
    m = _DURATION_RE.match(str(value or "").strip())
    if not m:
        return None
    return int(m.group(1)) * _DURATION_MULT_MS[m.group(2)]


def _ok(live: dict[str, Any], value: Any) -> dict[str, Any]:
    live["have"] = True
    live["result"] = value
    live["exc"] = None
    return {"status": "ok", "payload": None}  # never cached: args_hash is always None


def _err(live: dict[str, Any], err: BaseException) -> dict[str, Any]:
    live["exc"] = err
    return {"status": "error", "class": classify_mcp_error(err), "message": str(err)}


def make_wrapped_send_request(original: Callable[..., Any]) -> Callable[..., Any]:
    """Wrap a ``ClientSession.send_request``-shaped callable so each JSON-RPC
    request routes through the process backend. ``original`` is called as an
    UNBOUND function ``original(self, request, result_type, timeout, metadata,
    progress_callback)`` — the shape a class attribute assignment needs — so
    the returned wrapper can be assigned directly onto a session class."""

    async def keel_mcp_send_request(
        self: Any,
        request: Any,
        result_type: Any = None,
        request_read_timeout_seconds: Any = None,
        metadata: Any = None,
        progress_callback: Any = None,
    ) -> Any:
        backend = _runtime.get_backend()
        if backend is None:
            return await original(
                self, request, result_type, request_read_timeout_seconds, metadata, progress_callback
            )
        server = _safe_server_name(self)
        target = f"mcp:{server}"
        method = _request_method(request)
        op = f"mcp {server} {method}"
        # Idempotency is keyed off the method (Level 0 hard rule): read-ish
        # methods retry; tools/call and unknown methods are observed-not-retried.
        idempotent = is_idempotent_mcp_method(method)
        req = {"v": 1, "target": target, "op": op, "idempotent": idempotent, "args_hash": None}
        # Impose a per-attempt deadline (via the SDK's own read-timeout arg)
        # only for idempotent methods — never inject a thrown timeout into a
        # possibly-succeeding side-effecting call; otherwise pass the caller's
        # own timeout through unchanged.
        timeout_ms = _duration_ms(backend.layer(target, "timeout")) if idempotent else None
        effective_timeout = (
            timedelta(milliseconds=timeout_ms) if timeout_ms is not None else request_read_timeout_seconds
        )

        live: dict[str, Any] = {"result": None, "have": False, "exc": None}

        async def aeffect(_attempt: int) -> dict[str, Any]:
            try:
                result = await original(self, request, result_type, effective_timeout, metadata, progress_callback)
            except Exception as err:  # not BaseException: cancellation escapes unchanged
                return _err(live, err)
            _remember_server_name(self, method, result)
            return _ok(live, result)

        started = time.perf_counter()
        exec_async = getattr(backend, "execute_async", None)
        if callable(exec_async):
            # NATIVE async path: the real async core awaits our coroutine
            # directly on the caller's loop (mirrors adapters.httpx_pack).
            outcome = await exec_async(req, aeffect)
        else:
            # STUB async path: the synchronous stub cannot await, so each
            # attempt is driven in a worker thread that marshals the await
            # back onto this loop (mirrors adapters.httpx_pack / packs.tool).
            loop = asyncio.get_running_loop()

            def effect(attempt: int) -> dict[str, Any]:
                return asyncio.run_coroutine_threadsafe(aeffect(attempt), loop).result()

            outcome = await loop.run_in_executor(None, lambda: backend.execute(req, effect))

        discovery = _runtime.get_discovery()
        if discovery is not None:
            discovery.record(target, outcome, round((time.perf_counter() - started) * 1000))

        if outcome.get("result") == "ok":
            return live["result"] if live["have"] and not outcome.get("from_cache") else outcome.get("payload")
        original_exc = live["exc"]
        if original_exc is not None:
            _attach_outcome(original_exc, outcome)
            raise original_exc
        # No side-band original (e.g. a breaker fast-fail, KEEL-E012): surface
        # the core's own error, still carrying the outcome.
        err = outcome.get("error") or {}
        synthetic = KeelError(err.get("code") or "KEEL-E040", err.get("message") or f"keel: MCP call to {server} failed")
        _attach_outcome(synthetic, outcome)
        raise synthetic

    return keel_mcp_send_request


def detect() -> Detection:
    """Present iff ``mcp`` is importable — decided WITHOUT importing it
    (importability + installed version only, per adapter-pack rule 1)."""
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
            patch_point="mcp.client.session.ClientSession.send_request",
            upstream_api=(
                "mcp Python SDK: BaseSession.send_request(request, result_type) -> result "
                "— the JSON-RPC request/response correlation boundary"
            ),
            why_stable=(
                "shared by every client transport (stdio, streamable HTTP) and every "
                "convenience method (call_tool, list_tools, read_resource, …); patched on "
                "ClientSession specifically so ServerSession, which also inherits "
                "send_request from BaseSession, is left untouched"
            ),
        )
    ]


def targets() -> list[TargetDecl]:
    return [
        TargetDecl(
            pattern="mcp:<server>",
            kind="mcp",
            idempotency_rule=(
                "keyed off the JSON-RPC method: read-ish methods (initialize, ping, "
                "*/list, resources/read, prompts/get, completion/complete) are retryable; "
                "tools/call and unknown methods are observed-not-retried (KEEL-E014), "
                "never auto-retried in v0.1"
            ),
            args_hash_rule="none (MCP calls are not cached — potentially side-effecting)",
        )
    ]


def defaults() -> dict[str, Any]:
    """Empty: mcp: targets inherit ``[defaults.outbound]`` (mirrors the Node
    twin and the ``tool:`` pack) — there is no ``[defaults.mcp]`` in the frozen
    smart-defaults pack."""
    return {}


def patch_client_session(session_class: type) -> Callable[[], None]:
    """Patch ``session_class.send_request`` in place. Idempotent — a second
    patch of the SAME class is a no-op (checked via the ``WRAPPED_ATTR`` marker
    on the class's current ``send_request``, not a module-level flag, so
    patching one class never blocks — or gets confused by — patching another)
    and reversible: the returned callable restores the original. Mirrors the
    Node twin's ``patchClientRequest``."""
    original = getattr(session_class, "send_request", None)
    if not callable(original) or getattr(original, WRAPPED_ATTR, False):
        return lambda: None
    wrapped = make_wrapped_send_request(original)
    setattr(wrapped, WRAPPED_ATTR, True)
    session_class.send_request = wrapped

    def uninstall() -> None:
        if session_class.send_request is wrapped:
            session_class.send_request = original

    return uninstall


_installed = False
_uninstall_real: Callable[[], None] | None = None
#: The mcp-specific lazy import hook, if armed (module docs: "Installation is
#: LAZY"). A dedicated `_AdapterFinder` instance, separate from the httpx/
#: requests adapters' own — inserting/removing it never touches theirs.
_lazy_finder: _AdapterFinder | None = None


def install() -> bool:
    """Patch the REAL mcp ``ClientSession.send_request``, if importable. No-arg
    and idempotent, mirroring the httpx/requests adapters' ``install()``
    convention — called either immediately (the SDK was already imported
    before bootstrap) or by the lazy import hook the first time the user's
    program imports ``mcp`` for its own reasons."""
    global _installed, _uninstall_real
    if _installed:
        return True
    try:
        from mcp.client.session import ClientSession
    except ImportError:
        return False
    _uninstall_real = patch_client_session(ClientSession)
    _installed = True
    return True


def uninstall() -> None:
    """Restore the real ``ClientSession.send_request`` (test teardown /
    uninstall-clean)."""
    global _installed, _uninstall_real
    if _uninstall_real is not None:
        _uninstall_real()
    _uninstall_real = None
    _installed = False


def _uninstall_lazy() -> None:
    """Remove the armed import hook (if the user's program never imported
    ``mcp``) and restore the real class (if it did)."""
    global _lazy_finder
    if _lazy_finder is not None:
        try:
            sys.meta_path.remove(_lazy_finder)
        except ValueError:
            pass
        _lazy_finder = None
    uninstall()


def install_mcp_pack(*, session_class: type | None = None) -> dict[str, Any]:
    """Auto-detect and arm the MCP client SDK, best-effort — an absent or
    incompatible SDK (or any unexpected failure) is a silent no-op, never
    fatal. Called from bootstrap; returns the same shape as the Node front
    end's ``installMcpPack``: ``{"active": False}``, or ``{"active": True,
    "name": ..., "uninstall": callable}``.

    ``session_class`` is a test seam: when given, the class is patched
    IMMEDIATELY (bypassing detection/laziness entirely) — deterministic for
    tests, which supply their own fake session and want to see the effect
    right away. With no override (the real bootstrap call), installation is
    LAZY (module docs): present-but-unimported costs one cheap ``find_spec``,
    not a real import of the SDK's dependency chain.
    """
    if session_class is not None:
        if not callable(getattr(session_class, "send_request", None)):
            return {"active": False}  # not a patchable session-shaped class
        return {"active": True, "name": NAME, "uninstall": patch_client_session(session_class)}

    global _lazy_finder
    try:
        if importlib.util.find_spec(MODULE) is None:  # cheap: does not import it
            return {"active": False}
        if MODULE in sys.modules:
            # Already imported by something before bootstrap ran: patch now.
            return {"active": True, "name": NAME, "uninstall": uninstall} if install() else {"active": False}
        if _lazy_finder is None:
            _lazy_finder = _AdapterFinder({MODULE: sys.modules[__name__]})
            sys.meta_path.insert(0, _lazy_finder)
        return {"active": True, "name": NAME, "uninstall": _uninstall_lazy}
    except Exception:
        return {"active": False}


def _is_pinned(version: str) -> bool:
    return any(version == p or version.startswith(p + ".") for p in _PINNED)


__all__ = [
    "MODULE",
    "NAME",
    "detect",
    "seams",
    "targets",
    "defaults",
    "patch_client_session",
    "install",
    "uninstall",
    "install_mcp_pack",
    "is_idempotent_mcp_method",
    "classify_mcp_error",
    "make_wrapped_send_request",
]
