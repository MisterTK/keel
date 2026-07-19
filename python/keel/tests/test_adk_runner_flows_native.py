"""WS5 Task 3 (5a truth): the designated ``Runner.run_async`` flow wrap
(``keel.packs.adk_pack``), proven against the REAL native journal — no fake
backend anywhere in this module.

``test_packs_adk.py``'s ``RunnerFlowWrapTest`` proves the wrapper's control
flow against ``_FakeAdkFlowBackend``, a double that models the native core's
replay/attempt semantics from reading the brief and ``test_flows.py``. This
module exists to CROSS-CHECK those modeled assumptions against the actual
compiled ``keel_core`` + on-disk sqlite journal (``load_backend("native",
cwd=...)``, direct ``sqlite3`` SELECTs on the ``steps``/``flows`` tables — the
same convention ``test_flows.py``'s ``NativeFlowReplayTest`` and
``test_resume_demo.py`` use), and to prove the amendment's recovery story
(abandon -> re-enter -> substitute -> complete) end-to-end through the real
wrapper rather than through a hand-rolled fake.

Requires the native core (``keel_core``); skips cleanly without it (the fast
CI path — no wheel built there for this leg).
"""

from __future__ import annotations

import asyncio
import json
import subprocess
import sqlite3
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any

from fake_adk import (
    FakeAdkModules,
    FakeEvent,
    FakeLLMRegistry,
    FakeLlmRequest,
    FakeLlmResponse,
    FakeModel,
    FakeRunner,
)

from keel import _runtime
from keel._backend import load_backend
from keel._discovery import Discovery
from keel._policy import FlowEntrypoint
from keel.packs import adk_pack
from keel.packs.tool import wrap_tool

try:
    import keel_core  # noqa: F401

    _NATIVE = True
except ImportError:
    _NATIVE = False

_REPO = Path(__file__).resolve().parents[3]


def _keel_binary() -> str | None:
    """The built `keel` CLI, if present — mirrors `test_resume_demo.py`'s
    helper exactly (same repo-root depth: `python/keel/tests/<this file>`)."""
    for candidate in (_REPO / "target" / "debug" / "keel", _REPO / "target" / "release" / "keel"):
        if candidate.exists():
            return str(candidate)
    return None


@unittest.skipUnless(_NATIVE, "keel_core native module not built (maturin develop in crates/keel-py)")
class _NativeAdkFlowTestBase(unittest.TestCase):
    """Shared fixture: a designated Runner-flow entrypoint over a REAL native
    backend with an on-disk journal at ``<cwd>/.keel/journal.db``."""

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
        # `_runtime.clear_runtime()` resets `flow_entrypoints` (along with
        # backend/discovery/flow_active), so no explicit prior-state
        # save/restore is needed here.
        _runtime.clear_runtime()
        for d in self._discoveries:
            d.close()
        self._tmp.cleanup()

    def designate(self) -> None:
        entry = FlowEntrypoint(
            raw=adk_pack.RUNNER_FLOW_ENTRYPOINT,
            module="google.adk.runners",
            function="Runner.run_async",
        )
        _runtime.set_flow_entrypoints([entry])

    def native_backend(self) -> Any:
        backend = load_backend("native", cwd=self.cwd)
        backend.configure({})
        discovery = Discovery(self.cwd)
        self._discoveries.append(discovery)
        _runtime.set_runtime(backend, discovery)
        return backend

    async def _drain(self, agen: Any) -> list[Any]:
        return [event async for event in agen]

    # -- direct sqlite3 helpers over the on-disk journal ---------------------

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

    def _steps(self, flow_id: str, kind: str | None = None) -> list[sqlite3.Row]:
        conn = self._conn()
        try:
            sql = "SELECT * FROM steps WHERE flow_id = ?"
            params: tuple[Any, ...] = (flow_id,)
            if kind is not None:
                sql += " AND kind = ?"
                params = (flow_id, kind)
            sql += " ORDER BY seq"
            return conn.execute(sql, params).fetchall()
        finally:
            conn.close()


