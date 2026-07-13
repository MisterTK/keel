"""The ``mcp:`` client-session pack: the adapter-pack four operations, target
derivation, method-keyed idempotency, and the Level 0 hard rule (a
side-effecting ``tools/call`` is observed, never retried — KEEL-E014) — the
cross-language parity contract with the Node twin
(``node/keel/test/mcp.test.mjs``: same policy in → same report counters out).

Tests run OFFLINE against a structural fake session (no dependency on the
`mcp` package being installed); a couple of ``detect()``/real-class tests are
conditional on it actually being present, mirroring ``ProviderPackTest`` in
``test_packs_llm.py``.
"""

from __future__ import annotations

import asyncio
import importlib.util
import sys
import unittest
from datetime import timedelta
from types import SimpleNamespace
from typing import Any
from unittest import mock

from keel import _runtime
from keel._backend import load_backend
from keel._defaults import apply_pack_defaults
from keel._errors import KeelError
from keel._wrap import WRAPPED_ATTR
from keel.packs import mcp_pack
from keel.packs.mcp_pack import (
    classify_mcp_error,
    install_mcp_pack,
    is_idempotent_mcp_method,
    make_wrapped_send_request,
    patch_client_session,
)

_RETRY_CONN = {"attempts": 3, "on": ["conn"], "schedule": "fixed(1ms)"}


def _session(name: str = "svc") -> SimpleNamespace:
    return SimpleNamespace(server_info=SimpleNamespace(name=name))


class McpPackContractTest(unittest.TestCase):
    def test_four_operations(self) -> None:
        det = mcp_pack.detect()
        present = importlib.util.find_spec("mcp") is not None
        self.assertEqual(det.matched, present)
        seams = mcp_pack.seams()
        self.assertEqual(seams[0].patch_point, "mcp.client.session.ClientSession.send_request")
        self.assertIn("ServerSession", seams[0].why_stable)
        decls = mcp_pack.targets()
        self.assertEqual(decls[0].pattern, "mcp:<server>")
        self.assertEqual(decls[0].kind, "mcp")
        self.assertIn("KEEL-E014", decls[0].idempotency_rule)
        # No [defaults.mcp] in the frozen pack: mcp: inherits [defaults.outbound].
        self.assertEqual(mcp_pack.defaults(), {})


class IdempotentMethodTest(unittest.TestCase):
    def test_reads_retry_writes_and_unknown_do_not(self) -> None:
        for m in (
            "initialize",
            "ping",
            "tools/list",
            "resources/list",
            "resources/templates/list",
            "resources/read",
            "prompts/list",
            "prompts/get",
            "completion/complete",
        ):
            self.assertTrue(is_idempotent_mcp_method(m), m)
        for m in ("tools/call", "logging/setLevel", "resources/subscribe", "frobnicate", "", None, 7):
            self.assertFalse(is_idempotent_mcp_method(m), repr(m))


class ClassifyMcpErrorTest(unittest.TestCase):
    def test_timeout_conn_other(self) -> None:
        self.assertEqual(classify_mcp_error(TimeoutError("slow")), "timeout")
        self.assertEqual(classify_mcp_error(asyncio.TimeoutError()), "timeout")
        # v1 McpError shape: `.error.code` (duck-typed, no import of mcp's types).
        fake_v1 = Exception("timed out")
        fake_v1.error = SimpleNamespace(code=408)  # type: ignore[attr-defined]
        self.assertEqual(classify_mcp_error(fake_v1), "timeout")
        # A hypothetical v2 flattened shape: `.code` directly.
        fake_v2 = Exception("timed out")
        fake_v2.code = -32001  # type: ignore[attr-defined]
        self.assertEqual(classify_mcp_error(fake_v2), "timeout")
        self.assertEqual(classify_mcp_error(ConnectionError("down")), "conn")
        self.assertEqual(classify_mcp_error(ConnectionResetError("reset")), "conn")
        self.assertEqual(classify_mcp_error(Exception("transport closed")), "conn")
        self.assertEqual(classify_mcp_error(ValueError("bad tool args")), "other")


class McpTestBase(unittest.TestCase):
    def tearDown(self) -> None:
        _runtime.clear_runtime()

    def install(self, policy: dict[str, Any]) -> Any:
        backend = load_backend("stub")
        backend.configure(policy)
        _runtime.set_runtime(backend, None)
        return backend


