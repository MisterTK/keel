"""Runtime self-diagnosis of the cache-poll pathology (WS9, issue #78): N
consecutive cache hits for ONE identical call, spaced like a status poll,
almost always means a status-poll loop is being fed a replayed "still
running" response. Keel names it once per (target, args_hash) instead of
letting the app hang to its own deadline (field incidents 2026-08-30 and
2026-09-15). No new error code — a warning line with a `code` in its JSON
twin; `keel explain` content lands with poll v2 (#93)."""

from __future__ import annotations

import os
import threading
import time
from typing import Any, Callable

#: Cap on tracked `(target, args_hash)` runs and on the fired-key set, in
#: BOTH languages (Node twin: `node/keel/src/cachepoll.mjs`'s `MAX_RUNS`/
#: `MAX_FIRED`) — this detector lives inside other people's long-running
#: server processes, and a server making many DISTINCT cached calls (an LLM
#: app with varying prompts is precisely our audience) must not accumulate
#: one entry per distinct call for the life of the process. Named constants,
#: not magic numbers.
MAX_RUNS = 1024
MAX_FIRED = 1024


class CachePollDetector:
    """Per-`(target, args_hash)` run tracker. A `from_cache` outcome
    increments the run's hit count; any other outcome for the same key
    resets it — "N consecutive" means consecutive. Fires once per key per
    process when `hits >= min_hits` AND the run's span (`last - first`) is
    at least `min_span_s` — the span rule is what tells a status poll (hits
    spaced seconds apart) apart from a test suite replaying one prompt in a
    burst. Never fires when `args_hash` is `None`/falsy.

    Thread-safe: the sync HTTP packs (`requests_pack`, `urllib3_pack`,
    `httpx_pack._run_sync`, `urllib_pack`) genuinely run on multiple OS
    threads in a threaded WSGI/worker-pool host, and `discovery.record()`
    calls `observe()` OUTSIDE `Discovery._lock`'s scope (that lock only
    wraps the SQLite write) — so this class owns its own lock rather than
    reusing `Discovery._lock` (which must not widen to cover this, and this
    must stay independently usable). `on_suspect` is invoked AFTER the lock
    is released — decide-and-mark happens under the lock, emitting to
    stderr while holding it is how you get a deadlock later."""

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
        self._lock = threading.Lock()
        self._runs: dict[tuple[str, str], list[float]] = {}  # key -> [hits, first, last]
        # A dict used as an insertion-ordered set (Python 3.7+ dicts preserve
        # insertion order) so eviction can drop the oldest-fired key cheaply.
        self._fired: dict[tuple[str, str], None] = {}

    def observe(self, target: str, args_hash: str | None, outcome: dict[str, Any]) -> bool:
        if not args_hash:
            return False
        key = (target, args_hash)
        suspect: tuple[str, int, int] | None = None
        with self._lock:
            if not outcome.get("from_cache"):
                self._runs.pop(key, None)
                return False
            now = self._clock()
            run = self._runs.get(key)
            if run is None:
                if len(self._runs) >= MAX_RUNS:
                    self._evict_oldest_run_locked()
                self._runs[key] = [1, now, now]
                return False
            run[0] += 1
            run[2] = now
            if key in self._fired or run[0] < self._min_hits or (run[2] - run[1]) < self._min_span:
                return False
            self._mark_fired_locked(key)
            suspect = (target, int(run[0]), int(run[2] - run[1]))
        # Outside the lock: on_suspect (bootstrap's `emit`, which writes to
        # stderr) must never run while this lock is held.
        if suspect is not None and self._on_suspect is not None:
            self._on_suspect(*suspect)
        return suspect is not None

    def _evict_oldest_run_locked(self) -> None:
        """Drop the tracked run with the OLDEST `last` timestamp. A key that
        has not been touched recently is by definition not a live poll run,
        so evicting it cannot lose a detection in progress. Caller holds
        `self._lock`."""
        oldest_key = min(self._runs, key=lambda k: self._runs[k][2])
        del self._runs[oldest_key]

    def _mark_fired_locked(self, key: tuple[str, str]) -> None:
        """Simple insertion-order eviction once the fired set is full.
        Re-firing a key after it has been evicted from a full fired set is
        acceptable, and strictly better than unbounded growth. Caller holds
        `self._lock`."""
        if len(self._fired) >= MAX_FIRED:
            oldest = next(iter(self._fired))
            del self._fired[oldest]
        self._fired[key] = None


def cache_poll_suspect_warning(
    target: str, hits: int, span_s: int, version: str
) -> tuple[str, dict[str, Any]]:
    """The text + `KEEL_LOG_FORMAT=json` twin for one `CachePollDetector`
    firing (#78/WS9), pulled out as its own testable builder (mirroring
    `_deploy.py`'s `ephemeral_journal_warning`) rather than inlined at each of
    bootstrap.py's/bootstrap.mjs's call sites. `severity` is `WARNING` (#130):
    this line names an operator-visible pathology — a status poll silently
    fed a replayed response — and must be as filterable as the activation and
    refusal lines are."""
    text = (
        f"keel ▸ warning: {target} served {hits} consecutive cache hits for one identical "
        f"call over {span_s}s — if this is a status poll, set cache = "
        "{ mode = \"off\" } on that target "
        "— or give the status route its own poll policy (README: Poll)\n"
    )
    obj: dict[str, Any] = {
        "keel": "warning",
        "code": "cache-poll-suspect",
        "target": target,
        "hits": hits,
        "span_s": span_s,
        "severity": "WARNING",
        "version": version,
    }
    return text, obj
