"""urllib3 pack integration tests, driven by the local fault server.

Same invariant + resilience matrix as the httpx/requests suites (urllib3 is a
dev dependency here — transitively required by ``requests``), through the
``HTTPConnectionPool.urlopen`` seam, plus the double-wrap guard: a call driven
by the requests pack must be judged (and retried) exactly once, not once by
``requests_pack`` and again by ``urllib3_pack``."""

from __future__ import annotations

import sqlite3
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

import requests
import urllib3

from keel import _runtime
from keel._backend import load_backend
from keel._defaults import level0_defaults
from keel._discovery import Discovery
from keel._errors import KeelError
from keel.adapters import requests_pack, urllib3_pack

from .faultserver import FaultServer, fail, ok, reset, slow, status, throttled

_NON_RETRYABLE_CONN = {
    "target": {"127.0.0.1": {"retry": {"attempts": 3, "on": ["timeout"], "schedule": "fixed(1ms)"}}}
}


def _stable_headers(resp: urllib3.HTTPResponse) -> dict[str, str]:
    return {k.lower(): v for k, v in resp.headers.items() if k.lower() != "date"}


class Urllib3Base(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = TemporaryDirectory()
        self.cwd = Path(self._tmp.name)
        self.backend = load_backend("stub")
        self.backend.configure(level0_defaults())
        self.discovery = Discovery(self.cwd)
        _runtime.set_runtime(self.backend, self.discovery)
        urllib3_pack.install()
        self.pool = urllib3.PoolManager()

    def tearDown(self) -> None:
        self.pool.clear()
        urllib3_pack.uninstall()
        _runtime.clear_runtime()
        self.discovery.close()
        self._tmp.cleanup()

    def rows(self) -> dict[str, sqlite3.Row]:
        self.discovery.close()
        conn = sqlite3.connect(self.discovery.db_path)
        conn.row_factory = sqlite3.Row
        try:
            return {r["target"]: r for r in conn.execute("SELECT * FROM discovery")}
        finally:
            conn.close()


class TransparencyTest(Urllib3Base):
    def test_success_path_is_byte_identical_to_unwrapped(self) -> None:
        body = b"payload-\x00\xff-bytes"
        headers = {"X-Custom": "v1", "Content-Type": "application/octet-stream"}
        with FaultServer([ok(body, headers), ok(body, headers)]) as srv:
            urllib3_pack.uninstall()  # control: real, unwrapped urllib3
            control = self.pool.urlopen("GET", srv.url("/p"))
            urllib3_pack.install()
            got = self.pool.urlopen("GET", srv.url("/p"))
        self.assertEqual(got.status, control.status)
        self.assertEqual(bytes(got.data), bytes(control.data))
        self.assertEqual(_stable_headers(got), _stable_headers(control))
        self.assertEqual(got.keel_outcome["result"], "ok")  # the real object, tagged


class ResilienceTest(Urllib3Base):
    """``retries=False`` disables urllib3's OWN default retry (which — unlike
    httpx/requests — silently retries connect/read errors, and 429/503 with a
    ``Retry-After`` header, internally per its default ``Retry(total=3)``; see
    ``urllib3_pack`` module docs) so these tests isolate the Keel retry layer
    the seam drives, exactly like the httpx/requests sibling suites."""

    def test_5xx_then_ok_is_retried(self) -> None:
        with FaultServer([fail(503), ok(b"recovered")]) as srv:
            r = self.pool.urlopen("GET", srv.url(), retries=False)
        self.assertEqual(r.status, 200)
        self.assertEqual(r.data, b"recovered")
        self.assertEqual(r.keel_outcome["attempts"], 2)

    def test_429_retry_after_seconds_governs_backoff(self) -> None:
        with FaultServer([throttled("1"), ok(b"ok")]) as srv:
            r = self.pool.urlopen("GET", srv.url(), retries=False)
        self.assertEqual(r.status, 200)
        self.assertEqual(r.keel_outcome["attempts"], 2)
        # Level 0 backoff is 200ms; Retry-After 1s wins: max(200, 1000) = 1000.
        self.assertEqual(r.keel_outcome["waits_ms"], [1000])

    def test_connection_reset_is_retried(self) -> None:
        with FaultServer([reset(), ok(b"after-reset")]) as srv:
            r = self.pool.urlopen("GET", srv.url(), retries=False)
        self.assertEqual(r.status, 200)
        self.assertEqual(r.data, b"after-reset")
        self.assertEqual(r.keel_outcome["attempts"], 2)

    def test_timeout_is_retried(self) -> None:
        with FaultServer([slow(0.3, ok(b"slow")), ok(b"fast")]) as srv:
            r = self.pool.urlopen("GET", srv.url(), timeout=0.1, retries=False)
        self.assertEqual(r.status, 200)
        self.assertEqual(r.data, b"fast")
        self.assertEqual(r.keel_outcome["attempts"], 2)

    def test_non_429_4xx_passes_through_unretried(self) -> None:
        with FaultServer([status(404, b"missing")]) as srv:
            r = self.pool.urlopen("GET", srv.url(), retries=False)
        self.assertEqual(r.status, 404)
        self.assertEqual(r.data, b"missing")
        self.assertEqual(r.keel_outcome["result"], "ok")  # 4xx is the program's business
        self.assertEqual(r.keel_outcome["attempts"], 1)

    def test_urllib3_own_default_retry_absorbs_a_transient_fault_below_keel(self) -> None:
        """Documents the compounding-retry interaction (module docs): with
        urllib3's DEFAULT retries left on, a connect/read fault can be
        resolved by urllib3 itself before Keel's own retry layer ever sees a
        failure — Keel still reports the call as one successful attempt."""
        with FaultServer([reset(), ok(b"absorbed-by-urllib3")]) as srv:
            r = self.pool.urlopen("GET", srv.url())  # default retries: NOT disabled
        self.assertEqual(r.status, 200)
        self.assertEqual(r.data, b"absorbed-by-urllib3")
        self.assertEqual(r.keel_outcome["attempts"], 1)


class HardRulesTest(Urllib3Base):
    def test_post_without_key_is_observed_not_retried(self) -> None:
        with FaultServer([fail(503), ok(b"unreached")]) as srv:
            r = self.pool.urlopen("POST", srv.url(), body=b"body")
        self.assertEqual(r.status, 503)  # last real response, unchanged
        self.assertEqual(r.keel_outcome["error"]["code"], "KEEL-E014")
        self.assertEqual(r.keel_outcome["attempts"], 1)

    def test_post_with_idempotency_key_is_retried(self) -> None:
        with FaultServer([fail(503), ok(b"posted")]) as srv:
            r = self.pool.urlopen("POST", srv.url(), body=b"body", headers={"Idempotency-Key": "abc"})
        self.assertEqual(r.status, 200)
        self.assertEqual(r.keel_outcome["attempts"], 2)

    def test_original_transport_exception_reraised_unchanged(self) -> None:
        self.backend.configure(_NON_RETRYABLE_CONN)  # conn not in retry.on -> E015
        with FaultServer([reset()]) as srv:
            with self.assertRaises(urllib3.exceptions.HTTPError) as ctx:
                self.pool.urlopen("GET", srv.url(), retries=False)
            exc = ctx.exception
            self.assertNotIsInstance(exc, KeelError)  # the real error, not a synthesized one
            self.assertEqual(exc.keel_outcome["error"]["code"], "KEEL-E015")
            self.assertIs(exc.keel_outcome["error"]["original"], exc)  # exact identity


class DirectPoolTest(Urllib3Base):
    def test_bare_connection_pool_is_wrapped_too(self) -> None:
        with FaultServer([fail(503), ok(b"ok-via-pool")]) as srv:
            pool = urllib3.HTTPConnectionPool("127.0.0.1", srv.port)
            try:
                r = pool.urlopen("GET", "/")
            finally:
                pool.close()
        self.assertEqual(r.status, 200)
        self.assertEqual(r.data, b"ok-via-pool")
        self.assertEqual(r.keel_outcome["attempts"], 2)


class CacheReplayTest(Urllib3Base):
    def test_cache_hit_rebuilds_a_real_http_response(self) -> None:
        self.backend.configure({**level0_defaults(), "target": {"127.0.0.1": {"cache": {"ttl": "10s"}}}})
        with FaultServer([ok(b'{"a":1}', {"Content-Type": "application/json"})]) as srv:
            first = self.pool.urlopen("GET", srv.url("/x"))
            second = self.pool.urlopen("GET", srv.url("/x"))
        self.assertFalse(first.keel_outcome["from_cache"])
        self.assertTrue(second.keel_outcome["from_cache"])
        self.assertIsInstance(second, urllib3.HTTPResponse)
        self.assertEqual(second.data, b'{"a":1}')
        self.assertEqual(second.status, 200)


class DoubleWrapGuardTest(unittest.TestCase):
    """requests vendors its own urllib3 pools; a requests call must be judged
    (and retried) exactly once — not once by requests_pack and again by
    urllib3_pack."""

    def setUp(self) -> None:
        self._tmp = TemporaryDirectory()
        self.cwd = Path(self._tmp.name)
        self.backend = load_backend("stub")
        self.backend.configure(level0_defaults())
        self.discovery = Discovery(self.cwd)
        _runtime.set_runtime(self.backend, self.discovery)
        requests_pack.install()
        urllib3_pack.install()

    def tearDown(self) -> None:
        requests_pack.uninstall()
        urllib3_pack.uninstall()
        _runtime.clear_runtime()
        self.discovery.close()
        self._tmp.cleanup()

    def _rows(self) -> dict[str, sqlite3.Row]:
        self.discovery.close()
        conn = sqlite3.connect(self.discovery.db_path)
        conn.row_factory = sqlite3.Row
        try:
            return {r["target"]: r for r in conn.execute("SELECT * FROM discovery")}
        finally:
            conn.close()

    def test_requests_call_is_not_double_retried(self) -> None:
        with FaultServer([fail(503), ok(b"recovered")]) as srv:
            with requests.Session() as s:
                r = s.get(srv.url())
        self.assertEqual(r.status_code, 200)
        self.assertEqual(r.content, b"recovered")
        # ONE Keel retry loop: 2 attempts total, not 4 (2 requests-level x 2
        # urllib3-level would mean the guard failed).
        self.assertEqual(r.keel_outcome["attempts"], 2)
        row = self._rows()["127.0.0.1"]
        self.assertEqual(row["calls"], 1, "urllib3_pack must not record a second call for the same request")
        self.assertEqual(row["attempts"], 2)


if __name__ == "__main__":
    unittest.main()
