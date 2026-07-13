"""Tier 2 durable-flow designation for `keel run` (dx-spec §1 Level 2,
architecture-spec §4.3–4.4).

When `keel run <script>` targets a module named by a `[flows] entrypoints`
`py:<module>:<function>`, the front end runs that function *as a durable flow*:
it opens (or resumes) the flow through the native backend, so every intercepted
call inside is journaled and — on a rerun after a crash — already-completed
steps are substituted from the journal instead of re-fired. Time and random
reads are virtualized inside the flow scope only, and restored on exit.

Tier 2 requires the native core AND an attached journal: the pure-Python stub
cannot journal/replay, and a native core with no journal has nothing to resume
from — either case is a precise, actionable error (never a silent Tier-1
downgrade — a Level 0 surprise is a P0). Both gates are checked *here*, before
`enter_flow`, so the backend's last-resort KEEL-E040 ("pass a journal_path") is
unreachable from `keel run`. The policy itself is valid — what is missing is a
capability of this build/configuration — so the front-end error is KEEL-E005
(unsupported-configuration), not the validation code E001.

An `async def` flow body runs on its own event loop (`asyncio.run`); its
intercepted async effects route through the SAME open `FlowHandle` as a
synchronous flow's calls (`keel_core`'s `execute_async` async flow bridge), so
they are journaled and replayed identically. Concurrent awaited effects inside
one flow (`asyncio.gather`) are admitted — and therefore journaled — in the
order their calls *reach* the handle, never in completion order (normative:
conformance/README.md "Async steps inside a flow"). Keep fan-out order
deterministic (await sequentially, or fan out in a fixed, data-independent
order) so a resume's dispatch order matches the first run's.
"""

from __future__ import annotations

import asyncio
import hashlib
import importlib
import os
import re
import struct
import sys
from contextlib import contextmanager
from pathlib import Path
from typing import Any, Iterator, Mapping, Sequence

from . import _runtime
from ._errors import KeelError, is_keel_error
from ._policy import FlowEntrypoint

#: Front-end value-step keys (the module-docs convention shared with the golden
#: fixtures and keel-core's replay matcher). Niladic reads use a `-` args hash.
_TIME_KEY = "py:time.time#-"
_TIME_NS_KEY = "py:time.time_ns#-"
_RANDOM_KEY = "py:random.random#-"


def match_flow(target: str, entrypoints: Sequence[FlowEntrypoint]) -> FlowEntrypoint | None:
    """The flow entrypoint whose module PATH matches the `target` script, if any.

    Identity is anchored to the file the module imports from: a single-component
    module (`pipeline`) matches any `…/pipeline.py`, and a dotted module
    (`jobs.pipeline`) matches ONLY `…/jobs/pipeline.py`. Matching a bare file stem
    (the old rule) let a different script that merely shares a name — e.g. a
    scratch `pipeline.py` in another directory — enter and resume the production
    flow's journal (flow identity never includes which file ran), replaying
    foreign step outcomes into foreign code. Requiring the module path to match
    the file's path suffix closes that.

    Glob designation (docs/targeting.md): an entry whose module contains `*`
    (`py:pipeline.*:main`) matches when any dotted-module reading of the target
    path — built from the file's stem outward while path components are valid
    identifiers, shortest first — matches the glob (`*` crosses dots). CONCRETE
    entries always win over globs, then declaration order; the returned entry is
    resolved to the matched concrete module (raw = `py:<module>:<function>`) so
    two different scripts under one glob NEVER share a flow identity, with `via`
    recording the designating pattern for messages."""
    if not entrypoints:
        return None
    tparts = Path(target).parts
    for entry in entrypoints:
        if "*" in entry.module:
            continue  # concrete designations take precedence over globs
        mod = entry.module.split(".")
        want = tuple(mod[:-1]) + (mod[-1] + ".py",)
        if len(want) <= len(tparts) and tparts[-len(want):] == want:
            return entry
    candidates = _module_candidates(tparts)
    if not candidates:
        return None
    for entry in entrypoints:
        if "*" not in entry.module:
            continue
        rx = _glob_regex(entry.module)
        for module in candidates:  # shortest (least-assuming import path) first
            if rx.match(module):
                return FlowEntrypoint(
                    raw=f"py:{module}:{entry.function}",
                    module=module,
                    function=entry.function,
                    via=entry.raw,
                )
    return None


def _glob_regex(glob: str) -> re.Pattern[str]:
    """`*`-only glob → anchored regex (`*` matches any run of characters, dots
    included; everything else literal). The same rule as outbound target
    patterns (`_targets._glob_regex`) — one glob dialect everywhere."""
    return re.compile("^" + ".*".join(re.escape(p) for p in glob.split("*")) + "$")


