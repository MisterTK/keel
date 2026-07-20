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
Separately, and OFF by default, ``KEEL_MCP_CLASSIFY_ISERROR`` (issue #16)
opts into classifying the raw MCP tool-result convention too: a call that
executes fine at the transport level but reports its own business-logic
failure comes back as an ordinary-looking ``isError: true`` dict, which ADK
itself treats as a success (``_detect_error_in_response`` only logs it). Set
truthy, Keel counts that shape as a FAILURE the same way, still returning it
to the agent unchanged; unset, it passes through exactly as before this
opt-in existed.
"""

from __future__ import annotations

import base64
import functools
import hashlib
import importlib.metadata
import os
import sys
import threading
import time
import uuid
import weakref
from typing import Any, Callable

from .. import _runtime
from ..adapters import _http, _llm_policy
from ..adapters._pack import Detection, Seam, TargetDecl
from .._errors import KeelError
from .._flow import backend_has_journal, backend_supports_flows, exit_flow_or_warn
from .._wrap import ENVELOPE_VERSION, _json_safe
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

#: `KeelSessionService` step targets (design doc issue #15 §3.1). All three
#: share the `tool:` namespace — same reasoning as `langgraph_pack`'s
#: `CHECKPOINT_*_TARGET` constants: the frozen `targetKey` grammar
#: (`contracts/policy.schema.json`) admits no other prefix for a
#: framework-pack-owned call boundary, and `adk_pack`'s own code constructs
#: and passes these strings directly, so there is no external-grammar
#: matching concern the way chunk-6/#27's `cmd:` case had (design §5's CCR
#: table).
SESSION_EVENT_TARGET = "tool:adk.session_event"
SESSION_IDENTITY_TARGET = "tool:adk.session_identity"
SESSION_DELETE_TARGET = "tool:adk.session_delete"

#: The (app_name, user_id, session_id) identity of the CURRENTLY open
#: Runner-flow, or None when no flow is open. Set (and the counter below
#: reset) exactly once per flow entry, by `_run_async_flow_wrapper` itself —
#: see that function's own comment for why this can't piggyback on an
#: existing call site. `KeelSessionService`'s write-path honesty gate
#: (design §6 item 4) reads this to distinguish "this IS the active flow"
#: from "a DIFFERENT flow already holds the singleton slot", which a bare
#: `_runtime.in_active_flow()` check cannot do (only one flow is ever open
#: at a time, so the busy case reports `True` too).
_active_session_identity: tuple[str, str, str] | None = None
#: Per-flow `tool:adk.session_event` sequence counter (design §3.1's
#: `"<session_id>:<seq>"` step-key convention — mirrors `langgraph_pack`'s
#: `CHECKPOINT_PUT_TARGET` seq). Reset to 0 at the SAME call site that sets
#: `_active_session_identity` above — every flow entry, unconditionally. A
#: single shared counter (not a dict keyed by flow_id) is sufficient because
#: only one flow is ever open per process at a time.
_session_event_seq = 0

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
        # Write `tool:adk.session_identity` exactly ONCE per flow, HERE —
        # before `gen = inner()` is ever driven (design doc issue #15 §3.1's
        # write-path sequencing-bug fix). A first draft of that design placed
        # this write at the SAME call site as the `journal_random(
        # "adk:invocation_id", ...)` correlation call below — but that call
        # site sits INSIDE the `async for event in gen:` loop, gated on
        # `if not correlated:`, so it only fires AFTER `inner()` (the real
        # `Runner.run_async`) has already produced its FIRST event. Per §6
        # item 1's confirmed assumption (verified against real google-adk
        # 2.4.0: `runners.py:795-799`/`:1410-1416`), `Runner.run_async`
        # itself calls `session_service.append_event(...)` BEFORE yielding
        # each event — so by the time this wrapper regained control at that
        # later site, `KeelSessionService.append_event` would already have
        # journaled the flow's FIRST `tool:adk.session_event` step. Reusing
        # that site would put `session_identity` AFTER the first
        # `session_event` step in journal order, breaking §3.2's read
        # algorithm (which assumes `session_identity` is always a flow's
        # FIRST `adk.session_*` step). So this is a genuinely new, EARLIER
        # call site — not a piggyback on an existing line.
        #
        # `self.app_name`: confirmed directly against the installed
        # `google-adk==2.4.0` package (`runners.py:156,223`) — `Runner.
        # __init__` always sets `self.app_name = app_name or app.name`, so
        # every constructed Runner (whichever of agent=/node=/app= built it)
        # carries a plain string `app_name` attribute; `run_async` itself
        # takes no `app_name` parameter, so this is the only "stable value"
        # (design §3.4) available at this call site.
        #
        # Also resets the per-flow `tool:adk.session_event` sequence counter
        # and the "which session does the open flow belong to" identity that
        # `KeelSessionService`'s write-path honesty gate (design §6 item 4)
        # checks. Only ONE Keel Tier 2 flow can be open per process at a time
        # (the existing `_runtime.in_active_flow()` singleton this whole
        # module already depends on), so a single shared module-level
        # counter + identity — reset unconditionally on every flow entry —
        # is sufficient; no per-flow-id dict is needed (simpler than the
        # dict the design doc's prose suggested).
        global _active_session_identity, _session_event_seq
        _active_session_identity = (self.app_name, user_id, session_id)
        _session_event_seq = 0
        # `execute` presence check (not just `backend_supports_flows`'s own
        # enter_flow/exit_flow check above): every REAL backend (native or
        # the pure-Python stub) always carries the full configure/execute/
        # report surface together (`_backend.py` module docs) — this is
        # purely tolerance for a minimal test double that only fakes
        # enter_flow/exit_flow/journal_random to exercise flow bookkeeping
        # in isolation, without a KeelSessionService in the picture at all.
        if callable(getattr(backend, "execute", None)):
            _record_session_step(
                backend,
                SESSION_IDENTITY_TARGET,
                f"adk session_identity app={self.app_name} session={session_id}",
                "-",
                {"app_name": self.app_name, "user_id": user_id, "session_id": session_id},
            )
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
            _active_session_identity = None
            raise
        except BaseException:
            if not replayed:  # never demote an already-completed (replayed) flow
                exit_flow_or_warn(backend, "failed")
            _runtime.set_flow_active(False)
            _active_session_identity = None
            raise
        else:
            exit_flow_or_warn(backend, "completed")
            _runtime.set_flow_active(False)
            _active_session_identity = None
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

    original_model = getattr(llm_request, "model", None)
    failing_cls = _resolve_model_class(LLMRegistry, original_model)

    for index, entry in enumerate(chain, start=1):
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
        except Exception as hop_error:
            _note_model_fallback_hop_failed(entry, index, len(chain), original_model, hop_error)
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


_noted_model_fallback_hop_failures: set[str] = set()


def _note_model_fallback_hop_failed(
    entry: str, index: int, total: int, original_model: Any, error: Exception
) -> None:
    """Note (once per entry name, ``KEEL_QUIET``-aware — mirrors
    ``_note_model_fallback_skip``) a fallback-chain entry that resolved AND
    constructed fine but whose ``generate_content_async`` call itself raised
    (issue #19): distinct from ``_note_model_fallback_skip`` — that note
    covers a hop that never got a chance to run (unresolvable name /
    ``new_llm`` construction failure); this one covers a hop that ran and
    failed at the actual generate call, which was previously silent. Also
    carries the hop's position (``index`` of ``total``) and the ORIGINAL
    failing model name so a fallback provider's own ``llm:<provider>``
    transport-seam traffic (which the transport seam logs/counts under its
    own target) can be correlated back to "this was ADK fallback hop N of M,
    replacing originally-failing model X" from this note's text alone — a
    deliberately shallow fix; deeper transport-seam/journal-schema
    correlation would touch the frozen ``contracts/`` surface and is out of
    scope for this fast-follow."""
    if entry in _noted_model_fallback_hop_failures:
        return
    _noted_model_fallback_hop_failures.add(entry)
    if os.environ.get("KEEL_QUIET", "").strip().lower() in _TRUTHY:
        return
    sys.stderr.write(
        f"keel ▸ adk: model fallback hop {index}/{total} ({entry!r}, "
        f"replacing originally-failing model {original_model!r}) failed "
        f"calling generate_content_async ({error!r}) — skipped; the next "
        "chain entry is tried\n"
    )


class _McpErrorDict(Exception):
    """Internal sentinel: an ADK graceful-error dict, raised inside the
    wrapped effect so the core records a failure, then caught at the rebound
    wrapper and unwrapped — the agent-visible value never changes. Covers two
    distinct shapes (see ``_rebind_tool``'s docstring): the ``{"error": ...}``
    transport-failure shape (``_is_mcp_error_dict``, unconditional), and,
    opt-in only via ``KEEL_MCP_CLASSIFY_ISERROR``, the raw MCP
    ``isError: true`` business-logic shape (``_is_mcp_business_error_dict``).
    Either way the payload is the exact, unmodified dict the underlying call
    produced."""

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


#: The full field set of MCP's `CallToolResult` — verified directly against
#: the real `mcp` package (1.28.1, the same version pinned by the adapter
#: farm and co-installed for this check, `types.py` line 1363):
#: `class CallToolResult(Result): content: list[ContentBlock]` (required),
#: `structuredContent: dict[str, Any] | None = None`, `isError: bool = False`
#: — plus `meta: dict[str, Any] | None = None` (declared `alias="_meta"`,
#: but `model_dump()` — with or without `by_alias=True` — emits the key as
#: `"meta"`, confirmed by constructing a real `CallToolResult` and dumping
#: it) inherited from `Result`. Used by `_is_mcp_business_error_dict` to
#: reject a dict carrying any key outside this set.
_MCP_RESULT_KEYS = {"content", "isError", "structuredContent", "meta"}


def _is_mcp_business_error_dict(result: Any) -> bool:
    """Mirror of the RAW MCP tool-result error convention — `isError: true`
    — a DIFFERENT shape than `_is_mcp_error_dict` above (see its docstring's
    closing note). Verified directly against `mcp_tool.py`'s
    `_detect_error_in_response` (lines 472-476, issue #16 verification, real
    `google-adk` 2.4.0 + `mcp` 1.28.1 in a throwaway venv): ADK's OWN
    detector for this shape is `isinstance(response, dict) and
    response.get("isError")` — but it is wired only into a telemetry hook
    that logs and returns, never altering what `run_async` hands back (per
    `_is_mcp_error_dict`'s note). The dict actually returned on this path
    (`_run_async_impl`, line 455: `response.model_dump(exclude_none=True,
    mode="json")` of a real `CallToolResult`, then handed back unmodified by
    `run_async`'s success path — confirmed by reading both call chains) is
    NOT the same shape a naive `.get("isError")` truthiness check would
    suggest: `isError` defaults to `False`, not `None`, so
    `exclude_none=True` NEVER drops it — a genuinely successful call dumps
    `{"content": [...], "isError": False}` (confirmed by constructing a real
    `CallToolResult` and calling `model_dump` directly), meaning the key is
    present either way and only its VALUE distinguishes success from
    failure. So this checks `result.get("isError") is True` — the literal
    `bool`, matching the field's declared type exactly, the same precision
    `_is_mcp_error_dict` applies to its own `str` check — plus `content`
    (the one field `CallToolResult` always carries, success or failure)
    present as a `list`, plus every key in `result` drawn from
    `_MCP_RESULT_KEYS`. Deliberately strict, identical philosophy to
    `_is_mcp_error_dict`: a non-MCP-shaped dict is a tool RESULT and must
    never be reclassified — this is the ONE shape a dict must have to be
    read as a business-logic failure, not merely "has a truthy isError key".
    Opt-in only (`KEEL_MCP_CLASSIFY_ISERROR`, see `_rebind_tool`): ADK itself
    treats this shape as an ordinary successful call, so matching it changes
    discovery/breaker accounting for callers who never asked for that — it
    stays off unless explicitly enabled."""
    return (
        isinstance(result, dict)
        and result.get("isError") is True
        and isinstance(result.get("content"), list)
        and set(result) <= _MCP_RESULT_KEYS
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
    the agent sees byte-identical output either way. This path is
    unconditional — it fires regardless of ``KEEL_MCP_CLASSIFY_ISERROR``
    below.

    Opt-in MCP business-error classification (``KEEL_MCP_CLASSIFY_ISERROR``,
    issue #16): the transport shape above covers only calls where the MCP
    session itself failed. A call that executes fine at the transport level
    but reports its OWN business-logic failure returns an ordinary-looking
    ``isError: true`` `CallToolResult` dict (see
    ``_is_mcp_business_error_dict``) — today invisible to Keel, since it
    comes back from ``McpTool.run_async`` looking like any other successful
    result, so the breaker never trips on repeated business-logic errors.
    Because ADK itself treats this shape as a normal successful call,
    reclassifying it changes discovery/breaker accounting for every caller,
    so it is gated behind an env var rather than unconditional (the same
    ``_TRUTHY`` set ``KEEL_QUIET``/``KEEL_FLOW_LEASE_MS`` read; a new
    ``keel.toml`` policy key would need a Contract Change Request, per
    ``contracts/policy.schema.json``'s ``additionalProperties: false`` —
    explicitly out of scope for this fast-follow). Default OFF: unset (or
    not truthy), behavior is byte-identical to before this opt-in existed.
    Set truthy AND ``_is_mcp_business_error_dict`` matches, ``_invoke``
    raises the SAME ``_McpErrorDict`` sentinel as the transport path —
    identical unwrap-on-the-way-out behavior, so the agent sees
    byte-identical output regardless of whether Keel classified the call as
    a failure."""
    original = tool.run_async  # the bound method, captured pre-shadow
    is_mcp = _is_mcp_tool(tool)

    async def _invoke(*, args: dict[str, Any], tool_context: Any) -> Any:
        result = await original(args=args, tool_context=tool_context)
        if is_mcp and (
            _is_mcp_error_dict(result)
            or (
                os.environ.get("KEEL_MCP_CLASSIFY_ISERROR", "").strip().lower() in _TRUTHY
                and _is_mcp_business_error_dict(result)
            )
        ):
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


# --- KeelSessionService: a Keel-journal-backed BaseSessionService -----------
#
# Design doc: issue #15 §3.1 (write path), §3.2/§3.2a/§3.2b (read path, its
# same-flow crash/resume story, and its stated scan-cost limit), §3.4 (public
# API surface), §4 (no physical compaction, ever — trimming is read-time
# only), §6 (open questions this implementation resolves or explicitly
# defers). Mirrors `KeelSaver` (this module's LangGraph analog, above) in
# spirit — same write-through-the-open-flow mechanism, same "refuse loudly
# outside a flow" philosophy for durable writes — but is NOT a copy:
# `KeelSaver`'s whole instance lifetime is scoped to ONE open Tier 2 flow (a
# LangGraph invocation IS the flow), so its reads only ever need "this flow's
# own replayed/live puts". A `KeelSessionService` instance is long-lived
# across MANY flows (one per `Runner.run_async` turn — design §0), so a
# `get_session()` call routinely lands on a LATER, already-closed flow, or
# even a fresh process after a restart — this pack's read path is therefore a
# genuine in-process journal reader (§3.2), built through the journal's
# ALREADY-OPEN connection (never a second same-process `sqlite3` handle —
# issue #14's exact bug, which on the read side would be worse: a torn WAL
# read doesn't throw, it can return a stale/partial row) via two new
# read-only `Journal` methods, `flows_by_entrypoint`/`steps_for_flow`
# (`crates/keel-journal`, bound on `keel-py`'s `KeelCore`).


def _can_read_journal(backend: Any) -> bool:
    """Whether `backend` exposes the two new read-only `Journal` methods
    (design §3.2) — native-core-only, and Python-only in v1 (design §5/§7:
    `keel-node` is not touched). A `False` result degrades the read path to
    cache-only rather than raising `AttributeError` — the same
    never-crash-on-a-missing-capability discipline `backend_supports_flows`/
    `backend_has_journal` already apply on the write side."""
    return (
        backend is not None
        and callable(getattr(backend, "flows_by_entrypoint", None))
        and callable(getattr(backend, "steps_for_flow", None))
    )


def _record_session_step(backend: Any, target: str, op: str, args_hash: str, payload: dict[str, Any]) -> None:
    """Journal one `KeelSessionService` step through the CURRENTLY open Keel
    Tier 2 flow. Mirrors `langgraph_pack._record_step` exactly: the payload
    is already fully known (ADK handed us a complete event/identity/delete to
    persist), so the effect never fails and its outcome is never read back —
    durability + `keel trace` visibility are the only reasons to journal."""
    request = {
        "v": ENVELOPE_VERSION,
        "target": target,
        "op": op,
        "idempotent": False,
        "args_hash": args_hash,
    }
    backend.execute(request, lambda _attempt: {"status": "ok", "payload": payload})


def _copy_session_light(session: Any) -> Any:
    """A shallow copy whose container fields (`events`, `state`) are ALSO
    shallow-copied — mirrors `InMemorySessionService`'s own `_light_copy`
    (verified against the real 2.4.0 package) so mutating the copy (e.g.
    `GetSessionConfig` trimming, below) never mutates the cached original."""
    copied = session.model_copy(deep=False)
    copied.events = list(session.events)
    copied.state = dict(session.state)
    return copied


def _apply_get_session_config(session: Any, config: Any) -> Any:
    """Slice a COPY of `session`'s event list per `config` (design §4: no
    physical compaction, ever — trimming is a read-time concern only).
    Verbatim port of `InMemorySessionService._get_session_impl`'s own
    trimming logic (verified against the real 2.4.0 package), including its
    exact `after_timestamp` quirk: if EVERY event's timestamp is >= the
    cutoff, the scan-back loop ends at `i == -1` and the real code leaves
    `events` UNTRIMMED rather than emptying it — reproduced here byte-for-
    byte rather than "fixed", since diverging from the real service's
    observable behavior would be a correctness regression, not an
    improvement."""
    copied = _copy_session_light(session)
    if config is None:
        return copied
    num_recent = getattr(config, "num_recent_events", None)
    if num_recent is not None:
        copied.events = [] if num_recent == 0 else copied.events[-num_recent:]
    after = getattr(config, "after_timestamp", None)
    if after:
        i = len(copied.events) - 1
        while i >= 0:
            if copied.events[i].timestamp < after:
                break
            i -= 1
        if i >= 0:
            copied.events = copied.events[i + 1 :]
    return copied


def _encode_content(content: Any) -> dict[str, Any] | None:
    """A `google.genai.types.Content` -> a JSON-safe dict (design §3.1's
    event-content-encoding scheme). Only ever called from `append_event`,
    where `content` is always a real `Content` or `None` — needs no
    `google.genai` import of its own."""
    if content is None:
        return None
    return {"role": content.role, "parts": [_encode_part(p) for p in (content.parts or [])]}


def _encode_part(part: Any) -> dict[str, Any]:
    """One `Part` -> a JSON-safe dict — three cases, in the order design
    §3.1 names them:

    * plain text (no `thought`/`thought_signature` riding along) passes
      through as `{"text": ...}`;
    * inline/binary data (images, audio, video) is base64-encoded under a
      distinct `inline_data_b64` key, mime type preserved, so the decoder
      never has to guess a binary part from its shape alone;
    * everything else — function_call/function_response, code execution,
      thinking parts (`text` + `thought`), `thought_signature` bytes, ... —
      falls back to `model_dump(exclude_none=True, mode="json")`, the EXACT
      call `VertexAiSessionService.append_event` itself uses for its
      `raw_event` persistence (real precedent, not invented here — design
      §3.1). This fallback is binary-safe too: confirmed directly against
      the real 2.4.0 package that `google.genai._common.BaseModel` sets both
      `ser_json_bytes="base64"` AND `val_json_bytes="base64"`, so a `bytes`
      field (e.g. `thought_signature`) round-trips through
      `model_dump(mode="json")` -> `Part(**that_dict)` byte-for-byte.
    """
    if part.text is not None and part.thought is None and part.thought_signature is None:
        return {"text": part.text}
    inline_data = part.inline_data
    if inline_data is not None and inline_data.data is not None:
        return {
            "inline_data_b64": base64.b64encode(inline_data.data).decode("ascii"),
            "mime_type": inline_data.mime_type,
        }
    return part.model_dump(exclude_none=True, mode="json")


def _decode_content(data: dict[str, Any] | None, content_cls: type, part_cls: type, blob_cls: type) -> Any:
    """The inverse of `_encode_content` — reconstructs a REAL `Content`
    object (not a dict) so a replayed `Session.events` list is exactly as
    usable to downstream ADK code (content builders, `is_final_response()`,
    ...) as a live one. Classes are passed in rather than imported here: only
    the caller (inside `_base_session_service_cls`, where `google.genai` is
    guaranteed importable because `google.adk` already depends on it) knows
    they are safe to import."""
    if data is None:
        return None
    return content_cls(
        role=data.get("role"),
        parts=[_decode_part(p, part_cls, blob_cls) for p in (data.get("parts") or [])],
    )


def _decode_part(data: dict[str, Any], part_cls: type, blob_cls: type) -> Any:
    if "inline_data_b64" in data:
        raw = data.get("inline_data_b64")
        return part_cls(
            inline_data=blob_cls(
                data=base64.b64decode(raw) if isinstance(raw, str) else None,
                mime_type=data.get("mime_type"),
            )
        )
    if set(data) <= {"text"}:
        return part_cls(text=data.get("text"))
    # function_call/function_response/... : `Part(**data)` round-trips a real
    # `model_dump(exclude_none=True, mode="json")` dict exactly (confirmed
    # against the real package — see `_encode_part`'s docstring).
    return part_cls(**data)


#: Cache-miss sentinel (module docs): distinguishes "this process already
#: knows this session was soft-deleted" from an ordinary cache miss, so a
#: repeated `get_session` for a deleted session doesn't re-run the §3.2b
#: full-scan fallback every time.
_DELETED = object()

_session_service_cls: type | None = None


def _base_session_service_cls() -> type:
    """Build (once) and return the `BaseSessionService` subclass. A function,
    not a module-level `class` statement, so `google.adk`/`google.genai` are
    imported only when a caller actually asks for a session service
    (adapter-pack rule 1) — mirrors `langgraph_pack._base_checkpoint_saver_cls`
    exactly."""
    global _session_service_cls
    if _session_service_cls is not None:
        return _session_service_cls
    try:
        from google.adk.errors.already_exists_error import AlreadyExistsError
        from google.adk.events.event import Event
        from google.adk.events.event_actions import EventActions
        from google.adk.sessions import BaseSessionService, Session
        from google.adk.sessions.base_session_service import ListSessionsResponse
        from google.genai.types import Blob, Content, Part
    except ImportError as exc:
        raise KeelError(
            "KEEL-E005",
            "KeelSessionService needs the `google-adk` package installed (it "
            "implements google.adk.sessions.base_session_service."
            "BaseSessionService); install google-adk or pass a different "
            "session_service to your Runner",
        ) from exc

    class _KeelSessionService(BaseSessionService):  # type: ignore[misc,valid-type]
        """`BaseSessionService` backed by the currently open Keel Tier 2
        Runner-flow (design doc issue #15).

        Write path (§3.1): `append_event` always calls the REAL base
        implementation first (state-delta application, temp-state handling,
        `session.events.append` — the exact `await super().append_event(...)`
        pattern confirmed against `VertexAiSessionService`), then — gated by
        `_write_gate` below — journals one `tool:adk.session_event` step
        through the flow `_run_async_flow_wrapper` already has open.
        `create_session` NEVER journals (§6 item 3: an empty session with no
        turn ever run against it is a deliberate, STATED gap, not a bug).
        `delete_session` mirrors `KeelSaver.delete_thread`'s soft-delete
        pattern — one step, the prior journal is never rewritten.

        Read path (§3.2/§3.2a/§3.2b): an in-process cache
        (`self._cache[(app_name, user_id, session_id)]`) serves any session
        this process has itself touched, exactly like `KeelSaver`'s `_by_ns`/
        `_by_id`. A cache MISS falls back to a real in-process journal read
        (`_scan_flows`) through the backend's ALREADY-OPEN connection — never
        a second same-process `sqlite3` handle (issue #14) — which is an
        accepted, STATED v1 cost: a cold read scans every flow ever created
        for `RUNNER_FLOW_ENTRYPOINT`, across every user/session of the whole
        app (§3.2b), not just the requesting session's own turn count.

        Scope limits stated plainly, not silently (§7): no physical
        compaction ever (§4 — `GetSessionConfig` trims the RECONSTRUCTED
        event list at read time only); `get_session` promises eventual, not
        linearizable, consistency under concurrent multi-process writers to
        the same session_id (§6 item 2); a session that is `create_session`d
        but never has a turn run against it is invisible to a different
        process/a fresh cache (§6 item 3); Python-only, no Node/TS analog
        (§5); an incoming `state` dict at `create_session` time is stored as
        plain session-scoped state — the real service's separate app-/user-
        scoped state maps (cross-session state SHARING) are a distinct
        feature the design doc never scopes, a stated v1 gap for a later
        phase to revisit if needed.
        """

        def __init__(self, *, app_name: str, **kwargs: Any) -> None:
            super().__init__(**kwargs)
            #: Bound at construction (design §3.4): a STABLE identity for
            #: `_write_gate`'s honesty check, deliberately NOT re-derived
            #: from whatever a particular in-flight call happens to pass as
            #: its own `app_name` argument (e.g. `create_session`'s caller
            #: could pass anything).
            self._app_name = app_name
            #: (app_name, user_id, session_id) -> Session | _DELETED.
            self._cache: dict[tuple[str, str, str], Any] = {}

        # -- writes: journaled through the active flow (§3.1) ---------------

        async def append_event(self, session: Any, event: Any) -> Any:
            result = await super().append_event(session=session, event=event)
            if not event.partial:
                # `event.partial` early-returns from the REAL base method
                # (no state/events mutation at all) — mirror that here too,
                # so this never journals a step for an event the live
                # session doesn't actually contain.
                backend = self._write_gate(session.user_id, session.id)
                if backend is not None:
                    global _session_event_seq
                    _session_event_seq += 1
                    seq = _session_event_seq
                    state_delta = dict(event.actions.state_delta) if event.actions else {}
                    payload = {
                        "event_id": event.id,
                        "author": event.author,
                        "invocation_id": event.invocation_id,
                        "timestamp": event.timestamp,
                        "content": _encode_content(event.content),
                        "state_delta": {k: _json_safe(v) for k, v in state_delta.items()},
                        "partial": event.partial,
                    }
                    _record_session_step(
                        backend,
                        SESSION_EVENT_TARGET,
                        f"adk session_event session={session.id} seq={seq}",
                        f"{session.id}:{seq}",
                        payload,
                    )
            self._cache[(session.app_name, session.user_id, session.id)] = session
            return result

        async def create_session(
            self,
            *,
            app_name: str,
            user_id: str,
            state: dict[str, Any] | None = None,
            session_id: str | None = None,
        ) -> Any:
            # Mirrors `InMemorySessionService._create_session_impl`'s core
            # shape (AlreadyExistsError check, session_id generation, Session
            # construction — verified against the real 2.4.0 package) but
            # NEVER journals (§6 item 3) and does not replicate the real
            # service's separate app-/user-scoped state maps (class docs).
            resolved_id = session_id.strip() if session_id and session_id.strip() else str(uuid.uuid4())
            key = (app_name, user_id, resolved_id)
            if session_id and self._cache.get(key) not in (None, _DELETED):
                raise AlreadyExistsError(f"Session with id {resolved_id} already exists.")
            new_session = Session(
                app_name=app_name,
                user_id=user_id,
                id=resolved_id,
                state=dict(state) if isinstance(state, dict) else {},
                last_update_time=time.time(),
            )
            self._cache[key] = new_session
            return _copy_session_light(new_session)

        async def delete_session(self, *, app_name: str, user_id: str, session_id: str) -> None:
            backend = self._write_gate(user_id, session_id)
            if backend is not None:
                _record_session_step(
                    backend,
                    SESSION_DELETE_TARGET,
                    f"adk session_delete session={session_id}",
                    session_id,
                    {"session_id": session_id},
                )
            self._cache[(app_name, user_id, session_id)] = _DELETED

        def _write_gate(self, user_id: str, session_id: str) -> Any | None:
            """The two-case honesty gate (design §6 item 4), shared by
            `append_event` and `delete_session` — anything that journals a
            step through the currently open Runner-flow needs the SAME
            distinction, using `_flow_entrypoint_designated()` rather than a
            bare `_runtime.in_active_flow()` check (which cannot tell "no
            flow open" apart from "a DIFFERENT flow has the singleton
            slot" — only one flow is ever open at a time, so the busy case
            reports `True` too):

            * undesignated (`_flow_entrypoint_designated() is None`) or no
              backend at all: silently degrade to plain in-memory behavior
              (return None, write nothing) — this fires on every ADK turn
              regardless of whether Keel is configured for Tier 2 at all,
              unlike `KeelSaver`, which is only ever constructed by a caller
              who already opted in;
            * designated, but no flow is open OR a DIFFERENT session's flow
              holds the process-wide singleton slot: raise KEEL-E005,
              loudly — misattributing this write to someone else's flow
              would be a correctness bug, not just a missed durability
              opportunity;
            * designated and this IS the active flow: return the backend.
            """
            if _flow_entrypoint_designated() is None:
                return None
            backend = _runtime.get_backend()
            if backend is None:
                return None
            wanted = (self._app_name, user_id, session_id)
            if not _runtime.in_active_flow() or _active_session_identity != wanted:
                raise KeelError(
                    "KEEL-E005",
                    "KeelSessionService needs an OPEN Keel Tier 2 Runner-flow "
                    "whose identity matches THIS session: writes are "
                    "journaled into the CURRENTLY RUNNING flow's steps "
                    '(design doc issue #15 §3.1, "one file, one trace '
                    'view"), and this call landed with no MATCHING flow open '
                    "— either no Runner-flow is active on this backend at "
                    "all, or a DIFFERENT session's flow holds the "
                    "process-wide singleton slot.\n"
                    "  next: call this only from inside a designated "
                    f"{RUNNER_FLOW_ENTRYPOINT!r} invocation for "
                    f"app_name={self._app_name!r}, user_id={user_id!r}, "
                    f"session_id={session_id!r} — or remove this entrypoint "
                    "from [flows] to use a different (non-durable) session "
                    "service.",
                )
            return backend

        # -- reads: cache first, then a real journal read (§3.2) -------------

        async def get_session(
            self,
            *,
            app_name: str,
            user_id: str,
            session_id: str,
            config: Any | None = None,
        ) -> Any:
            key = (app_name, user_id, session_id)
            cached = self._cache.get(key)
            if cached is not None:
                return None if cached is _DELETED else _apply_get_session_config(cached, config)
            backend = _runtime.get_backend()
            if not _can_read_journal(backend):
                return None
            matches = self._scan_flows(backend).get(key)
            if not matches:
                return None
            events, state, deleted = self._replay(matches)
            if deleted:
                self._cache[key] = _DELETED
                return None
            session = Session(
                app_name=app_name,
                user_id=user_id,
                id=session_id,
                state=state,
                events=events,
                last_update_time=events[-1].timestamp if events else 0.0,
            )
            self._cache[key] = session
            return _apply_get_session_config(session, config)

        async def list_sessions(self, *, app_name: str, user_id: str | None = None) -> Any:
            found: dict[tuple[str, str], Any] = {}
            for (a, u, s), sess in list(self._cache.items()):
                if a != app_name or sess is _DELETED:
                    continue
                if user_id is not None and u != user_id:
                    continue
                found[(u, s)] = sess
            backend = _runtime.get_backend()
            if _can_read_journal(backend):
                for (a, u, s), matches in self._scan_flows(backend).items():
                    if a != app_name or (u, s) in found:
                        continue
                    if user_id is not None and u != user_id:
                        continue
                    events, state, deleted = self._replay(matches)
                    if deleted:
                        self._cache[(a, u, s)] = _DELETED
                        continue
                    session = Session(
                        app_name=a,
                        user_id=u,
                        id=s,
                        state=state,
                        events=events,
                        last_update_time=events[-1].timestamp if events else 0.0,
                    )
                    self._cache[(a, u, s)] = session
                    found[(u, s)] = session
            sessions = []
            for sess in found.values():
                # `ListSessionsResponse`'s own contract (real 2.4.0 package,
                # `base_session_service.py`): "the events and states are not
                # set within each Session object" — mirrors
                # `InMemorySessionService._list_sessions_impl` exactly: build
                # the full session, then blank events.
                copied = _copy_session_light(sess)
                copied.events = []
                sessions.append(copied)
            return ListSessionsResponse(sessions=sessions)

        def _scan_flows(
            self, backend: Any
        ) -> dict[tuple[str, str, str], list[tuple[str, list[dict[str, Any]]]]]:
            """One full scan of `flows_by_entrypoint(RUNNER_FLOW_ENTRYPOINT)`
            (§3.2 step 1; §3.2b's accepted full-scan cost — paid ONCE per
            call here, not once per session found), grouping each flow's
            steps by the (app_name, user_id, session_id) identity its
            `SESSION_IDENTITY_TARGET` step recorded. Groups preserve
            `flows_by_entrypoint`'s own `created_at` order."""
            grouped: dict[tuple[str, str, str], list[tuple[str, list[dict[str, Any]]]]] = {}
            for flow in backend.flows_by_entrypoint(RUNNER_FLOW_ENTRYPOINT):
                steps = backend.steps_for_flow(flow["flow_id"])
                identity = self._identity_of(steps)
                if identity is None:
                    continue
                grouped.setdefault(identity, []).append((flow["flow_id"], steps))
            return grouped

        @staticmethod
        def _identity_of(steps: list[dict[str, Any]]) -> tuple[str, str, str] | None:
            prefix = SESSION_IDENTITY_TARGET + "#"
            for step in steps:
                if str(step.get("step_key") or "").startswith(prefix):
                    payload = step.get("payload")
                    if not isinstance(payload, dict):
                        return None
                    app_name, user_id, session_id = (
                        payload.get("app_name"),
                        payload.get("user_id"),
                        payload.get("session_id"),
                    )
                    if isinstance(app_name, str) and isinstance(user_id, str) and isinstance(session_id, str):
                        return (app_name, user_id, session_id)
                    return None
            return None

        def _replay(
            self, matches: list[tuple[str, list[dict[str, Any]]]]
        ) -> tuple[list[Any], dict[str, Any], bool]:
            """Replay one session's matching flows' `tool:adk.session_event`/
            `tool:adk.session_delete` steps, in the order `_scan_flows`
            already preserves (flow `created_at` order, then each flow's own
            `seq` order — i.e. the SAME order the live process wrote them
            in), reconstructing `events`/`state`/deleted-ness exactly as
            `BaseSessionService.append_event`'s own `_update_session_state`
            would (state_delta keys overwrite earlier same-key values, never
            merged/deep-merged)."""
            events: list[Any] = []
            state: dict[str, Any] = {}
            deleted = False
            event_prefix = SESSION_EVENT_TARGET + "#"
            delete_prefix = SESSION_DELETE_TARGET + "#"
            for _flow_id, steps in matches:
                for step in steps:
                    key = str(step.get("step_key") or "")
                    if key.startswith(event_prefix):
                        payload = step.get("payload")
                        if not isinstance(payload, dict):
                            continue  # undecodable payload: skip, don't crash a whole read
                        events.append(self._decode_event(payload))
                        for k, v in (payload.get("state_delta") or {}).items():
                            state[k] = v
                    elif key.startswith(delete_prefix):
                        deleted = True
            return events, state, deleted

        def _decode_event(self, payload: dict[str, Any]) -> Any:
            return Event(
                id=str(payload.get("event_id") or ""),
                author=str(payload.get("author") or ""),
                invocation_id=str(payload.get("invocation_id") or ""),
                timestamp=float(payload.get("timestamp") or 0.0),
                content=_decode_content(payload.get("content"), Content, Part, Blob),
                actions=EventActions(state_delta=dict(payload.get("state_delta") or {})),
                partial=payload.get("partial"),
            )

    _session_service_cls = _KeelSessionService
    return _session_service_cls


def KeelSessionService(*, app_name: str, **kwargs: Any) -> Any:
    """Factory returning a `BaseSessionService` instance whose writes are
    journaled as steps of the CURRENTLY OPEN Keel Tier 2 Runner-flow (design
    doc issue #15, §3.4). A callable, not a `class` statement, so
    `google.adk`/`google.genai` are imported only when this is actually
    called (adapter-pack rule 1) — used exactly like a constructor:
    ``session_service = KeelSessionService(app_name="my_app")``, then wired
    into ADK manually: ``Runner(session_service=session_service,
    app_name="my_app", ...)`` (§3.4 recommends manual wiring, matching
    `KeelSaver`'s own precedent — no auto-substitution into `Runner.__init__`
    in v1)."""
    return _base_session_service_cls()(app_name=app_name, **kwargs)


__all__ = [
    "MODULE",
    "NAME",
    "PLUGIN_NAME",
    "RUNNER_FLOW_ENTRYPOINT",
    "SESSION_EVENT_TARGET",
    "SESSION_IDENTITY_TARGET",
    "SESSION_DELETE_TARGET",
    "detect",
    "seams",
    "targets",
    "defaults",
    "install",
    "uninstall",
    "KeelSessionService",
]
