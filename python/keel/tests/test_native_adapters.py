"""Adapter behavior under the NATIVE core (Task 14 "the swap").

The other adapter suites pin behavior against the stub (the no-wheel CI path);
this one drives the httpx pack through the real ``keel_core`` engine to prove
parity across the FFI seam — the response-envelope serialization (success path
byte-transparency + retry) and, for item 3, the async path routed through
``keel_core.execute_async`` (no worker-thread bridge). Skips when the native
module is not built.
"""

from __future__ import annotations

import asyncio
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

import httpx

from keel import _runtime
from keel._backend import load_backend
from keel._defaults import level0_defaults
from keel._discovery import Discovery
from keel.adapters import httpx_pack
from .faultserver import FaultServer, fail, ok

try:
    import keel_core  # noqa: F401

    _NATIVE = True
except ImportError:
    _NATIVE = False

# Retry conn/timeout/5xx fast so the real engine's backoff stays sub-ms in tests.
_FAST = {
    "target": {
        "127.0.0.1": {
            "retry": {"attempts": 3, "on": ["conn", "timeout", "5xx"], "schedule": "fixed(1ms)"}
        }
    }
}


@unittest.skipUnless(_NATIVE, "keel_core native module not built (maturin develop in crates/keel-py)")
class NativeHttpxTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = TemporaryDirectory()
        self.cwd = Path(self._tmp.name)
        # A native backend (with a journal at <cwd>/.keel/journal.db) — the real
        # engine behind the httpx seam.
        self.backend = load_backend("native", cwd=self.cwd)
        self.backend.configure({**level0_defaults(), **_FAST})
        self.discovery = Discovery(self.cwd)
        _runtime.set_runtime(self.backend, self.discovery)
        httpx_pack.install()

    def tearDown(self) -> None:
        httpx_pack.uninstall()
        _runtime.clear_runtime()
        self.discovery.close()
        self._tmp.cleanup()

    def test_native_is_actually_selected(self) -> None:
        # Guards against a silent fall-back to the stub masking the FFI path.
        self.assertTrue(hasattr(self.backend, "execute_async"), "native exposes execute_async")
        self.assertNotEqual(type(self.backend).__module__, "keel_core_stub")

    def test_sync_success_is_byte_transparent(self) -> None:
        body = b"native-\x00\xff-bytes"
        with FaultServer([ok(body, {"X-Custom": "v1"})]) as srv:
            with httpx.Client() as c:
                r = c.get(srv.url("/p"))
        self.assertEqual(r.status_code, 200)
        self.assertEqual(bytes(r.content), body, "live response returned unchanged across the FFI")
        self.assertEqual(r.headers["x-custom"], "v1")
        self.assertEqual(r.keel_outcome["result"], "ok")
        self.assertFalse(r.keel_outcome["from_cache"])

    def test_sync_retry_5xx_then_success(self) -> None:
        with FaultServer([fail(503), ok(b"recovered")]) as srv:
            with httpx.Client() as c:
                r = c.get(srv.url("/flaky"))
        self.assertEqual(r.status_code, 200)
        self.assertEqual(r.content, b"recovered")
        self.assertEqual(r.keel_outcome["attempts"], 2, "retried through the native engine")

    def test_async_retry_via_execute_async(self) -> None:
        # Item 3: the async seam routes through keel_core.execute_async (the effect
        # is awaited directly on the loop — no worker-thread bridge).
        async def go() -> httpx.Response:
            with FaultServer([fail(503), ok(b"async-recovered")]) as srv:
                async with httpx.AsyncClient() as c:
                    return await c.get(srv.url("/flaky"))

        r = asyncio.run(go())
        self.assertEqual(r.status_code, 200)
        self.assertEqual(r.content, b"async-recovered")
        self.assertEqual(r.keel_outcome["attempts"], 2)

    def test_async_success_is_byte_transparent(self) -> None:
        async def go() -> httpx.Response:
            with FaultServer([ok(b"async-ok")]) as srv:
                async with httpx.AsyncClient() as c:
                    return await c.get(srv.url("/p"))

        r = asyncio.run(go())
        self.assertEqual(r.content, b"async-ok")
        self.assertEqual(r.keel_outcome["result"], "ok")


if __name__ == "__main__":
    unittest.main()
