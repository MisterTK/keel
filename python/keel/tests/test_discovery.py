"""Discovery accounting parity with the canonical crate / Node twin.

Focus: `breaker_opens` counts ONLY fail-fast rejections (outcomes with error code
KEEL-E012), not every outcome whose `breaker` field reads "open". The `breaker`
field is also stamped on the terminal failure that TRIPS the breaker and on a
cache hit served while the breaker is open — counting those (the old behavior)
over-reports vs the Rust core and Node, corrupting the shared .keel/discovery.db.
"""

from __future__ import annotations

import sqlite3
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

from keel._backend import load_backend
from keel._discovery import (
    SCHEMA_VERSION,
    Discovery,
    _migrate,
    _row_from_outcome,
)

# Column order of the row tuple / discovery table (see _discovery.py).
_BREAKER_OPENS = 8
_FAILURES = 5
_CACHE_HITS = 6
_NOT_RETRIED = 15
_UNWRAPPED_CALLS = 16

# The v1 schema exactly as shipped before the versioned migration, for
# building legacy fixture files.
_V1_SCHEMA = """\
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


class RowFromOutcomeTest(unittest.TestCase):
    def test_e012_fail_fast_counts_as_a_breaker_open(self) -> None:
        row = _row_from_outcome(
            "svc",
            {"result": "error", "breaker": "open", "attempts": 0, "error": {"code": "KEEL-E012", "class": "other"}},
            1,
        )
        self.assertEqual(row[_BREAKER_OPENS], 1)
        self.assertEqual(row[_FAILURES], 1)

    def test_tripping_terminal_failure_is_not_a_breaker_open(self) -> None:
        # The call that trips the breaker has breaker=="open" but code KEEL-E010.
        row = _row_from_outcome(
            "svc",
            {"result": "error", "breaker": "open", "attempts": 3, "error": {"code": "KEEL-E010", "class": "other"}},
            1,
        )
        self.assertEqual(row[_BREAKER_OPENS], 0, "tripping failure must NOT count (canonical rule)")

    def test_cache_hit_served_while_open_is_not_a_breaker_open(self) -> None:
        row = _row_from_outcome("svc", {"result": "ok", "from_cache": True, "breaker": "open"}, 1)
        self.assertEqual(row[_BREAKER_OPENS], 0)
        self.assertEqual(row[_CACHE_HITS], 1)

    def test_closed_success_is_not_a_breaker_open(self) -> None:
        row = _row_from_outcome("svc", {"result": "ok", "breaker": "closed"}, 1)
        self.assertEqual(row[_BREAKER_OPENS], 0)

    def test_e014_counts_as_not_retried(self) -> None:
        row = _row_from_outcome(
            "svc",
            {"result": "error", "attempts": 1, "error": {"code": "KEEL-E014", "class": "other"}},
            1,
        )
        self.assertEqual(row[_NOT_RETRIED], 1)

    def test_other_errors_do_not_count_as_not_retried(self) -> None:
        row = _row_from_outcome(
            "svc",
            {"result": "error", "attempts": 3, "error": {"code": "KEEL-E010", "class": "other"}},
            1,
        )
        self.assertEqual(row[_NOT_RETRIED], 0)

    def test_wrapped_flag_controls_unwrapped_calls(self) -> None:
        wrapped = _row_from_outcome("svc", {"result": "ok"}, 1, wrapped=True)
        unwrapped = _row_from_outcome("svc", {"result": "ok"}, 1, wrapped=False)
        self.assertEqual(wrapped[_UNWRAPPED_CALLS], 0)
        self.assertEqual(unwrapped[_UNWRAPPED_CALLS], 1)


class BreakerOpensEndToEndTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = TemporaryDirectory()
        self.cwd = Path(self._tmp.name)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def test_trip_then_reject_records_one_breaker_open(self) -> None:
        backend = load_backend("stub")
        backend.configure(
            {
                "target": {
                    "svc": {
                        "breaker": {"failures": 1, "cooldown": "60s"},
                        "retry": {"attempts": 1, "on": ["other"], "schedule": "fixed(1ms)"},
                    }
                }
            }
        )
        disc = Discovery(self.cwd)
        req = {"v": 1, "target": "svc", "op": "svc", "idempotent": True, "args_hash": None}

        def boom(_attempt: int) -> dict:
            return {"status": "error", "class": "other", "message": "down"}

        o1 = backend.execute(req, boom)  # terminal failure TRIPS the breaker (E010)
        disc.record("svc", o1, 1)
        o2 = backend.execute(req, boom)  # breaker open → fail-fast (E012)
        disc.record("svc", o2, 1)
        disc.close()

        # Sanity on the two outcomes' codes, then the persisted count.
        self.assertNotEqual(o1["error"]["code"], "KEEL-E012")
        self.assertEqual(o2["error"]["code"], "KEEL-E012")
        self.assertEqual(o1["breaker"], "open")  # both stamp breaker=="open"...

        conn = sqlite3.connect(disc.db_path)
        conn.row_factory = sqlite3.Row
        try:
            row = conn.execute("SELECT breaker_opens FROM discovery WHERE target='svc'").fetchone()
        finally:
            conn.close()
        self.assertEqual(row["breaker_opens"], 1, "...but only the E012 fail-fast is a breaker_open")


def _ok(target: str = "svc") -> dict:
    return {"result": "ok", "attempts": 1}


class KnownTargetsEndToEndTest(unittest.TestCase):
    """`Discovery(cwd, known_targets)` classifies wrapped vs. unwrapped calls
    by exact membership in the effective policy's `[target]` keys — parity
    with the Rust core's `policy.target.contains_key(&request.target)`."""

    def setUp(self) -> None:
        self._tmp = TemporaryDirectory()
        self.cwd = Path(self._tmp.name)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def _row(self, target: str) -> sqlite3.Row:
        conn = sqlite3.connect(self.cwd / ".keel" / "discovery.db")
        conn.row_factory = sqlite3.Row
        try:
            return conn.execute(
                "SELECT * FROM discovery WHERE target = ?", (target,)
            ).fetchone()
        finally:
            conn.close()

    def test_target_with_explicit_policy_entry_is_wrapped(self) -> None:
        disc = Discovery(self.cwd, frozenset({"api.example.com"}))
        disc.record("api.example.com", _ok(), 1)
        disc.close()
        self.assertEqual(self._row("api.example.com")["unwrapped_calls"], 0)

    def test_target_without_a_policy_entry_is_unwrapped(self) -> None:
        disc = Discovery(self.cwd, frozenset({"api.example.com"}))
        disc.record("api.other.com", _ok(), 1)
        disc.close()
        self.assertEqual(self._row("api.other.com")["unwrapped_calls"], 1)

    def test_no_known_targets_means_every_call_is_unwrapped(self) -> None:
        disc = Discovery(self.cwd)
        disc.record("api.example.com", _ok(), 1)
        disc.close()
        self.assertEqual(self._row("api.example.com")["unwrapped_calls"], 1)


