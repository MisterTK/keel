"""The stdlib ``subprocess`` adapter pack: in-process ``cmd:`` durable-flow
interception (CCR-5, chunk-8 / issue #27).

When a program's ``subprocess.run(...)`` (or ``check_output``/``call``/
``check_call`` — see "Coverage" below) is observed with an argv that matches a
declared ``[flows.match."cmd:<name>"]`` rule, Keel dispatches it as a Tier-2
durable flow instead of letting it run unwrapped. This is the runtime-native,
zero-``keel exec`` equivalent of ``crates/keel-cli/src/exec.rs``'s ``cmd:``
flow: **at-most-once dispatch per identity** and crash-safe **replay-skip** of
a completed command. It is emphatically NOT exactly-once execution *inside* the
child — Keel cannot un-send a subprocess's side effects; the guarantee is at
the dispatch boundary (a completed flow never re-runs its command).

Everything below the dispatch model is journaled/replayed by the SAME native
FFI surface the HTTP packs and ``keel run`` flows use — ``backend.enter_flow`` /
``execute`` / ``exit_flow`` — nothing more. No new PyO3 methods and no direct
``.keel/journal.db`` read (the issue #14 second-reader trap): every flow-state
interaction goes through that existing surface.

Design decisions, empirically grounded (probed against the real native
``keel_core`` before writing — see the chunk-8 task report):

* **Replay-skip is free (Open Question 1 → "it substitutes").** ``execute()``
  on an already-``Completed`` flow SUBSTITUTES the recorded step outcome
  WITHOUT re-invoking the effect (proven: a poisoned effect that would double a
  side-effect counter never fires; the ORIGINAL recorded payload is returned —
  cf. ``tests/test_flows.py::test_completed_flow_replays_without_refiring_
  effects``). So routing the real subprocess call through ``execute()`` inside
  the ``enter_flow``/``exit_flow`` bracket gives correct replay-skip for a
  successfully-completed ``cmd:`` flow with no new FFI: a re-dispatch of the
  same identity rebuilds and returns the recorded ``CompletedProcess`` (or the
  recorded returncode for ``call``) and DOES NOT re-run the command. Because
  this branch holds, the ``KeelCmdFlowReplayUnsupported`` fallback the design
  reserved for the "does NOT substitute" branch is never needed.

* **on_busy without exec.rs's wait/skip/dead-PID machinery (Open Question 2).**
  ``enter_flow`` raises ``KEEL-E030`` on a live cross-process holder and
  ``KEEL-E032`` on a dead/attempt-exhausted flow. Per ``CmdFlow.on_busy``:

    - ``fail`` → raise :class:`KeelCmdFlowBusy` immediately.
    - ``wait`` → poll ``enter_flow`` every :data:`_POLL_S` up to a **bounded**
      :data:`_MAX_WAIT_S` (a deliberate, documented divergence from exec.rs's
      UNBOUNDED wait: exec.rs is a one-shot CLI a human is watching; we run
      inside a live, possibly-production program where an unbounded block is a
      far bigger risk). A genuinely dead holder — same-process or cross-process,
      the lease TTL does not distinguish — is reclaimed once its lease lapses
      (the core's 30s default), so the poll loop reclaims it without any
      ``kill(pid,0)`` probe; slower than exec.rs's instant reclaim but correct.
      Exceeding the bound raises :class:`KeelCmdFlowBusy` (timeout message).
    - ``skip`` → do NOT fabricate a fake success (the caller expects a real
      ``CompletedProcess``). Instead run the REAL, original ``subprocess.run``
      unwrapped and return its real result, forfeiting at-most-once for that one
      call — a safe, honest reinterpretation of exec.rs's literal "exit 0
      without running" (which only makes sense in the CLI's one-shot context).

  ``KEEL-E032`` is ALWAYS a hard :class:`KeelCmdFlowDead` (never skipped).

* **Same-process at-most-once needs the in-process lock (probed).** The native
  PyO3 lease holder is ``pid-<os pid>`` (crates/keel-py/src/lib.rs), so EVERY
  backend in one process shares one holder: a same-process re-entry re-acquires
  its own lease and does NOT raise ``KEEL-E030``. Cross-process contention
  (different pids) does. Therefore :data:`_locks` (one ``threading.Lock`` per
  flow identity, held across enter→execute→exit) is **load-bearing** for
  same-process at-most-once — two threads running the same matched command
  concurrently would otherwise both get live handles and both run it. It also
  makes a same-process second caller wait cheaply on a local lock instead of
  busy-polling ``enter_flow``. Cross-process at-most-once stays the lease's job.

* **cwd-inclusive identity (TK sign-off, CCR-5).** ``args_hash =
  sha256(argv.join("\\0") + "\\0" + os.getcwd())[:16]`` — DIVERGES from
  exec.rs's argv-only formula by folding in the working directory, so the same
  argv in two directories is two flows. ``code_hash`` (fences resume across a
  changed program, not part of identity) mirrors exec.rs: ``sha256(
  resolved_program + "\\0" + argv.join("\\0"))[:16]``. ``explicit_key`` is
  unset in v1 (no ``--key`` equivalent).

Coverage (verified against the running CPython's ``Lib/subprocess.py``):

* ``subprocess.run`` — patched directly.
* ``subprocess.check_output`` — calls the module-global ``run(...)`` internally,
  so patching ``run`` covers it for free (its ``.stdout`` is served from the
  journaled payload on replay).
* ``subprocess.call`` — patched directly (returns a bare returncode int; never
  raises on nonzero).
* ``subprocess.check_call`` — calls the module-global ``call(...)`` internally,
  so patching ``call`` covers it; ``check_call``'s own ``CalledProcessError``
  raise happens OUTSIDE this seam (on the returned int), and thus fires
  identically on a replayed returncode.

Because ``check_output``/``check_call`` resolve ``run``/``call`` as module
globals at call time, a call *held through* those names is still intercepted;
only a DIRECT pre-activation reference (``from subprocess import run as r``)
escapes — the same held-reference limitation every module-function seam has.

Scope (v1, per the design, narrowed by verified fact — mirrors the design's
own ``os.system``/``asyncio`` exclusions):

* Only argv-as-a-list/tuple calls are matched. ``shell=True`` and a string
  command are never matched and always pass through untouched (they are just
  the same "unmatched → untouched" path any non-list call takes).
* A launch failure (an ``OSError`` — e.g. ``FileNotFoundError`` — at spawn, so
  the command never ran) marks the flow ``failed`` and RE-RAISES the original
  exception unchanged (dx-spec: never swallow errors). Because the
  ``execute()``-only FFI records a terminal error step that later re-dispatches
  SUBSTITUTE (exec.rs sidesteps this via raw-journal writes we cannot reach from
  Python), a re-dispatch of a launch-failed identity cannot auto-retry the
  launch in-process; it raises :class:`KeelCmdFlowFailed` loudly rather than
  fabricate a success. (v1 limit; ``keel exec`` remains the CLI workaround.)
* One flow per process: while a matched command's flow is open the core has one
  active handle, so any OTHER intercepted call in the same process during that
  window journals into it. Matched calls made while already inside a Tier-2
  flow (``_runtime.in_active_flow()``) pass through unwrapped rather than nest.

Stdlib convention (mirrors ``urllib_pack``): ``detect()`` always matches and
reports the Python runtime version (there is no pip pin for a stdlib module).
"""

