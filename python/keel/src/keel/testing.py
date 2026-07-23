"""Offline replay for a `keel record` capture — the reusable core that `keel
record test`-generated test files import (rather than duplicating matching
logic per generated file). See ``docs/recording-format.md`` for the full
non-contract line format and the canonical request-matching rule this module
implements:

  1. ``target`` must match exactly.
  2. If the live call's ``args_hash`` is not ``None``, it must equal a
     recorded call's ``args_hash`` exactly (byte-for-byte).
  3. Otherwise (``args_hash`` is ``None`` on both sides — a non-idempotent
     call, which never gets an ``args_hash``), the ``op`` strings must match
     instead.
  4. Among the recorded calls satisfying 1-3, the FIRST one not yet consumed
     is served (recordings are consumed in the order they were made, so a
     repeated call to the identical target replays its recorded repeats in
     order).
  5. No candidate remains → :class:`UnmatchedEffect`, naming
     target/op/args_hash. Replay **never** silently passes an unrecorded call
     through live.

This module has no hard dependency on pytest — :func:`replay_fixture` imports
it lazily, so importing ``keel.testing`` itself never requires it.
"""

from __future__ import annotations

import json
from collections import deque
from pathlib import Path
from typing import TYPE_CHECKING, Any, Callable, Iterator

if TYPE_CHECKING:
    from ._backend import Backend


class UnmatchedEffect(RuntimeError):
    """Raised by :meth:`ReplayBackend.execute` when a live call has no
    recorded match — a novel, reordered, or already-exhausted effect the
    recording does not cover."""


def _match_key(target: str, op: str, args_hash: str | None) -> tuple[str, str]:
    return (target, f"h:{args_hash}") if args_hash is not None else (target, f"o:{op}")


class Recording:
    """A parsed `.ndjson` recording: the `meta` header plus its `call` lines,
    in recorded order. Malformed/foreign lines (a future format's ``type``, a
    blank line) are skipped rather than failing the whole load — only a
    missing or non-`meta` header line is fatal (it means this isn't a Keel
    recording at all)."""

    def __init__(self, meta: dict[str, Any], calls: list[dict[str, Any]]) -> None:
        self.meta = meta
        self.calls = calls

    @classmethod
    def load(cls, path: str | Path) -> "Recording":
        p = Path(path)
        try:
            lines = p.read_text(encoding="utf-8").splitlines()
        except OSError as err:
            raise ValueError(f"keel record test: cannot read {p}: {err}") from err
        if not lines or not lines[0].strip():
            raise ValueError(f"keel record test: {p} is empty — nothing was recorded")
        meta = json.loads(lines[0])
        if not isinstance(meta, dict) or meta.get("type") != "meta":
            raise ValueError(f"keel record test: {p} has no meta header — not a Keel recording")
        calls: list[dict[str, Any]] = []
        for line in lines[1:]:
            line = line.strip()
            if not line:
                continue
            obj = json.loads(line)
            if isinstance(obj, dict) and obj.get("type") == "call":
                calls.append(obj)
        return cls(meta, calls)


