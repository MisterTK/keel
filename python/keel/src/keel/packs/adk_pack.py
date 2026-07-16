"""The Google ADK framework pack (dx-spec §4.2): a ``keel`` plugin registered
automatically at import time via ADK's own plugin/callback API.

ADK's vendor-documented gap (dx-spec §4): *"no built-in retry/backoff; error
handling is callback-based and left to the developer."* Verified directly
against the real ``google-adk`` package (2.4.0, throwaway venv — never a repo
dependency; adapter-pack rule 1 forbids importing a library that is not
present *and in use*): ``google.genai``'s ``SyncHttpxClient``/
``AsyncHttpxClient`` subclass ``httpx.Client``/``httpx.AsyncClient`` without
overriding ``__init__``, and its own retry helper defaults to
``stop_after_attempt(1)`` (never retries) unless the caller explicitly opts
into ``HttpRetryOptions`` — i.e. ADK genuinely retries nothing on its own,
matching the vendor claim exactly.

Two Keel target classes come out of this pack, split by where the seam lives:

* ``llm:google-genai`` — **needs no code here.** ADK's Gemini model backend
  rides the ``google-genai`` SDK, which rides ``httpx``; ``keel.adapters.
  httpx_pack`` already intercepts every ``httpx.Client``/``AsyncClient``
  request and ``adapters._http.LLM_HOST_PROVIDERS`` already host-maps
  ``generativelanguage.googleapis.com`` → ``llm:google-genai``. This pack
  declares the target (below) for documentation/doctor visibility only — the
  same "declared but not owned" pattern ``openai_pack``/``anthropic_pack`` use
  for their own ``llm:<provider>`` targets.
* ``tool:<name>`` — **is this pack's actual seam.** Every ADK tool invocation
  is wrapped through :func:`keel.packs.tool.wrap_tool` at ADK's own
  ``before_tool_callback`` plugin hook (below), giving every tool call
  discovery/breaker/timeout coverage for free and turning "on_tool_error
  wiring" from hand-written callback code into policy (dx-spec §4.2) — with
  retry itself opt-in exactly like every other ``tool:`` call site (Level 0
  hard rule — see ``keel.packs.tool``'s module docs; a framework pack never
  asserts re-invoke safety on a tool author's behalf).

Idempotency-key injection HAS landed in the shared HTTP adapters (CCR-2:
``contracts/adapter-pack.md`` "Idempotency-key injection";
``keel.adapters._http.resolve_idempotency_injection``) — policy-gated,
per-target opt-in. So an ``llm:google-genai`` POST is observed-not-retried
(KEEL-E014) by default, and becomes safely retryable when the target's
policy opts into injection. Tool calls are unaffected either way: a
framework pack never asserts re-invoke safety on a tool author's behalf.

Rebind-on-first-sight (why the plugin does NOT execute tools itself):
``PluginManager._run_callbacks`` stops at the first non-``None`` result AND
ADK's tool executor skips agent-level ``before_tool_callback``s and the real
call once a plugin returns non-``None`` — so a plugin that executes the tool
and returns its result silently bypasses agent-level before-callbacks, and a
failure raised from the plugin surfaces OUTSIDE the executor's try/except,
where user ``on_tool_error`` handlers (including ADK's own
``ReflectAndRetryToolPlugin``) never see it. Keel's callback therefore only
REBINDS ``tool.run_async`` (instance attribute shadowing the class method,
marker-guarded, restored by ``uninstall()``) and returns ``None``: ADK's
sequence proceeds exactly as unwrapped — agent before-callbacks run, the
real call happens at step 3 through Keel's wrapper (breaker/timeout guard
attempt 1: policy is consulted before the first underlying invoke), and a
terminal failure raises from the real call as the ORIGINAL exception, never
``RuntimeError``-wrapped. Instances that reject ``setattr`` (slots/frozen)
fall back to executing via the plugin loop — full Keel coverage, with the
documented cost that agent-level before-callbacks are bypassed for that
tool only (noted once on stderr).

MCP interplay: under ADK's ``_MCP_GRACEFUL_ERROR_HANDLING`` feature flag,
``McpTool`` converts failures into ``{"error": "<message>"}`` results. The
rebound wrapper detects MCP tools structurally (MRO class name) and counts
an error-shaped dict as a FAILURE for breaker/discovery accounting while
returning it to the agent unchanged. ``McpTool``'s own single blind retry
runs beneath Keel; each underlying JSON-RPC attempt is separately visible
to the ``mcp:<server>`` target at the transport seam (``keel.packs.
mcp_pack``), which sees raw failures regardless of the graceful flag.
"""

