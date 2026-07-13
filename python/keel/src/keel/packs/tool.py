"""The ``tool:`` semantic target pack + the wrap API framework packs build on.

DX spec §4.1: ``tool:<name>`` — agent tool invocations, wrapped at the
framework's own boundary so retries happen *below* the LLM loop (a failed tool
call is retried without burning tokens on a new LLM turn). This module is the
Python half of that foundation (the Node twin is
``node/keel/src/packs/tool.mjs``): framework packs (pydantic-ai, openai-agents,
crewai, ADK, langgraph, …) call :func:`wrap_tool` at their tool-execution seam
and get policy resolution, discovery recording, and doctor visibility for free.
Like the generic ``llm`` pack, this pack owns NO seam of its own
(``seams() == []``) — the seam belongs to each framework pack.

Defaults (documented decision): contracts/defaults.toml — the frozen
smart-defaults pack — defines NO ``[defaults.tool]`` table, so ``tool:``
targets ship no defaults fragment of their own (``defaults() == {}``,
mirroring the ``mcp:`` pack) and inherit the generic profile through the
backend's target resolution: exact ``[target."tool:<name>"]`` first, then
``[defaults.outbound]``. Because a tool call is NON-IDEMPOTENT by default,
that inherited profile is effectively the generic non-idempotent one: the
outbound retry layer is inert (an error is observed, not retried —
KEEL-E014), nothing is cached (``args_hash`` is ``None``), and the breaker
still protects the target (repeated failures fail fast, KEEL-E012).

Idempotency (Level 0 hard rule, dx-spec §1): a tool runs arbitrary
side-effecting code, so a tool call is never auto-retried.
``wrap_tool(..., idempotent=True)`` is the opt-in, made by the framework pack
(or the tool's author) AT THE WRAP SITE — the safety judgment lives in the
adapter, closest to the library (contracts/adapter-pack.md). There is
deliberately no keel.toml knob to flip it in v0.1: the frozen policy schema
has none and we do not invent policy surface — the same v0.1 decision as the
``mcp:`` pack's tools/call rule. A declared-idempotent tool then retries per
policy on transient classes (see :func:`classify_tool_error`) and may opt
into caching via an explicit ``[target."tool:<name>"] cache`` ttl; any other
exception is class ``other`` — NOT in the default ``retry.on`` — so a plain
tool bug propagates unchanged unless the target's policy adds ``other``.

Timeouts: the per-attempt wall-clock timeout is armed by the CORE, only for
idempotent requests and only where the effect actually awaits (the native
async path; see ``crates/keel-core`` ``run_attempts``). The wrapper injects no
deadline of its own — same contract as the ``py:``/``ts:`` function wrappers.

Live objects vs. the core boundary: identical to ``keel._wrap`` — the return
value / raised exception are held side-band (the native core requires a JSON
payload), the live result is returned on the success path, the ORIGINAL
exception re-raises on terminal failure (DX invariant 5), and only a cache
HIT returns the round-tripped JSON payload.
"""

from __future__ import annotations

import asyncio
import functools
import inspect
import re
import time
from typing import Any, Callable

from .. import _runtime
from .._errors import KeelError
from .._wrap import ENVELOPE_VERSION, WRAPPED_ATTR, _args_hash, _attach_outcome, _json_safe
from ..adapters._pack import Detection, Seam, TargetDecl

#: Marker attribute carrying the wrapper's resolved ``tool:<name>`` target, so
#: framework packs and tooling can recognise (and avoid double-wrapping) a
#: Keel-wrapped tool. The Node twin sets ``__keelTarget``.
TARGET_ATTR = "__keel_target__"

#: The <name> part of the frozen targetKey grammar for semantic targets
#: (contracts/policy.schema.json: ``^(llm|tool|mcp):[A-Za-z0-9_][A-Za-z0-9_.-]*$``).
#: Exact-match keys only — the grammar has no glob form for ``tool:`` targets.
_TOOL_NAME_RE = re.compile(r"[A-Za-z0-9_][A-Za-z0-9_.-]*")


def is_valid_tool_name(name: object) -> bool:
    """True iff ``name`` can appear as ``tool:<name>`` in keel.toml. Packs that
    auto-wrap every framework-registered tool should check-and-skip (noting the
    skip for doctor) rather than let :func:`wrap_tool` raise mid-bootstrap —
    "if a call site cannot be wrapped safely, do nothing and note it"."""
    return isinstance(name, str) and _TOOL_NAME_RE.fullmatch(name) is not None


