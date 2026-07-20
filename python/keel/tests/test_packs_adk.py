"""The Google ADK framework pack (dx-spec §4.2).

Runs entirely OFFLINE against the structural fake in ``fixtures/fake_adk.py``
(signatures verified against the real ``google-adk`` 2.4.0 in a throwaway
venv — never a repo dependency, mirrors how ``node/keel/test/ai-sdk.test.mjs``
tests the AI SDK pack against a fake model). Covers: honest detection, the
adapter-pack contract shape, install/uninstall reversibility on
``Runner.__init__``, zero-code-change plugin auto-registration across every
ADK construction shape (``Runner(agent=...)``, ``InMemoryRunner(...)``,
``Runner(app=App(plugins=[...]))``), and the ``before_tool_callback`` seam's
Level 0 semantics (non-idempotent by default, skip-and-note for an unwrappable
tool name, discovery recording).
"""

from __future__ import annotations

import asyncio
import base64
import hashlib
import inspect
import io
import os
import sqlite3
import sys
import threading
import unittest
from importlib import metadata
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any, Callable
from unittest import mock

from fake_adk import (
    FakeAdkModules,
    FakeAlreadyExistsError,
    FakeApp,
    FakeBasePlugin,
    FakeBlob,
    FakeClaude,
    FakeContent,
    FakeEvent,
    FakeEventActions,
    FakeGemini,
    FakeGetSessionConfig,
    FakeInMemoryRunner,
    FakeLLMRegistry,
    FakeLlmRequest,
    FakeLlmResponse,
    FakePart,
    FakeRunner,
    FakeSession,
    FakeTool,
    FakeSlottedTool,
    McpTool,
)

from keel import _runtime
from keel._backend import load_backend
from keel._discovery import Discovery
from keel._errors import KeelError
from keel._policy import FlowEntrypoint
from keel.adapters import available_packs, install_adapters, uninstall_adapters
from keel.packs import adk_pack


class AdkTestBase(unittest.TestCase):
    def setUp(self) -> None:
        adk_pack._noted_skips.clear()
        adk_pack._noted_fallbacks.clear()
        adk_pack._noted_model_fallback_skips.clear()
        adk_pack._noted_model_fallback_hop_failures.clear()
        adk_pack._rebound.clear()
        adk_pack._noted_busy = False
        # KeelSessionService (design doc issue #15) module-level state: the
        # built session-service CLASS is cached across calls (mirrors
        # `langgraph_pack._saver_cls`) — reset it so every test observes a
        # FRESH `_base_session_service_cls()` build (needed for e.g. the
        # "google.adk absent" test to actually exercise the ImportError path
        # rather than reusing a class a PRIOR test already built under
        # `FakeAdkModules()`). The per-flow session-event sequence counter
        # and "whose flow is this" identity (`_run_async_flow_wrapper`'s own
        # reset site) are reset here too, for the same cross-test isolation
        # reason `_noted_busy`/`_rebound` already get it.
        adk_pack._session_service_cls = None
        adk_pack._active_session_identity = None
        adk_pack._session_event_seq = 0
        FakeLLMRegistry.reset()
        self._tmp = TemporaryDirectory()
        self.cwd = Path(self._tmp.name)
        self._discoveries: list[Discovery] = []

    def tearDown(self) -> None:
        _runtime.clear_runtime()
        for d in self._discoveries:
            d.close()
        self._tmp.cleanup()

    def install_runtime(self, policy: dict[str, Any]) -> tuple[Any, Discovery]:
        backend = load_backend("stub")
        backend.configure(policy)
        discovery = Discovery(self.cwd)
        self._discoveries.append(discovery)
        _runtime.set_runtime(backend, discovery)
        return backend, discovery

    def read_rows(self, discovery: Discovery) -> dict[str, sqlite3.Row]:
        discovery.close()
        conn = sqlite3.connect(discovery.db_path)
        conn.row_factory = sqlite3.Row
        try:
            return {r["target"]: r for r in conn.execute("SELECT * FROM discovery")}
        finally:
            conn.close()


class DetectTest(unittest.TestCase):
    def test_absent_is_honestly_unmatched(self) -> None:
        # google.adk is genuinely not installed in this test environment.
        det = adk_pack.detect()
        self.assertFalse(det.matched)

    def test_present_but_no_distribution_metadata_is_best_effort(self) -> None:
        with FakeAdkModules():
            det = adk_pack.detect()
        self.assertTrue(det.matched)
        self.assertEqual(det.name, "google-adk")
        self.assertEqual(det.confidence, "best_effort")

    def test_pinned_version_reports_pinned(self) -> None:
        with FakeAdkModules(), mock.patch.object(metadata, "version", return_value="2.4.0"):
            det = adk_pack.detect()
        self.assertEqual(det.version, "2.4.0")
        self.assertEqual(det.confidence, "pinned")

    def test_unpinned_major_is_best_effort(self) -> None:
        with FakeAdkModules(), mock.patch.object(metadata, "version", return_value="0.9.0"):
            det = adk_pack.detect()
        self.assertEqual(det.confidence, "best_effort")


class ContractShapeTest(unittest.TestCase):
    def test_seams_documents_runner_init(self) -> None:
        # Task 2 added a second seam entry for the run_async patch point
        # (WS5 core: designated Runner.run_async invocations become Tier 2
        # flows) — this count is intentionally bumped from 1 to 2.
        seams = adk_pack.seams()
        self.assertEqual(len(seams), 2)
        patch_points = {s.patch_point for s in seams}
        self.assertIn("google.adk.runners.Runner.__init__", patch_points)
        self.assertIn("google.adk.runners.Runner.run_async", patch_points)

    def test_targets_declares_tool_and_llm(self) -> None:
        decls = {d.kind: d for d in adk_pack.targets()}
        self.assertEqual(decls["tool"].pattern, "tool:<name>")
        self.assertIn("non-idempotent by default", decls["tool"].idempotency_rule)
        self.assertEqual(decls["llm"].pattern, "llm:google-genai")
        self.assertIn("httpx_pack", decls["llm"].idempotency_rule)
        # Task 4 (5b): the transport seam's own text is preserved verbatim
        # above (still asserted), but the target's idempotency_rule now ALSO
        # documents the plugin-level cross-provider fallback hop this task
        # added (on_model_error_callback) — updated deliberately in the same
        # commit that added the behavior, not a stale doc drifting behind it.
        self.assertIn("on_model_error_callback", decls["llm"].idempotency_rule)

    def test_defaults_empty(self) -> None:
        # No [defaults.adk] in the frozen pack (tool:/mcp: pattern).
        self.assertEqual(adk_pack.defaults(), {})


class InstallReversibilityTest(unittest.TestCase):
    def tearDown(self) -> None:
        # Belt-and-suspenders: each test below already uninstalls inside its
        # own `with FakeAdkModules()` block, but re-enter here too in case a
        # failing assertion skipped that — never leave `_installed` stuck.
        with FakeAdkModules():
            adk_pack.uninstall()

    def test_install_patches_runner_init_and_uninstall_restores_it(self) -> None:
        with FakeAdkModules():
            from google.adk.runners import Runner  # the fake, via sys.modules

            pristine = Runner.__init__
            adk_pack.install()
            self.assertIsNot(Runner.__init__, pristine, "Runner.__init__ patched")
            self.assertTrue(getattr(Runner.__init__, "__keel_wrapped__", False))
            adk_pack.uninstall()
            self.assertIs(Runner.__init__, pristine, "Runner.__init__ restored")

    def test_install_is_idempotent_and_noop_when_absent(self) -> None:
        adk_pack.install()  # google.adk genuinely absent here: a no-op
        adk_pack.uninstall()  # never armed: also a no-op, must not raise
        with FakeAdkModules():
            from google.adk.runners import Runner

            pristine = Runner.__init__
            adk_pack.install()
            adk_pack.install()  # second call: no double-patch
            self.assertTrue(getattr(Runner.__init__, "__keel_wrapped__", False))
            wrapped_once = Runner.__init__
            adk_pack.install()
            self.assertIs(Runner.__init__, wrapped_once)
            adk_pack.uninstall()
            self.assertIs(Runner.__init__, pristine)


class PluginAutoRegistrationTest(unittest.TestCase):
    def tearDown(self) -> None:
        # FakeRunner/FakeInMemoryRunner are shared class objects across every
        # `with FakeAdkModules()` block (fixture module-level singletons), so
        # a patch applied inside one block persists on that same class object
        # after the block exits — re-enter the fake context here so
        # `uninstall()` (a no-op if `install()` was never called) restores it
        # correctly rather than leaking into the next test.
        with FakeAdkModules():
            adk_pack.uninstall()

    def test_runner_agent_construction_gets_the_plugin(self) -> None:
        with FakeAdkModules():
            adk_pack.install()
            from google.adk.runners import Runner

            runner = Runner(app_name="app", agent=None)
            plugin = runner.plugin_manager.get_plugin(adk_pack.PLUGIN_NAME)
            self.assertIsNotNone(plugin)
            self.assertEqual(plugin.name, "keel")

    def test_in_memory_runner_construction_gets_the_plugin(self) -> None:
        # InMemoryRunner.__init__ forwards to Runner.__init__ via super() —
        # patching only Runner.__init__ must cover this shape too.
        with FakeAdkModules():
            adk_pack.install()
            from google.adk.runners import InMemoryRunner

            runner = InMemoryRunner(agent=None, app_name="app")
            self.assertIsNotNone(runner.plugin_manager.get_plugin(adk_pack.PLUGIN_NAME))

    def test_app_plugins_construction_shape_gets_the_plugin(self) -> None:
        # The modern, recommended Runner(app=App(plugins=[...])) shape — the
        # deprecated plugins= kwarg is not used here at all.
        with FakeAdkModules():
            adk_pack.install()
            from google.adk.runners import Runner

            app = FakeApp(name="app", plugins=[])
            runner = Runner(app=app)
            self.assertIsNotNone(runner.plugin_manager.get_plugin(adk_pack.PLUGIN_NAME))

    def test_users_own_keel_named_plugin_is_not_clobbered(self) -> None:
        with FakeAdkModules():
            adk_pack.install()
            from google.adk.runners import Runner

            mine = FakeBasePlugin("keel")
            runner = Runner(app_name="app", agent=None, plugins=[mine])
            self.assertIs(runner.plugin_manager.get_plugin("keel"), mine)

    def test_two_runners_each_get_the_shared_plugin_instance(self) -> None:
        with FakeAdkModules():
            adk_pack.install()
            from google.adk.runners import Runner

            r1 = Runner(app_name="a", agent=None)
            r2 = Runner(app_name="b", agent=None)
            p1 = r1.plugin_manager.get_plugin("keel")
            p2 = r2.plugin_manager.get_plugin("keel")
            self.assertIs(p1, p2, "one shared KeelPlugin singleton, registered on every runner")

    def test_attach_plugin_defensive_when_plugin_manager_missing(self) -> None:
        # A hypothetical/unsupported ADK shape whose Runner never sets
        # `self.plugin_manager` at all: `_attach_plugin` must do nothing
        # unsafe, not raise (Level 0: "if a call site cannot be wrapped
        # safely, do nothing").
        class NoPluginManager:
            pass

        adk_pack._attach_plugin(NoPluginManager())  # must not raise

    def test_attach_plugin_defensive_when_plugin_manager_is_the_wrong_shape(self) -> None:
        class HalfBaked:
            plugin_manager = object()  # present, but not a real PluginManager

        adk_pack._attach_plugin(HalfBaked())  # must not raise


class InstallAdaptersIntegrationTest(unittest.TestCase):
    """`keel.adapters.install_adapters()` — the real `bootstrap.install_keel`
    call shape — arms adk_pack through the identical lazy mechanism as
    httpx/requests, registered like every other framework pack (`keel.
    adapters._framework_packs`), so it also shows up in `available_packs()`.
    """

    def tearDown(self) -> None:
        with FakeAdkModules():
            uninstall_adapters()

    def test_adk_pack_armed_and_reported_like_any_framework_pack(self) -> None:
        with FakeAdkModules():
            from google.adk.runners import Runner

            pristine = Runner.__init__
            present = install_adapters()
            self.assertIn("google-adk", {d.name for d in present})
            self.assertIsNot(Runner.__init__, pristine, "retroactively patched: already imported")
            self.assertIn("google-adk", {d.name for d in available_packs() if d.matched})
            uninstall_adapters()
            self.assertIs(Runner.__init__, pristine)


