"""The exit-time console summary — `[telemetry].console` (design spec Part A).

Two stderr lines at process exit answering "did Keel do anything this run?":

    keel ▸ 47 calls · absorbed 3 rate limits · 2 retries succeeded · 1 breaker trip · 4 calls unprotected
           keel report --open for the full picture

Counts are accumulated in memory by `Discovery.record()` (the one seam every
intercepted call passes through — and the only place that knows whether the
target was wrapped), then formatted once from `bootstrap._flush`. Wording is
pinned by the shared corpus in conformance/console_summary/, which the Node
front end consumes too: identical counts must print identical bytes.
"""

from __future__ import annotations

import shutil
import threading
from typing import Any

PREFIX = "keel ▸ "
# Continuation lines align under the text after the prefix (7 columns).
INDENT = " " * len(PREFIX)

COUNT_KEYS = (
    "calls",
    "throttled",
    "retries_succeeded",
    "breaker_trips",
    "cache_hits",
    "not_retried",
    "unprotected",
)


class Summary:
    """In-memory per-run counters. Thread-safe: adapters record from worker
    threads, so increments share one lock (mirrors `Discovery._lock`)."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._counts: dict[str, int] = {k: 0 for k in COUNT_KEYS}
        self._by_target: dict[str, int] = {}
        # JSON-summary-only (WS9, #78) — never printed in the text form, so it
        # is not one of COUNT_KEYS.
        self._cache_poll_suspects = 0

    def observe(self, outcome: dict[str, Any], wrapped: bool, target: str | None = None) -> None:
        """Fold one call's outcome envelope in. Classification mirrors
        `_discovery._row_from_outcome` exactly, except `retries_succeeded`,
        which is narrower than discovery's `retries` (success only).

        `target` is optional and, when given alongside an unwrapped call,
        feeds `unprotected_by_target()` — the attribution behind the exit
        summary's "N calls unprotected (…)" breakdown (#96)."""
        result = outcome.get("result")
        from_cache = bool(outcome.get("from_cache"))
        attempts = int(outcome.get("attempts", 0) or 0)
        err = outcome.get("error") or {}
        code = err.get("code")
        with self._lock:
            c = self._counts
            c["calls"] += 1
            if outcome.get("throttled"):
                c["throttled"] += 1
            if result == "ok" and not from_cache and attempts > 1:
                c["retries_succeeded"] += 1
            if code == "KEEL-E012":  # breaker fast-fail (and LLM budget block, by design)
                c["breaker_trips"] += 1
            if result == "ok" and from_cache:
                c["cache_hits"] += 1
            if code == "KEEL-E014":  # observed, not retried
                c["not_retried"] += 1
            if not wrapped:
                c["unprotected"] += 1
                if target:
                    self._by_target[target] = self._by_target.get(target, 0) + 1

    def counts(self) -> dict[str, int]:
        with self._lock:
            return dict(self._counts)

    def unprotected_by_target(self) -> dict[str, int]:
        with self._lock:
            return dict(self._by_target)

    def note_cache_poll_suspect(self) -> None:
        """Called once per fired detector key (WS9, #78) — JSON summary only."""
        with self._lock:
            self._cache_poll_suspects += 1

    def cache_poll_suspects(self) -> int:
        with self._lock:
            return self._cache_poll_suspects


def _n(count: int, singular: str, plural: str) -> str:
    return f"{count} {singular if count == 1 else plural}"


def _unprotected_segment(n: int, by_target: dict[str, int] | None) -> str:
    """The `unprotected` segment: a bare count, or — when a per-target
    breakdown is supplied — the count plus a parenthetical naming the top
    three targets (count desc, then name asc), with a `+{k} others` tail when
    more than three targets contributed (#96)."""
    seg = f"{_n(n, 'call', 'calls')} unprotected"
    if not by_target:
        return seg
    ranked = sorted(by_target.items(), key=lambda kv: (-kv[1], kv[0]))
    top = ", ".join(f"{t} {c}" for t, c in ranked[:3])
    rest = len(ranked) - 3
    if rest > 0:
        top += f", +{_n(rest, 'other', 'others')}"
    return f"{seg} ({top})"


def format_summary(
    counts: dict[str, int], keel_on_path: bool, by_target: dict[str, int] | None = None
) -> str:
    """The two lines (with trailing newlines), or `""` when nothing was
    intercepted — a no-op run stays silent."""
    calls = int(counts.get("calls", 0))
    if calls == 0:
        return ""
    segments = [_n(calls, "call", "calls")]
    throttled = int(counts.get("throttled", 0))
    if throttled:
        segments.append(f"absorbed {_n(throttled, 'rate limit', 'rate limits')}")
    retries = int(counts.get("retries_succeeded", 0))
    if retries:
        segments.append(f"{_n(retries, 'retry', 'retries')} succeeded")
    trips = int(counts.get("breaker_trips", 0))
    if trips:
        segments.append(_n(trips, "breaker trip", "breaker trips"))
    cached = int(counts.get("cache_hits", 0))
    if cached:
        segments.append(f"{cached} served from cache")
    not_retried = int(counts.get("not_retried", 0))
    if not_retried:
        segments.append(f"{_n(not_retried, 'failure', 'failures')} not retried")
    unprotected = int(counts.get("unprotected", 0))
    if unprotected:
        segments.append(_unprotected_segment(unprotected, by_target))
    command = "keel report --open" if keel_on_path else "uvx --from keelrun-cli keel report --open"
    return f"{PREFIX}{' · '.join(segments)}\n{INDENT}{command} for the full picture\n"


def format_summary_json(
    counts: dict[str, int],
    meta: dict[str, Any],
    by_target: dict[str, int] | None = None,
    cache_poll_suspects: int = 0,
) -> str:
    """The `KEEL_LOG_FORMAT=json` twin of `format_summary`: one line, sorted
    keys, no spaces. Unlike the text form it prints even at zero calls — in a
    container "Keel activated and intercepted nothing" is itself the evidence
    the outage post-mortem needed. Pinned by conformance/console_summary_json/,
    which the Node front end reads too (identical bytes, both languages).
    `unprotected_by_target` carries the FULL map (every target, sorted keys,
    `{}` when none) — the text line only ever names the top three (#96).
    `cache_poll_suspects` (WS9, #78) is JSON-summary-only — never printed in
    the text form, so it is not one of COUNT_KEYS."""
    from ._log import dumps_line

    obj: dict[str, Any] = {k: int(counts.get(k, 0)) for k in COUNT_KEYS}
    obj["unprotected_by_target"] = dict(sorted((by_target or {}).items()))
    obj["cache_poll_suspects"] = int(cache_poll_suspects)
    obj.update({"keel": "summary", **meta})
    return dumps_line(obj)


def keel_on_path() -> bool:
    """Whether the `keel` CLI binary is installed — decides which bridge line
    to print. Checked once, at emit time."""
    return shutil.which("keel") is not None