class TargetDerivationTest(McpTestBase):
    def test_target_is_mcp_server_idempotency_is_method_keyed_never_cached(self) -> None:
        self.install({})
        session = _session("weather")

        async def original(self: Any, request: Any, result_type: Any, timeout: Any, metadata: Any, progress: Any) -> Any:
            return {"pong": True}

        wrapped = make_wrapped_send_request(original)

        async def run() -> None:
            await wrapped(session, SimpleNamespace(method="tools/list"), None, None, None, None)
            await wrapped(session, SimpleNamespace(method="tools/call"), None, None, None, None)

        asyncio.run(run())
        report = _runtime.get_backend().report()["targets"]
        self.assertIn("mcp:weather", report)
        # Both calls hit the same target; args_hash is always None (never cached).
        self.assertEqual(report["mcp:weather"]["calls"], 2)

    def test_unknown_server_before_initialize(self) -> None:
        self.install({})
        session = SimpleNamespace()  # no server_info yet: pre-handshake

        async def original(self: Any, request: Any, result_type: Any, timeout: Any, metadata: Any, progress: Any) -> Any:
            return SimpleNamespace(serverInfo=SimpleNamespace(name="svc"))

        wrapped = make_wrapped_send_request(original)
        asyncio.run(wrapped(session, SimpleNamespace(method="initialize"), None, None, None, None))
        self.assertIn("mcp:unknown", _runtime.get_backend().report()["targets"])

    def test_server_name_cached_from_initialize_for_later_calls(self) -> None:
        # The pinned mcp SDK doesn't retain server identity on the session
        # itself, so the pack must capture it off a successful `initialize`
        # response (see module docs) — otherwise every call would resolve to
        # mcp:unknown for the session's whole lifetime.
        self.install({})
        session = SimpleNamespace()
        calls: list[str] = []

        async def original(self: Any, request: Any, result_type: Any, timeout: Any, metadata: Any, progress: Any) -> Any:
            calls.append(request.method)
            if request.method == "initialize":
                return SimpleNamespace(serverInfo=SimpleNamespace(name="weather"))
            return {"tools": []}

        wrapped = make_wrapped_send_request(original)

        async def run() -> None:
            await wrapped(session, SimpleNamespace(method="initialize"), None, None, None, None)
            await wrapped(session, SimpleNamespace(method="tools/list"), None, None, None, None)

        asyncio.run(run())
        report = _runtime.get_backend().report()["targets"]
        self.assertIn("mcp:unknown", report, "the initialize call itself is pre-handshake")
        self.assertIn("mcp:weather", report, "the later call resolves under the cached server name")
        self.assertEqual(report["mcp:weather"]["calls"], 1)


class NonIdempotentDefaultTest(McpTestBase):
    def test_tools_call_observed_not_retried_e014(self) -> None:
        backend = self.install({"target": {"mcp:svc": {"retry": _RETRY_CONN}}})
        session = _session()
        calls = {"n": 0}
        original_exc = ConnectionError("connection reset")

        async def original(self: Any, request: Any, result_type: Any, timeout: Any, metadata: Any, progress: Any) -> Any:
            calls["n"] += 1
            raise original_exc

        wrapped = make_wrapped_send_request(original)

        async def run() -> None:
            with self.assertRaises(ConnectionError) as ctx:
                await wrapped(session, SimpleNamespace(method="tools/call"), None, None, None, None)
            self.assertIs(ctx.exception, original_exc, "original exception object re-raised")
            self.assertEqual(ctx.exception.keel_outcome["error"]["code"], "KEEL-E014")

        asyncio.run(run())
        self.assertEqual(calls["n"], 1, "a side-effecting tools/call is NOT retried")
        self.assertEqual(
            backend.report()["targets"]["mcp:svc"],
            {
                "attempts": 1,
                "breaker_opens": 0,
                "breaker_state": "closed",
                "cache_hits": 0,
                "calls": 1,
                "failures": 1,
                "retries": 0,
                "successes": 0,
                "throttled": 0,
            },
        )

    def test_never_cached_even_with_cache_configured(self) -> None:
        self.install({"target": {"mcp:svc": {"cache": {"ttl": "60s"}}}})
        session = _session()
        calls = {"n": 0}

        async def original(self: Any, request: Any, result_type: Any, timeout: Any, metadata: Any, progress: Any) -> Any:
            calls["n"] += 1
            return {"n": calls["n"]}

        wrapped = make_wrapped_send_request(original)

        async def run() -> None:
            first = await wrapped(session, SimpleNamespace(method="tools/call", params={"name": "x"}), None, None, None, None)
            second = await wrapped(session, SimpleNamespace(method="tools/call", params={"name": "x"}), None, None, None, None)
            self.assertEqual(first, {"n": 1})
            self.assertEqual(second, {"n": 2})

        asyncio.run(run())
        self.assertEqual(calls["n"], 2, "a side-effecting call executed every time")