def tool_target(name: str) -> str:
    """The policy target key for a tool name, validated against the frozen
    target grammar. An invalid name could never be a keel.toml key, so wrapping
    it would create an unroutable target — config-shaped misuse, KEEL-E001."""
    if not is_valid_tool_name(name):
        raise KeelError(
            "KEEL-E001",
            f"invalid tool name {name!r}: a tool: target must match "
            "[A-Za-z0-9_][A-Za-z0-9_.-]* (contracts/policy.schema.json targetKey); "
            "rename the tool or skip wrapping it",
        )
    return f"tool:{name}"


def classify_tool_error(err: BaseException) -> str:
    """Classify a raised tool exception into a core error class.
    ``TimeoutError`` (asyncio's since 3.11, ``socket.timeout``) → ``timeout``
    and ``ConnectionError`` (reset/refused/aborted) → ``conn`` — both in the
    default ``retry.on``, so a declared-idempotent tool retries transient
    infrastructure failures out of the box (the tool: analogue of the mcp
    pack's classifier). Everything else is ``other``: a tool bug propagates
    unchanged by default. ``asyncio.CancelledError`` is a ``BaseException``
    and never reaches this — caller cancellation escapes the wrapper unchanged
    (parity with the Node twin's ``AbortError`` → ``cancelled``)."""
    if isinstance(err, TimeoutError):
        return "timeout"
    if isinstance(err, ConnectionError):
        return "conn"
    return "other"


def _ok(live: dict[str, Any], value: Any) -> dict[str, Any]:
    live["result"] = value
    live["have"] = True
    live["exc"] = None
    return {"status": "ok", "payload": _json_safe(value)}


def _err(live: dict[str, Any], err: BaseException) -> dict[str, Any]:
    live["exc"] = err
    return {"status": "error", "class": classify_tool_error(err), "message": str(err)}


def _record(target: str, outcome: dict[str, Any], started: float) -> None:
    discovery = _runtime.get_discovery()
    if discovery is not None:
        discovery.record(target, outcome, round((time.perf_counter() - started) * 1000))


def _finish(name: str, outcome: dict[str, Any], live: dict[str, Any]) -> Any:
    """Deliver a core outcome using the side-band live objects (module docs)."""
    if outcome.get("result") == "ok":
        # Live call → the real return value, unchanged (identity preserved);
        # cache hit → the round-tripped JSON payload (no live call to return).
        if live["have"] and not outcome.get("from_cache"):
            return live["result"]
        return outcome.get("payload")
    err = outcome.get("error") or {}
    original = live["exc"]
    if original is not None:
        _attach_outcome(original, outcome)
        raise original
    # No side-band original (e.g. a breaker fast-fail, KEEL-E012): surface the
    # core's own error, still carrying the outcome.
    synthetic = KeelError(err.get("code") or "KEEL-E040", err.get("message") or f"keel: tool {name} failed")
    _attach_outcome(synthetic, outcome)
    raise synthetic


