"""Runtime self-diagnosis of the cache-poll pathology (WS9, issue #78): N
consecutive cache hits for ONE identical call, spaced like a status poll,
almost always means a status-poll loop is being fed a replayed "still
running" response. Keel names it once per (target, args_hash) instead of
letting the app hang to its own deadline (field incidents 2026-08-30 and
2026-09-15). No new error code — a warning line with a `code` in its JSON
twin; `keel explain` content lands with poll v2 (#93)."""

from __future__ import annotations

import os
import time
from typing import Any, Callable


class CachePollDetector:
    """Per-`(target, args_hash)` run tracker. A `from_cache` outcome
    increments the run's hit count; any other outcome for the same key
    resets it — "N consecutive" means consecutive. Fires once per key per
    process when `hits >= min_hits` AND the run's span (`last - first`) is
    at least `min_span_s` — the span rule is what tells a status poll (hits
    spaced seconds apart) apart from a test suite replaying one prompt in a
    burst. Never fires when `args_hash` is `None`/falsy."""

    def __init__(
        self,
        clock: Callable[[], float] = time.monotonic,
        *,
        min_hits: int = 5,
        min_span_s: float | None = None,
        on_suspect: Callable[[str, int, int], None] | None = None,
    ) -> None:
        self._clock = clock
        self._min_hits = min_hits
        # KEEL_CACHEPOLL_MIN_SPAN_S is UNSTABLE — test-only, read once at
        # construction, so the acceptance test can shrink the ~minutes-long
        # real pathology into a ~25s run without changing production defaults.
        env_span = os.environ.get("KEEL_CACHEPOLL_MIN_SPAN_S")
        self._min_span = float(min_span_s if min_span_s is not None else (env_span or 20.0))
        self._on_suspect = on_suspect
        self._runs: dict[tuple[str, str], list[float]] = {}  # key -> [hits, first, last]
        self._fired: set[tuple[str, str]] = set()

    def observe(self, target: str, args_hash: str | None, outcome: dict[str, Any]) -> bool:
        if not args_hash:
            return False
        key = (target, args_hash)
        if not outcome.get("from_cache"):
            self._runs.pop(key, None)
            return False
        now = self._clock()
        run = self._runs.get(key)
        if run is None:
            self._runs[key] = [1, now, now]
            return False
        run[0] += 1
        run[2] = now
        if key in self._fired or run[0] < self._min_hits or (run[2] - run[1]) < self._min_span:
            return False
        self._fired.add(key)
        if self._on_suspect is not None:
            self._on_suspect(target, int(run[0]), int(run[2] - run[1]))
        return True
