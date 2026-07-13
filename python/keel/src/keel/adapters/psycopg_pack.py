"""The psycopg (v3) adapter pack: breaker + timeout observability for every
query, zero code changes, through the cursor's own execute seam.

Seam: ``psycopg.Cursor.execute`` / ``psycopg.Cursor.executemany``, and their
async twins ``psycopg.AsyncCursor.execute`` / ``AsyncCursor.executemany``.
``Connection.execute()`` (a documented convenience shortcut) creates a cursor
and calls ``cur.execute()`` internally, so patching the cursor class covers
both call styles — the same "patch the narrow point everything funnels
through" shape as the HTTP packs' transport/adapter seams.

Idempotency: **always non-idempotent** (KEEL-E014 — observed, not retried).
Unlike an HTTP method, a SQL statement's side effects cannot be judged safe
to repeat from the driver seam: the query argument can be a plain string, a
multi-statement script, or a ``psycopg.sql.Composed`` object built from
fragments, and even a leading ``SELECT`` can hide a write in a CTE
(``WITH t AS (INSERT ... RETURNING ...) SELECT * FROM t``). Verb-sniffing a
string prefix — the way the HTTP packs trust a GET — is a fine heuristic
when the cost of being wrong is a duplicated read; it is not an acceptable
one when the cost of being wrong is a duplicated write, so this pack does
not attempt it and instead ships the deliberately conservative "breaker +
timeout only" profile: retry is inert for every ``<db host>[:<port>]``
target, while breaker (repeated connection failures fail fast, KEEL-E012)
and timeout (armed by the core for the async path; see below) still apply.
A caller who needs retried reads can wrap them explicitly at a higher level
(e.g. ``py:`` on a read-only helper function) where the safety judgment is
the caller's own, explicit assertion.

Timeouts: exactly the ``tool:`` pack's story (``packs/tool.py`` module
docs) — the core arms a real per-attempt deadline only where the effect
actually awaits (the native async path, i.e. ``AsyncCursor``); a *sync*
``Cursor.execute()`` call blocks the calling thread and cannot be
pre-empted by the core, so "timeout" for sync psycopg means observing
whatever psycopg/libpq itself reports (a server-side ``statement_timeout``
raises ``psycopg.errors.QueryCanceled``), not a deadline Keel imposes.

Caching: never (``args_hash`` is always ``None`` — "breaker + timeout only").

Target: ``<host>[:<port>]`` (``kind="host"``, the bare-host branch of the
frozen target grammar — a database server is a network endpoint, exactly
like an HTTP host, just with a meaningful non-standard port), derived from
``cursor.connection.info``. Not derived from the library-generic ``_http``
helpers (there is no meaningful HTTP method/LLM-host mapping for a SQL
call); this pack owns its own small judgment instead.
"""

from __future__ import annotations

import asyncio
import functools
import importlib.metadata
import importlib.util
import time
from typing import Any, Callable

from .. import _runtime
from .._errors import KeelError
from .._wrap import ENVELOPE_VERSION
from ._pack import Detection, Seam, TargetDecl

MODULE = "psycopg"
NAME = "psycopg"

#: Versions this pack certifies via contract tests (prefix match).
_PINNED = ("3.2", "3.3")

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
            patch_point="psycopg.Cursor.execute / psycopg.Cursor.executemany",
            upstream_api="psycopg v3 cursor API: Cursor.execute(query, params) -> Cursor",
            why_stable=(
                "Every query path funnels through the cursor: "
                "Connection.execute() is a documented shortcut that creates "
                "a cursor and calls cur.execute() internally, so patching "
                "the cursor class covers both call styles."
            ),
        ),
        Seam(
            patch_point="psycopg.AsyncCursor.execute / psycopg.AsyncCursor.executemany",
            upstream_api="psycopg v3 async cursor API: AsyncCursor.execute(query, params) -> AsyncCursor",
            why_stable="The async twin of the cursor seam, for AsyncConnection users; same shortcut relationship.",
        ),
    ]