class ReplayBackend:
    """A ``Backend`` (see ``keel._backend.Backend``) that serves ``execute``
    calls from a :class:`Recording` instead of running real effects.
    ``configure``/``report`` are no-ops; the caller's real effect closure is
    NEVER invoked — a match is served purely from the recording, and a miss
    raises :class:`UnmatchedEffect` rather than falling through to it."""

    def __init__(self, recording: Recording, resolver: "Backend | None" = None) -> None:
        # `resolver` is the real backend that was active immediately before
        # `install_replay` swapped this one in (see there). `resolve_target`/
        # `layer` delegate to it when present, since a from-scratch replay has
        # no compiled policy of its own to answer either from — see both
        # methods below for why this matters (Task 10 regression, issue: HTTP
        # packs call `_runtime.get_backend().resolve_target(...)`
        # unconditionally, so whatever backend is installed must expose it).
        self._resolver = resolver
        self._queues: dict[tuple[str, str], deque[dict[str, Any]]] = {}
        for call in recording.calls:
            key = _match_key(
                str(call.get("target", "")), str(call.get("op", "")), call.get("args_hash")
            )
            self._queues.setdefault(key, deque()).append(call)

    def configure(self, policy: dict[str, Any]) -> None:  # noqa: ARG002 - Backend protocol
        return None

    def layer(self, target: str, key: str) -> Any:
        # No resolver: unchanged from before — a harmless no-op (pinned by
        # test_layer_and_configure_and_report_are_harmless_no_ops).
        if self._resolver is not None:
            return self._resolver.layer(target, key)
        return None

    def resolve_target(
        self,
        method: str,
        host: str,
        scheme: str | None = None,
        port: int | None = None,
        path: str | None = None,
    ) -> str:
        """The policy target key a live HTTP pack call resolves to during
        replay. Delegates to the pre-replay backend (`resolver`) when one was
        supplied, so a recording made under a given `keel.toml` replays
        against the SAME target the original call computed
        (docs/recording-format.md rule 1: `target` must match exactly) —
        assuming the normal usage pattern of the same policy being in effect
        during both recording and replay. With no resolver (e.g. a bare
        ``ReplayBackend(rec)`` built directly, no policy in scope), this
        deliberately falls back to the bare host — the same "no policy
        configured" default the core/stub use when there's no compiled
        `[target]` table — purely for backward compatibility with direct
        construction; it is NOT a claim that this reproduces real target
        resolution.
        """
        if self._resolver is not None:
            return self._resolver.resolve_target(method, host, scheme=scheme, port=port, path=path)
        return host

    def execute(self, request: dict[str, Any], effect: Callable[[int], Any]) -> dict[str, Any]:
        del effect  # replay never invokes the real effect (that's the point)
        target = str(request.get("target", ""))
        op = str(request.get("op", ""))
        args_hash = request.get("args_hash")
        queue = self._queues.get(_match_key(target, op, args_hash))
        if not queue:
            raise UnmatchedEffect(
                f"keel record test: no recorded call matches target={target!r} "
                f"op={op!r} args_hash={args_hash!r} — re-record, or check the "
                "code under test for an unrecorded/novel effect"
            )
        outcome = queue.popleft()["outcome"]
        # A replayed "ok" is served with no live call at all, so it must look
        # like a cache hit to the adapter (the HTTP packs' `deliver()` returns
        # the LIVE response object when `from_cache` is falsy — there is none
        # here — and only rebuilds one from `payload` when `from_cache` is
        # true). A recording made from a real, non-cached call has
        # `from_cache: False`; flip it here rather than at capture time, so
        # the recording still reads as "what really happened" on disk.
        if isinstance(outcome, dict) and outcome.get("result") == "ok" and not outcome.get("from_cache"):
            outcome = {**outcome, "from_cache": True}
        return outcome

    def report(self) -> dict[str, Any]:
        return {}


def install_replay(path: str | Path) -> Callable[[], None]:
    """Install a :class:`ReplayBackend` for ``path`` as the process runtime
    backend (parity with ``_runtime.set_runtime``), arming every
    dynamic-lookup adapter (httpx/requests/urllib3/aiohttp/boto3/psycopg —
    everything that reads ``_runtime.get_backend()`` per call; see
    ``docs/recording-format.md``'s "Known limitations" for what this does NOT
    cover). Returns an ``uninstall`` callable that restores the previous
    backend — call it (or use :func:`replay_fixture`, which does this for
    you) when the replay scope ends."""
    from . import _runtime
    from .adapters import install_adapters

    install_adapters()  # idempotent; arms present libraries lazily
    previous_backend = _runtime.get_backend()
    previous_discovery = _runtime.get_discovery()
    # `resolver=previous_backend`: resolve_target/layer delegate to whatever
    # backend was active before this swap (see ReplayBackend.resolve_target's
    # docstring for why this is required, not optional).
    _runtime.set_runtime(
        ReplayBackend(Recording.load(path), resolver=previous_backend), previous_discovery
    )

    def _uninstall() -> None:
        _runtime.set_runtime(previous_backend, previous_discovery)

    return _uninstall


def replay_fixture(path: str | Path) -> Any:
    """A pytest fixture factory for `keel record test`-generated files:

        keel_replay = replay_fixture("recording.ndjson")

        def test_it(keel_replay):
            ...

    Installs the replay backend for the duration of each test that requests
    the fixture and restores the previous runtime afterward. Requires pytest
    (imported lazily — importing ``keel.testing`` itself does not)."""
    import pytest

    @pytest.fixture
    def _keel_replay_fixture() -> Iterator[ReplayBackend]:
        from . import _runtime

        uninstall = install_replay(path)
        try:
            yield _runtime.get_backend()  # the just-installed ReplayBackend
        finally:
            uninstall()

    return _keel_replay_fixture


__all__ = [
    "Recording",
    "ReplayBackend",
    "UnmatchedEffect",
    "install_replay",
    "replay_fixture",
]
