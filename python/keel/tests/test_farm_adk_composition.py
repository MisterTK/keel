"""The composition E2E (WS3 Task 3): a genuine ``google-adk`` agent driving a
real ``MCPToolset`` over a real stdio MCP server (``fixtures/mcp_stdio_server.
py``), with Keel's ``adk_pack`` (tool rebind) AND ``mcp_pack`` (transport
seam) BOTH live at once. Runs ONLY under ``KEEL_ADAPTER_FARM=1`` (see
``test_farm_adk.py``'s module docs for the shared convention); the offline
fast path is ``test_packs_adk.py``/``test_packs_mcp.py`` against structural
fakes, which can only assert Keel's OWN contract, not that a real, unscripted
``LlmAgent`` + ``Runner`` + ``MCPToolset`` triple actually exercises it. This
module pins, for real:

* real agent-level ``before_tool_callback`` execution (the WS1 carry-forward
  claim that rebind-on-first-sight preserves ADK's own callback sequencing —
  ``test_farm_adk.py`` proved this against a bare ``PluginManager`` call; this
  module proves it through the FULL ``Runner.run_async`` agent loop);
* real ``on_tool_error``/graceful-error behavior for an MCP tool failure,
  with the active ``_MCP_GRACEFUL_ERROR_HANDLING`` mode DETECTED at runtime
  (never assumed) and the matching branch asserted;
* both discovery layers (``tool:<name>`` from ``adk_pack``'s rebind,
  ``mcp:<server>`` from ``mcp_pack``'s transport patch) seeing the SAME
  underlying traffic, from one real end-to-end agent turn.

Certified against google-adk==2.4.0 + mcp==1.28.1 co-installed in one venv
(the exact pins Task 1 proved coexist; ``google.adk.tools.mcp_tool`` imports
``mcp`` at module level regardless of ADK's own declared dependencies).

## The ``always_fails`` design (why it crashes the process instead of raising)

The obvious design for an MCP tool that "always fails" is a Python function
that raises. Empirically verified in a scratch venv (this exact pin) that
this does NOT exercise the MCP-graceful-error path at all: the real MCP
protocol swallows a tool's own business-logic exception into a *successful*
JSON-RPC response (``CallToolResult(isError=True)``) — already established by
``test_farm_mcp.py``'s own module docstring/tests for the bare transport
seam — so ``McpTool._run_async_impl`` (``mcp_tool.py``) never sees an
exception to catch; it just ``response.model_dump()``s the ``isError=True``
result and returns ``{"content": [...], "isError": True}`` unchanged. That
shape does NOT match ``adk_pack._is_mcp_error_dict`` (which requires EXACTLY
``{"error": "<str>"}``), so a plain raising tool is silently recorded as a
Keel SUCCESS at the ``tool:`` layer — the graceful-error path
(``_is_mcp_error_dict`` / ``_McpErrorDict``) is real code guarding a DIFFERENT
failure mode: a transport-level crash mid-call (``McpTool.run_async``'s outer
``except McpError`` / ``except Exception``, fed by
``SessionContext._run_guarded`` racing the background session task — see
``mcp_session_manager.py`` module docs: "the 5-minute hang seen when Model
Armor... returns a 403 mid-tool-call").

So ``fixtures/mcp_stdio_server.py``'s ``always_fails`` tool hard-crashes the
server process (``os._exit(1)``) instead of raising — a genuine transport
failure, verified empirically to produce exactly
``{"error": "MCP tool execution failed: Connection closed"}`` through the
real ``McpTool.run_async``, matching ``_is_mcp_error_dict``'s shape. ADK's own
``retry_on_errors`` decorator (``mcp_session_manager.py``) retries the crashed
call exactly ONCE, transparently respawning the stdio subprocess via a fresh
``create_session()`` — the retry ALSO crashes (the tool always fails), and
THAT second failure is what finally surfaces as the ``{"error": ...}`` dict.
This means the real transport sees TWO separate ``tools/call`` attempts for
one agent-visible tool invocation — both independently visible to
``mcp:farm-fixture`` (mcp_pack.py's documented claim: "McpTool's own single
blind retry runs beneath Keel; each underlying JSON-RPC attempt is
separately visible... which sees raw failures regardless of the graceful
flag") — while the ``tool:always_fails`` layer sees exactly ONE call (the
single ``adk_pack``-wrapped ``run_async`` invocation), confirming Keel itself
never adds a retry of its own on top of ADK's.

## Graceful-error mode detection

``google.adk.features.is_feature_enabled(FeatureName._MCP_GRACEFUL_ERROR_
HANDLING)`` is read at runtime (never assumed) to pick the correct branch
of assertion (c). At the certified pin (google-adk==2.4.0) this feature is
registered ``default_on=True`` (``_feature_registry.py``), so the graceful
branch is the one this module actually exercises and certifies; the
non-graceful branch's real behavior (an uncaught exception propagating to
``on_tool_error_callback``) is documented but not separately driven here —
ADK's own module docs state the non-graceful path "awaits the call directly"
with no background-task race, i.e. a genuine transport crash there risks the
same ~300s hang the graceful path was built to fix, which is not something
to provoke in CI. The whole run is wrapped in ``asyncio.wait_for`` as a
belt-and-braces guard against exactly that, in case a future pin bump ever
flips the default.

## Bonus leg (Gemini + HttpOptions)

``google.adk.models.google_llm.Gemini`` exposes a first-class ``base_url``
field (no subclassing needed, contrary to the module's own "for full control,
subclass Gemini" docstring, which is about location/project/credentials, not
this specific field) — ``Gemini(model=..., base_url=<fault-server-url>)``
routes real ``google.genai`` traffic through ``keel.adapters.httpx_pack`` to
a local ``FaultServer``, exactly the ``test_packs_llm_e2e.py`` ``map_host``
pattern. This worked on the FIRST attempt (no bail-out needed) — see
``GeminiBonusLegTest`` below.
"""