class ToolWrappingTest(AdkTestBase):
    """The redesigned seam: before_tool_callback REBINDS tool.run_async on
    first sight and returns None, so ADK's own sequence (agent-level
    before-callbacks -> real call -> on_tool_error path) proceeds unchanged
    with Keel wrapped directly around the real call."""

    def plugin(self) -> Any:
        with FakeAdkModules():
            return adk_pack._plugin()  # lazily built against the fake BasePlugin

    def see(self, plugin: Any, tool: Any, tool_args: dict[str, Any] | None = None) -> Any:
        """One before_tool_callback invocation (ADK's step 1)."""
        return asyncio.run(
            plugin.before_tool_callback(tool=tool, tool_args=tool_args or {}, tool_context=object())
        )

    def test_first_sight_rebinds_and_returns_none(self) -> None:
        # Returning None is the contract that preserves agent-level
        # before_tool_callbacks: ADK only skips them on a non-None return.
        self.install_runtime({"target": {"tool:get_weather": {}}})
        tool = FakeTool("get_weather", lambda city: {"forecast": f"sunny in {city}"})
        self.assertIsNone(self.see(self.plugin(), tool))
        self.assertTrue(getattr(tool.run_async, adk_pack._REBOUND_ATTR, False))
        self.assertEqual(tool.calls, 0, "the callback never executes the tool itself")

    def test_rebound_run_async_returns_raw_result(self) -> None:
        # Keel now sits BELOW ADK's __build_response_event, so results pass
        # through raw — dicts and scalars alike; ADK normalizes above us.
        self.install_runtime({"target": {"tool:get_weather": {}}})
        plugin = self.plugin()
        tool = FakeTool("get_weather", lambda city: {"forecast": f"sunny in {city}"})
        self.see(plugin, tool)
        result = asyncio.run(tool.run_async(args={"city": "nyc"}, tool_context=object()))
        self.assertEqual(result, {"forecast": "sunny in nyc"})
        self.assertEqual(tool.calls, 1)
        scalar = FakeTool("count", lambda: 42)
        self.see(plugin, scalar)
        self.assertEqual(asyncio.run(scalar.run_async(args={}, tool_context=object())), 42)

    def test_second_sight_does_not_double_wrap(self) -> None:
        self.install_runtime({"target": {"tool:get_weather": {}}})
        plugin = self.plugin()
        tool = FakeTool("get_weather", lambda city: city)
        self.see(plugin, tool)
        wrapped_once = tool.run_async
        self.assertIsNone(self.see(plugin, tool))
        self.assertIs(tool.run_async, wrapped_once, "second sight is a no-op")

    def test_non_idempotent_default_not_retried_e014_and_error_raises_at_the_real_call(self) -> None:
        _, discovery = self.install_runtime(
            {"target": {"tool:charge_card": {"retry": {"attempts": 3, "on": ["conn"], "schedule": "fixed(1ms)"}}}}
        )
        original = ConnectionError("reset")

        def charge() -> None:
            raise original

        tool = FakeTool("charge_card", charge)
        plugin = self.plugin()
        # Step 1 (before_tool_callback) must NOT raise — the failure has to
        # surface from the real call (ADK's step 3), inside ADK's
        # try/except, so user on_tool_error handlers fire again.
        self.assertIsNone(self.see(plugin, tool))
        with self.assertRaises(ConnectionError) as ctx:
            asyncio.run(tool.run_async(args={}, tool_context=object()))
        self.assertIs(ctx.exception, original, "original exception, not RuntimeError-wrapped")
        self.assertEqual(tool.calls, 1, "a side-effecting ADK tool is never auto-retried")
        self.assertEqual(ctx.exception.keel_outcome["error"]["code"], "KEEL-E014")
        rows = self.read_rows(discovery)
        self.assertEqual(rows["tool:charge_card"]["failures"], 1)

    def test_async_tool_function_supported(self) -> None:
        self.install_runtime({"target": {"tool:fetch": {}}})

        async def fetch() -> dict[str, str]:
            return {"page": "1"}

        tool = FakeTool("fetch", fetch)
        plugin = self.plugin()
        self.see(plugin, tool)
        self.assertEqual(asyncio.run(tool.run_async(args={}, tool_context=object())), {"page": "1"})

    def test_invalid_tool_name_skipped_unwrapped(self) -> None:
        self.install_runtime({})
        tool = FakeTool("get weather", lambda: {"ok": True})  # space: not a valid tool: name
        self.assertIsNone(self.see(self.plugin(), tool))
        self.assertFalse(getattr(tool.run_async, adk_pack._REBOUND_ATTR, False))
        self.assertEqual(tool.calls, 0)

    def test_missing_or_non_string_name_skipped(self) -> None:
        self.install_runtime({})
        tool = FakeTool("", lambda: 1)
        tool.name = None  # some exotic tool object
        self.assertIsNone(self.see(self.plugin(), tool))
        self.assertFalse(getattr(tool.run_async, adk_pack._REBOUND_ATTR, False))


class CrossThreadRebindRaceTest(AdkTestBase):
    """The check-then-act in `_on_before_tool` (the "already rebound?"
    getattr, then the separate `_rebind_tool(tool, name)` call) has no lock
    between the two steps. Within a single event loop this is safe -- there
    is no `await` between the check and the act, so asyncio can never
    interleave two callbacks mid-sequence. The real hazard is CROSS-THREAD:
    two OS threads, each driving its OWN event loop (two concurrent ADK
    Runner sessions on separate threads sharing a tool instance/toolset --
    a realistic host-app pattern), can both execute the getattr check
    before either thread's `setattr` (inside `_rebind_tool`) has landed --
    nothing serializes across that multi-bytecode sequence. Whichever
    thread's `_rebind_tool` call runs second then captures the FIRST
    thread's already-installed wrapper as `original`, silently
    double-wrapping the tool (double breaker/retry/discovery accounting on
    every real call through it from then on).

    Reproducing this reliably (not by GIL-scheduling luck) needs the race
    window WIDENED rather than merely invited: a `threading.Barrier(2,
    timeout=...)` is planted at the very first line of `_rebind_tool`
    itself (patched in for the duration of the test, restored after). A
    thread only ever reaches that line after ITS OWN "already rebound?"
    check already returned False -- so a caller arriving there has, by
    construction, already observed the pre-race state. Two callers that
    both arrive rendezvous at the barrier BEFORE either is allowed to run
    the real rebind body (the read of `tool.run_async`, the wrapper
    construction, the `setattr`) -- proving both checks raced -- which is
    exactly the "reach the already-rebound check at nearly the same
    moment" widening the brief called for. A solitary caller (the
    lock-serialized, fixed world, where the second thread's check never
    even sees False) just times out on the barrier and proceeds normally,
    so the same test terminates cleanly whether the code is locked or not.

    The observable: how many times `_rebind_tool` actually ran (how many
    times a wrap_tool-produced closure was constructed and installed).
    Locked correctly, that number is 1 no matter how the two threads are
    scheduled. Racing unlocked, forcing both callers to prove they passed
    the check before either proceeds pins it at 2 -- the exact defect."""

    def test_two_threads_two_event_loops_rebind_exactly_once(self) -> None:
        self.install_runtime({"target": {"tool:get_weather": {}}})
        tool = FakeTool("get_weather", lambda city: city)

        real_rebind_tool = adk_pack._rebind_tool
        # timeout: a lock-serialized (fixed) world only ever has ONE caller
        # reach this barrier -- it must not hang forever waiting for a
        # second party that a correct fix never sends.
        barrier = threading.Barrier(2, timeout=1.0)
        count_lock = threading.Lock()
        calls = {"n": 0}

        def counting_rebind_tool(t: Any, name: str) -> bool:
            with count_lock:
                calls["n"] += 1
            try:
                barrier.wait()
            except threading.BrokenBarrierError:
                pass  # solitary caller: the other thread's check already saw "rebound" (the fix)
            return real_rebind_tool(t, name)

        errors: list[BaseException] = []

        def worker() -> None:
            try:
                asyncio.run(adk_pack._on_before_tool(tool, {}, object()))
            except BaseException as exc:  # pragma: no cover - surfaced via assertion, not silently lost
                errors.append(exc)

        with mock.patch.object(adk_pack, "_rebind_tool", counting_rebind_tool):
            t1 = threading.Thread(target=worker)
            t2 = threading.Thread(target=worker)
            t1.start()
            t2.start()
            t1.join(timeout=5)
            t2.join(timeout=5)

        self.assertFalse(t1.is_alive() or t2.is_alive(), "a worker thread hung")
        self.assertFalse(errors, f"worker thread(s) raised: {errors!r}")
        self.assertTrue(
            getattr(tool.run_async, adk_pack._REBOUND_ATTR, False),
            "the tool ends up rebound either way",
        )
        self.assertEqual(
            calls["n"],
            1,
            "the tool was rebound more than once: two threads both observed "
            "'not yet rebound' and both ran _rebind_tool -- the cross-thread "
            "check-then-act race this test targets",
        )


class RebindLifecycleTest(AdkTestBase):
    def test_uninstall_restores_rebound_instances(self) -> None:
        self.install_runtime({"target": {"tool:get_weather": {}}})
        with FakeAdkModules():
            adk_pack.install()
            tool = FakeTool("get_weather", lambda city: city)
            asyncio.run(
                adk_pack._plugin().before_tool_callback(
                    tool=tool, tool_args={}, tool_context=object()
                )
            )
            self.assertIn("run_async", tool.__dict__, "rebind shadows the class method")
            adk_pack.uninstall()
            self.assertNotIn("run_async", tool.__dict__, "shadow removed: class method restored")
            self.assertEqual(asyncio.run(tool.run_async(args={"city": "x"}, tool_context=object())), "x")

    def test_restore_rebound_setattr_branch_restores_prior_instance_attribute(self) -> None:
        # When a tool ALREADY carries its own instance-level run_async (some
        # pre-existing shadow, not the class method) at the moment Keel first
        # sees it, `_rebind_tool` captures that as `prior` — not `_ABSENT`.
        # `_restore_rebound`'s `prior is not _ABSENT` branch must setattr
        # that ORIGINAL instance attribute back on uninstall, not delattr it
        # (delattr would wrongly fall through to the class method instead of
        # restoring what the tool actually had before Keel touched it).
        self.install_runtime({"target": {"tool:get_weather": {}}})
        with FakeAdkModules():
            adk_pack.install()
            tool = FakeTool("get_weather", lambda city: city)

            async def prior_run_async(*, args: dict[str, Any], tool_context: Any) -> Any:
                return "prior-instance-value"

            tool.run_async = prior_run_async  # instance-level shadow, pre-existing
            asyncio.run(
                adk_pack._plugin().before_tool_callback(
                    tool=tool, tool_args={}, tool_context=object()
                )
            )
            self.assertIsNot(tool.run_async, prior_run_async, "Keel rebinds over the prior shadow")
            adk_pack.uninstall()
            self.assertIn("run_async", tool.__dict__, "instance attribute restored, not deleted")
            self.assertIs(tool.run_async, prior_run_async, "the ORIGINAL instance attribute is restored")
        self.assertEqual(
            asyncio.run(tool.run_async(args={}, tool_context=object())), "prior-instance-value"
        )


class SetattrFallbackTest(AdkTestBase):
    """A tool instance that rejects rebinding still gets full Keel coverage
    via the old loop-in-callback path — coverage never silently drops."""

    def plugin(self) -> Any:
        with FakeAdkModules():
            return adk_pack._plugin()

    def test_slotted_tool_falls_back_to_plugin_loop_with_normalized_result(self) -> None:
        self.install_runtime({"target": {"tool:count": {}}})
        tool = FakeSlottedTool("count", lambda: 42)
        with mock.patch.object(sys, "stderr", new_callable=io.StringIO) as err:
            result = asyncio.run(
                self.plugin().before_tool_callback(tool=tool, tool_args={}, tool_context=object())
            )
        # Fallback = the pre-redesign contract: executed in the callback,
        # normalized like ADK's own __build_response_event.
        self.assertEqual(result, {"result": 42})
        self.assertEqual(tool.calls, 1)
        self.assertIn("rejects attribute rebinding", err.getvalue())

    def test_fallback_failure_accounting_and_e014(self) -> None:
        _, discovery = self.install_runtime(
            {"target": {"tool:charge": {"retry": {"attempts": 3, "on": ["conn"], "schedule": "fixed(1ms)"}}}}
        )
        original = ConnectionError("reset")

        def charge() -> None:
            raise original

        tool = FakeSlottedTool("charge", charge)
        with self.assertRaises(ConnectionError) as ctx:
            asyncio.run(
                self.plugin().before_tool_callback(tool=tool, tool_args={}, tool_context=object())
            )
        self.assertIs(ctx.exception, original)
        self.assertEqual(tool.calls, 1)
        self.assertEqual(ctx.exception.keel_outcome["error"]["code"], "KEEL-E014")
        self.assertEqual(self.read_rows(discovery)["tool:charge"]["failures"], 1)

    def test_fallback_note_emitted_once_and_quietable(self) -> None:
        self.install_runtime({"target": {"tool:count": {}}})
        plugin = self.plugin()
        tool = FakeSlottedTool("count", lambda: 1)
        with mock.patch.object(sys, "stderr", new_callable=io.StringIO) as err:
            asyncio.run(plugin.before_tool_callback(tool=tool, tool_args={}, tool_context=object()))
            asyncio.run(plugin.before_tool_callback(tool=tool, tool_args={}, tool_context=object()))
        self.assertEqual(err.getvalue().count("rejects attribute rebinding"), 1, "noted once, not per-call")
        adk_pack._noted_fallbacks.clear()
        with mock.patch.dict(os.environ, {"KEEL_QUIET": "1"}), mock.patch.object(
            sys, "stderr", new_callable=io.StringIO
        ) as err:
            asyncio.run(plugin.before_tool_callback(tool=tool, tool_args={}, tool_context=object()))
        self.assertEqual(err.getvalue(), "")


