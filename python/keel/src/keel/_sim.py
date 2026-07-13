"""`keel sim`: adapter-level fault injection driven by a declarative fault
plan (`KEEL_SIM_PLAN=<path>`, set by `keel sim <plan>` on the child).

See ``docs/sim-format.md`` for the full (non-contract) plan format and the
architecture-spec §8 rationale. In short: this module wraps the ADAPTER's
``effect`` closure — the callable ``Backend.execute``/``execute_async`` calls
once per Tier 1 attempt — never ``execute``'s return value, so a scripted
failure is genuinely retried/backed-off/breaker-tripped by the real backend's
own resilience logic, exactly like a real failure would be. Every other
``Backend`` member (``enter_flow``/``exit_flow``/``journal_time``/
``journal_random``/``report``/``layer``) delegates straight through
(``__getattr__``, mirroring ``_backend._NativeBackend`` and
``_record.RecordingBackend``) — Tier 2 flow control and journal semantics are
completely untouched by fault injection.
"""

from __future__ import annotations

import asyncio
import json
import os
import signal
import sys
import time
from pathlib import Path
from typing import Any, Callable, Mapping

SIM_VERSION = 1

#: 128 + SIGKILL(9) — the exit code a POSIX shell reports for a process a
#: real `kill -9` terminated. Used as the fallback hard-exit code on a
#: platform with no SIGKILL (documented in docs/sim-format.md).
SIM_CRASH_EXIT_CODE = 137

#: Per-`kind` default HTTP status when the directive does not name one.
_DEFAULT_STATUS = {"5xx": 503, "429": 429, "http": 500}

_TRUTHY = {"1", "true", "yes"}


def _default_crash() -> None:
    """Hard-crash this process right now: no cleanup, no atexit, no exception
    unwinding — the closest in-process model of a real `kill -9` (module
    docs). A real, uncatchable `SIGKILL` sent to our own pid mirrors the
    docs' own vocabulary exactly; `os._exit` is the fallback on a platform
    with no SIGKILL (e.g. native Windows)."""
    try:
        os.kill(os.getpid(), signal.SIGKILL)
    except (AttributeError, OSError):
        pass
    os._exit(SIM_CRASH_EXIT_CODE)  # pragma: no cover - SIGKILL never returns on POSIX


class _Cursor:
    """Per-target consumed-directive counters, persisted to a JSON sidecar
    next to the plan file (`<plan path>.cursor.json`) so a `"crash"`
    directive's hard restart resumes the SAME fault sequence instead of
    replaying it from the top — mirrors how the flow journal itself survives
    a real crash. Best-effort: a read/write failure degrades to an in-memory
    (non-persisted) cursor rather than breaking the simulated program."""

    def __init__(self, path: Path) -> None:
        self._path = path
        try:
            loaded = json.loads(path.read_text())
            self._counts: dict[str, int] = loaded if isinstance(loaded, dict) else {}
        except (OSError, ValueError):
            self._counts = {}

    def next_index(self, target: str) -> int:
        return self._counts.get(target, 0)

    def bump(self, target: str) -> None:
        self._counts[target] = self.next_index(target) + 1
        try:
            self._path.write_text(json.dumps(self._counts))
            # fsync so the counter survives the crash we may be about to
            # trigger (best-effort: some filesystems/paths cannot be opened
            # read-only for fsync on every platform — never fatal).
            fd = os.open(self._path, os.O_RDONLY)
            try:
                os.fsync(fd)
            finally:
                os.close(fd)
        except OSError:
            pass


def _directive_at(directives: list[dict[str, Any]], index: int) -> dict[str, Any] | None:
    """The directive `index` (0-based, across every attempt this target has
    ever seen) selects, honoring each entry's `repeat` (default 1) — mirrors
    `tools/faultproxy`'s `Scenario` cursor. `None` once the sequence is spent
    (every further attempt passes through to the real effect live)."""
    remaining = index
    for directive in directives:
        span = max(1, int(directive.get("repeat", 1)))
        if remaining < span:
            return directive
        remaining -= span
    return None


def _resolve(directive: dict[str, Any]) -> dict[str, Any] | None | str:
    """`None` → passthrough (call the real effect). `"crash"` → the caller
    must hard-crash. Otherwise the synthetic `AttemptResult` dict to return
    without ever calling the real effect."""
    kind = directive.get("kind", "ok")
    if kind == "ok":
        return None
    if kind == "crash":
        return "crash"
    if kind == "conn":
        return {"status": "error", "class": "conn", "message": "keel sim: injected connection failure"}
    if kind == "timeout":
        return {"status": "error", "class": "timeout", "message": "keel sim: injected timeout"}
    status = int(directive.get("status", _DEFAULT_STATUS.get(kind, 500)))
    result: dict[str, Any] = {
        "status": "error",
        "class": "http",
        "http_status": status,
        "message": f"keel sim: injected HTTP {status}",
    }
    if "retry_after_ms" in directive:
        result["retry_after_ms"] = directive["retry_after_ms"]
    return result


