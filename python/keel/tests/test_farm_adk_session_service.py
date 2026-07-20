"""Farm contract test: ``keel.packs.adk_pack.KeelSessionService`` against the
REAL pinned ``google-adk==2.4.0`` (design doc issue #15, chunk-9's
certification pass — design §8's "pointer only" test plan, resolved here).

Runs ONLY under ``KEEL_ADAPTER_FARM=1`` AND a built native module (skips
cleanly otherwise — mirrors ``test_farm_adk_composition_native.py``'s
gating exactly, since ``KeelSessionService``'s write path needs a REAL
journal-backed Tier 2 flow just like that module's composition tests). The
offline fast path is ``test_packs_adk.py``'s ``KeelSessionService*`` classes
against ``fixtures/fake_adk.py``'s structural fake and
``_FakeSessionJournalBackend`` (a double that does NOT model native-core
replay-substitution semantics for ``execute()`` steps at all — see that
fixture's own ``execute()``, which always re-invokes the effect lambda
fresh). This module is the FIRST place those replay semantics are exercised
for ``KeelSessionService`` specifically, against the real native core.

Certifies, against the real 2.4.0 package + a real maturin-built
``keel_core``:

* ``detect()`` reports ``pinned`` confidence on the installed version;
* ``KeelSessionService()`` constructs a REAL ``BaseSessionService`` subclass
  whose ``create_session``/``get_session``/``list_sessions``/
  ``delete_session`` signatures bind against the shapes this pack's own
  code calls them with (mirrors ``test_farm_langgraph.py``'s
  ``test_checkpoint_base_imports_and_keel_saver_signatures_bind``);
* a REAL ``google.adk.runners.Runner`` (not ``InMemoryRunner``, which
  hardcodes its own ``InMemorySessionService`` — design §3.4's documented
  manual wiring needs the plain ``Runner(session_service=...)`` constructor)
  wired with ``KeelSessionService``, driven through one full turn via a
  scripted ``BaseLlm`` + a real local ``FunctionTool`` (no MCP/subprocess
  needed — that composition is already covered by
  ``test_farm_adk_composition_native.py`` and is orthogonal to this
  module's job), empirically proves the REAL Runner internals call
  ``session_service.append_event(...)`` for every non-partial event
  (design §6 item 1's assumption, previously verified only by reading
  ``runners.py`` source — here it is driven and counted for real) and that
  the resulting journal holds ``session_identity``-before-``session_event``
  steps in the documented order;
* **design §3.2a's REQUIRED crash-mid-turn-then-resume convergence
  certification** — see ``RealSessionServiceCrashResumeConvergenceTest``'s
  own docstring for the result. This is the single most important thing
  this module checks: the design doc explicitly named this "an explicit
  open item for the implementation/certification pass, not resolved by
  th[e] design pass" and required this exact experiment before shipping.
* event content encoding (``inline_data``/``function_call`` parts) round
  trips through the REAL ``Event``/``Content``/``Part``/``Blob`` classes
  across the full write -> journal -> fresh-instance-read path (not just
  the private ``_encode_part``/``_decode_part`` helpers in isolation, which
  ``test_packs_adk.py``'s ``KeelSessionServiceContentEncodingTest`` already
  covers against the fake).

## Build/run ritual (CLAUDE.md's farm-cert convention)

Mirrors ``adapter-farm.yml``'s ``native-adk-composition`` job exactly::

    python3 -m venv .venv && . .venv/bin/activate
    pip install --upgrade pip maturin "google-adk==2.4.0"
    maturin develop -m crates/keel-py/Cargo.toml   # from repo root
    KEEL_ADAPTER_FARM=1 (cd python/keel && python3 -m unittest \\
        tests.test_farm_adk_session_service -v)

No ``mcp`` package needed (unlike the composition module) — this module
never touches MCP.
"""

from __future__ import annotations

import asyncio
import inspect
import os
import sqlite3
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any
from unittest import mock

FARM = os.environ.get("KEEL_ADAPTER_FARM") == "1"
SKIP = (
    "KEEL_ADAPTER_FARM=1 not set (offline fast path — see test_packs_adk.py's "
    "KeelSessionService* classes / test_farm_adk.py / test_adk_runner_flows_native.py)"
)