from __future__ import annotations

import base64
import functools
import hashlib
import logging
import os
import platform
import shutil
import subprocess
import threading
import time
from typing import Any, Callable, NamedTuple, Sequence

from .. import _runtime
from .._flow import _glob_regex
from .._policy import CmdFlow
from ._pack import Detection, Seam, TargetDecl

MODULE = "subprocess"
NAME = "subprocess"

#: Python runtime lines this pack certifies (prefix match) — the stdlib
#: convention exception (see ``urllib_pack._PINNED``): there is no pip version
#: to pin, so the "version" is the interpreter's.
_PINNED = ("3.11", "3.12", "3.13", "3.14")

#: How long to sleep between ``enter_flow`` retries under ``on_busy = wait``
#: (exec.rs's cadence). See :data:`_MAX_WAIT_S`.
_POLL_S = 0.5

#: The BOUNDED ceiling on an ``on_busy = wait`` block — a deliberate divergence
#: from exec.rs's unbounded wait (module docs). Three lease TTLs (the core's
#: default lease is 30s), so a genuinely dead holder (reclaimed once its lease
#: lapses) is always waited through, while a stuck live holder does not hang a
#: production caller forever.
_MAX_WAIT_S = 90.0

_log = logging.getLogger("keel.adapters.subprocess")