from __future__ import annotations

import functools
import hashlib
import importlib.metadata
import os
import sys
import weakref
from typing import Any, Callable

from .. import _runtime
from ..adapters._pack import Detection, Seam, TargetDecl
from .._errors import KeelError
from .._flow import backend_has_journal, backend_supports_flows
from ._provider import module_present
from .tool import is_valid_tool_name, wrap_tool

MODULE = "google.adk"

#: The designated Tier-2 Runner entrypoint (frozen `entrypointRef` grammar
#: `^(py|ts|rs):\S+$`; `_policy.py`'s rsplit-based parser already accepts a
#: dotted qualname like ``Runner.run_async`` as the function segment —
#: verified by reading ``extract_flow_entrypoints``). A user opts in by
#: adding this exact string to ``[flows] entrypoints`` in ``keel.toml``.
RUNNER_FLOW_ENTRYPOINT = "py:google.adk.runners:Runner.run_async"
#: The PyPI distribution name differs from the import name; used for the
#: `importlib.metadata.version` lookup only.
NAME = "google-adk"

#: Versions this pack certifies via contract tests (prefix match), verified
#: directly against the real package. Outside the range `detect()` reports
#: `best_effort` — the pack still tries (adapter-pack rule 2).
_PINNED = ("1", "2")

#: The registered plugin's unique name (`BasePlugin.name` / `PluginManager.
#: get_plugin`/`register_plugin` key).
PLUGIN_NAME = "keel"

_TRUTHY = {"1", "true", "yes"}

_installed = False
_orig: dict[str, Any] = {}
_plugin_singleton: Any = None
_noted_skips: set[str] = set()

#: Marker attribute set on a rebound instance's replacement ``run_async``.
_REBOUND_ATTR = "__keel_adk_rebound__"

#: Sentinel: the tool had NO instance-level ``run_async`` before rebinding
#: (the overwhelmingly normal case — the method lives on the class).
_ABSENT = object()

#: Rebound tool instance → its prior instance-dict ``run_async`` entry (or
#: ``_ABSENT``). Weak keys: a garbage-collected tool needs no restoration.
#: A tool that cannot be weak-referenced still gets rebound — it just cannot
#: be individually restored by ``uninstall()`` (noted in the docstring).
_rebound: "weakref.WeakKeyDictionary[Any, Any]" = weakref.WeakKeyDictionary()


# --- contract operations -----------------------------------------------------


def detect() -> Detection:
    """Present iff ``google.adk`` is importable — decided WITHOUT importing it
    (importability + installed distribution metadata only, adapter-pack rule
    1)."""
    if not module_present(MODULE):
        return Detection(matched=False)
    try:
        version = importlib.metadata.version(NAME)
    except importlib.metadata.PackageNotFoundError:
        version = ""
    confidence = "pinned" if _is_pinned(version) else "best_effort"
    return Detection(matched=True, name=NAME, version=version, confidence=confidence)


def seams() -> list[Seam]:
    return [
        Seam(
            patch_point="google.adk.runners.Runner.__init__",
            upstream_api=(
                "ADK Runner: Runner(*, app=None, agent=None, node=None, "
                "plugins=None, ...) resolves onto self.plugin_manager: "
                "PluginManager (register_plugin/get_plugin)"
            ),
            why_stable=(
                "Runner.__init__ is ADK's single construction chokepoint — "
                "InMemoryRunner.__init__ forwards to it via super().__init__, "
                "and agent=/node=/app= all resolve into one App before "
                "self.plugin_manager is built. Reading back plugin_manager "
                "post-construction and registering through its own documented "
                "get_plugin/register_plugin API (never poking private state) "
                "covers every construction shape uniformly and stays fully "
                "reversible. Tool coverage rides instance-level run_async "
                "rebinding done lazily from before_tool_callback — the callback "
                "itself always returns None for rebindable tools, preserving "
                "ADK's own callback and error sequence."
            ),
        ),
        Seam(
            patch_point="google.adk.runners.Runner.run_async",
            upstream_api=(
                "ADK Runner: run_async(self, *, user_id, session_id, "
                "invocation_id=None, new_message=None, ...) -> "
                "AsyncGenerator[Event, None]"
            ),
            why_stable=(
                "A class-level method patch on the same documented Runner "
                "surface as __init__ above — active only under explicit "
                "[flows] designation (RUNNER_FLOW_ENTRYPOINT in [flows] "
                "entrypoints). An undesignated Runner, or one built while "
                "Keel has no backend, sees the original async generator "
                "byte-for-byte via functools.wraps; a designated call opens "
                "a Tier 2 durable flow around the underlying event stream "
                "without altering the events ADK itself yields."
            ),
        ),
    ]


