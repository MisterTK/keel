"""Policy-layer resolution at the HTTP seam.

Two knobs that used to be dead in the Python front end (no backend exposed a
public ``layer()``) are now honored on BOTH backends:

  * ``idempotency.header`` — a POST carrying the configured header is retryable
    (was silently ignored under native → divergent retry decisions vs Node);
    a POST carrying NO recognized key is now INJECTED one and retried too
    (contracts/adapter-pack.md "Idempotency-key injection" — see
    ``test_adapters_httpx.py``/``test_adapters_requests.py`` for the fuller
    injection matrix: stable-across-retries, caller-key-wins, distinct
    per-logical-call keys);
  * the cache-body buffering gate — a response body is buffered ONLY when a cache
    ttl is actually configured, so a streaming/SSE GET passes through unbuffered
    at Level 0 (Python used to force-read every GET body).

The idempotency e2e runs against the stub AND the native core; the buffering
gate is backend-agnostic (pack logic) so it runs on the stub.
"""

from __future__ import annotations

import gc
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any

import httpx

from keel import _runtime
from keel._backend import load_backend
from keel._defaults import level0_defaults
from keel._discovery import Discovery
from keel.adapters import _http, httpx_pack
from .faultserver import FaultServer, fail, ok

try:
    import keel_core  # noqa: F401

    _NATIVE = True
except ImportError:
    _NATIVE = False

_RETRY_5XX = {"retry": {"attempts": 3, "on": ["5xx"], "schedule": "fixed(1ms)"}}
_IDEM = {"idempotency": {"header": "X-Request-Token"}}


class LayerResolutionUnitTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = TemporaryDirectory()
        self.cwd = Path(self._tmp.name)

    def tearDown(self) -> None:
        _runtime.clear_runtime()
        self._tmp.cleanup()

    def _runtime_with(self, policy: dict[str, Any]) -> None:
        backend = load_backend("stub")
        backend.configure(policy)
        _runtime.set_runtime(backend, None)

    def test_idempotency_header_resolves_from_policy(self) -> None:
        self._runtime_with({"target": {"api.stripe.com": _IDEM}})
        self.assertEqual(_http.idempotency_header("api.stripe.com"), "X-Request-Token")
        self.assertIsNone(_http.idempotency_header("api.example.com"))

    def test_cache_gate_true_only_where_a_ttl_is_resolved(self) -> None:
        self._runtime_with(
            {
                "defaults": {"outbound": {"timeout": "30s"}, "llm": {"cache": {"ttl": "24h"}}},
                "target": {"api.stripe.com": {}},
            }
        )
        self.assertTrue(_http.cache_configured("llm:openai"), "defaults.llm.cache.ttl → buffered")
        self.assertFalse(_http.cache_configured("api.example.com"), "outbound: no cache")
        self.assertFalse(_http.cache_configured("api.stripe.com"), "target: no cache")

    def test_no_backend_is_safe_defaults(self) -> None:
        _runtime.set_runtime(None, None)
        self.assertIsNone(_http.idempotency_header("x"))
        self.assertFalse(_http.cache_configured("x"))