_installed = False
_orig: dict[str, Any] = {}

#: The compiled ``cmd:`` match rules, most-specific-first. ``None`` until the
#: first call compiles them from ``_runtime.get_cmd_flows()``; ``()`` means "no
#: rule declared" (the zero-rules short-circuit). Reset on ``uninstall``.
_compiled: "tuple[_CompiledCmd, ...] | None" = None

#: One ``threading.Lock`` per flow identity, serializing same-process
#: same-identity dispatch (module docs: load-bearing for same-process
#: at-most-once, since the native lease holder is per-pid).
_locks_guard = threading.Lock()
_locks: dict[str, threading.Lock] = {}

#: Sentinel: ``enter`` decided to run the command unwrapped (``on_busy = skip``
#: hit a live cross-process holder).
_SKIP_UNWRAPPED = object()


# --- exceptions --------------------------------------------------------------


class KeelCmdFlowError(Exception):
    """Base for the ``cmd:`` in-process interception errors."""


class KeelCmdFlowBusy(KeelCmdFlowError):
    """A matched ``cmd:`` flow is held by a live cross-process holder and
    ``[flows] on_busy`` is ``fail`` (raised immediately), or is ``wait`` and the
    bounded :data:`_MAX_WAIT_S` elapsed (timeout message). Corresponds to the
    core's ``KEEL-E030``."""


class KeelCmdFlowDead(KeelCmdFlowError):
    """A matched ``cmd:`` flow is ``dead`` (its flow-level attempt cap is
    exhausted / it was poisoned): the core's ``KEEL-E032``. ALWAYS a hard
    failure — never skipped, never configurable — so a poison flow does not
    silently re-run its command."""


class KeelCmdFlowFailed(KeelCmdFlowError):
    """A re-dispatch of an identity whose only recorded step is a LAUNCH failure
    (the command never ran). The ``execute()``-only FFI substitutes that
    terminal error step rather than re-attempting the launch, so this is raised
    loudly instead of fabricating a success. Change the argv/cwd (a new
    identity) or use ``keel exec`` to re-drive it. (v1 limit — see module
    docs.)"""


# --- contract operations -----------------------------------------------------


def detect() -> Detection:
    """Always present (stdlib). The version reported is the Python runtime's —
    see the module docstring's stdlib convention note."""
    version = platform.python_version()
    confidence = "pinned" if _is_pinned(version) else "best_effort"
    return Detection(matched=True, name=NAME, version=version, confidence=confidence)


def seams() -> list[Seam]:
    return [
        Seam(
            patch_point="subprocess.run",
            upstream_api="subprocess.run(args, ...) -> CompletedProcess",
            why_stable=(
                "The module-level run() is the modern subprocess entrypoint and "
                "check_output() dispatches through it; both resolve run as a "
                "module global at call time, so wrapping run intercepts a "
                "matched argv on either path. Stdlib API, stable across the "
                "supported interpreter lines."
            ),
        ),
        Seam(
            patch_point="subprocess.call",
            upstream_api="subprocess.call(args, ...) -> int returncode",
            why_stable=(
                "check_call() dispatches through the module-level call(); "
                "wrapping call intercepts a matched argv on either path. "
                "check_call's own CalledProcessError raise is on the returned "
                "int, outside this seam, so it fires identically on replay."
            ),
        ),
    ]


def targets() -> list[TargetDecl]:
    """No outbound-shaped targets: a ``cmd:`` flow is a Tier-2 durable-flow
    entrypoint (declared in ``[flows] entrypoints`` + ``[flows.match]``), not an
    outbound host/URL target, and the frozen ``TargetDecl.kind`` has no ``cmd``
    class. Nothing to declare here."""
    return []


def defaults() -> dict[str, Any]:
    """No pack-specific policy fragment."""
    return {}


# --- install / uninstall -----------------------------------------------------