class RunnerFlowRealJournalTest(_NativeAdkFlowTestBase):
    """Item (a) from the brief: a designated fake-ADK Runner over the real
    native backend journals seq0's attempt marker, the correlated
    ``adk:invocation_id`` value step, and completes — asserted with direct
    SELECTs on ``steps``/``flows``, not through any fake backend double."""

    def test_full_lifecycle_journal_rows_over_native_backend(self) -> None:
        self.designate()
        self.native_backend()
        with FakeAdkModules():
            adk_pack.install()
            from google.adk.runners import Runner

            runner = Runner(
                app_name="app",
                events=[FakeEvent(invocation_id="inv-1"), FakeEvent(invocation_id="inv-1")],
            )
            events = asyncio.run(
                self._drain(
                    runner.run_async(
                        user_id="u1", session_id="s1", invocation_id="inv-1", new_message={"text": "hi"}
                    )
                )
            )
            adk_pack.uninstall()

        self.assertEqual(len(events), 2)
        self.assertFalse(_runtime.in_active_flow())

        flow = self._flow_row()
        self.assertEqual(flow["status"], "completed")
        self.assertEqual(flow["entrypoint"], adk_pack.RUNNER_FLOW_ENTRYPOINT)

        steps = self._steps(flow["flow_id"])
        markers = [s for s in steps if s["kind"] == "marker"]
        randoms = [s for s in steps if s["kind"] == "random"]
        self.assertEqual(len(markers), 1)
        self.assertEqual(markers[0]["seq"], 0)
        self.assertEqual(markers[0]["attempt"], 1, "a clean single-attempt run")
        self.assertEqual(len(randoms), 1)
        self.assertEqual(randoms[0]["step_key"], "adk:invocation_id")
        self.assertEqual(bytes(randoms[0]["payload"]), b"inv-1", "the raw correlated invocation id bytes")


class JournalRandomReplaySemanticsTest(_NativeAdkFlowTestBase):
    """Cross-check 1 (Task 2 review carry-forward): does
    ``journal_random("adk:invocation_id", ...)`` really behave like the fake's
    ``setdefault`` model — first call journals the value, a later re-entry
    returns the ORIGINAL bytes rather than whatever the caller passes this
    time? Pinned directly against ``keel_core.KeelCore``, no ADK wrapper
    involved, so this is a fact about the core, not about adk_pack's use of
    it."""

    def test_replay_returns_the_original_bytes_not_the_new_call_data(self) -> None:
        core = keel_core.KeelCore(journal_path=str(self.journal_path))
        core.configure({})
        core.enter_flow(adk_pack.RUNNER_FLOW_ENTRYPOINT, "ah-1", explicit_key="inv-1")
        first = core.journal_random("adk:invocation_id", b"inv-1")
        self.assertEqual(first, b"inv-1", "live call returns the bytes it was given")
        core.exit_flow("completed")

        info = core.enter_flow(adk_pack.RUNNER_FLOW_ENTRYPOINT, "ah-1", explicit_key="inv-1")
        self.assertTrue(info["replay"], "re-entering a completed flow is a pure replay")
        second = core.journal_random("adk:invocation_id", b"DIFFERENT-VALUE-THIS-CALL")
        self.assertEqual(
            second,
            b"inv-1",
            "VERIFIED: replay substitutes the ORIGINAL recorded bytes, matching the "
            "fake's setdefault ('echo the first-ever value') model exactly",
        )
        core.exit_flow("completed")  # cross-check 2, see below: must not raise

    def test_exit_flow_completed_on_an_already_completed_replayed_flow_is_accepted(self) -> None:
        # Cross-check 2: does the real bridge accept exit_flow("completed") on
        # a flow that is ALREADY completed (a replay re-entry)? Per
        # crates/keel-journal/src/sqlite.rs's complete_flow: the UPDATE is
        # gated on `status != 'completed'`, so a redundant complete(Completed)
        # is a documented harmless no-op — never an error. Pinned here so the
        # wrapper's unconditional `backend.exit_flow("completed")` on its
        # success path is verified safe with NO guard needed.
        core = keel_core.KeelCore(journal_path=str(self.journal_path))
        core.configure({})
        core.enter_flow(adk_pack.RUNNER_FLOW_ENTRYPOINT, "ah-2")
        core.exit_flow("completed")
        info = core.enter_flow(adk_pack.RUNNER_FLOW_ENTRYPOINT, "ah-2")
        self.assertTrue(info["replay"])
        core.exit_flow("completed")  # must not raise KeelCoreError

        conn = sqlite3.connect(self.journal_path)
        try:
            status = conn.execute("SELECT status FROM flows").fetchone()[0]
        finally:
            conn.close()
        self.assertEqual(status, "completed")


