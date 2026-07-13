"""`keel record run` capture: a transparent tee over the runtime `Backend`
that appends every intercepted effect's request/outcome envelope to a
recording file, without changing the wrapped call's behavior.

See ``docs/recording-format.md`` for the full (non-contract) line format, the
capture-seam rationale, and documented v1 limitations. In short: this module
never touches ``contracts/`` — the recording lives entirely at the front-end
boundary between an adapter (httpx/requests/`py:` wrappers/…) and the
``Backend`` protocol (``_backend.py``).
"""

from __future__ import annotations

import atexit
import json
import sys
import threading
import time
from pathlib import Path
from typing import Any, Callable, Mapping

RECORDING_VERSION = 1

#: Header names redacted from a recorded outcome's HTTP response envelope
#: (``payload.headers``) before the line is written. Extend per-run with
#: ``KEEL_RECORD_REDACT_HEADERS`` (comma list, merged with these defaults).
DEFAULT_REDACT_HEADERS: frozenset[str] = frozenset(
    {
        "authorization",
        "proxy-authorization",
        "cookie",
        "set-cookie",
        "x-api-key",
        "api-key",
        "x-auth-token",
        "x-goog-api-key",
    }
)

_TRUTHY = {"1", "true", "yes"}


def redact_headers_from_env(env: Mapping[str, str]) -> frozenset[str]:
    """The active redact set: the defaults plus any comma-separated names in
    ``KEEL_RECORD_REDACT_HEADERS`` (never a replacement of the defaults —
    recording must never make it EASIER to leak a secret than the baseline)."""
    extra = env.get("KEEL_RECORD_REDACT_HEADERS", "")
    names = {h.strip().lower() for h in extra.split(",") if h.strip()}
    return frozenset(DEFAULT_REDACT_HEADERS | names)


def _redact_payload(payload: Any, redact: frozenset[str]) -> Any:
    """Redact header VALUES (never keys) in an HTTP response envelope's
    ``headers`` list (``[[name, value], ...]``, see ``adapters/_http.py``'s
    ``response_envelope``). Any other payload shape (a `py:`/`ts:` function's
    JSON return value, or an envelope with no ``headers`` key) is returned
    unchanged — Keel never inspects arbitrary user data for secrets, only the
    well-known HTTP header shape."""
    if not isinstance(payload, dict):
        return payload
    headers = payload.get("headers")
    if not isinstance(headers, list):
        return payload
    redacted = dict(payload)
    redacted["headers"] = [
        [k, "[REDACTED]" if isinstance(k, str) and k.lower() in redact else v]
        for k, v in headers
    ]
    return redacted


def _has_body(outcome: Any) -> bool:
    """Whether ``outcome``'s payload carries an actual captured body: a
    non-empty ``body_b64`` for an HTTP envelope, or any non-null payload for a
    `py:`/`ts:` function target. Purely informational (surfaced by `keel
    record list`) — never affects matching."""
    if not isinstance(outcome, dict):
        return False
    payload = outcome.get("payload")
    if not isinstance(payload, dict):
        return payload is not None
    return isinstance(payload.get("body_b64"), str) or (
        "__keel_http__" not in payload
    )


class _JsonlWriter:
    """A single-writer append-only NDJSON file. Thread-safe (adapters run
    their effect on whatever thread/task owns the call); never raises into the
    caller's real effect — a write failure degrades to dropping that one line
    rather than breaking the program being recorded (recording is strictly
    best-effort observability, same posture as the Tier 1 event sink)."""

    def __init__(self, path: Path) -> None:
        self._lock = threading.Lock()
        self._seq = 0
        path.parent.mkdir(parents=True, exist_ok=True)
        self._fh = path.open("a", encoding="utf-8")

    def write_meta(
        self,
        *,
        recording_id: str,
        language: str,
        target: str,
        args: list[str],
        redact_headers: frozenset[str],
    ) -> None:
        self._write(
            {
                "v": RECORDING_VERSION,
                "type": "meta",
                "id": recording_id,
                "language": language,
                "target": target,
                "args": args,
                "started_at_ms": round(time.time() * 1000),
                "redacted_headers": sorted(redact_headers),
            }
        )

    def write_call(
        self,
        *,
        target: str,
        op: str,
        idempotent: bool,
        args_hash: str | None,
        outcome: dict[str, Any],
        latency_ms: int,
    ) -> None:
        with self._lock:
            self._seq += 1
            seq = self._seq
        self._write(
            {
                "v": RECORDING_VERSION,
                "type": "call",
                "seq": seq,
                "target": target,
                "op": op,
                "idempotent": bool(idempotent),
                "args_hash": args_hash,
                "attempts": outcome.get("attempts"),
                "latency_ms": latency_ms,
                "body_captured": _has_body(outcome),
                "outcome": outcome,
            }
        )

    def _write(self, obj: dict[str, Any]) -> None:
        try:
            text = json.dumps(obj, default=lambda _o: "<unserializable>")
        except Exception:  # pragma: no cover - defensive, never seen in practice
            try:
                sys.stderr.write("keel ▸ record: dropped an unserializable line\n")
            except Exception:
                pass
            return
        with self._lock:
            try:
                self._fh.write(text + "\n")
                self._fh.flush()
            except Exception:  # pragma: no cover - best-effort observability
                pass

    def close(self) -> None:
        with self._lock:
            try:
                self._fh.close()
            except Exception:
                pass


