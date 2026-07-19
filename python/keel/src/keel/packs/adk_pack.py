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

* ``llm:google-genai`` — **needs no transport code here.** ADK's Gemini model
  backend rides the ``google-genai`` SDK, which rides ``httpx``; ``keel.
  adapters.httpx_pack`` already intercepts every ``httpx.Client``/
  ``AsyncClient`` request and ``adapters._http.LLM_HOST_PROVIDERS`` already
  host-maps ``generativelanguage.googleapis.com`` → ``llm:google-genai``. This
  pack declares the target (below) for documentation/doctor visibility, the
  same "declared but not owned" pattern ``openai_pack``/``anthropic_pack`` use
  for their own ``llm:<provider>`` targets — EXCEPT for one seam the
  transport layer structurally cannot cover: cross-PROVIDER fallback. The
  transport seam can only rewrite a model name on the SAME host/endpoint it
  already targeted (a JSON body field or URL path segment); it can never
  construct a request for a genuinely different provider (different auth,
  endpoint, request/response shape). ``_KeelPlugin.on_model_error_callback``
  below fills that gap: ADK's own ``on_model_error`` plugin hook is a real
  Python call site that CAN build a fresh request via ``google.adk.models.
  registry.LLMRegistry``, so a ``fallback`` chain entry naming a different
  provider's model is actually dispatched to that provider, not merely
  rewritten onto the failing one's endpoint.
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
import threading
import weakref
from typing import Any, Callable

from .. import _runtime
from ..adapters import _http, _llm_policy
from ..adapters._pack import Detection, Seam, TargetDecl
from .._errors import KeelError
from .._flow import backend_has_journal, backend_supports_flows, exit_flow_or_warn
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

#: Serializes `_on_before_tool`'s "already rebound?" check-then-act against
#: `_rebind_tool` ACROSS THREADS. Two ADK Runner sessions can share a tool
#: instance while each drives its own event loop on its own OS thread; within
#: a single event loop the check-then-act is already safe (no `await` sits
#: between them, so asyncio can't interleave two callbacks mid-sequence), but
#: nothing serializes the same sequence across two real threads. A plain
#: `threading.Lock` held only across the check + the (fully synchronous, no
#: `await` inside it) `_rebind_tool` call briefly stalls a concurrent
#: callback on another thread, never the event loop itself.
_rebind_lock = threading.Lock()


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
                "its own; declared here for doctor/documentation visibility, "
                "mirroring openai_pack/anthropic_pack. It DOES additionally "
                "enforce this target's `fallback` chain at the plugin level "
                "(`on_model_error_callback`) for genuinely cross-provider "
                "hops — the one Python call site that can construct a "
                "request for a different provider; the transport seam above "
                "can only rewrite the model name on the same host."
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

    Reads ``_runtime.get_flow_entrypoints()`` — the same process-global
    accessor shape ``get_backend()``/``in_active_flow()`` already use, set
    once by ``bootstrap.install_keel()`` via ``_runtime.set_flow_entrypoints``
    right after ``_policy.extract_flow_entrypoints`` computes it. A plain
    ``_runtime`` read has no install side effect (exactly like those two
    accessors), so — unlike calling ``install_keel()`` itself, which is
    re-entrant and would perform a fresh install as an unwanted side effect
    on a bare/not-yet-bootstrapped process — this is safe to call from
    arbitrary Runner-construction code as a passive designation check.
    ``get_flow_entrypoints()`` returns ``()`` both when Keel was never
    installed and when ``KEEL_DISABLE`` short-circuited before installing
    (``install_keel`` returns before touching ``_runtime`` in that branch),
    so the two cases are indistinguishable here, and correctly so: both mean
    "no Tier 2 designation is in force", same as finding no matching entry.

    The match is EXACT: only an entrypoint whose parsed ``module``/
    ``function`` are precisely ``"google.adk.runners"`` /
    ``"Runner.run_async"`` designates this arm — a glob entrypoint (even one
    that could in principle resolve to the same pair) does not count, since
    `[flows] entrypoints` globs are designed for `keel run`'s script-path
    matching (`_flow.match_flow`), not for matching a live Python call site.
    """
    for entry in _runtime.get_flow_entrypoints():
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
    backend at all.

    Note: unlike `_flow.py`'s `run_as_flow`, `KeyboardInterrupt` (and
    `asyncio.CancelledError` — an async-generator's own abandonment/cancel
    signal, name-checked here alongside it since both reach this same
    `except BaseException` arm) here intentionally follow the same
    failed-path as any other `BaseException` rather than being left
    `running` for resume — this wrapper runs inside a SURVIVING process (a
    long-lived Runner host), not one about to exit, so the same
    surviving-process rationale that governs `GeneratorExit` below applies:
    leaving the flow open-forever on interrupt is never free here."""

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
        except GeneratorExit:
            # Abandonment (client disconnect, caller aclose) in a SURVIVING
            # process: release the handle so the next same-identity turn can
            # re-enter and substitute completed steps. Counts an attempt
            # (exit "failed"), unlike keel run's KeyboardInterrupt precedent
            # — there the process dies, so leaving the flow running is free.
            # `exit_flow_or_warn` (not a bare `backend.exit_flow` call)
            # degrades a journal-WRITE failure to a stderr line rather than
            # letting it raise out of this handler — a new exception raised
            # while handling `GeneratorExit` would make the caller's
            # `aclose()` itself raise instead of closing quietly, which ADK's
            # Runner is not written to expect (issue #14).
            if not replayed:
                exit_flow_or_warn(backend, "failed")
            _runtime.set_flow_active(False)
            raise
        except BaseException:
            if not replayed:  # never demote an already-completed (replayed) flow
                exit_flow_or_warn(backend, "failed")
            _runtime.set_flow_active(False)
            raise
        else:
            exit_flow_or_warn(backend, "completed")
            _runtime.set_flow_active(False)
        # NOTE (decision 8, revised): abandonment now exits the flow "failed"
        # and clears flow_active exactly like any other failure, rather than
        # leaving the handle open-and-running forever — this wrapper lives in
        # a SURVIVING process (a long-lived Runner host), where an
        # open-forever handle would wedge every later same-identity turn
        # (silently unwrapped) and make in-process resume impossible. This
        # differs from `_flow.py`'s `KeyboardInterrupt` precedent, which
        # leaves the flow `running` for resume BECAUSE there the process is
        # about to die — an open handle costs nothing in that shape.
        # `exit_flow("completed")` in the success (`else`) branch stays
        # unconditional on `replayed`, mirroring `_flow.py`'s own asymmetric
        # handling (unconditional success stamp, `replayed`-guarded failure
        # stamp). Test-adjudicated: see `RunnerFlowWrapTest.
        # test_abandonment_releases_the_flow_for_in_process_retry` and
        # `test_replay_completed_entry_never_demoted` in `test_packs_adk.py`.

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

            async def on_model_error_callback(
                self, *, callback_context: Any, llm_request: Any, error: Exception
            ) -> Any:
                return await _model_fallback(llm_request, error)

        _plugin_singleton = _KeelPlugin()
    return _plugin_singleton