class McpErrorDictTest(AdkTestBase):
    """ADK graceful error handling returns {"error": ...} dicts from McpTool
    — a *successful* call from a naive wrapper's perspective. Keel must
    count it as a failure (breaker/discovery) while returning it unchanged.

    Also covers the opt-in `KEEL_MCP_CLASSIFY_ISERROR` path (issue #16): the
    raw MCP `isError: true` business-logic convention, invisible to ADK's
    own success/failure handling, classified as a failure only when the env
    var is explicitly set — default OFF, byte-identical otherwise."""

    def plugin(self) -> Any:
        with FakeAdkModules():
            return adk_pack._plugin()

    def rebound(self, tool: Any) -> Any:
        plugin = self.plugin()
        asyncio.run(plugin.before_tool_callback(tool=tool, tool_args={}, tool_context=object()))
        return tool

    def test_error_dict_counts_as_failure_but_returns_unchanged(self) -> None:
        _, discovery = self.install_runtime({"target": {"tool:mcp_search": {}}})
        tool = self.rebound(McpTool("mcp_search", lambda: {"error": "connection closed"}))
        result = asyncio.run(tool.run_async(args={}, tool_context=object()))
        self.assertEqual(result, {"error": "connection closed"}, "agent-visible value unchanged")
        self.assertEqual(tool.calls, 1, "never re-invoked: tools stay non-idempotent")
        rows = self.read_rows(discovery)
        self.assertEqual(rows["tool:mcp_search"]["failures"], 1, "breaker/discovery sees the failure")

    def test_successful_mcp_result_recorded_as_success(self) -> None:
        _, discovery = self.install_runtime({"target": {"tool:mcp_search": {}}})
        tool = self.rebound(McpTool("mcp_search", lambda: {"content": ["hit"]}))
        result = asyncio.run(tool.run_async(args={}, tool_context=object()))
        self.assertEqual(result, {"content": ["hit"]})
        rows = self.read_rows(discovery)
        self.assertEqual(rows["tool:mcp_search"]["failures"], 0)

    def test_non_mcp_tool_error_shaped_dict_is_not_reclassified(self) -> None:
        # A plain FunctionTool legitimately returning {"error": ...} is a
        # RESULT, not a failure — classification applies to McpTool only.
        _, discovery = self.install_runtime({"target": {"tool:validator": {}}})
        tool = self.rebound(FakeTool("validator", lambda: {"error": "field x is required"}))
        result = asyncio.run(tool.run_async(args={}, tool_context=object()))
        self.assertEqual(result, {"error": "field x is required"})
        rows = self.read_rows(discovery)
        self.assertEqual(rows["tool:validator"]["failures"], 0)

    def test_extra_keys_or_non_string_error_is_not_an_error_dict(self) -> None:
        self.assertFalse(adk_pack._is_mcp_error_dict({"error": "x", "detail": "y"}))
        self.assertFalse(adk_pack._is_mcp_error_dict({"error": 500}))
        self.assertFalse(adk_pack._is_mcp_error_dict(["error"]))
        self.assertTrue(adk_pack._is_mcp_error_dict({"error": "x"}))

    def test_is_mcp_tool_true_for_deprecated_mcptool_alias_by_name_alone(self) -> None:
        # ADK's deprecated alias `class MCPTool(McpTool)` (mcp_tool.py line
        # 602) is matched by _is_mcp_tool's MRO-name check purely on the
        # class NAME "MCPTool" — not by inheriting from this fixture's
        # McpTool. Construct a class named "MCPTool" with no relation to the
        # fixture at all to prove the check fires on the name, not the type.
        class MCPTool:
            pass

        self.assertTrue(adk_pack._is_mcp_tool(MCPTool()))

    # -- issue #16: opt-in raw MCP `isError: true` classification ----------

    def test_iserror_dict_classified_as_failure_when_env_var_set(self) -> None:
        _, discovery = self.install_runtime({"target": {"tool:mcp_search": {}}})
        payload = {"content": [{"type": "text", "text": "not found"}], "isError": True}
        with mock.patch.dict(os.environ, {"KEEL_MCP_CLASSIFY_ISERROR": "1"}):
            tool = self.rebound(McpTool("mcp_search", lambda: dict(payload)))
            result = asyncio.run(tool.run_async(args={}, tool_context=object()))
        self.assertEqual(result, payload, "agent-visible value unchanged")
        rows = self.read_rows(discovery)
        self.assertEqual(rows["tool:mcp_search"]["failures"], 1, "breaker/discovery sees the failure")

    def test_iserror_dict_not_reclassified_when_env_var_unset(self) -> None:
        # Default OFF: identical isError:true result passes through as an
        # ordinary successful result — byte-identical to pre-opt-in behavior.
        _, discovery = self.install_runtime({"target": {"tool:mcp_search": {}}})
        payload = {"content": [{"type": "text", "text": "not found"}], "isError": True}
        self.assertNotIn("KEEL_MCP_CLASSIFY_ISERROR", os.environ)
        tool = self.rebound(McpTool("mcp_search", lambda: dict(payload)))
        result = asyncio.run(tool.run_async(args={}, tool_context=object()))
        self.assertEqual(result, payload)
        rows = self.read_rows(discovery)
        self.assertEqual(rows["tool:mcp_search"]["failures"], 0, "default OFF: no reclassification")

    def test_transport_error_dict_still_unconditional_regardless_of_env_var(self) -> None:
        # Regression: the pre-existing {"error": ...} classification must
        # never become gated by the new opt-in, in either env var state.
        # Distinct target names per iteration: install_runtime reuses the
        # same on-disk discovery db across calls in this test (same self.cwd)
        # so failure counts would otherwise accumulate across iterations.
        cases = [
            ({}, "tool:mcp_search_env_unset"),
            ({"KEEL_MCP_CLASSIFY_ISERROR": "1"}, "tool:mcp_search_env_set"),
        ]
        for env, target in cases:
            with self.subTest(env=env):
                name = target.removeprefix("tool:")
                _, discovery = self.install_runtime({"target": {target: {}}})
                tool = self.rebound(McpTool(name, lambda: {"error": "connection closed"}))
                with mock.patch.dict(os.environ, env):
                    result = asyncio.run(tool.run_async(args={}, tool_context=object()))
                self.assertEqual(result, {"error": "connection closed"})
                rows = self.read_rows(discovery)
                self.assertEqual(rows[target]["failures"], 1)

    def test_is_mcp_business_error_dict_shape_matching(self) -> None:
        # Strict match: isError literally True (not merely truthy) plus a
        # `content` list, keys drawn only from CallToolResult's real fields.
        self.assertTrue(
            adk_pack._is_mcp_business_error_dict({"content": [], "isError": True})
        )
        self.assertTrue(
            adk_pack._is_mcp_business_error_dict(
                {"content": [], "isError": True, "structuredContent": {"code": 404}}
            )
        )
        self.assertTrue(
            adk_pack._is_mcp_business_error_dict(
                {"content": [], "isError": True, "meta": {"trace": "abc"}}
            )
        )
        # A successful call: isError present but False (never omitted by
        # ADK's exclude_none dump, since False is not None).
        self.assertFalse(
            adk_pack._is_mcp_business_error_dict({"content": [], "isError": False})
        )
        # Truthy but not the literal bool True.
        self.assertFalse(
            adk_pack._is_mcp_business_error_dict({"content": [], "isError": 1})
        )
        # Missing/wrong-typed `content`.
        self.assertFalse(adk_pack._is_mcp_business_error_dict({"isError": True}))
        self.assertFalse(
            adk_pack._is_mcp_business_error_dict({"content": "oops", "isError": True})
        )
        # A key outside CallToolResult's real field set.
        self.assertFalse(
            adk_pack._is_mcp_business_error_dict(
                {"content": [], "isError": True, "extra": "nope"}
            )
        )
        # The transport-failure shape must never match here.
        self.assertFalse(adk_pack._is_mcp_business_error_dict({"error": "x"}))
        self.assertFalse(adk_pack._is_mcp_business_error_dict(["isError"]))


class _NoFlowSurfaceBackend:
    """A stub-shaped backend: no `enter_flow`/`exit_flow` at all."""


class _FlowCapableBackend:
    """A native-shaped backend: has the flow surface; `persistent` toggles
    whether a journal is attached."""

    def __init__(self, persistent: bool) -> None:
        self.persistent = persistent

    def enter_flow(self, *args: Any, **kwargs: Any) -> dict[str, Any]:
        return {}

    def exit_flow(self, *args: Any, **kwargs: Any) -> None:
        return None


class RunnerFlowDesignationTest(unittest.TestCase):
    """WS5 foundation: the Runner-flow designation matcher, Tier-2 gates, and
    flow-identity helpers that Task 2's generator wrap will consume. No
    generator wrap here — that is the NEXT task."""

    def setUp(self) -> None:
        self._prior_flow_entrypoints = _runtime.get_flow_entrypoints()

    def tearDown(self) -> None:
        # `_flow_entrypoint_designated` reads `_runtime.get_flow_entrypoints()`
        # (see its docstring), so every test that pokes it must restore the
        # real suite-wide state afterward.
        _runtime.set_flow_entrypoints(self._prior_flow_entrypoints)

    # -- _flow_entrypoint_designated ------------------------------------

    def test_designated_when_exact_match_present(self) -> None:
        entry = FlowEntrypoint(
            raw="py:google.adk.runners:Runner.run_async",
            module="google.adk.runners",
            function="Runner.run_async",
        )
        _runtime.set_flow_entrypoints([entry])
        self.assertEqual(
            adk_pack._flow_entrypoint_designated(),
            "py:google.adk.runners:Runner.run_async",
        )

    def test_undesignated_when_never_installed_or_disabled(self) -> None:
        # `install_keel()` never calls `_runtime.set_flow_entrypoints()` when
        # `KEEL_DISABLE` is set (it returns before that point) —
        # bootstrap-disabled and never-installed are the SAME shape here:
        # `_runtime.get_flow_entrypoints()` returns `()`.
        _runtime.set_flow_entrypoints(())
        self.assertIsNone(adk_pack._flow_entrypoint_designated())

    def test_undesignated_when_no_matching_entrypoint(self) -> None:
        other = FlowEntrypoint(raw="py:pipeline:main", module="pipeline", function="main")
        _runtime.set_flow_entrypoints([other])
        self.assertIsNone(adk_pack._flow_entrypoint_designated())

    def test_undesignated_when_no_flow_entrypoints_at_all(self) -> None:
        _runtime.set_flow_entrypoints([])
        self.assertIsNone(adk_pack._flow_entrypoint_designated())

    def test_glob_entrypoint_does_not_designate(self) -> None:
        # Designation is EXACT-match only: a glob over
        # google.adk.runners does not count, even though it could in
        # principle resolve to the same module/function pair.
        glob_entry = FlowEntrypoint(
            raw="py:google.adk.*:Runner.run_async",
            module="google.adk.*",
            function="Runner.run_async",
        )
        _runtime.set_flow_entrypoints([glob_entry])
        self.assertIsNone(adk_pack._flow_entrypoint_designated())

    def test_runner_flow_entrypoint_constant(self) -> None:
        self.assertEqual(
            adk_pack.RUNNER_FLOW_ENTRYPOINT, "py:google.adk.runners:Runner.run_async"
        )

    # -- _flow_gates_or_raise --------------------------------------------

    def test_gates_raise_e005_on_stub_shaped_backend(self) -> None:
        with self.assertRaises(KeelError) as ctx:
            adk_pack._flow_gates_or_raise(_NoFlowSurfaceBackend())
        self.assertEqual(ctx.exception.code, "KEEL-E005")
        self.assertIn("needs the native core", ctx.exception.message)

    def test_gates_raise_e005_without_journal(self) -> None:
        with self.assertRaises(KeelError) as ctx:
            adk_pack._flow_gates_or_raise(_FlowCapableBackend(persistent=False))
        self.assertEqual(ctx.exception.code, "KEEL-E005")
        self.assertIn("needs a journal", ctx.exception.message)

    def test_gates_pass_on_flow_capable_backend(self) -> None:
        self.assertIsNone(adk_pack._flow_gates_or_raise(_FlowCapableBackend(persistent=True)))

    def test_gates_never_raise_system_exit(self) -> None:
        # `keel._flow`'s CLI-facing helpers exit the process; this pack's
        # gates must RAISE instead, since a Runner call is a library call,
        # not a CLI entrypoint.
        try:
            adk_pack._flow_gates_or_raise(_NoFlowSurfaceBackend())
        except SystemExit:
            self.fail("_flow_gates_or_raise must raise KeelError, never SystemExit")
        except KeelError:
            pass

    # -- _runner_flow_identity / _runner_args_hash / _content_fingerprint --

    def test_identity_stable_for_same_user_session_invocation(self) -> None:
        first = adk_pack._runner_flow_identity("u1", "s1", "inv-1", {"role": "user"})
        second = adk_pack._runner_flow_identity("u1", "s1", "inv-1", {"role": "user"})
        self.assertEqual(first, second)
        self.assertEqual(first[0], adk_pack.RUNNER_FLOW_ENTRYPOINT)

    def test_identity_differs_across_invocation_ids(self) -> None:
        first = adk_pack._runner_flow_identity("u1", "s1", "inv-1", {"role": "user"})
        second = adk_pack._runner_flow_identity("u1", "s1", "inv-2", {"role": "user"})
        self.assertNotEqual(first[1], second[1])

    def test_identity_differs_across_users_or_sessions(self) -> None:
        base = adk_pack._runner_flow_identity("u1", "s1", "inv-1", {})
        other_user = adk_pack._runner_flow_identity("u2", "s1", "inv-1", {})
        other_session = adk_pack._runner_flow_identity("u1", "s2", "inv-1", {})
        self.assertNotEqual(base[1], other_user[1])
        self.assertNotEqual(base[1], other_session[1])

    def test_identity_uses_content_fingerprint_when_invocation_id_none(self) -> None:
        same_message = {"role": "user", "text": "hi"}
        first = adk_pack._runner_flow_identity("u1", "s1", None, same_message)
        second = adk_pack._runner_flow_identity("u1", "s1", None, same_message)
        self.assertEqual(first, second, "same content fingerprint => same identity")
        different = adk_pack._runner_flow_identity("u1", "s1", None, {"role": "user", "text": "bye"})
        self.assertNotEqual(first[1], different[1])

    def test_content_fingerprint_is_16_hex_sha256_of_repr(self) -> None:
        message = {"role": "user", "text": "hi"}
        expected = hashlib.sha256(repr(message).encode()).hexdigest()[:16]
        self.assertEqual(adk_pack._content_fingerprint(message), expected)

    def test_runner_args_hash_matches_flow_args_hash_algorithm(self) -> None:
        parts = ["u1", "s1", "inv-1"]
        expected = hashlib.sha256(repr(list(parts)).encode()).hexdigest()[:16]
        self.assertEqual(adk_pack._runner_args_hash(parts), expected)