try:
    import keel_core  # noqa: F401

    _NATIVE = True
except ImportError:
    _NATIVE = False

_NATIVE_SKIP = "keel_core native module not built (maturin develop in crates/keel-py)"

if FARM:  # real imports only in farm mode — never at fast-path collection time
    from google.adk.events.event import Event
    from google.adk.models.base_llm import BaseLlm
    from google.adk.models.llm_response import LlmResponse
    from google.adk.runners import Runner
    from google.adk.sessions import BaseSessionService
    from google.adk.tools.function_tool import FunctionTool
    from google.genai import types

from keel import _runtime
from keel._backend import load_backend
from keel._discovery import Discovery
from keel._policy import FlowEntrypoint
from keel.packs import adk_pack


def _new_message(text: str = "go") -> "types.Content":
    """A fresh, field-identical ``Content`` each call — ``repr()``-stable (no
    random ids), matching ``test_farm_adk_composition_native.py``'s exact
    resume-identity technique: two separately-constructed calls with the
    same text fingerprint to the SAME Keel flow identity via
    ``_runner_flow_identity``'s content-fingerprint fallback."""
    return types.Content(role="user", parts=[types.Part(text=text)])


def _has_function_response(event: Any) -> bool:
    content = getattr(event, "content", None)
    parts = getattr(content, "parts", None) or []
    return any(getattr(p, "function_response", None) is not None for p in parts)


if FARM:

    class ScriptedModel(BaseLlm):
        """A ``BaseLlm`` that never calls a real model: turn 1 drives a real
        local tool with one argument, turn 2 drives it AGAIN with a
        different argument (two distinct, real, sequential tool effects —
        the crash/resume test's substitution story needs two), turn 3 ends
        the invocation with plain text. Mirrors
        ``test_farm_adk_composition_native.py``'s ``ScriptedModel`` exactly,
        swapping the real MCP ``echo`` tool for a plain local ``FunctionTool``
        (no subprocess/stdio needed — orthogonal to this module's job)."""

        model: str = "scripted-session-service-model"
        turn: int = 0

        async def generate_content_async(self, llm_request: Any, stream: bool = False):
            self.turn += 1
            if self.turn == 1:
                part = types.Part(function_call=types.FunctionCall(name="bump", args={"label": "first"}))
            elif self.turn == 2:
                part = types.Part(function_call=types.FunctionCall(name="bump", args={"label": "second"}))
            else:
                part = types.Part(text="turn complete")
            yield LlmResponse(content=types.Content(role="model", parts=[part]), partial=False)


def _bump_tool(counter: dict[str, int]) -> "FunctionTool":
    """A real local ``FunctionTool`` whose body has a REAL, observable
    side effect (`counter["n"]` incrementing) — so a replay-substituted call
    (the journaled effect's outcome reused, the Python body never re-run)
    is empirically distinguishable from a freshly re-executed one, exactly
    like ``test_adk_runner_flows_native.EndToEndRecoveryTest``'s ``bump()``
    helper and ``test_farm_adk_composition_native.py``'s real MCP ``echo``
    call-counter."""

    def bump(label: str) -> dict[str, Any]:
        counter["n"] += 1
        return {"label": label, "n": counter["n"]}

    return FunctionTool(func=bump)


