"""httpx pack integration tests, driven by the local fault server.

Covers both the load-bearing DX invariants (success-path byte-transparency,
original-exception identity, POST-not-retried) and the resilience matrix
(5xx-then-ok, 429 + Retry-After, connection reset, timeout) for the sync and
async transports, plus custom-transport coverage via the client-init seam and a
canonical discovery row."""

from __future__ import annotations

import asyncio
import sqlite3
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

import httpx

from keel import _runtime
from keel._backend import load_backend
from keel._defaults import level0_defaults
from keel._discovery import Discovery
from keel._errors import KeelError
from keel.adapters import httpx_pack

from .faultserver import FaultServer, fail, ok, reset, slow, status, throttled

_NON_RETRYABLE_CONN = {
    "target": {"127.0.0.1": {"retry": {"attempts": 3, "on": ["timeout"], "schedule": "fixed(1ms)"}}}
}


def _stable_headers(resp: httpx.Response) -> dict[str, str]:
    return {k.lower(): v for k, v in resp.headers.items() if k.lower() != "date"}


class HttpxBase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = TemporaryDirectory()
        self.cwd = Path(self._tmp.name)
        self.backend = load_backend("stub")
        self.backend.configure(level0_defaults())
        self.discovery = Discovery(self.cwd)
        _runtime.set_runtime(self.backend, self.discovery)
        httpx_pack.install()

    def tearDown(self) -> None:
        httpx_pack.uninstall()
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


class SyncTransparencyTest(HttpxBase):
    def test_success_path_is_byte_identical_to_unwrapped(self) -> None:
        body = b"payload-\x00\xff-bytes"
        headers = {"X-Custom": "v1", "Content-Type": "application/octet-stream"}
        with FaultServer([ok(body, headers), ok(body, headers)]) as srv:
            httpx_pack.uninstall()  # control: real, unwrapped httpx
            with httpx.Client() as c:
                control = c.get(srv.url("/p"))
            httpx_pack.install()
            with httpx.Client() as c:
                got = c.get(srv.url("/p"))
        self.assertEqual(got.status_code, control.status_code)
        self.assertEqual(bytes(got.content), bytes(control.content))
        self.assertEqual(_stable_headers(got), _stable_headers(control))
        self.assertEqual(got.keel_outcome["result"], "ok")  # the real object, tagged


class SyncResilienceTest(HttpxBase):
    def test_5xx_then_ok_is_retried(self) -> None:
        with FaultServer([fail(503), ok(b"recovered")]) as srv:
            with httpx.Client() as c:
                r = c.get(srv.url())
            self.assertEqual(r.status_code, 200)
            self.assertEqual(r.content, b"recovered")
            self.assertEqual(r.keel_outcome["attempts"], 2)
            self.assertEqual(srv.served, 2)

    def test_429_retry_after_seconds_governs_backoff(self) -> None:
        with FaultServer([throttled("1"), ok(b"ok")]) as srv:
            with httpx.Client() as c:
                r = c.get(srv.url())
        self.assertEqual(r.status_code, 200)
        self.assertEqual(r.keel_outcome["attempts"], 2)
        # Level 0 backoff is 200ms; Retry-After 1s wins: max(200, 1000) = 1000.
        self.assertEqual(r.keel_outcome["waits_ms"], [1000])

    def test_connection_reset_is_retried(self) -> None:
        with FaultServer([reset(), ok(b"after-reset")]) as srv:
            with httpx.Client() as c:
                r = c.get(srv.url())
        self.assertEqual(r.status_code, 200)
        self.assertEqual(r.content, b"after-reset")
        self.assertEqual(r.keel_outcome["attempts"], 2)

    def test_timeout_is_retried(self) -> None:
        with FaultServer([slow(0.3, ok(b"slow")), ok(b"fast")]) as srv:
            with httpx.Client(timeout=0.1) as c:
                r = c.get(srv.url())
        self.assertEqual(r.status_code, 200)
        self.assertEqual(r.content, b"fast")
        self.assertEqual(r.keel_outcome["attempts"], 2)

    def test_non_429_4xx_passes_through_unretried(self) -> None:
        with FaultServer([status(404, b"missing")]) as srv:
            with httpx.Client() as c:
                r = c.get(srv.url())
            self.assertEqual(r.status_code, 404)
            self.assertEqual(r.content, b"missing")
            self.assertEqual(r.keel_outcome["result"], "ok")  # 4xx is the program's business
            self.assertEqual(r.keel_outcome["attempts"], 1)
            self.assertEqual(srv.served, 1)