from __future__ import annotations

import asyncio
import os
import sqlite3
import sys
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from types import MappingProxyType
from typing import Any
from unittest import mock

FARM = os.environ.get("KEEL_ADAPTER_FARM") == "1"
SKIP = "KEEL_ADAPTER_FARM=1 not set (offline fast path — see test_packs_adk.py / test_packs_mcp.py)"

if FARM:  # real imports only in farm mode — never at fast-path collection time
    from google.adk.agents.llm_agent import LlmAgent
    from google.adk.features import FeatureName, is_feature_enabled
    from google.adk.models.base_llm import BaseLlm
    from google.adk.models.google_llm import Gemini
    from google.adk.models.llm_request import LlmRequest
    from google.adk.models.llm_response import LlmResponse
    from google.adk.plugins.base_plugin import BasePlugin
    from google.adk.runners import InMemoryRunner, Runner
    from google.adk.tools.mcp_tool.mcp_session_manager import StdioConnectionParams
    from google.adk.tools.mcp_tool.mcp_toolset import MCPToolset
    from google.genai import types
    from mcp import StdioServerParameters
    from mcp.client.session import ClientSession

from keel import _runtime
from keel._backend import load_backend
from keel._discovery import Discovery
from keel.adapters import _http
from keel.bootstrap import install_keel, uninstall_keel
from keel.packs import adk_pack, mcp_pack

from .faultserver import FaultServer, ok

FIXTURE = Path(__file__).resolve().parent / "fixtures" / "mcp_stdio_server.py"

_JSON = {"Content-Type": "application/json"}
_GEMINI_BODY = (
    b'{"candidates":[{"content":{"role":"model","parts":[{"text":"hi from the fault server"}]},'
    b'"finishReason":"STOP"}]}'
)


def _echo_call() -> "types.Part":
    return types.Part(function_call=types.FunctionCall(name="echo", args={"text": "keel-composition"}))


def _always_fails_call() -> "types.Part":
    return types.Part(function_call=types.FunctionCall(name="always_fails", args={}))


def _final_text() -> "types.Part":
    return types.Part(text="turn complete")


if FARM:

    class ScriptedModel(BaseLlm):
        """A ``BaseLlm`` that never calls out to a real model: turn 1 invokes
        the real MCP ``echo`` tool, turn 2 invokes the real MCP
        ``always_fails`` tool, turn 3 returns plain text — driving the agent
        loop through exactly one real tool success and one real tool
        failure, end to end."""

        model: str = "scripted-composition-model"
        turn: int = 0

        async def generate_content_async(self, llm_request: Any, stream: bool = False):
            self.turn += 1
            if self.turn == 1:
                part = _echo_call()
            elif self.turn == 2:
                part = _always_fails_call()
            else:
                part = _final_text()
            yield LlmResponse(content=types.Content(role="model", parts=[part]), partial=False)