def install() -> None:
    global _installed
    if _installed:
        return
    _orig["run"] = subprocess.run
    _orig["call"] = subprocess.call
    subprocess.run = _run_wrapper(_orig["run"])  # type: ignore[assignment]
    subprocess.call = _call_wrapper(_orig["call"])  # type: ignore[assignment]
    _installed = True


def uninstall() -> None:
    global _installed, _compiled
    if not _installed:
        return
    subprocess.run = _orig["run"]  # type: ignore[assignment]
    subprocess.call = _orig["call"]  # type: ignore[assignment]
    _orig.clear()
    _compiled = None
    with _locks_guard:
        _locks.clear()
    _installed = False


# --- match-rule compilation --------------------------------------------------


class _CompiledCmd(NamedTuple):
    """One compiled ``[flows.match."cmd:<name>"]`` argv rule."""

    entrypoint: str  # the full "cmd:<name>" string (the flow entrypoint)
    on_busy: str  # "skip" | "wait" | "fail"
    patterns: tuple  # per-position anchored glob matchers (re.Pattern[str])
    wildcards: int  # specificity: total `*` count across positions (fewer wins)
    literal: int  # specificity: total non-`*` char count (more wins)


def _compile(cmd_flows: dict[str, CmdFlow]) -> "tuple[_CompiledCmd, ...]":
    """Compile the declared ``cmd:`` flows into most-specific-first match rules.

    A rule with EMPTY ``argv_patterns`` (a ``cmd:`` entrypoint with no
    ``[flows.match]`` rule) matches nothing in-process and is dropped — the
    interceptor requires an explicit argv rule to fire. Specificity tie-break
    reuses the outbound target specificity ordering (fewest ``*``, then most
    literal chars, then key lexicographic) minus the HTTP method tier — the
    same tie-break the backend's ``resolve_target`` applies to ``[target]``
    patterns."""
    out: list[_CompiledCmd] = []
    for flow in cmd_flows.values():
        if not flow.argv_patterns:
            continue
        patterns = tuple(_glob_regex(p) for p in flow.argv_patterns)
        wildcards = sum(p.count("*") for p in flow.argv_patterns)
        literal = sum(len(p) - p.count("*") for p in flow.argv_patterns)
        out.append(
            _CompiledCmd(
                entrypoint=flow.name,
                on_busy=flow.on_busy,
                patterns=patterns,
                wildcards=wildcards,
                literal=literal,
            )
        )
    out.sort(key=lambda c: (c.wildcards, -c.literal, c.entrypoint))
    return tuple(out)


def _rules() -> "tuple[_CompiledCmd, ...]":
    """The compiled rules, compiling+caching on first use from the runtime's
    parsed ``cmd:`` flows (set by ``bootstrap.install_keel``). The empty-tuple
    result IS the zero-rules short-circuit — no keel.toml re-parse per call."""
    global _compiled
    if _compiled is None:
        with _locks_guard:
            if _compiled is None:  # double-checked; compiling twice is harmless
                _compiled = _compile(_runtime.get_cmd_flows())
    return _compiled


def _match(rules: "tuple[_CompiledCmd, ...]", argv: Sequence[str]) -> "_CompiledCmd | None":
    """The most specific rule whose per-position patterns all match ``argv``
    (positional; the observed argv must be at least as long as the pattern;
    trailing observed args are unconstrained), or ``None``. ``rules`` is
    pre-sorted most-specific-first, so the first match wins."""
    for rule in rules:
        if len(argv) < len(rule.patterns):
            continue
        if all(rule.patterns[i].match(argv[i]) for i in range(len(rule.patterns))):
            return rule
    return None


# --- identity ----------------------------------------------------------------


def _str_argv(args0: Any) -> "list[str] | None":
    """Coerce a subprocess ``args`` value to a ``list[str]`` for matching, or
    ``None`` when it is not an argv-as-a-sequence (a bare string command,
    ``shell=True`` string, or a sequence with a non-string-shaped element) —
    the "unmatched → untouched" passthrough."""
    if isinstance(args0, (str, bytes, bytearray)) or not isinstance(args0, (list, tuple)):
        return None
    out: list[str] = []
    for x in args0:
        if isinstance(x, str):
            out.append(x)
        elif isinstance(x, os.PathLike):
            fx = os.fspath(x)
            out.append(fx if isinstance(fx, str) else fx.decode("utf-8", "surrogateescape"))
        elif isinstance(x, (bytes, bytearray)):
            out.append(bytes(x).decode("utf-8", "surrogateescape"))
        else:
            return None
    return out