def targets() -> list[TargetDecl]:
    return [
        TargetDecl(
            pattern="tool:<name>",
            kind="tool",
            idempotency_rule=(
                "identical to the generic tool: pack (adapter-pack rule 3: no "
                "resilience logic of its own) — non-idempotent by default, "
                "observed not retried (KEEL-E014). This pack auto-wraps every "
                "plugin-visible ADK tool call with idempotent=False always: a "
                "generic framework pack cannot assert a given tool is safe to "
                "re-invoke on the tool author's behalf."
            ),
            args_hash_rule="None (a non-idempotent tool call is never cached)",
        ),
        TargetDecl(
            pattern="llm:google-genai",
            kind="llm",
            idempotency_rule=(
                "not produced by this pack: ADK's Gemini model backend rides "
                "the google-genai SDK, which rides httpx — already "
                "intercepted by keel.adapters.httpx_pack's transport seam and "
                "host-mapped to llm:google-genai (adapters/_http.py "
                "LLM_HOST_PROVIDERS). This pack owns no model-call seam of "
                "its own; declared here for doctor/documentation visibility "
                "only, mirroring openai_pack/anthropic_pack."
            ),
            args_hash_rule="as for the httpx host-mapped llm: target",
        ),
    ]


def defaults() -> dict[str, Any]:
    """Empty: contracts/defaults.toml defines no ``[defaults.adk]`` table, so
    both target classes above inherit through the existing chain
    (``tool:`` → ``[defaults.outbound]``; ``llm:google-genai`` →
    ``[defaults.llm]``) — mirrors the ``tool:``/``mcp:`` packs exactly."""
    return {}


# --- Tier 2 Runner-flow designation and wrap (WS5 core) ---------------------
#
# A user opts a specific ADK `Runner.run_async` invocation into Tier 2 by
# adding `RUNNER_FLOW_ENTRYPOINT` to `[flows] entrypoints` in `keel.toml`.
# This section holds the designation matcher, the Tier-2 gates, the
# flow-identity helpers, AND the async-generator wrap itself
# (`_run_async_wrapper`) that consumes them via
# `backend.enter_flow(*_runner_flow_identity(...))`.


def _flow_entrypoint_designated() -> str | None:
    """The designated Runner-flow entrypoint's raw string
    (``RUNNER_FLOW_ENTRYPOINT``, echoed back verbatim), or ``None`` when
    undesignated OR when Keel bootstrap is disabled/never ran.

    Reads ``keel.bootstrap._STATE.state`` directly — a deliberate
    module-private reach into a sibling module, not a new ``_runtime`` API.
    ``keel.bootstrap.install_keel()`` is re-entrant (its already-installed
    path returns the full cached state, including ``flow_entrypoints``), but
    calling it here to *read* that state would also *perform* a fresh
    install as a side effect on a bare/not-yet-bootstrapped process — wrong
    for what is meant to be a passive designation check invoked from
    arbitrary Runner-construction code. Reading the cached ``_STATE.state``
    instead never triggers that side effect: ``None`` (never installed, or
    ``KEEL_DISABLE`` short-circuited before ``_STATE.state`` was ever
    populated — ``install_keel`` returns before touching ``_STATE`` in that
    branch, so the two cases are indistinguishable here, and correctly so:
    both mean "no Tier 2 designation is in force") means undesignated,
    exactly like finding no matching entry.

    The match is EXACT: only an entrypoint whose parsed ``module``/
    ``function`` are precisely ``"google.adk.runners"`` /
    ``"Runner.run_async"`` designates this arm — a glob entrypoint (even one
    that could in principle resolve to the same pair) does not count, since
    `[flows] entrypoints` globs are designed for `keel run`'s script-path
    matching (`_flow.match_flow`), not for matching a live Python call site.
    """
    # Local import: avoids a circular import at module-load time (`bootstrap`
    # -> `keel.packs` -> this module, since `bootstrap.install_keel` imports
    # `keel.packs` for `install_mcp_pack`/`present_provider_defaults` — a
    # module-level `from .. import bootstrap` here would deadlock that chain
    # the first time either module is imported first; verified by import
    # order flip: `import keel.packs.adk_pack` before any `keel.bootstrap`
    # import raised `ImportError: cannot import name 'install_mcp_pack' from
    # partially initialized module 'keel.packs'`).
    from .. import bootstrap

    state = bootstrap._STATE.state
    if state is None:
        return None
    for entry in state.get("flow_entrypoints") or ():
        if entry.module == "google.adk.runners" and entry.function == "Runner.run_async":
            return entry.raw
    return None