def _module_candidates(tparts: Sequence[str]) -> list[str]:
    """Dotted-module readings of a script path, shortest first: for
    `demo/pipeline/ingest.py` → `ingest`, `pipeline.ingest`,
    `demo.pipeline.ingest`. Built from the stem outward, stopping at the first
    path component that is not a valid identifier (it could never be an
    importable package, so a glob match on it would designate an unrunnable
    module)."""
    if not tparts or not tparts[-1].endswith(".py"):
        return []
    stem = tparts[-1][: -len(".py")]
    if not stem.isidentifier():
        return []
    dotted = stem
    out = [dotted]
    for comp in reversed(tparts[:-1]):
        if not comp.isidentifier():
            break
        dotted = f"{comp}.{dotted}"
        out.append(dotted)
    return out


def backend_supports_flows(backend: Any) -> bool:
    """Whether `backend` exposes the Tier 2 flow surface (native only)."""
    return callable(getattr(backend, "enter_flow", None)) and callable(
        getattr(backend, "exit_flow", None)
    )


def _args_hash(args: Sequence[str]) -> str:
    """A stable hash of the flow's CLI arguments — part of its identity, so a
    rerun with the same args resumes the same flow."""
    return hashlib.sha256(repr(list(args)).encode("utf-8")).hexdigest()[:16]


def _code_hash(target: str) -> str | None:
    """A hash of the flow script's source, fencing replay across code changes
    (a changed deploy is expected to diverge; §4.4). None if unreadable."""
    try:
        data = Path(target).read_bytes()
    except OSError:
        return None
    return hashlib.sha256(data).hexdigest()[:16]


@contextmanager
def virtualize_time_random(backend: Any) -> Iterator[None]:
    """Patch `time.time`/`time.time_ns`/`random.random` to journal-backed values
    for the duration of a flow, then restore the originals. On replay the backend
    substitutes the recorded value, so a resumed flow observes the same clock and
    randomness it did on its first run.

    The backend decides what actually becomes a value step: on the native core a
    read that happens *inside* an intercepted effect passes through to the live
    value (it is NOT journaled — only the flow's top-level reads between steps are
    recorded), which also avoids re-locking the active-flow mutex mid-effect. The
    pure-Python stub has no such reentrancy and still journals in-effect reads —
    a known stub/native divergence for flows that read the clock inside an effect."""
    import random as _random
    import time as _time

    orig_time, orig_time_ns, orig_random = _time.time, _time.time_ns, _random.random

    def v_time() -> float:
        # Journal integer seconds (the fixtures' shape); return seconds as float.
        return float(backend.journal_time(_TIME_KEY, int(orig_time())))

    def v_time_ns() -> int:
        return int(backend.journal_time(_TIME_NS_KEY, orig_time_ns()))

    def v_random() -> float:
        drawn = struct.pack("<d", orig_random())
        recorded = backend.journal_random(_RANDOM_KEY, drawn)
        return struct.unpack("<d", recorded)[0]

    _time.time, _time.time_ns, _random.random = v_time, v_time_ns, v_random
    try:
        yield
    finally:
        _time.time, _time.time_ns, _random.random = orig_time, orig_time_ns, orig_random


def _import_flow_function(target: str, entry: FlowEntrypoint) -> Any:
    """Import the flow's module (NOT as `__main__`, so its `if __name__ ==
    '__main__'` guard does not double-run the body) and return its function."""
    module = importlib.import_module(entry.module)
    func = getattr(module, entry.function, None)
    if not callable(func):
        designated = f" (designated by [flows] glob {entry.via!r})" if entry.via else ""
        next_step = (
            f"; add a {entry.function}() to the module or narrow the glob"
            if entry.via
            else ""
        )
        raise KeelError(
            "KEEL-E040",
            f"flow entrypoint {entry.raw!r}{designated} names {entry.function!r}, "
            f"which is not a callable in module {entry.module!r}{next_step}",
        )
    return func


def _unsupported_on_stub(entry: FlowEntrypoint) -> None:
    """Emit the precise what/why/next error (KEEL-E005) for a flow under a
    non-native backend and exit 1 (Tier 2 requires the native core). The policy
    is valid; the capability is missing — unsupported-configuration, not E001."""
    sys.stderr.write(
        f"keel ▸ KEEL-E005: Tier 2 durable flow {entry.raw!r} needs the native core.\n"
        "  why:  crash-safe resume journals and replays each step; the pure-Python "
        "stub backend cannot do that.\n"
        "  next: build the native module (`maturin develop` in crates/keel-py) or set "
        "KEEL_BACKEND=native, then re-run.\n"
    )
    raise SystemExit(1)