class EndToEndRecoveryTest(_NativeAdkFlowTestBase):
    """Cross-check 3: the amendment's whole reason for existing, proven
    end-to-end through the REAL wrapper + REAL journal: a designated run is
    abandoned mid-stream (`aclose`, the crash shape), the flow is left
    `failed` in the journal (not wedged `running`-forever), a SAME-identity
    re-invoke re-enters as attempt 2, the already-journaled effect
    substitutes (the side-effect counter does not double-fire), and the flow
    completes."""

    def test_abandon_then_reenter_substitutes_the_first_effect_and_completes(self) -> None:
        self.designate()
        self.native_backend()
        counter = {"n": 0}

        async def bump(label: str) -> Any:
            async def _do() -> dict[str, Any]:
                counter["n"] += 1
                return {"label": label, "n": counter["n"]}

            return await wrap_tool(f"bump_{label}", _do)()

        class _TwoEffectRunner(FakeRunner):
            async def run_async(
                self,
                *,
                user_id: str,
                session_id: str,
                invocation_id: str | None = None,
                new_message: Any = None,
                **kwargs: Any,
            ) -> Any:
                await bump("one")
                yield self.events[0]
                await bump("two")
                yield self.events[1]

        events = [FakeEvent(invocation_id="inv-1"), FakeEvent(invocation_id="inv-1")]

        with FakeAdkModules():
            import google.adk.runners as runners_mod

            runners_mod.Runner = _TwoEffectRunner
            adk_pack.install()

            runner1 = runners_mod.Runner(app_name="app", events=list(events))

            async def abandon() -> Any:
                gen = runner1.run_async(user_id="u1", session_id="s1", invocation_id="inv-1")
                first = await gen.__anext__()
                await gen.aclose()
                return first

            first_event = asyncio.run(abandon())
            self.assertIsNotNone(first_event)
            self.assertEqual(counter["n"], 1, "only the first effect fired before abandonment")
            self.assertFalse(_runtime.in_active_flow(), "abandonment released the flow handle")

            # NOTE: deliberately NOT reading the journal via sqlite3 here, between
            # the abandon and the resume. VERIFIED (isolated repro, no adk_pack
            # involved): opening-and-closing a plain `sqlite3.connect()` against
            # this SAME on-disk journal.db WHILE the native KeelCore instance
            # backing this test is still alive, in between two of its own
            # enter_flow/exit_flow calls, silently drops every one of that
            # core's SUBSEQUENT journal writes for the rest of the process (no
            # exception — matches crates/keel-core/src/flow.rs's own documented
            # "degrade a journal failure to warn!" design, so nothing surfaces
            # in Python). Root cause smells like a WAL-checkpoint-on-close race
            # between the stdlib sqlite3 module's SQLite build and rusqlite's,
            # since the on-disk `-wal`/`-shm` sidecar files vanish the instant
            # the extra Python connection closes even though the Rust
            # connection is still open. This is a real gotcha for anything that
            # reads journal.db while a flow is concurrently active on the SAME
            # process (e.g. a hypothetical in-process `keel flows` call) — every
            # journal-row assertion in this module is deferred to AFTER the
            # relevant KeelCore's flow-writing work is fully done, exactly like
            # `test_resume_demo.py`'s subprocess precedent (it only reads
            # journal.db after the writer process has already exited).

            # SAME identity re-invoke, on the SAME backend/journal.
            runner2 = runners_mod.Runner(app_name="app", events=list(events))
            second_events = asyncio.run(
                self._drain(
                    runner2.run_async(user_id="u1", session_id="s1", invocation_id="inv-1")
                )
            )
            adk_pack.uninstall()

        self.assertEqual(len(second_events), 2, "the resumed run completes normally")
        self.assertEqual(
            counter["n"], 2, "the substituted first effect must NOT double-fire; only the new second one does"
        )

        flow = self._flow_row()
        self.assertEqual(flow["status"], "completed")
        markers = self._steps(flow["flow_id"], kind="marker")
        self.assertEqual(markers[0]["attempt"], 2, "abandonment consumed attempt 1; resume is attempt 2")
        effect_steps = self._steps(flow["flow_id"], kind="effect")
        self.assertEqual(
            [s["step_key"] for s in effect_steps],
            ["tool:bump_one#-", "tool:bump_two#-"],
            "exactly one journal row per effect, ever — no duplicate for the substituted one",
        )

    def test_repeated_abandonment_hits_the_attempt_cap_and_the_flow_goes_dead(self) -> None:
        # Cross-check 3's attempt-accounting half (conformance rule 10):
        # default max_attempts=3 (FlowConfig::default, crates/keel-core/src/
        # flow.rs). Three clean abandon-and-reenter cycles stay within the
        # cap; the FOURTH enter_flow call must be refused with KEEL-E032 and
        # the flow marked dead — raised from the wrapper's very first
        # `__anext__()` (enter_flow happens before the try/except block, so
        # `_runtime.set_flow_active` is never even reached).
        self.designate()
        self.native_backend()
        events = [FakeEvent(invocation_id="inv-1"), FakeEvent(invocation_id="inv-1")]

        async def abandon_once() -> None:
            with FakeAdkModules():
                adk_pack.install()
                from google.adk.runners import Runner

                runner = Runner(app_name="app", events=list(events))
                gen = runner.run_async(user_id="u1", session_id="s1", invocation_id="inv-1")
                await gen.__anext__()
                await gen.aclose()
                adk_pack.uninstall()

        # No sqlite3 read between iterations — see the "poisoned write" note on
        # test_abandon_then_reenter_substitutes_the_first_effect_and_completes:
        # reading journal.db mid-sequence, while this test's own KeelCore is
        # still alive between its own enter/exit calls, silently drops that
        # core's LATER writes (verified in isolation, no adk_pack involved).
        for _ in range(1, 4):
            asyncio.run(abandon_once())

        async def fourth_attempt() -> None:
            with FakeAdkModules():
                adk_pack.install()
                from google.adk.runners import Runner

                runner = Runner(app_name="app", events=list(events))
                gen = runner.run_async(user_id="u1", session_id="s1", invocation_id="inv-1")
                try:
                    await gen.__anext__()
                finally:
                    adk_pack.uninstall()

        with self.assertRaises(keel_core.KeelCoreError) as ctx:
            asyncio.run(fourth_attempt())
        self.assertEqual(ctx.exception.code, "KEEL-E032")
        self.assertFalse(
            _runtime.in_active_flow(), "enter_flow raised before set_flow_active(True) ever ran"
        )

        flow = self._flow_row()
        self.assertEqual(flow["status"], "dead", "the attempt cap marks the flow dead, KEEL-E032 shape")