def _flow_gates_or_raise(backend: Any) -> None:
    """Tier 2 requires the native core AND an attached journal — the same two
    gates `keel._flow.run_as_flow` checks for `keel run` (`_unsupported_on_stub`
    / `_unsupported_without_journal`), reused here via `backend_supports_flows`/
    `backend_has_journal` and re-worded for the Runner context. Unlike those
    CLI-facing helpers, this RAISES `KeelError` (KEEL-E005) rather than writing
    to stderr and calling `SystemExit` — a designated Runner call is a library
    call inside a long-lived process, not a CLI entrypoint to exit."""
    if not backend_supports_flows(backend):
        raise KeelError(
            "KEEL-E005",
            f"Tier 2 durable flow {RUNNER_FLOW_ENTRYPOINT!r} needs the native core.\n"
            "  why:  crash-safe resume journals and replays each step; the pure-Python "
            "stub backend cannot do that.\n"
            "  next: build the native module (`maturin develop` in crates/keel-py) or set "
            "KEEL_BACKEND=native, then re-run.",
        )
    if not backend_has_journal(backend):
        raise KeelError(
            "KEEL-E005",
            f"durable flow {RUNNER_FLOW_ENTRYPOINT!r} needs a journal, but none is attached.\n"
            "  why:  Tier 2 journals and replays each step; with no journal there is nothing "
            "to record to or resume from.\n"
            "  next: let the native core open .keel/journal.db (check KEEL_JOURNAL and "
            "directory permissions), or remove this entrypoint from [flows].",
        )


def _runner_args_hash(parts: list[str]) -> str:
    """A stable 16-hex-char sha256 of ``repr(list(parts))`` — the same
    algorithm as ``keel._flow._args_hash``, reimplemented here (not imported)
    to keep this pack decoupled from that module's private helper."""
    return hashlib.sha256(repr(list(parts)).encode("utf-8")).hexdigest()[:16]


def _content_fingerprint(new_message: Any) -> str:
    """A stable 16-hex-char sha256 of ``repr(new_message)`` — the fallback
    identity component `_runner_flow_identity` uses when ADK hands back no
    ``invocation_id`` (``None``): the message content itself stands in for
    "this specific call" so two calls with the same content still resume the
    same flow, and two different messages never collide."""
    return hashlib.sha256(repr(new_message).encode("utf-8")).hexdigest()[:16]


def _runner_flow_identity(
    user_id: str, session_id: str, invocation_id: str | None, new_message: Any
) -> tuple[str, str]:
    """The designated Runner flow's identity: ``(entrypoint_raw, args_hash)``,
    directly usable as ``backend.enter_flow(*_runner_flow_identity(...))``'s
    first two positional arguments.

    ``entrypoint_raw`` is always ``RUNNER_FLOW_ENTRYPOINT`` — the exact-match
    designation this whole module section exists for. ``args_hash`` folds in
    ``user_id``/``session_id`` plus a stable per-call identifier: ADK's own
    ``invocation_id`` when present, or ``_content_fingerprint(new_message)``
    when it is ``None`` — so the SAME ``(user, session, invocation)`` always
    hashes to the SAME value (stable identity across a crash/resume), while a
    different invocation (or, lacking one, different message content) hashes
    to a different value."""
    ident = invocation_id if invocation_id is not None else _content_fingerprint(new_message)
    return RUNNER_FLOW_ENTRYPOINT, _runner_args_hash([user_id, session_id, ident])


