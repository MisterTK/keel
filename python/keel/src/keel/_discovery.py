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
        last_error_status INTEGER,
        not_retried       INTEGER NOT NULL DEFAULT 0,  -- KEEL-E014: observed, not retried
        unwrapped_calls   INTEGER NOT NULL DEFAULT 0   -- calls with no [target] policy entry
    ) WITHOUT ROWID;

    CREATE TABLE discovery_daily (                     -- rolling daily buckets
        target          TEXT NOT NULL,                 -- (kept RETENTION_DAYS days)
        day             INTEGER NOT NULL,               -- UTC day index: ms / 86_400_000
        calls           INTEGER NOT NULL DEFAULT 0,
        attempts        INTEGER NOT NULL DEFAULT 0,
        retries         INTEGER NOT NULL DEFAULT 0,
        successes       INTEGER NOT NULL DEFAULT 0,
        failures        INTEGER NOT NULL DEFAULT 0,
        cache_hits      INTEGER NOT NULL DEFAULT 0,
        throttled       INTEGER NOT NULL DEFAULT 0,
        breaker_opens   INTEGER NOT NULL DEFAULT 0,
        not_retried     INTEGER NOT NULL DEFAULT 0,
        unwrapped_calls INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (target, day)
    ) WITHOUT ROWID;

