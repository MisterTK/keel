"""psycopg (v3) pack tests against a structural fake of psycopg (psycopg is
not installed in this environment and must never become a repo dependency —
see CLAUDE.md). The fake mirrors just the shapes ``psycopg_pack`` touches:
``Cursor``/``AsyncCursor`` with ``execute``/``executemany``, ``.connection``
carrying ``.info.host``/``.info.port``, and the ``OperationalError`` /
``errors.QueryCanceled`` exception hierarchy.

The design (seam choice, exception classification, non-idempotent-always
judgment) was verified against the REAL psycopg v3 in a throwaway venv during
development; this fake reproduces those same observed shapes."""

from __future__ import annotations

import asyncio
import importlib.machinery
import sqlite3
import sys
import types
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any

from keel import _runtime
from keel._backend import load_backend
from keel._defaults import level0_defaults
from keel._discovery import Discovery
from keel._errors import KeelError


# --- the structural fake ------------------------------------------------------


class _FakeError(Exception):
    pass


class _FakeDatabaseError(_FakeError):
    pass


class _FakeOperationalError(_FakeDatabaseError):
    pass


class _FakeQueryCanceled(_FakeOperationalError):
    pass


class _FakeConnectionInfo:
    def __init__(self, host: str, port: int | None) -> None:
        self.host = host
        self.port = port


class _FakeConnection:
    def __init__(self, host: str = "db.internal", port: int | None = 5432) -> None:
        self.info = _FakeConnectionInfo(host, port)


class _FakeCursor:
    """The seam target: ``execute``/``executemany`` are exactly what
    ``psycopg_pack`` patches on the real ``psycopg.Cursor``."""

    def __init__(self, connection: _FakeConnection, script: list[Any]) -> None:
        self.connection = connection
        self._script = list(script)
        self.calls: list[Any] = []

    def execute(self, query: Any, params: Any = None, **kwargs: Any) -> "_FakeCursor":
        self.calls.append((query, params))
        directive = self._script.pop(0) if self._script else self
        if isinstance(directive, BaseException):
            raise directive
        return self

    def executemany(self, query: Any, params_seq: Any, **kwargs: Any) -> None:
        self.calls.append((query, list(params_seq)))
        directive = self._script.pop(0) if self._script else None
        if isinstance(directive, BaseException):
            raise directive


class _FakeAsyncCursor:
    def __init__(self, connection: _FakeConnection, script: list[Any]) -> None:
        self.connection = connection
        self._script = list(script)
        self.calls: list[Any] = []

    async def execute(self, query: Any, params: Any = None, **kwargs: Any) -> "_FakeAsyncCursor":
        self.calls.append((query, params))
        directive = self._script.pop(0) if self._script else self
        if isinstance(directive, BaseException):
            raise directive
        return self

    async def executemany(self, query: Any, params_seq: Any, **kwargs: Any) -> None:
        self.calls.append((query, list(params_seq)))
        directive = self._script.pop(0) if self._script else None
        if isinstance(directive, BaseException):
            raise directive


def _install_fake_psycopg() -> types.ModuleType:
    root = types.ModuleType("psycopg")
    root.__spec__ = importlib.machinery.ModuleSpec("psycopg", loader=None)
    root.__path__ = []

    errors_mod = types.ModuleType("psycopg.errors")
    errors_mod.QueryCanceled = _FakeQueryCanceled

    root.Cursor = _FakeCursor
    root.AsyncCursor = _FakeAsyncCursor
    root.Error = _FakeError
    root.DatabaseError = _FakeDatabaseError
    root.OperationalError = _FakeOperationalError
    root.errors = errors_mod
    sys.modules["psycopg"] = root
    sys.modules["psycopg.errors"] = errors_mod
    return root


def _uninstall_fake_psycopg() -> None:
    for name in ("psycopg.errors", "psycopg"):
        sys.modules.pop(name, None)


# --- tests ---------------------------------------------------------------------


class PsycopgTestBase(unittest.TestCase):
    def setUp(self) -> None:
        _install_fake_psycopg()
        self.addCleanup(_uninstall_fake_psycopg)
        self._tmp = TemporaryDirectory()
        self.cwd = Path(self._tmp.name)
        self.backend = load_backend("stub")
        self.backend.configure(level0_defaults())
        self.discovery = Discovery(self.cwd)
        _runtime.set_runtime(self.backend, self.discovery)
        from keel.adapters import psycopg_pack

        self.psycopg_pack = psycopg_pack
        psycopg_pack.install()
        self.addCleanup(psycopg_pack.uninstall)

    def tearDown(self) -> None:
        _runtime.clear_runtime()
        self.discovery.close()
        self._tmp.cleanup()

    def cursor(self, script: list[Any], host: str = "db.internal", port: int | None = 5432) -> _FakeCursor:
        return _FakeCursor(_FakeConnection(host, port), script)

    def acursor(self, script: list[Any], host: str = "db.internal", port: int | None = 5432) -> _FakeAsyncCursor:
        return _FakeAsyncCursor(_FakeConnection(host, port), script)

    def rows(self) -> dict[str, sqlite3.Row]:
        self.discovery.close()
        conn = sqlite3.connect(self.discovery.db_path)
        conn.row_factory = sqlite3.Row
        try:
            return {r["target"]: r for r in conn.execute("SELECT * FROM discovery")}
        finally:
            conn.close()