class GeneratorExitReplayedCombinationTest(_NativeAdkFlowTestBase):
    """Cross-check 5: abandoning a REPLAY-ONLY entry (an already-completed
    flow re-entered) must NOT demote it — the `if not replayed` guard on the
    GeneratorExit branch, proven over the real journal (the fake-backed
    `test_replay_completed_entry_never_demoted` only exercises the
    `BaseException` branch of this guard, never `GeneratorExit`)."""

    def test_abandoning_a_replayed_entry_does_not_demote_it(self) -> None:
        self.designate()
        self.native_backend()

        with FakeAdkModules():
            adk_pack.install()
            from google.adk.runners import Runner

            # Run 1: complete normally. (No sqlite3 read here — see the "poisoned
            # write" note on EndToEndRecoveryTest: reading journal.db via a
            # separate connection while this test's own KeelCore is still
            # alive, between two of its own enter/exit calls, silently drops
            # that core's later writes. Every assertion here is deferred to
            # after ALL flow work on this journal is done.)
            runner1 = Runner(app_name="app", events=[FakeEvent(invocation_id="inv-1")])
            asyncio.run(
                self._drain(runner1.run_async(user_id="u1", session_id="s1", invocation_id="inv-1"))
            )

            # Run 2: SAME identity -> a pure replay entry. Abandon it mid-stream.
            runner2 = Runner(
                app_name="app",
                events=[FakeEvent(invocation_id="inv-1"), FakeEvent(invocation_id="inv-1")],
            )

            async def abandon() -> Any:
                gen = runner2.run_async(user_id="u1", session_id="s1", invocation_id="inv-1")
                first = await gen.__anext__()
                await gen.aclose()
                return first

            asyncio.run(abandon())
            adk_pack.uninstall()

        flow = self._flow_row()
        self.assertEqual(
            flow["status"], "completed", "abandoning a replayed entry must never demote it to failed"
        )
        self.assertFalse(_runtime.in_active_flow())