class LegacySchemaMigrationTest(unittest.TestCase):
    """A v1 `discovery.db` (no counter columns, no daily table) is migrated in
    place on open, mirroring `keel_journal::discovery`'s versioned migration."""

    def setUp(self) -> None:
        self._tmp = TemporaryDirectory()
        self.cwd = Path(self._tmp.name)
        (self.cwd / ".keel").mkdir()
        self.db_path = self.cwd / ".keel" / "discovery.db"

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def _build_v1_fixture(self) -> None:
        conn = sqlite3.connect(self.db_path)
        try:
            conn.executescript(_V1_SCHEMA)
            conn.execute(
                "INSERT INTO discovery VALUES "
                "('api.old', 10, 12, 2, 8, 2, 0, 1, 0, 500, 90, ?, ?, 'http', 503)",
                (1_783_728_000_000, 1_783_728_001_000),
            )
            conn.commit()
        finally:
            conn.close()

    def test_v1_file_is_migrated_in_place_preserving_rows(self) -> None:
        self._build_v1_fixture()

        disc = Discovery(self.cwd, frozenset({"api.old"}))
        disc.record("api.old", _ok(), 5)
        disc.close()

        conn = sqlite3.connect(self.db_path)
        conn.row_factory = sqlite3.Row
        try:
            (version,) = conn.execute("PRAGMA user_version").fetchone()
            self.assertEqual(version, SCHEMA_VERSION)
            row = conn.execute(
                "SELECT * FROM discovery WHERE target = 'api.old'"
            ).fetchone()
            self.assertEqual(row["calls"], 11, "the old row's count plus the new call")
            self.assertEqual(row["last_error_status"], 503, "legacy data preserved")
            self.assertEqual(row["not_retried"], 0)
            self.assertEqual(row["unwrapped_calls"], 0, "recorded call was wrapped")
            daily = conn.execute("SELECT COUNT(*) FROM discovery_daily").fetchone()[0]
            self.assertEqual(daily, 1, "the daily table now exists and has one bucket")
        finally:
            conn.close()

    def test_migration_is_idempotent_across_reopens(self) -> None:
        self._build_v1_fixture()
        for _ in range(2):
            conn = sqlite3.connect(self.db_path)
            try:
                _migrate(conn)
            finally:
                conn.close()
        conn = sqlite3.connect(self.db_path)
        try:
            cols = conn.execute(
                "SELECT COUNT(*) FROM pragma_table_info('discovery')"
            ).fetchone()[0]
        finally:
            conn.close()
        self.assertEqual(cols, 17, "no duplicate ALTER TABLE columns")


