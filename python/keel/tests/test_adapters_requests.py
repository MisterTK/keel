"""requests pack integration tests, driven by the local fault server.

Same invariant + resilience matrix as the httpx suite, through the
``HTTPAdapter.send`` seam and a real ``requests.Session``."""

from __future__ import annotations

import gzip
import sqlite3
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

import requests

from keel import _runtime
from keel._backend import load_backend
from keel._defaults import level0_defaults
from keel._discovery import Discovery
from keel._errors import KeelError
from keel.adapters import requests_pack

from .faultserver import FaultServer, fail, ok, reset, slow, sse, status, throttled

_NON_RETRYABLE_CONN = {
    "target": {"127.0.0.1": {"retry": {"attempts": 3, "on": ["timeout"], "schedule": "fixed(1ms)"}}}
}


def _stable_headers(resp: requests.Response) -> dict[str, str]:
    return {k.lower(): v for k, v in resp.headers.items() if k.lower() != "date"}


class RequestsBase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = TemporaryDirectory()
        self.cwd = Path(self._tmp.name)
        self.backend = load_backend("stub")
        self.backend.configure(level0_defaults())
        self.discovery = Discovery(self.cwd)
        _runtime.set_runtime(self.backend, self.discovery)
        requests_pack.install()

    def tearDown(self) -> None:
        requests_pack.uninstall()
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


class TransparencyTest(RequestsBase):
    def test_success_path_is_byte_identical_to_unwrapped(self) -> None:
        body = b"payload-\x00\xff-bytes"
        headers = {"X-Custom": "v1", "Content-Type": "application/octet-stream"}
        with FaultServer([ok(body, headers), ok(body, headers)]) as srv:
            requests_pack.uninstall()  # control: real, unwrapped requests
            with requests.Session() as s:
                control = s.get(srv.url("/p"))
            requests_pack.install()
            with requests.Session() as s:
                got = s.get(srv.url("/p"))
        self.assertEqual(got.status_code, control.status_code)
        self.assertEqual(got.content, control.content)
        self.assertEqual(_stable_headers(got), _stable_headers(control))
        self.assertEqual(got.keel_outcome["result"], "ok")


class ResilienceTest(RequestsBase):
    def test_5xx_then_ok_is_retried(self) -> None:
        with FaultServer([fail(503), ok(b"recovered")]) as srv:
            with requests.Session() as s:
                r = s.get(srv.url())
            self.assertEqual(r.status_code, 200)
            self.assertEqual(r.content, b"recovered")
            self.assertEqual(r.keel_outcome["attempts"], 2)
            self.assertEqual(srv.served, 2)

    def test_429_retry_after_seconds_governs_backoff(self) -> None:
        with FaultServer([throttled("1"), ok(b"ok")]) as srv:
            with requests.Session() as s:
                r = s.get(srv.url())
        self.assertEqual(r.status_code, 200)
        self.assertEqual(r.keel_outcome["waits_ms"], [1000])

    def test_connection_reset_is_retried(self) -> None:
        with FaultServer([reset(), ok(b"after-reset")]) as srv:
            with requests.Session() as s:
                r = s.get(srv.url())
        self.assertEqual(r.status_code, 200)
        self.assertEqual(r.content, b"after-reset")
        self.assertEqual(r.keel_outcome["attempts"], 2)

    def test_timeout_is_retried(self) -> None:
        with FaultServer([slow(0.3, ok(b"slow")), ok(b"fast")]) as srv:
            with requests.Session() as s:
                r = s.get(srv.url(), timeout=0.1)
        self.assertEqual(r.status_code, 200)
        self.assertEqual(r.content, b"fast")
        self.assertEqual(r.keel_outcome["attempts"], 2)

    def test_policy_timeout_wins_when_tighter(self) -> None:
        # issue #32: a configured policy `timeout` was completely inert for
        # sync callers — requests' `timeout` is a `send()` kwarg, not part of
        # the request object, and nothing overrode it with the resolved
        # policy value. Policy 100ms beats a loose caller timeout of 5s:
        # attempt 1 must abort client-side in ~100ms (well under the 400ms
        # delayed response), then attempt 2 succeeds fast.
        self.backend.configure(
            {**level0_defaults(), "target": {"127.0.0.1": {"timeout": "100ms"}}}
        )
        with FaultServer([slow(0.4, ok(b"slow")), ok(b"fast")]) as srv:
            with requests.Session() as s:
                r = s.get(srv.url(), timeout=5)
        self.assertEqual(r.status_code, 200)
        self.assertEqual(r.content, b"fast")
        self.assertEqual(r.keel_outcome["attempts"], 2)

    def test_non_429_4xx_passes_through_unretried(self) -> None:
        with FaultServer([status(404, b"missing")]) as srv:
            with requests.Session() as s:
                r = s.get(srv.url())
            self.assertEqual(r.status_code, 404)
            self.assertEqual(r.content, b"missing")
            self.assertEqual(r.keel_outcome["result"], "ok")
            self.assertEqual(srv.served, 1)