class WrappedEffectAdmissionOrderTest(_NativeAdkFlowTestBase):
    """Cross-check 4: inside a designated run, two keel-wrapped effects
    awaited by the fake tool body admit and journal in the order their calls
    REACH the open flow handle, after the correlation step — never racing.
    Creation is staggered by ~10ms exactly like `test_flows.py`'s
    `test_concurrent_async_effects_serialize_in_admission_order` (a genuinely
    real-time async bridge on this path; no virtual clock to fast-forward)."""

    def test_wrapped_effects_admit_in_call_order_after_correlation(self) -> None:
        self.designate()
        self.native_backend()

        class _StaggeredRunner(FakeRunner):
            async def run_async(
                self,
                *,
                user_id: str,
                session_id: str,
                invocation_id: str | None = None,
                new_message: Any = None,
                **kwargs: Any,
            ) -> Any:
                yield self.events[0]

                async def eff_a() -> dict[str, str]:
                    return {"label": "a"}

                async def eff_b() -> dict[str, str]:
                    return {"label": "b"}

                t0 = asyncio.ensure_future(wrap_tool("effect_a", eff_a)())
                await asyncio.sleep(0.01)
                t1 = asyncio.ensure_future(wrap_tool("effect_b", eff_b)())
                await asyncio.gather(t0, t1)
                yield self.events[1]

        with FakeAdkModules():
            import google.adk.runners as runners_mod

            runners_mod.Runner = _StaggeredRunner
            adk_pack.install()
            runner = runners_mod.Runner(
                app_name="app",
                events=[FakeEvent(invocation_id="inv-1"), FakeEvent(invocation_id="inv-1")],
            )
            events = asyncio.run(
                self._drain(
                    runner.run_async(user_id="u1", session_id="s1", invocation_id="inv-1")
                )
            )
            adk_pack.uninstall()

        self.assertEqual(len(events), 2)
        flow = self._flow_row()
        self.assertEqual(flow["status"], "completed")
        steps = self._steps(flow["flow_id"])
        random_step = next(s for s in steps if s["kind"] == "random")
        effect_steps = [s for s in steps if s["kind"] == "effect"]
        self.assertEqual(
            [s["step_key"] for s in effect_steps],
            ["tool:effect_a#-", "tool:effect_b#-"],
            "effects journaled in call order, never raced",
        )
        self.assertLess(
            random_step["seq"],
            effect_steps[0]["seq"],
            "both wrapped effects admit AFTER the correlation step",
        )


class KeelFlowsCliShowsTheFlowTest(_NativeAdkFlowTestBase):
    """Item (c) from the brief: `keel flows` (the built CLI) reports the
    designated Runner-flow once it completes. Skipped when the binary isn't
    built — same precedent as `test_resume_demo.py`."""

    def test_keel_flows_cli_reports_the_completed_runner_flow(self) -> None:
        keel = _keel_binary()
        if keel is None:
            self.skipTest("keel CLI binary not built (target/debug/keel absent)")

        self.designate()
        self.native_backend()
        with FakeAdkModules():
            adk_pack.install()
            from google.adk.runners import Runner

            runner = Runner(app_name="app", events=[FakeEvent(invocation_id="inv-1")])
            asyncio.run(
                self._drain(runner.run_async(user_id="u1", session_id="s1", invocation_id="inv-1"))
            )
            adk_pack.uninstall()

        out = subprocess.run(
            [keel, "--json", "flows"], cwd=self.cwd, capture_output=True, text=True
        )
        self.assertEqual(out.returncode, 0, out.stderr)
        report = json.loads(out.stdout)
        self.assertEqual(report["count"], 1)
        self.assertEqual(report["flows"][0]["status"], "completed")
        self.assertEqual(report["flows"][0]["entrypoint"], adk_pack.RUNNER_FLOW_ENTRYPOINT)