class _KeelSessionServiceFarmTestBase(unittest.TestCase):
    """Shared fixture: a designated Runner-flow entrypoint over a REAL
    native backend with an on-disk journal at ``<cwd>/.keel/journal.db``.
    Mirrors ``test_farm_adk_composition_native.py``'s
    ``_NativeAdkCompositionTestBase`` and
    ``test_adk_runner_flows_native.py``'s ``_NativeAdkFlowTestBase``."""

    def setUp(self) -> None:
        adk_pack._noted_skips.clear()
        adk_pack._noted_fallbacks.clear()
        adk_pack._rebound.clear()
        adk_pack._noted_busy = False
        adk_pack._session_service_cls = None
        adk_pack._active_session_identity = None
        adk_pack._session_event_seq = 0
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

    def make_runner(self, model: "ScriptedModel", session_service: Any, app_name: str, counter: dict) -> "Runner":
        from google.adk.agents.llm_agent import LlmAgent

        tool = _bump_tool(counter)
        agent = LlmAgent(name="bumper", model=model, tools=[tool])
        return Runner(agent=agent, app_name=app_name, session_service=session_service)

    # -- direct sqlite3 helpers over the on-disk journal ---------------------
    # NOTE (same "poisoned write" precedent as test_adk_runner_flows_native.py
    # and test_farm_adk_composition_native.py): opening a separate sqlite3
    # connection against journal.db WHILE this test's own native KeelCore
    # instance is still alive between two of its own enter_flow/exit_flow
    # calls silently drops that core's SUBSEQUENT writes. Every read below is
    # deferred to AFTER all flow work is fully done — never interleaved.

    def _conn(self) -> sqlite3.Connection:
        conn = sqlite3.connect(self.journal_path)
        conn.row_factory = sqlite3.Row
        return conn

    def _flow_rows(self) -> list[sqlite3.Row]:
        conn = self._conn()
        try:
            return conn.execute("SELECT * FROM flows ORDER BY created_at").fetchall()
        finally:
            conn.close()

    def _steps(self, flow_id: str) -> list[sqlite3.Row]:
        conn = self._conn()
        try:
            return conn.execute("SELECT * FROM steps WHERE flow_id = ? ORDER BY seq", (flow_id,)).fetchall()
        finally:
            conn.close()


@unittest.skipUnless(FARM, SKIP)
class KeelSessionServiceFarmContractTest(unittest.TestCase):
    """No native backend needed for these — pure construction/introspection
    against the real ``google-adk`` package."""

    def setUp(self) -> None:
        adk_pack._session_service_cls = None

    def test_detect_reports_pinned_on_the_installed_version(self) -> None:
        det = adk_pack.detect()
        self.assertTrue(det.matched)
        self.assertEqual(det.confidence, "pinned", f"real version {det.version} fell out of range")

    def test_keel_session_service_constructs_a_real_base_session_service_subclass(self) -> None:
        svc = adk_pack.KeelSessionService(app_name="app")
        self.assertIsInstance(svc, BaseSessionService)

    def test_abstract_method_signatures_bind_against_the_real_base_class(self) -> None:
        # Mirrors test_farm_langgraph.py's
        # test_checkpoint_base_imports_and_keel_saver_signatures_bind: proves
        # this pack's hand-written override signatures actually match the
        # call shapes a real ADK Runner (and this pack's own read/write code)
        # uses, bound against the REAL BaseSessionService's four abstract
        # methods (append_event is concrete/non-abstract — design doc §
        # "ADK package verification findings" item 4 — so it's not in this
        # list, but is exercised end-to-end by the tests below).
        svc = adk_pack.KeelSessionService(app_name="app")
        inspect.signature(svc.create_session).bind(app_name="app", user_id="u1")
        inspect.signature(svc.create_session).bind(app_name="app", user_id="u1", state={"x": 1}, session_id="s1")
        inspect.signature(svc.get_session).bind(app_name="app", user_id="u1", session_id="s1")
        inspect.signature(svc.list_sessions).bind(app_name="app")
        inspect.signature(svc.list_sessions).bind(app_name="app", user_id="u1")
        inspect.signature(svc.delete_session).bind(app_name="app", user_id="u1", session_id="s1")
        # And: these overrides are actually the SAME method objects the real
        # BaseSessionService class declares as abstract (proves this is a
        # genuine override, not an unrelated same-named method).
        for name in ("create_session", "get_session", "list_sessions", "delete_session"):
            self.assertIn(name, BaseSessionService.__abstractmethods__)