class SyncHardRulesTest(HttpxBase):
    def test_post_without_key_is_observed_not_retried(self) -> None:
        with FaultServer([fail(503), ok(b"unreached")]) as srv:
            with httpx.Client() as c:
                r = c.post(srv.url(), content=b"body")
            self.assertEqual(r.status_code, 503)  # last real response, unchanged
            self.assertEqual(r.keel_outcome["error"]["code"], "KEEL-E014")
            self.assertEqual(r.keel_outcome["attempts"], 1)
            self.assertEqual(srv.served, 1)  # NOT retried

    def test_post_with_idempotency_key_is_retried(self) -> None:
        with FaultServer([fail(503), ok(b"posted")]) as srv:
            with httpx.Client() as c:
                r = c.post(srv.url(), content=b"body", headers={"Idempotency-Key": "abc"})
            self.assertEqual(r.status_code, 200)
            self.assertEqual(r.keel_outcome["attempts"], 2)
            self.assertEqual(srv.served, 2)

    def test_original_transport_exception_reraised_unchanged(self) -> None:
        self.backend.configure(_NON_RETRYABLE_CONN)  # conn not in retry.on → E015
        with FaultServer([reset()]) as srv:
            with httpx.Client() as c:
                with self.assertRaises(httpx.HTTPError) as ctx:
                    c.get(srv.url())
            exc = ctx.exception
            self.assertIsInstance(exc, httpx.TransportError)
            self.assertNotIsInstance(exc, KeelError)  # the real error, not a synthesized one
            self.assertEqual(exc.keel_outcome["error"]["code"], "KEEL-E015")
            self.assertIs(exc.keel_outcome["error"]["original"], exc)  # exact identity
            self.assertEqual(srv.served, 1)


class AsyncTest(HttpxBase):
    def test_async_success_is_byte_transparent(self) -> None:
        with FaultServer([ok(b"async-body")]) as srv:
            async def go() -> httpx.Response:
                async with httpx.AsyncClient() as c:
                    return await c.get(srv.url())

            r = asyncio.run(go())
        self.assertEqual(r.status_code, 200)
        self.assertEqual(r.content, b"async-body")
        self.assertEqual(r.keel_outcome["result"], "ok")

    def test_async_5xx_then_ok_is_retried(self) -> None:
        with FaultServer([fail(503), ok(b"async-ok")]) as srv:
            async def go() -> httpx.Response:
                async with httpx.AsyncClient() as c:
                    return await c.get(srv.url())

            r = asyncio.run(go())
        self.assertEqual(r.status_code, 200)
        self.assertEqual(r.content, b"async-ok")
        self.assertEqual(r.keel_outcome["attempts"], 2)

    def test_async_post_not_retried(self) -> None:
        with FaultServer([fail(503), ok(b"unreached")]) as srv:
            async def go() -> httpx.Response:
                async with httpx.AsyncClient() as c:
                    return await c.post(srv.url(), content=b"x")

            r = asyncio.run(go())
            self.assertEqual(r.status_code, 503)
            self.assertEqual(r.keel_outcome["error"]["code"], "KEEL-E014")
            self.assertEqual(srv.served, 1)


class CustomTransportTest(HttpxBase):
    def test_client_init_wraps_a_custom_transport(self) -> None:
        calls = {"n": 0}

        def handler(request: httpx.Request) -> httpx.Response:
            calls["n"] += 1
            return httpx.Response(503, content=b"first") if calls["n"] == 1 else httpx.Response(200, content=b"second")

        transport = httpx.MockTransport(handler)
        with httpx.Client(transport=transport) as c:
            r = c.get("http://any.host/x")
        self.assertEqual(r.status_code, 200)
        self.assertEqual(r.content, b"second")
        self.assertEqual(calls["n"], 2)  # retried through the custom transport
        self.assertTrue(getattr(transport.handle_request, "__keel_wrapped__", False))


class DiscoveryTest(HttpxBase):
    def test_canonical_row_written_for_http_target(self) -> None:
        with FaultServer([fail(503), ok(b"ok")]) as srv:
            with httpx.Client() as c:
                c.get(srv.url())
        row = self.rows()["127.0.0.1"]
        self.assertEqual(row["calls"], 1)
        self.assertEqual(row["successes"], 1)
        self.assertEqual(row["failures"], 0)
        self.assertEqual(row["attempts"], 2)
        self.assertEqual(row["retries"], 1)


if __name__ == "__main__":
    unittest.main()