@unittest.skipUnless(FARM, SKIP)
class AdkMcpCompositionTest(unittest.TestCase):
    """The launch pitch, executed for real: real ``LlmAgent`` + real
    ``InMemoryRunner`` + real ``MCPToolset`` over a real stdio server, with
    ``adk_pack`` and ``mcp_pack`` both installed."""

    def setUp(self) -> None:
        adk_pack._noted_skips.clear()
        adk_pack._noted_fallbacks.clear()
        adk_pack._rebound.clear()
        self._tmp = TemporaryDirectory()
        self.cwd = Path(self._tmp.name)
        self.backend = load_backend("stub")
        self.backend.configure(
            {
                "target": {
                    "tool:echo": {},
                    "tool:always_fails": {},
                    "mcp:farm-fixture": {},
                }
            }
        )
        self.discovery = Discovery(self.cwd)
        _runtime.set_runtime(self.backend, self.discovery)
        # Captured BEFORE install() so assertion (e) has a real pristine
        # baseline to compare against, not an assumption about what "restored"
        # means.
        self._pristine_runner_init = Runner.__init__
        self._pristine_send_request = ClientSession.send_request
        adk_pack.install()
        mcp_pack.install()

    def tearDown(self) -> None:
        adk_pack.uninstall()
        mcp_pack.uninstall()
        _runtime.clear_runtime()
        self.discovery.close()
        self._tmp.cleanup()

    def read_rows(self) -> dict[str, sqlite3.Row]:
        # discovery is re-openable (lazy _connect) — tearDown's own close()
        # on an already-closed connection afterward is a safe no-op.
        self.discovery.close()
        conn = sqlite3.connect(self.discovery.db_path)
        conn.row_factory = sqlite3.Row
        try:
            return {r["target"]: r for r in conn.execute("SELECT * FROM discovery")}
        finally:
            conn.close()

    def test_composition_end_to_end(self) -> None:
        before_tool_seen: list[Any] = []

        async def agent_before_tool_callback(tool: Any, args: dict[str, Any], tool_context: Any) -> None:
            # Positional signature — the REAL agent-level BeforeToolCallback
            # shape (llm_agent.py: Callable[[BaseTool, dict, ToolContext], ...]),
            # distinct from the plugin's keyword-only shape.
            before_tool_seen.append(tool)
            return None

        on_tool_error_seen: list[tuple[str, str]] = []

        class ErrorRecorder(BasePlugin):
            def __init__(self) -> None:
                super().__init__(name="composition-error-recorder")

            async def on_tool_error_callback(
                self, *, tool: Any, tool_args: dict[str, Any], tool_context: Any, error: Exception
            ) -> None:
                on_tool_error_seen.append((tool.name, repr(error)))
                return None

        params = StdioConnectionParams(
            server_params=StdioServerParameters(command=sys.executable, args=[str(FIXTURE)]),
            timeout=10,
        )
        toolset = MCPToolset(connection_params=params)
        model = ScriptedModel()
        agent = LlmAgent(
            name="composer",
            model=model,
            tools=[toolset],
            before_tool_callback=agent_before_tool_callback,
        )
        runner = InMemoryRunner(agent=agent, app_name="farm-composition", plugins=[ErrorRecorder()])

        async def drive() -> list[Any]:
            try:
                session = await runner.session_service.create_session(app_name="farm-composition", user_id="u1")
                events = []
                async for event in runner.run_async(
                    user_id="u1",
                    session_id=session.id,
                    new_message=types.Content(role="user", parts=[types.Part(text="go")]),
                ):
                    events.append(event)
                return events
            finally:
                # Closed in the SAME event loop drive() ran in — closing the
                # toolset from a second, separate asyncio.run() call leaves
                # its stdio session cleanup referencing an already-closed
                # loop ("resources may be leaked").
                await toolset.close()

        events = asyncio.run(asyncio.wait_for(drive(), timeout=30))

        self.assertTrue(events, "the agent turn produced events")
        self.assertEqual(model.turn, 3, "the scripted model was driven through all three turns")

        # --- (a) both discovery layers saw traffic -----------------------
        rows = self.read_rows()
        self.assertIn("tool:echo", rows)
        self.assertEqual(rows["tool:echo"]["successes"], 1)
        self.assertEqual(rows["tool:echo"]["failures"], 0)
        self.assertIn("mcp:farm-fixture", rows)
        self.assertGreater(rows["mcp:farm-fixture"]["calls"], 0)

        # --- (b) the agent-level before_tool_callback fired for echo -----
        # (the WS1 claim, now proven through the REAL PluginManager +
        # Runner.run_async loop, not just a bare plugin-manager call.)
        seen_names = [t.name for t in before_tool_seen]
        self.assertIn("echo", seen_names)
        self.assertIn("always_fails", seen_names)
        echo_tool = next(t for t in before_tool_seen if t.name == "echo")
        always_fails_tool = next(t for t in before_tool_seen if t.name == "always_fails")
        self.assertTrue(
            getattr(echo_tool.run_async, adk_pack._REBOUND_ATTR, False),
            "keel rebound the real McpTool on first sight",
        )

        # --- (c) graceful-error mode detected + matching branch ----------
        graceful = is_feature_enabled(FeatureName._MCP_GRACEFUL_ERROR_HANDLING)
        print(f"[composition] _MCP_GRACEFUL_ERROR_HANDLING active: {graceful}")
        self.assertIn("tool:always_fails", rows)
        if graceful:
            # ADK converts the transport crash into {"error": "..."} instead
            # of raising — Keel's _is_mcp_error_dict classification turns
            # that into a recorded FAILURE at the tool: layer, and
            # on_tool_error never fires (the rebound run_async returns the
            # payload normally, it never raises to ADK's executor).
            self.assertEqual(on_tool_error_seen, [])
            self.assertEqual(rows["tool:always_fails"]["successes"], 0)
            self.assertEqual(rows["tool:always_fails"]["failures"], 1)
        else:
            # Pre-fix ADK behavior: the crash's exception propagates as-is
            # through the rebound run_async, and ADK's own on_tool_error
            # plugin hook fires with the ORIGINAL exception (never
            # RuntimeError-wrapped) — still recorded as a tool: failure.
            self.assertTrue(any(name == "always_fails" for name, _ in on_tool_error_seen))
            self.assertEqual(rows["tool:always_fails"]["failures"], 1)

        # --- (d) tools/call was never auto-retried BY KEEL ----------------
        # ADK's OWN retry_on_errors invokes tools/call twice for the crashed
        # always_fails call (its documented single blind retry, respawning
        # the subprocess) — both attempts are separately visible here, but
        # Keel itself never retries a tools/call (Level 0: non-idempotent).
        self.assertEqual(rows["mcp:farm-fixture"]["retries"], 0)

        # --- (e) uninstall leaves the real classes pristine ---------------
        adk_pack.uninstall()
        mcp_pack.uninstall()
        self.assertIs(Runner.__init__, self._pristine_runner_init)
        self.assertIs(ClientSession.send_request, self._pristine_send_request)
        self.assertFalse(getattr(echo_tool.run_async, adk_pack._REBOUND_ATTR, False))
        self.assertFalse(getattr(always_fails_tool.run_async, adk_pack._REBOUND_ATTR, False))
        # tearDown calls uninstall() again — both packs' uninstall() are
        # documented idempotent no-ops when already uninstalled.