@unittest.skipUnless(FARM and _NATIVE, SKIP if not FARM else _NATIVE_SKIP)
class RealSessionServiceEndToEndTest(_KeelSessionServiceFarmTestBase):
    """A designated Runner flow, driven by a REAL ``Runner`` +
    ``KeelSessionService`` + a real local tool, completes end-to-end with
    its steps landing in the REAL native journal — the "at minimum" bar,
    mirroring ``RealAdkNativeFlowEndToEndTest``."""

    def test_full_turn_calls_append_event_and_journals_identity_then_events(self) -> None:
        self.designate()
        self.native_backend()
        adk_pack.install()

        app_name = "farm-session-service"
        session_service = adk_pack.KeelSessionService(app_name=app_name)
        cls = type(session_service)
        calls: list[Any] = []
        original = cls.append_event

        async def counting(self: Any, *a: Any, **kw: Any) -> Any:
            calls.append((a, kw))
            return await original(self, *a, **kw)

        model = ScriptedModel()
        counter = {"n": 0}
        runner = self.make_runner(model, session_service, app_name, counter)

        async def drive() -> tuple[str, list[Any]]:
            session = await session_service.create_session(app_name=app_name, user_id="u1")
            events: list[Any] = []
            async for event in runner.run_async(
                user_id="u1", session_id=session.id, new_message=_new_message()
            ):
                events.append(event)
            return session.id, events

        with mock.patch.object(cls, "append_event", counting):
            session_id, events = asyncio.run(asyncio.wait_for(drive(), timeout=30))

        self.assertEqual(len(events), 5, "function_call+response x2, then final text")
        self.assertEqual(model.turn, 3)
        self.assertEqual(counter["n"], 2, "both real tool effects fired exactly once")
        self.assertGreaterEqual(
            len(calls),
            len(events),
            "the REAL Runner internals called session_service.append_event(...) for "
            "at least every yielded event (design §6 item 1, empirically proven here "
            "against real google-adk==2.4.0, not just read from runners.py source)",
        )
        self.assertFalse(_runtime.in_active_flow())

        flows = self._flow_rows()
        self.assertEqual(len(flows), 1)
        flow = flows[0]
        self.assertEqual(flow["status"], "completed")
        self.assertEqual(flow["entrypoint"], adk_pack.RUNNER_FLOW_ENTRYPOINT)

        steps = self._steps(flow["flow_id"])
        identity_prefix = adk_pack.SESSION_IDENTITY_TARGET + "#"
        event_prefix = adk_pack.SESSION_EVENT_TARGET + "#"
        identity_steps = [s for s in steps if str(s["step_key"]).startswith(identity_prefix)]
        event_steps = [s for s in steps if str(s["step_key"]).startswith(event_prefix)]
        self.assertEqual(len(identity_steps), 1, "session_identity written exactly once per flow")
        self.assertEqual(
            len(event_steps),
            len(calls),
            "one tool:adk.session_event journal step per REAL (non-partial) append_event call",
        )
        self.assertLess(
            identity_steps[0]["seq"],
            event_steps[0]["seq"],
            "session_identity precedes every session_event step (design §3.1's sequencing fix)",
        )
        self.assertEqual(
            [s["seq"] for s in event_steps],
            sorted(s["seq"] for s in event_steps),
            "session_event steps land in call order",
        )


