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
daily table is created, and `user_version` is stamped to 2. A v2 file gains
the `activations` table (one row per process, WS8/#92) and is stamped to 3.
Mirrors `crates/keel-journal/src/discovery.rs::migrate` exactly, so either
writer can open a file the other created.

`activations` records one row per process's lazily-remembered call to
`record_activation` (ts_ms/pid/language/version/cwd/keel_cwd/policy_source/
policy_path/flows_configured/argv0), written on the first successful
`_connect()` (i.e. the first recorded call) or at `close()` if no call ever
happened — whichever comes first, and exactly once — so an activated-but-idle
process still leaves exactly one row of evidence at exit, and a process that
never activates never touches the filesystem. Retention keeps the newest 50
rows (by `ts_ms` then `rowid`), mirroring the crate.

Discovery is best-effort: it must never throw into, slow, or add output to
the user's program (DX invariant 4). Every public method swallows its own
errors and returns quietly.
"""

from __future__ import annotations

import sqlite3
import threading
from pathlib import Path
from time import time as _wall_clock  # captured at import: immune to in-flow
from typing import TYPE_CHECKING, Any  # time virtualization (keel's own clock is never journaled)

if TYPE_CHECKING:
    from ._summary import Summary

#: Current discovery schema version, stamped in `PRAGMA user_version`.
#: Mirrors `keel_journal::discovery::DISCOVERY_SCHEMA_VERSION`.
SCHEMA_VERSION = 3

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

_ACTIVATIONS_SCHEMA = """\
CREATE TABLE IF NOT EXISTS activations (
    ts_ms            INTEGER NOT NULL,
    pid              INTEGER NOT NULL,
    language         TEXT    NOT NULL,
    version          TEXT    NOT NULL,
    cwd              TEXT    NOT NULL,
    keel_cwd         TEXT,
    policy_source    TEXT    NOT NULL,
    policy_path      TEXT,
    flows_configured INTEGER NOT NULL DEFAULT 0,
    argv0            TEXT    NOT NULL DEFAULT ''
);"""

_ACTIVATION_INSERT = (
    "INSERT INTO activations (ts_ms, pid, language, version, cwd, keel_cwd, policy_source, "
    "policy_path, flows_configured, argv0) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
)
_ACTIVATION_PRUNE = (
    "DELETE FROM activations WHERE rowid NOT IN "
    "(SELECT rowid FROM activations ORDER BY ts_ms DESC, rowid DESC LIMIT 50)"
)


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
        summary: "Summary | None" = None,
    ) -> None:
        self.db_path = Path(cwd or Path.cwd()) / ".keel" / "discovery.db"
        self._known_targets = known_targets or frozenset()
        # The exit-time console summary (`_summary.Summary`), fed from
        # `record()` because this is the one place that knows `wrapped`.
        self._summary = summary
        self._lock = threading.Lock()
        self._conn: sqlite3.Connection | None = None
        self._last_prune_day: int | None = None
        self._activation: dict[str, Any] | None = None
        self._activation_written = False

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
        if self._summary is not None:
            try:
                self._summary.observe(outcome, wrapped)
            except Exception:  # noqa: BLE001 — the summary never breaks a call
                pass
        row = _row_from_outcome(target, outcome, latency_ms, wrapped)
        now_ms = row[12]  # last_seen_ms, per _row_from_outcome's column order
        day = now_ms // MS_PER_DAY
        try:
            with self._lock:
                conn = self._connect()
                if conn is None:
                    return
                self._write_activation(conn)
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

    def record_activation(self, row: dict[str, Any]) -> None:
        """Remember this process's activation (WS8). Written lazily — with the
        first recorded call, or at close() — so an idle activation touches the
        filesystem exactly once, at exit."""
        self._activation = dict(row)

    def _write_activation(self, conn: sqlite3.Connection) -> None:
        if self._activation is None or self._activation_written:
            return
        a = self._activation
        conn.execute(_ACTIVATION_INSERT, (
            int(a["ts_ms"]), int(a["pid"]), a["language"], a["version"], a["cwd"], a.get("keel_cwd"),
            a["policy_source"], a.get("policy_path"), int(bool(a.get("flows_configured"))), a.get("argv0") or "",
        ))
        conn.execute(_ACTIVATION_PRUNE)
        self._activation_written = True

    def close(self) -> None:
        with self._lock:
            if self._activation is not None and not self._activation_written:
                try:
                    conn = self._connect()
                    if conn is not None:
                        self._write_activation(conn)
                        conn.commit()
                except sqlite3.Error:
                    pass
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
    if version < 2:
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
    if version < 3:
        conn.executescript(_ACTIVATIONS_SCHEMA)
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