class HardRulesTest(RequestsBase):
    def test_post_without_key_is_observed_not_retried(self) -> None:
        with FaultServer([fail(503), ok(b"unreached")]) as srv:
            with requests.Session() as s:
                r = s.post(srv.url(), data=b"body")
            self.assertEqual(r.status_code, 503)
            self.assertEqual(r.keel_outcome["error"]["code"], "KEEL-E014")
            self.assertEqual(r.keel_outcome["attempts"], 1)
            self.assertEqual(srv.served, 1)

    def test_post_with_idempotency_key_is_retried(self) -> None:
        with FaultServer([fail(503), ok(b"posted")]) as srv:
            with requests.Session() as s:
                r = s.post(srv.url(), data=b"body", headers={"Idempotency-Key": "abc"})
            self.assertEqual(r.status_code, 200)
            self.assertEqual(r.keel_outcome["attempts"], 2)
            self.assertEqual(srv.served, 2)

    def test_original_transport_exception_reraised_unchanged(self) -> None:
        self.backend.configure(_NON_RETRYABLE_CONN)
        with FaultServer([reset()]) as srv:
            with requests.Session() as s:
                with self.assertRaises(requests.exceptions.RequestException) as ctx:
                    s.get(srv.url())
            exc = ctx.exception
            self.assertIsInstance(exc, requests.exceptions.ConnectionError)
            self.assertNotIsInstance(exc, KeelError)
            self.assertEqual(exc.keel_outcome["error"]["code"], "KEEL-E015")
            self.assertIs(exc.keel_outcome["error"]["original"], exc)
            self.assertEqual(srv.served, 1)


class IdempotencyInjectionTest(RequestsBase):
    """contracts/adapter-pack.md "Idempotency-key injection", exercised through
    a real ``requests.Session`` + a local server (no external network)."""

    def _configure_injection(self) -> None:
        policy = level0_defaults()
        policy["target"] = {"127.0.0.1": {"idempotency": {"header": "Idempotency-Key"}}}
        self.backend.configure(policy)

    def test_post_without_a_caller_key_is_injected_and_retried(self) -> None:
        self._configure_injection()
        with FaultServer([fail(503), ok(b"posted")]) as srv:
            with requests.Session() as s:
                r = s.post(srv.url(), data=b"body")
            self.assertEqual(r.status_code, 200)  # judgment flip: retried to success
            self.assertEqual(r.keel_outcome["attempts"], 2)
            self.assertEqual(srv.served, 2)
            keys = [h.get("idempotency-key") for h in srv.headers]
            self.assertEqual(len(keys), 2)
            self.assertIsNotNone(keys[0])
            self.assertEqual(keys[0], keys[1])  # rule 2: stable across retries

    def test_caller_supplied_key_is_never_overwritten(self) -> None:
        self._configure_injection()
        with FaultServer([fail(503), ok(b"posted")]) as srv:
            with requests.Session() as s:
                r = s.post(srv.url(), data=b"body", headers={"Idempotency-Key": "caller-key"})
            self.assertEqual(r.status_code, 200)
            self.assertEqual(srv.headers[0]["idempotency-key"], "caller-key")
            self.assertEqual(srv.headers[1]["idempotency-key"], "caller-key")

    def test_two_logical_calls_mint_distinct_keys(self) -> None:
        self._configure_injection()
        with FaultServer([ok(b"one"), ok(b"two")]) as srv:
            with requests.Session() as s:
                s.post(srv.url(), data=b"a")
                s.post(srv.url(), data=b"b")
        self.assertNotEqual(srv.headers[0]["idempotency-key"], srv.headers[1]["idempotency-key"])

    def test_no_configured_header_means_no_injection_post_still_not_retried(self) -> None:
        with FaultServer([fail(503), ok(b"unreached")]) as srv:
            with requests.Session() as s:
                r = s.post(srv.url(), data=b"body")
            self.assertEqual(r.keel_outcome["error"]["code"], "KEEL-E014")
            self.assertEqual(srv.served, 1)
            self.assertNotIn("idempotency-key", srv.headers[0])
            self.assertEqual(srv.served, 1)


