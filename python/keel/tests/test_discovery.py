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
from keel._discovery import Discovery, _row_from_outcome

# Column order of the row tuple / discovery table (see _discovery.py).
_BREAKER_OPENS = 8
_FAILURES = 5
_CACHE_HITS = 6


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


if __name__ == "__main__":
    unittest.main()
