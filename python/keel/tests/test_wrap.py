"""The wrapper factory: routing a function call through the backend, retrying
per policy, re-raising the ORIGINAL exception unchanged on terminal failure
(DX invariant 5), and recording one discovery row per call.

These are the most load-bearing behaviors of the front end, so they are
exercised directly against the real stub backend and a real SQLite discovery
store (no import machinery in the way)."""

from __future__ import annotations

import sqlite3
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any

from keel import _runtime
from keel._backend import load_backend
from keel._discovery import Discovery
from keel._wrap import wrap_function

_RETRY_OTHER = {"attempts": 3, "on": ["other"], "schedule": "fixed(1ms)"}


class WrapTestBase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = TemporaryDirectory()
        self.cwd = Path(self._tmp.name)

    def tearDown(self) -> None:
        _runtime.clear_runtime()
        self._tmp.cleanup()

    def install(self, policy: dict[str, Any]) -> tuple[Any, Discovery]:
        backend = load_backend("stub")
        backend.configure(policy)
        discovery = Discovery(self.cwd)
        _runtime.set_runtime(backend, discovery)
        return backend, discovery

    def read_rows(self, discovery: Discovery) -> dict[str, sqlite3.Row]:
        discovery.close()  # flush WAL and release before we read
        conn = sqlite3.connect(discovery.db_path)
        conn.row_factory = sqlite3.Row
        try:
            return {r["target"]: r for r in conn.execute("SELECT * FROM discovery")}
        finally:
            conn.close()


class RetryThroughBackendTest(WrapTestBase):
    def test_flaky_function_retried_then_succeeds(self) -> None:
        backend, _ = self.install({"target": {"py:m.flaky": {"retry": _RETRY_OTHER}}})
        calls = {"n": 0}

        def flaky() -> str:
            calls["n"] += 1
            if calls["n"] < 3:
                raise ValueError("transient")
            return f"ok-{calls['n']}"

        wrapped = wrap_function("py:m.flaky", "py:m.flaky", flaky)
        self.assertEqual(wrapped(), "ok-3")  # succeeded on the 3rd attempt
        self.assertEqual(calls["n"], 3)

        stats = backend.report()["targets"]["py:m.flaky"]
        self.assertEqual(stats["attempts"], 3)
        self.assertEqual(stats["retries"], 2)
        self.assertEqual(stats["successes"], 1)
        self.assertEqual(stats["failures"], 0)


class OriginalExceptionIdentityTest(WrapTestBase):
    def test_attempts_exhausted_reraises_original_object_e010(self) -> None:
        self.install({"target": {"py:m.always": {"retry": {**_RETRY_OTHER, "attempts": 2}}}})
        original = ValueError("connection refused")

        def always() -> None:
            raise original  # same object every attempt (class other, retryable)

        wrapped = wrap_function("py:m.always", "py:m.always", always)
        with self.assertRaises(ValueError) as ctx:
            wrapped()
        self.assertIs(ctx.exception, original, "must be the exact original object")
        self.assertEqual(ctx.exception.keel_outcome["error"]["code"], "KEEL-E010")
        self.assertEqual(ctx.exception.keel_outcome["attempts"], 2)

    def test_non_retryable_reraises_original_on_first_attempt_e015(self) -> None:
        # retry.on lists only "timeout"; a raised exception is class "other",
        # so it is non-retryable → E015, re-raised unchanged on attempt 1.
        self.install(
            {"target": {"py:m.bad": {"retry": {"attempts": 5, "on": ["timeout"], "schedule": "fixed(1ms)"}}}}
        )
        original = KeyError("nope")
        calls = {"n": 0}

        def bad() -> None:
            calls["n"] += 1
            raise original

        wrapped = wrap_function("py:m.bad", "py:m.bad", bad)
        with self.assertRaises(KeyError) as ctx:
            wrapped()
        self.assertIs(ctx.exception, original)
        self.assertEqual(calls["n"], 1, "non-retryable: not retried")
        self.assertEqual(ctx.exception.keel_outcome["error"]["code"], "KEEL-E015")
        self.assertEqual(ctx.exception.keel_outcome["attempts"], 1)


class DiscoveryRowsTest(WrapTestBase):
    def test_success_and_failure_rows_written_with_canonical_columns(self) -> None:
        _, discovery = self.install(
            {
                "target": {
                    "py:m.ok": {},
                    "py:m.err": {"retry": {"attempts": 2, "on": ["timeout"], "schedule": "fixed(1ms)"}},
                }
            }
        )

        wrap_function("py:m.ok", "py:m.ok", lambda: 42)()

        def boom() -> None:
            raise RuntimeError("kaboom")

        with self.assertRaises(RuntimeError):
            wrap_function("py:m.err", "py:m.err", boom)()

        rows = self.read_rows(discovery)

        ok = rows["py:m.ok"]
        self.assertEqual(ok["calls"], 1)
        self.assertEqual(ok["successes"], 1)
        self.assertEqual(ok["failures"], 0)
        self.assertEqual(ok["cache_hits"], 0)
        self.assertEqual(ok["attempts"], 1)
        self.assertEqual(ok["retries"], 0)
        self.assertIsNone(ok["last_error_class"])
        self.assertLessEqual(ok["first_seen_ms"], ok["last_seen_ms"])

        err = rows["py:m.err"]
        self.assertEqual(err["calls"], 1)
        self.assertEqual(err["failures"], 1)
        self.assertEqual(err["successes"], 0)
        self.assertEqual(err["last_error_class"], "other")

    def test_repeated_calls_accumulate_in_one_row(self) -> None:
        _, discovery = self.install({"target": {"py:m.ok": {}}})
        w = wrap_function("py:m.ok", "py:m.ok", lambda: "x")
        w()
        w()
        w()
        rows = self.read_rows(discovery)
        self.assertEqual(rows["py:m.ok"]["calls"], 3)
        self.assertEqual(rows["py:m.ok"]["successes"], 3)


if __name__ == "__main__":
    unittest.main()