def _lease_ms() -> int | None:
    """`KEEL_FLOW_LEASE_MS`, read the same way `_flow.py:279-281` reads it for
    `keel run` — absent/empty means "let the backend pick its own default
    lease"."""
    raw = os.environ.get("KEEL_FLOW_LEASE_MS")
    return int(raw) if raw else None


_noted_busy = False


def _note_flow_busy() -> None:
    """Note (once per process, `KEEL_QUIET`-aware — mirrors `_note_skip`/
    `_note_fallback`) that a designated `Runner.run_async` call landed while
    another Tier 2 flow is already active on this backend. Keel's flow
    handle is a process-wide singleton (`_runtime.in_active_flow`), so a
    nested/concurrent designated call cannot open a second flow; it proceeds
    unwrapped rather than erroring."""
    global _noted_busy
    if _noted_busy:
        return
    _noted_busy = True
    if os.environ.get("KEEL_QUIET", "").strip().lower() in _TRUTHY:
        return
    sys.stderr.write(
        f"keel ▸ adk: {RUNNER_FLOW_ENTRYPOINT!r} invoked while another Tier 2 "
        "flow is already active on this backend — this call proceeds "
        "unwrapped (nested/concurrent designated Runner flows are not "
        "supported)\n"
    )


def _run_async_wrapper(orig: Callable[..., Any]) -> Callable[..., Any]:
    """Patch factory for `Runner.run_async` (installed by `install()`, stored
    as `_orig["run_async"]`, restored by `uninstall()`). Produces
    `_run_async_flow_wrapper`: an async-generator-aware wrap that opens a
    Tier 2 durable flow around a DESIGNATED call's event stream, and is
    byte-transparent (via `functools.wraps` over `orig`) for every other
    call — undesignated Runners, or any Runner built while Keel has no
    backend at all."""

    @functools.wraps(orig)
    async def _run_async_flow_wrapper(
        self: Any,
        *,
        user_id: str,
        session_id: str,
        invocation_id: str | None = None,
        new_message: Any = None,
        **kwargs: Any,
    ) -> Any:
        entry_raw = _flow_entrypoint_designated()
        backend = _runtime.get_backend()
        inner = lambda: orig(
            self,
            user_id=user_id,
            session_id=session_id,
            invocation_id=invocation_id,
            new_message=new_message,
            **kwargs,
        )
        if entry_raw is None or backend is None:
            async for event in inner():
                yield event
            return
        _flow_gates_or_raise(backend)  # loud KEEL-E005 (decision 5): never a silent un-journaled downgrade
        if _runtime.in_active_flow():  # singleton handle (decision 2): only one open flow per process
            _note_flow_busy()
            async for event in inner():
                yield event
            return
        _, args_hash = _runner_flow_identity(user_id, session_id, invocation_id, new_message)
        info = backend.enter_flow(
            entry_raw, args_hash, code_hash=None, explicit_key=invocation_id, lease_ms=_lease_ms()
        )
        replayed = bool(info.get("replay"))
        _runtime.set_flow_active(True)
        correlated = False
        try:
            gen = inner()
            async for event in gen:
                if not correlated:
                    inv = getattr(event, "invocation_id", "") or ""
                    backend.journal_random("adk:invocation_id", inv.encode("utf-8"))
                    correlated = True
                yield event
        except GeneratorExit:  # abandonment -> flow stays running (decision 8)
            raise
        except BaseException:
            if not replayed:  # never demote an already-completed (replayed) flow
                backend.exit_flow("failed")
            _runtime.set_flow_active(False)
            raise
        else:
            backend.exit_flow("completed")
            _runtime.set_flow_active(False)
        # NOTE: set_flow_active(False) deliberately NOT in a finally — on
        # GeneratorExit the flow stays open-and-running only until process
        # exit anyway (abandonment is the crash shape); a finally would also
        # fire on it and mask the running-flow semantics. This mirrors
        # `_flow.py`'s own asymmetric handling (its final `exit_flow(
        # "completed")` is unconditional on success, while every failure
        # branch guards on `replayed`). Test-adjudicated: see
        # `RunnerFlowWrapTest.test_abandonment_leaves_flow_running_and_active`
        # in `test_packs_adk.py`, which asserts `in_active_flow()` stays True
        # and NO `exit_flow` call is recorded after an `aclose()`.

    return _run_async_flow_wrapper


# --- install / uninstall -----------------------------------------------------