class _FakeAdkFlowBackend:
    """A native-shaped double for the run_async flow wrap: mirrors
    `test_flows.py`'s `_FakeFlowBackend` (enter/exit recorded, `persistent`
    toggles journal presence — both gates `_flow_gates_or_raise` checks pass
    by default here) plus `journal_random`, which echoes back the FIRST
    value ever recorded for a key on every later call — modeling the native
    core's replay behavior (a resumed flow gets the recorded value back, not
    a fresh one) well enough to assert correlation without the compiled
    core."""

    def __init__(
        self, replay: bool = False, persistent: bool = True, throw_on_exit: bool = False
    ) -> None:
        self.entered: list[tuple[Any, ...]] = []
        self.exited: list[str] = []
        self.random: dict[str, bytes] = {}
        self._replay = replay
        self.persistent = persistent
        # Minimal re-enter support (decision 8 revision): once a prior
        # attempt for a given (entrypoint, args_hash) identity has recorded
        # an exit (any status — abandonment now exits "failed" too), the
        # NEXT enter_flow for that same identity comes back as a replay,
        # modeling the native core substituting the already-recorded steps.
        self._prior_exit_for: dict[tuple[str, str], str] = {}
        # Models issue #14: exit_flow's own journal WRITE fails, distinct
        # from whatever outcome the wrapped run_async body itself just had.
        self._throw_on_exit = throw_on_exit

    def enter_flow(
        self,
        entrypoint: str,
        args_hash: str,
        code_hash: str | None = None,
        explicit_key: str | None = None,
        lease_ms: int | None = None,
    ) -> dict[str, Any]:
        self.entered.append((entrypoint, args_hash, code_hash, explicit_key, lease_ms))
        replay = self._replay or (entrypoint, args_hash) in self._prior_exit_for
        return {
            "flow_id": "fid-1",
            "status": "completed" if replay else "running",
            "replay": replay,
        }

    def exit_flow(self, status: str) -> None:
        self.exited.append(status)
        if self.entered:
            entrypoint, args_hash = self.entered[-1][0], self.entered[-1][1]
            self._prior_exit_for[(entrypoint, args_hash)] = status
        if self._throw_on_exit:
            raise KeelError("KEEL-E040", "complete_flow failed: injected failure")

    def journal_random(self, key: str, data: bytes) -> bytes:
        return self.random.setdefault(key, data)


class RunnerFlowWrapTest(AdkTestBase):
    """WS5 core: a DESIGNATED `Runner.run_async` invocation becomes a Tier 2
    journaled flow (`adk_pack._run_async_wrapper`'s async-generator patch);
    every other call stays byte-transparent. Runs entirely against
    `_FakeAdkFlowBackend` — no compiled core, no real `google.adk`."""

    # No setUp/tearDown override needed: `AdkTestBase.tearDown()` already
    # calls `_runtime.clear_runtime()`, which resets `flow_entrypoints`
    # (along with backend/discovery/flow_active) after every test.

    def designate(self) -> None:
        entry = FlowEntrypoint(
            raw=adk_pack.RUNNER_FLOW_ENTRYPOINT,
            module="google.adk.runners",
            function="Runner.run_async",
        )
        _runtime.set_flow_entrypoints([entry])

    def undesignate(self) -> None:
        _runtime.set_flow_entrypoints(())

    def use_backend(self, backend: Any) -> None:
        discovery = Discovery(self.cwd)
        self._discoveries.append(discovery)
        _runtime.set_runtime(backend, discovery)

    async def _drain(self, agen: Any) -> list[Any]:
        return [event async for event in agen]

    # -- full lifecycle ----------------------------------------------------

    def test_full_lifecycle_enters_correlates_and_completes(self) -> None:
        self.designate()
        backend = _FakeAdkFlowBackend()
        self.use_backend(backend)
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
        self.assertEqual(len(backend.entered), 1)
        entrypoint, args_hash, code_hash, explicit_key, lease_ms = backend.entered[0]
        self.assertEqual(entrypoint, adk_pack.RUNNER_FLOW_ENTRYPOINT)
        self.assertIsNone(code_hash)
        self.assertEqual(explicit_key, "inv-1", "explicit_key is the raw invocation_id")
        _, expected_hash = adk_pack._runner_flow_identity("u1", "s1", "inv-1", {"text": "hi"})
        self.assertEqual(args_hash, expected_hash)
        self.assertEqual(backend.exited, ["completed"])
        self.assertEqual(backend.random["adk:invocation_id"], b"inv-1")
        self.assertFalse(_runtime.in_active_flow(), "flow_active reset after a clean completion")

    def test_lease_ms_env_forwarded(self) -> None:
        self.designate()
        backend = _FakeAdkFlowBackend()
        self.use_backend(backend)
        with FakeAdkModules(), mock.patch.dict(os.environ, {"KEEL_FLOW_LEASE_MS": "5000"}):
            adk_pack.install()
            from google.adk.runners import Runner

            runner = Runner(app_name="app", events=[FakeEvent(invocation_id="inv-1")])
            asyncio.run(
                self._drain(runner.run_async(user_id="u1", session_id="s1", invocation_id="inv-1"))
            )
            adk_pack.uninstall()
        self.assertEqual(backend.entered[0][4], 5000)

    # -- failure mid-stream --------------------------------------------------

    def test_failure_mid_stream_marks_failed_and_reraises_original(self) -> None:
        self.designate()
        backend = _FakeAdkFlowBackend()
        self.use_backend(backend)
        boom = RuntimeError("boom")
        with FakeAdkModules():
            adk_pack.install()
            from google.adk.runners import Runner

            runner = Runner(app_name="app", events=[FakeEvent(invocation_id="inv-1"), boom])
            with self.assertRaises(RuntimeError) as ctx:
                asyncio.run(
                    self._drain(runner.run_async(user_id="u1", session_id="s1", invocation_id="inv-1"))
                )
            adk_pack.uninstall()
        self.assertIs(ctx.exception, boom, "original exception, never wrapped")
        self.assertEqual(backend.exited, ["failed"])
        self.assertFalse(_runtime.in_active_flow(), "flow_active reset after a real failure")

    def test_keyboard_interrupt_mid_stream_marks_failed_and_reraises_original(self) -> None:
        # Unlike `_flow.py`'s `run_as_flow` (which leaves a `KeyboardInterrupt`
        # flow `running` for resume, since that path expects the process to
        # die), this wrapper runs inside a SURVIVING long-lived Runner host —
        # its `except BaseException` arm treats `KeyboardInterrupt` the same
        # as any other failure (`_run_async_wrapper` docstring).
        self.designate()
        backend = _FakeAdkFlowBackend()
        self.use_backend(backend)
        boom = KeyboardInterrupt()
        with FakeAdkModules():
            adk_pack.install()
            from google.adk.runners import Runner

            runner = Runner(app_name="app", events=[FakeEvent(invocation_id="inv-1"), boom])
            with self.assertRaises(KeyboardInterrupt) as ctx:
                asyncio.run(
                    self._drain(runner.run_async(user_id="u1", session_id="s1", invocation_id="inv-1"))
                )
            adk_pack.uninstall()
        self.assertIs(ctx.exception, boom, "original exception, never wrapped")
        self.assertEqual(backend.exited, ["failed"])
        self.assertFalse(_runtime.in_active_flow(), "flow_active reset after a real failure")

    def test_cancelled_error_mid_stream_marks_failed_and_reraises_original(self) -> None:
        # Same rationale as the `KeyboardInterrupt` case above:
        # `asyncio.CancelledError` is an async-generator's own
        # abandonment/cancel signal, name-checked alongside `KeyboardInterrupt`
        # in the same `except BaseException` arm rather than being left
        # `running` for resume (`_run_async_wrapper` docstring).
        self.designate()
        backend = _FakeAdkFlowBackend()
        self.use_backend(backend)
        boom = asyncio.CancelledError()
        with FakeAdkModules():
            adk_pack.install()
            from google.adk.runners import Runner

            runner = Runner(app_name="app", events=[FakeEvent(invocation_id="inv-1"), boom])
            with self.assertRaises(asyncio.CancelledError) as ctx:
                asyncio.run(
                    self._drain(runner.run_async(user_id="u1", session_id="s1", invocation_id="inv-1"))
                )
            adk_pack.uninstall()
        self.assertIs(ctx.exception, boom, "original exception, never wrapped")
        self.assertEqual(backend.exited, ["failed"])
        self.assertFalse(_runtime.in_active_flow(), "flow_active reset after a real failure")

    def test_exit_flow_write_failure_on_success_path_is_reported_not_raised(self) -> None:
        # Issue #14: exit_flow can now raise a KEEL-E040 when the JOURNAL
        # WRITE itself fails, distinct from the wrapped run_async body's own
        # outcome. On the success (`else`) path that failure must degrade to
        # a stderr line, not propagate out of an async generator the caller
        # is iterating normally.
        self.designate()
        backend = _FakeAdkFlowBackend(throw_on_exit=True)
        self.use_backend(backend)
        with FakeAdkModules():
            adk_pack.install()
            from google.adk.runners import Runner

            runner = Runner(app_name="app", events=[FakeEvent(invocation_id="inv-1")])
            with mock.patch.object(sys, "stderr", new_callable=io.StringIO) as err:
                events = asyncio.run(
                    self._drain(
                        runner.run_async(user_id="u1", session_id="s1", invocation_id="inv-1")
                    )
                )
            adk_pack.uninstall()
        self.assertEqual(len(events), 1)
        self.assertEqual(backend.exited, ["completed"])
        self.assertFalse(_runtime.in_active_flow(), "flow_active still reset despite the write failure")
        self.assertIn("KEEL-E040", err.getvalue())
        self.assertIn("not journaled", err.getvalue())

    def test_exit_flow_write_failure_never_replaces_the_flow_bodys_own_exception(self) -> None:
        # The critical issue #14 regression this pins: exit_flow("failed")
        # raising must NOT prevent the wrapped body's real exception from
        # propagating unchanged — it must not be replaced by the unrelated
        # journal-write error.
        self.designate()
        backend = _FakeAdkFlowBackend(throw_on_exit=True)
        self.use_backend(backend)
        boom = RuntimeError("boom")
        with FakeAdkModules():
            adk_pack.install()
            from google.adk.runners import Runner

            runner = Runner(app_name="app", events=[FakeEvent(invocation_id="inv-1"), boom])
            with mock.patch.object(sys, "stderr", new_callable=io.StringIO) as err:
                with self.assertRaises(RuntimeError) as ctx:
                    asyncio.run(
                        self._drain(
                            runner.run_async(user_id="u1", session_id="s1", invocation_id="inv-1")
                        )
                    )
            adk_pack.uninstall()
        self.assertIs(ctx.exception, boom, "original exception, never replaced by the journal error")
        self.assertEqual(backend.exited, ["failed"])
        self.assertIn("KEEL-E040", err.getvalue())
        self.assertIn("not journaled", err.getvalue())

    # -- abandonment (GeneratorExit) -----------------------------------------

    def test_abandonment_releases_the_flow_for_in_process_retry(self) -> None:
        # Decision 8, revised: in a SURVIVING process, leaving the flow
        # running-and-active forever after abandonment wedges every later
        # same-identity turn (silently unwrapped) and makes in-process
        # resume impossible. Abandonment now counts an attempt (exit
        # "failed") and releases the handle, exactly like any other failure.
        self.designate()
        backend = _FakeAdkFlowBackend()
        self.use_backend(backend)
        with FakeAdkModules():
            adk_pack.install()
            from google.adk.runners import Runner

            runner = Runner(
                app_name="app",
                events=[
                    FakeEvent(invocation_id="inv-1"),
                    FakeEvent(invocation_id="inv-1"),
                    FakeEvent(invocation_id="inv-1"),
                ],
            )

            async def abandon() -> Any:
                gen = runner.run_async(user_id="u1", session_id="s1", invocation_id="inv-1")
                first = await gen.__anext__()
                await gen.aclose()
                return first

            first = asyncio.run(abandon())
            self.assertIsNotNone(first)
            self.assertEqual(len(backend.entered), 1)
            self.assertEqual(backend.exited, ["failed"], "abandonment now counts an attempt")
            self.assertFalse(_runtime.in_active_flow(), "abandonment releases the flow handle")

            # A second designated run_async with the SAME identity, on the
            # SAME backend, must be able to re-enter (not wedged busy) and
            # complete wrapped.
            runner2 = Runner(app_name="app", events=[FakeEvent(invocation_id="inv-1")])
            events2 = asyncio.run(
                self._drain(
                    runner2.run_async(user_id="u1", session_id="s1", invocation_id="inv-1")
                )
            )
            adk_pack.uninstall()
        self.assertEqual(len(events2), 1, "second turn completes wrapped, not the busy path")
        self.assertEqual(len(backend.entered), 2, "re-entered for the retry, not skipped as busy")
        self.assertEqual(backend.exited, ["failed", "completed"])

    def test_abandonment_exit_flow_write_failure_does_not_break_aclose(self) -> None:
        # Issue #14: exit_flow("failed") raising inside the GeneratorExit
        # handler must NOT make gen.aclose() itself raise — ADK's Runner is
        # not written to expect aclose() to fail, and this handler's own
        # `raise` (re-raising GeneratorExit to close the generator normally)
        # must still be what the caller observes.
        self.designate()
        backend = _FakeAdkFlowBackend(throw_on_exit=True)
        self.use_backend(backend)
        with FakeAdkModules():
            adk_pack.install()
            from google.adk.runners import Runner

            runner = Runner(
                app_name="app",
                events=[FakeEvent(invocation_id="inv-1"), FakeEvent(invocation_id="inv-1")],
            )

            async def abandon() -> None:
                gen = runner.run_async(user_id="u1", session_id="s1", invocation_id="inv-1")
                await gen.__anext__()
                await gen.aclose()  # must not raise despite exit_flow failing

            with mock.patch.object(sys, "stderr", new_callable=io.StringIO) as err:
                asyncio.run(abandon())
            adk_pack.uninstall()
        self.assertEqual(backend.exited, ["failed"])
        self.assertFalse(_runtime.in_active_flow(), "flow_active reset despite the write failure")
        self.assertIn("KEEL-E040", err.getvalue())
        self.assertIn("not journaled", err.getvalue())

    def test_replay_completed_entry_never_demoted(self) -> None:
        # An already-COMPLETED (replayed) flow must never be demoted to
        # failed — the `if not replayed` guard on the failure paths.
        self.designate()

        # (a) exhaustion path: a replay=True entry still records "completed"
        # on success — `exit_flow("completed")` in the success branch is
        # unconditional on `replayed`, mirroring `_flow.py`'s own
        # completed -> completed unconditional final line.
        backend_ok = _FakeAdkFlowBackend(replay=True)
        self.use_backend(backend_ok)
        with FakeAdkModules():
            adk_pack.install()
            from google.adk.runners import Runner

            runner = Runner(app_name="app", events=[FakeEvent(invocation_id="inv-1")])
            events = asyncio.run(
                self._drain(runner.run_async(user_id="u1", session_id="s1", invocation_id="inv-1"))
            )
            adk_pack.uninstall()
        self.assertEqual(len(events), 1)
        self.assertEqual(backend_ok.exited, ["completed"])

        # (b) mid-stream exception on a replay=True entry: exit_flow(
        # "failed") must NOT be recorded.
        backend_fail = _FakeAdkFlowBackend(replay=True)
        self.use_backend(backend_fail)
        boom = RuntimeError("boom")
        with FakeAdkModules():
            adk_pack.install()
            from google.adk.runners import Runner

            runner = Runner(app_name="app", events=[FakeEvent(invocation_id="inv-1"), boom])
            with self.assertRaises(RuntimeError) as ctx:
                asyncio.run(
                    self._drain(runner.run_async(user_id="u1", session_id="s1", invocation_id="inv-1"))
                )
            adk_pack.uninstall()
        self.assertIs(ctx.exception, boom, "original exception, never wrapped")
        self.assertEqual(backend_fail.exited, [], "a replayed flow is never demoted to failed")
        self.assertFalse(_runtime.in_active_flow(), "flow_active still reset without an exit_flow call")

    # -- raise before the first event (review trace 2) -----------------------

    def test_raise_before_first_event_marks_failed_and_resets_flag(self) -> None:
        # The inner generator's very first __anext__ raises, before any
        # event is ever yielded (so `correlated`/journal_random never fire).
        # Must still mark the flow failed and reset flow_active.
        self.designate()
        backend = _FakeAdkFlowBackend()
        self.use_backend(backend)
        boom = RuntimeError("boom before any event")
        with FakeAdkModules():
            adk_pack.install()
            from google.adk.runners import Runner

            runner = Runner(app_name="app", events=[boom])
            with self.assertRaises(RuntimeError) as ctx:
                asyncio.run(
                    self._drain(runner.run_async(user_id="u1", session_id="s1", invocation_id="inv-1"))
                )
            adk_pack.uninstall()
        self.assertIs(ctx.exception, boom, "original exception, never wrapped")
        self.assertEqual(backend.exited, ["failed"])
        self.assertFalse(_runtime.in_active_flow(), "flow_active reset even on an immediate failure")

    # -- busy path (another flow already open) -------------------------------

    def test_busy_path_notes_once_and_passes_through_unwrapped(self) -> None:
        self.designate()
        backend = _FakeAdkFlowBackend()
        self.use_backend(backend)
        _runtime.set_flow_active(True)  # another flow is already open on this backend
        with FakeAdkModules():
            adk_pack.install()
            from google.adk.runners import Runner

            runner = Runner(app_name="app", events=[FakeEvent(invocation_id="inv-1")])
            with mock.patch.object(sys, "stderr", new_callable=io.StringIO) as err:
                events1 = asyncio.run(
                    self._drain(runner.run_async(user_id="u1", session_id="s1", invocation_id="inv-1"))
                )
                events2 = asyncio.run(
                    self._drain(runner.run_async(user_id="u1", session_id="s1", invocation_id="inv-1"))
                )
            adk_pack.uninstall()
        self.assertEqual(len(events1), 1)
        self.assertEqual(len(events2), 1)
        self.assertEqual(backend.entered, [], "busy path never opens a flow")
        self.assertEqual(err.getvalue().count("already active"), 1, "noted once, not per-call")

    # -- undesignated path: byte-transparent, never touches the backend -----

    def test_undesignated_path_never_touches_backend(self) -> None:
        self.undesignate()
        backend = _FakeAdkFlowBackend()
        self.use_backend(backend)
        with FakeAdkModules():
            adk_pack.install()
            from google.adk.runners import Runner

            event = FakeEvent(invocation_id="inv-1")
            runner = Runner(app_name="app", events=[event])
            events = asyncio.run(
                self._drain(runner.run_async(user_id="u1", session_id="s1", invocation_id="inv-1"))
            )
            adk_pack.uninstall()
        self.assertEqual(events, [event])
        self.assertEqual(backend.entered, [])
        self.assertEqual(backend.exited, [])

    def test_no_backend_at_all_is_also_byte_transparent(self) -> None:
        # Designated, but Keel was never bootstrapped on this process
        # (get_backend() is None): also passes straight through.
        self.designate()
        with FakeAdkModules():
            adk_pack.install()
            from google.adk.runners import Runner

            event = FakeEvent(invocation_id="inv-1")
            runner = Runner(app_name="app", events=[event])
            events = asyncio.run(
                self._drain(runner.run_async(user_id="u1", session_id="s1", invocation_id="inv-1"))
            )
            adk_pack.uninstall()
        self.assertEqual(events, [event])

    # -- gates ----------------------------------------------------------------

    def test_designated_but_ungated_backend_raises_e005(self) -> None:
        self.designate()
        _runtime.set_runtime(_NoFlowSurfaceBackend(), None)
        with FakeAdkModules():
            adk_pack.install()
            from google.adk.runners import Runner

            runner = Runner(app_name="app", events=[FakeEvent(invocation_id="inv-1")])
            with self.assertRaises(KeelError) as ctx:
                asyncio.run(
                    self._drain(runner.run_async(user_id="u1", session_id="s1", invocation_id="inv-1"))
                )
            adk_pack.uninstall()
        self.assertEqual(ctx.exception.code, "KEEL-E005")

    # -- Runner.run (sync bridge) is never itself patched --------------------

    def test_runner_run_sync_bridges_through_the_same_wrap_not_double_wrapped(self) -> None:
        self.designate()
        backend = _FakeAdkFlowBackend()
        self.use_backend(backend)
        with FakeAdkModules():
            from google.adk.runners import Runner

            pristine_run = Runner.run
            adk_pack.install()
            self.assertIs(Runner.run, pristine_run, "install() never patches Runner.run itself")
            runner = Runner(app_name="app", events=[FakeEvent(invocation_id="inv-1")])
            events = runner.run(user_id="u1", session_id="s1", invocation_id="inv-1")
            adk_pack.uninstall()
        self.assertEqual(len(events), 1)
        self.assertEqual(len(backend.entered), 1, "exactly one flow entered via the sync bridge")
        self.assertEqual(backend.exited, ["completed"])

    # -- uninstall restores ----------------------------------------------------

    def test_uninstall_restores_run_async(self) -> None:
        with FakeAdkModules():
            from google.adk.runners import Runner

            pristine = Runner.run_async
            adk_pack.install()
            self.assertIsNot(Runner.run_async, pristine)
            adk_pack.uninstall()
            self.assertIs(Runner.run_async, pristine)


