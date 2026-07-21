"""Closes issue #22 item (d): the LAST untested seam of the WS3/WS5
composition story. Two facts were each proven separately, but never
together:

* ``test_farm_adk_composition.py``'s ``AdkMcpCompositionTest`` proves real
  ``google-adk`` (``LlmAgent`` + ``InMemoryRunner`` + a real ``MCPToolset``
  over a real stdio MCP server) drives Keel's ``adk_pack``/``mcp_pack``
  seams correctly — against the pure-Python STUB backend, and with no
  ``[flows]`` entrypoint designated (Tier 1 only).
* ``test_adk_runner_flows_native.py`` proves the designated Runner-flow wrap
  (``adk_pack._run_async_wrapper``) round-trips correctly against the REAL
  native (maturin-built) journal — but only through ``FakeAdkModules``, a
  structural double, never a real ADK install.

This module is the composition of both: a designated ``Runner.run_async``
flow, driven by a REAL ``InMemoryRunner`` + real ``MCPToolset`` (over the
same stdio fixture the farm composition test uses), journaling into the REAL
native ``keel_core`` — i.e. the whole production stack, for real, at once.
Runs ONLY under ``KEEL_ADAPTER_FARM=1`` AND a built native module (skips
cleanly otherwise — see ``SKIP`` below); the CI leg that actually exercises
it is the new ``adapter-farm.yml`` job added alongside this file (a native
build step, unlike every other job in that workflow).

## Why this module does NOT install ``mcp_pack`` (an issue #22(d) finding,
## fixed by issue #38 — kept installing ``adk_pack`` only pending farm re-cert)

Empirically verified in a scratch venv (google-adk==2.4.0 + mcp==1.28.1 +
this repo's maturin-built ``keel_core``): installing BOTH ``adk_pack`` AND
``mcp_pack`` while a Tier 2 flow is DESIGNATED and OPEN deadlocked the
process on the very first real MCP tool call — every other combination tried
(either pack alone with an open flow; both packs together against the native
backend with NO flow open, i.e. plain Tier 1) completed normally. Root cause
(issue #38, fixed): ``adk_pack``'s ``tool:echo`` wrap and ``mcp_pack``'s
``mcp:farm-fixture`` wrap both called into ``execute_async`` for the SAME
outgoing call while the SAME flow was open — the outer call held
``active_flow``'s lock for its whole step, so the inner call's
``.lock().await`` could never be granted. Fixed in ``crates/keel-py/src/lib.rs``
(``execute_async``'s nested-call passthrough, ``async_in_effect``/
``guarded_awaitable``) and proven with a minimal bare-``KeelCore`` repro at
``test_flows.py``'s
``NativeFlowReplayTest.test_nested_execute_async_from_inside_an_open_flow_passes_through``
— no ADK/MCP involved, confirming it was core-level re-entrancy, not specific
to this pack pairing.

This module still installs ``adk_pack`` only, NOT because the deadlock is
unfixed, but because flipping it to install both packs needs re-certifying
against the real, pinned ``google-adk``/``mcp`` libs (this environment has
neither installed) before trusting it in the composition CI leg — tracked as
a fast-follow rather than flipped blind in the same change as the core fix.

Consequently: every test class below installs ``adk_pack`` only. Real
``MCPToolset``/``McpTool`` traffic still flows for real (``adk_pack``'s own
tool-rebind wraps ``McpTool.run_async`` directly — see ``_rebind_tool`` — so
``mcp_pack`` is not required for the MCP tool call itself to be real,
policy-wrapped, and journaled; it only adds the SEPARATE ``mcp:<server>``
transport-layer discovery target, which is deliberately not under test
here). The crash/resume test independently verifies real-vs-replayed
transport traffic with a plain counting wrap around
``mcp.client.session.ClientSession.send_request`` (the same chokepoint
``mcp_pack`` targets — see ``_request_method`` — but a bare counter, not a
full pack install), so the "no re-invocation on replay" claim is still
proven against the REAL transport, not just against journal row counts.

## Resume identity: the content-fingerprint fallback, not ADK's own resume

``Runner.run_async`` accepts an ``invocation_id`` explicitly meant "to
resume an interrupted invocation" — real ADK machinery, independent of
Keel's own. Passing the SAME explicit ``invocation_id`` across the
abandon/re-enter pair would conflate ADK's own resume semantics with Keel's,
an unnecessary confound. This module instead leaves ``invocation_id`` unset
on both calls, relying on ``_runner_flow_identity``'s documented fallback:
when ADK hands back no ``invocation_id``, the identity is keyed off
``_content_fingerprint(new_message)`` — so two calls with byte-identical
``new_message`` content resolve to the SAME Keel flow identity while each
remains an entirely ordinary, fresh ADK invocation from ADK's own point of
view. A FRESH ``InMemoryRunner`` (own session service) + FRESH ``MCPToolset``
(own stdio subprocess) is used for the second call — explicitly creating a
session under the FIRST run's session id (``InMemorySessionService`` auto-
creates on ``create_session`` regardless) — matching the real "a crash kills
the process; a NEW process re-enters" story this whole amendment exists for,
never a same-process retry.

## The abandon point

Verified empirically (a throwaway probe against the real event stream): a
successful ADK tool-call turn yields exactly two events in order — the
model's ``function_call`` event, then the ``function_response`` event once
the tool has actually run — before the NEXT model turn is invoked at all.
Rather than hard-code that as a magic "pull 2 events", the crash/resume
test below pulls events until ``_has_function_response`` sees one, which is
self-describing and survives a future ADK event-shape change better than a
count would.
"""

