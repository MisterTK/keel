"""Farm contract test: keel.packs.mcp_pack against the REAL mcp Python SDK.

Runs ONLY under KEEL_ADAPTER_FARM=1 (see test_farm_adk.py's module docs for
the full convention). The offline fast path is tests/test_packs_mcp.py
against a structural fake session. This module certifies, on the real
package (mcp 1.28.1 — the latest 1.x release at certification time; see
ws3-task-1-report.md for the exact pin Task 2's matrix freezes):

* ``mcp.client.session.ClientSession.send_request`` exists;
* its UNBOUND signature binds ``(self, request, result_type,
  request_read_timeout_seconds, metadata, progress_callback)`` — the pack's
  documented assumption (mcp_pack.py:228-230) — via
  ``inspect.signature(...).bind`` with sentinel values;
* patch/unpatch round-trips via the pack's public ``install()``/``uninstall()``
  (mcp_pack.py:392-417);
* the behavioral leg: a REAL client + server connected over
  ``mcp.shared.memory``'s in-memory transport (mirroring the Node farm test's
  ``InMemoryTransport`` leg) proves ``tools/call`` is classified
  non-idempotent (never retried — one attempt, then the original error,
  KEEL-E014) and ``resources/read`` is classified idempotent (retried per
  policy, KEEL-E010 only after exhausting retries) — exactly the method
  table mcp_pack.py:96-104 encodes.

No adjustment to the pack's calls was needed against the real 1.28.1 API —
``send_request``'s signature and ``_request_method``'s ``.root.method``
extraction both matched the module's documented assumptions when driven
against genuine request objects built by a real ``ClientSession``.

Behavioral-leg design note: the real MCP protocol swallows a TOOL's own
business-logic exception into a *successful* JSON-RPC response
(``CallToolResult(isError=True)``) — never a transport-level failure — so a
raising ``@server.tool()`` function can't exercise retry/no-retry behavior at
all (`send_request` never sees an error to classify). To observe the
idempotency classification for real, a connection-level fault is injected
at the exact seam this pack patches: the TRUE ``ClientSession.send_request``
is captured first, then temporarily replaced with a wrapper that raises a
``ConnectionError`` for exactly the two methods under test and calls through
to the true original otherwise; ``mcp_pack.install()`` then patches THAT
wrapper (composing correctly, since ``install()`` always wraps whatever
``send_request`` currently is). Every request in this test — the failing
ones and the passing ones — is still a real request object built by a real
``ClientSession`` against a real connected ``FastMCP`` server; only the
underlying transport call is short-circuited for the two methods being
certified.
"""

from __future__ import annotations

import asyncio
import inspect
import os
import unittest

FARM = os.environ.get("KEEL_ADAPTER_FARM") == "1"
SKIP = "KEEL_ADAPTER_FARM=1 not set (offline fast path — see test_packs_mcp.py)"

if FARM:  # real imports only in farm mode — never at fast-path collection time
    from mcp.client.session import ClientSession
    from mcp.server.fastmcp import FastMCP
    from mcp.shared.memory import create_connected_server_and_client_session

from keel import _runtime
from keel._backend import load_backend
from keel.packs import mcp_pack
from keel.packs.mcp_pack import _request_method


def _build_server() -> "FastMCP":
    server = FastMCP("farm-server")

    @server.tool()
    def real_tool() -> str:
        return "ok"

    @server.resource("res://greeting")
    def greeting() -> str:
        return "hello"

    return server