class ModelFallbackTest(AdkTestBase):
    """`_KeelPlugin.on_model_error_callback` (Task 4 / decision 7): true
    cross-model fallback via ADK's own plugin hook — the one Python call
    site that CAN construct a request for a genuinely different provider,
    unlike the transport seam (which can only rewrite a model name on the
    SAME host). Every branch runs against `FakeLLMRegistry`
    (`fixtures/fake_adk.py`): a per-test dict of model name -> fake model
    instance/class, reset in `AdkTestBase.setUp`."""

    def plugin(self) -> Any:
        with FakeAdkModules():
            return adk_pack._plugin()

    def fire(self, plugin: Any, llm_request: Any, error: Exception) -> Any:
        """Drive one `on_model_error_callback` invocation inside
        `FakeAdkModules()` — the callback's function-local `from
        google.adk.models.registry import LLMRegistry` (adapter-pack rule 1:
        no top-level import of a library not present/in use) resolves
        against the fake only while that context manager is active."""
        with FakeAdkModules():
            return asyncio.run(
                plugin.on_model_error_callback(
                    callback_context=object(), llm_request=llm_request, error=error
                )
            )

    # -- chain / no-chase gates ------------------------------------------------

    def test_empty_chain_returns_none_immediately(self) -> None:
        self.install_runtime({"target": {"llm:google-genai": {}}})  # no `fallback` configured
        result = self.fire(self.plugin(), FakeLlmRequest(model="gemini-2.0-flash"), RuntimeError("boom"))
        self.assertIsNone(result)

    def test_e012_breaker_open_never_chases(self) -> None:
        self.install_runtime({"target": {"llm:google-genai": {"fallback": ["claude-fallback"]}}})
        fallback_model = FakeClaude(responses=[FakeLlmResponse("should never be seen")])
        FakeLLMRegistry.configure("claude-fallback", fallback_model)
        error = RuntimeError("breaker open")
        error.keel_outcome = {"error": {"code": "KEEL-E012"}}
        result = self.fire(self.plugin(), FakeLlmRequest(model="gemini-2.0-flash"), error)
        self.assertIsNone(result)
        self.assertEqual(fallback_model.calls, [], "E012 never chases -- the fallback model is never even driven")

    def test_error_without_keel_outcome_is_chaseable(self) -> None:
        # A failure with NO `keel_outcome` attribute at all (e.g. raised
        # before Keel's transport seam ever saw it) has no code to
        # disqualify it -- fed to should_fallback as {"code": None}, which
        # is chaseable (an empty dict would wrongly read as "no error").
        self.install_runtime({"target": {"llm:google-genai": {"fallback": ["claude-fallback"]}}})
        response = FakeLlmResponse("cross-provider answer")
        FakeLLMRegistry.configure("claude-fallback", FakeClaude(responses=[response]))
        result = self.fire(
            self.plugin(), FakeLlmRequest(model="gemini-2.0-flash"), RuntimeError("pre-transport failure")
        )
        self.assertIs(result, response)

    # -- same-class skip / cross-class chase (decision 7) -----------------------

    def test_same_class_entry_skipped_cross_class_tried(self) -> None:
        self.install_runtime(
            {"target": {"llm:google-genai": {"fallback": ["gemini-same", "claude-cross"]}}}
        )
        # The FAILING model's class is resolved from llm_request.model.
        FakeLLMRegistry.configure("gemini-2.0-flash", FakeGemini())
        same_provider = FakeGemini(responses=[FakeLlmResponse("should never be used")])
        FakeLLMRegistry.configure("gemini-same", same_provider)
        cross_response = FakeLlmResponse("claude answered")
        cross_provider = FakeClaude(responses=[cross_response])
        FakeLLMRegistry.configure("claude-cross", cross_provider)
        result = self.fire(self.plugin(), FakeLlmRequest(model="gemini-2.0-flash"), RuntimeError("boom"))
        self.assertIs(result, cross_response)
        self.assertEqual(same_provider.calls, [], "same provider class: left for the transport seam to have chased")
        self.assertEqual(len(cross_provider.calls), 1)

    # -- first success wins; the final response of a stream is kept -------------

    def test_first_success_returned_second_never_tried(self) -> None:
        self.install_runtime({"target": {"llm:google-genai": {"fallback": ["claude-a", "claude-b"]}}})
        first_response = FakeLlmResponse("first")
        first = FakeClaude(responses=[first_response])
        second = FakeClaude(responses=[FakeLlmResponse("second")])
        FakeLLMRegistry.configure("claude-a", first)
        FakeLLMRegistry.configure("claude-b", second)
        result = self.fire(self.plugin(), FakeLlmRequest(model="gemini-2.0-flash"), RuntimeError("boom"))
        self.assertIs(result, first_response)
        self.assertEqual(len(first.calls), 1)
        self.assertEqual(second.calls, [], "the second entry is never tried once the first succeeds")

    def test_final_response_of_a_multi_yield_stream_is_kept(self) -> None:
        self.install_runtime({"target": {"llm:google-genai": {"fallback": ["claude-a"]}}})
        partial, final = FakeLlmResponse("partial"), FakeLlmResponse("final")
        FakeLLMRegistry.configure("claude-a", FakeClaude(responses=[partial, final]))
        result = self.fire(self.plugin(), FakeLlmRequest(model="gemini-2.0-flash"), RuntimeError("boom"))
        self.assertIs(result, final)

    # -- hop exception -> next entry ---------------------------------------------

    def test_hop_exception_tries_the_next_entry(self) -> None:
        self.install_runtime(
            {"target": {"llm:google-genai": {"fallback": ["claude-broken", "claude-ok"]}}}
        )
        broken = FakeClaude(error=RuntimeError("provider 500"))
        ok_response = FakeLlmResponse("ok")
        FakeLLMRegistry.configure("claude-broken", broken)
        FakeLLMRegistry.configure("claude-ok", FakeClaude(responses=[ok_response]))
        result = self.fire(self.plugin(), FakeLlmRequest(model="gemini-2.0-flash"), RuntimeError("boom"))
        self.assertIs(result, ok_response)
        self.assertEqual(len(broken.calls), 1)

    # -- registry-resolution failure: unknown name / missing package -------------

    def test_unknown_model_name_skipped_and_noted_once(self) -> None:
        # "nonexistent-model" is never registered -> LLMRegistry.resolve raises.
        self.install_runtime(
            {"target": {"llm:google-genai": {"fallback": ["nonexistent-model", "claude-ok"]}}}
        )
        ok_response = FakeLlmResponse("ok")
        FakeLLMRegistry.configure("claude-ok", FakeClaude(responses=[ok_response]))
        plugin = self.plugin()
        with mock.patch.object(sys, "stderr", new_callable=io.StringIO) as err:
            result = self.fire(plugin, FakeLlmRequest(model="gemini-2.0-flash"), RuntimeError("boom"))
        self.assertIs(result, ok_response)
        self.assertIn("nonexistent-model", err.getvalue())
        self.assertEqual(err.getvalue().count("could not be resolved"), 1)
        # A second failure of the SAME entry name is noted only once, ever.
        with mock.patch.object(sys, "stderr", new_callable=io.StringIO) as err2:
            self.fire(plugin, FakeLlmRequest(model="gemini-2.0-flash"), RuntimeError("boom"))
        self.assertEqual(err2.getvalue(), "")

    def test_new_llm_missing_package_skipped_and_noted(self) -> None:
        # resolve() succeeds (a real class IS registered for the name), but
        # new_llm() fails -- the "resolvable name, unbuildable model" shape
        # (e.g. the provider's extras were never installed).
        self.install_runtime(
            {"target": {"llm:google-genai": {"fallback": ["claude-uninstalled", "claude-ok"]}}}
        )
        FakeLLMRegistry.break_new_llm(
            "claude-uninstalled", FakeClaude, ImportError("anthropic package not installed")
        )
        ok_response = FakeLlmResponse("ok")
        FakeLLMRegistry.configure("claude-ok", FakeClaude(responses=[ok_response]))
        with mock.patch.object(sys, "stderr", new_callable=io.StringIO) as err:
            result = self.fire(self.plugin(), FakeLlmRequest(model="gemini-2.0-flash"), RuntimeError("boom"))
        self.assertIs(result, ok_response)
        self.assertIn("claude-uninstalled", err.getvalue())

    def test_quiet_env_suppresses_the_skip_note(self) -> None:
        self.install_runtime({"target": {"llm:google-genai": {"fallback": ["nonexistent-model"]}}})
        plugin = self.plugin()
        with mock.patch.dict(os.environ, {"KEEL_QUIET": "1"}), mock.patch.object(
            sys, "stderr", new_callable=io.StringIO
        ) as err:
            result = self.fire(plugin, FakeLlmRequest(model="gemini-2.0-flash"), RuntimeError("boom"))
        self.assertIsNone(result)
        self.assertEqual(err.getvalue(), "")

    # -- all entries exhausted: the original error propagates (returns None) ----

    def test_all_entries_exhausted_returns_none(self) -> None:
        self.install_runtime(
            {"target": {"llm:google-genai": {"fallback": ["claude-broken", "nonexistent-model"]}}}
        )
        FakeLLMRegistry.configure("claude-broken", FakeClaude(error=RuntimeError("still down")))
        result = self.fire(self.plugin(), FakeLlmRequest(model="gemini-2.0-flash"), RuntimeError("boom"))
        self.assertIsNone(result)

    # -- generate_content_async raising mid-hop: previously silent (issue #19) --

    def test_generate_content_async_failure_noted_once_with_hop_correlation(self) -> None:
        # "claude-broken" resolves and constructs fine (unlike
        # test_unknown_model_name_skipped_and_noted_once /
        # test_new_llm_missing_package_skipped_and_noted, which fail earlier)
        # -- it fails at the actual generate_content_async call, which used
        # to be silently swallowed by a bare `except Exception: continue`.
        self.install_runtime(
            {"target": {"llm:google-genai": {"fallback": ["claude-broken", "claude-ok"]}}}
        )
        broken = FakeClaude(error=RuntimeError("provider 500"))
        ok_response = FakeLlmResponse("ok")
        FakeLLMRegistry.configure("claude-broken", broken)
        FakeLLMRegistry.configure("claude-ok", FakeClaude(responses=[ok_response]))
        plugin = self.plugin()
        with mock.patch.object(sys, "stderr", new_callable=io.StringIO) as err:
            result = self.fire(
                plugin, FakeLlmRequest(model="gemini-2.0-flash"), RuntimeError("boom")
            )
        # The chain still proceeds to the next entry exactly as before.
        self.assertIs(result, ok_response)
        self.assertEqual(len(broken.calls), 1)
        note = err.getvalue()
        self.assertEqual(note.count("generate_content_async"), 1, note)
        self.assertIn("claude-broken", note)
        # Hop correlation: position in the chain (1 of 2) and the ORIGINAL
        # failing model name, so this note alone ties a fallback provider's
        # own llm:<provider> transport-seam traffic back to "this was ADK
        # fallback hop N of M, replacing originally-failing model X."
        self.assertIn("1/2", note)
        self.assertIn("gemini-2.0-flash", note)
        # A second failure of the SAME entry name is noted only once, ever.
        with mock.patch.object(sys, "stderr", new_callable=io.StringIO) as err2:
            self.fire(plugin, FakeLlmRequest(model="gemini-2.0-flash"), RuntimeError("boom"))
        self.assertEqual(err2.getvalue(), "")

    def test_quiet_env_suppresses_the_hop_failure_note(self) -> None:
        self.install_runtime({"target": {"llm:google-genai": {"fallback": ["claude-broken"]}}})
        FakeLLMRegistry.configure("claude-broken", FakeClaude(error=RuntimeError("provider 500")))
        plugin = self.plugin()
        with mock.patch.dict(os.environ, {"KEEL_QUIET": "1"}), mock.patch.object(
            sys, "stderr", new_callable=io.StringIO
        ) as err:
            result = self.fire(plugin, FakeLlmRequest(model="gemini-2.0-flash"), RuntimeError("boom"))
        self.assertIsNone(result)  # only entry, and it failed: chain exhausted
        self.assertEqual(err.getvalue(), "")

    # -- soft-error LlmResponse from a fallback hop: accepted as success --------

    def test_soft_error_response_from_fallback_hop_is_accepted(self) -> None:
        # A fallback hop's generate_content_async can complete WITHOUT
        # raising but still yield a response object that carries its own
        # error_code-like attribute (ADK's own soft-error response shape).
        # _model_fallback does not special-case this -- any non-None
        # response from a hop is accepted as success, matching ADK's own
        # primary-path semantics (issue #19's "untested in either
        # direction" nuance -- this locks in the accepted behavior).
        self.install_runtime({"target": {"llm:google-genai": {"fallback": ["claude-soft-error"]}}})
        soft_error_response = FakeLlmResponse("degraded", error_code="SAFETY", error_message="blocked")
        FakeLLMRegistry.configure("claude-soft-error", FakeClaude(responses=[soft_error_response]))
        result = self.fire(self.plugin(), FakeLlmRequest(model="gemini-2.0-flash"), RuntimeError("boom"))
        self.assertIs(result, soft_error_response)
        self.assertEqual(getattr(result, "error_code", None), "SAFETY")