def install() -> None:
    """Patch ``Runner.__init__`` so every constructed Runner (any of
    ``agent=``/``node=``/``app=``) gets Keel's plugin auto-registered, AND
    ``Runner.run_async`` so a DESIGNATED invocation becomes a Tier 2 durable
    flow (``_run_async_wrapper``). Idempotent; a no-op if ``google.adk`` is
    not importable."""
    global _installed
    if _installed:
        return
    try:
        from google.adk.runners import Runner
    except ImportError:
        return
    _orig["init"] = Runner.__init__
    Runner.__init__ = _init_wrapper(_orig["init"])  # type: ignore[method-assign]
    _orig["run_async"] = Runner.run_async
    Runner.run_async = _run_async_wrapper(_orig["run_async"])  # type: ignore[method-assign]
    _installed = True


def uninstall() -> None:
    """Restore the original ``Runner.__init__``/``Runner.run_async`` and
    un-shadow every rebound tool instance still alive (weak registry — a
    collected tool needs nothing). Runners already constructed keep whatever
    plugins they were given (mirrors httpx_pack: uninstall un-arms future
    construction)."""
    global _installed
    _restore_rebound()
    if not _installed:
        return
    try:
        from google.adk.runners import Runner
    except ImportError:
        _orig.clear()
        _installed = False
        return
    Runner.__init__ = _orig["init"]  # type: ignore[method-assign]
    Runner.run_async = _orig["run_async"]  # type: ignore[method-assign]
    _orig.clear()
    _installed = False


def _restore_rebound() -> None:
    for tool, prior in list(_rebound.items()):
        try:
            if prior is _ABSENT:
                if "run_async" in tool.__dict__:
                    delattr(tool, "run_async")
            else:
                setattr(tool, "run_async", prior)
        except (AttributeError, TypeError):
            pass  # never let a hostile tool object break uninstall
    _rebound.clear()


def _init_wrapper(orig_init: Callable[..., None]) -> Callable[..., None]:
    @functools.wraps(orig_init)
    def __init__(self: Any, *args: Any, **kwargs: Any) -> None:
        orig_init(self, *args, **kwargs)
        _attach_plugin(self)

    __init__.__keel_wrapped__ = True  # type: ignore[attr-defined]
    return __init__


def _attach_plugin(runner: Any) -> None:
    """Register Keel's plugin on a freshly constructed Runner, unless one is
    already registered (a user's own, or — defensively — a prior
    construction): ``PluginManager.register_plugin`` raises ``ValueError`` on
    a name clash, so this pack checks first rather than relying on a catch."""
    manager = getattr(runner, "plugin_manager", None)
    register = getattr(manager, "register_plugin", None)
    get = getattr(manager, "get_plugin", None)
    if not callable(register) or not callable(get):
        return  # unexpected shape (an unsupported ADK version): do nothing unsafe
    if get(PLUGIN_NAME) is not None:
        return
    register(_plugin())


# --- the plugin ---------------------------------------------------------------


def _plugin() -> Any:
    """The shared ``KeelPlugin`` singleton, built lazily against whatever
    ``BasePlugin`` is importable RIGHT NOW — never at module import time, so
    this module stays stdlib-only when ``google.adk`` is absent (adapter-pack
    rule 1: a pack never imports its library unless present and in use)."""
    global _plugin_singleton
    if _plugin_singleton is None:
        from google.adk.plugins.base_plugin import BasePlugin

        class _KeelPlugin(BasePlugin):  # noqa: keep tiny — one callback, one job
            def __init__(self) -> None:
                super().__init__(name=PLUGIN_NAME)

            async def before_tool_callback(
                self, *, tool: Any, tool_args: dict[str, Any], tool_context: Any
            ) -> dict[str, Any] | None:
                return await _on_before_tool(tool, tool_args, tool_context)

        _plugin_singleton = _KeelPlugin()
    return _plugin_singleton