from __future__ import annotations

import asyncio
import os
import sqlite3
import sys
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any
from unittest import mock

FARM = os.environ.get("KEEL_ADAPTER_FARM") == "1"
SKIP = (
    "KEEL_ADAPTER_FARM=1 not set (offline fast path — see test_packs_adk.py / "
    "test_farm_adk_composition.py / test_adk_runner_flows_native.py)"
)

try:
    import keel_core  # noqa: F401

    _NATIVE = True
except ImportError:
    _NATIVE = False

_NATIVE_SKIP = "keel_core native module not built (maturin develop in crates/keel-py)"

if FARM:  # real imports only in farm mode — never at fast-path collection time
    from google.adk.agents.llm_agent import LlmAgent
    from google.adk.models.base_llm import BaseLlm
    from google.adk.models.llm_response import LlmResponse
    from google.adk.runners import InMemoryRunner
    from google.adk.tools.mcp_tool.mcp_session_manager import StdioConnectionParams
    from google.adk.tools.mcp_tool.mcp_toolset import MCPToolset
    from google.genai import types
    from mcp import StdioServerParameters
    from mcp.client.session import ClientSession

from keel import _runtime
from keel._backend import load_backend
from keel._discovery import Discovery
from keel._policy import FlowEntrypoint
from keel.packs import adk_pack
from keel.packs.mcp_pack import _request_method

FIXTURE = Path(__file__).resolve().parent / "fixtures" / "mcp_stdio_server.py"


def _echo_call(text: str) -> "types.Part":
    return types.Part(function_call=types.FunctionCall(name="echo", args={"text": text}))


def _final_text() -> "types.Part":
    return types.Part(text="turn complete")


def _new_message() -> "types.Content":
    """A fresh, field-identical ``Content`` each call — ``repr()``-stable
    (no random ids), so two separately-constructed calls fingerprint the
    same (see module docs: the resume identity relies on this)."""
    return types.Content(role="user", parts=[types.Part(text="go")])


def _has_function_response(event: Any) -> bool:
    content = getattr(event, "content", None)
    parts = getattr(content, "parts", None) or []
    return any(getattr(p, "function_response", None) is not None for p in parts)


