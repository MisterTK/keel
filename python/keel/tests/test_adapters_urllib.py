"""urllib.request pack integration tests, driven by the local fault server.

Same invariant + resilience matrix as the httpx/requests/urllib3 suites,
through the ``OpenerDirector.open`` seam. The urllib-specific rows: urllib
RAISES ``HTTPError`` for >=400 responses instead of returning them, so the
suite pins that ``except HTTPError`` semantics survive wrapping exactly —
non-transient errors raise unretried, transient ones raise the ORIGINAL
``HTTPError`` after retries exhaust, and a cache hit on a 4xx re-raises a
rebuilt ``HTTPError``. Plus the seam-coverage rows (held ``urlopen``
reference, ``build_opener``, ``install_opener``), the redirect re-entrancy
guard (one Keel call for the whole redirect chain), and tighter-wins timeout
composition in both directions."""

from __future__ import annotations

import http.client
import sqlite3
import unittest
import urllib.error
import urllib.request
from pathlib import Path
from tempfile import TemporaryDirectory

# Held BEFORE any install(): proves the OpenerDirector seam catches references
# captured at import time (one level deeper than patching the `urlopen` name).
from urllib.request import urlopen as held_urlopen

from keel import _runtime
from keel._backend import load_backend
from keel._defaults import level0_defaults
from keel._errors import KeelError
from keel._discovery import Discovery
from keel.adapters import urllib_pack

from .faultserver import FaultServer, fail, ok, reset, slow, status, throttled

_NON_RETRYABLE_CONN = {
    "target": {"127.0.0.1": {"retry": {"attempts": 3, "on": ["timeout"], "schedule": "fixed(1ms)"}}}
}


def _stable_headers(resp) -> dict[str, str]:
    return {k.lower(): v for k, v in resp.headers.items() if k.lower() != "date"}


class UrllibBase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = TemporaryDirectory()
        self.cwd = Path(self._tmp.name)
        self.backend = load_backend("stub")
        self.backend.configure(level0_defaults())
        self.discovery = Discovery(self.cwd)
        _runtime.set_runtime(self.backend, self.discovery)
        urllib_pack.install()

    def tearDown(self) -> None:
        urllib_pack.uninstall()
        _runtime.clear_runtime()
        self.discovery.close()
        self._tmp.cleanup()

    def rows(self) -> dict[str, sqlite3.Row]:
        self.discovery.close()
        if not self.discovery.db_path.exists():
            # Discovery opens its sqlite file lazily on the first `record()`;
            # a test that never drives a judged call (e.g. the non-HTTP-
            # scheme passthrough) leaves the `.keel` dir never created.
            return {}
        conn = sqlite3.connect(self.discovery.db_path)
        conn.row_factory = sqlite3.Row
        try:
            return {r["target"]: r for r in conn.execute("SELECT * FROM discovery")}
        finally:
            conn.close()


class TransparencyTest(UrllibBase):
    def test_success_path_is_byte_identical_to_unwrapped(self) -> None:
        body = b"payload-\x00\xff-bytes"
        headers = {"X-Custom": "v1", "Content-Type": "application/octet-stream"}
        with FaultServer([ok(body, headers), ok(body, headers)]) as srv:
            urllib_pack.uninstall()  # control: real, unwrapped urllib
            with urllib.request.urlopen(srv.url("/p")) as control:
                control_body = control.read()
                control_headers = _stable_headers(control)
                control_status = control.status
            urllib_pack.install()
            with urllib.request.urlopen(srv.url("/p")) as got:
                got_body = got.read()
                got_headers = _stable_headers(got)
                got_status = got.status
                self.assertIsInstance(got, http.client.HTTPResponse)  # the REAL object
                self.assertEqual(got.keel_outcome["result"], "ok")
        self.assertEqual(got_status, control_status)
        self.assertEqual(got_body, control_body)
        self.assertEqual(got_headers, control_headers)

    def test_held_urlopen_reference_is_intercepted(self) -> None:
        # `held_urlopen` was imported at module load, before install() — the
        # OpenerDirector.open seam still sees it.
        with FaultServer([fail(503), ok(b"held")]) as srv:
            with held_urlopen(srv.url()) as r:
                self.assertEqual(r.read(), b"held")
                self.assertEqual(r.keel_outcome["attempts"], 2)

    def test_build_opener_and_install_opener_are_intercepted(self) -> None:
        with FaultServer([fail(503), ok(b"opener"), fail(503), ok(b"installed")]) as srv:
            opener = urllib.request.build_opener()
            with opener.open(srv.url()) as r:
                self.assertEqual(r.read(), b"opener")
                self.assertEqual(r.keel_outcome["attempts"], 2)
            urllib.request.install_opener(urllib.request.build_opener())
            try:
                with urllib.request.urlopen(srv.url()) as r2:
                    self.assertEqual(r2.read(), b"installed")
                    self.assertEqual(r2.keel_outcome["attempts"], 2)
            finally:
                urllib.request.install_opener(None)  # restore default resolution

    def test_non_http_scheme_passes_through_unjudged(self) -> None:
        path = self.cwd / "f.txt"
        path.write_bytes(b"local")
        with urllib.request.urlopen(path.as_uri()) as r:
            self.assertEqual(r.read(), b"local")
            self.assertFalse(hasattr(r, "keel_outcome"))
        self.assertEqual(self.rows(), {})