def wrap_tool(name: str, fn: Callable[..., Any], *, idempotent: bool = False) -> Callable[..., Any]:
    """Wrap a tool callable as the ``tool:<name>`` policy target.

    The small API framework packs build on: each call routes through the
    backend's ``execute`` under ``tool:<name>`` (policy: exact target, then
    ``[defaults.outbound]``) and is recorded in discovery. ``fn`` may be sync
    or ``async def`` — the wrapper matches (an async tool returns an async
    wrapper driving the native ``execute_async`` path when available, else the
    stub's worker-thread marshal, exactly like the httpx async seam).

    ``idempotent`` defaults to ``False``: a tool call is observed, never
    retried (KEEL-E014 on a would-be-retryable error). Pass ``idempotent=True``
    ONLY for a tool that is safe to re-invoke (a read); that declaration is the
    wrap site's assertion, exactly as listing a ``py:`` target is the user's
    (see module docs). ``args_hash`` is derived only for idempotent tools — a
    side-effecting tool is never served from cache, even under a misconfigured
    ``[target]`` cache layer.
    """
    target = tool_target(name)
    if not callable(fn):
        raise KeelError("KEEL-E001", f"wrap_tool({name!r}): fn must be callable")
    op = f"tool {name} {getattr(fn, '__qualname__', None) or '?'}"

    def build_request(args: tuple[Any, ...], kwargs: dict[str, Any]) -> dict[str, Any]:
        return {
            "v": ENVELOPE_VERSION,
            "target": target,
            "op": op,
            "idempotent": idempotent,
            "args_hash": _args_hash(args, kwargs) if idempotent else None,
        }

    if not inspect.iscoroutinefunction(fn):

        @functools.wraps(fn)
        def wrapper(*args: Any, **kwargs: Any) -> Any:
            backend = _runtime.get_backend()
            if backend is None:
                return fn(*args, **kwargs)  # disabled / uninstalled: transparent
            live: dict[str, Any] = {"result": None, "have": False, "exc": None}

            def effect(_attempt: int) -> dict[str, Any]:
                try:
                    value = fn(*args, **kwargs)
                except Exception as err:  # not BaseException: let exit/interrupt fly
                    return _err(live, err)
                return _ok(live, value)

            started = time.perf_counter()
            outcome = backend.execute(build_request(args, kwargs), effect)
            _record(target, outcome, started)
            return _finish(name, outcome, live)

        setattr(wrapper, WRAPPED_ATTR, True)
        setattr(wrapper, TARGET_ATTR, target)
        return wrapper

    @functools.wraps(fn)
    async def async_wrapper(*args: Any, **kwargs: Any) -> Any:
        backend = _runtime.get_backend()
        if backend is None:
            return await fn(*args, **kwargs)  # disabled / uninstalled: transparent
        live: dict[str, Any] = {"result": None, "have": False, "exc": None}
        exec_async = getattr(backend, "execute_async", None)
        started = time.perf_counter()
        if callable(exec_async):
            # NATIVE async path: the core awaits our coroutine on the caller's
            # loop (mirrors adapters.httpx_pack._run_async). Each attempt calls
            # fn(...) again — a coroutine is single-await.
            async def aeffect(_attempt: int) -> dict[str, Any]:
                try:
                    value = await fn(*args, **kwargs)
                except Exception as err:
                    return _err(live, err)
                return _ok(live, value)

            outcome = await exec_async(build_request(args, kwargs), aeffect)
        else:
            # STUB async path: the synchronous stub cannot await, so attempts
            # are driven in a worker thread that marshals each await back onto
            # this loop (mirrors adapters.httpx_pack._run_async).
            loop = asyncio.get_running_loop()

            def effect(_attempt: int) -> dict[str, Any]:
                future = asyncio.run_coroutine_threadsafe(fn(*args, **kwargs), loop)
                try:
                    value = future.result()
                except Exception as err:
                    return _err(live, err)
                return _ok(live, value)

            request = build_request(args, kwargs)
            outcome = await loop.run_in_executor(None, lambda: backend.execute(request, effect))
        _record(target, outcome, started)
        return _finish(name, outcome, live)

    setattr(async_wrapper, WRAPPED_ATTR, True)
    setattr(async_wrapper, TARGET_ATTR, target)
    return async_wrapper


class _ToolPack:
    """The ``tool:`` adapter pack — the four uniform operations (adapter-pack.md).

    A semantic pack like ``llm``: it always "matches" (there is no external
    library version to pin, so confidence is ``pinned``) and owns no seam —
    targets are produced by :func:`wrap_tool` at each framework pack's own
    tool-execution boundary.
    """

    def detect(self) -> Detection:
        return Detection(matched=True, name="tool", confidence="pinned")

    def seams(self) -> list[Seam]:
        # No seam of its own: the tool-execution seam belongs to the framework
        # pack that calls wrap_tool (it declares the patch point + stability).
        return []

    def targets(self) -> list[TargetDecl]:
        return [
            TargetDecl(
                pattern="tool:<name>",
                kind="tool",
                idempotency_rule=(
                    "a tool call is non-idempotent by default — observed, not "
                    "retried (KEEL-E014) — unless the wrapping pack declares "
                    "idempotent=True at the wrap site (the safety judgment lives "
                    "in the adapter); no keel.toml knob flips it in v0.1"
                ),
                args_hash_rule=(
                    "sha256 over the repr-normalized call args for a "
                    "declared-idempotent tool (cache-key material; caching still "
                    "needs an explicit [target] cache ttl); None otherwise — a "
                    "side-effecting tool is never served from cache"
                ),
            )
        ]

    def defaults(self) -> dict[str, Any]:
        """Empty: contracts/defaults.toml defines no ``[defaults.tool]``, so
        ``tool:`` targets inherit ``[defaults.outbound]`` via the backend's
        target resolution (mirrors the mcp pack)."""
        return {}


#: The ``tool:`` pack singleton (mirrors Node's ``toolPack``).
tool_pack = _ToolPack()


__all__ = [
    "TARGET_ATTR",
    "classify_tool_error",
    "is_valid_tool_name",
    "tool_pack",
    "tool_target",
    "wrap_tool",
]