class CacheReplayTest(RequestsBase):
    """No cache-hit gzip test existed for requests; issue #66 (the httpx-#60
    sibling bug: a rebuilt response kept declaring the wire encoding of a
    body that is already decoded)."""

    _CACHE = {"target": {"127.0.0.1": {"cache": {"ttl": "10s"}}}}

    def test_gzip_response_replays_from_cache_intact(self) -> None:
        self.backend.configure({**level0_defaults(), **self._CACHE})
        body = gzip.compress(b'{"a":1}')
        with FaultServer(
            [ok(body, {"Content-Type": "application/json", "Content-Encoding": "gzip"})]
        ) as srv:
            first = requests.get(srv.url("/gen"))
            self.assertFalse(first.keel_outcome["from_cache"])
            self.assertEqual(first.json(), {"a": 1})  # byte-transparency: live call
            second = requests.get(srv.url("/gen"))
            self.assertTrue(second.keel_outcome["from_cache"])
            self.assertEqual(second.status_code, 200)
            self.assertEqual(second.json(), {"a": 1})
            self.assertNotIn("Content-Encoding", second.headers)
            self.assertEqual(second.headers["Content-Type"], "application/json")
        self.assertEqual(srv.served, 1)


class StreamingResponseTest(RequestsBase):
    """Issue #84: with the buffer gate ON (a cache ttl on the target), a
    `text/event-stream` response is still never read at the seam — touching
    `resp.content` drains the urllib3 stream the caller is about to iterate,
    and a live SSE session may never end at all."""

    _CACHE = {"target": {"127.0.0.1": {"cache": {"ttl": "10s"}}}}
    _EVENTS = [b"data: one\n\n", b"data: two\n\n", b"data: three\n\n"]

    def test_sse_body_is_not_read_at_the_seam(self) -> None:
        self.backend.configure({**level0_defaults(), **self._CACHE})
        with FaultServer([sse(self._EVENTS)]) as srv:
            with requests.Session() as s:
                r = s.get(srv.url("/sse"), stream=True)
                self.assertEqual(r.headers["Content-Type"], "text/event-stream")
                self.assertFalse(r._content_consumed)  # the pack left the stream alone
                self.assertNotIn("body_b64", r.keel_outcome["payload"])
                data = [ln.decode() for ln in r.iter_lines() if ln.startswith(b"data:")]
                r.close()
        self.assertEqual(data, ["data: one", "data: two", "data: three"])
        self.assertEqual(srv.served, 1)

    def test_non_streaming_body_is_still_buffered_at_the_seam(self) -> None:
        self.backend.configure({**level0_defaults(), **self._CACHE})
        with FaultServer([ok(b'{"a":1}', {"Content-Type": "application/json"})]) as srv:
            with requests.Session() as s:
                r = s.get(srv.url("/j"), stream=True)
                self.assertTrue(r._content_consumed)  # buffered by the pack
                self.assertIn("body_b64", r.keel_outcome["payload"])
                self.assertEqual(r.json(), {"a": 1})
        self.assertEqual(srv.served, 1)


class DiscoveryTest(RequestsBase):
    def test_canonical_row_written_for_http_target(self) -> None:
        with FaultServer([fail(503), ok(b"ok")]) as srv:
            with requests.Session() as s:
                s.get(srv.url())
        row = self.rows()["127.0.0.1"]
        self.assertEqual(row["calls"], 1)
        self.assertEqual(row["successes"], 1)
        self.assertEqual(row["attempts"], 2)
        self.assertEqual(row["retries"], 1)


if __name__ == "__main__":
    unittest.main()