class ResilienceTest(UrllibBase):
    def test_5xx_then_ok_is_retried(self) -> None:
        with FaultServer([fail(503), ok(b"recovered")]) as srv:
            with urllib.request.urlopen(srv.url()) as r:
                self.assertEqual(r.status, 200)
                self.assertEqual(r.read(), b"recovered")
                self.assertEqual(r.keel_outcome["attempts"], 2)

    def test_429_retry_after_seconds_governs_backoff(self) -> None:
        with FaultServer([throttled("1"), ok(b"ok")]) as srv:
            with urllib.request.urlopen(srv.url()) as r:
                self.assertEqual(r.status, 200)
                self.assertEqual(r.keel_outcome["attempts"], 2)
                # Level 0 backoff is 200ms; Retry-After 1s wins: max(200, 1000).
                self.assertEqual(r.keel_outcome["waits_ms"], [1000])

    def test_connection_reset_is_retried(self) -> None:
        with FaultServer([reset(), ok(b"after-reset")]) as srv:
            with urllib.request.urlopen(srv.url()) as r:
                self.assertEqual(r.read(), b"after-reset")
                self.assertEqual(r.keel_outcome["attempts"], 2)

    def test_caller_timeout_is_retried(self) -> None:
        with FaultServer([slow(0.3, ok(b"slow")), ok(b"fast")]) as srv:
            with urllib.request.urlopen(srv.url(), timeout=0.1) as r:
                self.assertEqual(r.read(), b"fast")
                self.assertEqual(r.keel_outcome["attempts"], 2)

    def test_policy_timeout_wins_when_tighter(self) -> None:
        # Policy 100ms beats a loose caller timeout of 5s: attempt 1 must time
        # out client-side in ~100ms, then attempt 2 succeeds.
        self.backend.configure(
            {**level0_defaults(), "target": {"127.0.0.1": {"timeout": "100ms"}}}
        )
        with FaultServer([slow(0.4, ok(b"slow")), ok(b"fast")]) as srv:
            with urllib.request.urlopen(srv.url(), timeout=5) as r:
                self.assertEqual(r.read(), b"fast")
                self.assertEqual(r.keel_outcome["attempts"], 2)

    def test_non_429_4xx_raises_httperror_unchanged_and_unretried(self) -> None:
        with FaultServer([status(404, b"missing")]) as srv:
            with self.assertRaises(urllib.error.HTTPError) as ctx:
                urllib.request.urlopen(srv.url())
            self.assertEqual(ctx.exception.code, 404)
            self.assertEqual(ctx.exception.read(), b"missing")
            # Core-level SUCCESS (a real response is never a Keel failure) —
            # the raise is urllib's own semantics, preserved.
            self.assertEqual(ctx.exception.keel_outcome["result"], "ok")
            self.assertEqual(srv.served, 1)

    def test_exhausted_retries_reraise_the_original_httperror(self) -> None:
        with FaultServer([fail(503), fail(503), fail(503)]) as srv:
            with self.assertRaises(urllib.error.HTTPError) as ctx:
                urllib.request.urlopen(srv.url())
            self.assertEqual(ctx.exception.code, 503)
            self.assertNotIsInstance(ctx.exception, KeelError)
            self.assertEqual(ctx.exception.keel_outcome["error"]["code"], "KEEL-E010")
            self.assertEqual(srv.served, 3)