@unittest.skipUnless(FARM and _NATIVE, SKIP if not FARM else _NATIVE_SKIP)
class RealSessionServiceCrashResumeConvergenceTest(_KeelSessionServiceFarmTestBase):
    """Design doc issue #15 §3.2a's REQUIRED certification, quoted verbatim:
    "build a crash-mid-turn-then-resume farm test analogous to
    test_adk_runner_flows_native.py's existing abandonment/resume coverage,
    and confirm the resumed Session.state/Session.events converge to the
    values a same-flow get_session() reconstructs from the journal after the
    fact." Reuses that module's exact abandonment technique
    (``gen.__anext__()``/``gen.aclose()``, the real ``EndToEndRecoveryTest``
    shape) and ``test_farm_adk_composition_native.py``'s exact same-content
    resume-identity technique, applied here with a REAL ``Runner`` + REAL
    ``KeelSessionService`` + REAL native journal.

    RESULT — DIVERGED (see ``FARM CERTIFICATION SUMMARY`` in the shipping
    commit for the authoritative write-up): ``Session.state`` and the
    SUBSTANTIVE content of every event (author, text, tool-call
    name/args/response) converge exactly, as does the FULL fingerprint
    (id/timestamp/invocation_id/content) of every event that was NEVER
    replay-substituted. But for the events belonging to run 1's abandoned
    (replay-substituted) prefix — the user message plus the first
    function_call/function_response pair — `Event.id`/`Event.timestamp`/
    `Event.invocation_id` (freshly, non-deterministically assigned by REAL
    ADK code on every construction, never virtualized by Keel) diverge
    between the LIVE session object and the journal reconstruction, and so
    does the RAW content of the function_call/function_response pair
    specifically (ADK assigns its own `function_call.id`/
    `function_response.id` correlation id fresh on every redrive too — this
    is NOT the same as the underlying tool's own `execute()` outcome, which
    Keel DOES correctly substitute and which is proven identical below).
    This is a REAL correctness gap relative to design §3.2a's stated goal;
    see this class's own assertions below for the exact, verified shape,
    and the shipping commit's FARM CERTIFICATION SUMMARY for the
    recommended next step. If a future ADK/Keel change makes any of these
    assertions start failing differently, that is real signal, not a
    flaky test.
    """

    def test_crash_mid_turn_then_resume_converges_with_the_journal_reconstruction(self) -> None:
        self.designate()
        self.native_backend()
        adk_pack.install()

        app_name = "farm-session-crash-resume"
        user_id = "u1"

        # ---- run 1: abandon right after the FIRST tool effect's response --
        counter1 = {"n": 0}
        model1 = ScriptedModel()
        session_service1 = adk_pack.KeelSessionService(app_name=app_name)
        runner1 = self.make_runner(model1, session_service1, app_name, counter1)
        msg1 = _new_message()
        session_ids: dict[str, str] = {}

        async def abandon() -> list[Any]:
            session = await session_service1.create_session(app_name=app_name, user_id=user_id)
            session_ids["id"] = session.id
            gen = runner1.run_async(user_id=user_id, session_id=session.id, new_message=msg1)
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

        run1_events = asyncio.run(asyncio.wait_for(abandon(), timeout=30))
        self.assertEqual(len(run1_events), 2, "function_call, then function_response — one full tool turn")
        self.assertEqual(counter1["n"], 1, "only the first REAL bump effect fired before abandonment")
        self.assertFalse(_runtime.in_active_flow(), "abandonment released the flow handle")

        # No journal read here — see the class's "poisoned write" note in
        # the base fixture; every assertion is deferred to after BOTH runs.

        # ---- run 2: a FRESH KeelSessionService instance + FRESH Runner ----
        # ("a crash kills the process; a NEW process re-enters") — SAME
        # message content (repr-identical) => same Keel flow identity via
        # the content-fingerprint fallback (_runner_flow_identity).
        counter2 = {"n": 0}
        model2 = ScriptedModel()
        session_service2 = adk_pack.KeelSessionService(app_name=app_name)
        runner2 = self.make_runner(model2, session_service2, app_name, counter2)
        msg2 = _new_message()
        self.assertEqual(repr(msg1), repr(msg2), "sanity: the fallback identity needs byte-identical repr")

        async def resume() -> list[Any]:
            session = await session_service2.create_session(
                app_name=app_name, user_id=user_id, session_id=session_ids["id"]
            )
            return [
                event
                async for event in runner2.run_async(
                    user_id=user_id, session_id=session.id, new_message=msg2
                )
            ]

        run2_events = asyncio.run(asyncio.wait_for(resume(), timeout=30))
        self.assertEqual(len(run2_events), 5, "the resumed run completes normally, all the way through")
        self.assertEqual(model2.turn, 3)
        self.assertEqual(
            counter2["n"],
            1,
            "exactly ONE new real bump effect (the second) — the first (substituted "
            "from run 1's journal) never re-fires its real Python body",
        )
        self.assertFalse(_runtime.in_active_flow())

        flows = self._flow_rows()
        self.assertEqual(len(flows), 1, "same flow identity resumed, not a second flow")
        flow = flows[0]
        self.assertEqual(flow["status"], "completed")
        markers = [s for s in self._steps(flow["flow_id"]) if s["kind"] == "marker"]
        self.assertEqual(markers[0]["attempt"], 2, "abandonment consumed attempt 1; resume is attempt 2")

        # ---- the actual §3.2a convergence check -----------------------------
        live_session = asyncio.run(
            session_service2.get_session(app_name=app_name, user_id=user_id, session_id=session_ids["id"])
        )
        self.assertIsNotNone(live_session)
        # 6, not 5: `Runner.run_async` also calls `append_event` for the
        # user's OWN message (`_append_user_event`, verified against real
        # runners.py during the design/verification phase) — it is APPENDED
        # to `session.events` unconditionally, even though it is only ever
        # YIELDED when `yield_user_message=True` (default False, so
        # `run2_events` above — the YIELDED stream — correctly stays 5).
        # Order: [user_msg, function_call_1, function_response_1,
        # function_call_2, function_response_2, final_text].
        self.assertEqual(len(live_session.events), 6)

        # A BRAND NEW instance, empty cache — forces the real §3.2 fallback
        # (flows_by_entrypoint/steps_for_flow scan-and-replay through the
        # journal) rather than returning session_service2's own live object.
        session_service3 = adk_pack.KeelSessionService(app_name=app_name)
        reconstructed = asyncio.run(
            session_service3.get_session(app_name=app_name, user_id=user_id, session_id=session_ids["id"])
        )
        self.assertIsNotNone(reconstructed)
        self.assertEqual(len(reconstructed.events), 6)

        def _fingerprint(session: Any) -> list[dict[str, Any]]:
            # Exactly the fields design §3.1's payload schema claims to
            # persist and design §3.2's read path claims to reconstruct —
            # NOT a full Event.model_dump() (which would also compare
            # ADK-internal bookkeeping fields — node_info, output,
            # long_running_tool_ids, branch, isolation_scope — that the
            # documented payload schema never claims to round-trip at all;
            # comparing those would manufacture a false "divergence" out of
            # an already-stated, non-silent scope limit, not test the
            # actual §3.2a convergence property).
            return [
                {
                    "id": e.id,
                    "author": e.author,
                    "invocation_id": e.invocation_id,
                    "timestamp": e.timestamp,
                    "content": adk_pack._encode_content(e.content),
                    "partial": e.partial,
                }
                for e in session.events
            ]

        live_fp = _fingerprint(live_session)
        reconstructed_fp = _fingerprint(reconstructed)

        def _strip_function_ids(content: dict[str, Any] | None) -> dict[str, Any] | None:
            """Strip ADK's own internally-assigned `function_call.id`/
            `function_response.id` correlation ids (see the CONFIRMED
            DIVERGENCE comment block below) so the SUBSTANTIVE tool-call
            value — `name`/`args`/`response` — can be compared on its own,
            separately from that correlation id."""
            if content is None:
                return None
            stripped: dict[str, Any] = {"role": content.get("role"), "parts": []}
            for part in content.get("parts") or []:
                part = dict(part)
                for key in ("function_call", "function_response"):
                    if isinstance(part.get(key), dict):
                        inner = dict(part[key])
                        inner.pop("id", None)
                        part[key] = inner
                stripped["parts"].append(part)
            return stripped

        live_content = [fp["content"] for fp in live_fp]
        reconstructed_content = [fp["content"] for fp in reconstructed_fp]

        # Events 0, 3, 4, 5 (index 0: the user's own plain-text message,
        # which was replay-substituted but carries no ADK-internal
        # correlation id to diverge on; indices 3-5: never substituted at
        # all) converge EXACTLY, content included.
        for i in (0, 3, 4, 5):
            self.assertEqual(
                live_content[i],
                reconstructed_content[i],
                f"event[{i}] content must converge exactly (design §3.2a)",
            )

        # Events 1, 2 (the REPLAY-SUBSTITUTED function_call/function_response
        # pair): CONFIRMED DIVERGENCE in the raw content dict — but the
        # SUBSTANTIVE tool-call value (name/args, and the response payload,
        # which comes from the SEPARATELY-substituted `tool:` effect step
        # and is therefore itself frozen/correct) still converges once
        # ADK's own `function_call.id`/`function_response.id` correlation
        # id is excluded. Root cause: ADK assigns these ids fresh
        # (`adk-<uuid>`) during ITS OWN event-construction pipeline on
        # EVERY invocation, including the redrive inside a resumed Tier 2
        # attempt — Keel's replay machinery virtualizes the TOOL's own
        # `execute()` outcome (proven identical: see the `response` field
        # below), but has no mechanism to virtualize this ADK-internal id,
        # since it is assigned deep inside `Runner.run_async`'s own
        # post-processing, entirely outside any Keel-wrapped call boundary.
        for i in (1, 2):
            self.assertNotEqual(
                live_content[i],
                reconstructed_content[i],
                f"CONFIRMED DIVERGENCE: event[{i}]'s raw content differs (ADK's own "
                "function_call/function_response correlation id — see this method's "
                "comments above)",
            )
            self.assertEqual(
                _strip_function_ids(live_content[i]),
                _strip_function_ids(reconstructed_content[i]),
                f"event[{i}]'s SUBSTANTIVE tool-call value (name/args/response) still "
                "converges once ADK's own correlation id is excluded",
            )

        live_authors = [fp["author"] for fp in live_fp]
        reconstructed_authors = [fp["author"] for fp in reconstructed_fp]
        self.assertEqual(live_authors, reconstructed_authors)
        live_partial = [fp["partial"] for fp in live_fp]
        reconstructed_partial = [fp["partial"] for fp in reconstructed_fp]
        self.assertEqual(live_partial, reconstructed_partial)

        # `Session.state` converges exactly — state_delta application is
        # idempotent regardless of which attempt's event object produced it.
        self.assertEqual(
            live_session.state,
            reconstructed.state,
            "resumed LIVE Session.state must converge to the journal reconstruction (design §3.2a)",
        )

        # `event.id`/`event.timestamp` (and, for events sharing run 1's
        # invocation, `event.invocation_id`) are a REAL, CONFIRMED
        # divergence for the THREE REPLAY-SUBSTITUTED events (index 0, 1, 2:
        # the user message plus the function_call/function_response pair,
        # all recorded during the abandoned attempt 1) — NOT a false
        # positive from comparing untracked fields (see `_fingerprint`'s own
        # docstring above): these ARE part of the documented §3.1 payload
        # schema and ARE what `_decode_event` reconstructs. Root cause:
        # `Event.id` is a random uuid assigned in `model_post_init`
        # whenever an `Event` is constructed with no explicit `id` (confirmed against
        # `google/adk/events/event.py`'s own "Generates a random ID for the
        # event" comment), and `Event.timestamp` is
        # `Field(default_factory=lambda: platform_time.get_time())` — i.e.
        # BOTH are freshly, non-deterministically generated by the REAL ADK
        # code on EVERY construction, including the redrive inside attempt
        # 2. `KeelSessionService.append_event`'s LIVE mutation
        # (`session.events.append(event)`, inside the inherited
        # `super().append_event()`) always uses THIS attempt's freshly
        # constructed `event` object, unconditionally — it does not consult
        # what `_record_session_step`'s underlying `backend.execute()` call
        # returns (whether fresh or replay-substituted), because that
        # return value is discarded (`_record_session_step` doesn't return
        # anything `append_event` reads). Meanwhile the JOURNAL's own copy
        # of the first two events' `tool:adk.session_event` steps IS
        # replay-substituted on resume (matching
        # `EndToEndRecoveryTest`'s independently-proven "exactly one
        # journal row per effect, ever — no duplicate for the substituted
        # one" behavior for the SAME `execute()` mechanism) — so it still
        # carries attempt 1's ORIGINAL `id`/`timestamp`/`invocation_id` for
        # those two events, not attempt 2's. This is a REAL correctness gap
        # relative to design §3.2a's stated goal ("If they ever diverge,
        # that is a real bug to fix") — reported precisely here rather than
        # silently avoided; see the shipping commit's FARM CERTIFICATION
        # SUMMARY for the recommended fast-follow. The assertions below
        # PIN the exact, verified shape of the divergence (which two
        # events, which fields) rather than asserting false convergence.
        for i in (0, 1, 2):
            self.assertNotEqual(
                live_fp[i]["id"],
                reconstructed_fp[i]["id"],
                f"CONFIRMED DIVERGENCE: event[{i}].id differs between the live (freshly "
                "redriven) object and the journal-reconstructed (replay-substituted, "
                "attempt-1-frozen) one — see this test's docstring/comments",
            )
            self.assertNotEqual(
                live_fp[i]["timestamp"],
                reconstructed_fp[i]["timestamp"],
                f"CONFIRMED DIVERGENCE: event[{i}].timestamp likewise diverges (fresh "
                "wall-clock value vs. attempt-1's frozen one)",
            )
        # Events 3-5 were never substituted (produced fresh exactly once,
        # during attempt 2's own un-replayed tail) — id/timestamp DO
        # converge for those, confirming the divergence is specifically a
        # replay-substitution artifact, not a general encode/decode bug.
        for i in (3, 4, 5):
            self.assertEqual(
                live_fp[i]["id"],
                reconstructed_fp[i]["id"],
                f"event[{i}] was never replay-substituted — id must converge",
            )
            self.assertEqual(
                live_fp[i]["timestamp"],
                reconstructed_fp[i]["timestamp"],
                f"event[{i}] was never replay-substituted — timestamp must converge",
            )


