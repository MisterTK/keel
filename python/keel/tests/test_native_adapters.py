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
import gc
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
from .test_flows import _inject_running_step

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
        # Drop the native core so its journal's SQLite connection closes now
        # (via Drop) rather than at GC — keeps test output free of ResourceWarnings.
        self.backend = None
        gc.collect()
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


@unittest.skipUnless(_NATIVE, "keel_core native module not built (maturin develop in crates/keel-py)")
class NativeHttpxFlowIdempotencyTest(unittest.TestCase):
    """End-to-end proof of contracts/adapter-pack.md "Idempotency-key
    injection" rule 3, through the REAL adapter (httpx_pack + _http.py) and
    REAL native core — not a fake backend, and not the raw KeelCore binding
    test in test_flows.py, which pins the binding surface in isolation.

    A single httpx call's step is left `running` (`_inject_running_step`) —
    the process died after the (simulated) crashed attempt injected its key
    and sent its request, before the terminal outcome was recorded. On
    resume, the actual retried HTTP request that reaches the wire must carry
    the SAME ``Idempotency-Key`` header value the crashed attempt did —
    proven by inspecting the header FaultServer actually received, not by
    reading the journal back. (Deliberately one step, not two: a completed
    step's *replay* reconstructing an httpx.Response from the journal is a
    separate, already-covered concern — `NativeHttpxTest`/`test_flows.py` —
    and orthogonal to this rule.)"""

    def setUp(self) -> None:
        self._tmp = TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.cwd = Path(self._tmp.name)
        # `load_backend("native", cwd=...)` attaches the journal at
        # `<cwd>/.keel/journal.db` (`_backend._journal_path`'s default).
        self.journal_path = str(self.cwd / ".keel" / "journal.db")
        self.entrypoint = "py:billing.charge:main"
        self.args_hash = "ah-idem-e2e"
        self.flow_id = f"{self.entrypoint}#{self.args_hash}#"
        self.policy = {
            **level0_defaults(),
            "defaults": {
                "outbound": {
                    "retry": {"attempts": 3, "on": ["conn", "timeout", "5xx"], "schedule": "fixed(1ms)"},
                    "idempotency": {"header": "Idempotency-Key"},
                },
            },
        }

    def _core(self) -> object:
        backend = load_backend("native", cwd=self.cwd)
        backend.configure(self.policy)
        return backend

    def test_resumed_step_injects_the_same_key_the_crashed_attempt_did(self) -> None:
        srv = FaultServer([ok(b"charge-ok")])
        srv.__enter__()
        self.addCleanup(srv.__exit__)

        # Open the flow once just to create the `flows` row (identity), then
        # crash immediately: step 1's `running` record (carrying the key a
        # real adapter would have injected before its request) is journaled
        # directly — never through a live call — modeling the process dying
        # right after that write, before any response was recorded.
        backend1 = self._core()
        backend1.enter_flow(self.entrypoint, self.args_hash, code_hash="ch-1")
        step_key = "127.0.0.1#-"  # POST to a non-llm: target hashes to None -> "-"
        _inject_running_step(
            self.journal_path, self.flow_id, seq=1, step_key=step_key, idempotency_key="ik-crashed-e2e"
        )
        backend1 = None
        gc.collect()

        backend2 = self._core()
        _runtime.set_runtime(backend2, None)
        httpx_pack.install()
        self.addCleanup(httpx_pack.uninstall)
        info = backend2.enter_flow(self.entrypoint, self.args_hash, code_hash="ch-1")
        self.assertFalse(info["replay"], "an uncompleted flow resumes live, not a pure replay")
        _runtime.set_flow_active(True)  # the flag `_flow.run_as_flow` sets around a flow body
        try:
            with httpx.Client() as c:
                r = c.post(srv.url("/charge"), json={"amount": 100})
            self.assertEqual(r.status_code, 200)
        finally:
            _runtime.set_flow_active(False)
            backend2.exit_flow("completed")
            _runtime.clear_runtime()

        self.assertEqual(srv.served, 1, "the resumed step hit the network exactly once, live")
        # The load-bearing assertion: the header the resumed attempt actually
        # sent on the wire is IDENTICAL to the crashed attempt's key — not
        # merely that some Idempotency-Key header was present.
        self.assertEqual(srv.headers[-1].get("idempotency-key"), "ik-crashed-e2e")


if __name__ == "__main__":
    unittest.main()