def targets() -> list[TargetDecl]:
    return [
        TargetDecl(
            pattern="<db host>[:<port>]",
            kind="host",
            idempotency_rule=(
                "always non-idempotent — observed, not retried (KEEL-E014); "
                "breaker and timeout still apply (module docs)"
            ),
            args_hash_rule="None always — psycopg calls are never cached by this pack",
        )
    ]


def defaults() -> dict[str, Any]:
    """No pack-specific fragment: ``<host>[:<port>]`` targets inherit
    ``[defaults.outbound]`` (breaker + timeout meaningfully apply; retry is
    inert — see targets()/idempotency_rule)."""
    return {}


# --- install / uninstall -----------------------------------------------------


def install() -> None:
    """Patch the psycopg seams. Idempotent; a no-op if psycopg is not
    importable."""
    global _installed
    if _installed:
        return
    try:
        import psycopg
    except ImportError:
        return
    _orig["execute"] = psycopg.Cursor.execute
    _orig["executemany"] = psycopg.Cursor.executemany
    _orig["aexecute"] = psycopg.AsyncCursor.execute
    _orig["aexecutemany"] = psycopg.AsyncCursor.executemany
    psycopg.Cursor.execute = _sync_wrapper(_orig["execute"])  # type: ignore[method-assign]
    psycopg.Cursor.executemany = _sync_wrapper(_orig["executemany"])  # type: ignore[method-assign]
    psycopg.AsyncCursor.execute = _async_wrapper(_orig["aexecute"])  # type: ignore[method-assign]
    psycopg.AsyncCursor.executemany = _async_wrapper(_orig["aexecutemany"])  # type: ignore[method-assign]
    _installed = True


def uninstall() -> None:
    global _installed
    if not _installed:
        return
    import psycopg

    psycopg.Cursor.execute = _orig["execute"]  # type: ignore[method-assign]
    psycopg.Cursor.executemany = _orig["executemany"]  # type: ignore[method-assign]
    psycopg.AsyncCursor.execute = _orig["aexecute"]  # type: ignore[method-assign]
    psycopg.AsyncCursor.executemany = _orig["aexecutemany"]  # type: ignore[method-assign]
    _orig.clear()
    _installed = False


# --- judgment ----------------------------------------------------------------


def _target_for(connection: Any) -> str:
    info = getattr(connection, "info", None)
    host = getattr(info, "host", None) or "unknown"
    port = getattr(info, "port", None)
    return f"{host}:{port}" if port else str(host)


def _classify(err: BaseException) -> str:
    import psycopg

    if isinstance(err, psycopg.errors.QueryCanceled):
        return "timeout"  # statement_timeout / explicit cancellation
    if isinstance(err, TimeoutError):
        return "timeout"
    if isinstance(err, psycopg.OperationalError):
        return "conn"  # refused/reset/network-down/connect-timeout, bundled by libpq
    return "other"


def _ok(live: dict[str, Any], value: Any) -> dict[str, Any]:
    live["result"] = value
    live["have"] = True
    live["exc"] = None
    return {"status": "ok", "payload": None}  # never cached (module docs)


def _err(live: dict[str, Any], err: BaseException) -> dict[str, Any]:
    live["exc"] = err
    return {"status": "error", "class": _classify(err), "message": str(err)}


def _finish(target: str, outcome: dict[str, Any], live: dict[str, Any]) -> Any:
    """Deliver a core outcome using the side-band live objects (module docs;
    mirrors ``packs/tool.py._finish``) — a cache hit never happens here
    (``args_hash`` is always ``None``), so the ``live["have"]`` branch is the
    only real path on success."""
    if outcome.get("result") == "ok":
        return live["result"] if live["have"] else outcome.get("payload")
    err = outcome.get("error") or {}
    original = live["exc"]
    if original is not None:
        _attach(original, outcome)
        raise original
    synthetic = KeelError(err.get("code") or "KEEL-E040", err.get("message") or f"keel: query on {target} failed")
    _attach(synthetic, outcome)
    raise synthetic