class DailyBucketsTest(unittest.TestCase):
    """Rolling daily buckets make `retries saved this week` a real window
    instead of a lifetime total (dx-spec §6)."""

    def setUp(self) -> None:
        self._tmp = TemporaryDirectory()
        self.cwd = Path(self._tmp.name)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def test_buckets_older_than_retention_are_pruned_when_the_day_advances(self) -> None:
        # Deterministic: drive `_prune` directly against fixed day numbers
        # rather than the wall clock (mirrors the crate's ManualClock tests).
        disc = Discovery(self.cwd)
        conn = disc._connect()
        assert conn is not None
        conn.execute("INSERT INTO discovery_daily (target, day, calls) VALUES ('svc', 100, 1)")
        conn.execute("INSERT INTO discovery_daily (target, day, calls) VALUES ('svc', 200, 1)")
        conn.commit()

        disc._prune(conn, 200)  # day 100 falls outside [200 - 29, 200]
        conn.commit()
        days = [row[0] for row in conn.execute("SELECT day FROM discovery_daily ORDER BY day")]
        disc.close()

        self.assertEqual(days, [200], "the stale bucket at day 100 was pruned")

    def test_prune_runs_at_most_once_per_day(self) -> None:
        disc = Discovery(self.cwd)
        conn = disc._connect()
        assert conn is not None
        conn.execute("INSERT INTO discovery_daily (target, day, calls) VALUES ('svc', 5, 1)")
        conn.commit()

        disc._prune(conn, 200)  # prunes day 5 (outside [171, 200])
        conn.execute("INSERT INTO discovery_daily (target, day, calls) VALUES ('svc', 5, 1)")
        conn.commit()
        disc._prune(conn, 200)  # same day again: a no-op, so day 5 survives
        conn.commit()

        days = {row[0] for row in conn.execute("SELECT day FROM discovery_daily")}
        disc.close()
        self.assertIn(5, days, "a same-day re-check must not re-run the DELETE")


if __name__ == "__main__":
    unittest.main()
