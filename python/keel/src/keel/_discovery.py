"""The discovery store: per-target traffic aggregates in `.keel/discovery.db`.

This is the third evidence source behind `keel init`/`status`/`doctor` (DX
spec §2). Its schema matches the canonical one owned by the keel-journal
crate (Task 1) column-for-column, so a `.keel/discovery.db` written by the
Python front end is readable by the same tools as one written by the core:

    CREATE TABLE discovery (
        target            TEXT PRIMARY KEY,
        calls             INTEGER NOT NULL DEFAULT 0,  -- intercepted calls
        attempts          INTEGER NOT NULL DEFAULT 0,  -- upstream attempts (Σ)
        retries           INTEGER NOT NULL DEFAULT 0,  -- attempts beyond the 1st
        successes         INTEGER NOT NULL DEFAULT 0,
        failures          INTEGER NOT NULL DEFAULT 0,
        cache_hits        INTEGER NOT NULL DEFAULT 0,
        throttled         INTEGER NOT NULL DEFAULT 0,
        breaker_opens     INTEGER NOT NULL DEFAULT 0,
        total_latency_ms  INTEGER NOT NULL DEFAULT 0,
        max_latency_ms    INTEGER NOT NULL DEFAULT 0,
        first_seen_ms     INTEGER NOT NULL,
        last_seen_ms      INTEGER NOT NULL,
        last_error_class  TEXT,
        last_error_status INTEGER
    ) WITHOUT ROWID;

Accounting mirrors the crate: a cache hit is a `call` and a `cache_hit` only
(no upstream attempt), so `calls == successes + failures + cache_hits`. Every
mutation is a single UPSERT, so two processes recording into one file
accumulate correctly without a transaction (WAL, `busy_timeout`).

Discovery is best-effort: it must never throw into, slow, or add output to
the user's program (DX invariant 4). Every public method swallows its own
errors and returns quietly.
"""

from __future__ import annotations

import sqlite3
import threading
import time
from pathlib import Path
from typing import Any

_DISCOVERY_SCHEMA = """\
CREATE TABLE IF NOT EXISTS discovery (
    target            TEXT PRIMARY KEY,
    calls             INTEGER NOT NULL DEFAULT 0,
    attempts          INTEGER NOT NULL DEFAULT 0,
    retries           INTEGER NOT NULL DEFAULT 0,
    successes         INTEGER NOT NULL DEFAULT 0,
    failures          INTEGER NOT NULL DEFAULT 0,
    cache_hits        INTEGER NOT NULL DEFAULT 0,
    throttled         INTEGER NOT NULL DEFAULT 0,
    breaker_opens     INTEGER NOT NULL DEFAULT 0,
    total_latency_ms  INTEGER NOT NULL DEFAULT 0,
    max_latency_ms    INTEGER NOT NULL DEFAULT 0,
    first_seen_ms     INTEGER NOT NULL,
    last_seen_ms      INTEGER NOT NULL,
    last_error_class  TEXT,
    last_error_status INTEGER
) WITHOUT ROWID;"""

# `?` (qmark) placeholders in the same column order as the crate's numbered
# statement; the ON CONFLICT body is identical (counters add, extremes keep,
# first_seen shrinks / last_seen grows, error columns move together).
_UPSERT = """\
INSERT INTO discovery
    (target, calls, attempts, retries, successes, failures, cache_hits,
     throttled, breaker_opens, total_latency_ms, max_latency_ms,
     first_seen_ms, last_seen_ms, last_error_class, last_error_status)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(target) DO UPDATE SET
    calls            = calls + excluded.calls,
    attempts         = attempts + excluded.attempts,
    retries          = retries + excluded.retries,
    successes        = successes + excluded.successes,
    failures         = failures + excluded.failures,
    cache_hits       = cache_hits + excluded.cache_hits,
    throttled        = throttled + excluded.throttled,
    breaker_opens    = breaker_opens + excluded.breaker_opens,
    total_latency_ms = total_latency_ms + excluded.total_latency_ms,
    max_latency_ms   = max(max_latency_ms, excluded.max_latency_ms),
    first_seen_ms    = min(first_seen_ms, excluded.first_seen_ms),
    last_seen_ms     = max(last_seen_ms, excluded.last_seen_ms),
    last_error_class = coalesce(excluded.last_error_class, last_error_class),
    last_error_status = CASE
        WHEN excluded.last_error_class IS NOT NULL THEN excluded.last_error_status
        ELSE last_error_status END"""


class Discovery:
    """A per-target traffic ledger over its own WAL-mode SQLite file. One
    connection per process (shared across threads under a lock, matching the
    crate's `Mutex<Connection>`); one UPSERT per recorded call."""

    def __init__(self, cwd: str | Path | None = None) -> None:
        self.db_path = Path(cwd or Path.cwd()) / ".keel" / "discovery.db"
        self._lock = threading.Lock()
        self._conn: sqlite3.Connection | None = None

    def _connect(self) -> sqlite3.Connection | None:
        """Open (once) the connection, lazily so a disabled/never-recording
        run never touches the filesystem. Returns None if the store can't be
        opened (permissions, fs) — recording then no-ops."""
        if self._conn is not None:
            return self._conn
        try:
            self.db_path.parent.mkdir(parents=True, exist_ok=True)
            conn = sqlite3.connect(self.db_path, check_same_thread=False)
            conn.execute("PRAGMA journal_mode = WAL;")
            conn.execute("PRAGMA busy_timeout = 5000;")
            conn.execute("PRAGMA synchronous = NORMAL;")
            conn.executescript(_DISCOVERY_SCHEMA)
            conn.commit()
        except sqlite3.Error:
            return None
        self._conn = conn
        return conn

    def record(self, target: str, outcome: dict[str, Any], latency_ms: int) -> None:
        """Fold one intercepted call's outcome envelope into its target's
        aggregates. Best-effort: never raises."""
        row = _row_from_outcome(target, outcome, latency_ms)
        try:
            with self._lock:
                conn = self._connect()
                if conn is None:
                    return
                conn.execute(_UPSERT, row)
                conn.commit()
        except sqlite3.Error:
            pass  # best-effort: discovery never breaks the user's program

    def close(self) -> None:
        with self._lock:
            if self._conn is not None:
                try:
                    self._conn.close()
                finally:
                    self._conn = None


def _row_from_outcome(
    target: str, outcome: dict[str, Any], latency_ms: int
) -> tuple[Any, ...]:
    """Project a core outcome envelope onto one `discovery` row's 15 values."""
    result = outcome.get("result")
    from_cache = bool(outcome.get("from_cache"))
    attempts = int(outcome.get("attempts", 0) or 0)

    cache_hit = result == "ok" and from_cache
    success = result == "ok" and not from_cache
    failure = result != "ok"

    err = outcome.get("error") or {}
    last_error_class = err.get("class") if failure else None
    last_error_status = err.get("http_status") if failure else None

    now = int(time.time() * 1000)
    return (
        target,
        1,  # calls
        attempts,
        attempts - 1 if attempts > 0 else 0,  # retries
        1 if success else 0,
        1 if failure else 0,
        1 if cache_hit else 0,
        1 if outcome.get("throttled") else 0,
        1 if outcome.get("breaker") == "open" else 0,
        latency_ms,  # total_latency_ms
        latency_ms,  # max_latency_ms
        now,  # first_seen_ms
        now,  # last_seen_ms
        last_error_class,
        last_error_status,
    )