def _args_hash(argv: Sequence[str]) -> str:
    """Identity digest over the NUL-joined argv PLUS the working directory
    (cwd-inclusive divergence from exec.rs; module docs). 16 hex chars, matching
    exec.rs's ``sha16`` width."""
    material = "\x00".join(argv) + "\x00" + os.getcwd()
    return hashlib.sha256(material.encode("utf-8", "surrogateescape")).hexdigest()[:16]


def _code_hash(argv: Sequence[str]) -> str:
    """``code_hash`` fences replay across a changed program binary (mirrors
    exec.rs): the resolved ``argv[0]`` (PATH lookup, else verbatim) plus the
    argv. Not part of identity."""
    program = shutil.which(argv[0]) or argv[0] if argv else ""
    material = program + "\x00" + "\x00".join(argv)
    return hashlib.sha256(material.encode("utf-8", "surrogateescape")).hexdigest()[:16]


def _op_string(argv: Sequence[str]) -> str:
    """A readable ``op`` for the journal/trace (display only; never part of the
    step key)."""
    joined = "cmd " + " ".join(argv)
    return joined if len(joined) <= 200 else joined[:197] + "..."


def _flow_lock(key: str) -> threading.Lock:
    with _locks_guard:
        lock = _locks.get(key)
        if lock is None:
            lock = threading.Lock()
            _locks[key] = lock
        return lock


# --- payload (de)serialization for replay-skip -------------------------------


def _encode_stream(value: Any) -> "dict[str, Any] | None":
    """A JSON-safe envelope for a captured stdout/stderr (``bytes`` → base64,
    ``str`` → verbatim, ``None`` → ``None``) so a replayed ``CompletedProcess``
    is byte-identical to the recorded one."""
    if value is None:
        return None
    if isinstance(value, (bytes, bytearray)):
        return {"t": "b", "v": base64.b64encode(bytes(value)).decode("ascii")}
    return {"t": "s", "v": str(value)}


def _decode_stream(env: Any) -> Any:
    if not isinstance(env, dict):
        return None
    if env.get("t") == "b":
        return base64.b64decode(env.get("v", ""))
    return env.get("v")


def _payload_run(
    returncode: int, argv: Sequence[str], stdout: Any, stderr: Any, check: bool
) -> dict[str, Any]:
    return {
        "kind": "run",
        "returncode": int(returncode),
        "argv": list(argv),
        "check": bool(check),
        "stdout": _encode_stream(stdout),
        "stderr": _encode_stream(stderr),
    }


def _rebuild_run(payload: dict[str, Any]) -> subprocess.CompletedProcess:
    """Rebuild a ``CompletedProcess`` from a replayed step payload, RE-RAISING a
    ``CalledProcessError`` when the recorded call was ``check=True`` and exited
    nonzero — so ``check``'s raise semantics survive replay exactly (the same
    way ``urllib_pack`` re-raises a cached ``HTTPError``)."""
    argv = payload.get("argv")
    rc = int(payload.get("returncode", 0))
    stdout = _decode_stream(payload.get("stdout"))
    stderr = _decode_stream(payload.get("stderr"))
    if payload.get("check") and rc != 0:
        raise subprocess.CalledProcessError(rc, argv, output=stdout, stderr=stderr)
    return subprocess.CompletedProcess(argv, rc, stdout, stderr)


# --- enter (on_busy) ---------------------------------------------------------