class SimBackend:
    """Wraps ``inner`` (a ``Backend``): for every ``execute``/``execute_async``
    call, wraps the caller's ``effect`` closure so a scripted fault plan can
    inject a failure/latency/crash into one Tier 1 attempt without ``inner``
    ever seeing anything but a normal (possibly synthetic) attempt outcome —
    its own retry/backoff/breaker/cache decisions run for real over it."""

    def __init__(
        self,
        inner: Any,
        faults: Mapping[str, list[dict[str, Any]]],
        cursor: _Cursor,
        crash: Callable[[], None] = _default_crash,
    ) -> None:
        self._inner = inner
        self._faults = faults
        self._cursor = cursor
        self._crash = crash

    def configure(self, policy: dict[str, Any]) -> None:
        self._inner.configure(policy)

    def _directives_for(self, request: Any) -> list[dict[str, Any]] | None:
        target = str(request.get("target", "")) if isinstance(request, dict) else ""
        return self._faults.get(target) or None

    def execute(self, request: dict[str, Any], effect: Callable[[int], dict[str, Any]]) -> dict[str, Any]:
        directives = self._directives_for(request)
        if directives is None:
            return self._inner.execute(request, effect)
        target = str(request.get("target", ""))

        def wrapped(attempt: int) -> dict[str, Any]:
            return self._apply(target, directives, attempt, effect)

        return self._inner.execute(request, wrapped)

    def report(self) -> dict[str, Any]:
        return self._inner.report()

    def _apply(
        self,
        target: str,
        directives: list[dict[str, Any]],
        attempt: int,
        effect: Callable[[int], dict[str, Any]],
    ) -> dict[str, Any]:
        index = self._cursor.next_index(target)
        directive = _directive_at(directives, index)
        self._cursor.bump(target)
        if directive is None:
            return effect(attempt)
        delay_ms = directive.get("delay_ms")
        if delay_ms:
            time.sleep(max(0, int(delay_ms)) / 1000.0)
        outcome = _resolve(directive)
        if outcome is None:
            return effect(attempt)
        if outcome == "crash":
            self._crash()
            raise AssertionError("unreachable: crash() must not return")  # pragma: no cover
        return outcome

    async def _apply_async(
        self,
        target: str,
        directives: list[dict[str, Any]],
        attempt: int,
        effect: Callable[[int], Any],
    ) -> dict[str, Any]:
        index = self._cursor.next_index(target)
        directive = _directive_at(directives, index)
        self._cursor.bump(target)
        if directive is None:
            return await effect(attempt)
        delay_ms = directive.get("delay_ms")
        if delay_ms:
            await asyncio.sleep(max(0, int(delay_ms)) / 1000.0)
        outcome = _resolve(directive)
        if outcome is None:
            return await effect(attempt)
        if outcome == "crash":
            self._crash()
            raise AssertionError("unreachable: crash() must not return")  # pragma: no cover
        return outcome

    def __getattr__(self, name: str) -> Any:
        attr = getattr(self._inner, name)
        if name == "execute_async" and callable(attr):

            async def _wrapped(request: dict[str, Any], effect: Callable[[int], Any]) -> dict[str, Any]:
                directives = self._directives_for(request)
                if directives is None:
                    return await attr(request, effect)
                target = str(request.get("target", ""))

                async def wrapped(a: int) -> dict[str, Any]:
                    return await self._apply_async(target, directives, a, effect)

                return await attr(request, wrapped)

            return _wrapped
        return attr


def _load_faults(plan_path: str) -> dict[str, list[dict[str, Any]]]:
    try:
        data = json.loads(Path(plan_path).read_text())
    except (OSError, ValueError) as exc:
        sys.stderr.write(f"keel ▸ KEEL_SIM_PLAN={plan_path!r} could not be read: {exc}\n")
        raise SystemExit(1) from exc
    faults = data.get("faults") if isinstance(data, dict) else None
    if not isinstance(faults, dict):
        return {}
    out: dict[str, list[dict[str, Any]]] = {}
    for target, directives in faults.items():
        if isinstance(target, str) and isinstance(directives, list):
            out[target] = [d for d in directives if isinstance(d, dict)]
    return out


def install_sim(
    backend: Any,
    *,
    plan_path: str,
    env: Mapping[str, str],
) -> SimBackend:
    """Wrap `backend` for `keel sim <plan>` (`KEEL_SIM_PLAN=<plan_path>`)."""
    faults = _load_faults(plan_path)
    cursor = _Cursor(Path(f"{plan_path}.cursor.json"))
    if env.get("KEEL_QUIET", "").strip().lower() not in _TRUTHY:
        sys.stderr.write(f"keel ▸ fault-simulating with {plan_path} — see docs/sim-format.md\n")
    return SimBackend(backend, faults, cursor)


__all__ = [
    "SIM_CRASH_EXIT_CODE",
    "SIM_VERSION",
    "SimBackend",
    "install_sim",
]