class IdempotentReadRetryTest(McpTestBase):
    def test_resources_list_retried_per_defaults_outbound(self) -> None:
        backend = self.install(apply_pack_defaults({}))  # mcp:svc inherits defaults.outbound
        session = _session()
        calls = {"n": 0}

        async def original(self: Any, request: Any, result_type: Any, timeout: Any, metadata: Any, progress: Any) -> Any:
            calls["n"] += 1
            if calls["n"] == 1:
                raise ConnectionError("connection reset")
            return {"resources": []}

        wrapped = make_wrapped_send_request(original)
        result = asyncio.run(wrapped(session, SimpleNamespace(method="resources/list"), None, None, None, None))
        self.assertEqual(result, {"resources": []})
        self.assertEqual(calls["n"], 2, "read-ish method retried per defaults.outbound")
        t = backend.report()["targets"]["mcp:svc"]
        self.assertEqual(t["attempts"], 2)
        self.assertEqual(t["retries"], 1)
        self.assertEqual(t["successes"], 1)

    def test_retries_a_conn_error_then_succeeds(self) -> None:
        backend = self.install({"target": {"mcp:svc": {"retry": _RETRY_CONN}}})
        session = _session()
        calls = {"n": 0}

        async def original(self: Any, request: Any, result_type: Any, timeout: Any, metadata: Any, progress: Any) -> Any:
            calls["n"] += 1
            if calls["n"] == 1:
                raise Exception("connection closed")
            return {"tools": []}

        wrapped = make_wrapped_send_request(original)
        result = asyncio.run(wrapped(session, SimpleNamespace(method="tools/list"), None, None, None, None))
        self.assertEqual(result, {"tools": []})
        self.assertEqual(calls["n"], 2)
        self.assertEqual(
            backend.report()["targets"]["mcp:svc"],
            {
                "attempts": 2,
                "breaker_opens": 0,
                "breaker_state": "closed",
                "cache_hits": 0,
                "calls": 1,
                "failures": 0,
                "retries": 1,
                "successes": 1,
                "throttled": 0,
            },
        )

    def test_success_returns_live_object_identity(self) -> None:
        self.install({})
        session = _session()
        sentinel = {"nested": (1, 2)}  # tuple survives only via live delivery

        async def original(self: Any, request: Any, result_type: Any, timeout: Any, metadata: Any, progress: Any) -> Any:
            return sentinel

        wrapped = make_wrapped_send_request(original)
        result = asyncio.run(wrapped(session, SimpleNamespace(method="ping"), None, None, None, None))
        self.assertIs(result, sentinel)