def _enter(backend: Any, entrypoint: str, args_hash: str, code_hash: str, on_busy: str) -> Any:
    """``enter_flow`` with ``on_busy`` handling. Returns the enter-info dict on a
    live/replay handle, or :data:`_SKIP_UNWRAPPED` when ``skip`` yields to a live
    cross-process holder. Raises :class:`KeelCmdFlowBusy`/:class:`KeelCmdFlowDead`
    on the terminal ``fail``/timeout/dead paths."""
    waited = 0.0
    while True:
        try:
            return backend.enter_flow(entrypoint, args_hash, code_hash=code_hash)
        except Exception as exc:  # native KeelCoreError carries `.code`
            code = getattr(exc, "code", None)
            if code == "KEEL-E030":  # busy: a live cross-process holder
                if on_busy == "fail":
                    raise KeelCmdFlowBusy(
                        f"cmd flow {entrypoint} is busy (held by a live process); "
                        "refusing (flows.on_busy = fail)."
                    ) from exc
                if on_busy == "wait":
                    if waited >= _MAX_WAIT_S:
                        raise KeelCmdFlowBusy(
                            f"cmd flow {entrypoint} still busy after {_MAX_WAIT_S:g}s "
                            "(flows.on_busy = wait; bounded to avoid hanging a live "
                            "process — a dead holder is reclaimed once its lease lapses)."
                        ) from exc
                    time.sleep(_POLL_S)
                    waited += _POLL_S
                    continue
                # skip (default): run the real command unwrapped (never fabricate).
                _log.debug(
                    "cmd flow %s busy; running unwrapped (flows.on_busy = skip, "
                    "at-most-once forfeited for this call)",
                    entrypoint,
                )
                return _SKIP_UNWRAPPED
            if code == "KEEL-E032":  # dead: hard failure, never skipped
                raise KeelCmdFlowDead(
                    f"cmd flow {entrypoint} is dead (attempt cap exhausted / poisoned); "
                    "refusing to resume (KEEL-E032). Inspect with `keel trace`."
                ) from exc
            raise


# --- the seam ----------------------------------------------------------------


def _should_dispatch(
    backend: Any, args: tuple, kwargs: dict
) -> "tuple[list[str], _CompiledCmd] | None":
    """The shared pre-flight for both seams. Returns the coerced ``argv`` and the
    matched rule to dispatch, or ``None`` to pass straight through (the zero-cost
    path for unmatched / unsupported calls)."""
    if backend is None:  # Keel disabled/uninstalled
        return None
    rules = _rules()
    if not rules:  # zero-rules short-circuit (NFR2: never on the unwrapped hot path)
        return None
    if not args or kwargs.get("shell"):  # no argv, or a shell string command
        return None
    argv = _str_argv(args[0])
    if not argv:  # not an argv-as-a-list (bare string, empty, non-string element)
        return None
    rule = _match(rules, argv)
    if rule is None:  # no rule matches this argv
        return None
    from .. import _flow

    if not _flow.backend_supports_flows(backend) or not _flow.backend_has_journal(backend):
        # A stub backend, or a native core with no journal, cannot durably
        # dispatch. Run the command unwrapped rather than break it (resilience
        # first) — cmd interception is an opt-in enhancement, not load-bearing
        # for the call itself.
        _log.debug("cmd interception inactive (no Tier-2 journal); running %s unwrapped", argv[0])
        return None
    if _runtime.in_active_flow():
        # Already inside a Tier-2 flow: one flow per process (module docs).
        _log.debug("cmd %s called inside an open flow; running unwrapped (one flow/process)", argv[0])
        return None
    return argv, rule