class RecordingBackend:
    """A transparent tee over ``inner`` (a ``Backend``): ``execute`` and
    ``execute_async`` are forwarded unchanged and their outcome is appended to
    a recording; every other attribute (``report``, ``layer``, the Tier 2 flow
    surface, …) delegates straight through via ``__getattr__``, exactly like
    ``_backend._NativeBackend``. Recording is a pure observer — it never
    alters what the wrapped call receives."""

    def __init__(self, inner: Any, writer: _JsonlWriter, redact: frozenset[str]) -> None:
        self._inner = inner
        self._writer = writer
        self._redact = redact

    def configure(self, policy: dict[str, Any]) -> None:
        self._inner.configure(policy)

    def execute(self, request: dict[str, Any], effect: Callable[[int], Any]) -> dict[str, Any]:
        started = time.perf_counter()
        outcome = self._inner.execute(request, effect)
        self._record(request, outcome, time.perf_counter() - started)
        return outcome

    def report(self) -> dict[str, Any]:
        return self._inner.report()

    def _record(self, request: Any, outcome: Any, elapsed_s: float) -> None:
        if not isinstance(request, dict) or not isinstance(outcome, dict):
            return  # not a shape we understand; never crash the real call for it
        redacted = dict(outcome)
        if "payload" in redacted:
            redacted["payload"] = _redact_payload(redacted["payload"], self._redact)
        self._writer.write_call(
            target=str(request.get("target", "")),
            op=str(request.get("op", "")),
            idempotent=bool(request.get("idempotent", False)),
            args_hash=request.get("args_hash"),
            outcome=redacted,
            latency_ms=round(elapsed_s * 1000),
        )

    def __getattr__(self, name: str) -> Any:
        attr = getattr(self._inner, name)
        if name == "execute_async" and callable(attr):

            async def _wrapped(request: dict[str, Any], effect: Callable[[int], Any]) -> dict[str, Any]:
                started = time.perf_counter()
                outcome = await attr(request, effect)
                self._record(request, outcome, time.perf_counter() - started)
                return outcome

            return _wrapped
        return attr


def install_recording(
    backend: Any,
    *,
    path: str,
    target: str,
    args: list[str] | tuple[str, ...],
    env: Mapping[str, str],
) -> RecordingBackend:
    """Wrap ``backend`` for `keel record run`: writes the `meta` header
    immediately, then returns the tee to install as the process's runtime
    backend (``_runtime.set_runtime``). Also prints the one-line "recording
    to …" banner (suppressed by ``KEEL_QUIET``, matching
    ``bootstrap._banner``)."""
    p = Path(path)
    redact = redact_headers_from_env(env)
    writer = _JsonlWriter(p)
    writer.write_meta(
        recording_id=p.stem,
        language="python",
        target=target,
        args=list(args),
        redact_headers=redact,
    )
    atexit.register(writer.close)  # best-effort fd hygiene; every line is
    # already flushed to disk as it's written, so this changes nothing about
    # durability — it only avoids leaking the handle for the process lifetime.
    if env.get("KEEL_QUIET", "").strip().lower() not in _TRUTHY:
        sys.stderr.write(f"keel ▸ recording to {p} — `keel record list` to inspect\n")
    return RecordingBackend(backend, writer, redact)


__all__ = [
    "RECORDING_VERSION",
    "DEFAULT_REDACT_HEADERS",
    "RecordingBackend",
    "install_recording",
    "redact_headers_from_env",
]