class ContractTest(PsycopgTestBase):
    def test_detect_reports_psycopg_present(self) -> None:
        d = self.psycopg_pack.detect()
        self.assertTrue(d.matched)
        self.assertEqual(d.name, "psycopg")

    def test_seams_targets_defaults(self) -> None:
        seams = self.psycopg_pack.seams()
        self.assertEqual(len(seams), 2)
        self.assertIn("psycopg.Cursor.execute", seams[0].patch_point)
        targets = self.psycopg_pack.targets()
        self.assertEqual(targets[0].pattern, "<db host>[:<port>]")
        self.assertEqual(targets[0].kind, "host")
        self.assertIn("non-idempotent", targets[0].idempotency_rule)
        self.assertEqual(self.psycopg_pack.defaults(), {})

    def test_install_uninstall_reversible(self) -> None:
        import psycopg

        self.assertTrue(getattr(psycopg.Cursor.execute, "__keel_wrapped__", False))
        self.assertTrue(getattr(psycopg.Cursor.executemany, "__keel_wrapped__", False))
        self.assertTrue(getattr(psycopg.AsyncCursor.execute, "__keel_wrapped__", False))
        self.assertTrue(getattr(psycopg.AsyncCursor.executemany, "__keel_wrapped__", False))
        self.psycopg_pack.uninstall()
        self.assertFalse(getattr(psycopg.Cursor.execute, "__keel_wrapped__", False))
        self.assertIs(psycopg.Cursor.execute, _FakeCursor.execute)
        self.psycopg_pack.install()  # restore for addCleanup symmetry


class TargetAndTransparencyTest(PsycopgTestBase):
    def test_target_is_host_port(self) -> None:
        cur = self.cursor([])
        cur.execute("SELECT 1")
        self.assertIn("db.internal:5432", self.rows())

    def test_target_without_port_is_bare_host(self) -> None:
        cur = self.cursor([], host="db.internal", port=None)
        cur.execute("SELECT 1")
        self.assertIn("db.internal", self.rows())

    def test_success_returns_the_cursor_unchanged(self) -> None:
        cur = self.cursor([])
        result = cur.execute("SELECT 1")
        self.assertIs(result, cur)


class NonIdempotentTest(PsycopgTestBase):
    def test_a_transient_looking_error_is_still_observed_not_retried(self) -> None:
        cur = self.cursor([_FakeOperationalError("connection reset")])
        with self.assertRaises(_FakeOperationalError) as ctx:
            cur.execute("INSERT INTO t VALUES (1)")
        self.assertEqual(ctx.exception.keel_outcome["error"]["code"], "KEEL-E014")
        self.assertEqual(ctx.exception.keel_outcome["attempts"], 1)
        self.assertEqual(len(cur.calls), 1, "never retried, even for a connection-shaped error")

    def test_select_is_also_never_retried(self) -> None:
        # The pack deliberately does not verb-sniff (module docs): even a
        # syntactically pure SELECT is observed, not retried.
        cur = self.cursor([_FakeOperationalError("reset")])
        with self.assertRaises(_FakeOperationalError):
            cur.execute("SELECT * FROM t")
        self.assertEqual(len(cur.calls), 1)

    def test_success_path_is_never_cached(self) -> None:
        self.backend.configure({**level0_defaults(), "target": {"db.internal:5432": {"cache": {"ttl": "10s"}}}})
        cur = self.cursor([])
        first = cur.execute("SELECT 1")
        second = cur.execute("SELECT 1")
        self.assertEqual(len(cur.calls), 2, "never served from cache")
        self.assertIs(first, cur)
        self.assertIs(second, cur)


class ErrorClassificationTest(PsycopgTestBase):
    def test_query_canceled_is_classified_timeout(self) -> None:
        cur = self.cursor([_FakeQueryCanceled("canceling statement due to statement timeout")])
        with self.assertRaises(_FakeQueryCanceled) as ctx:
            cur.execute("SELECT pg_sleep(10)")
        self.assertEqual(ctx.exception.keel_outcome["error"]["class"], "timeout")

    def test_operational_error_is_classified_conn(self) -> None:
        cur = self.cursor([_FakeOperationalError("could not connect to server")])
        with self.assertRaises(_FakeOperationalError) as ctx:
            cur.execute("SELECT 1")
        self.assertEqual(ctx.exception.keel_outcome["error"]["class"], "conn")

    def test_original_exception_reraised_unchanged(self) -> None:
        # Unlike the HTTP packs (which may return a live "transient" response
        # object instead of the exception that produced it), psycopg has no
        # "bad but valid" response shape: every failure IS an exception, and
        # _finish always re-raises that exact object (mirrors packs/tool.py) —
        # there is no separate "original" field to round-trip.
        cur = self.cursor([_FakeOperationalError("boom")])
        with self.assertRaises(_FakeOperationalError) as ctx:
            cur.execute("SELECT 1")
        exc = ctx.exception
        self.assertNotIsInstance(exc, KeelError)
        self.assertIn(str(exc), exc.keel_outcome["error"]["message"])
        self.assertEqual(exc.keel_outcome["error"]["class"], "conn")


class ExecuteManyTest(PsycopgTestBase):
    def test_executemany_is_wrapped_and_not_retried(self) -> None:
        cur = self.cursor([])
        cur.executemany("INSERT INTO t VALUES (%s)", [(1,), (2,)])
        self.assertEqual(cur.calls, [("INSERT INTO t VALUES (%s)", [(1,), (2,)])])
        self.assertIn("db.internal:5432", self.rows())


class AsyncCursorTest(PsycopgTestBase):
    def test_async_execute_success_and_error(self) -> None:
        async def go() -> None:
            acur = self.acursor([])
            result = await acur.execute("SELECT 1")
            self.assertIs(result, acur)

            acur2 = self.acursor([_FakeOperationalError("reset")])
            with self.assertRaises(_FakeOperationalError) as ctx:
                await acur2.execute("INSERT INTO t VALUES (1)")
            self.assertEqual(ctx.exception.keel_outcome["attempts"], 1)

        asyncio.run(go())


if __name__ == "__main__":
    unittest.main()