class ModelFallbackPluginShapeTest(AdkTestBase):
    """PluginManager-shape sanity: ADK's plugin callbacks are called with
    keyword arguments only (`BasePlugin`'s documented contract), and a
    substituted response is handed back to the agent as the IDENTICAL object
    a fallback model produced -- `PluginManager` yields it verbatim; Keel
    never wraps or copies it."""

    def test_callback_signature_is_keyword_only(self) -> None:
        with FakeAdkModules():
            plugin = adk_pack._plugin()
        sig = inspect.signature(type(plugin).on_model_error_callback)
        params = [p for name, p in sig.parameters.items() if name != "self"]
        self.assertEqual({p.name for p in params}, {"callback_context", "llm_request", "error"})
        self.assertTrue(
            all(p.kind == inspect.Parameter.KEYWORD_ONLY for p in params),
            "ADK's PluginManager calls every plugin callback with keyword arguments only",
        )

    def test_returns_the_response_object_itself(self) -> None:
        self.install_runtime({"target": {"llm:google-genai": {"fallback": ["claude-a"]}}})
        response = FakeLlmResponse("verbatim")
        FakeLLMRegistry.configure("claude-a", FakeClaude(responses=[response]))
        with FakeAdkModules():
            plugin = adk_pack._plugin()
            result = asyncio.run(
                plugin.on_model_error_callback(
                    callback_context=object(),
                    llm_request=FakeLlmRequest(model="gemini-2.0-flash"),
                    error=RuntimeError("boom"),
                )
            )
        self.assertIs(result, response, "PluginManager substitutes this object verbatim, not a copy")


# --- KeelSessionService (design doc issue #15) -------------------------------
#
# Runs entirely OFFLINE against the structural fakes `fake_adk.py` adds for
# this feature (`FakeSession`/`FakeBaseSessionService`/`FakeEventActions`/
# `FakeBlob`/`FakePart`/`FakeContent`/... — registered into `FakeAdkModules`
# under `google.adk.sessions`/`google.adk.events`/`google.adk.errors`/
# `google.genai.types`). Per the WS3 convention (CLAUDE.md: "New agent-pack
# seams extend THREE test layers together"), this is layer 2 (offline pack
# tests); layer 3 (a `test_farm_adk_session_service.py` module certifying
# against the real pinned `google-adk==2.4.0`) is a later phase's job, not
# this one's.


def _fake_session(app_name: str, user_id: str, session_id: str, **kwargs: Any) -> FakeSession:
    return FakeSession(app_name=app_name, user_id=user_id, id=session_id, **kwargs)


def _fake_event(
    *,
    event_id: str = "e1",
    author: str = "model",
    invocation_id: str = "inv-1",
    timestamp: float = 1.0,
    content: Any = None,
    state_delta: dict[str, Any] | None = None,
    partial: bool = False,
) -> FakeEvent:
    """A fully-shaped `Event` for driving `KeelSessionService.append_event`
    directly — every field the write path (design §3.1) actually reads."""
    return FakeEvent(
        id=event_id,
        author=author,
        invocation_id=invocation_id,
        timestamp=timestamp,
        content=content,
        actions=FakeEventActions(state_delta=state_delta or {}),
        partial=partial,
    )