def _dispatch(
    argv: "list[str]",
    rule: _CompiledCmd,
    backend: Any,
    effect: Callable[[dict[str, Any]], dict[str, Any]],
    live: dict[str, Any],
    from_payload: Callable[[dict[str, Any]], Any],
) -> Any:
    """Open (or resume) the ``cmd:`` flow and drive ``effect`` through
    ``execute()`` inside the enter→exit bracket, under the same-identity local
    lock. ``effect`` fills ``live`` side-band (``result``/``exc``/``raise``) and
    returns the core outcome dict; ``from_payload`` rebuilds the caller-facing
    result from a replay-substituted step payload."""
    from .. import _flow

    entrypoint = rule.entrypoint
    args_hash = _args_hash(argv)
    code_hash = _code_hash(argv)
    with _flow_lock(f"{entrypoint}#{args_hash}"):
        handle = _enter(backend, entrypoint, args_hash, code_hash, rule.on_busy)
        if handle is _SKIP_UNWRAPPED:
            return live["run_unwrapped"]()
        request = {
            "v": 1,
            "target": entrypoint,
            "op": _op_string(argv),
            "args_hash": args_hash,
            "idempotent": False,
        }
        outcome = backend.execute(request, effect)
        if live["exc"] is not None:  # live launch failure (command never ran)
            _flow.exit_flow_or_warn(backend, "failed")
            raise live["exc"]
        if live["raise"] is not None:  # live check=True nonzero (command DID run)
            _flow.exit_flow_or_warn(backend, "completed")
            raise live["raise"]
        if live["result"] is not None:  # live run (any returncode)
            _flow.exit_flow_or_warn(backend, "completed")
            return live["result"]
        # Replay-substituted (the effect never fired):
        if isinstance(outcome, dict) and outcome.get("result") == "error":
            _flow.exit_flow_or_warn(backend, "failed")
            raise KeelCmdFlowFailed(
                f"cmd flow {entrypoint} previously failed to launch and cannot be "
                "re-dispatched in-process (its recorded failure is replay-substituted). "
                "Change the argv/cwd (a new identity) or re-drive it with `keel exec`."
            )
        payload = outcome.get("payload") if isinstance(outcome, dict) else None
        _flow.exit_flow_or_warn(backend, "completed")
        return from_payload(payload or {})


def _run_wrapper(orig: Callable[..., Any]) -> Callable[..., Any]:
    @functools.wraps(orig)
    def run(*args: Any, **kwargs: Any) -> Any:
        backend = _runtime.get_backend()
        decision = _should_dispatch(backend, args, kwargs)
        if decision is None:
            return orig(*args, **kwargs)
        argv, rule = decision
        live: dict[str, Any] = {
            "result": None,
            "exc": None,
            "raise": None,
            "run_unwrapped": lambda: orig(*args, **kwargs),
        }

        def effect(_request: dict[str, Any]) -> dict[str, Any]:
            try:
                result = orig(*args, **kwargs)
            except subprocess.CalledProcessError as cpe:  # ran, check=True nonzero
                live["raise"] = cpe
                return {
                    "status": "ok",
                    "payload": _payload_run(cpe.returncode, argv, cpe.output, cpe.stderr, True),
                }
            except OSError as spawn_err:  # did NOT run (spawn failure)
                live["exc"] = spawn_err
                return {"status": "error", "class": "other", "message": str(spawn_err)}
            live["result"] = result
            return {
                "status": "ok",
                "payload": _payload_run(
                    result.returncode, argv, result.stdout, result.stderr, bool(kwargs.get("check"))
                ),
            }

        return _dispatch(argv, rule, backend, effect, live, _rebuild_run)

    run.__keel_wrapped__ = True  # type: ignore[attr-defined]
    return run


def _call_wrapper(orig: Callable[..., Any]) -> Callable[..., Any]:
    @functools.wraps(orig)
    def call(*args: Any, **kwargs: Any) -> Any:
        backend = _runtime.get_backend()
        decision = _should_dispatch(backend, args, kwargs)
        if decision is None:
            return orig(*args, **kwargs)
        argv, rule = decision
        live: dict[str, Any] = {
            "result": None,
            "exc": None,
            "raise": None,
            "run_unwrapped": lambda: orig(*args, **kwargs),
        }

        def effect(_request: dict[str, Any]) -> dict[str, Any]:
            try:
                returncode = orig(*args, **kwargs)  # int; call never raises on nonzero
            except OSError as spawn_err:
                live["exc"] = spawn_err
                return {"status": "error", "class": "other", "message": str(spawn_err)}
            live["result"] = returncode
            return {
                "status": "ok",
                "payload": {"kind": "call", "returncode": int(returncode), "argv": list(argv)},
            }

        return _dispatch(
            argv, rule, backend, effect, live, lambda p: int(p.get("returncode", 0))
        )

    call.__keel_wrapped__ = True  # type: ignore[attr-defined]
    return call


def _is_pinned(version: str) -> bool:
    return any(version == p or version.startswith(p + ".") for p in _PINNED)


__all__ = [
    "MODULE",
    "NAME",
    "KeelCmdFlowError",
    "KeelCmdFlowBusy",
    "KeelCmdFlowDead",
    "KeelCmdFlowFailed",
    "detect",
    "seams",
    "targets",
    "defaults",
    "install",
    "uninstall",
]