@unittest.skipUnless(FARM, SKIP)
class GeminiBonusLegTest(unittest.TestCase):
    """Bonus step (brief-authorized bail-out if this resists a real attempt):
    a real ``google.adk.models.google_llm.Gemini`` model routed at a local
    ``FaultServer`` via its own ``base_url`` field (``google.genai``'s
    ``HttpOptions`` seam, one layer down) plus the ``test_packs_llm_e2e.py``
    ``map_host`` pattern. This worked on the FIRST focused attempt — no
    subclassing of ``Gemini`` was needed, contrary to the module's own "for
    full control, subclass Gemini" docstring (that guidance is about
    location/project/credentials; ``base_url`` is already a first-class
    field). No bail-out needed."""

    def setUp(self) -> None:
        self._tmp = TemporaryDirectory()
        self.cwd = self._tmp.name

    def tearDown(self) -> None:
        uninstall_keel()
        self._tmp.cleanup()

    @staticmethod
    def map_host(host: str, provider: str):
        merged = {**dict(_http.LLM_HOST_PROVIDERS), host: provider}
        return mock.patch.object(_http, "LLM_HOST_PROVIDERS", MappingProxyType(merged))

    def test_gemini_base_url_reaches_llm_google_genai_target(self) -> None:
        with mock.patch.dict(os.environ, {"GOOGLE_API_KEY": "farm-fixture-fake-key"}):
            with self.map_host("127.0.0.1", "google-genai"):
                with FaultServer([ok(_GEMINI_BODY, _JSON)]) as srv:
                    result = install_keel(cwd=self.cwd, env={"KEEL_BACKEND": "stub", "KEEL_QUIET": "1"})
                    backend = result["backend"]

                    model = Gemini(model="gemini-2.5-flash", base_url=srv.url(""))
                    req = LlmRequest(
                        model="gemini-2.5-flash",
                        contents=[types.Content(role="user", parts=[types.Part(text="hi")])],
                    )

                    async def drive() -> list[Any]:
                        return [r async for r in model.generate_content_async(req, stream=False)]

                    responses = asyncio.run(asyncio.wait_for(drive(), timeout=15))

                    self.assertEqual(srv.served, 1)
                    self.assertEqual(len(responses), 1)
                    self.assertEqual(responses[0].content.parts[0].text, "hi from the fault server")
                    report = backend.report()["targets"]
                    self.assertIn("llm:google-genai", report)
                    self.assertEqual(report["llm:google-genai"]["successes"], 1)


if __name__ == "__main__":
    unittest.main()