def backend_has_journal(backend: Any) -> bool:
    """Whether `backend` has a journal attached (the native `persistent` flag).
    Tier 2 replay lives in that journal; a native core with none cannot resume."""
    return bool(getattr(backend, "persistent", False))


def _unsupported_without_journal(entry: FlowEntrypoint) -> None:
    """Emit the precise config-level error (KEEL-E005, unsupported-configuration)
    for a native backend with no journal, and exit 1. Checked *before*
    `enter_flow`, so the backend's last-resort KEEL-E040 ("pass a journal_path")
    is never reached from `keel run` — the front end owns this diagnosis at the
    correct (policy) altitude."""
    sys.stderr.write(
        f"keel ▸ KEEL-E005: durable flow {entry.raw!r} needs a journal, but none is attached.\n"
        "  why:  Tier 2 journals and replays each step; with no journal there is nothing "
        "to record to or resume from.\n"
        "  next: let the native core open .keel/journal.db (check KEEL_JOURNAL and directory "
        "permissions), or remove this entrypoint from [flows].\n"
    )
    raise SystemExit(1)


def run_as_flow(
    target: str,
    entry: FlowEntrypoint,
    backend: Any,
    args: Sequence[str],
    *,
    env: Mapping[str, str] | None = None,
) -> None:
    """Run `entry`'s function as a durable flow through `backend`. Opens/resumes
    the flow, runs the body with time/random virtualized, and stamps the terminal
    status on exit.

    Terminal status is chosen carefully so a rerun never bricks a working script:
      * a clean ``SystemExit`` (code 0/None) — the ordinary ``main()`` success
        exit ``_run.py`` passes through — completes the flow (not `failed`);
      * a real exception on a fresh (non-replayed) run marks it `failed` and
        propagates unchanged (DX invariant 5);
      * an already-COMPLETED (replayed) flow is NEVER demoted to `failed` — a
        designed replay-miss (KEEL-E031) after a code change, or any error while
        re-running finished code, must not re-open a done flow for live
        re-execution (nor march it toward `dead`);
      * ``KeyboardInterrupt`` leaves the flow `running` so it can be resumed,
        rather than burning an attempt."""
    env = env if env is not None else os.environ
    if not backend_supports_flows(backend):
        _unsupported_on_stub(entry)  # exits
    if not backend_has_journal(backend):
        _unsupported_without_journal(entry)  # exits

    func = _import_flow_function(target, entry)
    kwargs: dict[str, Any] = {"code_hash": _code_hash(target)}
    lease_ms = env.get("KEEL_FLOW_LEASE_MS")
    if lease_ms:
        kwargs["lease_ms"] = int(lease_ms)

    try:
        info = backend.enter_flow(entry.raw, _args_hash(args), **kwargs)
    except BaseException as exc:  # a lease held by a live holder (E030), dead (E032)
        if is_keel_error(exc):
            code = getattr(exc, "code", "KEEL-E040")
            message = getattr(exc, "message", str(exc))
            sys.stderr.write(f"keel ▸ {code}: {message}\n")
            raise SystemExit(1) from exc
        raise

    replayed = bool(info.get("replay"))
    verb = "replaying completed" if replayed else "running"
    if env.get("KEEL_QUIET", "").strip().lower() not in {"1", "true", "yes"}:
        sys.stderr.write(f"keel ▸ {verb} flow {entry.raw} [{info.get('flow_id')}]\n")

    try:
        with virtualize_time_random(backend):
            # Flip the "a flow body is running" flag (module docs, `_runtime`)
            # for exactly the scope where `execute()` is journaled: packs that
            # persist through the flow journal (the LangGraph checkpointer)
            # read this to refuse rather than silently run un-journaled.
            _runtime.set_flow_active(True)
            try:
                if asyncio.iscoroutinefunction(func):
                    # An async flow body drives its own event loop; its
                    # intercepted calls await `execute_async`, which routes
                    # through this SAME open flow handle (see module docs).
                    asyncio.run(func())
                else:
                    func()
            finally:
                _runtime.set_flow_active(False)
    except SystemExit as exc:
        if exc.code in (None, 0):  # clean exit == success (common main() shape)
            backend.exit_flow("completed")
        elif not replayed:
            backend.exit_flow("failed")
        raise
    except KeyboardInterrupt:
        raise  # leave the flow 'running' for resume; don't stamp 'failed'
    except BaseException:
        if not replayed:  # never demote an already-completed (replayed) flow
            backend.exit_flow("failed")
        raise
    backend.exit_flow("completed")