Accounting mirrors the crate: a cache hit is a `call` and a `cache_hit` only
(no upstream attempt), so `calls == successes + failures + cache_hits`.
`breaker_opens` counts calls that FAILED FAST on an open breaker — outcomes with
error code `KEEL-E012` — matching the Rust core (`error.code == BreakerOpen`,
crates/keel-journal/src/discovery.rs) and the Node twin, NOT every outcome whose
`breaker` field reads "open" (that field is also stamped on the terminal failure
that trips the breaker and on a cache hit served while open). `not_retried`
counts calls that resolved KEEL-E014 (failed, and Keel refused to retry because
the call is not idempotent — the DX Level 0 hard rule's "observed, not
retried"). `unwrapped_calls` counts calls whose target had no explicit
`[target."…"]` entry in the EFFECTIVE policy handed to `backend.configure()`
(the same policy the core layers no pack underneath, per the CCR) — the honest
coverage gap `keel status` reports; `Discovery` is told the set of explicit
target keys once, at construction, by `bootstrap.install_keel`. Every mutation
is a single UPSERT per table, so two processes recording into one file
accumulate correctly without a transaction (WAL, `busy_timeout`).

Migration: a file written by the previous (v1, `user_version = 0`) schema is
upgraded in place on first open — the two counter columns are appended
(`ALTER TABLE … ADD COLUMN`, so column order matches a fresh v2 file), the
daily table is created, and `user_version` is stamped to 2. Mirrors
`crates/keel-journal/src/discovery.rs::migrate` exactly, so either writer can
open a file the other created.

Discovery is best-effort: it must never throw into, slow, or add output to
the user's program (DX invariant 4). Every public method swallows its own
errors and returns quietly.
"""

from __future__ import annotations

import sqlite3
import threading
from pathlib import Path
from time import time as _wall_clock  # captured at import: immune to in-flow
from typing import Any  # time virtualization (keel's own clock is never journaled)

#: Current discovery schema version, stamped in `PRAGMA user_version`.
#: Mirrors `keel_journal::discovery::DISCOVERY_SCHEMA_VERSION`.
SCHEMA_VERSION = 2

#: How many trailing UTC days of `discovery_daily` buckets are kept. Mirrors
#: `keel_journal::discovery::RETENTION_DAYS`.
RETENTION_DAYS = 30

#: Milliseconds per UTC day; `day = now_ms // MS_PER_DAY` is the bucket key.
MS_PER_DAY = 86_400_000

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
    last_error_status INTEGER,
    not_retried       INTEGER NOT NULL DEFAULT 0,
    unwrapped_calls   INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;"""

_DAILY_SCHEMA = """\
CREATE TABLE IF NOT EXISTS discovery_daily (
    target          TEXT NOT NULL,
    day             INTEGER NOT NULL,
    calls           INTEGER NOT NULL DEFAULT 0,
    attempts        INTEGER NOT NULL DEFAULT 0,
    retries         INTEGER NOT NULL DEFAULT 0,
    successes       INTEGER NOT NULL DEFAULT 0,
    failures        INTEGER NOT NULL DEFAULT 0,
    cache_hits      INTEGER NOT NULL DEFAULT 0,
    throttled       INTEGER NOT NULL DEFAULT 0,
    breaker_opens   INTEGER NOT NULL DEFAULT 0,
    not_retried     INTEGER NOT NULL DEFAULT 0,
    unwrapped_calls INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (target, day)
) WITHOUT ROWID;"""

# `?` (qmark) placeholders in the same column order as the crate's numbered
# statement; the ON CONFLICT body is identical (counters add, extremes keep,
# first_seen shrinks / last_seen grows, error columns move together).
_UPSERT = """\
INSERT INTO discovery
    (target, calls, attempts, retries, successes, failures, cache_hits,
     throttled, breaker_opens, total_latency_ms, max_latency_ms,
     first_seen_ms, last_seen_ms, last_error_class, last_error_status,
     not_retried, unwrapped_calls)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
        ELSE last_error_status END,
    not_retried      = not_retried + excluded.not_retried,
    unwrapped_calls  = unwrapped_calls + excluded.unwrapped_calls"""

_DAILY_UPSERT = """\
INSERT INTO discovery_daily
    (target, day, calls, attempts, retries, successes, failures, cache_hits,
     throttled, breaker_opens, not_retried, unwrapped_calls)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(target, day) DO UPDATE SET
    calls           = calls + excluded.calls,
    attempts        = attempts + excluded.attempts,
    retries         = retries + excluded.retries,
    successes       = successes + excluded.successes,
    failures        = failures + excluded.failures,
    cache_hits      = cache_hits + excluded.cache_hits,
    throttled       = throttled + excluded.throttled,
    breaker_opens   = breaker_opens + excluded.breaker_opens,
    not_retried     = not_retried + excluded.not_retried,
    unwrapped_calls = unwrapped_calls + excluded.unwrapped_calls"""


class Discovery:
    """A per-target traffic ledger over its own WAL-mode SQLite file. One
    connection per process (shared across threads under a lock, matching the
    crate's `Mutex<Connection>`); one UPSERT per table per recorded call.

    `known_targets` is the set of EXPLICIT `[target."…"]` keys in the
    effective policy (defaults < packs < user, before `backend.configure()`)
    — used to classify each recorded call as wrapped (an explicit entry
    applied) or not (the coverage gap `keel status` reports). Passing none
    (the default) means every call is counted unwrapped, which is correct for
    a store opened outside `bootstrap.install_keel` (e.g. read-only tooling
    that never records)."""

    def __init__(
        self,
        cwd: str | Path | None = None,
        known_targets: frozenset[str] | None = None,
    ) -> None:
        self.db_path = Path(cwd or Path.cwd()) / ".keel" / "discovery.db"
        self._known_targets = known_targets or frozenset()
        self._lock = threading.Lock()
        self._conn: sqlite3.Connection | None = None
        self._last_prune_day: int | None = None

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
            _migrate(conn)
        except sqlite3.Error:
            return None
        self._conn = conn
        return conn

    def record(self, target: str, outcome: dict[str, Any], latency_ms: int) -> None:
        """Fold one intercepted call's outcome envelope into its target's
        aggregates (lifetime row plus the clock-day bucket). Best-effort:
        never raises."""
        wrapped = target in self._known_targets
        row = _row_from_outcome(target, outcome, latency_ms, wrapped)
        now_ms = row[12]  # last_seen_ms, per _row_from_outcome's column order
        day = now_ms // MS_PER_DAY
        try:
            with self._lock:
                conn = self._connect()
                if conn is None:
                    return
                conn.execute(_UPSERT, row)
                conn.execute(_DAILY_UPSERT, _daily_row(row, day))
                self._prune(conn, day)
                conn.commit()
        except sqlite3.Error:
            pass  # best-effort: discovery never breaks the user's program

    def _prune(self, conn: sqlite3.Connection, day: int) -> None:
        """Drop daily buckets older than the retention window, at most once
        per advanced day (mirrors the crate's `DiscoveryStore::prune`)."""
        if self._last_prune_day == day:
            return
        self._last_prune_day = day
        conn.execute(
            "DELETE FROM discovery_daily WHERE day < ?",
            (day - (RETENTION_DAYS - 1),),
        )

    def close(self) -> None:
        with self._lock:
            if self._conn is not None:
                try:
                    self._conn.close()
                finally:
                    self._conn = None


def _migrate(conn: sqlite3.Connection) -> None:
    """Bring a connection to [`SCHEMA_VERSION`]: create the current schema on
    a fresh file, or append the v2 counter columns and the daily table to a
    legacy (v1) one. Mirrors `keel_journal::discovery::migrate` — idempotent,
    so re-opening an already-migrated file is a no-op."""
    (version,) = conn.execute("PRAGMA user_version").fetchone()
    if version >= SCHEMA_VERSION:
        return
    has_table = conn.execute(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'discovery')"
    ).fetchone()[0]
    if has_table:
        has_column = conn.execute(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('discovery') "
            "WHERE name = 'not_retried')"
        ).fetchone()[0]
        if not has_column:
            # Appended, so a migrated file's column order matches a fresh v2 one.
            conn.execute(
                "ALTER TABLE discovery ADD COLUMN not_retried INTEGER NOT NULL DEFAULT 0"
            )
            conn.execute(
                "ALTER TABLE discovery ADD COLUMN unwrapped_calls INTEGER NOT NULL DEFAULT 0"
            )
    else:
        conn.executescript(_DISCOVERY_SCHEMA)
    conn.executescript(_DAILY_SCHEMA)
    conn.execute(f"PRAGMA user_version = {SCHEMA_VERSION}")
    conn.commit()