class _FakeSessionJournalBackend:
    """`_FakeAdkFlowBackend`'s flow lifecycle (`enter_flow`/`exit_flow`/
    `journal_random`) PLUS a real, flow-scoped step journal backing
    `execute()`/`flows_by_entrypoint()`/`steps_for_flow()` (design doc issue
    #15 §3.2) — enough to exercise `KeelSessionService`'s read path
    end-to-end offline: no compiled core, no real `google.adk`, but a
    faithful enough journal shape (flow_id -> ordered steps, each carrying
    `seq`/`step_key`/`payload`) for the pack's OWN `_scan_flows`/`_replay`
    to run against completely unmodified.

    Unlike `_FakeAdkFlowBackend` (which hardcodes a single `"fid-1"` — that
    fixture never needed more than one flow open at a time),
    `KeelSessionService`'s whole point is reading ACROSS many past flow_ids
    (design §0), so `enter_flow` here mints a fresh id per NEW
    `(entrypoint, args_hash)` identity (and reuses the same id for a
    same-identity replay, mirroring the real core) and this class tracks
    calls to the two read methods for the read-path's cache-hit/cache-miss
    call-count assertions."""

    def __init__(self) -> None:
        self.entered: list[tuple[Any, ...]] = []
        self.exited: list[str] = []
        self.random: dict[str, bytes] = {}
        self.persistent = True
        self.last_flow_id: str | None = None
        self._current_flow_id: str | None = None
        self._flow_seq = 0
        # flow_id -> {"entrypoint", "args_hash", "created_at", "steps": [...]}
        self._flows: dict[str, dict[str, Any]] = {}
        self._flow_id_for_identity: dict[tuple[str, str], str] = {}
        self.flows_by_entrypoint_calls = 0
        self.steps_for_flow_calls: list[str] = []

    def enter_flow(
        self,
        entrypoint: str,
        args_hash: str,
        code_hash: str | None = None,
        explicit_key: str | None = None,
        lease_ms: int | None = None,
    ) -> dict[str, Any]:
        self.entered.append((entrypoint, args_hash, code_hash, explicit_key, lease_ms))
        identity = (entrypoint, args_hash)
        replay = identity in self._flow_id_for_identity
        if replay:
            flow_id = self._flow_id_for_identity[identity]
        else:
            self._flow_seq += 1
            flow_id = f"fid-{self._flow_seq}"
            self._flows[flow_id] = {
                "entrypoint": entrypoint,
                "args_hash": args_hash,
                "created_at": self._flow_seq,
                "steps": [],
            }
            self._flow_id_for_identity[identity] = flow_id
        self._current_flow_id = flow_id
        self.last_flow_id = flow_id
        return {"flow_id": flow_id, "status": "completed" if replay else "running", "replay": replay}

    def exit_flow(self, status: str) -> None:
        self.exited.append(status)

    def journal_random(self, key: str, data: bytes) -> bytes:
        return self.random.setdefault(key, data)

    def execute(self, request: dict[str, Any], effect: Any) -> dict[str, Any]:
        result = effect(0)
        payload = result.get("payload")
        step_key = f"{request['target']}#{request['args_hash']}"
        flow = self._flows[self._current_flow_id]
        seq = len(flow["steps"]) + 1
        flow["steps"].append({"seq": seq, "step_key": step_key, "payload": payload})
        return {"result": result.get("status", "ok"), "payload": payload}

    def flows_by_entrypoint(self, entrypoint: str) -> list[dict[str, Any]]:
        self.flows_by_entrypoint_calls += 1
        return [
            {"flow_id": fid, "entrypoint": f["entrypoint"], "created_at": f["created_at"]}
            for fid, f in sorted(self._flows.items(), key=lambda kv: kv[1]["created_at"])
            if f["entrypoint"] == entrypoint
        ]

    def steps_for_flow(self, flow_id: str) -> list[dict[str, Any]]:
        self.steps_for_flow_calls.append(flow_id)
        return [dict(s) for s in self._flows[flow_id]["steps"]]


class _FlowRunner:
    """A `self`-substitute for driving `adk_pack._run_async_wrapper` (the
    Runner-flow wrap itself) directly, bypassing `FakeRunner`'s plugin/tool
    machinery entirely (irrelevant to `KeelSessionService` coverage): the
    wrapper's identity-write call site reads only `self.app_name` (design
    §3.4's confirmed source — `Runner.__init__` always sets it)."""

    def __init__(self, app_name: str) -> None:
        self.app_name = app_name


def _drive_flow(
    app_name: str, user_id: str, session_id: str, invocation_id: str, body: Callable[[], Any]
) -> str:
    """Drives ONE simulated Runner-flow turn through the REAL
    `adk_pack._run_async_wrapper` (not a reimplementation of its reset
    logic) — opens the flow, writes `tool:adk.session_identity` at the
    wrapper's own (corrected, design §3.1) call site, awaits `body()`
    (typically one or more `KeelSessionService.append_event` calls), yields
    one event to correlate/complete, then exits the flow. Requires the
    caller to have already designated `RUNNER_FLOW_ENTRYPOINT` and installed
    a backend via `_runtime.set_runtime(...)`. Returns the backend's
    `last_flow_id` after the turn completes, so callers can inspect that
    flow's own steps via `backend.steps_for_flow(flow_id)`."""

    async def orig(
        self: Any,
        *,
        user_id: str,
        session_id: str,
        invocation_id: str | None = None,
        new_message: Any = None,
        **kwargs: Any,
    ) -> Any:
        await body()
        yield FakeEvent(invocation_id=invocation_id)

    wrapped = adk_pack._run_async_wrapper(orig)
    runner = _FlowRunner(app_name=app_name)

    async def run() -> None:
        async for _event in wrapped(
            runner, user_id=user_id, session_id=session_id, invocation_id=invocation_id
        ):
            pass

    asyncio.run(run())
    backend = _runtime.get_backend()
    return backend.last_flow_id


class KeelSessionServiceWithoutAdkTest(AdkTestBase):
    """Mirrors `CheckpointerWithoutLangGraphTest`: the factory needs
    `google.adk` installed (KEEL-E005 otherwise)."""

    def test_factory_needs_google_adk_installed(self) -> None:
        # google.adk is genuinely not installed in this test environment,
        # and AdkTestBase.setUp resets `_session_service_cls` to None so a
        # PRIOR test's fake build can't paper over this one.
        with self.assertRaises(KeelError) as ctx:
            adk_pack.KeelSessionService(app_name="app")
        self.assertEqual(ctx.exception.code, "KEEL-E005")


class KeelSessionServiceHonestyGateTest(AdkTestBase):
    """The write-path honesty gate (design §6 item 4), mirroring
    `CheckpointerHonestyGateTest`'s structure but with THREE cases instead
    of two — `_write_gate` distinguishes "never designated" (silent
    in-memory degrade) from "designated but no/wrong flow open" (KEEL-E005)
    using `_flow_entrypoint_designated()`, which a bare
    `_runtime.in_active_flow()` check cannot tell apart."""

    def _designate(self) -> None:
        entry = FlowEntrypoint(
            raw=adk_pack.RUNNER_FLOW_ENTRYPOINT, module="google.adk.runners", function="Runner.run_async"
        )
        _runtime.set_flow_entrypoints([entry])

    def test_undesignated_degrades_silently_no_write_no_error(self) -> None:
        # [flows] entrypoints never named the Runner entrypoint at all — the
        # common case for every app that hasn't opted into Tier 2 yet.
        _runtime.set_flow_entrypoints(())
        backend = _FakeSessionJournalBackend()
        _runtime.set_runtime(backend, None)
        with FakeAdkModules():
            svc = adk_pack.KeelSessionService(app_name="app")
            session = _fake_session("app", "u1", "s1")
            event = _fake_event(state_delta={"x": 1})
            result = asyncio.run(svc.append_event(session, event))
        self.assertEqual(backend.entered, [], "no flow ever entered")
        self.assertEqual(backend.flows_by_entrypoint_calls, 0)
        self.assertIs(result, event, "the base method's own return value still comes back")
        self.assertEqual(session.state.get("x"), 1, "the live in-memory mutation still happens")
        self.assertEqual(session.events, [event])

    def test_designated_but_no_flow_open_raises_e005(self) -> None:
        self._designate()
        backend = _FakeSessionJournalBackend()
        _runtime.set_runtime(backend, None)
        self.assertFalse(_runtime.in_active_flow())
        with FakeAdkModules():
            svc = adk_pack.KeelSessionService(app_name="app")
            session = _fake_session("app", "u1", "s1")
            event = _fake_event()
            with self.assertRaises(KeelError) as ctx:
                asyncio.run(svc.append_event(session, event))
        self.assertEqual(ctx.exception.code, "KEEL-E005")

    def test_designated_but_a_different_sessions_flow_is_active_raises_e005(self) -> None:
        # The process-wide singleton flow handle is held by a DIFFERENT
        # session's turn — misattributing this write to it would be a
        # correctness bug, not just a missed durability opportunity.
        self._designate()
        backend = _FakeSessionJournalBackend()
        _runtime.set_runtime(backend, None)
        adk_pack._active_session_identity = ("app", "someone-else", "their-session")
        _runtime.set_flow_active(True)
        with FakeAdkModules():
            svc = adk_pack.KeelSessionService(app_name="app")
            session = _fake_session("app", "u1", "s1")
            event = _fake_event()
            with self.assertRaises(KeelError) as ctx:
                asyncio.run(svc.append_event(session, event))
        self.assertEqual(ctx.exception.code, "KEEL-E005")

    def test_designated_and_this_session_is_the_active_flow_writes_normally(self) -> None:
        self._designate()
        backend = _FakeSessionJournalBackend()
        _runtime.set_runtime(backend, None)
        # Prime a real flow slot (`execute()` writes into whichever flow
        # `enter_flow` last opened) — `set_flow_active`/`_active_session_identity`
        # alone (the honesty gate's own inputs) don't imply a flow exists for
        # `execute()` to index into; a real `_run_async_flow_wrapper` call
        # always does both together (`enter_flow` before `set_flow_active`).
        backend.enter_flow(adk_pack.RUNNER_FLOW_ENTRYPOINT, "ah-1")
        adk_pack._active_session_identity = ("app", "u1", "s1")
        _runtime.set_flow_active(True)
        with FakeAdkModules():
            svc = adk_pack.KeelSessionService(app_name="app")
            session = _fake_session("app", "u1", "s1")
            event = _fake_event(state_delta={"greeted": True})
            asyncio.run(svc.append_event(session, event))
        steps = backend.steps_for_flow(backend.last_flow_id)
        self.assertEqual(len(steps), 1)
        self.assertEqual(steps[0]["step_key"], f"{adk_pack.SESSION_EVENT_TARGET}#s1:1")
        self.assertEqual(session.state.get("greeted"), True)


class KeelSessionServiceHonestyGateDeleteTest(AdkTestBase):
    """The same three-case gate, exercised via `delete_session` instead of
    `append_event` — `_write_gate` is shared, but this pins that BOTH write
    call sites actually use it."""

    def _designate(self) -> None:
        entry = FlowEntrypoint(
            raw=adk_pack.RUNNER_FLOW_ENTRYPOINT, module="google.adk.runners", function="Runner.run_async"
        )
        _runtime.set_flow_entrypoints([entry])

    def test_undesignated_delete_degrades_silently(self) -> None:
        _runtime.set_flow_entrypoints(())
        backend = _FakeSessionJournalBackend()
        _runtime.set_runtime(backend, None)
        with FakeAdkModules():
            svc = adk_pack.KeelSessionService(app_name="app")
            asyncio.run(svc.delete_session(app_name="app", user_id="u1", session_id="s1"))
        self.assertEqual(backend.entered, [])

    def test_designated_no_matching_flow_delete_raises_e005(self) -> None:
        self._designate()
        backend = _FakeSessionJournalBackend()
        _runtime.set_runtime(backend, None)
        with FakeAdkModules():
            svc = adk_pack.KeelSessionService(app_name="app")
            with self.assertRaises(KeelError) as ctx:
                asyncio.run(svc.delete_session(app_name="app", user_id="u1", session_id="s1"))
        self.assertEqual(ctx.exception.code, "KEEL-E005")


class KeelSessionServiceWritePathTest(AdkTestBase):
    """Write-path shape (design §3.1): `append_event` journals
    `tool:adk.session_event` with the documented `<session_id>:<seq>`
    args_hash; `create_session` journals NOTHING (§6 item 3); `delete_session`
    journals `tool:adk.session_delete` keyed by the bare session id."""

    def setUp(self) -> None:
        super().setUp()
        entry = FlowEntrypoint(
            raw=adk_pack.RUNNER_FLOW_ENTRYPOINT, module="google.adk.runners", function="Runner.run_async"
        )
        _runtime.set_flow_entrypoints([entry])
        self.backend = _FakeSessionJournalBackend()
        _runtime.set_runtime(self.backend, None)
        adk_pack._active_session_identity = ("app", "u1", "s1")
        _runtime.set_flow_active(True)
        # Prime a real flow slot (append_event/delete_session write THROUGH
        # `execute()`, which indexes `self._current_flow_id` — a bare
        # `set_flow_active(True)` alone, without ever calling `enter_flow`,
        # leaves no flow for `execute()` to write into).
        self.backend.enter_flow(adk_pack.RUNNER_FLOW_ENTRYPOINT, "ah-1")

    def test_append_event_journals_the_documented_step_key(self) -> None:
        with FakeAdkModules():
            svc = adk_pack.KeelSessionService(app_name="app")
            session = _fake_session("app", "u1", "s1")
            asyncio.run(svc.append_event(session, _fake_event(event_id="e1")))
            asyncio.run(svc.append_event(session, _fake_event(event_id="e2")))
        steps = self.backend.steps_for_flow(self.backend.last_flow_id)
        self.assertEqual(
            [s["step_key"] for s in steps],
            [f"{adk_pack.SESSION_EVENT_TARGET}#s1:1", f"{adk_pack.SESSION_EVENT_TARGET}#s1:2"],
        )

    def test_partial_event_is_never_journaled_or_appended(self) -> None:
        with FakeAdkModules():
            svc = adk_pack.KeelSessionService(app_name="app")
            session = _fake_session("app", "u1", "s1")
            asyncio.run(svc.append_event(session, _fake_event(partial=True)))
        self.assertEqual(self.backend.steps_for_flow(self.backend.last_flow_id), [])
        self.assertEqual(session.events, [], "the REAL base method never appends a partial event either")

    def test_create_session_journals_nothing(self) -> None:
        with FakeAdkModules():
            svc = adk_pack.KeelSessionService(app_name="app")
            asyncio.run(svc.create_session(app_name="app", user_id="u1", session_id="s-new"))
        self.assertEqual(self.backend.steps_for_flow(self.backend.last_flow_id), [])

    def test_create_session_duplicate_id_raises_already_exists(self) -> None:
        with FakeAdkModules():
            svc = adk_pack.KeelSessionService(app_name="app")
            asyncio.run(svc.create_session(app_name="app", user_id="u1", session_id="dup"))
            with self.assertRaises(FakeAlreadyExistsError):
                asyncio.run(svc.create_session(app_name="app", user_id="u1", session_id="dup"))

    def test_delete_session_journals_the_documented_step_key(self) -> None:
        with FakeAdkModules():
            svc = adk_pack.KeelSessionService(app_name="app")
            asyncio.run(svc.delete_session(app_name="app", user_id="u1", session_id="s1"))
        steps = self.backend.steps_for_flow(self.backend.last_flow_id)
        self.assertEqual([s["step_key"] for s in steps], [f"{adk_pack.SESSION_DELETE_TARGET}#s1"])