async def _on_before_tool(tool: Any, tool_args: dict[str, Any], tool_context: Any) -> dict[str, Any] | None:
    """The ``before_tool_callback`` body. Returning ``None`` is the contract
    that keeps ADK's own sequence intact: agent-level before-callbacks still
    run (ADK only skips them on a non-``None`` plugin return), the real call
    happens at ADK's step 3 through our rebound wrapper, and a terminal
    failure raises from the real call — inside ADK's try/except — so user
    ``on_tool_error`` plugins/callbacks (including ADK's own
    ``ReflectAndRetryToolPlugin``) fire exactly as they would unwrapped."""
    name = getattr(tool, "name", None)
    if not is_valid_tool_name(name):
        _note_skip(name)
        return None  # not our target grammar: let ADK invoke it, unwrapped
    if getattr(getattr(tool, "run_async", None), _REBOUND_ATTR, False):
        return None  # already rebound on a prior sight
    if _rebind_tool(tool, name):
        return None  # rebound: ADK proceeds normally, Keel wraps the real call
    # Instance rejects setattr (slots/frozen): keep coverage via the old
    # loop-in-callback path — documented trade-off (agent-level
    # before-callbacks are bypassed for THIS tool only).
    _note_fallback(name)
    return await _call_via_plugin_loop(tool, tool_args, tool_context)


class _McpErrorDict(Exception):
    """Internal sentinel: an ADK graceful-error dict, raised inside the
    wrapped effect so the core records a failure, then caught at the rebound
    wrapper and unwrapped — the agent-visible value never changes."""

    def __init__(self, payload: dict[str, Any]) -> None:
        super().__init__(str(payload.get("error", "")))
        self.payload = payload


def _is_mcp_tool(tool: Any) -> bool:
    """MRO-name check (no ADK import, works for subclasses): ADK's class is
    ``google.adk.tools.mcp_tool.mcp_tool.McpTool`` (verified against the real
    package during Task 3 Step 1: ``class McpTool(BaseAuthenticatedTool)`` at
    ``mcp_tool.py`` line 124, plus its deprecated alias
    ``class MCPTool(McpTool)`` at line 602 — matching either MRO class name
    covers both spellings without importing ``google.adk``)."""
    return any(c.__name__ in ("McpTool", "MCPTool") for c in type(tool).__mro__)


def _is_mcp_error_dict(result: Any) -> bool:
    """Mirror of ADK's graceful-error shape (``_MCP_GRACEFUL_ERROR_HANDLING``
    → ``{"error": "<message>"}``, exactly one key, string value). Verified
    directly against ``mcp_tool.py``'s ``run_async`` (lines 358-367, Task 3
    Step 1): under the feature flag, both swallowed-failure branches return
    exactly this shape — ``except McpError as e: return {"error": f"MCP tool
    execution failed: {e}"}`` and ``except Exception as e: return {"error":
    f"Unexpected error during MCP tool execution: {e}"}`` — no other keys, a
    plain ``str`` message either way, confirming the plan's expected shape
    exactly (no divergence to mirror). Note: ``_detect_error_in_response``
    (line 472, checking a DIFFERENT shape — ``{"isError": True}``, the raw
    MCP tool-result convention) IS wired up, but only into ADK's own
    ``functions.py`` ``_detect_error_type_for_telemetry`` — logging only,
    explicitly documented there as "does not modify the response" and swallows
    its own exceptions — so it never changes what `run_async` returns and is
    not the rule to mirror here; the ``{"error": ...}`` shape checked above is
    the one that actually reaches the agent as the tool's result. Deliberately
    strict: a non-MCP-shaped dict is a tool RESULT and must never be
    reclassified."""
    return (
        isinstance(result, dict)
        and set(result) == {"error"}
        and isinstance(result["error"], str)
    )