if FARM:

    class ScriptedModel(BaseLlm):
        """A ``BaseLlm`` that never calls out to a real model: turn 1 drives
        the real MCP ``echo`` tool with one argument, turn 2 drives it AGAIN
        with a different argument (two distinct, real, sequential effects —
        the crash/resume test's substitution story needs two), turn 3 ends
        the invocation with plain text."""

        model: str = "scripted-native-composition-model"
        turn: int = 0

        async def generate_content_async(self, llm_request: Any, stream: bool = False):
            self.turn += 1
            if self.turn == 1:
                part = _echo_call("first")
            elif self.turn == 2:
                part = _echo_call("second")
            else:
                part = _final_text()
            yield LlmResponse(content=types.Content(role="model", parts=[part]), partial=False)


class _NativeAdkCompositionTestBase(unittest.TestCase):
    """Shared fixture: a designated Runner-flow entrypoint, a REAL native
    backend with an on-disk journal at ``<cwd>/.keel/journal.db``, and
    helpers to build a real ``InMemoryRunner`` + real ``MCPToolset`` pair
    over the farm fixture server. Mirrors ``_NativeAdkFlowTestBase``
    (``test_adk_runner_flows_native.py``) and ``AdkMcpCompositionTest``
    (``test_farm_adk_composition.py``) at once."""

    def setUp(self) -> None:
        adk_pack._noted_skips.clear()
        adk_pack._noted_fallbacks.clear()
        adk_pack._rebound.clear()
        adk_pack._noted_busy = False
        self._tmp = TemporaryDirectory()
        self.cwd = Path(self._tmp.name)
        self.journal_path = self.cwd / ".keel" / "journal.db"
        self._discoveries: list[Discovery] = []

    def tearDown(self) -> None:
        adk_pack.uninstall()
        _runtime.clear_runtime()
        for d in self._discoveries:
            d.close()
        self._tmp.cleanup()

    def designate(self) -> None:
        entry = FlowEntrypoint(
            raw=adk_pack.RUNNER_FLOW_ENTRYPOINT, module="google.adk.runners", function="Runner.run_async"
        )
        _runtime.set_flow_entrypoints([entry])

    def native_backend(self) -> Any:
        backend = load_backend("native", cwd=self.cwd)
        backend.configure({})
        discovery = Discovery(self.cwd)
        self._discoveries.append(discovery)
        _runtime.set_runtime(backend, discovery)
        return backend

    def make_toolset_and_agent(self, model: "ScriptedModel") -> tuple[Any, Any]:
        params = StdioConnectionParams(
            server_params=StdioServerParameters(command=sys.executable, args=[str(FIXTURE)]),
            timeout=10,
        )
        toolset = MCPToolset(connection_params=params)
        agent = LlmAgent(name="composer", model=model, tools=[toolset])
        return toolset, agent

    # -- direct sqlite3 helpers over the on-disk journal ---------------------
    # NOTE (see EndToEndRecoveryTest's "poisoned write" note in
    # test_adk_runner_flows_native.py, verified independently again for this
    # module): opening a separate sqlite3 connection against journal.db WHILE
    # this test's own native KeelCore instance is still alive between two of
    # its own enter_flow/exit_flow calls silently drops that core's
    # SUBSEQUENT writes. Every read below is deferred to AFTER all flow work
    # is fully done — never interleaved.

    def _conn(self) -> sqlite3.Connection:
        conn = sqlite3.connect(self.journal_path)
        conn.row_factory = sqlite3.Row
        return conn

    def _flow_row(self) -> sqlite3.Row:
        conn = self._conn()
        try:
            rows = conn.execute("SELECT * FROM flows").fetchall()
            self.assertEqual(len(rows), 1, "exactly one flow row expected in this journal")
            return rows[0]
        finally:
            conn.close()

    def _steps(self, flow_id: str) -> list[sqlite3.Row]:
        conn = self._conn()
        try:
            return conn.execute(
                "SELECT * FROM steps WHERE flow_id = ? ORDER BY seq", (flow_id,)
            ).fetchall()
        finally:
            conn.close()