async def _on_before_tool(tool: Any, tool_args: dict[str, Any], tool_context: Any) -> dict[str, Any] | None:
    """The ``before_tool_callback`` body. Returning ``None`` is the contract
    that keeps ADK's own sequence intact: agent-level before-callbacks still
    run (ADK only skips them on a non-``None`` plugin return), the real call
    happens at ADK's step 3 through our rebound wrapper, and a terminal
    failure raises from the real call — inside ADK's try/except — so user
    ``on_tool_error`` plugins/callbacks (including ADK's own
    ``ReflectAndRetryToolPlugin``) fire exactly as they would unwrapped.

    The "already rebound?" check and the ``_rebind_tool`` call are a
    check-then-act, held under ``_rebind_lock`` to serialize them ACROSS
    THREADS: two ADK Runner sessions on separate OS threads (each driving
    its own event loop) can share a tool instance, and without the lock both
    threads' checks can observe "not yet rebound" before either thread's
    ``setattr`` (inside ``_rebind_tool``) has landed — the second thread's
    ``_rebind_tool`` call then captures the FIRST thread's already-installed
    wrapper as ``original``, silently double-wrapping the tool (double
    breaker/retry/discovery accounting on every real call through it from
    then on). ``_rebind_tool`` has no ``await`` inside it (fully
    synchronous), so holding a plain ``threading.Lock`` across it briefly
    serializes a concurrent callback on another thread without risking an
    event-loop deadlock."""
    name = getattr(tool, "name", None)
    if not is_valid_tool_name(name):
        _note_skip(name)
        return None  # not our target grammar: let ADK invoke it, unwrapped
    with _rebind_lock:
        if getattr(getattr(tool, "run_async", None), _REBOUND_ATTR, False):
            return None  # already rebound on a prior sight
        if _rebind_tool(tool, name):
            return None  # rebound: ADK proceeds normally, Keel wraps the real call
    # Instance rejects setattr (slots/frozen): keep coverage via the old
    # loop-in-callback path — documented trade-off (agent-level
    # before-callbacks are bypassed for THIS tool only).
    _note_fallback(name)
    return await _call_via_plugin_loop(tool, tool_args, tool_context)


def _model_fallback_chain() -> list[str]:
    """The configured ``llm:google-genai`` fallback chain, read live per call
    (mirrors ``httpx_pack``/``requests_pack``'s own ``_llm_fallback_chain``)
    — non-list or missing config collapses to ``[]`` (fast path: no chain
    configured, do nothing)."""
    cfg = _http.resolve_layer("llm:google-genai", "fallback")
    return [m for m in cfg if isinstance(m, str) and m] if isinstance(cfg, list) else []