class KeelSessionServiceStepOrderingTest(AdkTestBase):
    """The two properties the design review specifically added/fixed (§3.1):
    `session_identity` is written BEFORE the first `session_event` in
    journal order, and the per-flow `session_event` sequence counter RESETS
    to start at 1 for every new flow rather than accumulating across turns
    — both driven through the REAL `_run_async_wrapper` (`_drive_flow`), not
    a reimplementation of its reset logic, so a regression in the actual
    reset call site would fail these tests."""

    def setUp(self) -> None:
        super().setUp()
        entry = FlowEntrypoint(
            raw=adk_pack.RUNNER_FLOW_ENTRYPOINT, module="google.adk.runners", function="Runner.run_async"
        )
        _runtime.set_flow_entrypoints([entry])
        self.backend = _FakeSessionJournalBackend()
        _runtime.set_runtime(self.backend, None)

    def test_session_identity_is_journaled_before_the_first_session_event(self) -> None:
        with FakeAdkModules():
            svc = adk_pack.KeelSessionService(app_name="app")

            async def body() -> None:
                session = _fake_session("app", "u1", "s1")
                await svc.append_event(session, _fake_event(event_id="e1"))

            flow_id = _drive_flow("app", "u1", "s1", "inv-1", body)

        steps = self.backend.steps_for_flow(flow_id)
        self.assertEqual(len(steps), 2)
        self.assertEqual(steps[0]["seq"], 1)
        self.assertEqual(steps[0]["step_key"], f"{adk_pack.SESSION_IDENTITY_TARGET}#-")
        self.assertEqual(steps[0]["payload"], {"app_name": "app", "user_id": "u1", "session_id": "s1"})
        self.assertEqual(steps[1]["seq"], 2)
        self.assertEqual(steps[1]["step_key"], f"{adk_pack.SESSION_EVENT_TARGET}#s1:1")

    def test_session_event_seq_resets_to_1_for_every_new_flow(self) -> None:
        with FakeAdkModules():
            svc = adk_pack.KeelSessionService(app_name="app")

            async def body_flow1() -> None:
                session = _fake_session("app", "u1", "s1")
                await svc.append_event(session, _fake_event(event_id="e1"))
                await svc.append_event(session, _fake_event(event_id="e2"))

            flow1_id = _drive_flow("app", "u1", "s1", "inv-1", body_flow1)

            async def body_flow2() -> None:
                # A brand-new turn (different invocation_id => a genuinely
                # NEW flow, not a replay of flow1) for the SAME session.
                session = _fake_session("app", "u1", "s1")
                await svc.append_event(session, _fake_event(event_id="e3"))
                await svc.append_event(session, _fake_event(event_id="e4"))

            flow2_id = _drive_flow("app", "u1", "s1", "inv-2", body_flow2)

        self.assertNotEqual(flow1_id, flow2_id, "two distinct flows, not a replay of the same one")
        event_keys1 = [
            s["step_key"]
            for s in self.backend.steps_for_flow(flow1_id)
            if s["step_key"].startswith(adk_pack.SESSION_EVENT_TARGET)
        ]
        event_keys2 = [
            s["step_key"]
            for s in self.backend.steps_for_flow(flow2_id)
            if s["step_key"].startswith(adk_pack.SESSION_EVENT_TARGET)
        ]
        self.assertEqual(
            event_keys1, [f"{adk_pack.SESSION_EVENT_TARGET}#s1:1", f"{adk_pack.SESSION_EVENT_TARGET}#s1:2"]
        )
        self.assertEqual(
            event_keys2,
            [f"{adk_pack.SESSION_EVENT_TARGET}#s1:1", f"{adk_pack.SESSION_EVENT_TARGET}#s1:2"],
            "seq must RESET to 1 for the new flow, not continue accumulating as 3, 4 "
            "(the exact sequencing bug the design doc's own review caught, §3.1)",
        )


class KeelSessionServiceContentEncodingTest(unittest.TestCase):
    """Event content encoding (design §3.1's "event mapping" open question):
    a `Part` round-trips through `_encode_part`/`_decode_part` for all three
    documented cases. Exercises the private encode/decode helpers directly
    (they take `content_cls`/`part_cls`/`blob_cls` as plain arguments, not
    imports of their own) rather than the full write/read path — no
    `FakeAdkModules` needed."""

    def test_plain_text_part_passes_through_unencoded(self) -> None:
        part = FakePart(text="hello")
        self.assertEqual(adk_pack._encode_part(part), {"text": "hello"})

    def test_inline_data_part_survives_round_trip_as_base64_dict(self) -> None:
        raw = b"\x89PNG\r\n\x1a\n"
        part = FakePart(inline_data=FakeBlob(data=raw, mime_type="image/png"))
        encoded = adk_pack._encode_part(part)
        self.assertEqual(
            encoded, {"inline_data_b64": base64.b64encode(raw).decode("ascii"), "mime_type": "image/png"}
        )
        decoded = adk_pack._decode_part(encoded, FakePart, FakeBlob)
        self.assertEqual(decoded.inline_data.data, raw)
        self.assertEqual(decoded.inline_data.mime_type, "image/png")

    def test_function_call_part_survives_round_trip_via_model_dump_fallback(self) -> None:
        part = FakePart(function_call={"name": "search", "args": {"q": "keel"}})
        encoded = adk_pack._encode_part(part)
        self.assertEqual(encoded, {"function_call": {"name": "search", "args": {"q": "keel"}}})
        decoded = adk_pack._decode_part(encoded, FakePart, FakeBlob)
        self.assertEqual(decoded.function_call, {"name": "search", "args": {"q": "keel"}})

    def test_function_response_part_survives_round_trip_via_model_dump_fallback(self) -> None:
        part = FakePart(function_response={"name": "search", "response": {"result": [1, 2]}})
        encoded = adk_pack._encode_part(part)
        self.assertEqual(encoded, {"function_response": {"name": "search", "response": {"result": [1, 2]}}})
        decoded = adk_pack._decode_part(encoded, FakePart, FakeBlob)
        self.assertEqual(decoded.function_response, {"name": "search", "response": {"result": [1, 2]}})

    def test_content_round_trip_preserves_role_and_part_order(self) -> None:
        content = FakeContent(
            role="user",
            parts=[FakePart(text="hi"), FakePart(inline_data=FakeBlob(data=b"x", mime_type="a/b"))],
        )
        encoded = adk_pack._encode_content(content)
        decoded = adk_pack._decode_content(encoded, FakeContent, FakePart, FakeBlob)
        self.assertEqual(decoded.role, "user")
        self.assertEqual(decoded.parts[0].text, "hi")
        self.assertEqual(decoded.parts[1].inline_data.data, b"x")
        self.assertEqual(decoded.parts[1].inline_data.mime_type, "a/b")

    def test_none_content_round_trips_to_none(self) -> None:
        self.assertIsNone(adk_pack._encode_content(None))
        self.assertIsNone(adk_pack._decode_content(None, FakeContent, FakePart, FakeBlob))


class KeelSessionServiceReadPathTest(AdkTestBase):
    """The read path (design §3.2): an in-process cache hit makes ZERO
    calls to `flows_by_entrypoint`/`steps_for_flow`; a cache miss falls back
    to a real scan-and-replay that reconstructs `Session.events`/`state` in
    order ACROSS multiple past flows; a `create_session` that never had a
    turn run against it is invisible to a fresh (cache-miss) reader (§6
    item 3)."""

    def setUp(self) -> None:
        super().setUp()
        entry = FlowEntrypoint(
            raw=adk_pack.RUNNER_FLOW_ENTRYPOINT, module="google.adk.runners", function="Runner.run_async"
        )
        _runtime.set_flow_entrypoints([entry])
        self.backend = _FakeSessionJournalBackend()
        _runtime.set_runtime(self.backend, None)

    def test_cache_hit_makes_zero_calls_to_the_journal_read_methods(self) -> None:
        with FakeAdkModules():
            svc = adk_pack.KeelSessionService(app_name="app")

            async def body() -> None:
                session = _fake_session("app", "u1", "s1")
                await svc.append_event(session, _fake_event(event_id="e1"))

            _drive_flow("app", "u1", "s1", "inv-1", body)
            self.backend.flows_by_entrypoint_calls = 0  # ignore the write path's own bookkeeping
            self.backend.steps_for_flow_calls = []

            result = asyncio.run(svc.get_session(app_name="app", user_id="u1", session_id="s1"))
        self.assertIsNotNone(result, "the process's own write already populated the cache")
        self.assertEqual(self.backend.flows_by_entrypoint_calls, 0)
        self.assertEqual(self.backend.steps_for_flow_calls, [])

    def test_list_sessions_always_scans_even_when_the_cache_already_has_every_session(self) -> None:
        # UNLIKE get_session's exact-key point lookup, `list_sessions` has no
        # way to know its own cache is COMPLETE for (app_name, user_id) —
        # some other process/turn could have created a session this one
        # never touched — so design §3.2 step 4 states its "whole job" is an
        # unconditional `flows_by_entrypoint` scan-and-group, every call,
        # regardless of what the cache already holds. This pins that
        # (correct, deliberate) behavior rather than asserting a zero-call
        # fast path `list_sessions` never promised.
        with FakeAdkModules():
            svc = adk_pack.KeelSessionService(app_name="app")

            async def body() -> None:
                session = _fake_session("app", "u1", "s1")
                await svc.append_event(session, _fake_event(event_id="e1"))

            _drive_flow("app", "u1", "s1", "inv-1", body)
            self.backend.flows_by_entrypoint_calls = 0
            self.backend.steps_for_flow_calls = []

            response = asyncio.run(svc.list_sessions(app_name="app", user_id="u1"))
        self.assertEqual([s.id for s in response.sessions], ["s1"])
        self.assertEqual(self.backend.flows_by_entrypoint_calls, 1)

    def test_get_session_config_trims_a_cache_hit_without_mutating_the_cached_original(self) -> None:
        # design §4: no physical compaction, ever — GetSessionConfig trims
        # the RECONSTRUCTED event list at read time only.
        with FakeAdkModules():
            svc = adk_pack.KeelSessionService(app_name="app")

            async def body() -> None:
                session = _fake_session("app", "u1", "s1")
                await svc.append_event(session, _fake_event(event_id="e1"))
                await svc.append_event(session, _fake_event(event_id="e2"))

            _drive_flow("app", "u1", "s1", "inv-1", body)

            trimmed = asyncio.run(
                svc.get_session(
                    app_name="app",
                    user_id="u1",
                    session_id="s1",
                    config=FakeGetSessionConfig(num_recent_events=1),
                )
            )
            untrimmed = asyncio.run(svc.get_session(app_name="app", user_id="u1", session_id="s1"))
        self.assertEqual([e.id for e in trimmed.events], ["e2"])
        self.assertEqual(
            [e.id for e in untrimmed.events], ["e1", "e2"], "trimming never mutates the cached original"
        )

    def test_cache_miss_fallback_reconstructs_events_and_state_across_multiple_flows(self) -> None:
        with FakeAdkModules():
            svc1 = adk_pack.KeelSessionService(app_name="app")

            async def turn1() -> None:
                session = _fake_session("app", "u1", "s1")
                await svc1.append_event(session, _fake_event(event_id="e1", state_delta={"n": 1}))

            _drive_flow("app", "u1", "s1", "inv-1", turn1)

            async def turn2() -> None:
                session = _fake_session("app", "u1", "s1")
                await svc1.append_event(session, _fake_event(event_id="e2", state_delta={"n": 2, "m": "x"}))

            _drive_flow("app", "u1", "s1", "inv-2", turn2)

            # A FRESH instance — simulates a different process / a cold
            # cache: it has never seen this session, so `get_session` must
            # take the real scan-and-replay fallback (§3.2 step 2).
            svc2 = adk_pack.KeelSessionService(app_name="app")
            session = asyncio.run(svc2.get_session(app_name="app", user_id="u1", session_id="s1"))
        self.assertIsNotNone(session)
        self.assertEqual([e.id for e in session.events], ["e1", "e2"], "flow created_at order")
        self.assertEqual(session.state, {"n": 2, "m": "x"}, "later state_delta overwrites earlier same key")
        self.assertGreaterEqual(self.backend.flows_by_entrypoint_calls, 1)
        self.assertGreaterEqual(len(self.backend.steps_for_flow_calls), 2, "both flows were read")

    def test_create_session_then_never_run_a_turn_is_invisible_to_a_fresh_reader(self) -> None:
        # design §6 item 3: create_session never journals, so a session that
        # never had a turn run against it does not exist from a DIFFERENT
        # process's (or a fresh cache's) point of view.
        with FakeAdkModules():
            svc1 = adk_pack.KeelSessionService(app_name="app")
            asyncio.run(svc1.create_session(app_name="app", user_id="u1", session_id="s-never-run"))
            self.assertEqual(self.backend.entered, [], "create_session never opens/uses a flow")

            svc2 = adk_pack.KeelSessionService(app_name="app")
            result = asyncio.run(svc2.get_session(app_name="app", user_id="u1", session_id="s-never-run"))
        self.assertIsNone(result)


if __name__ == "__main__":
    unittest.main()