@unittest.skipUnless(FARM and _NATIVE, SKIP if not FARM else _NATIVE_SKIP)
class RealAdkNativeFlowEndToEndTest(_NativeAdkCompositionTestBase):
    """A designated Runner flow, driven by a real InMemoryRunner + real
    MCPToolset over the real stdio fixture, completes end-to-end with its
    steps landing in the REAL native journal — the brief's "at minimum"
    bar."""

    def test_full_lifecycle_lands_in_the_real_native_journal(self) -> None:
        self.designate()
        self.native_backend()
        adk_pack.install()

        model = ScriptedModel()
        toolset, agent = self.make_toolset_and_agent(model)
        runner = InMemoryRunner(agent=agent, app_name="native-composition")

        async def drive() -> list[Any]:
            try:
                session = await runner.session_service.create_session(
                    app_name="native-composition", user_id="u1"
                )
                events = []
                async for event in runner.run_async(
                    user_id="u1", session_id=session.id, new_message=_new_message()
                ):
                    events.append(event)
                return events
            finally:
                await toolset.close()

        events = asyncio.run(asyncio.wait_for(drive(), timeout=30))

        self.assertEqual(len(events), 5, "function_call+response x2, then final text")
        self.assertEqual(model.turn, 3)
        self.assertFalse(_runtime.in_active_flow())

        flow = self._flow_row()
        self.assertEqual(flow["status"], "completed")
        self.assertEqual(flow["entrypoint"], adk_pack.RUNNER_FLOW_ENTRYPOINT)

        steps = self._steps(flow["flow_id"])
        markers = [s for s in steps if s["kind"] == "marker"]
        randoms = [s for s in steps if s["kind"] == "random"]
        effects = [s for s in steps if s["kind"] == "effect"]
        self.assertEqual(len(markers), 1)
        self.assertEqual(markers[0]["seq"], 0)
        self.assertEqual(markers[0]["attempt"], 1, "a clean single-attempt run")
        self.assertEqual(len(randoms), 1)
        self.assertEqual(randoms[0]["step_key"], "adk:invocation_id")
        self.assertEqual(
            [s["step_key"] for s in effects],
            ["tool:echo#-", "tool:echo#-"],
            "both real MCP echo calls journaled as effects, in call order",
        )
        self.assertLess(randoms[0]["seq"], effects[0]["seq"], "effects admit after correlation")
        self.assertLess(effects[0]["seq"], effects[1]["seq"])