def _attach(exc: BaseException, outcome: dict[str, Any]) -> None:
    try:
        exc.keel_outcome = outcome  # type: ignore[attr-defined]
    except Exception:
        pass


def _record(target: str, outcome: dict[str, Any], started: float) -> None:
    discovery = _runtime.get_discovery()
    if discovery is not None:
        discovery.record(target, outcome, round((time.perf_counter() - started) * 1000))


# --- sync seam -----------------------------------------------------------------


def _run_sync(self: Any, do_call: Callable[[], Any]) -> Any:
    backend = _runtime.get_backend()
    if backend is None:
        return do_call()  # disabled / uninstalled: transparent
    target = _target_for(self.connection)
    env = {"v": ENVELOPE_VERSION, "target": target, "op": f"execute {target}", "idempotent": False, "args_hash": None}
    live: dict[str, Any] = {"result": None, "have": False, "exc": None}

    def effect(_attempt: int) -> dict[str, Any]:
        try:
            value = do_call()
        except Exception as err:  # not BaseException: let exit/interrupt fly
            return _err(live, err)
        return _ok(live, value)

    started = time.perf_counter()
    outcome = backend.execute(env, effect)
    _record(target, outcome, started)
    return _finish(target, outcome, live)


def _sync_wrapper(orig: Callable[..., Any]) -> Callable[..., Any]:
    @functools.wraps(orig)
    def execute(self: Any, *args: Any, **kwargs: Any) -> Any:
        return _run_sync(self, lambda: orig(self, *args, **kwargs))

    execute.__keel_wrapped__ = True  # type: ignore[attr-defined]
    return execute


# --- async seam ------------------------------------------------------------


async def _run_async(self: Any, do_call: Callable[[], Any]) -> Any:
    backend = _runtime.get_backend()
    if backend is None:
        return await do_call()  # disabled / uninstalled: transparent
    target = _target_for(self.connection)
    env = {"v": ENVELOPE_VERSION, "target": target, "op": f"execute {target}", "idempotent": False, "args_hash": None}
    live: dict[str, Any] = {"result": None, "have": False, "exc": None}
    exec_async = getattr(backend, "execute_async", None)
    started = time.perf_counter()

    if callable(exec_async):
        # NATIVE async path: the core awaits our coroutine directly on the
        # caller's loop, arming a real per-attempt deadline (mirrors
        # packs/tool.py's async wrapper).
        async def aeffect(_attempt: int) -> dict[str, Any]:
            try:
                value = await do_call()
            except Exception as err:
                return _err(live, err)
            return _ok(live, value)

        outcome = await exec_async(env, aeffect)
    else:
        # STUB async path: the synchronous stub cannot await, so the attempt
        # is driven in a worker thread that marshals the await back onto this
        # loop (mirrors packs/tool.py's async wrapper).
        loop = asyncio.get_running_loop()

        def effect(_attempt: int) -> dict[str, Any]:
            future = asyncio.run_coroutine_threadsafe(do_call(), loop)
            try:
                value = future.result()
            except Exception as err:
                return _err(live, err)
            return _ok(live, value)

        outcome = await loop.run_in_executor(None, lambda: backend.execute(env, effect))
    _record(target, outcome, started)
    return _finish(target, outcome, live)


def _async_wrapper(orig: Callable[..., Any]) -> Callable[..., Any]:
    @functools.wraps(orig)
    async def execute(self: Any, *args: Any, **kwargs: Any) -> Any:
        return await _run_async(self, lambda: orig(self, *args, **kwargs))

    execute.__keel_wrapped__ = True  # type: ignore[attr-defined]
    return execute


def _is_pinned(version: str) -> bool:
    return any(version == p or version.startswith(p + ".") for p in _PINNED)


__all__ = ["MODULE", "NAME", "detect", "seams", "targets", "defaults", "install", "uninstall"]