class HardRulesTest(UrllibBase):
    def test_post_without_key_is_observed_not_retried(self) -> None:
        with FaultServer([fail(503), ok(b"unreached")]) as srv:
            with self.assertRaises(urllib.error.HTTPError) as ctx:
                urllib.request.urlopen(srv.url(), data=b"body")
            self.assertEqual(ctx.exception.code, 503)  # the real error, raised once
            self.assertEqual(ctx.exception.keel_outcome["error"]["code"], "KEEL-E014")
            self.assertEqual(srv.served, 1)

    def test_request_object_with_separate_data_kwarg_is_judged_as_post(self) -> None:
        # OpenerDirector.open folds a non-None `data` kwarg into a passed
        # Request (`req.data = data`) before dispatch — a call built as
        # `urlopen(Request(url), data=b"...")` must be judged POST (and
        # therefore hit the no-idempotency-key hard rule), not misread as
        # GET because `req.data` is still None at judgment time.
        with FaultServer([fail(503), ok(b"unreached")]) as srv:
            req = urllib.request.Request(srv.url())
            with self.assertRaises(urllib.error.HTTPError) as ctx:
                urllib.request.urlopen(req, data=b"x")
            self.assertEqual(ctx.exception.code, 503)
            self.assertEqual(ctx.exception.keel_outcome["error"]["code"], "KEEL-E014")
            self.assertEqual(srv.served, 1)

    def test_post_with_idempotency_key_is_retried(self) -> None:
        with FaultServer([fail(503), ok(b"posted")]) as srv:
            req = urllib.request.Request(
                srv.url(), data=b"body", headers={"Idempotency-Key": "abc"}
            )
            with urllib.request.urlopen(req) as r:
                self.assertEqual(r.read(), b"posted")
                self.assertEqual(r.keel_outcome["attempts"], 2)

    def test_original_transport_exception_reraised_unchanged(self) -> None:
        self.backend.configure(_NON_RETRYABLE_CONN)  # conn not in retry.on -> E015
        with FaultServer([reset()]) as srv:
            # Real urllib.request only wraps a request()-time OSError as
            # URLError (see AbstractHTTPHandler.do_open); a connection closed
            # with no response is discovered later in getresponse(), which is
            # NOT wrapped and raises the raw http.client.RemoteDisconnected
            # (a ConnectionResetError/OSError subclass). Verified against the
            # unwrapped stdlib before pinning this — the pack must preserve
            # THAT exact exception, not invent a URLError.
            with self.assertRaises(http.client.RemoteDisconnected) as ctx:
                urllib.request.urlopen(srv.url())
            exc = ctx.exception
            self.assertNotIsInstance(exc, KeelError)
            self.assertEqual(exc.keel_outcome["error"]["code"], "KEEL-E015")
            self.assertIs(exc.keel_outcome["error"]["original"], exc)


class RedirectTest(UrllibBase):
    def test_redirect_chain_is_one_keel_call(self) -> None:
        # HTTPRedirectHandler re-enters OpenerDirector.open for the hop; the
        # seam-ownership guard must pass the inner open straight through, so
        # the whole chain is ONE judged call (one attempt, one discovery row).
        with FaultServer([]) as srv:
            srv._script = [
                status(302, b"", {"Location": srv.url("/final")}),
                ok(b"landed"),
            ]
            with urllib.request.urlopen(srv.url("/start")) as r:
                self.assertEqual(r.read(), b"landed")
                self.assertEqual(r.keel_outcome["attempts"], 1)
            self.assertEqual(srv.served, 2)
        row = self.rows()["127.0.0.1"]
        self.assertEqual(row["calls"], 1)