@unittest.skipUnless(FARM and _NATIVE, SKIP if not FARM else _NATIVE_SKIP)
class RealAdkNativeFlowCrashResumeTest(_NativeAdkCompositionTestBase):
    """The brief's "ideally also" bar: the amendment's whole reason for
    existing (abandon -> re-enter -> substitute -> complete), proven with
    REAL ADK objects over the REAL native journal — the real-object twin of
    ``test_adk_runner_flows_native.EndToEndRecoveryTest``."""

    def _patched_send_request_counter(self) -> tuple[Any, dict[str, int]]:
        """A bare counting wrap around the REAL
        ``ClientSession.send_request`` (mcp_pack's own patch chokepoint —
        see module docs for why mcp_pack itself is not installed here):
        counts real ``tools/call`` JSON-RPC requests, independent of Keel's
        own journal, so the "replay never re-hits the real transport" claim
        is verified against the transport itself, not just row counts."""
        counts = {"tools/call": 0}
        original = ClientSession.send_request

        async def counting(self: Any, request: Any, *args: Any, **kwargs: Any) -> Any:
            if _request_method(request) == "tools/call":
                counts["tools/call"] += 1
            return await original(self, request, *args, **kwargs)

        return mock.patch.object(ClientSession, "send_request", counting), counts

    def test_abandon_then_reenter_substitutes_the_first_real_effect_and_completes(self) -> None:
        self.designate()
        self.native_backend()
        adk_pack.install()
        patcher, counts = self._patched_send_request_counter()
        patcher.start()
        self.addCleanup(patcher.stop)

        # ---- run 1: abandon right after the FIRST tool effect lands -------
        model1 = ScriptedModel()
        toolset1, agent1 = self.make_toolset_and_agent(model1)
        runner1 = InMemoryRunner(agent=agent1, app_name="native-composition")
        msg1 = _new_message()
        session_ids: dict[str, str] = {}

        async def abandon() -> list[Any]:
            try:
                session = await runner1.session_service.create_session(
                    app_name="native-composition", user_id="u1"
                )
                session_ids["id"] = session.id
                gen = runner1.run_async(user_id="u1", session_id=session.id, new_message=msg1)
                collected: list[Any] = []
                try:
                    while True:
                        event = await gen.__anext__()
                        collected.append(event)
                        if _has_function_response(event):
                            break
                finally:
                    await gen.aclose()
                return collected
            finally:
                await toolset1.close()

        run1_events = asyncio.run(asyncio.wait_for(abandon(), timeout=30))

        self.assertEqual(len(run1_events), 2, "function_call, then function_response — one full tool turn")
        self.assertEqual(counts["tools/call"], 1, "exactly one REAL tools/call for the first echo")
        self.assertFalse(_runtime.in_active_flow(), "abandonment released the flow handle")

        # No journal read here — see the class docs' "poisoned write" note;
        # every assertion is deferred to after ALL flow work (both runs).

        # ---- run 2: a FRESH runner + FRESH toolset/subprocess + FRESH -----
        # session (same session id), SAME message content -> same Keel flow
        # identity via the content-fingerprint fallback (module docs).
        model2 = ScriptedModel()
        toolset2, agent2 = self.make_toolset_and_agent(model2)
        runner2 = InMemoryRunner(agent=agent2, app_name="native-composition")
        msg2 = _new_message()
        self.assertEqual(repr(msg1), repr(msg2), "sanity: the fallback identity needs byte-identical repr")

        # A FRESH runner has its OWN (empty) InMemorySessionService — explicitly
        # create a session under run 1's SAME session id (the real "a crash
        # kills the process; a new process re-enters" story: nothing carries
        # over except the identifiers a caller would persist itself).
        async def resume() -> list[Any]:
            try:
                session = await runner2.session_service.create_session(
                    app_name="native-composition", user_id="u1", session_id=session_ids["id"]
                )
                events: list[Any] = []
                async for event in runner2.run_async(
                    user_id="u1", session_id=session.id, new_message=msg2
                ):
                    events.append(event)
                return events
            finally:
                await toolset2.close()

        run2_events = asyncio.run(asyncio.wait_for(resume(), timeout=30))

        self.assertEqual(len(run2_events), 5, "the resumed run completes normally, all the way through")
        self.assertEqual(model2.turn, 3)
        self.assertEqual(
            counts["tools/call"],
            2,
            "exactly ONE more real tools/call (the second echo) — the first never re-hits the transport",
        )
        self.assertFalse(_runtime.in_active_flow())

        flow = self._flow_row()
        self.assertEqual(flow["status"], "completed")
        markers = [s for s in self._steps(flow["flow_id"]) if s["kind"] == "marker"]
        effects = [s for s in self._steps(flow["flow_id"]) if s["kind"] == "effect"]
        self.assertEqual(markers[0]["attempt"], 2, "abandonment consumed attempt 1; resume is attempt 2")
        self.assertEqual(
            [s["step_key"] for s in effects],
            ["tool:echo#-", "tool:echo#-"],
            "exactly one journal row per real effect, ever — no duplicate for the substituted one",
        )


if __name__ == "__main__":
    unittest.main()