def _resolve_model_class(registry: Any, name: Any, *, note_on_failure: bool = False) -> Any | None:
    """``LLMRegistry.resolve(name)``, or ``None`` on any failure (unknown
    model name / not a string). ``note_on_failure`` is set only for CHAIN
    entries — a failure resolving the ORIGINAL failing model's own class
    (``llm_request.model``) is silently tolerated (it just disables the
    same-class skip below, rather than being reported as a fallback-chain
    problem)."""
    if not isinstance(name, str) or not name:
        return None
    try:
        return registry.resolve(name)
    except Exception:
        if note_on_failure:
            _note_model_fallback_skip(name)
        return None


async def _model_fallback(llm_request: Any, error: Exception) -> Any:
    """The ``on_model_error_callback`` body (decision 7 / 5b): true
    cross-model fallback. ADK's plugin hook contract (``PluginManager``):
    returning a non-``None`` ``LlmResponse`` SUBSTITUTES it — yielded to the
    agent exactly as if the (failing) model had answered itself; returning
    ``None`` lets the original error propagate unchanged. This is the one
    Python call site that can construct a request for a genuinely different
    PROVIDER (``google.adk.models.registry.LLMRegistry`` resolves a model
    name to whichever provider-specific class owns it and builds a fresh
    request from ``llm_request``) — the transport seam (httpx_pack) can only
    rewrite the model name on the SAME host/endpoint the failing call already
    targeted, so it defers same-provider hops to itself and leaves
    cross-provider hops to this hook (same-class skip, below).

    No-chase guard: reuses ``_llm_policy.should_fallback`` — never chases a
    breaker-open/budget-exhausted failure (KEEL-E012), exactly like the
    transport seam. Transport-seam-thrown exceptions carry a ``keel_outcome``
    attribute (``adapters._http.attach_outcome``); an error WITHOUT one (e.g.
    a failure raised before Keel's transport seam ever saw it — a
    request-construction error inside ADK/the SDK itself) has no ``code`` to
    disqualify it, so it is treated as chaseable: fed to ``should_fallback``
    as ``{"code": None}``, which is truthy-and-not-E012 (deliberate; verified
    against ``_llm_policy.should_fallback``'s ``not error`` / ``code not in
    _NO_FALLBACK_CODES`` shape — an EMPTY dict would read as "no error" and
    wrongly block the chase, so the sentinel dict is a real dict with an
    absent code, not `{}`). Note: `callback_context` (a `ReadonlyContext`)
    exposes no model accessor either — the other half of the same-class-skip
    rationale above: neither it nor `llm_request` hands back a model
    INSTANCE to `type()` directly, which is why `failing_cls` below is
    resolved from `llm_request.model`'s plain name string via the same
    `LLMRegistry.resolve` path used for chain entries, rather than read off
    either context object."""
    chain = _model_fallback_chain()
    if not chain:
        return None
    outcome = getattr(error, "keel_outcome", None)
    error_dict = outcome.get("error") if isinstance(outcome, dict) else None
    if not _llm_policy.should_fallback(error_dict if error_dict is not None else {"code": None}):
        return None  # KEEL-E012 (breaker/budget): never chase — same rule as the transport seam

    from google.adk.models.registry import LLMRegistry  # function-local: adapter-pack rule 1

    failing_cls = _resolve_model_class(LLMRegistry, getattr(llm_request, "model", None))

    for entry in chain:
        entry_cls = _resolve_model_class(LLMRegistry, entry, note_on_failure=True)
        if entry_cls is None:
            continue  # unknown model / missing package: already noted
        if failing_cls is not None and entry_cls is failing_cls:
            continue  # same provider class: the transport seam already chased this hop
        try:
            model = LLMRegistry.new_llm(entry)
        except Exception:
            _note_model_fallback_skip(entry)
            continue
        response = None
        try:
            async for resp in model.generate_content_async(llm_request, stream=False):
                response = resp  # keep only the FINAL response of this hop's stream
        except Exception:
            continue  # this hop failed too: try the next chain entry
        if response is not None:
            return response
    return None  # every chain entry skipped/failed/exhausted: the original error propagates


_noted_model_fallback_skips: set[str] = set()


def _note_model_fallback_skip(name: str) -> None:
    """Note (once per model name, ``KEEL_QUIET``-aware — mirrors
    ``_note_skip``/``_note_fallback``) a fallback-chain entry
    ``LLMRegistry`` could not resolve or construct (an unrecognized model
    name, or its provider's package is not installed) — the hop is skipped,
    not fatal; the next chain entry is tried."""
    if name in _noted_model_fallback_skips:
        return
    _noted_model_fallback_skips.add(name)
    if os.environ.get("KEEL_QUIET", "").strip().lower() in _TRUTHY:
        return
    sys.stderr.write(
        f"keel ▸ adk: model fallback entry {name!r} could not be resolved via "
        "LLMRegistry (unknown model name, or its provider package is not "
        "installed) — skipped; the next chain entry is tried\n"
    )


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

    Callers MUST hold ``_rebind_lock`` (``_on_before_tool`` does): this
    function is fully synchronous (no ``await`` inside it) but captures
    ``tool.run_async`` as ``original`` before shadowing it, so two unlocked
    callers on two different threads can each capture the OTHER's
    already-installed wrapper as ``original`` — the cross-thread
    double-wrap the lock exists to prevent.

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