class CacheReplayTest(UrllibBase):
    def test_cache_hit_rebuilds_a_real_readable_response(self) -> None:
        self.backend.configure(
            {**level0_defaults(), "target": {"127.0.0.1": {"cache": {"ttl": "10s"}}}}
        )
        with FaultServer([ok(b'{"a":1}', {"Content-Type": "application/json"})]) as srv:
            with urllib.request.urlopen(srv.url("/x")) as first:
                self.assertFalse(first.keel_outcome["from_cache"])
                # Buffered-for-cache, yet still fully readable by the caller.
                self.assertEqual(first.read(), b'{"a":1}')
                self.assertEqual(first.status, 200)
            with urllib.request.urlopen(srv.url("/x")) as second:
                self.assertTrue(second.keel_outcome["from_cache"])
                self.assertEqual(second.read(), b'{"a":1}')
                self.assertEqual(second.status, 200)
                self.assertEqual(second.headers.get("Content-Type"), "application/json")
        self.assertEqual(srv.served, 1)

    def test_cached_4xx_replays_as_httperror(self) -> None:
        # urllib semantics survive the cache: a 404 raised live must ALSO
        # raise on replay, with the body preserved.
        self.backend.configure(
            {**level0_defaults(), "target": {"127.0.0.1": {"cache": {"ttl": "10s"}}}}
        )
        with FaultServer([status(404, b"gone")]) as srv:
            with self.assertRaises(urllib.error.HTTPError) as first:
                urllib.request.urlopen(srv.url("/y"))
            self.assertEqual(first.exception.read(), b"gone")
            with self.assertRaises(urllib.error.HTTPError) as second:
                urllib.request.urlopen(srv.url("/y"))
            self.assertTrue(second.exception.keel_outcome["from_cache"])
            self.assertEqual(second.exception.code, 404)
            self.assertEqual(second.exception.read(), b"gone")
        self.assertEqual(srv.served, 1)


class PollTest(UrllibBase):
    """CCR-3 end-to-end at the seam: the claude-trader fetch_short_metrics
    loop shape (submit-then-poll against a status field) collapses into one
    urlopen call. The stub backend advances a virtual clock, so 10s/90s
    policies run instantly offline."""

    def _configure_poll(self, deadline: str = "90s") -> None:
        poll = {
            "interval": "10s",
            "deadline": deadline,
            "until": {"field": "status", "terminal": ["completed", "failed"]},
        }
        self.backend.configure({**level0_defaults(), "target": {"127.0.0.1": {"poll": poll}}})

    def test_pending_then_terminal_is_one_call(self) -> None:
        self._configure_poll()
        running = ok(b'{"status": "running"}', {"Content-Type": "application/json"})
        done = ok(b'{"status": "completed", "value": 7}', {"Content-Type": "application/json"})
        with FaultServer([running, running, done]) as srv:
            with urllib.request.urlopen(srv.url("/research")) as r:
                self.assertEqual(r.status, 200)
                self.assertEqual(r.read(), b'{"status": "completed", "value": 7}')
                self.assertEqual(r.keel_outcome["attempts"], 3)
            self.assertEqual(srv.served, 3)

    def test_deadline_raises_keel_e016(self) -> None:
        self._configure_poll(deadline="25s")
        running = ok(b'{"status": "running"}', {"Content-Type": "application/json"})
        with FaultServer([running, running, running]) as srv:
            with self.assertRaises(KeelError) as ctx:
                urllib.request.urlopen(srv.url("/research"))
            self.assertIn("KEEL-E016", str(ctx.exception))
            self.assertEqual(srv.served, 3)

    def test_non_json_body_fails_open(self) -> None:
        self._configure_poll()
        with FaultServer([ok(b"plain text", {"Content-Type": "text/plain"})]) as srv:
            with urllib.request.urlopen(srv.url("/research")) as r:
                self.assertEqual(r.read(), b"plain text")
                self.assertEqual(r.keel_outcome["attempts"], 1)


class InstallLifecycleTest(UrllibBase):
    def test_uninstall_restores_and_reinstall_is_idempotent(self) -> None:
        from urllib.request import OpenerDirector

        wrapped = OpenerDirector.open
        self.assertTrue(getattr(wrapped, "__keel_wrapped__", False))
        urllib_pack.install()  # double install: no double-wrap
        self.assertIs(OpenerDirector.open, wrapped)
        urllib_pack.uninstall()
        self.assertFalse(getattr(OpenerDirector.open, "__keel_wrapped__", False))
        with FaultServer([ok(b"bare")]) as srv:
            with urllib.request.urlopen(srv.url()) as r:
                self.assertFalse(hasattr(r, "keel_outcome"))
        urllib_pack.install()  # restore for tearDown symmetry


if __name__ == "__main__":
    unittest.main()