class TimeoutInjectionTest(McpTestBase):
    def test_policy_timeout_injected_only_for_idempotent_methods(self) -> None:
        self.install({"target": {"mcp:svc": {"timeout": "250ms"}}})
        session = _session()
        seen: list[Any] = []

        async def original(self: Any, request: Any, result_type: Any, timeout: Any, metadata: Any, progress: Any) -> Any:
            seen.append(timeout)
            return {}

        wrapped = make_wrapped_send_request(original)

        async def run() -> None:
            await wrapped(session, SimpleNamespace(method="ping"), None, None, None, None)  # idempotent
            await wrapped(session, SimpleNamespace(method="tools/call"), None, "caller-timeout", None, None)  # not

        asyncio.run(run())
        self.assertEqual(seen[0], timedelta(milliseconds=250), "policy timeout injected for an idempotent method")
        self.assertEqual(seen[1], "caller-timeout", "the caller's own timeout passes through for tools/call unchanged")

    def test_a_would_be_hung_server_times_out_and_degrades_e010(self) -> None:
        # Simulates the SDK's own request_read_timeout_seconds firing (as the
        # real send_request does via anyio.fail_after) by having the fake
        # server raise a timeout-shaped error whenever Keel injects a timeout —
        # deterministic, no real sleep.
        backend = self.install(
            {"target": {"mcp:svc": {"timeout": "50ms", "retry": {"attempts": 2, "schedule": "fixed(1ms)", "on": ["timeout"]}}}}
        )
        session = _session()
        seen_timeouts: list[Any] = []

        async def original(self: Any, request: Any, result_type: Any, timeout: Any, metadata: Any, progress: Any) -> Any:
            seen_timeouts.append(timeout)
            err = Exception("Timed out while waiting for response")
            err.error = SimpleNamespace(code=408)  # type: ignore[attr-defined]
            raise err

        wrapped = make_wrapped_send_request(original)
        with self.assertRaises(Exception) as ctx:
            asyncio.run(wrapped(session, SimpleNamespace(method="tools/list"), None, None, None, None))
        self.assertEqual(ctx.exception.keel_outcome["error"]["code"], "KEEL-E010")
        self.assertEqual(ctx.exception.keel_outcome["error"]["class"], "timeout")
        self.assertEqual(len(seen_timeouts), 2, "retried per policy before giving up")
        self.assertTrue(all(t == timedelta(milliseconds=50) for t in seen_timeouts))
        self.assertEqual(backend.report()["targets"]["mcp:svc"]["attempts"], 2)


class BreakerTest(McpTestBase):
    def test_breaker_opens_after_repeated_failures_then_fails_fast_e012(self) -> None:
        backend = self.install(
            {
                "target": {
                    "mcp:svc": {
                        "retry": {"attempts": 1, "on": ["conn"]},
                        "breaker": {"failures": 2, "cooldown": "30s"},
                    }
                }
            }
        )
        session = _session()
        attempts = {"n": 0}

        async def original(self: Any, request: Any, result_type: Any, timeout: Any, metadata: Any, progress: Any) -> Any:
            attempts["n"] += 1
            raise ConnectionError("refused")

        wrapped = make_wrapped_send_request(original)

        async def run() -> None:
            for _ in range(2):
                with self.assertRaises(ConnectionError):
                    await wrapped(session, SimpleNamespace(method="x"), None, None, None, None)
            with self.assertRaises(KeelError) as ctx:
                await wrapped(session, SimpleNamespace(method="x"), None, None, None, None)
            self.assertEqual(ctx.exception.code, "KEEL-E012")

        asyncio.run(run())
        self.assertEqual(attempts["n"], 2, "third call fails fast — the transport is not touched")
        t = backend.report()["targets"]["mcp:svc"]
        self.assertEqual(t["breaker_opens"], 1)
        self.assertEqual(t["breaker_state"], "open")


class PassthroughTest(unittest.TestCase):
    def tearDown(self) -> None:
        _runtime.clear_runtime()

    def test_passthrough_when_disabled(self) -> None:
        _runtime.clear_runtime()

        async def original(self: Any, request: Any, result_type: Any, timeout: Any, metadata: Any, progress: Any) -> Any:
            return {"ok": request.method}

        wrapped = make_wrapped_send_request(original)
        result = asyncio.run(wrapped(_session(), SimpleNamespace(method="ping"), None, None, None, None))
        self.assertEqual(result, {"ok": "ping"})


class PatchAndInstallTest(unittest.TestCase):
    def test_patch_client_session_patches_and_reverses(self) -> None:
        class FakeSession:
            async def send_request(
                self, request: Any, result_type: Any = None, request_read_timeout_seconds: Any = None,
                metadata: Any = None, progress_callback: Any = None,
            ) -> Any:
                return {"ok": request.method}

        _runtime.clear_runtime()  # global backend is None -> pass-through
        uninstall = patch_client_session(FakeSession)
        self.assertTrue(getattr(FakeSession.send_request, WRAPPED_ATTR))
        result = asyncio.run(FakeSession().send_request(SimpleNamespace(method="ping")))
        self.assertEqual(result, {"ok": "ping"}, "pass-through when no backend is active")
        # second patch is a no-op.
        noop = patch_client_session(FakeSession)
        noop()
        self.assertTrue(getattr(FakeSession.send_request, WRAPPED_ATTR), "no-op patch did not disturb the wrap")
        uninstall()
        self.assertFalse(getattr(FakeSession.send_request, WRAPPED_ATTR, False))

    def test_install_mcp_pack_with_injected_session_class(self) -> None:
        class FakeSession:
            async def send_request(
                self, request: Any, result_type: Any = None, request_read_timeout_seconds: Any = None,
                metadata: Any = None, progress_callback: Any = None,
            ) -> Any:
                return {"ok": True}

        result = install_mcp_pack(session_class=FakeSession)
        self.assertTrue(result["active"])
        self.assertEqual(result["name"], "mcp")
        result["uninstall"]()
        self.assertFalse(getattr(FakeSession.send_request, WRAPPED_ATTR, False))

    def test_install_mcp_pack_absent_sdk_is_a_silent_noop(self) -> None:
        class NotASession:
            pass  # no send_request at all

        result = install_mcp_pack(session_class=NotASession)
        self.assertEqual(result, {"active": False})


