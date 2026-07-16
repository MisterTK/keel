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
import hashlib
import io
import os
import sqlite3
import sys
import unittest
from importlib import metadata
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any
from unittest import mock

from fake_adk import (
    FakeAdkModules,
    FakeApp,
    FakeBasePlugin,
    FakeEvent,
    FakeInMemoryRunner,
    FakeRunner,
    FakeTool,
    FakeSlottedTool,
    McpTool,
)

from keel import _runtime, bootstrap
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
        adk_pack._rebound.clear()
        adk_pack._noted_busy = False
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
    count it as a failure (breaker/discovery) while returning it unchanged."""

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
        self._prior_state = bootstrap._STATE.state
        self._prior_installed = bootstrap._STATE.installed

    def tearDown(self) -> None:
        # `_flow_entrypoint_designated` reads `bootstrap._STATE` directly
        # (module-private, deliberately — see its docstring), so every test
        # that pokes it must restore the real suite-wide state afterward.
        bootstrap._STATE.state = self._prior_state
        bootstrap._STATE.installed = self._prior_installed

    # -- _flow_entrypoint_designated ------------------------------------

    def test_designated_when_exact_match_present(self) -> None:
        entry = FlowEntrypoint(
            raw="py:google.adk.runners:Runner.run_async",
            module="google.adk.runners",
            function="Runner.run_async",
        )
        bootstrap._STATE.state = {"flow_entrypoints": [entry]}
        bootstrap._STATE.installed = True
        self.assertEqual(
            adk_pack._flow_entrypoint_designated(),
            "py:google.adk.runners:Runner.run_async",
        )

    def test_undesignated_when_never_installed_or_disabled(self) -> None:
        # `install_keel()` never populates `_STATE.state` when KEEL_DISABLE is
        # set (it returns before that point) — bootstrap-disabled and
        # never-installed are the SAME shape here: `_STATE.state is None`.
        bootstrap._STATE.state = None
        bootstrap._STATE.installed = False
        self.assertIsNone(adk_pack._flow_entrypoint_designated())

    def test_undesignated_when_no_matching_entrypoint(self) -> None:
        other = FlowEntrypoint(raw="py:pipeline:main", module="pipeline", function="main")
        bootstrap._STATE.state = {"flow_entrypoints": [other]}
        bootstrap._STATE.installed = True
        self.assertIsNone(adk_pack._flow_entrypoint_designated())

    def test_undesignated_when_no_flow_entrypoints_at_all(self) -> None:
        bootstrap._STATE.state = {"flow_entrypoints": []}
        bootstrap._STATE.installed = True
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
        bootstrap._STATE.state = {"flow_entrypoints": [glob_entry]}
        bootstrap._STATE.installed = True
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

    def __init__(self, replay: bool = False, persistent: bool = True) -> None:
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

    def journal_random(self, key: str, data: bytes) -> bytes:
        return self.random.setdefault(key, data)


class RunnerFlowWrapTest(AdkTestBase):
    """WS5 core: a DESIGNATED `Runner.run_async` invocation becomes a Tier 2
    journaled flow (`adk_pack._run_async_wrapper`'s async-generator patch);
    every other call stays byte-transparent. Runs entirely against
    `_FakeAdkFlowBackend` — no compiled core, no real `google.adk`."""

    def setUp(self) -> None:
        super().setUp()
        self._prior_state = bootstrap._STATE.state
        self._prior_installed = bootstrap._STATE.installed

    def tearDown(self) -> None:
        bootstrap._STATE.state = self._prior_state
        bootstrap._STATE.installed = self._prior_installed
        super().tearDown()

    def designate(self) -> None:
        entry = FlowEntrypoint(
            raw=adk_pack.RUNNER_FLOW_ENTRYPOINT,
            module="google.adk.runners",
            function="Runner.run_async",
        )
        bootstrap._STATE.state = {"flow_entrypoints": [entry]}
        bootstrap._STATE.installed = True

    def undesignate(self) -> None:
        bootstrap._STATE.state = None
        bootstrap._STATE.installed = False

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


if __name__ == "__main__":
    unittest.main()