@unittest.skipUnless(FARM and _NATIVE, SKIP if not FARM else _NATIVE_SKIP)
class RealEventContentEncodingTest(_KeelSessionServiceFarmTestBase):
    """Event content encoding (design §3.1's "event mapping" open question)
    against the REAL ``Event``/``Content``/``Part``/``Blob`` classes (not
    ``fixtures/fake_adk.py``'s fake, which ``test_packs_adk.py``'s
    ``KeelSessionServiceContentEncodingTest`` already covers) — a real
    binary ``inline_data`` payload, a plain text part, and a
    ``function_call`` part all round-trip through the FULL write -> journal
    -> fresh-instance-read path, not just the private encode/decode
    helpers in isolation."""

    def test_inline_data_and_function_call_parts_round_trip_through_the_real_path(self) -> None:
        self.designate()
        self.native_backend()
        adk_pack.install()

        app_name = "farm-content-encoding"
        user_id, session_id = "u1", "s1"
        raw = b"\x89PNG\r\n\x1a\n\x00\x01\x02\x03real-bytes-fixture"

        class _Runner:
            def __init__(self, app_name: str) -> None:
                self.app_name = app_name

        async def orig(
            self: Any,
            *,
            user_id: str,
            session_id: str,
            invocation_id: str | None = None,
            new_message: Any = None,
            **kwargs: Any,
        ) -> Any:
            session_service = adk_pack.KeelSessionService(app_name=app_name)
            session = await session_service.create_session(
                app_name=app_name, user_id=user_id, session_id=session_id
            )
            await session_service.append_event(
                session,
                Event(
                    author="model",
                    invocation_id="inv-content-1",
                    content=types.Content(role="model", parts=[types.Part(text="hello")]),
                ),
            )
            await session_service.append_event(
                session,
                Event(
                    author="model",
                    invocation_id="inv-content-1",
                    content=types.Content(
                        role="model",
                        parts=[types.Part(inline_data=types.Blob(data=raw, mime_type="image/png"))],
                    ),
                ),
            )
            await session_service.append_event(
                session,
                Event(
                    author="model",
                    invocation_id="inv-content-1",
                    content=types.Content(
                        role="model",
                        parts=[types.Part(function_call=types.FunctionCall(name="lookup", args={"q": "keel"}))],
                    ),
                ),
            )
            yield Event(author="model", invocation_id="inv-content-1")

        wrapped = adk_pack._run_async_wrapper(orig)
        runner = _Runner(app_name)

        async def run() -> None:
            async for _event in wrapped(runner, user_id=user_id, session_id=session_id):
                pass

        asyncio.run(run())
        self.assertFalse(_runtime.in_active_flow())

        # A FRESH instance — forces the real journal-scan fallback, not a
        # same-process cache hit.
        fresh = adk_pack.KeelSessionService(app_name=app_name)
        reconstructed = asyncio.run(fresh.get_session(app_name=app_name, user_id=user_id, session_id=session_id))
        self.assertIsNotNone(reconstructed)
        self.assertEqual(len(reconstructed.events), 3)

        text_part = reconstructed.events[0].content.parts[0]
        self.assertEqual(text_part.text, "hello")

        binary_part = reconstructed.events[1].content.parts[0]
        self.assertEqual(binary_part.inline_data.data, raw, "binary inline_data survives byte-for-byte")
        self.assertEqual(binary_part.inline_data.mime_type, "image/png")

        fc_part = reconstructed.events[2].content.parts[0]
        self.assertEqual(fc_part.function_call.name, "lookup")
        self.assertEqual(fc_part.function_call.args, {"q": "keel"})


if __name__ == "__main__":
    unittest.main()