class _IdempotencyHeaderE2E:
    """Configured idempotency.header makes a POST retryable — on each backend."""

    KIND = "stub"

    def setUp(self) -> None:
        self._tmp = TemporaryDirectory()
        self.cwd = Path(self._tmp.name)
        self.backend: Any = None
        self.discovery: Any = None

    def tearDown(self) -> None:
        httpx_pack.uninstall()
        _runtime.clear_runtime()
        if self.discovery is not None:
            self.discovery.close()
        self.backend = None
        gc.collect()
        self._tmp.cleanup()

    def _install(self, policy: dict[str, Any]) -> None:
        self.backend = load_backend(self.KIND, cwd=self.cwd)
        self.backend.configure(policy)
        self.discovery = Discovery(self.cwd)
        _runtime.set_runtime(self.backend, self.discovery)
        httpx_pack.install()

    def test_post_with_configured_header_is_retried(self) -> None:
        self._install({**level0_defaults(), "target": {"127.0.0.1": {**_RETRY_5XX, **_IDEM}}})
        with FaultServer([fail(503), ok(b"ok")]) as srv:
            with httpx.Client() as c:
                r = c.post(srv.url("/p"), headers={"X-Request-Token": "abc"})
        self.assertEqual(r.status_code, 200, f"[{self.KIND}] configured-header POST retried to success")
        self.assertEqual(r.keel_outcome["attempts"], 2)

    def test_post_without_a_caller_header_is_injected_and_retried(self) -> None:
        # contracts/adapter-pack.md "Idempotency-key injection": a configured
        # `idempotency.header` with no caller-supplied key means the adapter
        # MINTS one and injects it — the judgment flip makes this retryable,
        # superseding the old recognition-only behavior (a bare POST used to
        # be observed, not retried, here).
        self._install({**level0_defaults(), "target": {"127.0.0.1": {**_RETRY_5XX, **_IDEM}}})
        with FaultServer([fail(503), ok(b"ok")]) as srv:
            with httpx.Client() as c:
                r = c.post(srv.url("/p"))
        self.assertEqual(r.status_code, 200, f"[{self.KIND}] injected-key POST retried to success")
        self.assertEqual(r.keel_outcome["attempts"], 2)
        keys = [h.get("x-request-token") for h in srv.headers]
        self.assertEqual(len(keys), 2)
        self.assertIsNotNone(keys[0])
        self.assertEqual(keys[0], keys[1])  # same minted key on every attempt


class IdempotencyHeaderStub(_IdempotencyHeaderE2E, unittest.TestCase):
    KIND = "stub"


@unittest.skipUnless(_NATIVE, "keel_core native module not built (maturin develop in crates/keel-py)")
class IdempotencyHeaderNative(_IdempotencyHeaderE2E, unittest.TestCase):
    KIND = "native"


class StreamingBufferGateTest(unittest.TestCase):
    """A streaming GET with no cache configured is NOT consumed at the seam."""

    def setUp(self) -> None:
        self._tmp = TemporaryDirectory()
        self.cwd = Path(self._tmp.name)
        self.backend = load_backend("stub")
        self.backend.configure(level0_defaults())  # no cache on defaults.outbound
        self.discovery = Discovery(self.cwd)
        _runtime.set_runtime(self.backend, self.discovery)
        httpx_pack.install()

    def tearDown(self) -> None:
        httpx_pack.uninstall()
        _runtime.clear_runtime()
        self.discovery.close()
        self._tmp.cleanup()

    def test_streaming_get_not_consumed_when_no_cache(self) -> None:
        body = b"stream-body-" + b"x" * 2048
        with FaultServer([ok(body)]) as srv:
            with httpx.Client() as c:
                with c.stream("GET", srv.url("/s")) as r:
                    # The old code force-read every GET body at the seam; with the
                    # cache-ttl gate and no cache configured, it must stay lazy.
                    self.assertFalse(
                        r.is_stream_consumed, "seam pre-read a streaming GET with no cache configured"
                    )
                    chunks = list(r.iter_bytes())
        self.assertEqual(b"".join(chunks), body, "body still delivered intact when streamed")

    def test_streaming_get_retries_5xx_without_prebuffering(self) -> None:
        # Streaming + retry interact: a transient 5xx must retry (the winning
        # response is a fresh attempt), yet the seam must STILL leave the final
        # streaming body unconsumed when no cache is configured. This is the path
        # the pass-through gate is most likely to regress on, since the retry loop
        # re-fires the effect and swaps the live response.
        body = b"retried-stream-" + b"y" * 4096
        with FaultServer([fail(503), ok(body)]) as srv:
            with httpx.Client() as c:
                with c.stream("GET", srv.url("/s")) as r:
                    self.assertEqual(
                        r.keel_outcome["attempts"], 2, "transient 5xx should have retried once"
                    )
                    self.assertEqual(r.status_code, 200, "the winning attempt is the 200")
                    self.assertFalse(
                        r.is_stream_consumed,
                        "seam pre-read the retried streaming GET body with no cache configured",
                    )
                    chunks = list(r.iter_bytes())
        self.assertEqual(b"".join(chunks), body, "retried streamed body delivered intact")


if __name__ == "__main__":
    unittest.main()