def _row_from_outcome(
    target: str, outcome: dict[str, Any], latency_ms: int, wrapped: bool = True
) -> tuple[Any, ...]:
    """Project a core outcome envelope onto one `discovery` row's 17 values."""
    result = outcome.get("result")
    from_cache = bool(outcome.get("from_cache"))
    attempts = int(outcome.get("attempts", 0) or 0)

    cache_hit = result == "ok" and from_cache
    success = result == "ok" and not from_cache
    failure = result != "ok"

    err = outcome.get("error") or {}
    last_error_class = err.get("class") if failure else None
    last_error_status = err.get("http_status") if failure else None

    now = int(_wall_clock() * 1000)
    return (
        target,
        1,  # calls
        attempts,
        attempts - 1 if attempts > 0 else 0,  # retries
        1 if success else 0,
        1 if failure else 0,
        1 if cache_hit else 0,
        1 if outcome.get("throttled") else 0,
        # breaker_opens = fail-fast rejections only (KEEL-E012), the canonical
        # rule shared with the Rust core and Node — NOT any breaker=="open" stamp.
        1 if err.get("code") == "KEEL-E012" else 0,
        latency_ms,  # total_latency_ms
        latency_ms,  # max_latency_ms
        now,  # first_seen_ms
        now,  # last_seen_ms
        last_error_class,
        last_error_status,
        # not_retried = KEEL-E014: observed, not retried (Level 0 hard rule).
        1 if err.get("code") == "KEEL-E014" else 0,
        0 if wrapped else 1,  # unwrapped_calls
    )


def _daily_row(row: tuple[Any, ...], day: int) -> tuple[Any, ...]:
    """Project a `discovery` row (see [`_row_from_outcome`]) onto its
    `discovery_daily` twin: same target and counters, keyed by `day` instead
    of the seen-timestamps and error columns the daily table omits."""
    (
        target,
        calls,
        attempts,
        retries,
        successes,
        failures,
        cache_hits,
        throttled,
        breaker_opens,
        _total_latency_ms,
        _max_latency_ms,
        _first_seen_ms,
        _last_seen_ms,
        _last_error_class,
        _last_error_status,
        not_retried,
        unwrapped_calls,
    ) = row
    return (
        target,
        day,
        calls,
        attempts,
        retries,
        successes,
        failures,
        cache_hits,
        throttled,
        breaker_opens,
        not_retried,
        unwrapped_calls,
    )