@unittest.skipUnless(FARM, SKIP)
class McpFarmContractTest(unittest.TestCase):
    def tearDown(self) -> None:
        mcp_pack.uninstall()
        _runtime.clear_runtime()

    def test_detect_reports_pinned_on_the_installed_version(self) -> None:
        det = mcp_pack.detect()
        self.assertTrue(det.matched)
        self.assertEqual(det.confidence, "pinned", f"real version {det.version} fell out of range")

    def test_send_request_exists_and_unbound_signature_binds(self) -> None:
        self.assertTrue(hasattr(ClientSession, "send_request"))
        sig = inspect.signature(ClientSession.send_request)
        sentinel = object()
        bound = sig.bind(sentinel, sentinel, sentinel, sentinel, sentinel, sentinel)
        bound.apply_defaults()
        self.assertEqual(
            list(bound.arguments.keys()),
            ["self", "request", "result_type", "request_read_timeout_seconds", "metadata", "progress_callback"],
        )

    def test_install_uninstall_round_trips_on_the_real_client_session(self) -> None:
        pristine = ClientSession.send_request
        installed = mcp_pack.install()
        self.assertTrue(installed)
        self.assertIsNot(ClientSession.send_request, pristine)
        mcp_pack.uninstall()
        self.assertIs(ClientSession.send_request, pristine)

    def test_raising_tool_yields_error_result_not_transport_exception(self) -> None:
        # Pins the premise the module's fault-injection design (see module
        # docstring above) rests on: a real MCP tool that raises its own
        # business-logic exception must surface to the client as a
        # *successful* CallToolResult(isError=True), never as a propagated
        # transport exception. This was verified empirically during the
        # original farm task but was not pinned by any test — if a future
        # mcp SDK release changes this behavior, this module's
        # fault-injection design rationale must be revisited.
        server = FastMCP("farm-raising-server")

        @server.tool()
        def raising_tool() -> str:
            raise RuntimeError("boom")

        async def drive():
            async with create_connected_server_and_client_session(server._mcp_server) as session:
                await session.initialize()
                return await session.call_tool("raising_tool", {})

        try:
            result = asyncio.run(drive())
        except Exception as exc:  # noqa: BLE001 - the premise under test is that this never fires
            self.fail(f"a raising tool must not propagate a transport exception, got {exc!r}")
        self.assertTrue(result.isError)

    def test_real_round_trip_tools_call_never_retried_resources_read_retried(self) -> None:
        server = _build_server()
        true_original = ClientSession.send_request
        attempt_counts = {"tools/call": 0, "resources/read": 0}

        async def flaky_send_request(
            self,
            request,
            result_type=None,
            request_read_timeout_seconds=None,
            metadata=None,
            progress_callback=None,
        ):
            method = _request_method(request)
            if method in attempt_counts:
                attempt_counts[method] += 1
                raise ConnectionError(f"simulated failure for {method}")
            return await true_original(
                self, request, result_type, request_read_timeout_seconds, metadata, progress_callback
            )

        # A fault-injecting wrapper sits BENEATH the pack's own patch — see
        # module docs. mcp_pack.install() wraps whatever send_request
        # currently is, so this composes correctly.
        ClientSession.send_request = flaky_send_request
        try:
            backend = load_backend("stub")
            backend.configure(
                {"target": {"mcp:farm-server": {"retry": {"attempts": 3, "on": ["conn"], "schedule": "fixed(1ms)"}}}}
            )
            _runtime.set_runtime(backend, None)
            mcp_pack.install()

            async def drive() -> tuple[Exception, Exception]:
                async with create_connected_server_and_client_session(server._mcp_server) as session:
                    await session.initialize()
                    try:
                        await session.call_tool("real_tool", {})
                        tool_exc = None
                    except Exception as exc:  # noqa: BLE001 - captured for assertions below
                        tool_exc = exc
                    try:
                        await session.read_resource("res://greeting")
                        resource_exc = None
                    except Exception as exc:  # noqa: BLE001
                        resource_exc = exc
                    return tool_exc, resource_exc

            tool_exc, resource_exc = asyncio.run(drive())

            self.assertIsInstance(tool_exc, ConnectionError)
            self.assertEqual(tool_exc.keel_outcome["error"]["code"], "KEEL-E014")
            self.assertEqual(attempt_counts["tools/call"], 1, "a side-effecting tools/call is NEVER retried")

            self.assertIsNotNone(resource_exc)
            self.assertEqual(resource_exc.keel_outcome["error"]["code"], "KEEL-E010")
            self.assertEqual(attempt_counts["resources/read"], 3, "an idempotent read retries per policy")
        finally:
            ClientSession.send_request = true_original


if __name__ == "__main__":
    unittest.main()