def _rebind_tool(tool: Any, name: str) -> bool:
    """Rebind ``tool.run_async`` (instance attribute shadowing the class
    method) to a Keel-wrapped version. The breaker still guards attempt 1:
    ``wrap_tool``'s wrapper consults policy before the first underlying
    invoke. Returns ``False`` when the instance rejects attribute assignment.

    MCP error-dict classification: under ADK's ``_MCP_GRACEFUL_ERROR_HANDLING``
    feature flag, a failed ``McpTool`` call RETURNS ``{"error": "..."}``
    instead of raising — a naive wrapper records success and the breaker
    never trips. For an ``McpTool`` (``is_mcp``, detected once per rebind),
    the wrapped effect raises the internal ``_McpErrorDict`` sentinel on an
    MCP-shaped error dict so ``wrap_tool``'s core records a failure
    (``classify_tool_error`` files it under ``other``); the rebound
    ``run_async`` unwraps it back to the identical payload on the way out, so
    the agent sees byte-identical output either way."""
    original = tool.run_async  # the bound method, captured pre-shadow
    is_mcp = _is_mcp_tool(tool)

    async def _invoke(*, args: dict[str, Any], tool_context: Any) -> Any:
        result = await original(args=args, tool_context=tool_context)
        if is_mcp and _is_mcp_error_dict(result):
            raise _McpErrorDict(result)  # counted as a failure by the core
        return result

    _invoke.__qualname__ = f"adk.tool.{name}"
    inner = wrap_tool(name, _invoke, idempotent=False)

    @functools.wraps(original)
    async def run_async(*, args: dict[str, Any], tool_context: Any) -> Any:
        try:
            return await inner(args=args, tool_context=tool_context)
        except _McpErrorDict as err:
            return err.payload  # unwrap: the agent sees exactly what ADK produced

    setattr(run_async, _REBOUND_ATTR, True)
    prior = tool.__dict__.get("run_async", _ABSENT) if hasattr(tool, "__dict__") else _ABSENT
    try:
        setattr(tool, "run_async", run_async)
    except (AttributeError, TypeError):
        return False
    try:
        _rebound[tool] = prior
    except TypeError:
        pass  # not weakref-able: rebound, but uninstall() cannot restore it
    return True


async def _call_via_plugin_loop(tool: Any, tool_args: dict[str, Any], tool_context: Any) -> dict[str, Any] | None:
    """Setattr-rejection fallback path: when a tool instance rejects
    ``setattr`` (slots/frozen), execute the tool's Keel wrapper via the
    plugin loop instead of rebinding ``run_async``. Resolve the tool's name
    against the frozen ``tool:<name>`` grammar — skip-and-note (Level 0:
    "if a call site cannot be wrapped safely, do nothing" — ``tool.py``'s
    own guidance for auto-wrap packs) — or drive the full retry-eligible
    call through :func:`keel.packs.tool.wrap_tool` and hand back a result
    ADK's own normalization already treats identically to a real call's raw
    return (``__build_response_event``: ``if not isinstance(function_result,
    dict): function_result = {'result': function_result}``) — this pack just
    applies the same rule one step earlier, so the plugin's ``is not None``
    short-circuit check is unambiguous even for a tool that legitimately
    returns ``None``."""
    name = getattr(tool, "name", None)
    if not is_valid_tool_name(name):
        _note_skip(name)
        return None  # not our target grammar: let ADK invoke it, unwrapped

    async def _invoke() -> Any:
        return await tool.run_async(args=tool_args, tool_context=tool_context)

    _invoke.__qualname__ = f"adk.tool.{name}"
    result = await wrap_tool(name, _invoke, idempotent=False)()
    return result if isinstance(result, dict) else {"result": result}


def _note_skip(name: object) -> None:
    """Note an unwrappable tool name once (not per-call, to avoid log spam)."""
    key = repr(name)
    if key in _noted_skips:
        return
    _noted_skips.add(key)
    if os.environ.get("KEEL_QUIET", "").strip().lower() in _TRUTHY:
        return
    sys.stderr.write(
        f"keel ▸ adk: tool name {name!r} does not match the tool: target "
        "grammar ([A-Za-z0-9_][A-Za-z0-9_.-]*) — left unwrapped (still runs, "
        "just without Keel's breaker/timeout/discovery coverage)\n"
    )


_noted_fallbacks: set[str] = set()


def _note_fallback(name: str) -> None:
    """Note (once per name) a tool instance that rejected the rebind: Keel
    keeps full coverage via the plugin-loop path, but agent-level
    before_tool_callbacks are bypassed for this tool — worth a line."""
    if name in _noted_fallbacks:
        return
    _noted_fallbacks.add(name)
    if os.environ.get("KEEL_QUIET", "").strip().lower() in _TRUTHY:
        return
    sys.stderr.write(
        f"keel ▸ adk: tool {name!r} rejects attribute rebinding (slots/frozen) — "
        "covered via the plugin loop instead; agent-level before_tool_callbacks "
        "are bypassed for this tool only\n"
    )


def _is_pinned(version: str) -> bool:
    return any(version == p or version.startswith(p + ".") for p in _PINNED)


__all__ = [
    "MODULE",
    "NAME",
    "PLUGIN_NAME",
    "RUNNER_FLOW_ENTRYPOINT",
    "detect",
    "seams",
    "targets",
    "defaults",
    "install",
    "uninstall",
]