class ModelFallbackWithinFlowCompositionTest(_NativeAdkFlowTestBase):
    """WS5 coda: the 5a x 5b COMPOSITION — an ``on_model_error`` cross-model
    fallback (``adk_pack._model_fallback``, decision 7) firing INSIDE an open
    designated Runner flow (``adk_pack._run_async_wrapper``, the amendment
    this whole module otherwise certifies). Both halves are proven
    separately: the flow wrap over the REAL journal above in this module;
    the fallback callback against ``FakeLLMRegistry`` in ``test_packs_adk.
    py``'s ``ModelFallbackTest``. This class proves they compose over the
    REAL native backend: the fallback hop's own effect — performed inside
    whatever model ``_model_fallback`` resolves and drives — journals as a
    genuine flow step through ``execute_async`` into the SAME open flow
    (the native ``KeelCore`` instance tracks "a flow is open" as its own
    state once ``enter_flow`` has run; every ``wrap_tool`` call reaching it
    before the matching ``exit_flow`` journals as that flow's next step,
    regardless of how deep in the call stack it originates — proven already
    by ``EndToEndRecoveryTest``'s two-effect runner above), admission-ordered
    after the correlation step and the runner body's own first effect —
    and, the load-bearing claim, SUBSTITUTES (never re-fires) on a
    crash+reenter exactly like any other flow step.

    The designated Runner body below deliberately yields once FIRST (the
    same convention ``WrappedEffectAdmissionOrderTest``'s ``_StaggeredRunner``
    uses) so the correlation step admits before either effect — matching
    this class's ordering assertion — rather than folding everything into
    one eager first ``__anext__()`` (which would admit both effects before
    the wrapper ever sees an event to correlate against).
    """

    def setUp(self) -> None:
        super().setUp()
        FakeLLMRegistry.reset()
        adk_pack._noted_model_fallback_skips.clear()

    def _scenario(self) -> tuple[type, dict[str, int]]:
        """Build one designated Runner class over a shared side-effect
        counter and a fallback model registered into ``FakeLLMRegistry`` —
        fresh closures per test (the registry itself is reset in ``setUp``).
        """
        counter: dict[str, int] = {}

        async def bump(label: str) -> Any:
            async def _do() -> dict[str, Any]:
                counter[label] = counter.get(label, 0) + 1
                return {"label": label, "n": counter[label]}

            return await wrap_tool(f"bump_{label}", _do)()

        fallback_response = FakeLlmResponse(content="fallback-answer")

        class _FallbackModelWithEffect(FakeModel):
            """The chosen fallback hop's model: its OWN
            ``generate_content_async`` performs a real keel-wrapped async
            effect (``bump('fallback')``) before yielding a scripted
            response — proving the hop's side effect journals through
            ``execute_async`` into whatever flow is currently open, exactly
            like any other awaited effect inside the designated Runner
            body."""

            async def generate_content_async(self, llm_request: Any, stream: bool = False) -> Any:
                self.calls.append(llm_request)
                await bump("fallback")
                yield fallback_response

        fallback_model = _FallbackModelWithEffect()
        FakeLLMRegistry.configure("fake-fallback-model", fallback_model)

        failing_error = RuntimeError("model transport failure")
        failing_error.keel_outcome = {"error": {"code": "KEEL-E010"}}  # non-E012: chaseable
        failing_model = FakeModel(error=failing_error)

        class _ComposedRunner(FakeRunner):
            """Designated body: yield once first (correlation trigger), one
            keel-wrapped effect, a failing model call driven straight into
            the real ``_KeelPlugin.on_model_error_callback`` (the exact
            plugin surface ADK itself would call on a model failure), then a
            final event quoting whatever response the fallback resolved."""

            async def run_async(
                self,
                *,
                user_id: str,
                session_id: str,
                invocation_id: str | None = None,
                new_message: Any = None,
                **kwargs: Any,
            ) -> Any:
                yield self.events[0]
                await bump("one")
                llm_request = FakeLlmRequest(model="gemini-2.0-flash")
                error: Exception | None = None
                try:
                    async for _ in failing_model.generate_content_async(llm_request, stream=False):
                        pass
                except Exception as exc:
                    error = exc
                response = await adk_pack._plugin().on_model_error_callback(
                    callback_context=object(), llm_request=llm_request, error=error
                )
                yield FakeEvent(
                    invocation_id=invocation_id,
                    content=response.content if response is not None else None,
                )

        return _ComposedRunner, counter

    def test_fallback_hop_effect_journals_inside_the_open_flow_admission_ordered(self) -> None:
        self.designate()
        backend = self.native_backend()
        backend.configure({"target": {"llm:google-genai": {"fallback": ["fake-fallback-model"]}}})
        runner_cls, counter = self._scenario()

        with FakeAdkModules():
            import google.adk.runners as runners_mod

            runners_mod.Runner = runner_cls
            adk_pack.install()
            runner = runners_mod.Runner(app_name="app", events=[FakeEvent(invocation_id="inv-1")])
            events = asyncio.run(
                self._drain(runner.run_async(user_id="u1", session_id="s1", invocation_id="inv-1"))
            )
            adk_pack.uninstall()

        self.assertEqual(len(events), 2)
        self.assertEqual(events[1].content, "fallback-answer", "final event quotes the fallback response")
        self.assertEqual(counter, {"one": 1, "fallback": 1})
        self.assertFalse(_runtime.in_active_flow())

        flow = self._flow_row()
        self.assertEqual(flow["status"], "completed")
        steps = self._steps(flow["flow_id"])
        random_steps = [s for s in steps if s["kind"] == "random"]
        effect_steps = [s for s in steps if s["kind"] == "effect"]
        self.assertEqual(len(random_steps), 1)
        self.assertEqual(
            [s["step_key"] for s in effect_steps],
            ["tool:bump_one#-", "tool:bump_fallback#-"],
            "the runner's own effect, then the fallback hop's effect, in call order",
        )
        self.assertLess(
            random_steps[0]["seq"],
            effect_steps[0]["seq"],
            "both effects admit AFTER the correlation step",
        )
        self.assertLess(
            effect_steps[0]["seq"],
            effect_steps[1]["seq"],
            "admission order: the runner's own effect before the fallback hop's",
        )

    def test_abandon_after_the_fallback_hop_then_reenter_substitutes_both_effects(self) -> None:
        self.designate()
        backend = self.native_backend()
        backend.configure({"target": {"llm:google-genai": {"fallback": ["fake-fallback-model"]}}})
        runner_cls, counter = self._scenario()

        with FakeAdkModules():
            import google.adk.runners as runners_mod

            runners_mod.Runner = runner_cls
            adk_pack.install()

            runner1 = runners_mod.Runner(app_name="app", events=[FakeEvent(invocation_id="inv-1")])

            async def abandon_after_the_fallback_hop() -> Any:
                gen = runner1.run_async(user_id="u1", session_id="s1", invocation_id="inv-1")
                await gen.__anext__()  # the correlation-trigger event
                second = await gen.__anext__()  # fallback-quoting event: BOTH effects have journaled by now
                await gen.aclose()  # crash shape: abandon before the generator's own StopAsyncIteration
                return second

            second_event = asyncio.run(abandon_after_the_fallback_hop())
            self.assertEqual(second_event.content, "fallback-answer")
            self.assertEqual(
                counter, {"one": 1, "fallback": 1}, "both effects fired exactly once before abandonment"
            )
            self.assertFalse(_runtime.in_active_flow(), "abandonment released the flow handle")

            # NOTE: deliberately no sqlite3 read here between the abandon and the
            # resume — see EndToEndRecoveryTest's "poisoned write" note earlier in
            # this module: a second connection opened against this journal while
            # this test's own KeelCore is still alive silently drops its later
            # writes. Every assertion below is deferred to after all flow work is
            # fully done.

            runner2 = runners_mod.Runner(app_name="app", events=[FakeEvent(invocation_id="inv-1")])
            second_run_events = asyncio.run(
                self._drain(runner2.run_async(user_id="u1", session_id="s1", invocation_id="inv-1"))
            )
            adk_pack.uninstall()

        self.assertEqual(len(second_run_events), 2, "the resumed run completes normally")
        self.assertEqual(second_run_events[1].content, "fallback-answer")
        self.assertEqual(
            counter,
            {"one": 1, "fallback": 1},
            "BOTH the runner's own effect and the fallback hop's effect substitute on replay -- neither re-fires",
        )

        flow = self._flow_row()
        self.assertEqual(flow["status"], "completed")
        markers = self._steps(flow["flow_id"], kind="marker")
        self.assertEqual(markers[0]["attempt"], 2, "abandonment consumed attempt 1; resume is attempt 2")
        effect_steps = self._steps(flow["flow_id"], kind="effect")
        self.assertEqual(
            [s["step_key"] for s in effect_steps],
            ["tool:bump_one#-", "tool:bump_fallback#-"],
            "exactly one journal row per effect, ever -- no duplicate for either substituted effect",
        )


if __name__ == "__main__":
    unittest.main()