class LazyInstallTest(unittest.TestCase):
    """With no `session_class` override, installation is LAZY (module docs):
    a present-but-unimported SDK costs one cheap `find_spec`, never a real
    import of its dependency chain — the regression this guards is the
    `keel run` startup-overhead budget (test_run.StartupBudgetTest)."""

    def setUp(self) -> None:
        self._meta_path_len = len(sys.meta_path)

    def tearDown(self) -> None:
        # Belt-and-suspenders: never leak a finder into later tests even if an
        # assertion fails mid-test before its own uninstall() runs.
        while len(sys.meta_path) > self._meta_path_len:
            sys.meta_path.pop(0)

    def test_absent_sdk_returns_inactive_and_touches_no_meta_path(self) -> None:
        with mock.patch("importlib.util.find_spec", return_value=None):
            result = install_mcp_pack()
        self.assertEqual(result, {"active": False})
        self.assertEqual(len(sys.meta_path), self._meta_path_len)

    def test_present_but_unimported_sdk_is_armed_without_importing_it(self) -> None:
        with mock.patch.object(mcp_pack, "install") as mock_install:
            with mock.patch("importlib.util.find_spec", return_value=object()):
                with mock.patch.dict(sys.modules):
                    sys.modules.pop(mcp_pack.MODULE, None)  # simulate "not yet imported"
                    result = install_mcp_pack()
        self.assertTrue(result["active"])
        mock_install.assert_not_called()  # armed, not patched — no real import happened
        self.assertEqual(len(sys.meta_path), self._meta_path_len + 1, "a lazy hook was armed")
        result["uninstall"]()
        self.assertEqual(len(sys.meta_path), self._meta_path_len, "uninstall removed the hook")

    def test_already_imported_sdk_is_patched_immediately(self) -> None:
        with mock.patch.object(mcp_pack, "install", return_value=True) as mock_install:
            with mock.patch("importlib.util.find_spec", return_value=object()):
                with mock.patch.dict(sys.modules, {mcp_pack.MODULE: object()}):
                    result = install_mcp_pack()
        self.assertTrue(result["active"])
        mock_install.assert_called_once()
        self.assertEqual(len(sys.meta_path), self._meta_path_len, "no lazy hook needed")

    @unittest.skipUnless(importlib.util.find_spec("mcp") is not None, "mcp SDK not installed")
    def test_real_end_to_end_lazy_arm_and_teardown(self) -> None:
        # No mocking: exercise the real decision against whatever state this
        # process happens to be in (mcp may already be imported by another
        # test module) — either branch must return a working, reversible result.
        result = install_mcp_pack()
        self.assertTrue(result["active"])
        self.assertEqual(result["name"], "mcp")
        result["uninstall"]()  # must not raise, whichever branch it took


@unittest.skipUnless(importlib.util.find_spec("mcp") is not None, "mcp SDK not installed")
class RealSdkShapeTest(unittest.TestCase):
    """When the real SDK happens to be present, verify our seam assumptions
    against the actual `ClientSession` class rather than only our own fake."""

    def test_real_client_session_send_request_is_patchable_and_reversible(self) -> None:
        from mcp.client.session import ClientSession

        original = ClientSession.send_request
        uninstall = patch_client_session(ClientSession)
        try:
            self.assertTrue(getattr(ClientSession.send_request, WRAPPED_ATTR, False))
        finally:
            uninstall()
        self.assertIs(ClientSession.send_request, original)


if __name__ == "__main__":
    unittest.main()
